//! Sole-writer event adapter. Core reset permission is still a parent obligation.
//! Execute each call under the parent's event serialization; never dispatch a
//! later event until its returned write has completed or failed closed.

use std::io::Write;

use super::{
    framing::FrameWriter,
    goal::{Action, Controller, Delivery, StoppedGoalTicket},
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

    pub(crate) fn native_frame(&mut self, frame: &[u8]) -> Result<(), Error> {
        let action = self.controller.native_frame(frame);
        self.apply(action, || false)
    }

    /// Called only by the serialized parent. The callback consumes/revalidates
    /// core authority immediately before the automatic write, not before the
    /// asynchronous final-read exchange. It is never used for ordinary frames.
    /// Denial closes this episode without any native activation bytes or retry.
    pub(crate) fn native_frame_admitted(
        &mut self,
        frame: &[u8],
        authorize: impl FnOnce() -> bool,
    ) -> Result<(), Error> {
        let action = self.controller.native_frame(frame);
        self.apply(action, authorize)
    }

    pub(crate) fn stopped_ticket(&mut self) -> Option<StoppedGoalTicket> {
        self.controller.stopped_ticket()
    }

    /// Parent must revalidate its distinct core authority immediately before
    /// this call. A native ownership ticket alone never permits automatic work.
    pub(crate) fn begin_final_read(&mut self, ticket: StoppedGoalTicket) -> Result<(), Error> {
        let action = self.controller.begin_final_read(ticket);
        self.apply(action, || false)
    }

    pub(crate) fn delivery(&self) -> Delivery {
        self.controller.delivery()
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
                    self.controller.disconnect();
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
