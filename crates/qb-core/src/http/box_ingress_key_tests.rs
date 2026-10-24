//! Actual production TCP ingress with an optional real registry; no service owner.
use super::super::{RunningServers, spawn_with_box_registry, spawn_with_routes};
use super::*;
use crate::{
    boxes::{BoxName, BoxRegistry},
    config::Config,
    local_secrets::LocalSecrets,
};
use axum::{body::Body, routing::get};
use futures_util::StreamExt;
use std::{path::Path, time::Duration};

const CLIENT: &str = "synthetic-global-client";
const ADMIN: &str = "synthetic-global-admin";

struct Fixture {
    _home: tempfile::TempDir,
    registry: BoxRegistry,
    a: BoxScope,
    a_key: Zeroizing<String>,
    b_key: Zeroizing<String>,
}

impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/tests/box-key-ingress");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (a, a_key) = registry
            .add(BoxName::parse("synthetic-a").unwrap())
            .unwrap()
            .into_parts();
        let (_, b_key) = registry
            .add(BoxName::parse("synthetic-b").unwrap())
            .unwrap()
            .into_parts();
        Self {
            _home: home,
            registry,
            a,
            a_key,
            b_key,
        }
    }

    async fn server(&self, enabled: bool) -> Server {
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        let routes = Router::new().route("/identity", get(identity)).route(
            "/stream",
            get(|| async {
                Body::from_stream(
                    futures_util::stream::once(async {
                        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"synthetic-start"))
                    })
                    .chain(futures_util::stream::pending()),
                )
            }),
        );
        let admin = Router::new().route("/identity", get(identity));
        let secrets = LocalSecrets {
            client_key: CLIENT.into(),
            admin_token: ADMIN.into(),
        };
        let running = if enabled {
            spawn_with_box_registry(&config, secrets, routes, admin, Some(self.registry.clone()))
                .await
        } else {
            spawn_with_routes(&config, secrets, routes, admin).await
        }
        .unwrap();
        Server {
            running,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        }
    }
}

async fn identity(request: Request) -> String {
    assert!(request.extensions().get::<BoxScope>().is_none());
    match request.extensions().get::<BoxAuthority>() {
        Some(authority) => {
            let admitted = request
                .extensions()
                .get::<crate::boxes::BoxRequest>()
                .unwrap();
            assert_eq!(admitted.scope(), authority.scope());
            assert_eq!(
                crate::boxes::current_request().unwrap().scope(),
                authority.scope()
            );
            authority.scope().name().as_str().to_owned()
        }
        None => {
            assert!(
                request
                    .extensions()
                    .get::<crate::boxes::BoxRequest>()
                    .is_none()
            );
            assert!(crate::boxes::current_request().is_none());
            "global".to_owned()
        }
    }
}

