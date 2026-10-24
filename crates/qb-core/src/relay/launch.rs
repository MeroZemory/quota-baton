//! D22 catalog observations use the relay's client and credential owner.
use super::*;
use crate::catalog::{CatalogError, KnownCatalog, Observation, RefreshFailure, RefreshMode};

const CATALOG_DEADLINE: Duration = Duration::from_secs(15);
const BOUND_STALE_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

/// Read-only bounded D22 metadata side copy; never reserialize an input body.
/// Complete malformed/duplicate-key metadata is rejected. Streaming prefixes
/// may expose a model before the rest of the JSON arrives, as C8 already does.
pub(super) fn request_model(headers: &HeaderMap, prefix: &[u8]) -> Result<String, ()> {
    use std::io::{Cursor, Read};
    let mut encodings = headers.get_all("content-encoding").iter();
    let encoding = encodings
        .next()
        .map(|value| value.to_str().map(str::trim))
        .transpose()
        .map_err(|_| ())?;
    if encodings.next().is_some() {
        return Err(());
    }
    let prefix = &prefix[..prefix.len().min(upload::PEEK_LIMIT)];
    let encoding = encoding.unwrap_or("identity").to_ascii_lowercase();
    let mut decoded = Vec::new();
    let bytes = if encoding == "identity" {
        prefix
    } else {
        let reader: Box<dyn Read> = match encoding.as_str() {
            "zstd" => {
                let mut decoder =
                    zstd::stream::read::Decoder::new(Cursor::new(prefix)).map_err(|_| ())?;
                decoder.window_log_max(23).map_err(|_| ())?;
                Box::new(decoder)
            }
            "gzip" => Box::new(flate2::read::GzDecoder::new(Cursor::new(prefix))),
            "deflate" => Box::new(flate2::read::ZlibDecoder::new(Cursor::new(prefix))),
            "br" => {
                // Match the relay side decoders: reject the large-window
                // extension before the library can allocate its ring buffer.
                if prefix.first().is_some_and(|byte| byte & 0x7f == 0x11) {
                    return Err(());
                }
                Box::new(brotli::Decompressor::new(Cursor::new(prefix), 4096))
            }
            _ => return Err(()),
        };
        // A compressed prefix can end before the input stream does. Keep only
        // bounded decoded metadata, even when the side decoder reaches EOF.
        let _ = reader
            .take(upload::PEEK_LIMIT as u64)
            .read_to_end(&mut decoded);
        decoded.as_slice()
    };
    #[derive(serde::Deserialize)]
    struct Model {
        model: Option<String>,
    }
    let model = match serde_json::from_slice::<Model>(bytes) {
        Ok(value) => value.model,
        Err(error) if error.is_eof() => session::head_model(bytes),
        Err(_) => return Err(()),
    };
    model
        .filter(|model| crate::catalog::valid_model_id(model))
        .ok_or(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub model: Option<String>,
    #[serde(default)]
    pub model_is_explicit: bool,
    pub account_pref: Option<String>,
    pub resume_id: Option<String>,
    #[serde(default)]
    pub move_for_model: bool,
    /// Authenticated launcher intent only; plain issuance preserves policy.
    #[serde(default)]
    pub urgent: bool,
}

pub use qb_proto::admin_launch::LaunchReply;

#[derive(Clone, Copy, Debug)]
pub enum LaunchError {
    Invalid,
    Unavailable,
    ConsentRequired,
    UnsupportedModel,
    Capacity,
    Store,
}
impl LaunchError {
    pub fn status(self) -> StatusCode {
        match self {
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::ConsentRequired | Self::UnsupportedModel => StatusCode::CONFLICT,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_launch",
            Self::Unavailable => "launch_unavailable",
            Self::ConsentRequired => "model_move_consent_required",
            Self::UnsupportedModel => "model_not_supported",
            Self::Capacity => "launch_capacity",
            Self::Store => "launch_store_error",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::Invalid => "실행 옵션이 올바르지 않습니다.",
            Self::Unavailable => {
                "모델 목록을 확인한 사용 가능 계정이 없습니다. 잠시 후 다시 실행해 주세요."
            }
            Self::ConsentRequired => {
                "기존 세션의 계정에서 요청한 모델을 사용할 수 없습니다. 다른 계정으로 옮기려면 --account 또는 --move-for-model을 지정해 주세요."
            }
            Self::UnsupportedModel => {
                "이 세션의 계정은 요청한 모델을 지원하지 않습니다. 모델을 바꾸거나 codex-qb를 --move-for-model로 다시 실행해 주세요."
            }
            Self::Capacity => {
                "실행 연결을 더 만들 수 없습니다. 사용하지 않는 실행을 종료한 뒤 다시 시도해 주세요."
            }
            Self::Store => "계정 연결을 안전하게 저장하지 못했습니다. 코어 로그를 확인해 주세요.",
        }
    }
}

fn validate_issue_request(request: &LaunchRequest, port: u16) -> Result<(), LaunchError> {
    let identifier = |value: &str| {
        !value.is_empty()
            && value.len() <= 240
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    };
    if port == 0
        || request
            .model
            .as_ref()
            .is_some_and(|m| !crate::catalog::valid_model_id(m))
        || (request.model_is_explicit && request.model.is_none())
        || request.resume_id.as_ref().is_some_and(|id| !identifier(id))
        || request.account_pref.as_ref().is_some_and(|name| {
            name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control)
        })
    {
        return Err(LaunchError::Invalid);
    }
    Ok(())
}

/// Match exact local IDs or labels as one namespace, never prefix/first match.
/// Resolve against all Codex accounts, including currently unusable accounts:
/// an unambiguous but unusable preference remains a preference, not a pin.
fn resolve_issue_preference<'a>(
    preference: Option<&str>,
    accounts: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<Option<String>, LaunchError> {
    let Some(preference) = preference else {
        return Ok(None);
    };
    let matches: BTreeSet<_> = accounts
        .filter(|(id, label)| *id == preference || *label == preference)
        .map(|(id, _)| id)
        .collect();
    if matches.len() != 1 {
        return Err(LaunchError::Invalid);
    }
    Ok(matches.first().map(|id| (*id).to_owned()))
}

fn issue_anchor() -> Result<String, LaunchError> {
    use rand::TryRngCore;
    // Independent random namespace, never derived from the secret routing token.
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| LaunchError::Store)?;
    let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("launch:{suffix}"))
}

fn issue_binding_error(error: crate::bindings::LaunchError) -> LaunchError {
    use crate::bindings::LaunchError as B;
    match error {
        B::InvalidMetadata | B::AnchorConflict => LaunchError::Invalid,
        B::ConsentRequired => LaunchError::ConsentRequired,
        B::ModelUnsupported => LaunchError::UnsupportedModel,
        B::Capacity => LaunchError::Capacity,
        B::Retired => LaunchError::Store,
        B::UnknownLaunch
        | B::NoEligibleAccount
        | B::EntitlementsUnknown
        | B::Busy
        | B::MoveGuard
        | B::PolicyBlocked
        | B::BindingChanged => LaunchError::Unavailable,
    }
}

fn issue_token_error(error: crate::launch_tokens::LaunchTokenError) -> LaunchError {
    if error == crate::launch_tokens::LaunchTokenError::Capacity {
        LaunchError::Capacity
    } else {
        LaunchError::Store
    }
}

/// Private issuance transaction. No plaintext token is published until success.
/// The pin prevents expiry during a long durable write; every failure/unwind
/// drops the pin and erases the unpublished registry entry. No binding undo.
struct PendingIssue {
    registry: crate::launch_tokens::LaunchTokenRegistry,
    token: crate::launch_tokens::LaunchToken,
    pin: Option<crate::launch_tokens::LaunchPin>,
    published: bool,
}
impl PendingIssue {
    fn new(
        registry: crate::launch_tokens::LaunchTokenRegistry,
        anchor: &str,
    ) -> Result<Self, LaunchError> {
        let token = registry
            .issue(crate::launch_tokens::LaunchMetadata {
                binding_anchor_key: anchor.to_owned(),
                advertised_models: BTreeSet::new(),
            })
            .map_err(issue_token_error)?;
        let mut pending = Self {
            registry,
            token,
            pin: None,
            published: false,
        };
        pending.pin = Some(
            pending
                .registry
                .get_or_pin(pending.token.expose_secret())
                .map_err(issue_token_error)?,
        );
        Ok(pending)
    }
}
impl Drop for PendingIssue {
    fn drop(&mut self) {
        self.pin.take();
        if !self.published {
            self.registry.discard_unpublished(&self.token);
        }
    }
}

use std::collections::BTreeMap;

const ISSUE_CATALOG_BUDGET: Duration = Duration::from_secs(8);
const ISSUE_CATALOG_CONCURRENCY: usize = 4;

/// One deadline for the whole collection, including queued catalogs. A timed-out
/// batch is unusable for placement; completed partial observations stay in cache.
/// Buffered futures are directly owned here: dropping this future cancels them.
async fn collect_issue_catalogs<T, F, Fut>(
    ids: impl IntoIterator<Item = String>,
    bound: Option<&str>,
    deadline: tokio::time::Instant,
    mut fetch: F,
) -> Result<BTreeMap<String, T>, LaunchError>
where
    F: FnMut(String, bool) -> Fut,
    Fut: std::future::Future<Output = Result<T, ()>>,
{
    use futures_util::{StreamExt, stream};
    let collect = async {
        let pending = stream::iter(ids)
            .map(|id| {
                let existing = bound == Some(id.as_str());
                let future = fetch(id.clone(), existing);
                async move { (id, existing, future.await) }
            })
            .buffer_unordered(ISSUE_CATALOG_CONCURRENCY);
        tokio::pin!(pending);
        let mut observed = BTreeMap::new();
        loop {
            // timeout_at alone can accept an immediately-ready future after its
            // deadline. Check before polling/refilling and after every result.
            if tokio::time::Instant::now() >= deadline {
                return Err(LaunchError::Unavailable);
            }
            let next = pending.next().await;
            if tokio::time::Instant::now() >= deadline {
                return Err(LaunchError::Unavailable);
            }
            let Some((id, existing, result)) = next else {
                return Ok(observed);
            };
            match result {
                Ok(catalog) => {
                    observed.insert(id, catalog);
                }
                // An unknown bound catalog must never authorize a move.
                Err(()) if existing => return Err(LaunchError::Unavailable),
                Err(()) => {}
            }
        }
    };
    tokio::time::timeout_at(deadline, collect)
        .await
        .map_err(|_| LaunchError::Unavailable)?
}

