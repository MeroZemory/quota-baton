//! Sole-writer event adapter. Core reset permission is still a parent obligation.
//! Execute each call under the parent's event serialization; never dispatch a
//! later event until its returned write has completed or failed closed.

use std::io::Write;

use super::{
    framing::FrameWriter,
    goal::{Action, Controller, Delivery, NextEpisodeProof, StoppedGoalTicket},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Protocol,
    AuthorityDenied,
    WriteUncertain,
}

pub(crate) struct Dispatcher<N, U> {
    controller: Controller,
    native: FrameWriter<N>,
    ui: FrameWriter<U>,
}

impl<N: Write, U: Write> Dispatcher<N, U> {
    pub(crate) fn new(namespace: String, native: N, ui: U) -> Result<Self, Error> {
        Ok(Self {
            controller: Controller::new(namespace).map_err(|_| Error::Protocol)?,
            native: FrameWriter::new(native),
            ui: FrameWriter::new(ui),
        })
    }

    pub(crate) fn ui_frame(&mut self, frame: &[u8]) -> Result<(), Error> {
        let action = self.controller.ui_frame(frame);
        self.apply(action, || false)
    }

    #[cfg(test)]
    #[allow(dead_code)] // Isolated controller adapters; production uses committed.
    pub(crate) fn native_frame(&mut self, frame: &[u8]) -> Result<(), Error> {
        let action = self.controller.native_frame(frame);
        self.apply(action, || false)
    }

    /// Called only by the serialized parent. The callback consumes/revalidates
    /// core authority immediately before the automatic write, not before the
    /// asynchronous final-read exchange. It is never used for ordinary frames.
    /// Denial closes this episode without any native activation bytes or retry.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn native_frame_admitted(
        &mut self,
        frame: &[u8],
        authorize: impl FnOnce() -> bool,
    ) -> Result<(), Error> {
        let action = self.controller.native_frame(frame);
        self.apply(action, authorize)
    }

    /// Dedicated parent only: lets its ingress sequencer cover the COMPLETE
    /// physical activation frame, delimiter and flush. A bounded async Consume
    /// may run before acquiring that gate; the callback must then recheck its
    /// ingress checkpoint under the gate and write directly through this sole
    /// writer. Never enqueue automatic bytes for a later writer.
    pub(crate) fn native_frame_committed(
        &mut self,
        frame: &[u8],
        commit: impl FnOnce(&mut FrameWriter<N>, &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let action = self.controller.native_frame(frame);
        match action {
            Ok(Action::ResumeNative(frame)) => match commit(&mut self.native, &frame) {
                Ok(()) => Ok(()),
                Err(Error::AuthorityDenied) => {
                    self.controller.cancel_resume();
                    Err(Error::AuthorityDenied)
                }
                Err(error) => {
                    self.controller.disconnect();
                    Err(error)
                }
            },
            other => self.apply(other, || false),
        }
    }

    pub(crate) fn stopped_ticket(&mut self) -> Option<StoppedGoalTicket> {
        self.controller.stopped_ticket()
    }

    pub(crate) fn take_next_episode_proof(&mut self) -> Result<Option<NextEpisodeProof>, Error> {
        self.controller
            .take_next_episode_proof()
            .map_err(|_| Error::AuthorityDenied)
    }

    /// Replace only native ownership state; retain the existing physical writers.
    /// A failed rearm consumes/vetoes automatic work, not ordinary user transport.
    pub(crate) fn rearm(
        &mut self,
        proof: NextEpisodeProof,
        namespace: String,
    ) -> Result<(), Error> {
        self.controller
            .rearm(proof, namespace)
            .map_err(|_| Error::AuthorityDenied)
    }

    /// Parent must revalidate its distinct core authority immediately before
    /// this call. A native ownership ticket alone never permits automatic work.
    pub(crate) fn begin_final_read(&mut self, ticket: StoppedGoalTicket) -> Result<(), Error> {
        let action = self.controller.begin_final_read(ticket);
        if matches!(
            action,
            Err(super::goal::Error::TicketInvalid | super::goal::Error::AttemptConsumed)
        ) {
            // A real user edit or authority cancellation may invalidate the
            // ticket while Inspect is pending. This is a clean auto veto, not
            // malformed native protocol or loss of the ordinary TUI transport.
            self.controller.cancel_resume();
            return Err(Error::AuthorityDenied);
        }
        self.apply(action, || false)
    }

    pub(crate) fn delivery(&self) -> Delivery {
        self.controller.delivery()
    }

    /// A closed core control channel cancels automatic work, not ordinary TUI
    /// input/output. Actual native/UI transport loss still uses disconnect().
    pub(crate) fn cancel_resume(&mut self) {
        self.controller.cancel_resume();
    }

    /// Reader EOF/error and either child exit must invoke this before teardown.
    pub(crate) fn disconnect(&mut self) {
        self.controller.disconnect();
    }

    fn apply(
        &mut self,
        action: Result<Action, super::goal::Error>,
        authorize: impl FnOnce() -> bool,
    ) -> Result<(), Error> {
        let action = action.map_err(|_| {
            self.controller.disconnect();
            Error::Protocol
        })?;
        let result = match action {
            Action::ToNative(frame) => self.native.send(&frame),
            Action::ResumeNative(frame) => {
                if !authorize() {
                    // Revocation of automatic authority is not native/UI
                    // transport loss. Keep the ordinary user-owned session
                    // live, but never rearm this automatic attempt.
                    self.controller.cancel_resume();
                    return Err(Error::AuthorityDenied);
                }
                self.native.send(&frame)
            }
            Action::ToUi(frame) => self.ui.send(&frame),
            Action::Suppress => Ok(()),
        };
        result.map_err(|_| {
            // Includes downstream UI failure: no UI owner means no auto work.
            // Controller preserves UnknownNoRetry for an attempted command.
            self.controller.disconnect();
            Error::WriteUncertain
        })
    }
}
