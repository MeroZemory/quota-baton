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
    #[tokio::test]
    async fn r11_ws_live_ordinary_same_account_promotion_enforces_managed_models() {
        for binary in [false, true] {
            let t = LaunchHarness::new(2).await;
            let key = "r11-live-promoted";
            let canonical = format!("codex:{key}");
            let base = format!("http://{}/v1", t.h.servers.data_addr);
            let (mut client, response) = connect_async(t.request(&base, Some(key))).await.unwrap();
            assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
            let account = response.headers()["x-quota-baton-account"]
                .to_str()
                .unwrap()
                .to_owned();
            capacity_count(&t.h, &account, 1).await;
            let first = r#"{ "type":"response.create", "model":"synthetic-model", "input":[] }"#;
            t.turn(&mut client, first).await;
            assert!(
                !t.h.state
                    .bindings
                    .launch_info(&canonical, Utc::now())
                    .unwrap()
                    .managed
            );
            assert_eq!(t.data(), vec![Message::text(first)]);

            // The other account supports the rejected model. Promotion must
            // keep this live socket on its existing account without shopping.
            let other = t.h.ids.iter().find(|id| **id != account).unwrap();
            t.catalogs
                .set(&t.h, other, &["synthetic-model", "other-account-only"]);
            let launch =
                t.h.state
                    .issue_launch(
                        relay::LaunchRequest {
                            model: Some("synthetic-model".into()),
                            model_is_explicit: true,
                            account_pref: Some(account.clone()),
                            resume_id: Some(key.into()),
                            move_for_model: false,
                        },
                        t.h.servers.data_addr.port(),
                    )
                    .await
                    .unwrap();
            assert_eq!(
                launch.account, account,
                "same-account live resume remains legitimate"
            );
            let response = reqwest::Client::new()
                .get(&launch.catalog_url)
                .bearer_auth("synthetic-local-key")
                .header("session-id", key)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let menu = response.json::<Value>().await.unwrap();
            assert!(
                menu["models"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["slug"] == "alternate-model")
            );
            assert!(
                !menu["models"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["slug"] == "other-account-only")
            );
            let promoted =
                t.h.state
                    .bindings
                    .launch_info(&canonical, Utc::now())
                    .unwrap();
            assert!(promoted.managed);
            assert_eq!(promoted.account, account);
            assert_eq!(promoted.canonical_key, canonical);
            assert!(promoted.advertised_models.contains("alternate-model"));
            let routing = t.h.state.bindings.routing_view();
            let moves: Vec<_> =
                t.h.ledger()
                    .into_iter()
                    .filter(|row| row["event"] == "session_move")
                    .collect();
            let catalog_before = t.catalogs.requests.lock().unwrap().len();

            let unsupported =
                r#"{ "type":"response.create", "model":"other-account-only", "input":[] }"#;
            let frame = |raw: &str| {
                if binary {
                    Message::binary(raw.as_bytes().to_vec())
                } else {
                    Message::text(raw.to_owned())
                }
            };
            client.send(frame(unsupported)).await.unwrap();
            let Message::Text(error) = receive(&mut client).await else {
                panic!("local model explanation expected")
            };
            let error: Value = serde_json::from_str(&error).unwrap();
            assert_eq!(error["error"]["code"], "model_not_supported");
            assert!(!error["error"]["message"].as_str().unwrap().is_ascii());
            assert_eq!(
                t.data(),
                vec![Message::text(first)],
                "unsupported turn never reaches upstream"
            );
            assert_eq!(
                t.h.fake.state.requests().len(),
                1,
                "no move or replacement WS dial"
            );
            assert_eq!(
                t.h.state
                    .bindings
                    .launch_info(&canonical, Utc::now())
                    .unwrap(),
                promoted
            );
            capacity_count(&t.h, &account, 1).await;
            let row = t.h.store.load(&account).unwrap();
            let token = t.h.store.tokens(&row).unwrap();
            assert_eq!(
                &t.catalogs.requests.lock().unwrap()[catalog_before..],
                &[format!("Bearer {}", token.access_token)],
                "unsupported-model refresh only visits the bound account"
            );

            // Keep using the original ordinary socket after the local error.
            let supported = r#"{ "input":[], "type" : "response.create", "model":"alternate-model", "fixture_extra":{"preserve":true} }"#;
            let supported_frame = frame(supported);
            t.h.fake
                .state
                .push_reply([Message::text(r#"{"type":"response.completed"}"#)]);
            client.send(supported_frame.clone()).await.unwrap();
            assert_eq!(
                receive(&mut client).await,
                Message::text(r#"{"type":"response.completed"}"#)
            );
            assert_eq!(t.data(), vec![Message::text(first), supported_frame]);
            assert_eq!(t.h.fake.state.requests().len(), 1);
            let after =
                t.h.state
                    .bindings
                    .launch_info(&canonical, Utc::now())
                    .unwrap();
            assert_eq!(after.account, account);
            assert_eq!(after.canonical_key, canonical);
            assert_eq!(after.current_model.as_deref(), Some("alternate-model"));
            assert_eq!(after.advertised_models, promoted.advertised_models);
            assert_eq!(
                t.h.state.bindings.export().unwrap()["bindings"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(t.h.state.bindings.routing_view(), routing);
            assert_eq!(
                t.h.ledger()
                    .into_iter()
                    .filter(|row| row["event"] == "session_move")
                    .collect::<Vec<_>>(),
                moves
            );
            client.close(None).await.unwrap();
            drop(client);
            capacity_count(&t.h, &account, 0).await;
        }
    }

    #[tokio::test]
    async fn r11_ws_advertised_long_and_slash_model_ids_preserve_original_turn_bytes() {
        for model in ["m".repeat(101), "synthetic/model".to_owned()] {
            let t = LaunchHarness::new(1).await;
            let account = &t.h.ids[0];
            t.catalogs.set(&t.h, account, &[&model]);
            let launch =
                t.h.state
                    .issue_launch(
                        relay::LaunchRequest {
                            model: Some(model.clone()),
                            model_is_explicit: true,
                            account_pref: Some(account.clone()),
                            resume_id: None,
                            move_for_model: false,
                        },
                        t.h.servers.data_addr.port(),
                    )
                    .await
                    .unwrap();
            assert_eq!(&launch.account, account);
            assert_eq!(launch.model.as_deref(), Some(model.as_str()));
            let key = "r11-wide-model-id";
            let mut client = t.connect(&launch, Some(key)).await;
            let response = reqwest::Client::new()
                .get(&launch.catalog_url)
                .bearer_auth("synthetic-local-key")
                .header("session-id", key)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.json::<Value>().await.unwrap()["models"][0]["slug"],
                model
            );
            let raw = format!(
                r#"{{ "input":[], "type":"response.create", "model":"{model}", "fixture_extra":{{"keep":true}} }}"#
            );
            t.turn(&mut client, &raw).await;
            assert_eq!(t.data(), vec![Message::text(raw)]);
            assert_eq!(t.h.fake.state.requests().len(), 1);
            let info =
                t.h.state
                    .bindings
                    .launch_info(&format!("codex:{key}"), Utc::now())
                    .unwrap();
            assert!(info.managed);
            assert_eq!(&info.account, account);
            assert_eq!(info.current_model.as_deref(), Some(model.as_str()));
            assert!(info.advertised_models.contains(&model));
            client.close(None).await.unwrap();
            drop(client);
            capacity_count(&t.h, account, 0).await;
        }
    }

    #[tokio::test]
    async fn r11_ws_pending_upgrade_http_adoption_rejects_stale_choice_without_orphan_or_capacity_leak()
     {
        let t = LaunchHarness::new(2).await;
        let launch = t.issue(None).await;
        let snapshot = t.h.state.bindings.export().unwrap();
        let provisional = snapshot["bindings"][0]["key"].as_str().unwrap().to_owned();
        assert!(provisional.starts_with("launch:"));
        let key = "r11-pending-upgrade-adoption";
        let canonical = format!("codex:{key}");
        let (arrived, release) = t.h.state.pause_next_ws_selection();
        // Poll a real token-only upgrade through model admission and candidate
        // collection, then hold its selector before the mutation gate.
        let pending = connect_async(t.request(&launch.base_url, None));
        tokio::pin!(pending);
        tokio::select! {
            _ = &mut pending => panic!("upgrade escaped the selection pause"),
            result = tokio::time::timeout(Duration::from_secs(5), arrived) => {
                result.unwrap().unwrap();
            }
        }
        assert!(
            t.h.fake.state.requests().is_empty(),
            "no upstream WS dial before admission"
        );
        for account in &t.h.ids {
            capacity_count(&t.h, account, 0).await;
        }
        t.http_reply();
        let raw = r#"{ "model":"synthetic-model", "input":[], "fixture_extra":"HTTP adoption" }"#;
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(format!("{}/responses", launch.base_url))
                .bearer_auth("synthetic-local-key")
                .header("session-id", key)
                .body(raw)
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(5), response.bytes())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            t.h.state.bindings.current(Some(&canonical)).as_deref(),
            Some(launch.account.as_str())
        );
        assert!(t.h.state.bindings.current(Some(&provisional)).is_none());
        let calls = t.h.token_fake.state.requests();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path_and_query, "/responses");
        assert_eq!(calls[0].body.as_ref(), raw.as_bytes());

        // The HTTP request has adopted and completed before the stale upgrade
        // can acquire mutations. Refusal must happen before capacity or dial.
        release.send(()).unwrap();
        let Error::Http(response) = tokio::time::timeout(Duration::from_secs(5), &mut pending)
            .await
            .unwrap()
            .unwrap_err()
        else {
            panic!("stale upgrade must receive an explicit HTTP refusal")
        };
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = serde_json::from_slice(response.body().as_ref().unwrap()).unwrap();
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert!(error["error"]["message"].is_string());
        assert!(
            t.h.fake.state.requests().is_empty(),
            "stale choice never dials upstream"
        );
        assert!(t.data().is_empty());
        for account in &t.h.ids {
            capacity_count(&t.h, account, 0).await;
        }

        let assert_reloadable = || {
            assert!(t.h.state.bindings.current(Some(&provisional)).is_none());
            let info =
                t.h.state
                    .bindings
                    .launch_info(&provisional, Utc::now())
                    .unwrap();
            assert!(info.managed);
            assert_eq!(info.canonical_key, canonical);
            assert_eq!(info.account, launch.account);
            let snapshot = t.h.state.bindings.export().unwrap();
            assert_eq!(snapshot["bindings"].as_array().unwrap().len(), 1);
            assert_eq!(snapshot["bindings"][0]["key"], canonical);
            assert_eq!(snapshot["bindings"][0]["account"], launch.account);
            assert_eq!(snapshot["launches"].as_object().unwrap().len(), 1);
            let anchors = snapshot["launch_anchors"].as_object().unwrap();
            assert!(!anchors.is_empty());
            for anchor in anchors.values() {
                assert_eq!(anchor["canonical_key"], canonical);
            }
            t.h.state.bindings.flush().unwrap();
            let restored =
                qb_core::bindings::BindingTable::new(&t.h.home, Default::default()).unwrap();
            assert_eq!(
                restored.launch_info(&provisional, Utc::now()).unwrap(),
                info
            );
        };
        assert_reloadable();
        let routing = t.h.state.bindings.routing_view();
        let moves: Vec<_> =
            t.h.ledger()
                .into_iter()
                .filter(|row| row["event"] == "session_move")
                .collect();

        // Retry the original handle without a C8 header. Its anchor must now
        // own the adopted account/group, with exactly one live socket slot.
        let mut client = t.connect(&launch, None).await;
        capacity_count(&t.h, &launch.account, 1).await;
        for account in t.h.ids.iter().filter(|account| **account != launch.account) {
            capacity_count(&t.h, account, 0).await;
        }
        let raw = r#"{ "type":"response.create", "model":"alternate-model", "input":[], "fixture_extra":{"preserve":true} }"#;
        t.turn(&mut client, raw).await;
        assert_eq!(t.data(), vec![Message::text(raw)]);
        let calls = t.h.fake.state.requests();
        assert_eq!(calls.len(), 1, "only the fresh upgrade dials upstream");
        assert_eq!(
            calls[0].headers["chatgpt-account-id"],
            t.h.token_fake.state.requests()[0].headers["chatgpt-account-id"]
        );
        assert_reloadable();
        assert_eq!(
            t.h.state
                .bindings
                .launch_info(&canonical, Utc::now())
                .unwrap()
                .current_model
                .as_deref(),
            Some("alternate-model")
        );
        client.close(None).await.unwrap();
        drop(client);
        for account in &t.h.ids {
            capacity_count(&t.h, account, 0).await;
        }
        assert_reloadable();
        assert_eq!(t.h.state.bindings.routing_view(), routing);
        assert_eq!(
            t.h.ledger()
                .into_iter()
                .filter(|row| row["event"] == "session_move")
                .collect::<Vec<_>>(),
            moves
        );
    }

    async fn ordinary_with_open_turn(t: &LaunchHarness, key: &str) -> (Client, String, Message) {
        let base = format!("http://{}/v1", t.h.servers.data_addr);
        let (mut client, response) = connect_async(t.request(&base, Some(key))).await.unwrap();
        let account = response.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap()
            .to_owned();
        capacity_count(&t.h, &account, 1).await;
        let first = Message::text(
            r#"{ "type":"response.create", "model":"synthetic-model", "input":[], "fixture_marker":"older turn" }"#,
        );
        let created =
            Message::text(r#"{"type":"response.created","response":{"id":"r11-promoted-old"}}"#);
        t.h.fake.state.push_reply([created.clone()]);
        client.send(first.clone()).await.unwrap();
        assert_eq!(receive(&mut client).await, created);
        assert!(
            !t.h.state
                .bindings
                .launch_info(&format!("codex:{key}"), Utc::now())
                .unwrap()
                .managed
        );
        (client, account, first)
    }

    async fn promote_open_ordinary(t: &LaunchHarness, key: &str, account: &str) {
        let launch = tokio::time::timeout(
            Duration::from_secs(5),
            t.h.state.issue_launch(
                relay::LaunchRequest {
                    model: Some("synthetic-model".into()),
                    model_is_explicit: true,
                    account_pref: Some(account.into()),
                    resume_id: Some(key.into()),
                    move_for_model: false,
                },
                t.h.servers.data_addr.port(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            launch.account, account,
            "live promotion stays on the bound account"
        );
        let info =
            t.h.state
                .bindings
                .launch_info(&format!("codex:{key}"), Utc::now())
                .unwrap();
        assert!(info.managed);
        assert_eq!(info.account, account);
    }

    async fn drain_promoted_old_turn(t: &LaunchHarness, client: &mut Client, account: &str) {
        t.h.state.begin_drain().await.unwrap();
        let tail = Message::text(
            r#"{"type":"response.output_text.delta","response_id":"r11-promoted-old","delta":"old tail"}"#,
        );
        let terminal =
            Message::text(r#"{"type":"response.completed","response":{"id":"r11-promoted-old"}}"#);
        t.h.fake.state.send(tail.clone()).unwrap();
        t.h.fake.state.send(terminal.clone()).unwrap();
        assert_eq!(receive(client).await, tail, "older completion remains live");
        assert_eq!(receive(client).await, terminal);
        assert_close(receive(client).await, 1001);
        capacity_count(&t.h, account, 0).await;
        for other in t.h.ids.iter().filter(|id| id.as_str() != account) {
            capacity_count(&t.h, other, 0).await;
        }
        assert_eq!(t.h.fake.state.requests().len(), 1, "no shopping or redial");
    }

    #[tokio::test]
    async fn r11_ws_queued_ordinary_promotion_reparses_original_model_after_pacing() {
        let allowed = format!("vendor/{}", "m".repeat(101));
        let rejected_long = "u".repeat(101);
        for (case, requested, expected_error) in [
            ("null", Some(Value::Null), Some("invalid_launch")),
            (
                "slash",
                Some(json!("other/account-only")),
                Some("model_not_supported"),
            ),
            (
                "long",
                Some(json!(&rejected_long)),
                Some("model_not_supported"),
            ),
            ("supported", Some(json!(&allowed)), None),
            ("omitted", None, None),
        ] {
            let t = LaunchHarness::new(2).await;
            let key = format!("r11-after-pacing-{case}");
            let canonical = format!("codex:{key}");
            let (mut client, account, first) = ordinary_with_open_turn(&t, &key).await;
            t.catalogs.set(
                &t.h,
                &account,
                &["synthetic-model", "alternate-model", &allowed],
            );
            let other = t.h.ids.iter().find(|id| **id != account).unwrap();
            t.catalogs.set(
                &t.h,
                other,
                &["synthetic-model", "other/account-only", &rejected_long],
            );
            let field = requested
                .as_ref()
                .map_or_else(String::new, |model| format!("\"model\":{model},"));
            let raw = format!(
                r#"{{ "type":"response.create", {field} "input":[], "fixture_marker":"after-pacing-{case}" }}"#
            );
            let (arrived, release) =
                t.h.state
                    .pause_ws_ordinary_after_pacing(&canonical, raw.as_bytes());
            client.send(Message::text(raw.clone())).await.unwrap();
            // This holds the ordinary admission future after pacing/eligibility,
            // before the pump observes the new managed metadata. No timer races.
            tokio::time::timeout(Duration::from_secs(5), arrived)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(t.data(), vec![first.clone()]);
            promote_open_ordinary(&t, &key, &account).await;
            if expected_error.is_none() {
                t.h.fake.state.push_reply([
                    Message::text(
                        r#"{"type":"response.created","response":{"id":"r11-promoted-new"}}"#,
                    ),
                    Message::text(
                        r#"{"type":"response.completed","response":{"id":"r11-promoted-new"}}"#,
                    ),
                ]);
            }
            release.send(()).unwrap();
            if let Some(code) = expected_error {
                let Message::Text(error) = receive(&mut client).await else {
                    panic!("local model error expected")
                };
                let error: Value = serde_json::from_str(&error).unwrap();
                assert_eq!(error["error"]["code"], code, "{case}");
                assert_eq!(
                    t.data(),
                    vec![first.clone()],
                    "rejected original frame never sent"
                );
            } else {
                assert_eq!(
                    receive(&mut client).await,
                    Message::text(
                        r#"{"type":"response.created","response":{"id":"r11-promoted-new"}}"#
                    )
                );
                assert_eq!(
                    receive(&mut client).await,
                    Message::text(
                        r#"{"type":"response.completed","response":{"id":"r11-promoted-new"}}"#
                    )
                );
                assert_eq!(
                    t.data(),
                    vec![first.clone(), Message::text(raw)],
                    "original accepted bytes preserved"
                );
            }
            let info =
                t.h.state
                    .bindings
                    .launch_info(&canonical, Utc::now())
                    .unwrap();
            assert_eq!(info.account, account);
            let expected_model = if case == "supported" {
                allowed.as_str()
            } else {
                "synthetic-model"
            };
            assert_eq!(info.current_model.as_deref(), Some(expected_model));
            capacity_count(&t.h, &account, 1).await;
            drain_promoted_old_turn(&t, &mut client, &account).await;
            drop(client);
        }
    }

    #[tokio::test]
    async fn r11_ws_final_ordinary_promotion_veto_preserves_older_turn_text_and_binary() {
        for binary in [false, true] {
            let t = LaunchHarness::new(2).await;
            let key = "r11-final-ordinary-promotion";
            let canonical = format!("codex:{key}");
            let (mut client, account, first) = ordinary_with_open_turn(&t, key).await;
            let raw = r#"{ "type":"response.create", "model":"other/account-only", "input":[], "fixture_marker":"final ordinary gate" }"#;
            // Different pending targets must not steal this frame's gate even when
            // registered earlier on the same RelayState.
            let (mut other_session, other_session_release) =
                t.h.state
                    .pause_ws_ordinary_before_write("codex:r11-other-session", raw.as_bytes());
            let (mut other_payload, other_payload_release) =
                t.h.state
                    .pause_ws_ordinary_before_write(&canonical, b"different frame");
            let (arrived, release) =
                t.h.state
                    .pause_ws_ordinary_before_write(&canonical, raw.as_bytes());
            let frame = if binary {
                Message::binary(raw.as_bytes().to_vec())
            } else {
                Message::text(raw)
            };
            client.send(frame).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), arrived)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                other_session.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(matches!(
                other_payload.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert_eq!(
                t.data(),
                vec![first.clone()],
                "pause occurs before first write"
            );
            promote_open_ordinary(&t, key, &account).await;
            release.send(()).unwrap();
            let Message::Text(error) = receive(&mut client).await else {
                panic!("local promotion veto expected")
            };
            let error: Value = serde_json::from_str(&error).unwrap();
            assert_eq!(error["error"]["code"], "launch_binding_changed");
            assert_eq!(
                t.data(),
                vec![first.clone()],
                "no bytes from the unadmitted frame"
            );
            capacity_count(&t.h, &account, 1).await;

            // Promotion has been observed: the same socket can now admit a valid
            // turn normally, while the earlier dispatched completion stays open.
            let supported = r#"{ "type":"response.create", "model":"alternate-model", "input":[], "fixture_marker":"after veto" }"#;
            t.h.fake.state.push_reply([
                Message::text(
                    r#"{"type":"response.created","response":{"id":"r11-promoted-new"}}"#,
                ),
                Message::text(
                    r#"{"type":"response.completed","response":{"id":"r11-promoted-new"}}"#,
                ),
            ]);
            client.send(Message::text(supported)).await.unwrap();
            assert_eq!(
                receive(&mut client).await,
                Message::text(
                    r#"{"type":"response.created","response":{"id":"r11-promoted-new"}}"#
                )
            );
            assert_eq!(
                receive(&mut client).await,
                Message::text(
                    r#"{"type":"response.completed","response":{"id":"r11-promoted-new"}}"#
                )
            );
            assert_eq!(t.data(), vec![first, Message::text(supported)]);
            let info =
                t.h.state
                    .bindings
                    .launch_info(&canonical, Utc::now())
                    .unwrap();
            assert_eq!(info.account, account);
            assert_eq!(info.current_model.as_deref(), Some("alternate-model"));
            drain_promoted_old_turn(&t, &mut client, &account).await;
            drop(client);
            drop((
                other_session,
                other_payload,
                other_session_release,
                other_payload_release,
            ));
        }
    }
}
