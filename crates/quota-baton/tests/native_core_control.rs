//! Synthetic TCP/WebSocket peers only. No native child, core authority fixture,
//! credentials, profiles or provider traffic. Production module is unregistered.
#[allow(dead_code)] // This isolated target exercises a subset of the production adapter.
#[path = "../src/native_resume/core_control.rs"]
mod core_control;

use core_control::{Consumption, CoreControl, Error, Inspection, Observation};
use futures_util::{SinkExt, StreamExt};
use qb_proto::native_reset::{Command, Event, MAX_MESSAGE_BYTES, Refusal, Reply, Request, VERSION};
use std::time::Duration;
use tokio::{
    net::{TcpListener, TcpStream},
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request as Handshake, Response},
        http::HeaderValue,
    },
};

const EPISODE: u64 = 17;
const HANDLE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

async fn pair() -> (CoreControl, WebSocketStream<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}/l/{HANDLE}/v1");
    let connect = CoreControl::connect(&base, HeaderValue::from_static("Bearer synthetic-only"));
    let accept = async {
        let (stream, _) = listener.accept().await.unwrap();
        drop(listener);
        #[allow(clippy::result_large_err)]
        let check = |request: &Handshake, response: Response| {
            assert!(request.uri().path() == format!("/l/{HANDLE}/v1/qb/native-owner"));
            assert!(request.uri().query().is_none());
            assert!(
                request
                    .headers()
                    .get("authorization")
                    .is_some_and(|v| v.as_bytes() == b"Bearer synthetic-only")
            );
            assert!(!request.headers().contains_key("origin"));
            assert!(!request.headers().contains_key("sec-websocket-protocol"));
            assert!(!request.headers().contains_key("sec-websocket-extensions"));
            Ok(response)
        };
        accept_hdr_async(stream, check).await.unwrap()
    };
    let (client, server) = timeout(Duration::from_secs(3), async {
        tokio::join!(connect, accept)
    })
    .await
    .unwrap();
    (client.unwrap(), server)
}

