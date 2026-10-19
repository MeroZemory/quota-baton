//! Bounded, opt-in diagnostics. Never consulted by placement or health.
//! Bands are deliberately uncalibrated scenarios, not probabilistic promises.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

const MAX_SERIES: usize = 1024;
const MAX_SAMPLES: usize = 32;
const MAX_AGE: i64 = 300;
const MAX_HORIZON: i64 = 900;
#[derive(Clone)]
struct Sample {
    at: DateTime<Utc>,
    used: f64,
}
struct Series {
    reset: DateTime<Utc>,
    plan: Value,
    samples: VecDeque<Sample>,
    prediction: Option<(DateTime<Utc>, f64, f64, f64)>,
    evaluated: u64,
    covered: u64,
}
#[derive(Default)]
pub struct Shadow {
    enabled: bool,
    series: BTreeMap<(String, String), Series>,
    regimes: BTreeMap<String, (Option<usize>, Option<String>)>,
}
fn date(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}
fn supported(name: &str) -> bool {
    matches!(name, "5h" | "7d" | "30d")
}
impl Shadow {
    pub fn enable(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.series.clear();
            self.regimes.clear();
        }
    }
    pub fn regime(&mut self, id: &str, active: Option<usize>, model: Option<&str>) {
        if !self.enabled || crate::observability::safe_identifier(id).is_none() {
            return;
        }
        if !self.regimes.contains_key(id) && self.regimes.len() >= MAX_SERIES {
            return;
        }
        let entry = self.regimes.entry(id.to_owned()).or_default();
        let changed = active.is_some_and(|n| entry.0 != Some(n))
            || model.is_some_and(|m| entry.1.as_deref() != Some(m));
        if active.is_some() {
            entry.0 = active;
        }
        if let Some(model) = model.and_then(crate::observability::safe_identifier) {
            entry.1 = Some(model);
        }
        if changed {
            self.series.retain(|(account, _), _| account != id);
        }
    }
    pub fn observe(&mut self, id: &str, snapshot: &Value, now: DateTime<Utc>) {
        if !self.enabled || crate::observability::safe_identifier(id).is_none() {
            return;
        }
        self.series.retain(|_, s| {
            s.samples
                .back()
                .is_some_and(|p| (now - p.at).num_seconds() <= 3600)
        });
        let Some(windows) = snapshot["windows"].as_array() else {
            return;
        };
        for w in windows.iter().take(8) {
            let Some(name) = w["name"].as_str().filter(|name| supported(name)) else {
                continue;
            };
            let Some(at) = date(&w["observed_at"]) else {
                continue;
            };
            let Some(reset) = date(&w["reset_at"]) else {
                continue;
            };
            let Some(used) = w["used_pct"]
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
            else {
                continue;
            };
            if at > now || (now - at).num_seconds() > MAX_AGE || reset <= at {
                continue;
            }
            let key = (id.to_owned(), name.to_owned());
            if !self.series.contains_key(&key) && self.series.len() >= MAX_SERIES {
                continue;
            }
            let series = self.series.entry(key).or_insert_with(|| Series {
                reset,
                plan: snapshot["plan"].clone(),
                samples: VecDeque::new(),
                prediction: None,
                evaluated: 0,
                covered: 0,
            });
            // A new reset epoch, counter rollback, or a large observation gap
            // invalidates history. Never learn negative consumption.
            if series.reset != reset
                || series.plan != snapshot["plan"]
                || series.samples.back().is_some_and(|p| {
                    (at > p.at && used < p.used) || (at - p.at).num_seconds() > MAX_AGE
                })
            {
                series.samples.clear();
                series.prediction = None;
                series.reset = reset;
                series.plan = snapshot["plan"].clone();
            }
            if series
                .samples
                .back()
                .is_some_and(|p| at <= p.at || (at - p.at).num_seconds() < 15)
            {
                continue;
            }
            if let Some((until, baseline, lower, upper)) = series.prediction
                && at >= until
            {
                // Only a sample close to the forecast boundary can calibrate
                // that horizon; a late sample includes different consumption.
                if (at - until).num_seconds() <= 15 {
                    series.evaluated = series.evaluated.saturating_add(1);
                    let actual = used - baseline;
                    if (lower..=upper).contains(&actual) {
                        series.covered = series.covered.saturating_add(1);
                    }
                }
                series.prediction = None;
            }
            series.samples.push_back(Sample { at, used });
            while series.samples.len() > MAX_SAMPLES {
                series.samples.pop_front();
            }
        }
    }
    /// Compare only; active load is not attributed consumption or a reservation.
    pub fn view(
        &mut self,
        id: &str,
        snapshot: &Value,
        active: usize,
        inflight: usize,
        now: DateTime<Utc>,
    ) -> Value {
        if !self.enabled {
            return json!({"enabled":false,"placement_effect":false});
        }
        self.regime(id, Some(active), None);
        let mut windows = Vec::new();
        for w in snapshot["windows"].as_array().into_iter().flatten().take(8) {
            let Some(name) = w["name"].as_str() else {
                continue;
            };
            let fallback = |reason| json!({"window":name,"status":"fallback","reason":reason});
            if !supported(name) {
                windows.push(fallback("unsupported_window"));
                continue;
            }
            let Some(s) = self.series.get_mut(&(id.to_owned(), name.to_owned())) else {
                windows.push(fallback("insufficient_observations"));
                continue;
            };
            let Some(last) = s.samples.back() else {
                continue;
            };
            if date(&w["reset_at"]) != Some(s.reset) {
                windows.push(fallback("reset_changed"));
                continue;
            }
            let horizon = (s.reset - now).num_seconds();
            let span = s
                .samples
                .front()
                .map_or(0, |p| (last.at - p.at).num_seconds());
            if last.at > now || (now - last.at).num_seconds() > MAX_AGE {
                windows.push(fallback("stale"));
                continue;
            }
            if s.samples.len() < 5 || span < 120 {
                windows.push(fallback("insufficient_observations"));
                continue;
            }
            if !(1..=MAX_HORIZON).contains(&horizon) || horizon > span {
                windows.push(fallback("unvalidated_horizon"));
                continue;
            }
            if inflight != 0 {
                windows.push(fallback("unobserved_inflight_consumption"));
                continue;
            }
            // Empirical future scenarios, widened for quantization and traffic
            // variability. No confidence level or session attribution claimed.
            let rates: Vec<_> = s
                .samples
                .iter()
                .zip(s.samples.iter().skip(1))
                .map(|(a, b)| (b.used - a.used) / (b.at - a.at).num_seconds().max(1) as f64)
                .collect();
            let low = rates.iter().copied().fold(f64::INFINITY, f64::min) * 0.5 * horizon as f64;
            let high = rates.iter().copied().fold(0.0, f64::max) * 2.0 * horizon as f64 + 2.0;
            let remaining = 100.0 - last.used;
            let classification = if low > remaining + 2.0 {
                "existing_demand_exhaustion_risk"
            } else if remaining - high - 3.0 > 0.0 {
                "possible_spare_capacity"
            } else {
                "uncertain"
            };
            // Evaluate a bounded 120 s forecast separately from the reset
            // scenario. Observed coverage is diagnostic, not certification.
            if s.prediction.is_none() && horizon > 120 {
                s.prediction = Some((
                    last.at + chrono::Duration::seconds(120),
                    last.used,
                    low * 120.0 / horizon as f64,
                    high * 120.0 / horizon as f64,
                ));
            }
            windows.push(
                json!({"window":name,"status":"shadow","calibration":"uncalibrated",
                "classification":classification,"remaining_pp":remaining,"demand_lower_pp":low,
                "demand_upper_pp":high,"margin_pp":3,"horizon_secs":horizon,
                "samples":s.samples.len(),"evaluated":s.evaluated,"covered":s.covered}),
            );
        }
        json!({"enabled":true,"placement_effect":false,"active_groups":active,
            "external_traffic":"not_attributed","new_demand_reservation":"unknown","windows":windows})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(at: DateTime<Utc>, reset: DateTime<Utc>, used: f64) -> Value {
        json!({"windows":[{"name":"5h","used_pct":used,"observed_at":at,"reset_at":reset}]})
    }
    #[test]
    fn opt_in_shadow_is_bounded_and_never_claims_placement_or_probability() {
        let now = Utc::now();
        let reset = now + chrono::Duration::seconds(150);
        let mut s = Shadow::default();
        let v = snapshot(now, reset, 80.0);
        s.observe("cl-fixture", &v, now);
        assert!(s.series.is_empty());
        assert_eq!(s.view("cl-fixture", &v, 2, 0, now)["enabled"], false);
        s.enable(true);
        s.regime("cl-fixture", Some(2), None);
        for i in 0..9 {
            let at = now - chrono::Duration::seconds(240 - i * 30);
            s.observe(
                "cl-fixture",
                &snapshot(at, reset, 75.0 + i as f64 * 0.5),
                at,
            );
        }
        let view = s.view("cl-fixture", &snapshot(now, reset, 79.0), 2, 0, now);
        assert_eq!(view["placement_effect"], false);
        assert_eq!(view["windows"][0]["status"], "shadow");
        assert_eq!(view["windows"][0]["calibration"], "uncalibrated");
        assert_eq!(
            s.view("cl-fixture", &v, 2, 1, now)["windows"][0]["reason"],
            "unobserved_inflight_consumption"
        );
    }
    #[test]
    fn reset_rollback_partial_and_stale_samples_never_become_zero_demand() {
        let now = Utc::now();
        let reset = now + chrono::Duration::seconds(150);
        let mut s = Shadow::default();
        s.enable(true);
        s.regime("cl-fixture", Some(1), None);
        for i in 0..9 {
            let at = now - chrono::Duration::seconds(240 - i * 30);
            s.observe("cl-fixture", &snapshot(at, reset, 70.0 + i as f64), at);
        }
        let fresh = snapshot(now, reset, 78.0);
        s.observe(
            "cl-fixture",
            &snapshot(now - chrono::Duration::seconds(600), reset, 1.0),
            now,
        );
        assert_eq!(s.series.values().next().unwrap().samples.len(), 9);
        s.observe(
            "cl-fixture",
            &snapshot(now + chrono::Duration::seconds(30), reset, 2.0),
            now + chrono::Duration::seconds(30),
        );
        assert_eq!(s.series.values().next().unwrap().samples.len(), 1);
        assert_eq!(
            s.view(
                "cl-fixture",
                &fresh,
                1,
                0,
                now + chrono::Duration::seconds(30)
            )["windows"][0]["reason"],
            "insufficient_observations"
        );
        s.observe(
            "cl-fixture",
            &snapshot(
                now + chrono::Duration::seconds(60),
                reset + chrono::Duration::hours(5),
                3.0,
            ),
            now + chrono::Duration::seconds(60),
        );
        assert_eq!(s.series.values().next().unwrap().samples.len(), 1);
        assert_eq!(
            s.view("cl-fixture", &fresh, 1, 0, now)["windows"][0]["reason"],
            "reset_changed"
        );
    }
    #[test]
    fn model_and_active_load_changes_invalidate_forecast_and_coverage_is_observed() {
        let now = Utc::now();
        let reset = now + chrono::Duration::seconds(150);
        let mut s = Shadow::default();
        s.enable(true);
        s.regime("cl-fixture", Some(2), Some("synthetic-model-a"));
        for i in 0..9 {
            let at = now - chrono::Duration::seconds(240 - i * 30);
            s.observe(
                "cl-fixture",
                &snapshot(at, reset, 75.0 + i as f64 * 0.5),
                at,
            );
        }
        let view = s.view("cl-fixture", &snapshot(now, reset, 79.0), 2, 0, now);
        assert_eq!(view["windows"][0]["status"], "shadow");
        let future = now + chrono::Duration::seconds(120);
        s.observe("cl-fixture", &snapshot(future, reset, 81.0), future);
        assert_eq!(s.series.values().next().unwrap().evaluated, 1);
        assert_eq!(s.series.values().next().unwrap().covered, 1);
        s.regime("cl-fixture", Some(3), None);
        assert!(s.series.is_empty());
        s.observe("cl-fixture", &snapshot(future, reset, 81.0), future);
        s.regime("cl-fixture", None, Some("synthetic-model-b"));
        assert!(s.series.is_empty());
    }
    #[test]
    fn observation_storage_has_fixed_series_and_sample_caps() {
        let now = Utc::now();
        let reset = now + chrono::Duration::hours(1);
        let mut s = Shadow::default();
        s.enable(true);
        for i in 0..MAX_SERIES + 10 {
            s.observe(&format!("cl-{i}"), &snapshot(now, reset, 1.0), now);
        }
        assert_eq!(s.series.len(), MAX_SERIES);
        for i in 1..100 {
            let at = now + chrono::Duration::seconds(i * 15);
            s.observe("cl-0", &snapshot(at, reset, i as f64), at);
        }
        assert_eq!(
            s.series[&("cl-0".into(), "5h".into())].samples.len(),
            MAX_SAMPLES
        );
    }
}
