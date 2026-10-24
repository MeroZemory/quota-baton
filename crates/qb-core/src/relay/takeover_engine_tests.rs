//! Takeover refresh regressions using numeric-loopback scripted upstreams.
use super::*;
use crate::accounts::takeover::{self, *};
use std::sync::atomic::{AtomicBool, Ordering};

struct SourceFixture {
    proof: SourceProof,
    bytes: Vec<u8>,
    now: chrono::DateTime<chrono::Utc>,
    retired: AtomicBool,
}
impl SourceAccess for SourceFixture {
    fn snapshot(&self, _: Provider, _: &std::path::Path) -> Result<SourceSnapshot, TakeoverError> {
        if self.retired.load(Ordering::SeqCst) {
            return Err(TakeoverError::Unavailable);
        }
        Ok(SourceSnapshot {
            proof: self.proof.clone(),
            bytes: zeroize::Zeroizing::new(self.bytes.clone()),
        })
    }
    fn owner(&self, _: Provider, _: &SourceProof) -> Owner {
        Owner::Stopped
    }
    fn retire(
        &self,
        _: Provider,
        proof: &SourceProof,
        _: &std::path::Path,
        keep: bool,
    ) -> Result<(), TakeoverError> {
        if proof != &self.proof {
            return Err(TakeoverError::Conflict);
        }
        if !keep {
            self.retired.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.now
    }
}
struct ProfileFixture {
    hold: AtomicBool,
    entered: Notify,
    release: Notify,
}
impl ProfileResolver for ProfileFixture {
    fn endpoint(&self) -> &str {
        "http://127.0.0.1:23456/api/oauth/profile"
    }
    fn claude<'a>(&'a self, _: &'a str) -> AsyncResult<'a, ProfileIdentity> {
        Box::pin(async move {
            if self.hold.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(ProfileIdentity {
                identity: crate::accounts::Identity::Claude {
                    account_uuid: "engine-user".into(),
                    organization_uuid: Some("engine-org".into()),
                    email: "engine@example.invalid".into(),
                },
                email: "engine@example.invalid".into(),
                plan: None,
            })
        })
    }
}
struct EngineFixture {
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    gates: TokenGates,
    deps: Dependencies,
    profile: Arc<ProfileFixture>,
    path: std::path::PathBuf,
    _root: tempfile::TempDir,
}
impl EngineFixture {
    fn new() -> Self {
        Self::with_expiry(2)
    }
    fn with_expiry(hours: i64) -> Self {
        use sha2::Digest;
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/staging/r6-core/engine-homes");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(root.path()).unwrap());
        let obs = Arc::new(Observability::new(root.path(), None).unwrap());
        let runtime = AccountRuntime::new(store.clone(), obs.clone(), 1.0);
        let gates = TokenGates::new(
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            runtime,
        );
        let now = chrono::Utc::now();
        let path = root.path().join("native/.credentials.json");
        let bytes=serde_json::to_vec(&serde_json::json!({"claudeAiOauth":{"accessToken":"synthetic-access",
            "refreshToken":"synthetic-refresh","expiresAt":(now+chrono::Duration::hours(hours)).timestamp_millis()}})).unwrap();
        let hash = sha2::Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let source = Arc::new(SourceFixture {
            proof: SourceProof {
                path: path.clone(),
                volume: 1,
                index: 52,
                modified: now - chrono::Duration::minutes(5),
                sha256: hash,
            },
            bytes,
            now,
            retired: AtomicBool::new(false),
        });
        let profile = Arc::new(ProfileFixture {
            hold: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let deps = Dependencies {
            source,
            profile: profile.clone(),
        };
        Self {
            store,
            obs,
            gates,
            deps,
            profile,
            path,
            _root: root,
        }
    }
    fn request(&self, phase: Phase, inspect_digest: Option<String>) -> TakeoverRequest {
        TakeoverRequest {
            provider: Provider::Claude,
            path: self.path.clone(),
            phase,
            inspect_digest,
            replace: false,
            keep_source: false,
        }
    }
    async fn inspected(&self) -> TakeoverRequest {
        let value = self
            .gates
            .takeover(
                self.store.clone(),
                self.obs.clone(),
                self.request(Phase::Inspect, None),
                self.deps.clone(),
            )
            .await
            .unwrap();
        self.request(
            Phase::Commit,
            Some(value["inspect_digest"].as_str().unwrap().into()),
        )
    }
}

#[tokio::test]
async fn r6_takeover_engine_uses_one_existing_refresh_request_and_receipt_on_replay() {
    let f = EngineFixture::new();
    let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
    fake.state.push(qb_testkit::ScriptedResponse::json(axum::http::StatusCode::OK,
        r#"{"access_token":"synthetic-access","refresh_token":"synthetic-new-refresh","expires_in":3600}"#));
    let gates = f
        .gates
        .clone()
        .with_token_endpoint(Provider::Claude, fake.url.clone());
    let request = f.inspected().await;
    let value = gates
        .takeover(
            f.store.clone(),
            f.obs.clone(),
            request.clone(),
            f.deps.clone(),
        )
        .await
        .unwrap();
    assert_eq!(value["refresh"]["ok"], true);
    assert_eq!(fake.state.requests().len(), 1);
    assert_eq!(
        gates
            .takeover(f.store.clone(), f.obs.clone(), request, f.deps.clone())
            .await
            .unwrap(),
        value
    );
    assert_eq!(fake.state.requests().len(), 1);
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn r6_takeover_cancelled_profile_preparation_is_detached_and_tracked_by_drain() {
    let f = EngineFixture::new();
    let request = f.inspected().await;
    f.profile.hold.store(true, Ordering::SeqCst);
    let (g, s, o, d) = (
        f.gates.clone(),
        f.store.clone(),
        f.obs.clone(),
        f.deps.clone(),
    );
    let caller = tokio::spawn(async move { g.takeover(s, o, request, d).await });
    tokio::time::timeout(Duration::from_secs(2), f.profile.entered.notified())
        .await
        .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let gates = f.gates.clone();
    let mut drain = tokio::spawn(async move { gates.shutdown_refreshes().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut drain)
            .await
            .is_err()
    );
    f.profile.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), drain)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(f.store.list().unwrap().is_empty());
    assert!(f.store.takeover_journals().unwrap().is_empty());
}

#[tokio::test]
async fn r6_started_takeover_fences_ordinary_force_expiry_and_current_access_without_token_post() {
    let f = EngineFixture::with_expiry(-1);
    let request = f.inspected().await;
    let Preparation::Commit(journal) =
        takeover::prepare(f.store.clone(), f.deps.clone(), request.clone())
            .await
            .unwrap()
    else {
        panic!("plan")
    };
    let journal = takeover::install(&f.store, &f.deps, *journal).unwrap();
    let mut account = f.store.load(&journal.account.id).unwrap();
    let tokens = f
        .store
        .takeover_tokens(&account, &journal.operation)
        .unwrap();
    let ready = f.store.takeover_receipt(&account.id).unwrap().unwrap();
    account.token_meta.refresh_uncertain = true;
    let started = Receipt {
        operation: journal.operation.clone(),
        state: ReceiptState::Started,
    };
    let account = f
        .store
        .save_takeover_receipt(
            &account,
            &tokens,
            Some(account.generation),
            Some(&ready),
            &started,
        )
        .unwrap();
    let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates
        .clone()
        .with_token_endpoint(Provider::Claude, fake.url.clone());
    assert!(
        gates
            .before_request(f.store.clone(), f.obs.clone(), account.id.clone())
            .await
            .is_err()
    );
    assert!(
        gates
            .manual_refresh(f.store.clone(), f.obs.clone(), account.id.clone(), true)
            .await
            .is_err()
    );
    let _ = gates
        .after_401(f.store.clone(), f.obs.clone(), account.clone(), &tokens)
        .await;
    assert!(
        gates
            .takeover(f.store.clone(), f.obs.clone(), request, f.deps.clone())
            .await
            .is_err()
    );
    assert!(fake.state.requests().is_empty());
    assert!(f.store.tokens(&account).is_err());
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn r6_fresh_login_discards_held_takeover_response_with_identical_access_and_drain_finishes() {
    let f = EngineFixture::new();
    let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
    let mut response = qb_testkit::ScriptedResponse::json(
        axum::http::StatusCode::OK,
        r#"{"access_token":"synthetic-access","refresh_token":"synthetic-late-rotation","expires_in":3600}"#,
    );
    response.chunks[0].0 = Duration::from_millis(400);
    fake.state.push(response);
    let gates = f
        .gates
        .clone()
        .with_token_endpoint(Provider::Claude, fake.url.clone());
    let request = f.inspected().await;
    let (g, s, o, d, r) = (
        gates.clone(),
        f.store.clone(),
        f.obs.clone(),
        f.deps.clone(),
        request.clone(),
    );
    let commit = tokio::spawn(async move { g.takeover(s, o, r, d).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    let mut account = f.store.list().unwrap().remove(0);
    assert_eq!(
        f.store
            .takeover_receipt(&account.id)
            .unwrap()
            .unwrap()
            .state,
        ReceiptState::Started
    );
    account.source = Source::Oauth;
    account.token_meta.refresh_uncertain = false;
    // Shorter expiry is valid evidence from a new authorization-code exchange.
    account.token_meta.expires_at = Some(chrono::Utc::now() + chrono::Duration::minutes(10));
    let login_tokens = Tokens {
        access_token: "synthetic-access".into(),
        refresh_token: "synthetic-fresh-family".into(),
        id_token: None,
    };
    let saved = crate::accounts::login::save(&f.store, &account, &login_tokens, false)
        .unwrap()
        .account;
    let result = commit.await.unwrap().unwrap();
    assert_eq!(result["refresh"]["ok"], false);
    let current = f.store.load(&saved.id).unwrap();
    assert_eq!(current.source, Source::Oauth);
    assert_eq!(
        f.store.tokens(&current).unwrap().refresh_token,
        "synthetic-fresh-family"
    );
    assert_eq!(
        gates
            .takeover(f.store.clone(), f.obs.clone(), request, f.deps.clone())
            .await
            .unwrap(),
        result
    );
    assert_eq!(fake.state.requests().len(), 1);
    tokio::time::timeout(Duration::from_secs(3), gates.shutdown_refreshes())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn r6_fresh_login_discards_pending_rotation_after_real_write_failure_and_drain_finishes() {
    use std::os::windows::fs::OpenOptionsExt;
    let f = EngineFixture::new();
    let request = f.inspected().await;
    let Preparation::Commit(journal) =
        takeover::prepare(f.store.clone(), f.deps.clone(), request.clone())
            .await
            .unwrap()
    else {
        panic!("plan")
    };
    let journal = takeover::install(&f.store, &f.deps, *journal).unwrap();
    let mut account = f.store.load(&journal.account.id).unwrap();
    let tokens = f
        .store
        .takeover_tokens(&account, &journal.operation)
        .unwrap();
    let before = f.store.takeover_receipt(&account.id).unwrap().unwrap();
    account.token_meta.refresh_uncertain = true;
    let started = Receipt {
        operation: journal.operation.clone(),
        state: ReceiptState::Started,
    };
    account = f
        .store
        .save_takeover_receipt(
            &account,
            &tokens,
            Some(account.generation),
            Some(&before),
            &started,
        )
        .unwrap();
    let mut rotated = account.clone();
    rotated.token_meta.refresh_uncertain = false;
    rotated.token_meta.last_refresh_at = Some(chrono::Utc::now());
    let rotation = Rotation {
        account: rotated,
        tokens: Tokens {
            access_token: tokens.access_token.clone(),
            refresh_token: "synthetic-held-rotated".into(),
            id_token: None,
        },
        failed_token: zeroize::Zeroizing::new(tokens.access_token.clone()),
        store_error_in_memory: false,
        takeover_operation: Some(journal.operation.clone()),
        box_request: None,
    };
    // A real deny-delete file handle makes the atomic replacement fail.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(
            f.store
                .home()
                .join("accounts")
                .join(format!("{}.json", account.id)),
        )
        .unwrap();
    assert!(persist_async(f.store.clone(), &rotation).await.is_err());
    assert_eq!(
        f.store
            .takeover_receipt(&account.id)
            .unwrap()
            .unwrap()
            .state,
        ReceiptState::Started
    );
    let gate = f.gates.gate(&account.id).await;
    gate.lock().await.pending = Some(rotation);
    drop(held);
    account.source = Source::Oauth;
    account.token_meta.refresh_uncertain = false;
    let login_tokens = Tokens {
        access_token: tokens.access_token.clone(),
        refresh_token: "synthetic-fresh-family".into(),
        id_token: None,
    };
    let saved = crate::accounts::login::save(&f.store, &account, &login_tokens, false)
        .unwrap()
        .account;
    let activity = f.gates.lifecycle.track_write();
    let (g, s, o, retry_gate) = (
        f.gates.clone(),
        f.store.clone(),
        f.obs.clone(),
        gate.clone(),
    );
    tokio::spawn(async move {
        let _activity = activity;
        g.retry_rotation(s, o, retry_gate).await;
    });
    tokio::time::timeout(Duration::from_secs(3), f.gates.shutdown_refreshes())
        .await
        .unwrap()
        .unwrap();
    assert!(gate.lock().await.pending.is_none());
    assert_eq!(
        f.store
            .tokens(&f.store.load(&saved.id).unwrap())
            .unwrap()
            .refresh_token,
        "synthetic-fresh-family"
    );
    assert_eq!(
        takeover::finalize(&f.store, journal).unwrap()["refresh"]["ok"],
        false
    );
}

struct HttpProfile {
    endpoint: String,
    client: reqwest::Client,
}
impl ProfileResolver for HttpProfile {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }
    fn claude<'a>(&'a self, access: &'a str) -> AsyncResult<'a, ProfileIdentity> {
        Box::pin(async move {
            let response = self
                .client
                .get(&self.endpoint)
                .bearer_auth(access)
                .send()
                .await
                .map_err(|_| TakeoverError::Unavailable)?;
            if !response.status().is_success() {
                return Err(TakeoverError::Invalid);
            }
            Ok(ProfileIdentity {
                identity: crate::accounts::Identity::Claude {
                    account_uuid: "engine-user".into(),
                    organization_uuid: Some("engine-org".into()),
                    email: "engine@example.invalid".into(),
                },
                email: "engine@example.invalid".into(),
                plan: None,
            })
        })
    }
}

#[tokio::test]
async fn r6_kept_claude_terminal_snapshot_inspects_after_old_profile_rejection_without_get_or_post()
{
    let f = EngineFixture::new();
    let profile_server = qb_testkit::ScriptedUpstream::start().await.unwrap();
    let profile = Arc::new(HttpProfile {
        endpoint: profile_server.url.clone(),
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    });
    let deps = Dependencies {
        source: f.deps.source.clone(),
        profile: profile.clone(),
    };
    profile_server
        .state
        .push(qb_testkit::ScriptedResponse::json(
            axum::http::StatusCode::OK,
            "{}",
        ));
    profile_server
        .state
        .push(qb_testkit::ScriptedResponse::json(
            axum::http::StatusCode::OK,
            "{}",
        ));
    let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
    fake.state.push(qb_testkit::ScriptedResponse::json(axum::http::StatusCode::OK,
        r#"{"access_token":"synthetic-rotated-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600}"#));
    let gates = f
        .gates
        .clone()
        .with_token_endpoint(Provider::Claude, fake.url.clone());
    let mut inspect = f.request(Phase::Inspect, None);
    inspect.keep_source = true;
    let first = gates
        .takeover(
            f.store.clone(),
            f.obs.clone(),
            inspect.clone(),
            deps.clone(),
        )
        .await
        .unwrap();
    let mut commit = inspect.clone();
    commit.phase = Phase::Commit;
    commit.inspect_digest = Some(first["inspect_digest"].as_str().unwrap().into());
    let completed = gates
        .takeover(f.store.clone(), f.obs.clone(), commit.clone(), deps.clone())
        .await
        .unwrap();
    assert_eq!(completed["refresh"]["ok"], true);
    assert_eq!(fake.state.requests().len(), 1);
    assert_eq!(profile_server.state.requests().len(), 2);
    // Demonstrate that the old credential now actually fails a loopback GET.
    profile_server
        .state
        .push(qb_testkit::ScriptedResponse::json(
            axum::http::StatusCode::UNAUTHORIZED,
            "{}",
        ));
    assert!(profile.claude("synthetic-access").await.is_err());
    let before = profile_server.state.requests().len();
    let again = gates
        .takeover(f.store.clone(), f.obs.clone(), inspect, deps.clone())
        .await
        .unwrap();
    assert_eq!(again["inspect_digest"], first["inspect_digest"]);
    commit.inspect_digest = Some(again["inspect_digest"].as_str().unwrap().into());
    assert_eq!(
        gates
            .takeover(f.store.clone(), f.obs.clone(), commit, deps)
            .await
            .unwrap(),
        completed
    );
    assert_eq!(profile_server.state.requests().len(), before);
    assert_eq!(fake.state.requests().len(), 1);
    let id = completed["id"].as_str().unwrap();
    assert_eq!(
        f.store
            .tokens(&f.store.load(id).unwrap())
            .unwrap()
            .refresh_token,
        "synthetic-rotated-refresh"
    );
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn r6_busy_takeover_gate_never_serves_source_cache_before_rotation_or_fresh_login() {
    use std::sync::atomic::AtomicUsize;
    struct HeldToken {
        entered: Notify,
        release: Notify,
        posts: AtomicUsize,
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        for supersede in [false, true] {
            let f = EngineFixture::new();
            let endpoint = Arc::new(HeldToken {
                entered: Notify::new(),
                release: Notify::new(),
                posts: AtomicUsize::new(0),
            });
            let app = axum::Router::new()
                .route(
                    "/token",
                    axum::routing::post(
                        |axum::extract::State(held): axum::extract::State<Arc<HeldToken>>| async move {
                            held.posts.fetch_add(1, Ordering::SeqCst);
                            held.entered.notify_one();
                            held.release.notified().await;
                            axum::Json(
                                serde_json::json!({"access_token":"synthetic-rotated-access",
                        "refresh_token":"synthetic-rotated-family","expires_in":3600}),
                            )
                        },
                    ),
                )
                .with_state(endpoint.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            assert!(!matches!(addr.port(), 10100 | 10101));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let gates = f
                .gates
                .clone()
                .with_token_endpoint(Provider::Claude, format!("http://{addr}/token"));
            let request = f.inspected().await;
            let (g, s, o, d) = (
                gates.clone(),
                f.store.clone(),
                f.obs.clone(),
                f.deps.clone(),
            );
            let mut commit = tokio::spawn(async move { g.takeover(s, o, request, d).await });
            tokio::select! {
                _ = endpoint.entered.notified() => {},
                result = &mut commit => match result {
                    Ok(Err(error)) => panic!(
                        "takeover failed before held token endpoint entry (supersede={supersede}): {error:?}"
                    ),
                    Ok(Ok(_)) => panic!(
                        "takeover completed before held token endpoint entry (supersede={supersede})"
                    ),
                    Err(error) => panic!(
                        "takeover task failed before held token endpoint entry (supersede={supersede}): {error}"
                    ),
                },
            }
            let mut account = f.store.list().unwrap().remove(0);
            assert_eq!(
                f.store
                    .takeover_receipt(&account.id)
                    .unwrap()
                    .unwrap()
                    .state,
                ReceiptState::Started
            );
            assert!(!gates.ahead_access.lock().unwrap().contains_key(&account.id));
            let waiting = gates.before_request(f.store.clone(), f.obs.clone(), account.id.clone());
            tokio::pin!(waiting);
            assert!(futures_util::poll!(waiting.as_mut()).is_pending());
            if supersede {
                account.source = Source::Oauth;
                account.token_meta.refresh_uncertain = false;
                let fresh = Tokens {
                    access_token: "synthetic-fresh-access".into(),
                    refresh_token: "synthetic-fresh-family".into(),
                    id_token: None,
                };
                crate::accounts::login::save(&f.store, &account, &fresh, false).unwrap();
                assert!(futures_util::poll!(waiting.as_mut()).is_pending());
            }
            endpoint.release.notify_one();
            assert_eq!(commit.await.unwrap().unwrap()["refresh"]["ok"], !supersede);
            let (served, tokens) = tokio::time::timeout(Duration::from_secs(3), waiting)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                tokens.access_token,
                if supersede {
                    "synthetic-fresh-access"
                } else {
                    "synthetic-rotated-access"
                }
            );
            assert_eq!(
                tokens.refresh_token,
                if supersede {
                    "synthetic-fresh-family"
                } else {
                    "synthetic-rotated-family"
                }
            );
            assert!(!f.store.takeover_fenced(&served.id).unwrap());
            assert_eq!(endpoint.posts.load(Ordering::SeqCst), 1);
            gates.shutdown_refreshes().await.unwrap();
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
        }
    })
    .await
    .expect("busy takeover gate regression exceeded whole-test 30s watchdog");
}
