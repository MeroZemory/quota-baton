//! Poll side-copy parsing. The caller bounds the response body before parsing.
//! Unknown fields are ignored; strings are never copied without a whitelist.

use crate::accounts::Provider;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value, json};

const CLAUDE_WINDOWS: &[(&str, &str)] = &[
    ("five_hour", "5h"),
    ("seven_day", "7d"),
    ("seven_day_opus", "7d_oi"),
    ("seven_day_sonnet", "7d_sonnet"),
];

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn plan(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        matches!(
            *s,
            "free" | "plus" | "pro" | "team" | "business" | "enterprise" | "edu" | "go" | "max"
        )
    })
}

fn status(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        matches!(
            *s,
            "allowed" | "allowed_warning" | "rejected" | "true" | "false"
        )
    })
}

fn active_limit(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|s| matches!(*s, "primary" | "secondary" | "5h" | "7d" | "credits"))
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_i64()
        .and_then(|n| DateTime::from_timestamp(n, 0))
        .or_else(|| {
            let text = value.as_str()?;
            text.parse::<i64>()
                .ok()
                .and_then(|n| DateTime::from_timestamp(n, 0))
                .or_else(|| {
                    DateTime::parse_from_rfc3339(text)
                        .ok()
                        .map(|t| t.with_timezone(&Utc))
                })
        })
}

fn reset(window: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    timestamp(&window["reset_at"])
        .or_else(|| timestamp(&window["resets_at"]))
        .or_else(|| {
            let seconds = window["reset_after_seconds"].as_i64().filter(|n| *n >= 0)?;
            now.checked_add_signed(Duration::try_seconds(seconds)?)
        })
}

fn window(name: &str, value: &Value, claude: bool, now: DateTime<Utc>) -> Option<Value> {
    value.as_object()?;
    let used = number(
        &value[if claude {
            "utilization"
        } else {
            "used_percent"
        }],
    );
    let reset_at = reset(value, now);
    let state = status(&value["status"]);
    if used.is_none() && reset_at.is_none() && state.is_none() {
        return None;
    }
    Some(json!({"name": name, "used_pct": used, "reset_at": reset_at,
        "observed_at": used.map(|_| now), "status": state}))
}

/// Returns a value deserializable as observability's Snapshot, or None when no
/// recognized usage data is present. Null/unknown fields never invent usage.
pub(super) fn snapshot(provider: Provider, body: &[u8], now: DateTime<Utc>) -> Option<Value> {
    let root: Value = serde_json::from_slice(body).ok()?;
    root.as_object()?;
    let mut windows = Vec::new();
    let mut signals = Map::new();
    let mut credits = None;
    let mut account_plan = None;
    match provider {
        Provider::Codex => {
            let rate = &root["rate_limit"];
            for slot in ["primary", "secondary"] {
                let value = &rate[format!("{slot}_window")];
                let name = match value["limit_window_seconds"].as_u64().filter(|n| *n > 0) {
                    Some(18_000) => "5h".to_owned(),
                    Some(604_800) => "7d".to_owned(),
                    Some(n) if n % 60 == 0 => format!("{}m", n / 60),
                    Some(n) => format!("{n}s"),
                    None => slot.to_owned(),
                };
                if let Some(w) = window(&name, value, false, now) {
                    windows.push(w);
                }
            }
            if let Some(reached) = rate["limit_reached"].as_bool() {
                signals.insert("limit_reached".into(), reached.into());
            }
            let active = active_limit(&rate["active_limit"]);
            if let Some(active) = active {
                signals.insert("active_limit".into(), active.into());
            }
            if let Some(serving) = rate["serving_from_credits"]
                .as_bool()
                .or_else(|| root["credits"]["serving_from_credits"].as_bool())
                .or_else(|| (active == Some("credits")).then_some(true))
            {
                signals.insert("serving_from_credits".into(), serving.into());
            }
            account_plan = plan(&root["plan_type"]);
            let c = &root["credits"];
            let has = c["has_credits"].as_bool();
            let unlimited = c["unlimited"].as_bool();
            let balance = number(&c["balance"]);
            if has.is_some() || unlimited.is_some() || balance.is_some() {
                credits =
                    Some(json!({"has_credits": has, "unlimited": unlimited, "balance": balance}));
            }
        }
        Provider::Claude => {
            for (key, name) in CLAUDE_WINDOWS {
                if let Some(w) = window(name, &root[*key], true, now) {
                    windows.push(w);
                }
            }
        }
    }
    if windows.is_empty() && signals.is_empty() && credits.is_none() && account_plan.is_none() {
        return None;
    }
    Some(
        json!({"windows": windows, "plan": account_plan, "credits": credits,
        "source": "poll", "observed_at": now, "signals": signals}),
    )
}

