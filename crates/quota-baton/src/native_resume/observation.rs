//! Explicit harness observation only; never authority or user export.
//! Callers must use the same testkit+debug_assertions gate as this module.
//! No args, endpoints, credentials, objectives or arbitrary text enter events.
//! One successful capture per process. emit never waits for a mutex/channel or
//! performs I/O. Full/contended/error/bounded output latches loss. Drop removes
//! the sole sender and joins the writer without a queue sentinel or runtime.
//! Synchronous filesystem I/O has no wall-time guarantee; no worker is detached.
//! Summary is best effort after a write failure and included in the 64KiB cap.
//! Only consuming finish() reports final success, including flush and join. A
//! pre-finish lost() snapshot or summary loss:false is not a completion receipt.
#![cfg(all(feature = "testkit", debug_assertions))]

use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{
        Mutex, TryLockError,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};

use serde::{Serialize, Serializer};

pub(crate) const CHANNEL_CAPACITY: usize = 8;
pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const SUMMARY_RESERVE: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    InvalidThread,
    InvalidPath,
    AlreadyStarted,
    Poisoned,
    Open,
    Spawn,
    Loss,
    Join,
}

/// Canonical lowercase UUID hex: 32 digits or 36 bytes with standard hyphens.
/// Fixed storage; no arbitrary identifier text or secret-shaped string escapes.
pub(crate) struct ThreadId {
    bytes: [u8; 36],
    len: usize,
}

impl ThreadId {
    pub(crate) fn parse(value: &str) -> Result<Self, Error> {
        if !matches!(value.len(), 32 | 36) {
            return Err(Error::InvalidThread);
        }
        for (index, byte) in value.bytes().enumerate() {
            let hyphen = value.len() == 36 && matches!(index, 8 | 13 | 18 | 23);
            if (hyphen && byte != b'-')
                || !(hyphen || byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(Error::InvalidThread);
            }
        }
        let mut bytes = [0; 36];
        bytes[..value.len()].copy_from_slice(value.as_bytes());
        Ok(Self {
            bytes,
            len: value.len(),
        })
    }
}

impl Serialize for ThreadId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Construction above guarantees ASCII; no user input in this error.
        let text = std::str::from_utf8(&self.bytes[..self.len])
            .map_err(|_| serde::ser::Error::custom("observer-thread-invalid"))?;
        serializer.serialize_str(text)
    }
}

/// Fixed non-content records only. Neither Event nor ThreadId is Clone or Debug.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event {
    Entry,
    ChildrenSpawned,
    EndpointAccepted,
    ParentEntered,
    Cleanup,
    StoppedThread { thread_id: ThreadId },
    Observed { episode: u64 },
    Inspected { episode: u64, ready: bool },
    Consumed { episode: u64, granted: bool },
    ActivationWritten,
    ActivationAcknowledged,
}

struct Stats {
    flags: AtomicU8,
}

impl Stats {
    const fn new() -> Self {
        Self {
            flags: AtomicU8::new(0),
        }
    }
    fn lose(&self) {
        self.flags.fetch_or(2, Ordering::AcqRel);
    }
    fn lost(&self) -> bool {
        self.flags.load(Ordering::Acquire) & 2 != 0
    }
    fn active(&self) -> bool {
        self.flags.load(Ordering::Acquire) & 1 != 0
    }
    fn activate(&self) {
        self.flags.fetch_or(1, Ordering::AcqRel);
    }
    fn deactivate(&self) {
        self.flags.fetch_and(!1, Ordering::AcqRel);
    }
    fn lose_if_active(&self) {
        // Same atomic orders contention loss against closure. An emit that
        // loses this race is outside capture, not a late summary mutation.
        let _ = self
            .flags
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |flags| {
                (flags & 1 != 0).then_some(flags | 2)
            });
    }
}

struct Global {
    started: bool,
    sender: Option<SyncSender<Event>>,
}

static GLOBAL: Mutex<Global> = Mutex::new(Global {
    started: false,
    sender: None,
});
static STATS: Stats = Stats::new();

fn publish(sender: &SyncSender<Event>, event: Event, stats: &Stats) {
    if sender.try_send(event).is_err() {
        stats.lose();
    }
}

