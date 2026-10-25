//! Proposed admin::boxes child. Lifecycle backend deliberately NOT implemented.
use super::{error, issue_operation_entry};
use crate::{
    boxes::{BoxName, BoxRegistry, BoxScope, BoxSummary, RegistryError},
    issue_operation::IssueOperation,
};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use qb_proto::admin_boxes::{AddReply, BoxListReply, BoxView};
use serde::Deserialize;
use std::{future::Future, sync::Arc};
use zeroize::Zeroizing;

/// Exact proposed HTTP-facing service contract; no success/default backend.
/// Admitted work/cleanup remains supervisor-owned when a returned future drops.
pub(crate) trait BoxAdminService: Clone + Send + Sync + 'static {
    fn list(&self) -> impl Future<Output = Result<Vec<BoxSummary>, BoxAdminError>> + Send;
    fn add_for_operation(
        &self,
        name: BoxName,
        operation: IssueOperation,
    ) -> impl Future<Output = Result<Response, BoxAdminError>> + Send;
    fn revoke_for_operation(
        &self,
        name: BoxName,
        operation: IssueOperation,
    ) -> impl Future<Output = Result<BoxScope, BoxAdminError>> + Send;
}

pub(crate) enum BoxAdminError {
    Registry(RegistryError),
    Unavailable,
    Native,
}

impl From<RegistryError> for BoxAdminError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBoxRequest {
    name: String,
}

fn box_view(scope: &BoxScope, revoked: bool) -> BoxView {
    BoxView {
        name: scope.name().as_str().to_owned(),
        epoch: scope.epoch(),
        revoked,
    }
}

/// Merge only inside the existing host-admin safety layer. Never box ingress.
pub(crate) fn box_admin_routes<S: BoxAdminService>(service: S) -> Router {
    Router::new()
        .route(
            "/admin/v1/boxes",
            get(list::<S>).merge(
                post(add::<S>)
                    .layer(DefaultBodyLimit::max(1024))
                    .layer(middleware::from_fn(issue_operation_entry)),
            ),
        )
        .route(
            "/admin/v1/boxes/{name}/revoke",
            post(revoke::<S>).layer(middleware::from_fn(issue_operation_entry)),
        )
        .with_state(service)
        .layer(middleware::from_fn(no_store_entry))
}

#[derive(Clone)]
struct ScopedLaunchState {
    registry: BoxRegistry,
    relay: Arc<crate::relay::RelayState>,
    port: u16,
}

/// Host admin only: the path selects a current registry grant, never supplies
/// authority. The returned launch capability still requires the box client key.
pub(crate) fn scoped_launch_routes(
    registry: BoxRegistry,
    relay: Arc<crate::relay::RelayState>,
    port: u16,
) -> Router {
    Router::new()
        .route("/admin/v1/boxes/{name}/launches", post(scoped_launch))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn(issue_operation_entry))
        .layer(middleware::from_fn(no_store_entry))
        .with_state(ScopedLaunchState {
            registry,
            relay,
            port,
        })
}

async fn scoped_launch(
    State(state): State<ScopedLaunchState>,
    Extension(operation): Extension<IssueOperation>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    request: Result<Json<crate::relay::LaunchRequest>, JsonRejection>,
) -> Response {
    let (Ok(Path(name)), Ok(Json(request))) = (path, request) else {
        return failure(BoxAdminError::Registry(RegistryError::InvalidName));
    };
    let authority = match BoxName::parse(&name).and_then(|name| state.registry.try_authority(&name))
    {
        Ok(authority) => authority,
        Err(error) => return failure(error.into()),
    };
    match state
        .relay
        .issue_box_launch_for_operation(request, state.port, operation, state.registry, authority)
        .await
    {
        Ok(reply) => Json(reply).into_response(),
        Err(error) => (
            error.status(),
            Json(qb_proto::admin_launch::LaunchErrorReply {
                error: qb_proto::admin_launch::LaunchErrorDetail {
                    code: error.code().into(),
                    message: error.message().into(),
                },
            }),
        )
            .into_response(),
    }
}

