use super::*;
use crate::{
    accounts::{Account, Health, HealthReason, Identity, Source, TokenMeta, Tokens, UserState},
    catalog::{RefreshFailure, RefreshMode},
    relay::{LaunchError, RelayState},
};
use chrono::{Duration, Utc};
use serde_json::{Value, json};

struct Fixture {
    relay: Arc<RelayState>,
    _home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r30-admin-models-tests");
        std::fs::create_dir_all(&base).unwrap();
        let home = qb_testkit::fixture::home_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(crate::observability::Observability::new(home.path(), None).unwrap());
        let config = crate::config::Config::default();
        // No schedulers: all catalogs and credentials are synthetic.
        let relay = Arc::new(
            RelayState::new(
                store,
                &config,
                obs,
                Arc::new(crate::debug_pin::DebugPins::default()),
            )
            .unwrap(),
        );
        Self { relay, _home: home }
    }

    fn account(&self, provider: Provider, name: &str) -> Account {
        let email = format!("{name}@example.invalid");
        let identity = match provider {
            Provider::Claude => Identity::Claude {
                account_uuid: name.into(),
                organization_uuid: Some("synthetic-r30-org".into()),
                email: email.clone(),
            },
            Provider::Codex => Identity::Codex {
                account_id: name.into(),
                user_id: None,
                email: email.clone(),
            },
        };
        let account = Account {
            schema: 1,
            id: identity.id(),
            provider,
            label: name.into(),
            email,
            identity,
            plan: Some("pro".into()),
            plan_observed_at: None,
            subscription: None,
            source: Source::Oauth,
            user_state: UserState::Active,
            health: Health::Ready,
            auth_failures: 0,
            payment_failures: 0,
            payment_until: None,
            auth_probe_pending: false,
            quota_cooling: None,
            quota_credit_releasable: false,
            drain: false,
            allow_credits: false,
            generation: 0,
            token_meta: TokenMeta::default(),
        };
        self.relay
            .accounts
            .store
            .save(
                &account,
                &Tokens {
                    access_token: "synthetic-r30-access".into(),
                    refresh_token: "synthetic-r30-refresh".into(),
                    id_token: None,
                },
                None,
            )
            .unwrap()
    }

    fn catalog(&self, account: &Account, raw: Value) {
        self.relay
            .catalogs
            .begin_refresh(&account.id, RefreshMode::Force)
            .unwrap()
            .unwrap()
            .finish(Ok(raw))
            .unwrap();
    }

    async fn models(&self) -> AdminModels {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = super::super::runtime_routes(self.relay.clone(), 0);
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        // Use the existing router constructor without selecting OS-specific
        // state fields or adding a test-only transport dependency.
        let result = async {
            let response = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(2))
                .build()?
                .get(format!("http://{address}/admin/v1/models"))
                .send()
                .await?;
            let status = response.status();
            Ok::<_, reqwest::Error>((status, response.bytes().await?))
        }
        .await;
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let (status, body) = result.unwrap();
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }
}

fn model<'a>(reply: &'a AdminModels, provider: &str, id: &str) -> &'a AdminModel {
    reply
        .providers
        .iter()
        .find(|p| p.provider == provider)
        .unwrap()
        .models
        .iter()
        .find(|m| m.id == id)
        .unwrap()
}

#[tokio::test]
async fn each_provider_lists_only_entitled_accounts_and_scoped_hold_recovers() {
    let f = Fixture::new();
    let reset_at = Utc::now() + Duration::hours(1);
    for provider in [Provider::Claude, Provider::Codex] {
        let first = f.account(provider, &format!("{}-first", provider.as_str()));
        let second = f.account(provider, &format!("{}-second", provider.as_str()));
        let raw = |has_a| match provider {
            Provider::Claude => {
                if has_a {
                    json!({"data":[{"id":"model-a","display_name":"Model A"},{"id":"model-b"}]})
                } else {
                    json!({"data":[{"id":"model-b"}]})
                }
            }
            Provider::Codex => {
                if has_a {
                    json!({"models":[{"slug":"model-a","display_name":"Model A",
                        "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}],
                        "default_reasoning_level":"high"},{"slug":"model-b"}]})
                } else {
                    json!({"models":[{"slug":"model-b"}]})
                }
            }
        };
        f.catalog(&first, raw(false));
        f.catalog(&second, raw(true));
        let ready = f.models().await;
        let a = model(&ready, provider.as_str(), "model-a");
        assert_eq!(a.accounts.len(), 1);
        assert_eq!(a.accounts[0].id, second.id);
        assert!(a.accounts[0].usable_now);
        assert_eq!(a.accounts[0].limited_until, None);
        assert_eq!(a.display_name.as_deref(), Some("Model A"));
        if provider == Provider::Codex {
            assert_eq!(a.efforts, Some(vec!["low".into(), "high".into()]));
            assert_eq!(a.default_effort.as_deref(), Some("high"));
        } else {
            assert!(a.efforts.is_none() && a.default_effort.is_none());
        }
        f.relay
            .accounts
            .store
            .update_metadata(&second.id, |a| {
                a.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: reset_at,
                    scope: "model:model-a".into(),
                };
            })
            .unwrap();
        let limited = f.models().await;
        let a = model(&limited, provider.as_str(), "model-a");
        assert!(!a.accounts[0].usable_now);
        assert_eq!(a.accounts[0].limited_until, Some(reset_at));
        let b = model(&limited, provider.as_str(), "model-b");
        assert_eq!(b.accounts.len(), 2);
        assert!(
            b.accounts
                .iter()
                .all(|account| account.usable_now && account.limited_until.is_none())
        );
        assert_eq!(
            a.observed_at,
            model(&ready, provider.as_str(), "model-a").observed_at
        );
        f.relay
            .accounts
            .store
            .update_metadata(&second.id, |a| a.health = Health::Ready)
            .unwrap();
        assert!(model(&f.models().await, provider.as_str(), "model-a").accounts[0].usable_now);
    }
    std::thread::spawn(move || drop(f)).join().unwrap();
}

