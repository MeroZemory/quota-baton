//! Ephemeral warning accounting; no body access, persistence, or client logging.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

const WINDOW: Duration = Duration::from_secs(600);
const MAX_CLIENTS: usize = 4096;
const MAX_SESSIONS: usize = 65_536;
pub(crate) type Identity = [u8; 32];

// No Debug/Serialize: these identities must never enter warning/log payloads.
pub(crate) fn client_identity(headers: &HeaderMap) -> Option<Identity> {
    fn single<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?.to_str().ok()?.trim();
        (!value.is_empty() && values.next().is_none()).then_some(value)
    }
    let (domain, value) = single(headers, "originator")
        .map(|value| (b"originator:".as_slice(), value))
        .or_else(|| single(headers, "user-agent").map(|value| (b"ua:".as_slice(), value)))?;
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(value.as_bytes());
    Some(hash.finalize().into())
}

pub(crate) struct BurstTracker {
    threshold: u32,
    counts: HashMap<Identity, u32>,
    seen: HashSet<(Identity, Identity)>,
    expiry: VecDeque<(Instant, Identity, Identity)>,
    last_now: Option<Instant>,
}

impl BurstTracker {
    pub(crate) fn new(threshold: u32) -> Self {
        Self {
            threshold,
            counts: HashMap::new(),
            seen: HashSet::new(),
            expiry: VecDeque::new(),
            last_now: None,
        }
    }

    // Caller supplies monotonic time; expiry is lazy, also bounded while idle.
    pub(crate) fn expire(&mut self, now: Instant) {
        let now = self.last_now.map_or(now, |last| last.max(now));
        self.last_now = Some(now);
        while self
            .expiry
            .front()
            .is_some_and(|(at, _, _)| now.duration_since(*at) >= WINDOW)
        {
            let (_, client, session) = self.expiry.pop_front().expect("checked front");
            self.seen.remove(&(client, session));
            if let Some(count) = self.counts.get_mut(&client) {
                *count -= 1;
                if *count == 0 {
                    self.counts.remove(&client);
                }
            }
        }
    }

    /// The crossing event is sparse, but an ongoing excess remains observable
    /// to a tray that subscribes after that event. Call after new_placement/expire.
    pub(crate) fn excess(&self, client: Identity) -> Option<u32> {
        self.counts
            .get(&client)
            .copied()
            .filter(|count| *count > self.threshold)
    }

