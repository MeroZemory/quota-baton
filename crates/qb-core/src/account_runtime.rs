use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration as StdDuration, Instant},
};

use axum::{
    Json,
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, broadcast};

use crate::{
    accounts::{Account, AccountStore, Health, HealthReason, Provider, UserState},
    observability::Observability,
};

// Complete usage polls may retire a window. Retain its bounded samples in memory,
// but never expose a forecast for a window absent from the current snapshot.
fn visible_usage_forecast(
    mut forecast: crate::usage_forecast::AccountUsageForecast,
    snapshot: &Value,
    credits_in_use: bool,
) -> crate::usage_forecast::AccountUsageForecast {
    forecast.windows.retain(|window| {
        snapshot["windows"].as_array().is_some_and(|windows| {
            windows
                .iter()
                .any(|current| current["name"].as_str() == Some(&window.name))
        })
    });
    forecast.earliest_window = forecast.windows.iter().filter(|window| {
        matches!(window.outcome, crate::usage_forecast::ForecastOutcome::Exhausted | crate::usage_forecast::ForecastOutcome::Exhaustion)
            && matches!((window.exhaustion_at, window.reset_at), (Some(at), Some(reset)) if at < reset)
    }).min_by_key(|window| window.exhaustion_at).map(|window| window.name.clone());
    if !credits_in_use {
        forecast.credit = None;
    }
    forecast
}

#[cfg(test)]
mod r13_forecast_view_tests {
    use super::*;
    #[test]
    fn r13_forecast_view_hides_retired_windows_and_unpermitted_credit() {
        let now = Utc::now();
        let mut samples = crate::usage_forecast::Forecasts::default();
        for name in ["5h", "7d"] {
            samples.observe("cx-123abc", name, 100., Some(now + Duration::hours(2)), now);
        }
        samples.observe_credit("cx-123abc", 10., now);
        let raw = samples.view("cx-123abc", now, None);
        let snapshot = json!({"windows":[{"name":"7d"}]});
        let hidden = visible_usage_forecast(raw.clone(), &snapshot, false);
        assert_eq!(hidden.windows.len(), 1);
        assert_eq!(hidden.windows[0].name, "7d");
        assert_eq!(hidden.earliest_window.as_deref(), Some("7d"));
        assert!(hidden.credit.is_none());
        assert!(
            visible_usage_forecast(raw, &snapshot, true)
                .credit
                .is_some()
        );
    }
}

#[derive(Clone, Debug)]
pub enum HealthSignal {
    Cooling {
        reason: HealthReason,
        until: DateTime<Utc>,
        scope: String,
    },
    Revoked(String),
    AuthFailure,
    Ready,
}

#[derive(Clone)]
struct LimitResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

struct QuotaNoticeEntry {
    provider: Provider,
    notice: Value,
    state: Value,
    // Retained only in memory, never included in notices/events.
    model: Option<String>,
}
type QuotaNotices = BTreeMap<String, QuotaNoticeEntry>;

#[derive(Clone, PartialEq)]
pub(crate) struct PendingQuotaTicket {
    pub id: String,
    version: u64,
    state: Value,
    session_hash: Option<String>,
    pub model: Option<String>,
}
type PendingQuota = BTreeMap<String, PendingQuotaTicket>;

struct ReleasedQuota {
    at: DateTime<Utc>,
    pending: bool,
    signals: serde_json::Map<String, Value>,
    signal_times: BTreeMap<String, DateTime<Utc>>,
    window_statuses: BTreeMap<String, (Value, DateTime<Utc>)>,
}

impl ReleasedQuota {
    fn observe(&mut self, reported: &Value, snapshot: &Value) {
        let time = |value: &Value| value.as_str()?.parse::<DateTime<Utc>>().ok();
        if let Some(at) = time(&reported["observed_at"]).filter(|at| *at > self.at) {
            if let Some(signals) = reported["signals"].as_object() {
                for (key, value) in signals {
                    if !value.is_null()
                        && self.signal_times.get(key).is_none_or(|prior| at >= *prior)
                    {
                        self.signals.insert(key.clone(), value.clone());
                        self.signal_times.insert(key.clone(), at);
                    }
                }
            }
            if let Some(windows) = reported["windows"].as_array() {
                for window in windows {
                    if let Some(name) = window["name"].as_str()
                        && !window["status"].is_null()
                        && self
                            .window_statuses
                            .get(name)
                            .is_none_or(|(_, prior)| at >= *prior)
                    {
                        self.window_statuses
                            .insert(name.to_owned(), (window.clone(), at));
                    }
                }
            }
        }
        // Canonical nulls include reset invalidations. Retain their timestamps
        // so a delayed explicit report cannot resurrect a superseded claim.
        if let Some(at) = time(&snapshot["observed_at"]).filter(|at| *at > self.at) {
            if let Some(signals) = snapshot["signals"].as_object() {
                for (key, value) in signals {
                    if value.is_null()
                        && self.signal_times.get(key).is_none_or(|prior| at >= *prior)
                    {
                        self.signals.remove(key);
                        self.signal_times.insert(key.clone(), at);
                    }
                }
            }
            if let Some(windows) = snapshot["windows"].as_array() {
                for window in windows {
                    if let Some(name) = window["name"].as_str()
                        && window["status"].is_null()
                        && self
                            .window_statuses
                            .get(name)
                            .is_none_or(|(_, prior)| at >= *prior)
                    {
                        self.window_statuses
                            .insert(name.to_owned(), (window.clone(), at));
                    }
                }
            }
        }
    }
}

fn filter_released_quota(account: &Account, snapshot: &mut Value, release: &ReleasedQuota) {
    // Clearing quota is never permission to spend disallowed credits.
    if account.provider == Provider::Codex
        && !account.allow_credits
        && (crate::credits::entitled(snapshot)
            || snapshot["signals"]["serving_from_credits"] == true
            || snapshot["signals"]["active_limit"] == "credits")
    {
        return;
    }
    let newer = |value: &Value| {
        value
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|observed| observed.with_timezone(&Utc) > release.at)
    };
    if let Some(windows) = snapshot["windows"].as_array_mut() {
        windows.retain_mut(|window| {
            let numeric_fresh = newer(&window["observed_at"]);
            let status = window["name"]
                .as_str()
                .and_then(|name| release.window_statuses.get(name));
            window["status"] =
                status.map_or(Value::Null, |(reported, _)| reported["status"].clone());
            if !numeric_fresh {
                window["used_pct"] = Value::Null;
                window["observed_at"] = Value::Null;
                window["reset_at"] =
                    status.map_or(Value::Null, |(reported, _)| reported["reset_at"].clone());
            }
            numeric_fresh || !window["status"].is_null()
        });
    }
    // A credit-only partial update must not make an inherited old limit flag
    // fresh. Only flags actually reported after release regain authority.
    snapshot["signals"] = Value::Object(release.signals.clone());
}

#[cfg(test)]
mod release_probe_tests {
    use super::*;

    struct PollFixture {
        runtime: Arc<AccountRuntime>,
        id: String,
        _home: tempfile::TempDir,
    }

    impl PollFixture {
        fn new() -> Self {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/r12.9/runtime-fixtures");
            std::fs::create_dir_all(&base).unwrap();
            let home = tempfile::tempdir_in(base).unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            let runtime = AccountRuntime::new(store.clone(), obs, 0.0);
            let mut account = crate::accounts::health::tests::fixture();
            account.allow_credits = true;
            store
                .save(
                    &account,
                    &crate::accounts::Tokens {
                        access_token: "synthetic-access".into(),
                        refresh_token: "synthetic-refresh".into(),
                        id_token: None,
                    },
                    None,
                )
                .unwrap();
            Self {
                runtime,
                id: account.id,
                _home: home,
            }
        }

        fn merge(&self, body: Value, at: DateTime<Utc>) -> (Value, Value) {
            let reported = crate::quota::parse_snapshot_for_test(
                Provider::Codex,
                &serde_json::to_vec(&body).unwrap(),
                at,
            )
            .unwrap();
            // These fixtures contain only the primary slot, so production also
            // treats them as partial polls and preserves missing windows.
            let merged = self
                .runtime
                .obs
                .observe_poll(&self.id, reported.clone(), false)
                .unwrap();
            (merged, reported)
        }

        async fn apply(&self, merged: Value, reported: &Value) -> Account {
            self.runtime
                .apply_quota_snapshot_reported(&self.id, merged, reported, None)
                .await
                .unwrap()
        }

        async fn release(&self, initial: Value) -> DateTime<Utc> {
            let (merged, reported) = self.merge(initial, Utc::now() - Duration::seconds(10));
            let held = self.apply(merged, &reported).await;
            assert!(matches!(
                held.health,
                Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
            ));
            self.runtime.release_quota_hold(&self.id).await.unwrap();
            self.runtime.released_quota.lock().unwrap()[&self.id].at
        }

        fn selection(&self, account: &Account, merged: Value) -> Value {
            self.runtime.selection_quota(account, Some(merged)).unwrap()
        }
    }

    #[tokio::test]
    async fn outage_release_poll_status_only_keeps_fresh_status_without_old_numbers() {
        for (status, rejected) in [("rejected", true), ("allowed", false)] {
            let f = PollFixture::new();
            let reset = Utc::now() + Duration::hours(1);
            let released = f.release(json!({"rate_limit":{"primary_window":{
                "limit_window_seconds":18000,"used_percent":100,"status":"rejected","reset_at":reset
            }}})).await;
            let at = released + Duration::seconds(1);
            let (merged, reported) = f.merge(
                json!({"rate_limit":{"primary_window":{
                    "limit_window_seconds":18000,"status":status
                }}}),
                at,
            );
            assert!(reported["windows"][0]["observed_at"].is_null());
            assert_eq!(merged["windows"][0]["used_pct"], 100.0);
            let raw = f.runtime.obs.quota_snapshot(&f.id).unwrap();
            let account = f.apply(merged.clone(), &reported).await;
            assert_eq!(
                matches!(
                    account.health,
                    Health::Cooling {
                        reason: HealthReason::Quota,
                        ..
                    }
                ),
                rejected
            );
            if rejected {
                assert!(!account.quota_credit_releasable);
            } else {
                assert_eq!(account.health, Health::Ready);
            }
            let selected = f.selection(&account, merged);
            assert_eq!(selected["windows"][0]["status"], status);
            assert!(selected["windows"][0]["used_pct"].is_null());
            assert!(selected["windows"][0]["reset_at"].is_null());
            assert_eq!(f.runtime.obs.quota_snapshot(&f.id).unwrap(), raw);
            if rejected {
                let (merged, delayed) = f.merge(
                    json!({"rate_limit":{"primary_window":{
                        "limit_window_seconds":18000,"status":"allowed"
                    }}}),
                    at - Duration::milliseconds(500),
                );
                let account = f.apply(merged.clone(), &delayed).await;
                assert_eq!(
                    f.selection(&account, merged)["windows"][0]["status"],
                    "rejected"
                );
            }
        }
    }

    #[tokio::test]
    async fn outage_release_poll_numeric_update_does_not_refresh_inherited_status() {
        let f = PollFixture::new();
        let reset = Utc::now() + Duration::hours(1);
        let released = f
            .release(json!({"rate_limit":{"primary_window":{
                "limit_window_seconds":18000,"used_percent":100,"status":"rejected","reset_at":reset
            }}}))
            .await;
        let (merged, reported) = f.merge(
            json!({"rate_limit":{"primary_window":{
                "limit_window_seconds":18000,"used_percent":10,"reset_at":reset
            }}}),
            released + Duration::seconds(1),
        );
        assert!(reported["windows"][0]["status"].is_null());
        assert_eq!(merged["windows"][0]["status"], "rejected");
        let raw = f.runtime.obs.quota_snapshot(&f.id).unwrap();
        let account = f.apply(merged.clone(), &reported).await;
        assert_eq!(account.health, Health::Ready);
        let selected = f.selection(&account, merged);
        assert_eq!(selected["windows"][0]["used_pct"], 10.0);
        assert!(selected["windows"][0]["status"].is_null());
        assert_eq!(f.runtime.obs.quota_snapshot(&f.id).unwrap(), raw);
    }

