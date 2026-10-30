//! Process-local measurements. Cold starts are unweighted; no historical identity join.
use crate::accounts::Account;
pub(crate) use crate::bindings::kernel::selection_capacity::{
    CapacityFacts, CapacitySample, WindowSamples, remaining_tokens,
};
use crate::bindings::kernel::selection_capacity::{compatible, known_plan, observation, supported};
use crate::observability::RequestRecord;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_SAMPLES: usize = 16;
const MAX_WINDOWS: usize = 16;
const MAX_ACCOUNTS: usize = 256;
const MAX_AGE_SECS: i64 = 300;
const MAX_INTERVAL_SECS: i64 = 86_400;

struct Baseline {
    at: DateTime<Utc>,
    last_at: DateTime<Utc>,
    reset: DateTime<Utc>,
    used: f64,
    tokens: u64,
    latest_completion: Option<DateTime<Utc>>,
}

struct AccountMeasurements {
    facts: CapacityFacts,
    baselines: BTreeMap<String, Baseline>,
}

#[derive(Default)]
pub(crate) struct Measurements {
    accounts: BTreeMap<String, AccountMeasurements>,
    completions: BTreeMap<String, DateTime<Utc>>,
    plan_fences: BTreeMap<String, (u64, DateTime<Utc>)>,
    unknown_completion: Option<DateTime<Utc>>,
    unclocked_traffic: bool,
    // Unattributable traffic fences every open interval, including absent IDs.
    unknown_traffic: u64,
}

impl Measurements {
    /// Feed each completed native record exactly once, before the next complete poll.
    pub(crate) fn record_request(&mut self, record: &RequestRecord) {
        if record.attempts == 0 && record.attempt_details.is_empty() {
            return;
        }
        let mut ids = BTreeSet::new();
        if let Some(id) = &record.account {
            ids.insert(id.as_str());
        }
        for attempt in &record.attempt_details {
            ids.insert(attempt.account.as_str());
        }
        let attributed = record.account.as_deref().filter(|id| {
            !id.is_empty()
                && record.attempts == 1
                && record.attempt_details.len() <= 1
                && record.attempt_details.iter().all(|a| a.account == *id)
        });
        let timing = record.ts.parse::<DateTime<Utc>>().ok().and_then(|start| {
            let ms = i64::try_from(record.dur_ms).ok()?;
            Some((start, start.checked_add_signed(Duration::milliseconds(ms))?))
        });
        let tokens = record.cache_usage.and_then(|u| u.input_tokens);
        // Poll timestamps describe dispatch, not response application. Keep
        // completion evidence even without a baseline for delayed first polls.
        if let Some((_, end)) = timing {
            if ids.is_empty() {
                self.unknown_completion =
                    Some(self.unknown_completion.map_or(end, |at| at.max(end)));
            }
            for id in &ids {
                if !self.completions.contains_key(*id) && self.completions.len() >= MAX_ACCOUNTS {
                    self.unclocked_traffic = true;
                    break;
                }
                self.completions
                    .entry((*id).to_owned())
                    .and_modify(|at| *at = (*at).max(end))
                    .or_insert(end);
            }
        } else {
            // Unknown timing cannot be repaired by inventing an observation.
            self.unclocked_traffic = true;
        }
        if let (Some(id), Some((start, end)), Some(tokens)) = (attributed, timing, tokens) {
            if let Some(state) = self.accounts.get_mut(id) {
                if record.family != state.facts.provider.as_str() || start.timestamp() < 0 {
                    state.baselines.clear();
                    return;
                }
                state.baselines.retain(|_, b| {
                    // Equality is ambiguous at the poll boundary, too.
                    if start <= b.at {
                        return false;
                    }
                    let Some(sum) = b.tokens.checked_add(tokens) else {
                        return false;
                    };
                    b.tokens = sum;
                    b.latest_completion = Some(b.latest_completion.map_or(end, |old| old.max(end)));
                    true
                });
            }
            return;
        }
        // Missing attempts may hide additional IDs even when the final ID is known.
        let unknown = ids.is_empty()
            || ids.contains("")
            || (record.attempts as usize > record.attempt_details.len() && record.attempts != 1);
        if unknown {
            if let Some((_, end)) = timing {
                self.unknown_completion =
                    Some(self.unknown_completion.map_or(end, |at| at.max(end)));
            }
            self.unknown_traffic = self.unknown_traffic.saturating_add(1);
            for state in self.accounts.values_mut() {
                state.baselines.clear();
            }
        } else {
            for id in ids {
                if let Some(state) = self.accounts.get_mut(id) {
                    state.baselines.clear();
                }
            }
        }
    }

