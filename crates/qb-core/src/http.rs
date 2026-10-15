use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinHandle,
};

use crate::{config::Config, local_secrets::LocalSecrets};

pub struct RunningServers {
    pub data_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub data_task: JoinHandle<std::io::Result<()>>,
    pub admin_task: JoinHandle<std::io::Result<()>>,
    shutdown: watch::Sender<bool>,
    listener_closed: [watch::Receiver<bool>; 2],
}
impl RunningServers {
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }
    pub async fn wait_for_listener_close(&self) {
        for closed in &self.listener_closed {
            let mut closed = closed.clone();
            while !*closed.borrow_and_update() {
                if closed.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}

struct HandoverListener {
    listener: Option<TcpListener>,
    closed: watch::Sender<bool>,
}
impl axum::serve::Listener for HandoverListener {
    type Io = TcpStream;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        let (stream, addr) =
            axum::serve::Listener::accept(self.listener.as_mut().expect("listener alive")).await;
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "failed to set TCP_NODELAY on accepted HTTP socket");
        }
        (stream, addr)
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.as_ref().expect("listener alive").local_addr()
    }
}
impl Drop for HandoverListener {
    fn drop(&mut self) {
        drop(self.listener.take());
        self.closed.send_replace(true);
    }
}
#[derive(Clone)]
struct Peer(SocketAddr);
impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, HandoverListener>>
    for Peer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, HandoverListener>) -> Self {
        Self(*stream.remote_addr())
    }
}
impl Drop for RunningServers {
    fn drop(&mut self) {
        self.data_task.abort();
        self.admin_task.abort();
    }
}

#[derive(Clone)]
struct Safety {
    port: u16,
    key: Arc<str>,
    other_key: Arc<str>,
    admin: bool,
}

pub async fn spawn(config: &Config, secrets: LocalSecrets) -> anyhow::Result<RunningServers> {
    spawn_with_admin(config, secrets, Router::new()).await
}

pub async fn spawn_with_admin(
    config: &Config,
    secrets: LocalSecrets,
    extra_admin: Router,
) -> anyhow::Result<RunningServers> {
    spawn_with_routes(
        config,
        secrets,
        Router::new().fallback(not_found),
        extra_admin,
    )
    .await
}

pub async fn spawn_with_routes(
    config: &Config,
    secrets: LocalSecrets,
    data_routes: Router,
    extra_admin: Router,
) -> anyhow::Result<RunningServers> {
    anyhow::ensure!(
        !secrets.client_key.is_empty() && !secrets.admin_token.is_empty(),
        "local secrets must not be empty"
    );
    anyhow::ensure!(
        secrets.client_key != secrets.admin_token,
        "local secrets must be distinct"
    );
    let data = bind_listener(config.server.data_port).await?;
    let admin = bind_listener(config.server.admin_port).await?;
    let data_addr = data.local_addr()?;
    let admin_addr = admin.local_addr()?;
    let (shutdown, mut data_shutdown) = watch::channel(false);
    let mut admin_shutdown = data_shutdown.clone();
    let (data_closed, data_closed_rx) = watch::channel(false);
    let (admin_closed, admin_closed_rx) = watch::channel(false);
    let client_key: Arc<str> = secrets.client_key.into();
    let admin_token: Arc<str> = secrets.admin_token.into();
    let data_app = data_routes.layer(middleware::from_fn_with_state(
        Safety {
            port: data_addr.port(),
            key: client_key.clone(),
            other_key: admin_token.clone(),
            admin: false,
        },
        safety,
    ));
    let started_at = chrono::Utc::now().to_rfc3339();
    let admin_app = Router::new()
        .route(
            "/admin/v1/health",
            get(move || async move {
                Json(qb_proto::HealthResponse {
                    ok: true,
                    version: env!("CARGO_PKG_VERSION").into(),
                    pid: std::process::id(),
                    started_at,
                })
            }),
        )
        .merge(extra_admin)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            Safety {
                port: admin_addr.port(),
                key: admin_token,
                other_key: client_key,
                admin: true,
            },
            safety,
        ));
    let data_task = tokio::spawn(async move {
        axum::serve(
            HandoverListener {
                listener: Some(data),
                closed: data_closed,
            },
            data_app.into_make_service_with_connect_info::<Peer>(),
        )
        .with_graceful_shutdown(async move {
            while !*data_shutdown.borrow_and_update() {
                if data_shutdown.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
    });
    let admin_task = tokio::spawn(async move {
        axum::serve(
            HandoverListener {
                listener: Some(admin),
                closed: admin_closed,
            },
            admin_app.into_make_service_with_connect_info::<Peer>(),
        )
        .with_graceful_shutdown(async move {
            while !*admin_shutdown.borrow_and_update() {
                if admin_shutdown.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
    });
    Ok(RunningServers {
        data_addr,
        admin_addr,
        data_task,
        admin_task,
        shutdown,
        listener_closed: [data_closed_rx, admin_closed_rx],
    })
}

async fn bind_listener(port: u16) -> anyhow::Result<TcpListener> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn safety(
    State(state): State<Safety>,
    ConnectInfo(Peer(peer)): ConnectInfo<Peer>,
    request: Request,
    next: Next,
) -> Response {
    if !peer.ip().is_loopback() {
        return error(
            StatusCode::FORBIDDEN,
            "invalid_request_error",
            "Loopback peer required",
            request.uri().path(),
        );
    }
    let hosts = request.headers().get_all("host");
    let mut hosts = hosts.iter();
    let host = hosts.next().and_then(|value| value.to_str().ok());
    let valid = host.is_some_and(|host| {
        host == format!("127.0.0.1:{}", state.port) || host == format!("localhost:{}", state.port)
    });
    if !valid || hosts.next().is_some() {
        tracing::warn!(port = state.port, "Rejected request with invalid Host");
        return error(
            StatusCode::MISDIRECTED_REQUEST,
            "invalid_request_error",
            "Invalid Host",
            request.uri().path(),
        );
    }
    if request.headers().contains_key("origin") {
        return error(
            StatusCode::FORBIDDEN,
            "invalid_request_error",
            "Origin is forbidden",
            request.uri().path(),
        );
    }
    let bearer = single_header(request.headers(), "authorization")
        .and_then(|value| value.strip_prefix("Bearer "));
    let api_key = (!state.admin)
        .then(|| single_header(request.headers(), "x-api-key"))
        .flatten();
    // 다른 포트의 키가 함께 전송된 요청도 거부해 포트 간 경계를 유지한다.
    let wrong_port = [bearer, api_key]
        .into_iter()
        .flatten()
        .any(|key| equal(key, &state.other_key));
    let authorized = [bearer, api_key]
        .into_iter()
        .flatten()
        .any(|key| equal(key, &state.key));
    if wrong_port || !authorized {
        return error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Invalid local credential",
            request.uri().path(),
        );
    }
    next.run(request).await
}

fn single_header<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        None
    } else {
        Some(value)
    }
}

fn equal(candidate: &str, expected: &str) -> bool {
    bool::from(candidate.as_bytes().ct_eq(expected.as_bytes()))
}

async fn not_found(request: Request) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        "Route not found",
        request.uri().path(),
    )
}

fn error(status: StatusCode, kind: &str, message: &str, path: &str) -> Response {
    let mut body = serde_json::json!({"error": {"type": kind, "message": message}});
    if path == "/v1/messages" || path.starts_with("/v1/messages/") {
        body["type"] = "error".into();
    }
    (status, Json(body)).into_response()
}