    #[tokio::test]
    async fn outage_release_poll_reset_invalidates_cached_limit_and_delayed_report() {
        let f = PollFixture::new();
        let reset = Utc::now() + Duration::hours(1);
        let full = json!({"rate_limit":{"limit_reached":true,"primary_window":{
            "limit_window_seconds":18000,"used_percent":100,"reset_at":reset
        }}});
        let released = f.release(full.clone()).await;
        let (merged, delayed) = f.merge(full, released + Duration::seconds(1));
        let account = f.apply(merged, &delayed).await;
        assert!(matches!(
            account.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        let (merged, reported) = f.merge(
            json!({"rate_limit":{"primary_window":{
                "limit_window_seconds":18000,"used_percent":10,"reset_at":reset+Duration::hours(5)
            }}}),
            released + Duration::seconds(2),
        );
        assert!(merged["signals"]["limit_reached"].is_null());
        let account = f.apply(merged.clone(), &reported).await;
        assert_eq!(account.health, Health::Ready);
        assert!(f.selection(&account, merged.clone())["signals"]["limit_reached"].is_null());
        let account = f.apply(merged, &delayed).await;
        assert_eq!(account.health, Health::Ready);
        let (merged, partial) = f.merge(
            json!({"credits":{"balance":0}}),
            released + Duration::seconds(3),
        );
        let account = f.apply(merged.clone(), &partial).await;
        assert_eq!(account.health, Health::Ready);
        assert!(f.selection(&account, merged)["signals"]["limit_reached"].is_null());
    }

    #[tokio::test]
    async fn outage_release_preserves_nonquota_volatile_blockers() {
        for reason in [
            HealthReason::StoreError,
            HealthReason::Auth,
            HealthReason::LinkStale,
        ] {
            let f = PollFixture::new();
            let mut blocked = f.runtime.store.load(&f.id).unwrap();
            blocked.health = Health::Cooling {
                reason,
                until: Utc::now() + Duration::hours(1),
                scope: "account".into(),
            };
            f.runtime.emit_account(&blocked);
            f.release(json!({"rate_limit":{"limit_reached":true,"primary_window":{
                "limit_window_seconds":18000,"used_percent":100,"reset_at":Utc::now()+Duration::hours(1)
            }}})).await;
            assert_eq!(f.runtime.store.load(&f.id).unwrap().health, Health::Ready);
            assert!(f.runtime.blocked_in_memory(&f.id));
            assert_eq!(f.runtime.volatile.lock().unwrap()[&f.id], blocked.health);
        }
    }

    #[test]
    fn outage_release_supersedes_only_old_evidence_and_preserves_credit_permission() {
        let now = Utc::now();
        for allow in [false, true] {
            for credits in [json!({"has_credits":false}), json!({"has_credits":true})] {
                let mut account = crate::accounts::health::tests::fixture();
                account.allow_credits = allow;
                let mut snapshot = json!({"observed_at":now-Duration::seconds(1),
                    "windows":[
                        {"name":"5h","used_pct":100,"observed_at":now-Duration::seconds(1)},
                        {"name":"7d","used_pct":100,"observed_at":now+Duration::seconds(1)}
                    ],"credits":credits,"signals":{"limit_reached":true}});
                let original = snapshot.clone();
                let mut release = ReleasedQuota {
                    at: now,
                    pending: true,
                    signals: Default::default(),
                    signal_times: Default::default(),
                    window_statuses: Default::default(),
                };
                filter_released_quota(&account, &mut snapshot, &release);
                if !allow && credits["has_credits"] == true {
                    assert_eq!(snapshot, original);
                } else {
                    assert_eq!(snapshot["windows"].as_array().unwrap().len(), 1);
                    assert_eq!(snapshot["windows"][0]["name"], "7d");
                    assert!(snapshot["signals"].as_object().unwrap().is_empty());
                    assert_eq!(snapshot["credits"], credits);
                }
                let mut fresh = original;
                fresh["observed_at"] = json!(now + Duration::seconds(1));
                release.signals.insert("limit_reached".into(), true.into());
                filter_released_quota(&account, &mut fresh, &release);
                assert_eq!(fresh["signals"]["limit_reached"], true);
            }
        }
    }

    #[test]
    fn outage_release_probe_is_single_flight_and_cancellation_allows_retry() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r12.9/runtime-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let runtime = AccountRuntime::new(store, obs, 0.0);
        let account = crate::accounts::health::tests::fixture();
        runtime.released_quota.lock().unwrap().insert(
            account.id.clone(),
            ReleasedQuota {
                at: Utc::now(),
                pending: true,
                signals: Default::default(),
                signal_times: Default::default(),
                window_statuses: Default::default(),
            },
        );
        let first = runtime
            .begin_probe(&account)
            .unwrap()
            .expect("quota probe guard");
        assert!(runtime.begin_probe(&account).is_err());
        drop(first);
        assert!(runtime.begin_probe(&account).unwrap().is_some());
    }

    #[tokio::test]
    async fn outage_release_partial_credit_update_does_not_refresh_inherited_limit() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r12.9/runtime-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let runtime = AccountRuntime::new(store.clone(), obs, 0.0);
        let account = crate::accounts::health::tests::fixture();
        let id = &account.id;
        store
            .save(
                &account,
                &crate::accounts::Tokens {
                    access_token: "synthetic-access".into(),
                    refresh_token: "synthetic-refresh".into(),
                    id_token: None,
                },
                None,
            )
            .unwrap();
        let old = Utc::now() - Duration::seconds(10);
        let mut merged = json!({"observed_at":old,"windows":[{
            "name":"5h","used_pct":100,"reset_at":Utc::now()+Duration::hours(1),"observed_at":old
        }],"signals":{"limit_reached":true},"credits":{"has_credits":false,"balance":0}});
        runtime
            .apply_quota_snapshot(id, merged.clone(), None)
            .await
            .unwrap();
        runtime.release_quota_hold(id).await.unwrap();
        let at = runtime.released_quota.lock().unwrap()[id].at + Duration::seconds(1);
        merged["observed_at"] = json!(at);
        let partial = json!({"observed_at":at,"signals":{},"credits":{"balance":0}});
        assert_eq!(
            runtime
                .apply_quota_snapshot_reported(id, merged.clone(), &partial, None)
                .await
                .unwrap()
                .health,
            Health::Ready
        );
        assert!(
            runtime
                .selection_quota(&account, Some(merged.clone()))
                .unwrap()["signals"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        let fresh = json!({"observed_at":at+Duration::seconds(1),"signals":{"limit_reached":true}});
        assert!(matches!(
            runtime
                .apply_quota_snapshot_reported(id, merged.clone(), &fresh, None)
                .await
                .unwrap()
                .health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        // A delayed flag report cannot overwrite a newer report merely
        // because a different partial update made the merged snapshot newer.
        let delayed = json!({"observed_at":at,"signals":{"limit_reached":false}});
        runtime
            .apply_quota_snapshot_reported(id, merged.clone(), &delayed, None)
            .await
            .unwrap();
        assert_eq!(
            runtime.selection_quota(&account, Some(merged)).unwrap()["signals"]["limit_reached"],
            true
        );
    }
}

#[cfg(test)]
mod selection_admission_tests {
    use super::*;
    use crate::{
        accounts::{
            Tokens,
            takeover::{Receipt, ReceiptState},
        },
        bindings::{BindingTable, Candidate, Decision},
        config::RoutingConfig,
    };

    struct Fixture {
        runtime: Arc<AccountRuntime>,
        table: Arc<BindingTable>,
        account: Account,
        _home: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/r11-prep/selection-admission-fixtures");
            std::fs::create_dir_all(&base).unwrap();
            let home = tempfile::tempdir_in(base).unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            let runtime = AccountRuntime::new(store.clone(), obs, 0.0);
            let table = BindingTable::new(home.path(), RoutingConfig::default()).unwrap();
            let account = store
                .save(
                    &crate::accounts::health::tests::fixture(),
                    &Self::tokens("synthetic-access"),
                    None,
                )
                .unwrap();
            Self {
                runtime,
                table,
                account,
                _home: home,
            }
        }

        fn tokens(access: &str) -> Tokens {
            Tokens {
                access_token: access.into(),
                refresh_token: "synthetic-refresh".into(),
                id_token: None,
            }
        }

        fn eligible(&self, existing: bool) -> bool {
            self.runtime
                .with_selection_admission(&self.table, Provider::Codex, None, |eligible| {
                    eligible(&self.account.id, existing)
                })
                .unwrap()
        }
    }

    #[test]
    fn r11_selection_admission_rejects_stale_paused_destination_without_publication() {
        let f = Fixture::new();
        let stale = Candidate {
            account: f.account.clone(),
            quota: None,
            inflight: 0,
        };
        let key = "codex:r11-stale-destination";
        let before = f.table.routing_view();
        f.runtime
            .store
            .update_metadata(&f.account.id, |a| a.user_state = UserState::Paused)
            .unwrap();
        let decision = f
            .runtime
            .with_selection_admission(&f.table, Provider::Codex, None, |eligible| {
                f.table.choose_with_hold_at_admitted_coverage(
                    Provider::Codex,
                    Some(key),
                    None,
                    None,
                    &[stale],
                    None,
                    0,
                    true,
                    false,
                    Utc::now(),
                    None,
                    |id| eligible(id, f.table.existing_placement(Some(key), id)),
                )
            })
            .unwrap();
        assert!(matches!(decision, Decision::None));
        assert_eq!(f.table.current(Some(key)), None);
        assert_eq!(f.table.pending_count(&f.account.id), 0);
        assert_eq!(f.table.routing_view(), before);
    }

    #[test]
    fn r11_selection_admission_preserves_existing_drain_rules_and_runtime_blockers() {
        let f = Fixture::new();
        assert!(f.eligible(false));
        f.runtime
            .store
            .update_metadata(&f.account.id, |a| a.drain = true)
            .unwrap();
        assert!(!f.eligible(false));
        assert!(f.eligible(true));
        let account = f
            .runtime
            .store
            .update_metadata(&f.account.id, |a| {
                a.drain = false;
                a.auth_failures = 1;
            })
            .unwrap();
        let probe = f.runtime.begin_probe(&account).unwrap().unwrap();
        f.runtime
            .with_selection_admission(&f.table, Provider::Codex, None, |eligible| {
                assert!(f.runtime.released_quota.try_lock().is_err());
                assert!(f.runtime.probes.try_lock().is_err());
                assert!(f.runtime.volatile.try_lock().is_err());
                assert!(!eligible(&f.account.id, true));
            })
            .unwrap();
        drop(probe);
        assert!(f.eligible(false));
        let mut blocked = account;
        blocked.health = Health::Cooling {
            reason: HealthReason::StoreError,
            until: Utc::now() + Duration::minutes(1),
            scope: "account".into(),
        };
        f.runtime.emit_account(&blocked);
        assert!(!f.eligible(true));
        f.runtime.clear_memory_health(&blocked);
        assert!(f.eligible(false));
        f.runtime
            .store
            .update_metadata(&f.account.id, |a| {
                a.health = Health::NeedsRelogin {
                    reason: "synthetic".into(),
                    since: Utc::now(),
                };
            })
            .unwrap();
        assert!(!f.eligible(true));
    }

    #[test]
    fn r11_selection_admission_checks_current_credentials_takeover_and_stop() {
        for fenced in [false, true] {
            let f = Fixture::new();
            assert!(f.eligible(false));
            if fenced {
                f.runtime
                    .store
                    .save_takeover_receipt(
                        &f.account,
                        &Fixture::tokens("synthetic-access"),
                        Some(f.account.generation),
                        None,
                        &Receipt {
                            operation: "1".repeat(64),
                            state: ReceiptState::Installed,
                        },
                    )
                    .unwrap();
            } else {
                f.runtime
                    .store
                    .save(&f.account, &Fixture::tokens(""), Some(f.account.generation))
                    .unwrap();
            }
            assert!(!f.eligible(true));
        }
        let f = Fixture::new();
        f.runtime.store.stop_writes();
        assert!(!f.eligible(true));
    }

    #[test]
    fn r11_selection_admission_uses_actual_quota_and_preserves_c7_hold() {
        let f = Fixture::new();
        let key = "codex:r11-admission-hold";
        let now = Utc::now();
        let until = now + Duration::seconds(30);
        let snapshot = |used, at| {
            json!({
                "observed_at": at, "source":"poll", "windows":[
                    {"name":"5h", "used_pct":used, "reset_at":until, "observed_at":at}
                ]
            })
        };
        f.runtime
            .obs
            .observe_poll(&f.account.id, snapshot(98.0, now), true)
            .unwrap();
        let pending = f.table.begin_request(None, &f.account.id);
        // D20 projection reaches 99%, but actual usage still permits dispatch.
        assert!(f.eligible(false));
        drop(pending);
        let candidate = Candidate {
            account: f.account.clone(),
            quota: f.runtime.obs.quota_snapshot(&f.account.id),
            inflight: 0,
        };
        match f.table.choose(
            Provider::Codex,
            Some(key),
            None,
            None,
            &[candidate],
            None,
            0,
        ) {
            Decision::Selected { lease, .. } => {
                lease.dispatched();
                drop(lease);
            }
            other => panic!("{other:?}"),
        }
        f.runtime
            .obs
            .observe_poll(
                &f.account.id,
                snapshot(100.0, now + Duration::seconds(1)),
                true,
            )
            .unwrap();
        assert!(!f.eligible(true));
        let account = f
            .runtime
            .store
            .update_metadata(&f.account.id, |a| {
                a.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope: "account".into(),
                };
                a.quota_cooling = Some(crate::accounts::QuotaCooling {
                    window: "5h".into(),
                    reset_at: until,
                    started_at: now,
                });
            })
            .unwrap();
        let candidate = Candidate {
            account,
            quota: None,
            inflight: 0,
        };
        let called = std::cell::Cell::new(false);
        let decision = f
            .runtime
            .with_selection_admission(&f.table, Provider::Codex, None, |eligible| {
                f.table.choose_with_hold_at_admitted_coverage(
                    Provider::Codex,
                    Some(key),
                    None,
                    None,
                    &[candidate],
                    None,
                    0,
                    true,
                    false,
                    now,
                    None,
                    |id| {
                        called.set(true);
                        eligible(id, true)
                    },
                )
            })
            .unwrap();
        assert!(matches!(decision, Decision::Hold { until: reset } if reset == until));
        assert!(!called.get(), "C7 must hold before dispatch admission");
        assert_eq!(f.table.current(Some(key)), Some(f.account.id));
    }
}

/// The exact upstream observation that a later successful commit may confirm.
#[derive(Clone, Default)]
pub struct QuotaObservation {
    observed_at: Value,
    pub(crate) quota_until: Option<DateTime<Utc>>,
}

pub struct AccountRuntime {
    pub store: Arc<AccountStore>,
    obs: Arc<Observability>,
    events: broadcast::Sender<Value>,
    limits: Mutex<BTreeMap<String, LimitResponse>>,
    probes: Mutex<BTreeSet<String>>,
    released_quota: Arc<Mutex<BTreeMap<String, ReleasedQuota>>>,
    volatile: Mutex<BTreeMap<String, Health>>,
    inflight: Mutex<BTreeMap<String, usize>>,
    poller: Mutex<Option<Weak<crate::quota::UsagePoller>>>,
    refresh_attention: Mutex<BTreeSet<String>>,
    pacing: Mutex<BTreeMap<String, PacingState>>,
    rate_hold_secs: AtomicU64,
    quota_notices: Arc<Mutex<QuotaNotices>>,
    pending_quota: Arc<Mutex<PendingQuota>>,
    quota_resets: Mutex<BTreeMap<String, DateTime<Utc>>>,
    pending_nonce: AtomicU64,
}

const MIN_RELEASE: StdDuration = StdDuration::from_millis(250);
const MAX_RELEASE: StdDuration = StdDuration::from_secs(5);
const DEFAULT_RATE_HOLD_SECS: u64 = 60;

struct PacingWaiter {
    deadline: Instant,
    forced: std::sync::atomic::AtomicBool,
    notify: Notify,
    cancelled: std::sync::atomic::AtomicBool,
}

struct PacingState {
    backoff_until: Option<Instant>,
    window_set_at: Option<Instant>,
    window_had_429: bool,
    step_raised: bool,
    step: u8,
    release_interval: StdDuration,
    last_release: Option<Instant>,
    last_429: Option<Instant>,
    step_decay_at: Option<Instant>,
    step_decay_interval: StdDuration,
    release_decay_at: Option<Instant>,
    waiting: VecDeque<Arc<PacingWaiter>>,
    release_task: bool,
    wake: Arc<Notify>,
}

impl Default for PacingState {
    fn default() -> Self {
        Self {
            backoff_until: None,
            window_set_at: None,
            window_had_429: false,
            step_raised: false,
            step: 0,
            release_interval: MIN_RELEASE,
            last_release: None,
            last_429: None,
            step_decay_at: None,
            step_decay_interval: StdDuration::from_secs(1),
            release_decay_at: None,
            waiting: VecDeque::new(),
            release_task: false,
            wake: Arc::new(Notify::new()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacingSnapshot {
    pub rate_backoff_until: Option<Instant>,
    pub rate_step: u8,
    pub rate_release_interval: StdDuration,
    pub waiting: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Released,
    Forced,
}

pub struct AdmissionGuard {
    admission: Admission,
    waited: bool,
    runtime: Weak<AccountRuntime>,
    id: String,
}

struct AdmissionCancellation {
    runtime: Weak<AccountRuntime>,
    id: String,
    waiter: Arc<PacingWaiter>,
}

impl Drop for AdmissionCancellation {
    fn drop(&mut self) {
        self.waiter.cancelled.store(true, Ordering::Release);
        if let Some(runtime) = self.runtime.upgrade()
            && let Ok(states) = runtime.pacing.lock()
            && let Some(state) = states.get(&self.id)
        {
            state.wake.notify_one();
        }
    }
}

impl AdmissionGuard {
    pub fn waited(&self) -> bool {
        self.waited
    }
    pub fn forced(&self) -> bool {
        self.admission == Admission::Forced
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        let _ = self.runtime.upgrade();
        let _ = &self.id;
    }
}

pub struct InflightGuard {
    runtime: Weak<AccountRuntime>,
    id: String,
}
impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade()
            && let Ok(mut counts) = runtime.inflight.lock()
            && let Some(count) = counts.get_mut(&self.id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.id);
            }
        }
    }
}

pub struct Probe {
    runtime: Weak<AccountRuntime>,
    id: String,
}
#[derive(Debug, thiserror::Error)]
#[error("account authentication probe is already running")]
pub struct ProbeBusy;
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.upgrade()
            && let Ok(mut probes) = runtime.probes.lock()
        {
            probes.remove(&self.id);
        }
    }
}

