//! N7-L observation-only journal. No estimator, provider requests or background task.
//! Call sample/shutdown on the owner's blocking worker; observe_account never does IO.
mod recovery;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use qb_proto::admin_account::{
    AccountUsageForecast, AdminAccount, AdminHealth, AdminProvider, AdminUserState,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

const ACCOUNTS: usize = 128;
const WINDOWS: usize = 16;
const PENDING: usize = 16;
const CHECKPOINT_CAP: usize = 16 * 1024 * 1024;
const LOG_CAP: u64 = 64 * 1024 * 1024;
const DAY: i64 = 86400;
const RETENTION: i64 = 7 * DAY;
// Fixed implementer criterion: at most two 60-second sampling periods late.
// A later snapshot never stands in for a historical target observation.
const TARGET_OBSERVATION_GRACE_SECONDS: i64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailurePhase {
    Open,
    Observation,
    Sample,
    Checkpoint,
    Archive,
    Close,
    Recovery,
}
impl FailurePhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Observation => "observation",
            Self::Sample => "sample",
            Self::Checkpoint => "checkpoint",
            Self::Archive => "archive",
            Self::Close => "close",
            Self::Recovery => "recovery",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureReason {
    WindowName,
    DuplicateWindow,
    Forecast,
    Timestamp,
    Identity,
    DuplicateAccount,
    Bounds,
    Clock,
    Sequence,
    Storage,
    Closed,
    Stopped,
    Poisoned,
    Unknown,
    TaskJoin,
    SnapshotUnavailable,
}
impl FailureReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WindowName => "window_name",
            Self::DuplicateWindow => "duplicate_window",
            Self::Forecast => "forecast",
            Self::Timestamp => "timestamp",
            Self::Identity => "identity",
            Self::DuplicateAccount => "duplicate_account",
            Self::Bounds => "bounds",
            Self::Clock => "clock",
            Self::Sequence => "sequence",
            Self::Storage => "storage",
            Self::Closed => "closed",
            Self::Stopped => "stopped",
            Self::Poisoned => "poisoned",
            Self::Unknown => "unknown",
            Self::TaskJoin => "task_join",
            Self::SnapshotUnavailable => "snapshot_unavailable",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailureDiagnostic {
    pub phase: FailurePhase,
    pub reason: FailureReason,
}
impl std::fmt::Display for FailureDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.phase.as_str(), self.reason.as_str())
    }
}
impl std::error::Error for FailureDiagnostic {}

