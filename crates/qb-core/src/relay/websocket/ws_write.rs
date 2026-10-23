//! Physical sink adapter: begin once at first start_send after poll_ready.
//! The pump supplies the exact original create context.
//! One adapter owns ONE entire create write, including every continuation.
//! Context is captured once, never refreshed from poll-time mutable context.
use super::ws_attempt::{Leg, Sent, WriteTicket};
use futures_util::Sink;
use std::{
    num::NonZeroUsize,
    pin::Pin,
    task::{Context, Poll},
};
use tokio_tungstenite::tungstenite::Message;

pub(super) struct ForwardWrite<S> {
    inner: S,
    leg: Leg,
    key: Option<String>,
    model: Option<String>,
    started: bool,
    sent: Option<Sent>,
    ticket: Option<WriteTicket>,
    expected_fragments: usize,
    accepted_fragments: usize,
    flushed_fragments: usize,
    failed: bool,
}
impl<S> ForwardWrite<S> {
    // Coordinator derives count from the ORIGINAL message using write()'s exact
    // PARSE_LIMIT + Text/Binary predicate and 64 KiB chunking (otherwise one).
    // A wrong count is a wiring bug; this adapter does not classify creates.
    pub(super) fn new(
        inner: S,
        leg: Leg,
        key: Option<String>,
        model: Option<String>,
        expected_fragments: NonZeroUsize,
    ) -> Self {
        Self {
            inner,
            leg,
            key,
            model,
            started: false,
            sent: None,
            ticket: None,
            expected_fragments: expected_fragments.get(),
            accepted_fragments: 0,
            flushed_fragments: 0,
            failed: false,
        }
    }
    // Called only after write() has flushed ALL fragments successfully. Flush
    // of an intermediate 64 KiB fragment is not completion of the create.
    pub(super) fn finish(mut self, success: bool) -> (S, Option<Sent>) {
        if success
            && !self.failed
            && self.accepted_fragments == self.expected_fragments
            && self.flushed_fragments == self.expected_fragments
        {
            if let Some(ticket) = self.ticket.take() {
                ticket.flush();
            }
        }
        // An unfinished ticket burns witness on failed/partial write.
        drop(self.ticket.take());
        (self.inner, self.sent)
    }

    fn fail(&mut self) {
        self.failed = true;
        // Burn immediately, even if a caller retains the adapter after error.
        drop(self.ticket.take());
    }
}
impl<S: Sink<Message> + Unpin> Sink<Message> for ForwardWrite<S> {
    type Error = S::Error;
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let result = Pin::new(&mut self.inner).poll_ready(cx);
        if matches!(&result, Poll::Ready(Err(_))) {
            self.fail();
        }
        result
    }
    fn start_send(mut self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
        // Sink contract requires a preceding successful poll_ready. Pending
        // readiness and future construction cannot begin a witness.
        if !self.started && !self.failed {
            self.started = true;
            if let Some((sent, ticket)) = self.leg.start(self.key.as_deref(), self.model.as_deref())
            {
                self.sent = Some(sent);
                self.ticket = Some(ticket);
            }
        }
        // Even failed start_send taints: transport may have accepted partial data.
        let result = Pin::new(&mut self.inner).start_send(message);
        if result.is_err() {
            self.fail();
        } else {
            self.accepted_fragments = self.accepted_fragments.saturating_add(1);
            if self.accepted_fragments > self.expected_fragments {
                self.fail();
            }
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        match &result {
            Poll::Ready(Ok(())) => self.flushed_fragments = self.accepted_fragments,
            Poll::Ready(Err(_)) => self.fail(),
            Poll::Pending => {}
        }
        // NO ticket.flush here: each SinkExt::send flushes a fragment.
        result
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Closing is not successful completion of the original write.
        self.fail();
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[cfg(test)]
#[path = "ws_write/tests.rs"]
mod tests;