impl AccountRuntime {
    pub fn new(store: Arc<AccountStore>, obs: Arc<Observability>, _allowance: f64) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        obs.set_events(events.clone());
        let writer = obs.clone();
        let changes = events.clone();
        let quota_notices = Arc::new(Mutex::new(QuotaNotices::new()));
        let notices = quota_notices.clone();
        let pending_quota = Arc::new(Mutex::new(PendingQuota::new()));
        let pending = pending_quota.clone();
        {
            // Account callbacks run under this same gate. Seed the pre-mutation
            // settings before installing the callback; no await/network is involved.
            let transaction = store.transaction();
            match transaction.as_ref() {
                Ok(_) => match store.list_unlocked() {
                    Ok(existing) => obs.dashboard().baseline_accounts(&existing),
                    Err(_) => tracing::warn!("Account observer baseline unavailable"),
                },
                Err(_) => tracing::warn!("Account observer transaction unavailable"),
            }
            store.set_observer(Arc::new(move |account,previous| {
            if let Ok(mut notices) = notices.lock()
                && notices.get(&account.id).is_some_and(|entry| entry.state != notice_state(account)) {
                notices.remove(&account.id);
            }
            if let Ok(mut pending) = pending.lock()
                && pending.get(&account.id).is_some_and(|entry| entry.state != notice_state(account)) {
                pending.remove(&account.id);
            }
            if previous != &account.health {writer.record_account_health(account,previous);}
            writer.record_account(account);
            let _=changes.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state,"plan":account.plan,"generation":account.generation}}));
        }));
        }
        Arc::new(Self {
            store,
            obs,
            events,
            limits: Mutex::new(BTreeMap::new()),
            probes: Mutex::new(BTreeSet::new()),
            released_quota: Arc::new(Mutex::new(BTreeMap::new())),
            volatile: Mutex::new(BTreeMap::new()),
            inflight: Mutex::new(BTreeMap::new()),
            poller: Mutex::new(None),
            refresh_attention: Mutex::new(BTreeSet::new()),
            pacing: Mutex::new(BTreeMap::new()),
            rate_hold_secs: AtomicU64::new(DEFAULT_RATE_HOLD_SECS),
            quota_notices,
            pending_quota,
            quota_resets: Mutex::new(BTreeMap::new()),
            pending_nonce: AtomicU64::new(0),
        })
    }

    /// Only typed notification fields enter this bounded, process-local map.
    pub(crate) fn set_quota_notice(&self, account: &Account, notice: Value, model: Option<&str>) {
        publish_quota_notice(&self.quota_notices, &self.events, account, notice, model);
    }

    pub(crate) fn observe_quota_reset(&self, id: &str, reset: Option<DateTime<Utc>>) {
        if let Ok(mut resets) = self.quota_resets.lock() {
            resets.retain(|_, reset| *reset > Utc::now());
            resets.remove(id);
            if let Some(reset) = reset.filter(|r| *r > Utc::now())
                && resets.len() < 4096
            {
                resets.insert(id.to_owned(), reset);
            }
        }
    }

    pub(crate) fn observed_quota_reset(&self, account: &Account) -> Option<DateTime<Utc>> {
        let reset = self.quota_resets.lock().ok()?.get(&account.id).copied()?;
        matches!(&account.health, Health::Cooling { reason: HealthReason::Quota, until, .. }
            if *until == reset && reset > Utc::now())
        .then_some(reset)
    }

    /// Observation-only entries serialize as quota_notice:null until traffic
    /// resolves an actual forced placement. They share the existing bounded map.
    pub(crate) fn quota_observations(
        &self,
        provider: Provider,
        prior: Option<&str>,
    ) -> Vec<(String, Value, Option<String>)> {
        self.quota_notices
            .lock()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|(id, entry)| {
                        entry.provider == provider && prior.is_none_or(|prior| prior == id.as_str())
                    })
                    .map(|(id, entry)| (id.clone(), entry.state.clone(), entry.model.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn quota_observation_current(&self, id: &str, state: &Value) -> bool {
        self.quota_notices
            .lock()
            .map(|entries| entries.get(id).is_some_and(|entry| &entry.state == state))
            .unwrap_or(false)
    }

    #[cfg(feature = "testkit")]
    pub fn pending_quota_count_for_test(&self, key: &str) -> usize {
        self.pending_quota_tickets(quota_session(Some(key)).as_deref())
            .len()
    }

    pub(crate) fn pending_quota_tickets(
        &self,
        session_hash: Option<&str>,
    ) -> Vec<PendingQuotaTicket> {
        self.pending_quota
            .lock()
            .map(|pending| {
                pending
                    .values()
                    .filter(|entry| entry.session_hash.as_deref() == session_hash)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) async fn pending_quota(
        &self,
        id: &str,
        session_hash: Option<String>,
        model: Option<&str>,
    ) -> Option<PendingQuotaTicket> {
        let store = self.store.clone();
        let key = id.to_owned();
        let account = tokio::task::spawn_blocking(move || store.load(&key))
            .await
            .ok()?
            .ok()?;
        let mut pending = self.pending_quota.lock().ok()?;
        if pending.len() >= 4096 && !pending.contains_key(id) {
            return None;
        }
        let version = self
            .pending_nonce
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |version| {
                version.checked_add(1)
            })
            .ok()?;
        let ticket = PendingQuotaTicket {
            id: id.to_owned(),
            version,
            state: notice_state(&account),
            session_hash,
            model: model.map(str::to_owned),
        };
        pending.insert(id.to_owned(), ticket.clone());
        Some(ticket)
    }

    /// The captured nonce, session, model and state must ALL still match.
    /// Enumeration followed by any await cannot consume a replacement record.
    pub(crate) fn take_pending_quota(&self, ticket: &PendingQuotaTicket) -> bool {
        let Ok(mut pending) = self.pending_quota.lock() else {
            return false;
        };
        if pending.get(&ticket.id) != Some(ticket) {
            return false;
        }
        pending.remove(&ticket.id);
        true
    }

    pub(crate) async fn credit_headers(
        &self,
        id: &str,
        headers: &HeaderMap,
        observation: &QuotaObservation,
    ) {
        // Require evidence on THIS successful response, never a merged old signal.
        let serving = headers
            .get("x-codex-credits-serving")
            .and_then(|v| v.to_str().ok());
        let active = headers
            .get("x-codex-active-limit")
            .and_then(|v| v.to_str().ok());
        if serving == Some("true") || (serving.is_none() && active == Some("credits")) {
            self.credit_notice(id, observation.observed_at.clone())
                .await;
        }
    }

    async fn credit_notice(&self, id: &str, observed_at: Value) {
        if observed_at.is_null() {
            return;
        }
        let store = self.store.clone();
        let obs = self.obs.clone();
        let notices = self.quota_notices.clone();
        let events = self.events.clone();
        let id = id.to_owned();
        // Serialize the final guard, latest observation, health reclassification,
        // and display with settings changes. No stale success may reopen a paid
        // span after off or override newer exhaustion evidence.
        let _ = tokio::task::spawn_blocking(move || -> Result<(), ()> {
            let _guard = store.transaction().map_err(|_| ())?;
            let account = store.read_account_unlocked(&id).map_err(|_| ())?;
            if !paid_observation_allowed(&account) { return Ok(()); }
            let Some(snapshot) = obs.paid_serving_proof(&id, &observed_at) else { return Ok(()); };
            if crate::credits::serving_ended(&snapshot) { return Ok(()); }
            let account = store.update_metadata_unlocked(&id, |a| {
                a.apply_health_signal(crate::accounts::HealthSignal::UsageSnapshot {
                    snapshot:snapshot.clone(), model:None, projected_allowance:0.0
                }, Utc::now());
            }).map_err(|_| ())?;
            if !paid_observation_allowed(&account) { return Ok(()); }
            dashboard_credit_observation(&obs, &id, true, &snapshot);
            let (window,reset_at) = crate::relay::notices::limiting_window(&account,&snapshot);
            publish_quota_notice(&notices,&events,&account,json!({"action":"credits","window":window,
                "reset_at":reset_at,"next_account":null,"earliest_account":null,"observed_at":Utc::now(),"model_limited":false}),None);
            Ok(())
        }).await;
    }

    pub fn set_rate_hold_secs(&self, secs: u64) {
        self.rate_hold_secs.store(secs.max(1), Ordering::Relaxed);
    }

    fn pacing_state<'a>(
        states: &'a mut BTreeMap<String, PacingState>,
        id: &str,
    ) -> &'a mut PacingState {
        states.entry(id.to_owned()).or_default()
    }

    pub fn pacing_snapshot(&self, id: &str) -> PacingSnapshot {
        self.pacing_snapshot_at(id, Instant::now())
    }

    pub fn snapshot(&self, id: &str) -> PacingSnapshot {
        self.pacing_snapshot(id)
    }

    /// Test-clock-friendly single-step admission check. The async `admit`
    /// method uses the same gate and additionally queues blocked callers.
    pub fn admit_at(&self, id: &str, now: Instant) -> bool {
        let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
        let state = Self::pacing_state(&mut states, id);
        Self::decay_pacing(state, now);
        let pacing = state.step > 0 || state.backoff_until.is_some();
        let open = state.waiting.is_empty()
            && (!pacing
                || (state.backoff_until.is_none_or(|until| now >= until)
                    && state.last_release.is_none_or(|last| {
                        now.saturating_duration_since(last) >= state.release_interval
                    })));
        if open && pacing {
            state.last_release = Some(now);
        }
        open
    }

    pub fn pacing_snapshot_at(&self, id: &str, now: Instant) -> PacingSnapshot {
        let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
        let state = Self::pacing_state(&mut states, id);
        Self::decay_pacing(state, now);
        PacingSnapshot {
            rate_backoff_until: state.backoff_until,
            rate_step: state.step,
            rate_release_interval: state.release_interval,
            waiting: state.waiting.len(),
        }
    }

    fn decay_pacing(state: &mut PacingState, now: Instant) {
        if state.backoff_until.is_some_and(|until| until <= now) {
            state.backoff_until = None;
        }
        if state.last_429.is_none() {
            return;
        }
        // A window containing a 429 is not quiet. Each following full quiet
        // window decays one step; successful responses do not reset this clock.
        while state.step > 0 && state.step_decay_at.is_some_and(|at| at <= now) {
            if state.step == 1 && !state.waiting.is_empty() {
                // A draining FIFO is still absorbing the storm. Do not erase
                // its final pacing step and release the entire tail at once.
                state.step_decay_at = Some(now + state.step_decay_interval);
                break;
            }
            state.step -= 1;
            state.step_decay_at = state.step_decay_at.map(|at| at + state.step_decay_interval);
        }
        if state.step == 0 {
            state.step_decay_at = None;
        }
        // Release spacing has its own sixty-second quiet clock. Never advance
        // last_429 while lazily applying either kind of decay.
        while state.release_interval > MIN_RELEASE
            && state.release_decay_at.is_some_and(|at| at <= now)
        {
            state.release_interval = state.release_interval.mul_f64(0.75).max(MIN_RELEASE);
            state.release_decay_at = state
                .release_decay_at
                .map(|at| at + StdDuration::from_secs(60));
        }
        if state.release_interval == MIN_RELEASE {
            state.release_decay_at = None;
        }
    }

    fn finish_queue_transition(state: &mut PacingState, had_waiters: bool, now: Instant) {
        if had_waiters && state.waiting.is_empty() && state.step == 1 {
            let after_drain = now + state.step_decay_interval;
            state.step_decay_at = Some(
                state
                    .step_decay_at
                    .map_or(after_drain, |old| old.max(after_drain)),
            );
        }
    }

    pub fn observe_429(&self, id: &str, retry_after: Option<StdDuration>) -> PacingSnapshot {
        self.observe_429_at(id, Instant::now(), Instant::now(), retry_after, 1.0)
    }

    pub fn observe_429_at(
        &self,
        id: &str,
        sent_at: Instant,
        now: Instant,
        retry_after: Option<StdDuration>,
        jitter: f64,
    ) -> PacingSnapshot {
        let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
        let state = Self::pacing_state(&mut states, id);
        Self::decay_pacing(state, now);
        let new_window = state.backoff_until.is_none_or(|until| now >= until);
        if new_window && state.window_had_429 {
            state.release_interval = (state.release_interval * 2).min(MAX_RELEASE);
        }
        if new_window {
            state.step_raised = false;
        }
        let raises_step =
            !state.step_raised && state.window_set_at.is_none_or(|set| sent_at >= set);
        if raises_step {
            state.step = state.step.saturating_add(1).min(5);
            state.step_raised = true;
        }
        let exponential = StdDuration::from_secs(1u64 << state.step);
        let base = retry_after.unwrap_or_default().max(exponential);
        let factor = jitter.clamp(0.8, 1.2);
        // Jitter may shorten our exponential guess, never server advice.
        let delay = base.mul_f64(factor).max(retry_after.unwrap_or_default());
        let candidate = now + delay;
        state.backoff_until = Some(
            state
                .backoff_until
                .map_or(candidate, |old| old.max(candidate)),
        );
        if new_window {
            state.window_set_at = Some(now);
            state.window_had_429 = true;
        }
        state.last_429 = Some(now);
        state.step_decay_interval = delay;
        state.step_decay_at = state.backoff_until.map(|until| until + delay);
        state.release_decay_at = Some(now + StdDuration::from_secs(60));
        state.wake.notify_one();
        PacingSnapshot {
            rate_backoff_until: state.backoff_until,
            rate_step: state.step,
            rate_release_interval: state.release_interval,
            waiting: state.waiting.len(),
        }
    }

    pub async fn admit(self: &Arc<Self>, id: &str) -> AdmissionGuard {
        let deadline =
            Instant::now() + StdDuration::from_secs(self.rate_hold_secs.load(Ordering::Relaxed));
        self.admit_until(id, deadline).await
    }

    /// Record the one allowed final send after initial pacing admission expires.
    /// An expired retry or cancelled wait must not call this method.
    pub fn record_forced_send(&self, id: &str) {
        let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
        let state = Self::pacing_state(&mut states, id);
        let now = Instant::now();
        Self::decay_pacing(state, now);
        if state.step > 0 || state.backoff_until.is_some() {
            state.last_release = Some(now);
            state.wake.notify_one();
        }
    }

    /// The request budget also bounds later retries. Normal releases stay FIFO;
    /// budget expiry returns Forced. Only initial admission may send once;
    /// that actual dispatch must call record_forced_send. Retries stop on expiry.
    pub async fn admit_until(self: &Arc<Self>, id: &str, deadline: Instant) -> AdmissionGuard {
        let local_deadline =
            Instant::now() + StdDuration::from_secs(self.rate_hold_secs.load(Ordering::Relaxed));
        let waiter = Arc::new(PacingWaiter {
            deadline: deadline.min(local_deadline),
            forced: std::sync::atomic::AtomicBool::new(false),
            notify: Notify::new(),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        });
        let immediate = {
            let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
            let state = Self::pacing_state(&mut states, id);
            let now = Instant::now();
            Self::decay_pacing(state, now);
            let pacing = state.step > 0 || state.backoff_until.is_some();
            let open = state.waiting.is_empty()
                && (!pacing
                    || (state.backoff_until.is_none_or(|until| now >= until)
                        && state.last_release.is_none_or(|last| {
                            now.saturating_duration_since(last) >= state.release_interval
                        })));
            if open || now >= waiter.deadline {
                waiter
                    .forced
                    .store(!open && now >= waiter.deadline, Ordering::Release);
                if pacing && open {
                    state.last_release = Some(now);
                }
                true
            } else {
                state.waiting.push_back(waiter.clone());
                state.wake.notify_one(); // a later arrival can have less request budget
                if !state.release_task {
                    state.release_task = true;
                    let runtime = Arc::clone(self);
                    let account = id.to_owned();
                    tokio::spawn(async move {
                        runtime.release_loop(account).await;
                    });
                }
                false
            }
        };
        if !immediate {
            let cancellation = AdmissionCancellation {
                runtime: Arc::downgrade(self),
                id: id.to_owned(),
                waiter: waiter.clone(),
            };
            waiter.notify.notified().await;
            drop(cancellation);
        }
        AdmissionGuard {
            waited: !immediate,
            admission: if waiter.forced.load(Ordering::Acquire) {
                Admission::Forced
            } else {
                Admission::Released
            },
            runtime: Arc::downgrade(self),
            id: id.to_owned(),
        }
    }

    async fn release_loop(self: Arc<Self>, id: String) {
        loop {
            let (wait, wake, done) = {
                let mut states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
                let state = Self::pacing_state(&mut states, &id);
                let now = Instant::now();
                Self::decay_pacing(state, now);
                let had_waiters = !state.waiting.is_empty();
                while state
                    .waiting
                    .front()
                    .is_some_and(|waiter| waiter.cancelled.load(Ordering::Acquire))
                {
                    state.waiting.pop_front();
                }
                Self::finish_queue_transition(state, had_waiters, now);
                let selected = state
                    .waiting
                    .iter()
                    .position(|w| !w.cancelled.load(Ordering::Acquire) && now >= w.deadline)
                    .unwrap_or(0);
                let Some(waiter) = state.waiting.get(selected).cloned() else {
                    state.release_task = false;
                    break;
                };
                let deadline = waiter.deadline;
                let pacing = state.step > 0 || state.backoff_until.is_some();
                let gate = if pacing {
                    state.backoff_until.unwrap_or(now).max(
                        state
                            .last_release
                            .map_or(now, |last| last + state.release_interval),
                    )
                } else {
                    now
                };
                if now >= gate || now >= deadline {
                    let forced = now >= deadline;
                    state.waiting.remove(selected);
                    Self::finish_queue_transition(state, true, now);
                    if pacing && !forced {
                        state.last_release = Some(now);
                    }
                    (Some(waiter), state.wake.clone(), forced)
                } else {
                    (None, state.wake.clone(), false)
                }
            };
            if let Some(waiter) = wait {
                waiter.forced.store(done, Ordering::Release);
                waiter.notify.notify_one();
                continue;
            }
            let sleep_for = {
                let states = self.pacing.lock().unwrap_or_else(|e| e.into_inner());
                let state = states.get(&id).expect("pacing state exists");
                let now = Instant::now();
                let pacing = state.step > 0 || state.backoff_until.is_some_and(|until| until > now);
                let next = if pacing {
                    state
                        .backoff_until
                        .unwrap_or(now)
                        .max(
                            state
                                .last_release
                                .map_or(now, |last| last + state.release_interval),
                        )
                        .min(
                            state
                                .waiting
                                .iter()
                                .filter(|w| !w.cancelled.load(Ordering::Acquire))
                                .map(|w| w.deadline)
                                .min()
                                .unwrap_or(now),
                        )
                } else {
                    now
                };
                next.saturating_duration_since(now)
            };
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {},
                _ = wake.notified() => {},
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub fn set_refresh_attention(&self, id: &str, attention: bool) {
        let mut flags = self
            .refresh_attention
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let changed = if attention {
            flags.insert(id.into())
        } else {
            flags.remove(id)
        };
        if changed {
            let _ = self.events.send(
                json!({"event":"account_changed","data":{"id":id,"refresh_attention":attention}}),
            );
        }
    }

    pub(crate) fn set_poller(&self, poller: &Arc<crate::quota::UsagePoller>) {
        *self.poller.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(poller));
    }

    pub fn track_inflight(self: &Arc<Self>, id: &str) -> InflightGuard {
        if let Ok(mut counts) = self.inflight.lock() {
            let count = counts.entry(id.to_owned()).or_default();
            *count = count.saturating_add(1);
        }
        InflightGuard {
            runtime: Arc::downgrade(self),
            id: id.to_owned(),
        }
    }

    pub fn inflight_count(&self, id: &str) -> usize {
        self.inflight
            .lock()
            .ok()
            .and_then(|counts| counts.get(id).copied())
            .unwrap_or(0)
    }

    pub fn probing(&self, id: &str) -> bool {
        self.probes.lock().map(|p| p.contains(id)).unwrap_or(true)
    }
    pub fn blocked_in_memory(&self, id: &str) -> bool {
        self.volatile
            .lock()
            .map(|v| v.contains_key(id))
            .unwrap_or(true)
    }
    pub fn clear_memory_health(&self, account: &Account) {
        self.clear_memory_health_if(account, false);
    }

    fn clear_memory_health_if(&self, account: &Account, quota_only: bool) {
        if let Ok(mut pending) = self.pending_quota.lock() {
            pending.remove(&account.id);
        }
        if let Ok(mut notices) = self.quota_notices.lock() {
            notices.remove(&account.id);
        }
        if let Ok(mut health) = self.volatile.lock()
            && (!quota_only
                || health.get(&account.id).is_some_and(|health| {
                    matches!(
                        health,
                        Health::Cooling {
                            reason: HealthReason::Quota,
                            ..
                        }
                    )
                }))
        {
            health.remove(&account.id);
        }
        let _=self.events.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state}}));
    }
    pub fn begin_probe(self: &Arc<Self>, account: &Account) -> Result<Option<Probe>, ProbeBusy> {
        let released = self.released_quota.lock().map_err(|_| ProbeBusy)?;
        let quota_pending = released.get(&account.id).is_some_and(|r| r.pending);
        if (!account.auth_probe_pending && account.auth_failures == 0 && !quota_pending)
            || !matches!(
                account.health,
                Health::Ready
                    | Health::Cooling {
                        reason: HealthReason::Quota,
                        ..
                    }
            )
        {
            return Ok(None);
        }
        let mut probes = self.probes.lock().map_err(|_| ProbeBusy)?;
        if !probes.insert(account.id.clone()) {
            return Err(ProbeBusy);
        }
        Ok(Some(Probe {
            runtime: Arc::downgrade(self),
            id: account.id.clone(),
        }))
    }
    pub async fn probe_success(&self, id: &str) -> Result<(), ()> {
        self.finish_probe(id, true).await
    }

    pub(crate) async fn poll_probe_success(&self, id: &str) -> Result<(), ()> {
        self.finish_probe(id, false).await
    }

    async fn finish_probe(&self, id: &str, serving: bool) -> Result<(), ()> {
        let store = self.store.clone();
        let id = id.to_owned();
        let released_id = id.clone();
        tokio::task::spawn_blocking(move || {
            store
                .update_metadata(&id, |a| {
                    if a.health == Health::Ready {
                        a.auth_failures = 0;
                        a.auth_probe_pending = false;
                    }
                })
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())??;
        if serving
            && let Ok(mut released) = self.released_quota.lock()
            && let Some(release) = released.get_mut(&released_id)
        {
            release.pending = false;
        }
        Ok(())
    }

    pub fn emit_account(&self, account: &Account) {
        if let Ok(mut pending) = self.pending_quota.lock() {
            pending.remove(&account.id);
        }
        if let Ok(mut notices) = self.quota_notices.lock() {
            notices.remove(&account.id);
        }
        if let Ok(mut health) = self.volatile.lock() {
            health.insert(account.id.clone(), account.health.clone());
        }
        let _=self.events.send(json!({"event":"account_changed","data":{"id":account.id,"health":account.health,"user_state":account.user_state}}));
    }

    /// Explicit operator probe. Only quota cooling is cleared; credentials,
    /// payment/auth restrictions and user choices remain unchanged.
    pub async fn release_quota_hold(self: &Arc<Self>, id: &str) -> Result<Account, ()> {
        let store = self.store.clone();
        let runtime = self.clone();
        let id = id.to_owned();
        let account = tokio::task::spawn_blocking(move || {
            let _guard = store.transaction().map_err(|_| ())?;
            let account = store.read_account_unlocked(&id).map_err(|_| ())?;
            if store.writes_stopped() {
                return Err(());
            }
            // A paid-serving claim is a payment permission gate, not just
            // exhausted subscription quota. Releasing it cannot grant consent.
            if account.provider == Provider::Codex
                && !account.allow_credits
                && runtime.obs.quota_snapshot(&id).is_some_and(|snapshot| {
                    snapshot["signals"]["serving_from_credits"] == true
                        || snapshot["signals"]["active_limit"] == "credits"
                })
            {
                return Err(());
            }
            if !matches!(
                account.health,
                Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
            ) {
                return Err(());
            }
            // Publish the cutoff with the durable state under the same writer
            // lock. Cancellation cannot leave Ready without its probe marker.
            let released_at = Utc::now();
            let account = store
                .update_metadata_unlocked(&id, |account| {
                    account.health = Health::Ready;
                    account.quota_cooling = None;
                    account.quota_credit_releasable = false;
                })
                .map_err(|_| ())?;
            runtime.released_quota.lock().map_err(|_| ())?.insert(
                account.id.clone(),
                ReleasedQuota {
                    at: released_at,
                    pending: true,
                    signals: Default::default(),
                    signal_times: Default::default(),
                    window_statuses: Default::default(),
                },
            );
            runtime.clear_memory_health_if(&account, true);
            Ok::<_, ()>(account)
        })
        .await
        .map_err(|_| ())??;
        Ok(account)
    }

    /// Explicit release supersedes only older selection evidence. Display and
    /// persisted usage stay intact; fresh observations and health gates win.
    /// Process-local: restart conservatively requires another operator release.
    pub(crate) fn selection_quota(
        &self,
        account: &Account,
        mut quota: Option<Value>,
    ) -> Option<Value> {
        let Ok(released) = self.released_quota.lock() else {
            return quota;
        };
        let Some(release) = released.get(&account.id) else {
            return quota;
        };
        let Some(snapshot) = quota.as_mut() else {
            return quota;
        };
        filter_released_quota(account, snapshot, release);
        quota
    }

    /// Wrap the blocking chooser, then call `eligible(id, existing_placement)`
    /// from its admission callback while the binding mutation gate is held.
    /// Keep candidates intact so C7 can still hold a cooling original account.
    ///
    /// Lock order: store transaction -> released quota -> probes -> volatile
    /// health -> binding mutation. The callback must not reacquire store/runtime
    /// locks. All guards remain held through policy and binding publication.
    pub(crate) fn with_selection_admission<T>(
        &self,
        bindings: &crate::bindings::BindingTable,
        provider: Provider,
        model: Option<&str>,
        choose: impl FnOnce(&dyn Fn(&str, bool) -> bool) -> T,
    ) -> Result<T, ()> {
        self.with_selection_admission_models(bindings, provider, |eligible| {
            choose(&|id, existing| eligible(id, existing, model))
        })
    }

    /// Placement may retain a proved model or clear an unproved implicit default.
    /// Check its actual effective model without reacquiring any admission locks.
    pub(crate) fn with_selection_admission_models<T>(
        &self,
        bindings: &crate::bindings::BindingTable,
        provider: Provider,
        choose: impl FnOnce(&dyn Fn(&str, bool, Option<&str>) -> bool) -> T,
    ) -> Result<T, ()> {
        let _transaction = self.store.transaction().map_err(|_| ())?;
        // begin_probe also locks released quota before probes.
        let released = self.released_quota.lock().map_err(|_| ())?;
        let probes = self.probes.lock().map_err(|_| ())?;
        let volatile = self.volatile.lock().map_err(|_| ())?;
        let eligible = |id: &str, existing_placement: bool, model: Option<&str>| {
            if self.store.writes_stopped() || probes.contains(id) || volatile.contains_key(id) {
                return false;
            }
            let Ok(mut account) = self.store.read_account_unlocked(id) else {
                return false;
            };
            // Expire only this local copy: no observer or durable mutation is
            // needed to assess the current row under the transaction.
            account.expire_health(Utc::now());
            if account.provider != provider
                || !account.eligible(model)
                || (account.drain && !existing_placement)
                || !self
                    .store
                    .tokens_unlocked(&account)
                    .is_ok_and(|tokens| !tokens.access_token.is_empty())
            {
                return false;
            }
            let mut quota = self.obs.quota_snapshot(id);
            if let (Some(snapshot), Some(release)) = (quota.as_mut(), released.get(id)) {
                filter_released_quota(&account, snapshot, release);
            }
            bindings.account_dispatch_eligible(
                &crate::bindings::Candidate {
                    account,
                    quota,
                    inflight: 0,
                },
                model,
            )
        };
        Ok(choose(&eligible))
    }

    pub async fn transition(&self, id: &str, signal: HealthSignal) -> Result<Account, ()> {
        let store = self.store.clone();
        let id = id.to_owned();
        let account = tokio::task::spawn_blocking(move || {
            let updated = store
                .update_metadata(&id, |account| {
                    use crate::accounts::HealthSignal as Signal;
                    let signal = match signal {
                        HealthSignal::Cooling {
                            reason,
                            until,
                            scope,
                        } => Signal::Cooling {
                            reason,
                            until,
                            scope,
                        },
                        HealthSignal::Revoked(reason) => Signal::Revoked { reason },
                        HealthSignal::AuthFailure => Signal::AuthAfterRefresh,
                        HealthSignal::Ready => Signal::Ready,
                    };
                    account.apply_health_signal(signal, Utc::now());
                })
                .map_err(|_| ())?;
            Ok::<_, ()>(updated)
        })
        .await
        .map_err(|_| ())??;
        Ok(account)
    }

    pub async fn eligible(&self, id: &str, model: Option<&str>) -> bool {
        if self.blocked_in_memory(id) || self.probing(id) {
            return false;
        }
        let store = self.store.clone();
        let id = id.to_owned();
        let model = model.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            store.takeover_fenced(&id).is_ok_and(|fenced| !fenced)
                && store.load(&id).is_ok_and(|a| a.eligible(model.as_deref()))
        })
        .await
        .unwrap_or(false)
    }

    pub async fn change(
        &self,
        id: &str,
        user_state: Option<UserState>,
        label: Option<String>,
        drain: Option<bool>,
        allow_credits: Option<bool>,
    ) -> Result<Account, ()> {
        if label
            .as_ref()
            .is_some_and(|l| l.len() > 256 || l.chars().any(char::is_control))
        {
            return Err(());
        }
        let store = self.store.clone();
        let id = id.to_owned();
        let obs = self.obs.clone();
        let notices = self.quota_notices.clone();
        let account = tokio::task::spawn_blocking(move || {
            store
                .update_settings(&id, drain, allow_credits, |a| {
                    if let Some(state) = user_state {
                        a.user_state = state;
                    }
                    if let Some(label) = label {
                        a.label = label;
                    }
                    if let Some(drain) = drain {
                        a.drain = drain;
                    }
                    if let Some(allow) = allow_credits {
                        a.allow_credits = allow;
                    }
                    if allow_credits == Some(false) {
                        let snapshot = obs.quota_snapshot(&a.id).unwrap_or_default();
                        dashboard_credit_observation(&obs, &a.id, false, &snapshot);
                        if let Ok(mut notices) = notices.lock()
                            && notices
                                .get(&a.id)
                                .is_some_and(|entry| entry.notice["action"] == "credits")
                        {
                            notices.remove(&a.id);
                        }
                    }
                    if allow_credits.is_some()
                        && let Some(snapshot) = obs.quota_snapshot(&id)
                    {
                        let now = Utc::now();
                        let signal = || crate::accounts::HealthSignal::UsageSnapshot {
                            snapshot: snapshot.clone(),
                            model: None,
                            projected_allowance: 0.0,
                        };
                        if allow_credits == Some(true)
                            && a.quota_credit_releasable
                            && matches!(&a.health, Health::Cooling {
                                reason: HealthReason::Quota, scope, ..
                            } if scope == "account")
                        {
                            // Release only a hold that C5 attributes to disabled credits.
                            // Classify from Ready so existing cooling cannot mask hard limits.
                            let mut classified = a.clone();
                            classified.health = Health::Ready;
                            classified.quota_cooling = None;
                            classified.allow_credits = false;
                            classified.apply_health_signal(signal(), now);
                            let credit_hold = matches!(&classified.health, Health::Cooling {
                                reason: HealthReason::Quota, scope, ..
                            } if scope == "account");
                            classified.health = Health::Ready;
                            classified.quota_cooling = None;
                            classified.allow_credits = true;
                            classified.apply_health_signal(signal(), now);
                            if credit_hold && classified.health == Health::Ready {
                                a.apply_health_signal(crate::accounts::HealthSignal::Ready, now);
                            }
                        }
                        // Settings and observed health commit under the same store lock.
                        a.apply_health_signal(signal(), now);
                    }
                })
                .map_err(|_| ())
        })
        .await
        .map_err(|_| ())??;
        Ok(account)
    }

    pub async fn initialize_settings(
        &self,
        settings: std::collections::BTreeMap<String, crate::config::AccountConfig>,
    ) -> Result<(), ()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            for account in store.list().map_err(|_| ())? {
                if let Some(settings) = settings.get(&account.id)
                    && (account.drain != settings.drain
                        || account.allow_credits != settings.allow_credits)
                {
                    store
                        .update_metadata(&account.id, |a| {
                            a.drain = settings.drain;
                            a.allow_credits = settings.allow_credits;
                        })
                        .map_err(|_| ())?;
                }
            }
            Ok::<_, ()>(())
        })
        .await
        .map_err(|_| ())?
    }

    pub async fn views(&self) -> Result<Vec<Value>, ()> {
        let store = self.store.clone();
        let (accounts, credential_ids, takeover_fenced) = tokio::task::spawn_blocking(move || {
            let accounts = store.list().map_err(|_| ())?;
            let takeover_fenced = store.takeover_fenced_ids().map_err(|_| ())?;
            let credential_ids: BTreeSet<_> = accounts
                .iter()
                .filter(|a| store.tokens(a).is_ok_and(|t| !t.access_token.is_empty()))
                .map(|a| a.id.clone())
                .collect();
            Ok::<_, ()>((accounts, credential_ids, takeover_fenced))
        })
        .await
        .map_err(|_| ())??;
        let now = Utc::now();
        let eligible: Vec<_> = accounts
            .iter()
            .filter(|a| {
                credential_ids.contains(&a.id)
                    && !a.drain
                    && a.eligible_at(None, now)
                    && !self.probing(&a.id)
                    && !self.blocked_in_memory(&a.id)
                    && !a.auth_probe_pending
                    && a.auth_failures == 0
            })
            .collect();
        if let Ok(mut released) = self.released_quota.lock() {
            released.retain(|id, _| accounts.iter().any(|a| &a.id == id));
        }
        if let Ok(mut resets) = self.quota_resets.lock() {
            resets.retain(|id, reset| *reset > now && accounts.iter().any(|a| &a.id == id));
        }
        let available = |a: &Account, model: Option<&str>| {
            credential_ids.contains(&a.id)
                && !a.drain
                && a.eligible_at(model, now)
                && !self.probing(&a.id)
                && !self.blocked_in_memory(&a.id)
                && !a.auth_probe_pending
                && a.auth_failures == 0
        };
        let mut notices = self.quota_notices.lock().map_err(|_| ())?;
        notices.retain(|id, entry| {
            let Some(a) = accounts.iter().find(|a| &a.id == id) else {
                return false;
            };
            let model = entry.model.as_deref();
            let notice = &mut entry.notice;
            if notice_state(a) != entry.state || a.user_state != UserState::Active {
                return false;
            }
            if notice["action"] == "credits" {
                if !a.allow_credits
                    || a.payment_until.is_some_and(|until| until > now)
                    || !matches!(
                        a.health,
                        Health::Ready
                            | Health::Cooling {
                                reason: HealthReason::Quota,
                                ..
                            }
                    )
                {
                    return false;
                }
            } else if !crate::relay::notices::quota_applies(a, model) {
                return false;
            }
            if notice["action"] == "failover"
                && !accounts.iter().any(|other| {
                    notice["next_account"] == other.id
                        && other.provider == a.provider
                        && available(other, model)
                })
            {
                return false;
            }
            if notice["action"] == "exhausted" {
                if accounts
                    .iter()
                    .any(|other| other.provider == a.provider && available(other, model))
                {
                    return false;
                }
                // Re-evaluate the whole participating pool, not only the previous winner.
                let earliest = accounts
                    .iter()
                    .filter(|other| {
                        other.provider == a.provider
                            && other.user_state == UserState::Active
                            && !other.drain
                    })
                    .filter_map(|other| {
                        let snapshot = self.obs.quota_snapshot(&other.id).unwrap_or_default();
                        crate::relay::notices::limiting_reset(
                            other,
                            &snapshot,
                            model,
                            self.observed_quota_reset(other),
                        )
                        .map(|reset| (other.id.clone(), reset))
                    })
                    .min_by_key(|(_, reset)| *reset);
                let reset_at = json!(earliest.as_ref().map(|(_, reset)| *reset));
                let earliest_account = json!(earliest.map(|(id, _)| id));
                let model_limited = json!(accounts.iter().any(|other| other.provider
                    == a.provider
                    && other.user_state == UserState::Active
                    && !other.drain
                    && crate::relay::notices::model_quota_applies(other, model)));
                if notice["reset_at"] != reset_at
                    || notice["earliest_account"] != earliest_account
                    || notice["model_limited"] != model_limited
                {
                    notice["reset_at"] = reset_at;
                    notice["earliest_account"] = earliest_account;
                    notice["model_limited"] = model_limited;
                    notice["observed_at"] = json!(now);
                }
            }
            notice["reset_at"]
                .as_str()
                .and_then(|v| v.parse::<DateTime<Utc>>().ok())
                .is_none_or(|r| r > now)
        });
        if let Ok(mut pending) = self.pending_quota.lock() {
            pending.retain(|id, entry| {
                accounts.iter().any(|a| {
                    &a.id == id
                        && a.user_state == UserState::Active
                        && notice_state(a) == entry.state
                })
            });
        }
        Ok(accounts.iter().map(|a| {
            let quota_notice = notices.get(&a.id).map(|entry| entry.notice.clone());
            let has_alternative = eligible.iter().any(|other| other.id != a.id && other.provider == a.provider);
            // Reuse actual successful C9 paid-serving context, not capability,
            // allowance, or a projected meter. C5 accepts the upstream credit
            // claim even when its displayed percent is rounded below 100.
            let credits_in_use = a.provider == Provider::Codex && a.allow_credits
                && quota_notice.as_ref().is_some_and(|notice| notice["action"] == "credits");
            let quota = self.obs.quota_snapshot(&a.id).unwrap_or_default();
            let pacing = self.pacing_snapshot(&a.id);
            let backoff_until = pacing.rate_backoff_until.and_then(|until| {
                let remaining = until.saturating_duration_since(Instant::now());
                chrono::Duration::from_std(remaining).ok().map(|d| Utc::now() + d)
            });
            let health=self.volatile.lock().ok().and_then(|v|v.get(&a.id).cloned()).unwrap_or(a.health.clone());
            let pending = self.probing(&a.id) || ((a.auth_probe_pending || a.auth_failures > 0) && health == Health::Ready);
            let quota_credit_releasable = a.quota_credit_releasable
                && a.payment_until.is_none_or(|until| until <= now)
                && !pending && !a.auth_probe_pending && a.auth_failures == 0
                && !takeover_fenced.contains(&a.id) && !self.blocked_in_memory(&a.id);
            let health = if pending { json!({"kind":"cooling","reason":"auth","until":Utc::now(),"scope":"account","probe_pending":true}) } else { json!(health) };
            let usage_forecast = visible_usage_forecast(self.obs.dashboard().usage_forecast(&a.id, now, None), &quota, credits_in_use);
            json!({"pool_eligible":a.user_state == UserState::Active && !pending && !takeover_fenced.contains(&a.id) && health["kind"] == "ready","takeover_pending":takeover_fenced.contains(&a.id),"quota_cooling":a.quota_cooling,"quota_credit_releasable":quota_credit_releasable,"observed_at":quota["observed_at"],"quota_notice":quota_notice,"credits_in_use":credits_in_use,"usage_forecast":usage_forecast,"has_alternative":has_alternative,"id":a.id,"generation":a.generation,"last_refresh_at":a.token_meta.last_refresh_at,"provider":a.provider,"label":a.label,"email":a.email,"plan":quota["plan"].as_str().map(str::to_owned).or(a.plan.clone()),"user_state":a.user_state,"source":a.source,"health":health,"probe_pending":pending,"pacing":{"backoff_until":backoff_until,"step":pacing.rate_step,"release_interval_ms":pacing.rate_release_interval.as_millis() as u64,"waiting":pacing.waiting},"windows":quota["windows"].as_array().cloned().unwrap_or_default(),"credits":quota["credits"],"last_used_at":null,"bound_sessions":0,"token_expires_at":a.token_meta.expires_at,"drain":a.drain,"allow_credits":a.allow_credits,"refresh_attention":takeover_fenced.contains(&a.id) || self.refresh_attention.lock().map(|f|f.contains(&a.id)).unwrap_or(false)})
        }).collect())
    }

    /// Build account views with the live affinity statistics held by the
    /// bindings store.  AccountRuntime does not own that store, so callers
    /// that expose the complete AccountView pass it explicitly.
    pub async fn views_with_bindings(
        &self,
        bindings: &crate::bindings::BindingTable,
    ) -> Result<Vec<Value>, ()> {
        let mut views = self.views().await?;
        for view in &mut views {
            if let Some(id) = view["id"].as_str().map(str::to_owned) {
                let (count, last_used) = bindings.account_stats(&id);
                view["bound_sessions"] = json!(count);
                view["last_used_at"] = json!(last_used);
                let active = bindings.active_sessions(&id, Utc::now());
                view["active_sessions"] = json!(active);
                view["spread_threshold"] = json!(bindings.spread_threshold_for(&id));
                let mut forecast = self.obs.dashboard().usage_forecast(
                    &id,
                    Utc::now(),
                    bindings.account_count_changed_at(&id),
                );
                forecast = visible_usage_forecast(
                    forecast,
                    view,
                    view["credits_in_use"].as_bool() == Some(true),
                );
                view["usage_forecast"] = json!(forecast);
            }
        }
        Ok(views)
    }

    pub(crate) async fn eligible_unknown_model(&self, id: &str) -> bool {
        if !self.eligible(id, None).await {
            return false;
        }
        let volatile = self
            .volatile
            .lock()
            .ok()
            .and_then(|map| map.get(id).cloned());
        if matches!(volatile, Some(Health::Cooling { until, scope, .. }) if until > Utc::now() && scope.starts_with("model:"))
        {
            return false;
        }
        let store = self.store.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || store.load(&id).is_ok_and(|a| !matches!(a.health, Health::Cooling { until, scope, .. } if until > Utc::now() && scope.starts_with("model:")))).await.unwrap_or(false)
    }

    pub async fn observe_headers(
        &self,
        id: &str,
        provider: Provider,
        headers: &HeaderMap,
    ) -> QuotaObservation {
        self.observe_headers_for_model(id, provider, headers, None)
            .await
    }

    pub async fn observe_headers_for_model(
        &self,
        id: &str,
        provider: Provider,
        headers: &HeaderMap,
        model: Option<&str>,
    ) -> QuotaObservation {
        self.observe_headers_inner(id, provider, headers, model, None)
            .await
    }

    pub(crate) async fn observe_headers_asof(
        &self,
        id: &str,
        provider: Provider,
        headers: &HeaderMap,
        model: Option<&str>,
        evidence: (
            &crate::bindings::BindingTable,
            crate::bindings::UsageObservationAsOf,
        ),
    ) -> QuotaObservation {
        self.observe_headers_inner(id, provider, headers, model, Some(evidence))
            .await
    }

    async fn observe_headers_inner(
        &self,
        id: &str,
        provider: Provider,
        headers: &HeaderMap,
        model: Option<&str>,
        evidence: Option<(
            &crate::bindings::BindingTable,
            crate::bindings::UsageObservationAsOf,
        )>,
    ) -> QuotaObservation {
        let Some((snapshot, reported)) = self.obs.observe_headers_reported(id, provider, headers)
        else {
            return QuotaObservation::default();
        };
        if let Some((bindings, asof)) = evidence {
            bindings.note_usage_observation_asof_token(id, &reported, asof);
        }
        let observed_at = snapshot["observed_at"].clone();
        let quota_until = self
            .apply_quota_snapshot_reported(id, snapshot, &reported, model)
            .await
            .ok()
            .and_then(|account| match account.health {
                Health::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope,
                } if until > Utc::now()
                    && (scope == "account"
                        || model.is_some_and(|m| scope == format!("model:{m}"))) =>
                {
                    Some(until)
                }
                _ => None,
            });
        QuotaObservation {
            observed_at,
            quota_until,
        }
    }

    pub async fn observe_codex_event(&self, id: &str, event: &Value) {
        self.observe_codex_event_for_model(id, event, None).await;
    }

    pub async fn observe_codex_event_for_model(
        &self,
        id: &str,
        event: &Value,
        model: Option<&str>,
    ) {
        self.observe_codex_event_inner(id, event, model, None).await;
    }
    pub(crate) async fn observe_codex_event_asof(
        &self,
        id: &str,
        event: &Value,
        model: Option<&str>,
        evidence: (
            &crate::bindings::BindingTable,
            crate::bindings::UsageObservationAsOf,
        ),
    ) {
        self.observe_codex_event_inner(id, event, model, Some(evidence))
            .await;
    }
    async fn observe_codex_event_inner(
        &self,
        id: &str,
        event: &Value,
        model: Option<&str>,
        evidence: Option<(
            &crate::bindings::BindingTable,
            crate::bindings::UsageObservationAsOf,
        )>,
    ) {
        if event["type"] != "codex.rate_limits" {
            return;
        }
        let Some((snapshot, reported)) = self.obs.observe_codex_event_reported(id, event) else {
            return;
        };
        if let Some((bindings, asof)) = evidence {
            bindings.note_usage_observation_asof_token(id, &reported, asof);
        }
        let observed_at = snapshot["observed_at"].clone();
        let _ = self
            .apply_quota_snapshot_reported(id, snapshot, &reported, model)
            .await;
        let limits = event.get("rate_limits").unwrap_or(event);
        let serving = event
            .get("serving_from_credits")
            .or_else(|| limits.get("serving_from_credits"))
            .or_else(|| event.get("credits").and_then(|c| c.get("used")))
            .and_then(Value::as_bool);
        let active = event
            .get("active_limit")
            .or_else(|| limits.get("active_limit"))
            .and_then(Value::as_str);
        if serving == Some(true) || (serving.is_none() && active == Some("credits")) {
            self.credit_notice(id, observed_at).await;
        }
    }

    /// Apply an observed snapshot through the same persisted reducer as outcomes
    /// and credential failures. Per-window timestamps identify fresh evidence.
    pub async fn apply_quota_snapshot(
        &self,
        id: &str,
        snapshot: Value,
        model: Option<&str>,
    ) -> Result<Account, ()> {
        let reported = snapshot.clone();
        self.apply_quota_snapshot_reported(id, snapshot, &reported, model)
            .await
    }

    pub(crate) async fn apply_quota_snapshot_reported(
        &self,
        id: &str,
        snapshot: Value,
        reported: &Value,
        model: Option<&str>,
    ) -> Result<Account, ()> {
        let reported = reported.clone();
        let store = self.store.clone();
        let obs = self.obs.clone();
        let notices = self.quota_notices.clone();
        let account_id = id.to_owned();
        let model = model.map(str::to_owned);
        let released_quota = self.released_quota.clone();
        let (account, snapshot) = tokio::task::spawn_blocking(move || -> Result<_, ()> {
            let _guard = store.transaction().map_err(|_| ())?;
            let time = |v: &Value| v.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok());
            let mut snapshot = snapshot;
            let mut model = model;
            if let Some(current) = obs.quota_snapshot(&account_id)
                && let Some((new, incoming)) =
                    time(&current["observed_at"]).zip(time(&snapshot["observed_at"]))
                && new >= incoming
            {
                if new > incoming {
                    model = None;
                }
                snapshot = current;
            }
            if let Ok(mut released) = released_quota.lock()
                && let Some(release) = released.get_mut(&account_id)
            {
                release.observe(&reported, &snapshot);
            }
            let account = store
                .update_metadata_unlocked(&account_id, |account| {
                    let mut health_snapshot = snapshot.clone();
                    if let Ok(released) = released_quota.lock()
                        && let Some(release) = released.get(&account_id)
                    {
                        filter_released_quota(account, &mut health_snapshot, release);
                    }
                    account.apply_health_signal(
                        crate::accounts::HealthSignal::UsageSnapshot {
                            snapshot: health_snapshot,
                            model: model.clone(),
                            projected_allowance: 0.0,
                        },
                        Utc::now(),
                    );
                })
                .map_err(|_| ())?;
            apply_snapshot_notices(
                &obs,
                &notices,
                &account_id,
                &account,
                &snapshot,
                model.as_deref(),
            );
            Ok((account, snapshot))
        })
        .await
        .map_err(|_| ())??;
        let _ = self
            .events
            .send(json!({"event":"quota_updated","data":{"id":id,"snapshot":snapshot}}));
        Ok(account)
    }

    pub fn cache_limit(&self, id: &str, status: StatusCode, headers: &HeaderMap, raw: &[u8]) {
        if raw.len() > 65536 {
            return;
        }
        let headers = headers
            .iter()
            .filter(|(name, _)| {
                let n = name.as_str();
                n.starts_with("x-codex-")
                    || n.starts_with("anthropic-ratelimit-")
                    || matches!(n, "retry-after" | "content-type" | "content-encoding")
            })
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        if let Ok(mut limits) = self.limits.lock() {
            limits.insert(
                id.into(),
                LimitResponse {
                    status,
                    headers,
                    body: Bytes::copy_from_slice(raw),
                },
            );
        }
    }

    pub async fn unavailable(
        &self,
        provider: Provider,
        websocket: bool,
        model: Option<&str>,
    ) -> Response {
        let poller = self
            .poller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(poller) = poller {
            poller.before_unavailable(provider).await;
        }
        self.unavailable_known(provider, websocket, model).await
    }

    pub(crate) async fn unavailable_known(
        &self,
        provider: Provider,
        websocket: bool,
        model: Option<&str>,
    ) -> Response {
        let store = self.store.clone();
        let accounts = tokio::task::spawn_blocking(move || store.list())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let candidates: Vec<_> = accounts
            .iter()
            .filter(|a| a.provider == provider && a.user_state == UserState::Active)
            .collect();
        let volatile = self.volatile.lock().map(|v| v.clone()).unwrap_or_default();
        let now = Utc::now();
        let mut cooling = Vec::new();
        for a in &candidates {
            for health in std::iter::once(&a.health).chain(volatile.get(&a.id)) {
                if let Health::Cooling {
                    reason,
                    until,
                    scope,
                } = health
                    && *until > now
                    && (scope == "account" || model.is_some_and(|m| scope == &format!("model:{m}")))
                {
                    let replay = matches!(reason, HealthReason::Quota | HealthReason::Auth);
                    cooling.push((*a, *until, replay));
                }
            }
            if self.probing(&a.id)
                || ((a.auth_probe_pending || a.auth_failures > 0) && a.health == Health::Ready)
            {
                cooling.push((*a, now + Duration::seconds(1), false));
            }
        }
        if !cooling.is_empty() {
            // Prefer genuine limit-response resets when available. Other
            // temporary blockers still produce row 14, using the native shape.
            cooling.sort_by_key(|(_, until, replay)| (!*replay, *until));
            let (a, until, replay) = cooling[0];
            let seconds = (until - Utc::now()).num_seconds().max(1);
            let mut result = if !websocket
                && replay
                && let Some(cache) = self.limits.lock().ok().and_then(|m| m.get(&a.id).cloned())
            {
                let mut r = Response::new(Body::from(cache.body));
                *r.status_mut() = cache.status;
                *r.headers_mut() = cache.headers;
                r
            } else {
                let error = if provider == Provider::Codex {
                    json!({"error":{"type":"usage_limit_reached","message":"quota-baton: all accounts are cooling","resets_at":until.timestamp(),"resets_in_seconds":seconds}})
                } else {
                    json!({"type":"error","error":{"type":"rate_limit_error","message":"quota-baton: all accounts are cooling"}})
                };
                (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response()
            };
            result.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&seconds.to_string()).unwrap(),
            );
            result
        } else {
            let mut error = json!({"error":{"type":"invalid_request_error","message":format!("quota-baton: no usable {} account — open the tray",provider.as_str())}});
            if provider == Provider::Claude {
                error["type"] = "error".into();
            }
            (StatusCode::BAD_REQUEST, Json(error)).into_response()
        }
    }
}

