use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use axum::http::{HeaderMap, Method};
use futures_util::{Sink, SinkExt, StreamExt, future::BoxFuture, stream::SplitSink};
use serde_json::Value;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{
    Message,
    protocol::{
        CloseFrame,
        frame::{
            Frame,
            coding::{Data, OpCode},
        },
    },
};

use super::{ClientSocket, UpstreamSocket};
use crate::{
    accounts::Provider::Codex,
    guard::{InflightBudget, Reservation},
    observability::{Observability, safe_error_code, safe_identifier},
    relay::ledger::Completion,
    session::Session,
};

const PARSE_LIMIT: usize = 1024 * 1024;
const TYPE_LIMIT: usize = 100;
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(600);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Context {
    pub runtime: Option<Arc<crate::account_runtime::AccountRuntime>>,
    pub relay: Option<Arc<crate::relay::RelayState>>,
    pub credentials: Option<(crate::accounts::Account, zeroize::Zeroizing<String>)>,
    pub budget: Arc<InflightBudget>,
    pub observability: Arc<Observability>,
    pub account: String,
    pub session: Session,
    pub headers: HeaderMap,
    pub idle_close: Duration,
    pub close_after_response: bool,
    pub capture_ticket: Option<PathBuf>,
    pub connection: String,
}

pub(in crate::relay) struct CurrentCompletion {
    completion: Completion,
    committed: bool,
    first_message: bool,
    model: Option<String>,
    response_id: Option<String>,
}

impl CurrentCompletion {
    fn new(context: &Context) -> Self {
        let mut completion = Completion::new(
            context.observability.clone(),
            &Method::GET,
            "/v1/responses",
            Codex,
            &context.headers,
        );
        completion.session(&context.session);
        completion.runtime = context.runtime.clone();
        if let Some(relay) = &context.relay {
            completion.track(relay.inflight_requests.clone());
            completion.binding = Some(
                relay
                    .bindings
                    .begin_request(context.session.key.as_deref(), &context.account),
            );
        }
        completion.record.transport = "ws".into();
        completion.record.account = Some(context.account.clone());
        completion.record.status = 101;
        completion.record.upstream_status = Some(101);
        completion.attempt(&context.account);
        Self {
            completion,
            committed: false,
            first_message: true,
            model: None,
            response_id: None,
        }
    }

    fn upstream(&mut self, event: Option<&Value>) -> Option<&'static str> {
        if let Some(usage) = event.and_then(crate::observability::cache_usage) {
            self.completion.record.cache_usage = Some(usage);
        }
        if self.first_message {
            self.completion.record.ttfb_ms = self.completion.elapsed_ms();
            self.first_message = false;
        }
        let kind = event
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str);
        let error = matches!(kind, Some("error" | "response.failed"));
        if error {
            self.completion.record.upstream_error_code = event.and_then(safe_error_code);
            if let Some(status) = event
                .and_then(|event| event.get("status"))
                .and_then(Value::as_u64)
                .filter(|status| (100..=599).contains(status))
            {
                self.completion.record.status = status as u16;
                self.completion.record.upstream_status = Some(status as u16);
            }
        } else if kind.is_some_and(|kind| kind.starts_with("response.")) {
            self.committed = true;
        }
        match kind {
            Some("response.completed" | "response.done") => Some("ok"),
            Some("error" | "response.failed") => Some("upstream_error"),
            _ => None,
        }
    }
}

