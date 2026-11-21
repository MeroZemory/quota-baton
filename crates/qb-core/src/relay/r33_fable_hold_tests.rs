//! R33 replay of the 2026-11-20 16:43:49Z Claude Fable rejection (account
//! cl-b8da25, max_5x) through the real header observation, 429 classifier,
//! account store and ordinary usage poll. Synthetic identity and tokens only.
//!
//! The real header set was: 5h `allowed_warning`, 7d `allowed_warning` (44%),
//! 7d_oi 100% `rejected` resetting at the unified reset, unified status
//! `rejected`, representative claim `five_hour`, retry-after 242170 s, and a
//! first 5h-derived account hold ending 1h36m11s after the request. Absolute
//! epoch headers cannot be replayed against the wall clock, so every deadline
//! keeps its real offset from the request time.
//!
//! The 2026-11-21 poll body had `seven_day_opus: null` and the Fable weekly
//! window only inside `limits[]` (kind `weekly_scoped`, display name `Fable`,
//! 1%). The other `limits[]` rows were `session` and `weekly_all`.
use super::outcome::{self, Kind};
use crate::{
    account_runtime::AccountRuntime,
    accounts::{Account, AccountStore, Health, HealthReason, Identity, Provider, Tokens},
    observability::Observability,
};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde_json::{Value, json};
use std::sync::Arc;

const FABLE: &str = "claude-fable-5-2";
const HAIKU: &str = "claude-haiku-4-5";
const SONNET: &str = "claude-sonnet-5-5";

struct Fixture {
    runtime: Arc<AccountRuntime>,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    id: String,
    _home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r33/fable-hold-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let home = qb_testkit::fixture::home_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let runtime = AccountRuntime::new(store.clone(), obs.clone(), 0.0);
        let identity = Identity::Claude {
            account_uuid: "synthetic-r33-fable".into(),
            organization_uuid: None,
            email: "r33@example.invalid".into(),
        };
        let account: Account = serde_json::from_value(json!({
            "schema":1,"id":identity.id(),"provider":"claude","label":"Synthetic",
            "email":"r33@example.invalid","identity":identity,"plan":"max_5x",
            "source":{"kind":"oauth"},"user_state":"active","health":{"kind":"ready"},
            "generation":0,
            "token_meta":{"expires_at":null,"last_refresh_at":null,"refresh_uncertain":false}
        }))
        .unwrap();
        let saved = store
            .save(
                &account,
                &Tokens {
                    access_token: "synthetic-access".into(),
                    refresh_token: "synthetic-refresh".into(),
                    id_token: None,
                },
                None,
            )
            .unwrap();
        Self {
            runtime,
            store,
            obs,
            id: saved.id,
            _home: home,
        }
    }

    fn account(&self) -> Account {
        self.store.load(&self.id).unwrap()
    }
}

struct Rejection {
    headers: HeaderMap,
    weekly_reset: DateTime<Utc>,
    short_reset: DateTime<Utc>,
}

/// The 16:43:49Z header set with every deadline at its real offset from `at`.
fn real_fable_rejection(at: DateTime<Utc>) -> Rejection {
    let whole = |seconds: i64| {
        DateTime::from_timestamp((at + Duration::seconds(seconds)).timestamp(), 0).unwrap()
    };
    let short_reset = whole(5771); // 18:20:00Z, the first 5h-derived hold deadline
    let weekly_reset = whole(242_170); // 2026-11-23T12:00:00Z, retry-after 242170 s
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
        (
            "anthropic-ratelimit-unified-reset",
            weekly_reset.timestamp().to_string(),
        ),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "five_hour".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-5h-status",
            "allowed_warning".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-5h-utilization",
            "0.52".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-5h-reset",
            short_reset.timestamp().to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-status",
            "allowed_warning".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-7d-utilization",
            "0.44".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-7d-reset",
            weekly_reset.timestamp().to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d_oi-status",
            "rejected".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-7d_oi-utilization",
            "1.0".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-7d_oi-reset",
            weekly_reset.timestamp().to_string(),
        ),
        ("retry-after", "242170".to_owned()),
    ] {
        headers.insert(name, value.parse().unwrap());
    }
    Rejection {
        headers,
        weekly_reset,
        short_reset,
    }
}

