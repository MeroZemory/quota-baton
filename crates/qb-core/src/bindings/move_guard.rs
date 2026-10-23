//! Runtime move budgets. Call prepare/commit under the binding mutation gate.
//! A failed policy/admission drops the permit without consuming a move.
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

const SESSIONS: usize = 20_000;
const RECORDS: usize = 128;
const GLOBAL_RECORDS: usize = 4096;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    pub session_ten_minutes: usize,
    pub session_hour: usize,
    pub global_ten_minutes: usize,
    pub trip_seconds: u64,
    pub reread_notice_tokens: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            session_ten_minutes: 1,
            session_hour: 3,
            global_ten_minutes: 6,
            trip_seconds: 240,
            reread_notice_tokens: 100_000,
        }
    }
}
impl Limits {
    pub(crate) fn bounded(mut self) -> Self {
        self.session_ten_minutes = self.session_ten_minutes.clamp(1, 16);
        self.session_hour = self.session_hour.clamp(1, 64);
        self.global_ten_minutes = self.global_ten_minutes.clamp(1, 128);
        self.trip_seconds = self.trip_seconds.clamp(1, 3600);
        self.reread_notice_tokens = self.reread_notice_tokens.clamp(1, 1_000_000_000);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Refusal {
    SessionTenMinutes,
    SessionHour,
    PingPong,
    GlobalTrip,
    Capacity,
    StalePermit,
}

impl Refusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::SessionTenMinutes => "session_ten_minutes",
            Self::SessionHour => "session_hour",
            Self::PingPong => "ping_pong",
            Self::GlobalTrip => "global_trip",
            Self::Capacity => "capacity",
            Self::StalePermit => "stale_permit",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Denied {
    pub reason: Refusal,
    pub until: DateTime<Utc>,
    pub newly_tripped: bool,
}

/// A lower bound captured before reading any part of a candidate snapshot.
/// This orders observations; it is never part of the semantic fingerprint.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Observation {
    started: Option<Instant>,
}

impl Observation {
    pub(crate) fn lower_bound(self) -> Option<Instant> {
        self.started
    }

    pub(crate) fn before_collection() -> Self {
        Self {
            started: Some(Instant::now()),
        }
    }

