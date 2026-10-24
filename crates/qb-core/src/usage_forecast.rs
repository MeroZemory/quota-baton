//! Display-only, restart-cold usage estimates from actual reported observations.
//!
//! Two observed steps conservatively means two positive usage increments (or
//! credit decreases), not two polls. Equal values contribute wall-clock coverage
//! but no steps. The last value is held through `now`; no activity is invented.
//! Callers supply fresh observations, and gate credit display on permitted live
//! serving. Nothing here participates in routing, health, or shadow forecasts.
//! Integer reset countdowns have receipt-time jitter: deadlines within one
//! second of a still-future stored reset share its original, stable identity.
//! This display-only tolerance never suppresses a value drop or a passed reset.

use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Duration, Utc};

const MAX_ACCOUNTS: usize = 128;
const MAX_WINDOWS_PER_ACCOUNT: usize = 16;
const MAX_SAMPLES_PER_SERIES: usize = 256;
const RECENT_SECONDS: i64 = 30 * 60;
const MIN_SECONDS: f64 = 10.0 * 60.0;

// One shared wire schema; all numerical estimation stays in this module.
pub use qb_proto::admin_account::{ForecastBasis, ForecastOutcome};
pub type UsageProjection = qb_proto::admin_account::UsageProjection<DateTime<Utc>>;
pub type WindowUsageForecast = qb_proto::admin_account::WindowUsageForecast<DateTime<Utc>>;
pub type CreditUsageForecast = qb_proto::admin_account::CreditUsageForecast<DateTime<Utc>>;
pub type AccountUsageForecast = qb_proto::admin_account::AccountUsageForecast<DateTime<Utc>>;

#[derive(Clone, Debug)]
struct Sample {
    at: DateTime<Utc>,
    value: f64,
}

#[derive(Clone, Debug)]
struct Track {
    samples: VecDeque<Sample>,
    // Eligibility is based on actual observations, never coalesced point pairs.
    positive_steps: VecDeque<DateTime<Utc>>,
    segment_start: DateTime<Utc>,
    truncated: bool,
}

impl Track {
    fn new(at: DateTime<Utc>, value: f64) -> Self {
        Self {
            samples: VecDeque::from([Sample { at, value }]),
            positive_steps: VecDeque::new(),
            segment_start: at,
            truncated: false,
        }
    }

    fn last(&self) -> &Sample {
        // Tracks are created with one sample and never emptied.
        self.samples.back().expect("nonempty observation track")
    }

    fn push(&mut self, at: DateTime<Utc>, value: f64, credit: bool) {
        let positive = if credit {
            value < self.last().value
        } else {
            value > self.last().value
        };
        if positive {
            self.positive_steps.push_back(at);
            while self.positive_steps.len() > 2 {
                self.positive_steps.pop_front();
            }
        }
        self.samples.push_back(Sample { at, value });
        if let Some(cutoff) = at.checked_sub_signed(Duration::seconds(RECENT_SECONDS)) {
            // Retain the predecessor, even when it is outside the 30m span.
            while self.samples.len() > 1 && self.samples[1].at <= cutoff {
                self.samples.pop_front();
            }
        }
        while self.samples.len() > MAX_SAMPLES_PER_SERIES {
            // Coalesce the densest interior interval, preserving the oldest
            // predecessor and latest raw value. Ties choose the earliest point.
            // Interpolation is approximate after compaction, but never loses
            // wall-clock coverage or actual positive-step evidence.
            let interior = (1..self.samples.len() - 1)
                .min_by_key(|&i| {
                    self.samples[i + 1]
                        .at
                        .signed_duration_since(self.samples[i - 1].at)
                })
                .expect("sample cap leaves interior points");
            self.samples.remove(interior);
            self.truncated = true;
        }
    }

    fn recent(&self, now: DateTime<Utc>, credit: bool) -> Option<(f64, DateTime<Utc>)> {
        let last = self.last();
        if now < last.at {
            return None;
        }
        let start = self
            .segment_start
            .max(now.checked_sub_signed(Duration::seconds(RECENT_SECONDS))?);
        let span = seconds_between(start, now)?;
        if span < MIN_SECONDS || self.samples.front()?.at > start {
            return None;
        }
        if self.positive_steps.len() < 2 || self.positive_steps[0] <= start {
            return None;
        }
        let mut baseline = None;
        let mut previous: Option<&Sample> = None;
        for sample in &self.samples {
            if sample.at <= start {
                baseline = Some(sample.value);
            }
            if let Some(prev) = previous
                && prev.at < start
                && sample.at > start
            {
                let fraction =
                    seconds_between(prev.at, start)? / seconds_between(prev.at, sample.at)?;
                baseline = Some(prev.value + (sample.value - prev.value) * fraction);
            }
            previous = Some(sample);
        }
        let baseline = baseline?;
        let delta = if credit {
            baseline - last.value
        } else {
            last.value - baseline
        };
        let rate = delta / span * 3600.0;
        (rate.is_finite() && rate >= 0.0).then_some((rate, start))
    }
}

#[derive(Clone, Debug)]
struct WindowState {
    window_seconds: i64,
    reset_at: Option<DateTime<Utc>>,
    last_event_at: DateTime<Utc>,
    awaiting_usage: bool,
    // Reset-only reports may precede the first actual used-percent reading.
    track: Option<Track>,
}

#[derive(Clone, Debug)]
struct AccountState {
    windows: BTreeMap<String, WindowState>,
    credit: Option<Track>,
    last_observed: DateTime<Utc>,
    truncated: bool,
}

