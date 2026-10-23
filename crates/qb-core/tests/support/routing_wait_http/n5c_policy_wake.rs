//! Child of routing_wait_http::entry; all credentials/accounts are synthetic.
use super::*;
use qb_core::routing::{Mode, On5hLimit};
use std::sync::mpsc;

const URGENCY_MARKER: &str = "urgency policy wake must not wait for blocked native inspection";
const KEEP_MARKER: &str =
    "unrelated policy publication must retain the same pending Store inspection";

// An owned real Store transaction, never a scoped join or a test authority hook.
// Drop releases on assertion failure; explicit completion uses a timed channel.
struct HeldStore {
    release: Option<mpsc::SyncSender<()>>,
    finished: mpsc::Receiver<bool>,
    _worker: std::thread::JoinHandle<()>,
}

impl HeldStore {
    fn start(h: &Harness, source: &str) -> Self {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished) = mpsc::sync_channel(1);
        let store = h.store.clone();
        let source = source.to_owned();
        let worker = std::thread::spawn(move || {
            let result = store.update_metadata(&source, |_account| {
                // The native pre-mutation hook already revoked the receipt and
                // notified renewal; Store remains held until explicit release.
                let _ = entered_tx.send(());
                release_rx
                    .recv_timeout(Duration::from_secs(25))
                    .expect("bounded synthetic Store lease abandoned");
            });
            let _ = finished_tx.send(result.is_ok());
        });
        let held = Self {
            release: Some(release_tx),
            finished,
            _worker: worker,
        };
        entered_rx
            .recv_timeout(PHASE)
            .expect("native metadata prehook must run while holding Store");
        held
    }

    fn assert_held(&self) {
        assert!(matches!(
            self.finished.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    fn finish(mut self) {
        self.release.take().unwrap().send(()).unwrap();
        assert!(
            self.finished
                .recv_timeout(PHASE)
                .expect("owned native Store worker must finish after release")
        );
        // No unbounded join: the completion send is the worker's final action.
    }
}

impl Drop for HeldStore {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.try_send(());
        }
    }
}

async fn urgency(h: &Harness, session: &str, value: &str) {
    timeout(
        PHASE,
        h.admin(
            reqwest::Method::POST,
            "/admin/v1/routing",
            Some(json!({
                "kind":"urgent",
                "id":qb_core::routing::session_id(&format!("codex:{session}")),
                "on_5h_limit":value
            })),
        ),
    )
    .await
    .expect("authenticated urgency publication must finish independently of Store");
}

async fn strict_fixture(
    session: &str,
) -> (
    Harness,
    String,
    String,
    ScriptedUpstream,
    chrono::DateTime<Utc>,
) {
    let (h, source, alternate, usage) = entry_fixture(session, 60).await;
    timeout(
        PHASE,
        h.admin(
            reqwest::Method::POST,
            "/admin/v1/routing",
            Some(json!({
                "kind":"session",
                "id":qb_core::routing::session_id(&format!("codex:{session}")),
                "settings":{"mode":"strict", "reset_wait_secs":60,
                    "on_5h_limit":"wait"}
            })),
        ),
    )
    .await
    .unwrap();
    let plan = h
        .state
        .bindings
        .policies
        .plan(Some(&format!("codex:{session}")));
    assert_eq!(plan.settings.mode, Mode::Strict);
    assert_eq!(plan.settings.on_5h_limit, On5hLimit::Wait);
    assert_eq!(plan.anchor.as_deref(), Some(source.as_str()));
    // B is eligible: Strict, rather than an artificially paused alternate,
    // must prevent urgency from turning into cross-account dispatch authority.
    let b = h.store.load(&alternate).unwrap();
    assert_eq!(b.user_state, qb_core::accounts::UserState::Active);
    assert!(matches!(b.health, Health::Ready));
    let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 20, 0).unwrap();
    full_poll_before_request(&h, &usage, &source, reset).await;
    assert!(reset - Utc::now() > ChronoDuration::seconds(15));
    (h, source, alternate, usage, reset)
}