async fn request(server: &mut WebSocketStream<TcpStream>) -> Request {
    let message = timeout(Duration::from_secs(2), server.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(text) = message else {
        panic!("expected request text")
    };
    assert!(text.len() <= MAX_MESSAGE_BYTES);
    let request: Request = serde_json::from_str(&text).unwrap();
    assert!(request.valid_shape());
    request
}

async fn reply(server: &mut WebSocketStream<TcpStream>, id: Option<u64>, event: Event) {
    let text = serde_json::to_string(&Reply {
        version: VERSION,
        request_id: id,
        event,
    })
    .unwrap();
    server.send(Message::Text(text.into())).await.unwrap();
}

async fn waiting(client: &mut CoreControl, server: &mut WebSocketStream<TcpStream>) {
    let serve = async {
        let request = request(server).await;
        assert_eq!(request.request_id, 1);
        assert!(
            matches!(request.command, Command::Observe { thread_id } if thread_id == "thread-a")
        );
        reply(
            server,
            Some(1),
            Event::Waiting {
                episode: EPISODE,
                reset_unix_ms: 1,
            },
        )
        .await;
    };
    let (observed, ()) = tokio::join!(client.observe("thread-a", deadline()), serve);
    assert!(matches!(
        observed,
        Ok(Observation::Waiting {
            episode: EPISODE,
            reset_unix_ms: 1
        })
    ));
}

async fn no_more_requests(server: &mut WebSocketStream<TcpStream>) {
    let closed = timeout(Duration::from_secs(2), server.next())
        .await
        .unwrap();
    assert!(matches!(
        closed,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
}

#[tokio::test]
async fn actual_loopback_keeps_due_during_inspection_and_consumes_exact_once() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    let serve = async {
        let req = request(&mut server).await;
        assert!(matches!(req.command, Command::Inspect { episode: EPISODE }));
        assert_eq!(req.request_id, 2);
        reply(&mut server, None, Event::Due { episode: EPISODE }).await;
        reply(&mut server, Some(2), Event::Ready { episode: EPISODE }).await;
    };
    let (inspected, ()) = tokio::join!(client.inspect(EPISODE, deadline()), serve);
    assert!(matches!(inspected, Ok(Inspection::Ready)));
    assert_eq!(client.next_due(deadline()).await.unwrap(), EPISODE);
    let serve = async {
        let req = request(&mut server).await;
        assert!(matches!(req.command, Command::Consume { episode: EPISODE }));
        assert_eq!(req.request_id, 3);
        reply(&mut server, Some(3), Event::Granted { episode: EPISODE }).await;
    };
    let (consumed, ()) = tokio::join!(client.consume(EPISODE, deadline()), serve);
    let Ok(Consumption::Granted(grant)) = consumed else {
        panic!("expected exact grant")
    };
    assert_eq!(grant.request_id(), 3);
    assert!(grant.authorizes(EPISODE));
    assert!(matches!(
        client.consume(EPISODE, deadline()).await,
        Err(Error::Closed)
    ));
    assert!(client.is_closed());
    no_more_requests(&mut server).await;
}

#[tokio::test]
async fn ready_wrong_episode_wrong_request_and_unsolicited_grant_never_authorize() {
    for (id, event) in [
        (Some(2), Event::Ready { episode: EPISODE }),
        (Some(1), Event::Granted { episode: EPISODE }),
        (
            Some(2),
            Event::Granted {
                episode: EPISODE + 1,
            },
        ),
        (None, Event::Granted { episode: EPISODE }),
    ] {
        let (mut client, mut server) = pair().await;
        waiting(&mut client, &mut server).await;
        let serve = async {
            let req = request(&mut server).await;
            assert!(matches!(req.command, Command::Consume { episode: EPISODE }));
            reply(&mut server, id, event).await;
        };
        let (result, ()) = tokio::join!(client.consume(EPISODE, deadline()), serve);
        assert!(matches!(result, Err(Error::Protocol | Error::Replay)));
        assert!(client.is_closed());
        assert!(matches!(
            client.consume(EPISODE, deadline()).await,
            Err(Error::Closed)
        ));
        no_more_requests(&mut server).await;
    }
}

#[tokio::test]
async fn peer_close_after_consume_is_unknown_and_never_retried() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    let serve = async move {
        let req = request(&mut server).await;
        assert!(matches!(req.command, Command::Consume { episode: EPISODE }));
        server.send(Message::Close(None)).await.unwrap();
    };
    let (result, ()) = tokio::join!(client.consume(EPISODE, deadline()), serve);
    assert!(matches!(result, Err(Error::Closed)));
    assert!(client.is_closed());
    assert!(matches!(
        client.consume(EPISODE, deadline()).await,
        Err(Error::Closed)
    ));
}

#[tokio::test]
async fn cancellation_after_physical_request_drops_the_only_socket() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    let mut pending = Box::pin(client.consume(EPISODE, deadline()));
    let req = tokio::select! {
        result = &mut pending => { assert!(result.is_err()); panic!("reply must remain pending") },
        req = request(&mut server) => req,
    };
    assert!(matches!(req.command, Command::Consume { episode: EPISODE }));
    drop(pending);
    assert!(client.is_closed());
    assert!(matches!(
        client.inspect(EPISODE, deadline()).await,
        Err(Error::Closed)
    ));
    no_more_requests(&mut server).await;
}

#[tokio::test]
async fn replayed_reply_and_conflicting_hint_permanently_close() {
    for replay_reply in [false, true] {
        let (mut client, mut server) = pair().await;
        waiting(&mut client, &mut server).await;
        let serve = async {
            request(&mut server).await;
            if replay_reply {
                reply(
                    &mut server,
                    Some(1),
                    Event::Waiting {
                        episode: EPISODE,
                        reset_unix_ms: 1,
                    },
                )
                .await;
            } else {
                reply(
                    &mut server,
                    None,
                    Event::Due {
                        episode: EPISODE + 1,
                    },
                )
                .await;
            }
        };
        let (result, ()) = tokio::join!(client.inspect(EPISODE, deadline()), serve);
        assert!(matches!(result, Err(Error::Replay | Error::Protocol)));
        assert!(client.is_closed());
        no_more_requests(&mut server).await;
    }
}