#[cfg(test)]
mod issue_budget_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    fn ids(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("a{i}")).collect()
    }

    #[tokio::test]
    async fn issue_budget_limits_concurrency_and_collects_every_success() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let observed = collect_issue_catalogs(
            ids(13),
            None,
            tokio::time::Instant::now() + Duration::from_secs(2),
            |id, existing| {
                assert!(!existing);
                let active = active.clone();
                let peak = peak.clone();
                async move {
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    let _guard = Active(active);
                    peak.fetch_max(count, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok::<_, ()>(id)
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(observed.len(), 13);
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn issue_budget_timeout_discards_partial_set_and_drops_inflight() {
        let active = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let result = collect_issue_catalogs(
            ids(64),
            None,
            tokio::time::Instant::now() + Duration::from_millis(25),
            |id, _| {
                let active = active.clone();
                let started = started.clone();
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    active.fetch_add(1, Ordering::SeqCst);
                    let _guard = Active(active);
                    if id == "a0" {
                        return Ok::<_, ()>(BTreeSet::<String>::new());
                    }
                    std::future::pending().await
                }
            },
        )
        .await;
        assert!(matches!(result, Err(LaunchError::Unavailable)));
        assert_eq!(started.load(Ordering::SeqCst), 5);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn issue_budget_unknown_is_absent_known_empty_is_present_and_bound_is_required() {
        let observed = collect_issue_catalogs(
            vec!["unknown".into(), "empty".into()],
            None,
            tokio::time::Instant::now() + Duration::from_secs(2),
            |id, _| async move {
                if id == "unknown" {
                    Err(())
                } else {
                    Ok(BTreeSet::<String>::new())
                }
            },
        )
        .await
        .unwrap();
        assert!(!observed.contains_key("unknown"));
        assert!(observed.get("empty").unwrap().is_empty());
        let result = collect_issue_catalogs(
            vec!["bound".into()],
            Some("bound"),
            tokio::time::Instant::now() + Duration::from_secs(2),
            |_, existing| async move {
                assert!(existing);
                Err::<(), _>(())
            },
        )
        .await;
        assert!(matches!(result, Err(LaunchError::Unavailable)));
    }

    #[tokio::test]
    async fn issue_budget_expired_deadline_never_starts_even_ready_catalogs() {
        let started = AtomicUsize::new(0);
        let result = collect_issue_catalogs(ids(64), None, tokio::time::Instant::now(), |_, _| {
            started.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok::<_, ()>(()))
        })
        .await;
        assert!(matches!(result, Err(LaunchError::Unavailable)));
        assert_eq!(started.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn issue_budget_caller_cancellation_drops_owned_catalog_futures() {
        let active = Arc::new(AtomicUsize::new(0));
        let collect = collect_issue_catalogs(
            ids(64),
            None,
            tokio::time::Instant::now() + Duration::from_secs(8),
            |_, _| {
                let active = active.clone();
                async move {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _guard = Active(active);
                    std::future::pending::<Result<(), ()>>().await
                }
            },
        );
        let mut collect = Box::pin(collect);
        // Poll exactly once with a no-op waker. No timer sleeps or spawned tasks.
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(collect.as_mut(), &mut context).is_pending());
        assert_eq!(active.load(Ordering::SeqCst), 4);
        drop(collect);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
}

#[derive(Clone)]
pub(super) struct LaunchIngress {
    pub(super) pin: Option<Arc<crate::launch_tokens::LaunchPin>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::import;
    use qb_testkit::{ScriptedResponse, ScriptedUpstream};
    use serde_json::json;
    use std::{fs, path::Path};

    #[test]
    fn request_model_bounds_compressed_metadata_windows() {
        use std::io::Write;
        let json = br#"{"model":"gpt-synthetic","input":[]}"#;
        for encoding in ["gzip", "deflate", "br", "zstd"] {
            let encoded = match encoding {
                "gzip" => {
                    let mut writer =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                    writer.write_all(json).unwrap();
                    writer.finish().unwrap()
                }
                "deflate" => {
                    let mut writer =
                        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                    writer.write_all(json).unwrap();
                    writer.finish().unwrap()
                }
                "br" => {
                    let mut output = Vec::new();
                    {
                        let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 4, 22);
                        writer.write_all(json).unwrap();
                    }
                    output
                }
                _ => zstd::stream::encode_all(std::io::Cursor::new(json), 1).unwrap(),
            };
            let mut headers = HeaderMap::new();
            headers.insert("content-encoding", encoding.parse().unwrap());
            assert_eq!(
                request_model(&headers, &encoded).unwrap(),
                "gpt-synthetic",
                "{encoding}"
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "br".parse().unwrap());
        assert!(request_model(&headers, &[0x11, 0xff, 0xff, 0xff]).is_err());
    }

    struct Fixture {
        _root: tempfile::TempDir,
        cpa: std::path::PathBuf,
        relay: Arc<RelayState>,
        upstream: ScriptedUpstream,
        id: String,
    }
    fn credential(version: u8) -> serde_json::Value {
        json!({"type":"codex","email":"catalog@example.invalid","account_id":"catalog-account",
            "access_token":format!("catalog-synthetic-access-{version}"),
            "refresh_token":"catalog-synthetic-refresh","expired":"2099-01-01T00:00:00Z",
            "last_refresh":format!("2026-10-2{version}T00:00:00Z")})
    }
    impl Fixture {
        async fn new() -> Self {
            let base =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r11-catalog-tests");
            fs::create_dir_all(&base).unwrap();
            let root = tempfile::tempdir_in(base).unwrap();
            let home = root.path().join("home");
            let cpa = root.path().join("synthetic-cpa");
            fs::create_dir_all(&cpa).unwrap();
            fs::write(
                cpa.join("account.json"),
                serde_json::to_vec(&credential(0)).unwrap(),
            )
            .unwrap();
            let store = Arc::new(AccountStore::acquire(&home).unwrap());
            import::run(
                &store,
                &cpa,
                &["codex:catalog@example.invalid".into()],
                false,
            )
            .unwrap();
            let id = store.list().unwrap()[0].id.clone();
            let upstream = ScriptedUpstream::start().await.unwrap();
            let mut config = Config::default();
            config.upstream.codex_base = upstream.url.clone();
            let obs = Arc::new(Observability::new(&home, None).unwrap());
            let relay = Arc::new(
                RelayState::new(store, &config, obs, Arc::new(DebugPins::default())).unwrap(),
            );
            Self {
                _root: root,
                cpa,
                relay,
                upstream,
                id,
            }
        }
        fn reply(&self, status: StatusCode, value: serde_json::Value) {
            self.upstream
                .state
                .push(ScriptedResponse::json(status, value.to_string()));
        }
    }

    #[tokio::test]
    async fn catalog_singleflight_preserves_provider_capabilities_and_credential_headers() {
        let f = Fixture::new().await;
        let raw = json!({"models":[{"slug":"synthetic-model","tools":{"image":true},"display_name":"Synthetic"}],"capabilities":{"daybreak":true}});
        let mut response = ScriptedResponse::json(StatusCode::OK, raw.to_string());
        response.chunks[0].0 = Duration::from_millis(100);
        f.upstream.state.push(response);
        let (a, b) = tokio::join!(
            f.relay.launch_catalog(&f.id, true, false),
            f.relay.launch_catalog(&f.id, true, false)
        );
        assert_eq!(a.unwrap().raw(), &raw);
        assert!(b.unwrap().supported().contains("synthetic-model"));
        let calls = f.upstream.state.requests();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path_and_query, "/models?client_version=0.163.0");
        assert_eq!(
            calls[0].headers["authorization"],
            "Bearer catalog-synthetic-access-0"
        );
        assert_eq!(calls[0].headers["chatgpt-account-id"], "catalog-account");
        assert!(
            calls[0].headers["user-agent"]
                .to_str()
                .unwrap()
                .starts_with("quota-baton/")
        );
    }

    #[tokio::test]
    async fn catalog_failures_do_not_become_empty_or_admit_new_placements_with_stale_evidence() {
        let f = Fixture::new().await;
        f.reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"synthetic"}),
        );
        assert!(f.relay.launch_catalog(&f.id, false, false).await.is_err());
        f.reply(
            StatusCode::OK,
            json!({"models":[{"slug":"synthetic-model"}]}),
        );
        assert!(
            f.relay
                .launch_catalog(&f.id, true, false)
                .await
                .unwrap()
                .supported()
                .contains("synthetic-model")
        );
        f.reply(StatusCode::BAD_GATEWAY, json!({"error":"synthetic"}));
        assert!(
            f.relay
                .launch_catalog(&f.id, true, true)
                .await
                .unwrap()
                .supported()
                .contains("synthetic-model")
        );
        assert!(f.relay.launch_catalog(&f.id, false, false).await.is_err());
        assert_eq!(f.upstream.state.requests().len(), 3);
        f.reply(StatusCode::OK, json!({"models":[]}));
        assert!(
            f.relay
                .launch_catalog(&f.id, true, false)
                .await
                .unwrap()
                .supported()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn catalog_current_401_uses_shared_linked_reload_without_marking_relogin() {
        let f = Fixture::new().await;
        f.reply(
            StatusCode::UNAUTHORIZED,
            json!({"error":{"code":"invalid_token"}}),
        );
        f.reply(StatusCode::OK, json!({"models":[{"slug":"after-reload"}]}));
        let (arrived, release) = f.upstream.state.pause_next_response();
        let (catalog, ()) = tokio::join!(f.relay.launch_catalog(&f.id, false, false), async {
            tokio::time::timeout(Duration::from_secs(5), arrived)
                .await
                .expect("first catalog request did not arrive")
                .expect("first catalog response gate dropped");
            // The first before_request reload must still read token 0.
            // Publish its replacement only after that request is recorded.
            fs::write(
                f.cpa.join("account.json"),
                serde_json::to_vec(&credential(1)).unwrap(),
            )
            .unwrap();
            release.send(()).expect("catalog response gate abandoned");
        });
        assert!(catalog.unwrap().supported().contains("after-reload"));
        let calls = f.upstream.state.requests();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].headers["authorization"],
            "Bearer catalog-synthetic-access-0"
        );
        assert_eq!(
            calls[1].headers["authorization"],
            "Bearer catalog-synthetic-access-1"
        );
        assert!(!matches!(
            f.relay.store.load(&f.id).unwrap().health,
            Health::NeedsRelogin { .. }
        ));
    }

    #[tokio::test]
    async fn catalog_rejects_oversized_and_malformed_data_without_upstream_shopping() {
        let f = Fixture::new().await;
        f.upstream.state.push(ScriptedResponse::json(
            StatusCode::OK,
            "x".repeat(crate::catalog::MAX_RAW_BYTES + 1),
        ));
        assert!(f.relay.launch_catalog(&f.id, true, false).await.is_err());
        f.reply(StatusCode::OK, json!({"models":[{"slug":"bad model"}]}));
        assert!(f.relay.launch_catalog(&f.id, true, false).await.is_err());
        assert_eq!(f.upstream.state.requests().len(), 2);
    }

    #[tokio::test]
    async fn launch_body_keeps_pin_through_frames_and_preserves_trailers_then_releases_at_eof() {
        use http_body_util::{BodyExt, StreamBody};
        let registry = crate::launch_tokens::LaunchTokenRegistry::default();
        let token = registry
            .issue(crate::launch_tokens::LaunchMetadata {
                binding_anchor_key: "codex:synthetic-anchor".into(),
                advertised_models: BTreeSet::new(),
            })
            .unwrap();
        let pin = Arc::new(registry.get_or_pin(token.expose_secret()).unwrap());
        let weak = Arc::downgrade(&pin);
        let mut trailers = HeaderMap::new();
        trailers.insert("x-synthetic-trailer", HeaderValue::from_static("kept"));
        let stream = futures_util::stream::iter([
            Ok::<_, std::io::Error>(hyper::body::Frame::data(Bytes::from_static(b"unaltered"))),
            Ok(hyper::body::Frame::trailers(trailers)),
        ]);
        let response = Response::new(axum::body::Body::new(StreamBody::new(stream)));
        let mut body = pin_body(response, Some(pin)).into_body();
        assert!(weak.upgrade().is_some());
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "unaltered"
        );
        assert!(weak.upgrade().is_some());
        assert_eq!(
            body.frame()
                .await
                .unwrap()
                .unwrap()
                .into_trailers()
                .unwrap()["x-synthetic-trailer"],
            "kept"
        );
        assert!(body.frame().await.is_none());
        assert!(
            weak.upgrade().is_none(),
            "EOF releases activity even while the body object remains alive"
        );
        let pin = Arc::new(registry.get_or_pin(token.expose_secret()).unwrap());
        let weak = Arc::downgrade(&pin);
        let body = pin_body(Response::new(axum::body::Body::empty()), Some(pin));
        assert!(weak.upgrade().is_some());
        drop(body);
        assert!(weak.upgrade().is_none(), "cancel releases activity");
    }
    fn issue_request(model: Option<&str>) -> LaunchRequest {
        LaunchRequest {
            model: model.map(str::to_owned),
            model_is_explicit: false,
            account_pref: None,
            resume_id: None,
            move_for_model: false,
            urgent: false,
        }
    }
    fn issued_handle(reply: &LaunchReply) -> &str {
        reply
            .base_url
            .split("/l/")
            .nth(1)
            .unwrap()
            .strip_suffix("/v1")
            .unwrap()
    }
    async fn warm_issue_catalog(f: &Fixture, id: &str, models: &[&str]) {
        f.reply(
            StatusCode::OK,
            json!({"models": models.iter()
            .map(|model| json!({"slug":model})).collect::<Vec<_>>()}),
        );
        f.relay.launch_catalog(id, true, false).await.unwrap();
    }

    async fn model_admission_fixture(cooling: bool) -> (Fixture, crate::bindings::LaunchInfo) {
        let f = Fixture::new().await;
        warm_issue_catalog(&f, &f.id, &["common"]).await;
        let reply = f
            .relay
            .issue_launch(issue_request(Some("common")), 10200)
            .await
            .unwrap();
        let pin = f
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&reply))
            .unwrap();
        let info = f
            .relay
            .bindings
            .launch_info(&pin.metadata().binding_anchor_key, chrono::Utc::now())
            .unwrap();
        assert!(info.managed);
        assert_eq!(info.current_model.as_deref(), Some("common"));
        if cooling {
            let now = chrono::Utc::now();
            let reset = now + chrono::Duration::hours(4);
            f.relay
                .store
                .update_metadata(&f.id, |account| {
                    account.health = crate::accounts::Health::Cooling {
                        reason: crate::accounts::HealthReason::Quota,
                        until: reset,
                        scope: "account".into(),
                    };
                    account.quota_cooling = Some(crate::accounts::QuotaCooling {
                        window: "5h".into(),
                        reset_at: reset,
                        started_at: now,
                    });
                })
                .unwrap();
        }
        (f, info)
    }

    #[tokio::test]
    async fn r13_g_current_model_recovery_precedes_stalled_source_catalog() {
        for requested in [None, Some("common")] {
            let (f, info) = model_admission_fixture(true).await;
            let refresh = CatalogRefreshOwner::new(
                f.relay
                    .catalogs
                    .begin_refresh(&f.id, RefreshMode::Force)
                    .unwrap()
                    .unwrap(),
                &f.relay.catalog_changed,
            );
            let requests = f.upstream.state.requests().len();
            let admitted = tokio::time::timeout(
                Duration::from_secs(2),
                f.relay.admit_data_model(
                    &info,
                    requested,
                    tokio::time::Instant::now() + Duration::from_secs(10),
                ),
            )
            .await
            .expect("current-model recovery must not await the stalled source catalog");
            assert_eq!(admitted.unwrap().as_deref(), Some("common"));
            assert!(f.relay.catalogs.view(&f.id).unwrap().refreshing);
            assert_eq!(f.upstream.state.requests().len(), requests);
            assert_eq!(
                f.relay
                    .bindings
                    .current(Some(&info.canonical_key))
                    .as_deref(),
                Some(f.id.as_str())
            );
            assert!(
                !f.relay
                    .dispatch_data_allowed(Some(&info.canonical_key), &f.id, Some("common"))
                    .await
            );
            drop(refresh);
        }
    }

    #[tokio::test]
    async fn r13_g_new_model_and_usable_source_still_require_catalog_admission() {
        for cooling in [false, true] {
            let (f, info) = model_admission_fixture(cooling).await;
            let requested = if cooling { "special" } else { "common" };
            let supported = if cooling { "common" } else { "special" };
            let refresh = CatalogRefreshOwner::new(
                f.relay
                    .catalogs
                    .begin_refresh(&f.id, RefreshMode::Force)
                    .unwrap()
                    .unwrap(),
                &f.relay.catalog_changed,
            );
            let admission = f.relay.admit_data_model(
                &info,
                Some(requested),
                tokio::time::Instant::now() + Duration::from_secs(10),
            );
            tokio::pin!(admission);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut admission)
                    .await
                    .is_err(),
                "new models and usable sources must await their catalog"
            );
            // Both the singleflight result and the forced unsupported-model
            // re-observation refuse this pick.
            let catalog = json!({"models":[{"slug":supported}]});
            f.reply(StatusCode::OK, catalog.clone());
            refresh.finish(Ok(catalog)).unwrap();
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), &mut admission)
                    .await
                    .unwrap(),
                Err(DataLaunchError::UnsupportedModel)
            ));
            let current = f
                .relay
                .bindings
                .launch_info(&info.canonical_key, chrono::Utc::now())
                .unwrap();
            assert_eq!(current.account, f.id);
            assert_eq!(current.current_model.as_deref(), Some("common"));
        }
    }

    async fn second_issue_account(f: &Fixture) -> String {
        let mut second = credential(0);
        second["email"] = json!("second@example.invalid");
        second["account_id"] = json!("second-account");
        fs::write(
            f.cpa.join("second.json"),
            serde_json::to_vec(&second).unwrap(),
        )
        .unwrap();
        import::run(
            &f.relay.store,
            &f.cpa,
            &["codex:second@example.invalid".into()],
            false,
        )
        .unwrap();
        f.relay
            .store
            .list()
            .unwrap()
            .into_iter()
            .find(|a| a.email == "second@example.invalid")
            .unwrap()
            .id
    }

    #[test]
    fn issue_launch_preference_rejects_ambiguity_including_id_label_collisions() {
        for (input, rows) in [
            ("same", vec![("a", "same"), ("b", "same")]),
            ("a", vec![("a", "first"), ("b", "a")]),
            ("missing", vec![("a", "first")]),
        ] {
            assert!(matches!(
                resolve_issue_preference(Some(input), rows.into_iter()),
                Err(LaunchError::Invalid)
            ));
        }
        assert_eq!(
            resolve_issue_preference(Some("a"), [("a", "a")].into_iter()).unwrap(),
            Some("a".into())
        );
    }

    #[tokio::test]
    async fn issue_launch_validates_before_any_catalog_request_or_placement() {
        let f = Fixture::new().await;
        for request in [
            issue_request(None),
            issue_request(Some("bad model")),
            LaunchRequest {
                resume_id: Some("../bad".into()),
                ..issue_request(Some("m"))
            },
            LaunchRequest {
                model_is_explicit: true,
                ..issue_request(None)
            },
        ] {
            assert!(matches!(
                f.relay.issue_launch(request, 10200).await,
                Err(LaunchError::Invalid)
            ));
        }
        assert!(matches!(
            f.relay.issue_launch(issue_request(Some("m")), 0).await,
            Err(LaunchError::Invalid)
        ));
        assert!(f.upstream.state.requests().is_empty());
    }

    #[test]
    fn n5c_launch_urgent_wire_is_optional_bool_and_rejects_unknown_authority() {
        let plain: LaunchRequest = serde_json::from_value(json!({"model":"common"})).unwrap();
        assert!(!plain.urgent);
        let urgent: LaunchRequest =
            serde_json::from_value(json!({"model":"common","urgent":true})).unwrap();
        assert!(urgent.urgent);
        for value in [
            json!(null),
            json!("move"),
            json!(1),
            json!({"session_id":"synthetic"}),
        ] {
            assert!(
                serde_json::from_value::<LaunchRequest>(json!({"model":"common","urgent":value}))
                    .is_err()
            );
        }
        assert!(
            serde_json::from_value::<LaunchRequest>(json!({
                "model":"common", "urgent":true, "policy_id":"synthetic-display-label",
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn n5c_urgent_launch_prepublication_and_plain_resume_preserve_overlay() {
        let f = Fixture::new().await;
        warm_issue_catalog(&f, &f.id, &["common"]).await;
        let first = f
            .relay
            .issue_launch(
                LaunchRequest {
                    urgent: true,
                    ..issue_request(Some("common"))
                },
                10200,
            )
            .await
            .unwrap();
        let pin = f
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&first))
            .unwrap();
        let anchor = pin.metadata().binding_anchor_key;
        assert_eq!(
            f.relay
                .bindings
                .routing_plan(Some(&anchor))
                .settings
                .on_5h_limit,
            crate::routing::On5hLimit::Move
        );
        let id = "12345678-1234-4234-8234-123456789abc";
        let key = format!("codex:{id}");
        f.relay
            .bindings
            .resolve_launch(Some(&anchor), Some(&key), chrono::Utc::now())
            .unwrap();
        for expected in [
            crate::routing::On5hLimit::Move,
            crate::routing::On5hLimit::Wait,
        ] {
            f.relay
                .bindings
                .set_routing_session_urgent(&crate::routing::session_id(&key), expected)
                .unwrap();
            let resumed = f
                .relay
                .issue_launch(
                    LaunchRequest {
                        resume_id: Some(id.into()),
                        ..issue_request(Some("common"))
                    },
                    10200,
                )
                .await
                .unwrap();
            assert_eq!(resumed.account, first.account);
            assert_eq!(
                f.relay
                    .bindings
                    .routing_plan(Some(&key))
                    .settings
                    .on_5h_limit,
                expected
            );
        }
        f.relay
            .bindings
            .set_routing_session(
                &crate::routing::session_id(&key),
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Strict,
                    reset_wait_secs: 17,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                false,
            )
            .unwrap();
        f.relay
            .issue_launch(
                LaunchRequest {
                    urgent: true,
                    resume_id: Some(id.into()),
                    ..issue_request(Some("common"))
                },
                10200,
            )
            .await
            .unwrap();
        let plan = f.relay.bindings.routing_plan(Some(&key));
        assert_eq!(plan.settings.mode, crate::routing::Mode::Strict);
        assert_eq!(plan.settings.reset_wait_secs, 17);
        assert_eq!(plan.settings.on_5h_limit, crate::routing::On5hLimit::Move);
        assert_eq!(plan.anchor.as_deref(), Some(first.account.as_str()));
    }

    #[tokio::test]
    async fn issue_launch_selects_effective_model_and_never_pins_incompatible_preference() {
        let f = Fixture::new().await;
        let second = second_issue_account(&f).await;
        warm_issue_catalog(&f, &f.id, &["common"]).await;
        warm_issue_catalog(&f, &second, &["common", "special"]).await;
        let reply = f
            .relay
            .issue_launch(
                LaunchRequest {
                    account_pref: Some(f.id.clone()),
                    ..issue_request(Some("special"))
                },
                10200,
            )
            .await
            .unwrap();
        assert_eq!(reply.account, second);
        assert_eq!(reply.model.as_deref(), Some("special"));
        let pin = f
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&reply))
            .unwrap();
        let metadata = pin.metadata();
        assert!(metadata.binding_anchor_key.starts_with("launch:"));
        assert!(!metadata.binding_anchor_key.contains(issued_handle(&reply)));
        assert!(metadata.advertised_models.is_empty());
        let info = f
            .relay
            .bindings
            .launch_info(&metadata.binding_anchor_key, chrono::Utc::now())
            .unwrap();
        assert_eq!(info.account, reply.account);
        assert!(reply.catalog_url.ends_with("/v1/models"));
        let another = f
            .relay
            .issue_launch(issue_request(Some("special")), 10200)
            .await
            .unwrap();
        assert!(issued_handle(&reply) != issued_handle(&another));
        let another_pin = f
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&another))
            .unwrap();
        assert_ne!(
            metadata.binding_anchor_key,
            another_pin.metadata().binding_anchor_key
        );
    }

    #[tokio::test]
    async fn issue_launch_resume_preserves_canonical_account_and_implicit_current_model() {
        let f = Fixture::new().await;
        let second = second_issue_account(&f).await;
        warm_issue_catalog(&f, &f.id, &["common"]).await;
        warm_issue_catalog(&f, &second, &["common", "special"]).await;
        let first = f
            .relay
            .issue_launch(
                LaunchRequest {
                    account_pref: Some(f.id.clone()),
                    ..issue_request(Some("common"))
                },
                10200,
            )
            .await
            .unwrap();
        let pin = f
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&first))
            .unwrap();
        let anchor = pin.metadata().binding_anchor_key;
        f.relay
            .bindings
            .resolve_launch(Some(&anchor), Some("codex:resume-test"), chrono::Utc::now())
            .unwrap();
        let resumed = f
            .relay
            .issue_launch(
                LaunchRequest {
                    account_pref: Some(second),
                    resume_id: Some("resume-test".into()),
                    ..issue_request(Some("special"))
                },
                10200,
            )
            .await
            .unwrap();
        assert_eq!(resumed.account, f.id);
        assert_eq!(resumed.model.as_deref(), Some("common"));
        let resumed_pin = f
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&resumed))
            .unwrap();
        assert_eq!(
            f.relay
                .bindings
                .launch_info(
                    &resumed_pin.metadata().binding_anchor_key,
                    chrono::Utc::now()
                )
                .unwrap()
                .canonical_key,
            "codex:resume-test"
        );
    }

    #[tokio::test]
    async fn r13_g_resume_refreshes_cached_destination_menu_without_source_catalog_or_flags() {
        for move_for_model in [false, true] {
            let f = Fixture::new().await;
            let second = second_issue_account(&f).await;
            warm_issue_catalog(&f, &f.id, &["common"]).await;
            warm_issue_catalog(&f, &second, &["common"]).await;
            let first = f
                .relay
                .issue_launch(
                    LaunchRequest {
                        account_pref: Some(f.id.clone()),
                        ..issue_request(Some("common"))
                    },
                    10200,
                )
                .await
                .unwrap();
            let pin = f
                .relay
                .launch_tokens
                .get_or_pin(issued_handle(&first))
                .unwrap();
            f.relay
                .bindings
                .resolve_launch(
                    Some(&pin.metadata().binding_anchor_key),
                    Some("codex:r13-g-model-resume"),
                    chrono::Utc::now(),
                )
                .unwrap();
            let now = chrono::Utc::now();
            f.relay
                .store
                .update_metadata(&f.id, |account| {
                    account.health = crate::accounts::Health::Cooling {
                        reason: crate::accounts::HealthReason::Quota,
                        until: now + chrono::Duration::hours(4),
                        scope: "account".into(),
                    };
                })
                .unwrap();
            f.relay.catalogs.remove(&f.id).unwrap();
            let before = f.upstream.state.requests().len();
            // The ready destination now serves the explicit model, although
            // its previously cached menu did not list it.
            f.reply(
                StatusCode::OK,
                json!({"models":[{"slug":"common"},{"slug":"special"}]}),
            );
            let resumed = f
                .relay
                .issue_launch(
                    LaunchRequest {
                        resume_id: Some("r13-g-model-resume".into()),
                        model_is_explicit: true,
                        move_for_model,
                        ..issue_request(Some("special"))
                    },
                    10200,
                )
                .await
                .unwrap();
            assert_eq!(resumed.account, second);
            assert_eq!(resumed.model.as_deref(), Some("special"));
            let calls = f.upstream.state.requests();
            assert_eq!(
                calls.len(),
                before + 1,
                "only destination menu is refreshed"
            );
            let destination = f.relay.store.load(&second).unwrap();
            let tokens = f.relay.store.tokens(&destination).unwrap();
            assert_eq!(
                calls[before].headers["authorization"],
                format!("Bearer {}", tokens.access_token)
            );
            assert!(calls[before].path_and_query.starts_with("/models"));
        }
    }

    #[tokio::test]
    async fn r13_g_model_scoped_current_limit_recovers_explicit_and_implicit_resume() {
        for explicit in [true, false] {
            let f = Fixture::new().await;
            let second = second_issue_account(&f).await;
            warm_issue_catalog(&f, &f.id, &["common"]).await;
            warm_issue_catalog(&f, &second, &["common"]).await;
            let first = f
                .relay
                .issue_launch(
                    LaunchRequest {
                        account_pref: Some(f.id.clone()),
                        ..issue_request(Some("common"))
                    },
                    10200,
                )
                .await
                .unwrap();
            let pin = f
                .relay
                .launch_tokens
                .get_or_pin(issued_handle(&first))
                .unwrap();
            f.relay
                .bindings
                .resolve_launch(
                    Some(&pin.metadata().binding_anchor_key),
                    Some("codex:r13-g-scoped-resume"),
                    chrono::Utc::now(),
                )
                .unwrap();
            f.relay
                .store
                .update_metadata(&f.id, |account| {
                    account.health = crate::accounts::Health::Cooling {
                        reason: crate::accounts::HealthReason::Quota,
                        until: chrono::Utc::now() + chrono::Duration::hours(4),
                        scope: "model:common".into(),
                    };
                })
                .unwrap();
            f.relay.catalogs.remove(&f.id).unwrap();
            let before = f.upstream.state.requests().len();
            let resumed = f
                .relay
                .issue_launch(
                    LaunchRequest {
                        resume_id: Some("r13-g-scoped-resume".into()),
                        model_is_explicit: explicit,
                        ..issue_request(explicit.then_some("common"))
                    },
                    10200,
                )
                .await
                .unwrap();
            assert_eq!(resumed.account, second);
            assert_eq!(resumed.model.as_deref(), Some("common"));
            assert_eq!(
                f.upstream.state.requests().len(),
                before,
                "a current-model limit cannot fetch the unusable source catalog"
            );
        }
    }

    #[tokio::test]
    async fn issue_launch_explicit_resume_requires_consent_and_strict_move_is_durable() {
        let f = Fixture::new().await;
        let second = second_issue_account(&f).await;
        warm_issue_catalog(&f, &f.id, &["common"]).await;
        warm_issue_catalog(&f, &second, &["common", "special"]).await;
        let first = f
            .relay
            .issue_launch(
                LaunchRequest {
                    account_pref: Some(f.id.clone()),
                    ..issue_request(Some("common"))
                },
                10200,
            )
            .await
            .unwrap();
        let pin = f
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&first))
            .unwrap();
        let anchor = pin.metadata().binding_anchor_key;
        f.relay
            .bindings
            .resolve_launch(
                Some(&anchor),
                Some("codex:strict-resume"),
                chrono::Utc::now(),
            )
            .unwrap();
        f.relay
            .bindings
            .policies
            .set_session(
                &crate::routing::session_id(&anchor),
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Strict,
                    reset_wait_secs: 7,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                Some(&f.id),
            )
            .unwrap();
        let denied = f
            .relay
            .issue_launch(
                LaunchRequest {
                    resume_id: Some("strict-resume".into()),
                    model_is_explicit: true,
                    ..issue_request(Some("special"))
                },
                10200,
            )
            .await;
        assert!(matches!(denied, Err(LaunchError::ConsentRequired)));
        assert_eq!(
            f.relay
                .bindings
                .launch_info("codex:strict-resume", chrono::Utc::now())
                .unwrap()
                .account,
            f.id
        );
        let moved = f
            .relay
            .issue_launch(
                LaunchRequest {
                    resume_id: Some("strict-resume".into()),
                    model_is_explicit: true,
                    move_for_model: true,
                    ..issue_request(Some("special"))
                },
                10200,
            )
            .await
            .unwrap();
        assert_eq!(moved.account, second);
        let plan = f.relay.bindings.routing_plan(Some("codex:strict-resume"));
        assert_eq!(plan.settings.mode, crate::routing::Mode::Strict);
        assert_eq!(plan.anchor.as_deref(), Some(second.as_str()));
        // An undelivered token is disposable; already durable history is not.
        // Exercise cleanup after a completed authorization without inventing
        // a reversible policy transaction or transport-failure injection API.
        let durable_path = f.relay.store.home().join("state/routing-policies.json");
        let durable = fs::read(&durable_path).unwrap();
        drop(PendingIssue::new(f.relay.launch_tokens.clone(), "launch:late-error").unwrap());
        assert_eq!(fs::read(&durable_path).unwrap(), durable);
        let restarted = crate::routing::PolicyStore::new(
            f.relay.store.home(),
            crate::routing::Settings::default(),
        )
        .unwrap();
        assert_eq!(
            restarted.plan(Some(&anchor)).anchor.as_deref(),
            Some(second.as_str())
        );
    }

    #[tokio::test]
    async fn issue_launch_token_capacity_precedes_binding_mutation() {
        let f = Fixture::new().await;
        warm_issue_catalog(&f, &f.id, &["common"]).await;
        for i in 0..crate::launch_tokens::CAPACITY {
            f.relay
                .launch_tokens
                .issue(crate::launch_tokens::LaunchMetadata {
                    binding_anchor_key: format!("launch:capacity-{i}"),
                    advertised_models: BTreeSet::new(),
                })
                .unwrap();
        }
        let before = f.relay.bindings.export().unwrap();
        assert!(matches!(
            f.relay
                .issue_launch(issue_request(Some("common")), 10200)
                .await,
            Err(LaunchError::Capacity)
        ));
        assert_eq!(f.relay.bindings.export().unwrap(), before);
    }

    #[test]
    fn issue_launch_unpublished_token_rolls_back_on_error_and_unwind() {
        let registry = crate::launch_tokens::LaunchTokenRegistry::default();
        let pending = PendingIssue::new(registry.clone(), "launch:rollback").unwrap();
        let handle = pending.token.expose_secret().to_owned();
        drop(pending);
        assert!(matches!(
            registry.get_or_pin(&handle),
            Err(crate::launch_tokens::LaunchTokenError::Missing)
        ));
        let pending = PendingIssue::new(registry.clone(), "launch:unwind").unwrap();
        let handle = pending.token.expose_secret().to_owned();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _pending = pending;
                panic!("synthetic issuance failure");
            }))
            .is_err()
        );
        assert!(registry.get_or_pin(&handle).is_err());
        let mut pending = PendingIssue::new(registry.clone(), "launch:published").unwrap();
        let handle = pending.token.expose_secret().to_owned();
        pending.published = true;
        drop(pending);
        assert!(registry.get_or_pin(&handle).is_ok());
    }

    #[tokio::test]
    async fn issue_launch_failed_strict_writer_preserves_binding_and_anchor_and_frees_token() {
        let f = Fixture::new().await;
        let second = second_issue_account(&f).await;
        warm_issue_catalog(&f, &f.id, &["common"]).await;
        warm_issue_catalog(&f, &second, &["special"]).await;
        let first = f
            .relay
            .issue_launch(issue_request(Some("common")), 10200)
            .await
            .unwrap();
        let pin = f
            .relay
            .launch_tokens
            .get_or_pin(issued_handle(&first))
            .unwrap();
        let anchor = pin.metadata().binding_anchor_key;
        f.relay
            .bindings
            .resolve_launch(
                Some(&anchor),
                Some("codex:frozen-resume"),
                chrono::Utc::now(),
            )
            .unwrap();
        f.relay
            .bindings
            .policies
            .set_session(
                &crate::routing::session_id(&anchor),
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Strict,
                    reset_wait_secs: 7,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                Some(&f.id),
            )
            .unwrap();
        f.relay.bindings.policies.freeze();
        assert!(matches!(
            f.relay
                .issue_launch(
                    LaunchRequest {
                        resume_id: Some("frozen-resume".into()),
                        model_is_explicit: true,
                        move_for_model: true,
                        ..issue_request(Some("special"))
                    },
                    10200
                )
                .await,
            Err(LaunchError::Unavailable)
        ));
        assert_eq!(
            f.relay
                .bindings
                .launch_info("codex:frozen-resume", chrono::Utc::now())
                .unwrap()
                .account,
            f.id
        );
        assert_eq!(
            f.relay
                .bindings
                .routing_plan(Some("codex:frozen-resume"))
                .anchor
                .as_deref(),
            Some(f.id.as_str())
        );
        // Exactly the delivered first handle remains; the failed attempt must
        // not consume a hidden slot. This also tests callback-error RAII cleanup.
        for i in 1..crate::launch_tokens::CAPACITY {
            f.relay
                .launch_tokens
                .issue(crate::launch_tokens::LaunchMetadata {
                    binding_anchor_key: format!("launch:after-failure-{i}"),
                    advertised_models: BTreeSet::new(),
                })
                .unwrap();
        }
    }

    #[test]
    fn issue_launch_error_mapping_keeps_capacity_distinct_and_store_failures_closed() {
        use crate::bindings::LaunchError as B;
        use crate::launch_tokens::LaunchTokenError as T;
        assert!(matches!(
            issue_token_error(T::Capacity),
            LaunchError::Capacity
        ));
        assert!(matches!(issue_token_error(T::Entropy), LaunchError::Store));
        assert!(matches!(
            issue_binding_error(B::Capacity),
            LaunchError::Capacity
        ));
        assert!(matches!(
            issue_binding_error(B::ConsentRequired),
            LaunchError::ConsentRequired
        ));
        for error in [
            B::Busy,
            B::PolicyBlocked,
            B::EntitlementsUnknown,
            B::BindingChanged,
        ] {
            assert!(matches!(
                issue_binding_error(error),
                LaunchError::Unavailable
            ));
        }
    }

    #[tokio::test]
    async fn issue_launch_unknown_catalog_is_not_an_empty_entitlement_or_a_placement() {
        let f = Fixture::new().await;
        f.reply(StatusCode::BAD_GATEWAY, json!({"error":"synthetic"}));
        let before = f.relay.bindings.export().unwrap();
        assert!(matches!(
            f.relay
                .issue_launch(issue_request(Some("common")), 10200)
                .await,
            Err(LaunchError::Unavailable)
        ));
        assert_eq!(f.relay.bindings.export().unwrap(), before);
        assert_eq!(f.upstream.state.requests().len(), 1);
        assert!(
            f.upstream.state.requests()[0]
                .path_and_query
                .starts_with("/models")
        );
    }
}

