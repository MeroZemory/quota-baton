//! Actual synthetic loopback WS only; no native process or live authority.
#[path = "../src/native_resume/control_actor.rs"]
mod control_actor;
#[path = "../src/native_resume/core_control.rs"]
mod core_control;

use std::{future::Future, time::Duration};

use control_actor::{Error, channels, run};
use core_control::{Consumption, CoreControl, Inspection, Observation};
use futures_util::{SinkExt, StreamExt};
use qb_proto::native_reset::{Command, Event, Refusal, Reply, Request, VERSION};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_async,
    tungstenite::{Message, http::HeaderValue},
};

const EPISODE: u64 = 17;
const BUDGET: Duration = Duration::from_secs(3);

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic operation failed"),
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    require(timeout(Duration::from_secs(8), future).await)
}

fn deadline() -> Instant {
    Instant::now() + BUDGET
}

async fn pair() -> (CoreControl, WebSocketStream<TcpStream>) {
    let listener = require(TcpListener::bind("127.0.0.1:0").await);
    let address = require(listener.local_addr());
    let base = format!("http://{address}/l/{}/v1", "a".repeat(43));
    let connect = CoreControl::connect(&base, HeaderValue::from_static("Bearer synthetic-only"));
    let accept = async {
        let (stream, _) = require(listener.accept().await);
        require(accept_async(stream).await)
    };
    let (client, server) = bounded(async { tokio::join!(connect, accept) }).await;
    let client = require(client);
    assert!(!client.is_closed());
    (client, server)
}

async fn request(server: &mut WebSocketStream<TcpStream>) -> Request {
    let message = require(require(timeout(BUDGET, server.next()).await).expect("request"));
    let Message::Text(text) = message else {
        panic!("expected request")
    };
    let request: Request = require(serde_json::from_str(&text));
    assert!(request.valid_shape());
    request
}

async fn reply(server: &mut WebSocketStream<TcpStream>, id: Option<u64>, event: Event) {
    let bytes = require(serde_json::to_string(&Reply {
        version: VERSION,
        request_id: id,
        event,
    }));
    require(require(
        timeout(BUDGET, server.send(Message::Text(bytes.into()))).await,
    ));
}

async fn observed(server: &mut WebSocketStream<TcpStream>, immediate: bool) {
    let request = request(server).await;
    assert!(matches!(request.command, Command::Observe { thread_id } if thread_id == "thread-a"));
    let event = if immediate {
        Event::Due { episode: EPISODE }
    } else {
        Event::Waiting {
            episode: EPISODE,
            reset_unix_ms: 1,
        }
    };
    reply(server, Some(request.request_id), event).await;
}

async fn inspected(server: &mut WebSocketStream<TcpStream>, ready: bool) {
    let request = request(server).await;
    assert!(matches!(
        request.command,
        Command::Inspect { episode: EPISODE }
    ));
    let event = if ready {
        Event::Ready { episode: EPISODE }
    } else {
        Event::Refused {
            reason: Refusal::Unavailable,
        }
    };
    reply(server, Some(request.request_id), event).await;
}

