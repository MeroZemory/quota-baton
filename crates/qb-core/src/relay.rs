mod credentials;
mod headers;
mod hold;
mod ledger;
mod response;
mod upload;

use std::{sync::Arc, time::Duration};

#[cfg(all(feature = "phase0", debug_assertions))]
use axum::body::Body;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;

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
    #[cfg(all(feature = "phase0", debug_assertions))]
    body_capture: Option<Arc<crate::phase0::BodyCapture>>,
}

impl RelayState {
    pub fn new(
        store: Arc<AccountStore>,
        config: &Config,
        observability: Arc<Observability>,
        pins: Arc<DebugPins>,
    ) -> anyhow::Result<Self> {
        config.upstream.validate()?;
        let client = upstream_client()?;
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
            #[cfg(all(feature = "phase0", debug_assertions))]
            body_capture: None,
        })
    }

    #[cfg(all(feature = "phase0", debug_assertions))]
    pub fn with_body_capture(mut self, capture: Arc<crate::phase0::BodyCapture>) -> Self {
        self.body_capture = Some(capture);
        self
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

fn upstream_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
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
        .map_err(|_| anyhow::anyhow!("Could not create upstream HTTP client"))
}

pub fn router(state: Arc<RelayState>) -> Router {
    Router::new().fallback(relay).with_state(state)
}

#[cfg(all(feature = "phase0", debug_assertions))]
#[derive(Clone)]
struct ForcedAccount(String);

#[cfg(all(feature = "phase0", debug_assertions))]
#[derive(serde::Deserialize)]
pub struct ReplayRequest {
    pub capture_path: std::path::PathBuf,
    pub account_id: String,
    #[serde(default)]
    pub strip_encrypted: crate::phase0::StripMode,
    #[serde(default)]
    pub tamper_encrypted: bool,
}

#[cfg(all(feature = "phase0", debug_assertions))]
pub fn phase0_routes(state: Arc<RelayState>) -> Router {
    Router::new()
        .route(
            "/admin/v1/phase0/replay",
            axum::routing::post(replay_capture),
        )
        .with_state(state)
}

#[cfg(all(feature = "phase0", debug_assertions))]
async fn replay_capture(
    State(state): State<Arc<RelayState>>,
    payload: Result<Json<ReplayRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let result = async {
        let Json(payload) =
            payload.map_err(|_| anyhow::anyhow!("Invalid phase0 replay request"))?;
        let store = state.store.clone();
        let id = payload.account_id.clone();
        let (account, tokens) = tokio::task::spawn_blocking(move || {
            let account = store.load(&id)?;
            let tokens = store.tokens(&account)?;
            Ok::<_, crate::accounts::StoreError>((account, tokens))
        })
        .await??;
        let prepared = tokio::task::spawn_blocking(move || {
            crate::phase0::prepare(
                &payload.capture_path,
                account.provider,
                payload.strip_encrypted,
                payload.tamper_encrypted,
            )
        })
        .await??;
        // 내보내는 것은 암호화 항목의 개수뿐이며 본문과 원래 헤더는 로그에 넣지 않는다.
        tracing::info!(presence = %prepared.presence, "Phase 0 replay presence");
        let diagnostics = prepared.diagnostics;
        let mut forbidden = prepared.forbidden;
        forbidden.push(tokens.access_token.clone());
        forbidden.push(tokens.refresh_token.clone());
        if let Some(token) = &tokens.id_token {
            forbidden.push(token.clone());
        }
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(prepared.route)
            .body(Body::from(prepared.body))?;
        *request.headers_mut() = prepared.headers;
        request
            .extensions_mut()
            .insert(ForcedAccount(account.id.clone()));
        // 제품의 선택 이후 경로를 그대로 실행한다. pin 변경이나 별도 토큰 읽기 경로를 만들지 않는다.
        let response = relay(State(state.clone()), request).await;
        let store = state.store.clone();
        let id = account.id.clone();
        if let Ok(Ok(tokens)) = tokio::task::spawn_blocking(move || {
            let current = store.load(&id)?;
            store.tokens(&current)
        })
        .await
        {
            forbidden.push(tokens.access_token.clone());
            forbidden.push(tokens.refresh_token.clone());
            if let Some(token) = &tokens.id_token {
                forbidden.push(token.clone());
            }
        }
        let mut summary = crate::phase0::summarize(response, &account.id, &forbidden).await?;
        if let Some(fields) = diagnostics.as_object() {
            for (key, value) in fields {
                summary[key] = value.clone();
            }
        }
        Ok::<_, anyhow::Error>(summary)
    }
    .await;
    match result {
        Ok(summary) => Json(summary).into_response(),
        Err(_) => native_error(
            StatusCode::BAD_REQUEST,
            Provider::Codex,
            "invalid_request_error",
            "Phase 0 replay failed; check capture path and account",
        ),
    }
}

