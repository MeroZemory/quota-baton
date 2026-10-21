// Included once by m4a_ws.rs. The support directory avoids another Cargo target.
mod r11_actual {
    use super::*;
    use axum::response::IntoResponse;
    use std::{collections::BTreeMap, sync::Mutex};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::oneshot,
    };

    struct CatalogGate {
        arrived: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    }

    #[derive(Clone, Default)]
    struct CatalogFixture {
        models: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
        gate: Arc<Mutex<Option<CatalogGate>>>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl CatalogFixture {
        fn set(&self, h: &Harness, account: &str, models: &[&str]) {
            let row = h.store.load(account).unwrap();
            let token = h.store.tokens(&row).unwrap();
            self.models.lock().unwrap().insert(
                format!("Bearer {}", token.access_token),
                models.iter().map(|model| (*model).to_owned()).collect(),
            );
        }

        fn pause(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (arrived, observed) = oneshot::channel();
            let (release, gate) = oneshot::channel();
            assert!(
                self.gate
                    .lock()
                    .unwrap()
                    .replace(CatalogGate {
                        arrived,
                        release: gate
                    })
                    .is_none()
            );
            (observed, release)
        }

        async fn respond(self, headers: HeaderMap) -> axum::response::Response {
            let authorization = headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            self.requests.lock().unwrap().push(authorization.clone());
            let gate = self.gate.lock().unwrap().take();
            if let Some(gate) = gate {
                let _ = gate.arrived.send(());
                if gate.release.await.is_err() {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
            }
            let models = self
                .models
                .lock()
                .unwrap()
                .get(&authorization)
                .cloned()
                .unwrap_or_default();
            let mut response = axum::Json(json!({"models": models.into_iter()
                .map(|slug| json!({"slug": slug})).collect::<Vec<_>>()}))
            .into_response();
            // Each HTTP exchange gets its own dispatch decision in the fixture
            // proxy. A later WS dial cannot reuse a catalog connection.
            response
                .headers_mut()
                .insert("connection", "close".parse().unwrap());
            response
        }
    }

    struct LaunchHarness {
        h: Harness,
        catalogs: CatalogFixture,
        _catalog_task: RetryProxy,
        _proxy_task: RetryProxy,
    }

    impl LaunchHarness {
        async fn new(count: usize) -> Self {
            let mut h =
                Harness::new_with_options(count, false, false, Some((2, 3)), Some((5, 2))).await;
            // Reuse the synthetic homes, account import, WS recorder and serial
            // guard. Replace only the unused fixture relay/listeners, before any
            // launch or data request, to multiplex catalogs, HTTP and actual WS.
            h.servers.shutdown();
            h.servers.wait_for_listener_close().await;
            h.fake.state.keep_old_connections();
            let catalogs = CatalogFixture::default();
            for id in &h.ids {
                catalogs.set(&h, id, &["synthetic-model", "alternate-model"]);
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let catalog_address = listener.local_addr().unwrap().to_string();
            let catalog_route = catalogs.clone();
            let router = axum::Router::new().route(
                "/models",
                axum::routing::get(move |headers: HeaderMap| {
                    catalog_route.clone().respond(headers)
                }),
            );
            let catalog_task = RetryProxy(tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let ws_address = h.fake.url.strip_prefix("http://").unwrap().to_owned();
            let http_address = h.token_fake.url.strip_prefix("http://").unwrap().to_owned();
            let proxy_task = RetryProxy(tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            let catalog = catalog_address.clone();
                            let ws = ws_address.clone();
                            let http = http_address.clone();
                            connections.spawn(async move { let _ = forward(stream, catalog, ws, http).await; });
                        }
                        _ = connections.join_next(), if !connections.is_empty() => {}
                    }
                }
            }));
            let mut config = Config::default();
            config.server.data_port = 0;
            config.server.admin_port = 0;
            config.upstream.codex_base = base;
            config.routing.request_timeout_secs = 5;
            config.routing.ws_upgrade_hold_secs = 2;
            config.routing.rate_hold_secs = 2;
            config.routing.rate_hold_max_retries = 3;
            h.state = Arc::new(
                RelayState::new(h.store.clone(), &config, h.obs.clone(), h.pins.clone())
                    .unwrap()
                    .with_token_endpoint(Provider::Codex, h.token_fake.url.clone()),
            );
            h.servers = http::spawn_with_routes(
                &config,
                LocalSecrets {
                    client_key: "synthetic-local-key".into(),
                    admin_token: "synthetic-admin-key".into(),
                },
                relay::router(h.state.clone()),
                axum::Router::new(),
            )
            .await
            .unwrap();
            Self {
                h,
                catalogs,
                _catalog_task: catalog_task,
                _proxy_task: proxy_task,
            }
        }

        async fn issue(&self, account: Option<&str>) -> relay::LaunchReply {
            self.h
                .state
                .issue_launch(
                    relay::LaunchRequest {
                        model: Some("synthetic-model".into()),
                        model_is_explicit: true,
                        account_pref: account.map(str::to_owned),
                        resume_id: None,
                        move_for_model: false,
                    },
                    self.h.servers.data_addr.port(),
                )
                .await
                .unwrap()
        }

        fn request(&self, base: &str, key: Option<&str>) -> axum::http::Request<()> {
            let mut request = format!("{}/responses", base.replacen("http://", "ws://", 1))
                .into_client_request()
                .unwrap();
            request.headers_mut().insert(
                "authorization",
                "Bearer synthetic-local-key".parse().unwrap(),
            );
            if let Some(key) = key {
                request
                    .headers_mut()
                    .insert("session-id", key.parse().unwrap());
            }
            request
        }

        async fn connect(&self, launch: &relay::LaunchReply, key: Option<&str>) -> Client {
            let (client, response) = connect_async(self.request(&launch.base_url, key))
                .await
                .unwrap();
            assert_eq!(response.headers()["x-quota-baton-account"], launch.account);
            client
        }

        async fn turn(&self, client: &mut Client, raw: &str) {
            self.h
                .fake
                .state
                .push_reply([Message::text(r#"{"type":"response.completed"}"#)]);
            client.send(Message::text(raw.to_owned())).await.unwrap();
            assert_eq!(
                receive(client).await,
                Message::text(r#"{"type":"response.completed"}"#)
            );
        }

        fn http_reply(&self) {
            let mut reply = ScriptedResponse::json(
                StatusCode::OK,
                r#"{"id":"r11-http","object":"response","status":"completed","output":[]}"#,
            );
            reply.headers.insert("connection", "close".parse().unwrap());
            self.h.token_fake.state.push(reply);
        }

        fn data(&self) -> Vec<Message> {
            self.h
                .fake
                .state
                .messages()
                .into_iter()
                .filter(|message| matches!(message, Message::Text(_) | Message::Binary(_)))
                .collect()
        }
    }

    async fn forward(
        mut inbound: tokio::net::TcpStream,
        catalog: String,
        ws: String,
        http: String,
    ) -> std::io::Result<()> {
        let mut line = Vec::new();
        while line.len() < 8192 {
            let byte = inbound.read_u8().await?;
            line.push(byte);
            if byte == b'\n' {
                break;
            }
        }
        let target = if line.starts_with(b"GET /models") {
            catalog
        } else if line.starts_with(b"POST /responses") {
            http
        } else {
            ws
        };
        let mut outbound = tokio::net::TcpStream::connect(target).await?;
        outbound.write_all(&line).await?;
        tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
        Ok(())
    }

    #[tokio::test]
    async fn r11_ws_header_adoption_known_key_precedence_and_original_bytes() {
        let t = LaunchHarness::new(1).await;
        let launch = t.issue(None).await;
        let mut client = t.connect(&launch, Some("r11-c8")).await;
        let raw = r#"{ "type":"response.create", "model":"synthetic-model", "session_id":"body-is-not-c8", "input":[] }"#;
        t.turn(&mut client, raw).await;
        assert_eq!(t.data(), vec![Message::text(raw)]);
        assert!(
            t.h.state
                .bindings
                .current(Some("codex:body-is-not-c8"))
                .is_none()
        );
        assert_eq!(
            t.h.state.bindings.current(Some("codex:r11-c8")).as_deref(),
            Some(launch.account.as_str())
        );
        client.close(None).await.unwrap();
        drop(client);
        capacity_count(&t.h, &launch.account, 0).await;
        let unknown = "Z".repeat(43);
        let base = format!("http://{}/l/{unknown}/v1", t.h.servers.data_addr);
        let (mut known, response) = connect_async(t.request(&base, Some("r11-c8")))
            .await
            .unwrap();
        assert_eq!(response.headers()["x-quota-baton-account"], launch.account);
        t.turn(&mut known, raw).await;
        let before = t.h.fake.state.requests().len();
        let Error::Http(response) = connect_async(t.request(&base, Some("r11-unbound")))
            .await
            .unwrap_err()
        else {
            panic!("explicit relaunch expected")
        };
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let error: Value = serde_json::from_slice(response.body().as_ref().unwrap()).unwrap();
        assert_eq!(error["error"]["type"], "launch_expired");
        assert_eq!(t.h.fake.state.requests().len(), before);
        known.close(None).await.unwrap();
        drop(known);
        capacity_count(&t.h, &launch.account, 0).await;
        let handle = launch
            .base_url
            .split("/l/")
            .nth(1)
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        assert!(
            !t.h.ledger()
                .iter()
                .any(|row| row.to_string().contains(handle) || row.to_string().contains(&unknown))
        );
    }

    #[tokio::test]
    async fn r11_ws_later_unsupported_and_invalid_models_never_send_or_shop() {
        let t = LaunchHarness::new(2).await;
        let launch = t.issue(Some(&t.h.ids[0])).await;
        let mut client = t.connect(&launch, None).await;
        let raw = r#"{"type":"response.create","model":"synthetic-model","session_id":"not-a-header","input":[]}"#;
        t.turn(&mut client, raw).await;
        assert!(
            t.h.state
                .bindings
                .current(Some("codex:not-a-header"))
                .is_none()
        );
        for (raw, expected) in [
            (
                r#"{"type":"response.create","model":"not-supported","input":[]}"#,
                "model_not_supported",
            ),
            (
                r#"{"type":"response.create","model":null}"#,
                "invalid_launch",
            ),
            (
                r#"{"type":"response.create","model":"synthetic-model","model":"alternate-model"}"#,
                "invalid_launch",
            ),
            (r#"{"type":"response.create","model":7}"#, "invalid_launch"),
        ] {
            client.send(Message::text(raw)).await.unwrap();
            let Message::Text(error) = receive(&mut client).await else {
                panic!("local model explanation expected")
            };
            let error: Value = serde_json::from_str(&error).unwrap();
            assert_eq!(error["error"]["code"], expected);
            assert!(!error["error"]["message"].as_str().unwrap().is_ascii());
            assert_eq!(t.data().len(), 1);
            assert_eq!(t.h.fake.state.requests().len(), 1);
        }
        // Model metadata after a large input must still be guarded, while the
        // accepted original bytes pass unchanged to the original account.
        let raw = format!(
            r#"{{"input":"{}","type":"response.create","model":"alternate-model"}}"#,
            "x".repeat(1024 * 1024 + 1)
        );
        t.turn(&mut client, &raw).await;
        assert_eq!(t.data().last(), Some(&Message::text(raw)));
        assert_eq!(t.h.fake.state.requests().len(), 1);
        client.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn r11_ws_live_provisional_pin_redirects_during_actual_http_adoption() {
        let t = LaunchHarness::new(2).await;
        let launch = t.issue(None).await;
        let mut client = t.connect(&launch, None).await;
        let (arrived, release) = t.h.token_fake.state.pause_next_response();
        t.http_reply();
        let request = reqwest::Client::new()
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-local-key")
            .header("session-id", "r11-real-header")
            .body(r#"{"model":"synthetic-model","input":[]}"#);
        let adopting = tokio::spawn(async move { request.send().await.unwrap() });
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            t.h.state
                .bindings
                .current(Some("codex:r11-real-header"))
                .as_deref(),
            Some(launch.account.as_str())
        );
        let raw = r#"{"type":"response.create","model":"alternate-model","session_id":"still-not-c8","input":[]}"#;
        t.turn(&mut client, raw).await;
        let info =
            t.h.state
                .bindings
                .launch_info("codex:r11-real-header", Utc::now())
                .unwrap();
        assert_eq!(info.canonical_key, "codex:r11-real-header");
        assert_eq!(info.account, launch.account);
        assert_eq!(info.current_model.as_deref(), Some("alternate-model"));
        assert!(
            t.h.state
                .bindings
                .current(Some("codex:still-not-c8"))
                .is_none()
        );
        release.send(()).unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), adopting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        assert_eq!(t.data(), vec![Message::text(raw)]);
        client.close(None).await.unwrap();
        drop(client);
        capacity_count(&t.h, &launch.account, 0).await;
        let resumed =
            t.h.state
                .issue_launch(
                    relay::LaunchRequest {
                        model: Some("alternate-model".into()),
                        model_is_explicit: true,
                        account_pref: None,
                        resume_id: Some("r11-real-header".into()),
                        move_for_model: false,
                    },
                    t.h.servers.data_addr.port(),
                )
                .await
                .unwrap();
        assert_eq!(resumed.account, launch.account);
    }

    #[tokio::test]
    async fn r11_ws_managed_full_healthy_account_waits_without_capacity_shopping() {
        let t = LaunchHarness::new(2).await;
        let launch = t.issue(Some(&t.h.ids[0])).await;
        let mut live = t.connect(&launch, Some("r11-cap-live")).await;
        // A new launch preference may fall back under D20's evidence cap.
        // Reuse the bound launch so this rejection measures A, not another
        // account selected for a fresh session.
        t.h.fake.state.queue_send(Message::text(json!({"type":"error","status":400,"error":{"code":"websocket_connection_limit_reached"}}).to_string()));
        let mut rejected = t.connect(&launch, Some("r11-cap-live")).await;
        let Message::Text(error) = receive(&mut rejected).await else {
            panic!("synthetic capacity evidence expected")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&error).unwrap()["error"]["code"],
            "websocket_connection_limit_reached"
        );
        rejected.close(None).await.unwrap();
        drop(rejected);
        capacity_count(&t.h, &launch.account, 1).await;
        let before = t.h.fake.state.requests().len();
        // Capture identity before the pending selector can hold the data mutex.
        // Failure diagnostics must not synchronously wait on that same mutex.
        let canonical_before =
            t.h.state
                .bindings
                .launch_info("codex:r11-cap-live", Utc::now())
                .map(|info| (info.canonical_key, info.account, info.current_model));
        let waits_before = t.h.state.ws_capacity_waits_for_test();
        let request = t.request(&launch.base_url, Some("r11-cap-live"));
        let mut waiting = tokio::spawn(async move { connect_async(request).await });
        let observed = tokio::select! {
            biased;
            result = &mut waiting => Err(match result {
                Ok(Err(Error::Http(response))) => {
                    let body = response.body().as_deref().map(|body|
                        String::from_utf8_lossy(&body[..body.len().min(512)]).into_owned());
                    format!("HTTP {} body={body:?}", response.status())
                }
                Ok(Err(error)) => format!("client error: {error}"),
                Ok(Ok((_client, response))) => format!(
                    "unexpected HTTP {} account={:?}", response.status(),
                    response.headers().get("x-quota-baton-account")),
                Err(error) => format!("upgrade task: {error}"),
            }),
            result = tokio::time::timeout(Duration::from_secs(2), async {
                while t.h.state.ws_capacity_waits_for_test() == waits_before {
                    tokio::task::yield_now().await;
                }
            }) => result.map_err(|_| "capacity wait marker timeout".to_owned()),
        };
        if let Err(detail) = observed {
            let upgrade_pending = !waiting.is_finished();
            waiting.abort();
            panic!(
                "actual upgrade did not remain in capacity wait: {detail}; \
                 upgrade_pending={upgrade_pending}; waits={waits_before}->{}; \
                 upstream_requests={before}->{}; reserved={:?}; canonical_before={canonical_before:?}",
                t.h.state.ws_capacity_waits_for_test(),
                t.h.fake.state.requests().len(),
                t.h.ids
                    .iter()
                    .map(|id| (id, t.h.state.ws_reserved_for_test(id)))
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(t.h.fake.state.requests().len(), before);
        assert_eq!(
            t.h.state
                .bindings
                .current(Some("codex:r11-cap-live"))
                .as_deref(),
            Some(launch.account.as_str())
        );
        for id in t.h.ids.iter().filter(|id| *id != &launch.account) {
            assert_eq!(t.h.state.ws_reserved_for_test(id), 0);
        }
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        capacity_count(&t.h, &launch.account, 1).await;
        t.turn(
            &mut live,
            r#"{"type":"response.create","model":"synthetic-model","input":[]}"#,
        )
        .await;
        assert_eq!(t.h.fake.state.requests().len(), before);
        live.close(None).await.unwrap();
        drop(live);
        capacity_count(&t.h, &launch.account, 0).await;
    }

    #[tokio::test]
    async fn r11_ws_catalog_wait_cancellation_and_drain_drop_before_gate_release() {
        for drain in [false, true] {
            let t = LaunchHarness::new(1).await;
            let launch = t.issue(None).await;
            let mut client = t.connect(&launch, Some("r11-catalog-wait")).await;
            let (arrived, release) = t.catalogs.pause();
            client
                .send(Message::text(
                    r#"{"type":"response.create","model":"not-supported","input":[]}"#,
                ))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), arrived)
                .await
                .unwrap()
                .unwrap();
            if drain {
                t.h.state.begin_drain().await.unwrap();
                assert_close(receive(&mut client).await, 1001);
            } else {
                client.close(None).await.unwrap();
            }
            drop(client);
            capacity_count(&t.h, &launch.account, 0).await;
            assert!(t.data().is_empty());
            let info =
                t.h.state
                    .bindings
                    .launch_info("codex:r11-catalog-wait", Utc::now())
                    .unwrap();
            assert_eq!(info.current_model.as_deref(), Some("synthetic-model"));
            let _ = release.send(());
        }
    }

    #[tokio::test]
    async fn r11_ws_same_account_rate_redial_keeps_original_bytes_and_one_permit() {
        let t = LaunchHarness::new(1).await;
        let launch = t.issue(None).await;
        let mut client = t.connect(&launch, Some("r11-redial")).await;
        t.h.fake.state.push_reply([
            Message::text(
                r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#,
            ),
            Message::Close(None),
        ]);
        t.h.fake
            .state
            .push_reply([Message::text(r#"{"type":"response.completed"}"#)]);
        let raw = r#"{ "type":"response.create", "model":"alternate-model", "input":[] }"#;
        client.send(Message::text(raw)).await.unwrap();
        assert_eq!(
            receive(&mut client).await,
            Message::text(r#"{"type":"response.completed"}"#)
        );
        assert_eq!(t.h.fake.state.requests().len(), 2);
        assert_eq!(t.data(), vec![Message::text(raw), Message::text(raw)]);
        assert_eq!(t.h.state.ws_reserved_for_test(&launch.account), 1);
        let info =
            t.h.state
                .bindings
                .launch_info("codex:r11-redial", Utc::now())
                .unwrap();
        assert_eq!(info.account, launch.account);
        assert_eq!(info.current_model.as_deref(), Some("alternate-model"));
        client.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn r11_ws_forced_reconnect_filters_current_model_and_prefers_full_coverage() {
        let t = LaunchHarness::new(4).await;
        let a = &t.h.ids[0];
        let incompatible = &t.h.ids[1];
        let partial = &t.h.ids[2];
        let covering = &t.h.ids[3];
        t.catalogs.set(&t.h, incompatible, &["unrelated-model"]);
        t.catalogs.set(&t.h, partial, &["synthetic-model"]);
        let launch = t.issue(Some(a)).await;
        assert_eq!(&launch.account, a);
        // Read the account-local menu so full advertised coverage is real
        // evidence, rather than a fixture assumption about issue_launch.
        let response = reqwest::Client::new()
            .get(&launch.catalog_url)
            .bearer_auth("synthetic-local-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let menu: Value = response.json().await.unwrap();
        assert!(
            menu["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["slug"] == "alternate-model")
        );
        let mut client = t.connect(&launch, Some("r11-forced")).await;
        t.h.fake
            .state
            .push_reply([Message::Text(quota_error().into())]);
        let raw = r#"{"type":"response.create","model":"synthetic-model","input":[]}"#;
        client.send(Message::text(raw)).await.unwrap();
        assert_close(receive(&mut client).await, 1012);
        // The availability check must not publish through the live old pin.
        // Drop and observe its release before the new admitted upgrade.
        drop(client);
        capacity_count(&t.h, a, 0).await;
        assert_eq!(
            t.h.state
                .bindings
                .current(Some("codex:r11-forced"))
                .as_deref(),
            Some(a.as_str())
        );
        let (mut reconnected, response) =
            connect_async(t.request(&launch.base_url, Some("r11-forced")))
                .await
                .unwrap();
        assert_eq!(
            response.headers()["x-quota-baton-account"],
            covering.as_str()
        );
        assert_eq!(
            t.h.state
                .bindings
                .current(Some("codex:r11-forced"))
                .as_deref(),
            Some(covering.as_str())
        );
        assert_eq!(
            t.data(),
            vec![Message::text(raw)],
            "no cross-account replay"
        );
        let before = t.h.fake.state.requests().len();
        t.turn(
            &mut reconnected,
            r#"{"type":"response.create","model":"alternate-model","input":[]}"#,
        )
        .await;
        assert_eq!(t.h.fake.state.requests().len(), before);
        reconnected.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn r11_ws_model_plan_denial_keeps_signal_refreshes_bound_only_and_never_shops() {
        for handshake in [false, true] {
            let t = LaunchHarness::new(2).await;
            let launch = t.issue(Some(&t.h.ids[0])).await;
            let denied = if handshake {
                "synthetic-model"
            } else {
                "alternate-model"
            };
            let message = format!(
                "The '{denied}' model is not supported when using Codex with a ChatGPT account"
            );
            let error =
                json!({"type":"error","status":400,"error":{"message":message}}).to_string();
            let before = t.catalogs.requests.lock().unwrap().len();
            if handshake {
                t.catalogs.set(&t.h, &launch.account, &["alternate-model"]);
                t.h.fake.state.push_handshake(
                    StatusCode::BAD_REQUEST,
                    error.clone(),
                    HeaderMap::new(),
                );
                let Error::Http(response) =
                    connect_async(t.request(&launch.base_url, Some("r11-plan")))
                        .await
                        .unwrap_err()
                else {
                    panic!("original model-plan failure expected")
                };
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                assert_eq!(
                    response.body().as_ref().unwrap().as_slice(),
                    error.as_bytes()
                );
                assert!(t.data().is_empty());
            } else {
                let mut client = t.connect(&launch, Some("r11-plan")).await;
                t.catalogs.set(&t.h, &launch.account, &["synthetic-model"]);
                t.h.fake.state.push_reply([Message::text(error.clone())]);
                let raw = r#"{"type":"response.create","model":"alternate-model","input":[]}"#;
                client.send(Message::text(raw)).await.unwrap();
                assert_eq!(receive(&mut client).await, Message::text(error));
                assert_eq!(t.data(), vec![Message::text(raw)], "no capability replay");
                drop(client);
            }
            let account = t.h.store.load(&launch.account).unwrap();
            assert!(
                matches!(&account.health, Health::Cooling { reason: HealthReason::Permission, scope, until } if scope == &format!("model:{denied}") && *until > Utc::now())
            );
            assert!(!account.eligible(Some(denied)));
            assert!(account.eligible(Some(if handshake {
                "alternate-model"
            } else {
                "synthetic-model"
            })));
            assert_eq!(
                t.h.state
                    .bindings
                    .current(Some("codex:r11-plan"))
                    .as_deref(),
                Some(launch.account.as_str())
            );
            let requests = t.catalogs.requests.lock().unwrap().clone();
            let token = t.h.store.tokens(&account).unwrap();
            assert_eq!(
                &requests[before..],
                &[format!("Bearer {}", token.access_token)]
            );
            assert_eq!(t.h.fake.state.requests().len(), 1, "no second account dial");
            capacity_count(&t.h, &launch.account, 0).await;
        }
    }

    #[tokio::test]
    async fn r11_ws_drain_at_final_create_gate_finishes_older_dispatched_turn() {
        let t = LaunchHarness::new(1).await;
        let launch = t.issue(None).await;
        let mut client = t.connect(&launch, Some("r11-final-gate")).await;
        let created = Message::text(r#"{"type":"response.created","response":{"id":"r11-old"}}"#);
        t.h.fake.state.push_reply([created.clone()]);
        let first = r#"{"type":"response.create","model":"synthetic-model","input":[]}"#;
        client.send(Message::text(first)).await.unwrap();
        assert_eq!(receive(&mut client).await, created);
        let (arrived, release) = t.h.state.pause_next_ws_create_dispatch();
        let second = r#"{"type":"response.create","model":"alternate-model","input":[]}"#;
        client.send(Message::text(second)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(t.data(), vec![Message::text(first)]);
        t.h.state.begin_drain().await.unwrap();
        let tail = Message::text(
            r#"{"type":"response.output_text.delta","response_id":"r11-old","delta":"tail"}"#,
        );
        let terminal =
            Message::text(r#"{"type":"response.completed","response":{"id":"r11-old"}}"#);
        t.h.fake.state.send(tail.clone()).unwrap();
        t.h.fake.state.send(terminal.clone()).unwrap();
        assert_eq!(receive(&mut client).await, tail);
        assert_eq!(receive(&mut client).await, terminal);
        assert_close(receive(&mut client).await, 1001);
        drop(client);
        // Withhold release until the old turn and close have arrived. Cancellation
        // must drop the final gate, rather than depend on its callback returning.
        capacity_count(&t.h, &launch.account, 0).await;
        let _ = release.send(());
        assert_eq!(
            t.data(),
            vec![Message::text(first)],
            "zero turn2 upstream bytes"
        );
        assert_eq!(t.h.fake.state.requests().len(), 1);
        assert!(t.h.ledger().iter().any(|row| row["transport"] == "ws"
            && row["model"] == "synthetic-model"
            && row["outcome"] == "ok"));
    }

    #[tokio::test]
    async fn r11_ws_recovered_bound_account_outweighs_prior_reconnect_exclusion() {
        for strict in [false, true] {
            let t = LaunchHarness::new(2).await;
            let launch = t.issue(Some(&t.h.ids[0])).await;
            let mut client = t.connect(&launch, Some("synthetic-ws-session")).await;
            t.h.fake
                .state
                .push_reply([Message::Text(quota_error().into())]);
            let raw = r#"{"type":"response.create","model":"synthetic-model","input":[]}"#;
            client.send(Message::text(raw)).await.unwrap();
            assert_close(receive(&mut client).await, 1012);
            drop(client);
            capacity_count(&t.h, &launch.account, 0).await;
            assert_eq!(
                t.h.state
                    .bindings
                    .current(Some("codex:synthetic-ws-session"))
                    .as_deref(),
                Some(launch.account.as_str())
            );
            t.h.state
                .accounts
                .release_quota_hold(&launch.account)
                .await
                .unwrap();
            assert!(
                t.h.state
                    .accounts
                    .eligible(&launch.account, Some("synthetic-model"))
                    .await
            );
            if strict {
                routing_ws::strict(&t.h).await;
            }
            let (mut recovered, response) =
                connect_async(t.request(&launch.base_url, Some("synthetic-ws-session")))
                    .await
                    .unwrap();
            assert_eq!(response.headers()["x-quota-baton-account"], launch.account);
            assert_eq!(
                t.h.state
                    .bindings
                    .current(Some("codex:synthetic-ws-session"))
                    .as_deref(),
                Some(launch.account.as_str())
            );
            assert_eq!(
                t.data(),
                vec![Message::text(raw)],
                "availability probe did not replay or move"
            );
            t.turn(&mut recovered, raw).await;
            assert_eq!(t.h.fake.state.requests().len(), 2);
            recovered.close(None).await.unwrap();
        }
    }
}