struct Server {
    running: RunningServers,
    client: reqwest::Client,
}
impl Server {
    fn data(&self) -> reqwest::RequestBuilder {
        self.client
            .get(format!("http://{}/identity", self.running.data_addr))
    }
    fn admin(&self) -> reqwest::RequestBuilder {
        self.client
            .get(format!("http://{}/identity", self.running.admin_addr))
    }
    async fn stop(mut self) {
        self.running.shutdown();
        tokio::time::timeout(Duration::from_secs(5), async {
            self.running.wait_for_listener_close().await;
            (&mut self.running.data_task).await.unwrap().unwrap();
            (&mut self.running.admin_task).await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn optional_registry_identifies_keys_and_revoke_isolates_shared_tcp() {
    let f = Fixture::new();
    let s = f.server(true).await;
    let a = s.data().bearer_auth(f.a_key.as_str()).send().await.unwrap();
    assert_eq!(a.status(), StatusCode::OK);
    assert_eq!(a.text().await.unwrap(), "synthetic-a");
    let b = s
        .data()
        .header("x-api-key", f.b_key.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(b.status(), StatusCode::OK);
    assert_eq!(b.text().await.unwrap(), "synthetic-b");
    f.registry.revoke(f.a.name()).unwrap();
    assert_eq!(
        s.data()
            .bearer_auth(f.a_key.as_str())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        s.data()
            .bearer_auth(f.b_key.as_str())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "synthetic-b"
    );
    s.stop().await;
}

#[tokio::test]
async fn global_credential_order_and_admin_boundary_remain_unchanged() {
    let f = Fixture::new();
    for enabled in [false, true] {
        let s = f.server(enabled).await;
        assert_eq!(
            s.data()
                .bearer_auth(CLIENT)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "global"
        );
        // The legacy global OR policy is deliberately not the box-only strict parser.
        assert_eq!(
            s.data()
                .bearer_auth(CLIENT)
                .header("x-api-key", f.a_key.as_str())
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "global"
        );
        assert_eq!(
            s.data()
                .bearer_auth(CLIENT)
                .header("x-api-key", ADMIN)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            s.data()
                .bearer_auth(f.a_key.as_str())
                .header("x-api-key", ADMIN)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            s.admin()
                .bearer_auth(ADMIN)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "global"
        );
        for key in [CLIENT, f.a_key.as_str(), f.b_key.as_str()] {
            assert_eq!(
                s.admin().bearer_auth(key).send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            s.admin()
                .header("x-api-key", ADMIN)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        if !enabled {
            assert_eq!(
                s.data()
                    .bearer_auth(f.a_key.as_str())
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        s.stop().await;
    }
}

#[tokio::test]
async fn key_only_credentials_remain_strict_after_host_origin_checks() {
    let f = Fixture::new();
    let s = f.server(true).await;
    assert_eq!(
        s.data()
            .bearer_auth(f.a_key.as_str())
            .header("x-api-key", f.b_key.as_str())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        s.data()
            .header("authorization", format!("Bearer {}", f.a_key.as_str()))
            .header("authorization", format!("Bearer {}", f.a_key.as_str()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        s.data()
            .bearer_auth(f.a_key.as_str())
            .header("host", "attacker.invalid")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::MISDIRECTED_REQUEST
    );
    assert_eq!(
        s.data()
            .bearer_auth(f.a_key.as_str())
            .header("origin", "http://localhost")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        s.data()
            .bearer_auth(f.a_key.as_str())
            .header("host", format!("localhost:{}", s.running.data_addr.port()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    s.stop().await;
}

#[tokio::test]
async fn revoke_wakes_pending_response_body_without_revoking_other_box() {
    let f = Fixture::new();
    let authority = f.registry.try_identify(&f.a_key).unwrap();
    let s = f.server(true).await;
    let mut response = s
        .client
        .get(format!("http://{}/stream", s.running.data_addr))
        .bearer_auth(f.a_key.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.chunk().await.unwrap().unwrap().as_ref(),
        b"synthetic-start"
    );
    f.registry.revoke(f.a.name()).unwrap();
    assert!(
        response.chunk().await.is_err(),
        "revocation must truncate, not successfully end, the body"
    );
    tokio::time::timeout(Duration::from_secs(5), authority.wait_quiescent())
        .await
        .unwrap();
    assert_eq!(
        s.data()
            .bearer_auth(f.b_key.as_str())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "synthetic-b"
    );
    s.stop().await;
}

#[tokio::test]
async fn revocable_body_preserves_frames_and_trailers_before_revocation() {
    use http_body_util::BodyExt;
    let f = Fixture::new();
    let authority = f.registry.try_identify(&f.a_key).unwrap();
    let mut trailers = HeaderMap::new();
    trailers.insert("x-synthetic-trailer", "complete".parse().unwrap());
    let frames = futures_util::stream::iter([
        Ok::<_, std::io::Error>(hyper::body::Frame::data(bytes::Bytes::from_static(b"data"))),
        Ok(hyper::body::Frame::trailers(trailers.clone())),
    ]);
    let mut body = RevocableBody {
        inner: Some(Body::new(http_body_util::StreamBody::new(frames))),
        revoked: Box::pin(async move { authority.watch_revocation().revoked().await }),
    };
    assert_eq!(
        body.frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap()
            .as_ref(),
        b"data"
    );
    assert_eq!(
        body.frame()
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap(),
        trailers
    );
    assert!(body.frame().await.is_none());
}
