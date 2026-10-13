mod credentials;
mod headers;
mod hold;
mod ledger;

use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;

use crate::{
    accounts::{Account, AccountStore, Provider, UserState},
    config::Config,
    debug_pin::DebugPins,
    guard::{InflightBudget, RetainedBody, mib_bytes},
    observability::Observability,
    session,
};
use credentials::TokenGates;
pub use hold::CommitDecision;
use ledger::Completion;

pub struct RelayState {
    store: Arc<AccountStore>,
    client: reqwest::Client,
    anthropic_base: String,
    codex_base: String,
    pub budget: Arc<InflightBudget>,
    retain_cap: usize,
    observability: Arc<Observability>,
    pins: Arc<DebugPins>,
    tokens: TokenGates,
}

impl RelayState {
    pub fn new(
        store: Arc<AccountStore>,
        config: &Config,
        observability: Arc<Observability>,
        pins: Arc<DebugPins>,
    ) -> anyhow::Result<Self> {
        config.upstream.validate()?;
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(600))
            .build()
            .map_err(|_| anyhow::anyhow!("릴레이 HTTP 클라이언트를 만들 수 없습니다."))?;
        Ok(Self {
            store,
            client,
            anthropic_base: config.upstream.anthropic_base.trim_end_matches('/').into(),
            codex_base: config.upstream.codex_base.trim_end_matches('/').into(),
            budget: InflightBudget::new(mib_bytes(config.guard.inflight_budget_mib)),
            retain_cap: mib_bytes(config.guard.retain_max_per_request_mib),
            observability,
            pins,
            tokens: TokenGates::default(),
        })
    }

    async fn select(&self, provider: Provider, key: Option<&str>) -> Result<Option<Account>, ()> {
        let pinned = self.pins.get(provider, key);
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let accounts = store.list().map_err(|_| ())?;
            Ok(match pinned {
                Some(id) => accounts
                    .into_iter()
                    .find(|account| account.provider == provider && account.id == id),
                None => accounts.into_iter().find(|account| {
                    account.provider == provider && account.user_state == UserState::Active
                }),
            })
        })
        .await
        .map_err(|_| ())?
    }
}

pub fn router(state: Arc<RelayState>) -> Router {
    Router::new().fallback(relay).with_state(state)
}

fn route(method: &Method, path: &str, headers: &HeaderMap) -> Option<(Provider, String)> {
    match (method, path) {
        (&Method::POST, "/v1/messages" | "/v1/messages/count_tokens") => {
            Some((Provider::Claude, path.into()))
        }
        (&Method::GET, "/v1/models") if headers.contains_key("anthropic-version") => {
            Some((Provider::Claude, path.into()))
        }
        (&Method::GET, "/v1/models") => Some((Provider::Codex, "/models".into())),
        (
            &Method::POST,
            "/v1/responses"
            | "/v1/responses/compact"
            | "/v1/alpha/search"
            | "/v1/images/generations"
            | "/v1/images/edits",
        ) => Some((
            Provider::Codex,
            path.strip_prefix("/v1").expect("matched route").into(),
        )),
        _ => None,
    }
}

fn native_error(status: StatusCode, provider: Provider, kind: &str, message: &str) -> Response {
    let mut body = serde_json::json!({"error": {"type": kind, "message": message}});
    if provider == Provider::Claude {
        body["type"] = "error".into();
    }
    (status, Json(body)).into_response()
}

fn failure(
    completion: &mut Completion,
    status: StatusCode,
    provider: Provider,
    outcome: &str,
    message: &str,
) -> Response {
    completion.outcome(status.as_u16(), outcome);
    let kind = if status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND {
        "invalid_request_error"
    } else if provider == Provider::Claude {
        "api_error"
    } else {
        "server_error"
    };
    native_error(status, provider, kind, message)
}

