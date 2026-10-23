//! Synthetic WebSockets only. No spawned tasks, native processes or authority.
#[path = "../src/native_resume/endpoint.rs"]
mod endpoint;
#[path = "../src/native_resume/framing.rs"]
mod framing;
#[path = "../src/native_resume/socket_pump.rs"]
mod socket_pump;

use std::{future::Future, io::Cursor, time::Duration};

use endpoint::Endpoint;
use futures_util::{SinkExt, StreamExt};
use socket_pump::{Error, Frame, QUEUE_FRAMES, channels, run, run_test_socket};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout},
};
use tokio_tungstenite::tungstenite::{
    Message,
    client::IntoClientRequest,
    http::header::AUTHORIZATION,
    protocol::{Role, WebSocketConfig},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic operation failed"),
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    require(timeout(Duration::from_secs(12), future).await)
}

async fn pair() -> (
    WebSocketStream<endpoint::EndpointIo>,
    WebSocketStream<MaybeTlsStream<TcpStream>>,
) {
    let endpoint = require(Endpoint::bind().await);
    let mut request = require(endpoint.url().into_client_request());
    let mut auth = require(
        format!("Bearer {}", endpoint.token())
            .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>(),
    );
    auth.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, auth);
    let (server, client) = tokio::join!(
        endpoint.accept(Instant::now() + Duration::from_secs(5)),
        connect_async(request),
    );
    (require(server), require(client).0)
}

fn frame(bytes: &[u8]) -> Frame {
    require(Frame::new(bytes.to_vec()))
}

fn text(bytes: &[u8]) -> Message {
    Message::Text(require(String::from_utf8(bytes.to_vec())).into())
}

#[tokio::test]
async fn authenticated_socket_exact_text_bytes_both_directions_and_ping_pong() {
    bounded(async {
        let (server, mut client) = pair().await;
        let (mut parent, io) = channels();
        let incoming = br#" { "id" : "ui-\u0031", "method":"account/read", "params":{} } "#;
        let outgoing = br#" {"id":"ui-1", "result": {"value":"\u0061"} } "#;
        let peer = async {
            require(client.send(text(incoming)).await);
            assert_eq!(require(parent.from_ui.recv().await.ok_or(())).as_bytes(), incoming);
            require(parent.to_ui.send(frame(outgoing)).await);
            let message = require(client.next().await.ok_or(()));
            assert_eq!(require(message).into_text().ok().as_deref(), Some(require(std::str::from_utf8(outgoing))));
            require(client.send(Message::Ping(b"owned-ping".to_vec().into())).await);
            assert!(matches!(require(require(client.next().await.ok_or(()))), Message::Pong(bytes) if bytes.as_ref() == b"owned-ping"));
            assert!(parent.from_ui.try_recv().is_err(), "control messages are not JSON work");
            require(client.send(Message::Pong(b"unsolicited".to_vec().into())).await);
            require(client.send(Message::Close(None)).await);
        };
        let (result, ()) = tokio::join!(run(server, io), peer);
        assert_eq!(result, Err(Error::PeerClosed));
        assert!(parent.from_ui.recv().await.is_none());
    }).await;
}

#[tokio::test]
async fn binary_malformed_multiline_and_nonobject_frames_refuse_and_close_queues() {
    bounded(async {
        for (message, expected) in [
            (Message::Binary(vec![1, 2, 3].into()), Error::Binary),
            (text(b"{private"), Error::InvalidFrame),
            (text(b"[]"), Error::InvalidFrame),
            (text(b"{}\n"), Error::InvalidFrame),
            (text(b"{}\r"), Error::InvalidFrame),
            (text(b""), Error::InvalidFrame),
        ] {
            let (server, mut client) = pair().await;
            let (mut parent, io) = channels();
            let peer = async {
                require(client.send(message).await);
            };
            let (result, ()) = tokio::join!(run(server, io), peer);
            assert_eq!(result, Err(expected));
            assert!(parent.from_ui.recv().await.is_none());
            assert!(parent.to_ui.send(frame(b"{}")).await.is_err());
            assert!(!expected.to_string().contains("private"));
        }
    })
    .await;
}

#[tokio::test]
async fn eight_frame_queues_and_inbound_backpressure_have_deadlines() {
    bounded(async {
        let (server, mut client) = pair().await;
        let (parent, io) = channels();
        assert_eq!(parent.from_ui.max_capacity(), QUEUE_FRAMES);
        assert_eq!(QUEUE_FRAMES, 8);
        let peer = async {
            for _ in 0..=QUEUE_FRAMES {
                require(client.send(text(b"{}")).await);
            }
        };
        let (result, ()) = tokio::join!(
            run_test_socket(server, io, Duration::from_millis(100)),
            peer,
        );
        assert_eq!(result, Err(Error::BackpressureTimeout));
        assert!(parent.to_ui.send(frame(b"{}")).await.is_err());
        // Already queued input is not authority; owner must discard it on exit.
        drop(parent);
    })
    .await;
}

#[tokio::test]
async fn parent_channel_closure_cancels_other_direction_even_when_idle() {
    bounded(async {
        for receiving in [false, true] {
            let (server, _client) = pair().await;
            let (parent, io) = channels();
            let socket_pump::ParentIo { from_ui, to_ui } = parent;
            if receiving {
                drop(from_ui);
                assert_eq!(run(server, io).await, Err(Error::ParentClosed));
                drop(to_ui);
            } else {
                drop(to_ui);
                assert_eq!(run(server, io).await, Err(Error::ParentClosed));
                drop(from_ui);
            }
        }
    })
    .await;
}