impl std::fmt::Debug for LaunchIngress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LaunchIngress([REDACTED])")
    }
}

/// Forward frames unchanged, including trailers. Header delivery does not end
/// launch activity: only EOF/cancellation drops this body's lifetime pin.
struct LaunchBody {
    inner: axum::body::Body,
    pin: Option<Arc<crate::launch_tokens::LaunchPin>>,
}

impl hyper::body::Body for LaunchBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        let result = hyper::body::Body::poll_frame(std::pin::Pin::new(&mut self.inner), cx);
        if matches!(&result, std::task::Poll::Ready(None | Some(Err(_)))) {
            self.pin.take();
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        hyper::body::Body::is_end_stream(&self.inner)
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        hyper::body::Body::size_hint(&self.inner)
    }
}

pub(super) fn pin_body(
    mut response: Response,
    pin: Option<Arc<crate::launch_tokens::LaunchPin>>,
) -> Response {
    if let Some(pin) = pin {
        let inner = std::mem::replace(response.body_mut(), axum::body::Body::empty());
        *response.body_mut() = axum::body::Body::new(LaunchBody {
            inner,
            pin: Some(pin),
        });
    }
    response
}

/// Only the ticket owner constructs this. A waiter owns only `Notified`.
struct CatalogRefreshOwner<'a> {
    guard: Option<crate::catalog::RefreshGuard>,
    changed: &'a tokio::sync::Notify,
}

