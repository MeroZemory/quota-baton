//! Bounded, deterministic move-cost reducer. Parsing and identity interning are separate.
pub use crate::accounts::Provider;
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, VecDeque};
pub type Usage = super::CacheUsage;

const MAX_SESSIONS: usize = 1024;
const MAX_MOVES: usize = 4096;
const MAX_REQUESTS: usize = 4096;
const BASELINE_SAMPLES: usize = 8;

/// Stable canonical identity across aliases/adoption. Never recycle within this reducer.
pub type SessionId = u64;
pub type MoveId = u64;

/// Opaque interned account/model identities, supplied by integration (no retained strings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regime {
    pub provider: Provider,
    pub account: u64,
    pub model: u64,
}

/// Numeric DTO matching CacheUsage fields. Parsing/stream-field merging is external.
pub fn uncached(provider: Provider, usage: Usage) -> Option<u64> {
    match provider {
        Provider::Codex => usage.input_tokens?.checked_sub(usage.cached_tokens?),
        // Claude input excludes BOTH cache counters. Read tokens are never subtracted.
        // Creation must be explicitly present, including explicit zero.
        Provider::Claude => usage
            .input_tokens?
            .checked_add(usage.cache_creation_input_tokens?),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    ClockReversed,
    Capacity,
    IdExhausted,
    WrongOwner,
    UnknownTicket,
    InvalidIdentity,
    AliasConflict,
    StaleRegime,
}

/// Non-Clone, private fields: finish consumes precisely the ticket issued by begin.
#[derive(Debug)]
pub struct RequestTicket {
    owner: u128,
    id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum Completion {
    Normal,
    Move {
        id: MoveId,
        reread_tokens: Option<u64>,
    },
    /// Request belonged to an earlier regime/move, or its move aged out.
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Aggregate {
    pub tokens: u64,
    pub known_count: u64,
    pub unknown_count: u64,
    /// Start of this reducer's continuous observation, NOT the start of a full day.
    pub observed_since: DateTime<Utc>,
    /// False after any capacity/ID rejection; restart with a new observation epoch to recover.
    pub recording_complete: bool,
    pub tokens_saturated: bool,
}

struct Session {
    regime: Regime,
    epoch: u64,
    baseline: VecDeque<u64>,
    pending: Option<MoveId>,
    active_move: Option<MoveId>,
    cold: bool,
    touched: DateTime<Utc>,
}
struct Move {
    session: SessionId,
    at: DateTime<Utc>,
    baseline: Option<u64>,
    // Kept separately when a handshake cannot yet name the destination model.
    baseline_model: Option<u64>,
    cost: Option<u64>,
    claimed: bool,
}
struct Request {
    session: SessionId,
    regime: Regime,
    epoch: u64,
    at: DateTime<Utc>,
    move_id: Option<MoveId>,
    normal: bool,
}

pub struct Accounting {
    owner: u128,
    observed_since: DateTime<Utc>,
    now: DateTime<Utc>,
    next_id: u64,
    complete: bool,
    sessions: BTreeMap<SessionId, Session>,
    moves: BTreeMap<MoveId, Move>,
    requests: BTreeMap<u64, Request>,
}

fn median(samples: &VecDeque<u64>) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<_> = samples.iter().copied().collect();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        Some(sorted[n / 2])
    } else {
        let (lo, hi) = (sorted[n / 2 - 1], sorted[n / 2]);
        Some(lo + (hi - lo) / 2) // Floor median, overflow-safe even at u64::MAX.
    }
}

impl Accounting {
    /// owner must be unique per live instance, including replacement instances.
    pub fn new(owner: u128, now: DateTime<Utc>) -> Self {
        Self {
            owner,
            observed_since: now,
            now,
            next_id: 0,
            complete: true,
            sessions: BTreeMap::new(),
            moves: BTreeMap::new(),
            requests: BTreeMap::new(),
        }
    }

    /// Inject processing time, monotonically. Completion order may differ from start order.
    /// Retention is (now - 24h, now]; aggregates count by move time, not completion time.
    pub fn advance(&mut self, now: DateTime<Utc>) -> Result<(), Error> {
        if now < self.now {
            return Err(Error::ClockReversed);
        }
        self.now = now;
        self.moves
            .retain(|_, m| now.signed_duration_since(m.at) < Duration::hours(24));
        self.requests
            .retain(|_, r| now.signed_duration_since(r.at) < Duration::hours(24));
        self.sessions
            .retain(|_, s| now.signed_duration_since(s.touched) < Duration::hours(24));
        for s in self.sessions.values_mut() {
            if s.active_move
                .is_some_and(|id| !self.moves.contains_key(&id))
            {
                s.pending = None;
                s.active_move = None;
                s.cold = false;
            }
        }
        Ok(())
    }

    fn reject<T>(&mut self, error: Error) -> Result<T, Error> {
        self.complete = false;
        Err(error)
    }
    fn id(&mut self) -> Result<u64, Error> {
        let Some(next) = self.next_id.checked_add(1) else {
            return self.reject(Error::IdExhausted);
        };
        self.next_id = next;
        Ok(next)
    }
    fn session(&mut self, id: SessionId, regime: Regime) -> Result<(), Error> {
        if !self.sessions.contains_key(&id) {
            if self.sessions.len() == MAX_SESSIONS {
                return self.reject(Error::Capacity);
            }
            let epoch = self.id()?;
            self.sessions.insert(
                id,
                Session {
                    regime,
                    epoch,
                    baseline: VecDeque::new(),
                    pending: None,
                    active_move: None,
                    cold: false,
                    touched: self.now,
                },
            );
        }
        if self.sessions[&id].regime != regime {
            let epoch = self.id()?;
            let s = self.sessions.get_mut(&id).unwrap();
            s.regime = regime;
            s.epoch = epoch;
            s.baseline.clear();
            s.pending = None; // Old move stays explicitly unknown.
            s.active_move = None;
            s.cold = false;
        }
        self.sessions.get_mut(&id).unwrap().touched = self.now;
        Ok(())
    }

    /// Call once per committed real move, under the same serialization as request start.
    /// Initial placement/alias changes must NOT call this. Unknown target models defer
    /// the baseline model check until the actual destination turn starts.
    pub fn moved(
        &mut self,
        session: SessionId,
        target: Regime,
        now: DateTime<Utc>,
    ) -> Result<MoveId, Error> {
        self.advance(now)?;
        if self.moves.len() == MAX_MOVES {
            return self.reject(Error::Capacity);
        }
        let id = self.id()?;
        let (baseline, baseline_model) = self.sessions.get(&session).map_or((None, None), |s| {
            if s.regime.model != 0
                && s.regime.provider == target.provider
                && (target.model == 0 || s.regime.model == target.model)
            {
                (median(&s.baseline), Some(s.regime.model))
            } else {
                (None, None)
            }
        });
        self.session(session, target)?;
        let s = self.sessions.get_mut(&session).unwrap();
        s.epoch = id; // Invalidate pre-move and superseded cold request tickets.
        s.baseline.clear();
        s.pending = Some(id);
        s.active_move = Some(id);
        s.cold = true;
        self.moves.insert(
            id,
            Move {
                session,
                at: now,
                baseline,
                baseline_model,
                cost: None,
                claimed: false,
            },
        );
        Ok(id)
    }

    /// Claim at TURN START, never at completion. One call per provider turn (including WS).
    pub fn begin(
        &mut self,
        session: SessionId,
        regime: Regime,
        now: DateTime<Utc>,
    ) -> Result<RequestTicket, Error> {
        self.advance(now)?;
        let resolve_model = self.sessions.get(&session).is_some_and(|s| {
            s.cold
                && s.pending.is_some()
                && s.regime.model == 0
                && regime.model != 0
                && s.regime.provider == regime.provider
                && s.regime.account == regime.account
        });
        // Account/provider transitions require a committed move, never a late begin.
        // While a cold claim is pending/in flight, even a model mismatch must not
        // clear the frozen baseline or steal attribution from its destination.
        if self.sessions.get(&session).is_some_and(|s| {
            s.regime.account != regime.account
                || s.regime.provider != regime.provider
                || (s.cold && s.regime != regime && !resolve_model)
        }) {
            return Err(Error::StaleRegime);
        }
        if self.requests.len() == MAX_REQUESTS {
            return self.reject(Error::Capacity);
        }
        let id = self.id()?;
        if resolve_model {
            let s = self.sessions.get_mut(&session).unwrap();
            // Preserve the pending claim and frozen baseline; never use session(),
            // which would treat this resolution as a regime reset and discard them.
            s.regime = regime;
            s.epoch = id;
            s.touched = now;
        } else {
            self.session(session, regime)?;
        }
        let s = self.sessions.get_mut(&session).unwrap();
        let move_id = s.pending.take();
        if let Some(mid) = move_id {
            self.moves.get_mut(&mid).unwrap().claimed = true;
        }
        self.requests.insert(
            id,
            Request {
                session,
                regime,
                epoch: s.epoch,
                at: now,
                move_id,
                normal: !s.cold,
            },
        );
        Ok(RequestTicket {
            owner: self.owner,
            id,
        })
    }

    /// success means a completed normal provider turn; failures/cancellation provide no usage.
    /// The FIRST post-move turn consumes the claim even when usage/baseline is unknown.
    pub fn finish(
        &mut self,
        ticket: RequestTicket,
        success: bool,
        usage: Usage,
        now: DateTime<Utc>,
    ) -> Result<Completion, Error> {
        if ticket.owner != self.owner {
            return Err(Error::WrongOwner);
        }
        self.advance(now)?;
        let r = self
            .requests
            .remove(&ticket.id)
            .ok_or(Error::UnknownTicket)?;
        let Some(s) = self.sessions.get_mut(&r.session) else {
            return Ok(Completion::Stale);
        };
        if s.epoch != r.epoch || s.regime != r.regime {
            return Ok(Completion::Stale);
        }
        s.touched = now;
        let value = (success && r.regime.model != 0)
            .then(|| uncached(r.regime.provider, usage))
            .flatten();
        if let Some(id) = r.move_id {
            let Some(m) = self.moves.get_mut(&id) else {
                return Ok(Completion::Stale);
            };
            debug_assert!(m.claimed && m.session == r.session);
            m.cost = value
                .zip(m.baseline)
                .filter(|_| m.baseline_model == Some(r.regime.model))
                .map(|(v, baseline)| v.saturating_sub(baseline));
            s.cold = false;
            Ok(Completion::Move {
                id,
                reread_tokens: m.cost,
            })
        } else {
            if r.normal
                && let Some(v) = value
            {
                if s.baseline.len() == BASELINE_SAMPLES {
                    s.baseline.pop_front();
                }
                s.baseline.push_back(v);
            }
            Ok(Completion::Normal)
        }
    }

    /// Rolling hour and rolling 24h, not calendar-day totals. Query also performs pruning.
    pub fn totals(&mut self, now: DateTime<Utc>) -> Result<(Aggregate, Aggregate), Error> {
        self.advance(now)?;
        let aggregate = |width| {
            let mut a = Aggregate {
                tokens: 0,
                known_count: 0,
                unknown_count: 0,
                observed_since: self.observed_since,
                recording_complete: self.complete,
                tokens_saturated: false,
            };
            for m in self
                .moves
                .values()
                .filter(|m| now.signed_duration_since(m.at) < width)
            {
                if let Some(cost) = m.cost {
                    a.known_count += 1; // <= MAX_MOVES
                    let (sum, overflow) = a.tokens.overflowing_add(cost);
                    a.tokens = if overflow { u64::MAX } else { sum };
                    a.tokens_saturated |= overflow;
                } else {
                    a.unknown_count += 1;
                }
            }
            a
        };
        Ok((
            aggregate(Duration::hours(1)),
            aggregate(Duration::hours(24)),
        ))
    }
}

/// Coverage always accompanies totals; a restart starts a new observation epoch.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Snapshot {
    pub hour: Aggregate,
    pub rolling24h: Aggregate,
    /// Wall times were clamped to processing order (clock regression or racing callers).
    pub clock_adjusted: bool,
}

