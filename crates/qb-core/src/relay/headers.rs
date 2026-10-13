use axum::http::{HeaderMap, HeaderValue};

use crate::accounts::{Account, Identity, Provider};

fn per_hop(name: &str) -> bool {
    matches!(
        name,
        "connection" | "keep-alive" | "te" | "trailer" | "transfer-encoding" | "upgrade"
    ) || name.starts_with("proxy-")
        || name.starts_with("sec-websocket-")
}

pub(super) fn filtered(headers: &HeaderMap, request: bool) -> HeaderMap {
    let connection: Vec<_> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(str::trim))
        .collect();
    let mut result = HeaderMap::new();
    for (name, value) in headers {
        if per_hop(name.as_str())
            || connection
                .iter()
                .any(|hop| name.as_str().eq_ignore_ascii_case(hop))
            || (request
                && matches!(
                    name.as_str(),
                    "host" | "authorization" | "x-api-key" | "origin"
                ))
        {
            continue;
        }
        result.append(name.clone(), value.clone());
    }
    result
}

pub(super) fn upstream(
    headers: &HeaderMap,
    account: &Account,
    token: &str,
) -> Result<HeaderMap, ()> {
    let mut result = filtered(headers, true);
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| ())?;
    authorization.set_sensitive(true);
    result.insert("authorization", authorization);
    match account.provider {
        Provider::Claude => {
            let mut betas: Vec<_> = result.get_all("anthropic-beta").iter().cloned().collect();
            let present = betas.iter().any(|value| {
                value
                    .as_bytes()
                    .split(|b| *b == b',')
                    .any(|beta| beta.trim_ascii() == b"oauth-2025-04-20")
            });
            if !present {
                if let Some(last) = betas.last_mut() {
                    let mut value = last.as_bytes().to_vec();
                    if !value.is_empty() {
                        value.push(b',');
                    }
                    value.extend_from_slice(b"oauth-2025-04-20");
                    *last = HeaderValue::from_bytes(&value).map_err(|_| ())?;
                } else {
                    betas.push(HeaderValue::from_static("oauth-2025-04-20"));
                }
                result.remove("anthropic-beta");
                for value in betas {
                    result.append("anthropic-beta", value);
                }
            }
        }
        Provider::Codex => {
            let Identity::Codex { account_id, .. } = &account.identity else {
                return Err(());
            };
            result.insert(
                "chatgpt-account-id",
                HeaderValue::from_str(account_id).map_err(|_| ())?,
            );
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Health, Source, TokenMeta, UserState};

    fn account(provider: Provider) -> Account {
        let identity = match provider {
            Provider::Claude => Identity::Claude {
                account_uuid: "fixture".into(),
                organization_uuid: None,
                email: "fixture@example.invalid".into(),
            },
            Provider::Codex => Identity::Codex {
                user_id: None,
                account_id: "fixture-account".into(),
                email: "fixture@example.invalid".into(),
            },
        };
        Account {
            schema: 1,
            id: identity.id(),
            provider,
            label: "fixture".into(),
            email: "fixture@example.invalid".into(),
            plan: None,
            identity,
            source: Source::Oauth,
            user_state: UserState::Active,
            health: Health::Ready,
            auth_failures: 0,
            auth_probe_pending: false,
            quota_cooling: None,
            drain: false,
            allow_credits: false,
            generation: 1,
            token_meta: TokenMeta::default(),
        }
    }

    #[test]
    fn credentials_betas_and_dynamic_hop_headers() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer local-fixture"),
            ("x-api-key", "local-fixture"),
            ("host", "localhost"),
            ("origin", "fixture"),
            ("connection", "X-Private-Hop"),
            ("x-private-hop", "removed"),
            ("proxy-fixture", "removed"),
            ("sec-websocket-key", "removed"),
            ("anthropic-beta", "existing"),
            ("content-encoding", "zstd"),
            ("user-agent", "fixture/1"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        headers.append("anthropic-beta", HeaderValue::from_static("second"));
        let cl = account(Provider::Claude);
        let result = upstream(&headers, &cl, "upstream-fixture").unwrap();
        assert_eq!(result["authorization"], "Bearer upstream-fixture");
        for name in [
            "host",
            "x-api-key",
            "origin",
            "connection",
            "x-private-hop",
            "proxy-fixture",
            "sec-websocket-key",
        ] {
            assert!(!result.contains_key(name));
        }
        let betas: Vec<_> = result
            .get_all("anthropic-beta")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(betas, ["existing", "second,oauth-2025-04-20"]);
        assert_eq!(
            upstream(&result, &cl, "upstream-fixture")
                .unwrap()
                .get_all("anthropic-beta")
                .iter()
                .count(),
            2
        );
        let cx = upstream(&headers, &account(Provider::Codex), "upstream-fixture").unwrap();
        assert_eq!(cx["chatgpt-account-id"], "fixture-account");
        let response = filtered(&headers, false);
        assert_eq!(response["content-encoding"], "zstd");
        assert_eq!(response["user-agent"], "fixture/1");
        assert!(!response.contains_key("x-private-hop"));
    }
}
