//! Local fixture servers and a sanitized M4a WebSocket reconnect client.

use futures_util::{SinkExt, StreamExt};
use qb_testkit::{
    ScriptedResponse, ScriptedUpstream,
    websocket::{Message, WebSocketUpstream},
};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    net::IpAddr,
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{client::IntoClientRequest, protocol::CloseFrame},
};

// Same fixed fixture key as ws_fakes; never read credentials or environment keys.
const SYNTHETIC_LOCAL_KEY: &str = "QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI";
const TIMEOUT: Duration = Duration::from_secs(10);
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [] => serve_fakes().await,
        [flag, url] if flag == "--client" => run_client(url).await,
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid arguments").into()),
    };
    if result.is_err() {
        // Do not print URLs supplied by the client, errors, headers, or bodies.
        eprintln!("{}", json!({"status":"failed"}));
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

async fn serve_fakes() -> Result<()> {
    let http = ScriptedUpstream::start().await?;
    let ws = WebSocketUpstream::start().await?;
    ws.state.set_echo(false);
    let reset = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 600;
    let mut quota = ScriptedResponse::json(
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"synthetic quota rejection"}}"#,
    );
    quota
        .headers
        .insert("anthropic-ratelimit-unified-5h-status", "rejected".parse()?);
    quota.headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.to_string().parse()?,
    );
    http.state.push(quota);
    http.state.push(ScriptedResponse::json(axum::http::StatusCode::OK,
        json!({"id":"synthetic-http-response","type":"message","role":"assistant","model":"fake-model", "content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}).to_string()));
    ws.state.push_reply([Message::text(json!({
        "type":"error","status":429,
        "error":{"type":"usage_limit_reached","message":"synthetic quota rejection","resets_in_seconds":600}
    }).to_string())]);
    ws.state.push_reply([
        Message::text(json!({"type":"response.created","response":{"id":"synthetic-ws-response","status":"in_progress"}}).to_string()),
        Message::text(json!({"type":"response.completed","response":{"id":"synthetic-ws-response","status":"completed","model":"fake-model","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}).to_string()),
    ]);
    // Both fixture URLs use HTTP because RelayState performs its own upgrade.
    println!("{}", json!({"http":http.url,"ws":ws.url}));
    io::stdout().flush()?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn client_request(url: &str) -> Result<axum::http::Request<()>> {
    let mut request = url.into_client_request()?;
    let uri = request.uri();
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
    let loopback = host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if uri.scheme_str() != Some("ws")
        || !loopback
        || uri
            .port_u16()
            .is_none_or(|port| matches!(port, 0 | 10100 | 10101))
        || uri.path() != "/v1/responses"
        || uri.query().is_some()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "expected loopback fixture URL").into(),
        );
    }
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {SYNTHETIC_LOCAL_KEY}").parse()?,
    );
    Ok(request)
}

async fn connect(url: &str) -> Result<(Client, String)> {
    let request = client_request(url)?;
    let (socket, response) = tokio::time::timeout(TIMEOUT, connect_async(request)).await??;
    if response.status() != axum::http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(io::Error::other("expected upgrade").into());
    }
    let account = response
        .headers()
        .get("x-quota-baton-account")
        .and_then(|v| v.to_str().ok())
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .ok_or_else(|| io::Error::other("missing safe account identifier"))?
        .to_owned();
    println!(
        "{}",
        json!({"type":"upgrade","account":account,"status":101})
    );
    Ok((socket, account))
}

async fn receive(socket: &mut Client, deadline: tokio::time::Instant) -> Result<Message> {
    Ok(tokio::time::timeout_at(deadline, socket.next())
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing fixture message"))??)
}

async fn run_client(url: &str) -> Result<()> {
    // Construct once and clone the exact bytes on reconnect; keep no transcript.
    let create = Message::text(
        json!({"type":"response.create","model":"fake-model","input":[{
            "role":"user","content":[{"type":"input_text","text":"synthetic hello"}]
        }]})
        .to_string(),
    );
    let (mut first, first_account) = connect(url).await?;
    tokio::time::timeout(TIMEOUT, first.send(create.clone())).await??;
    println!(
        "{}",
        json!({"type":"response.create","account":first_account})
    );
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        match receive(&mut first, deadline).await? {
            Message::Close(Some(frame)) if u16::from(frame.code) == 1012 => {
                println!(
                    "{}",
                    json!({"type":"close","account":first_account,"close":1012})
                );
                let _ = tokio::time::timeout(TIMEOUT, first.flush()).await;
                break;
            }
            Message::Ping(_) => {
                tokio::time::timeout(TIMEOUT, first.flush()).await??;
            }
            Message::Pong(_) => {}
            // In particular, any error frame fails this check without printing it.
            _ => {
                return Err(
                    io::Error::other("expected precommit close without error frame").into(),
                );
            }
        }
    }
    drop(first);
    let (mut second, second_account) = connect(url).await?;
    if second_account == first_account {
        return Err(io::Error::other("reconnect did not change account").into());
    }
    tokio::time::timeout(TIMEOUT, second.send(create)).await??;
    println!(
        "{}",
        json!({"type":"response.create","account":second_account})
    );
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut created = false;
    loop {
        match receive(&mut second, deadline).await? {
            Message::Text(text) => {
                let value: Value = serde_json::from_str(text.as_str())?;
                match value.get("type").and_then(Value::as_str) {
                    Some("response.created") if !created => {
                        created = true;
                        println!(
                            "{}",
                            json!({"type":"response.created","account":second_account})
                        );
                    }
                    Some("response.completed") if created => {
                        println!(
                            "{}",
                            json!({"type":"response.completed","account":second_account})
                        );
                        tokio::time::timeout(
                            TIMEOUT,
                            second.close(Some(CloseFrame {
                                code: 1000.into(),
                                reason: "".into(),
                            })),
                        )
                        .await??;
                        println!(
                            "{}",
                            json!({"type":"close","account":second_account,"close":1000})
                        );
                        return Ok(());
                    }
                    _ => return Err(io::Error::other("unexpected completion sequence").into()),
                }
            }
            Message::Ping(_) => {
                tokio::time::timeout(TIMEOUT, second.flush()).await??;
            }
            Message::Pong(_) => {}
            _ => return Err(io::Error::other("unexpected completion message").into()),
        }
    }
}
