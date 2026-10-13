pub mod websocket;
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: Method,
    pub path_and_query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Clone)]
pub struct ScriptedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub chunks: Vec<(Duration, Bytes)>,
}

impl ScriptedResponse {
    pub fn json(status: StatusCode, body: impl Into<Bytes>) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        Self {
            status,
            headers,
            chunks: vec![(Duration::ZERO, body.into())],
        }
    }
    pub fn sse(chunks: Vec<(Duration, Bytes)>) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        Self {
            status: StatusCode::OK,
            headers,
            chunks,
        }
    }
}

#[derive(Clone, Default)]
pub struct ScriptedState {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    window_limiter: Arc<Mutex<Option<WindowLimiter>>>,
}

#[derive(Clone)]
struct WindowLimiter {
    limit: u32,
    window: Duration,
    buckets: Arc<Mutex<std::collections::HashMap<String, (Instant, u32)>>>,
    round_429: Arc<Mutex<Vec<u64>>>,
}

impl ScriptedState {
    pub fn push(&self, response: ScriptedResponse) {
        self.responses.lock().unwrap().push_back(response);
    }
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
    /// Enable a real-time per-credential request window for synthetic storm tests.
    /// The credential is taken from the upstream Authorization header; its value
    /// never leaves the test process or appears in test output.
    pub fn enable_window_limiter(&self, limit: u32, window: Duration) {
        *self.window_limiter.lock().unwrap() = Some(WindowLimiter {
            limit,
            window,
            buckets: Arc::new(Mutex::new(std::collections::HashMap::new())),
            round_429: Arc::new(Mutex::new(Vec::new())),
        });
    }
    pub fn begin_limiter_round(&self) {
        if let Some(limiter) = self.window_limiter.lock().unwrap().as_ref() {
            limiter.round_429.lock().unwrap().push(0);
        }
    }
    pub fn limiter_round_429_counts(&self) -> Vec<u64> {
        self.window_limiter
            .lock()
            .unwrap()
            .as_ref()
            .map(|limiter| limiter.round_429.lock().unwrap().clone())
            .unwrap_or_default()
    }
    fn limiter_response(&self, headers: &HeaderMap) -> Option<ScriptedResponse> {
        let limiter = self.window_limiter.lock().unwrap().clone()?;
        let key = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("missing")
            .to_owned();
        let now = Instant::now();
        let mut buckets = limiter.buckets.lock().unwrap();
        let bucket = buckets.entry(key).or_insert((now, 0));
        if now.duration_since(bucket.0) >= limiter.window {
            *bucket = (now, 0);
        }
        bucket.1 += 1;
        if bucket.1 <= limiter.limit {
            Some(ScriptedResponse::json(StatusCode::OK, "{\"ok\":true}"))
        } else {
            if let Some(count) = limiter.round_429.lock().unwrap().last_mut() {
                *count += 1;
            }
            Some(ScriptedResponse::json(
                StatusCode::TOO_MANY_REQUESTS,
                "{\"detail\":\"Rate limit exceeded\"}",
            ))
        }
    }
    pub fn router(&self) -> axum::Router {
        axum::Router::new()
            .fallback(scripted)
            .with_state(self.clone())
    }
}

async fn scripted(State(state): State<ScriptedState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap();
    state.requests.lock().unwrap().push(RecordedRequest {
        method: parts.method,
        path_and_query: parts.uri.path_and_query().unwrap().as_str().into(),
        headers: parts.headers.clone(),
        body,
    });
    let response = state.limiter_response(&parts.headers).unwrap_or_else(|| {
        state
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ScriptedResponse::json(StatusCode::OK, "{\"fake\":true}"))
    });
    let stream = async_stream::stream! {
        for (delay,bytes) in response.chunks {
            tokio::time::sleep(delay).await;
            yield Ok::<Bytes,std::io::Error>(bytes);
        }
    };
    let mut result = Response::new(Body::from_stream(stream));
    *result.status_mut() = response.status;
    *result.headers_mut() = response.headers;
    result
}

pub struct ScriptedUpstream {
    pub url: String,
    pub state: ScriptedState,
    task: tokio::task::JoinHandle<()>,
}

impl ScriptedUpstream {
    pub async fn start() -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let state = ScriptedState::default();
        let app = state.router();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { url, state, task })
    }
}
impl Drop for ScriptedUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct FakeUpstream {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}

impl FakeUpstream {
    pub async fn start() -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let app = axum::Router::new().fallback(|| async { "fake upstream" });
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { url, task })
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn smoke() {
        let server = super::FakeUpstream::start().await.unwrap();
        assert_eq!(
            reqwest::get(&server.url)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "fake upstream"
        );
    }
}
