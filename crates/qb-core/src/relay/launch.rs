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
                Box::new(zstd::stream::read::Decoder::new(Cursor::new(prefix)).map_err(|_| ())?)
            }
            "gzip" => Box::new(flate2::read::GzDecoder::new(Cursor::new(prefix))),
            "deflate" => Box::new(flate2::read::ZlibDecoder::new(Cursor::new(prefix))),
            "br" => Box::new(brotli::Decompressor::new(Cursor::new(prefix), 4096)),
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
        .and_then(|model| crate::observability::safe_identifier(&model))
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
}

/// URLs contain a routing handle. Deliberately no Debug or Display.
#[derive(serde::Serialize)]
pub struct LaunchReply {
    pub base_url: String,
    pub catalog_url: String,
    pub account: String,
    pub model: Option<String>,
}

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
        fs::write(
            f.cpa.join("account.json"),
            serde_json::to_vec(&credential(1)).unwrap(),
        )
        .unwrap();
        f.reply(
            StatusCode::UNAUTHORIZED,
            json!({"error":{"code":"invalid_token"}}),
        );
        f.reply(StatusCode::OK, json!({"models":[{"slug":"after-reload"}]}));
        assert!(
            f.relay
                .launch_catalog(&f.id, false, false)
                .await
                .unwrap()
                .supported()
                .contains("after-reload")
        );
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

impl RelayState {
    /// Incomplete D22 checkpoint: issuance stays closed until binding and
    /// durable policy authorization have been integrated and verified.
    pub async fn issue_launch(
        &self,
        _request: LaunchRequest,
        _data_port: u16,
    ) -> Result<LaunchReply, LaunchError> {
        Err(LaunchError::Unavailable)
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
                    let result = tokio::time::timeout_at(deadline, self.fetch_launch_catalog(id))
                        .await
                        .unwrap_or(Err(RefreshFailure::Transport));
                    let _ = guard.finish(result);
                    self.catalog_changed.notify_waiters();
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