#[tokio::test]
async fn unassignable_and_unknown_accounts_contribute_nothing_failed_refresh_retains_facts() {
    let f = Fixture::new();
    for provider in [Provider::Claude, Provider::Codex] {
        let retained = f.account(provider, &format!("{}-retained", provider.as_str()));
        f.catalog(&retained, json!({"data":[{"id":"retained"}]}));
        let before = f.models().await;
        f.relay
            .catalogs
            .begin_refresh(&retained.id, RefreshMode::Force)
            .unwrap()
            .unwrap()
            .finish(Err(RefreshFailure::Upstream))
            .unwrap();
        let unknown = f.account(provider, &format!("{}-unknown", provider.as_str()));
        assert!(f.relay.catalogs.view(&unknown.id).unwrap().refresh_due);
        for excluded in ["disabled", "paused", "relogin", "draining"] {
            let account = f.account(provider, &format!("{}-{excluded}", provider.as_str()));
            f.catalog(
                &account,
                json!({"data":[{"id":"excluded"},{"id":"retained"}]}),
            );
            f.relay
                .accounts
                .store
                .update_metadata(&account.id, |a| match excluded {
                    "disabled" => a.user_state = UserState::Disabled,
                    "paused" => a.user_state = UserState::Paused,
                    "relogin" => {
                        a.health = Health::NeedsRelogin {
                            reason: "synthetic".into(),
                            since: Utc::now(),
                        }
                    }
                    "draining" => a.drain = true,
                    _ => unreachable!(),
                })
                .unwrap();
        }
        if provider == Provider::Claude {
            let free = f.account(provider, "claude-free");
            f.catalog(&free, json!({"data":[{"id":"excluded"},{"id":"retained"}]}));
            f.relay
                .accounts
                .store
                .update_metadata(&free.id, |a| a.plan = Some("free".into()))
                .unwrap();
        }
        let after = f.models().await;
        let group = after
            .providers
            .iter()
            .find(|p| p.provider == provider.as_str())
            .unwrap();
        assert_eq!(group.models.len(), 1);
        let model = &group.models[0];
        assert_eq!(model.id, "retained");
        assert_eq!(model.accounts.len(), 1);
        assert_eq!(model.accounts[0].id, retained.id);
        assert_eq!(
            model.observed_at,
            before
                .providers
                .iter()
                .find(|p| p.provider == provider.as_str())
                .unwrap()
                .models[0]
                .observed_at
        );
        assert!(
            !model.stale,
            "a just-failed refresh does not age the last success"
        );
    }
    std::thread::spawn(move || drop(f)).join().unwrap();
}

async fn launch_body(error: LaunchError) -> (StatusCode, Value) {
    let response = crate::admin::launch_error(error);
    (
        response.status(),
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn launch_model_limit_adds_exact_recovery_fields_preserving_other_errors() {
    let reset_at = Utc::now() + Duration::minutes(3);
    let (status, body) = launch_body(LaunchError::ModelLimited {
        retry_after: 180,
        earliest_account: "entitled-only".into(),
        reset_at,
    })
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({"error": {
            "code": "launch_unavailable", "message": LaunchError::Unavailable.message(),
            "reason": "model_limited", "retry_after": 180,
            "earliest_account": "entitled-only", "reset_at": reset_at,
        }})
    );
    for error in [
        LaunchError::Invalid,
        LaunchError::Unavailable,
        LaunchError::ConsentRequired,
        LaunchError::UnsupportedModel,
        LaunchError::Capacity,
        LaunchError::Store,
    ] {
        let expected = json!({"error": {"code": error.code(), "message": error.message()}});
        let expected_status = error.status();
        let (status, body) = launch_body(error).await;
        assert_eq!(status, expected_status);
        assert_eq!(body, expected);
    }
}
