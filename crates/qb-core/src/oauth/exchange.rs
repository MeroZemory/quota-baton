use super::{LoginError, parameters::Parameters};
use crate::accounts::{Account, Health, Identity, Provider, Source, TokenMeta, Tokens, UserState};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde::Deserialize;
use zeroize::Zeroizing;

#[derive(Deserialize)]
struct TokenReply {
    #[serde(flatten)]
    tokens: Tokens,
    expires_in: i64,
}

pub(super) async fn exchange(
    client: &reqwest::Client,
    params: &Parameters,
    code: &str,
    verifier: &str,
    state: &str,
) -> Result<(Account, Tokens), LoginError> {
    let redirect = params.redirect();
    let fields = [
        ("grant_type", "authorization_code"),
        ("client_id", params.client_id),
        ("code", code),
        ("redirect_uri", redirect.as_str()),
        ("code_verifier", verifier),
    ];
    let request = client
        .post(&params.token)
        .header(reqwest::header::USER_AGENT, user_agent())
        .header(reqwest::header::ACCEPT, "application/json");
    let request = match params.provider {
        Provider::Claude => request.json(&serde_json::json!({
            "grant_type": "authorization_code", "client_id": params.client_id,
            "code": code, "redirect_uri": redirect, "code_verifier": verifier, "state": state,
        })),
        Provider::Codex => request.form(&fields),
    };
    let bytes = body(
        request
            .send()
            .await
            .map_err(|_| LoginError::ExchangeFailed)?,
    )
    .await?;
    let reply: TokenReply =
        serde_json::from_slice(&bytes).map_err(|_| LoginError::ExchangeFailed)?;
    if reply.tokens.access_token.is_empty()
        || reply.tokens.refresh_token.is_empty()
        || reply.expires_in <= 0
    {
        return Err(LoginError::ExchangeFailed);
    }
    let (identity, email, plan) = match params.provider {
        Provider::Claude => {
            let response = client
                .get(&params.profile)
                .bearer_auth(&reply.tokens.access_token)
                .header(reqwest::header::USER_AGENT, user_agent())
                .header(reqwest::header::ACCEPT, "application/json")
                .header("anthropic-beta", "oauth-2025-04-20")
                .send()
                .await
                .map_err(|_| LoginError::ExchangeFailed)?;
            let bytes = body(response).await?;
            let profile: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| LoginError::InvalidIdentity)?;
            let account_uuid = required(&profile["account"], "uuid")?;
            let organization_uuid = required(&profile["organization"], "uuid")?;
            let email = required(&profile["account"], "email")?;
            (
                Identity::Claude {
                    account_uuid,
                    organization_uuid: Some(organization_uuid),
                    email: email.clone(),
                },
                email,
                None,
            )
        }
        Provider::Codex => {
            // Claims are used only to label/match an identity, never to authorize a request.
            let id_token = reply
                .tokens
                .id_token
                .as_ref()
                .ok_or(LoginError::InvalidIdentity)?;
            let parts: Vec<_> = id_token.split('.').collect();
            if parts.len() != 3 {
                return Err(LoginError::InvalidIdentity);
            }
            let payload = Zeroizing::new(
                URL_SAFE_NO_PAD
                    .decode(parts[1])
                    .map_err(|_| LoginError::InvalidIdentity)?,
            );
            let claims: serde_json::Value =
                serde_json::from_slice(&payload).map_err(|_| LoginError::InvalidIdentity)?;
            let auth = &claims["https://api.openai.com/auth"];
            let account_id = required(auth, "chatgpt_account_id")?;
            let user_id = required(auth, "chatgpt_user_id")?;
            let email = required(&claims, "email")?;
            let plan = auth["chatgpt_plan_type"].as_str().map(str::to_owned);
            (
                Identity::Codex {
                    account_id,
                    user_id: Some(user_id),
                    email: email.clone(),
                },
                email,
                plan,
            )
        }
    };
    let now = Utc::now();
    let expires_at = now
        .checked_add_signed(
            chrono::Duration::try_seconds(reply.expires_in).ok_or(LoginError::ExchangeFailed)?,
        )
        .ok_or(LoginError::ExchangeFailed)?;
    let account = Account {
        schema: 1,
        id: identity.id(),
        provider: params.provider,
        label: email.clone(),
        email,
        plan,
        identity,
        source: Source::Oauth,
        user_state: UserState::Active,
        health: Health::Ready,
        auth_failures: 0,
        auth_probe_pending: false,
        quota_cooling: None,
        drain: false,
        allow_credits: false,
        generation: 0,
        token_meta: TokenMeta {
            expires_at: Some(expires_at),
            last_refresh_at: Some(now),
            refresh_uncertain: false,
        },
    };
    Ok((account, reply.tokens))
}

fn required(value: &serde_json::Value, name: &str) -> Result<String, LoginError> {
    value[name]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(LoginError::InvalidIdentity)
}
fn user_agent() -> &'static str {
    concat!("quota-baton/", env!("CARGO_PKG_VERSION"))
}

async fn body(mut response: reqwest::Response) -> Result<Zeroizing<Vec<u8>>, LoginError> {
    if !response.status().is_success() {
        return Err(LoginError::ExchangeFailed);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| LoginError::ExchangeFailed)?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(LoginError::ExchangeFailed);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
