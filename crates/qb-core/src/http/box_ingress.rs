//! Shared bound-listener and key-identity admission; no listener ownership here.
use super::{check_host_origin, error, single_header};
use crate::boxes::{BoxAuthority, BoxScope, RegistryError};
use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::Response,
};
use std::future::Future;
use zeroize::Zeroizing;

#[derive(Clone)]
struct BoxSafety<F> {
    scope: BoxScope,
    port: u16,
    authenticate: F,
}

/// Call ONLY inside BoxTransport's listener-derived Router factory. Pass the
/// ordinary unprotected relay Router, never a TCP-protected or admin Router.
/// Integration owns the callback's blocking work and joins it before lease drop.
pub(crate) fn protect_box_routes<F, Fut>(
    routes: Router,
    scope: BoxScope,
    box_visible_port: u16,
    authenticate: F,
) -> Router
where
    F: Fn(BoxScope, Zeroizing<String>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<BoxAuthority, RegistryError>> + Send + 'static,
{
    routes.layer(middleware::from_fn_with_state(
        BoxSafety {
            scope,
            port: box_visible_port,
            authenticate,
        },
        box_safety::<F, Fut>,
    ))
}

async fn box_safety<F, Fut>(
    State(state): State<BoxSafety<F>>,
    mut request: Request,
    next: Next,
) -> Response
where
    F: Fn(BoxScope, Zeroizing<String>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<BoxAuthority, RegistryError>> + Send + 'static,
{
    // Scope comes solely from state fixed by the selected listener. No header,
    // URL, lease token or preexisting extension supplies authentication evidence.
    clear_box_extensions(&mut request);
    if let Err(response) = check_host_origin(&request, state.port) {
        return response;
    }
    let Some(key) = box_credential(request.headers()) else {
        return invalid_credential(request.uri().path());
    };
    let authority = match (state.authenticate)(state.scope.clone(), key).await {
        Ok(authority) if authority.scope() == &state.scope && !authority.is_revoked() => authority,
        Ok(_) | Err(RegistryError::Unauthorized) => {
            return invalid_credential(request.uri().path());
        }
        Err(_) => {
            return authentication_unavailable(request.uri().path());
        }
    };
    run_box_request(request, next, authority).await
}

pub(super) fn clear_box_extensions(request: &mut Request) {
    request.extensions_mut().remove::<BoxAuthority>();
    request.extensions_mut().remove::<BoxScope>();
    request
        .extensions_mut()
        .remove::<crate::boxes::BoxRequest>();
}

pub(super) fn authentication_unavailable(path: &str) -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "api_error",
        "Box authentication unavailable",
        path,
    )
}

pub(super) async fn run_box_request(
    mut request: Request,
    next: Next,
    authority: BoxAuthority,
) -> Response {
    clear_box_extensions(&mut request);
    let body_revocation = authority.watch_revocation();
    let admitted = match authority.admit() {
        Ok(admitted) => admitted,
        Err(_) => return invalid_credential(request.uri().path()),
    };
    let mut revoked = authority.watch_revocation();
    let path = request.uri().path().to_owned();
    request.extensions_mut().insert(authority);
    request.extensions_mut().insert(admitted.clone());
    let response = crate::boxes::scope_request(admitted.clone(), async {
        tokio::select! {
            biased;
            _ = revoked.revoked() => invalid_credential(&path),
            response = next.run(request) => response,
        }
    })
    .await;
    let mut response = response;
    crate::boxes::retain_response(&mut response, admitted);
    let inner = std::mem::replace(response.body_mut(), axum::body::Body::empty());
    *response.body_mut() = axum::body::Body::new(RevocableBody {
        inner: Some(inner),
        revoked: Box::pin(async move {
            let mut revocation = body_revocation;
            revocation.revoked().await;
        }),
    });
    response
}

/// A pending body is woken by its own grant, without closing shared listeners
/// or interrupting the mandatory descendant work retained by RequestBody.
struct RevocableBody {
    inner: Option<axum::body::Body>,
    revoked: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl hyper::body::Body for RevocableBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        if self.inner.is_none() {
            return std::task::Poll::Ready(None);
        }
        if self.revoked.as_mut().poll(cx).is_ready() {
            // Dropping RequestBody runs its inner destructor in the exact scope.
            self.inner.take();
            return std::task::Poll::Ready(Some(Err(axum::Error::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Box request revoked",
            )))));
        }
        let result = hyper::body::Body::poll_frame(
            std::pin::Pin::new(self.inner.as_mut().expect("body present")),
            cx,
        );
        if matches!(&result, std::task::Poll::Ready(None | Some(Err(_)))) {
            self.inner.take();
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(hyper::body::Body::is_end_stream)
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner
            .as_ref()
            .map_or_else(hyper::body::SizeHint::new, hyper::body::Body::size_hint)
    }
}

fn invalid_credential(path: &str) -> Response {
    error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "Invalid box credential",
        path,
    )
}

pub(super) fn box_credential(headers: &HeaderMap) -> Option<Zeroizing<String>> {
    // Box-only strictness: exactly one credential field, including across both
    // accepted forms. Do not retrofit this rule into legacy TCP authorization.
    let count = headers.get_all("authorization").iter().count()
        + headers.get_all("x-api-key").iter().count();
    if count != 1 {
        return None;
    }
    let key = if headers.contains_key("authorization") {
        single_header(headers, "authorization")?.strip_prefix("Bearer ")?
    } else {
        single_header(headers, "x-api-key")?
    };
    if key.len() != 43
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(Zeroizing::new(key.to_owned()))
}

#[cfg(test)]
#[path = "box_ingress_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "box_ingress_key_tests.rs"]
mod key_tests;
