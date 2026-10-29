//! Synthetic native permit proposals and direct Breaker commits only.
//! This fixture does not perform binding mutations or session moves, and is not
//! a reconstruction of an incident. Expectations are a manual schedule.

use super::{
    Account, BindingTable, Candidate, Data, Health, HealthReason, MoveObservation,
    MoveObservations, Provider, RoutingConfig, UserState, move_guard,
};
use crate::{
    accounts::{Identity, Source, TokenMeta},
    observability::ReplayRuntime,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Duration as Delta, Utc};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Schedule metadata, not a claim obtained by reading or replaying the tape.
#[derive(Debug, serde::Serialize)]
pub struct CaptureSummary {
    pub epoch_path: PathBuf,
    pub entries: usize,
    pub span_seconds: u64,
    pub scope: &'static str,
    pub qualifier: &'static str,
}

/// Close the actual writer even if attachment fails or an assertion unwinds.
struct Shutdown(Arc<ReplayRuntime>);

impl Drop for Shutdown {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

fn account(id: &str, provider: Provider) -> Account {
    let email = "synthetic@example.invalid";
    let identity = match provider {
        Provider::Claude => Identity::Claude {
            account_uuid: id.into(),
            organization_uuid: None,
            email: email.into(),
        },
        Provider::Codex => Identity::Codex {
            account_id: id.into(),
            email: email.into(),
            user_id: None,
        },
    };
    Account {
        schema: 1,
        id: id.into(),
        provider,
        label: id.into(),
        email: email.into(),
        plan: Some("pro".into()),
        plan_observed_at: None,
        subscription: None,
        identity,
        source: Source::Oauth,
        user_state: UserState::Active,
        health: Health::Ready,
        auth_failures: 0,
        payment_failures: 0,
        payment_until: None,
        auth_probe_pending: false,
        quota_cooling: None,
        quota_credit_releasable: false,
        drain: false,
        allow_credits: false,
        generation: 0,
        token_meta: TokenMeta::default(),
    }
}

fn candidate(id: &str, provider: Provider) -> Candidate {
    Candidate {
        account: account(id, provider),
        quota: None,
        inflight: 0,
    }
}

fn candidates() -> [Candidate; 5] {
    [
        candidate("hour-a", Provider::Codex),
        candidate("hour-b", Provider::Codex),
        candidate("free-before", Provider::Claude),
        candidate("free-lapsed", Provider::Claude),
        candidate("free-destination", Provider::Claude),
    ]
}

/// Run 63 real permit entries over one synthetic hour; return four approvals.
/// Direct Breaker commits validate permit proposals, not actual session moves.
pub(super) fn scripted_hour(table: &BindingTable) -> usize {
    let start: DateTime<Utc> = "2026-10-22T03:01:20Z".parse().unwrap();
    let d = Data::default();
    let [a, b, fa, mut fb, fc] = candidates();
    let mut main_source = &a;
    let mut allowed = 0;
    let mut entries = 0;
    let mut denied = 0;
    let mut not_required = 0;
    for minute in 0..=60 {
        let now = start + Delta::minutes(minute);
        let to = if minute == 0 {
            &b
        } else if minute == 12 || minute == 60 {
            &a
        } else {
            main_source
        };
        let observations = MoveObservations::same(MoveObservation::before_collection());
        let result = table.prepare_move_with_source(
            &d,
            "codex:synthetic-hour",
            Some(&main_source.account.id),
            &to.account.id,
            Some(main_source),
            &[a.clone(), b.clone()],
            None,
            observations,
            None,
            "forced",
            now,
        );
        entries += 1;
        match minute {
            0 | 60 => {
                let permit = result.unwrap().expect("manual schedule requires a move");
                table
                    .move_guard
                    .lock()
                    .unwrap()
                    .commit(permit, &to.account.id)
                    .unwrap();
                main_source = to;
                allowed += 1;
            }
            12 => {
                let refusal = result.unwrap_err();
                assert_eq!(refusal.reason, move_guard::Refusal::PingPong);
                assert_eq!(refusal.until, start + Delta::minutes(30));
                denied += 1;
            }
            _ => {
                assert!(
                    result.unwrap().is_none(),
                    "unchanged source requires no permit"
                );
                not_required += 1;
            }
        }
        if minute == 40 || minute == 45 {
            if minute == 45 {
                fb.account.plan = Some("free".into());
                // Expired raw cooling and absent quota must not hide a newly
                // Free Claude failure, five minutes after the prior approval.
                fb.account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: now - Delta::seconds(1),
                    scope: "account".into(),
                };
            }
            let (source, target) = if minute == 40 { (&fa, &fb) } else { (&fb, &fc) };
            let observations = MoveObservations::same(MoveObservation::before_collection());
            let result = table.prepare_move_with_source(
                &d,
                "claude:synthetic-free",
                Some(&source.account.id),
                &target.account.id,
                Some(source),
                &[fa.clone(), fb.clone(), fc.clone()],
                None,
                observations,
                None,
                "forced",
                now,
            );
            entries += 1;
            let permit = result
                .unwrap()
                .expect("manual Free-lapse schedule requires a move");
            table
                .move_guard
                .lock()
                .unwrap()
                .commit(permit, &target.account.id)
                .unwrap();
            allowed += 1;
        }
    }
    assert_eq!(entries, 63);
    assert_eq!(allowed, 4);
    assert_eq!(denied, 1);
    assert_eq!(not_required, 58);
    allowed
}

/// Capture the manual schedule using the native writer in a new synthetic home.
/// No account store, credentials, provider calls, or tape reader are involved.
pub fn capture(home: &Path) -> Result<CaptureSummary> {
    // The platform facade exclusively creates one private leaf, rejecting every
    // existing leaf type rather than accepting a raced existing directory.
    crate::platform::create_private_directory_new(home)
        .context("fixture home must be a fresh nonexistent leaf")?;
    crate::platform::prepare_observation_directory(&home.join("state"))
        .context("prepare private synthetic fixture state")?;

    let table = BindingTable::new(home, RoutingConfig::default())?;
    let root = home.join("state/decision-tape-v6");
    let runtime = Shutdown(Arc::new(ReplayRuntime::start_synthetic_tape(&root, 32)?));
    let initial_accounts: Vec<Account> = candidates().into_iter().map(|c| c.account).collect();
    table.attach_tape(runtime.0.clone(), &initial_accounts)?;
    assert_eq!(scripted_hour(&table), 4);
    drop(runtime); // Flush and close the saved writer before dropping the table.
    drop(table);

    // Inspect only this known root, with a fixed bound; never walk the home or
    // use a reader to derive the schedule's expected decisions or entry count.
    let mut epoch_path = None;
    for (index, entry) in fs::read_dir(&root)?.enumerate() {
        ensure!(index < 8, "synthetic tape root exceeds fixture entry bound");
        let entry = entry?;
        if entry.file_name() == "e00" {
            ensure!(
                entry.file_type()?.is_dir(),
                "synthetic epoch must be a directory"
            );
            epoch_path = Some(entry.path());
        }
    }
    Ok(CaptureSummary {
        epoch_path: epoch_path.context("native writer did not persist synthetic epoch e00")?,
        entries: 63,
        span_seconds: 3600,
        scope: "synthetic",
        qualifier: "native permit proposals and direct Breaker commits; no binding or session moves",
    })
}