#[cfg(any(test, all(feature = "shape-capture", debug_assertions)))]
fn usage_scalar(provider: Provider, path: &[&str], value: &Value) -> Option<Value> {
    match (provider, path) {
        (Provider::Codex, ["plan_type"]) => plan(value).map(Value::from),
        (Provider::Codex, ["rate_limit", "active_limit"]) => active_limit(value).map(Value::from),
        (Provider::Codex, ["rate_limit", "limit_reached" | "serving_from_credits"])
        | (
            Provider::Codex,
            [
                "credits",
                "has_credits" | "unlimited" | "serving_from_credits",
            ],
        ) => value.as_bool().map(Value::from),
        (Provider::Codex, ["credits", "balance"]) => number(value).map(Value::from),
        (Provider::Codex, ["rate_limit", "primary_window" | "secondary_window", field])
            if matches!(
                *field,
                "used_percent" | "limit_window_seconds" | "reset_at" | "reset_after_seconds"
            ) =>
        {
            number(value).map(Value::from)
        }
        (
            Provider::Codex,
            [
                "rate_limit",
                "primary_window" | "secondary_window",
                "status",
            ],
        ) => status(value).map(Value::from),
        (Provider::Claude, [key, field]) if CLAUDE_WINDOWS.iter().any(|(k, _)| k == key) => {
            match *field {
                "utilization" | "resets_at" | "reset_at" | "reset_after_seconds" => {
                    number(value).map(Value::from)
                }
                "status" => status(value).map(Value::from),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Captures key paths to depth three and only known usage scalars. Arrays and
/// arbitrary strings are omitted. Limits also bound output for wide objects.
#[cfg(any(test, all(feature = "shape-capture", debug_assertions)))]
pub(super) fn shape(provider: Provider, body: &[u8]) -> Value {
    fn visit(
        provider: Provider,
        value: &Value,
        path: &mut Vec<String>,
        keys: &mut Vec<String>,
        usage: &mut Map<String, Value>,
    ) {
        if path.len() == 3 || keys.len() == 256 {
            return;
        }
        let Some(object) = value.as_object() else {
            return;
        };
        for (key, value) in object {
            if keys.len() == 256 {
                break;
            }
            // An upstream can put PII in keys too: retain identifier-shaped keys only.
            if key.is_empty()
                || key.len() > 64
                || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                continue;
            }
            path.push(key.clone());
            let joined = path.join(".");
            keys.push(joined.clone());
            let borrowed: Vec<&str> = path.iter().map(String::as_str).collect();
            if let Some(scalar) = usage_scalar(provider, &borrowed, value) {
                usage.insert(joined, scalar);
            }
            visit(provider, value, path, keys, usage);
            path.pop();
        }
    }
    let mut keys = Vec::new();
    let mut usage = Map::new();
    if let Ok(root) = serde_json::from_slice::<Value>(body) {
        visit(provider, &root, &mut Vec::new(), &mut keys, &mut usage);
    }
    json!({"keys": keys, "usage": usage})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }
    fn parse(provider: Provider, value: Value) -> Value {
        snapshot(provider, &serde_json::to_vec(&value).unwrap(), now()).unwrap()
    }

    #[test]
    fn codex_windows_credits_and_signals() {
        let result = parse(
            Provider::Codex,
            json!({"rate_limit": {
            "limit_reached": true, "active_limit": "credits",
            "primary_window": {"used_percent": 101, "limit_window_seconds": 18000, "reset_after_seconds": 60},
            "secondary_window": {"used_percent": 42, "limit_window_seconds": 604800, "reset_at": 1800000120}},
            "plan_type": "plus", "credits": {"has_credits": true, "unlimited": false, "balance": "12.50"}}),
        );
        assert_eq!(result["source"], "poll");
        assert_eq!(result["windows"][0]["name"], "5h");
        assert_eq!(result["windows"][0]["used_pct"], 101.0);
        assert_eq!(
            result["windows"][0]["reset_at"],
            json!(now() + Duration::seconds(60))
        );
        assert_eq!(result["windows"][1]["name"], "7d");
        assert_eq!(
            result["windows"][1]["reset_at"],
            json!(now() + Duration::seconds(120))
        );
        assert_eq!(result["signals"]["limit_reached"], true);
        assert_eq!(result["signals"]["serving_from_credits"], true);
        assert_eq!(result["plan"], "plus");
        assert_eq!(result["credits"]["balance"], 12.5);
    }

    #[test]
    fn missing_duration_unknown_duration_and_reset_only() {
        for (duration, expected) in [
            (Value::Null, "primary"),
            (json!(3600), "60m"),
            (json!(61), "61s"),
        ] {
            let result = parse(
                Provider::Codex,
                json!({"rate_limit": {"primary_window": {
                "limit_window_seconds": duration, "reset_at": "2027-01-15T08:00:00Z"}}}),
            );
            assert_eq!(result["windows"][0]["name"], expected);
            assert!(result["windows"][0]["used_pct"].is_null());
            assert!(result["windows"][0]["observed_at"].is_null());
            assert_eq!(result["windows"][0]["reset_at"], "2027-01-15T08:00:00Z");
            assert_eq!(result["windows"].as_array().unwrap().len(), 1);
        }
    }

    #[test]
    fn claude_oauth_utilization_is_percent_in_all_supported_buckets() {
        for utilization in [0.0, 0.5, 1.0, 35.0, 73.0] {
            let body = serde_json::to_vec(&json!({
                "five_hour": {"utilization": utilization, "resets_at": "2027-01-15T08:00:00Z"},
                "seven_day": {"utilization": utilization, "resets_at": "2027-01-21T08:00:00Z"},
                "seven_day_opus": {"utilization": utilization, "resets_at": null},
                "seven_day_sonnet": {"utilization": utilization, "resets_at": null},
                "seven_day_oauth_apps": null,
                "seven_day_cowork": null,
                "extra_usage": {"is_enabled": false, "monthly_limit": null, "used_credits": 0}
            }))
            .unwrap();
            let result = snapshot(Provider::Claude, &body, now()).unwrap();
            let windows = result["windows"].as_array().unwrap();
            assert_eq!(windows.len(), 4);
            for (window, name) in windows.iter().zip(["5h", "7d", "7d_oi", "7d_sonnet"]) {
                assert_eq!(window["name"], name);
                assert_eq!(window["used_pct"], utilization, "{name}");
                assert_eq!(window["observed_at"], json!(now()));
            }
            assert_eq!(windows[0]["reset_at"], "2027-01-15T08:00:00Z");
            assert_eq!(windows[1]["reset_at"], "2027-01-21T08:00:00Z");
            assert!(windows[2]["reset_at"].is_null());
            assert!(windows[3]["reset_at"].is_null());
            assert_eq!(result["signals"], json!({}));
        }
    }

    #[test]
    fn claude_ignores_other_buckets_even_with_recognized_fields() {
        let mut value = json!({
            "seven_day_oauth_apps": {"utilization": 73, "resets_at": "2027-01-15T08:00:00Z"},
            "seven_day_cowork": {"utilization": 35, "status": "rejected"},
            "seven_day_opaque_a1b2": {"utilization": 1, "reset_after_seconds": 60},
            "opaque_c3d4": {"utilization": 0.5, "status": "allowed"},
            "extra_usage": {"is_enabled": true, "monthly_limit": 100, "used_credits": 73}
        });
        let body = serde_json::to_vec(&value).unwrap();
        assert!(snapshot(Provider::Claude, &body, now()).is_none());
        assert_eq!(shape(Provider::Claude, &body)["usage"], json!({}));

        value["five_hour"] = json!({"utilization": 0, "resets_at": null});
        let body = serde_json::to_vec(&value).unwrap();
        let result = snapshot(Provider::Claude, &body, now()).unwrap();
        assert_eq!(result["windows"].as_array().unwrap().len(), 1);
        assert_eq!(result["windows"][0]["name"], "5h");
        assert_eq!(result["windows"][0]["used_pct"], 0.0);
        assert_eq!(
            shape(Provider::Claude, &body)["usage"],
            json!({"five_hour.utilization": 0.0})
        );
    }

    #[test]
    fn mismatch_invalid_and_unknown_are_tolerated() {
        for provider in [Provider::Claude, Provider::Codex] {
            for body in [
                b"not json".as_slice(),
                b"[]",
                b"null",
                b"{}",
                b"{\"error\":\"synthetic\"}",
            ] {
                assert!(snapshot(provider, body, now()).is_none());
            }
        }
        assert!(
            snapshot(
                Provider::Claude,
                br#"{"five_hour":{"utilization":-1},"seven_day":"bad"}"#,
                now()
            )
            .is_none()
        );
        let result = parse(
            Provider::Codex,
            json!({"rate_limit": {"limit_reached": false,
            "primary_window": {"used_percent": "NaN", "reset_after_seconds": i64::MAX}},
            "plan_type": "synthetic-secret", "credits": {"balance": "inf"}}),
        );
        assert_eq!(result["signals"]["limit_reached"], false);
        assert_eq!(result["windows"], json!([]));
        assert!(result["plan"].is_null());
        assert!(result["credits"].is_null());
    }

    #[test]
    fn snapshot_and_shape_never_copy_secret_values() {
        let body = serde_json::to_vec(&json!({"access_token": "synthetic-token-secret",
            "email": "synthetic@example.test", "plan_type": "synthetic-plan-secret",
            "credits": {"balance": "synthetic-balance-secret", "has_credits": true},
            "rate_limit": {"limit_reached": true, "primary_window": {"used_percent": 25,
                "status": "synthetic-status-secret", "reset_at": "synthetic-reset-secret"}},
            "unknown": {"used_percent": 987654, "status": "allowed"},
            "synthetic@example.test": 123}))
        .unwrap();
        let capture = shape(Provider::Codex, &body);
        assert_eq!(
            capture["usage"]["rate_limit.primary_window.used_percent"],
            25.0
        );
        assert_eq!(capture["usage"]["credits.has_credits"], true);
        assert!(capture["usage"]["unknown.used_percent"].is_null());
        for output in [
            capture.to_string(),
            snapshot(Provider::Codex, &body, now()).unwrap().to_string(),
        ] {
            for secret in [
                "synthetic-token-secret",
                "synthetic@example.test",
                "synthetic-plan-secret",
                "synthetic-balance-secret",
                "synthetic-status-secret",
                "synthetic-reset-secret",
                "987654",
            ] {
                assert!(!output.contains(secret), "leaked {secret}");
            }
        }
    }

    #[test]
    fn shape_depth_width_and_claude_whitelist() {
        let captured = shape(Provider::Claude, br#"{"five_hour":{"utilization":0.5,"status":"allowed","resets_at":"2027-01-15T08:00:00Z"},"a":{"b":{"c":{"secret":123}}}}"#);
        assert_eq!(captured["usage"]["five_hour.utilization"], 0.5);
        assert_eq!(captured["usage"]["five_hour.status"], "allowed");
        assert!(captured["usage"]["five_hour.resets_at"].is_null());
        assert!(!captured.to_string().contains("secret"));
        let wide: Map<String, Value> = (0..1000).map(|i| (format!("key_{i}"), json!(i))).collect();
        let captured = shape(Provider::Codex, &serde_json::to_vec(&wide).unwrap());
        assert_eq!(captured["keys"].as_array().unwrap().len(), 256);
        assert_eq!(
            shape(Provider::Claude, b"invalid"),
            json!({"keys": [], "usage": {}})
        );
    }
}
