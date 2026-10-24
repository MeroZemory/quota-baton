//! Same actual route bytes -> shared response aliases -> exact JSON facts.
//! Separate fixture from sessions/account candidates; synthetic local state only.
#![cfg(debug_assertions)]
use axum::{Router, http::StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use futures_util::FutureExt;
use qb_core::{
    accounts::{AccountStore, Provider},
    config::Config,
    dashboard::{
        AccountId, MoveReason, RequestOutcome, SessionId,
        history::{
            self, ActivityInput, ActivityKind, AttentionCode, CreditsObservation, SettingAction,
            UsageObservation, WindowObservation, WindowScope,
        },
    },
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::RelayState,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    fs,
    net::{Ipv4Addr, TcpListener},
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
type Captured = Arc<Mutex<Vec<Vec<u8>>>>;

async fn capture(
    axum::extract::State(bodies): axum::extract::State<Captured>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 8 * 1024 * 1024).await.unwrap();
    bodies.lock().unwrap().push(bytes.to_vec());
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}
const ADMIN: &str = "sample-acct-history-admin";

struct Harness {
    servers: RunningServers,
    relay: Arc<RelayState>,
    obs: Arc<Observability>,
    store: Arc<AccountStore>,
    root: tempfile::TempDir,
    upstream_trap: TcpListener,
    captured: Captured,
}
impl Harness {
    async fn new() -> Self {
        let base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r15-history-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let home = root.path().join("sample-acct-home");
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        let upstream_trap = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        upstream_trap.set_nonblocking(true).unwrap();
        let mut config = Config::default();
        config.server.admin_port = 0;
        config.server.data_port = 0;
        config.dashboard.resolve_claude_project = false;
        let upstream = format!("http://{}", upstream_trap.local_addr().unwrap());
        config.upstream.codex_base = upstream.clone();
        config.upstream.anthropic_base = upstream.clone();
        config.upstream.codex_usage_base = upstream.clone();
        config.upstream.claude_usage_base = upstream;
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let relay = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap(),
        );
        // No quota/refresh/profile schedulers and no real account import.
        let captured = Captured::default();
        let routes = qb_core::admin::runtime_routes(relay.clone(), 0).layer(
            axum::middleware::from_fn_with_state(captured.clone(), capture),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "sample-acct-history-client".into(),
                admin_token: ADMIN.into(),
            },
            Router::new(),
            routes,
        )
        .await
        .unwrap();
        Self {
            servers,
            relay,
            obs,
            store,
            root,
            upstream_trap,
            captured,
        }
    }
    async fn close(mut self) {
        self.servers.shutdown();
        tokio::time::timeout(Duration::from_secs(5), async {
            (&mut self.servers.data_task).await.unwrap().unwrap();
            (&mut self.servers.admin_task).await.unwrap().unwrap();
        })
        .await
        .expect("owned history listeners stop");
        std::thread::spawn(move || {
            drop(self.servers);
            drop(self.relay);
            drop(self.obs);
            drop(self.store);
            drop(self.root);
            drop(self.upstream_trap);
        })
        .join()
        .expect("history writer closes before synthetic home deletion");
    }
}
async fn same_response<T: DeserializeOwned + Serialize>(
    client: &reqwest::Client,
    harness: &Harness,
    path: &str,
) -> Value {
    let response = client
        .get(format!(
            "http://{}/admin/v1/{path}",
            harness.servers.admin_addr
        ))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.unwrap();
    let original: Value = serde_json::from_slice(&body).unwrap();
    let shared: T = serde_json::from_slice(&body).unwrap();
    let serialized = serde_json::to_vec(&shared).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&serialized).unwrap(),
        original
    );
    assert_eq!(
        serialized.as_slice(),
        &body[..],
        "current response keys/order changed"
    );
    original
}