impl AccountState {
    fn make_window_room(&mut self, name: &str) {
        if !self.windows.contains_key(name) && self.windows.len() >= MAX_WINDOWS_PER_ACCOUNT {
            let oldest = self
                .windows
                .iter()
                .min_by_key(|(_, window)| window.last_event_at)
                .map(|(name, _)| name.clone());
            if let Some(oldest) = oldest {
                self.windows.remove(&oldest);
                self.truncated = true;
            }
        }
    }
}

/// Bounds are 128 accounts, 16 windows/account and 256 samples/series. Oldest
/// accounts/windows are evicted deterministically and restart cold on return.
#[derive(Clone, Debug, Default)]
pub struct Forecasts {
    accounts: BTreeMap<String, AccountState>,
}

impl Forecasts {
    pub fn observe(
        &mut self,
        id: &str,
        name: &str,
        used_pct: f64,
        reset_at: Option<DateTime<Utc>>,
        observed_at: DateTime<Utc>,
    ) {
        let Some(window_seconds) = window_seconds(name) else {
            return;
        };
        // Over-limit reports are exhausted; negative/non-finite reports are invalid.
        if !valid_id(id) || !used_pct.is_finite() || used_pct < 0.0 {
            return;
        }
        let used_pct = used_pct.min(100.0);
        let account = self.account(id, observed_at);
        account.make_window_room(name);
        if let Some(window) = account.windows.get_mut(name) {
            if observed_at <= window.last_event_at {
                return;
            }
            let next_reset = stable_reset(window.reset_at, reset_at, observed_at);
            let reset_changed =
                matches!((window.reset_at, next_reset), (Some(a), Some(b)) if a != b);
            let reset_passed = window.reset_at.is_some_and(|reset| observed_at >= reset);
            if window.awaiting_usage
                || reset_changed
                || reset_passed
                || window
                    .track
                    .as_ref()
                    .is_none_or(|track| used_pct < track.last().value)
            {
                window.track = Some(Track::new(observed_at, used_pct));
            } else {
                window
                    .track
                    .as_mut()
                    .expect("existing observation track")
                    .push(observed_at, used_pct, false);
            }
            window.reset_at = next_reset;
            window.last_event_at = observed_at;
            window.awaiting_usage = false;
        } else {
            account.windows.insert(
                name.to_owned(),
                WindowState {
                    window_seconds,
                    reset_at,
                    last_event_at: observed_at,
                    awaiting_usage: false,
                    track: Some(Track::new(observed_at, used_pct)),
                },
            );
        }
        account.last_observed = account.last_observed.max(observed_at);
    }

    /// Fresh reset metadata without a usage reading. A changed known reset
    /// invalidates estimates until fresh usage arrives; it never creates a step.
    /// Call instead of `observe` when the report has a reset but no used percent.
    pub fn observe_reset(
        &mut self,
        id: &str,
        name: &str,
        reset_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
    ) {
        let Some(window_seconds) = window_seconds(name) else {
            return;
        };
        if !valid_id(id) {
            return;
        }
        let account = self.account(id, observed_at);
        account.make_window_room(name);
        let window = account
            .windows
            .entry(name.to_owned())
            .or_insert_with(|| WindowState {
                window_seconds,
                reset_at: None,
                last_event_at: observed_at,
                awaiting_usage: false,
                track: None,
            });
        // A newly inserted metadata-only window has no previous event.
        if window.track.is_none() && window.reset_at.is_none() {
            window.reset_at = Some(reset_at);
            account.last_observed = account.last_observed.max(observed_at);
            return;
        }
        if observed_at <= window.last_event_at {
            return;
        }
        let next_reset = stable_reset(window.reset_at, Some(reset_at), observed_at);
        if window
            .reset_at
            .is_some_and(|old| Some(old) != next_reset || old <= observed_at)
        {
            window.awaiting_usage = true;
        }
        window.reset_at = next_reset;
        window.last_event_at = observed_at;
        account.last_observed = account.last_observed.max(observed_at);
    }

    pub fn observe_credit(&mut self, id: &str, balance: f64, observed_at: DateTime<Utc>) {
        if !valid_id(id) || !balance.is_finite() || balance < 0.0 {
            return;
        }
        let account = self.account(id, observed_at);
        match account.credit.as_mut() {
            Some(track) => {
                let last = track.last();
                if observed_at <= last.at {
                    return;
                }
                if balance > last.value {
                    *track = Track::new(observed_at, balance);
                } else {
                    track.push(observed_at, balance, true);
                }
            }
            None => account.credit = Some(Track::new(observed_at, balance)),
        }
        account.last_observed = account.last_observed.max(observed_at);
    }

    fn account(&mut self, id: &str, observed_at: DateTime<Utc>) -> &mut AccountState {
        if !self.accounts.contains_key(id) && self.accounts.len() >= MAX_ACCOUNTS {
            let oldest = self
                .accounts
                .iter()
                .min_by_key(|(_, account)| account.last_observed)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                self.accounts.remove(&oldest);
            }
        }
        self.accounts
            .entry(id.to_owned())
            .or_insert_with(|| AccountState {
                windows: BTreeMap::new(),
                credit: None,
                last_observed: observed_at,
                truncated: false,
            })
    }

    pub fn view(
        &self,
        id: &str,
        now: DateTime<Utc>,
        last_bound_count_change: Option<DateTime<Utc>>,
    ) -> AccountUsageForecast {
        let mut result = AccountUsageForecast {
            as_of: now,
            observed_at: None,
            windows: Vec::new(),
            earliest_window: None,
            credit: None,
            truncated: false,
        };
        let Some(account) = self.accounts.get(id) else {
            return result;
        };
        for (name, state) in &account.windows {
            if let Some(track) = state.track.as_ref().filter(|track| track.last().at <= now) {
                result.windows.push(window_view(
                    name,
                    state,
                    track,
                    now,
                    last_bound_count_change,
                ));
            }
        }
        result.earliest_window = result.windows.iter().filter(|window| {
            matches!(window.outcome, ForecastOutcome::Exhaustion | ForecastOutcome::Exhausted)
                && matches!((window.exhaustion_at, window.reset_at), (Some(at), Some(reset)) if at < reset)
        }).min_by_key(|window| window.exhaustion_at).map(|window| window.name.clone());
        result.credit = account
            .credit
            .as_ref()
            .filter(|track| track.last().at <= now)
            .map(|track| credit_view(track, now, last_bound_count_change));
        result.observed_at = result
            .windows
            .iter()
            .map(|window| window.observed_at)
            .chain(result.credit.iter().map(|credit| credit.observed_at))
            .max();
        result.truncated = account.truncated
            || result.windows.iter().any(|window| window.truncated)
            || result
                .credit
                .as_ref()
                .is_some_and(|credit| credit.truncated);
        result
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256
}