impl<'a> CatalogRefreshOwner<'a> {
    fn new(guard: crate::catalog::RefreshGuard, changed: &'a tokio::sync::Notify) -> Self {
        Self {
            guard: Some(guard),
            changed,
        }
    }

    fn finish(
        mut self,
        reply: Result<serde_json::Value, crate::catalog::RefreshFailure>,
    ) -> Result<(), crate::catalog::CatalogError> {
        // finish consumes the guard and releases its cache lock before self Drop.
        self.guard.take().expect("refresh owner").finish(reply)
    }
}

impl Drop for CatalogRefreshOwner<'_> {
    fn drop(&mut self) {
        // Cancellation/unwind must clear the ticket before waking its waiters.
        drop(self.guard.take());
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
mod operation_catalog_owner_tests {
    use super::*;
    use crate::catalog::{CatalogCache, Observation};
    use std::future::Future;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct ObserveWake {
        cache: CatalogCache,
        woke: AtomicBool,
        saw_refreshing: AtomicBool,
    }

    impl std::task::Wake for ObserveWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.saw_refreshing
                .store(self.cache.view("a").unwrap().refreshing, Ordering::SeqCst);
            self.woke.store(true, Ordering::SeqCst);
        }
    }

    fn ready<F: Future>(mut future: std::pin::Pin<&mut F>) -> bool {
        let waker = std::task::Waker::noop();
        future
            .as_mut()
            .poll(&mut std::task::Context::from_waker(waker))
            .is_ready()
    }

    #[tokio::test]
    async fn owner_drop_clears_ticket_before_notifying_registered_waiter() {
        let cache = CatalogCache::default();
        let changed = tokio::sync::Notify::new();
        let owner = CatalogRefreshOwner::new(
            cache
                .begin_refresh("a", RefreshMode::Force)
                .unwrap()
                .unwrap(),
            &changed,
        );
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable(); // same ordering as launch_catalog
        let observer = Arc::new(ObserveWake {
            cache: cache.clone(),
            woke: AtomicBool::new(false),
            saw_refreshing: AtomicBool::new(true),
        });
        let waker = std::task::Waker::from(observer.clone());
        assert!(
            notified
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );
        assert!(cache.view("a").unwrap().refreshing);
        drop(owner);
        assert!(observer.woke.load(Ordering::SeqCst));
        assert!(!observer.saw_refreshing.load(Ordering::SeqCst));
        assert!(ready(notified.as_mut()));
        let view = cache.view("a").unwrap();
        assert!(!view.refreshing);
        assert!(matches!(
            view.observation,
            Observation::RefreshFailed {
                reason: RefreshFailure::Cancelled,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn waiter_drop_does_not_cancel_owners_ticket_or_notify() {
        let cache = CatalogCache::default();
        let changed = tokio::sync::Notify::new();
        let owner = CatalogRefreshOwner::new(
            cache
                .begin_refresh("a", RefreshMode::Force)
                .unwrap()
                .unwrap(),
            &changed,
        );
        let mut waiter = Box::pin(changed.notified());
        waiter.as_mut().enable();
        assert!(!ready(waiter.as_mut()));
        drop(waiter);
        assert!(cache.view("a").unwrap().refreshing);
        assert!(matches!(
            cache.begin_refresh("a", RefreshMode::Force),
            Err(CatalogError::RefreshInFlight)
        ));
        drop(owner);
        assert!(!cache.view("a").unwrap().refreshing);
    }

    #[tokio::test]
    async fn finish_error_and_unwind_both_clear_then_notify() {
        for mode in 0..3 {
            let cache = CatalogCache::default();
            let changed = tokio::sync::Notify::new();
            let owner = CatalogRefreshOwner::new(
                cache
                    .begin_refresh("a", RefreshMode::Force)
                    .unwrap()
                    .unwrap(),
                &changed,
            );
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if mode == 0 {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                        let _owner = owner;
                        panic!("synthetic owner unwind");
                    }))
                    .is_err()
                );
            } else if mode == 1 {
                owner.finish(Err(RefreshFailure::Transport)).unwrap();
            } else {
                owner.finish(Ok(serde_json::json!({"models":[]}))).unwrap();
            }
            assert!(ready(notified.as_mut()));
            assert!(!cache.view("a").unwrap().refreshing);
            if mode == 2 {
                assert!(matches!(
                    cache.view("a").unwrap().observation,
                    Observation::Known(_)
                ));
            }
        }
    }

    struct OutputOwner {
        pending: Option<PendingIssue>,
        dropped: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Drop for OutputOwner {
        fn drop(&mut self) {
            drop(self.pending.take());
            let _ = self.dropped.take().unwrap().send(());
        }
    }

    #[tokio::test]
    async fn cancellation_during_admitted_blocking_work_keeps_token_with_task() {
        let registry = crate::launch_tokens::LaunchTokenRegistry::default();
        let operation = crate::issue_operation::IssueOperation::new(
            tokio::time::Instant::now() + Duration::from_secs(3600),
        );
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (dropped, dropped_rx) = tokio::sync::oneshot::channel();
        let task_registry = registry.clone();
        let task_operation = operation.clone();
        let mut caller = Box::pin(async move {
            let _caller = task_operation.caller_guard();
            tokio::task::spawn_blocking(move || {
                let pending = PendingIssue::new(task_registry, "launch:operation-pin").unwrap();
                task_operation.admit().unwrap();
                entered
                    .send(pending.token.expose_secret().to_owned())
                    .unwrap();
                release_rx.recv().unwrap();
                assert!(task_operation.check().is_ok());
                OutputOwner {
                    pending: Some(pending),
                    dropped: Some(dropped),
                }
            })
            .await
            .unwrap()
        });
        assert!(!ready(caller.as_mut()));
        let handle = entered_rx.await.unwrap();
        drop(caller);
        assert!(operation.check().is_ok()); // admission already won
        drop(registry.get_or_pin(&handle).unwrap());
        release.send(()).unwrap();
        dropped_rx.await.unwrap();
        assert!(registry.get_or_pin(&handle).is_err());
    }

    #[tokio::test]
    async fn ready_unconsumed_blocking_output_discards_unpublished_token() {
        let registry = crate::launch_tokens::LaunchTokenRegistry::default();
        let pending = PendingIssue::new(registry.clone(), "launch:operation-ready").unwrap();
        let handle = pending.token.expose_secret().to_owned();
        let (dropped, dropped_rx) = tokio::sync::oneshot::channel();
        let join = tokio::task::spawn_blocking(move || OutputOwner {
            pending: Some(pending),
            dropped: Some(dropped),
        });
        while !join.is_finished() {
            tokio::task::yield_now().await;
        }
        drop(join); // never poll the ready JoinHandle's output
        dropped_rx.await.unwrap();
        assert!(registry.get_or_pin(&handle).is_err());
    }
}

