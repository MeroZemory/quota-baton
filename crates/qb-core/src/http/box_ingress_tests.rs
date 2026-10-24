//! Common middleware tests over actual loopback HTTP, not native box sockets.
use super::*;
use crate::boxes::{BoxName, BoxRegistry};
use axum::{body::Body, routing::get};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};

struct Fixture {
    _home: tempfile::TempDir,
    registry: BoxRegistry,
    a: BoxScope,
    a_key: Zeroizing<String>,
    b: BoxScope,
    b_key: Zeroizing<String>,
}

impl Fixture {
    fn new() -> Self {
        // Lexically normalized ancestors, under the project rather than /tmp.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/tests/box-ingress");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (a, a_key) = registry
            .add(BoxName::parse("synthetic-a").unwrap())
            .unwrap()
            .into_parts();
        let (b, b_key) = registry
            .add(BoxName::parse("synthetic-b").unwrap())
            .unwrap()
            .into_parts();
        Self {
            _home: home,
            registry,
            a,
            a_key,
            b,
            b_key,
        }
    }

    async fn server(&self, scope: &BoxScope) -> Server {
        let registry = self.registry.clone();
        Server::start(
            scope.clone(),
            move |scope, key| {
                // Same nonblocking callback as the native service bootstrap.
                std::future::ready(registry.try_authenticate(&scope, &key))
            },
            None,
        )
        .await
    }
}

struct Server {
    port: u16,
    client: reqwest::Client,
    hits: Arc<AtomicUsize>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn start<F, Fut>(scope: BoxScope, authenticate: F, forged: Option<BoxAuthority>) -> Self
    where
        F: Fn(BoxScope, Zeroizing<String>) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Result<BoxAuthority, RegistryError>> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        let expected = scope.clone();
        let routes = Router::new().route(
            "/v1/messages",
            get(move |request: Request<Body>| {
                let expected = expected.clone();
                let hits = handler_hits.clone();
                async move {
                    let authority = request.extensions().get::<BoxAuthority>().unwrap();
                    assert_eq!(authority.scope(), &expected);
                    assert!(!authority.is_revoked());
                    assert!(request.extensions().get::<BoxScope>().is_none());
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let mut routes = protect_box_routes(routes, scope, port, authenticate);
        if let Some(authority) = forged {
            // An outer in-process layer can supply extensions; HTTP headers cannot.
            routes = routes.layer(middleware::from_fn(
                move |mut request: Request, next: Next| {
                    let authority = authority.clone();
                    async move {
                        request.extensions_mut().insert(authority.scope().clone());
                        request.extensions_mut().insert(authority);
                        next.run(request).await
                    }
                },
            ));
        }
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, routes)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            port,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            hits,
            stop: Some(stop),
            task,
        }
    }

    fn request(&self) -> reqwest::RequestBuilder {
        self.client
            .get(format!("http://127.0.0.1:{}/v1/messages", self.port))
    }

    async fn close(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Panic-path fallback; successful tests explicitly shut down and join.
        self.task.abort();
    }
}

async fn expect(request: reqwest::RequestBuilder, status: StatusCode) {
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), status);
    let body = response.bytes().await.unwrap();
    if status == StatusCode::UNAUTHORIZED {
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "error": {"type": "authentication_error", "message": "Invalid box credential"},
                "type": "error"
            })
        );
    }
}

