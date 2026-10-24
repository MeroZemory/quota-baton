//! These are actual HTTP relay tests, not direct calls to the new pending API.
//! Expected production seam: observe_http_429_headers_pending applies native
//! headers immediately; finish_http_429_report cuts/applies/mints only after the
//! bounded complete body is classified and reconciled. The empty-body positive
//! reconciles Rate -> Quota with signal None, preserving the whole Account.
//! Known matching quota JSON composes the sealed raw header evidence with the
//! actual native body hardening, without changing either native transition.
//! They intentionally fail
//! against the earlier header-time mint/unconditional quota invalidation path.
//!
//! Fixture follows relay.rs CPA import/RelayState/spawn_with_routes setup. An
//! explicit Axum body gate replaces ScriptedResponse.chunks' elapsed delay so
//! slow persistence cannot accidentally let the body complete before assertions.
//! No test-only production hook or synthetic proof construction is used.
//! Scope excludes truncated transport errors/oversized held bodies, failed
//! release persistence, WS, and complete N5a admission/cancellation acceptance.

use super::*;
use crate::{
    accounts::{Health, HealthReason, Provider},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
    routing::Mode,
};
use axum::{Router, body::Body, http::StatusCode, response::Response};
use bytes::Bytes;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

const BOUND: Duration = Duration::from_secs(5);
const KEY: &str = "synthetic-pending-http-key";

struct HttpFixture {
    // Drop server handles before the temporary home, including on assertion panic.
    servers: RunningServers,
    upstream: JoinHandle<()>,
    state: Arc<RelayState>,
    store: Arc<AccountStore>,
    calls: Arc<AtomicUsize>,
    release: Option<oneshot::Sender<()>>,
    id: String,
    reset: DateTime<Utc>,
    weekly_reset: DateTime<Utc>,
    _home: tempfile::TempDir,
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.upstream.abort();
        self.servers.shutdown();
    }
}