#[tokio::test]
async fn peer_eof_and_dropping_pump_release_all_socket_queue_owners() {
    bounded(async {
        let (server, client) = pair().await;
        let (mut parent, io) = channels();
        drop(client); // Abrupt TCP EOF, not a graceful WebSocket close.
        assert_eq!(run(server, io).await, Err(Error::WebSocket));
        assert!(parent.from_ui.recv().await.is_none());

        let (server, mut client) = pair().await;
        let (mut parent, io) = channels();
        // Explicitly drop a POLLED idle pump. No spawned work can retain halves.
        let mut pump = Box::pin(run(server, io));
        tokio::select! {
            result = &mut pump => panic!("idle pump exited: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
        drop(pump);
        assert!(parent.from_ui.recv().await.is_none());
        assert!(parent.to_ui.send(frame(b"{}")).await.is_err());
        assert!(matches!(client.next().await, None | Some(Err(_))));
    })
    .await;
}

#[tokio::test]
async fn blocked_real_websocket_sink_times_out_without_detached_writer() {
    bounded(async {
        // Tiny in-memory transport makes an actual WS text write deterministically
        // stall. The peer remains open but never reads; no TCP buffer assumptions.
        let (server, peer) = tokio::io::duplex(64);
        let config = WebSocketConfig::default()
            .write_buffer_size(0)
            .max_frame_size(Some(framing::MAX_FRAME_BYTES))
            .max_message_size(Some(framing::MAX_FRAME_BYTES));
        let server = WebSocketStream::from_raw_socket(server, Role::Server, Some(config)).await;
        let (mut parent, io) = channels();
        let payload = format!("{{\"data\":\"{}\"}}", "x".repeat(4096));
        require(parent.to_ui.send(frame(payload.as_bytes())).await);
        assert_eq!(
            run_test_socket(server, io, Duration::from_millis(100)).await,
            Err(Error::SendTimeout)
        );
        assert!(parent.from_ui.recv().await.is_none());
        assert!(parent.to_ui.send(frame(b"{}")).await.is_err());
        drop(peer);
    })
    .await;
}

#[tokio::test]
async fn maximum_text_frame_passes_and_oversized_wire_message_fails() {
    bounded(async {
        let payload = format!("{{\"x\":\"{}\"}}", "x".repeat(framing::MAX_FRAME_BYTES - 8));
        assert_eq!(payload.len(), framing::MAX_FRAME_BYTES);
        let (server, mut client) = pair().await;
        let (mut parent, io) = channels();
        let peer = async {
            require(client.send(text(payload.as_bytes())).await);
            assert_eq!(
                require(parent.from_ui.recv().await.ok_or(())).as_bytes(),
                payload.as_bytes()
            );
            require(client.send(Message::Close(None)).await);
        };
        let (result, ()) = tokio::join!(run(server, io), peer);
        assert_eq!(result, Err(Error::PeerClosed));

        let (server, mut client) = pair().await;
        let (mut parent, io) = channels();
        let peer = async {
            // Peer write may observe the server's early size-limit disconnect.
            let _ = client.send(text(format!("{payload} ").as_bytes())).await;
        };
        let (result, ()) = tokio::join!(run(server, io), peer);
        assert_eq!(result, Err(Error::WebSocket));
        assert!(parent.from_ui.recv().await.is_none());
    })
    .await;
}

#[test]
fn frame_validation_and_existing_stdio_boundary_keep_exact_payload() {
    let payload = br#" {"method":"initialized","params":{}} "#;
    let validated = frame(payload);
    let mut stdio = Vec::new();
    {
        let mut writer = framing::FrameWriter::new(&mut stdio);
        require(writer.send(validated.as_bytes()));
    }
    assert_eq!(&stdio[..stdio.len() - 1], payload);
    assert_eq!(stdio.last(), Some(&b'\n'));
    let mut reader = Cursor::new(stdio);
    assert_eq!(
        require(framing::read_frame(&mut reader)).as_deref(),
        Some(payload.as_slice())
    );
    assert!(require(framing::read_frame(&mut reader)).is_none());
    for bytes in [b"[]".as_slice(), b"{}\n", b"\xff", b"{private"] {
        assert!(matches!(
            Frame::new(bytes.to_vec()),
            Err(Error::InvalidFrame)
        ));
    }
    assert!(matches!(
        Frame::new(vec![b' '; framing::MAX_FRAME_BYTES + 1]),
        Err(Error::FrameBound)
    ));
}

#[test]
fn synchronous_dispatcher_output_enqueue_is_bounded_and_never_blocks() {
    let (parent, socket) = channels();
    for _ in 0..QUEUE_FRAMES {
        require(parent.to_ui.try_send(frame(b"{}")));
    }
    assert_eq!(parent.to_ui.try_send(frame(b"{}")), Err(Error::QueueFull));
    drop(socket);
    assert_eq!(
        parent.to_ui.try_send(frame(b"{}")),
        Err(Error::ParentClosed)
    );
}

#[tokio::test]
async fn async_parent_enqueue_has_its_own_backpressure_deadline() {
    bounded(async {
        let (parent, socket) = channels();
        for _ in 0..QUEUE_FRAMES {
            require(parent.to_ui.send(frame(b"{}")).await);
        }
        assert_eq!(
            parent.to_ui.send(frame(b"{}")).await,
            Err(Error::BackpressureTimeout)
        );
        drop(socket);
        assert_eq!(
            parent.to_ui.send(frame(b"{}")).await,
            Err(Error::ParentClosed)
        );
    })
    .await;
}
