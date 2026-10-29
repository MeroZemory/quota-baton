//! Partial decision-local observations. These values cannot mint authority.
//! Schema 1 is capture-only: its incomplete router input image MUST be counted
//! insufficient. In particular, computed predicate bits are not replay inputs.
use super::*;
use crate::observability::decision_comparison::{Outcome, OutcomeKind};
use sha2::{Digest, Sha256};
use std::cell::RefCell;

pub(crate) type SelectionRecorder = Arc<
    dyn Fn(
            Entry,
            u64,
            u64,
            &kernel::DecisionInputs,
            &kernel::selection::SelectionDecision,
        ) -> Option<u64>
        + Send
        + Sync,
>;

pub type Recorder = Arc<dyn Fn(&DecisionRecord) -> Option<u64> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Entry {
    Chooser,
    Launch,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inputs {
    pub entry: Entry,
    pub at: DateTime<Utc>,
    pub provider: Provider,
    pub key: Option<String>,
    pub parent: Option<String>,
    pub model: Option<String>,
    pub pinned: Option<String>,
    pub request_bytes: u64,
    pub hold_allowed: bool,
    pub hold_timed_out: bool,
    pub coverage: Option<Vec<String>>,
    pub root_hint_present: bool,
    pub launch_metadata_valid: Option<bool>,
    pub launch: Option<LaunchInputs>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchInputs {
    pub resume: Option<String>,
    pub model_is_explicit: bool,
    pub account_preference: Option<String>,
    pub allow_move_for_model: bool,
    pub operation_present: bool,
    pub admission_present: bool,
    pub entitlements: Vec<(String, Vec<String>)>,
}

/// Predicate answers, in native evaluation order (including short circuiting).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Admission,
    Selection,
    AdmissionSource,
    Pin,
    Store,
    Failure,
    RebindSource,
    RebindGuard,
    RebindEvidence,
    RebindPermit,
    Authorize,
    LaunchAdmission,
    Operation,
    DispatchAllowed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub check: Check,
    pub value: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebindMarker {
    pub entered: u64,
    pub completed: u64,
    pub replacements: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateRead {
    pub account: String,
    /// Native ID ordering, not hash ordering; no raw identifiers on wire.
    pub order: usize,
    pub provider_matches: bool,
    pub eligible: bool,
    pub drain: bool,
    pub outside: bool,
    pub gate: bool,
    pub unknown: bool,
    pub weekly: Option<DateTime<Utc>>,
    pub five: Option<DateTime<Utc>>,
    /// None means the native positive-infinity unknown usage sentinel.
    pub usage: Option<f64>,
    pub active: usize,
    pub reserved: usize,
    pub threshold: usize,
    pub pacing: bool,
    pub capped: bool,
    pub placed: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadState {
    pub candidates: Vec<CandidateRead>,
    pub bound: Option<String>,
    pub hold_until: Option<DateTime<Utc>>,
    pub outside_unknown: bool,
    pub capacity_available: bool,
    /// An explicit fail-closed domain declaration, never a default native fact.
    pub unsupported: Vec<Unsupported>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unsupported {
    InternalRouterInputs,
    LaunchState,
    PolicyState,
    Parent,
    RootHint,
    Coverage,
    Rebalance,
    SourceMove,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecord {
    pub schema: u16,
    /// Writer sets the archive ordinal returned by the producer callback.
    pub ordinal: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_id: Option<u64>,
    pub inputs: Inputs,
    pub state: Option<ReadState>,
    pub answers: Vec<Answer>,
    pub rebind: RebindMarker,
    pub outcome: Outcome,
    pub capture_aborted: bool,
    pub read_state_complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selection_ordinals: Vec<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub selection_refusals: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplayRefusal {
    #[error("unsupported decision schema")]
    Schema,
    #[error("decision read state is not represented")]
    Unsupported,
    #[error("malformed decision observation")]
    Malformed,
    #[error("missing or inconsistent admission transcript")]
    Transcript,
    #[error("incomplete rebind coverage")]
    RebindCoverage,
}

pub(super) fn compact(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..32].to_owned()
}
fn outcome(kind: OutcomeKind, account: Option<String>, until: Option<DateTime<Utc>>) -> Outcome {
    Outcome {
        kind,
        account,
        until,
        refusal: None,
        moves: 0,
    }
}

/// This producer currently records only a partial router read projection.
/// Refuse it explicitly; evaluating frozen routing predicates is not native replay.
/// The expected outcome is never used as an evaluation input.
pub fn replay_record(record: &DecisionRecord) -> Result<Outcome, ReplayRefusal> {
    if record.schema != 1 {
        return Err(ReplayRefusal::Schema);
    }
    Err(ReplayRefusal::Unsupported)
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}

struct Trace {
    id: u64,
    table: usize,
    inputs: Inputs,
    state: Option<ReadState>,
    answers: Vec<Answer>,
    rebind: RebindMarker,
    selection_ordinals: Vec<u64>,
    selection_refusals: u64,
}
thread_local! { static ACTIVE: RefCell<Vec<Trace>> = const { RefCell::new(Vec::new()) }; }
static NEXT_CAPTURE: AtomicU64 = AtomicU64::new(1);
pub(super) struct Capture<'a> {
    recorder: Option<Recorder>,
    inputs: Inputs,
    table: usize,
    id: u64,
    losses: &'a AtomicU64,
}
impl<'a> Capture<'a> {
    pub(super) fn begin(table: &'a BindingTable, inputs: impl FnOnce() -> Inputs) -> Option<Self> {
        let recorder = table
            .decision_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        let inputs = inputs();
        let id = NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed);
        ACTIVE.with_borrow_mut(|stack| {
            stack.push(Trace {
                id,
                table: table as *const _ as usize,
                inputs: inputs.clone(),
                state: None,
                answers: Vec::new(),
                rebind: RebindMarker::default(),
                selection_ordinals: Vec::new(),
                selection_refusals: 0,
            })
        });
        Some(Self {
            recorder: Some(recorder),
            inputs,
            table: table as *const _ as usize,
            id,
            losses: &table.decision_capture_losses,
        })
    }
    pub(super) fn finish(mut self, result: Outcome) {
        let trace = self.take_trace();
        let Some(trace) = trace else {
            return;
        };
        let record = DecisionRecord {
            schema: 1,
            ordinal: self.id,
            capture_id: Some(self.id),
            inputs: self.inputs.clone(),
            state: trace.state,
            answers: trace.answers,
            rebind: trace.rebind,
            outcome: result,
            capture_aborted: false,
            read_state_complete: false,
            selection_ordinals: trace.selection_ordinals,
            selection_refusals: trace.selection_refusals,
        };
        self.publish(&record);
    }
    fn take_trace(&self) -> Option<Trace> {
        ACTIVE
            .try_with(|active| {
                active.try_borrow_mut().ok().and_then(|mut stack| {
                    let index = stack
                        .iter()
                        .rposition(|t| t.id == self.id && t.table == self.table)?;
                    Some(stack.remove(index))
                })
            })
            .ok()
            .flatten()
    }
    fn publish(&mut self, record: &DecisionRecord) {
        if let Some(recorder) = self.recorder.take()
            && !matches!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| recorder(record))),
                Ok(Some(_))
            )
        {
            self.losses.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl Drop for Capture<'_> {
    fn drop(&mut self) {
        if self.recorder.is_some() {
            self.losses.fetch_add(1, Ordering::Relaxed);
            let trace = self.take_trace();
            let mut aborted = outcome(OutcomeKind::Refusal, None, None);
            aborted.refusal = Some("capture_aborted".to_owned());
            let record = DecisionRecord {
                schema: 1,
                ordinal: self.id,
                capture_id: Some(self.id),
                inputs: self.inputs.clone(),
                state: trace.as_ref().and_then(|t| t.state.clone()),
                answers: trace.as_ref().map_or_else(Vec::new, |t| t.answers.clone()),
                rebind: trace
                    .as_ref()
                    .map_or_else(RebindMarker::default, |t| t.rebind.clone()),
                outcome: aborted,
                capture_aborted: true,
                read_state_complete: false,
                selection_ordinals: trace
                    .as_ref()
                    .map_or_else(Vec::new, |t| t.selection_ordinals.clone()),
                selection_refusals: trace.as_ref().map_or(0, |t| t.selection_refusals),
            };
            self.publish(&record);
        }
    }
}
fn trace(table: &BindingTable, f: impl FnOnce(&mut Trace)) {
    ACTIVE.with_borrow_mut(|stack| {
        if let Some(trace) = stack
            .last_mut()
            .filter(|t| t.table == table as *const _ as usize)
        {
            f(trace);
        }
    });
}
/// Only an actual synchronous chooser/launch capture may publish a selection.
pub(super) fn observe_selection(
    table: &BindingTable,
    inputs: &kernel::DecisionInputs,
    actual: &kernel::selection::SelectionDecision,
) {
    let context = ACTIVE.with_borrow(|stack| {
        stack
            .last()
            .filter(|t| t.table == table as *const _ as usize)
            .map(|t| {
                (
                    t.id,
                    t.inputs.entry,
                    t.selection_ordinals.len() as u64 + t.selection_refusals + 1,
                )
            })
    });
    let Some((capture_id, entry, invocation)) = context else {
        return;
    };
    let recorder = table
        .selection_recorder
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(recorder) = recorder else { return };
    let ordinal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        recorder(entry, capture_id, invocation, inputs, actual)
    }))
    .ok()
    .flatten();
    ACTIVE.with_borrow_mut(|stack| {
        if let Some(trace) = stack
            .last_mut()
            .filter(|t| t.id == capture_id && t.table == table as *const _ as usize)
        {
            if let Some(ordinal) = ordinal {
                trace.selection_ordinals.push(ordinal);
            } else {
                trace.selection_refusals += 1;
                table
                    .decision_capture_losses
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    });
}
pub(super) fn selection_refused(table: &BindingTable) {
    trace(table, |t| t.selection_refusals += 1);
    table
        .decision_capture_losses
        .fetch_add(1, Ordering::Relaxed);
}
pub(super) fn check(table: &BindingTable, check: Check, value: bool) -> bool {
    trace(table, |t| t.answers.push(Answer { check, value }));
    value
}
pub(super) fn rebind_enter(table: &BindingTable) {
    trace(table, |t| t.rebind.entered += 1);
}
pub(super) fn rebind_complete(table: &BindingTable, moves: &[MoveEvent]) {
    trace(table, |t| {
        t.rebind.completed += 1;
        t.rebind.replacements += replacements(moves);
    });
}
fn replacements(moves: &[MoveEvent]) -> u64 {
    moves
        .iter()
        .filter(|m| m.from.as_ref().is_some_and(|from| from != &m.to))
        .count() as u64
}
pub(super) fn decision_outcome(decision: &Decision) -> Outcome {
    match decision {
        Decision::None => outcome(OutcomeKind::None, None, None),
        Decision::Blocked => outcome(OutcomeKind::Blocked, None, None),
        Decision::Hold { until } => outcome(OutcomeKind::Hold, None, Some(*until)),
        Decision::MoveHeld { until } => outcome(OutcomeKind::MoveHeld, None, Some(*until)),
        Decision::Selected { account, moves, .. } => {
            let mut o = outcome(OutcomeKind::Dispatch, Some(compact(account)), None);
            o.moves = replacements(moves);
            o
        }
    }
}

impl BindingTable {
    pub fn set_decision_recorder(&self, recorder: Recorder) {
        *self
            .decision_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(recorder);
    }
    pub fn decision_tape_losses(&self) -> u64 {
        self.decision_capture_losses.load(Ordering::Acquire)
    }
}

/// Called under the live mutation/data locks, immediately before selection.
#[allow(clippy::too_many_arguments)]
pub(super) fn observe_choice(
    table: &BindingTable,
    d: &Data,
    provider: Provider,
    key: Option<&str>,
    model: Option<&str>,
    candidates: &[Candidate],
    now: DateTime<Utc>,
) {
    trace(table, |t| {
        let inputs = &t.inputs;
        let plan = table.policies.plan(launch::policy_key(d, key));
        let mut unsupported = vec![Unsupported::InternalRouterInputs];
        if inputs.entry == Entry::Launch {
            unsupported.push(Unsupported::LaunchState);
        }
        if plan.anchor.is_some() || plan.project.is_some() {
            unsupported.push(Unsupported::PolicyState);
        }
        if inputs.parent.is_some() {
            unsupported.push(Unsupported::Parent);
        }
        if inputs.root_hint_present {
            unsupported.push(Unsupported::RootHint);
        }
        if inputs.coverage.is_some() {
            unsupported.push(Unsupported::Coverage);
        }
        let bound = key
            .and_then(|k| {
                table
                    .retained_binding(d, k, now)
                    .map(|b| b.account.as_str())
            })
            .or_else(|| {
                key.is_none()
                    .then(|| d.sticky.get(provider.as_str()).map(String::as_str))
                    .flatten()
            });
        if bound.is_some() && plan.settings.mode == crate::routing::Mode::Rebalance {
            unsupported.push(Unsupported::Rebalance);
        }
        if bound.is_none() && table.rebind_source(d, provider, key).is_some() {
            unsupported.push(Unsupported::SourceMove);
        }
        let outside_unknown = d.outside_unknown
            || d.outside_failure
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire));
        let mut order: Vec<_> = candidates.iter().map(|c| c.account.id.as_str()).collect();
        order.sort_unstable();
        let reads = candidates
            .iter()
            .map(|c| {
                let m = metrics(c, d, &table.config, now);
                CandidateRead {
                    account: compact(&c.account.id),
                    order: order
                        .binary_search(&c.account.id.as_str())
                        .expect("candidate ID present"),
                    provider_matches: c.account.provider == provider,
                    eligible: effective_account(c, d, &table.config, model, now)
                        .eligible_at(model, now)
                        && (!c.account.drain || BindingTable::accepted_in(d, key, &c.account.id))
                        && bound.is_none_or(|source| {
                            source == c.account.id
                                || (!outside_unknown && !d.outside_used.contains(&c.account.id))
                        }),
                    drain: c.account.drain,
                    outside: d.outside_used.contains(&c.account.id),
                    gate: m.gate,
                    unknown: m.unknown,
                    weekly: m.weekly,
                    five: m.five,
                    usage: m.usage.is_finite().then_some(m.usage),
                    active: table.active_sessions_in(d, &c.account.id, now),
                    reserved: table.reserved_in(d, &c.account.id, key.map(group), now),
                    threshold: table.spread_threshold_for(&c.account.id),
                    pacing: d.pacing.get(&c.account.id).copied().unwrap_or(false),
                    capped: placement_capped(c, d, &table.config, now),
                    placed: placement_evidence(c, d, now).1,
                }
            })
            .collect();
        let hold_until = bound
            .and_then(|id| candidates.iter().find(|c| c.account.id == id))
            .and_then(|c| {
                if c.account.user_state != UserState::Active {
                    return None;
                }
                let Health::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope,
                } = &c.account.health
                else {
                    return None;
                };
                if *until <= now
                    || !(scope == "account"
                        || scope
                            .strip_prefix("model:")
                            .is_some_and(|m| model == Some(m)))
                {
                    return None;
                }
                let reset = c.account.quota_cooling.as_ref()?.reset_at;
                (reset > now
                    && (reset - now).to_std().is_ok_and(|wait| {
                        wait <= Duration::from_secs(plan.settings.reset_wait_secs)
                    }))
                .then_some(reset)
            });
        let state = ReadState {
            candidates: reads,
            bound: bound.map(compact),
            hold_until,
            outside_unknown,
            capacity_available: true,
            unsupported,
        };
        t.state = Some(state);
    });
}
pub(super) fn capacity_refused(table: &BindingTable) {
    trace(table, |t| {
        if let Some(state) = &mut t.state {
            state.capacity_available = false;
        }
    });
}

