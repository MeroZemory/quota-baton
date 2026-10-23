//! Pure D30 evidence reducer, owned separately for each account/window-name.
//!
//! This module does not send, poll, persist, select accounts, or grant authority.
//! The caller serializes observations and durably publishes the resulting mark.
//! `coverage_known` must certify the interval from the previous observation's
//! source start through this report's application (and from the known window
//! start for a first observation). It must certify window observation coverage
//! without an unknown gap, physical sends on every transport, work already
//! active at baseline, and unresolved own usage. An
//! empty completion ledger or zero current inflight is not that certificate.
//!
//! Explicit conservative assumption: observed own traffic, including uncertain
//! active work, taints the remainder of its observed window AND the next window.
//! This is not proof that provider charges settle at that boundary. Callers with
//! unresolved later charges must continue supplying unknown coverage. Restart or
//! takeover cannot turn lost traffic coverage into a known zero generation.
//!
//! Sparse numeric samples may certify a clean window: `coverage_known` must
//! cover its unsampled head/tail too, without an unaccounted outside rise. On an
//! observed consecutive window transition it certifies closure of the preceding
//! window as well as coverage into the new one. This is a caller certificate,
//! not inferred from sparse endpoints, an empty ledger or a timer. Unknown
//! coverage may consequently retain a mark for longer than two windows.

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

const NOISE_PERCENTAGE_POINTS: f64 = 1.0;

/// Provider window identity. Never infer an observed start/zero from a reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Window {
    pub start: Option<DateTime<Utc>>,
    pub reset: DateTime<Utc>,
    pub duration_secs: i64,
}

impl Window {
    fn next_reset(self) -> Option<DateTime<Utc>> {
        let duration = TimeDelta::try_seconds(self.duration_secs)?;
        if self.duration_secs <= 0 || self.start?.checked_add_signed(duration)? != self.reset {
            return None;
        }
        self.reset.checked_add_signed(duration)
    }
}

/// A physical-send fence, not a token count or a D20 placement count.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OwnTraffic {
    /// Monotonic per account; increment before every possible physical send.
    pub generation: u64,
    /// Includes full named-window continuity, including unsampled boundaries;
    /// a transition also certifies the preceding window's remaining coverage.
    pub coverage_known: bool,
    pub active_or_unknown: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Observation {
    pub window: Window,
    pub used_pct: Option<f64>,
    pub observed_at: DateTime<Utc>,
    /// Source-start ordinal; delayed/equal reports cannot replace newer evidence.
    pub order: u64,
    pub own: OwnTraffic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Ignored,
    Unknown,
    Unchanged,
    Marked,
    Renewed,
    Cleared,
}

/// Serialize the entire state with its account/window-name association.
/// Missing/corrupt state is UNKNOWN, never evidence for clearing a saved mark.
/// Restore only trusted, schema-validated state; no process-local ordering clock
/// may be reset while keeping `coverage_known=true`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    last_order: Option<u64>,
    last_at: Option<DateTime<Utc>>,
    generation: Option<u64>,
    window: Option<Window>,
    baseline: Option<f64>,
    last_pct: Option<f64>,
    clean_from_start: bool,
    uncertain: bool,
    tainted_through: Option<DateTime<Utc>>,
    marked_through: Option<DateTime<Utc>>,
}

impl State {
    pub fn is_marked(&self) -> bool {
        self.marked_through.is_some()
    }

    /// Minimum protection horizon, NOT a TTL. Only `observe` can clear a mark.
    pub fn protected_through(&self) -> Option<DateTime<Utc>> {
        self.marked_through
    }

    fn invalidate(&mut self) {
        if let Some(through) = self.window.and_then(Window::next_reset) {
            self.tainted_through =
                Some(self.tainted_through.map_or(through, |old| old.max(through)));
        }
        self.baseline = None;
        self.last_pct = None;
        self.clean_from_start = false;
        self.uncertain = true;
    }

