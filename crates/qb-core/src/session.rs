use axum::http::HeaderMap;
use serde::Deserialize;

use crate::accounts::Provider;

const SCAN_LIMIT: usize = 64 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub key: Option<String>,
    pub parent: Option<String>,
    pub model: Option<String>,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?.trim();
    (!value.is_empty() && values.next().is_none()).then_some(value)
}

pub fn classify(provider: Provider, path: &str, headers: &HeaderMap, body: &[u8]) -> Session {
    match provider {
        Provider::Codex => {
            #[derive(Deserialize)]
            struct Turn {
                session_id: Option<String>,
            }
            let sid = header(headers, "session-id")
                .or_else(|| header(headers, "session_id"))
                .map(str::to_owned)
                .or_else(|| {
                    let raw = header(headers, "x-codex-turn-metadata")?;
                    if raw.len() > SCAN_LIMIT {
                        return None;
                    }
                    serde_json::from_str::<Turn>(raw)
                        .ok()?
                        .session_id
                        .filter(|id| !id.is_empty())
                });
            Session {
                key: sid.map(|id| format!("codex:{id}")),
                ..Session::default()
            }
        }
        Provider::Claude => {
            let identity = header(headers, "content-encoding")
                .is_none_or(|value| value.eq_ignore_ascii_case("identity"));
            let peek = path == "/v1/messages" && identity;
            let sid = header(headers, "x-claude-code-session-id")
                .map(str::to_owned)
                .or_else(|| peek.then(|| tail_session(body)).flatten());
            let model = peek.then(|| head_model(body)).flatten();
            let Some(sid) = sid else {
                return Session {
                    model,
                    ..Session::default()
                };
            };
            let root = format!("claude:{sid}");
            match header(headers, "x-claude-code-agent-id") {
                None | Some("main") => Session {
                    key: Some(root),
                    parent: None,
                    model,
                },
                Some(agent) => {
                    let parent = match header(headers, "x-claude-code-parent-agent-id") {
                        None | Some("main") => root.clone(),
                        Some(parent) => format!("{root}:agent:{parent}"),
                    };
                    Session {
                        key: Some(format!("{root}:agent:{agent}")),
                        parent: Some(parent),
                        model,
                    }
                }
            }
        }
    }
}

fn whitespace(bytes: &[u8]) -> &[u8] {
    let offset = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[offset..]
}

// DOM 없이 값 하나만 읽는다. 미지 필드는 IgnoredAny로 건너뛴다.
fn value<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<(T, &'a [u8])> {
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<T>();
    let parsed = values.next()?.ok()?;
    Some((parsed, &bytes[values.byte_offset()..]))
}

fn head_model(body: &[u8]) -> Option<String> {
    let mut rest = whitespace(&body[..body.len().min(SCAN_LIMIT)]).strip_prefix(b"{")?;
    loop {
        let (key, next) = value::<String>(whitespace(rest))?;
        rest = whitespace(next).strip_prefix(b":")?;
        if key == "model" {
            return value::<String>(whitespace(rest)).map(|(model, _)| model);
        }
        rest = value::<serde::de::IgnoredAny>(whitespace(rest))?.1;
        rest = whitespace(rest).strip_prefix(b",")?;
    }
}

fn tail_session(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Metadata {
        user_id: Option<String>,
    }
    #[derive(Deserialize)]
    struct UserId {
        session_id: Option<String>,
    }
    let tail = &body[body.len().saturating_sub(SCAN_LIMIT)..];
    for (offset, window) in tail.windows(b"\"metadata\"".len()).enumerate().rev() {
        if window != b"\"metadata\"" {
            continue;
        }
        let Some(rest) = whitespace(&tail[offset + window.len()..]).strip_prefix(b":") else {
            continue;
        };
        let Some((metadata, mut rest)) = value::<Metadata>(whitespace(rest)) else {
            continue;
        };
        // 이후가 최상위 객체의 끝인지 확인해 중첩 metadata와 문자열 내부를 배제한다.
        loop {
            rest = whitespace(rest);
            if rest == b"}" {
                break;
            }
            let Some(next) = rest.strip_prefix(b",") else {
                break;
            };
            let Some((_, next)) = value::<String>(whitespace(next)) else {
                break;
            };
            let Some(next) = whitespace(next).strip_prefix(b":") else {
                break;
            };
            let Some((_, next)) = value::<serde::de::IgnoredAny>(whitespace(next)) else {
                break;
            };
            rest = next;
        }
        if whitespace(rest) != b"}" {
            continue;
        }
        let user = metadata.user_id?;
        let sid = serde_json::from_str::<UserId>(&user)
            .ok()
            .and_then(|user| user.session_id)
            .or_else(|| user.rsplit_once("_session_").map(|(_, id)| id.to_owned()));
        if let Some(sid) = sid.filter(|id| !id.is_empty()) {
            return Some(sid);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_root_agent_parent_and_bounded_peek() {
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-session-id", "root".parse().unwrap());
        headers.insert("x-claude-code-agent-id", "child".parse().unwrap());
        headers.insert("x-claude-code-parent-agent-id", "parent".parse().unwrap());
        let session = classify(
            Provider::Claude,
            "/v1/messages",
            &headers,
            br#"{"model":"fixture"}"#,
        );
        assert_eq!(session.key.as_deref(), Some("claude:root:agent:child"));
        assert_eq!(session.parent.as_deref(), Some("claude:root:agent:parent"));
        assert_eq!(session.model.as_deref(), Some("fixture"));
        headers.insert("content-encoding", "zstd".parse().unwrap());
        assert_eq!(
            classify(
                Provider::Claude,
                "/v1/messages",
                &headers,
                b"invalid compressed fixture"
            )
            .model,
            None
        );
    }

    #[test]
    fn claude_tail_scan_only_identity_messages() {
        let body = br#"{"model":"fixture","metadata":{"user_id":"{\"session_id\":\"tail\"}"},"max_tokens":4}"#;
        let headers = HeaderMap::new();
        assert_eq!(
            classify(Provider::Claude, "/v1/messages", &headers, body)
                .key
                .as_deref(),
            Some("claude:tail")
        );
        assert_eq!(
            classify(
                Provider::Claude,
                "/v1/messages/count_tokens",
                &headers,
                body
            )
            .key,
            None
        );
        assert_eq!(
            tail_session(br#"{"nested":{"metadata":{"user_id":"x_session_wrong"}}}"#),
            None
        );
        let mut long = br#"{"metadata":{"user_id":"x_session_old"},"padding":""#.to_vec();
        long.extend(std::iter::repeat_n(b'x', SCAN_LIMIT));
        long.extend_from_slice(b"\"}");
        assert_eq!(tail_session(&long), None);
        assert_eq!(
            head_model(br#"{"nested":{"model":"wrong"},"model":"right"}"#).as_deref(),
            Some("right")
        );
    }

    #[test]
    fn codex_headers_only_and_aliases() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-turn-metadata",
            r#"{"session_id":"fallback"}"#.parse().unwrap(),
        );
        assert_eq!(
            classify(Provider::Codex, "/v1/alpha/search", &headers, b"no JSON")
                .key
                .as_deref(),
            Some("codex:fallback")
        );
        headers.insert("session_id", "root".parse().unwrap());
        assert_eq!(
            classify(Provider::Codex, "/v1/responses", &headers, b"no JSON")
                .key
                .as_deref(),
            Some("codex:root")
        );
        assert_eq!(
            classify(
                Provider::Codex,
                "/v1/responses",
                &HeaderMap::new(),
                br#"{"session_id":"body"}"#
            ),
            Session::default()
        );
    }
}
