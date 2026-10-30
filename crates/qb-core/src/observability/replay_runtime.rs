//! Startup-only metadata capture. Producers never perform filesystem I/O.
use super::replay_recorder::tape::ProducerFailureReason;
#[cfg(test)]
use super::replay_recorder::{Archive, BuildIdentity, Provenance};
use super::replay_recorder::{Event, Id, IdentityDomain, Owner, RecordResult, Recorder, Stamp};
use std::{
    io,
    path::Path,
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

pub(crate) struct Runtime {
    recorder: Recorder,
    tape: Option<super::replay_recorder::tape::Producer>,
    next_id: std::sync::atomic::AtomicU64,
    start: Instant,
    start_utc_ns: i64,
    cursors: Mutex<[u64; 8]>,
    stop: mpsc::Sender<()>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Runtime {
    #[cfg(test)]
    pub(crate) fn start(root: &Path, disk_limit_mib: u64) -> io::Result<Self> {
        let start = Instant::now();
        let start_utc_ns = chrono::Utc::now()
            .timestamp_nanos_opt()
            .ok_or_else(|| io::Error::other("routing_replay_clock_range"))?;
        let (recorder, archive) = Recorder::enable_with_limits(
            root,
            BuildIdentity::default(),
            Provenance::OperatorOptIn,
            stamp(start),
            super::replay_recorder::Limits::new(
                disk_limit_mib
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| io::Error::other("routing_replay_disk_limit"))?,
            )?,
        )?;
        let (stop, stopped) = mpsc::channel();
        let producer = recorder.clone();
        let worker = thread::Builder::new()
            .name("qb-routing-replay".into())
            .spawn(move || writer(archive, producer, start, stopped))?;
        Ok(Self {
            recorder,
            tape: None,
            next_id: std::sync::atomic::AtomicU64::new(1),
            start,
            start_utc_ns,
            cursors: Mutex::new([0; 8]),
            stop,
            worker: Mutex::new(Some(worker)),
        })
    }
    /// Main supplies the separate state/decision-tape-v8 root. Legacy roots are
    /// neither inspected nor removed, even when their disk cap was exhausted.
    pub(crate) fn start_tape(
        root: &Path,
        disk_limit_mib: u64,
        hourly_encoded_byte_limit: u64,
    ) -> io::Result<Self> {
        Self::start_tape_with_provenance(
            root,
            disk_limit_mib,
            hourly_encoded_byte_limit,
            super::replay_recorder::Provenance::OperatorOptIn,
        )
    }

    #[cfg(any(test, feature = "testkit"))]
    pub(crate) fn start_synthetic_tape(root: &Path, disk_limit_mib: u64) -> io::Result<Self> {
        Self::start_tape_with_provenance(
            root,
            disk_limit_mib,
            crate::config::DEFAULT_REPLAY_HOURLY_ENCODED_BYTES,
            super::replay_recorder::Provenance::Synthetic,
        )
    }

    fn start_tape_with_provenance(
        root: &Path,
        disk_limit_mib: u64,
        hourly_encoded_byte_limit: u64,
        provenance: super::replay_recorder::Provenance,
    ) -> io::Result<Self> {
        let start = Instant::now();
        let start_utc_ns = chrono::Utc::now()
            .timestamp_nanos_opt()
            .ok_or_else(|| io::Error::other("decision_tape_clock_range"))?;
        let limits = super::replay_recorder::Limits::new(
            disk_limit_mib
                .checked_mul(1024 * 1024)
                .ok_or_else(|| io::Error::other("decision_tape_disk_limit"))?,
        )?
        .with_hourly_encoded_byte_limit(hourly_encoded_byte_limit)?;
        let (tape, archive) = super::replay_recorder::tape::open_with_provenance(
            root,
            limits,
            Stamp {
                utc_ns: start_utc_ns,
                monotonic_ns: 0,
            },
            provenance,
        )?;
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("qb-decision-tape".into())
            .spawn(move || tape_writer(archive, start, stopped))?;
        Ok(Self {
            recorder: Recorder::default(),
            tape: Some(tape),
            next_id: std::sync::atomic::AtomicU64::new(1),
            start,
            start_utc_ns,
            cursors: Mutex::new([0; 8]),
            stop,
            worker: Mutex::new(Some(worker)),
        })
    }
    pub(crate) fn startup_witness_failed(&self, reason: crate::bindings::StartupWitnessFailure) {
        use crate::bindings::StartupWitnessFailure;
        self.failed(match reason {
            StartupWitnessFailure::ActiveState => ProducerFailureReason::StartupActiveState,
            StartupWitnessFailure::ImageSerialization => {
                ProducerFailureReason::StartupImageSerialization
            }
            StartupWitnessFailure::AccountsSerialization => {
                ProducerFailureReason::StartupAccountsSerialization
            }
            StartupWitnessFailure::Publication => ProducerFailureReason::StartupPublication,
        });
    }
    fn failed(&self, reason: ProducerFailureReason) {
        if let Some(tape) = &self.tape {
            tape.failed(reason);
        } else {
            self.recorder.producer_failed();
        }
    }
    pub(crate) fn next_id(&self) -> Option<u64> {
        self.next_id
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |n| n.checked_add(1),
            )
            .ok()
            .or_else(|| {
                self.failed(ProducerFailureReason::RuntimeIdExhausted);
                None
            })
    }
    pub(crate) fn identity(&self, domain: IdentityDomain, raw: &str) -> Option<Id> {
        if let Some(tape) = &self.tape {
            tape.identity(domain, raw.as_bytes())
        } else {
            self.recorder.identity(domain, raw.as_bytes())
        }
    }
    // UTC is the process-origin mapping; monotonic bounds retain exact collection order.
    pub(crate) fn stamp_at(&self, at: Instant) -> Option<Stamp> {
        let offset = at.checked_duration_since(self.start)?.as_nanos();
        Some(Stamp {
            utc_ns: self.start_utc_ns.checked_add(i64::try_from(offset).ok()?)?,
            monotonic_ns: u64::try_from(offset).ok()?,
        })
    }
    pub(crate) fn stamp(&self) -> Stamp {
        stamp(self.start)
    }
    pub(crate) fn record(&self, owner: Owner, cause: Option<u64>, event: Event) -> Option<u64> {
        // Serialize each producer cursor with its publication; a shared timestamp
        // sampled before this lock would spuriously report concurrent clock reversal.
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = &mut cursors[owner as usize];
        let Some(next) = cursor.checked_add(1) else {
            self.failed(ProducerFailureReason::MetadataCursorExhausted);
            return None;
        };
        *cursor = next;
        if let Some(tape) = &self.tape {
            return tape.metadata(self.stamp(), owner, next, cause, event);
        }
        match self
            .recorder
            .record(self.stamp(), owner, next, cause, event)
        {
            RecordResult::Queued(sequence) => Some(sequence),
            _ => None,
        }
    }
    /// Serializes only the native allowlisted DTO; writer assigns its ordinal.
    pub(crate) fn record_decision(
        &self,
        record: &crate::bindings::decision_tape::DecisionRecord,
    ) -> Option<u64> {
        let tape = self.tape.as_ref()?;
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = &mut cursors[Owner::Bindings as usize];
        let Some(next) = cursor.checked_add(1) else {
            tape.failed(ProducerFailureReason::DecisionCursorExhausted);
            return None;
        };
        *cursor = next;
        tape.decision(self.stamp(), next, record)
    }

    pub(crate) fn record_selection(
        &self,
        entry: crate::bindings::decision_tape::Entry,
        capture_id: u64,
        invocation: u64,
        inputs: &crate::bindings::kernel::DecisionInputs,
        actual: &crate::bindings::kernel::selection::SelectionDecision,
    ) -> Option<u64> {
        let tape = self.tape.as_ref()?;
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = &mut cursors[Owner::Bindings as usize];
        let Some(next) = cursor.checked_add(1) else {
            tape.failed(ProducerFailureReason::SelectionCursorExhausted);
            return None;
        };
        *cursor = next;
        tape.selection(
            self.stamp(),
            next,
            entry,
            capture_id,
            invocation,
            inputs,
            actual,
        )
    }

    pub(crate) fn record_permit(
        &self,
        inputs: &crate::bindings::kernel::DecisionInputs,
        actual: &crate::bindings::kernel::PermitDecision,
    ) -> Option<u64> {
        let tape = self.tape.as_ref()?;
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = &mut cursors[Owner::Bindings as usize];
        let Some(next) = cursor.checked_add(1) else {
            tape.failed(ProducerFailureReason::PermitCursorExhausted);
            return None;
        };
        *cursor = next;
        tape.permit(self.stamp(), next, inputs, actual)
    }

    pub(crate) fn record_permit_commit(&self, proposal: Option<u64>) -> Option<u64> {
        let tape = self.tape.as_ref()?;
        let Some(proposal) = proposal else {
            tape.failed(ProducerFailureReason::CommitWithoutProposal);
            return None;
        };
        let mut cursors = self.cursors.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = &mut cursors[Owner::Bindings as usize];
        let Some(next) = cursor.checked_add(1) else {
            tape.failed(ProducerFailureReason::CommitCursorExhausted);
            return None;
        };
        *cursor = next;
        tape.permit_commit(self.stamp(), next, proposal)
    }
    pub(crate) fn disable(&self) {
        self.recorder.disable();
        if let Some(tape) = &self.tape {
            tape.disable();
        }
    }
    /// Called on a blocking thread before lease handover. Existing producer Arcs
    /// may survive drain, but cannot keep the archive/root lock alive afterward.
    pub(crate) fn shutdown(&self) {
        self.disable();
        // Retain this control lock through join so concurrent shutdown callers
        // cannot report completion while another caller still owns the writer.
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.stop.send(());
        if let Some(handle) = worker.take()
            && handle.join().is_err()
        {
            tracing::warn!(
                code = "routing_replay_join_failed",
                "Routing replay writer stopped unexpectedly"
            );
        }
    }
}
fn tape_writer(
    mut archive: super::replay_recorder::tape::Writer,
    start: Instant,
    stopped: mpsc::Receiver<()>,
) {
    loop {
        match stopped.recv_timeout(Duration::from_millis(250)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if archive.flush(stamp(start)).is_err() {
            tracing::warn!(
                code = "decision_tape_write_failed",
                "Decision tape capture stopped after storage failure"
            );
            break;
        }
    }
    if archive.close(stamp(start)).is_err() {
        tracing::warn!(
            code = "decision_tape_close_failed",
            "Decision tape was not cleanly sealed"
        );
    }
}
fn stamp(start: Instant) -> Stamp {
    Stamp {
        utc_ns: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX),
        monotonic_ns: u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX),
    }
}
#[cfg(test)]
fn writer(mut archive: Archive, producer: Recorder, start: Instant, stopped: mpsc::Receiver<()>) {
    let mut faulted = false;
    loop {
        match stopped.recv_timeout(Duration::from_millis(250)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if faulted {
            let _ = archive.maintain(stamp(start));
        } else if archive.flush(stamp(start)).is_err() {
            producer.disable();
            faulted = true;
            tracing::warn!(
                code = "routing_replay_write_failed",
                "Routing replay capture disabled after a storage failure"
            );
        }
    }
    producer.disable();
    if archive.close(stamp(start)).is_err() {
        tracing::warn!(
            code = "routing_replay_close_failed",
            "Routing replay capture was not cleanly sealed"
        );
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.disable();
        let _ = self.stop.send(());
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(worker) = worker {
            // Explicit shutdown joins before handover; a final request's fallback
            // Drop must not block a Tokio worker on archive I/O.
            if tokio::runtime::Handle::try_current().is_ok() {
                let _ = thread::Builder::new()
                    .name("qb-routing-replay-stop".into())
                    .spawn(move || {
                        let _ = worker.join();
                    });
            } else {
                let _ = worker.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::replay_recorder::{Event, Origin, Owner, Provider};
    use std::sync::Arc;

    fn directory() -> tempfile::TempDir {
        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let root = executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r142-runtime-fixtures");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }
    fn event(runtime: &Runtime) -> Event {
        Event::Account {
            account: runtime
                .identity(IdentityDomain::Account, "private-account-canary")
                .unwrap(),
            provider: Provider::Codex,
            order_rank: 0,
            revision: 1,
            eligible: Some(true),
            credential_available: true,
            drain: false,
            origin: Origin::Operator,
        }
    }
    fn manifest(root: &Path, epoch: &str) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(root.join(epoch).join("manifest.json")).unwrap())
            .unwrap()
    }
    #[test]
    fn tape_runtime_installs_the_configured_hourly_budget_before_capture() {
        let dir = directory();
        let root = dir.path().join("configured-tape");
        let runtime = Runtime::start_tape(&root, 16, 12_345).unwrap();
        runtime.shutdown();
        assert_eq!(manifest(&root, "e00")["hourly_encoded_byte_limit"], 12_345);
        assert_eq!(manifest(&root, "e00")["sealed"], true);
        let invalid = dir.path().join("invalid-tape");
        assert!(Runtime::start_tape(&invalid, 16, 0).is_err());
        assert!(!invalid.exists());
    }
    #[test]
    fn r142_replay_runtime_drains_on_close_and_off_rejects_new_metadata() {
        let dir = directory();
        let root = dir.path().join("capture");
        let runtime = Runtime::start(&root, 16).unwrap();
        assert!(
            runtime
                .record(Owner::Runtime, None, event(&runtime))
                .is_some()
        );
        runtime.disable();
        assert!(
            runtime
                .record(Owner::Runtime, None, event(&runtime))
                .is_none()
        );
        drop(runtime);
        let manifest = manifest(&root, "e00");
        assert_eq!(manifest["sealed"], true);
        assert_eq!(manifest["initial_state_reconciled"], false);
        assert_eq!(manifest["loss"]["producer_failure"], 0);
        let mut text = String::new();
        for entry in std::fs::read_dir(root.join("e00")).unwrap() {
            let p = entry.unwrap().path();
            if p.is_file() {
                text.push_str(&std::fs::read_to_string(p).unwrap());
            }
        }
        assert!(!text.contains("private-account-canary"));
        assert!(text.contains("\"kind\":\"Account\""));
    }
    #[test]
    fn r142_replay_runtime_concurrent_producers_keep_cursor_order_without_false_gaps() {
        let dir = directory();
        let root = dir.path().join("capture");
        let runtime = Arc::new(Runtime::start(&root, 16).unwrap());
        let jobs: Vec<_> = (0..4)
            .map(|_| {
                let runtime = runtime.clone();
                thread::spawn(move || {
                    for _ in 0..20 {
                        assert!(
                            runtime
                                .record(Owner::Runtime, None, event(&runtime))
                                .is_some()
                        );
                    }
                })
            })
            .collect();
        for job in jobs {
            job.join().unwrap();
        }
        runtime.shutdown();
        let manifest = manifest(&root, "e00");
        assert_eq!(manifest["loss"]["owner_gap"], 0);
        assert_eq!(manifest["loss"]["clock_regressed"], 0);
        assert_eq!(manifest["sealed"], true);
    }
    #[test]
    fn r142_replay_runtime_shutdown_releases_root_with_retained_producers() {
        let dir = directory();
        let root = dir.path().join("capture");
        let runtime = Arc::new(Runtime::start(&root, 16).unwrap());
        let retained = runtime.clone();
        let retained_recorder = runtime.recorder.clone();
        assert!(
            runtime
                .record(Owner::Runtime, None, event(&runtime))
                .is_some()
        );

        runtime.shutdown();
        let first = manifest(&root, "e00");
        assert_eq!(first["sealed"], true);
        assert_eq!(first["loss"]["producer_failure"], 0);
        for flag in [
            "initial_state_reconciled",
            "owner_hooks_complete",
            "replay_ready",
            "actual_trace_accepted",
        ] {
            assert_eq!(first[flag], false);
        }
        assert!(
            retained
                .record(Owner::Runtime, None, event(&retained))
                .is_none()
        );
        assert_eq!(
            retained_recorder.record(retained.stamp(), Owner::Runtime, 2, None, event(&retained)),
            RecordResult::Disabled
        );

        let successor =
            Runtime::start(&root, 16).expect("joined shutdown must release the root lock");
        retained.shutdown(); // An old idempotent shutdown must not stop its successor.
        assert!(
            successor
                .record(Owner::Runtime, None, event(&successor))
                .is_some()
        );
        successor.shutdown();
        assert_eq!(manifest(&root, "e01")["sealed"], true);
        assert!(Arc::strong_count(&retained) >= 2);
        assert!(!retained_recorder.status().unwrap().enabled);
    }
    #[test]
    fn r142_replay_runtime_counter_exhaustion_is_a_sticky_producer_failure() {
        for exhaust_ids in [true, false] {
            let dir = directory();
            let root = dir.path().join("capture");
            let runtime = Runtime::start(&root, 16).unwrap();
            if exhaust_ids {
                runtime
                    .next_id
                    .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
                assert!(runtime.next_id().is_none());
            } else {
                runtime.cursors.lock().unwrap()[Owner::Runtime as usize] = u64::MAX;
                assert!(
                    runtime
                        .record(Owner::Runtime, None, event(&runtime))
                        .is_none()
                );
            }
            let status = runtime.recorder.status().unwrap();
            assert!(!status.enabled);
            assert_eq!(status.loss.producer_failure, 1);
            runtime.shutdown();
            assert_eq!(manifest(&root, "e00")["loss"]["producer_failure"], 1);
        }
    }

    #[test]
    fn schema6_runtime_callsite_reasons_count_once() {
        use crate::bindings::decision_tape::{DecisionRecord, Entry, Inputs, RebindMarker};
        use crate::bindings::{
            StartupWitnessFailure,
            kernel::{DecisionInputs, PermitDecision},
        };
        use crate::observability::decision_comparison::{Outcome, OutcomeKind};
        use ProducerFailureReason::*;
        let dir = directory();
        let root = dir.path().join("tape");
        let runtime = Runtime::start_synthetic_tape(&root, 16).unwrap();
        let now = chrono::Utc::now();
        let inputs = DecisionInputs {
            selection: None,
            now,
            model: None,
            key: None,
            from: None,
            to: "target".into(),
            accounts: Default::default(),
            traffic: Default::default(),
            pending: Default::default(),
            groups: Default::default(),
            bindings: Default::default(),
            accepted_unbound: Default::default(),
            sticky: Default::default(),
            outside_failure: false,
            permit: None,
        };
        let decision = DecisionRecord {
            schema: 1,
            ordinal: 0,
            capture_id: None,
            inputs: Inputs {
                entry: Entry::Chooser,
                at: now,
                provider: crate::accounts::Provider::Codex,
                key: None,
                parent: None,
                model: None,
                pinned: None,
                request_bytes: 0,
                hold_allowed: false,
                hold_timed_out: false,
                coverage: None,
                root_hint_present: false,
                launch_metadata_valid: None,
                launch: None,
            },
            state: None,
            answers: Vec::new(),
            rebind: RebindMarker::default(),
            outcome: Outcome {
                kind: OutcomeKind::None,
                account: None,
                until: None,
                refusal: None,
                moves: 0,
            },
            capture_aborted: false,
            read_state_complete: false,
            selection_ordinals: Vec::new(),
            selection_refusals: 0,
        };
        runtime
            .next_id
            .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        assert!(runtime.next_id().is_none());
        runtime.cursors.lock().unwrap()[Owner::Runtime as usize] = u64::MAX;
        assert!(
            runtime
                .record(Owner::Runtime, None, event(&runtime))
                .is_none()
        );
        runtime.cursors.lock().unwrap()[Owner::Bindings as usize] = u64::MAX;
        assert!(runtime.record_decision(&decision).is_none());
        assert!(
            runtime
                .record_permit(&inputs, &PermitDecision::NotRequired)
                .is_none()
        );
        assert!(runtime.record_permit_commit(None).is_none());
        assert!(runtime.record_permit_commit(Some(1)).is_none());
        for reason in [
            StartupWitnessFailure::ActiveState,
            StartupWitnessFailure::ImageSerialization,
            StartupWitnessFailure::AccountsSerialization,
            StartupWitnessFailure::Publication,
        ] {
            runtime.startup_witness_failed(reason);
        }
        runtime.disable();
        runtime.failed(Unspecified);
        runtime.shutdown();
        let m: super::super::replay_recorder::tape::Manifest =
            serde_json::from_value(manifest(&root, "e00")).unwrap();
        let reasons = [
            Unspecified,
            RuntimeIdExhausted,
            MetadataCursorExhausted,
            DecisionCursorExhausted,
            PermitCursorExhausted,
            CommitCursorExhausted,
            CommitWithoutProposal,
            StartupActiveState,
            StartupImageSerialization,
            StartupAccountsSerialization,
            StartupPublication,
        ];
        assert_eq!(m.losses.producer_failure, reasons.len() as u64);
        assert_eq!(m.producer_failure_reasons.len(), reasons.len());
        for reason in reasons {
            assert_eq!(m.producer_failure_reasons[&reason], 1);
        }
        let epoch = crate::observability::decision_reader::load(&root.join("e00")).unwrap();
        assert_eq!(epoch.losses, reasons.len() as u64);
        assert!(!epoch.complete);
        assert!(!epoch.permit_complete);
    }
}