impl HttpFixture {
    async fn new(body: impl FnOnce(i64) -> String) -> (Self, String) {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r13-part-c/http-owner-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let cpa = root.path().join("cpa");
        std::fs::create_dir_all(&cpa).unwrap();
        std::fs::write(
            cpa.join("synthetic.json"),
            serde_json::to_vec(&json!({
                "type":"codex", "email":"pending-http@example.invalid",
                "account_id":"synthetic-pending-http", "plan_type":"plus",
                "access_token":"synthetic-current-access",
                "refresh_token":"synthetic-current-refresh",
                "expired":"2099-01-01T00:00:00Z",
                "last_refresh":Utc::now().to_rfc3339()
            }))
            .unwrap(),
        )
        .unwrap();
        let store = Arc::new(AccountStore::acquire(&root.path().join("home")).unwrap());
        crate::platform::seed_account_fixtures(
            &store,
            &cpa,
            &["codex:pending-http@example.invalid".into()],
            false,
        )
        .unwrap();
        let id = store.list().unwrap().remove(0).id;
        let reset = DateTime::from_timestamp(Utc::now().timestamp() + 3600, 0).unwrap();
        let weekly_reset = reset + chrono::Duration::hours(47);
        let raw_body = body(reset.timestamp());
        let (release, gate) = oneshot::channel();
        let gate = Arc::new(Mutex::new(Some(gate)));
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let upstream_body = raw_body.clone();
        let app = Router::new().fallback(move || {
            let gate = gate.lock().unwrap().take();
            let body = upstream_body.clone();
            counted.fetch_add(1, Ordering::Relaxed);
            async move {
                let Some(gate) = gate else {
                    return Response::builder()
                        .status(500)
                        .body(Body::from("unexpected synthetic retry"))
                        .unwrap();
                };
                let stream = futures_util::stream::once(async move {
                    tokio::time::timeout(BOUND, gate)
                        .await
                        .map_err(|_| std::io::Error::other("synthetic body gate timed out"))?
                        .map_err(|_| std::io::Error::other("synthetic body gate abandoned"))?;
                    Ok::<Bytes, std::io::Error>(Bytes::from(body))
                });
                Response::builder()
                    .status(429)
                    .header("content-type", "application/json")
                    .header("x-codex-primary-window-minutes", "300")
                    .header("x-codex-primary-used-percent", "100")
                    .header("x-codex-primary-reset-at", reset.timestamp().to_string())
                    .header("x-codex-secondary-window-minutes", "10080")
                    .header("x-codex-secondary-used-percent", "10")
                    .header(
                        "x-codex-secondary-reset-at",
                        weekly_reset.timestamp().to_string(),
                    )
                    .header("x-codex-active-limit", "5h")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        });
        let listener = tokio::time::timeout(BOUND, TcpListener::bind("127.0.0.1:0"))
            .await
            .unwrap()
            .unwrap();
        let address = listener.local_addr().unwrap();
        assert!(address.ip().is_loopback());
        let upstream = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        // Exercise native body relay without a replayable upload: Strict's
        // retry/failover path intentionally replaces quota replies with 409.
        // Wait admission and replay behavior are separate acceptance tests.
        config.guard.retain_max_per_request_mib = 0;
        config.routing.request_timeout_secs = 10;
        config.routing.move_guard.trip_seconds = 5;
        config.upstream.codex_base = format!("http://{address}");
        config.upstream.anthropic_base = config.upstream.codex_base.clone();
        crate::routing::PolicyStore::new(store.home(), crate::routing::Settings::default())
            .unwrap()
            .set_session(
                &crate::routing::session_id("codex:synthetic-pending-http"),
                Some(crate::routing::Settings {
                    mode: Mode::Strict,
                    reset_wait_secs: 0,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                None,
            )
            .unwrap();
        let obs = Arc::new(Observability::new(store.home(), None).unwrap());
        let state = Arc::new(
            RelayState::new(store.clone(), &config, obs, Arc::new(DebugPins::default())).unwrap(),
        );
        let servers = tokio::time::timeout(
            BOUND,
            http::spawn_with_routes(
                &config,
                LocalSecrets {
                    client_key: KEY.into(),
                    admin_token: "synthetic-pending-admin".into(),
                },
                relay::router(state.clone()),
                Router::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(servers.data_addr.ip().is_loopback());
        assert!(servers.admin_addr.ip().is_loopback());
        (
            Self {
                servers,
                upstream,
                state,
                store,
                calls,
                release: Some(release),
                id,
                reset,
                weekly_reset,
                _home: root,
            },
            raw_body,
        )
    }

    fn proof_reset(&self) -> Option<DateTime<Utc>> {
        let _gate = self.store.transaction().unwrap();
        self.state
            .accounts
            .wait_owner
            .proofs
            .acquire(&self.id)
            .ok()
            .map(|(_, proof)| proof.reset_at)
    }

    async fn native_headers_applied(&self) {
        tokio::time::timeout(BOUND, async {
            loop {
                let account = self.store.load(&self.id).unwrap();
                if account
                    .quota_cooling
                    .as_ref()
                    .is_some_and(|quota| quota.window == "5h" && quota.reset_at == self.reset)
                {
                    assert!(matches!(account.health, Health::Cooling {
                        reason: HealthReason::Quota, until, ..
                    } if until == self.reset));
                    assert!(
                        account.quota_credit_releasable,
                        "Codex utilization headers establish a releasable native hold"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("native 5h quota headers were not applied while body gated");
    }

    async fn newer_partial_headers(&self) {
        let origin = tokio::time::timeout(BOUND, self.state.accounts.capture_poll_origin(&self.id))
            .await
            .unwrap()
            .unwrap();
        let asof = self.state.bindings.begin_usage_observation_for(&self.id);
        let mut headers = axum::http::HeaderMap::new();
        // Real header parser/report path; deliberately omits the weekly window.
        for (name, value) in [
            ("x-codex-primary-window-minutes", "300".to_owned()),
            ("x-codex-primary-used-percent", "100".to_owned()),
            (
                "x-codex-primary-reset-at",
                self.reset.timestamp().to_string(),
            ),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        tokio::time::timeout(
            BOUND,
            self.state.accounts.observe_headers_asof_origin(
                &self.id,
                Provider::Codex,
                &headers,
                None,
                (&self.state.bindings, asof),
                Some(origin),
            ),
        )
        .await
        .unwrap();
        assert!(
            self.proof_reset().is_none(),
            "newer partial report cannot borrow cached completeness"
        );
    }
}

fn matching_body(reset: i64) -> String {
    format!(
        " {{\"error\":{{\"type\":\"usage_limit_reached\",\"resets_at\":{reset},\"message\":\"synthetic bounded body\"}}}} \n"
    )
}

async fn run_gated_case(
    body: impl FnOnce(i64) -> String,
    newer_partial: bool,
    expected_proof: bool,
    final_until_offset_seconds: i64,
    expected_releasable: bool,
) {
    let (mut fixture, raw_body) = HttpFixture::new(body).await;
    assert!(fixture.reset > Utc::now() + chrono::Duration::seconds(60));
    assert!(fixture.weekly_reset > fixture.reset);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(BOUND)
        .build()
        .unwrap();
    let request = client
        .post(format!("http://{}/v1/responses", fixture.servers.data_addr))
        .bearer_auth(KEY)
        .header("session-id", "synthetic-pending-http")
        .body("{}")
        .send();
    tokio::pin!(request);
    tokio::select! {
        result = &mut request => panic!("relay escaped the gated 429 body: {result:?}"),
        () = fixture.native_headers_applied() => {}
    }
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    assert!(
        fixture.proof_reset().is_none(),
        "native header application must not mint before body classification"
    );
    let header_account = fixture.store.load(&fixture.id).unwrap();
    if newer_partial {
        fixture.newer_partial_headers().await;
    }
    fixture.release.take().unwrap().send(()).unwrap();
    let response = tokio::time::timeout(BOUND, request).await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let bytes = tokio::time::timeout(BOUND, response.bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        bytes.as_ref(),
        raw_body.as_bytes(),
        "native response bytes must remain verbatim"
    );
    tokio::time::timeout(BOUND, async {
        while fixture.state.inflight_requests.load(Ordering::Relaxed) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fixture.proof_reset(),
        expected_proof.then_some(fixture.reset)
    );
    assert_eq!(
        fixture.calls.load(Ordering::Relaxed),
        1,
        "strict mode must not retry/fail over"
    );
    let account = fixture.store.load(&fixture.id).unwrap();
    assert!(matches!(
        account.health,
        Health::Cooling {
            reason: HealthReason::Quota,
            until, ..
        } if until == fixture.reset + chrono::Duration::seconds(final_until_offset_seconds)
    ));
    assert_eq!(
        account.quota_cooling.as_ref().map(|quota| quota.reset_at),
        (final_until_offset_seconds == 0).then_some(fixture.reset),
        "a conflicting longer native hold clears the old short-window evidence"
    );
    assert_eq!(
        account.quota_credit_releasable, expected_releasable,
        "assert native hardening/no-op rather than silently changing proof expectations"
    );
    if expected_releasable {
        assert_eq!(
            account, header_account,
            "empty completed body must preserve the whole native Account"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n5a_http_429_native_headers_precede_body_but_proof_waits_for_empty_completion() {
    run_gated_case(|_| String::new(), false, true, 0, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n5a_http_429_newer_partial_report_while_body_held_fences_pending_mint() {
    run_gated_case(|_| String::new(), true, false, 0, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n5a_http_429_matching_known_body_hardens_native_hold_and_composes_proof() {
    run_gated_case(matching_body, false, true, 0, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n5a_http_429_conflicting_longer_body_reset_preserves_native_change_without_proof() {
    run_gated_case(
        |reset| matching_body(reset + 3600),
        false,
        false,
        3600,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n5a_http_429_malformed_completed_body_declines_only_optional_proof() {
    for body in ["{\"error\":", "not synthetic json"] {
        // These are complete HTTP bodies with invalid JSON, not transport errors.
        // Native Rate -> Quota reconciliation still preserves the header hold.
        run_gated_case(|_| body.to_owned(), false, false, 0, true).await;
    }
}
