//! Claude adapter facts and finite outcomes; never retries a redemption.
use super::*;
use qb_proto::reset_credit_action::ClaudeResetResult;
use std::collections::BTreeMap;

pub(super) const DETAILS: &str = "/api/oauth/usage?cedar_ember=1&skip_spend=1";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Witness {
    resets_left: u64,
    clears: Vec<String>,
    percent_used: BTreeMap<String, serde_json::Number>,
}

pub(super) fn usage_path(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "/api/oauth/usage",
        Provider::Codex => "/backend-api/wham/usage",
    }
}

pub(super) fn choose<'a>(
    inventory: &'a ResetCreditInventory,
    id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<&'a ResetCreditGrant, ResetCreditRefusal> {
    if inventory.claude.is_some() {
        model::claude::choose(inventory, id, now)
    } else {
        let grant = model::choose(inventory, now)?;
        if id.is_some_and(|id| id != grant.id) {
            return Err(ResetCreditRefusal::NotUsable);
        }
        Ok(grant)
    }
}

pub(super) fn same_grant(
    current: &ResetCreditInventory,
    preview: &ResetCreditInventory,
    id: &str,
) -> bool {
    match (&current.claude, &preview.claude) {
        (None, None) => true,
        (Some(current), Some(preview)) => {
            let normalize = |facts: &qb_proto::reset_credits::ClaudeResetInventory| {
                facts
                    .grants
                    .iter()
                    .find(|grant| grant.id == id)
                    .cloned()
                    .map(|mut grant| {
                        // Traffic changes utilization, not the selected entitlement/effect.
                        grant.percent_used.clear();
                        grant
                    })
            };
            current.eligible == preview.eligible
                && current.cooldown_until == preview.cooldown_until
                && normalize(current).is_some()
                && normalize(current) == normalize(preview)
        }
        _ => false,
    }
}

pub(super) fn witness(inventory: &ResetCreditInventory, id: &str) -> Option<Witness> {
    let grant = inventory
        .claude
        .as_ref()?
        .grants
        .iter()
        .find(|grant| grant.id == id)?;
    Some(Witness {
        resets_left: grant.resets_left,
        clears: grant.clears.clone(),
        percent_used: grant.percent_used.clone(),
    })
}

pub(super) fn settled(fence: &Fence, inventory: &ResetCreditInventory) -> bool {
    let Some(before) = &fence.claude else {
        return false;
    };
    let Some(current) = inventory
        .claude
        .as_ref()
        .and_then(|facts| facts.grants.iter().find(|grant| grant.id == fence.grant))
    else {
        return false;
    };
    if inventory.state != ResetCreditState::Fresh {
        return false;
    }
    current.resets_left < before.resets_left
        || (!before.clears.is_empty()
            && before.clears.iter().any(|window| {
                before
                    .percent_used
                    .get(window)
                    .and_then(serde_json::Number::as_f64)
                    .is_some_and(|used| used > 0.)
            })
            && before.clears.iter().all(|window| {
                current
                    .percent_used
                    .get(window)
                    .and_then(serde_json::Number::as_f64)
                    == Some(0.)
            }))
}

pub(super) fn redeem_path(account: &Account) -> Result<String, Error> {
    let Identity::Claude {
        organization_uuid: Some(org),
        ..
    } = &account.identity
    else {
        return Err(Error::Ineligible);
    };
    // Identity supplies the organization component, never a URL or path.
    if org.is_empty()
        || org.len() > 128
        || !org
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err(Error::Ineligible);
    }
    Ok(format!("/api/organizations/{org}/reset_rate_limits"))
}

#[cfg(any(test, feature = "testkit"))]
pub(super) fn endpoint_allowed(url: &reqwest::Url) -> bool {
    reset_endpoint_allowed(url)
}

#[cfg(not(any(test, feature = "testkit")))]
pub(super) fn endpoint_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("api.anthropic.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
}

impl UsagePoller {
    async fn claude_reset_auth_refusal(&self, account: &Account, tokens: &crate::accounts::Tokens) {
        let store = self.store.clone();
        let account = account.clone();
        let token = zeroize::Zeroizing::new(tokens.access_token.clone());
        let _ = crate::boxes::spawn_blocking(move || {
            store.update_metadata_for_token(&account.id, &token, |current| {
                if current.identity == account.identity
                    && current.source == account.source
                    && current.provider == account.provider
                {
                    current.apply_health_signal(
                        crate::accounts::HealthSignal::Revoked {
                            reason: "reset_grant_auth_refused".into(),
                        },
                        Utc::now(),
                    );
                }
            })
        })
        .await;
    }

    pub(super) async fn claude_reset_response(
        &self,
        account: &Account,
        tokens: &crate::accounts::Tokens,
        response: reqwest::Response,
    ) -> (ResetCreditOutcome, Option<ClaudeResetResult>) {
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            let auth = status != reqwest::StatusCode::TOO_MANY_REQUESTS;
            if auth {
                self.claude_reset_auth_refusal(account, tokens).await;
            }
            return (
                ResetCreditOutcome::Refused,
                Some(ClaudeResetResult {
                    result: if auth { "ineligible" } else { "unavailable" }.into(),
                    reason: Some(
                        if auth {
                            "authentication_required"
                        } else {
                            "rate_limited"
                        }
                        .into(),
                    ),
                    resets_left: None,
                    cleared: Vec::new(),
                    weekly_resets_at: None,
                    cooldown_until: None,
                }),
            );
        }
        let Ok(body) = reset_body(response).await else {
            return (ResetCreditOutcome::Unknown, None);
        };
        // Native UI defaults unknown strings to unavailable. A mutation fence
        // must be stricter: malformed/indeterminate replies prove no outcome.
        if !matches!(
            body["result"].as_str(),
            Some(
                "reset"
                    | "already_used"
                    | "not_limited"
                    | "cooldown"
                    | "ineligible"
                    | "unavailable"
            )
        ) {
            return (ResetCreditOutcome::Unknown, None);
        }
        let Ok(reply) = model::claude::parse_result(&body) else {
            return (ResetCreditOutcome::Unknown, None);
        };
        let outcome = if matches!(
            reply.reason.as_deref(),
            Some("stamp_indeterminate" | "reset_unconfirmed")
        ) {
            ResetCreditOutcome::Unknown
        } else if reply.result == "reset" {
            ResetCreditOutcome::Reset
        } else {
            ResetCreditOutcome::Refused
        };
        (outcome, Some(reply))
    }
}
