//! One interpretation of Codex credit entitlement for routing and presentation.
use serde_json::Value;

pub fn available(has: Option<bool>, unlimited: Option<bool>, balance: Option<f64>) -> bool {
    if unlimited == Some(true) {
        return true;
    }
    if has == Some(false) || balance.is_some_and(|n| !n.is_finite() || n <= 0.0) {
        return false;
    }
    has == Some(true) || balance.is_some_and(|n| n.is_finite() && n > 0.0)
}

pub fn available_in(credits: &Value) -> bool {
    available(
        credits["has_credits"].as_bool(),
        credits["unlimited"].as_bool(),
        credits["balance"].as_f64(),
    )
}

/// Unknown metadata is distinct from an explicit denial of entitlement.
pub fn explicitly_absent(credits: &Value) -> bool {
    credits["unlimited"] != true
        && (credits["has_credits"] == false
            || credits["balance"].as_f64().is_some_and(|n| n <= 0.0))
}

pub fn serving_ended(snapshot: &Value) -> bool {
    explicitly_absent(&snapshot["credits"])
        || snapshot["signals"]["serving_from_credits"] == false
        || (snapshot["signals"]["serving_from_credits"] != true
            && snapshot["signals"]["active_limit"]
                .as_str()
                .is_some_and(|l| l != "credits"))
}

/// Successful paid-serving proof is process-local. Snapshot serialization in
/// observability intentionally excludes this key, so restart cannot invent it.
pub fn entitled(snapshot: &Value) -> bool {
    available_in(&snapshot["credits"])
        || (!serving_ended(snapshot) && snapshot["paid_serving_observed_at"].as_str().is_some())
}

/// Explicit current absence invalidates cached numeric balances. An unlimited
/// entitlement is independent of a finite balance.
pub fn normalize(credits: &mut Value) {
    if !credits.is_object() {
        return;
    }
    if credits["unlimited"] == true {
        return;
    }
    if credits["has_credits"] == false {
        if credits["balance"].as_f64().is_some_and(|n| n > 0.0) {
            credits["balance"] = Value::Null;
        }
        credits["unlimited"] = false.into();
    } else if credits["balance"].as_f64().is_some_and(|n| n <= 0.0) {
        credits["has_credits"] = false.into();
        credits["unlimited"] = false.into();
    }
}

/// A partial update inherits only compatible evidence. New entitlement or a
/// new numeric balance must not borrow contradictory flags from an old sample.
pub fn merge(next: &mut Value, old: &Value) {
    if next.is_null() {
        *next = old.clone();
    } else if next.is_object() {
        let new_entitlement = !next["has_credits"].is_null() || next["balance"].as_f64().is_some();
        for key in ["has_credits", "unlimited", "balance"] {
            if next[key].is_null()
                && !(new_entitlement && matches!(key, "has_credits" | "unlimited"))
                && !(key == "balance" && !next["has_credits"].is_null())
            {
                next[key] = old[key].clone();
            }
        }
    }
    normalize(next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalization_is_idempotent_even_for_conflicting_or_zero_balances() {
        for mut c in [
            json!({"balance":0}),
            json!({"has_credits":false,"balance":12}),
            json!({"has_credits":true,"balance":0}),
            json!({"has_credits":false,"unlimited":true}),
        ] {
            normalize(&mut c);
            let once = c.clone();
            normalize(&mut c);
            assert_eq!(c, once);
        }
    }

    #[test]
    fn entitlement_is_not_permission_and_absence_overrides_stale_balance() {
        for (c, expected) in [
            (json!({}), false),
            (json!({"has_credits":false,"balance":12}), false),
            (json!({"has_credits":true,"balance":0}), false),
            (json!({"has_credits":true}), true),
            (json!({"balance":12}), true),
            (json!({"has_credits":false,"unlimited":true}), true),
        ] {
            assert_eq!(available_in(&c), expected, "{c}");
        }
    }

    #[test]
    fn new_evidence_does_not_resurrect_old_credit_entitlement() {
        for (next, old, expected) in [
            (
                json!({"has_credits":false}),
                json!({"balance":12,"unlimited":true}),
                false,
            ),
            (
                json!({"balance":0}),
                json!({"has_credits":true,"unlimited":true}),
                false,
            ),
            (json!({"balance":12}), json!({"has_credits":false}), true),
            (json!({"has_credits":true}), json!({"balance":0}), true),
            (
                json!({"unlimited":false}),
                json!({"has_credits":true,"balance":12}),
                true,
            ),
        ] {
            let mut merged = next;
            merge(&mut merged, &old);
            assert_eq!(available_in(&merged), expected, "{merged}");
        }
    }
}