/// Per-provider subscription pool counts shared by admin state and CLI status.
/// Paused/disabled/relogin/probe-pending accounts are unavailable; active
/// canonical coolings are grouped by reason. No credential fields are read.
pub fn pool_counts(accounts: &[Value]) -> Value {
    let mut pools = json!({
        "claude": {"eligible":0,"cooling":{},"unavailable":0},
        "codex": {"eligible":0,"cooling":{},"unavailable":0}
    });
    for account in accounts {
        let Some(provider @ ("claude" | "codex")) = account["provider"].as_str() else {
            continue;
        };
        let pool = &mut pools[provider];
        let kind = account["health"]["kind"].as_str().unwrap_or("unknown");
        if account["user_state"] != "active"
            || account["probe_pending"] == true
            || !matches!(kind, "ready" | "cooling")
        {
            pool["unavailable"] = json!(pool["unavailable"].as_u64().unwrap_or(0) + 1);
        } else if kind == "cooling" {
            // Store-backed views have already expired their health. A volatile
            // store-error remains blocked until its pending write succeeds,
            // even after its displayed retry deadline.
            let reason = account["health"]["reason"]
                .as_str()
                .filter(|reason| {
                    matches!(
                        *reason,
                        "quota"
                            | "rate"
                            | "auth"
                            | "permission"
                            | "payment"
                            | "store_error"
                            | "link_stale"
                    )
                })
                .unwrap_or("unknown");
            pool["cooling"][reason] = json!(pool["cooling"][reason].as_u64().unwrap_or(0) + 1);
        } else {
            pool["eligible"] = json!(pool["eligible"].as_u64().unwrap_or(0) + 1);
        }
    }
    pools
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    #[test]
    fn c6_4_pools_count_each_provider_and_cooling_reason_without_credentials() {
        let accounts = vec![
            json!({"provider":"claude","user_state":"active","health":{"kind":"ready"}}),
            json!({"provider":"claude","user_state":"active","health":{"kind":"cooling","reason":"payment","until":"2099-01-01T00:00:00Z"}}),
            json!({"provider":"claude","user_state":"disabled","health":{"kind":"ready"}}),
            json!({"provider":"codex","user_state":"active","health":{"kind":"cooling","reason":"quota","until":"2099-01-01T00:00:00Z"}}),
            json!({"provider":"codex","user_state":"active","health":{"kind":"needs_relogin"}}),
            // Volatile persistence failures do not expire into eligibility.
            json!({"provider":"codex","user_state":"active","health":{"kind":"cooling","reason":"store_error","until":"2000-01-01T00:00:00Z"}}),
        ];
        assert_eq!(
            pool_counts(&accounts),
            json!({
                "claude":{"eligible":1,"cooling":{"payment":1},"unavailable":1},
                "codex":{"eligible":0,"cooling":{"quota":1,"store_error":1},"unavailable":1}
            })
        );
    }
}

