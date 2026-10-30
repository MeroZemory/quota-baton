//! A pre-authored synthetic hour through actual chooser entries and native tape.
//! This is neither real traffic nor a reconstruction of the 10-22 incident.
use super::*;
use crate::observability::ReplayRuntime;
use anyhow::{Context, ensure};

#[derive(Debug, Serialize)]
pub struct CaptureSummary {
    pub epoch_path: PathBuf,
    pub chooser_calls: usize,
    pub input_span_seconds: i64,
    pub scope: &'static str,
    pub power_selection_ordinal: u64,
}

struct Shutdown(Arc<ReplayRuntime>);
impl Drop for Shutdown {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

fn selected(decision: Decision, expected: &str) {
    match decision {
        Decision::Selected { account, lease, .. } => {
            assert_eq!(account, expected, "independent schedule account");
            lease.dispatched();
        }
        other => panic!("scripted native choice failed: {other:?}"),
    }
}

/// Exactly one Codex selection is the independently specified raw-fault control.
/// The warm binding is created by an ordinary chooser before tape attachment,
/// flushed, and reopened. No binding or account state is edited.
pub fn capture(home: &Path) -> Result<CaptureSummary> {
    crate::platform::create_private_directory_new(home)
        .context("selection fixture requires a new private leaf")?;
    crate::platform::prepare_observation_directory(&home.join("state"))?;
    let start: DateTime<Utc> = "2026-10-30T00:00:00Z".parse()?;
    let codex = permit_fixture::candidate("cx-synthetic-control", Provider::Codex);
    let claude = permit_fixture::candidate("cl-synthetic-hour", Provider::Claude);
    let config = RoutingConfig::default();
    let table = BindingTable::new(home, config.clone())?;
    selected(
        table.choose_with_hold_at(
            Provider::Codex,
            Some("codex:synthetic-control"),
            None,
            None,
            std::slice::from_ref(&codex),
            None,
            100,
            true,
            false,
            start,
        ),
        &codex.account.id,
    );
    table.freeze()?;
    drop(table);

    let table = BindingTable::new(home, config)?;
    let root = home.join("state/decision-tape-v9");
    let runtime = Shutdown(Arc::new(ReplayRuntime::start_synthetic_tape(&root, 32)?));
    table.attach_tape(
        runtime.0.clone(),
        &[codex.account.clone(), claude.account.clone()],
    )?;
    selected(
        table.choose_with_hold_at(
            Provider::Codex,
            Some("codex:synthetic-control"),
            None,
            None,
            std::slice::from_ref(&codex),
            None,
            100,
            true,
            false,
            start,
        ),
        &codex.account.id,
    );
    for minute in 0..=60 {
        selected(
            table.choose_with_hold_at(
                Provider::Claude,
                Some("claude:synthetic-hour"),
                None,
                None,
                std::slice::from_ref(&claude),
                None,
                100,
                true,
                false,
                start + chrono::Duration::minutes(minute),
            ),
            &claude.account.id,
        );
    }
    // A real empty collection and a refused admission retain their actual outer
    // results; neither is padded with a dispatch or manufactured rebind.
    assert!(matches!(
        table.choose_with_hold_at(
            Provider::Claude,
            Some("claude:empty"),
            None,
            None,
            &[],
            None,
            100,
            true,
            false,
            start + chrono::Duration::hours(1)
        ),
        Decision::None
    ));
    assert!(matches!(
        table.choose_with_hold_at_admitted(
            Provider::Claude,
            Some("claude:blocked"),
            None,
            None,
            std::slice::from_ref(&claude),
            None,
            100,
            true,
            false,
            start + chrono::Duration::hours(1),
            |_| false
        ),
        Decision::None
    ));
    drop(runtime);
    table.freeze()?;
    drop(table);
    let epoch_path = root.join("e00");
    let epoch = crate::observability::decision_reader::load(&epoch_path)?;
    ensure!(
        epoch.selection_complete,
        "synthetic selection archive incomplete"
    );
    ensure!(
        epoch.selections.len() == 64,
        "selection count differs from authored schedule"
    );
    let control = &epoch.selections[0];
    ensure!(
        control.record.refusal.is_none(),
        "control input not comparable"
    );
    // The control's fixed schedule position is checked after native publication;
    // its ordinal is provenance, never a kernel-derived expected answer.
    Ok(CaptureSummary {
        epoch_path,
        chooser_calls: 64,
        input_span_seconds: 3600,
        scope: "synthetic native chooser hour; not real capture",
        power_selection_ordinal: control.record.ordinal,
    })
}
