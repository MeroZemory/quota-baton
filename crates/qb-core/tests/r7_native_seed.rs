#![cfg(all(feature = "testkit", debug_assertions))]
//! Opt-in, persistent synthetic fixture for the coordinator's native harness.
//! Never starts a server, contacts an endpoint, or discovers a profile directory.
use anyhow::{Context, Result, ensure};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use chrono::{DateTime, Duration, Utc};
use qb_core::{
    accounts::{
        Account, AccountStore, Health, HealthReason, Identity, Provider, QuotaCooling, Source,
        TokenMeta, Tokens, UserState,
    },
    bindings::BindingTable,
    config::Config,
    dashboard::{
        AccountId, MoveReason, RequestOutcome, SessionId,
        history::{
            ActivityData, ActivityInput, ActivityKind, CreditsObservation, SettingAction,
            UsageObservation, WindowName, WindowObservation, WindowScope,
        },
    },
    observability::Observability,
};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

const DATA_PORT: u16 = 32170;
const ADMIN_PORT: u16 = 32171;
const UPSTREAM: &str = "http://127.0.0.1:32172";

// Stable slot is the harness input; the manifest contains only the resulting hash.
fn binding_key(provider: Provider, slot: usize) -> String {
    format!("{}:r7-native-seed-{slot:02}", provider.as_str())
}

fn reject_alias(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir(), "seed path must be a directory");
    ensure!(!metadata.file_type().is_symlink(), "seed alias rejected");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "seed reparse point rejected"
        );
    }
    Ok(())
}

fn seed_root(requested: &Path) -> Result<PathBuf> {
    // Require a pre-created EMPTY directory. Never create through an unchecked
    // ancestor or reuse a previous run's home, locks, manifest or token files.
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("implementation project")?;
    let sandbox = project.join(".sandbox");
    ensure!(
        requested.is_absolute(),
        "QB_R7_NATIVE_SEED must be absolute"
    );
    let relative = requested
        .strip_prefix(&sandbox)
        .context("seed must be inside this implementation project's .sandbox")?;
    ensure!(
        (1..=8).contains(&relative.components().count()),
        "seed needs a bounded descendant directory"
    );
    reject_alias(&sandbox)?;
    let mut current = sandbox.clone();
    for part in relative.components() {
        let Component::Normal(name) = part else {
            anyhow::bail!("non-normal seed component rejected");
        };
        let name = name.to_str().context("seed component must be UTF-8")?;
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && !name.contains([':', '\\', '/'])
                && !name.ends_with(['.', ' ']),
            "invalid seed component"
        );
        current.push(name);
        reject_alias(&current)?;
    }
    let root = fs::canonicalize(requested)?;
    let sandbox = fs::canonicalize(sandbox)?;
    ensure!(
        root != sandbox && root.starts_with(&sandbox),
        "seed escaped sandbox"
    );
    ensure!(
        fs::read_dir(&root)?.next().is_none(),
        "seed directory must be empty"
    );
    Ok(root)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn account(slot: usize, now: DateTime<Utc>) -> Account {
    let email = format!("r7-seed-{slot:02}@example.invalid");
    let identity = if slot < 3 {
        Identity::Claude {
            account_uuid: format!("r7-synthetic-claude-{slot:02}"),
            organization_uuid: Some("r7-synthetic-org".into()),
            email: email.clone(),
        }
    } else {
        Identity::Codex {
            account_id: format!("r7-synthetic-codex-{slot:02}"),
            email: email.clone(),
            user_id: None,
        }
    };
    let cooling = (slot == 1).then_some(QuotaCooling {
        window: "5h".into(),
        reset_at: now + Duration::minutes(90),
        started_at: now - Duration::minutes(10),
    });
    Account {
        schema: 1,
        id: identity.id(),
        provider: identity.provider(),
        label: format!("R7 synthetic {:02}", slot + 1),
        email,
        plan: Some(["pro", "max_5x", "max_20x", "plus", "pro", "free"][slot].into()),
        identity,
        source: Source::Oauth,
        user_state: if slot == 2 {
            UserState::Paused
        } else {
            UserState::Active
        },
        health: if let Some(q) = &cooling {
            Health::Cooling {
                reason: HealthReason::Quota,
                until: q.reset_at,
                scope: "account".into(),
            }
        } else {
            Health::Ready
        },
        auth_failures: 0,
        payment_failures: 0,
        payment_until: None,
        auth_probe_pending: false,
        quota_cooling: cooling,
        drain: slot == 4,
        allow_credits: slot == 3,
        quota_credit_releasable: false,
        generation: 0,
        token_meta: TokenMeta {
            expires_at: Some(now + Duration::hours(24)),
            last_refresh_at: Some(now),
            refresh_uncertain: false,
        },
    }
}