// Tests-only addition for crates/qb-core/src/account_runtime.rs.
// Append after the staged production candidate; do not replace production code.
#[cfg(test)]
mod r1_rate_tests {
    use super::*;
    use futures_util::poll;
    use std::task::Poll;

    const ID: &str = "synthetic-r1-rate";

    struct Fixture {
        runtime: Arc<AccountRuntime>,
        // Declared last: runtime/store/observability drop before the home.
        _home: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/r1-rate-test-fixtures");
            std::fs::create_dir_all(&base).unwrap();
            let home = tempfile::tempdir_in(base).unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            Self {
                runtime: AccountRuntime::new(store, obs, 1.0),
                _home: home,
            }
        }
    }

    #[tokio::test]
    async fn tray2_pending_replacement_during_await_is_not_consumed_for_same_or_other_session() {
        let f = Fixture::new();
        let cpa = f._home.path().join("synthetic-pending");
        std::fs::create_dir_all(&cpa).unwrap();
        std::fs::write(cpa.join("pending.json"), serde_json::to_vec(&json!({
            "type":"codex","email":"pending@example.invalid","account_id":"synthetic-pending",
            "access_token":"synthetic-access-pending","refresh_token":"synthetic-refresh-pending",
            "expired":"2099-01-01T00:00:00Z"
        })).unwrap()).unwrap();
        crate::accounts::import::run(
            &f.runtime.store,
            &cpa,
            &["codex:pending@example.invalid".into()],
            false,
        )
        .unwrap();
        let id = f.runtime.store.list().unwrap()[0].id.clone();
        for replacement_session in ["codex:S", "codex:T"] {
            let s = quota_session(Some("codex:S"));
            let replacement_hash = quota_session(Some(replacement_session));
            let first = f
                .runtime
                .pending_quota(&id, s.clone(), Some("private-model"))
                .await
                .unwrap();
            let (enumerated_tx, enumerated_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
            let runtime = f.runtime.clone();
            let consumer = tokio::spawn(async move {
                let ticket = runtime.pending_quota_tickets(s.as_deref()).pop().unwrap();
                enumerated_tx.send(()).unwrap();
                // Deterministic suspension at the production enumerate/await/take boundary.
                resume_rx.await.unwrap();
                runtime.take_pending_quota(&ticket)
            });
            enumerated_rx.await.unwrap();
            let replacement = f
                .runtime
                .pending_quota(&id, replacement_hash.clone(), Some("private-model"))
                .await
                .unwrap();
            assert_eq!(first.state, replacement.state);
            assert_ne!(first.version, replacement.version);
            resume_tx.send(()).unwrap();
            assert!(
                !consumer.await.unwrap(),
                "stale consumer removed a replacement pending error"
            );
            let retained = f.runtime.pending_quota_tickets(replacement_hash.as_deref());
            assert_eq!(retained.len(), 1);
            assert!(retained[0] == replacement);
            assert!(f.runtime.take_pending_quota(&replacement));
        }
    }

    fn at(start: Instant, seconds: u64) -> Instant {
        start + StdDuration::from_secs(seconds)
    }

    // Two non-quiet windows: [0,2] and [2,6]. Quiet step windows then
    // end at 10 and 14; independent spacing decay starts at 2+60=62.
    fn two_windows(runtime: &AccountRuntime, start: Instant) {
        let first = runtime.observe_429_at(ID, start, start, None, 1.0);
        assert_eq!(first.rate_step, 1);
        assert_eq!(first.rate_backoff_until, Some(at(start, 2)));
        assert_eq!(first.rate_release_interval, MIN_RELEASE);
        let second = runtime.observe_429_at(ID, at(start, 2), at(start, 2), None, 1.0);
        assert_eq!(second.rate_step, 2);
        assert_eq!(second.rate_backoff_until, Some(at(start, 6)));
        assert_eq!(second.rate_release_interval, StdDuration::from_millis(500));
    }

    #[test]
    fn draining_an_upper_step_keeps_its_original_quiet_schedule() {
        for removal in ["release", "deadline", "cancellation"] {
            let f = Fixture::new();
            let start = Instant::now();
            two_windows(&f.runtime, start);
            {
                let mut states = f.runtime.pacing.lock().unwrap();
                let state = states.get_mut(ID).unwrap();
                let waiter = Arc::new(PacingWaiter {
                    deadline: at(start, 60),
                    forced: std::sync::atomic::AtomicBool::new(removal == "deadline"),
                    notify: Notify::new(),
                    cancelled: std::sync::atomic::AtomicBool::new(removal == "cancellation"),
                });
                state.waiting.push_back(waiter);
                // Drain at8, between the window ending6 and quiet boundary10.
                state.waiting.pop_front();
                AccountRuntime::finish_queue_transition(state, true, at(start, 8));
            }
            assert_eq!(
                f.runtime
                    .pacing_snapshot_at(ID, at(start, 10) - StdDuration::from_nanos(1))
                    .rate_step,
                2,
                "{removal}"
            );
            assert_eq!(
                f.runtime.pacing_snapshot_at(ID, at(start, 10)).rate_step,
                1,
                "{removal}"
            );
            assert_eq!(
                f.runtime.pacing_snapshot_at(ID, at(start, 14)).rate_step,
                0,
                "{removal}"
            );
            let g = Fixture::new();
            two_windows(&g.runtime, start);
            {
                let mut states = g.runtime.pacing.lock().unwrap();
                let state = states.get_mut(ID).unwrap();
                AccountRuntime::finish_queue_transition(state, true, at(start, 8));
            }
            let next = g
                .runtime
                .observe_429_at(ID, at(start, 11), at(start, 11), None, 1.0);
            assert_eq!(next.rate_step, 2, "{removal}");
            assert_eq!(next.rate_backoff_until, Some(at(start, 15)), "{removal}");
        }
    }
    #[tokio::test]
    async fn actual_fifo_release_starts_the_last_quiet_interval_at_drain() {
        let f = Fixture::new();
        f.runtime.observe_429(ID, None);
        let guards = futures_util::future::join_all((0..16).map(|_| f.runtime.admit(ID))).await;
        let drained = f
            .runtime
            .pacing
            .lock()
            .unwrap()
            .get(ID)
            .unwrap()
            .last_release
            .unwrap();
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            drained + StdDuration::from_millis(400),
        ))
        .await;
        assert_eq!(
            f.runtime.pacing_snapshot(ID).rate_step,
            1,
            "the FIFO drained between decay boundaries"
        );
        let _first = f.runtime.admit(ID).await;
        let before = Instant::now();
        let _next = f.runtime.admit(ID).await;
        assert!(
            before.elapsed() >= StdDuration::from_millis(180),
            "post-drain admissions still need spacing"
        );
        let until = {
            let states = f.runtime.pacing.lock().unwrap();
            states.get(ID).unwrap().step_decay_at.unwrap()
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            until + StdDuration::from_millis(30),
        ))
        .await;
        assert_eq!(f.runtime.pacing_snapshot(ID).rate_step, 0);
        drop(guards);
    }
    #[test]
    fn a_queued_tail_keeps_a_full_quiet_interval_from_the_actual_drain() {
        for removal in ["release", "deadline", "cancellation"] {
            let f = Fixture::new();
            let start = Instant::now();
            f.runtime.observe_429_at(ID, start, start, None, 1.0);
            let waiter = Arc::new(PacingWaiter {
                deadline: at(start, 60),
                forced: std::sync::atomic::AtomicBool::new(false),
                notify: Notify::new(),
                cancelled: std::sync::atomic::AtomicBool::new(false),
            });
            f.runtime
                .pacing
                .lock()
                .unwrap()
                .get_mut(ID)
                .unwrap()
                .waiting
                .push_back(waiter);
            assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 4)).rate_step, 1);
            let drained = at(start, 5) + StdDuration::from_millis(750);
            {
                let mut states = f.runtime.pacing.lock().unwrap();
                let state = states.get_mut(ID).unwrap();
                let waiter = state.waiting.pop_front().unwrap();
                if removal == "cancellation" {
                    waiter.cancelled.store(true, Ordering::Release);
                }
                if removal == "deadline" {
                    waiter.forced.store(true, Ordering::Release);
                }
                AccountRuntime::finish_queue_transition(state, true, drained);
            }
            assert_eq!(
                f.runtime.pacing_snapshot_at(ID, at(start, 6)).rate_step,
                1,
                "{removal}"
            );
            let until = drained + StdDuration::from_secs(2);
            assert_eq!(
                f.runtime
                    .pacing_snapshot_at(ID, until - StdDuration::from_nanos(1))
                    .rate_step,
                1,
                "{removal}"
            );
            assert_eq!(
                f.runtime.pacing_snapshot_at(ID, until).rate_step,
                0,
                "{removal}"
            );
        }
    }
    #[test]
    fn r1_step_decays_at_full_quiet_backoff_boundaries_not_sixty_seconds() {
        let f = Fixture::new();
        let start = Instant::now();
        two_windows(&f.runtime, start);
        let window_end = f.runtime.pacing_snapshot_at(ID, at(start, 6));
        assert!(window_end.rate_backoff_until.is_none());
        assert_eq!(
            window_end.rate_step, 2,
            "the 429-bearing window was not quiet"
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 10) - StdDuration::from_nanos(1))
                .rate_step,
            2
        );
        let first_quiet = f.runtime.pacing_snapshot_at(ID, at(start, 10));
        assert_eq!(
            first_quiet.rate_step, 1,
            "one complete quiet backoff interval"
        );
        assert_eq!(
            first_quiet.rate_release_interval,
            StdDuration::from_millis(500)
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 14) - StdDuration::from_nanos(1))
                .rate_step,
            1
        );
        let second_quiet = f.runtime.pacing_snapshot_at(ID, at(start, 14));
        assert_eq!(second_quiet.rate_step, 0);
        assert_eq!(
            second_quiet.rate_release_interval,
            StdDuration::from_millis(500)
        );
    }

    #[test]
    fn r1_step_snapshots_do_not_move_release_decay_anchor_or_double_apply_decay() {
        let f = Fixture::new();
        let start = Instant::now();
        two_windows(&f.runtime, start);
        assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 14)).rate_step, 0);
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 62) - StdDuration::from_nanos(1))
                .rate_release_interval,
            StdDuration::from_millis(500)
        );
        let first = f.runtime.pacing_snapshot_at(ID, at(start, 62));
        assert_eq!(first.rate_release_interval, StdDuration::from_millis(375));
        assert_eq!(
            f.runtime.pacing_snapshot_at(ID, at(start, 62)),
            first,
            "lazy decay is idempotent"
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 122))
                .rate_release_interval,
            StdDuration::from_micros(281_250)
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 182))
                .rate_release_interval,
            MIN_RELEASE
        );
        let states = f.runtime.pacing.lock().unwrap();
        assert_eq!(
            states[ID].last_429,
            Some(at(start, 2)),
            "a decay is not another 429"
        );
    }

    #[test]
    fn r1_late_inflight_429_extends_window_and_restarts_both_quiet_clocks() {
        let f = Fixture::new();
        let start = Instant::now();
        two_windows(&f.runtime, start);
        // Sent before this window was set: it may extend the gate but must not
        // count as a new step. The new quiet boundaries are 13,17 and 65.
        let late = f.runtime.observe_429_at(ID, start, at(start, 5), None, 1.0);
        assert_eq!(late.rate_step, 2);
        assert_eq!(late.rate_backoff_until, Some(at(start, 9)));
        assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 10)).rate_step, 2);
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 13) - StdDuration::from_nanos(1))
                .rate_step,
            2
        );
        assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 13)).rate_step, 1);
        assert_eq!(f.runtime.pacing_snapshot_at(ID, at(start, 17)).rate_step, 0);
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 62))
                .rate_release_interval,
            StdDuration::from_millis(500)
        );
        assert_eq!(
            f.runtime
                .pacing_snapshot_at(ID, at(start, 65))
                .rate_release_interval,
            StdDuration::from_millis(375)
        );
    }

    #[test]
    fn r1_quiet_step_zero_removes_spacing_before_release_interval_reaches_floor() {
        let f = Fixture::new();
        let start = Instant::now();
        two_windows(&f.runtime, start);
        let now = at(start, 14);
        let snapshot = f.runtime.pacing_snapshot_at(ID, now);
        assert_eq!(snapshot.rate_step, 0);
        assert_eq!(
            snapshot.rate_release_interval,
            StdDuration::from_millis(500)
        );
        for _ in 0..32 {
            assert!(
                f.runtime.admit_at(ID, now),
                "step zero must not retain a spacing gate"
            );
        }
    }

    fn queue_deadlines(runtime: &AccountRuntime) -> Vec<Instant> {
        runtime.pacing.lock().unwrap()[ID]
            .waiting
            .iter()
            .map(|w| w.deadline)
            .collect()
    }

    fn last_release(runtime: &AccountRuntime) -> Option<Instant> {
        runtime.pacing.lock().unwrap()[ID].last_release
    }

    fn block(runtime: &AccountRuntime) {
        runtime.observe_429(ID, Some(StdDuration::from_secs(30)));
    }

    // Open the backoff only; deliberately leave last_release intact so the
    // real release_loop still enforces normal/forced release spacing.
    fn open_backoff(runtime: &AccountRuntime) {
        let mut states = runtime.pacing.lock().unwrap();
        let state = states.get_mut(ID).unwrap();
        state.backoff_until = None;
        state.wake.notify_one();
    }

    #[tokio::test]
    async fn r1_admit_until_uses_minimum_of_caller_deadline_and_local_cap() {
        let f = Fixture::new();
        block(&f.runtime);
        f.runtime.set_rate_hold_secs(1);
        let before = Instant::now();
        let mut local = Box::pin(f.runtime.admit_until(ID, at(before, 20)));
        assert!(poll!(local.as_mut()).is_pending());
        let after = Instant::now();
        let queued = queue_deadlines(&f.runtime);
        assert_eq!(queued.len(), 1);
        assert!(queued[0] >= at(before, 1) && queued[0] <= at(after, 1));
        drop(local);
        let caller = Instant::now() + StdDuration::from_millis(100);
        let mut shorter = Box::pin(f.runtime.admit_until(ID, caller));
        assert!(poll!(shorter.as_mut()).is_pending());
        assert_eq!(*queue_deadlines(&f.runtime).last().unwrap(), caller);
        drop(shorter);
        tokio::task::yield_now().await;
        assert!(queue_deadlines(&f.runtime).is_empty());
        assert!(!f.runtime.pacing.lock().unwrap()[ID].release_task);
        assert!(last_release(&f.runtime).is_none());
    }

    #[tokio::test]
    async fn r1_retries_reuse_one_absolute_budget_and_expired_retry_is_immediate() {
        let f = Fixture::new();
        block(&f.runtime);
        open_backoff(&f.runtime);
        let deadline = Instant::now() + StdDuration::from_millis(120);
        let first = f.runtime.admit_until(ID, deadline).await;
        assert!(!first.forced());
        block(&f.runtime); // A retry sees a new 429, not a new request budget.
        let mut retry = Box::pin(f.runtime.admit_until(ID, deadline));
        assert!(poll!(retry.as_mut()).is_pending());
        assert_eq!(*queue_deadlines(&f.runtime).last().unwrap(), deadline);
        let forced = tokio::time::timeout(StdDuration::from_millis(600), retry)
            .await
            .expect("retry renewed the total request budget");
        assert!(forced.forced());
        assert!(
            last_release(&f.runtime).unwrap() < deadline,
            "an unsent timeout cannot consume a release slot"
        );
        block(&f.runtime);
        let mut expired = Box::pin(f.runtime.admit_until(ID, deadline));
        let Poll::Ready(final_admission) = poll!(expired.as_mut()) else {
            panic!("already exhausted absolute budget was queued again");
        };
        assert!(final_admission.forced());
        assert!(queue_deadlines(&f.runtime).is_empty());
    }

    #[tokio::test]
    async fn r1_forced_admission_updates_shared_clock_before_next_normal_release() {
        let f = Fixture::new();
        block(&f.runtime);
        let forced = f.runtime.admit_until(ID, Instant::now()).await;
        assert!(forced.forced());
        assert!(
            last_release(&f.runtime).is_none(),
            "unsent expiry is not dispatch"
        );
        let before = Instant::now();
        f.runtime.record_forced_send(ID);
        let stamp = last_release(&f.runtime).expect("actual forced send must update shared clock");
        assert!(stamp >= before && stamp <= Instant::now());
        open_backoff(&f.runtime);
        let interval = f.runtime.pacing_snapshot(ID).rate_release_interval;
        assert!(
            !f.runtime
                .admit_at(ID, stamp + interval - StdDuration::from_nanos(1))
        );
        assert!(f.runtime.admit_at(ID, stamp + interval));
    }

    #[tokio::test]
    async fn unsent_expired_waiter_does_not_spend_the_next_waiters_release_slot() {
        let f = Fixture::new();
        block(&f.runtime);
        let mut expired = Box::pin(
            f.runtime
                .admit_until(ID, Instant::now() + StdDuration::from_millis(20)),
        );
        let mut next = Box::pin(
            f.runtime
                .admit_until(ID, Instant::now() + StdDuration::from_secs(10)),
        );
        assert!(poll!(expired.as_mut()).is_pending());
        assert!(poll!(next.as_mut()).is_pending());
        assert!(expired.await.forced());
        assert!(
            last_release(&f.runtime).is_none(),
            "an unsent timeout consumed a release slot"
        );
        open_backoff(&f.runtime);
        assert!(!next.await.forced());
        assert!(last_release(&f.runtime).is_some());
    }
    #[tokio::test]
    async fn open_gate_after_old_rate_deadline_is_a_normal_release_with_shared_clock() {
        let f = Fixture::new();
        block(&f.runtime);
        open_backoff(&f.runtime);
        let before = Instant::now();
        let forced = f
            .runtime
            .admit_until(ID, before - StdDuration::from_millis(1))
            .await;
        assert!(
            !forced.forced(),
            "an open gate does not spend an expired pacing wait budget"
        );
        let stamp = last_release(&f.runtime).expect("normal release did not update the clock");
        assert!(stamp >= before && stamp <= Instant::now());
        let interval = f.runtime.pacing_snapshot(ID).rate_release_interval;
        assert!(
            !f.runtime
                .admit_at(ID, stamp + interval - StdDuration::from_nanos(1))
        );
        assert!(f.runtime.admit_at(ID, stamp + interval));
    }

    #[tokio::test]
    async fn r1_normal_waiters_stay_fifo_and_cancelled_middle_consumes_no_release() {
        let f = Fixture::new();
        block(&f.runtime);
        let deadline = at(Instant::now(), 5);
        let mut first = Box::pin(f.runtime.admit_until(ID, deadline));
        let mut cancelled = Box::pin(f.runtime.admit_until(ID, deadline));
        let mut last = Box::pin(f.runtime.admit_until(ID, deadline));
        assert!(poll!(first.as_mut()).is_pending());
        assert!(poll!(cancelled.as_mut()).is_pending());
        assert!(poll!(last.as_mut()).is_pending());
        assert_eq!(queue_deadlines(&f.runtime).len(), 3);
        drop(cancelled);
        open_backoff(&f.runtime);
        let first_guard = tokio::time::timeout(StdDuration::from_millis(200), first)
            .await
            .unwrap();
        assert!(!first_guard.forced());
        let first_stamp = last_release(&f.runtime).unwrap();
        assert!(
            poll!(last.as_mut()).is_pending(),
            "normal tail overtook release spacing"
        );
        let last_guard = tokio::time::timeout(StdDuration::from_millis(700), last)
            .await
            .unwrap();
        assert!(!last_guard.forced());
        let last_stamp = last_release(&f.runtime).unwrap();
        let gap = last_stamp.duration_since(first_stamp);
        assert!(
            gap >= MIN_RELEASE && gap < MIN_RELEASE * 2,
            "cancelled middle consumed a release slot: {gap:?}"
        );
        assert!(queue_deadlines(&f.runtime).is_empty());
    }

    #[tokio::test]
    async fn r1_earlier_tail_deadline_wakes_sleeping_head_without_normal_fifo_bypass() {
        let f = Fixture::new();
        block(&f.runtime);
        let mut head = Box::pin(f.runtime.admit_until(ID, at(Instant::now(), 5)));
        assert!(poll!(head.as_mut()).is_pending());
        // Let the actual release task sleep on the head's later deadline.
        tokio::task::yield_now().await;
        let deadline = Instant::now() + StdDuration::from_millis(80);
        let mut tail = Box::pin(f.runtime.admit_until(ID, deadline));
        assert!(poll!(tail.as_mut()).is_pending());
        let tail_guard = tokio::time::timeout(StdDuration::from_millis(600), tail)
            .await
            .expect("earlier tail failed to wake the head's release-loop sleep");
        assert!(tail_guard.forced());
        assert!(
            last_release(&f.runtime).is_none(),
            "unsent tail expiry must not consume the head's release slot"
        );
        assert!(
            poll!(head.as_mut()).is_pending(),
            "head bypassed the still-closed gate"
        );
        assert_eq!(queue_deadlines(&f.runtime).len(), 1);
        open_backoff(&f.runtime);
        let head_guard = tokio::time::timeout(StdDuration::from_millis(700), head)
            .await
            .unwrap();
        assert!(!head_guard.forced());
        assert!(last_release(&f.runtime).unwrap() >= deadline);
        assert!(queue_deadlines(&f.runtime).is_empty());
    }

    #[tokio::test]
    async fn r1_cancelled_earlier_tail_neither_forces_nor_updates_release_clock() {
        let f = Fixture::new();
        block(&f.runtime);
        let mut head = Box::pin(f.runtime.admit_until(ID, at(Instant::now(), 5)));
        assert!(poll!(head.as_mut()).is_pending());
        let mut cancelled = Box::pin(
            f.runtime
                .admit_until(ID, Instant::now() + StdDuration::from_millis(30)),
        );
        assert!(poll!(cancelled.as_mut()).is_pending());
        drop(cancelled);
        tokio::time::sleep(StdDuration::from_millis(60)).await;
        assert!(
            last_release(&f.runtime).is_none(),
            "cancelled deadline created a phantom forced send"
        );
        assert!(poll!(head.as_mut()).is_pending());
        open_backoff(&f.runtime);
        let head_guard = tokio::time::timeout(StdDuration::from_millis(200), head)
            .await
            .unwrap();
        assert!(!head_guard.forced());
        assert!(queue_deadlines(&f.runtime).is_empty());
    }

    #[tokio::test]
    async fn r1_unpaced_admit_until_never_queues_or_starts_spacing() {
        let f = Fixture::new();
        let deadline = at(Instant::now(), 5);
        let mut guards = Vec::new();
        for _ in 0..32 {
            let mut future = Box::pin(f.runtime.admit_until(ID, deadline));
            let Poll::Ready(guard) = poll!(future.as_mut()) else {
                panic!("unpaced account was queued or concurrency-limited");
            };
            assert!(!guard.forced());
            guards.push(guard); // Keep all admissions live concurrently.
        }
        assert!(queue_deadlines(&f.runtime).is_empty());
        assert!(last_release(&f.runtime).is_none());
        assert!(!f.runtime.pacing.lock().unwrap()[ID].release_task);
        assert_eq!(guards.len(), 32);
    }
}