impl RelayState {
    /// D22 issuance only. The authenticated caller supplies the actual data port
    /// and the launcher-resolved effective model; no config is read here.
    pub async fn issue_launch(
        &self,
        request: LaunchRequest,
        data_port: u16,
    ) -> Result<LaunchReply, LaunchError> {
        // Direct/internal callers get the same guard; HTTP uses route entry.
        let operation = crate::issue_operation::IssueOperation::new(
            tokio::time::Instant::now() + ISSUE_CATALOG_BUDGET,
        );
        let _caller = operation.caller_guard();
        self.issue_launch_for_operation(request, data_port, operation)
            .await
    }

    pub(crate) async fn issue_launch_for_operation(
        &self,
        request: LaunchRequest,
        data_port: u16,
        operation: crate::issue_operation::IssueOperation,
    ) -> Result<LaunchReply, LaunchError> {
        // Guard also makes direct cancellation of this future observable.
        let caller = operation.caller_guard();
        let result = tokio::time::timeout_at(
            operation.deadline(),
            self.issue_launch_with_preference(request, data_port, operation.clone()),
        )
        .await
        .map_err(|_| LaunchError::Unavailable)?;
        caller.disarm();
        result
    }

    async fn issue_launch_with_preference(
        &self,
        request: LaunchRequest,
        data_port: u16,
        operation: crate::issue_operation::IssueOperation,
    ) -> Result<LaunchReply, LaunchError> {
        operation.check().map_err(|_| LaunchError::Unavailable)?;
        validate_issue_request(&request, data_port)?;
        if self.is_draining() {
            return Err(LaunchError::Unavailable);
        }
        let resume_key = request.resume_id.as_ref().map(|id| format!("codex:{id}"));
        // launch_info understands canonical aliases; current() alone does not.
        // This is a metadata observation, never adoption or placement.
        let prior_table = self.bindings.clone();
        let prior_key = resume_key.clone();
        let prior_operation = operation.clone();
        let prior = tokio::task::spawn_blocking(move || {
            prior_operation
                .check()
                .map_err(|_| LaunchError::Unavailable)?;
            Ok::<_, LaunchError>(
                prior_key
                    .as_deref()
                    .and_then(|key| prior_table.launch_info(key, chrono::Utc::now()).ok()),
            )
        })
        .await
        .map_err(|_| LaunchError::Store)??;
        operation.check().map_err(|_| LaunchError::Unavailable)?;
        if request.model.is_none()
            && prior
                .as_ref()
                .and_then(|info| info.current_model.as_ref())
                .is_none()
        {
            return Err(LaunchError::Invalid);
        }
        let store = self.store.clone();
        let list_operation = operation.clone();
        let accounts = tokio::task::spawn_blocking(move || {
            list_operation
                .check()
                .map_err(|_| LaunchError::Unavailable)?;
            store.list().map_err(|_| LaunchError::Store)
        })
        .await
        .map_err(|_| LaunchError::Store)??;
        operation.check().map_err(|_| LaunchError::Unavailable)?;
        let preference = resolve_issue_preference(
            request.account_pref.as_deref(),
            accounts
                .iter()
                .filter(|a| a.provider == Provider::Codex)
                .map(|a| (a.id.as_str(), a.label.as_str())),
        )?;
        let mut candidates = self
            .candidates(
                Provider::Codex,
                resume_key.as_deref(),
                request.model.as_deref(),
                &BTreeSet::new(),
            )
            .await
            .map_err(|_| LaunchError::Store)?;
        // Fetch without any binding/policy/registry lock. Unknown != empty.
        // Stale evidence is allowed only for the already bound account.
        let mut ids: BTreeSet<String> = candidates
            .iter()
            .filter(|c| c.account.eligible(None))
            .map(|c| c.account.id.clone())
            .collect();
        let bound_usable = prior.as_ref().is_some_and(|info| {
            candidates.iter().any(|candidate| {
                candidate.account.id == info.account
                    && candidate.account.eligible(info.current_model.as_deref())
            })
        });
        if let Some(info) = prior.as_ref().filter(|_| !bound_usable) {
            ids.remove(&info.account);
        }
        if let Some(info) = prior.as_ref().filter(|_| bound_usable) {
            ids.insert(info.account.clone());
        }
        // One shared budget includes every queued catalog, not eight seconds
        // per wave. Fail the whole issuance before placement on exhaustion.
        let deadline = operation.deadline();
        let explicit_model = request
            .model
            .as_deref()
            .filter(|_| request.model_is_explicit);
        let refresh_missing_catalog = prior.is_some() && !bound_usable;
        let entitlements: crate::bindings::LaunchEntitlements = collect_issue_catalogs(
            ids,
            prior
                .as_ref()
                .filter(|_| bound_usable)
                .map(|info| info.account.as_str()),
            deadline,
            |id, existing| async move {
                let mut catalog = self
                    .launch_catalog(&id, false, existing)
                    .await
                    .map_err(|_| ())?;
                // A cached menu predating a new model is not a current denial.
                // Refresh only a missing explicit pick, inside the SAME issue
                // budget; this never bypasses destination admission or D30.
                if refresh_missing_catalog
                    && explicit_model.is_some_and(|model| !catalog.supported().contains(model))
                {
                    catalog = self
                        .launch_catalog(&id, true, existing)
                        .await
                        .map_err(|_| ())?;
                }
                Ok(catalog.supported().clone())
            },
        )
        .await?
        .into_iter()
        .collect();
        let (refreshed, sources, observed, store_evidence, runtime_evidence) = self
            .candidates_with_evidence(
                Provider::Codex,
                resume_key.as_deref(),
                request.model.as_deref(),
                &BTreeSet::new(),
            )
            .await
            .map_err(|_| LaunchError::Store)?;
        candidates = refreshed;
        // Even a model-less selection must have a known catalog. Keep the bound
        // account for authoritative resume handling, never silently force it away.
        candidates.retain(|c| {
            entitlements.contains_key(&c.account.id)
                || prior
                    .as_ref()
                    .is_some_and(|info| info.account == c.account.id)
        });
        let table = self.bindings.clone();
        let registry = self.launch_tokens.clone();
        let obs = self.observability.clone();
        if self.is_draining() {
            return Err(LaunchError::Unavailable);
        }
        operation.check().map_err(|_| LaunchError::Unavailable)?;
        let task_operation = operation.clone();
        let runtime = self.accounts.clone();
        let store = self.store.clone();
        let prior_model = prior.as_ref().and_then(|p| p.current_model.clone());
        // Match placement: an implicit resume retains its proved current model.
        let admission_model = if prior.is_some() && !request.model_is_explicit {
            prior_model.or_else(|| request.model.clone())
        } else {
            request.model.clone().or(prior_model)
        };
        let (reply, mut pending) =
            tokio::task::spawn_blocking(move || -> Result<_, LaunchError> {
                runtime
                    .with_selection_admission_models(&table, Provider::Codex, |eligible| {
                        let operation = task_operation;
                        operation.check().map_err(|_| LaunchError::Unavailable)?;
                        // Preserve the original's current denial for model/consent
                        // classification. The transaction is already held; load()
                        // would reacquire it. Final selected-account admission below
                        // prevents this classification row authorizing dispatch.
                        if let Some(info) = &prior {
                            let candidate = candidates
                                .iter_mut()
                                .find(|candidate| candidate.account.id == info.account)
                                .ok_or(LaunchError::Unavailable)?;
                            candidate.account = store
                                .read_account_unlocked(&info.account)
                                .map_err(|_| LaunchError::Store)?;
                            candidate.account.expire_health(chrono::Utc::now());
                        }
                        candidates.retain(|candidate| {
                            prior
                                .as_ref()
                                .is_some_and(|info| info.account == candidate.account.id)
                                || eligible(
                                    &candidate.account.id,
                                    false,
                                    admission_model.as_deref(),
                                )
                        });
                        // Bound model/account changes while awaiting catalogs require a retry.
                        // Placement itself still re-resolves canonical identity under its gate.
                        let current = resume_key
                            .as_deref()
                            .and_then(|key| table.launch_info(key, chrono::Utc::now()).ok());
                        if current != prior {
                            return Err(LaunchError::Unavailable);
                        }
                        operation.check().map_err(|_| LaunchError::Unavailable)?;
                        let anchor = issue_anchor()?;
                        // Allocate before durable policy/binding mutation. Empty means no
                        // menu advertised yet; /models owns acceptance and metadata recording.
                        let pending = PendingIssue::new(registry, &anchor)?;
                        let placement = table
                            .place_launch_for_issue_evidence(
                                crate::bindings::LaunchRequest {
                                    anchor_key: &anchor,
                                    resume_key: resume_key.as_deref(),
                                    model: request.model.as_deref(),
                                    model_is_explicit: request.model_is_explicit,
                                    account_pref: preference.as_deref(),
                                    allow_move_for_model: request.move_for_model,
                                },
                                &candidates,
                                &sources,
                                crate::bindings::MoveObservations::same(observed),
                                store_evidence,
                                runtime_evidence,
                                &entitlements,
                                chrono::Utc::now(),
                                Some(&operation),
                                Some(eligible),
                                |policies, consent| {
                                    policies
                                        .authorize_model_move_for_issue(consent, Some(&operation))
                                },
                            )
                            .map_err(issue_binding_error)?;
                        if request.urgent {
                            table
                                .set_routing_session_urgent(
                                    &crate::routing::session_id(&placement.canonical_key),
                                    crate::routing::On5hLimit::Move,
                                )
                                .map_err(|_| LaunchError::Store)?;
                        }
                        for event in &placement.moves {
                            if event.is_placement {
                                obs.record_placement(event);
                            }
                            obs.record_group_move(
                                &event.key,
                                event.from.as_deref(),
                                &event.to,
                                &event.reason,
                                event.bytes,
                                event.keys,
                                event.debug_pin,
                            );
                            // Checkpoint ledger filters out model_choice/launch_model, but
                            // this event and record_group_move's panel update preserve why.
                            tracing::info!(account = %event.to, reason = %event.reason,
                    "launch account placement");
                        }
                        let base_url = format!(
                            "http://127.0.0.1:{data_port}/l/{}/v1",
                            pending.token.expose_secret()
                        );
                        pending.registry.bind_activity(&pending.token, &table);
                        let reply = LaunchReply {
                            catalog_url: format!("{base_url}/models"),
                            base_url,
                            account: placement.account,
                            model: placement.effective_model,
                        };
                        // Returning the guard keeps cancellation cleanup alive in the task
                        // output too. Never attempt to undo a durable policy/binding move.
                        Ok((reply, pending))
                    })
                    .map_err(|_| LaunchError::Store)?
            })
            .await
            .map_err(|_| LaunchError::Store)??;
        if !operation.can_reply() {
            return Err(LaunchError::Unavailable);
        }
        pending.published = true;
        Ok(reply)
    }

