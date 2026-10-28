//! Descriptive native metadata; callers must validate provider identity first.
use super::Provider;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SubscriptionMetadata {
    Claude {
        created_at: DateTime<Utc>,
    },
    Codex {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_start: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_until: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_checked: Option<DateTime<Utc>>,
        /// Native persistence state only. Never present this field in a DTO.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_plan_checked_for: Option<DateTime<Utc>>,
    },
}

impl SubscriptionMetadata {
    /// Enables correspondence checks against the owning Account's provider.
    pub fn provider(&self) -> Provider {
        match self {
            Self::Claude { .. } => Provider::Claude,
            Self::Codex { .. } => Provider::Codex,
        }
    }

    /// Caller must fence the recognized plan observation and commit atomically.
    pub(crate) fn mark_codex_end_checked(
        &mut self,
        expected_until: DateTime<Utc>,
        observed_at: DateTime<Utc>,
    ) -> bool {
        match self {
            Self::Codex {
                active_until: Some(until),
                end_plan_checked_for,
                ..
            } if *until == expected_until
                && observed_at >= expected_until
                && *end_plan_checked_for != Some(expected_until) =>
            {
                *end_plan_checked_for = Some(expected_until);
                true
            }
            _ => false,
        }
    }

    /// Missing/invalid individual claims cannot erase a previously known fact.
    /// Preserve a completed check only for the exact retained endpoint instant.
    pub(crate) fn with_previous_codex(mut self, previous: Option<&Self>) -> Self {
        if let (
            Self::Codex {
                active_start,
                active_until,
                last_checked,
                end_plan_checked_for,
            },
            Some(Self::Codex {
                active_start: old_start,
                active_until: old_until,
                last_checked: old_checked,
                end_plan_checked_for: old_end_checked,
            }),
        ) = (&mut self, previous)
        {
            *active_start = active_start.or(*old_start);
            *active_until = active_until.or(*old_until);
            *last_checked = last_checked.or(*old_checked);
            *end_plan_checked_for = old_end_checked.filter(|end| Some(*end) == *active_until);
        }
        self
    }
}

/// Accepts an already identity-validated Claude profile, not an account object.
/// Only the original organization subscription creation fact is considered.
pub fn from_claude_profile(profile: &Value) -> Option<SubscriptionMetadata> {
    let created_at = date(
        profile
            .get("organization")?
            .get("subscription_created_at")?,
    )?;
    Some(SubscriptionMetadata::Claude { created_at })
}

/// Accepts ONLY the already identity-validated `https://api.openai.com/auth` object.
/// Each claim is independent; no valid date means no metadata observation.
/// This does not decode a token, merge prior state, or acknowledge an end check.
pub fn from_codex_auth(auth: &Value) -> Option<SubscriptionMetadata> {
    let active_start = auth.get("chatgpt_subscription_active_start").and_then(date);
    let active_until = auth.get("chatgpt_subscription_active_until").and_then(date);
    let last_checked = auth.get("chatgpt_subscription_last_checked").and_then(date);
    if active_start.is_none() && active_until.is_none() && last_checked.is_none() {
        return None;
    }
    Some(SubscriptionMetadata::Codex {
        active_start,
        active_until,
        last_checked,
        end_plan_checked_for: None,
    })
}