/// No-op before/after active capture. A contended active sink loses the event;
/// it never waits behind start/drop and never formats the event or an error.
pub(crate) fn emit(event: Event) {
    if !STATS.active() {
        return;
    }
    match GLOBAL.try_lock() {
        Ok(global) => {
            if let Some(sender) = &global.sender {
                publish(sender, event, &STATS);
            }
        }
        Err(TryLockError::WouldBlock) => STATS.lose_if_active(),
        Err(TryLockError::Poisoned(_)) => STATS.lose_if_active(),
    }
}

pub(crate) struct Capture {
    worker: Option<JoinHandle<bool>>,
}

impl Capture {
    /// Explicit absolute new .jsonl path; existing canonical parent must resolve
    /// inside THIS worktree's .sandbox. No parent creation, append, overwrite or
    /// default destination. Failed starts do not consume the one successful slot.
    pub(crate) fn start(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut global = GLOBAL.lock().map_err(|_| Error::Poisoned)?;
        if global.started {
            return Err(Error::AlreadyStarted);
        }
        let path = path.as_ref();
        if !path.is_absolute() || path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            return Err(Error::InvalidPath);
        }
        let sandbox = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(".sandbox")
            .canonicalize()
            .map_err(|_| Error::InvalidPath)?;
        let parent = path
            .parent()
            .ok_or(Error::InvalidPath)?
            .canonicalize()
            .map_err(|_| Error::InvalidPath)?;
        if !parent.starts_with(&sandbox) {
            return Err(Error::InvalidPath);
        }
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::InvalidPath)?;
        let stem = filename.strip_suffix(".jsonl").ok_or(Error::InvalidPath)?;
        if stem.is_empty()
            || filename.len() > 128
            || !stem
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(Error::InvalidPath);
        }
        // Exclude Windows device names and alternate-stream/path syntax even
        // when the test is built on another platform. Only a new ordinary file.
        let reserved = stem.to_ascii_lowercase();
        if matches!(reserved.as_str(), "con" | "prn" | "aux" | "nul")
            || (reserved.len() == 4
                && (reserved.starts_with("com") || reserved.starts_with("lpt"))
                && reserved.as_bytes()[3].is_ascii_digit())
        {
            return Err(Error::InvalidPath);
        }
        let destination = parent.join(filename);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|_| Error::Open)?;
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let worker = match thread::Builder::new()
            .name("qb-native-observer".into())
            .spawn(move || writer_loop(file, receiver, &STATS))
        {
            Ok(worker) => worker,
            Err(_) => {
                // Only the file created above belongs to this failed capture.
                let _ = std::fs::remove_file(destination);
                return Err(Error::Spawn);
            }
        };
        global.started = true;
        global.sender = Some(sender);
        STATS.activate();
        Ok(Self {
            worker: Some(worker),
        })
    }

    /// Live snapshot only; final flush/join may still fail. Use finish to accept.
    pub(crate) fn lost(&self) -> bool {
        STATS.lost()
    }

    /// Consume this capture, close its channel, join, then check final loss.
    /// Failure is terminal; it does not permit reopening this process's capture.
    pub(crate) fn finish(mut self) -> Result<(), Error> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), Error> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let sender = {
            let mut global = match GLOBAL.lock() {
                Ok(global) => global,
                Err(poison) => {
                    STATS.lose();
                    poison.into_inner()
                }
            };
            STATS.deactivate();
            global.sender.take()
        };
        // No sender clones escape; channel closure wakes even an idle writer.
        drop(sender);
        join_writer(worker, &STATS)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Same close/join cleanup, but only explicit finish returns a receipt.
        let _ = self.stop();
    }
}

fn join_writer(worker: JoinHandle<bool>, stats: &Stats) -> Result<(), Error> {
    match worker.join() {
        Err(_) => {
            stats.lose();
            return Err(Error::Join);
        }
        Ok(false) => stats.lose(),
        Ok(true) => {}
    }
    if stats.lost() {
        Err(Error::Loss)
    } else {
        Ok(())
    }
}

#[derive(Serialize)]
struct Summary {
    event: &'static str,
    loss: bool,
    received: u64,
    written: u64,
}