    /// Failed refreshes never manufacture an empty entitlement set. Existing
    /// launches may retain a bounded previous menu; new placements fail closed.
    pub(crate) async fn launch_catalog(
        &self,
        id: &str,
        force: bool,
        existing: bool,
    ) -> Result<Arc<KnownCatalog>, ()> {
        let deadline = tokio::time::Instant::now() + CATALOG_DEADLINE;
        loop {
            let notified = self.catalog_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.catalogs.begin_refresh(
                id,
                if force {
                    RefreshMode::Force
                } else {
                    RefreshMode::IfDue
                },
            ) {
                Ok(Some(guard)) => {
                    let owner = CatalogRefreshOwner::new(guard, &self.catalog_changed);
                    let result = tokio::time::timeout_at(deadline, self.fetch_launch_catalog(id))
                        .await
                        .unwrap_or(Err(RefreshFailure::Transport));
                    let _ = owner.finish(result);
                    break;
                }
                Ok(None) => break,
                Err(CatalogError::RefreshInFlight) => {
                    tokio::time::timeout_at(deadline, notified)
                        .await
                        .map_err(|_| ())?;
                    if self.catalogs.view(id).map_err(|_| ())?.refreshing {
                        continue;
                    }
                    // The completed singleflight satisfies this caller's force
                    // request; do not immediately issue another forced fetch.
                    break;
                }
                Err(_) => return Err(()),
            }
        }
        match self.catalogs.view(id).map_err(|_| ())?.observation {
            Observation::Known(catalog) => Ok(catalog),
            Observation::RefreshFailed {
                previous: Some(catalog),
                ..
            } if existing && catalog.observed_at().elapsed() < BOUND_STALE_LIMIT => Ok(catalog),
            _ => Err(()),
        }
    }