#[tokio::test]
async fn repeated_due_hints_coalesce_without_becoming_a_reply_or_grant() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    let serve = async {
        let req = request(&mut server).await;
        for _ in 0..8 {
            reply(&mut server, None, Event::Due { episode: EPISODE }).await;
        }
        reply(
            &mut server,
            Some(req.request_id),
            Event::Ready { episode: EPISODE },
        )
        .await;
    };
    let (result, ()) = tokio::join!(client.inspect(EPISODE, deadline()), serve);
    assert!(matches!(result, Ok(Inspection::Ready)));
    assert_eq!(client.next_due(deadline()).await.unwrap(), EPISODE);
    // There was only one retained slot. No second notification can be returned
    // without reading the socket, where cancellation must still fail closed.
    let serve = async {
        reply(&mut server, None, Event::Cancelled { episode: EPISODE }).await;
    };
    let (result, ()) = tokio::join!(client.next_due(deadline()), serve);
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(client.is_closed());
    no_more_requests(&mut server).await;
}

#[tokio::test]
async fn malformed_binary_and_oversized_messages_fail_closed() {
    for message in [
        Message::Text("{private-invalid".into()),
        Message::Binary(vec![1].into()),
        Message::Text(" ".repeat(MAX_MESSAGE_BYTES + 1).into()),
    ] {
        let (mut client, mut server) = pair().await;
        waiting(&mut client, &mut server).await;
        let serve = async {
            request(&mut server).await;
            server.send(message).await.unwrap();
        };
        let (result, ()) = tokio::join!(client.inspect(EPISODE, deadline()), serve);
        let Err(error) = result else {
            panic!("invalid peer must fail")
        };
        assert!(matches!(error, Error::Protocol | Error::Transport));
        assert!(!error.to_string().contains("private"));
        assert!(client.is_closed());
        no_more_requests(&mut server).await;
    }
}

#[tokio::test]
async fn elapsed_original_deadline_sends_nothing() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    assert!(matches!(
        client.consume(EPISODE, Instant::now()).await,
        Err(Error::Timeout)
    ));
    no_more_requests(&mut server).await;
}

#[tokio::test]
async fn long_caller_deadline_cannot_extend_the_grant_beyond_five_seconds() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    let until = Instant::now() + Duration::from_secs(3600);
    let serve = async {
        request(&mut server).await;
        let received_at = Instant::now();
        reply(&mut server, Some(2), Event::Granted { episode: EPISODE }).await;
        received_at
    };
    let (result, received_at) = tokio::join!(client.consume(EPISODE, until), serve);
    let Ok(Consumption::Granted(grant)) = result else {
        panic!("expected timely grant")
    };
    // The request was physically received AFTER the consume budget started.
    // This rejects accidentally retaining the caller's one-hour deadline and
    // needs neither a paused Tokio clock nor a tight sleep/latency assertion.
    assert!(grant.test_deadline() <= received_at + Duration::from_secs(5));
    assert!(grant.test_deadline() < until);
    assert!(grant.authorizes(EPISODE));
    client.close();
    no_more_requests(&mut server).await;
}

#[tokio::test]
async fn idle_wait_cancellation_and_elapsed_deadline_close_without_a_due_hint() {
    for cancelled in [true, false] {
        let (mut client, mut server) = pair().await;
        waiting(&mut client, &mut server).await;
        if cancelled {
            let mut pending = Box::pin(client.next_due(deadline()));
            assert!(futures_util::poll!(pending.as_mut()).is_pending());
            drop(pending);
        } else {
            assert!(matches!(
                client.next_due(Instant::now()).await,
                Err(Error::Timeout)
            ));
        }
        assert!(client.is_closed());
        assert!(matches!(
            client.inspect(EPISODE, deadline()).await,
            Err(Error::Closed)
        ));
        no_more_requests(&mut server).await;
    }
}