fn window_seconds(name: &str) -> Option<i64> {
    if name.len() < 2 || name.len() > 16 {
        return None;
    }
    let multiplier = match name.as_bytes().last()? {
        b's' => 1,
        b'm' => 60,
        b'h' => 3600,
        b'd' => 86400,
        _ => return None,
    };
    let digits = &name[..name.len() - 1];
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds = digits.parse::<i64>().ok()?.checked_mul(multiplier)?;
    (seconds > 0).then_some(seconds)
}

fn seconds_between(start: DateTime<Utc>, end: DateTime<Utc>) -> Option<f64> {
    end.signed_duration_since(start)
        .to_std()
        .ok()
        .map(|duration| duration.as_secs_f64())
}

fn stable_reset(
    previous: Option<DateTime<Utc>>,
    reported: Option<DateTime<Utc>>,
    observed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match (previous, reported) {
        (Some(old), Some(new))
            if old > observed_at
                && new.signed_duration_since(old).abs() <= Duration::seconds(1) =>
        {
            Some(old)
        }
        (_, Some(new)) => Some(new),
        (Some(old), None) if old > observed_at => Some(old),
        _ => None,
    }
}

fn changing(start: DateTime<Utc>, now: DateTime<Utc>, change: Option<DateTime<Utc>>) -> bool {
    change.is_some_and(|change| start <= change && change <= now)
}

fn projection(
    remaining: f64,
    rate: Option<f64>,
    now: DateTime<Utc>,
    reset: Option<DateTime<Utc>>,
) -> (ForecastOutcome, Option<DateTime<Utc>>, Option<i64>) {
    if reset.is_some_and(|reset| reset <= now) {
        return (ForecastOutcome::ResetPending, None, None);
    }
    if remaining <= 0.0 {
        return (ForecastOutcome::Exhausted, Some(now), Some(0));
    }
    let Some(rate) = rate else {
        return (ForecastOutcome::Measuring, None, None);
    };
    if !rate.is_finite() || rate <= 0.0 {
        return (ForecastOutcome::NoFiniteForecast, None, None);
    }
    let seconds = (remaining / rate * 3600.0).ceil().max(1.0);
    // A positive rate can overflow the representable duration yet still prove
    // that a known, finite reset precedes exhaustion.
    if reset.is_some_and(|reset| {
        seconds_between(now, reset).is_some_and(|until_reset| seconds >= until_reset)
    }) {
        return (ForecastOutcome::NoExhaustionBeforeReset, None, None);
    }
    if !seconds.is_finite() || seconds >= i64::MAX as f64 {
        return (ForecastOutcome::NoFiniteForecast, None, None);
    }
    let seconds = seconds as i64;
    let at = Duration::try_seconds(seconds).and_then(|duration| now.checked_add_signed(duration));
    let Some(at) = at else {
        return (ForecastOutcome::NoFiniteForecast, None, None);
    };
    if reset.is_some_and(|reset| at >= reset) {
        return (ForecastOutcome::NoExhaustionBeforeReset, None, None);
    }
    (ForecastOutcome::Exhaustion, Some(at), Some(seconds))
}

fn window_view(
    name: &str,
    state: &WindowState,
    track: &Track,
    now: DateTime<Utc>,
    change: Option<DateTime<Utc>>,
) -> WindowUsageForecast {
    let last = track.last();
    let reset_pending = state.reset_at.is_some_and(|reset| reset <= now);
    let recent = (!reset_pending && !state.awaiting_usage)
        .then(|| track.recent(now, false))
        .flatten();
    let average = state
        .reset_at
        .filter(|reset| *reset > now && !state.awaiting_usage)
        .and_then(|reset| {
            Duration::try_seconds(state.window_seconds)
                .and_then(|duration| reset.checked_sub_signed(duration))
        })
        .and_then(|start| {
            let elapsed = seconds_between(start, now)?;
            let rate = last.value / elapsed * 3600.0;
            (elapsed > 0.0 && rate.is_finite()).then_some((rate, start))
        });
    let weekly = state.window_seconds == 7 * 86400;
    let chosen = if weekly {
        average.map(|(rate, start)| (ForecastBasis::Average, rate, start))
    } else {
        recent
            .map(|(rate, start)| (ForecastBasis::Recent, rate, start))
            .or_else(|| average.map(|(rate, start)| (ForecastBasis::Average, rate, start)))
    };
    let (outcome, exhaustion_at, seconds_to_exhaustion) = if state.awaiting_usage {
        (
            if reset_pending {
                ForecastOutcome::ResetPending
            } else {
                ForecastOutcome::Measuring
            },
            None,
            None,
        )
    } else {
        projection(
            100.0 - last.value,
            chosen.map(|(_, rate, _)| rate),
            now,
            state.reset_at,
        )
    };
    let secondary_recent = if weekly {
        recent.map(|(rate, start)| {
            let (outcome, exhaustion_at, seconds_to_exhaustion) =
                projection(100.0 - last.value, Some(rate), now, state.reset_at);
            UsageProjection {
                pct_per_hour: rate,
                measurement_start: start,
                exhaustion_at,
                seconds_to_exhaustion,
                outcome,
                changing: changing(start, now, change),
            }
        })
    } else {
        None
    };
    WindowUsageForecast {
        name: name.to_owned(),
        window_seconds: state.window_seconds,
        observed_at: last.at,
        used_pct: last.value,
        reset_at: state.reset_at,
        awaiting_usage: state.awaiting_usage,
        recent_pct_per_hour: recent.map(|(rate, _)| rate),
        average_pct_per_hour: average.map(|(rate, _)| rate),
        recent_span_start: recent.map(|(_, start)| start),
        measurement_start: chosen.map(|(_, _, start)| start),
        basis: chosen.map(|(basis, _, _)| basis),
        exhaustion_at,
        seconds_to_exhaustion,
        outcome,
        changing: chosen.is_some_and(|(_, _, start)| changing(start, now, change)),
        secondary_recent,
        truncated: track.truncated,
    }
}