async fn closed(server: &mut WebSocketStream<TcpStream>) {
    let next = require(timeout(BUDGET, server.next()).await);
    assert!(matches!(
        next,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
}

#[tokio::test]
async fn actual_waiting_due_explicit_inspect_consume_and_close_exact_once() {
    let (control, mut server) = pair().await;
    let (mut parent, io) = channels();
    let mut due = parent.take_due().expect("owned Due receiver");
    assert!(parent.take_due().is_none());
    let parent_work = async {
        assert!(matches!(
            parent.observe("thread-a", deadline()).await,
            Ok(Observation::Waiting {
                episode: EPISODE,
                reset_unix_ms: 1
            })
        ));
        let hint = require(timeout(BUDGET, due.recv()).await).expect("Due hint");
        assert_eq!(hint.episode(), EPISODE);
        assert!(matches!(
            parent.inspect(hint.episode(), deadline()).await,
            Ok(Inspection::Ready)
        ));
        let consumption = require(parent.consume(EPISODE, deadline()).await);
        let Consumption::Granted(grant) = consumption else {
            panic!("expected grant")
        };
        assert_eq!(grant.request_id(), 3);
        assert!(grant.test_deadline() > Instant::now());
        assert!(grant.authorizes(EPISODE));
        require(parent.close(deadline()).await);
    };
    let peer = async {
        observed(&mut server, false).await;
        reply(&mut server, None, Event::Due { episode: EPISODE }).await;
        inspected(&mut server, true).await;
        let consume = request(&mut server).await;
        assert!(matches!(
            consume.command,
            Command::Consume { episode: EPISODE }
        ));
        reply(
            &mut server,
            Some(consume.request_id),
            Event::Granted { episode: EPISODE },
        )
        .await;
        closed(&mut server).await;
    };
    let (result, (), ()) =
        bounded(async { tokio::join!(run(control, io, deadline()), parent_work, peer) }).await;
    require(result);
}

#[tokio::test]
async fn due_alone_never_sends_inspect_or_consume_and_parent_loss_closes() {
    let (control, mut server) = pair().await;
    let (mut parent, io) = channels();
    let mut due = parent.take_due().expect("owned Due receiver");
    let work = async {
        assert!(matches!(
            parent.observe("thread-a", deadline()).await,
            Ok(Observation::Due { episode: EPISODE })
        ));
        assert_eq!(due.recv().await.expect("hint").episode(), EPISODE);
        drop(parent);
    };
    let peer = async {
        observed(&mut server, true).await;
        closed(&mut server).await;
    };
    let (result, (), ()) =
        bounded(async { tokio::join!(run(control, io, deadline()), work, peer) }).await;
    assert!(matches!(
        result,
        Err(Error::Cancelled | Error::ParentGone | Error::HintLost)
    ));
}

#[tokio::test]
async fn inspect_or_consume_before_due_fails_closed_without_wire_command() {
    for consume in [false, true] {
        let (control, mut server) = pair().await;
        let (parent, io) = channels();
        let work = async {
            assert!(matches!(
                parent.observe("thread-a", deadline()).await,
                Ok(Observation::Waiting { .. })
            ));
            let result = if consume {
                parent.consume(EPISODE, deadline()).await.map(|_| ())
            } else {
                parent.inspect(EPISODE, deadline()).await.map(|_| ())
            };
            assert_eq!(result, Err(Error::Phase));
        };
        let peer = async {
            observed(&mut server, false).await;
            closed(&mut server).await;
        };
        let (result, (), ()) =
            bounded(async { tokio::join!(run(control, io, deadline()), work, peer) }).await;
        assert_eq!(result, Err(Error::Phase));
    }
}

#[tokio::test]
async fn close_while_next_due_polled_closes_socket_and_sends_zero_consume() {
    let (control, mut server) = pair().await;
    let (parent, io) = channels();
    let work = async {
        require(parent.observe("thread-a", deadline()).await);
        require(parent.close(deadline()).await);
    };
    let peer = async {
        observed(&mut server, false).await;
        closed(&mut server).await;
    };
    let (result, (), ()) =
        bounded(async { tokio::join!(run(control, io, deadline()), work, peer) }).await;
    require(result);
}

#[tokio::test]
async fn cancellation_before_inspect_response_closes_with_zero_consume() {
    let (control, mut server) = pair().await;
    let (mut parent, io) = channels();
    let mut due = parent.take_due().expect("owned Due receiver");
    let (seen_tx, seen_rx) = oneshot::channel();
    let work = async {
        require(parent.observe("thread-a", deadline()).await);
        due.recv().await.expect("hint");
        let inspect = parent.inspect(EPISODE, deadline());
        tokio::pin!(inspect);
        tokio::select! {
            biased;
            result = &mut inspect => { let _ = result; panic!("peer has not answered"); }
            result = seen_rx => { require(result); }
        }
        // Dropping the polled parent request closes the entire episode.
    };
    let peer = async {
        observed(&mut server, true).await;
        assert!(matches!(
            request(&mut server).await.command,
            Command::Inspect { .. }
        ));
        require(seen_tx.send(()));
        closed(&mut server).await;
    };
    let (result, (), ()) =
        bounded(async { tokio::join!(run(control, io, deadline()), work, peer) }).await;
    assert!(matches!(result, Err(Error::Cancelled | Error::ReplyLost)));
}

#[tokio::test]
async fn inspect_refusal_is_terminal_no_retry_or_consume() {
    let (control, mut server) = pair().await;
    let (mut parent, io) = channels();
    let mut due = parent.take_due().expect("owned Due receiver");
    let work = async {
        require(parent.observe("thread-a", deadline()).await);
        due.recv().await.expect("hint");
        assert!(matches!(
            parent.inspect(EPISODE, deadline()).await,
            Ok(Inspection::Refused(Refusal::Unavailable))
        ));
        assert!(parent.consume(EPISODE, deadline()).await.is_err());
    };
    let peer = async {
        observed(&mut server, true).await;
        inspected(&mut server, false).await;
        closed(&mut server).await;
    };
    let (result, (), ()) =
        bounded(async { tokio::join!(run(control, io, deadline()), work, peer) }).await;
    assert_eq!(result, Err(Error::Refused));
}

#[tokio::test]
async fn lost_reply_receiver_after_physical_consume_closes_without_second_command() {
    let (control, mut server) = pair().await;
    let (mut parent, io) = channels();
    let mut due = parent.take_due().expect("owned Due receiver");
    let (seen_tx, seen_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let work = async {
        require(parent.observe("thread-a", deadline()).await);
        due.recv().await.expect("hint");
        require(parent.inspect(EPISODE, deadline()).await);
        {
            let consume = parent.consume(EPISODE, deadline());
            tokio::pin!(consume);
            tokio::select! {
                result = &mut consume => { let _ = result; panic!("peer has not granted"); }
                result = seen_rx => { require(result); }
            }
        }
        require(dropped_tx.send(()));
        assert!(parent.consume(EPISODE, deadline()).await.is_err());
    };
    let peer = async {
        observed(&mut server, true).await;
        inspected(&mut server, true).await;
        let consumed = request(&mut server).await;
        assert!(matches!(
            consumed.command,
            Command::Consume { episode: EPISODE }
        ));
        require(seen_tx.send(()));
        require(dropped_rx.await);
        // The physical request occurred, but no valid grant can reach the parent.
        // A late response may fail to send because cancellation already closed TCP.
        let text = require(serde_json::to_string(&Reply {
            version: VERSION,
            request_id: Some(consumed.request_id),
            event: Event::Granted { episode: EPISODE },
        }));
        let _ = timeout(BUDGET, server.send(Message::Text(text.into()))).await;
        closed(&mut server).await;
    };
    let (result, (), ()) =
        bounded(async { tokio::join!(run(control, io, deadline()), work, peer) }).await;
    assert!(matches!(result, Err(Error::Cancelled | Error::ReplyLost)));
}

#[tokio::test]
async fn queued_commands_are_bounded_and_expired_command_never_reaches_peer() {
    let (parent, _io) = channels();
    let mut pending = Vec::new();
    for _ in 0..control_actor::COMMAND_CAPACITY {
        pending.push(Box::pin(parent.inspect(EPISODE, deadline())));
    }
    // Poll each request once without starting an actor: every queue slot is held.
    for request in &mut pending {
        tokio::select! {
            biased;
            result = request.as_mut() => { let _ = result; panic!("unserved request finished"); }
            () = std::future::ready(()) => {}
        }
    }
    assert_eq!(
        parent.inspect(EPISODE, deadline()).await.map(|_| ()),
        Err(Error::QueueFull)
    );
    drop(pending);
    drop(parent);

    let (control, mut server) = pair().await;
    let (parent, io) = channels();
    let work = async {
        assert!(matches!(
            parent.observe("thread-a", Instant::now()).await,
            Err(Error::Timeout)
        ));
    };
    let peer = async {
        closed(&mut server).await;
    };
    let (result, (), ()) =
        bounded(async { tokio::join!(run(control, io, deadline()), work, peer) }).await;
    assert_eq!(result, Err(Error::Cancelled));
}

#[tokio::test]
async fn dropping_actor_future_after_observe_closes_polled_due_wait() {
    let (control, mut server) = pair().await;
    let (parent, io) = channels();
    let mut actor = Box::pin(run(control, io, deadline()));
    let observe = parent.observe("thread-a", deadline());
    let peer = async {
        observed(&mut server, false).await;
    };
    let observed = bounded(async {
        tokio::select! {
            result = &mut actor => { let _ = result; panic!("actor ended early"); }
            (result, ()) = async { tokio::join!(observe, peer) } => result,
        }
    })
    .await;
    require(observed);
    drop(actor);
    closed(&mut server).await;
    assert!(parent.inspect(EPISODE, deadline()).await.is_err());
}

#[tokio::test]
async fn observe_and_consume_refusals_are_typed_terminal_results() {
    for refuse_observe in [true, false] {
        let (control, mut server) = pair().await;
        let (mut parent, io) = channels();
        let mut due = parent.take_due().expect("owned Due receiver");
        let work = async {
            let observation = require(parent.observe("thread-a", deadline()).await);
            if refuse_observe {
                assert!(matches!(
                    observation,
                    Observation::Refused(Refusal::Unavailable)
                ));
            } else {
                due.recv().await.expect("hint");
                require(parent.inspect(EPISODE, deadline()).await);
                assert!(matches!(
                    parent.consume(EPISODE, deadline()).await,
                    Ok(Consumption::Refused(Refusal::Unavailable))
                ));
            }
        };
        let peer = async {
            if refuse_observe {
                let observe = request(&mut server).await;
                reply(
                    &mut server,
                    Some(observe.request_id),
                    Event::Refused {
                        reason: Refusal::Unavailable,
                    },
                )
                .await;
            } else {
                observed(&mut server, true).await;
                inspected(&mut server, true).await;
                let consume = request(&mut server).await;
                assert!(matches!(
                    consume.command,
                    Command::Consume { episode: EPISODE }
                ));
                reply(
                    &mut server,
                    Some(consume.request_id),
                    Event::Refused {
                        reason: Refusal::Unavailable,
                    },
                )
                .await;
            }
            closed(&mut server).await;
        };
        let (result, (), ()) =
            bounded(async { tokio::join!(run(control, io, deadline()), work, peer) }).await;
        assert_eq!(result, Err(Error::Refused));
    }
}

#[tokio::test]
async fn idle_due_wait_lifetime_expires_without_polling_another_command() {
    let (control, mut server) = pair().await;
    let (parent, io) = channels();
    let work = async {
        require(parent.observe("thread-a", deadline()).await);
    };
    let peer = async {
        observed(&mut server, false).await;
        closed(&mut server).await;
    };
    let lifetime = Instant::now() + Duration::from_millis(100);
    let (result, (), ()) =
        bounded(async { tokio::join!(run(control, io, lifetime), work, peer) }).await;
    assert_eq!(result, Err(Error::Core(core_control::Error::Timeout)));
}

#[tokio::test]
async fn explicit_client_cancel_baseline_closes_instead_of_granting() {
    // Staged CoreControl public surface remains exercised in this test target;
    // actor Close is deliberately local, unlike this explicit protocol Cancel.
    let (mut control, mut server) = pair().await;
    let peer = async {
        observed(&mut server, true).await;
    };
    let (observed, ()) =
        bounded(async { tokio::join!(control.observe("thread-a", deadline()), peer) }).await;
    require(observed);
    let peer = async {
        let cancel = request(&mut server).await;
        assert!(matches!(
            cancel.command,
            Command::Cancel { episode: EPISODE }
        ));
        reply(
            &mut server,
            Some(cancel.request_id),
            Event::Cancelled { episode: EPISODE },
        )
        .await;
        closed(&mut server).await;
    };
    let (result, ()) =
        bounded(async { tokio::join!(control.cancel(EPISODE, deadline()), peer) }).await;
    require(result);
    assert!(control.is_closed());
}