#[tokio::test]
async fn grant_is_bound_to_episode_and_live_owned_socket() {
    for wrong_episode in [false, true] {
        let (mut client, mut server) = pair().await;
        waiting(&mut client, &mut server).await;
        let serve = async {
            request(&mut server).await;
            reply(&mut server, Some(2), Event::Granted { episode: EPISODE }).await;
        };
        let (result, ()) = tokio::join!(client.consume(EPISODE, deadline()), serve);
        let Ok(Consumption::Granted(grant)) = result else {
            panic!("expected grant")
        };
        assert!(grant.is_current(EPISODE));
        assert!(grant.is_current(EPISODE));
        assert!(!grant.is_current(EPISODE + 1));
        if wrong_episode {
            assert!(!grant.authorizes(EPISODE + 1));
            client.close();
        } else {
            drop(client);
            assert!(!grant.is_current(EPISODE));
            assert!(!grant.authorizes(EPISODE));
        }
        no_more_requests(&mut server).await;
    }
}

#[tokio::test]
async fn cancellation_only_due_wait_receives_hint_or_closes_when_dropped() {
    for cancelled in [false, true] {
        let (mut client, mut server) = pair().await;
        waiting(&mut client, &mut server).await;
        let mut pending = Box::pin(client.next_due_until_cancelled());
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        if cancelled {
            drop(pending);
            assert!(client.is_closed());
        } else {
            reply(&mut server, None, Event::Due { episode: EPISODE }).await;
            assert_eq!(
                timeout(Duration::from_secs(2), pending)
                    .await
                    .unwrap()
                    .unwrap(),
                EPISODE
            );
            assert!(!client.is_closed());
            client.close();
        }
        no_more_requests(&mut server).await;
    }
}

#[tokio::test]
async fn retained_grant_borrow_checks_explicit_close_and_original_deadline() {
    for close in [true, false] {
        let (mut client, mut server) = pair().await;
        waiting(&mut client, &mut server).await;
        let until = Instant::now() + Duration::from_millis(300);
        let serve = async {
            let req = request(&mut server).await;
            assert!(matches!(req.command, Command::Consume { episode: EPISODE }));
            reply(
                &mut server,
                Some(req.request_id),
                Event::Granted { episode: EPISODE },
            )
            .await;
        };
        let (result, ()) = tokio::join!(client.consume(EPISODE, until), serve);
        let Ok(Consumption::Granted(grant)) = result else {
            panic!("expected exact grant")
        };
        assert!(grant.is_current(EPISODE));
        // Retain the same noncloneable receipt until explicit invalidation.
        if close {
            client.close();
        } else {
            timeout(
                Duration::from_secs(2),
                tokio::time::sleep_until(grant.test_deadline()),
            )
            .await
            .unwrap();
        }
        assert!(!grant.is_current(EPISODE));
        assert!(!grant.authorizes(EPISODE));
        client.close();
        no_more_requests(&mut server).await;
    }
}

#[tokio::test]
async fn refused_inspection_is_not_ready_and_cancel_cannot_grant() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    let serve = async {
        let req = request(&mut server).await;
        reply(
            &mut server,
            Some(req.request_id),
            Event::Refused {
                reason: Refusal::NotDue,
            },
        )
        .await;
    };
    let (result, ()) = tokio::join!(client.inspect(EPISODE, deadline()), serve);
    assert!(matches!(result, Ok(Inspection::Refused(Refusal::NotDue))));
    let serve = async {
        let req = request(&mut server).await;
        assert!(matches!(req.command, Command::Cancel { episode: EPISODE }));
        reply(
            &mut server,
            Some(req.request_id),
            Event::Cancelled { episode: EPISODE },
        )
        .await;
    };
    let (result, ()) = tokio::join!(client.cancel(EPISODE, deadline()), serve);
    assert!(result.is_ok());
    assert!(client.is_closed());
    no_more_requests(&mut server).await;
}

