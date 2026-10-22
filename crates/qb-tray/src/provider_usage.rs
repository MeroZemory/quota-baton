//! Provider account-equivalents, using the current cohort throughout history.
//! Inputs are owner-verified, current-cohort facts, not translated display labels.
//! `valid_until` is an exclusive observation coverage bound, chosen by the adapter
//! from source freshness/reset knowledge. This module cannot certify that bound.
//! Phases are explicit verified cycles: never derive older/new cycles from one
//! current deadline. Future pace is an ideal schedule, not observed capacity.

use chrono::{DateTime, Duration, Timelike, Utc};
use std::collections::BTreeSet;

pub const MAX_ACCOUNTS: usize = 128;
pub const MAX_TOTAL_OBSERVATIONS: usize = 32_768;
pub const MAX_TOTAL_PHASES: usize = 4_096;
pub const MAX_TOTAL_RESETS: usize = 8_192;
pub const MAX_KNOTS: usize = 81_923;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountState {
    Active,
    Paused,
    Disabled,
    Excluded,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKind {
    Weekly,
    Free30Day,
    Unsupported,
}
impl WindowKind {
    fn seconds(self) -> Option<i64> {
        match self {
            Self::Weekly => Some(7 * 86400),
            Self::Free30Day => Some(30 * 86400),
            Self::Unsupported => None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Observation {
    pub at: DateTime<Utc>,
    /// None explicitly interrupts known history. Never substitute zero.
    pub used_pct: Option<f64>,
    /// Required to carry a point forward; None means unknown coverage.
    pub valid_until: Option<DateTime<Utc>>,
}
#[derive(Clone, Copy, Debug)]
pub struct Phase {
    pub start: DateTime<Utc>,
    pub reset: DateTime<Utc>,
}
#[derive(Clone, Debug)]
pub struct WindowInput {
    /// Adapter must select ONE account-scoped weekly/Free30d window. Model
    /// windows and 5h must map to Unsupported, not a manufactured long window.
    pub kind: WindowKind,
    pub observations: Vec<Observation>,
    pub phases: Vec<Phase>,
    /// Observed reset markers can interrupt actual history without a known phase.
    pub resets: Vec<DateTime<Utc>>,
    pub truncated: bool,
}
#[derive(Clone, Debug)]
pub struct AccountInput {
    pub id: String,
    pub provider: Provider,
    pub state: AccountState,
    /// Metadata only: deliberately never used as a weight.
    pub plan: String,
    pub window: Option<WindowInput>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coverage {
    pub known: usize,
    pub total: usize,
    /// Full-cohort total only; never silently sum a changing subset.
    pub value: Option<f64>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Values {
    pub remaining: Coverage,
    pub pace: Coverage,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Knot {
    pub at: DateTime<Utc>,
    /// Separate one-sided limits preserve steps/reset jumps, not diagonals.
    pub left: Values,
    pub right: Values,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GapSpan {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub remaining: f64,
    pub pace_from: f64,
    pub pace_to: f64,
    // Renderer splits at a sign crossing using these linear endpoint gaps.
    // No span is emitted across unknown coverage or into future actual data.
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Ahead,
    Behind,
    OnPace,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Verdict {
    pub difference: f64,
    pub direction: Direction,
    pub tenths: u64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Projection {
    pub provider: Provider,
    pub from: DateTime<Utc>,
    pub now: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub cohort: usize,
    pub excluded_paused: usize,
    pub excluded_disabled: usize,
    pub excluded_other: usize,
    pub weekly: usize,
    pub free_30_day: usize,
    pub history_truncated: bool,
    pub knots: Vec<Knot>,
    pub gap_spans: Vec<GapSpan>,
    pub current: Values,
    pub verdict: Option<Verdict>,
    /// Earliest discrete coverage/verified-phase change after `now`.
    /// A repaint deadline, never evidence of a future observation or next cycle.
    pub next_change: Option<DateTime<Utc>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Capacity,
    Identity,
    DuplicateObservation,
    InvalidPhase,
    ClockBounds,
}

const FRESH_SECONDS: i64 = 300;
const MAX_WINDOWS_PER_ACCOUNT: usize = 32;

/// Adapts actual dashboard DTOs, never translated labels or plan weights.
/// Both panels survive invalid/over-capacity input with explicit unknown values.
/// Paused/disabled counts remain separate; drained and unrecognized states are
/// counted as excluded_other. Historical membership is NOT reconstructed.
pub fn project_snapshot(
    snapshot: &crate::dashboard_model::DashboardSnapshot,
    clock: &crate::dashboard_model::Clock,
) -> Vec<Projection> {
    let now = clock.utc();
    let minute = clock
        .now
        .with_second(0)
        .and_then(|at| at.with_nanosecond(0))
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or(now);
    let accounts = snapshot_inputs(snapshot, now);
    [Provider::Claude, Provider::Codex]
        .into_iter()
        .map(|provider| {
            accounts
                .as_ref()
                .ok()
                .and_then(|accounts| project_in_minute(provider, accounts, now, minute).ok())
                .map(|mut projection| {
                    projection.history_truncated |= snapshot.usage.truncated;
                    projection
                })
                .unwrap_or_else(|| unknown_projection(provider, snapshot, now, minute))
        })
        .collect()
}

fn provider(value: &str) -> Option<Provider> {
    match value {
        "claude" => Some(Provider::Claude),
        "codex" => Some(Provider::Codex),
        _ => None,
    }
}

fn unknown_projection(
    selected: Provider,
    snapshot: &crate::dashboard_model::DashboardSnapshot,
    now: DateTime<Utc>,
    minute: DateTime<Utc>,
) -> Projection {
    // This fallback borrows existing rows, with no input-sized allocation.
    let rows = snapshot
        .accounts
        .raw
        .accounts
        .iter()
        .filter(|a| provider(&a.provider) == Some(selected));
    let mut cohort = 0;
    let mut excluded_paused = 0;
    let mut excluded_disabled = 0;
    let mut excluded_other = 0;
    for row in rows {
        match row.user_state.as_str() {
            "active" if !row.drain => cohort += 1,
            "paused" => excluded_paused += 1,
            "disabled" => excluded_disabled += 1,
            _ => excluded_other += 1,
        }
    }
    let unknown = Coverage {
        known: 0,
        total: cohort,
        value: None,
    };
    let current = Values {
        remaining: unknown,
        pace: unknown,
    };
    let from = minute
        .checked_sub_signed(Duration::hours(19))
        .unwrap_or(now);
    let to = minute.checked_add_signed(Duration::hours(5)).unwrap_or(now);
    Projection {
        provider: selected,
        from,
        now,
        to,
        cohort,
        excluded_paused,
        excluded_disabled,
        excluded_other,
        weekly: 0,
        free_30_day: 0,
        history_truncated: true,
        knots: [from, now, to]
            .into_iter()
            .map(|at| Knot {
                at,
                left: current,
                right: current,
            })
            .collect(),
        gap_spans: vec![],
        current,
        verdict: None,
        next_change: None,
    }
}

fn timestamp(value: Option<&str>) -> Option<DateTime<Utc>> {
    let value = value.filter(|value| value.len() <= 64)?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn long_kind(account: &crate::v2::Account, window: &crate::v2::Window) -> Option<WindowKind> {
    if !crate::v2::account_window(window) {
        return None;
    }
    match window.name.as_str() {
        "7d" | "10080m" => Some(WindowKind::Weekly),
        "secondary"
            if window
                .extra
                .get("window_seconds")
                .and_then(serde_json::Value::as_u64)
                == Some(7 * 86400) =>
        {
            Some(WindowKind::Weekly)
        }
        "43200m"
            if account.provider == "codex"
                && account
                    .plan
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case("free")) =>
        {
            Some(WindowKind::Free30Day)
        }
        _ => None,
    }
}

fn snapshot_inputs(
    snapshot: &crate::dashboard_model::DashboardSnapshot,
    now: DateTime<Utc>,
) -> Result<Vec<AccountInput>, Error> {
    let raw = &snapshot.accounts.raw.accounts;
    let history = &snapshot.usage.accounts;
    if raw.len() > MAX_ACCOUNTS || history.len() > MAX_ACCOUNTS {
        return Err(Error::Capacity);
    }
    let mut points = raw.len(); // Reserve room for at most one current point per account.
    let mut resets = raw.len();
    let mut history_ids = BTreeSet::new();
    for account in history {
        if account.id.is_empty() || account.id.len() > 256 || !history_ids.insert(&account.id) {
            return Err(Error::Identity);
        }
        if account.windows.len() > MAX_WINDOWS_PER_ACCOUNT {
            return Err(Error::Capacity);
        }
        let mut names = BTreeSet::new();
        for window in &account.windows {
            if window.name.len() > 128 || !names.insert(&window.name) {
                return Err(Error::Identity);
            }
            points = points
                .checked_add(window.points.len())
                .ok_or(Error::Capacity)?;
            resets = resets
                .checked_add(window.resets.len())
                .ok_or(Error::Capacity)?;
            if points > MAX_TOTAL_OBSERVATIONS || resets > MAX_TOTAL_RESETS {
                return Err(Error::Capacity);
            }
        }
    }
    let mut accounts = Vec::with_capacity(raw.len());
    let mut ids = BTreeSet::new();
    for account in raw {
        if account.id.is_empty()
            || account.id.len() > 256
            || account.plan.as_ref().is_some_and(|p| p.len() > 256)
            || !ids.insert(&account.id)
        {
            return Err(Error::Identity);
        }
        if account.windows.len() > MAX_WINDOWS_PER_ACCOUNT {
            return Err(Error::Capacity);
        }
        if account.windows.iter().any(|w| w.name.len() > 128) {
            return Err(Error::Identity);
        }
        let Some(provider) = provider(&account.provider) else {
            continue;
        };
        let state = match account.user_state.as_str() {
            "paused" => AccountState::Paused,
            "disabled" => AccountState::Disabled,
            "active" if !account.drain => AccountState::Active,
            _ => AccountState::Excluded,
        };
        let mut candidates = account
            .windows
            .iter()
            .filter_map(|w| long_kind(account, w).map(|kind| (w, kind)));
        let selected = candidates.next();
        if candidates.next().is_some() {
            return Err(Error::Identity); // Ambiguous long-window membership, never select by ordering.
        }
        let mut window = None;
        if let Some((current, kind)) = selected {
            let archived = history.iter().find(|h| h.id == account.id);
            let series = archived.and_then(|h| h.windows.iter().find(|w| w.name == current.name));
            let mut observations: Vec<Observation> = series
                .into_iter()
                .flat_map(|s| &s.points)
                .map(|(at, used_pct)| Observation {
                    at: *at,
                    used_pct: *used_pct,
                    valid_until: at.checked_add_signed(Duration::seconds(FRESH_SECONDS)),
                })
                .collect();
            let mut resets: Vec<_> = series
                .into_iter()
                .flat_map(|s| &s.resets)
                .copied()
                .filter(|at| *at <= now)
                .collect();
            let observed = timestamp(current.observed_at.as_deref());
            let reset = timestamp(current.reset_at.as_deref());
            if let Some(at) = observed {
                if let Some(existing) = observations.iter().find(|o| o.at == at) {
                    if existing.used_pct != current.used_pct {
                        return Err(Error::DuplicateObservation);
                    }
                } else {
                    observations.push(Observation {
                        at,
                        used_pct: current.used_pct,
                        valid_until: at.checked_add_signed(Duration::seconds(FRESH_SECONDS)),
                    });
                }
            }
            // Past reset markers are actual boundaries. Only two adjacent
            // verified markers a full window apart establish a historical phase.
            resets.sort_unstable();
            resets.dedup();
            let duration = Duration::seconds(kind.seconds().ok_or(Error::InvalidPhase)?);
            let mut phases: Vec<_> = resets
                .windows(2)
                .filter_map(|pair| {
                    (pair[1].signed_duration_since(pair[0]) == duration).then_some(Phase {
                        start: pair[0],
                        reset: pair[1],
                    })
                })
                .collect();
            if let Some(reset) = reset.filter(|_| observed.is_some_and(|at| at <= now)) {
                resets.push(reset);
                // This is ONE explicit current cycle, not repeated earlier or
                // future cycles. A future-stamped observation cannot establish it.
                if let (Some(start), Some(at)) = (reset.checked_sub_signed(duration), observed)
                    && at <= now
                    && start <= at
                    && at < reset
                    // A real intervening reset contradicts this inferred
                    // start. Keep measured usage, but leave pace unknown.
                    && !resets.iter().any(|at| start < *at && *at < reset)
                {
                    phases.push(Phase { start, reset });
                }
            }
            phases.sort_by_key(|p| (p.start, p.reset));
            phases.dedup_by_key(|p| (p.start, p.reset));
            window = Some(WindowInput {
                kind,
                observations,
                phases,
                resets,
                truncated: snapshot.usage.truncated
                    || archived.is_some_and(|h| h.truncated)
                    || series.is_some_and(|s| s.truncated),
            });
        }
        accounts.push(AccountInput {
            id: account.id.clone(),
            provider,
            state,
            plan: account.plan.clone().unwrap_or_default(),
            window,
        });
    }
    Ok(accounts)
}

struct Prepared<'a> {
    observations: Vec<&'a Observation>,
    phases: Vec<Phase>,
    resets: Vec<DateTime<Utc>>,
}
#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}
impl Side {
    fn started(self, a: DateTime<Utc>, t: DateTime<Utc>) -> bool {
        match self {
            Self::Left => a < t,
            Self::Right => a <= t,
        }
    }
    fn not_ended(self, t: DateTime<Utc>, e: DateTime<Utc>) -> bool {
        match self {
            Self::Left => t <= e,
            Self::Right => t < e,
        }
    }
}

fn prepare(window: Option<&WindowInput>) -> Result<Option<Prepared<'_>>, Error> {
    let Some(w) = window.filter(|w| w.kind.seconds().is_some()) else {
        return Ok(None);
    };
    let seconds = w.kind.seconds().unwrap();
    let mut observations: Vec<_> = w.observations.iter().collect();
    observations.sort_by_key(|o| o.at);
    if observations.windows(2).any(|pair| pair[0].at == pair[1].at) {
        return Err(Error::DuplicateObservation);
    }
    let mut phases = w.phases.clone();
    phases.sort_by_key(|p| (p.start, p.reset));
    if phases
        .iter()
        .any(|p| p.reset.signed_duration_since(p.start) != Duration::seconds(seconds))
        || phases.windows(2).any(|pair| pair[0].reset > pair[1].start)
    {
        return Err(Error::InvalidPhase);
    }
    let mut resets = w.resets.clone();
    resets.extend(phases.iter().flat_map(|p| [p.start, p.reset]));
    resets.sort_unstable();
    resets.dedup();
    if phases.iter().any(|phase| {
        let next = resets.partition_point(|reset| *reset <= phase.start);
        resets.get(next).is_some_and(|reset| *reset < phase.reset)
    }) {
        return Err(Error::InvalidPhase);
    }
    Ok(Some(Prepared {
        observations,
        phases,
        resets,
    }))
}

fn remaining(p: &Prepared<'_>, t: DateTime<Utc>, now: DateTime<Utc>, side: Side) -> Option<f64> {
    if t > now {
        return None;
    }
    let end = p.observations.partition_point(|o| side.started(o.at, t));
    let o = *p.observations.get(end.checked_sub(1)?)?;
    let used = o.used_pct.filter(|u| u.is_finite() && *u >= 0.)?;
    let until = o.valid_until.filter(|e| *e > o.at)?;
    if !side.not_ended(t, until) {
        return None;
    }
    // An observation exactly at a reset belongs to the NEW cycle. An older
    // observation cannot be held across a reset even if its supplied bound errs.
    let reset_index = p.resets.partition_point(|reset| *reset <= o.at);
    if p.resets
        .get(reset_index)
        .is_some_and(|reset| side.started(*reset, t))
    {
        return None;
    }
    Some((1. - used / 100.).clamp(0., 1.))
}

fn pace(p: &Prepared<'_>, t: DateTime<Utc>, side: Side) -> Option<f64> {
    // Lookup separate left/right phases to preserve a verified contiguous jump.
    let end = p
        .phases
        .partition_point(|phase| side.started(phase.start, t));
    let phase = p.phases.get(end.checked_sub(1)?)?;
    if !side.not_ended(t, phase.reset) {
        return None;
    }
    let numerator = phase.reset.signed_duration_since(t).num_nanoseconds()?;
    let denominator = phase
        .reset
        .signed_duration_since(phase.start)
        .num_nanoseconds()?;
    Some((numerator as f64 / denominator as f64).clamp(0., 1.))
}

fn next_change(inputs: &[Option<Prepared<'_>>], now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    inputs
        .iter()
        .flatten()
        .filter_map(|p| {
            // Only the latest currently contributing observation can expire. An old
            // superseded bound must not schedule a fictitious coverage change.
            let observation_end = if remaining(p, now, now, Side::Right).is_some() {
                let end = p.observations.partition_point(|o| o.at <= now);
                p.observations[end - 1]
                    .valid_until
                    .into_iter()
                    .chain(p.resets.iter().copied().find(|at| *at > now))
                    .filter(|at| *at > now)
                    .min()
            } else {
                None
            };
            // Includes known -> unknown at a phase end and unknown -> known at an
            // explicit verified start. Never manufacture a subsequent phase.
            let phase_change = p
                .phases
                .iter()
                .flat_map(|phase| [phase.start, phase.reset])
                .filter(|at| *at > now)
                .min();
            observation_end.into_iter().chain(phase_change).min()
        })
        .min()
}

fn coverage(values: impl Iterator<Item = Option<f64>>, total: usize) -> Coverage {
    let mut known = 0;
    let mut sum = 0.;
    for value in values.flatten() {
        known += 1;
        sum += value;
    }
    Coverage {
        known,
        total,
        value: (total > 0 && known == total).then_some(sum),
    }
}
fn values(
    inputs: &[Option<Prepared<'_>>],
    t: DateTime<Utc>,
    now: DateTime<Utc>,
    side: Side,
) -> Values {
    Values {
        remaining: coverage(
            inputs
                .iter()
                .map(|p| p.as_ref().and_then(|p| remaining(p, t, now, side))),
            inputs.len(),
        ),
        pace: coverage(
            inputs
                .iter()
                .map(|p| p.as_ref().and_then(|p| pace(p, t, side))),
            inputs.len(),
        ),
    }
}

/// Pure projection at injected UTC time, on the rolling minute-aligned domain.
/// The observation/deadline time remains the full injected time.
/// Membership is the CURRENT cohort throughout the chart, not reconstructed
/// historical account state. `history_truncated` must remain visible to users.
/// No futures/reset phases or observations are generated by this function.
pub fn project(
    provider: Provider,
    accounts: &[AccountInput],
    now: DateTime<Utc>,
) -> Result<Projection, Error> {
    let minute = now
        .with_second(0)
        .and_then(|at| at.with_nanosecond(0))
        .ok_or(Error::ClockBounds)?;
    project_in_minute(provider, accounts, now, minute)
}

fn project_in_minute(
    provider: Provider,
    accounts: &[AccountInput],
    now: DateTime<Utc>,
    minute: DateTime<Utc>,
) -> Result<Projection, Error> {
    if accounts.len() > MAX_ACCOUNTS {
        return Err(Error::Capacity);
    }
    let from = minute
        .checked_sub_signed(Duration::hours(19))
        .ok_or(Error::ClockBounds)?;
    let to = minute
        .checked_add_signed(Duration::hours(5))
        .ok_or(Error::ClockBounds)?;
    let mut observations = 0usize;
    let mut phases = 0usize;
    let mut resets = 0usize;
    for a in accounts {
        if a.id.is_empty() || a.id.len() > 256 || a.plan.len() > 256 {
            return Err(Error::Identity);
        }
        if let Some(w) = &a.window {
            observations = observations
                .checked_add(w.observations.len())
                .ok_or(Error::Capacity)?;
            phases = phases.checked_add(w.phases.len()).ok_or(Error::Capacity)?;
            resets = resets.checked_add(w.resets.len()).ok_or(Error::Capacity)?;
        }
    }
    if observations > MAX_TOTAL_OBSERVATIONS
        || phases > MAX_TOTAL_PHASES
        || resets > MAX_TOTAL_RESETS
    {
        return Err(Error::Capacity);
    }
    let mut rows: Vec<_> = accounts.iter().filter(|a| a.provider == provider).collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    if rows.windows(2).any(|p| p[0].id == p[1].id) {
        return Err(Error::Identity);
    }
    let excluded_paused = rows
        .iter()
        .filter(|a| a.state == AccountState::Paused)
        .count();
    let excluded_disabled = rows
        .iter()
        .filter(|a| a.state == AccountState::Disabled)
        .count();
    let excluded_other = rows
        .iter()
        .filter(|a| a.state == AccountState::Excluded)
        .count();
    rows.retain(|a| a.state == AccountState::Active);
    let weekly = rows
        .iter()
        .filter(|a| {
            a.window
                .as_ref()
                .is_some_and(|w| w.kind == WindowKind::Weekly)
        })
        .count();
    let free_30_day = rows
        .iter()
        .filter(|a| {
            a.window
                .as_ref()
                .is_some_and(|w| w.kind == WindowKind::Free30Day)
        })
        .count();
    let history_truncated = rows
        .iter()
        .any(|a| a.window.as_ref().is_some_and(|w| w.truncated));
    let inputs: Vec<_> = rows
        .iter()
        .map(|a| prepare(a.window.as_ref()))
        .collect::<Result<_, _>>()?;
    let mut times = BTreeSet::from([from, now, to]);
    for p in inputs.iter().flatten() {
        for o in &p.observations {
            if o.at <= now {
                if o.at >= from {
                    times.insert(o.at);
                }
                if let Some(end) = o.valid_until.filter(|end| *end >= from && *end <= now) {
                    times.insert(end);
                }
            }
        }
        for t in p
            .resets
            .iter()
            .chain(p.phases.iter().flat_map(|p| [&p.start, &p.reset]))
        {
            if *t >= from && *t <= to {
                times.insert(*t);
            }
        }
    }
    if times.len() > MAX_KNOTS {
        return Err(Error::Capacity);
    }
    let knots: Vec<_> = times
        .into_iter()
        .map(|at| Knot {
            at,
            left: values(&inputs, at, now, Side::Left),
            right: values(&inputs, at, now, Side::Right),
        })
        .collect();
    let mut gap_spans = Vec::new();
    for pair in knots.windows(2) {
        let [a, b] = pair else { unreachable!() };
        if a.at >= now {
            continue;
        }
        if let (Some(remaining), Some(pace_from), Some(pace_to), Some(end_remaining)) = (
            a.right.remaining.value,
            a.right.pace.value,
            b.left.pace.value,
            b.left.remaining.value,
        ) && remaining == end_remaining
        {
            gap_spans.push(GapSpan {
                from: a.at,
                to: b.at,
                remaining,
                pace_from,
                pace_to,
            });
        }
    }
    let current = values(&inputs, now, now, Side::Right);
    let verdict = current
        .remaining
        .value
        .zip(current.pace.value)
        .map(|(remaining, pace)| {
            let difference = remaining - pace;
            let tenths = (difference.abs() * 10.).round() as u64;
            let direction = if tenths == 0 {
                Direction::OnPace
            } else if difference > 0. {
                Direction::Ahead
            } else {
                Direction::Behind
            };
            Verdict {
                difference,
                direction,
                tenths,
            }
        });
    Ok(Projection {
        provider,
        from,
        now,
        to,
        cohort: rows.len(),
        excluded_paused,
        excluded_disabled,
        excluded_other,
        weekly,
        free_30_day,
        history_truncated,
        knots,
        gap_spans,
        current,
        verdict,
        next_change: next_change(&inputs, now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 23, 12, 0, 0).unwrap()
    }
    fn observation(at: DateTime<Utc>, used: Option<f64>, until: DateTime<Utc>) -> Observation {
        Observation {
            at,
            used_pct: used,
            valid_until: Some(until),
        }
    }
    fn account(
        id: &str,
        plan: &str,
        kind: WindowKind,
        used: f64,
        remaining_hours: i64,
    ) -> AccountInput {
        let duration = Duration::seconds(kind.seconds().unwrap());
        let reset = now() + Duration::hours(remaining_hours);
        AccountInput {
            id: id.into(),
            provider: Provider::Codex,
            state: AccountState::Active,
            plan: plan.into(),
            window: Some(WindowInput {
                kind,
                observations: vec![observation(now() - Duration::hours(1), Some(used), reset)],
                phases: vec![Phase {
                    start: reset - duration,
                    reset,
                }],
                resets: vec![],
                truncated: false,
            }),
        }
    }
    fn output(accounts: &[AccountInput]) -> Projection {
        project(Provider::Codex, accounts, now()).unwrap()
    }
    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-10, "{a} != {b}");
    }

    #[test]
    fn next_change_uses_earliest_contributor_not_superseded_or_excluded_expiry() {
        let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let mut b = account("b", "Plus", WindowKind::Weekly, 30., 80);
        let mut paused = account("paused", "Plus", WindowKind::Weekly, 0., 70);
        paused.state = AccountState::Paused;
        let w = a.window.as_mut().unwrap();
        w.observations[0].valid_until = Some(now() + Duration::seconds(1));
        w.observations
            .push(observation(now(), Some(25.), now() + Duration::seconds(8)));
        b.window.as_mut().unwrap().observations[0].valid_until = Some(now() + Duration::seconds(4));
        paused.window.as_mut().unwrap().observations[0].valid_until =
            Some(now() + Duration::seconds(2));
        assert_eq!(
            output(&[a, b, paused]).next_change,
            Some(now() + Duration::seconds(4))
        );
    }

    #[test]
    fn unequal_plans_staggered_resets_are_unweighted_and_order_independent() {
        let a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let b = account("b", "Max 20x", WindowKind::Weekly, 40., 126);
        let result = output(&[a.clone(), b.clone()]);
        close(result.current.remaining.value.unwrap(), 1.4);
        close(result.current.pace.value.unwrap(), 1.25);
        close(result.verdict.unwrap().difference, 0.15);
        assert_eq!(result.verdict.unwrap().direction, Direction::Ahead);
        assert_eq!(result, output(&[b, a]));
        assert_eq!(result.from, now() - Duration::hours(19));
        assert_eq!(result.to, now() + Duration::hours(5));
    }
    #[test]
    fn excluded_accounts_are_footnoted_and_other_provider_is_separate() {
        let a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let mut paused = a.clone();
        paused.id = "p".into();
        paused.state = AccountState::Paused;
        let mut disabled = a.clone();
        disabled.id = "d".into();
        disabled.state = AccountState::Disabled;
        let mut claude = a.clone();
        claude.id = "c".into();
        claude.provider = Provider::Claude;
        let result = output(&[a, paused, disabled, claude]);
        assert_eq!(
            (
                result.cohort,
                result.excluded_paused,
                result.excluded_disabled
            ),
            (1, 1, 1)
        );
        close(result.current.remaining.value.unwrap(), 0.8);
    }
    #[test]
    fn free_30_day_and_exhausted_account_are_capacity_not_plan_weights() {
        let a = account("a", "Free", WindowKind::Free30Day, 25., 360);
        let b = account("b", "Pro", WindowKind::Weekly, 120., 84);
        let result = output(&[a, b]);
        assert_eq!(
            (result.weekly, result.free_30_day, result.cohort),
            (1, 1, 2)
        );
        close(result.current.remaining.value.unwrap(), 0.75);
        close(result.current.pace.value.unwrap(), 1.);
        assert_eq!(result.verdict.unwrap().direction, Direction::Behind);
    }
    #[test]
    fn missing_and_unsupported_windows_never_become_zero_or_five_hour_aggregate() {
        let a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        for window in [
            None,
            Some(WindowInput {
                kind: WindowKind::Unsupported,
                observations: vec![],
                phases: vec![],
                resets: vec![],
                truncated: false,
            }),
        ] {
            let b = AccountInput {
                id: "b".into(),
                provider: Provider::Codex,
                state: AccountState::Active,
                plan: "5h only".into(),
                window,
            };
            let result = output(&[a.clone(), b]);
            assert_eq!(result.current.remaining.known, 1);
            assert_eq!(result.current.remaining.total, 2);
            assert_eq!(result.current.remaining.value, None);
            assert_eq!(result.verdict, None);
        }
    }
    #[test]
    fn missing_historical_phase_is_a_gap_not_backward_invented_cycles() {
        let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        a.window.as_mut().unwrap().phases.clear();
        let result = output(&[a]);
        close(result.current.remaining.value.unwrap(), 0.8);
        assert_eq!(result.current.pace.value, None);
        assert!(result.gap_spans.is_empty());
        assert_eq!(result.verdict, None);
    }
    #[test]
    fn verified_adjacent_phases_jump_and_old_observation_cannot_cross_reset() {
        let reset = now() - Duration::minutes(30);
        let duration = Duration::days(7);
        let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let w = a.window.as_mut().unwrap();
        w.phases = vec![
            Phase {
                start: reset - duration,
                reset,
            },
            Phase {
                start: reset,
                reset: reset + duration,
            },
        ];
        // Intentionally overlong input bound: the known reset must still cut it.
        w.observations = vec![observation(
            reset - Duration::minutes(10),
            Some(20.),
            now() + Duration::hours(1),
        )];
        let result = output(&[a.clone()]);
        let boundary = result.knots.iter().find(|k| k.at == reset).unwrap();
        close(boundary.left.pace.value.unwrap(), 0.);
        close(boundary.right.pace.value.unwrap(), 1.);
        close(boundary.left.remaining.value.unwrap(), 0.8);
        assert_eq!(boundary.right.remaining.value, None);
        assert_eq!(result.verdict, None);
        assert!(
            !result
                .gap_spans
                .iter()
                .any(|s| s.from < reset && s.to > reset)
        );
        a.window.as_mut().unwrap().observations.push(observation(
            reset,
            Some(10.),
            reset + duration,
        ));
        let result = output(&[a]);
        close(result.current.remaining.value.unwrap(), 0.9);
        assert!(result.verdict.is_some());
    }
    #[test]
    fn staggered_reset_jump_keeps_other_account_phase_and_requires_explicit_next_cycle() {
        let duration = Duration::days(7);
        let first_reset = now() - Duration::minutes(30);
        let second_reset = now() + Duration::hours(2);
        let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let mut b = account("b", "Pro", WindowKind::Weekly, 40., 84);
        let w = a.window.as_mut().unwrap();
        w.phases = vec![
            Phase {
                start: first_reset - duration,
                reset: first_reset,
            },
            Phase {
                start: first_reset,
                reset: first_reset + duration,
            },
        ];
        b.window.as_mut().unwrap().phases = vec![Phase {
            start: second_reset - duration,
            reset: second_reset,
        }];
        let result = output(&[a, b]);
        let first = result.knots.iter().find(|k| k.at == first_reset).unwrap();
        let other =
            (second_reset - first_reset).num_seconds() as f64 / duration.num_seconds() as f64;
        close(first.left.pace.value.unwrap(), other);
        close(first.right.pace.value.unwrap(), other + 1.);
        let second = result.knots.iter().find(|k| k.at == second_reset).unwrap();
        assert!(second.left.pace.value.is_some());
        assert_eq!(second.right.pace.known, 1);
        assert_eq!(second.right.pace.value, None, "no invented next cycle");
    }

    #[test]
    fn reset_marker_without_phase_interrupts_actual_and_does_not_invent_pace() {
        let reset = now() - Duration::minutes(5);
        let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let w = a.window.as_mut().unwrap();
        w.phases.clear();
        w.resets = vec![reset];
        let result = output(&[a]);
        assert_eq!(result.current.remaining.value, None);
        assert_eq!(result.current.pace.value, None);
        assert_eq!(result.verdict, None);
    }
    #[test]
    fn null_invalid_expired_and_future_observations_cannot_create_known_coverage() {
        for bad in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.)] {
            let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
            a.window.as_mut().unwrap().observations.push(observation(
                now() - Duration::minutes(5),
                bad,
                now() + Duration::hours(1),
            ));
            assert_eq!(output(&[a]).verdict, None);
        }
        let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        a.window.as_mut().unwrap().observations[0].valid_until = Some(now());
        assert_eq!(output(&[a.clone()]).current.remaining.value, None);
        a.window.as_mut().unwrap().observations[0].at = now() + Duration::minutes(1);
        assert_eq!(output(&[a]).current.remaining.value, None);
    }
    #[test]
    fn future_actual_is_never_extended_and_phase_gaps_stay_unknown() {
        let a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let result = output(&[a]);
        assert_eq!(result.knots.last().unwrap().right.remaining.value, None);
        assert!(result.gap_spans.iter().all(|s| s.to <= now()));
        let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let start = now() + Duration::hours(1);
        a.window.as_mut().unwrap().phases = vec![Phase {
            start,
            reset: start + Duration::days(7),
        }];
        assert_eq!(output(&[a]).current.pace.value, None);
    }
    #[test]
    fn empty_duplicate_invalid_phase_and_capacity_are_explicit() {
        assert_eq!(output(&[]).verdict, None);
        let a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        assert_eq!(
            project(Provider::Codex, &[a.clone(), a.clone()], now()),
            Err(Error::Identity)
        );
        let mut invalid = a.clone();
        invalid.window.as_mut().unwrap().phases[0].start += Duration::seconds(1);
        assert_eq!(
            project(Provider::Codex, &[invalid], now()),
            Err(Error::InvalidPhase)
        );
        assert_eq!(
            project(Provider::Codex, &vec![a; MAX_ACCOUNTS + 1], now()),
            Err(Error::Capacity)
        );
    }
    #[test]
    fn explicit_null_is_a_real_gap_until_next_observation_and_duplicate_times_refuse() {
        let mut a = account("a", "Plus", WindowKind::Weekly, 20., 84);
        let unknown = now() - Duration::minutes(40);
        let recovered = now() - Duration::minutes(20);
        let w = a.window.as_mut().unwrap();
        w.observations
            .push(observation(unknown, None, now() + Duration::hours(1)));
        w.observations.push(observation(
            recovered,
            Some(30.),
            now() + Duration::hours(1),
        ));
        let result = output(&[a.clone()]);
        let boundary = result.knots.iter().find(|k| k.at == unknown).unwrap();
        close(boundary.left.remaining.value.unwrap(), 0.8);
        assert_eq!(boundary.right.remaining.value, None);
        assert!(
            !result
                .gap_spans
                .iter()
                .any(|s| s.from < recovered && s.to > unknown)
        );
        close(result.current.remaining.value.unwrap(), 0.7);
        let duplicate = a.window.as_ref().unwrap().observations[0].clone();
        a.window.as_mut().unwrap().observations.push(duplicate);
        assert_eq!(
            project(Provider::Codex, &[a], now()),
            Err(Error::DuplicateObservation)
        );
    }

    #[test]
    fn truncated_history_is_visible_and_small_display_gap_is_on_pace() {
        let mut a = account("a", "Plus", WindowKind::Weekly, 49., 84);
        a.window.as_mut().unwrap().truncated = true;
        let result = output(&[a]);
        assert!(result.history_truncated);
        assert_eq!(result.verdict.unwrap().direction, Direction::OnPace);
        assert_eq!(result.verdict.unwrap().tenths, 0);
        close(result.verdict.unwrap().difference, 0.01);
    }

    #[test]
    fn explicit_phase_with_an_interior_verified_reset_is_rejected() {
        let mut a = account("sample", "Plus", WindowKind::Weekly, 20., 84);
        a.window
            .as_mut()
            .unwrap()
            .resets
            .push(now() - Duration::hours(1));
        assert_eq!(
            project(Provider::Codex, &[a], now()),
            Err(Error::InvalidPhase)
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::dashboard_model::{
        self as m, AccountHistory, Clock, DashboardSnapshot, HistoryWindow,
    };
    use serde_json::{Value, json};

    fn clock() -> Clock {
        let now = DateTime::parse_from_rfc3339("2026-10-23T21:00:37.123456789+09:00").unwrap();
        Clock {
            now,
            day_start: now.with_timezone(&Utc) - Duration::hours(12),
            day_end: now.with_timezone(&Utc) + Duration::hours(12),
        }
    }

    fn row(id: &str, provider: &str, plan: &str, used: f64, hours: i64) -> Value {
        json!({
            "id": id, "provider": provider, "label": "Synthetic account", "plan": plan,
            "user_state": "active", "health": {"kind": "ready"},
            "windows": [{
                "name": if plan.eq_ignore_ascii_case("free") { "43200m" } else { "7d" },
                "scope": "account", "used_pct": used,
                "observed_at": clock().utc() - Duration::seconds(30),
                "reset_at": clock().utc() + Duration::hours(hours)
            }]
        })
    }

    fn snapshot(rows: Vec<Value>) -> DashboardSnapshot {
        // Actual JSON -> tray DTO projection, rather than the pure input helper.
        let view = crate::v2::project(&json!({"accounts": rows}).to_string(), clock().utc().into())
            .unwrap();
        let mut snapshot = DashboardSnapshot::new(view);
        snapshot.data_at = Some(clock().utc());
        snapshot
    }

    fn codex(snapshot: &DashboardSnapshot) -> Projection {
        project_snapshot(snapshot, &clock())
            .into_iter()
            .find(|p| p.provider == Provider::Codex)
            .unwrap()
    }

    fn history(
        snapshot: &mut DashboardSnapshot,
        points: Vec<(DateTime<Utc>, Option<f64>)>,
        resets: Vec<DateTime<Utc>>,
    ) {
        snapshot.usage.accounts = vec![AccountHistory {
            id: "sample".into(),
            windows: vec![HistoryWindow {
                name: "7d".into(),
                points,
                resets,
                truncated: false,
            }],
            ..Default::default()
        }];
    }

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-10, "{actual} != {expected}");
    }

    #[test]
    fn snapshot_adapter_staggered_weekly_free30d_and_excluded_footnotes() {
        let mut paused = row("paused", "codex", "Plus", 0., 168);
        paused["user_state"] = json!("paused");
        let mut disabled = paused.clone();
        disabled["id"] = json!("disabled");
        disabled["user_state"] = json!("disabled");
        let mut drained = row("drained", "codex", "Plus", 0., 168);
        drained["drain"] = json!(true);
        let mut unknown = row("unknown-state", "codex", "Plus", 0., 168);
        unknown["user_state"] = json!("future_state");
        let mut data = snapshot(vec![
            row("small", "codex", "Plus", 20., 84),
            row("large", "codex", "Max 20x", 40., 126),
            row("free", "codex", "Free", 25., 360),
            row("other-provider", "claude", "Pro", 50., 168),
            paused,
            disabled,
            drained,
            unknown,
        ]);
        let result = project_snapshot(&data, &clock());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].provider, Provider::Claude);
        let p = &result[1];
        assert_eq!((p.cohort, p.weekly, p.free_30_day), (3, 2, 1));
        assert_eq!(
            (p.excluded_paused, p.excluded_disabled, p.excluded_other),
            (1, 1, 2)
        );
        close(p.current.remaining.value.unwrap(), 2.15);
        close(p.current.pace.value.unwrap(), 1.75);
        close(result[0].current.remaining.value.unwrap(), 0.5);
        assert!(p.verdict.is_some());
        data.accounts.raw.accounts.reverse();
        assert_eq!(project_snapshot(&data, &clock()), result);
    }

    #[test]
    fn snapshot_adapter_interior_reset_keeps_usage_but_refuses_current_phase() {
        let mut data = snapshot(vec![row("sample", "codex", "Plus", 20., 84)]);
        history(&mut data, vec![], vec![clock().utc() - Duration::hours(1)]);
        let p = codex(&data);
        close(p.current.remaining.value.unwrap(), 0.8);
        assert_eq!(p.current.pace.value, None);
        assert_eq!(p.verdict, None);
        assert!(
            p.knots
                .iter()
                .all(|k| k.left.pace.value.is_none() && k.right.pace.value.is_none())
        );
        assert!(p.gap_spans.is_empty());

        // A boundary equal to the inferred start agrees with this cycle.
        data.usage.accounts[0].windows[0].resets = vec![clock().utc() - Duration::hours(84)];
        let control = codex(&data);
        close(control.current.pace.value.unwrap(), 0.5);
        assert!(control.verdict.is_some());
    }

    #[test]
    fn snapshot_adapter_history_null_freshness_and_reset_cut_coverage() {
        let now = clock().utc();
        let null = now - Duration::minutes(9);
        let reset = now - Duration::seconds(390);
        let mut data = snapshot(vec![row("sample", "codex", "Plus", 20., 84)]);
        history(
            &mut data,
            vec![
                (now - Duration::minutes(10), Some(10.)),
                (null, None),
                (now - Duration::minutes(7), Some(30.)),
            ],
            vec![reset],
        );
        let p = codex(&data);
        let gap = p.knots.iter().find(|k| k.at == null).unwrap();
        close(gap.left.remaining.value.unwrap(), 0.9);
        assert_eq!(gap.right.remaining.value, None);
        let boundary = p.knots.iter().find(|k| k.at == reset).unwrap();
        close(boundary.left.remaining.value.unwrap(), 0.7);
        assert_eq!(boundary.right.remaining.value, None);
        let expired = p
            .knots
            .iter()
            .find(|k| k.at == now - Duration::minutes(5))
            .unwrap();
        assert_eq!(expired.right.remaining.value, None);
        close(p.current.remaining.value.unwrap(), 0.8);
        assert!(
            p.knots
                .iter()
                .filter(|k| k.at > now)
                .all(|k| k.right.remaining.value.is_none())
        );
    }

    #[test]
    fn snapshot_adapter_freshness_uses_each_source_timestamp_not_publication() {
        let mut current = row("sample", "codex", "Plus", 20., 84);
        current["windows"][0]["observed_at"] = json!(clock().utc() - Duration::seconds(299));
        let mut data = snapshot(vec![current]);
        close(codex(&data).current.remaining.value.unwrap(), 0.8);
        let mut later = clock();
        later.now += Duration::seconds(1);
        data.data_at = Some(later.utc());
        let result = project_snapshot(&data, &later);
        assert_eq!(result[1].current.remaining.value, None);
        assert_eq!(result[1].verdict, None);

        data.accounts.raw.accounts[0].windows[0].observed_at = None;
        history(
            &mut data,
            vec![(clock().utc() - Duration::seconds(301), Some(50.))],
            vec![],
        );
        assert_eq!(codex(&data).current.remaining.value, None);
        data.usage.accounts[0].windows[0].points[0].0 = clock().utc() - Duration::seconds(299);
        close(codex(&data).current.remaining.value.unwrap(), 0.5);
        data.usage.accounts[0].windows[0].points[0].0 = clock().utc() + Duration::seconds(1);
        assert_eq!(codex(&data).current.remaining.value, None);
    }

    #[test]
    fn snapshot_adapter_partial_or_missing_long_window_has_no_provider_total() {
        let mut short = row("short-only", "codex", "Plus", 1., 2);
        short["windows"][0]["name"] = json!("5h");
        let data = snapshot(vec![row("measured", "codex", "Plus", 20., 84), short]);
        let p = codex(&data);
        assert_eq!(
            (p.current.remaining.known, p.current.remaining.total),
            (1, 2)
        );
        assert_eq!(p.current.remaining.value, None);
        assert_eq!(p.current.pace.value, None);
        assert_eq!(p.verdict, None);
        assert!(p.gap_spans.is_empty());
    }

    #[test]
    fn snapshot_adapter_missing_current_forecast_and_other_rates_cannot_date_weekly_usage() {
        let mut current = row("sample", "codex", "Plus", 20., 84);
        current["windows"][0]["observed_at"] = Value::Null;
        current["observed_at"] = json!(clock().utc());
        let other = json!({
            "as_of": clock().utc(), "observed_at": clock().utc(),
            "windows": [{
                "name": "5h", "window_seconds": 18000, "observed_at": clock().utc(),
                "used_pct": 99., "reset_at": clock().utc() + Duration::hours(2),
                "awaiting_usage": false, "recent_pct_per_hour": 100., "average_pct_per_hour": 50.,
                "recent_span_start": null, "measurement_start": null, "basis": "recent",
                "exhaustion_at": clock().utc() + Duration::seconds(36),
                "seconds_to_exhaustion": 36, "outcome": "exhaustion", "changing": true,
                "secondary_recent": null, "truncated": false
            }], "earliest_window": "5h", "credit": null, "truncated": false
        });
        for forecast in [Value::Null, other] {
            current["usage_forecast"] = forecast;
            let data = snapshot(vec![current.clone()]);
            let p = codex(&data);
            assert_eq!(p.current.remaining.value, None);
            assert_eq!(p.current.pace.value, None);
            assert_eq!(p.verdict, None);
        }
        // Fresh measured quota does not require a rate estimate to exist.
        current["usage_forecast"] = Value::Null;
        current["windows"][0]["observed_at"] = json!(clock().utc() - Duration::seconds(30));
        close(
            codex(&snapshot(vec![current]))
                .current
                .remaining
                .value
                .unwrap(),
            0.8,
        );
    }

    #[test]
    fn snapshot_adapter_secondary_requires_duration_and_model_scope_never_aggregates() {
        let mut current = row("sample", "codex", "Plus", 20., 84);
        current["windows"][0]["name"] = json!("secondary");
        assert_eq!(
            codex(&snapshot(vec![current.clone()]))
                .current
                .remaining
                .value,
            None
        );
        current["windows"][0]["window_seconds"] = json!(604800);
        close(
            codex(&snapshot(vec![current.clone()]))
                .current
                .remaining
                .value
                .unwrap(),
            0.8,
        );
        current["windows"][0]["window_seconds"] = json!(18000);
        assert_eq!(
            codex(&snapshot(vec![current.clone()]))
                .current
                .remaining
                .value,
            None
        );
        current["windows"][0]["name"] = json!("7d");
        current["windows"][0]["scope"] = json!("model");
        assert_eq!(
            codex(&snapshot(vec![current.clone()]))
                .current
                .remaining
                .value,
            None
        );
        current["windows"][0]["name"] = json!("43200m");
        current["windows"][0]["scope"] = json!("account");
        assert_eq!(
            codex(&snapshot(vec![current])).current.remaining.value,
            None
        );
    }

    #[test]
    fn snapshot_adapter_empty_invalid_and_over_capacity_keep_two_unknown_panels() {
        let empty = project_snapshot(&snapshot(vec![]), &clock());
        assert_eq!(empty.len(), 2);
        assert!(
            empty
                .iter()
                .all(|p| p.cohort == 0 && p.current.remaining.value.is_none())
        );
        let current = row("sample", "codex", "Plus", 20., 84);
        let mut data = snapshot(vec![current.clone(), current]);
        let duplicate = project_snapshot(&data, &clock());
        assert_eq!(duplicate.len(), 2);
        assert!(duplicate.iter().all(|p| p.current.remaining.value.is_none()
            && p.verdict.is_none()
            && p.history_truncated));
        data.accounts.raw.accounts = (0..=MAX_ACCOUNTS)
            .map(|i| {
                let mut account = data.accounts.raw.accounts[0].clone();
                account.id = format!("synthetic-{i}");
                account
            })
            .collect();
        let capped = project_snapshot(&data, &clock());
        assert_eq!(capped.len(), 2);
        assert!(
            capped
                .iter()
                .all(|p| p.current.remaining.value.is_none() && p.verdict.is_none())
        );
    }

    fn assert_chart_deadline(data: &DashboardSnapshot, at: &Clock, deadline: DateTime<Utc>) {
        let mut state = crate::state::DashboardState {
            tab: crate::state::Tab::Usage,
            ..Default::default()
        };
        let view = m::project(data, &mut state, at);
        let scene = crate::geometry::Scene::build(
            &view,
            &state,
            [1320., 900.],
            &crate::geometry::Metrics::default(),
        );
        let chart = scene
            .charts
            .iter()
            .find(|c| c.id == "provider-usage:codex")
            .unwrap();
        assert!(chart.rect.bottom > 0. && chart.rect.top < 900.);
        assert!(
            chart
                .series
                .iter()
                .any(|s| s.future_resets.contains(&deadline)),
            "existing scanner must receive the exact discrete deadline"
        );
    }

    #[test]
    fn snapshot_deadline_reset_known_to_unknown_without_snapshot_update() {
        let mut current = row("sample", "codex", "Plus", 20., 84);
        let deadline = clock().utc() + Duration::seconds(1);
        current["windows"][0]["reset_at"] = json!(deadline);
        let data = snapshot(vec![current]);
        let before = codex(&data);
        assert_eq!(before.next_change, Some(deadline));
        assert_chart_deadline(&data, &clock(), deadline);
        assert!(before.current.remaining.value.is_some());
        assert!(before.current.pace.value.is_some());
        assert!(before.verdict.is_some());
        let mut at = clock();
        at.now += Duration::seconds(1);
        let after = project_snapshot(&data, &at);
        assert_eq!(after[1].current.remaining.value, None);
        assert_eq!(after[1].current.pace.value, None);
        assert_eq!(after[1].verdict, None);
        assert_eq!(after[1].next_change, None);
    }

    #[test]
    fn snapshot_deadline_freshness_299_to_300_without_snapshot_update() {
        let mut current = row("sample", "codex", "Plus", 20., 84);
        current["windows"][0]["observed_at"] = json!(clock().utc() - Duration::seconds(299));
        let data = snapshot(vec![current]);
        let before = codex(&data);
        assert_eq!(
            before.next_change,
            Some(clock().utc() + Duration::seconds(1))
        );
        assert_chart_deadline(&data, &clock(), clock().utc() + Duration::seconds(1));
        assert!(before.current.remaining.value.is_some());
        assert!(before.verdict.is_some());
        let mut at = clock();
        at.now += Duration::seconds(1);
        let after = project_snapshot(&data, &at);
        assert_eq!(after[1].current.remaining.value, None);
        assert_eq!(after[1].verdict, None);
        assert!(after[1].current.pace.value.is_some());
        assert_eq!(
            after[1].next_change,
            Some(clock().utc() + Duration::hours(84))
        );
    }

    #[test]
    fn snapshot_adapter_local_minute_domain_keeps_actual_deadline_clock() {
        let mut current = row("sample", "codex", "Plus", 20., 84);
        current["windows"][0]["reset_at"] = json!(clock().utc() + Duration::seconds(1));
        let data = snapshot(vec![current]);
        let mut local = clock();
        // Also exercise a fixed offset containing seconds, not just UTC rounding.
        local.now = local
            .now
            .with_timezone(&chrono::FixedOffset::east_opt(9 * 3600 + 30).unwrap());
        for elapsed in [0, 2] {
            let mut at = local.clone();
            at.now += Duration::seconds(elapsed);
            let result = project_snapshot(&data, &at);
            for p in &result {
                assert_eq!((p.from, p.to), m::usage_domain(&at));
                assert_eq!(p.now, at.utc());
                assert_ne!(p.now, p.from + Duration::hours(19));
            }
            if elapsed == 0 {
                close(result[1].current.remaining.value.unwrap(), 0.8);
            } else {
                assert_eq!(result[1].current.remaining.value, None);
                assert_eq!(result[1].current.pace.value, None);
            }
        }
    }

    #[test]
    fn snapshot_adapter_historical_phases_need_both_verified_boundaries() {
        let now = clock().utc();
        let end = now - Duration::hours(1);
        let mut current = row("sample", "codex", "Plus", 20., 84);
        current["windows"][0]["observed_at"] = Value::Null;
        let mut data = snapshot(vec![current]);
        history(&mut data, vec![], vec![end - Duration::days(7), end]);
        let p = codex(&data);
        let boundary = p.knots.iter().find(|k| k.at == end).unwrap();
        close(boundary.left.pace.value.unwrap(), 0.);
        assert_eq!(boundary.right.pace.value, None);
        assert_eq!(p.current.pace.value, None);
        data.usage.accounts[0].windows[0].resets.remove(0);
        assert!(
            codex(&data)
                .knots
                .iter()
                .all(|k| k.right.pace.value.is_none())
        );
    }
}