fn retry_body(bytes: Bytes) -> reqwest::Body {
    reqwest::Body::wrap_stream(futures_util::stream::once(std::future::ready(Ok::<
        _,
        std::io::Error,
    >(bytes))))
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
            "Unsupported route",
        );
    };
    let upload = match upload::read(body, state.retain_cap, completion.input_bytes.clone()).await {
        Ok(upload) => upload,
        Err(_) => {
            return failure(
                &mut completion,
                StatusCode::BAD_REQUEST,
                provider,
                "body_error",
                "Could not read request body",
            );
        }
    };
    let mut retained = if upload.overflow.is_none() {
        RetainedBody::admit(&upload.prefix, state.retain_cap, &state.budget)
    } else {
        None
    };
    completion.record.retained_copy = retained.is_some();
    let session = session::classify(provider, path, &parts.headers, &upload.prefix);
    completion.session(&session);
    #[cfg(all(feature = "shape-capture", debug_assertions))]
    let shape_ticket = {
        let observability = state.observability.clone();
        let headers = parts.headers.clone();
        let prefix = upload.prefix.clone();
        // 선택적 키 관찰은 런타임 스레드에서 큰 JSON을 파싱하지 않는다.
        tokio::task::spawn_blocking(move || observability.capture_request(&headers, &prefix))
            .await
            .ok()
            .flatten()
    };
    #[cfg(all(feature = "phase0", debug_assertions))]
    let selection = if let Some(ForcedAccount(id)) = parts.extensions.get::<ForcedAccount>() {
        let store = state.store.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            store
                .load(&id)
                .map(|account| (account.provider == provider).then_some(account))
                .map_err(|_| ())
        })
        .await
        .unwrap_or(Err(()))
    } else {
        state.select(provider, session.key.as_deref()).await
    };
    #[cfg(not(all(feature = "phase0", debug_assertions)))]
    let selection = state.select(provider, session.key.as_deref()).await;
    let account = match selection {
        Ok(Some(account)) => account,
        Ok(None) => {
            return failure(
                &mut completion,
                StatusCode::BAD_REQUEST,
                provider,
                "no_account",
                &format!(
                    "quota-baton: no usable {} account - open the quota-baton tray (사용 가능한 계정이 없습니다. 트레이를 여세요)",
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
                "Could not read account store",
            );
        }
    };
    completion.record.account = Some(account.id.clone());
    #[cfg(all(feature = "phase0", debug_assertions))]
    if upload.overflow.is_none()
        && parts.extensions.get::<ForcedAccount>().is_none()
        && let Some(capture) = &state.body_capture
    {
        let capture = capture.clone();
        let route = path.to_owned();
        let headers = parts.headers.clone();
        let account = account.id.clone();
        let raw = upload.prefix.clone();
        if !matches!(
            tokio::task::spawn_blocking(move || capture.record(&route, &headers, &account, &raw))
                .await,
            Ok(Ok(_))
        ) {
            tracing::warn!("Phase 0 body capture failed");
        }
    }
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
                "Could not read account tokens",
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
    let mut body_to_send = upload.into_body();
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
                        "Could not create upstream credential headers",
                    );
                }
            };
        completion.record.attempts += 1;
        let send = state
            .client
            .request(parts.method.clone(), &url)
            .headers(upstream_headers)
            .body(body_to_send)
            .send();
        let response = match tokio::time::timeout(Duration::from_secs(300), send).await {
            Ok(Ok(response)) => response,
            _ => {
                if !retry_used && let Some(copy) = &retained {
                    retry_used = true;
                    body_to_send = retry_body(copy.bytes());
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                return failure(
                    &mut completion,
                    StatusCode::BAD_GATEWAY,
                    provider,
                    "upstream_error",
                    "Could not connect to upstream",
                );
            }
        };
        let status = response.status();
        completion.record.upstream_status = Some(status.as_u16());
        state
            .observability
            .observe_headers(&account.id, provider, response.headers());
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        state
            .observability
            .capture_response(shape_ticket.as_deref(), response.headers());
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
                    body_to_send = retry_body(retained.as_ref().expect("retained retry").bytes());
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
            body_to_send = retry_body(copy.bytes());
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        if held.decision == CommitDecision::StreamError && held.prefix.is_empty() {
            return failure(
                &mut completion,
                StatusCode::BAD_GATEWAY,
                provider,
                "upstream_error",
                "Could not read upstream response",
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
    let body = response::body(completion, held, status, &response_headers, success_outcome);
    let mut response = Response::new(body);
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
