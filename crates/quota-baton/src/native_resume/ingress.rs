//! Bounded local input/write ordering, not native or core authority.
//!
//! One serialized parent owns Owner. Readers publish complete validated frames;
//! queue admission and the whole physical write use the same mutex. The parent
//! acknowledges ordinary events AFTER dispatching and completing their actions.
//! For a final-read response, the dispatcher's committed callback may acknowledge
//! it after the controller has processed it into a write candidate, then take an
//! empty-backlog checkpoint before Consume. with_current orders that candidate's
//! physical action. Never acknowledge merely on dequeue. A checked-out event
//! remains in the backlog until acknowledgment.
//! Core Consume must finish separately before with_current. No permit is minted
//! here, and unread kernel input/native internal changes cannot be arbitrated.
//!
//! The write closure MUST complete within a parent-enforced budget and perform
//! the whole frame, delimiter and flush directly, without another writer queue.
//! Do not call this module reentrantly from that closure. Teardown must be able
//! to terminate the owned jobs without this mutex, unblock I/O, then join workers.
//! Readers explicitly call close on EOF/error; dropping a publisher clone alone
//! does not mean EOF. Parent loss closes the sequencer. Nothing spawns tasks.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

pub(crate) const QUEUE_FRAMES: usize = 8;
pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    FrameBound,
    InvalidFrame,
    Revoked,
    QueueFull,
    Poisoned,
    EpochExhausted,
    Pending,
    WrongOwner,
    OutOfOrder,
    Stale,
}

/// Write errors remain typed for the parent; no payload/error formatting here.
pub(crate) enum CommitError<E> {
    Ingress(#[allow(dead_code)] Error),
    Write(E),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Source {
    Ui,
    Native,
}

/// Complete single-line JSON object payload, excluding the stdio LF delimiter.
/// Neither payload nor its owning event is Clone or Debug.
pub(crate) struct Frame(Box<[u8]>);

impl Frame {
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, Error> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(Error::FrameBound);
        }
        if bytes.is_empty() || bytes.iter().any(|b| matches!(*b, b'\r' | b'\n')) {
            return Err(Error::InvalidFrame);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| Error::InvalidFrame)?;
        if !value.is_object() {
            return Err(Error::InvalidFrame);
        }
        Ok(Self(bytes.into_boxed_slice()))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

pub(crate) struct Event {
    origin: Weak<Shared>,
    epoch: u64,
    source: Source,
    frame: Frame,
}

impl Event {
    pub(crate) fn source(&self) -> Source {
        self.source
    }

    pub(crate) fn frame(&self) -> &Frame {
        &self.frame
    }
}

/// Opaque, single-use ordering checkpoint; never core/native authorization.
pub(crate) struct Checkpoint {
    origin: Weak<Shared>,
    epoch: u64,
}

struct State {
    live: bool,
    epoch: u64,
    queue: VecDeque<Event>,
    in_flight: Option<u64>,
}

impl State {
    fn revoke(&mut self) {
        self.live = false;
        self.queue.clear();
        self.in_flight = None;
    }

    fn backlog(&self) -> usize {
        self.queue.len() + usize::from(self.in_flight.is_some())
    }

    fn advance(&mut self) -> Result<u64, Error> {
        let Some(epoch) = self.epoch.checked_add(1) else {
            self.revoke();
            return Err(Error::EpochExhausted);
        };
        self.epoch = epoch;
        Ok(epoch)
    }
}

struct Shared(Mutex<State>);

impl Shared {
    fn lock(&self) -> Result<MutexGuard<'_, State>, Error> {
        let state = match self.0.lock() {
            Ok(state) => state,
            Err(poison) => {
                poison.into_inner().revoke();
                return Err(Error::Poisoned);
            }
        };
        if !state.live {
            return Err(Error::Revoked);
        }
        Ok(state)
    }

    fn close(&self) {
        match self.0.lock() {
            Ok(mut state) => state.revoke(),
            Err(poison) => poison.into_inner().revoke(),
        }
    }
}

/// Clone shares ordering state only; no payload is cloned.
#[derive(Clone)]
pub(crate) struct Publisher(Arc<Shared>);

pub(crate) struct Owner(Arc<Shared>);

pub(crate) fn channel() -> (Owner, Publisher) {
    let shared = Arc::new(Shared(Mutex::new(State {
        live: true,
        epoch: 0,
        queue: VecDeque::with_capacity(QUEUE_FRAMES),
        in_flight: None,
    })));
    (Owner(shared.clone()), Publisher(shared))
}

impl Publisher {
    /// Admission is non-waiting with respect to capacity. Full means revoke,
    /// discard the backlog and fail closed, never wait/retry/drop just one frame.
    /// Mutex acquisition can wait for the parent's bounded whole-frame write.
    pub(crate) fn publish(&self, source: Source, frame: Frame) -> Result<(), Error> {
        let mut state = self.0.lock()?;
        if state.backlog() >= QUEUE_FRAMES {
            state.revoke();
            return Err(Error::QueueFull);
        }
        let epoch = state.advance()?;
        state.queue.push_back(Event {
            origin: Arc::downgrade(&self.0),
            epoch,
            source,
            frame,
        });
        Ok(())
    }