    async fn fetch_launch_catalog(&self, id: &str) -> Result<serde_json::Value, RefreshFailure> {
        let _slot = self
            .catalog_slots
            .acquire()
            .await
            .map_err(|_| RefreshFailure::Cancelled)?;
        if self.is_draining() {
            return Err(RefreshFailure::Cancelled);
        }
        #[cfg(any(test, all(feature = "testkit", debug_assertions)))]
        if !self.codex_base.starts_with("http://127.0.0.1:") {
            return Err(RefreshFailure::Transport);
        }
        let (mut account, mut tokens) = self
            .tokens
            .before_request(
                self.store.clone(),
                self.observability.clone(),
                id.to_owned(),
            )
            .await
            .map_err(|_| RefreshFailure::Unauthorized)?;
        if account.provider != Provider::Codex {
            return Err(RefreshFailure::Unauthorized);
        }
        for attempt in 0..2 {
            if self.is_draining() {
                return Err(RefreshFailure::Cancelled);
            }
            let headers =
                super::headers::upstream(&HeaderMap::new(), &account, &tokens.access_token)
                    .map_err(|_| RefreshFailure::Unauthorized)?;
            let mut response = self
                .client
                .get(format!("{}/models", self.codex_base))
                .query(&[("client_version", "0.163.0")])
                .headers(headers)
                .header("accept", "application/json")
                .header(
                    "user-agent",
                    concat!("quota-baton/", env!("CARGO_PKG_VERSION")),
                )
                .timeout(CATALOG_DEADLINE)
                .send()
                .await
                .map_err(|_| RefreshFailure::Transport)?;
            let status = response.status();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| RefreshFailure::Transport)?
            {
                if body.len().saturating_add(chunk.len()) > crate::catalog::MAX_RAW_BYTES {
                    return Err(RefreshFailure::InvalidCatalog(CatalogError::RawLimit));
                }
                body.extend_from_slice(&chunk);
            }
            if status == StatusCode::UNAUTHORIZED {
                if !credentials::is_linked(&account)
                    && poll_revoked(account.provider, &body)
                    && self
                        .tokens
                        .is_current(self.store.clone(), &account, &tokens)
                        .await
                        .map_err(|_| RefreshFailure::Unauthorized)?
                {
                    self.tokens
                        .revoke_if_current(self.store.clone(), &account, &tokens)
                        .await
                        .map_err(|_| RefreshFailure::Unauthorized)?;
                    return Err(RefreshFailure::Unauthorized);
                }
                if attempt == 0 {
                    if let Some((updated, refreshed)) = self
                        .tokens
                        .after_401(
                            self.store.clone(),
                            self.observability.clone(),
                            account.clone(),
                            &tokens,
                        )
                        .await
                        .map_err(|_| RefreshFailure::Unauthorized)?
                    {
                        account = updated;
                        tokens = refreshed;
                        continue;
                    }
                    self.tokens
                        .cool_unavailable_auth(self.store.clone(), &account, &tokens)
                        .await
                        .map_err(|_| RefreshFailure::Unauthorized)?;
                } else {
                    self.tokens
                        .auth_failed(
                            self.store.clone(),
                            self.observability.clone(),
                            account,
                            &tokens,
                        )
                        .await
                        .map_err(|_| RefreshFailure::Unauthorized)?;
                }
                return Err(RefreshFailure::Unauthorized);
            }
            if !status.is_success() {
                return Err(RefreshFailure::Upstream);
            }
            return serde_json::from_slice(&body)
                .map_err(|_| RefreshFailure::InvalidCatalog(CatalogError::InvalidShape));
        }
        Err(RefreshFailure::Unauthorized)
    }

    pub fn start_catalog_scheduler(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let relay = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = relay.wait_for_drain() => return,
                    _ = tick.tick() => {}
                }
                let store = relay.store.clone();
                let Ok(Ok(accounts)) = tokio::task::spawn_blocking(move || store.list()).await
                else {
                    continue;
                };
                for account in accounts.into_iter().filter(|a| {
                    a.provider == Provider::Codex
                        && a.user_state == UserState::Active
                        && a.eligible(None)
                }) {
                    if relay.is_draining() {
                        return;
                    }
                    let _ = relay.launch_catalog(&account.id, false, true).await;
                }
            }
        })
    }
}