fn writer_loop(mut writer: impl Write, receiver: Receiver<Event>, stats: &Stats) -> bool {
    let mut received = 0u64;
    let mut written = 0u64;
    let mut bytes = 0usize;
    let mut failed = false;
    for event in receiver {
        received = match received.checked_add(1) {
            Some(count) => count,
            None => {
                stats.lose();
                u64::MAX
            }
        };
        let mut line = match serde_json::to_vec(&event) {
            Ok(line) => line,
            Err(_) => {
                stats.lose();
                continue;
            }
        };
        line.push(b'\n');
        if failed || bytes.saturating_add(line.len()) > MAX_OUTPUT_BYTES - SUMMARY_RESERVE {
            stats.lose();
            continue;
        }
        // Account the whole attempted line even after a partial write error.
        bytes += line.len();
        if writer.write_all(&line).is_err() {
            stats.lose();
            failed = true;
        } else {
            written += 1; // bounded by the output cap
        }
    }
    let summary = Summary {
        event: "summary",
        loss: stats.lost(),
        received,
        written,
    };
    match serde_json::to_vec(&summary) {
        Ok(mut line) => {
            line.push(b'\n');
            if line.len() > SUMMARY_RESERVE
                || bytes.saturating_add(line.len()) > MAX_OUTPUT_BYTES
                || writer.write_all(&line).is_err()
            {
                stats.lose();
            }
        }
        Err(_) => stats.lose(),
    }
    if writer.flush().is_err() {
        stats.lose();
    }
    !stats.lost()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn full_channel_marks_loss_without_waiting_and_summary_counts_received_only() {
        let stats = Stats::new();
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        for _ in 0..CHANNEL_CAPACITY {
            publish(&sender, Event::Entry, &stats);
        }
        publish(&sender, Event::Cleanup, &stats);
        assert!(stats.lost());
        drop(sender);
        let mut output = Vec::new();
        writer_loop(&mut output, receiver, &stats);
        let rows: Vec<serde_json::Value> = output
            .split(|b| *b == b'\n')
            .filter(|row| !row.is_empty())
            .map(|row| serde_json::from_slice(row).unwrap())
            .collect();
        assert_eq!(rows.len(), CHANNEL_CAPACITY + 1);
        assert_eq!(rows.last().unwrap()["loss"], true);
        assert_eq!(rows.last().unwrap()["received"], CHANNEL_CAPACITY);
    }

    #[test]
    fn byte_bound_reserves_final_loss_summary() {
        let stats = Stats::new();
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let worker = thread::spawn(move || {
            for _ in 0..5000 {
                if sender.send(Event::ActivationAcknowledged).is_err() {
                    break;
                }
            }
        });
        let mut output = Vec::new();
        writer_loop(&mut output, receiver, &stats);
        worker.join().unwrap();
        assert!(output.len() <= MAX_OUTPUT_BYTES);
        let last = output
            .split(|b| *b == b'\n')
            .rfind(|row| !row.is_empty())
            .unwrap();
        let summary: serde_json::Value = serde_json::from_slice(last).unwrap();
        assert_eq!(summary["event"], "summary");
        assert_eq!(summary["loss"], true);
        assert_eq!(summary["received"], 5000);
        assert!(summary["written"].as_u64().unwrap() < 5000);
    }

    #[test]
    fn writer_error_drains_closed_queue_and_latches_loss() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::Other.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::Other.into())
            }
        }
        let stats = Stats::new();
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        for _ in 0..CHANNEL_CAPACITY {
            publish(&sender, Event::Entry, &stats);
        }
        drop(sender);
        writer_loop(Broken, receiver, &stats);
        assert!(stats.lost());
    }

    #[test]
    fn flush_only_failure_rejects_final_receipt_even_with_loss_false_summary() {
        struct FlushFails(Vec<u8>);
        impl Write for FlushFails {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::Other.into())
            }
        }
        let stats = Stats::new();
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        publish(&sender, Event::Entry, &stats);
        drop(sender);
        let worker = thread::spawn(move || {
            let writer_stats = Stats::new();
            let mut output = FlushFails(Vec::new());
            let success = writer_loop(&mut output, receiver, &writer_stats);
            let last = output
                .0
                .split(|b| *b == b'\n')
                .rfind(|row| !row.is_empty())
                .unwrap();
            let summary: serde_json::Value = serde_json::from_slice(last).unwrap();
            assert_eq!(summary["event"], "summary");
            assert_eq!(summary["loss"], false);
            assert_eq!(summary["written"], 1);
            assert!(!success);
            success
        });
        assert!(!stats.lost());
        assert_eq!(join_writer(worker, &stats), Err(Error::Loss));
        assert!(stats.lost());
    }

    #[test]
    fn writer_join_failure_rejects_final_receipt_and_latches_loss() {
        let stats = Stats::new();
        let worker = thread::spawn(|| -> bool { panic!("synthetic observer failure") });
        assert_eq!(join_writer(worker, &stats), Err(Error::Join));
        assert!(stats.lost());
    }
}