const BODY: &[u8] = br#"{"type":"error","error":{"type":"rate_limit_error"}}"#;

/// The relay order for a Claude 429: header observation, classification, then
/// the signal applied through the account owner.
async fn reject(f: &Fixture, rejection: &Rejection, model: &str) -> outcome::Decision {
    f.runtime
        .observe_headers_for_model(&f.id, Provider::Claude, &rejection.headers, Some(model))
        .await;
    let mut decision = outcome::classify(
        Provider::Claude,
        StatusCode::TOO_MANY_REQUESTS,
        &rejection.headers,
        BODY,
        Some(model),
    );
    assert_eq!(decision.kind, Kind::Quota);
    let body = AccountRuntime::qualify_http_429_body(None, Some(model));
    f.runtime
        .finish_http_429_report_attributed(
            &f.id,
            None,
            decision.quota_reset,
            decision.signal.take(),
            f.obs.quota_snapshot(&f.id),
            body,
        )
        .await
        .unwrap();
    decision
}

fn scoped(until: DateTime<Utc>) -> Health {
    Health::Cooling {
        reason: HealthReason::Quota,
        until,
        scope: format!("model:{FABLE}"),
    }
}

/// `/api/oauth/usage` body shape of 2026-11-21: `seven_day_opus` is null and
/// the Fable window is only a `limits[]` row. `percent` is the Fable usage.
fn usage_body(at: DateTime<Utc>, weekly_reset: DateTime<Utc>, fable_percent: f64) -> Vec<u8> {
    let kst = FixedOffset::east_opt(9 * 3600).unwrap();
    let local = |time: DateTime<Utc>| time.with_timezone(&kst).to_rfc3339();
    // The live scoped reset trailed the stored header reset by 0.42 s.
    let scoped_reset = weekly_reset - Duration::microseconds(419_352);
    serde_json::to_vec(&json!({
        "five_hour": {"utilization": 7.0, "resets_at": local(at + Duration::hours(4))},
        "seven_day": {"utilization": 45.0, "resets_at": local(weekly_reset)},
        "seven_day_opus": null,
        "seven_day_sonnet": null,
        "limits": [
            {"kind":"session","group":"session","percent":7,"severity":"normal",
             "resets_at":local(at + Duration::hours(4)),"scope":null,"is_active":false},
            {"kind":"weekly_all","group":"weekly","percent":45,"severity":"normal",
             "resets_at":local(weekly_reset),"scope":null,"is_active":true},
            {"kind":"weekly_scoped","group":"weekly","percent":fable_percent,
             "severity":"normal","resets_at":local(scoped_reset),
             "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null},
             "is_active":false}
        ]
    }))
    .unwrap()
}

/// The ordinary Claude poll path of `QuotaPoller::poll`, without the HTTP hop.
async fn poll(f: &Fixture, body: &[u8], started_at: DateTime<Utc>) -> (Account, Value) {
    let snapshot = crate::quota::parse_snapshot_for_test(Provider::Claude, body, started_at)
        .expect("usage body is recognized");
    let complete = crate::quota::is_complete_snapshot_for_test(Provider::Claude, body);
    assert!(complete, "the live poll reports both account windows");
    let merged = f
        .obs
        .observe_claude_poll(&f.id, snapshot.clone(), complete, started_at)
        .expect("poll is newer than the header observation");
    let account = f
        .runtime
        .apply_polled_quota_snapshot(&f.id, merged.clone(), &snapshot, None, None, None)
        .await
        .unwrap();
    (account, merged)
}