#[tokio::test]
async fn observe_due_and_refused_consumption_are_hints_not_grants() {
    let (mut client, mut server) = pair().await;
    let serve = async {
        let req = request(&mut server).await;
        reply(
            &mut server,
            Some(req.request_id),
            Event::Due { episode: EPISODE },
        )
        .await;
    };
    let (result, ()) = tokio::join!(client.observe("thread-a", deadline()), serve);
    assert!(matches!(result, Ok(Observation::Due { episode: EPISODE })));
    let serve = async {
        let req = request(&mut server).await;
        reply(
            &mut server,
            Some(req.request_id),
            Event::Refused {
                reason: Refusal::Unavailable,
            },
        )
        .await;
    };
    let (result, ()) = tokio::join!(client.consume(EPISODE, deadline()), serve);
    assert!(matches!(
        result,
        Ok(Consumption::Refused(Refusal::Unavailable))
    ));
    assert!(client.is_closed());
    no_more_requests(&mut server).await;

    let (mut client, mut server) = pair().await;
    let serve = async {
        let req = request(&mut server).await;
        reply(
            &mut server,
            Some(req.request_id),
            Event::Refused {
                reason: Refusal::Unavailable,
            },
        )
        .await;
    };
    let (result, ()) = tokio::join!(client.observe("thread-a", deadline()), serve);
    assert!(matches!(
        result,
        Ok(Observation::Refused(Refusal::Unavailable))
    ));
    assert!(client.is_closed());
    no_more_requests(&mut server).await;
}

#[tokio::test]
async fn real_pending_read_honors_original_deadline_and_no_reconnect() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    let until = Instant::now() + Duration::from_millis(100);
    let consume = client.consume(EPISODE, until);
    let serve = async {
        let req = request(&mut server).await;
        assert!(matches!(req.command, Command::Consume { episode: EPISODE }));
    };
    let (result, ()) = tokio::join!(consume, serve);
    let completed_at = Instant::now();
    assert!(matches!(result, Err(Error::Timeout)));
    assert!(completed_at >= until);
    assert!(
        completed_at <= until + Duration::from_secs(1),
        "pending consume must honor its original 100 ms deadline"
    );
    assert!(client.is_closed());
    assert!(matches!(
        client.observe("thread-a", deadline()).await,
        Err(Error::Closed)
    ));
    // The peer's two-second closure bound is separate from the client's 100 ms
    // deadline; equal concurrent budgets previously expired on the same tick.
    // An unexpected extra request remains buffered and still fails this check.
    no_more_requests(&mut server).await;
}

#[tokio::test]
async fn bearer_is_separate_from_url_and_handshake_rejection_does_not_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}/l/{HANDLE}/v1");
    assert!(matches!(
        CoreControl::connect(&base, HeaderValue::from_static("Basic invalid")).await,
        Err(Error::InvalidBearer)
    ));
    let serve = async {
        let (stream, _) = listener.accept().await.unwrap();
        // A plain TCP peer closing the only handshake cannot trigger a retry.
        drop(stream);
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    };
    let connect = CoreControl::connect(&base, HeaderValue::from_static("Bearer synthetic-only"));
    let (result, ()) = tokio::join!(connect, serve);
    assert!(matches!(result, Err(Error::Handshake)));
}

#[tokio::test]
async fn cancelled_hint_is_terminal_even_while_readiness_reply_is_pending() {
    let (mut client, mut server) = pair().await;
    waiting(&mut client, &mut server).await;
    let serve = async {
        request(&mut server).await;
        reply(&mut server, None, Event::Cancelled { episode: EPISODE }).await;
    };
    let (result, ()) = tokio::join!(client.inspect(EPISODE, deadline()), serve);
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(client.is_closed());
    no_more_requests(&mut server).await;
}