// Bounded resolver/model checks; endpoint callers own native protocol framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DataLaunchError {
    Relaunch,
    Invalid,
    Busy,
    UnsupportedModel,
    Unavailable,
    PolicyBlocked,
    Changed,
}

impl DataLaunchError {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::Relaunch => "launch_expired",
            Self::Invalid => "invalid_launch",
            Self::Busy => "launch_busy",
            Self::UnsupportedModel => "model_not_supported",
            Self::Unavailable => "launch_unavailable",
            Self::PolicyBlocked => "routing_policy_blocked",
            Self::Changed => "launch_binding_changed",
        }
    }

    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Relaunch => "실행 연결이 만료되었습니다. codex-qb를 다시 실행해 주세요.",
            Self::Invalid => "실행 연결의 대화 정보를 확인하지 못했습니다.",
            Self::Busy => {
                "이 대화에 진행 중인 요청이나 연결이 있습니다. 끝난 뒤 다시 실행해 주세요."
            }
            Self::UnsupportedModel => {
                "이 대화의 계정은 요청한 모델을 지원하지 않습니다. 모델을 바꾸거나 --move-for-model로 다시 실행해 주세요."
            }
            Self::Unavailable => {
                "계정의 모델 목록이나 연결을 확인하지 못했습니다. 잠시 후 다시 시도해 주세요."
            }
            Self::PolicyBlocked => "대화의 계정 사용 정책을 확인해 주세요.",
            Self::Changed => "대화의 계정이 변경되었습니다. codex-qb를 다시 실행해 주세요.",
        }
    }

    pub(super) fn status(self) -> StatusCode {
        match self {
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        }
    }
}

fn data_binding_error(error: crate::bindings::LaunchError) -> DataLaunchError {
    use crate::bindings::LaunchError as E;
    match error {
        E::UnknownLaunch => DataLaunchError::Relaunch,
        E::InvalidMetadata | E::AnchorConflict => DataLaunchError::Invalid,
        E::Busy => DataLaunchError::Busy,
        E::ModelUnsupported | E::ConsentRequired => DataLaunchError::UnsupportedModel,
        E::PolicyBlocked => DataLaunchError::PolicyBlocked,
        E::BindingChanged => DataLaunchError::Changed,
        E::NoEligibleAccount | E::EntitlementsUnknown | E::Capacity | E::Retired | E::MoveGuard => {
            DataLaunchError::Unavailable
        }
    }
}

impl RelayState {
    /// Existing C8 binding authority is resolved before optional token authority.
    /// None denotes ordinary unbound legacy traffic, never scoped expiry.
    pub(super) async fn resolve_data_launch(
        &self,
        ingress: Option<&LaunchIngress>,
        request_key: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Result<Option<crate::bindings::LaunchInfo>, DataLaunchError> {
        if self.is_draining() {
            return Err(DataLaunchError::Unavailable);
        }
        let scoped = ingress.is_some();
        let anchor = ingress
            .and_then(|ingress| ingress.pin.as_ref())
            .map(|pin| pin.metadata().binding_anchor_key);
        let key = request_key.map(str::to_owned);
        let table = self.bindings.clone();
        let operation = crate::issue_operation::IssueOperation::new(deadline);
        let caller = operation.caller_guard();
        let task_operation = operation.clone();
        let resolved = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || {
                table.resolve_launch_for_operation(
                    anchor.as_deref(),
                    key.as_deref(),
                    chrono::Utc::now(),
                    Some(&task_operation),
                )
            }),
        )
        .await
        .map_err(|_| DataLaunchError::Unavailable)?
        .map_err(|_| DataLaunchError::Unavailable)?;
        if !operation.can_reply() || self.is_draining() {
            return Err(DataLaunchError::Unavailable);
        }
        caller.disarm();
        match resolved {
            Ok(info) => Ok(Some(info)),
            Err(crate::bindings::LaunchError::UnknownLaunch) if !scoped => Ok(None),
            Err(error) => Err(data_binding_error(error)),
        }
    }

    /// Side metadata only. The original HTTP body / WS frame is never rewritten.
    /// Unknown catalogs do not authorize a model or imply an empty entitlement.
    pub(super) async fn admit_data_model(
        &self,
        info: &crate::bindings::LaunchInfo,
        requested_model: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Result<Option<String>, DataLaunchError> {
        if !info.managed {
            return Ok(requested_model.map(str::to_owned));
        }
        let model = requested_model
            .or(info.current_model.as_deref())
            .filter(|model| crate::catalog::valid_model_id(model))
            .map(str::to_owned)
            .ok_or(DataLaunchError::Invalid)?;
        // Resolve recovery before catalog I/O: an unusable source's refresh
        // can consume the entire WS upgrade deadline before selection runs.
        // Only its proved current model gets this exception; destination
        // entitlement and final dispatch checks remain mandatory.
        let recovering_current = info.current_model.as_deref() == Some(model.as_str())
            && !self
                .dispatch_data_allowed(Some(&info.canonical_key), &info.account, Some(&model))
                .await;
        let mut catalog = if recovering_current {
            None
        } else {
            let observed =
                tokio::time::timeout_at(deadline, self.launch_catalog(&info.account, false, true))
                    .await;
            match observed {
                Ok(Ok(catalog)) => Some(catalog),
                _ if info.current_model.as_deref() == Some(model.as_str())
                    && !self
                        .dispatch_data_allowed(
                            Some(&info.canonical_key),
                            &info.account,
                            Some(&model),
                        )
                        .await =>
                {
                    None
                }
                _ => return Err(DataLaunchError::Unavailable),
            }
        };
        // A proved current model can recover from an unusable source after
        // restart. Destination entitlement and policy gates still run before
        // dispatch; an unproved new pick never gets this exception.
        if let Some(catalog) = catalog.as_mut() {
            if !catalog.supported().contains(&model) {
                // A menu can change between launches. Re-observe only this account;
                // an unsupported pick never shops other accounts or sends the body.
                if let Ok(Ok(updated)) = tokio::time::timeout_at(
                    deadline,
                    self.launch_catalog(&info.account, true, true),
                )
                .await
                {
                    *catalog = updated;
                }
            }
            if !catalog.supported().contains(&model) {
                return Err(DataLaunchError::UnsupportedModel);
            }
        }
        if self.is_draining() {
            return Err(DataLaunchError::Unavailable);
        }
        let table = self.bindings.clone();
        let key = info.canonical_key.clone();
        let account = info.account.clone();
        let accepted_model = model.clone();
        let operation = crate::issue_operation::IssueOperation::new(deadline);
        let caller = operation.caller_guard();
        let task_operation = operation.clone();
        tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || {
                table.record_launch_model_for_operation(
                    &key,
                    &account,
                    &accepted_model,
                    chrono::Utc::now(),
                    Some(&task_operation),
                )
            }),
        )
        .await
        .map_err(|_| DataLaunchError::Unavailable)?
        .map_err(|_| DataLaunchError::Unavailable)?
        .map_err(data_binding_error)?;
        if !operation.can_reply() || self.is_draining() {
            return Err(DataLaunchError::Unavailable);
        }
        caller.disarm();
        Ok(Some(model))
    }
}