// Only completion metadata is queued; the pump still holds one wire message
// per direction. Local IDs keep delivery bookkeeping stable across new creates.
fn response_completion(
    current: &mut BTreeMap<u64, CurrentCompletion>,
    event: Option<&Value>,
) -> Option<u64> {
    let kind = event.and_then(|event| event["type"].as_str());
    let response_id = event.and_then(|event| {
        event["response_id"]
            .as_str()
            .or_else(|| event["response"]["id"].as_str())
    });
    if let Some(response_id) = response_id {
        if let Some((&id, _)) = current
            .iter()
            .find(|(_, active)| active.response_id.as_deref() == Some(response_id))
        {
            return Some(id);
        }
        if kind == Some("response.created") {
            let (&id, active) = current
                .iter_mut()
                .find(|(_, active)| active.response_id.is_none())?;
            active.response_id = Some(response_id.into());
            return Some(id);
        }
        // An explicit unknown ID must not complete some other turn.
        return None;
    }
    if matches!(kind, Some("response.created" | "error" | "response.failed"))
        && let Some((&id, _)) = current.iter().find(|(_, active)| !active.committed)
    {
        return Some(id);
    }
    current.keys().next().copied()
}

// Error-frame auth reload uses the credential that opened the pinned socket.
// It never replays a response.create into a different upstream connection.
async fn apply_error(
    context: &Context,
    event: &Value,
    model: Option<&str>,
) -> Result<crate::relay::outcome::Decision, ()> {
    use crate::relay::outcome::{self, Kind};
    // The shared HTTP classifier decodes at most 64 KiB. Reduce an already
    // parsed WS error to its decision fields so unrelated large metadata cannot
    // hide a signal. The original message is still relayed unchanged.
    let root = event.get("response").unwrap_or(event);
    let error = root.get("error").unwrap_or(root);
    let mut bounded_error = serde_json::Map::new();
    for field in ["type", "code"] {
        if let Some(value) = error
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| value.len() <= 256)
        {
            bounded_error.insert(field.into(), Value::String(value.into()));
        }
    }
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        bounded_error.insert(
            "message".into(),
            Value::String(message.chars().take(8192).collect()),
        );
    }
    for field in ["resets_at", "resets_in_seconds"] {
        if let Some(value) = error.get(field).filter(|value| value.is_number()) {
            bounded_error.insert(field.into(), value.clone());
        }
    }
    let raw = serde_json::to_vec(
        &serde_json::json!({"type":event.get("type"),"status":event.get("status"),"error":bounded_error}),
    )
    .unwrap_or_default();
    let failed = context
        .credentials
        .as_ref()
        .map(|(_, token)| crate::accounts::Tokens {
            access_token: token.to_string(),
            refresh_token: String::new(),
            id_token: None,
        });
    let current_token = if event.get("status").and_then(Value::as_u64) == Some(401)
        || error.get("type").and_then(Value::as_str) == Some("authentication_error")
    {
        if let (Some(relay), Some((account, _)), Some(failed)) =
            (&context.relay, &context.credentials, &failed)
        {
            relay
                .tokens
                .is_current(relay.store.clone(), account, failed)
                .await?
        } else {
            true
        }
    } else {
        true
    };
    let mut decision = outcome::classify(
        Codex,
        axum::http::StatusCode::OK,
        &HeaderMap::new(),
        &raw,
        model,
    );
    if !current_token && matches!(decision.kind, Kind::Auth | Kind::Revoked) {
        decision.kind = Kind::Auth;
        decision.signal = None;
    }
    if decision.kind == Kind::Auth {
        if let (Some(relay), Some((account, _)), Some(failed)) =
            (&context.relay, &context.credentials, &failed)
        {
            if current_token
                && super::token_retried(relay, &account.id, &failed.access_token, false)
            {
                relay
                    .tokens
                    .auth_failed(
                        relay.store.clone(),
                        context.observability.clone(),
                        account.clone(),
                        failed,
                    )
                    .await?;
            } else {
                match relay
                    .tokens
                    .after_401(
                        relay.store.clone(),
                        context.observability.clone(),
                        account.clone(),
                        failed,
                    )
                    .await?
                {
                    Some((updated, refreshed)) => {
                        super::token_retried(relay, &updated.id, &refreshed.access_token, true);
                    }
                    None => {
                        relay
                            .tokens
                            .cool_unavailable_auth(relay.store.clone(), account, failed)
                            .await?;
                    }
                }
            }
        }
    } else if decision.kind == Kind::Revoked {
        if let (Some(relay), Some((account, _)), Some(failed)) =
            (&context.relay, &context.credentials, &failed)
        {
            relay
                .tokens
                .revoke_if_current(relay.store.clone(), account, failed)
                .await?;
        } else if let (Some(runtime), Some(signal)) = (&context.runtime, decision.signal.take()) {
            runtime.transition(&context.account, signal).await?;
        }
    } else if let (Some(runtime), Some(signal)) = (&context.runtime, decision.signal.take()) {
        runtime.transition(&context.account, signal).await?;
    }
    Ok(decision)
}