    fn taint(&mut self, through: DateTime<Utc>) {
        self.tainted_through = Some(self.tainted_through.map_or(through, |old| old.max(through)));
        self.invalidate();
    }

    pub fn observe(&mut self, report: Observation) -> Outcome {
        if self.last_order.is_some_and(|old| report.order <= old) {
            return Outcome::Ignored;
        }
        self.last_order = Some(report.order);
        if self.last_at.is_some_and(|old| report.observed_at < old) {
            self.invalidate();
            return Outcome::Unknown;
        }
        self.last_at = Some(report.observed_at);

        let Some(through) = report.window.next_reset() else {
            if self.window.is_none() {
                self.window = Some(report.window);
            }
            self.invalidate();
            return Outcome::Unknown;
        };
        let start = report.window.start.expect("validated window start");
        if report.observed_at < start || report.observed_at > report.window.reset {
            self.invalidate();
            return Outcome::Unknown;
        }
        let changed_window = self.window != Some(report.window);
        let discontinuity = self.window.is_some_and(|old| {
            changed_window
                && (old.reset != start || old.duration_secs != report.window.duration_secs)
        });
        let closes_clean_window = changed_window
            && !discontinuity
            && self.clean_from_start
            && !self.uncertain
            && self.marked_through.is_some_and(|end| start >= end);
        if changed_window {
            self.window = Some(report.window);
            self.baseline = None;
            self.last_pct = None;
            self.clean_from_start = false;
        }

        let own_changed = self.generation.map_or(report.own.generation != 0, |old| {
            old != report.own.generation
        });
        // A counter rollback is uncertainty too; never accept a new zero as clean.
        self.generation = Some(report.own.generation);
        if own_changed
            || report.own.active_or_unknown
            || !report.own.coverage_known
            || discontinuity
        {
            self.taint(through);
            return Outcome::Unknown;
        }
        let Some(pct) = report
            .used_pct
            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
        else {
            self.taint(through);
            return Outcome::Unknown;
        };
        if self.tainted_through.is_some_and(|end| start < end) {
            self.baseline = None;
            self.clean_from_start = false;
            return Outcome::Unknown;
        }
        // An invalid/undated report cannot be repaired by resetting a baseline
        // halfway through that same window. Require a genuine later boundary.
        if self.uncertain && !changed_window {
            return Outcome::Unknown;
        }
        self.uncertain = false;

        if let Some(previous) = self.last_pct
            && pct < previous
        {
            // Same-reset decreases may be corrections/churn, not a new zero.
            self.taint(through);
            return Outcome::Unknown;
        }
        self.last_pct = Some(pct);
        let Some(baseline) = self.baseline else {
            self.baseline = Some(pct);
            // The validated coverage certificate includes the window head;
            // equality between sample time and boundary is not required.
            self.clean_from_start = true;
            if closes_clean_window {
                self.marked_through = None;
                return Outcome::Cleared;
            }
            return Outcome::Unchanged;
        };
        if pct - baseline > NOISE_PERCENTAGE_POINTS {
            // Keep the original baseline, so subthreshold rises accumulate.
            self.clean_from_start = false;
            let previous = self.marked_through;
            self.marked_through = Some(previous.map_or(through, |old| old.max(through)));
            return if previous.is_none() {
                Outcome::Marked
            } else if previous.is_some_and(|old| through > old) {
                Outcome::Renewed
            } else {
                Outcome::Unchanged
            };
        }
        Outcome::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(window: i64, offset: i64, pct: f64, order: u64) -> Observation {
        let start = DateTime::from_timestamp(1_800_000_000 + window * 100, 0).unwrap();
        Observation {
            window: Window {
                start: Some(start),
                reset: start + TimeDelta::seconds(100),
                duration_secs: 100,
            },
            used_pct: Some(pct),
            observed_at: start + TimeDelta::seconds(offset),
            order,
            own: OwnTraffic {
                generation: 0,
                coverage_known: true,
                active_or_unknown: false,
            },
        }
    }

    fn marked() -> State {
        let mut state = State::default();
        assert_eq!(state.observe(report(0, 0, 10.0, 1)), Outcome::Unchanged);
        assert_eq!(state.observe(report(0, 20, 12.0, 2)), Outcome::Marked);
        state
    }

    #[test]
    fn n10_zero_own_rises_mint_and_subthreshold_changes_accumulate() {
        let mut state = State::default();
        state.observe(report(0, 0, 10.0, 1));
        for (offset, pct, order) in [(10, 10.4, 2), (20, 10.8, 3), (30, 11.0, 4)] {
            assert_eq!(
                state.observe(report(0, offset, pct, order)),
                Outcome::Unchanged
            );
            assert!(!state.is_marked());
        }
        assert_eq!(state.observe(report(0, 40, 11.1, 5)), Outcome::Marked);
        assert_eq!(
            state.protected_through(),
            Some(report(1, 0, 0.0, 6).window.reset)
        );
    }

    #[test]
    fn n10_own_send_and_late_charges_taint_current_and_next_window() {
        let mut state = State::default();
        state.observe(report(0, 0, 0.0, 1));
        let mut own = report(0, 10, 30.0, 2);
        own.own.generation = 1;
        assert_eq!(state.observe(own), Outcome::Unknown);
        for (window, offset, pct, order) in [(0, 90, 60.0, 3), (1, 0, 0.0, 4), (1, 70, 90.0, 5)] {
            let mut late = report(window, offset, pct, order);
            late.own.generation = 1;
            assert_eq!(state.observe(late), Outcome::Unknown);
            assert!(!state.is_marked());
        }
        // Continued unresolved liability must still be declared by the caller.
        let mut later = report(2, 0, 40.0, 6);
        later.own.generation = 1;
        later.own.active_or_unknown = true;
        assert_eq!(state.observe(later), Outcome::Unknown);
        assert!(!state.is_marked());
    }

    #[test]
    fn n10_prebaseline_active_and_startup_unknown_cannot_mint() {
        for active in [false, true] {
            let mut state = State::default();
            let mut first = report(0, 0, 0.0, 1);
            first.own.coverage_known = active;
            first.own.active_or_unknown = active;
            assert_eq!(state.observe(first), Outcome::Unknown);
            assert_eq!(state.observe(report(0, 40, 50.0, 2)), Outcome::Unknown);
            assert!(!state.is_marked());
        }
    }

    #[test]
    fn n10_sparse_certified_clean_window_clears_only_on_consecutive_transition() {
        let mut state = marked();
        state.observe(report(0, 100, 12.0, 3));
        assert!(state.is_marked());
        state.observe(report(1, 10, 0.0, 4));
        state.observe(report(1, 90, 1.0, 5));
        assert!(state.is_marked());
        assert_eq!(state.observe(report(2, 15, 0.0, 6)), Outcome::Cleared);
        assert!(!state.is_marked());
    }

    #[test]
    fn n10_outside_rise_in_next_window_renews_protection() {
        let mut state = marked();
        state.observe(report(1, 0, 0.0, 3));
        assert_eq!(state.observe(report(1, 50, 2.0, 4)), Outcome::Renewed);
        state.observe(report(1, 100, 2.0, 5));
        assert!(state.is_marked());
        state.observe(report(2, 0, 0.0, 6));
        assert_eq!(state.observe(report(2, 100, 0.0, 7)), Outcome::Unchanged);
        assert!(state.is_marked());
        assert_eq!(state.observe(report(3, 10, 0.0, 8)), Outcome::Cleared);
    }

    #[test]
    fn n10_unknown_or_skipped_window_cannot_clear() {
        for variant in 0..2 {
            let mut state = marked();
            let mut first = report(if variant == 0 { 2 } else { 1 }, 10, 0.0, 3);
            if variant == 1 {
                first.own.coverage_known = false;
            }
            state.observe(first);
            state.observe(report(if variant == 0 { 2 } else { 1 }, 100, 0.0, 4));
            state.observe(report(if variant == 0 { 3 } else { 2 }, 10, 0.0, 5));
            assert!(state.is_marked());
        }
    }

    #[test]
    fn n10_missing_closure_coverage_cannot_clear_a_sparse_clean_candidate() {
        let mut state = marked();
        state.observe(report(1, 10, 0.0, 3));
        state.observe(report(1, 90, 0.5, 4));
        let mut transition = report(2, 10, 0.0, 5);
        transition.own.coverage_known = false;
        assert_eq!(state.observe(transition), Outcome::Unknown);
        assert!(state.is_marked());
    }

    #[test]
    fn n10_old_reports_reset_churn_and_clock_rollback_are_not_clean_evidence() {
        let mut state = marked();
        assert_eq!(state.observe(report(0, 0, 0.0, 1)), Outcome::Ignored);
        let mut churn = report(0, 30, 0.0, 3);
        churn.window.reset += TimeDelta::seconds(1);
        assert_eq!(state.observe(churn), Outcome::Unknown);
        assert!(state.is_marked());
        assert_eq!(state.observe(report(0, 10, 0.0, 4)), Outcome::Unknown);
        state.observe(report(1, 0, 0.0, 5));
        state.observe(report(1, 100, 0.0, 6));
        assert!(state.is_marked());
    }

    #[test]
    fn n10_serialization_preserves_mark_and_own_taint_not_a_clean_restart() {
        let mut state = marked();
        let mut own = report(0, 30, 20.0, 3);
        own.own.generation = 7;
        state.observe(own);
        let bytes = serde_json::to_vec(&state).unwrap();
        let mut restored: State = serde_json::from_slice(&bytes).unwrap();
        let mut next = report(1, 0, 0.0, 4);
        next.own.generation = 7;
        assert_eq!(restored.observe(next), Outcome::Unknown);
        assert!(restored.is_marked());
        assert_eq!(restored.protected_through(), state.protected_through());
    }

    #[test]
    fn n10_missing_start_invalid_numbers_and_same_reset_decrease_cannot_mint() {
        for value in [
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(-1.0),
            Some(101.0),
        ] {
            let mut state = State::default();
            state.observe(report(0, 0, 10.0, 1));
            let mut invalid = report(0, 10, 0.0, 2);
            invalid.used_pct = value;
            assert_eq!(state.observe(invalid), Outcome::Unknown);
            state.observe(report(0, 50, 90.0, 3));
            assert!(!state.is_marked());
        }
        let mut state = State::default();
        let mut undated = report(0, 0, 0.0, 1);
        undated.window.start = None;
        assert_eq!(state.observe(undated), Outcome::Unknown);
        state.observe(report(0, 10, 0.0, 2));
        state.observe(report(0, 50, 50.0, 3));
        assert!(!state.is_marked());

        let mut state = State::default();
        state.observe(report(0, 0, 10.0, 1));
        assert_eq!(state.observe(report(0, 10, 0.0, 2)), Outcome::Unknown);
        state.observe(report(0, 50, 50.0, 3));
        assert!(!state.is_marked());
    }

    #[test]
    fn n10_own_taint_expires_only_for_a_later_known_window_not_a_timer() {
        let mut state = State::default();
        let mut own = report(0, 10, 0.0, 1);
        own.own.generation = 1;
        state.observe(own);
        for (window, offset, pct, order) in [(1, 0, 0.0, 2), (1, 100, 30.0, 3)] {
            let mut late = report(window, offset, pct, order);
            late.own.generation = 1;
            assert_eq!(state.observe(late), Outcome::Unknown);
        }
        let mut baseline = report(2, 0, 0.0, 4);
        baseline.own.generation = 1;
        assert_eq!(state.observe(baseline), Outcome::Unchanged);
        let mut rise = report(2, 50, 20.0, 5);
        rise.own.generation = 1;
        assert_eq!(state.observe(rise), Outcome::Marked);
    }
}
