//! Pure interpretation of supplied capacity measurements; no native state reads.
use crate::accounts::{Account, Provider};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

const MIN_SAMPLES: usize = 3;
const MAX_SAMPLES: usize = 16;
const MAX_WINDOWS: usize = 16;
const MAX_AGE_SECS: i64 = 300;
const MAX_INTERVAL_SECS: i64 = 86_400;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapacityFacts {
    pub plan: String,
    pub provider: Provider,
    pub windows: BTreeMap<String, WindowSamples>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowSamples {
    pub samples: Vec<CapacitySample>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapacitySample {
    pub input_tokens: u64,
    pub used_pct_delta: f64,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub reset_at: DateTime<Utc>,
}

impl CapacityFacts {
    /// Structural wire validation; time relative to a decision is checked separately.
    pub(crate) fn valid(&self) -> bool {
        known_plan(self.provider, &self.plan)
            && self.windows.len() <= MAX_WINDOWS
            && self.windows.iter().all(|(name, window)| {
                supported(name)
                    && window.samples.len() <= MAX_SAMPLES
                    && window.samples.iter().all(|s| {
                        s.input_tokens > 0
                            && s.used_pct_delta.is_finite()
                            && (1.0..=100.0).contains(&s.used_pct_delta)
                            && s.start_at.timestamp() >= 0
                            && s.start_at < s.end_at
                            && s.end_at - s.start_at <= Duration::seconds(MAX_INTERVAL_SECS)
                            && s.end_at < s.reset_at
                    })
                    && window
                        .samples
                        .windows(2)
                        .all(|s| s[0].end_at <= s[1].start_at)
            })
    }
}

/// All and only native gating windows supplied by the caller must have estimates.
/// `None` means use the existing percent rule; this function never selects an account.
pub(crate) fn remaining_tokens(
    facts: &CapacityFacts,
    account: &Account,
    quota: &Value,
    projected_pct: &BTreeMap<String, f64>,
    now: DateTime<Utc>,
) -> Option<f64> {
    if !facts.valid()
        || facts.provider != account.provider
        || account.plan.as_deref() != Some(facts.plan.as_str())
        || !compatible(account, quota, now)
        || projected_pct.is_empty()
        || projected_pct.len() > MAX_WINDOWS
    {
        return None;
    }
    let windows = quota["windows"].as_array()?;
    let mut remaining = f64::INFINITY;
    for (name, projected) in projected_pct {
        if !supported(name) || !projected.is_finite() || *projected < 0.0 {
            return None;
        }
        let mut matching = windows
            .iter()
            .filter(|w| w["name"].as_str() == Some(name.as_str()));
        let raw = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        let (at, _, used) = observation(raw, now)?;
        if *projected < used {
            return None;
        }
        let samples = &facts.windows.get(name)?.samples;
        if samples.len() < MIN_SAMPLES || samples.iter().any(|s| s.end_at > at) {
            return None;
        }
        let mut ratios: Vec<f64> = samples
            .iter()
            .map(|s| s.input_tokens as f64 / s.used_pct_delta)
            .collect();
        ratios.sort_by(f64::total_cmp);
        let mid = ratios.len() / 2;
        let median = if ratios.len().is_multiple_of(2) {
            ratios[mid - 1] / 2.0 + ratios[mid] / 2.0
        } else {
            ratios[mid]
        };
        let tokens = (100.0 - projected).max(0.0) * median;
        if !tokens.is_finite() {
            return None;
        }
        remaining = remaining.min(tokens);
    }
    remaining.is_finite().then_some(remaining)
}

pub(crate) fn known_plan(provider: Provider, plan: &str) -> bool {
    match provider {
        Provider::Codex => matches!(
            plan,
            "free"
                | "go"
                | "plus"
                | "pro"
                | "prolite"
                | "promax"
                | "team"
                | "business"
                | "enterprise"
                | "edu"
        ),
        Provider::Claude => matches!(
            plan,
            "free" | "pro" | "max" | "max_5x" | "max_20x" | "team" | "enterprise"
        ),
    }
}

pub(crate) fn supported(name: &str) -> bool {
    if matches!(name, "primary" | "secondary") {
        return true;
    }
    if !(2..=21).contains(&name.len())
        || !matches!(name.as_bytes().last(), Some(b's' | b'm' | b'h' | b'd'))
    {
        return false;
    }
    let digits = &name[..name.len() - 1];
    digits.bytes().all(|b| b.is_ascii_digit()) && digits.parse::<u64>().is_ok_and(|n| n > 0)
}

fn date(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()?.parse().ok()
}

pub(crate) fn compatible(account: &Account, snapshot: &Value, now: DateTime<Utc>) -> bool {
    // Only the quota witness archived by QuotaFacts is interpreted here.
    // Native polling fences plan/source/signals before admitting measurements.
    let _ = (account, now);
    snapshot["paid_serving_observed_at"].is_null()
}

pub(crate) fn observation(
    raw: &Value,
    now: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>, f64)> {
    let at = date(&raw["observed_at"])?;
    let reset = date(&raw["reset_at"])?;
    let used = raw["used_pct"].as_f64()?;
    (at.timestamp() >= 0
        && at <= now
        && now - at <= Duration::seconds(MAX_AGE_SECS)
        && reset > now
        && used.is_finite()
        && (0.0..=100.0).contains(&used)
        && raw["status"] != "rejected")
        .then_some((at, reset, used))
}