    /// Call ONLY for a successful bound-group placement with reason == "new".
    /// The session argument is the canonical MoveEvent.key, not a raw alias.
    /// Returns the observed distinct count when crossing the threshold, never any identity.
    pub(crate) fn new_placement(
        &mut self,
        now: Instant,
        client: Identity,
        session: &str,
    ) -> Option<u32> {
        self.expire(now);
        let now = self.last_now.expect("expire sets clock");
        let session: Identity = Sha256::digest(session.as_bytes()).into();
        if self.seen.contains(&(client, session))
            || self.seen.len() >= MAX_SESSIONS
            || (!self.counts.contains_key(&client) && self.counts.len() >= MAX_CLIENTS)
        {
            return None;
        }
        self.seen.insert((client, session));
        self.expiry.push_back((now, client, session));
        let count = self.counts.entry(client).or_default();
        let previous = *count;
        *count += 1;
        (previous <= self.threshold && *count > self.threshold).then_some(*count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_is_strict_and_reports_observed_count() {
        let mut tracker = BurstTracker::new(30);
        let now = Instant::now();
        for n in 0..30 {
            assert_eq!(tracker.new_placement(now, [1; 32], &n.to_string()), None);
        }
        assert_eq!(tracker.new_placement(now, [1; 32], "31"), Some(31));
        assert_eq!(tracker.new_placement(now, [1; 32], "32"), None);
    }

    #[test]
    fn continuous_excess_stays_observable_after_crossing_event_ages_out() {
        let mut tracker = BurstTracker::new(30);
        let start = Instant::now();
        let mut events = 0;
        for n in 0..=1200 {
            events += usize::from(
                tracker
                    .new_placement(start + Duration::from_secs(n), [7; 32], &n.to_string())
                    .is_some(),
            );
            if n >= 30 {
                assert!(tracker.excess([7; 32]).is_some());
            }
        }
        assert_eq!(
            events, 1,
            "one event per threshold crossing, no ledger spam"
        );
        assert_eq!(tracker.excess([7; 32]), Some(600));
        tracker.expire(start + Duration::from_secs(1801));
        assert_eq!(tracker.excess([7; 32]), None);
    }

    #[test]
    fn rolling_window_expires_at_exact_boundary_without_refreshing_duplicates() {
        let mut tracker = BurstTracker::new(1);
        let now = Instant::now();
        assert_eq!(tracker.new_placement(now, [1; 32], "a"), None);
        assert_eq!(
            tracker.new_placement(now + Duration::from_secs(599), [1; 32], "a"),
            None
        );
        assert_eq!(tracker.new_placement(now + WINDOW, [1; 32], "b"), None);
        assert_eq!(tracker.new_placement(now + WINDOW, [1; 32], "c"), Some(2));
        tracker.expire(now + WINDOW + WINDOW);
        assert!(tracker.seen.is_empty());
        assert!(tracker.counts.is_empty());
        assert!(tracker.expiry.is_empty());
    }

    #[test]
    fn clients_are_isolated_and_repeated_placement_is_deduplicated() {
        let mut tracker = BurstTracker::new(1);
        let now = Instant::now();
        assert_eq!(tracker.new_placement(now, [1; 32], "same"), None);
        assert_eq!(tracker.new_placement(now, [2; 32], "same"), None);
        assert_eq!(tracker.new_placement(now, [1; 32], "same"), None);
        assert_eq!(tracker.new_placement(now, [1; 32], "other"), Some(2));
        assert_eq!(tracker.counts[&[2; 32]], 1);
    }

    #[test]
    fn window_rolls_per_session_instead_of_resetting_all_counts() {
        let mut tracker = BurstTracker::new(1);
        let now = Instant::now();
        assert_eq!(tracker.new_placement(now, [1; 32], "a"), None);
        assert_eq!(
            tracker.new_placement(now + Duration::from_secs(300), [1; 32], "b"),
            Some(2)
        );
        assert_eq!(tracker.new_placement(now + WINDOW, [1; 32], "c"), Some(2));
        tracker.expire(now + Duration::from_secs(900));
        assert_eq!(tracker.counts[&[1; 32]], 1);
    }

    #[test]
    fn config_has_c12_default_override_and_unsigned_type() {
        let default: crate::config::Config = toml::from_str("").unwrap();
        assert_eq!(default.tray.burst_new_sessions, 30);
        let configured: crate::config::Config =
            toml::from_str("[tray]\nburst_new_sessions = 2\n").unwrap();
        assert_eq!(configured.tray.burst_new_sessions, 2);
        for value in ["-1", "1.5", "\"30\"", "4294967296"] {
            assert!(
                toml::from_str::<crate::config::Config>(&format!(
                    "[tray]\nburst_new_sessions = {value}\n"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn identity_prefers_originator_and_falls_back_to_ua_without_anonymous_bucket() {
        let mut headers = HeaderMap::new();
        assert!(client_identity(&headers).is_none());
        headers.insert("user-agent", "fixture/1".parse().unwrap());
        let ua = client_identity(&headers).unwrap();
        headers.insert("originator", "fixture".parse().unwrap());
        let originator = client_identity(&headers).unwrap();
        assert_ne!(ua, originator);
        headers.insert("user-agent", "fixture/2".parse().unwrap());
        assert_eq!(client_identity(&headers), Some(originator));
        headers.append("originator", "ambiguous".parse().unwrap());
        headers.remove("user-agent");
        assert!(client_identity(&headers).is_none());
    }

    #[test]
    fn memory_caps_drop_new_observations_and_expiry_recovers_capacity() {
        let mut tracker = BurstTracker::new(0);
        let now = Instant::now();
        for n in 0..MAX_SESSIONS {
            tracker.new_placement(now, [1; 32], &n.to_string());
        }
        assert_eq!(tracker.new_placement(now, [1; 32], "overflow"), None);
        assert_eq!(tracker.expiry.len(), MAX_SESSIONS);
        assert_eq!(
            tracker.new_placement(now + WINDOW, [1; 32], "fresh"),
            Some(1)
        );
        tracker.expire(now + WINDOW + WINDOW);
        for n in 0..MAX_CLIENTS {
            let client = Sha256::digest(n.to_le_bytes()).into();
            tracker.new_placement(now + WINDOW + WINDOW, client, "one");
        }
        assert_eq!(
            tracker.new_placement(now + WINDOW + WINDOW, [2; 32], "overflow"),
            None
        );
        assert_eq!(tracker.counts.len(), MAX_CLIENTS);
    }
}