const MAX_IDENTITIES: usize = 8192;
const MAX_IDENTITY_BYTES: usize = 256;
struct Identity {
    id: u64,
    touched: DateTime<Utc>,
}

/// Full strings are interned, not truncated/hashed; distinct identities cannot collide.
/// Observability holds ONE mutex across registry and reducer operations.
pub(super) struct Registry {
    core: Accounting,
    identities: BTreeMap<(u8, String), Identity>,
    next_identity: u64,
    clock_adjusted: bool,
}

impl Registry {
    pub(super) fn new(now: DateTime<Utc>) -> Self {
        Self {
            core: Accounting::new(rand::random(), now),
            identities: BTreeMap::new(),
            next_identity: 0,
            clock_adjusted: false,
        }
    }

    fn clock(&mut self, now: DateTime<Utc>) -> DateTime<Utc> {
        self.clock_adjusted |= now < self.core.now;
        let now = now.max(self.core.now);
        // Clamp BEFORE consuming a non-Clone completion ticket.
        self.core.advance(now).expect("registry clock is monotonic");
        let mut live = std::collections::BTreeSet::new();
        for (id, s) in &self.core.sessions {
            live.extend([*id, s.regime.account, s.regime.model]);
        }
        for r in self.core.requests.values() {
            live.extend([r.session, r.regime.account, r.regime.model]);
        }
        for m in self.core.moves.values() {
            live.insert(m.session);
            if let Some(model) = m.baseline_model {
                live.insert(model);
            }
        }
        self.identities.retain(|_, v| {
            live.contains(&v.id) || now.signed_duration_since(v.touched) < Duration::hours(24)
        });
        now
    }