pub(super) fn launch_outcome(
    result: &std::result::Result<LaunchPlacement, LaunchError>,
) -> Outcome {
    match result {
        Ok(p) => {
            let mut o = outcome(OutcomeKind::Dispatch, Some(compact(&p.account)), None);
            o.moves = replacements(&p.moves);
            o
        }
        Err(error) => {
            let mut o = outcome(OutcomeKind::Refusal, None, None);
            o.refusal = Some(
                match error {
                    LaunchError::InvalidMetadata => "invalid_metadata",
                    LaunchError::AnchorConflict => "anchor_conflict",
                    LaunchError::UnknownLaunch => "unknown_launch",
                    LaunchError::NoEligibleAccount => "no_eligible_account",
                    LaunchError::EntitlementsUnknown => "entitlements_unknown",
                    LaunchError::ModelUnsupported => "model_unsupported",
                    LaunchError::ConsentRequired => "consent_required",
                    LaunchError::Busy => "busy",
                    LaunchError::Capacity => "capacity",
                    LaunchError::PolicyBlocked => "policy_blocked",
                    LaunchError::MoveGuard => "move_guard",
                    LaunchError::BindingChanged => "binding_changed",
                    LaunchError::Retired => "retired",
                }
                .to_owned(),
            );
            o
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupWitnessFailure {
    ActiveState,
    ImageSerialization,
    AccountsSerialization,
    Publication,
}

impl StartupWitnessFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActiveState => "active_state",
            Self::ImageSerialization => "image_serialization",
            Self::AccountsSerialization => "accounts_serialization",
            Self::Publication => "publication",
        }
    }
}