async fn relay(State(state): State<Arc<RelayState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    let family = if path == "/v1/messages" || path.starts_with("/v1/messages/") {
        Provider::Claude
    } else {
        Provider::Codex
    };
    let routed = route(&parts.method, path, &parts.headers);
    let provider = routed.as_ref().map_or(family, |(family, _)| *family);
    let mut completion = Completion::new(
        state.observability.clone(),
        &parts.method,
        path,
        provider,
        &parts.headers,
    );
    let Some((_, upstream_path)) = routed else {
        return failure(
            &mut completion,
            StatusCode::NOT_FOUND,
            provider,
            "unsupported",
            "지원하지 않는 경로입니다.",
        );
    };
    // M2 임시 형태: admission 전 전체 raw body가 예산을 일시적으로 넘을 수 있다.
    // 예산은 retained 저장소의 admission 검사이며 hard heap cap은 별도 메모리 guard가 담당한다.
    let raw = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return failure(
                &mut completion,
                StatusCode::BAD_REQUEST,
                provider,
                "body_error",
                "요청 본문을 읽을 수 없습니다.",
            );
        }
    };
    completion.record.bytes_in = u64::try_from(raw.len()).unwrap_or(u64::MAX);
    let mut retained = RetainedBody::admit(&raw, state.retain_cap, &state.budget);
    completion.record.retained_copy = retained.is_some();
    let session = session::classify(provider, path, &parts.headers, &raw);
    completion.session(&session);
    state.observability.capture(&parts.headers, &raw);
    let account = match state.select(provider, session.key.as_deref()).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return failure(
                &mut completion,
                StatusCode::BAD_REQUEST,
                provider,
                "no_account",
                &format!(
                    "quota-baton: 사용 가능한 {} 계정이 없습니다. 트레이를 열어 주세요.",
                    provider.as_str()
                ),
            );
        }
        Err(()) => {
            return failure(
                &mut completion,
                StatusCode::BAD_GATEWAY,
                provider,
                "store_error",
                "계정 저장소를 읽을 수 없습니다.",
            );
        }
    };
    completion.record.account = Some(account.id.clone());
    let (mut account, mut tokens) = match state
        .tokens
        .before_request(state.store.clone(), state.observability.clone(), account.id)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(()) => {
            return failure(
                &mut completion,
                StatusCode::BAD_GATEWAY,
                provider,
                "store_error",
                "계정 토큰을 읽을 수 없습니다.",
            );
        }
    };
    let base = match provider {
        Provider::Claude => &state.anthropic_base,
        Provider::Codex => &state.codex_base,
    };
    let mut url = format!("{base}{upstream_path}");
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let mut body_to_send = raw;
    let mut retry_used = false;
    let mut auth_checked = false;
    let (status, response_headers, held) = loop {
        let upstream_headers =
            match headers::upstream(&parts.headers, &account, &tokens.access_token) {
                Ok(headers) => headers,
                Err(()) => {
                    return failure(
                        &mut completion,
                        StatusCode::BAD_GATEWAY,
                        provider,
                        "credential_header_error",
                        "업스트림 인증 헤더를 만들 수 없습니다.",
                    );
                }
            };
        completion.record.attempts += 1;
        // wrap_stream은 raw Bytes 저장소를 공유하며 인코딩이나 직렬화를 하지 않는다.
        let request_body =
            reqwest::Body::wrap_stream(futures_util::stream::once(std::future::ready(Ok::<
                _,
                std::io::Error,
            >(
                body_to_send,
            ))));
        let send = state
            .client
            .request(parts.method.clone(), &url)
            .headers(upstream_headers)
            .body(request_body)
            .send();
        let response = match tokio::time::timeout(Duration::from_secs(300), send).await {
            Ok(Ok(response)) => response,
            _ => {
                if !retry_used && let Some(copy) = &retained {
                    retry_used = true;
                    body_to_send = copy.bytes();
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                return failure(
                    &mut completion,
                    StatusCode::BAD_GATEWAY,
                    provider,
                    "upstream_error",
                    "업스트림에 연결할 수 없습니다.",
                );
            }
        };
        let status = response.status();
        completion.record.upstream_status = Some(status.as_u16());
        state
            .observability
            .observe_headers(&account.id, provider, response.headers());
        if status == StatusCode::UNAUTHORIZED && !auth_checked {
            auth_checked = true;
            let refreshed = state
                .tokens
                .after_401(
                    state.store.clone(),
                    state.observability.clone(),
                    account.clone(),
                    &tokens,
                )
                .await;
            match refreshed {
                Ok(Some((updated, new_tokens))) if !retry_used && retained.is_some() => {
                    retry_used = true;
                    account = updated;
                    tokens = new_tokens;
                    body_to_send = retained.as_ref().expect("retained retry").bytes();
                    continue;
                }
                // 보관본이 없으면 reload 결과와 무관하게 401을 그대로 relay한다.
                _ => {}
            }
        }
        let response_headers = response.headers().clone();
        let held = hold::hold(
            status,
            &response_headers,
            Box::pin(response.bytes_stream()),
            Duration::from_secs(10),
        )
        .await;
        if held.decision == CommitDecision::StreamError
            && held.prefix.is_empty()
            && !retry_used
            && let Some(copy) = &retained
        {
            retry_used = true;
            body_to_send = copy.bytes();
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        if held.decision == CommitDecision::StreamError && held.prefix.is_empty() {
            return failure(
                &mut completion,
                StatusCode::BAD_GATEWAY,
                provider,
                "upstream_error",
                "업스트림 응답을 읽을 수 없습니다.",
            );
        }
        break (status, response_headers, held);
    };
    // M4는 이 명시적 pre-commit 결정 지점에서 retained body로 failover를 연결한다.
    let decision = held.decision;
    drop(retained.take());
    completion.record.status = status.as_u16();
    completion.record.ttfb_ms = completion.elapsed_ms();
    let success_outcome = if decision == CommitDecision::FirstError {
        "first_event_error"
    } else if status.is_success() {
        "ok"
    } else {
        "upstream_status_error"
    };
    let mut output_headers = headers::filtered(&response_headers, false);
    if let Ok(value) = HeaderValue::from_str(&account.id) {
        output_headers.insert("x-quota-baton-account", value);
    }
    let stream = async_stream::stream! {
        // generator가 한 번도 poll되지 않아도 captured completion의 Drop이 기록한다.
        let mut completion = completion;
        let mut held = held;
        if !held.prefix.is_empty() {
            completion.record.bytes_out += held.prefix.len() as u64;
            yield Ok::<Bytes, std::io::Error>(held.prefix);
        }
        if let Some(remainder) = held.remainder {
            completion.record.bytes_out += remainder.len() as u64;
            yield Ok(remainder);
        }
        if let Some(error) = held.terminal {
            completion.record.outcome = "upstream_stream_error".into();
            yield Err(error);
        } else {
            while let Some(chunk) = held.stream.next().await {
                match chunk {
                    Ok(chunk) => { completion.record.bytes_out += chunk.len() as u64; yield Ok(chunk); }
                    Err(error) => {
                        completion.record.outcome = "upstream_stream_error".into();
                        yield Err(hold::sanitized_stream_error(error));
                        return;
                    }
                }
            }
            completion.record.outcome = success_outcome.into();
        }
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = output_headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_methods_families_paths() {
        let mut headers = HeaderMap::new();
        for path in ["/v1/messages", "/v1/messages/count_tokens"] {
            assert_eq!(
                route(&Method::POST, path, &headers),
                Some((Provider::Claude, path.into()))
            );
            assert!(route(&Method::GET, path, &headers).is_none());
        }
        for path in [
            "/v1/responses",
            "/v1/responses/compact",
            "/v1/alpha/search",
            "/v1/images/generations",
            "/v1/images/edits",
        ] {
            assert_eq!(
                route(&Method::POST, path, &headers),
                Some((Provider::Codex, path[3..].into()))
            );
        }
        assert_eq!(
            route(&Method::GET, "/v1/models", &headers),
            Some((Provider::Codex, "/models".into()))
        );
        headers.insert("anthropic-version", "fixture".parse().unwrap());
        assert_eq!(
            route(&Method::GET, "/v1/models", &headers),
            Some((Provider::Claude, "/v1/models".into()))
        );
        for path in ["/v1/memories/trace_summarize", "/unknown", "/v1/responses/"] {
            assert!(route(&Method::POST, path, &headers).is_none());
        }
    }
}
