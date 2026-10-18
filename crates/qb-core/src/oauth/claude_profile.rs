//! Profile identity is source-confirmed; subscription fields below are R4 brief
//! candidates, not a verified provider schema. Never guess a Max multiplier.
use serde_json::Value;

pub(crate) fn plan(profile: &Value) -> Option<&'static str> {
    let organization = &profile["organization"];
    let kind = organization["organization_type"].as_str();
    // Organization subscriptions take precedence over personal flags.
    match kind {
        Some("team" | "claude_team") => return Some("team"),
        Some("enterprise" | "claude_enterprise") => return Some("enterprise"),
        _ => {}
    }
    if let Some(tier) = organization["rate_limit_tier"].as_str() {
        return match tier {
            "default_claude_max_5x" => Some("max_5x"),
            "default_claude_max_20x" => Some("max_20x"),
            "default_claude_pro" => Some("pro"),
            _ => {
                tracing::debug!(tier = %bounded_tier(tier), "Unknown Claude profile tier");
                None
            }
        };
    }
    if profile["account"]["has_claude_max"] == true || kind == Some("claude_max") {
        return None;
    }
    match kind {
        Some("claude_pro" | "pro") => Some("pro"),
        None if profile["account"]["has_claude_pro"] == true => Some("pro"),
        Some(other) => {
            tracing::debug!(tier = %bounded_tier(other), "Unknown Claude organization type");
            None
        }
        None => None,
    }
}

fn bounded_tier(raw: &str) -> String {
    raw.chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Tests must opt into numeric loopback; never resolve a hostname or dial CPA.
#[cfg(any(test, feature = "testkit"))]
pub(crate) fn synthetic_endpoint(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
    else {
        return false;
    };
    ip.is_loopback()
        && url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && !matches!(url.port_or_known_default(), Some(10100 | 10101))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn brief_candidate_shapes_map_without_inventing_multipliers() {
        // Synthetic candidate fixtures; CPA does NOT confirm these tier fields.
        for (shape, expected) in [
            (json!({"account":{"has_claude_pro":true}}), Some("pro")),
            (
                json!({"organization":{"organization_type":"claude_pro"}}),
                Some("pro"),
            ),
            (
                json!({"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_5x"}}),
                Some("max_5x"),
            ),
            (
                json!({"account":{"has_claude_max":true},"organization":{"rate_limit_tier":"default_claude_max_20x"}}),
                Some("max_20x"),
            ),
            (
                json!({"organization":{"organization_type":"team"}}),
                Some("team"),
            ),
            (
                json!({"organization":{"organization_type":"enterprise"}}),
                Some("enterprise"),
            ),
            (
                json!({"account":{"has_claude_max":true,"has_claude_pro":true}}),
                None,
            ),
            (
                json!({"account":{"has_claude_pro":true},"organization":{"rate_limit_tier":"default_claude_max_10x"}}),
                None,
            ),
            (
                json!({"organization":{"organization_type":"unknown"}}),
                None,
            ),
            (
                json!({"account":{"uuid":"synthetic","email":"synthetic@example.invalid"},"organization":{"uuid":"synthetic-org","name":"synthetic"}}),
                None,
            ),
            (json!({}), None),
        ] {
            assert_eq!(plan(&shape), expected);
        }
    }

    #[test]
    fn unknown_debug_value_is_bounded_and_cannot_inject_lines() {
        let raw = format!("unknown\n\r\t\u{1b}\"é{}", "x".repeat(500));
        let safe = bounded_tier(&raw);
        assert_eq!(safe.len(), 64);
        assert!(
            safe.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        );
    }

    #[tokio::test]
    async fn login_and_background_share_the_same_candidate_mapping() {
        use crate::accounts::Provider;
        use crate::oauth::{exchange, parameters::Parameters};
        use qb_testkit::{ScriptedResponse, ScriptedUpstream};
        let fake = ScriptedUpstream::start().await.unwrap();
        fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK,
            r#"{"access_token":"synthetic-login-access","refresh_token":"synthetic-login-refresh","expires_in":86400}"#));
        let profile = json!({"account":{"uuid":"synthetic-account","email":"synthetic@example.invalid"},
            "organization":{"uuid":"synthetic-org","rate_limit_tier":"default_claude_max_20x"}});
        fake.state.push(ScriptedResponse::json(
            reqwest::StatusCode::OK,
            profile.to_string(),
        ));
        let mut params = Parameters::new(Provider::Claude);
        params.token = format!("{}/synthetic-token", fake.url);
        params.profile = format!("{}/api/oauth/profile", fake.url);
        let (account, _) = exchange::exchange(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &params,
            "synthetic-code",
            "synthetic-verifier",
            "synthetic-state",
        )
        .await
        .unwrap();
        assert_eq!(account.plan.as_deref(), plan(&profile));
        assert_eq!(account.plan.as_deref(), Some("max_20x"));
        assert_eq!(fake.state.requests().len(), 2);
    }
}