fn windows(slot: usize) -> Vec<(&'static str, u32, f64)> {
    match slot {
        0 => vec![("5h", 300, 28.0), ("7d", 10080, 46.0)],
        1 => vec![("5h", 300, 100.0), ("7d", 10080, 84.0)],
        2 => vec![("5h", 300, 12.0), ("7d", 10080, 31.0)],
        3 => vec![("5h", 300, 67.0), ("7d", 10080, 73.0)],
        4 => vec![("60m", 60, 42.0), ("7d", 10080, 57.0)],
        // Free is ONE observed 30-day window, never fabricated 5h/weekly zeros.
        _ => vec![("43200m", 43200, 36.0)],
    }
}

fn reset(now: DateTime<Utc>, minutes: u32) -> DateTime<Utc> {
    now + Duration::minutes(match minutes {
        300 => 90,
        60 => 25,
        10080 => 3 * 24 * 60,
        _ => 20 * 24 * 60,
    })
}

fn header(headers: &mut HeaderMap, name: &str, value: impl ToString) -> Result<()> {
    headers.insert(
        HeaderName::from_bytes(name.as_bytes())?,
        HeaderValue::from_str(&value.to_string())?,
    );
    Ok(())
}

fn seed_usage(
    obs: &Observability,
    account: &Account,
    slot: usize,
    now: DateTime<Utc>,
) -> Result<()> {
    let id = AccountId::try_from(account.id.clone()).map_err(anyhow::Error::msg)?;
    let mut headers = HeaderMap::new();
    for (index, (name, minutes, current)) in windows(slot).into_iter().enumerate() {
        for hour in 0..30 {
            let at = now - Duration::hours(30 - hour);
            let short = minutes <= 300;
            let cycle_hours = i64::from(minutes / 60);
            let cycle_end =
                now - Duration::hours(30) + Duration::hours((hour / cycle_hours + 1) * cycle_hours);
            obs.dashboard().usage(UsageObservation {
                account: id.clone(),
                observed_at: at,
                window: Some(WindowObservation {
                    name: WindowName::try_from(name.to_owned()).map_err(anyhow::Error::msg)?,
                    scope: WindowScope::Account,
                    used_pct: Some(if short {
                        8.0 + (hour % cycle_hours) as f64 * 16.0 + slot as f64
                    } else {
                        (current - (30 - hour) as f64 * 0.7).max(0.0)
                    }),
                    reset_at: Some(if short {
                        cycle_end
                    } else {
                        reset(now, minutes)
                    }),
                }),
                credits: None,
            });
        }
        if account.provider == Provider::Claude {
            let prefix = format!("anthropic-ratelimit-unified-{name}");
            header(
                &mut headers,
                &format!("{prefix}-utilization"),
                current / 100.0,
            )?;
            header(
                &mut headers,
                &format!("{prefix}-reset"),
                reset(now, minutes).timestamp(),
            )?;
        } else {
            let prefix = format!(
                "x-codex-{}",
                if index == 0 { "primary" } else { "secondary" }
            );
            header(&mut headers, &format!("{prefix}-used-percent"), current)?;
            header(&mut headers, &format!("{prefix}-window-minutes"), minutes)?;
            header(
                &mut headers,
                &format!("{prefix}-reset-at"),
                reset(now, minutes).timestamp(),
            )?;
        }
    }
    if account.provider == Provider::Codex {
        header(
            &mut headers,
            "x-codex-plan-type",
            account.plan.as_deref().unwrap(),
        )?;
    }
    if slot == 3 {
        for (minutes_ago, serving, balance) in
            [(120, true, 18.5), (60, true, 15.0), (30, false, 13.25)]
        {
            obs.dashboard().usage(UsageObservation {
                account: id.clone(),
                observed_at: now - Duration::minutes(minutes_ago),
                window: None,
                credits: Some(CreditsObservation {
                    serving,
                    balance: Some(balance),
                }),
            });
        }
        header(&mut headers, "x-codex-credits-has-credits", true)?;
        header(&mut headers, "x-codex-credits-balance", 13.25)?;
        header(&mut headers, "x-codex-credits-serving", false)?;
    }
    // Real normalization and quota persistence; no hand-written quota schema.
    obs.observe_headers(&account.id, account.provider, &headers);
    ensure!(
        obs.quota_snapshot(&account.id).is_some(),
        "quota observation rejected"
    );
    Ok(())
}

