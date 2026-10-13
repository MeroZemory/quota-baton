use qb_testkit::ScriptedState;
use std::future::IntoFuture;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let anthropic = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let codex = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    println!("{} {}", anthropic.local_addr()?, codex.local_addr()?);
    let cl = ScriptedState::default();
    let cx = ScriptedState::default();
    cl.push(qb_testkit::ScriptedResponse::sse(vec![(std::time::Duration::ZERO,bytes::Bytes::from_static(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"))]));
    cx.push(qb_testkit::ScriptedResponse::sse(vec![(std::time::Duration::ZERO,bytes::Bytes::from_static(b"event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n"))]));
    tokio::try_join!(
        axum::serve(anthropic, cl.router()).into_future(),
        axum::serve(codex, cx.router()).into_future()
    )?;
    Ok(())
}