fn apply_snapshot_notices(
    obs: &Observability,
    notices: &Mutex<QuotaNotices>,
    id: &str,
    account: &Account,
    snapshot: &Value,
    origin_model: Option<&str>,
) {
    if crate::credits::serving_ended(snapshot) {
        dashboard_credit_observation(obs, id, false, snapshot);
    }
    if let Ok(mut notices) = notices.lock() {
        let recovered = notices.get(id).is_some_and(|entry| {
            let notice = &entry.notice;
            let time = |value: &Value| value.as_str().and_then(|v| v.parse::<DateTime<Utc>>().ok());
            let old_reset = time(&notice["reset_at"]);
            let observed = time(&notice["observed_at"]);
            crate::credits::serving_ended(snapshot) && notice["action"] == "credits"
                || snapshot["windows"].as_array().is_some_and(|windows| {
                    !windows.is_empty()
                        && windows.iter().all(|w| {
                            w["used_pct"].as_f64().is_some_and(|p| p < 99.0)
                                && time(&w["observed_at"])
                                    .zip(observed)
                                    .is_some_and(|(new, old)| new > old && new <= Utc::now())
                        })
                })
                || snapshot["windows"].as_array().is_some_and(|windows| {
                    windows.iter().any(|w| {
                        w["name"] == notice["window"]
                            && w["used_pct"].as_f64().is_some_and(|p| p < 100.0)
                            && time(&w["observed_at"])
                                .zip(observed)
                                .is_some_and(|(new, old)| new > old && new <= Utc::now())
                            && time(&w["reset_at"])
                                .zip(old_reset)
                                .is_some_and(|(new, old)| {
                                    (new - old).abs() > Duration::seconds(300)
                                })
                    })
                })
        });
        if recovered {
            notices.remove(id);
        }
        // A merged old window on a plan/credit-only update is not new quota evidence.
        let fresh_usage = snapshot["windows"].as_array().is_some_and(|windows| {
            windows.iter().any(|w| {
                w["observed_at"].as_str().is_some()
                    && w["observed_at"] == snapshot["observed_at"]
                    && w["observed_at"]
                        .as_str()
                        .and_then(|v| v.parse::<DateTime<Utc>>().ok())
                        .is_some_and(|time| {
                            time <= Utc::now()
                                && Utc::now().signed_duration_since(time).num_seconds() <= 300
                        })
            })
        });
        if !notices.contains_key(id)
            && notices.len() < 4096
            && fresh_usage
            && account.user_state == UserState::Active
            && !account.drain
            && crate::relay::notices::quota_applies(account, origin_model)
        {
            notices.insert(
                id.to_owned(),
                QuotaNoticeEntry {
                    provider: account.provider,
                    notice: Value::Null,
                    state: notice_state(account),
                    model: origin_model.map(str::to_owned),
                },
            );
        }
    }
}