async fn reconnect_available(context: &Context) -> Result<bool, ()> {
    let Some(relay) = &context.relay else {
        return Ok(false);
    };
    let key = context.session.key.as_deref();
    // Without a stable key, a model-scoped or unchanged account could be
    // reselected immediately. Relay its frame instead of spending a retry.
    if key.is_none() && relay.accounts.eligible(&context.account, None).await {
        return Ok(false);
    }
    let tried = std::collections::BTreeSet::from([context.account.clone()]);
    if relay.select(Codex, key, None, &tried).await?.is_none() {
        return Ok(false);
    }
    relay.exclude_reconnect(key, &context.account);
    Ok(true)
}

fn event_affects_account(event: &Value, account: &str) -> bool {
    event
        .get("data")
        .and_then(|data| data.get("id"))
        .and_then(Value::as_str)
        == Some(account)
}

fn parsed(text: &str) -> Option<Value> {
    (text.len() <= PARSE_LIMIT)
        .then(|| serde_json::from_str(text).ok())
        .flatten()
}

fn response_create(text: &str) -> bool {
    if text.len() <= PARSE_LIMIT {
        return parsed(text).is_some_and(|value| value["type"] == "response.create");
    }
    // 큰 입력은 Codex의 선두 type 필드만 검사하고 입력 본문을 파싱하지 않는다.
    let prefix = &text.as_bytes()[..text.len().min(256)];
    let mut remaining = prefix;
    for token in [b"{".as_slice(), b"\"type\"", b":", b"\"response.create\""] {
        remaining = remaining.trim_ascii_start();
        let Some(rest) = remaining.strip_prefix(token) else {
            return false;
        };
        remaining = rest;
    }
    matches!(remaining.trim_ascii_start().first(), Some(b',' | b'}'))
}

fn count_type(counts: &mut BTreeMap<String, u64>, event: &Value) {
    let Some(kind) = event
        .get("type")
        .and_then(Value::as_str)
        .and_then(safe_identifier)
    else {
        return;
    };
    if let Some(count) = counts.get_mut(&kind) {
        *count = count.saturating_add(1);
    } else if counts.len() < TYPE_LIMIT {
        counts.insert(kind, 1);
    }
}

struct Delivered {
    bytes: usize,
    request: Option<u64>,
    terminal: Option<&'static str>,
    completed: bool,
}

type Write<S> = BoxFuture<'static, (S, bool, Delivered)>;

fn write<S>(
    mut sink: S,
    message: Message,
    reservation: Reservation,
    delivered: Delivered,
) -> Write<S>
where
    S: Sink<Message> + Send + Unpin + 'static,
    S::Error: Send,
{
    Box::pin(async move {
        let fragment = message.len() > PARSE_LIMIT
            && matches!(&message, Message::Text(_) | Message::Binary(_));
        let success = if fragment {
            let kind = if matches!(&message, Message::Text(_)) {
                Data::Text
            } else {
                Data::Binary
            };
            let bytes = message.into_data();
            let mut success = true;
            for start in (0..bytes.len()).step_by(64 * 1024) {
                let end = (start + 64 * 1024).min(bytes.len());
                let opcode = OpCode::Data(if start == 0 { kind } else { Data::Continue });
                if sink
                    .send(Message::Frame(Frame::message(
                        bytes.slice(start..end),
                        opcode,
                        end == bytes.len(),
                    )))
                    .await
                    .is_err()
                {
                    success = false;
                    break;
                }
                // 각 프레임 사이에 읽기와 자동 pong이 실행될 기회를 준다.
                tokio::task::yield_now().await;
            }
            success
        } else {
            sink.send(message).await.is_ok()
        };
        // flush가 끝나기 전에는 예산을 반환하지 않는다.
        drop(reservation);
        (sink, success, delivered)
    })
}

