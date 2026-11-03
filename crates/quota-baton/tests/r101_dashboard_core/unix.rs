use qb_core::accounts::{
    Account, AccountStore, Health, Identity, Provider, Source, TokenMeta, Tokens, UserState,
};
use std::path::Path;

pub fn seed_account(store: &AccountStore, _external: &Path) {
    let email = "r101@example.invalid".to_owned();
    let identity = Identity::Codex {
        account_id: "r101-account".into(),
        user_id: None,
        email: email.clone(),
    };
    let account = Account {
        schema: 1,
        subscription: None,
        plan_observed_at: None,
        id: identity.id(),
        provider: Provider::Codex,
        label: email.clone(),
        email,
        plan: None,
        identity,
        source: Source::Oauth,
        user_state: UserState::Active,
        health: Health::Ready,
        auth_failures: 0,
        payment_failures: 0,
        payment_until: None,
        auth_probe_pending: false,
        quota_cooling: None,
        drain: false,
        allow_credits: false,
        quota_credit_releasable: false,
        generation: 0,
        token_meta: TokenMeta {
            expires_at: Some("2099-01-01T00:00:00Z".parse().unwrap()),
            last_refresh_at: Some("2026-10-21T00:00:00Z".parse().unwrap()),
            refresh_uncertain: false,
        },
    };
    let tokens = Tokens {
        access_token: "r101-synthetic-access".into(),
        refresh_token: "r101-synthetic-refresh".into(),
        id_token: None,
    };
    store.save(&account, &tokens, None).unwrap();
}

pub fn configure_command(_command: &mut tokio::process::Command) {}
