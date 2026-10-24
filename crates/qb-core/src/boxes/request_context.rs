//! Inherit an admitted box lifetime through its actual descendant tasks.
//! Revocation stops dispatch, never mandatory completion of an admitted write.
use super::BoxRequest;
use std::future::Future;
use tokio::task::JoinHandle;

tokio::task_local! {
    static REQUEST: Option<BoxRequest>;
}

pub(crate) fn current() -> Option<BoxRequest> {
    REQUEST.try_with(Clone::clone).ok().flatten()
}

pub(crate) async fn scope_request<F: Future>(request: BoxRequest, future: F) -> F::Output {
    REQUEST.scope(Some(request), future).await
}

/// Ordinary callers keep the original IssueOperation semantics. Scoped callers
/// serialize fresh admission with revoke, without cancelling admitted writes.
pub(crate) fn admit_operation(
    operation: &crate::issue_operation::IssueOperation,
) -> Result<(), ()> {
    match current() {
        Some(request) => request.admit_operation(operation),
        None => operation.admit(),
    }
}

pub(crate) fn dispatch_allowed() -> bool {
    REQUEST
        .try_with(|request| request.as_ref().is_none_or(|request| !request.is_revoked()))
        .unwrap_or(true)
}

pub(crate) fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let request = current();
    match request {
        Some(request) => tokio::spawn(REQUEST.scope(Some(request), future)),
        None => tokio::spawn(future),
    }
}

pub(crate) fn spawn_blocking<F, R>(work: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let request = current();
    match request {
        Some(request) => {
            tokio::task::spawn_blocking(move || REQUEST.sync_scope(Some(request), work))
        }
        None => tokio::task::spawn_blocking(work),
    }
}

/// Polling a body may itself create descendant work. Preserve all frames and
/// trailers and retain the same admitted lifetime until EOF, error or Drop.
pub(crate) fn retain_response(response: &mut axum::response::Response, request: BoxRequest) {
    let inner = std::mem::replace(response.body_mut(), axum::body::Body::empty());
    *response.body_mut() = axum::body::Body::new(RequestBody {
        inner: Some(inner),
        request: Some(request),
    });
}

struct RequestBody {
    inner: Option<axum::body::Body>,
    request: Option<BoxRequest>,
}

impl hyper::body::Body for RequestBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let request = self.request.clone();
        let Some(inner) = self.inner.as_mut() else {
            return std::task::Poll::Ready(None);
        };
        let result = REQUEST.sync_scope(request, || {
            hyper::body::Body::poll_frame(std::pin::Pin::new(inner), cx)
        });
        if matches!(&result, std::task::Poll::Ready(None | Some(Err(_)))) {
            let inner = self.inner.take();
            REQUEST.sync_scope(self.request.clone(), || drop(inner));
            self.request.take();
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

impl Drop for RequestBody {
    fn drop(&mut self) {
        let inner = self.inner.take();
        REQUEST.sync_scope(self.request.clone(), || drop(inner));
    }
}

#[cfg(test)]
#[path = "request_context_tests.rs"]
mod tests;