fn credit_view(
    track: &Track,
    now: DateTime<Utc>,
    change: Option<DateTime<Utc>>,
) -> CreditUsageForecast {
    let recent = track.recent(now, true);
    let (outcome, exhaustion_at, seconds_to_exhaustion) =
        projection(track.last().value, recent.map(|(rate, _)| rate), now, None);
    CreditUsageForecast {
        observed_at: track.last().at,
        balance: track.last().value,
        recent_credits_per_hour: recent.map(|(rate, _)| rate),
        measurement_start: recent.map(|(_, start)| start),
        exhaustion_at,
        seconds_to_exhaustion,
        outcome,
        changing: recent.is_some_and(|(_, start)| changing(start, now, change)),
        truncated: track.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + minute * 60, 0).unwrap()
    }

    fn observe(forecasts: &mut Forecasts, name: &str, minute: i64, used: f64) {
        forecasts.observe("a", name, used, Some(at(240)), at(minute));
    }

    fn window(forecasts: &Forecasts, minute: i64) -> WindowUsageForecast {
        forecasts.view("a", at(minute), None).windows.remove(0)
    }

    fn near(actual: Option<f64>, expected: f64) {
        assert!(
            (actual.unwrap() - expected).abs() < 1e-8,
            "{actual:?} != {expected}"
        );
    }

    #[test]
    fn r13_forecast_steady_interpolated_and_idle_wall_clock() {
        let mut f = Forecasts::default();
        for (minute, used) in [(0, 10.0), (10, 20.0), (20, 30.0), (30, 40.0), (40, 50.0)] {
            observe(&mut f, "5h", minute, used);
        }
        near(window(&f, 40).recent_pct_per_hour, 60.0);
        near(window(&f, 45).recent_pct_per_hour, 50.0);
        assert_eq!(window(&f, 45).recent_span_start, Some(at(15)));
        near(window(&f, 50).recent_pct_per_hour, 40.0);
        assert_eq!(window(&f, 60).recent_pct_per_hour, None);
    }

    #[test]
    fn r13_forecast_quantization_requires_two_positive_steps_and_ten_minutes() {
        let mut f = Forecasts::default();
        for (minute, used) in [(0, 10.0), (2, 10.0), (4, 11.0), (6, 11.0), (9, 12.0)] {
            observe(&mut f, "5h", minute, used);
        }
        assert_eq!(window(&f, 9).recent_pct_per_hour, None);
        assert_eq!(
            f.view("a", at(0) + Duration::seconds(599), None).windows[0].recent_pct_per_hour,
            None
        );
        near(window(&f, 10).recent_pct_per_hour, 12.0);
        observe(&mut f, "5h", 10, 12.0);
        near(window(&f, 10).recent_pct_per_hour, 12.0);
        let mut one = Forecasts::default();
        for (minute, used) in [(0, 10.0), (5, 11.0), (10, 11.0), (20, 11.0)] {
            observe(&mut one, "5h", minute, used);
        }
        assert_eq!(window(&one, 20).recent_pct_per_hour, None);
        assert_eq!(window(&one, 20).basis, Some(ForecastBasis::Average));
    }

    #[test]
    fn r13_forecast_reset_drop_identity_and_unknown_to_known() {
        let mut f = Forecasts::default();
        for (minute, used) in [(0, 10.0), (5, 11.0), (10, 12.0)] {
            f.observe("a", "5h", used, None, at(minute));
        }
        observe(&mut f, "5h", 15, 13.0);
        assert!(window(&f, 15).recent_pct_per_hour.is_some());
        f.observe("a", "5h", 13.0, Some(at(300)), at(16));
        assert_eq!(window(&f, 16).recent_pct_per_hour, None);
        f.observe("a", "5h", 14.0, Some(at(300)), at(21));
        f.observe("a", "5h", 15.0, Some(at(300)), at(26));
        near(window(&f, 26).recent_pct_per_hour, 12.0);
        f.observe("a", "5h", 1.0, Some(at(300)), at(27));
        assert_eq!(window(&f, 27).recent_pct_per_hour, None);
    }

    #[test]
    fn r13_forecast_restart_replay_and_passed_reset() {
        let mut f = Forecasts::default();
        for (minute, used) in [(0, 10.0), (5, 11.0), (10, 12.0)] {
            f.observe("a", "5h", used, Some(at(20)), at(minute));
        }
        let before = f.view("a", at(10), None);
        f.observe("a", "5h", 90.0, Some(at(200)), at(10));
        f.observe("a", "5h", 99.0, Some(at(200)), at(9));
        assert_eq!(before, f.view("a", at(10), None));
        assert_eq!(window(&f, 20).outcome, ForecastOutcome::ResetPending);
        assert_eq!(window(&f, 20).recent_pct_per_hour, None);
        f.observe("a", "5h", 13.0, Some(at(40)), at(21));
        assert_eq!(window(&f, 21).recent_pct_per_hour, None);
        let cold = Forecasts::default();
        assert!(cold.view("a", at(21), None).windows.is_empty());
    }

    #[test]
    fn r13_forecast_weekly_average_primary_and_session_confidence() {
        let mut f = Forecasts::default();
        let reset = at(7 * 24 * 60 - 60);
        for (minute, used) in [(0, 20.0), (5, 21.0), (10, 22.0)] {
            f.observe("a", "7d", used, Some(reset), at(minute));
        }
        let view = f.view("a", at(10), Some(at(-30)));
        let w = &view.windows[0];
        near(w.average_pct_per_hour, 22.0 * 60.0 / 70.0);
        near(w.recent_pct_per_hour, 12.0);
        assert_eq!(w.basis, Some(ForecastBasis::Average));
        assert!(w.changing);
        assert!(!w.secondary_recent.as_ref().unwrap().changing);
        assert!(!f.view("a", at(10), Some(at(11))).windows[0].changing);
        assert!(
            f.view("a", at(10), Some(at(5))).windows[0]
                .secondary_recent
                .as_ref()
                .unwrap()
                .changing
        );
    }

    #[test]
    fn r13_forecast_projection_reset_boundary_exhausted_and_earliest() {
        assert_eq!(
            projection(10.0, Some(60.0), at(0), Some(at(10))).0,
            ForecastOutcome::NoExhaustionBeforeReset
        );
        assert_eq!(
            projection(10.0, Some(0.0), at(0), None).0,
            ForecastOutcome::NoFiniteForecast
        );
        assert_eq!(
            projection(10.0, Some(f64::MIN_POSITIVE), at(0), None).0,
            ForecastOutcome::NoFiniteForecast
        );
        let mut f = Forecasts::default();
        for (minute, used) in [(0, 80.0), (5, 85.0), (10, 90.0)] {
            observe(&mut f, "5h", minute, used);
            observe(&mut f, "30d", minute, used - 10.0);
        }
        let view = f.view("a", at(10), None);
        assert_eq!(view.earliest_window.as_deref(), Some("5h"));
        let w = view.windows.iter().find(|w| w.name == "5h").unwrap();
        assert_eq!(w.exhaustion_at, Some(at(20)));
        observe(&mut f, "5h", 20, 100.0);
        let w = f
            .view("a", at(20), None)
            .windows
            .into_iter()
            .find(|w| w.name == "5h")
            .unwrap();
        assert_eq!(w.outcome, ForecastOutcome::Exhausted);
        assert_eq!(w.seconds_to_exhaustion, Some(0));
        assert_eq!(w.exhaustion_at, Some(at(20)));
    }

    #[test]
    fn r13_forecast_explicit_names_invalid_values_and_overage_clamped() {
        let mut f = Forecasts::default();
        for name in [
            "primary",
            "secondary",
            "7d_oi",
            "0h",
            "-1h",
            "+1h",
            "1.5h",
            "999999999999999d",
            "💡",
        ] {
            f.observe("a", name, 50.0, None, at(0));
        }
        for value in [-0.1, f64::NAN, f64::INFINITY] {
            f.observe("a", "5h", value, None, at(0));
        }
        assert!(f.view("a", at(0), None).windows.is_empty());
        for name in ["30s", "60m", "5h", "7d", "30d"] {
            f.observe("a", name, 50.0, None, at(0));
        }
        assert_eq!(f.view("a", at(0), None).windows.len(), 5);
        assert!(f.view("a", at(-1), None).windows.is_empty());
        f.observe("a", "5h", 120.0, None, at(1));
        let over = f
            .view("a", at(1), None)
            .windows
            .into_iter()
            .find(|window| window.name == "5h")
            .unwrap();
        assert_eq!(over.used_pct, 100.0);
        assert_eq!(over.outcome, ForecastOutcome::Exhausted);
        f.observe("", "5h", 1.0, None, at(0));
        assert!(!f.accounts.contains_key(""));
    }

    #[test]
    fn r13_forecast_bounded_compaction_preserves_coverage_and_eviction() {
        let mut f = Forecasts::default();
        for second in 0..=MAX_SAMPLES_PER_SERIES {
            f.observe(
                "a",
                "5h",
                second as f64 / 100.0,
                None,
                at(0) + Duration::seconds(second as i64),
            );
        }
        assert_eq!(
            f.accounts["a"].windows["5h"]
                .track
                .as_ref()
                .unwrap()
                .samples
                .len(),
            MAX_SAMPLES_PER_SERIES
        );
        let w = window(&f, 10);
        assert!(w.truncated);
        near(w.recent_pct_per_hour, 15.36);
        for (minute, used) in [(10, 3.0), (20, 4.0), (30, 5.0), (40, 6.0)] {
            f.observe("a", "5h", used, None, at(minute));
        }
        near(window(&f, 40).recent_pct_per_hour, 6.0);
        for n in 1..=MAX_WINDOWS_PER_ACCOUNT {
            f.observe("a", &format!("{n}s"), 1.0, None, at(50 + n as i64));
        }
        assert_eq!(f.accounts["a"].windows.len(), MAX_WINDOWS_PER_ACCOUNT);
        assert!(!f.accounts["a"].windows.contains_key("5h"));
        for n in 0..MAX_ACCOUNTS {
            f.observe_credit(&format!("b{n}"), 10.0, at(100 + n as i64));
        }
        assert_eq!(f.accounts.len(), MAX_ACCOUNTS);
        assert!(f.view("a", at(500), None).windows.is_empty());
    }

    #[test]
    fn r13_forecast_credit_observations_topup_idle_and_measurement_deficit() {
        let mut f = Forecasts::default();
        f.observe_credit("a", 10.0, at(0));
        f.observe_credit("a", 9.0, at(5));
        assert_eq!(
            f.view("a", at(10), None).credit.unwrap().outcome,
            ForecastOutcome::Measuring
        );
        f.observe_credit("a", 8.0, at(10));
        let c = f.view("a", at(20), Some(at(5))).credit.unwrap();
        near(c.recent_credits_per_hour, 6.0);
        assert_eq!(c.exhaustion_at, Some(at(100)));
        assert!(c.changing);
        assert_eq!(
            f.view("a", at(35), None).credit.unwrap().outcome,
            ForecastOutcome::Measuring
        );
        let before = f.view("a", at(20), None);
        f.observe_credit("a", 1.0, at(10));
        f.observe_credit("a", f64::NAN, at(21));
        f.observe_credit("a", -1.0, at(21));
        assert_eq!(before, f.view("a", at(20), None));
        f.observe_credit("a", 20.0, at(21));
        assert_eq!(
            f.view("a", at(31), None).credit.unwrap().outcome,
            ForecastOutcome::Measuring
        );
        f.observe_credit("a", 0.0, at(32));
        assert_eq!(
            f.view("a", at(32), None).credit.unwrap().outcome,
            ForecastOutcome::Exhausted
        );
    }

    #[test]
    fn r13_forecast_dto_roundtrip() {
        let mut f = Forecasts::default();
        observe(&mut f, "5h", 0, 10.0);
        f.observe_credit("a", 10.0, at(0));
        let dto = f.view("a", at(0), None);
        let json = serde_json::to_string(&dto).unwrap();
        assert_eq!(
            serde_json::from_str::<AccountUsageForecast>(&json).unwrap(),
            dto
        );
    }

    #[test]
    fn r13_forecast_credit_interpolation_and_bounded_compaction() {
        let mut f = Forecasts::default();
        for (minute, balance) in [(0, 100.0), (10, 90.0), (20, 80.0), (30, 70.0), (40, 60.0)] {
            f.observe_credit("a", balance, at(minute));
        }
        let c = f.view("a", at(45), None).credit.unwrap();
        near(c.recent_credits_per_hour, 50.0);
        assert_eq!(c.measurement_start, Some(at(15)));
        assert_eq!(c.seconds_to_exhaustion, Some(4320));
        for value in [f64::NAN, f64::INFINITY, -1.0] {
            f.observe_credit("a", value, at(46));
        }
        assert_eq!(f.view("a", at(45), None).credit.unwrap(), c);
        for second in 0..=MAX_SAMPLES_PER_SERIES {
            f.observe_credit(
                "b",
                100.0 - second as f64 / 100.0,
                at(0) + Duration::seconds(second as i64),
            );
        }
        let c = f.view("b", at(10), None).credit.unwrap();
        assert!(c.truncated);
        near(c.recent_credits_per_hour, 15.36);
        assert_eq!(c.outcome, ForecastOutcome::Exhaustion);
    }

    #[test]
    fn r13_forecast_average_zero_and_future_window_start() {
        let mut f = Forecasts::default();
        observe(&mut f, "5h", 0, 0.0);
        let w = window(&f, 0);
        near(w.average_pct_per_hour, 0.0);
        assert_eq!(w.outcome, ForecastOutcome::NoFiniteForecast);
        assert_eq!(w.exhaustion_at, None);
        f.observe("b", "5h", 20.0, Some(at(400)), at(0));
        let w = f.view("b", at(10), None).windows.remove(0);
        assert_eq!(w.average_pct_per_hour, None);
        assert_eq!(w.outcome, ForecastOutcome::Measuring);
    }

    #[test]
    fn r13_forecast_reset_only_invalidates_until_fresh_usage_and_blocks_replay() {
        let mut f = Forecasts::default();
        for (minute, used) in [(0, 10.0), (5, 11.0), (10, 12.0)] {
            observe(&mut f, "5h", minute, used);
        }
        f.observe_reset("a", "5h", at(300), at(15));
        let pending = window(&f, 15);
        assert!(pending.awaiting_usage);
        assert_eq!(pending.recent_pct_per_hour, None);
        assert_eq!(pending.average_pct_per_hour, None);
        assert_eq!(pending.outcome, ForecastOutcome::Measuring);
        f.observe("a", "5h", 100.0, Some(at(240)), at(14));
        f.observe_reset("a", "5h", at(240), at(15));
        assert_eq!(window(&f, 15), pending);
        for (minute, used) in [(16, 13.0), (21, 14.0), (26, 15.0)] {
            f.observe("a", "5h", used, None, at(minute));
        }
        let fresh = window(&f, 26);
        assert!(!fresh.awaiting_usage);
        assert_eq!(fresh.reset_at, Some(at(300)));
        near(fresh.recent_pct_per_hour, 12.0);
        assert_eq!(fresh.recent_span_start, Some(at(16)));
        f.observe("a", "5h", 100.0, Some(at(300)), at(27));
        f.observe_reset("a", "5h", at(400), at(28));
        assert_eq!(window(&f, 28).outcome, ForecastOutcome::Measuring);
        assert_eq!(window(&f, 28).exhaustion_at, None);
    }

    #[test]
    fn r13_forecast_reset_only_unknown_to_known_preserves_segment() {
        let mut f = Forecasts::default();
        for (minute, used) in [(0, 10.0), (5, 11.0), (10, 12.0)] {
            f.observe("a", "5h", used, None, at(minute));
        }
        f.observe_reset("a", "5h", at(240), at(15));
        let w = window(&f, 15);
        assert!(!w.awaiting_usage);
        near(w.recent_pct_per_hour, 8.0);
        assert_eq!(w.observed_at, at(10));
        f.observe_reset("a", "5h", at(240), at(16));
        assert!(!window(&f, 16).awaiting_usage);
        f.observe_reset("missing", "5h", at(240), at(16));
        assert!(f.view("missing", at(16), None).windows.is_empty());
    }

    #[test]
    fn r13_forecast_sustained_headers_over_an_hour_keep_recent_and_credit_rates() {
        let mut f = Forecasts::default();
        for second in 0..=4230 {
            let now = at(0) + Duration::seconds(second);
            let used = second as f64 / 100.0;
            f.observe("a", "5h", used, Some(at(300)), now);
            f.observe_credit("a", 100.0 - used, now);
            if second >= 600 && second % 600 == 0 {
                let dto = f.view("a", now, None);
                let w = &dto.windows[0];
                assert_eq!(w.basis, Some(ForecastBasis::Recent));
                near(w.recent_pct_per_hour, 36.0);
                assert!(w.truncated);
                let c = dto.credit.unwrap();
                near(c.recent_credits_per_hour, 36.0);
                assert!(c.truncated);
            }
        }
        let track = f.accounts["a"].windows["5h"].track.as_ref().unwrap();
        let start = at(0) + Duration::seconds(4230 - 1800);
        assert!(track.samples.front().unwrap().at <= start);
        assert!(track.samples[1].at > start);
        assert_eq!(track.last().at, at(0) + Duration::seconds(4230));
        assert!(track.samples.len() <= MAX_SAMPLES_PER_SERIES);
        assert!(f.accounts["a"].credit.as_ref().unwrap().samples.len() <= MAX_SAMPLES_PER_SERIES);
    }

    #[test]
    fn r13_forecast_compaction_keeps_actual_quantized_steps_not_coalesced_steps() {
        let mut f = Forecasts::default();
        for second in 0..=600 {
            let used = second.min(2) as f64;
            let now = at(0) + Duration::seconds(second);
            f.observe("a", "5h", used, None, now);
            f.observe_credit("a", 100.0 - used, now);
            f.observe("one", "5h", if second == 0 { 0.0 } else { 2.0 }, None, now);
        }
        let dto = f.view("a", at(10), None);
        near(dto.windows[0].recent_pct_per_hour, 12.0);
        near(dto.credit.unwrap().recent_credits_per_hour, 12.0);
        let track = f.accounts["a"].windows["5h"].track.as_ref().unwrap();
        assert_eq!(
            track.positive_steps.iter().copied().collect::<Vec<_>>(),
            vec![at(0) + Duration::seconds(1), at(0) + Duration::seconds(2)]
        );
        assert_eq!(
            f.view("one", at(10), None).windows[0].recent_pct_per_hour,
            None
        );
        assert_eq!(window(&f, 31).recent_pct_per_hour, None);
        assert_eq!(
            f.view("a", at(31), None)
                .credit
                .unwrap()
                .recent_credits_per_hour,
            None
        );
        f.observe("a", "5h", 0.0, None, at(11));
        f.observe_credit("a", 200.0, at(11));
        assert!(
            f.accounts["a"].windows["5h"]
                .track
                .as_ref()
                .unwrap()
                .positive_steps
                .is_empty()
        );
        assert!(
            f.accounts["a"]
                .credit
                .as_ref()
                .unwrap()
                .positive_steps
                .is_empty()
        );
        assert!(!window(&f, 11).truncated);
        assert!(!f.view("a", at(11), None).credit.unwrap().truncated);
    }

    #[test]
    fn r13_forecast_sustained_quantized_headers_keep_recent_steps() {
        let mut f = Forecasts::default();
        for second in 0..=4230 {
            let used = (second / 600) as f64;
            let now = at(0) + Duration::seconds(second);
            f.observe("a", "5h", used, Some(at(300)), now);
            f.observe_credit("a", 100.0 - used, now);
        }
        let dto = f.view("a", at(0) + Duration::seconds(4230), None);
        assert_eq!(dto.windows[0].basis, Some(ForecastBasis::Recent));
        near(dto.windows[0].recent_pct_per_hour, 6.0);
        near(dto.credit.unwrap().recent_credits_per_hour, 6.0);
    }

    #[test]
    fn r13_forecast_initial_reset_only_metadata_supplies_average_without_fake_usage() {
        let mut f = Forecasts::default();
        f.observe_reset("a", "5h", at(300), at(0));
        assert!(f.view("a", at(0), None).windows.is_empty());
        f.observe_reset("a", "5h", at(400), at(-1));
        f.observe("a", "5h", 20.0, None, at(10));
        let w = window(&f, 10);
        assert_eq!(w.reset_at, Some(at(300)));
        assert_eq!(w.observed_at, at(10));
        near(w.average_pct_per_hour, 120.0);
        assert_eq!(w.recent_pct_per_hour, None);
        assert_eq!(w.basis, Some(ForecastBasis::Average));
        f.observe_reset("a", "5h", at(400), at(11));
        assert!(window(&f, 11).awaiting_usage);
        assert_eq!(window(&f, 11).average_pct_per_hour, None);
    }

    #[test]
    fn r13_forecast_reset_only_metadata_shares_account_and_window_bounds() {
        let mut f = Forecasts::default();
        for n in 0..=MAX_ACCOUNTS {
            for window in 1..=MAX_WINDOWS_PER_ACCOUNT + 1 {
                f.observe_reset(
                    &format!("a{n}"),
                    &format!("{window}h"),
                    at(300),
                    at(n as i64 * 100 + window as i64),
                );
            }
        }
        assert_eq!(f.accounts.len(), MAX_ACCOUNTS);
        assert!(!f.accounts.contains_key("a0"));
        for account in f.accounts.values() {
            assert_eq!(account.windows.len(), MAX_WINDOWS_PER_ACCOUNT);
            assert!(!account.windows.contains_key("1h"));
            assert!(
                account
                    .windows
                    .values()
                    .all(|window| window.track.is_none())
            );
        }
        f.observe_reset("", "5h", at(300), at(0));
        f.observe_reset("invalid", "primary", at(300), at(0));
        assert!(!f.accounts.contains_key(""));
        assert!(!f.accounts.contains_key("invalid"));
    }

    #[test]
    fn r13_forecast_overflow_compares_known_reset_before_timestamp_conversion() {
        for rate in [f64::MIN_POSITIVE, 1e-12] {
            let result = projection(100.0, Some(rate), at(0), Some(at(60)));
            assert_eq!(
                result,
                (ForecastOutcome::NoExhaustionBeforeReset, None, None)
            );
        }
        assert_eq!(
            projection(100.0, Some(f64::MIN_POSITIVE), at(0), None),
            (ForecastOutcome::NoFiniteForecast, None, None)
        );
        assert_eq!(
            projection(100.0, Some(0.0), at(0), Some(at(60))).0,
            ForecastOutcome::NoFiniteForecast
        );
    }

    #[test]
    fn r13_forecast_countdown_receipt_jitter_preserves_segment_and_stable_reset() {
        let mut f = Forecasts::default();
        let anchor = at(300) + Duration::milliseconds(100);
        for (minute, millis, used) in [(0, 100, 10.0), (5, 900, 11.0), (10, 200, 12.0)] {
            let received = at(minute) + Duration::milliseconds(millis);
            // Integer remaining seconds decrease while receipt fractions vary.
            let reported = received + Duration::seconds(18_000 - minute * 60);
            f.observe("a", "5h", used, Some(reported), received);
        }
        let now = at(10) + Duration::milliseconds(200);
        let w = f.view("a", now, None).windows.remove(0);
        assert_eq!(w.reset_at, Some(anchor));
        assert_eq!(w.basis, Some(ForecastBasis::Recent));
        near(w.recent_pct_per_hour, 2.0 / 600.1 * 3600.0);
        let received = at(15) + Duration::milliseconds(950);
        f.observe_reset("a", "5h", received + Duration::seconds(17_100), received);
        let w = f.view("a", received, None).windows.remove(0);
        assert_eq!(w.reset_at, Some(anchor));
        assert!(!w.awaiting_usage);
        assert!(w.recent_pct_per_hour.is_some());
        f.observe_reset("a", "5h", anchor + Duration::hours(5), at(16));
        assert!(window(&f, 16).awaiting_usage);
        assert_eq!(window(&f, 16).recent_pct_per_hour, None);
    }

    #[test]
    fn r13_forecast_reset_tolerance_is_inclusive_and_never_creeps() {
        let anchor = at(300);
        assert_eq!(
            stable_reset(Some(anchor), Some(anchor + Duration::seconds(1)), at(1)),
            Some(anchor)
        );
        assert_eq!(
            stable_reset(Some(anchor), Some(anchor - Duration::seconds(1)), at(1)),
            Some(anchor)
        );
        let mut f = Forecasts::default();
        for (minute, shift, used) in [(0, 0, 10.0), (5, 800, 11.0), (10, -800, 12.0)] {
            f.observe(
                "a",
                "5h",
                used,
                Some(anchor + Duration::milliseconds(shift)),
                at(minute),
            );
        }
        near(window(&f, 10).recent_pct_per_hour, 12.0);
        f.observe(
            "a",
            "5h",
            13.0,
            Some(anchor + Duration::milliseconds(1600)),
            at(15),
        );
        let w = window(&f, 15);
        assert_eq!(w.reset_at, Some(anchor + Duration::milliseconds(1600)));
        assert_eq!(w.recent_pct_per_hour, None);
        let mut metadata = Forecasts::default();
        metadata.observe_reset("a", "5h", anchor, at(0));
        metadata.observe_reset("a", "5h", anchor + Duration::milliseconds(800), at(5));
        metadata.observe_reset("a", "5h", anchor + Duration::milliseconds(1600), at(10));
        assert!(metadata.accounts["a"].windows["5h"].awaiting_usage);
    }

    #[test]
    fn r13_forecast_tolerated_jitter_never_hides_drop_or_passed_reset() {
        let mut f = Forecasts::default();
        for (minute, used) in [(0, 10.0), (5, 11.0), (10, 12.0)] {
            f.observe("a", "5h", used, Some(at(20)), at(minute));
        }
        f.observe(
            "a",
            "5h",
            1.0,
            Some(at(20) + Duration::milliseconds(500)),
            at(11),
        );
        assert_eq!(window(&f, 11).recent_pct_per_hour, None);
        assert_eq!(window(&f, 11).reset_at, Some(at(20)));
        f.observe("a", "5h", 2.0, None, at(15));
        f.observe(
            "a",
            "5h",
            3.0,
            Some(at(20) + Duration::milliseconds(500)),
            at(20) + Duration::milliseconds(100),
        );
        let w = f
            .view("a", at(20) + Duration::milliseconds(100), None)
            .windows
            .remove(0);
        assert_eq!(w.recent_pct_per_hour, None);
        assert_eq!(w.reset_at, Some(at(20) + Duration::milliseconds(500)));
        let mut metadata = Forecasts::default();
        metadata.observe("a", "5h", 100.0, Some(at(20)), at(0));
        metadata.observe_reset("a", "5h", at(20) + Duration::milliseconds(500), at(20));
        assert!(window(&metadata, 20).awaiting_usage);
        assert_eq!(window(&metadata, 20).outcome, ForecastOutcome::Measuring);
    }
}