    fn intern(&mut self, namespace: u8, text: &str, now: DateTime<Utc>) -> Result<u64, Error> {
        if text.is_empty() || text.len() > MAX_IDENTITY_BYTES {
            return self.core.reject(Error::InvalidIdentity);
        }
        let key = (namespace, text.to_owned());
        if let Some(identity) = self.identities.get_mut(&key) {
            identity.touched = now;
            return Ok(identity.id);
        }
        if self.identities.len() == MAX_IDENTITIES {
            return self.core.reject(Error::Capacity);
        }
        let Some(id) = self.next_identity.checked_add(1) else {
            return self.core.reject(Error::IdExhausted);
        };
        self.next_identity = id;
        self.identities.insert(key, Identity { id, touched: now });
        Ok(id)
    }

    fn namespace(provider: Provider) -> u8 {
        if provider == Provider::Codex { 0 } else { 1 }
    }

    fn identity(
        &mut self,
        provider: Provider,
        key: &str,
        account: &str,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(SessionId, Regime), Error> {
        let namespace = Self::namespace(provider);
        let session = self.intern(namespace, key, now)?;
        let account = self.intern(namespace + 2, account, now)?;
        // Zero is reserved for unknown model: it can never train or supply a baseline.
        let model = model
            .map(|m| self.intern(namespace + 4, m, now))
            .transpose()?
            .unwrap_or(0);
        Ok((
            session,
            Regime {
                provider,
                account,
                model,
            },
        ))
    }

    pub(super) fn moved(
        &mut self,
        provider: Provider,
        key: &str,
        account: &str,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<MoveId, Error> {
        let now = self.clock(now);
        let (session, regime) = self.identity(provider, key, account, model, now)?;
        self.core.moved(session, regime, now)
    }
    pub(super) fn begin(
        &mut self,
        provider: Provider,
        key: &str,
        account: &str,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<RequestTicket, Error> {
        let now = self.clock(now);
        let (session, regime) = self.identity(provider, key, account, model, now)?;
        self.core.begin(session, regime, now)
    }
    pub(super) fn finish(
        &mut self,
        ticket: RequestTicket,
        success: bool,
        usage: Option<&Usage>,
        now: DateTime<Utc>,
    ) -> Result<Completion, Error> {
        let now = self.clock(now);
        self.core
            .finish(ticket, success, usage.copied().unwrap_or_default(), now)
    }
    pub(super) fn snapshot(&mut self, now: DateTime<Utc>) -> Snapshot {
        let now = self.clock(now);
        let (hour, rolling24h) = self.core.totals(now).expect("registry clock is monotonic");
        Snapshot {
            hour,
            rolling24h,
            clock_adjusted: self.clock_adjusted,
        }
    }
    pub(super) fn alias(
        &mut self,
        provider: Provider,
        existing: &str,
        alias: &str,
        now: DateTime<Utc>,
    ) -> Result<(), Error> {
        let now = self.clock(now);
        let namespace = Self::namespace(provider);
        let id = self.intern(namespace, existing, now)?;
        if alias.is_empty() || alias.len() > MAX_IDENTITY_BYTES {
            return self.core.reject(Error::InvalidIdentity);
        }
        let key = (namespace, alias.to_owned());
        if let Some(other) = self.identities.get(&key) {
            return if other.id == id {
                Ok(())
            } else {
                self.core.reject(Error::AliasConflict)
            };
        }
        if self.identities.len() == MAX_IDENTITIES {
            return self.core.reject(Error::Capacity);
        }
        self.identities.insert(key, Identity { id, touched: now });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + seconds, 0).unwrap()
    }
    fn regime(account: u64) -> Regime {
        Regime {
            provider: Provider::Codex,
            account,
            model: 1,
        }
    }
    fn usage(v: u64) -> Usage {
        Usage {
            input_tokens: Some(v),
            cached_tokens: Some(0),
            cache_creation_input_tokens: None,
        }
    }
    fn normal(a: &mut Accounting, value: u64, at: i64) {
        let ticket = a.begin(1, regime(1), t(at)).unwrap();
        assert_eq!(
            a.finish(ticket, true, usage(value), t(at)).unwrap(),
            Completion::Normal
        );
    }
    #[test]
    fn provider_arithmetic_is_explicit_and_checked() {
        let u = Usage {
            input_tokens: Some(20),
            cached_tokens: Some(12),
            cache_creation_input_tokens: Some(7),
        };
        assert_eq!(uncached(Provider::Codex, u), Some(8));
        assert_eq!(uncached(Provider::Claude, u), Some(27));
        assert_eq!(
            uncached(
                Provider::Codex,
                Usage {
                    cached_tokens: None,
                    ..u
                }
            ),
            None
        );
        assert_eq!(
            uncached(
                Provider::Codex,
                Usage {
                    cached_tokens: Some(21),
                    ..u
                }
            ),
            None
        );
        assert_eq!(
            uncached(
                Provider::Claude,
                Usage {
                    cache_creation_input_tokens: None,
                    ..u
                }
            ),
            None
        );
        assert_eq!(
            uncached(
                Provider::Claude,
                Usage {
                    input_tokens: Some(u64::MAX),
                    ..u
                }
            ),
            None
        );
        assert_eq!(
            uncached(
                Provider::Claude,
                Usage {
                    cached_tokens: None,
                    ..u
                }
            ),
            Some(27)
        );
    }
    #[test]
    fn bounded_pre_move_median_and_positive_excess() {
        let mut a = Accounting::new(1, t(0));
        for (i, v) in [999, 10, 20, 30, 40, 50, 60, 70, 80]
            .into_iter()
            .enumerate()
        {
            normal(&mut a, v, i as i64);
        }
        let mid = a.moved(1, regime(2), t(10)).unwrap();
        assert_eq!(a.moves[&mid].baseline, Some(45));
        let ticket = a.begin(1, regime(2), t(11)).unwrap();
        assert_eq!(
            a.finish(ticket, true, usage(100), t(12)).unwrap(),
            Completion::Move {
                id: mid,
                reread_tokens: Some(55)
            }
        );
        assert!(a.sessions[&1].baseline.is_empty());
        assert_eq!(
            median(&VecDeque::from([u64::MAX, u64::MAX])),
            Some(u64::MAX)
        );
        assert_eq!(100_u64.saturating_sub(200), 0);
    }
    #[test]
    fn old_completion_cannot_claim_and_two_moves_do_not_double_charge() {
        let mut a = Accounting::new(1, t(0));
        normal(&mut a, 10, 0);
        let old = a.begin(1, regime(1), t(1)).unwrap();
        let first = a.moved(1, regime(2), t(2)).unwrap();
        let cold = a.begin(1, regime(2), t(3)).unwrap();
        let second = a.moved(1, regime(3), t(4)).unwrap();
        assert_eq!(
            a.finish(old, true, usage(999), t(5)).unwrap(),
            Completion::Stale
        );
        assert_eq!(
            a.finish(cold, true, usage(999), t(6)).unwrap(),
            Completion::Stale
        );
        let next = a.begin(1, regime(3), t(7)).unwrap();
        assert_eq!(
            a.finish(next, true, usage(100), t(8)).unwrap(),
            Completion::Move {
                id: second,
                reread_tokens: None
            }
        );
        assert_eq!(a.moves[&first].cost, None);
        assert_eq!(a.totals(t(8)).unwrap().0.unknown_count, 2);
    }
    #[test]
    fn first_unknown_turn_is_not_reassigned_to_later_turn() {
        let mut a = Accounting::new(1, t(0));
        normal(&mut a, 10, 0);
        let id = a.moved(1, regime(2), t(1)).unwrap();
        let first = a.begin(1, regime(2), t(2)).unwrap();
        let concurrent = a.begin(1, regime(2), t(3)).unwrap();
        assert_eq!(
            a.finish(first, false, usage(900), t(4)).unwrap(),
            Completion::Move {
                id,
                reread_tokens: None
            }
        );
        a.finish(concurrent, true, usage(900), t(5)).unwrap();
        assert!(a.sessions[&1].baseline.is_empty());
        let later = a.begin(1, regime(2), t(6)).unwrap();
        a.finish(later, true, usage(20), t(7)).unwrap();
        assert_eq!(a.sessions[&1].baseline, VecDeque::from([20]));
        assert_eq!(a.moves[&id].cost, None);
    }
    #[test]
    fn regime_change_and_stable_alias_identity() {
        let mut a = Accounting::new(1, t(0));
        normal(&mut a, 10, 0);
        // Alias names stay outside the reducer: both resolve to canonical ID 1.
        let alias = a.begin(1, regime(1), t(1)).unwrap();
        a.finish(alias, true, usage(20), t(2)).unwrap();
        assert_eq!(median(&a.sessions[&1].baseline), Some(15));
        let changed = Regime {
            model: 2,
            ..regime(2)
        };
        let mid = a.moved(1, changed, t(3)).unwrap();
        assert_eq!(a.moves[&mid].baseline, None);
        let ticket = a.begin(1, changed, t(4)).unwrap();
        a.finish(ticket, true, usage(100), t(5)).unwrap();
        let reset = a
            .begin(
                1,
                Regime {
                    model: 9,
                    ..changed
                },
                t(6),
            )
            .unwrap();
        assert!(a.sessions[&1].baseline.is_empty());
        a.finish(reset, true, usage(30), t(7)).unwrap();
        assert_eq!(median(&a.sessions[&1].baseline), Some(30));
    }
    #[test]
    fn observation_boundaries_pruning_caps_and_clock() {
        let mut a = Accounting::new(1, t(0));
        a.moved(1, regime(1), t(1)).unwrap();
        let (hour, day) = a.totals(t(3601)).unwrap();
        assert_eq!((hour.unknown_count, day.unknown_count), (0, 1));
        assert_eq!(day.observed_since, t(0));
        assert_eq!(a.totals(t(86401)).unwrap().1.unknown_count, 0);
        assert!(a.moves.is_empty() && a.sessions.is_empty());
        assert_eq!(a.advance(t(0)), Err(Error::ClockReversed));
        let mut a = Accounting::new(2, t(0));
        for _ in 0..MAX_MOVES {
            a.moved(1, regime(1), t(0)).unwrap();
        }
        assert_eq!(a.moved(1, regime(1), t(0)), Err(Error::Capacity));
        assert!(!a.totals(t(0)).unwrap().1.recording_complete);
        assert_eq!(a.moves.len(), MAX_MOVES);
    }
    #[test]
    fn known_zero_unknown_and_sum_saturation_are_distinct() {
        let mut a = Accounting::new(1, t(0));
        normal(&mut a, 20, 0);
        let id = a.moved(1, regime(2), t(1)).unwrap();
        let ticket = a.begin(1, regime(2), t(2)).unwrap();
        a.finish(ticket, true, usage(10), t(3)).unwrap();
        assert_eq!(a.moves[&id].cost, Some(0));
        a.moved(2, regime(1), t(4)).unwrap(); // No baseline => unknown.
        let totals = a.totals(t(5)).unwrap().1;
        assert_eq!(
            (totals.tokens, totals.known_count, totals.unknown_count),
            (0, 1, 1)
        );
        // Synthetic numeric extreme, independent of baseline training.
        for m in a.moves.values_mut() {
            m.cost = Some(u64::MAX);
        }
        let totals = a.totals(t(6)).unwrap().1;
        assert_eq!(totals.tokens, u64::MAX);
        assert!(totals.tokens_saturated);
    }
    #[test]
    fn successful_missing_usage_is_unknown_even_with_baseline() {
        let mut a = Accounting::new(1, t(0));
        normal(&mut a, 10, 0);
        let id = a.moved(1, regime(2), t(1)).unwrap();
        let ticket = a.begin(1, regime(2), t(2)).unwrap();
        assert_eq!(
            a.finish(ticket, true, Usage::default(), t(3)).unwrap(),
            Completion::Move {
                id,
                reread_tokens: None
            }
        );
        assert_eq!(a.totals(t(3)).unwrap().0.unknown_count, 1);
    }
    #[test]
    fn claim_expiry_unfreezes_active_session_without_reusing_attribution() {
        let mut a = Accounting::new(1, t(0));
        a.moved(1, regime(1), t(0)).unwrap();
        let cold = a.begin(1, regime(1), t(1)).unwrap();
        // Keep session alive without resolving the first claim.
        let concurrent = a.begin(1, regime(1), t(86000)).unwrap();
        a.finish(concurrent, true, usage(900), t(86001)).unwrap();
        assert!(a.sessions[&1].cold);
        a.advance(t(86400)).unwrap();
        assert!(!a.sessions[&1].cold);
        assert_eq!(
            a.finish(cold, true, usage(900), t(86400)).unwrap(),
            Completion::Stale
        );
        assert!(a.sessions[&1].baseline.is_empty());
    }
    #[test]
    fn session_request_caps_and_id_exhaustion_are_explicit() {
        let mut a = Accounting::new(1, t(0));
        for session in 0..MAX_SESSIONS as u64 {
            a.begin(session, regime(1), t(0)).unwrap();
        }
        assert!(matches!(
            a.begin(MAX_SESSIONS as u64, regime(1), t(0)),
            Err(Error::Capacity)
        ));
        assert_eq!(a.sessions.len(), MAX_SESSIONS);
        let mut a = Accounting::new(2, t(0));
        for _ in 0..MAX_REQUESTS {
            a.begin(1, regime(1), t(0)).unwrap();
        }
        assert!(matches!(a.begin(1, regime(1), t(0)), Err(Error::Capacity)));
        assert_eq!(a.requests.len(), MAX_REQUESTS);
        let mut a = Accounting::new(3, t(0));
        a.next_id = u64::MAX;
        assert!(matches!(
            a.begin(1, regime(1), t(0)),
            Err(Error::IdExhausted)
        ));
        assert!(!a.complete);
    }
    #[test]
    fn wrong_owner_and_replay_cannot_consume_real_claim() {
        let mut a = Accounting::new(1, t(0));
        a.moved(1, regime(1), t(0)).unwrap();
        let ticket = a.begin(1, regime(1), t(0)).unwrap();
        // Only this child test module can construct a duplicate private ticket.
        let replay = RequestTicket {
            owner: ticket.owner,
            id: ticket.id,
        };
        let foreign = RequestTicket {
            owner: 99,
            id: ticket.id,
        };
        assert_eq!(
            a.finish(foreign, true, usage(10), t(0)),
            Err(Error::WrongOwner)
        );
        assert!(matches!(
            a.finish(ticket, true, usage(10), t(0)),
            Ok(Completion::Move { .. })
        ));
        assert_eq!(
            a.finish(replay, true, usage(10), t(0)),
            Err(Error::UnknownTicket)
        );
    }

    #[test]
    fn unknown_model_move_resolves_at_destination_and_measures_matching_source_model() {
        let mut a = Accounting::new(1, t(0));
        normal(&mut a, 20, 0);
        let unknown = Regime {
            model: 0,
            ..regime(2)
        };
        let id = a.moved(1, unknown, t(1)).unwrap();
        assert_eq!(a.moves[&id].baseline, Some(20));
        assert_eq!(a.moves[&id].baseline_model, Some(1));
        assert_eq!(a.totals(t(1)).unwrap().0.unknown_count, 1);
        assert!(matches!(
            a.begin(1, regime(1), t(2)),
            Err(Error::StaleRegime)
        ));
        assert_eq!(a.sessions[&1].regime, unknown);
        assert_eq!(a.sessions[&1].pending, Some(id));
        let create = a.begin(1, regime(2), t(3)).unwrap();
        assert_eq!(a.sessions[&1].regime, regime(2));
        // The resolution is one-shot; a concurrent different model cannot steal it.
        assert!(matches!(
            a.begin(
                1,
                Regime {
                    model: 2,
                    ..regime(2)
                },
                t(4)
            ),
            Err(Error::StaleRegime)
        ));
        assert_eq!(
            a.finish(create, true, usage(200), t(5)).unwrap(),
            Completion::Move {
                id,
                reread_tokens: Some(180)
            }
        );
        assert!(a.sessions[&1].baseline.is_empty());
        let normal = a.begin(1, regime(2), t(6)).unwrap();
        assert_eq!(
            a.finish(normal, true, usage(30), t(7)).unwrap(),
            Completion::Normal
        );
        assert_eq!(median(&a.sessions[&1].baseline), Some(30));
        assert!(matches!(
            a.begin(1, regime(1), t(8)),
            Err(Error::StaleRegime)
        ));
        let totals = a.totals(t(8)).unwrap().0;
        assert_eq!(
            (totals.tokens, totals.known_count, totals.unknown_count),
            (180, 1, 0)
        );
    }

    #[test]
    fn unknown_model_move_with_different_actual_model_is_unknown_then_trains_new_model() {
        let mut a = Accounting::new(1, t(0));
        normal(&mut a, 20, 0);
        let id = a
            .moved(
                1,
                Regime {
                    model: 0,
                    ..regime(2)
                },
                t(1),
            )
            .unwrap();
        let actual = Regime {
            model: 2,
            ..regime(2)
        };
        let create = a.begin(1, actual, t(2)).unwrap();
        assert_eq!(
            a.finish(create, true, usage(200), t(3)).unwrap(),
            Completion::Move {
                id,
                reread_tokens: None
            }
        );
        assert!(a.sessions[&1].baseline.is_empty());
        let normal = a.begin(1, actual, t(4)).unwrap();
        assert_eq!(
            a.finish(normal, true, usage(40), t(5)).unwrap(),
            Completion::Normal
        );
        assert_eq!(median(&a.sessions[&1].baseline), Some(40));
        let second = a
            .moved(
                1,
                Regime {
                    account: 3,
                    ..actual
                },
                t(6),
            )
            .unwrap();
        let create = a
            .begin(
                1,
                Regime {
                    account: 3,
                    ..actual
                },
                t(7),
            )
            .unwrap();
        assert_eq!(
            a.finish(create, true, usage(100), t(8)).unwrap(),
            Completion::Move {
                id: second,
                reread_tokens: Some(60)
            }
        );
        let totals = a.totals(t(8)).unwrap().0;
        assert_eq!(
            (totals.tokens, totals.known_count, totals.unknown_count),
            (60, 1, 1)
        );
    }

    #[test]
    fn registry_keeps_frozen_model_identity_until_unknown_target_resolves() {
        let mut r = Registry::new(t(0));
        let normal = r.begin(Provider::Codex, "s", "a", Some("m"), t(0)).unwrap();
        r.finish(normal, true, Some(&usage(20)), t(0)).unwrap();
        let id = r.moved(Provider::Codex, "s", "b", None, t(86399)).unwrap();
        assert!(matches!(
            r.begin(Provider::Codex, "s", "a", Some("m"), t(86400)),
            Err(Error::StaleRegime)
        ));
        // The model interner's own 24h TTL elapsed, but this move still needs its identity.
        let create = r
            .begin(Provider::Codex, "s", "b", Some("m"), t(86401))
            .unwrap();
        assert_eq!(
            r.finish(create, true, Some(&usage(200)), t(86402)).unwrap(),
            Completion::Move {
                id,
                reread_tokens: Some(180)
            }
        );
        let normal = r
            .begin(Provider::Codex, "s", "b", Some("m"), t(86403))
            .unwrap();
        assert_eq!(
            r.finish(normal, true, Some(&usage(30)), t(86404)).unwrap(),
            Completion::Normal
        );
        let snapshot = r.snapshot(t(86404));
        assert_eq!(
            (
                snapshot.hour.tokens,
                snapshot.hour.known_count,
                snapshot.hour.unknown_count
            ),
            (180, 1, 0)
        );
    }

    #[test]
    fn stale_begin_cannot_clear_pending_or_claimed_move_regime() {
        let mut a = Accounting::new(1, t(0));
        normal(&mut a, 10, 0);
        let id = a.moved(1, regime(2), t(1)).unwrap();
        let epoch = a.sessions[&1].epoch;
        assert!(matches!(
            a.begin(1, regime(1), t(2)),
            Err(Error::StaleRegime)
        ));
        let wrong_model = Regime {
            model: 9,
            ..regime(2)
        };
        assert!(matches!(
            a.begin(1, wrong_model, t(3)),
            Err(Error::StaleRegime)
        ));
        assert_eq!(a.sessions[&1].epoch, epoch);
        assert_eq!(a.sessions[&1].pending, Some(id));
        assert!(!a.moves[&id].claimed);
        assert_eq!(a.moves[&id].baseline, Some(10));
        let destination = a.begin(1, regime(2), t(4)).unwrap();
        assert!(matches!(
            a.begin(1, regime(1), t(5)),
            Err(Error::StaleRegime)
        ));
        assert!(matches!(
            a.begin(1, wrong_model, t(6)),
            Err(Error::StaleRegime)
        ));
        assert_eq!(
            a.finish(destination, true, usage(100), t(7)).unwrap(),
            Completion::Move {
                id,
                reread_tokens: Some(90)
            }
        );
        assert!(matches!(
            a.begin(1, regime(1), t(8)),
            Err(Error::StaleRegime)
        ));
        let totals = a.totals(t(8)).unwrap().0;
        assert_eq!(
            (totals.tokens, totals.known_count, totals.unknown_count),
            (90, 1, 0)
        );
        assert!(totals.recording_complete);
    }

    #[test]
    fn registry_stale_account_begin_preserves_destination_ticket_and_baseline() {
        let mut r = Registry::new(t(0));
        let normal = r.begin(Provider::Codex, "s", "a", Some("m"), t(0)).unwrap();
        r.finish(normal, true, Some(&usage(10)), t(1)).unwrap();
        let id = r.moved(Provider::Codex, "s", "b", Some("m"), t(2)).unwrap();
        assert!(matches!(
            r.begin(Provider::Codex, "s", "a", Some("m"), t(3)),
            Err(Error::StaleRegime)
        ));
        let destination = r.begin(Provider::Codex, "s", "b", Some("m"), t(4)).unwrap();
        assert_eq!(
            r.finish(destination, true, Some(&usage(100)), t(5))
                .unwrap(),
            Completion::Move {
                id,
                reread_tokens: Some(90)
            }
        );
        let snapshot = r.snapshot(t(5));
        assert_eq!(
            (snapshot.hour.known_count, snapshot.hour.unknown_count),
            (1, 0)
        );
        assert!(snapshot.hour.recording_complete);
    }

    #[test]
    fn registry_alias_preserves_baseline_and_claim_across_adoption() {
        let mut r = Registry::new(t(0));
        let ticket = r
            .begin(Provider::Codex, "provisional", "a", Some("m"), t(0))
            .unwrap();
        r.finish(ticket, true, Some(&usage(10)), t(1)).unwrap();
        r.alias(Provider::Codex, "provisional", "canonical", t(2))
            .unwrap();
        let id = r
            .moved(Provider::Codex, "canonical", "b", Some("m"), t(3))
            .unwrap();
        let ticket = r
            .begin(Provider::Codex, "provisional", "b", Some("m"), t(4))
            .unwrap();
        assert_eq!(
            r.finish(ticket, true, Some(&usage(100)), t(5)).unwrap(),
            Completion::Move {
                id,
                reread_tokens: Some(90)
            }
        );
        assert_eq!(r.snapshot(t(5)).hour.known_count, 1);
        assert_eq!(
            r.identities[&(0, "canonical".into())].id,
            r.identities[&(0, "provisional".into())].id
        );
    }

    #[test]
    fn registry_clamps_racing_clocks_without_losing_ticket() {
        let mut r = Registry::new(t(0));
        let ticket = r
            .begin(Provider::Codex, "s", "a", Some("m"), t(10))
            .unwrap();
        assert_eq!(
            r.finish(ticket, true, Some(&usage(10)), t(5)).unwrap(),
            Completion::Normal
        );
        let snapshot = r.snapshot(t(0));
        assert!(snapshot.clock_adjusted);
        assert!(snapshot.hour.recording_complete);
        assert_eq!(snapshot.hour.observed_since, t(0));
        assert_eq!(r.core.now, t(10));
        assert!(r.core.requests.is_empty());
    }

    #[test]
    fn registry_missing_model_cannot_train_or_invent_baseline() {
        let mut r = Registry::new(t(0));
        let ticket = r.begin(Provider::Claude, "s", "a", None, t(0)).unwrap();
        let usage = Usage {
            input_tokens: Some(10),
            cached_tokens: Some(100),
            cache_creation_input_tokens: Some(20),
        };
        r.finish(ticket, true, Some(&usage), t(1)).unwrap();
        r.moved(Provider::Claude, "s", "b", None, t(2)).unwrap();
        let ticket = r.begin(Provider::Claude, "s", "b", None, t(3)).unwrap();
        assert!(matches!(
            r.finish(ticket, true, Some(&usage), t(4)).unwrap(),
            Completion::Move {
                reread_tokens: None,
                ..
            }
        ));
        assert_eq!(r.snapshot(t(4)).hour.unknown_count, 1);
    }

    #[test]
    fn registry_identity_bounds_namespace_conflicts_and_pruning() {
        let mut r = Registry::new(t(0));
        let codex = r.intern(0, "s", t(0)).unwrap();
        let claude = r.intern(1, "s", t(0)).unwrap();
        assert_ne!(codex, claude);
        r.intern(0, "other", t(0)).unwrap();
        assert_eq!(
            r.alias(Provider::Codex, "s", "other", t(0)),
            Err(Error::AliasConflict)
        );
        assert_eq!(
            r.intern(0, &"x".repeat(MAX_IDENTITY_BYTES + 1), t(0)),
            Err(Error::InvalidIdentity)
        );
        for i in r.identities.len()..MAX_IDENTITIES {
            r.intern(4, &format!("m{i}"), t(0)).unwrap();
        }
        assert_eq!(r.intern(4, "overflow", t(0)), Err(Error::Capacity));
        assert_eq!(r.identities.len(), MAX_IDENTITIES);
        assert!(!r.snapshot(t(0)).hour.recording_complete);
        r.snapshot(t(86400));
        assert!(r.identities.is_empty());
    }
}
