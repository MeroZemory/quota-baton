mod pump;

use super::*;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
    },
};
pub(super) type UpstreamSocket = WebSocketStream<reqwest::Upgraded>;
pub(super) type ClientSocket = WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

pub(super) async fn upgrade(state: Arc<RelayState>, mut request: Request) -> Response {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let key = request
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok());
    let valid = key
        .and_then(|v| STANDARD.decode(v).ok())
        .is_some_and(|v| v.len() == 16)
        && header_token(request.headers(), "connection", "upgrade")
        && request
            .headers()
            .get("sec-websocket-version")
            .is_some_and(|v| v == "13");
    if !valid {
        return native_error(
            StatusCode::BAD_REQUEST,
            Provider::Codex,
            "invalid_request_error",
            "Invalid WebSocket upgrade",
        );
    }
    let client_accept = derive_accept_key(key.expect("validated key").as_bytes());
    let client_upgrade = hyper::upgrade::on(&mut request);
    let (parts, _) = request.into_parts();
    let session = session::classify(Provider::Codex, "/v1/responses", &parts.headers, &[]);
    let account = match state.select(Provider::Codex, session.key.as_deref()).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return native_error(
                StatusCode::BAD_REQUEST,
                Provider::Codex,
                "invalid_request_error",
                "quota-baton: no usable codex account - open the quota-baton tray (사용 가능한 계정이 없습니다. 트레이를 여세요)",
            );
        }
        Err(_) => {
            return native_error(
                StatusCode::BAD_GATEWAY,
                Provider::Codex,
                "server_error",
                "Could not read account store",
            );
        }
    };
    let (mut account, mut tokens) = match state
        .tokens
        .before_request(state.store.clone(), state.observability.clone(), account.id)
        .await
    {
        Ok(value) => value,
        Err(_) => {
            return native_error(
                StatusCode::BAD_GATEWAY,
                Provider::Codex,
                "server_error",
                "Could not read account tokens",
            );
        }
    };
    let ticket = state.observability.capture_request(&parts.headers, &[]);
    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!("{}/responses{query}", state.codex_base);
    let limit = mib_bytes(state.websocket.max_message_size_mib);
    let config = WebSocketConfig::default()
        .max_message_size(Some(limit))
        .max_frame_size(Some(limit))
        .write_buffer_size(0);
    let mut reloaded = false;
    let mut network_retried = false;
    let (upstream, upstream_headers) = loop {
        let mut forwarded = match headers::upstream(&parts.headers, &account, &tokens.access_token)
        {
            Ok(headers) => headers,
            Err(_) => return network_failure(),
        };
        // 각 leg의 키와 확장 협상은 독립적이다. HTTP 클라이언트의 TLS 설정은 공유한다.
        let key = generate_key();
        forwarded.insert("connection", HeaderValue::from_static("Upgrade"));
        forwarded.insert("upgrade", HeaderValue::from_static("websocket"));
        forwarded.insert("sec-websocket-version", HeaderValue::from_static("13"));
        forwarded.insert(
            "sec-websocket-key",
            HeaderValue::from_str(&key).expect("generated key"),
        );
        let result = tokio::time::timeout(
            Duration::from_secs(300),
            state.client.get(&url).headers(forwarded).send(),
        )
        .await;
        match result {
            Ok(Ok(response)) => {
                state.observability.observe_headers(
                    &account.id,
                    Provider::Codex,
                    response.headers(),
                );
                if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                    let headers = response.headers().clone();
                    if headers.get("sec-websocket-accept").is_none_or(|v| {
                        v.as_bytes() != derive_accept_key(key.as_bytes()).as_bytes()
                    }) || !header_token(&headers, "upgrade", "websocket")
                        || !header_token(&headers, "connection", "upgrade")
                        || headers.contains_key("sec-websocket-extensions")
                        || headers.contains_key("sec-websocket-protocol")
                    {
                        return network_failure();
                    }
                    let stream = match response.upgrade().await {
                        Ok(stream) => stream,
                        Err(_) => return network_failure(),
                    };
                    break (
                        WebSocketStream::from_raw_socket(stream, Role::Client, Some(config)).await,
                        headers,
                    );
                }
                if response.status() == StatusCode::UNAUTHORIZED && !reloaded {
                    reloaded = true;
                    if let Ok(Some((updated, refreshed))) = state
                        .tokens
                        .after_401(
                            state.store.clone(),
                            state.observability.clone(),
                            account.clone(),
                            &tokens,
                        )
                        .await
                    {
                        account = updated;
                        tokens = refreshed;
                        continue;
                    }
                }
                return handshake_error(response).await;
            }
            _ if !network_retried => {
                network_retried = true;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            _ => return network_failure(),
        }
    };
    state
        .observability
        .capture_ws_response(ticket.as_deref(), &upstream_headers);
    let forwarded = headers::filtered(&upstream_headers, false);
    let close_after_response = {
        #[cfg(all(feature = "phase0", debug_assertions))]
        {
            state.close_after_response
        }
        #[cfg(not(all(feature = "phase0", debug_assertions)))]
        {
            false
        }
    };
    let context = pump::Context {
        budget: state.budget.clone(),
        observability: state.observability.clone(),
        account: account.id.clone(),
        session,
        headers: parts.headers,
        idle_close: Duration::from_secs(state.websocket.idle_close_secs),
        close_after_response,
        capture_ticket: ticket,
        connection: format!("{:032x}", rand::random::<u128>()),
    };
    tokio::spawn(async move {
        if let Ok(client) = client_upgrade.await {
            let client = WebSocketStream::from_raw_socket(
                hyper_util::rt::TokioIo::new(client),
                Role::Server,
                Some(config),
            )
            .await;
            pump::run(client, upstream, context).await;
        }
    });
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    response
        .headers_mut()
        .insert("connection", HeaderValue::from_static("Upgrade"));
    response
        .headers_mut()
        .insert("upgrade", HeaderValue::from_static("websocket"));
    response.headers_mut().insert(
        "sec-websocket-accept",
        HeaderValue::from_str(&client_accept).expect("generated accept"),
    );
    for (name, value) in &forwarded {
        response.headers_mut().append(name.clone(), value.clone());
    }
    if let Ok(value) = HeaderValue::from_str(&account.id) {
        response
            .headers_mut()
            .insert("x-quota-baton-account", value);
    }
    response
}