fn seed_activity(obs: &Observability, accounts: &[Account], now: DateTime<Utc>) -> Result<()> {
    for (slot, account) in accounts.iter().enumerate() {
        let id = AccountId::try_from(account.id.clone()).map_err(anyhow::Error::msg)?;
        for (kind, ago, data) in [
            (
                ActivityKind::Login,
                Duration::hours(28),
                ActivityData::default(),
            ),
            (
                ActivityKind::TokenRefresh,
                Duration::hours(2),
                ActivityData {
                    outcome: Some(RequestOutcome::Ok),
                    expires_at: account.token_meta.expires_at,
                    ..Default::default()
                },
            ),
        ] {
            obs.dashboard().activity(ActivityInput {
                ts: now - ago,
                kind,
                provider: account.provider,
                account: Some(id.clone()),
                account_to: None,
                notable: false,
                data,
            });
        }
        if slot == 1 {
            let at = now - Duration::minutes(10);
            obs.dashboard().activity(ActivityInput {
                ts: at,
                kind: ActivityKind::QuotaReached,
                provider: account.provider,
                account: Some(id.clone()),
                account_to: None,
                notable: true,
                data: ActivityData {
                    window: Some(
                        WindowName::try_from("5h".to_owned()).map_err(anyhow::Error::msg)?,
                    ),
                    reset_at: Some(reset(now, 300)),
                    ..Default::default()
                },
            });
            obs.dashboard().activity(ActivityInput {
                ts: at + Duration::seconds(1),
                kind: ActivityKind::Move,
                provider: account.provider,
                account: Some(id.clone()),
                account_to: Some(
                    AccountId::try_from(accounts[0].id.clone()).map_err(anyhow::Error::msg)?,
                ),
                notable: true,
                data: ActivityData {
                    session: Some(SessionId::from_key(&binding_key(account.provider, slot))),
                    reason: Some(MoveReason::Forced),
                    ..Default::default()
                },
            });
        }
        let action = match slot {
            2 => Some(SettingAction::Pause),
            4 => Some(SettingAction::DrainOn),
            _ => None,
        };
        if let Some(action) = action {
            obs.dashboard().activity(ActivityInput {
                ts: now - Duration::minutes(15),
                kind: ActivityKind::AccountChanged,
                provider: account.provider,
                account: Some(id),
                account_to: None,
                notable: true,
                data: ActivityData {
                    action: Some(action),
                    ..Default::default()
                },
            });
        }
    }
    Ok(())
}

