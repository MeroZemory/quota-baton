//! Final-response handoff ownership for deferred native reset obligations.
//! A blocking inspection may outlive its caller. Its result must cancel any
//! unpublished obligation when dropped, including a detached task's result.

#[cfg(test)]
#[path = "native_reset_tests.rs"]
mod tests;

use crate::{
    account_runtime::{AccountRuntime, DeferredResetId},
    launch_tokens::LaunchOwnerScope,
};
use std::sync::Arc;

use axum::{
    extract::{
        FromRequestParts, Request,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use qb_proto::native_reset::{Command, Event, MAX_MESSAGE_BYTES, Refusal, Reply, VERSION};
use tokio::time::{Duration, Instant, timeout};

use crate::{
    account_runtime::DeferredResetStatus,
    issue_operation::IssueOperation,
    launch_tokens::{LaunchOwnerLease, LaunchPin},
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// Both the original monotonic lower bound and the wall reset must pass.
/// A backwards wall adjustment must not park an otherwise idle owner forever
/// after its original monotonic wake. Capped rechecks also tolerate another
/// wall adjustment without spinning on an instant that is already in the past.
fn next_reset_wake(
    wake: Instant,
    reset: chrono::DateTime<chrono::Utc>,
    mono_now: Instant,
    wall_now: chrono::DateTime<chrono::Utc>,
) -> Option<Instant> {
    if mono_now < wake {
        return Some(wake);
    }
    let remaining = (reset - wall_now).to_std().ok()?;
    if remaining.is_zero() {
        return None;
    }
    mono_now.checked_add(remaining.min(Duration::from_secs(1)))
}

/// Authentication has already run on the local data listener. Only an issued
/// scoped path can open this owner; ordinary API traffic cannot create one.
pub(super) async fn upgrade(
    state: Arc<super::RelayState>,
    request: Request,
    pin: Option<Arc<LaunchPin>>,
) -> Response {
    if state.is_draining()
        || request.uri().query().is_some()
        || [
            "origin",
            "sec-websocket-extensions",
            "sec-websocket-protocol",
        ]
        .iter()
        .any(|name| request.headers().contains_key(*name))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(pin) = pin else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let (mut parts, _) = request.into_parts();
    let Ok(upgrade) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(owner) = pin.open_deferred_owner() else {
        return StatusCode::CONFLICT.into_response();
    };
    upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| control(state, socket, pin, owner))
}

async fn send(socket: &mut WebSocket, request_id: Option<u64>, event: Event) -> Result<(), ()> {
    let reply = Reply {
        version: VERSION,
        request_id,
        event,
    };
    let bytes = serde_json::to_string(&reply).map_err(|_| ())?;
    if bytes.len() > MAX_MESSAGE_BYTES || !reply.valid_shape() {
        return Err(());
    }
    timeout(CONTROL_TIMEOUT, socket.send(Message::Text(bytes.into())))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

async fn control(
    state: Arc<super::RelayState>,
    mut socket: WebSocket,
    _pin: Arc<LaunchPin>,
    owner: LaunchOwnerLease,
) {
    let scope = owner.scope();
    let mut thread: Option<String> = None;
    let mut last_request = 0;
    let mut due_announced = false;
    let mut grant_issued = false;
    loop {
        // Poll once to arm notifications BEFORE status inspection. A future
        // merely constructed but not polled would miss registration/recovery.
        let watch_reset = !grant_issued;
        let mut changed = Box::pin(async {
            if watch_reset {
                state.accounts.deferred_reset_changed(&scope).await;
            } else {
                // Consumption ends the episode, not the owner's transport.
                // Its terminal record would otherwise make this watcher ready
                // forever and close the socket immediately after Granted.
                std::future::pending::<()>().await;
            }
        });
        let changed_ready = futures_util::poll!(changed.as_mut()).is_ready();
        // Before Observe, inspect only the core's already-confirmed root for
        // terminal cleanup. A cancelled registered episode must not spin on an
        // immediately ready watcher while starving incoming Close/Observe.
        let status_thread = thread
            .as_deref()
            .or_else(|| scope.root().and_then(|root| root.strip_prefix("codex:")));
        let status = status_thread
            .filter(|_| !grant_issued)
            .map(|thread| state.accounts.deferred_reset_status(&scope, thread));
        let mut wake = None;
        if let Some(status) = status {
            match status {
                DeferredResetStatus::Waiting {
                    id,
                    reset_at,
                    wake: at,
                }
                | DeferredResetStatus::Suspended {
                    id,
                    reset_at,
                    wake: at,
                } => {
                    let mono_now = Instant::now();
                    let wall_now = chrono::Utc::now();
                    if mono_now >= at && wall_now >= reset_at {
                        if thread.is_some() && !due_announced {
                            if send(
                                &mut socket,
                                None,
                                Event::Due {
                                    episode: id.as_u64(),
                                },
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                            due_announced = true;
                        }
                    } else {
                        wake = next_reset_wake(at, reset_at, mono_now, wall_now);
                    }
                }
                DeferredResetStatus::Cancelled => break,
                DeferredResetStatus::Empty => {}
            }
        }
        if !scope.live() || state.is_draining() {
            break;
        }
        if changed_ready {
            // A completed notification future cannot watch later changes.
            // Re-arm before the next inspection instead of parking on the socket.
            continue;
        }
        let timer = async {
            if let Some(at) = wake {
                tokio::time::sleep_until(at).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let message = tokio::select! {
            biased;
            _ = state.wait_for_drain_start() => break,
            _ = &mut changed, if !changed_ready => { due_announced = false; continue; },
            _ = timer => continue,
            message = socket.next() => message,
        };
        let Some(Ok(message)) = message else { break };
        let bytes = match message {
            Message::Text(text) => text,
            Message::Ping(bytes) => {
                if !timeout(CONTROL_TIMEOUT, socket.send(Message::Pong(bytes)))
                    .await
                    .is_ok_and(|result| result.is_ok())
                {
                    break;
                }
                continue;
            }
            Message::Pong(_) => continue,
            _ => break,
        };
        if bytes.len() > MAX_MESSAGE_BYTES {
            break;
        }
        let Ok(request) = serde_json::from_str::<qb_proto::native_reset::Request>(&bytes) else {
            break;
        };
        if !request.valid_shape() || request.request_id <= last_request {
            break;
        }
        last_request = request.request_id;
        let event = match request.command {
            Command::Observe { thread_id } => {
                if !crate::launch_tokens::valid_native_thread(&thread_id)
                    || thread.as_ref().is_some_and(|current| current != &thread_id)
                {
                    break;
                }
                thread = Some(thread_id);
                match state
                    .accounts
                    .deferred_reset_status(&scope, thread.as_deref().unwrap_or_default())
                {
                    DeferredResetStatus::Waiting { id, reset_at, .. }
                    | DeferredResetStatus::Suspended { id, reset_at, .. } => Event::Waiting {
                        episode: id.as_u64(),
                        reset_unix_ms: reset_at.timestamp_millis(),
                    },
                    DeferredResetStatus::Empty => Event::Refused {
                        reason: Refusal::Unavailable,
                    },
                    DeferredResetStatus::Cancelled => Event::Refused {
                        reason: Refusal::Cancelled,
                    },
                }
            }
            Command::Cancel { episode } => {
                let id = DeferredResetId::from_u64(episode);
                if state.accounts.cancel_deferred_reset(&scope, id) {
                    Event::Cancelled { episode }
                } else {
                    Event::Refused {
                        reason: Refusal::Cancelled,
                    }
                }
            }
            command @ (Command::Inspect { .. } | Command::Consume { .. }) => {
                let (episode, consume) = match command {
                    Command::Inspect { episode } => (episode, false),
                    Command::Consume { episode } => (episode, true),
                    _ => unreachable!(),
                };
                if thread.is_none() {
                    break;
                }
                let operation = IssueOperation::new(Instant::now() + CONTROL_TIMEOUT);
                let _caller = operation.caller_guard();
                let work_operation = operation.clone();
                let work_scope = scope.clone();
                let accounts = state.accounts.clone();
                let bindings = state.bindings.clone();
                let mut task = tokio::task::spawn_blocking(move || {
                    if consume {
                        accounts
                            .consume_deferred_reset(
                                &bindings,
                                &work_scope,
                                DeferredResetId::from_u64(episode),
                                &work_operation,
                            )
                            .map(|permit| permit.is_some())
                    } else {
                        accounts.inspect_deferred_reset(
                            &bindings,
                            &work_scope,
                            DeferredResetId::from_u64(episode),
                            &work_operation,
                        )
                    }
                });
                // Any new control input while consuming terminates the owner.
                // In particular, a close/cancel must not wait behind Store I/O.
                let result = tokio::select! {
                    biased;
                    _ = state.wait_for_drain_start() => break,
                    _ = tokio::time::sleep_until(operation.deadline()) => break,
                    _ = socket.next() => break,
                    result = &mut task => result,
                };
                if !operation.can_reply() || !scope.live() {
                    break;
                }
                if matches!(result, Ok(Ok(true))) {
                    if consume {
                        // A single-use receipt must remain usable at the parent
                        // write boundary. Keep this same owner live until peer
                        // close/drain; terminal core state still denies repeats.
                        grant_issued = true;
                        Event::Granted { episode }
                    } else {
                        Event::Ready { episode }
                    }
                } else {
                    Event::Refused {
                        reason: Refusal::Unavailable,
                    }
                }
            }
        };
        if send(&mut socket, Some(last_request), event).await.is_err() {
            break;
        }
    }
    // Owner drop revokes every associated obligation even on failed writes or
    // unknown delivery. No reconnect or automatic replay can reopen this epoch.
    drop(owner);
}

pub(super) struct PendingPublication {
    accounts: Arc<AccountRuntime>,
    scope: LaunchOwnerScope,
    id: Option<DeferredResetId>,
}

impl PendingPublication {
    pub(super) fn new(
        accounts: Arc<AccountRuntime>,
        scope: LaunchOwnerScope,
        id: DeferredResetId,
    ) -> Self {
        Self {
            accounts,
            scope,
            id: Some(id),
        }
    }

    /// Only the final coherent fallback handoff keeps this obligation. This
    /// does not claim the client received it, and never grants a native command.
    pub(super) fn keep(mut self) {
        self.id.take();
    }
}

impl Drop for PendingPublication {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.accounts.cancel_deferred_reset(&self.scope, id);
        }
    }
}
