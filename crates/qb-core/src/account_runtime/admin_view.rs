//! Domain-to-wire leaves for the direct AdminAccount producer.
use crate::accounts::{Health, HealthReason, ImportMode, Source, SourceMatch};
use chrono::{DateTime, SecondsFormat, Utc};
use qb_proto::admin_account::*;

/// Serialize the historical 30-key base view. Binding-aware callers serialize
/// all 32 shared fields instead; account construction never starts from a map.
pub(super) fn base_value(view: AdminAccount) -> Result<serde_json::Value, ()> {
    let mut value = bound_value(view)?;
    let object = value.as_object_mut().ok_or(())?;
    object.remove("active_sessions");
    object.remove("spread_threshold");
    Ok(value)
}

pub(super) fn bound_value(view: AdminAccount) -> Result<serde_json::Value, ()> {
    serde_json::to_value(view).map_err(|_| ())
}

pub(super) fn time(value: DateTime<Utc>) -> String {
    // Matches chrono's existing serde timestamp format, including subsecond precision.
    value.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// Reuse the authoritative generic forecast schema when changing its timestamp
/// parameter. This is a typed leaf conversion, never an account/key-map builder.
pub(super) fn wire<T: serde::Serialize, U: serde::de::DeserializeOwned>(
    value: &T,
) -> Result<U, ()> {
    let bytes = serde_json::to_vec(value).map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

pub(super) fn source(value: &Source) -> Result<AdminSource, ()> {
    Ok(match value {
        Source::Oauth => AdminSource::Oauth,
        Source::Takeover { from, at } => AdminSource::Takeover {
            from: from.to_str().ok_or(())?.to_owned(),
            at: time(*at),
        },
        Source::Cpa { mode, dir, r#match } => AdminSource::Cpa {
            mode: match mode {
                ImportMode::Linked => qb_proto::ImportMode::Linked,
                ImportMode::Adopted => qb_proto::ImportMode::Adopted,
            },
            dir: dir.to_str().ok_or(())?.to_owned(),
            r#match: match r#match {
                SourceMatch::Claude {
                    account_uuid,
                    organization_uuid,
                    email,
                } => AdminSourceMatch::Claude {
                    account_uuid: account_uuid.clone(),
                    organization_uuid: organization_uuid.clone(),
                    email: email.clone(),
                },
                SourceMatch::Codex { account_id, email } => AdminSourceMatch::Codex {
                    account_id: account_id.clone(),
                    email: email.clone(),
                },
            },
        },
    })
}

pub(super) fn health(value: &Health) -> AdminHealth {
    match value {
        Health::Ready => AdminHealth::Ready {},
        Health::Cooling {
            reason,
            until,
            scope,
        } => AdminHealth::Cooling {
            reason: match reason {
                HealthReason::Quota => AdminHealthReason::Quota,
                HealthReason::Rate => AdminHealthReason::Rate,
                HealthReason::Auth => AdminHealthReason::Auth,
                HealthReason::Permission => AdminHealthReason::Permission,
                HealthReason::Payment => AdminHealthReason::Payment,
                HealthReason::StoreError => AdminHealthReason::StoreError,
                HealthReason::LinkStale => AdminHealthReason::LinkStale,
            },
            until: time(*until),
            scope: scope.clone(),
            probe_pending: None,
        },
        Health::NeedsRelogin { reason, since } => AdminHealth::NeedsRelogin {
            reason: reason.clone(),
            since: time(*since),
        },
    }
}

/// Same pool classification as the legacy row reader, over typed facts.
/// pool_eligible deliberately remains distinct from these existing counters.
pub(crate) fn pools(accounts: &[AdminAccount]) -> qb_proto::admin_pool::AdminPools {
    let mut pools: qb_proto::admin_pool::AdminPools = Default::default();
    for account in accounts {
        let pool = match account.provider {
            AdminProvider::Claude => &mut pools.claude,
            AdminProvider::Codex => &mut pools.codex,
        };
        if account.user_state != AdminUserState::Active || account.probe_pending {
            pool.unavailable += 1;
            continue;
        }
        match &account.health {
            AdminHealth::Ready {} => pool.eligible += 1,
            AdminHealth::NeedsRelogin { .. } | AdminHealth::Unknown(_) => pool.unavailable += 1,
            AdminHealth::Cooling { reason, .. } => pool.cooling.increment(match reason {
                AdminHealthReason::Quota => "quota",
                AdminHealthReason::Rate => "rate",
                AdminHealthReason::Auth => "auth",
                AdminHealthReason::Permission => "permission",
                AdminHealthReason::Payment => "payment",
                AdminHealthReason::StoreError => "store_error",
                AdminHealthReason::LinkStale => "link_stale",
            }),
        }
    }
    pools
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn account() -> AdminAccount {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        AdminAccount {
            outside_used: false,
            pool_eligible: false,
            takeover_pending: true,
            quota_cooling: None,
            quota_credit_releasable: false,
            observed_at: None,
            quota_notice: None,
            credits_in_use: false,
            usage_forecast: AccountUsageForecast {
                as_of: time(now),
                observed_at: None,
                windows: vec![],
                earliest_window: None,
                credit: None,
                truncated: false,
            },
            has_alternative: false,
            id: "sample-acct".into(),
            generation: 1,
            last_refresh_at: None,
            provider: AdminProvider::Codex,
            label: "sample-acct".into(),
            email: "sample-acct@example.test".into(),
            plan: None,
            user_state: AdminUserState::Active,
            source: AdminSource::Oauth,
            health: AdminHealth::Ready {},
            probe_pending: false,
            pacing: AdminPacing {
                backoff_until: None,
                step: 0,
                release_interval_ms: 0,
                waiting: 0,
            },
            windows: vec![],
            credits: None,
            last_used_at: None,
            bound_sessions: 0,
            token_expires_at: None,
            drain: false,
            allow_credits: false,
            refresh_attention: false,
            active_sessions: 0,
            spread_threshold: 0,
        }
    }

    #[test]
    fn base_and_binding_wrappers_keep_exact_account_keys_order_and_binding_facts() {
        let expected = [
            "outside_used",
            "pool_eligible",
            "takeover_pending",
            "quota_cooling",
            "quota_credit_releasable",
            "observed_at",
            "quota_notice",
            "credits_in_use",
            "usage_forecast",
            "has_alternative",
            "id",
            "generation",
            "last_refresh_at",
            "provider",
            "label",
            "email",
            "plan",
            "user_state",
            "source",
            "health",
            "probe_pending",
            "pacing",
            "windows",
            "credits",
            "last_used_at",
            "bound_sessions",
            "token_expires_at",
            "drain",
            "allow_credits",
            "refresh_attention",
            "active_sessions",
            "spread_threshold",
        ];
        let mut view = account();
        view.bound_sessions = 4;
        view.last_used_at = Some("2027-01-15T08:00:00.123456789Z".into());
        view.active_sessions = 2;
        view.spread_threshold = 3;
        let complete = bound_value(view.clone()).unwrap();
        let keys: Vec<_> = complete
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, expected);
        assert_eq!(complete["bound_sessions"], 4);
        assert_eq!(complete["active_sessions"], 2);
        assert_eq!(complete["spread_threshold"], 3);
        assert_eq!(complete["last_used_at"], "2027-01-15T08:00:00.123456789Z");
        let base = base_value(view).unwrap();
        let keys: Vec<_> = base
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, expected[..30]);
        for key in &expected[..30] {
            assert_eq!(base[*key], complete[*key], "wrapper changed {key}");
        }
        // Required nullable keys stay present, not omitted/defaulted away.
        assert!(base["quota_cooling"].is_null());
        assert!(base["credits"].is_null());
        assert!(base.get("active_sessions").is_none());
        assert!(base.get("spread_threshold").is_none());
    }

    #[test]
    fn typed_pools_preserve_legacy_classification_and_cooling_encounter_order() {
        let mut rows = vec![account()];
        for reason in [
            AdminHealthReason::StoreError,
            AdminHealthReason::Payment,
            AdminHealthReason::Quota,
            AdminHealthReason::Rate,
            AdminHealthReason::Auth,
            AdminHealthReason::Permission,
            AdminHealthReason::LinkStale,
        ] {
            let mut row = account();
            row.health = AdminHealth::Cooling {
                reason,
                until: "2000-01-01T00:00:00Z".into(),
                scope: "account".into(),
                probe_pending: None,
            };
            rows.push(row);
        }
        let mut paused = account();
        paused.provider = AdminProvider::Claude;
        paused.user_state = AdminUserState::Paused;
        rows.push(paused);
        let mut probing = account();
        probing.probe_pending = true;
        rows.push(probing);
        let mut relogin = account();
        relogin.health = AdminHealth::NeedsRelogin {
            reason: "sample-acct".into(),
            since: "2000-01-01T00:00:00Z".into(),
        };
        rows.push(relogin);
        let raw = rows
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let actual = pools(&rows);
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            serde_json::to_string(&crate::account_runtime::typed_pool_counts(&raw)).unwrap()
        );
        // These historical pool counters do not substitute pool_eligible/drain
        // or expire displayed health using a new local clock.
        assert_eq!(actual.codex.eligible, 1);
        assert_eq!(actual.codex.unavailable, 2);
        assert_eq!(actual.claude.unavailable, 1);
        assert_eq!(actual.codex.cooling.iter().next(), Some(("store_error", 1)));
    }

    #[test]
    fn typed_forecast_timestamp_conversion_keeps_all_serialized_facts() {
        let now = DateTime::from_timestamp(1_800_000_000, 123_456_789).unwrap();
        let mut samples = crate::usage_forecast::Forecasts::default();
        for (minute, pct) in [(-10, 40.), (-5, 45.), (0, 50.)] {
            samples.observe(
                "sample-acct",
                "5h",
                pct,
                Some(now + chrono::Duration::hours(3)),
                now + chrono::Duration::minutes(minute),
            );
        }
        let forecast = samples.view("sample-acct", now, None);
        let typed: AccountUsageForecast = wire(&forecast).unwrap();
        assert_eq!(
            serde_json::to_string(&typed).unwrap(),
            serde_json::to_string(&forecast).unwrap()
        );
        let restored: crate::usage_forecast::AccountUsageForecast = wire(&typed).unwrap();
        assert_eq!(restored, forecast);
    }

    #[test]
    fn source_and_health_preserve_wire_order_omission_and_timestamp_precision() {
        let now = DateTime::from_timestamp(1_800_000_000, 123_456_789).unwrap();
        assert_eq!(
            serde_json::to_value(time(now)).unwrap(),
            serde_json::to_value(now).unwrap()
        );
        let sources = [
            Source::Oauth,
            Source::Takeover {
                from: PathBuf::from("sample-acct"),
                at: now,
            },
            Source::Cpa {
                mode: ImportMode::Linked,
                dir: PathBuf::from("sample-acct"),
                r#match: SourceMatch::Claude {
                    account_uuid: "sample-acct".into(),
                    organization_uuid: None,
                    email: "sample-acct@example.test".into(),
                },
            },
            Source::Cpa {
                mode: ImportMode::Adopted,
                dir: PathBuf::from("sample-acct"),
                r#match: SourceMatch::Codex {
                    account_id: "sample-acct".into(),
                    email: "sample-acct@example.test".into(),
                },
            },
        ];
        for value in sources {
            assert_eq!(
                serde_json::to_string(&source(&value).unwrap()).unwrap(),
                serde_json::to_string(&value).unwrap()
            );
        }
        for value in [
            Health::Ready,
            Health::Cooling {
                reason: HealthReason::Auth,
                until: now,
                scope: "account".into(),
            },
            Health::NeedsRelogin {
                reason: "sample-acct".into(),
                since: now,
            },
        ] {
            assert_eq!(
                serde_json::to_string(&health(&value)).unwrap(),
                serde_json::to_string(&value).unwrap()
            );
        }
    }
}
