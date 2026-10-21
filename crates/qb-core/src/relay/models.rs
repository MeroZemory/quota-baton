//! Scoped Codex model menus: observe one binding, never select or adopt one.
use super::launch::{DataLaunchError, LaunchIngress};
use super::*;
use axum::body::Body;

pub(super) async fn serve(
    state: Arc<RelayState>,
    headers: &HeaderMap,
    ingress: &LaunchIngress,
    completion: &mut Completion,
) -> Response {
    completion.dashboard_enabled = false;
    match prepare(&state, headers, ingress, completion).await {
        Ok(response) => response,
        Err(error) => http_data::error(completion, error),
    }
}

async fn prepare(
    state: &Arc<RelayState>,
    headers: &HeaderMap,
    ingress: &LaunchIngress,
    completion: &mut Completion,
) -> Result<Response, DataLaunchError> {
    let deadline = tokio::time::Instant::from_std(
        completion
            .precommit_deadline
            .ok_or(DataLaunchError::Unavailable)?,
    );
    let operation = crate::issue_operation::IssueOperation::new(deadline);
    let caller = operation.caller_guard();
    if state.is_draining() || operation.check().is_err() {
        return Err(DataLaunchError::Unavailable);
    }
    let mut session = session::classify(Provider::Codex, "/v1/models", headers, &[]);
    let request_key = session.key.clone();
    let anchor = ingress
        .pin
        .as_ref()
        .map(|pin| pin.metadata().binding_anchor_key);
    let table = state.bindings.clone();
    // One data-lock snapshot gives existing C8 priority even if the token is
    // absent, unknown or expired. It does not prune, touch, adopt or reserve.
    let info = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            table.models_launch_snapshot(request_key.as_deref(), anchor.as_deref())
        }),
    )
    .await
    .map_err(|_| DataLaunchError::Unavailable)?
    .map_err(|_| DataLaunchError::Unavailable)?
    .map_err(binding_error)?;
    session.key = Some(info.canonical_key.clone());
    completion.session(&session);
    completion.record.account = Some(info.account.clone());
    // An ordinary group has no advertised-model storage today. Do not silently
    // promote it to managed (which changes subsequent data admission).
    if !info.managed {
        return Err(DataLaunchError::Unavailable);
    }
    let account_header =
        HeaderValue::from_str(&info.account).map_err(|_| DataLaunchError::Unavailable)?;
    // Reuse the shared owner, refresh serialization, bounded decoder and stale
    // bound-account policy. The request deadline encloses its own 15s deadline.
    let catalog =
        tokio::time::timeout_at(deadline, state.launch_catalog(&info.account, false, true))
            .await
            .map_err(|_| DataLaunchError::Unavailable)?
            .map_err(|_| DataLaunchError::Unavailable)?;
    let bytes = serde_json::to_vec(catalog.raw()).map_err(|_| DataLaunchError::Unavailable)?;
    if bytes.len() > crate::catalog::MAX_RAW_BYTES {
        return Err(DataLaunchError::Unavailable);
    }
    let table = state.bindings.clone();
    let key = info.canonical_key;
    let account = info.account;
    let models = catalog.supported().clone();
    let task_operation = operation.clone();
    let owner = Arc::new(());
    let weak_owner = Arc::downgrade(&owner);
    let weak_state = Arc::downgrade(state);
    // The blocking job never owns the request lifetime. Check after acquiring
    // mutations, before pruning or writing advertised metadata. No retry/shop.
    let admitted = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            table.record_models_advertised_for_operation(
                &key,
                &account,
                &models,
                chrono::Utc::now(),
                &task_operation,
                &|| {
                    weak_owner.upgrade().is_some()
                        && weak_state
                            .upgrade()
                            .is_some_and(|state| !state.is_draining())
                },
            )
        }),
    )
    .await
    .map_err(|_| DataLaunchError::Unavailable)?
    .map_err(|_| DataLaunchError::Unavailable)?
    .map_err(binding_error)?;
    drop(owner);
    if !operation.can_reply()
        || state.is_draining()
        || admitted.account != account_header.to_str().unwrap_or("")
    {
        return Err(DataLaunchError::Unavailable);
    }
    completion.record.bytes_out = bytes.len() as u64;
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response
        .headers_mut()
        .insert("x-quota-baton-account", account_header);
    completion.outcome(StatusCode::OK.as_u16(), "ok");
    caller.disarm();
    // relay() already wraps every scoped response with launch::pin_body; the
    // valid launch pin survives through the complete body/trailers or Drop.
    Ok(response)
}

fn binding_error(error: crate::bindings::LaunchError) -> DataLaunchError {
    match error {
        crate::bindings::LaunchError::UnknownLaunch => DataLaunchError::Relaunch,
        crate::bindings::LaunchError::BindingChanged => DataLaunchError::Changed,
        _ => DataLaunchError::Unavailable,
    }
}