pub(crate) fn diagnostic(error: &anyhow::Error, phase: FailurePhase) -> FailureDiagnostic {
    // A typed context preserves the original failure across later persistence errors.
    if let Some(failure) = error.downcast_ref::<FailureDiagnostic>() {
        return *failure;
    }
    for cause in error.chain() {
        let reason = match cause.to_string().as_str() {
            "forecast_log_window_name_invalid" | "forecast_log_window_invalid" => {
                FailureReason::WindowName
            }
            "forecast_log_duplicate_window" => FailureReason::DuplicateWindow,
            "forecast_log_forecast_invalid"
            | "forecast_log_forecast_window_invalid"
            | "forecast_log_forecast_rate_invalid"
            | "forecast_log_forecast_credit_invalid"
            | "forecast_log_checkpoint_numeric"
            | "forecast_log_checkpoint_remaining"
            | "forecast_log_checkpoint_origin"
            | "forecast_log_checkpoint_window" => FailureReason::Forecast,
            "forecast_log_timestamp_invalid" => FailureReason::Timestamp,
            "forecast_log_invalid_identity"
            | "forecast_log_invalid_or_duplicate_identity"
            | "forecast_log_checkpoint_identity"
            | "forecast_log_checkpoint_generation_identity"
            | "forecast_log_checkpoint_input"
            | "forecast_log_checkpoint_provider" => FailureReason::Identity,
            "forecast_log_duplicate_account" => FailureReason::DuplicateAccount,
            "forecast_log_account_cap"
            | "forecast_log_window_cap"
            | "forecast_log_forecast_window_cap"
            | "forecast_log_pending_cap"
            | "forecast_log_checkpoint_cap"
            | "forecast_log_checkpoint_bounds"
            | "forecast_log_checkpoint_cohort"
            | "forecast_log_checkpoint_observation"
            | "forecast_log_segment_cap"
            | "forecast_log_log_cap"
            | "forecast_log_read_cap"
            | "forecast_log_size_overflow"
            | "forecast_log_revision_overflow" => FailureReason::Bounds,
            "forecast_log_clock_rollback"
            | "forecast_log_time_overflow"
            | "forecast_log_date_overflow"
            | "forecast_log_checkpoint_horizon" => FailureReason::Clock,
            "forecast_log_sequence_invalid"
            | "forecast_log_sequence_overflow"
            | "forecast_log_outbox_sequence"
            | "forecast_log_missing_journal_tail"
            | "forecast_log_checkpoint_behind_journal" => FailureReason::Sequence,
            "forecast_log_checkpoint_write"
            | "forecast_log_checkpoint_not_regular"
            | "forecast_log_segment_not_regular"
            | "forecast_log_torn_segment" => FailureReason::Storage,
            "forecast_log_closed" => FailureReason::Closed,
            "forecast_log_stopped" => FailureReason::Stopped,
            "forecast_log_worker_poisoned" | "forecast_log_observer_poisoned" => {
                FailureReason::Poisoned
            }
            "forecast_log_task_join" => FailureReason::TaskJoin,
            "forecast_log_snapshot_unavailable" => FailureReason::SnapshotUnavailable,
            _ => continue,
        };
        return FailureDiagnostic { phase, reason };
    }
    FailureDiagnostic {
        phase,
        reason: if error.chain().any(|cause| cause.is::<std::io::Error>()) {
            FailureReason::Storage
        } else {
            FailureReason::Unknown
        },
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub origins: u64,
    pub outcomes: u64,
    pub excluded_user_change: u64,
    pub excluded_restart: u64,
    pub excluded_observer_gap: u64,
    pub unavailable: u64,
    pub discarded_observer_races: u64,
    pub losses: u64,
    pub stopped: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    provider: String,
    state: AdminUserState,
    drain: bool,
    allow_credits: bool,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Users {
    accounts: BTreeMap<String, User>,
    revisions: BTreeMap<String, u64>,
    // Missing legacy witnesses remain unknown, never an invented generation zero.
    #[serde(default)]
    generations: BTreeMap<String, u64>,
    #[serde(skip)]
    session_seen: BTreeSet<String>,
    losses: u64,
}
impl Users {
    fn put(&mut self, id: &str, user: User) -> Result<()> {
        ensure!(
            valid_id(id, &user.provider),
            "forecast_log_invalid_identity"
        );
        if self.accounts.get(id) == Some(&user) {
            return Ok(());
        }
        ensure!(
            self.accounts.contains_key(id) || self.accounts.len() < ACCOUNTS,
            "forecast_log_account_cap"
        );
        let revision = self.revisions.entry(user.provider.clone()).or_default();
        *revision = revision
            .checked_add(1)
            .context("forecast_log_revision_overflow")?;
        self.accounts.insert(id.into(), user);
        Ok(())
    }
    fn revision(&self, provider: &str) -> u64 {
        self.revisions.get(provider).copied().unwrap_or(0)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Unavailable {
    TargetObservationLate,
    MissingOriginWindow,
    MissingWindow,
    InvalidWindow,
    MissingObservation,
    StaleObservation,
    ResetPending,
    MissingAccount,
    UserChanged,
    RestartGap,
    ObserverGap,
    TooFewAccounts,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Window {
    name: String,
    used_pct: Option<f64>,
    observed_at: Option<i64>,
    reset_at: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    id: String,
    user_state: AdminUserState,
    native_usable: bool,
    drain: bool,
    allow_credits: bool,
    probe_pending: bool,
    refresh_attention: bool,
    takeover_pending: bool,
    windows: Vec<Window>,
    existing_forecast: AccountUsageForecast,
    remaining: Option<f64>,
    unavailable: Option<Unavailable>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ForecastUnavailable {
    NoPooledModel,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    provider: String,
    at: i64,
    revision: u64,
    observer_losses: u64,
    cohort: Vec<Input>,
    remaining: Option<f64>,
    forecast_1h: Option<f64>,
    forecast_unavailable: ForecastUnavailable,
    forecast_3h: Option<f64>,
    // An unclean restart cannot prove absence of user edits during its gap.
    restart_gap: bool,
    done_1h: bool,
    done_3h: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Origin {
        origin: Origin,
    },
    Outcome {
        provider: String,
        origin_at: i64,
        horizon_seconds: i64,
        due_at: i64,
        sampled_at: i64,
        cohort: Vec<String>,
        observations: Vec<Input>,
        realised_r: Option<f64>,
        unavailable: Option<Unavailable>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    sequence: u64,
    sampled_at: i64,
    event: Event,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema: u32,
    clean: bool,
    last_sample: Option<i64>,
    last_origin_hour: Option<i64>,
    sequence: u64,
    users: Users,
    pending: Vec<Origin>,
    // Write-ahead outbox: append is idempotently recovered by sequence on open.
    outbox: Vec<Record>,
    status: Status,
    #[serde(default)]
    last_failure: Option<FailureDiagnostic>,
    #[serde(default)]
    recovery: Option<recovery::Receipt>,
}
impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            schema: 1,
            clean: true,
            last_sample: None,
            last_origin_hour: None,
            sequence: 0,
            users: Users::default(),
            pending: Vec::new(),
            outbox: Vec::new(),
            status: Status::default(),
            last_failure: None,
            recovery: None,
        }
    }
}
struct Worker {
    root: PathBuf,
    checkpoint: Checkpoint,
    log_cap: u64,
    recovered: bool,
    journal_last: u64,
    journal_bytes: u64,
}

pub struct ForecastLog {
    users: Mutex<Users>,
    worker: Mutex<Worker>,
    next_sample: AtomicI64,
    stopped: AtomicBool,
    closed: AtomicBool,
}
impl ForecastLog {
    pub fn open(home: &Path) -> Result<Self> {
        Self::open_inner(home)
            .map_err(|error| anyhow::Error::new(diagnostic(&error, FailurePhase::Open)))
    }
    fn open_inner(home: &Path) -> Result<Self> {
        let root = home.join("state/forecast-log");
        crate::platform::prepare_observation_directory(&root)?;
        let path = root.join("checkpoint.json");
        let mut checkpoint = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_file(),
                    "forecast_log_checkpoint_not_regular"
                );
                serde_json::from_slice::<Checkpoint>(&bounded(&path, CHECKPOINT_CAP)?)
                    .context("forecast_log_checkpoint_invalid")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Checkpoint::default(),
            Err(error) => return Err(error.into()),
        };
        validate_checkpoint(&checkpoint)?;
        // The native caller owns the Store lease; the preceding core closed its
        // logger before releasing it. Only the old, uncoded, zero-record failure
        // may start afresh. Keep its exact checkpoint in a sibling archive.
        if checkpoint.status.stopped
            && checkpoint.status.losses > 0
            && checkpoint.status.origins == 0
            && checkpoint.status.outcomes == 0
            && checkpoint.sequence == 0
            && checkpoint.last_sample.is_none()
            && checkpoint.last_origin_hour.is_none()
            && checkpoint.pending.is_empty()
            && checkpoint.outbox.is_empty()
            && checkpoint.last_failure.is_none()
            && checkpoint.recovery.is_none()
        {
            let receipt =
                recovery::archive_empty(&root, checkpoint.status.losses).map_err(|error| {
                    let failure = diagnostic(&error, FailurePhase::Recovery);
                    error.context(failure)
                })?;
            crate::platform::prepare_observation_directory(&root).context(FailureDiagnostic {
                phase: FailurePhase::Recovery,
                reason: FailureReason::Storage,
            })?;
            checkpoint = Checkpoint {
                recovery: Some(receipt),
                ..Checkpoint::default()
            };
            tracing::info!(
                phase = FailurePhase::Recovery.as_str(),
                reason = FailureReason::Stopped.as_str(),
                "Empty stopped forecast capture archived; starting fresh without backfill"
            );
        }
        let mut worker = Worker {
            root,
            checkpoint,
            log_cap: LOG_CAP,
            recovered: false,
            journal_last: 0,
            journal_bytes: 0,
        };
        // Recover append-vs-checkpoint interruptions before touching pending work.
        worker.drain_outbox()?;
        if !worker.checkpoint.clean {
            for origin in &mut worker.checkpoint.pending {
                origin.restart_gap = true;
            }
        }
        // Durable before publication: equal final user states cannot hide a
        // pause/resume lost between observation and the next periodic flush.
        worker.checkpoint.clean = false;
        worker.save()?;
        let stopped = worker.checkpoint.status.stopped;
        let users = worker.checkpoint.users.clone();
        Ok(Self {
            users: Mutex::new(users),
            worker: Mutex::new(worker),
            next_sample: AtomicI64::new(0),
            stopped: AtomicBool::new(stopped),
            closed: AtomicBool::new(false),
        })
    }
    pub fn due(&self, now: DateTime<Utc>) -> bool {
        !self.stopped.load(Ordering::Acquire) && !self.closed.load(Ordering::Acquire) && {
            let next = self.next_sample.load(Ordering::Acquire);
            now.timestamp() >= next || now.timestamp() < next.saturating_sub(60)
        }
    }
    /// Store observer; bounded memory only. Generation checks continuity only on first observation.
    pub fn observe_account(&self, account: &crate::accounts::Account) -> Result<()> {
        self.account_state(
            &account.id,
            account.provider.as_str(),
            match account.user_state {
                crate::accounts::UserState::Active => AdminUserState::Active,
                crate::accounts::UserState::Paused => AdminUserState::Paused,
                crate::accounts::UserState::Disabled => AdminUserState::Disabled,
            },
            account.drain,
            account.allow_credits,
            Some(account.generation),
        )
        .map_err(|error| anyhow::Error::new(diagnostic(&error, FailurePhase::Observation)))
    }
    fn account_state(
        &self,
        id: &str,
        provider: &str,
        state: AdminUserState,
        drain: bool,
        allow_credits: bool,
        generation: Option<u64>,
    ) -> Result<()> {
        ensure!(!self.closed.load(Ordering::Acquire), "forecast_log_closed");
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "forecast_log_stopped"
        );
        let mut users = self
            .users
            .lock()
            .map_err(|_| anyhow::anyhow!("forecast_log_observer_poisoned"))?;
        ensure!(!self.closed.load(Ordering::Acquire), "forecast_log_closed");
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "forecast_log_stopped"
        );
        let user = User {
            provider: provider.into(),
            state,
            drain,
            allow_credits,
        };
        // Compare the prior durable witness before replacing it. A closed gap
        // cannot classify a generation change as a definite user edit.
        let continuity_gap = generation.is_some_and(|generation| {
            !users.session_seen.contains(id)
                && users.accounts.get(id) == Some(&user)
                && users.generations.get(id).copied() != Some(generation)
        });
        let result = users.put(id, user);
        if result.is_err() {
            users.losses = users.losses.saturating_add(1);
        } else if let Some(generation) = generation {
            if continuity_gap {
                users.losses = users.losses.saturating_add(1);
            }
            // Automatic online updates refresh the next handover witness, but
            // generation never participates in User equality or revisions.
            users.generations.insert(id.into(), generation);
            users.session_seen.insert(id.into());
        }
        result
    }
    /// Full native snapshot only; no provider IO. Owner serializes these calls on a blocking worker.
    pub fn sample(&self, now: DateTime<Utc>, accounts: &[AdminAccount]) -> Result<()> {
        ensure!(!self.closed.load(Ordering::Acquire), "forecast_log_closed");
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "forecast_log_stopped"
        );
        let now = now.timestamp();
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| anyhow::anyhow!("forecast_log_worker_poisoned"))?;
        ensure!(!self.closed.load(Ordering::Acquire), "forecast_log_closed");
        let first_sample = worker.checkpoint.last_sample.is_none();
        let result = self.sample_inner(&mut worker, now, accounts);
        if let Err(error) = result {
            let failure = diagnostic(&error, FailurePhase::Sample);
            worker.checkpoint.last_failure = Some(failure);
            self.stopped.store(true, Ordering::Release);
            worker.checkpoint.status.stopped = true;
            worker.checkpoint.status.losses = worker.checkpoint.status.losses.saturating_add(1);
            // The caller receives failure even when persisting the loss also fails.
            if let Err(persistence) = worker.save() {
                return Err(
                    anyhow::Error::new(diagnostic(&persistence, FailurePhase::Checkpoint))
                        .context(failure),
                );
            }
            return Err(anyhow::Error::new(failure));
        }
        if first_sample && worker.checkpoint.last_sample == Some(now) {
            tracing::info!({ phase = FailurePhase::Sample.as_str(), event = "first_sample",
                origins = worker.checkpoint.status.origins, sequence = worker.checkpoint.sequence,
                losses = worker.checkpoint.status.losses }, "Forecast log first sample recorded");
        }
        self.next_sample
            .store(now.saturating_add(60), Ordering::Release);
        Ok(())
    }
    fn sample_inner(&self, worker: &mut Worker, now: i64, accounts: &[AdminAccount]) -> Result<()> {
        ensure!(accounts.len() <= ACCOUNTS, "forecast_log_account_cap");
        ensure!(
            worker.checkpoint.last_sample.is_none_or(|last| now >= last),
            "forecast_log_clock_rollback"
        );
        let mut ids = BTreeSet::new();
        let mut inputs = BTreeMap::new();
        for account in accounts {
            let provider = provider(account.provider);
            ensure!(
                valid_id(&account.id, provider),
                "forecast_log_invalid_identity"
            );
            ensure!(
                ids.insert(account.id.clone()),
                "forecast_log_duplicate_account"
            );
            inputs.insert(account.id.clone(), input(account, now)?);
        }
        let (users, observer_race) = {
            let mut users = self
                .users
                .lock()
                .map_err(|_| anyhow::anyhow!("forecast_log_observer_poisoned"))?;
            // Check the entire snapshot before seeding/removing any entries.
            // An ordinary user edit can make the already assembled DTO stale.
            let race = accounts.iter().any(|account| {
                let user = User {
                    provider: provider(account.provider).into(),
                    state: account.user_state,
                    drain: account.drain,
                    allow_credits: account.allow_credits,
                };
                users
                    .accounts
                    .get(&account.id)
                    .is_some_and(|old| old != &user)
            });
            if !race {
                for account in accounts {
                    users.put(
                        &account.id,
                        User {
                            provider: provider(account.provider).into(),
                            state: account.user_state,
                            drain: account.drain,
                            allow_credits: account.allow_credits,
                        },
                    )?;
                }
                let removed: Vec<_> = users
                    .accounts
                    .keys()
                    .filter(|id| !ids.contains(*id))
                    .cloned()
                    .collect();
                for id in removed {
                    let old = users.accounts.remove(&id).expect("listed user");
                    users.generations.remove(&id);
                    users.session_seen.remove(&id);
                    let revision = users.revisions.entry(old.provider).or_default();
                    *revision = revision
                        .checked_add(1)
                        .context("forecast_log_revision_overflow")?;
                }
            }
            (users.clone(), race)
        }; // Never hold the cheap store-observer lock across filesystem work.
        if observer_race {
            // Persist the observer's newer revision, not stale cohort/measurements.
            // No origin/outcome or sampling watermark advances. The outer call
            // schedules the next ordinary sample in 60s; this is not a stop error.
            worker.checkpoint.users = users;
            worker.checkpoint.status.discarded_observer_races = worker
                .checkpoint
                .status
                .discarded_observer_races
                .saturating_add(1);
            worker.checkpoint.status.losses = worker
                .checkpoint
                .status
                .losses
                .max(worker.checkpoint.users.losses);
            worker.save()?;
            return Ok(());
        }
        let mut next = worker.checkpoint.clone();
        next.users = users;
        next.last_sample = Some(now);
        let mut events = Vec::new();
        for origin in &mut next.pending {
            for horizon in [3600, 10800] {
                let done = if horizon == 3600 {
                    origin.done_1h
                } else {
                    origin.done_3h
                };
                let due = origin
                    .at
                    .checked_add(horizon)
                    .context("forecast_log_time_overflow")?;
                if done || now < due {
                    continue;
                }
                let mut observations = Vec::new();
                for account in &origin.cohort {
                    if let Some(value) = inputs.get(&account.id) {
                        observations.push(value.clone());
                    }
                }
                let reason = if origin.restart_gap {
                    Some(Unavailable::RestartGap)
                } else if next.users.revision(&origin.provider) != origin.revision {
                    Some(Unavailable::UserChanged)
                } else if next.users.losses != origin.observer_losses {
                    Some(Unavailable::ObserverGap)
                } else if now.saturating_sub(due) > TARGET_OBSERVATION_GRACE_SECONDS {
                    Some(Unavailable::TargetObservationLate)
                } else if origin.cohort.len() < 2 {
                    Some(Unavailable::TooFewAccounts)
                } else if observations.len() != origin.cohort.len() {
                    Some(Unavailable::MissingAccount)
                } else if origin
                    .cohort
                    .iter()
                    .zip(&observations)
                    .any(|(before, after)| {
                        before.windows.iter().any(|window| {
                            !after
                                .windows
                                .iter()
                                .any(|current| current.name == window.name)
                        })
                    })
                {
                    // No complete plan-change history exists here. A vanished
                    // limiting window cannot silently raise realised capacity.
                    Some(Unavailable::MissingOriginWindow)
                } else {
                    observations.iter().find_map(|a| a.unavailable)
                };
                let value = if reason.is_none() {
                    Some(
                        observations
                            .iter()
                            .map(|a| a.remaining.expect("known input"))
                            .sum(),
                    )
                } else {
                    None
                };
                match reason {
                    Some(Unavailable::RestartGap) => {
                        next.status.excluded_restart =
                            next.status.excluded_restart.saturating_add(1)
                    }
                    Some(Unavailable::UserChanged) => {
                        next.status.excluded_user_change =
                            next.status.excluded_user_change.saturating_add(1)
                    }
                    Some(Unavailable::ObserverGap) => {
                        next.status.excluded_observer_gap =
                            next.status.excluded_observer_gap.saturating_add(1)
                    }
                    Some(_) => next.status.unavailable = next.status.unavailable.saturating_add(1),
                    None => {}
                }
                next.status.outcomes = next.status.outcomes.saturating_add(1);
                events.push(Event::Outcome {
                    provider: origin.provider.clone(),
                    origin_at: origin.at,
                    horizon_seconds: horizon,
                    due_at: due,
                    sampled_at: now,
                    cohort: origin.cohort.iter().map(|a| a.id.clone()).collect(),
                    observations,
                    realised_r: value,
                    unavailable: reason,
                });
                if horizon == 3600 {
                    origin.done_1h = true;
                } else {
                    origin.done_3h = true;
                }
            }
        }
        next.pending.retain(|o| !o.done_1h || !o.done_3h);
        let hour = now.div_euclid(3600);
        if next.last_origin_hour != Some(hour) {
            for kind in [AdminProvider::Claude, AdminProvider::Codex] {
                let cohort: Vec<_> = accounts
                    .iter()
                    .filter(|a| a.provider == kind && usable(a))
                    .map(|a| inputs[&a.id].clone())
                    .collect();
                let remaining = if cohort.len() >= 2 && cohort.iter().all(|a| a.remaining.is_some())
                {
                    Some(cohort.iter().map(|a| a.remaining.unwrap()).sum())
                } else {
                    None
                };
                let origin = Origin {
                    provider: provider(kind).into(),
                    at: now,
                    revision: next.users.revision(provider(kind)),
                    observer_losses: next.users.losses,
                    cohort,
                    remaining,
                    forecast_1h: None,
                    forecast_unavailable: ForecastUnavailable::NoPooledModel,
                    forecast_3h: None,
                    restart_gap: false,
                    done_1h: false,
                    done_3h: false,
                };
                next.status.origins = next.status.origins.saturating_add(1);
                events.push(Event::Origin {
                    origin: origin.clone(),
                });
                next.pending.push(origin);
            }
            next.last_origin_hour = Some(hour);
        }
        ensure!(next.pending.len() <= PENDING, "forecast_log_pending_cap");
        for event in events {
            next.sequence = next
                .sequence
                .checked_add(1)
                .context("forecast_log_sequence_overflow")?;
            next.outbox.push(Record {
                sequence: next.sequence,
                sampled_at: now,
                event,
            });
        }
        next.status.losses = next.status.losses.max(next.users.losses);
        worker.prune(now)?;
        worker.checkpoint = next;
        worker.save()?; // Persist pending + outbox before appending externally visible records.
        worker.drain_outbox()
    }
    /// Owner must freeze store writes and stop/drain samplers before releasing
    /// its lease. No filesystem work occurs after this synchronous call returns.
    pub fn close(&self) -> Result<()> {
        self.close_inner()
            .map_err(|error| anyhow::Error::new(diagnostic(&error, FailurePhase::Close)))
    }
    fn close_inner(&self) -> Result<()> {
        let already_closed = self.closed.swap(true, Ordering::AcqRel);
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| anyhow::anyhow!("forecast_log_worker_poisoned"))?;
        if already_closed {
            ensure!(worker.checkpoint.clean, "forecast_log_close_incomplete");
            return Ok(());
        }
        let result = (|| -> Result<()> {
            worker.checkpoint.users = self
                .users
                .lock()
                .map_err(|_| anyhow::anyhow!("forecast_log_observer_poisoned"))?
                .clone();
            worker.checkpoint.status.losses = worker
                .checkpoint
                .status
                .losses
                .max(worker.checkpoint.users.losses);
            worker.save()?;
            worker.drain_outbox()?;
            worker.checkpoint.clean = true;
            worker.save()
        })();
        if let Err(error) = result {
            let failure = diagnostic(&error, FailurePhase::Close);
            worker.checkpoint.last_failure.get_or_insert(failure);
            self.stopped.store(true, Ordering::Release);
            worker.checkpoint.clean = false;
            worker.checkpoint.status.stopped = true;
            worker.checkpoint.status.losses = worker.checkpoint.status.losses.saturating_add(1);
            if let Err(persistence) = worker.save() {
                return Err(
                    anyhow::Error::new(diagnostic(&persistence, FailurePhase::Checkpoint))
                        .context(failure),
                );
            }
            return Err(anyhow::Error::new(failure));
        }
        Ok(())
    }
    pub fn status(&self) -> Result<Status> {
        let worker = self
            .worker
            .lock()
            .map_err(|_| anyhow::anyhow!("forecast_log_worker_poisoned"))?;
        Ok(worker.checkpoint.status)
    }
    pub fn last_failure(&self) -> Result<Option<FailureDiagnostic>> {
        self.worker
            .lock()
            .map(|worker| worker.checkpoint.last_failure)
            .map_err(|_| {
                anyhow::Error::new(FailureDiagnostic {
                    phase: FailurePhase::Open,
                    reason: FailureReason::Poisoned,
                })
            })
    }
}