async fn same_client_response(h: &Harness) {
    let client = qb_tray::client::AdminClient::new(h.servers.admin_addr.port(), ADMIN).unwrap();
    let activity = client.dashboard_activity_raw(7).await.unwrap();
    let original = h.captured.lock().unwrap().last().unwrap().clone();
    assert_eq!(serde_json::to_vec(&activity).unwrap(), original);
    let display = client.dashboard_activity(7).await.unwrap();
    let body = h.captured.lock().unwrap().last().unwrap().clone();
    let previous: qb_tray::dashboard_model::ActivityResponse =
        serde_json::from_slice(&body).unwrap();
    assert_eq!(
        serde_json::to_value(display).unwrap(),
        serde_json::to_value(previous).unwrap()
    );
    let usage = client.dashboard_usage_history_raw().await.unwrap();
    let original = h.captured.lock().unwrap().last().unwrap().clone();
    assert_eq!(serde_json::to_vec(&usage).unwrap(), original);
    let display = client.dashboard_usage_history().await.unwrap();
    let body = h.captured.lock().unwrap().last().unwrap().clone();
    let previous: qb_tray::dashboard_model::UsageHistory = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        serde_json::to_value(display).unwrap(),
        serde_json::to_value(previous).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_history_routes_roundtrip_empty_populated_sparse_and_nullable_facts() {
    let h = Harness::new().await;
    let result=AssertUnwindSafe(async {
        let client=reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
        let activity=same_response::<history::response::ActivityPage>(&client,&h,"activity").await;
        assert!(activity["events"].as_array().unwrap().is_empty());
        let usage=same_response::<history::UsageView>(&client,&h,"usage-history").await;
        assert!(usage["accounts"].as_array().unwrap().is_empty());
        same_client_response(&h).await;
        let now=Utc::now();
        // The numeric ID shape is required by current AccountId, not a real account.
        let id=AccountId::try_from("cx-000001".to_owned()).unwrap();
        let session=SessionId::from_key("codex:sample-acct-history");
        h.obs.dashboard().activity(ActivityInput {
            ts:now-ChronoDuration::seconds(2),kind:ActivityKind::Burst,
            provider:Provider::Codex,account:None,account_to:None,notable:true,
            data:history::ActivityData::default(),
        });
        h.obs.dashboard().activity(ActivityInput {
            ts:now-ChronoDuration::seconds(1),kind:ActivityKind::Burst,
            provider:Provider::Codex,account:Some(id.clone()),account_to:Some(id.clone()),notable:true,
            data:history::ActivityData{
                session:Some(session.clone()),sessions:vec![session],
                window:Some("5h".to_owned().try_into().unwrap()),
                reset_at:Some(now+ChronoDuration::hours(1)),
                expires_at:Some(now+ChronoDuration::hours(2)),reason:Some(MoveReason::Forced),
                attention:Some(AttentionCode::Burst),outcome:Some(RequestOutcome::Ok),
                action:Some(SettingAction::Pause),credits_used:Some(1.25),count:3,max_wait_ms:4,
                relayed:5,moved_groups:6,sessions_truncated:true,recovered:true,
                active:Some(0),threshold:Some(8),groups_hit:Some(9),
            },
        });
        for (minutes,pct,reset,serving,balance) in [
            (20,Some(25.),Some(now-ChronoDuration::minutes(10)),true,Some(10.)),
            (5,Some(30.),None,false,Some(8.)),
            (1,None,None,true,Some(7.)),
        ] {
            h.obs.dashboard().usage(UsageObservation{
                account:id.clone(),observed_at:now-ChronoDuration::minutes(minutes),
                window:Some(WindowObservation{
                    name:"5h".to_owned().try_into().unwrap(),scope:WindowScope::Account,
                    used_pct:pct,reset_at:reset,
                }),credits:Some(CreditsObservation{serving,balance}),
            });
        }
        let activity=same_response::<history::response::ActivityPage>(&client,&h,"activity?limit=100&days=7").await;
        let events=activity["events"].as_array().unwrap();
        let sparse=events.iter().find(|event|event["kind"]=="burst"&&event["account"].is_null()).unwrap();
        assert!(sparse["account_to"].is_null());
        assert_eq!(sparse["data"].as_object().unwrap().len(),6);
        let full=events.iter().find(|event|event["data"]["count"]==3).unwrap();
        assert_eq!(full["data"]["active"],0);
        assert_eq!(full["data"]["credits_used"],1.25);
        assert_eq!(full["data"]["sessions"].as_array().unwrap().len(),1);
        let limited=same_response::<history::response::ActivityPage>(&client,&h,"activity?limit=1").await;
        assert!(limited["truncated"].as_bool().unwrap());
        let since=activity["next_seq"].as_u64().unwrap();
        let delta=same_response::<history::response::ActivityPage>(&client,&h,&format!("activity?since_seq={since}")).await;
        assert!(delta["events"].as_array().unwrap().is_empty());
        let usage=same_response::<history::UsageView>(&client,&h,"usage-history?hours=24").await;
        let account=usage["accounts"].as_array().unwrap().iter().find(|row|row["id"]==id.as_str()).unwrap();
        let window=&account["windows"][0];
        assert!(window["points"].as_array().unwrap().iter().any(|point|point[1].is_null()));
        assert!(!window["resets"].as_array().unwrap().is_empty());
        let spans=account["credits_spans"].as_array().unwrap();
        assert!(spans.iter().any(|span|span[1].is_null()));
        assert!(spans.iter().any(|span|!span[1].is_null()));
        same_client_response(&h).await;
        for path in ["activity?limit=0","usage-history?hours=0","usage-history?hours=49"] {
            let response=client.get(format!("http://{}/admin/v1/{path}",h.servers.admin_addr))
                .bearer_auth(ADMIN).send().await.unwrap();
            assert_eq!(response.status(),StatusCode::BAD_REQUEST);
        }
        for path in ["activity","usage-history"] {
            let response=client.get(format!("http://{}/admin/v1/{path}",h.servers.admin_addr))
                .send().await.unwrap();
            assert_eq!(response.status(),StatusCode::UNAUTHORIZED);
        }
        assert!(matches!(h.upstream_trap.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
    }).catch_unwind().await;
    h.close().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