#[tokio::test]
async fn keys_are_bound_to_the_selected_box_and_global_secrets_never_authorize() {
    let fixture = Fixture::new();
    let a = fixture.server(&fixture.a).await;
    let b = fixture.server(&fixture.b).await;
    expect(
        a.request().bearer_auth(fixture.a_key.as_str()),
        StatusCode::NO_CONTENT,
    )
    .await;
    expect(
        b.request().header("x-api-key", fixture.b_key.as_str()),
        StatusCode::NO_CONTENT,
    )
    .await;
    expect(
        b.request().bearer_auth(fixture.a_key.as_str()),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    expect(
        a.request().bearer_auth(fixture.b_key.as_str()),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    let secrets = crate::local_secrets::load_or_create(fixture._home.path()).unwrap();
    for key in [&secrets.client_key, &secrets.admin_token] {
        expect(a.request().bearer_auth(key), StatusCode::UNAUTHORIZED).await;
        expect(
            a.request().header("x-api-key", key.as_str()),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    }
    assert_eq!(a.hits.load(Ordering::SeqCst), 1);
    assert_eq!(b.hits.load(Ordering::SeqCst), 1);
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn mixed_duplicate_missing_and_malformed_credentials_are_rejected() {
    let fixture = Fixture::new();
    let server = fixture.server(&fixture.a).await;
    let bearer = format!("Bearer {}", fixture.a_key.as_str());
    for headers in [
        vec![],
        vec![
            ("authorization", bearer.as_str()),
            ("x-api-key", fixture.a_key.as_str()),
        ],
        vec![
            ("authorization", bearer.as_str()),
            ("authorization", bearer.as_str()),
        ],
        vec![
            ("x-api-key", fixture.a_key.as_str()),
            ("x-api-key", fixture.a_key.as_str()),
        ],
        vec![("authorization", "bearer invalid")],
        vec![("x-api-key", "short")],
        vec![("x-api-key", "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!")],
    ] {
        let mut request = server.request();
        for (name, value) in headers {
            request = request.header(name, value);
        }
        expect(request, StatusCode::UNAUTHORIZED).await;
    }
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    server.close().await;
}

#[tokio::test]
async fn exact_loopback_hosts_and_no_origin_are_required_before_authentication() {
    let fixture = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let registry = fixture.registry.clone();
    let server = Server::start(
        fixture.a.clone(),
        move |scope, key| {
            count.fetch_add(1, Ordering::SeqCst);
            std::future::ready(registry.try_authenticate(&scope, &key))
        },
        None,
    )
    .await;
    for host in [
        format!("127.0.0.1:{}", server.port),
        format!("localhost:{}", server.port),
    ] {
        expect(
            server
                .request()
                .header("host", host)
                .bearer_auth(fixture.a_key.as_str()),
            StatusCode::NO_CONTENT,
        )
        .await;
    }
    for host in [
        "localhost".to_owned(),
        "localhost:0".to_owned(),
        format!("localhost.evil:{}", server.port),
        format!("[::1]:{}", server.port),
        format!("LOCALHOST:{}", server.port),
    ] {
        expect(
            server
                .request()
                .header("host", host)
                .bearer_auth(fixture.a_key.as_str()),
            StatusCode::MISDIRECTED_REQUEST,
        )
        .await;
    }
    for origin in [
        "null".to_owned(),
        format!("http://localhost:{}", server.port),
    ] {
        expect(
            server
                .request()
                .header("origin", origin)
                .bearer_auth(fixture.a_key.as_str()),
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
    server.close().await;
}

#[tokio::test]
async fn forged_extensions_are_removed_and_cannot_replace_listener_authority() {
    let fixture = Fixture::new();
    let forged = fixture
        .registry
        .try_authenticate(&fixture.b, &fixture.b_key)
        .unwrap();
    let registry = fixture.registry.clone();
    let server = Server::start(
        fixture.a.clone(),
        move |scope, key| std::future::ready(registry.try_authenticate(&scope, &key)),
        Some(forged),
    )
    .await;
    expect(server.request(), StatusCode::UNAUTHORIZED).await;
    expect(
        server.request().bearer_auth(fixture.b_key.as_str()),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    expect(
        server.request().bearer_auth(fixture.a_key.as_str()),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    server.close().await;
}

#[tokio::test]
async fn revoking_a_denies_its_key_while_b_continues() {
    let fixture = Fixture::new();
    let a = fixture.server(&fixture.a).await;
    let b = fixture.server(&fixture.b).await;
    expect(
        a.request().bearer_auth(fixture.a_key.as_str()),
        StatusCode::NO_CONTENT,
    )
    .await;
    fixture.registry.revoke(fixture.a.name()).unwrap();
    expect(
        a.request().bearer_auth(fixture.a_key.as_str()),
        StatusCode::UNAUTHORIZED,
    )
    .await;
    expect(
        b.request().bearer_auth(fixture.b_key.as_str()),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(a.hits.load(Ordering::SeqCst), 1);
    assert_eq!(b.hits.load(Ordering::SeqCst), 1);
    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn authority_revoked_during_async_auth_pause_is_rechecked_before_dispatch() {
    let fixture = Fixture::new();
    let registry = fixture.registry.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let auth_entered = entered.clone();
    let auth_release = release.clone();
    let server = Server::start(
        fixture.a.clone(),
        move |scope, key| {
            // Authenticate for real first, then hold that actual grant across an await.
            let result = registry.try_authenticate(&scope, &key);
            let entered = auth_entered.clone();
            let release = auth_release.clone();
            async move {
                entered.notify_one();
                release.notified().await;
                result
            }
        },
        None,
    )
    .await;
    let request = expect(
        server.request().bearer_auth(fixture.a_key.as_str()),
        StatusCode::UNAUTHORIZED,
    );
    let revoke = async {
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        fixture.registry.revoke(fixture.a.name()).unwrap();
        release.notify_one();
    };
    tokio::join!(request, revoke);
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    server.close().await;
}
