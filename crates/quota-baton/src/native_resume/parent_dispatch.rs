//! Serialized local parent adapter; the caller supplies separate core authority.
//! No threads, runtime, timers, or core IPC are created here. Authorization must
//! be bounded by the caller and must not reenter this parent. Teardown must
//! unblock owned physical writers before joining readers waiting on ingress.

use std::io::Write;

use super::{
    dispatcher::{self, Dispatcher},
    goal::{Delivery, StoppedGoalTicket},
    ingress::{self, CommitError, Publisher, Source},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Ingress(ingress::Error),
    Dispatch(dispatcher::Error),
}

pub(crate) struct Parent<N, U> {
    dispatcher: Dispatcher<N, U>,
    ingress: ingress::Owner,
    close: Publisher,
}

// Authorization can unwind without holding the ingress mutex. Nevertheless an
// unwinding owner operation must revoke all surviving readers immediately.
struct UnwindClose(Option<Publisher>);
impl Drop for UnwindClose {
    fn drop(&mut self) {
        if let Some(publisher) = self.0.take() {
            publisher.close();
        }
    }
}

impl<N: Write, U: Write> Parent<N, U> {
    pub(crate) fn new(namespace: String, native: N, ui: U) -> Result<(Self, Publisher), Error> {
        let dispatcher = Dispatcher::new(namespace, native, ui).map_err(Error::Dispatch)?;
        let (ingress, publisher) = ingress::channel();
        Ok((
            Self {
                dispatcher,
                ingress,
                close: publisher.clone(),
            },
            publisher,
        ))
    }

    pub(crate) fn stopped_ticket(&mut self) -> Option<StoppedGoalTicket> {
        self.dispatcher.stopped_ticket()
    }

    /// Caller has separately completed fresh nonconsuming core inspection.
    pub(crate) fn begin_final_read(&mut self, ticket: StoppedGoalTicket) -> Result<(), Error> {
        self.ingress.checkpoint().map_err(Error::Ingress)?;
        let mut unwind = UnwindClose(Some(self.close.clone()));
        let result = self
            .dispatcher
            .begin_final_read(ticket)
            .map_err(Error::Dispatch);
        if result.is_ok() {
            unwind.0 = None;
        }
        result
    }

    pub(crate) fn delivery(&self) -> Delivery {
        self.dispatcher.delivery()
    }

    pub(crate) fn cancel_resume(&mut self) {
        self.dispatcher.cancel_resume();
    }

    pub(crate) fn disconnect(&mut self) {
        self.ingress_close();
        self.dispatcher.disconnect();
    }

    fn ingress_close(&mut self) {
        self.close.close();
    }

    /// Process at most one complete admitted event. true means one event was
    /// interpreted; AuthorityDenied is a terminal automatic veto, not TUI loss.
    /// The callback is invoked only for a native activation candidate AFTER the
    /// controller interpreted its final reply and known backlog was excluded.
    /// It must obtain fresh core Consume, not reuse an earlier inspection hint.
    pub(crate) fn process_next(&mut self, authorize: impl FnOnce() -> bool) -> Result<bool, Error> {
        let mut unwind = UnwindClose(Some(self.close.clone()));
        let event = self.ingress.next_event().map_err(Error::Ingress)?;
        let Some(event) = event else {
            unwind.0 = None;
            return Ok(false);
        };
        let result = match event.source() {
            Source::Ui => {
                let result = self.dispatcher.ui_frame(event.frame().as_bytes());
                if result.is_ok() {
                    self.ingress.ack_processed(event).map_err(Error::Ingress)?;
                }
                result
            }
            Source::Native => {
                // One bounded native-frame copy permits moving its nonClone
                // receipt into ack inside the committed callback. The ingress
                // API deliberately exposes neither cloned events nor early ack.
                let bytes = event.frame().as_bytes().to_vec();
                let mut event = Some(event);
                let ingress = &mut self.ingress;
                let result =
                    self.dispatcher
                        .native_frame_committed(&bytes, |writer, activation| {
                            let Some(event) = event.take() else {
                                return Err(dispatcher::Error::AuthorityDenied);
                            };
                            ingress
                                .ack_processed(event)
                                .map_err(|_| dispatcher::Error::AuthorityDenied)?;
                            let checkpoint = ingress
                                .checkpoint()
                                .map_err(|_| dispatcher::Error::AuthorityDenied)?;
                            if !authorize() {
                                return Err(dispatcher::Error::AuthorityDenied);
                            }
                            match ingress.with_current(checkpoint, || writer.send(activation)) {
                                Ok(()) => Ok(()),
                                Err(CommitError::Ingress(_)) => {
                                    Err(dispatcher::Error::AuthorityDenied)
                                }
                                Err(CommitError::Write(_)) => {
                                    Err(dispatcher::Error::WriteUncertain)
                                }
                            }
                        });
                if result.is_ok()
                    && let Some(event) = event
                {
                    ingress.ack_processed(event).map_err(Error::Ingress)?;
                }
                result
            }
        };
        // Preserve the physical write's root cause. with_current already
        // revoked ingress after partial/error delivery; reporting that secondary
        // latch instead would hide the uncertain native effect. Keep unwind
        // armed so this return also closes ingress for ordinary write failures.
        if result == Err(dispatcher::Error::WriteUncertain) {
            self.dispatcher.disconnect();
            return Err(Error::Dispatch(dispatcher::Error::WriteUncertain));
        }
        // A revoked/poisoned ingress is transport loss, not a harmless local
        // activation veto. This also catches close/overflow during Consume.
        if let Err(error) = self.ingress.backlog() {
            self.dispatcher.disconnect();
            return Err(Error::Ingress(error));
        }
        match result {
            Ok(()) => {
                unwind.0 = None;
                Ok(true)
            }
            Err(dispatcher::Error::AuthorityDenied) => {
                unwind.0 = None;
                Err(Error::Dispatch(dispatcher::Error::AuthorityDenied))
            }
            Err(error) => {
                self.dispatcher.disconnect();
                Err(Error::Dispatch(error))
            }
        }
    }
}