#[tokio::test]
async fn r33_real_fable_rejection_holds_only_fable_with_a_cause_and_the_poll_releases_it() {
    let f = Fixture::new();
    let at = Utc::now();
    let rejection = real_fable_rejection(at);
    let decision = reject(&f, &rejection, FABLE).await;
    assert_eq!(decision.quota_reset, Some(rejection.weekly_reset));
    let held = f.account();
    assert_eq!(
        held.health,
        scoped(rejection.weekly_reset),
        "allowed_warning 5h/7d must not widen the Fable rejection to the account"
    );
    let cause = held
        .quota_cooling
        .clone()
        .expect("the hold records its cause window");
    assert_eq!(cause.window, "7d_oi");
    assert_eq!(cause.reset_at, rejection.weekly_reset);
    let now = Utc::now();
    assert!(held.eligible_at(Some(HAIKU), now));
    assert!(held.eligible_at(Some(SONNET), now));
    assert!(!held.eligible_at(Some(FABLE), now));
    assert!(rejection.short_reset < rejection.weekly_reset);

    // A later ordinary poll sees the Fable window at 1% in limits[] only.
    let started_at = Utc::now() + Duration::seconds(2);
    let body = usage_body(started_at, rejection.weekly_reset, 1.0);
    let (released, merged) = poll(&f, &body, started_at).await;
    let model = merged["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["name"] == "7d_oi")
        .expect("the Fable window reaches the snapshot as 7d_oi");
    assert_eq!(model["used_pct"].as_f64(), Some(1.0));
    assert!(model["status"].is_null(), "poll windows carry no status");
    assert!(
        merged["signals"]["unified_status"].is_null(),
        "the older request-header rejection is not carried past a newer complete poll"
    );
    assert_eq!(released.health, Health::Ready);
    assert_eq!(released.quota_cooling, None);
    let now = started_at + Duration::seconds(1);
    for model in [HAIKU, SONNET, FABLE] {
        assert!(released.eligible_at(Some(model), now), "{model}");
    }
}

#[tokio::test]
async fn r33_a_fable_poll_still_at_the_limit_keeps_the_fable_hold() {
    let f = Fixture::new();
    let rejection = real_fable_rejection(Utc::now());
    reject(&f, &rejection, FABLE).await;
    for percent in [100.0, 97.0] {
        let started_at = Utc::now() + Duration::seconds(2);
        let body = usage_body(started_at, rejection.weekly_reset, percent);
        let (held, _) = poll(&f, &body, started_at).await;
        assert_eq!(held.health, scoped(rejection.weekly_reset), "{percent}");
        let now = started_at + Duration::seconds(1);
        assert!(held.eligible_at(Some(HAIKU), now), "{percent}");
        assert!(!held.eligible_at(Some(FABLE), now), "{percent}");
    }
}

fn unified(pairs: &[(&str, String)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            axum::http::HeaderName::from_bytes(
                format!("anthropic-ratelimit-unified-{name}").as_bytes(),
            )
            .unwrap(),
            value.parse().unwrap(),
        );
    }
    headers
}

/// The relay order for a Claude 429 that carries only the given headers: header
/// observation, classification, then the signal applied through the owner with
/// the observed snapshot as the cause evidence.
async fn hold_from_headers(
    f: &Fixture,
    headers: &HeaderMap,
    model: &str,
) -> (outcome::Decision, Account) {
    f.runtime
        .observe_headers_for_model(&f.id, Provider::Claude, headers, Some(model))
        .await;
    let mut decision = outcome::classify(
        Provider::Claude,
        StatusCode::TOO_MANY_REQUESTS,
        headers,
        BODY,
        Some(model),
    );
    assert_eq!(decision.kind, Kind::Quota);
    let body = AccountRuntime::qualify_http_429_body(None, Some(model));
    f.runtime
        .finish_http_429_report_attributed(
            &f.id,
            None,
            decision.quota_reset,
            decision.signal.take(),
            f.obs.quota_snapshot(&f.id),
            body,
        )
        .await
        .unwrap();
    (decision, f.account())
}

fn whole(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp((Utc::now() + Duration::seconds(seconds)).timestamp(), 0).unwrap()
}

