//! One real shared-TCP provider-WS regression, with synthetic local credentials.
use crate::{
    accounts::{Account, AccountStore, Identity, Tokens},
    boxes::{BoxName, BoxRegistry},
    config::{Config, LinuxSecretBackend},
    debug_pin::DebugPins,
    http::spawn_with_box_registry,
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use futures_util::{SinkExt, StreamExt};
use qb_testkit::websocket::{Message, WebSocketUpstream};
use serde_json::json;
use std::{future::Future, io::Write, path::Path, sync::Arc, time::Duration};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic box WS operation failed"),
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    require(tokio::time::timeout(Duration::from_secs(5), future).await)
}

#[tokio::test]
async fn shared_tcp_box_revoke_closes_only_a_provider_ws_and_drains_its_grant() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/tests/box-provider-ws");
    require(std::fs::create_dir_all(&base));
    let home = require(tempfile::tempdir_in(base));
    let upstream = require(bounded(WebSocketUpstream::start()).await);
    // The fake otherwise replaces/closes A when B connects: that would mask
    // whether the real exact-grant watcher closed A.
    upstream.state.keep_old_connections();
    upstream.state.set_echo(false);
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    config.secrets.linux_backend = LinuxSecretBackend::File0600;
    config.upstream.codex_base = upstream.url.clone();
    config.websocket.idle_close_secs = 60;
    // Self-contained cross-platform secret fixture; no writable fake systemd
    // credential or Windows-only importer. NamedTempFile creates a private file.
    let mut config_file = require(tempfile::NamedTempFile::new_in(home.path()));
    require(config_file.write_all(require(toml::to_string(&config)).as_bytes()));
    require(config_file.persist(home.path().join("config.toml")));
    let store = Arc::new(require(AccountStore::acquire(home.path())));
    let identity = Identity::Codex {
        account_id: "synthetic-box-ws-owned".into(),
        user_id: None,
        email: "box-ws@example.invalid".into(),
    };
    let account: Account = require(serde_json::from_value(json!({
        "schema":1, "id":identity.id(), "provider":"codex", "label":"Synthetic WS",
        "email":"box-ws@example.invalid", "identity":identity,
        "source":{"kind":"oauth"}, "user_state":"active", "health":{"kind":"ready"},
        "generation":0,
        "token_meta":{"expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}
    })));
    require(store.save(
        &account,
        &Tokens {
            access_token: "synthetic-owned-ws-access".into(),
            refresh_token: "synthetic-owned-ws-refresh".into(),
            id_token: None,
        },
        None,
    ));
    let registry = require(BoxRegistry::open(home.path()));
    let (a_scope, a_key) =
        require(registry.add(require(BoxName::parse("synthetic-a")))).into_parts();
    let (_, b_key) = require(registry.add(require(BoxName::parse("synthetic-b")))).into_parts();
    let a_authority = require(registry.try_identify(&a_key));
    let b_authority = require(registry.try_identify(&b_key));
    let obs = Arc::new(require(Observability::new(home.path(), None)));
    let relay = Arc::new(require(RelayState::new(
        store,
        &config,
        obs.clone(),
        Arc::new(DebugPins::default()),
    )));
    let mut servers = require(
        bounded(spawn_with_box_registry(
            &config,
            LocalSecrets {
                client_key: "synthetic-global-client".into(),
                admin_token: "synthetic-global-admin".into(),
            },
            relay::router(relay.clone()),
            axum::Router::new(),
            Some(registry.clone()),
        ))
        .await,
    );

    let mut request_a =
        require(format!("ws://{}/v1/responses", servers.data_addr).into_client_request());
    request_a.headers_mut().insert(
        "authorization",
        require(format!("Bearer {}", a_key.as_str()).parse()),
    );
    request_a
        .headers_mut()
        .insert("session-id", "synthetic-box-a-session".parse().unwrap());
    let (mut a, response_a) = require(bounded(connect_async(request_a)).await);
    assert_eq!(
        response_a.status(),
        axum::http::StatusCode::SWITCHING_PROTOCOLS
    );
    upstream.state.push_reply([Message::text(
        json!({
            "type":"response.created","response":{"id":"synthetic-a-response"}
        })
        .to_string(),
    )]);
    require(
        bounded(
            a.send(Message::text(
                json!({
                    "type":"response.create","model":"synthetic-box-model","input":[]
                })
                .to_string(),
            )),
        )
        .await,
    );
    let initial_a = require(bounded(a.next()).await.expect("A provider frame"));
    assert!(matches!(initial_a, Message::Text(ref text) if text.contains("synthetic-a-response")));
    let a_upstream = require(upstream.state.sender());

    let mut request_b =
        require(format!("ws://{}/v1/responses", servers.data_addr).into_client_request());
    request_b.headers_mut().insert(
        "authorization",
        require(format!("Bearer {}", b_key.as_str()).parse()),
    );
    request_b
        .headers_mut()
        .insert("session-id", "synthetic-box-b-session".parse().unwrap());
    let (mut b, response_b) = require(bounded(connect_async(request_b)).await);
    assert_eq!(
        response_b.status(),
        axum::http::StatusCode::SWITCHING_PROTOCOLS
    );
    upstream.state.push_reply([Message::text(
        json!({
            "type":"response.created","response":{"id":"synthetic-b-response"}
        })
        .to_string(),
    )]);
    require(
        bounded(
            b.send(Message::text(
                json!({
                    "type":"response.create","model":"synthetic-box-model","input":[]
                })
                .to_string(),
            )),
        )
        .await,
    );
    let initial_b = require(bounded(b.next()).await.expect("B provider frame"));
    assert!(matches!(initial_b, Message::Text(ref text) if text.contains("synthetic-b-response")));
    let b_upstream = require(upstream.state.sender());
    assert!(!a_upstream.is_closed(), "opening B must not itself close A");

    // Do not manufacture an admission in this test. The real authenticated
    // upgrade/pump must retain A's existing request lifetime while it is live.
    let a_quiescent = a_authority.wait_quiescent();
    tokio::pin!(a_quiescent);
    assert!(
        a_quiescent
            .as_mut()
            .poll(&mut std::task::Context::from_waker(
                std::task::Waker::noop(),
            ))
            .is_pending(),
        "live A provider stream must own its admitted grant"
    );

    require(registry.revoke(a_scope.name()));
    assert!(matches!(
        bounded(a.next()).await,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
    bounded(a_upstream.closed()).await; // Actual provider-side receiver ended.
    bounded(a_quiescent.as_mut()).await;
    assert!(a_authority.is_revoked());
    assert!(!b_authority.is_revoked());
    assert!(!b_upstream.is_closed());
    let next_b = Message::text(
        json!({
            "type":"response.output_text.delta","delta":"synthetic-b-still-live"
        })
        .to_string(),
    );
    require(b_upstream.send(next_b.clone()));
    assert_eq!(
        require(bounded(b.next()).await.expect("B next provider frame")),
        next_b
    );
    require(
        bounded(b.send(Message::Ping(bytes::Bytes::from_static(
            b"synthetic-b-ping",
        ))))
        .await,
    );
    assert!(matches!(require(bounded(b.next()).await.expect("B pong")),
        Message::Pong(bytes) if bytes.as_ref() == b"synthetic-b-ping"));

    require(bounded(b.close(None)).await);
    bounded(b_upstream.closed()).await;
    bounded(b_authority.wait_quiescent()).await;
    servers.shutdown();
    bounded(async {
        servers.wait_for_listener_close().await;
        require(require((&mut servers.data_task).await));
        require(require((&mut servers.admin_task).await));
    })
    .await;
    // Borrow the retained relay, never move cleanup into a detached task. Keep
    // its store/observability owners through the bounded wait and flush; report
    // timeout as failure only after explicit local-owner cleanup. This is not a
    // production guarantee that a timed-out mandatory writer was joined.
    let refreshed = tokio::time::timeout(Duration::from_secs(5), relay.shutdown_refreshes()).await;
    let flushed = obs.flush();
    drop(servers);
    drop(relay);
    drop(obs);
    drop(upstream);
    require(require(refreshed));
    require(flushed);
}