async fn list<S: BoxAdminService>(State(service): State<S>) -> Response {
    no_store(match service.list().await {
        Ok(boxes) => Json(BoxListReply {
            boxes: boxes
                .into_iter()
                .map(|entry| box_view(&entry.scope, entry.revoked))
                .collect(),
        })
        .into_response(),
        Err(error) => failure(error),
    })
}

async fn add<S: BoxAdminService>(
    State(service): State<S>,
    Extension(operation): Extension<IssueOperation>,
    request: Result<Json<AddBoxRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = request else {
        return no_store(error(StatusCode::BAD_REQUEST, "Invalid box request"));
    };
    let name = match BoxName::parse(&request.name) {
        Ok(name) => name,
        Err(error) => return no_store(failure(error.into())),
    };
    no_store(match service.add_for_operation(name, operation).await {
        Ok(response) => response,
        Err(error) => failure(error),
    })
}

async fn revoke<S: BoxAdminService>(
    State(service): State<S>,
    Extension(operation): Extension<IssueOperation>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
) -> Response {
    let Ok(Path(name)) = path else {
        return no_store(error(StatusCode::BAD_REQUEST, "Invalid box name"));
    };
    let name = match BoxName::parse(&name) {
        Ok(name) => name,
        Err(error) => return no_store(failure(error.into())),
    };
    no_store(match service.revoke_for_operation(name, operation).await {
        // The service's success contract includes real native terminal cleanup.
        Ok(scope) => Json(box_view(&scope, true)).into_response(),
        Err(error) => failure(error),
    })
}

struct SecretBody(Zeroizing<Vec<u8>>);
impl AsRef<[u8]> for SecretBody {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Prepare outside publication locks. This neither admits nor publishes an add.
pub(crate) fn prepare_add_response(
    scope: &BoxScope,
    key: &Zeroizing<String>,
) -> Result<Response, BoxAdminError> {
    if key.len() != 43
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(BoxAdminError::Unavailable);
    }
    // Name <=63 ASCII bytes, key 43 bytes, epoch <=20 digits; this capacity
    // avoids growth/copies of the serialized secret buffer.
    let mut bytes = Zeroizing::new(Vec::with_capacity(256));
    serde_json::to_writer(
        &mut *bytes,
        &AddReply {
            name: scope.name().as_str(),
            epoch: scope.epoch(),
            client_key: key.as_str(),
        },
    )
    .map_err(|_| BoxAdminError::Unavailable)?;
    let bytes = bytes::Bytes::from_owner(SecretBody(bytes));
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::CREATED;
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    Ok(no_store(response))
}

fn failure(error_value: BoxAdminError) -> Response {
    let (status, message) = match error_value {
        BoxAdminError::Registry(RegistryError::InvalidName) => {
            (StatusCode::BAD_REQUEST, "Invalid box name")
        }
        BoxAdminError::Registry(RegistryError::AlreadyExists) => {
            (StatusCode::CONFLICT, "Box already exists")
        }
        BoxAdminError::Registry(RegistryError::Capacity) => {
            (StatusCode::CONFLICT, "Box capacity reached")
        }
        BoxAdminError::Registry(RegistryError::Unauthorized) => {
            (StatusCode::CONFLICT, "Box scope is no longer active")
        }
        BoxAdminError::Registry(RegistryError::NotFound) => {
            (StatusCode::NOT_FOUND, "Box was not found")
        }
        _ => (StatusCode::SERVICE_UNAVAILABLE, "Box operation unavailable"),
    };
    error(status, message)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("pragma", HeaderValue::from_static("no-cache"));
    response
}

async fn no_store_entry(request: axum::extract::Request, next: middleware::Next) -> Response {
    no_store(next.run(request).await)
}