fn publish_quota_notice(
    notices: &Mutex<QuotaNotices>,
    events: &broadcast::Sender<Value>,
    account: &Account,
    notice: Value,
    model: Option<&str>,
) {
    if account.user_state != UserState::Active || account.drain {
        return;
    }
    let Ok(mut notices) = notices.lock() else {
        return;
    };
    if notices.len() >= 4096 && !notices.contains_key(&account.id) {
        return;
    }
    if notices.get(&account.id).is_some_and(|entry| {
        entry.model.as_deref() == model
            && [
                "action",
                "window",
                "reset_at",
                "next_account",
                "earliest_account",
                "model_limited",
            ]
            .iter()
            .all(|key| entry.notice[*key] == notice[*key])
    }) {
        return;
    }
    notices.insert(
        account.id.clone(),
        QuotaNoticeEntry {
            provider: account.provider,
            notice: notice.clone(),
            state: notice_state(account),
            model: model.map(str::to_owned),
        },
    );
    drop(notices);
    let mut data = notice;
    data["kind"] = json!("quota_notice");
    data["account"] = json!(account.id);
    let _ = events.send(json!({"event":"core_warning","data":data}));
}

fn dashboard_credit_observation(obs: &Observability, id: &str, serving: bool, snapshot: &Value) {
    if let Ok(account) = id.to_owned().try_into() {
        obs.dashboard()
            .usage(crate::dashboard::history::UsageObservation {
                account,
                observed_at: Utc::now(),
                window: None,
                credits: Some(crate::dashboard::history::CreditsObservation {
                    serving,
                    balance: snapshot["credits"]["balance"]
                        .as_f64()
                        .filter(|n| n.is_finite() && *n >= 0.0),
                }),
            });
    }
}