#[test]
fn r7_native_seed() -> Result<()> {
    let Some(requested) = std::env::var_os("QB_R7_NATIVE_SEED") else {
        // No filesystem access, home discovery, observer, store or thread absent opt-in.
        return Ok(());
    };
    let root = seed_root(Path::new(&requested))?;
    let home = root.join("home");
    let now =
        DateTime::from_timestamp_millis(Utc::now().timestamp_millis()).context("seed clock")?;
    let mut config = Config::default();
    config.server.data_port = DATA_PORT;
    config.server.admin_port = ADMIN_PORT;
    config.dashboard.resolve_claude_project = false;
    config.quota.poll_min_interval_secs = 3600;
    config.upstream.anthropic_base = UPSTREAM.into();
    config.upstream.codex_base = UPSTREAM.into();
    config.upstream.claude_usage_base = UPSTREAM.into();
    config.upstream.codex_usage_base = UPSTREAM.into();
    config.upstream.claude_token_endpoint = Some(format!("{UPSTREAM}/synthetic/claude/token"));
    config.upstream.codex_token_endpoint = Some(format!("{UPSTREAM}/synthetic/codex/token"));
    config.upstream.validate()?;
    fs::create_dir(&home)?;
    write_new(
        &home.join("config.toml"),
        toml::to_string_pretty(&config)?.as_bytes(),
    )?;
    let store = AccountStore::acquire(&home)?;
    let mut accounts = Vec::new();
    for slot in 0..6 {
        let account = account(slot, now);
        let tokens = Tokens {
            access_token: format!("r7-SYNTHETIC-NOT-A-CREDENTIAL-access-{slot:02}"),
            refresh_token: format!("r7-SYNTHETIC-NOT-A-CREDENTIAL-refresh-{slot:02}"),
            id_token: None,
        };
        accounts.push(store.save(&account, &tokens, None).with_context(|| {
            format!(
                "save synthetic seed account slot {slot} provider {} id {}",
                account.provider.as_str(),
                account.id
            )
        })?);
    }
    let obs = Observability::new(&home, None)?;
    for (slot, account) in accounts.iter().enumerate() {
        seed_usage(&obs, account, slot, now)?;
    }
    seed_activity(&obs, &accounts, now)?;
    let bindings = BindingTable::new(&home, config.routing.clone())?;
    let mut sessions = Vec::new();
    let mut rows = Vec::new();
    for (slot, account) in accounts.iter().enumerate() {
        let key = binding_key(account.provider, slot);
        rows.push(
            json!({"key":key,"account":account.id,"last_used":now-Duration::minutes(6+slot as i64),
            "first_seen_at":now-Duration::hours(3),"bound_since":now-Duration::hours(2),
            "client_kind":if account.provider==Provider::Claude {"claude_cli"} else {"codex_cli"}}),
        );
        sessions.push(
            json!({"slot":slot,"provider":account.provider,"account_id":account.id,
            "session_hash":SessionId::from_key(&key),"idle":true}),
        );
    }
    bindings.import(json!({"schema":1,"bindings":rows}))?;
    bindings.freeze()?;
    obs.freeze_quota()?;
    ensure!(
        obs.dashboard()
            .activity_page(None, 2000, 7)
            .map_err(|e| anyhow::anyhow!("seed activity query: {e:?}"))?
            .events
            .len()
            >= 16,
        "activity missing"
    );
    for relative in [
        "state/bindings.json",
        "state/quota.json",
        "state/activity.jsonl",
        "state/usage-history.json",
    ] {
        ensure!(
            home.join(relative).is_file(),
            "seed archive missing: {relative}"
        );
    }
    // Manifest is written LAST: its existence means all fixture writers flushed.
    let manifest: Value = json!({
        "schema":1,"synthetic":true,"home":"home","generated_at":now,
        "expires_at":now+Duration::hours(24),
        "endpoints":{"data":format!("http://127.0.0.1:{DATA_PORT}"),
            "admin":format!("http://127.0.0.1:{ADMIN_PORT}"),"upstream":UPSTREAM,
            "routes":{"state":"/admin/v1/state","sessions":"/admin/v1/sessions?range=hour",
                "activity":"/admin/v1/activity?days=7","usage":"/admin/v1/usage-history?hours=48"}},
        "accounts":accounts.iter().enumerate().map(|(slot,a)| json!({"slot":slot,"id":a.id,
            "provider":a.provider,"plan":a.plan,"user_state":a.user_state,"drain":a.drain,
            "allow_credits":a.allow_credits,"windows":windows(slot).iter().map(|(name,_,_)|*name).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "sessions":sessions,
        "live_counters":"unknown_until_harness_traffic",
        "label_resolution":false
    });
    write_new(
        &root.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}