fn network_failure() -> Response {
    native_error(
        StatusCode::BAD_GATEWAY,
        Provider::Codex,
        "server_error",
        "Could not connect to upstream WebSocket",
    )
}

fn header_token(headers: &HeaderMap, name: &str, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(token))
}

async fn handshake_error(response: reqwest::Response) -> Response {
    let status = response.status();
    let response_headers = response.headers().clone();
    // 한도 분류용 앞부분만 보관한다. 뒤쪽은 원본 스트림으로 이어서 전달한다.
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    let mut raw = Vec::new();
    let mut remainder = None;
    while raw.len() < 65536 {
        match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                let take = chunk.len().min(65536 - raw.len());
                raw.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    remainder = Some(chunk.slice(take..));
                    break;
                }
            }
            Ok(None) => break,
            _ => return network_failure(),
        }
    }
    let parsed = hold::decoded_side(&response_headers, &raw)
        .and_then(|decoded| serde_json::from_slice::<serde_json::Value>(&decoded).ok());
    let usage_limit = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .is_some_and(|v| {
            ["type", "code"].iter().any(|field| {
                v.get(field).and_then(serde_json::Value::as_str) == Some("usage_limit_reached")
            })
        });
    if status == StatusCode::TOO_MANY_REQUESTS || usage_limit {
        let mut error = serde_json::json!({"error":{"type":"usage_limit_reached","message":"Upstream usage limit reached"}});
        for field in ["resets_at", "resets_in_seconds"] {
            if let Some(value) = parsed
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|v| v.get(field))
                .filter(|v| v.is_number())
            {
                error["error"][field] = value.clone();
            }
        }
        let mut result = (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response();
        for (name, value) in &headers::filtered(&response_headers, false) {
            if name != "content-length" && name != "content-encoding" && name != "content-type" {
                result.headers_mut().append(name.clone(), value.clone());
            }
        }
        return result;
    }
    // 계약상 426은 로컬 답변으로 내보내지 않는다.
    if status == StatusCode::UPGRADE_REQUIRED {
        return network_failure();
    }
    let output = async_stream::stream! {
        if !raw.is_empty() { yield Ok::<Bytes,std::io::Error>(raw.into()); }
        if let Some(chunk) = remainder { yield Ok(chunk); }
        while let Some(chunk) = stream.next().await { yield chunk.map_err(|_|std::io::Error::other("Upstream handshake body interrupted")); }
    };
    let mut result = Response::new(axum::body::Body::from_stream(output));
    *result.status_mut() = status;
    *result.headers_mut() = headers::filtered(&response_headers, false);
    result
}