impl std::fmt::Display for StartupWitnessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for StartupWitnessFailure {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialImage {
    pub schema: u16,
    pub bindings_sha256: String,
    pub policies_sha256: String,
    pub routing_sha256: String,
    pub revision: u64,
    pub bindings_rows: u32,
    pub activity_empty: bool,
    pub thresholds_sha256: String,
    pub outside_sha256: String,
    pub unknown_cost_sha256: String,
    pub replay_complete: bool,
}
impl BindingTable {
    /// Startup only; rejects any already-active decision state instead of inventing
    /// a pristine epoch. No files are read, restored or written by this method.
    pub fn decision_tape_initial_image(&self) -> Result<InitialImage> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if !d.active.is_empty()
            || !d.pins.is_empty()
            || !d.pending.is_empty()
            || !d.traffic.is_empty()
            || !d.placements.is_empty()
            || !d.rate_hits.is_empty()
            || !d.overload_marks.is_empty()
            || !d.successes.is_empty()
            || !d.epochs.is_empty()
            || !d.after_move.is_empty()
            || !d.last_request_bytes.is_empty()
            || !d.mark_history.is_empty()
            || !d.pacing.is_empty()
            || !d.overload_reported.is_empty()
            || !d.launch_activity.is_empty()
            || !d.deferred_fences.is_empty()
        {
            return Err(StartupWitnessFailure::ActiveState.into());
        }
        let digest = |value: &Value| -> Result<String> {
            Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
        };
        Ok(InitialImage {
            schema: 2,
            bindings_sha256: digest(&serde_json::to_value(snapshot(&d))?)?,
            policies_sha256: digest(&self.policies.list())?,
            routing_sha256: digest(&serde_json::to_value(&self.config)?)?,
            thresholds_sha256: digest(&serde_json::to_value(
                &*self.thresholds.lock().unwrap_or_else(|e| e.into_inner()),
            )?)?,
            outside_sha256: digest(&serde_json::json!({
                "used": &d.outside_used,
                "unknown": d.outside_unknown,
                "failure": d.outside_failure.as_ref().map(|flag| flag.load(Ordering::Acquire)),
            }))?,
            // Recovery deliberately lacks request-cost evidence. This is a
            // restored baseline, not live activity; commit it without clearing it.
            unknown_cost_sha256: digest(&serde_json::to_value(&d.unknown_cost)?)?,
            revision: d.revision,
            bindings_rows: u32::try_from(d.bindings.len()).context("initial binding row count")?,
            activity_empty: true,
            replay_complete: false,
        })
    }
}

#[cfg(test)]
mod tests;
