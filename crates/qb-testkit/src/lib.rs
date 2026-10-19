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
    pub received_at: Instant,
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
    start_batch: Arc<Mutex<Option<LimiterStartBatch>>>,
}

struct LimiterStartBatch {
    remaining: usize,
    barrier: Arc<tokio::sync::Barrier>,
    deadline: Option<tokio::time::Instant>,
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
            start_batch: Arc::new(Mutex::new(None)),
        });
    }
    /// Hold only the first batch before any limiter classification, so its
    /// genuine 429 feedback cannot pace requests still arriving in that batch.
    /// The shared five-second deadline starts at its first arrival. An incomplete
    /// batch returns a fixture 500 instead of waiting forever or fabricating 429s.
    pub fn gate_first_limiter_batch(&self, participants: usize) {
        assert!(participants > 0, "limiter start batch must not be empty");
        let limiter = self.window_limiter.lock().unwrap();
        let limiter = limiter.as_ref().expect("enable window limiter first");
        *limiter.start_batch.lock().unwrap() = Some(LimiterStartBatch {
            remaining: participants,
            barrier: Arc::new(tokio::sync::Barrier::new(participants)),
            deadline: None,
        });
    }
    async fn wait_for_limiter_start(&self) -> bool {
        let limiter = self.window_limiter.lock().unwrap().clone();
        let Some(limiter) = limiter else {
            return true;
        };
        let (barrier, deadline) = {
            let mut batch = limiter.start_batch.lock().unwrap();
            let Some(start) = batch.as_mut() else {
                return true;
            };
            let deadline = *start
                .deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(5));
            start.remaining -= 1;
            let barrier = start.barrier.clone();
            if start.remaining == 0 {
                // Never reuse tokio's cyclic barrier for retries or later rounds.
                *batch = None;
            }
            (barrier, deadline)
        };
        tokio::time::timeout_at(deadline, barrier.wait())
            .await
            .is_ok()
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
            let mut rounds = limiter.round_429.lock().unwrap();
            // Synthetic concurrent rounds retain their tag across real retries.
            let tagged = headers
                .get("user-agent")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("qb-storm-round/"))
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n < rounds.len());
            let index = tagged.or_else(|| rounds.len().checked_sub(1));
            if let Some(index) = index {
                rounds[index] += 1;
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
        received_at: Instant::now(),
    });
    if !state.wait_for_limiter_start().await {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("synthetic limiter start batch timed out"))
            .unwrap();
    }
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
    async fn overlapping_limiter_rounds_charge_the_originating_request_tag() {
        let server = super::ScriptedUpstream::start().await.unwrap();
        server
            .state
            .enable_window_limiter(1, std::time::Duration::from_secs(60));
        for _ in 0..3 {
            server.state.begin_limiter_round();
        }
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (round, status) in [(2, 200), (0, 429), (2, 429)] {
            let response = client
                .post(&server.url)
                .bearer_auth("synthetic-round-token")
                .header("user-agent", format!("qb-storm-round/{round}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
        }
        assert_eq!(server.state.limiter_round_429_counts(), vec![1, 0, 1]);
    }

    #[tokio::test]
    async fn limiter_start_batch_classifies_after_all_arrivals_and_only_gates_once() {
        let server = super::ScriptedUpstream::start().await.unwrap();
        server
            .state
            .enable_window_limiter(1, std::time::Duration::from_secs(60));
        server.state.gate_first_limiter_batch(2);
        server.state.begin_limiter_round();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let first_client = client.clone();
        let url = server.url.clone();
        let first = tokio::spawn(async move {
            first_client
                .get(url)
                .bearer_auth("synthetic-start-batch")
                .send()
                .await
                .unwrap()
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while server.state.requests().len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first synthetic arrival missing");
        assert!(!first.is_finished());
        assert_eq!(server.state.limiter_round_429_counts(), vec![0]);
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client
                .get(&server.url)
                .bearer_auth("synthetic-start-batch")
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), first)
            .await
            .unwrap()
            .unwrap();
        let mut statuses = [first.status().as_u16(), second.status().as_u16()];
        statuses.sort();
        assert_eq!(statuses, [200, 429]);
        let third = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client
                .get(&server.url)
                .bearer_auth("synthetic-start-batch")
                .send(),
        )
        .await
        .expect("completed start batch was incorrectly reused")
        .unwrap();
        assert_eq!(third.status(), 429);
        assert_eq!(server.state.limiter_round_429_counts(), vec![2]);
    }
    #[tokio::test]
    async fn limiter_start_batch_incomplete_returns_500_without_counting_a_rate_limit() {
        let server = super::ScriptedUpstream::start().await.unwrap();
        server
            .state
            .enable_window_limiter(1, std::time::Duration::from_secs(60));
        server.state.gate_first_limiter_batch(2);
        server.state.begin_limiter_round();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(5500),
            client
                .get(&server.url)
                .bearer_auth("synthetic-start-timeout")
                .send(),
        )
        .await
        .expect("incomplete limiter batch did not honor its five-second deadline")
        .unwrap();
        assert_eq!(response.status(), 500);
        assert_eq!(server.state.limiter_round_429_counts(), vec![0]);
        assert_eq!(server.state.requests().len(), 1);
    }

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