// Bound the supported RFC3339 subset before chrono parsing. Reject whitespace,
// epochs, missing zones, and fractions exceeding chrono's nanosecond precision.
// Lowercase t/z are permitted by RFC3339. No input text escapes this helper.
fn date(value: &Value) -> Option<DateTime<Utc>> {
    let raw = value.as_str()?;
    let b = raw.as_bytes();
    if !(20..=35).contains(&b.len()) {
        return None;
    }
    for (i, byte) in b.iter().take(19).enumerate() {
        let valid = match i {
            4 | 7 => *byte == b'-',
            10 => matches!(*byte, b'T' | b't'),
            13 | 16 => *byte == b':',
            _ => byte.is_ascii_digit(),
        };
        if !valid {
            return None;
        }
    }
    let mut zone = 19;
    if b[zone] == b'.' {
        zone += 1;
        let start = zone;
        while zone < b.len() && b[zone].is_ascii_digit() {
            zone += 1;
        }
        if !(1..=9).contains(&(zone - start)) {
            return None;
        }
    }
    match &b[zone..] {
        [b'Z' | b'z'] => {}
        [b'+' | b'-', h1, h2, b':', m1, m2]
            if [h1, h2, m1, m2].iter().all(|c| c.is_ascii_digit()) => {}
        _ => return None,
    }
    // RFC3339 -00:00 denotes an unknown local offset, not an exact UTC fact.
    if &b[zone..] == b"-00:00" {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn utc(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn claude_uses_only_original_organization_fact() {
        let profile = json!({
            "account": {"created_at": "2020-01-01T00:00:00Z"},
            "organization": {"subscription_created_at": "2026-10-22T12:34:56.123456Z"}
        });
        assert_eq!(
            from_claude_profile(&profile),
            Some(SubscriptionMetadata::Claude {
                created_at: utc("2026-10-22T12:34:56.123456Z"),
            })
        );
        assert_eq!(
            from_claude_profile(&json!({
                "account": {"created_at": "2020-01-01T00:00:00Z"},
                "subscription_created_at": "2026-10-22T12:34:56Z"
            })),
            None
        );
    }

    #[test]
    fn codex_literal_claims_are_independent_and_descriptive() {
        let auth = json!({
            "chatgpt_subscription_active_start": "2026-10-01T09:00:00+09:00",
            "chatgpt_subscription_active_until": "2026-11-01T00:00:00Z",
            "chatgpt_subscription_last_checked": "2026-10-22T01:02:03.123Z",
            "end_plan_checked_for": "2026-11-01T00:00:00Z"
        });
        assert_eq!(
            from_codex_auth(&auth),
            Some(SubscriptionMetadata::Codex {
                active_start: Some(utc("2026-10-01T00:00:00Z")),
                active_until: Some(utc("2026-11-01T00:00:00Z")),
                last_checked: Some(utc("2026-10-22T01:02:03.123Z")),
                end_plan_checked_for: None,
            })
        );
        assert_eq!(
            from_codex_auth(&json!({
                "chatgpt_subscription_active_start": false,
                "chatgpt_subscription_active_until": "2026-11-01T00:00:00Z",
                "chatgpt_subscription_last_checked": null
            })),
            Some(SubscriptionMetadata::Codex {
                active_start: None,
                active_until: Some(utc("2026-11-01T00:00:00Z")),
                last_checked: None,
                end_plan_checked_for: None,
            })
        );
        assert_eq!(
            from_codex_auth(&json!({"https://api.openai.com/auth": auth})),
            None
        );
    }

    #[test]
    fn missing_null_and_wrong_shapes_are_absence() {
        for value in [json!(null), json!({}), json!([]), json!(42), json!("date")] {
            assert_eq!(from_claude_profile(&value), None);
            assert_eq!(from_codex_auth(&value), None);
        }
        for value in [json!(null), json!({}), json!([]), json!(42), json!(false)] {
            assert_eq!(
                from_claude_profile(&json!({
                    "organization": {"subscription_created_at": value.clone()}
                })),
                None
            );
            assert_eq!(
                from_codex_auth(&json!({
                    "chatgpt_subscription_active_until": value
                })),
                None
            );
        }
        assert_eq!(from_claude_profile(&json!({"organization": null})), None);
    }

    #[test]
    fn invalid_dates_never_become_facts() {
        for raw in [
            "",
            "1790812800",
            "2026-10-01",
            "2026-10-01T00:00:00",
            "2026-02-30T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-10-01T24:00:00Z",
            "2026-10-01T00:00:00+25:00",
            "2026-10-01 00:00:00Z",
            " 2026-10-01T00:00:00Z",
            "2026-10-01T00:00:00Z ",
            "2026-10-01T00:00:00.Z",
            "2026-10-01T00:00:00.1234567890Z",
            "2026-10-01T00:00:00-00:00",
        ] {
            assert_eq!(date(&json!(raw)), None);
            assert_eq!(
                from_claude_profile(&json!({
                    "organization": {"subscription_created_at": raw}
                })),
                None
            );
            assert_eq!(
                from_codex_auth(&json!({
                    "chatgpt_subscription_active_until": raw
                })),
                None
            );
        }
        assert_eq!(
            date(&json!("2026-10-01t00:00:00z")),
            Some(utc("2026-10-01T00:00:00Z"))
        );
        assert_eq!(
            date(&json!("2026-10-01T00:00:00.123456789+09:00")),
            Some(utc("2026-09-30T15:00:00.123456789Z"))
        );
    }

    #[test]
    fn provider_tag_and_optional_native_fields_roundtrip() {
        let records = [
            (
                json!({"provider": "claude", "created_at": "2026-10-01T00:00:00Z"}),
                Provider::Claude,
            ),
            (json!({"provider": "codex"}), Provider::Codex),
            (
                json!({"provider": "codex", "active_start": null, "active_until": null,
                "last_checked": null, "end_plan_checked_for": null}),
                Provider::Codex,
            ),
            (
                json!({"provider": "codex", "active_until": "2026-11-01T00:00:00Z",
                "end_plan_checked_for": "2026-11-01T00:00:00Z"}),
                Provider::Codex,
            ),
        ];
        for (record, provider) in records {
            let metadata: SubscriptionMetadata = serde_json::from_value(record).unwrap();
            assert_eq!(metadata.provider(), provider);
            let encoded = serde_json::to_value(&metadata).unwrap();
            assert_eq!(encoded["provider"], json!(provider.as_str()));
            assert!(encoded.as_object().unwrap().values().all(|v| !v.is_null()));
            assert_eq!(
                serde_json::from_value::<SubscriptionMetadata>(encoded).unwrap(),
                metadata
            );
        }
        for record in [
            json!({"provider": "other"}),
            json!({"provider": "claude"}),
            json!({"provider": "claude", "created_at": null}),
        ] {
            assert!(serde_json::from_value::<SubscriptionMetadata>(record).is_err());
        }
    }

    #[test]
    fn end_check_requires_exact_current_endpoint_and_survives_only_that_endpoint() {
        let until = utc("2026-11-01T00:00:00Z");
        let mut previous = from_codex_auth(&json!({
            "chatgpt_subscription_active_until": "2026-11-01T00:00:00Z"
        }))
        .unwrap();
        assert!(!previous.mark_codex_end_checked(until, until - chrono::Duration::seconds(1)));
        assert!(!previous.mark_codex_end_checked(until + chrono::Duration::seconds(1), until));
        assert!(previous.mark_codex_end_checked(until, until));
        assert!(!previous.mark_codex_end_checked(until, until));
        let same = from_codex_auth(&json!({
            "chatgpt_subscription_active_until": "2026-11-01T09:00:00+09:00"
        }))
        .unwrap()
        .with_previous_codex(Some(&previous));
        assert_eq!(same, previous);
        let absent_end = from_codex_auth(&json!({
            "chatgpt_subscription_last_checked": "2026-10-22T00:00:00Z"
        }))
        .unwrap()
        .with_previous_codex(Some(&previous));
        assert!(matches!(absent_end, SubscriptionMetadata::Codex {
            active_until: Some(end), end_plan_checked_for: Some(marker), ..
        } if end == until && marker == until));
        let changed = from_codex_auth(&json!({
            "chatgpt_subscription_active_until": "2026-12-01T00:00:00Z"
        }))
        .unwrap()
        .with_previous_codex(Some(&previous));
        assert!(matches!(
            changed,
            SubscriptionMetadata::Codex {
                end_plan_checked_for: None,
                ..
            }
        ));
        let mut claude = SubscriptionMetadata::Claude { created_at: until };
        assert!(!claude.mark_codex_end_checked(until, until));
    }
}