    fn after(self, committed: Instant) -> bool {
        self.started.is_some_and(|started| started > committed)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Observations {
    pub source: Observation,
    pub target: Observation,
}

impl Observations {
    pub(crate) fn same(observation: Observation) -> Self {
        Self {
            source: observation,
            target: observation,
        }
    }

    /// Legacy snapshot APIs cannot establish chronology. They fail closed for
    /// evidence exceptions in every build, including unit tests.
    pub(crate) fn untracked() -> Self {
        Self::same(Observation::default())
    }
}

#[derive(Clone, Debug)]
struct Movement {
    at: DateTime<Utc>,
    committed: Instant,
    from: String,
    // Semantic capacity/reset fingerprint, never a poll timestamp or metadata
    // generation. A new poll of identical evidence does not authorize return.
    departure_evidence: u64,
    to: String,
    destination_evidence: u64,
}

// A shared failure moves many sessions, but consumes only one global incident.
// Unknown or usable-source moves are independent; never merge missing evidence.
#[derive(Clone, Debug)]
struct Incident {
    at: DateTime<Utc>,
    shared: Option<(String, u64)>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Breaker {
    sessions: BTreeMap<String, VecDeque<Movement>>,
    global: VecDeque<Incident>,
    tripped_until: Option<DateTime<Utc>>,
    last_now: Option<DateTime<Utc>>,
    revision: u64,
}

#[derive(Debug)]
#[must_use]
pub(crate) struct Permit {
    revision: u64,
    shared_incident: Option<(String, u64)>,
    key: String,
    at: DateTime<Utc>,
    from: String,
    to: String,
    departure_evidence: u64,
    destination_evidence: u64,
}

/// Minted only by a successful breaker commit. Neither selectors nor storage
/// can invent this receipt from a reason string or a candidate identifier.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CommittedMove {
    key: String,
    from: String,
    to: String,
}

impl CommittedMove {
    pub(super) fn allows(&self, from: &str, to: &str) -> bool {
        self.from == from && self.to == to
    }

    pub(super) fn for_group(&self, key: &str) -> bool {
        self.key == key
    }
}

impl Breaker {
    fn prune(&mut self, now: DateTime<Utc>) -> DateTime<Utc> {
        let now = self.last_now.map_or(now, |prior| now.max(prior));
        self.last_now = Some(now);
        self.global
            .retain(|incident| incident.at + Duration::minutes(10) > now);
        self.sessions.retain(|_, history| {
            history.retain(|m| m.at + Duration::hours(1) > now);
            !history.is_empty()
        });
        if self.tripped_until.is_some_and(|until| until <= now) {
            self.tripped_until = None;
            self.global.clear();
            self.revision = self.revision.saturating_add(1);
        }
        now
    }

    /// `fresh_unusable` and capacity fingerprints are supplied by the owning
    /// runtime modules, not parsed from the proposed reason string.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        &mut self,
        limits: &Limits,
        key: &str,
        from: &str,
        to: &str,
        departure_evidence: u64,
        destination_evidence: u64,
        source_unusable: bool,
        observations: Observations,
        now: DateTime<Utc>,
    ) -> Result<Permit, Denied> {
        let limits = limits.clone().bounded();
        let now = self.prune(now);
        let denial = |reason, until| Denied {
            reason,
            until,
            newly_tripped: false,
        };
        if key.is_empty()
            || key.len() > 256
            || from.is_empty()
            || from.len() > 128
            || to.is_empty()
            || to.len() > 128
            || from == to
        {
            return Err(denial(Refusal::Capacity, now + Duration::minutes(10)));
        }
        if let Some(until) = self.tripped_until {
            return Err(denial(Refusal::GlobalTrip, until));
        }
        let shared_incident =
            (source_unusable && departure_evidence != 0 && observations.source.started.is_some())
                .then(|| (from.to_owned(), departure_evidence));
        let shares_incident = shared_incident.as_ref().is_some_and(|key| {
            self.global
                .iter()
                .any(|incident| incident.shared.as_ref() == Some(key))
        });
        if !shares_incident && self.global.len() >= limits.global_ten_minutes {
            let until = now + Duration::seconds(limits.trip_seconds as i64);
            self.tripped_until = Some(until);
            self.revision = self.revision.saturating_add(1);
            return Err(Denied {
                reason: Refusal::GlobalTrip,
                until,
                newly_tripped: true,
            });
        }
        if (!shares_incident && self.global.len() >= GLOBAL_RECORDS)
            || self.revision == u64::MAX
            || (!self.sessions.contains_key(key) && self.sessions.len() >= SESSIONS)
        {
            return Err(denial(Refusal::Capacity, now + Duration::minutes(10)));
        }
        if let Some(history) = self.sessions.get(key) {
            if history.len() >= RECORDS {
                return Err(denial(
                    Refusal::Capacity,
                    history[0].at + Duration::hours(1),
                ));
            }
            if let Some(left) = history
                .iter()
                .rev()
                .find(|m| m.from == to && m.at + Duration::minutes(30) > now)
                && (left.departure_evidence == 0
                    || destination_evidence == 0
                    || left.departure_evidence == destination_evidence
                    || !observations.target.after(left.committed))
            {
                return Err(denial(Refusal::PingPong, left.at + Duration::minutes(30)));
            }
            // A differing old snapshot is not a failure since arrival. Stale,
            // equal or untracked observations keep the ordinary budget/hold.
            let fresh_unusable = source_unusable
                && history.back().is_some_and(|last| {
                    last.to == from
                        && observations.source.after(last.committed)
                        && departure_evidence != 0
                        && last.destination_evidence != 0
                        && last.destination_evidence != departure_evidence
                });
            if !fresh_unusable {
                let recent: Vec<_> = history
                    .iter()
                    .filter(|m| m.at + Duration::minutes(10) > now)
                    .collect();
                if recent.len() >= limits.session_ten_minutes {
                    return Err(denial(
                        Refusal::SessionTenMinutes,
                        recent[recent.len() - limits.session_ten_minutes].at
                            + Duration::minutes(10),
                    ));
                }
                if history.len() >= limits.session_hour {
                    return Err(denial(
                        Refusal::SessionHour,
                        history[history.len() - limits.session_hour].at + Duration::hours(1),
                    ));
                }
            }
        }
        Ok(Permit {
            revision: self.revision,
            shared_incident,
            key: key.into(),
            at: now,
            from: from.into(),
            to: to.into(),
            departure_evidence,
            destination_evidence,
        })
    }

    /// Call only after policy/admission success, just before changing the group.
    /// The mutation gate excludes other commits/clear/rename in between.
    pub(super) fn commit(
        &mut self,
        permit: Permit,
        actual_to: &str,
    ) -> Result<CommittedMove, Refusal> {
        if permit.revision != self.revision || permit.to != actual_to || self.revision == u64::MAX {
            return Err(Refusal::StalePermit);
        }
        self.revision += 1;
        let committed = CommittedMove {
            key: permit.key.clone(),
            from: permit.from.clone(),
            to: permit.to.clone(),
        };
        let already_counted = permit.shared_incident.as_ref().is_some_and(|key| {
            self.global
                .iter()
                .any(|incident| incident.shared.as_ref() == Some(key))
        });
        if !already_counted {
            self.global.push_back(Incident {
                at: permit.at,
                shared: permit.shared_incident,
            });
        }
        self.sessions
            .entry(permit.key)
            .or_default()
            .push_back(Movement {
                at: permit.at,
                committed: Instant::now(),
                from: permit.from,
                departure_evidence: permit.departure_evidence,
                to: permit.to,
                destination_evidence: permit.destination_evidence,
            });
        Ok(committed)
    }

    pub(crate) fn recent_moves(&self, now: DateTime<Utc>) -> Vec<super::MoveGuardPair> {
        let mut moves: Vec<_> = self
            .sessions
            .values()
            .flatten()
            .filter(|m| m.at + Duration::minutes(10) > now)
            .collect();
        moves.sort_by_key(|m| m.at);
        moves
            .into_iter()
            .rev()
            .take(128)
            .map(|m| super::MoveGuardPair {
                from: m.from.clone(),
                to: m.to.clone(),
            })
            .collect()
    }

    pub(crate) fn state(&mut self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.prune(now);
        self.tripped_until
    }

    pub(crate) fn clear(&mut self) {
        // Explicit acknowledgement clears global trip/history, not session
        // damage limits or departure evidence. Timeout has identical semantics.
        self.tripped_until = None;
        self.global.clear();
        self.revision = self.revision.saturating_add(1);
    }

    /// Adoption is an identity rename, never a budget reset or a new move.
    pub(crate) fn rename(&mut self, from: &str, to: &str) -> Result<(), Refusal> {
        if from == to {
            return Ok(());
        }
        if let Some(source) = self.sessions.get(from) {
            let count = source.len() + self.sessions.get(to).map_or(0, VecDeque::len);
            if count > RECORDS || self.revision == u64::MAX {
                return Err(Refusal::Capacity);
            }
        }
        if let Some(history) = self.sessions.remove(from) {
            let entry = self.sessions.entry(to.into()).or_default();
            entry.extend(history);
            entry.make_contiguous().sort_by_key(|m| m.at);
            self.revision = self.revision.saturating_add(1);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(2_000_000_000, 0).unwrap()
    }
    fn move_at(
        b: &mut Breaker,
        key: &str,
        from: &str,
        to: &str,
        fresh: bool,
        seconds: i64,
    ) -> Result<(), Denied> {
        let p = b.prepare(
            &Limits::default(),
            key,
            from,
            to,
            if fresh { 2 } else { 1 },
            1,
            fresh,
            Observations::same(Observation::before_collection()),
            now() + Duration::seconds(seconds),
        )?;
        b.commit(p, to).unwrap();
        Ok(())
    }
    #[test]
    fn r14_breaker_per_session_and_exact_boundary() {
        let mut b = Breaker::default();
        move_at(&mut b, "s", "a", "b", false, 0).unwrap();
        assert_eq!(
            move_at(&mut b, "s", "b", "c", false, 599)
                .unwrap_err()
                .reason,
            Refusal::SessionTenMinutes
        );
        move_at(&mut b, "s", "b", "c", false, 600).unwrap();
        move_at(&mut b, "s", "c", "d", false, 1200).unwrap();
        assert_eq!(
            move_at(&mut b, "s", "d", "e", false, 1800)
                .unwrap_err()
                .reason,
            Refusal::SessionHour
        );
        move_at(&mut b, "s", "d", "e", false, 3600).unwrap();
    }
    #[test]
    fn r14_breaker_fresh_unusable_never_overrides_pingpong_or_global() {
        let mut b = Breaker::default();
        move_at(&mut b, "s", "a", "b", false, 0).unwrap();
        assert_eq!(
            move_at(&mut b, "s", "b", "a", true, 1).unwrap_err().reason,
            Refusal::PingPong
        );
        let p = b
            .prepare(
                &Limits::default(),
                "s",
                "b",
                "a",
                2,
                2,
                true,
                Observations::same(Observation::before_collection()),
                now() + Duration::seconds(2),
            )
            .unwrap();
        b.commit(p, "a").unwrap();
        for i in 0..4 {
            move_at(&mut b, &format!("other{i}"), "a", "b", false, 3 + i).unwrap();
        }
        let denied = move_at(&mut b, "s", "a", "c", true, 7).unwrap_err();
        assert!(denied.newly_tripped);
        assert_eq!(denied.reason, Refusal::GlobalTrip);
        assert_eq!(
            move_at(&mut b, "new", "a", "b", true, 8).unwrap_err().until,
            denied.until
        );
        move_at(&mut b, "new", "a", "b", false, 607).unwrap();
    }
    #[test]
    fn r14_breaker_dropped_permit_does_not_charge_and_stale_cannot_commit() {
        let mut b = Breaker::default();
        let limits = Limits::default();
        drop(
            b.prepare(
                &limits,
                "s",
                "a",
                "b",
                1,
                1,
                false,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap(),
        );
        let p = b
            .prepare(
                &limits,
                "s",
                "a",
                "b",
                1,
                1,
                false,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap();
        b.clear();
        assert_eq!(b.commit(p, "b"), Err(Refusal::StalePermit));
        assert!(b.global.is_empty());
        move_at(&mut b, "s", "a", "b", false, 0).unwrap();
    }
    #[test]
    fn r14_breaker_alias_rename_retains_damage_budget() {
        let mut b = Breaker::default();
        move_at(&mut b, "provisional", "a", "b", false, 0).unwrap();
        b.rename("provisional", "canonical").unwrap();
        assert_eq!(
            move_at(&mut b, "canonical", "b", "c", false, 1)
                .unwrap_err()
                .reason,
            Refusal::SessionTenMinutes
        );
        assert_eq!(
            move_at(&mut b, "canonical", "b", "a", true, 1)
                .unwrap_err()
                .reason,
            Refusal::PingPong
        );
        let bounded = Limits {
            session_ten_minutes: 0,
            session_hour: 1000,
            global_ten_minutes: 0,
            trip_seconds: u64::MAX,
            reread_notice_tokens: 0,
        }
        .bounded();
        assert_eq!(
            (
                bounded.session_ten_minutes,
                bounded.session_hour,
                bounded.global_ten_minutes,
                bounded.trip_seconds
            ),
            (1, 64, 1, 3600)
        );
    }

    #[test]
    fn r14_observation_must_follow_commit_for_both_exceptions() {
        let mut b = Breaker::default();
        move_at(&mut b, "s", "a", "b", false, 0).unwrap();
        let committed = b.sessions["s"].back().unwrap().committed;
        let at = |started| Observation {
            started: Some(started),
        };
        for observation in [
            Observation::default(),
            at(committed - std::time::Duration::from_nanos(1)),
            at(committed),
        ] {
            let observations = Observations::same(observation);
            assert_eq!(
                b.prepare(
                    &Limits::default(),
                    "s",
                    "b",
                    "c",
                    2,
                    1,
                    true,
                    observations,
                    now() + Duration::seconds(1),
                )
                .unwrap_err()
                .reason,
                Refusal::SessionTenMinutes
            );
            assert_eq!(
                b.prepare(
                    &Limits::default(),
                    "s",
                    "b",
                    "a",
                    2,
                    2,
                    true,
                    observations,
                    now() + Duration::seconds(1),
                )
                .unwrap_err()
                .reason,
                Refusal::PingPong
            );
        }
        let observations = Observations::same(at(committed + std::time::Duration::from_nanos(1)));
        for (to, destination) in [("c", 1), ("a", 2)] {
            let permit = b
                .prepare(
                    &Limits::default(),
                    "s",
                    "b",
                    to,
                    2,
                    destination,
                    true,
                    observations,
                    now() + Duration::seconds(1),
                )
                .unwrap();
            drop(permit); // A fresh observation permits, but never charges, preparation.
        }
    }
    #[test]
    fn r141_twelve_sessions_share_one_failure_incident_without_sliding_expiry() {
        let mut breaker = Breaker::default();
        for i in 0..12 {
            let permit = breaker
                .prepare(
                    &Limits::default(),
                    &format!("session-{i}"),
                    "a",
                    "b",
                    900,
                    1,
                    true,
                    Observations::same(Observation::before_collection()),
                    now() + Duration::seconds(i),
                )
                .unwrap();
            breaker.commit(permit, "b").unwrap();
        }
        assert_eq!(breaker.sessions.len(), 12);
        assert_eq!(breaker.global.len(), 1);
        assert_eq!(breaker.global[0].at, now());
        assert!(breaker.state(now() + Duration::seconds(599)).is_none());
        assert_eq!(breaker.global.len(), 1);
        assert!(breaker.state(now() + Duration::seconds(600)).is_none());
        assert!(breaker.global.is_empty());
    }

    #[test]
    fn r141_six_distinct_flapping_incidents_trip_and_expire_at_240_seconds() {
        let mut breaker = Breaker::default();
        for i in 0..6 {
            let permit = breaker
                .prepare(
                    &Limits::default(),
                    &format!("session-{i}"),
                    "a",
                    "b",
                    900 + i,
                    1,
                    true,
                    Observations::same(Observation::before_collection()),
                    now(),
                )
                .unwrap();
            breaker.commit(permit, "b").unwrap();
        }
        assert_eq!(breaker.global.len(), 6);
        let denial = breaker
            .prepare(
                &Limits::default(),
                "seventh",
                "a",
                "b",
                999,
                1,
                true,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap_err();
        assert!(denial.newly_tripped);
        assert_eq!(denial.reason, Refusal::GlobalTrip);
        assert_eq!(denial.until, now() + Duration::seconds(240));
        assert!(breaker.state(now() + Duration::seconds(239)).is_some());
        assert!(breaker.state(now() + Duration::seconds(240)).is_none());
        assert!(breaker.global.is_empty());
    }

    #[test]
    fn r141_unknown_or_usable_source_moves_do_not_share_an_incident() {
        for (unusable, evidence, observations) in [
            (
                false,
                900,
                Observations::same(Observation::before_collection()),
            ),
            (
                true,
                0,
                Observations::same(Observation::before_collection()),
            ),
            (true, 900, Observations::untracked()),
        ] {
            let mut breaker = Breaker::default();
            for i in 0..6 {
                let permit = breaker
                    .prepare(
                        &Limits::default(),
                        &format!("s-{i}"),
                        "a",
                        "b",
                        evidence,
                        1,
                        unusable,
                        observations,
                        now(),
                    )
                    .unwrap();
                breaker.commit(permit, "b").unwrap();
            }
            assert_eq!(breaker.global.len(), 6);
            assert_eq!(
                breaker
                    .prepare(
                        &Limits::default(),
                        "seventh",
                        "a",
                        "b",
                        evidence,
                        1,
                        unusable,
                        observations,
                        now(),
                    )
                    .unwrap_err()
                    .reason,
                Refusal::GlobalTrip
            );
        }
    }
    #[test]
    fn r141_full_incident_budget_admits_only_existing_cohort_until_trip() {
        let mut breaker = Breaker::default();
        for i in 0..6 {
            let permit = breaker
                .prepare(
                    &Limits::default(),
                    &format!("cohort-{i}"),
                    "a",
                    "b",
                    900 + i,
                    1,
                    true,
                    Observations::same(Observation::before_collection()),
                    now(),
                )
                .unwrap();
            breaker.commit(permit, "b").unwrap();
        }
        let dropped = breaker
            .prepare(
                &Limits::default(),
                "dropped",
                "a",
                "c",
                900,
                1,
                true,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap();
        drop(dropped);
        assert_eq!(breaker.global.len(), 6);
        assert!(!breaker.sessions.contains_key("dropped"));
        let shared = breaker
            .prepare(
                &Limits::default(),
                "shared",
                "a",
                "c",
                900,
                1,
                true,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap();
        breaker.commit(shared, "c").unwrap();
        assert_eq!(breaker.global.len(), 6);
        assert_eq!(breaker.sessions["shared"].len(), 1);
        let trip = breaker
            .prepare(
                &Limits::default(),
                "new-incident",
                "a",
                "b",
                999,
                1,
                true,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap_err();
        assert_eq!(trip.reason, Refusal::GlobalTrip);
        assert!(trip.newly_tripped);
        assert_eq!(
            breaker
                .prepare(
                    &Limits::default(),
                    "shared-after-trip",
                    "a",
                    "c",
                    900,
                    1,
                    true,
                    Observations::same(Observation::before_collection()),
                    now(),
                )
                .unwrap_err()
                .reason,
            Refusal::GlobalTrip
        );
        assert!(!breaker.sessions.contains_key("shared-after-trip"));
    }

    #[test]
    fn r141_stale_shared_permit_cannot_recreate_cleared_incident() {
        let mut breaker = Breaker::default();
        let first = breaker
            .prepare(
                &Limits::default(),
                "first",
                "a",
                "b",
                900,
                1,
                true,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap();
        breaker.commit(first, "b").unwrap();
        let shared = breaker
            .prepare(
                &Limits::default(),
                "second",
                "a",
                "c",
                900,
                1,
                true,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap();
        breaker.clear();
        assert_eq!(breaker.commit(shared, "c"), Err(Refusal::StalePermit));
        assert!(breaker.global.is_empty());
        assert!(!breaker.sessions.contains_key("second"));
        assert_eq!(breaker.sessions["first"].len(), 1);
    }

    #[test]
    fn r141_shared_incident_never_exempts_session_or_unchanged_return_evidence() {
        let mut breaker = Breaker::default();
        let first = breaker
            .prepare(
                &Limits::default(),
                "session",
                "a",
                "b",
                700,
                800,
                true,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap();
        breaker.commit(first, "b").unwrap();
        let other = breaker
            .prepare(
                &Limits::default(),
                "other",
                "b",
                "c",
                800,
                900,
                true,
                Observations::same(Observation::before_collection()),
                now(),
            )
            .unwrap();
        breaker.commit(other, "c").unwrap();
        assert_eq!(breaker.global.len(), 2);
        // The same global incident does not make unchanged source semantics fresh.
        assert_eq!(
            breaker
                .prepare(
                    &Limits::default(),
                    "session",
                    "b",
                    "c",
                    800,
                    900,
                    true,
                    Observations::same(Observation::before_collection()),
                    now(),
                )
                .unwrap_err()
                .reason,
            Refusal::SessionTenMinutes
        );
        // Nor does it authorize a return to A without changed destination evidence.
        assert_eq!(
            breaker
                .prepare(
                    &Limits::default(),
                    "session",
                    "b",
                    "a",
                    800,
                    700,
                    true,
                    Observations::same(Observation::before_collection()),
                    now(),
                )
                .unwrap_err()
                .reason,
            Refusal::PingPong
        );
        assert_eq!(breaker.global.len(), 2);
        assert_eq!(breaker.sessions["session"].len(), 1);
    }
}
