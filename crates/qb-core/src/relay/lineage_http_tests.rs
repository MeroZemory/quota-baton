//! Real key-authenticated ingress, relay selection and captured provider HTTP.
use super::*;
use crate::{
    accounts::{Identity, Tokens},
    boxes::{BoxName, BoxRegistry},
    config::LinuxSecretBackend,
    local_secrets::LocalSecrets,
};
use futures_util::FutureExt;
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::json;
use std::{future::Future, io::Write, panic::AssertUnwindSafe, path::Path};

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic lineage HTTP operation failed"),
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    require(tokio::time::timeout(Duration::from_secs(5), future).await)
}

#[tokio::test]
async fn authenticated_box_http_strips_root_hint_and_prefers_eligible_root_account() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/tests/box-lineage-http");
    require(std::fs::create_dir_all(&base));
    let home = require(tempfile::tempdir_in(base));
    let upstream = require(bounded(ScriptedUpstream::start()).await);
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    config.secrets.linux_backend = LinuxSecretBackend::File0600;
    config.routing.weak_evidence_new_sessions = 100;
    config.upstream.codex_base = upstream.url.clone();
    config.upstream.anthropic_base = upstream.url.clone();
    let mut config_file = require(tempfile::NamedTempFile::new_in(home.path()));
    require(config_file.write_all(require(toml::to_string(&config)).as_bytes()));
    require(config_file.persist(home.path().join("config.toml")));
    let store = Arc::new(require(AccountStore::acquire(home.path())));
    let mut access_by_id = BTreeMap::new();
    for index in 0..2 {
        let email = format!("lineage-http-{index}@example.invalid");
        let identity = Identity::Codex {
            account_id: format!("synthetic-lineage-http-{index}"),
            user_id: None,
            email: email.clone(),
        };
        let account: Account = require(serde_json::from_value(json!({
            "schema":1, "id":identity.id(), "provider":"codex", "label":"Synthetic lineage",
            "email":email, "identity":identity, "source":{"kind":"oauth"},
            "user_state":"active", "health":{"kind":"ready"}, "generation":0,
            "token_meta":{"expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}
        })));
        let access = format!("synthetic-lineage-access-{index}");
        require(store.save(
            &account,
            &Tokens {
                access_token: access.clone(),
                refresh_token: format!("synthetic-lineage-refresh-{index}"),
                id_token: None,
            },
            None,
        ));
        access_by_id.insert(account.id, access);
    }
    let registry = require(BoxRegistry::open(home.path()));
    let (_, box_key) = require(registry.add(require(BoxName::parse("lineage-http")))).into_parts();
    let authority = require(registry.try_identify(&box_key));
    let obs = Arc::new(require(Observability::new(home.path(), None)));
    let state = Arc::new(require(RelayState::new(
        store,
        &config,
        obs.clone(),
        Arc::new(DebugPins::default()),
    )));
    let mut servers = require(
        bounded(crate::http::spawn_with_box_registry(
            &config,
            LocalSecrets {
                client_key: "synthetic-lineage-global".into(),
                admin_token: "synthetic-lineage-admin".into(),
            },
            router(state.clone()),
            Router::new(),
            Some(registry.clone()),
        ))
        .await,
    );
    // Catch assertion failures so the same retained server/relay owners still
    // perform cleanup. No spawned test driver or abandoned cleanup future.
    let exercised = AssertUnwindSafe(async {
        let mut candidates =
            require(bounded(state.candidates(Provider::Codex, None, None, &BTreeSet::new())).await);
        candidates.sort_by(|a, b| a.account.id.cmp(&b.account.id));
        assert_eq!(candidates.len(), 2);
        let native_id = candidates[0].account.id.clone();
        let root_id = candidates[1].account.id.clone();
        for candidate in &candidates {
            state
                .bindings
                .set_account_threshold(&candidate.account.id, Some(8));
        }
        // Seed only the retained root using the real table. The three new
        // placements below go through actual authenticated HTTP, never a
        // forged RootHint extension or a substitute selector.
        match state.bindings.choose(
            Provider::Codex,
            Some("codex:lineage-http-root"),
            None,
            None,
            &candidates[1..],
            None,
            100,
        ) {
            crate::bindings::Decision::Selected { account, lease, .. } => {
                assert_eq!(account, root_id);
                lease.dispatched();
            }
            _ => panic!("synthetic root placement failed"),
        }
        let client = require(reqwest::Client::builder().no_proxy().build());
        for (session, key, hint, expected) in [
            ("lineage-http-plain", box_key.as_str(), false, &native_id),
            ("lineage-http-hinted", box_key.as_str(), true, &root_id),
            (
                "lineage-http-global",
                "synthetic-lineage-global",
                true,
                &native_id,
            ),
        ] {
            upstream
                .state
                .push(ScriptedResponse::json(StatusCode::OK, "{}"));
            let mut request = client
                .post(format!("http://{}/v1/responses", servers.data_addr))
                .bearer_auth(key)
                .header("session-id", session)
                .header("content-type", "application/json")
                .body("{}");
            if hint {
                request = request.header("x-qb-root-session", "codex:lineage-http-root");
            }
            let response = require(bounded(request.send()).await);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()["x-quota-baton-account"]
                    .to_str()
                    .unwrap(),
                expected.as_str()
            );
            assert_eq!(require(bounded(response.bytes()).await).as_ref(), b"{}");
            assert_eq!(
                state
                    .bindings
                    .current(Some(&format!("codex:{session}")))
                    .as_ref(),
                Some(expected)
            );
            let captured = upstream.state.requests();
            let actual = captured.last().expect("actual provider request");
            assert_eq!(actual.headers["session-id"], session);
            assert!(!actual.headers.contains_key("x-qb-root-session"));
            // Verify actual provider credentials agree with the response and
            // binding; a response header alone cannot satisfy this assertion.
            assert!(
                actual.headers["authorization"].as_bytes()
                    == format!("Bearer {}", access_by_id[expected]).as_bytes()
            );
        }
        assert_eq!(upstream.state.requests().len(), 3);
        assert_eq!(
            state.bindings.current(Some("codex:lineage-http-root")),
            Some(root_id)
        );
        drop(client);
    })
    .catch_unwind()
    .await;

    servers.shutdown();
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        servers.wait_for_listener_close().await;
        tokio::join!(&mut servers.data_task, &mut servers.admin_task)
    })
    .await;
    let refreshed = tokio::time::timeout(Duration::from_secs(5), state.shutdown_refreshes()).await;
    let quiescent = tokio::time::timeout(Duration::from_secs(5), authority.wait_quiescent()).await;
    let flushed = obs.flush();
    drop(servers);
    drop(state);
    drop(obs);
    drop(registry);
    drop(upstream);
    let (data_stopped, admin_stopped) = require(stopped);
    require(require(data_stopped));
    require(require(admin_stopped));
    require(require(refreshed));
    require(quiescent);
    require(flushed);
    if let Err(panic) = exercised {
        std::panic::resume_unwind(panic);
    }
}