impl Worker {
    fn save(&self) -> Result<()> {
        self.save_inner()
            .map_err(|error| anyhow::Error::new(diagnostic(&error, FailurePhase::Checkpoint)))
    }
    fn save_inner(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.checkpoint)?;
        ensure!(bytes.len() <= CHECKPOINT_CAP, "forecast_log_checkpoint_cap");
        super::atomic_write(&self.root.join("checkpoint.json"), &bytes)
            .context("forecast_log_checkpoint_write")
    }
    fn segments(&self) -> Result<Vec<(i64, PathBuf)>> {
        let mut result = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(day) = name
                .to_str()
                .and_then(|n| n.strip_prefix("day-"))
                .and_then(|n| n.strip_suffix(".jsonl"))
                .and_then(|n| n.parse::<i64>().ok())
            else {
                continue;
            };
            ensure!(
                entry.file_type()?.is_file(),
                "forecast_log_segment_not_regular"
            );
            result.push((day, entry.path()));
        }
        ensure!(result.len() <= 8, "forecast_log_segment_cap");
        result.sort_by_key(|(day, _)| *day);
        Ok(result)
    }
    fn prune(&mut self, now: i64) -> Result<()> {
        for (day, path) in self.segments()? {
            if day
                .checked_add(1)
                .and_then(|day| day.checked_mul(DAY))
                .context("forecast_log_date_overflow")?
                <= now.saturating_sub(RETENTION)
            {
                let size = fs::metadata(&path)?.len();
                fs::remove_file(path)?;
                self.journal_bytes = self.journal_bytes.saturating_sub(size);
            }
        }
        Ok(())
    }
    fn drain_outbox(&mut self) -> Result<()> {
        self.drain_outbox_inner()
            .map_err(|error| anyhow::Error::new(diagnostic(&error, FailurePhase::Archive)))
    }
    fn drain_outbox_inner(&mut self) -> Result<()> {
        let mut total = self.journal_bytes;
        let mut highest = self.journal_last;
        // Scan once on open, not once per minute; subsequent appends retain watermarks.
        if !self.recovered {
            let segments = self.segments()?;
            for (_, path) in &segments {
                let size = fs::metadata(path)?.len();
                total = total
                    .checked_add(size)
                    .context("forecast_log_size_overflow")?;
                ensure!(total <= self.log_cap, "forecast_log_log_cap");
                let bytes = bounded(path, self.log_cap as usize)?;
                ensure!(
                    bytes.is_empty() || bytes.last() == Some(&b'\n'),
                    "forecast_log_torn_segment"
                );
                for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
                    let record: Record =
                        serde_json::from_slice(line).context("forecast_log_segment_invalid")?;
                    validate_record(&record)?;
                    ensure!(record.sequence > highest, "forecast_log_sequence_invalid");
                    highest = record.sequence;
                }
            }
            self.recovered = true;
        }
        ensure!(
            highest <= self.checkpoint.sequence,
            "forecast_log_checkpoint_behind_journal"
        );
        ensure!(
            highest == self.checkpoint.sequence
                || self
                    .checkpoint
                    .outbox
                    .last()
                    .is_some_and(|r| r.sequence == self.checkpoint.sequence),
            "forecast_log_missing_journal_tail"
        );
        for record in &self.checkpoint.outbox {
            if record.sequence <= highest {
                continue;
            }
            let mut bytes = serde_json::to_vec(record)?;
            bytes.push(b'\n');
            ensure!(
                total
                    .checked_add(bytes.len() as u64)
                    .is_some_and(|size| size <= self.log_cap),
                "forecast_log_log_cap"
            );
            let path = self
                .root
                .join(format!("day-{}.jsonl", record.sampled_at.div_euclid(DAY)));
            let mut file = OpenOptions::new().create(true).append(true).open(path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            total += bytes.len() as u64;
            highest = record.sequence;
            self.journal_last = highest;
            self.journal_bytes = total;
        }
        self.journal_last = highest;
        self.journal_bytes = total;
        self.checkpoint.outbox.clear();
        self.save()
    }
}
fn bounded(path: &Path, cap: usize) -> Result<Vec<u8>> {
    ensure!(
        fs::metadata(path)?.len() <= cap as u64,
        "forecast_log_read_cap"
    );
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= cap, "forecast_log_read_cap");
    Ok(bytes)
}
fn provider(p: AdminProvider) -> &'static str {
    match p {
        AdminProvider::Claude => "claude",
        AdminProvider::Codex => "codex",
    }
}
fn valid_id(id: &str, provider: &str) -> bool {
    let prefix = match provider {
        "claude" => "cl-",
        "codex" => "cx-",
        _ => return false,
    };
    id.len() == 9
        && id.starts_with(prefix)
        && id.as_bytes()[3..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}
fn timestamp(value: &str) -> Result<i64> {
    ensure!(value.len() <= 40, "forecast_log_timestamp_invalid");
    Ok(DateTime::parse_from_rfc3339(value)
        .context("forecast_log_timestamp_invalid")?
        .timestamp())
}
fn window_name(name: &str) -> bool {
    // Native slot names carry no duration; never infer 5h/7d from these aliases.
    if matches!(name, "primary" | "secondary" | "7d_oi" | "7d_sonnet") {
        return true;
    }
    if name.len() > 21 {
        return false;
    }
    let Some((unit, digits)) = name.as_bytes().split_last() else {
        return false;
    };
    !digits.is_empty()
        && digits.iter().all(u8::is_ascii_digit)
        && matches!(*unit, b's' | b'm' | b'h' | b'd')
        && name[..name.len() - 1]
            .parse::<u64>()
            .ok()
            .is_some_and(|n| n > 0)
}
fn usable(a: &AdminAccount) -> bool {
    a.user_state == AdminUserState::Active
        && a.pool_eligible
        && !a.drain
        && !a.takeover_pending
        && !a.probe_pending
        && !a.refresh_attention
        && matches!(a.health, AdminHealth::Ready {})
}
fn input(a: &AdminAccount, now: i64) -> Result<Input> {
    ensure!(
        a.windows.len() <= WINDOWS && a.usage_forecast.windows.len() <= WINDOWS,
        "forecast_log_window_cap"
    );
    // Forecast wire fields contain only numeric/timestamp/allowlisted-window facts;
    // no label, email, source, identity or arbitrary account payload is copied.
    validate_forecast(&a.usage_forecast)?;
    let f = &a.usage_forecast;
    let mut windows = Vec::new();
    let mut names = BTreeSet::new();
    let mut reason = a.windows.is_empty().then_some(Unavailable::MissingWindow);
    let mut remaining = 1.0f64;
    for w in &a.windows {
        ensure!(window_name(&w.name), "forecast_log_window_name_invalid");
        ensure!(names.insert(&w.name), "forecast_log_duplicate_window");
        let observed = w
            .observed_at
            .as_deref()
            .or(a.observed_at.as_deref())
            .map(timestamp)
            .transpose()?;
        let reset = w.reset_at.as_deref().map(timestamp).transpose()?;
        let valid = w
            .used_pct
            .filter(|x| x.is_finite() && (0.0..=100.0).contains(x));
        let missing = if valid.is_none() {
            Some(Unavailable::InvalidWindow)
        } else if observed.is_none() {
            Some(Unavailable::MissingObservation)
        } else if observed.is_some_and(|at| at > now || now.saturating_sub(at) > 300) {
            Some(Unavailable::StaleObservation)
        } else if reset
            .is_some_and(|at| at <= now && observed.is_some_and(|observed| observed < at))
        {
            Some(Unavailable::ResetPending)
        } else {
            None
        };
        reason = reason.or(missing);
        if let Some(used) = valid {
            remaining = remaining.min(1. - used / 100.);
        }
        windows.push(Window {
            name: w.name.clone(),
            used_pct: valid,
            observed_at: observed,
            reset_at: reset,
        });
    }
    Ok(Input {
        id: a.id.clone(),
        user_state: a.user_state,
        native_usable: usable(a),
        drain: a.drain,
        allow_credits: a.allow_credits,
        probe_pending: a.probe_pending,
        refresh_attention: a.refresh_attention,
        takeover_pending: a.takeover_pending,
        windows,
        existing_forecast: f.clone(),
        remaining: reason.is_none().then_some(remaining),
        unavailable: reason,
    })
}
fn validate_forecast(f: &AccountUsageForecast) -> Result<()> {
    ensure!(
        f.windows.len() <= WINDOWS,
        "forecast_log_forecast_window_cap"
    );
    timestamp(&f.as_of)?;
    if let Some(at) = &f.observed_at {
        timestamp(at)?;
    }
    ensure!(
        f.earliest_window.as_ref().is_none_or(|n| window_name(n)),
        "forecast_log_forecast_window_invalid"
    );
    for w in &f.windows {
        ensure!(
            window_name(&w.name)
                && w.window_seconds > 0
                && w.used_pct.is_finite()
                && (0.0..=100.0).contains(&w.used_pct),
            "forecast_log_forecast_invalid"
        );
        timestamp(&w.observed_at)?;
        for at in [
            &w.reset_at,
            &w.recent_span_start,
            &w.measurement_start,
            &w.exhaustion_at,
        ]
        .into_iter()
        .flatten()
        {
            timestamp(at)?;
        }
        ensure!(
            [w.recent_pct_per_hour, w.average_pct_per_hour]
                .into_iter()
                .flatten()
                .all(|x| x.is_finite() && x >= 0.),
            "forecast_log_forecast_rate_invalid"
        );
        if let Some(s) = &w.secondary_recent {
            ensure!(
                s.pct_per_hour.is_finite() && s.pct_per_hour >= 0.,
                "forecast_log_forecast_rate_invalid"
            );
            timestamp(&s.measurement_start)?;
            if let Some(at) = &s.exhaustion_at {
                timestamp(at)?;
            }
        }
    }
    if let Some(c) = &f.credit {
        timestamp(&c.observed_at)?;
        for at in [&c.measurement_start, &c.exhaustion_at]
            .into_iter()
            .flatten()
        {
            timestamp(at)?;
        }
        ensure!(
            c.balance.is_finite()
                && c.balance >= 0.
                && c.recent_credits_per_hour
                    .is_none_or(|x| x.is_finite() && x >= 0.),
            "forecast_log_forecast_credit_invalid"
        );
    }
    Ok(())
}
fn validate_checkpoint(c: &Checkpoint) -> Result<()> {
    if let Some(receipt) = &c.recovery {
        ensure!(
            receipt.archive.starts_with("forecast-log.failed-")
                && receipt.archive.len() <= 96
                && receipt
                    .archive
                    .strip_prefix("forecast-log.failed-")
                    .is_some_and(|suffix| {
                        !suffix.is_empty()
                            && suffix
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    })
                && DateTime::from_timestamp(receipt.at, 0).is_some()
                && receipt.prior_losses > 0,
            FailureDiagnostic {
                phase: FailurePhase::Open,
                reason: FailureReason::Bounds,
            }
        );
    }
    ensure!(
        c.schema == 1
            && c.users.accounts.len() <= ACCOUNTS
            && c.users.generations.len() <= ACCOUNTS
            && c.users.session_seen.len() <= ACCOUNTS
            && c.pending.len() <= PENDING
            && c.outbox.len() <= 32,
        "forecast_log_checkpoint_bounds"
    );
    ensure!(
        c.users
            .revisions
            .keys()
            .all(|p| matches!(p.as_str(), "claude" | "codex")),
        "forecast_log_checkpoint_provider"
    );
    for (id, user) in &c.users.accounts {
        ensure!(
            valid_id(id, &user.provider),
            "forecast_log_checkpoint_identity"
        );
    }
    ensure!(
        c.users
            .generations
            .keys()
            .chain(c.users.session_seen.iter())
            .all(|id| c.users.accounts.contains_key(id)),
        "forecast_log_checkpoint_generation_identity"
    );
    for o in &c.pending {
        validate_origin(o)?;
    }
    let mut sequence = 0;
    for record in &c.outbox {
        ensure!(
            record.sequence > sequence && record.sequence <= c.sequence,
            "forecast_log_outbox_sequence"
        );
        validate_record(record)?;
        sequence = record.sequence;
    }
    Ok(())
}

fn validate_input(a: &Input, provider: &str) -> Result<()> {
    ensure!(
        valid_id(&a.id, provider) && a.windows.len() <= WINDOWS,
        "forecast_log_checkpoint_input"
    );
    ensure!(
        a.remaining
            .is_none_or(|v| v.is_finite() && (0.0..=1.0).contains(&v))
            && a.remaining.is_some() == a.unavailable.is_none(),
        "forecast_log_checkpoint_numeric"
    );
    let mut names = BTreeSet::new();
    for w in &a.windows {
        ensure!(
            window_name(&w.name)
                && names.insert(&w.name)
                && w.used_pct
                    .is_none_or(|v| v.is_finite() && (0.0..=100.0).contains(&v)),
            "forecast_log_checkpoint_window"
        );
    }
    validate_forecast(&a.existing_forecast)
}
fn validate_origin(o: &Origin) -> Result<()> {
    ensure!(
        matches!(o.provider.as_str(), "claude" | "codex")
            && o.cohort.len() <= ACCOUNTS
            && o.forecast_1h.is_none()
            && o.forecast_3h.is_none(),
        "forecast_log_checkpoint_origin"
    );
    ensure!(
        o.remaining
            .is_none_or(|v| v.is_finite() && v >= 0. && v <= o.cohort.len() as f64),
        "forecast_log_checkpoint_remaining"
    );
    let mut ids = BTreeSet::new();
    for a in &o.cohort {
        ensure!(ids.insert(&a.id), "forecast_log_checkpoint_cohort");
        validate_input(a, &o.provider)?;
    }
    Ok(())
}
fn validate_record(record: &Record) -> Result<()> {
    match &record.event {
        Event::Origin { origin } => validate_origin(origin),
        Event::Outcome {
            provider,
            origin_at,
            horizon_seconds,
            due_at,
            sampled_at,
            cohort,
            observations,
            realised_r,
            unavailable,
        } => {
            ensure!(
                matches!(provider.as_str(), "claude" | "codex")
                    && matches!(*horizon_seconds, 3600 | 10800)
                    && origin_at.checked_add(*horizon_seconds) == Some(*due_at)
                    && sampled_at >= due_at,
                "forecast_log_checkpoint_horizon"
            );
            ensure!(
                cohort.len() <= ACCOUNTS && observations.len() <= cohort.len(),
                "forecast_log_checkpoint_cohort"
            );
            let ids: BTreeSet<_> = cohort.iter().cloned().collect();
            ensure!(
                ids.len() == cohort.len() && cohort.iter().all(|id| valid_id(id, provider)),
                "forecast_log_checkpoint_identity"
            );
            ensure!(
                realised_r.is_some() == unavailable.is_none()
                    && realised_r
                        .is_none_or(|v| v.is_finite() && v >= 0. && v <= cohort.len() as f64),
                "forecast_log_checkpoint_numeric"
            );
            let mut seen = BTreeSet::new();
            for a in observations {
                ensure!(
                    ids.contains(&a.id) && seen.insert(&a.id),
                    "forecast_log_checkpoint_observation"
                );
                validate_input(a, provider)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "forecast_log_tests.rs"]
mod tests;