fn flush<S, M>(mut sink: S) -> Write<S>
where
    S: Sink<M> + Send + Unpin + 'static,
    S::Error: Send,
{
    Box::pin(async move {
        let success = sink.flush().await.is_ok();
        (
            sink,
            success,
            Delivered {
                bytes: 0,
                request: None,
                terminal: None,
                completed: false,
            },
        )
    })
}

pub(super) async fn run(client: ClientSocket, upstream: UpstreamSocket, context: Context) {
    run_with_read_timeout(client, upstream, context, UPSTREAM_READ_TIMEOUT).await;
}

async fn run_with_read_timeout(
    client: ClientSocket,
    upstream: UpstreamSocket,
    context: Context,
    upstream_read_timeout: Duration,
) {
    let (client_sink, mut client_read) = client.split();
    let (upstream_sink, mut upstream_read) = upstream.split();
    let mut client_sink = Some(client_sink);
    let mut upstream_sink = Some(upstream_sink);
    let mut to_client: Option<Write<SplitSink<ClientSocket, Message>>> = None;
    let mut to_upstream: Option<Write<SplitSink<UpstreamSocket, Message>>> = None;
    let mut current = BTreeMap::<u64, CurrentCompletion>::new();
    let mut next_request = 0;
    let mut model: Option<String> = None;
    let mut health_events = context.runtime.as_ref().map(|runtime| runtime.subscribe());
    let mut client_admitted = false;
    let mut terminal_pending = false;
    let mut counts = BTreeMap::new();
    let mut last_activity = Instant::now();
    let mut upstream_activity = last_activity;
    let mut ending: Option<(Option<CloseFrame>, &'static str)> = None;
    let mut end_deadline = last_activity;
    context.observability.record_ws_connection(
        &context.connection,
        &context.account,
        "ws_open",
        None,
        "open",
    );

    if let Some(runtime) = &context.runtime
        && !runtime.eligible(&context.account, None).await
    {
        ending = Some((
            Some(CloseFrame {
                code: 1000.into(),
                reason: "".into(),
            }),
            "account_ineligible",
        ));
        end_deadline = Instant::now() + CLOSE_TIMEOUT;
    }

    loop {
        if ending.is_some() && to_client.is_none() && to_upstream.is_none() {
            break;
        }
        // 각 방향은 메시지 하나만 보유한다. split 잠금은 poll 사이에 해제되어
        // 큰 쓰기가 진행되는 동안 같은 소켓의 읽기와 자동 pong을 poll할 수 있다.
        tokio::select! {
            result = async { to_client.as_mut().unwrap().await }, if to_client.is_some() => {
                let (sink, success, delivered) = result;
                client_sink = Some(sink);
                to_client = None;
                if success {
                    last_activity = Instant::now();
                    if let Some(active) = delivered.request.and_then(|id| current.get_mut(&id)) {
                        active.completion.record.bytes_out = active.completion.record.bytes_out.saturating_add(delivered.bytes as u64);
                    }
                    if let Some(outcome) = delivered.terminal {
                        terminal_pending = false;
                        if let Some(active) = delivered.request.and_then(|id| current.remove(&id)) {
                            if outcome == "ok" && let (Some(relay), Some((account, token))) = (&context.relay, &context.credentials) {
                                super::clear_retried_token(relay, &account.id, token);
                            }
                            active.completion.finish(outcome);
                        }
                        if current.is_empty() && ending.is_none() && let Some(runtime) = &context.runtime
                            && !runtime.eligible(&context.account, model.as_deref()).await {
                            ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "account_ineligible"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                        if current.is_empty() && context.close_after_response && delivered.completed && ending.is_none() {
                            ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "response_complete"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                    }
                } else if ending.is_none() {
                    ending = Some((None, "client_write_error"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                }
            }
            result = async { to_upstream.as_mut().unwrap().await }, if to_upstream.is_some() => {
                let (sink, success, _) = result;
                upstream_sink = Some(sink);
                to_upstream = None;
                if success { last_activity = Instant::now(); }
                if !success && ending.is_none() {
                    for active in current.values_mut() { active.completion.record.outcome = "upstream_error".into(); }
                    ending = Some((None, "upstream_write_error"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                }
            }
            _ = context.budget.wait_available(), if !client_admitted && to_upstream.is_none() && ending.is_none() => {
                client_admitted = true;
            }
            message = client_read.next(), if client_admitted && to_upstream.is_none() && ending.is_none() && !terminal_pending => {
                client_admitted = false;
                last_activity = Instant::now();
                match message {
                    Some(Ok(message)) => {
                        let mut request = current.keys().next().copied();
                        if let Message::Text(text) = &message && response_create(text.as_str()) {
                            model = parsed(text.as_str()).and_then(|value| value.get("model").and_then(Value::as_str).map(str::to_owned));
                            if let Some(runtime) = &context.runtime && !runtime.eligible(&context.account, model.as_deref()).await {
                                ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "account_ineligible"));
                                end_deadline = Instant::now() + CLOSE_TIMEOUT;
                                continue;
                            }
                            let mut active = CurrentCompletion::new(&context);
                            active.model = model.clone();
                            active.completion.record.model = model.clone();
                            request = Some(next_request);
                            current.insert(next_request, active);
                            next_request += 1;
                        }
                        let bytes = message.len();
                        // 라이브러리가 예약한 로컬 pong을 반대쪽 전달 완료와 독립적으로 flush한다.
                        if matches!(&message, Message::Ping(_)) && to_client.is_none() {
                            to_client = Some(flush(client_sink.take().unwrap()));
                        }
                        if let Some(active) = request.and_then(|id| current.get_mut(&id)) { active.completion.input_bytes.fetch_add(bytes as u64, Ordering::Relaxed); }
                        if let Message::Close(frame) = &message {
                            ending = Some((frame.clone(), "client_close"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                        to_upstream = Some(write(upstream_sink.take().unwrap(), message, context.budget.reserve_message(bytes), Delivered { bytes, request, terminal: None, completed: false }));
                    }
                    _ => { ending = Some((None, "client_eof")); end_deadline = Instant::now() + CLOSE_TIMEOUT; }
                }
            }
            message = upstream_read.next(), if to_client.is_none() && ending.is_none() => {
                last_activity = Instant::now();
                upstream_activity = last_activity;
                match message {
                    Some(Ok(message)) => {
                        let bytes = message.len();
                        if matches!(&message, Message::Ping(_)) && to_upstream.is_none() {
                            to_upstream = Some(flush(upstream_sink.take().unwrap()));
                        }
                        let event = if let Message::Text(text) = &message { parsed(text.as_str()) } else { None };
                        let request = response_completion(&mut current, event.as_ref());
                        if let Some(event) = event.as_ref() {
                            if let Some(runtime) = &context.runtime {
                                let active_model = request.and_then(|id| current.get(&id)).and_then(|active| active.model.as_deref());
                                runtime.observe_codex_event_for_model(&context.account, event, active_model).await;
                            } else {
                                context.observability.observe_codex_event(&context.account, event);
                            }
                            if matches!(event.get("type").and_then(Value::as_str), Some("error" | "response.failed")) {
                                let active_model = request.and_then(|id| current.get(&id)).and_then(|active| active.model.as_deref());
                                let decision = match apply_error(&context, event, active_model).await {
                                    Ok(decision) => Some(decision),
                                    Err(()) => {
                                        ending = Some((Some(CloseFrame { code: 1011.into(), reason: "".into() }), "health_store_error"));
                                        end_deadline = Instant::now() + CLOSE_TIMEOUT;
                                        // Forward the original error even when health persistence fails.
                                        None
                                    }
                                };
                                // Row 13a excludes row 3 rate limits and requires a current request.
                                if current.len() == 1 && request.and_then(|id| current.get(&id)).is_some_and(|active| !active.committed)
                                    && let Some(decision) = decision
                                    && decision.failover() && decision.kind != crate::relay::outcome::Kind::Rate
                                    && reconnect_available(&context).await.unwrap_or(false) {
                                    if let Some(mut active) = request.and_then(|id| current.remove(&id)) {
                                        active.upstream(Some(event));
                                        active.completion.failover(decision.reason());
                                        active.completion.finish("failover");
                                    }
                                    ending = Some((Some(CloseFrame { code: 1012.into(), reason: "".into() }), "precommit_failover"));
                                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                                    // C6 row 13a suppresses this pre-commit error.
                                    continue;
                                }
                            }
                            count_type(&mut counts, event);
                        }
                        let terminal = request.and_then(|id| current.get_mut(&id)).and_then(|active| active.upstream(event.as_ref()));
                        terminal_pending = terminal.is_some();
                        let completed = event.as_ref().is_some_and(|event| event["type"] == "response.completed");
                        if let Message::Close(frame) = &message {
                            ending = Some((frame.clone(), "upstream_close"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                        to_client = Some(write(client_sink.take().unwrap(), message, context.budget.reserve_message(bytes), Delivered { bytes, request, terminal, completed }));
                    }
                    _ => {
                        for active in current.values_mut() { active.completion.record.outcome = "upstream_error".into(); }
                        ending = Some((None, "upstream_eof")); end_deadline = Instant::now() + CLOSE_TIMEOUT;
                    }
                }
            }
            event = async { health_events.as_mut().unwrap().recv().await }, if health_events.is_some() && ending.is_none() => {
                if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                    health_events = None;
                }
                let relevant = match event {
                    Ok(event) => event_affects_account(&event, &context.account),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
                };
                if relevant && current.is_empty() && !terminal_pending && let Some(runtime) = &context.runtime
                    && !runtime.eligible(&context.account, model.as_deref()).await {
                    ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "account_ineligible"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                }
            }
            _ = tokio::time::sleep_until(last_activity + context.idle_close), if ending.is_none() && to_client.is_none() && to_upstream.is_none() => {
                ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "idle_close"));
                end_deadline = Instant::now() + CLOSE_TIMEOUT;
            }
            _ = tokio::time::sleep_until(upstream_activity + upstream_read_timeout), if ending.is_none() => {
                for active in current.values_mut() { active.completion.record.outcome = "upstream_error".into(); }
                ending = Some((Some(CloseFrame { code: 1001.into(), reason: "".into() }), "upstream_read_timeout"));
                end_deadline = Instant::now() + CLOSE_TIMEOUT;
            }
            _ = tokio::time::sleep_until(end_deadline), if ending.is_some() => { break; }
        }
    }
    let (frame, reason) = ending.unwrap_or((None, "closed"));
    let code = frame.as_ref().map(|frame| frame.code.into());
    // 종료 handshake도 느린 상대 때문에 무기한 소켓을 붙잡지 않는다.
    let close_client = async {
        if let Some(mut sink) = client_sink {
            if reason == "client_close" {
                let _ = sink.flush().await;
            } else {
                let _ = sink.send(Message::Close(frame.clone())).await;
            }
        }
    };
    let close_upstream = async {
        if let Some(mut sink) = upstream_sink {
            if reason == "upstream_close" {
                let _ = sink.flush().await;
            } else {
                let _ = sink.send(Message::Close(frame.clone())).await;
            }
        }
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        tokio::join!(close_client, close_upstream);
    })
    .await;
    drop(current);
    context
        .observability
        .capture_ws_types(context.capture_ticket.as_deref(), &counts);
    context.observability.record_ws_connection(
        &context.connection,
        &context.account,
        "ws_close",
        code,
        reason,
    );
}

#[cfg(test)]
mod tests;
