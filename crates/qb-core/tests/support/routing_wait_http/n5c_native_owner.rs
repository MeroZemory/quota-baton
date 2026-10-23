//! Real launch issuance and authenticated owner transport for D26 contrasts.
use super::*;
use futures_util::{SinkExt, StreamExt};
use qb_proto::native_reset::{Command, Event, Reply, Request, VERSION};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

pub(in super::super) async fn full_poll(
    h: &Harness,
    usage: &ScriptedUpstream,
    source: &str,
    reset: chrono::DateTime<Utc>,
) {
    let before = h.fake.state.requests().len();
    usage.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({"rate_limit": {
            "primary_window":{"used_percent":100,"limit_window_seconds":18000,
                "reset_at":reset.timestamp()},
            "secondary_window":{"used_percent":10,"limit_window_seconds":604800,
                "reset_at":(reset + ChronoDuration::hours(47)).timestamp()},
            "active_limit":"5h"
        }})
        .to_string(),
    ));
    let account = timeout(PHASE, h.state.quota.poll(source))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(account.health, Health::Cooling {
        reason: HealthReason::Quota, until, ref scope,
    } if until == reset && scope == "account"));
    assert_eq!(account.quota_cooling.unwrap().reset_at, reset);
    assert_eq!(h.fake.state.requests().len(), before);
    assert_eq!(usage.state.requests().len(), 1);
}

pub(in super::super) fn assert_same_source_attempts(h: &Harness, expected: usize) {
    let requests = h.fake.state.requests();
    let data: Vec<_> = requests
        .iter()
        .filter(|r| r.path_and_query == "/responses")
        .collect();
    assert_eq!(data.len(), expected);
    for request in &data {
        assert_eq!(request.body.as_ref(), UPLOAD.as_bytes());
        assert!(request.headers.get("authorization").is_some());
        assert_eq!(
            request.headers.get("authorization"),
            data[0].headers.get("authorization")
        );
    }
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && !row["from"].is_null())
    );
}

pub(in super::super) struct NativeOwner {
    base_url: String,
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl NativeOwner {
    pub(in super::super) fn assert_private_body(&self, body: &[u8]) {
        let text = std::str::from_utf8(body).unwrap();
        let handle = self
            .base_url
            .split("/l/")
            .nth(1)
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        assert!(
            !text.contains(&self.base_url),
            "typed wait body disclosed launch URL"
        );
        assert!(
            !text.contains(handle),
            "typed wait body disclosed launch handle"
        );
    }

    pub(in super::super) async fn connect(h: &Harness, session: &str, source: &str) -> Self {
        for _ in h.ids() {
            h.fake.state.push(ScriptedResponse::json(
                StatusCode::OK,
                r#"{"models":[{"slug":"synthetic-model-http-wait"}]}"#,
            ));
        }
        let launch = timeout(
            PHASE,
            h.state.issue_launch(
                qb_core::relay::LaunchRequest {
                    model: Some("synthetic-model-http-wait".into()),
                    model_is_explicit: true,
                    account_pref: None,
                    resume_id: Some(session.into()),
                    move_for_model: false,
                    urgent: false,
                },
                h.servers.data_addr.port(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(launch.account, source);
        let url = format!(
            "{}/qb/native-owner",
            launch.base_url.replacen("http://", "ws://", 1)
        );
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer synthetic-client".parse().unwrap());
        let (socket, response) = timeout(PHASE, connect_async(request))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status().as_u16(), 101);
        Self {
            base_url: launch.base_url,
            socket,
        }
    }

    pub(in super::super) async fn request(&self, h: &Harness, session: &str) -> reqwest::Response {
        timeout(
            PHASE,
            h.client
                .post(format!("{}/responses", self.base_url))
                .bearer_auth("synthetic-client")
                .header("content-type", "application/json")
                .header("session-id", session)
                .timeout(Duration::from_secs(30))
                .body(UPLOAD)
                .send(),
        )
        .await
        .expect("connected-owner fallback must finish within original budget")
        .unwrap()
    }

    async fn exchange(&mut self, request_id: u64, command: Command) -> Event {
        let request = Request {
            version: VERSION,
            request_id,
            command,
        };
        timeout(
            PHASE,
            self.socket.send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            )),
        )
        .await
        .unwrap()
        .unwrap();
        let message = timeout(PHASE, self.socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Message::Text(text) = message else {
            panic!("expected correlated native-owner reply")
        };
        let reply: Reply = serde_json::from_str(&text).unwrap();
        assert!(reply.valid_shape());
        assert_eq!(reply.version, VERSION);
        assert_eq!(reply.request_id, Some(request_id));
        reply.event
    }

    pub(in super::super) async fn finish(mut self, session: &str, reset: chrono::DateTime<Utc>) {
        let Event::Waiting {
            episode,
            reset_unix_ms,
        } = self
            .exchange(
                1,
                Command::Observe {
                    thread_id: session.into(),
                },
            )
            .await
        else {
            panic!("D26 fallback must publish Waiting to its actual launch owner")
        };
        assert_ne!(episode, 0);
        assert_eq!(reset_unix_ms, reset.timestamp_millis());
        assert!(matches!(
            self.exchange(2, Command::Cancel { episode }).await,
            Event::Cancelled { episode: actual } if actual == episode
        ));
        timeout(PHASE, async {
            if let Some(message) = self.socket.next().await {
                assert!(matches!(message, Ok(Message::Close(_)) | Err(_)));
            }
        })
        .await
        .expect("cancelled owner must close before harness teardown");
    }
}