#[tokio::test]
async fn r33_a_request_level_claude_429_records_the_window_that_set_its_deadline() {
    // Producer under test: outcome::classify_inner's cooling(Quota) signal, the
    // 429 hold producer that recorded no cause window before R33.
    let weekly = whole(200_000);
    let short = whole(5_000);
    struct Case {
        name: &'static str,
        headers: HeaderMap,
        scope: String,
        until: DateTime<Utc>,
        cause: Option<(&'static str, DateTime<Utc>)>,
    }
    let account = "account".to_owned();
    let cases = [
        Case {
            name: "model-only 7d_oi",
            headers: unified(&[
                ("7d_oi-status", "rejected".into()),
                ("7d_oi-reset", weekly.timestamp().to_string()),
                ("representative-claim", "seven_day_opus".into()),
            ]),
            scope: format!("model:{FABLE}"),
            until: weekly,
            cause: Some(("7d_oi", weekly)),
        },
        Case {
            name: "model-only claim without a 7d_oi status uses the 7d_oi reset header",
            headers: unified(&[
                ("status", "rejected".into()),
                ("7d_oi-reset", weekly.timestamp().to_string()),
                ("representative-claim", "seven_day_opus".into()),
            ]),
            scope: format!("model:{FABLE}"),
            until: weekly,
            cause: Some(("7d_oi", weekly)),
        },
        Case {
            name: "5h only",
            headers: unified(&[
                ("5h-status", "rejected".into()),
                ("5h-reset", short.timestamp().to_string()),
            ]),
            scope: account.clone(),
            until: short,
            cause: Some(("5h", short)),
        },
        Case {
            name: "7d and 5h: the later reset is the deadline",
            headers: unified(&[
                ("5h-status", "rejected".into()),
                ("5h-reset", short.timestamp().to_string()),
                ("7d-status", "rejected".into()),
                ("7d-reset", weekly.timestamp().to_string()),
            ]),
            scope: account.clone(),
            until: weekly,
            cause: Some(("7d", weekly)),
        },
        Case {
            name: "unified reset fallback names no window",
            headers: unified(&[
                ("status", "rejected".into()),
                ("reset", weekly.timestamp().to_string()),
            ]),
            scope: account.clone(),
            until: weekly,
            cause: None,
        },
    ];
    for case in cases {
        let f = Fixture::new();
        let (_, held) = hold_from_headers(&f, &case.headers, FABLE).await;
        assert_eq!(
            held.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                until: case.until,
                scope: case.scope,
            },
            "{}",
            case.name
        );
        assert_eq!(
            held.quota_cooling.map(|c| (c.window, c.reset_at)),
            case.cause.map(|(window, reset)| (window.to_owned(), reset)),
            "{}",
            case.name
        );
    }
    // retry-after is a duration, not a window reset: no cause.
    let f = Fixture::new();
    let headers = unified(&[("status", "rejected".into())]);
    let mut headers = headers;
    headers.insert("retry-after", "600".parse().unwrap());
    let (_, held) = hold_from_headers(&f, &headers, FABLE).await;
    assert_eq!(held.quota_cooling, None);
}

#[tokio::test]
async fn r33_the_recorded_request_cause_lets_the_ordinary_poll_release_a_model_hold() {
    let f = Fixture::new();
    let weekly = whole(200_000);
    let headers = unified(&[
        ("7d_oi-status", "rejected".into()),
        ("7d_oi-reset", weekly.timestamp().to_string()),
        ("representative-claim", "seven_day_opus".into()),
    ]);
    let (_, held) = hold_from_headers(&f, &headers, FABLE).await;
    assert_eq!(held.quota_cooling.as_ref().unwrap().window, "7d_oi");
    let started_at = Utc::now() + Duration::seconds(2);
    let (released, _) = poll(&f, &usage_body(started_at, weekly, 1.0), started_at).await;
    assert_eq!(released.health, Health::Ready);
    assert_eq!(released.quota_cooling, None);
}