fn paid_observation_allowed(account: &Account) -> bool {
    account.provider == Provider::Codex
        && account.allow_credits
        && account
            .payment_until
            .is_none_or(|until| until <= Utc::now())
        && matches!(
            account.health,
            Health::Ready
                | Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
        )
}

fn notice_state(account: &Account) -> Value {
    json!([
        account.health,
        account.user_state,
        account.drain,
        account.allow_credits,
        account.quota_cooling,
        account.payment_until
    ])
}

/// Same bounded digest used by Completion; raw affinity keys stay private.
pub(crate) fn quota_session(key: Option<&str>) -> Option<String> {
    key.map(|key| {
        Sha256::digest(key.as_bytes())[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    })
}

// Append as an inline unit module to account_runtime.rs after branch integration.
#[cfg(test)]
mod r5_view_tests {
    use super::*;

    #[tokio::test]
    async fn credits_original_http_and_stream_revision_cannot_attach_to_a_later_reset() {
        for stream in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            let runtime = AccountRuntime::new(store.clone(), obs.clone(), 0.0);
            let mut account = crate::accounts::health::tests::fixture();
            account.allow_credits = true;
            let id = account.id.clone();
            store
                .save(
                    &account,
                    &crate::accounts::Tokens {
                        access_token: "synthetic-access".into(),
                        refresh_token: "synthetic-refresh".into(),
                        id_token: None,
                    },
                    None,
                )
                .unwrap();
            let mut paid = HeaderMap::new();
            paid.insert("x-codex-active-limit", "credits".parse().unwrap());
            let paid_event = json!({"type":"codex.rate_limits","active_limit":"credits"});
            let mut delayed = std::pin::pin!(async {
                if stream {
                    runtime.observe_codex_event(&id, &paid_event).await;
                } else {
                    let revision = runtime.observe_headers(&id, Provider::Codex, &paid).await;
                    runtime.credit_headers(&id, &paid, &revision).await;
                }
            });
            {
                let _gate = store.transaction().unwrap();
                let waker = futures_util::task::noop_waker();
                let mut context = std::task::Context::from_waker(&waker);
                assert!(std::future::Future::poll(delayed.as_mut(), &mut context).is_pending());
                let mut reset = HeaderMap::new();
                reset.insert("x-codex-primary-used-percent", "0".parse().unwrap());
                reset.insert(
                    "x-codex-primary-reset-at",
                    (Utc::now() + Duration::hours(5))
                        .timestamp()
                        .to_string()
                        .parse()
                        .unwrap(),
                );
                obs.observe_headers(&id, Provider::Codex, &reset);
            }
            delayed.await;
            assert_eq!(runtime.views().await.unwrap()[0]["credits_in_use"], false);
            assert!(obs.quota_snapshot(&id).unwrap()["paid_serving_observed_at"].is_null());
            assert!(
                obs.dashboard()
                    .usage_page(1)
                    .unwrap()
                    .accounts
                    .iter()
                    .all(|a| a.credits_spans.is_empty())
            );
        }
    }

    #[tokio::test]
    async fn credits_delayed_success_cannot_reopen_a_newer_off_or_empty_span() {
        for off in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            let runtime = AccountRuntime::new(store.clone(), obs.clone(), 0.0);
            let mut account = crate::accounts::health::tests::fixture();
            account.allow_credits = true;
            let id = account.id.clone();
            store
                .save(
                    &account,
                    &crate::accounts::Tokens {
                        access_token: "synthetic-access".into(),
                        refresh_token: "synthetic-refresh".into(),
                        id_token: None,
                    },
                    None,
                )
                .unwrap();
            let mut paid = HeaderMap::new();
            paid.insert("x-codex-active-limit", "credits".parse().unwrap());
            paid.insert("x-codex-credits-has-credits", "true".parse().unwrap());
            obs.observe_headers(&id, Provider::Codex, &paid);
            runtime
                .credit_headers(
                    &id,
                    &paid,
                    &QuotaObservation {
                        observed_at: obs.quota_snapshot(&id).unwrap()["observed_at"].clone(),
                        ..Default::default()
                    },
                )
                .await;
            assert_eq!(runtime.views().await.unwrap()[0]["credits_in_use"], true);
            let revision = QuotaObservation {
                observed_at: obs.quota_snapshot(&id).unwrap()["observed_at"].clone(),
                ..Default::default()
            };
            let mut pending = std::pin::pin!(runtime.credit_headers(&id, &paid, &revision));
            {
                let _gate = store.transaction().unwrap();
                let waker = futures_util::task::noop_waker();
                let mut context = std::task::Context::from_waker(&waker);
                assert!(std::future::Future::poll(pending.as_mut(), &mut context).is_pending());
                if off {
                    store
                        .update_metadata_unlocked(&id, |a| a.allow_credits = false)
                        .unwrap();
                } else {
                    let mut empty = HeaderMap::new();
                    empty.insert("x-codex-credits-balance", "-1".parse().unwrap());
                    obs.observe_headers(&id, Provider::Codex, &empty);
                }
                dashboard_credit_observation(&obs, &id, false, &obs.quota_snapshot(&id).unwrap());
            }
            if !off {
                runtime
                    .apply_quota_snapshot(&id, obs.quota_snapshot(&id).unwrap(), None)
                    .await
                    .unwrap();
            }
            pending.await;
            assert_eq!(runtime.views().await.unwrap()[0]["credits_in_use"], false);
            let history = obs.dashboard().usage_page(1).unwrap();
            assert!(
                history
                    .accounts
                    .iter()
                    .flat_map(|a| &a.credits_spans)
                    .all(|s| s.1.is_some())
            );
            if !off {
                let old_stop = obs.quota_snapshot(&id).unwrap();
                obs.observe_headers(&id, Provider::Codex, &paid);
                runtime
                    .credit_headers(
                        &id,
                        &paid,
                        &QuotaObservation {
                            observed_at: obs.quota_snapshot(&id).unwrap()["observed_at"].clone(),
                            ..Default::default()
                        },
                    )
                    .await;
                assert_eq!(runtime.views().await.unwrap()[0]["credits_in_use"], true);
                runtime
                    .apply_quota_snapshot(&id, old_stop, None)
                    .await
                    .unwrap();
                assert_eq!(runtime.views().await.unwrap()[0]["credits_in_use"], true);
                assert!(
                    obs.dashboard()
                        .usage_page(1)
                        .unwrap()
                        .accounts
                        .iter()
                        .flat_map(|a| &a.credits_spans)
                        .any(|s| s.1.is_none())
                );
            }
        }
    }

    #[tokio::test]
    async fn r5_view_preserves_pool_membership_limiting_window_and_observation_time() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r5-view-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let runtime = AccountRuntime::new(store.clone(), obs.clone(), 1.0);
        let now = Utc::now();
        let reset = now + Duration::hours(2);
        let mut expected = std::collections::BTreeMap::new();
        let mut quota_id = String::new();
        for case in [
            "ready", "drained", "paused", "disabled", "quota", "auth", "relogin",
        ] {
            let mut a = crate::accounts::health::tests::fixture();
            a.identity = crate::accounts::Identity::Codex {
                user_id: Some(format!("synthetic-r5-{case}")),
                account_id: format!("synthetic-r5-workspace-{case}"),
                email: format!("r5-{case}@example.invalid"),
            };
            a.id = a.identity.id();
            a.token_meta.expires_at = Some(now + Duration::days(1));
            a.drain = case == "drained";
            if case == "paused" {
                a.user_state = UserState::Paused;
            }
            if case == "disabled" {
                a.user_state = UserState::Disabled;
            }
            if case == "auth" {
                a.auth_probe_pending = true;
            }
            if case == "relogin" {
                a.health = Health::NeedsRelogin {
                    reason: "synthetic".into(),
                    since: now,
                };
            }
            if case == "quota" {
                a.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                };
                a.quota_cooling = Some(crate::accounts::QuotaCooling {
                    window: "7d".into(),
                    reset_at: reset,
                    started_at: now,
                });
                quota_id = a.id.clone();
            }
            let a = store
                .save(
                    &a,
                    &crate::accounts::Tokens {
                        access_token: format!("synthetic-r5-access-{case}"),
                        refresh_token: format!("synthetic-r5-refresh-{case}"),
                        id_token: None,
                    },
                    None,
                )
                .unwrap();
            expected.insert(a.id.clone(), matches!(case, "ready" | "drained"));
            obs.observe_poll(&a.id, json!({"observed_at":now,"source":"poll","windows":[{"name":"7d","used_pct":if case=="quota" {100.0}else{25.0},"reset_at":reset,"observed_at":now}]}), true).unwrap();
        }
        let views = runtime.views().await.unwrap();
        assert_eq!(views.len(), expected.len());
        for view in &views {
            assert_eq!(
                view["pool_eligible"],
                expected[view["id"].as_str().unwrap()]
            );
            assert_eq!(view["observed_at"], json!(now));
            assert!(view.get("access_token").is_none());
            assert!(view.get("refresh_token").is_none());
        }
        let pools = pool_counts(&views);
        assert_eq!(
            pools["codex"]["eligible"].as_u64(),
            Some(views.iter().filter(|a| a["pool_eligible"] == true).count() as u64)
        );
        let quota = views.iter().find(|a| a["id"] == quota_id).unwrap();
        assert_eq!(quota["quota_cooling"]["window"], "7d");
        assert_eq!(quota["quota_cooling"]["reset_at"], json!(reset));
    }
}