    /// `blocked` includes outside protection, stale evidence and in-flight requests.
    /// Only complete native polls may establish a baseline. No persistence is used.
    pub(crate) fn observe(
        &mut self,
        account: &Account,
        snapshot: &Value,
        blocked: bool,
        now: DateTime<Utc>,
    ) {
        if self
            .plan_fences
            .get(&account.id)
            .is_some_and(|(generation, _)| account.generation < *generation)
        {
            return;
        }
        let Some(plan) = account
            .plan
            .as_deref()
            .filter(|p| known_plan(account.provider, p))
        else {
            self.accounts.remove(&account.id);
            return;
        };
        if crate::observability::safe_identifier(&account.id).is_none() {
            return;
        }
        if !self.accounts.contains_key(&account.id) && self.accounts.len() >= MAX_ACCOUNTS {
            return;
        }
        let state =
            self.accounts
                .entry(account.id.clone())
                .or_insert_with(|| AccountMeasurements {
                    facts: CapacityFacts {
                        plan: plan.to_owned(),
                        provider: account.provider,
                        windows: BTreeMap::new(),
                    },
                    baselines: BTreeMap::new(),
                });
        if state.facts.plan != plan || state.facts.provider != account.provider {
            state.facts = CapacityFacts {
                plan: plan.to_owned(),
                provider: account.provider,
                windows: BTreeMap::new(),
            };
            state.baselines.clear();
        }
        if blocked
            || self.unclocked_traffic
            || !compatible(account, snapshot, now)
            || snapshot["source"] != "poll"
            || account.plan_observed_at.is_some_and(|at| at > now)
            || (!snapshot["plan"].is_null() && snapshot["plan"].as_str() != account.plan.as_deref())
            || snapshot["signals"]["serving_from_credits"] == true
            || snapshot["signals"]["active_limit"] == "credits"
        {
            state.baselines.clear();
            return;
        }
        let Some(windows) = snapshot["windows"]
            .as_array()
            .filter(|w| w.len() <= MAX_WINDOWS)
        else {
            state.baselines.clear();
            return;
        };
        let mut parsed = BTreeMap::new();
        for raw in windows {
            if crate::bindings::account_window(account.provider, raw).is_none() {
                continue;
            }
            let Some(name) = raw["name"].as_str().filter(|name| supported(name)) else {
                continue;
            };
            let Some((at, reset, used)) = observation(raw, now) else {
                state.baselines.clear();
                return;
            };
            if self
                .plan_fences
                .get(&account.id)
                .is_some_and(|(_, changed_at)| at <= *changed_at)
            {
                state.baselines.clear();
                return;
            }
            if self
                .completions
                .get(&account.id)
                .is_some_and(|end| *end >= at)
                || self.unknown_completion.is_some_and(|end| end >= at)
            {
                state.baselines.clear();
                return;
            }
            if parsed.insert(name.to_owned(), (at, reset, used)).is_some() {
                state.baselines.clear();
                return;
            }
        }
        state.baselines.retain(|name, _| parsed.contains_key(name));
        // Removed windows cannot accumulate an unbounded history of names.
        state
            .facts
            .windows
            .retain(|name, _| parsed.contains_key(name));
        for (name, (at, reset, used)) in parsed {
            let previous = state.baselines.remove(&name);
            if let Some(mut b) = previous {
                if at <= b.at || b.latest_completion.is_some_and(|end| end >= at) {
                    // Do not seed inside an overlapping request or regress poll time.
                    continue;
                }
                let clean = at > b.at
                    && at - b.last_at <= Duration::seconds(MAX_AGE_SECS)
                    && at - b.at <= Duration::seconds(MAX_INTERVAL_SECS)
                    && reset == b.reset
                    && used >= b.used
                    && b.latest_completion.is_none_or(|end| end < at);
                if clean {
                    let delta = used - b.used;
                    if delta >= 1.0 && b.tokens > 0 {
                        let samples = &mut state
                            .facts
                            .windows
                            .entry(name.clone())
                            .or_insert_with(|| WindowSamples {
                                samples: Vec::new(),
                            })
                            .samples;
                        if samples.len() == MAX_SAMPLES {
                            samples.remove(0);
                        }
                        samples.push(CapacitySample {
                            input_tokens: b.tokens,
                            used_pct_delta: delta,
                            start_at: b.at,
                            end_at: at,
                            reset_at: reset,
                        });
                    } else if delta < 1.0 {
                        // Accumulate sub-percentage steps without losing their own tokens.
                        b.last_at = at;
                        state.baselines.insert(name, b);
                        continue;
                    }
                }
            }
            state.baselines.insert(
                name,
                Baseline {
                    at,
                    last_at: at,
                    reset,
                    used,
                    tokens: 0,
                    latest_completion: None,
                },
            );
        }
    }

    pub(crate) fn facts(&self, id: &str) -> Option<CapacityFacts> {
        if self.unclocked_traffic {
            return None;
        }
        self.accounts.get(id).map(|s| s.facts.clone())
    }

    pub(crate) fn invalidate(&mut self, account: &Account, at: DateTime<Utc>) {
        if self
            .plan_fences
            .get(&account.id)
            .is_some_and(|(generation, _)| *generation > account.generation)
        {
            return;
        }
        if !self.plan_fences.contains_key(&account.id) && self.plan_fences.len() >= MAX_ACCOUNTS {
            self.unclocked_traffic = true;
            return;
        }
        self.plan_fences
            .insert(account.id.clone(), (account.generation, at));
        self.accounts.remove(&account.id);
    }
}

#[cfg(test)]
mod tests;
