//! 실제 serve 프로세스와 연결하는 로컬 전용 WebSocket 수동 점검 도구.

use futures_util::{SinkExt, StreamExt};
use qb_testkit::websocket::{MAX_MESSAGE_SIZE, Message, WebSocketUpstream};
use serde_json::json;
use std::{
    io::{self, Write},
    process::ExitCode,
    time::Duration,
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig},
};

// 수동 점검 스크립트의 local_secrets와 공유하는 고정 fixture: [0x42; 32]의 무패딩 base64url.
// admin 키는 스크립트에서 다른 바이트로 생성하며, 실제 키나 환경 변수는 사용하지 않는다.
const SYNTHETIC_LOCAL_KEY: &str = "QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI";
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [] => serve_fake().await,
        [flag, url] if flag == "--client" => run_client(url).await,
        _ => {
            eprintln!("Usage: ws_fakes [--client ws://127.0.0.1:<port>/v1/responses]");
            return ExitCode::FAILURE;
        }
    };
    if result.is_err() {
        // 핸드셰이크 오류 본문이나 요청 헤더는 진단 출력에 포함하지 않는다.
        eprintln!("{}", json!({"status": "failed"}));
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

async fn serve_fake() -> Result<()> {
    let upstream = WebSocketUpstream::start().await?;
    upstream.state.set_echo(false);
    println!("{}", json!({"url": upstream.url}));
    io::stdout().flush()?;
    let mut seen = 0;
    let mut response_number = 0;
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => { signal?; break; }
            _ = poll.tick() => {
                // 기록된 create마다 완료 이벤트를 생성하여 한 소켓의 여러 턴도 점검한다.
                let messages = upstream.state.messages();
                for message in &messages[seen..] {
                    let Message::Text(text) = message else { continue };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.as_str()) else { continue };
                    if value.get("type").and_then(|value| value.as_str()) != Some("response.create") { continue; }
                    response_number += 1;
                    let id = format!("fake-response-{response_number}");
                    let created = json!({"type": "response.created", "response": {"id": id, "status": "in_progress"}});
                    let completed = json!({"type": "response.completed", "response": {
                        "id": id, "status": "completed", "model": "fake-model", "output": [],
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                    }});
                    // 점검 클라이언트가 이미 닫혔으면 다음 연결을 기다린다.
                    let _ = upstream.state.send(Message::text(created.to_string()));
                    let _ = upstream.state.send(Message::text(completed.to_string()));
                }
                seen = messages.len();
            }
        }
    }
    Ok(())
}

async fn run_client(url: &str) -> Result<()> {
    let mut request = url.into_client_request()?;
    // 합성 키는 로컬 수동 점검에서만 전송한다.
    if request.uri().scheme_str() != Some("ws")
        || !matches!(
            request.uri().host(),
            Some("127.0.0.1" | "localhost" | "[::1]")
        )
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "expected loopback ws URL").into());
    }
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {SYNTHETIC_LOCAL_KEY}").parse()?,
    );
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_MESSAGE_SIZE));
    let (mut socket, response) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_async_with_config(request, Some(config), false),
    )
    .await??;
    println!("{}", json!({"status": response.status().as_u16()}));
    let create = Message::text(json!({
        "type": "response.create", "model": "fake-model",
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "synthetic hello"}]}]
    }).to_string());
    println!(
        "{}",
        json!({"direction": "sent", "message_type": "response.create", "bytes": create.len()})
    );
    socket.send(create).await?;
    let mut created_seen = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing completion"))??;
        let kind = match &message {
            Message::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(text.as_str())?;
                match value.get("type").and_then(|value| value.as_str()) {
                    Some("response.created") => {
                        created_seen = true;
                        "response.created"
                    }
                    Some("response.completed") => "response.completed",
                    Some("error") => "error",
                    _ => "other",
                }
            }
            Message::Binary(_) => "binary",
            Message::Ping(_) => "ping",
            Message::Pong(_) => "pong",
            Message::Close(_) => "close",
            Message::Frame(_) => "frame",
        };
        println!(
            "{}",
            json!({"direction": "received", "message_type": kind, "bytes": message.len()})
        );
        if kind == "response.completed" && created_seen {
            socket.close(None).await?;
            println!("{}", json!({"status": "completed"}));
            return Ok(());
        }
        if matches!(kind, "error" | "close" | "response.completed") {
            return Err(io::Error::other("unexpected completion sequence").into());
        }
        // 읽는 쪽의 자동 pong을 즉시 내보낸다. pong은 재전송하지 않는다.
        if kind == "ping" {
            socket.flush().await?;
        }
    }
}