    /// Reader EOF/error, pump exit, control/job loss or malformed-frame refusal.
    /// Frame validation failure occurs before publish; the reader MUST close.
    pub(crate) fn close(&self) {
        self.0.close();
    }
}

impl Owner {
    pub(crate) fn backlog(&self) -> Result<usize, Error> {
        Ok(self.0.lock()?.backlog())
    }

    /// At most one checked-out event, and only its receipt can acknowledge it.
    /// Dropping that receipt does not acknowledge it or make a checkpoint safe.
    pub(crate) fn next_event(&mut self) -> Result<Option<Event>, Error> {
        let mut state = self.0.lock()?;
        if state.in_flight.is_some() {
            return Err(Error::Pending);
        }
        let event = state.queue.pop_front();
        state.in_flight = event.as_ref().map(|event| event.epoch);
        Ok(event)
    }

    pub(crate) fn ack_processed(&mut self, event: Event) -> Result<(), Error> {
        let mut state = self.0.lock()?;
        if !Weak::ptr_eq(&event.origin, &Arc::downgrade(&self.0)) {
            state.revoke();
            return Err(Error::WrongOwner);
        }
        if state.in_flight != Some(event.epoch) {
            state.revoke();
            return Err(Error::OutOfOrder);
        }
        state.in_flight = None;
        Ok(())
    }

    pub(crate) fn checkpoint(&mut self) -> Result<Checkpoint, Error> {
        let state = self.0.lock()?;
        if state.backlog() != 0 {
            return Err(Error::Pending);
        }
        Ok(Checkpoint {
            origin: Arc::downgrade(&self.0),
            epoch: state.epoch,
        })
    }

    /// Fresh local veto immediately around a direct whole-frame physical write.
    /// The parent has ALREADY obtained separate fresh core Consume authority.
    /// Returning an error from write revokes; a panic poisons and revokes on the
    /// next access. Neither outcome is permission to retry an automatic command.
    pub(crate) fn with_current<T, E>(
        &mut self,
        checkpoint: Checkpoint,
        write: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, CommitError<E>> {
        let mut state = self.0.lock().map_err(CommitError::Ingress)?;
        if !Weak::ptr_eq(&checkpoint.origin, &Arc::downgrade(&self.0)) {
            state.revoke();
            return Err(CommitError::Ingress(Error::WrongOwner));
        }
        if state.epoch != checkpoint.epoch || state.backlog() != 0 {
            return Err(CommitError::Ingress(Error::Stale));
        }
        // Invalidate other checkpoints issued at this epoch before any bytes.
        state.advance().map_err(CommitError::Ingress)?;
        match write() {
            Ok(value) => Ok(value),
            Err(error) => {
                state.revoke();
                Err(CommitError::Write(error))
            }
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_exhaustion_revokes_before_admission_or_write() {
        let (mut owner, publisher) = channel();
        owner.0.lock().unwrap().epoch = u64::MAX;
        assert!(matches!(
            publisher.publish(Source::Ui, Frame::new(b"{}".to_vec()).unwrap()),
            Err(Error::EpochExhausted)
        ));
        assert!(matches!(owner.checkpoint(), Err(Error::Revoked)));

        let (mut owner, publisher) = channel();
        owner.0.lock().unwrap().epoch = u64::MAX;
        let checkpoint = owner.checkpoint().unwrap();
        assert!(matches!(
            owner.with_current(checkpoint, || -> Result<(), ()> {
                panic!("exhausted epoch must not write")
            }),
            Err(CommitError::Ingress(Error::EpochExhausted))
        ));
        assert_eq!(
            publisher.publish(Source::Native, Frame::new(b"{}".to_vec()).unwrap()),
            Err(Error::Revoked)
        );
    }
}