async fn blocked_renewal(unrelated_first: bool) {
    let session = if unrelated_first {
        "synthetic-n5c-unrelated-blocked-policy"
    } else {
        "synthetic-n5c-urgent-blocked-policy"
    };
    let (h, source, alternate, usage, reset) = strict_fixture(session).await;
    let key = format!("codex:{session}");
    let initial = h.state.bindings.policies.plan(Some(&key));
    let mut events = h.state.subscribe_http_quota_wait();
    let mut phases = h.state.subscribe_http_quota_wait_inspections();
    let headers = [("session-id", session)];
    let send = h.send("/v1/responses", &headers, UPLOAD);
    let interrupt = async {
        event(
            &mut events,
            "admitted",
            &source,
            PHASE,
            "real full poll must admit Wait",
        )
        .await;
        event(
            &mut phases,
            "inspection_started",
            &source,
            PHASE,
            "first native inspection",
        )
        .await;
        assert_eq!(
            h.fake.state.requests().len(),
            1,
            "initial wait must precede target send"
        );
        let held = HeldStore::start(&h, &source);
        event(
            &mut phases,
            "inspection_started",
            &source,
            PHASE,
            "native prehook must queue exactly one consuming renewal",
        )
        .await;
        held.assert_held();

        if unrelated_first {
            // Two causally acknowledged publications catch an outer-loop
            // restart after the first acknowledgement as an extra start event.
            for name in ["synthetic-unrelated-one", "synthetic-unrelated-two"] {
                timeout(
                    PHASE,
                    h.admin(
                        reqwest::Method::POST,
                        "/admin/v1/routing",
                        Some(json!({"kind":"project", "name":name,
                        "settings":{"mode":"recover", "reset_wait_secs":1,
                            "on_5h_limit":"move"}})),
                    ),
                )
                .await
                .unwrap();
                event(&mut phases, "policy_unchanged", &source, PHASE, KEEP_MARKER).await;
                held.assert_held();
                assert_no_more_events(&mut events);
                assert_eq!(h.fake.state.requests().len(), 1);
                let current = h.state.bindings.policies.plan(Some(&key));
                assert!(current.revision > initial.revision);
                assert_eq!(current.settings, initial.settings);
                assert_eq!(current.anchor, initial.anchor);
                assert_eq!(current.project, initial.project);
                assert_eq!(current.source, initial.source);
            }
        }

        urgency(&h, session, "move").await;
        // This is the blocking-select's synchronous return branch, NOT the
        // existing `reselect` event (which is deliberately Reset-only).
        event(
            &mut phases,
            "policy_revoked",
            &source,
            PHASE,
            URGENCY_MARKER,
        )
        .await;
        held.assert_held();
        assert!(
            Utc::now() < reset,
            "policy wake must precede original immutable reset"
        );
        assert_no_more_events(&mut events); // no second admission or Reset hint
        assert_no_more_events(&mut phases); // no third Store job
        assert_eq!(h.fake.state.requests().len(), 1);
        let current = h.state.bindings.policies.plan(Some(&key));
        assert_eq!(current.settings.on_5h_limit, On5hLimit::Move);
        assert_eq!(current.settings.mode, Mode::Strict);
        assert_eq!(current.settings.reset_wait_secs, 60);
        assert_eq!(current.anchor, initial.anchor);
        // Native selection is allowed to need Store. Do not await the HTTP
        // response until the real lease and its abandoned inspection can end.
        held.finish();
    };
    let ((status, _account, body), ()) = timeout(Duration::from_secs(15), async {
        tokio::join!(send, interrupt)
    })
    .await
    .expect("bounded native HTTP urgency fixture");
    assert_eq!(status, 409, "urgency cannot override native Strict");
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["type"], "routing_policy_blocked");
    assert!(Utc::now() < reset);
    timeout(PHASE, idle(&h, &source)).await.unwrap();
    assert_exact_attempts(&h, 1); // warm A only, no target A/B send or replay
    assert_eq!(usage.state.requests().len(), 1);
    assert_eq!(h.state.accounts.inflight_count(&alternate), 0);
    assert_eq!(h.state.bindings.pending_count(&alternate), 0);
    assert_eq!(
        h.state.bindings.current(Some(&key)).as_deref(),
        Some(source.as_str())
    );
    assert_no_more_events(&mut events);
    assert_no_more_events(&mut phases);
}

#[tokio::test]
async fn n5c_http_move_interrupts_store_blocked_admitted_renewal_without_overriding_strict() {
    blocked_renewal(false).await;
}

#[tokio::test]
async fn n5c_http_unrelated_publications_keep_one_pending_store_job_then_move_interrupts() {
    blocked_renewal(true).await;
}

#[tokio::test]
async fn n5c_http_move_before_first_admission_refuses_without_wait_hint() {
    let session = "synthetic-n5c-initial-move";
    let (h, source, alternate, _usage, reset) = strict_fixture(session).await;
    urgency(&h, session, "move").await;
    let mut events = h.state.subscribe_http_quota_wait();
    let mut phases = h.state.subscribe_http_quota_wait_inspections();
    let headers = [("session-id", session)];
    let send = h.send("/v1/responses", &headers, UPLOAD);
    let inspect = async {
        event(
            &mut phases,
            "inspection_started",
            &source,
            PHASE,
            "real initial inspection",
        )
        .await;
        event(
            &mut phases,
            "initial_refused",
            &source,
            PHASE,
            "Move must refuse the genuine full-proof first admission without a hint",
        )
        .await;
        assert_no_more_events(&mut events);
        assert_eq!(h.fake.state.requests().len(), 1);
        // None leaves legacy/native hold behavior intact. Explicitly zero the
        // native budget for bounded cleanup; do NOT claim Move bypasses D25/C7.
        timeout(PHASE, h.admin(reqwest::Method::POST, "/admin/v1/routing",
            Some(json!({"kind":"session", "id":qb_core::routing::session_id(&format!("codex:{session}")),
                "settings":{"mode":"strict", "reset_wait_secs":0}}))))
            .await.unwrap();
    };
    let ((status, _account, body), ()) = timeout(Duration::from_secs(12), async {
        tokio::join!(send, inspect)
    })
    .await
    .expect("bounded initial Move refusal and native cleanup");
    assert_eq!(status, 409);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["type"], "routing_policy_blocked");
    assert!(Utc::now() < reset);
    timeout(PHASE, idle(&h, &source)).await.unwrap();
    assert_exact_attempts(&h, 1);
    assert_eq!(h.state.accounts.inflight_count(&alternate), 0);
    assert_eq!(h.state.bindings.pending_count(&alternate), 0);
    let plan = h
        .state
        .bindings
        .policies
        .plan(Some(&format!("codex:{session}")));
    assert_eq!(
        plan.settings.on_5h_limit,
        On5hLimit::Move,
        "omitted edit must preserve urgency"
    );
    assert_eq!(plan.anchor.as_deref(), Some(source.as_str()));
    assert_no_more_events(&mut events);
    assert_no_more_events(&mut phases);
}

#[path = "n5c_initial_policy_race.rs"]
mod n5c_initial_policy_race;

#[path = "n5c_urgency_behavior.rs"]
mod n5c_urgency_behavior;
