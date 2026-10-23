//! Store-owned evidence of an actual unavailable credential or absent record.
//! Candidate filtering, probing and retry sets cannot issue these facts.
use super::super::Account;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub(super) struct SelectionOwner {
    pending: Mutex<BTreeMap<String, Vec<Weak<AtomicBool>>>>,
}

#[derive(Clone, Copy)]
enum Cause {
    Absent,
    Credential { generation: u64 },
}

#[derive(Clone)]
pub(crate) struct StoreUnavailableEvidence {
    owner: Weak<SelectionOwner>,
    source: String,
    cause: Cause,
    active: Arc<AtomicBool>,
}

impl SelectionOwner {
    fn issue(self: &Arc<Self>, source: &str, cause: Cause) -> StoreUnavailableEvidence {
        let active = Arc::new(AtomicBool::new(true));
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // Only outstanding observations are retained, never historical IDs.
        pending.retain(|_, entries| {
            entries.retain(|entry| entry.strong_count() > 0);
            !entries.is_empty()
        });
        pending
            .entry(source.to_owned())
            .or_default()
            .push(Arc::downgrade(&active));
        StoreUnavailableEvidence {
            owner: Arc::downgrade(self),
            source: source.to_owned(),
            cause,
            active,
        }
    }

    pub(super) fn absent(self: &Arc<Self>, id: &str) -> StoreUnavailableEvidence {
        self.issue(id, Cause::Absent)
    }

    pub(super) fn credential(self: &Arc<Self>, account: &Account) -> StoreUnavailableEvidence {
        self.issue(
            &account.id,
            Cause::Credential {
                generation: account.generation,
            },
        )
    }

    pub(super) fn revoke(&self, id: Option<&str>) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let entries = if let Some(id) = id {
            pending.remove(id).into_iter().flatten().collect::<Vec<_>>()
        } else {
            std::mem::take(&mut *pending)
                .into_values()
                .flatten()
                .collect()
        };
        for entry in entries {
            if let Some(active) = entry.upgrade() {
                active.store(false, Ordering::Release);
            }
        }
    }
}

impl StoreUnavailableEvidence {
    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn applies_to(&self, source: &str, account: Option<&Account>) -> bool {
        self.source == source
            && self.owner.upgrade().is_some()
            && self.active.load(Ordering::Acquire)
            && match self.cause {
                Cause::Absent => account.is_none(),
                Cause::Credential { generation } => account.is_some_and(|account| {
                    account.id == source && account.generation == generation
                }),
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{AccountStore, import};

    fn fixture() -> (tempfile::TempDir, AccountStore, std::path::PathBuf, Account) {
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/n6t-store");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let cpa = root.path().join("cpa");
        std::fs::create_dir_all(&cpa).unwrap();
        std::fs::write(
            cpa.join("sample.json"),
            serde_json::to_vec(&serde_json::json!({
                "type":"codex", "email":"sample@example.test", "account_id":"sample-account",
                "access_token":"synthetic-access", "refresh_token":"synthetic-refresh",
                "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-10-13T00:00:01Z"
            }))
            .unwrap(),
        )
        .unwrap();
        let store = AccountStore::acquire(&root.path().join("home")).unwrap();
        import::run(&store, &cpa, &["codex:sample@example.test".into()], false).unwrap();
        let account = store.list().unwrap().remove(0);
        (root, store, cpa, account)
    }

    #[test]
    fn n6t_store_credential_read_binds_generation_and_revokes_before_mutation() {
        let (_root, store, _cpa, account) = fixture();
        let (usable, proof) = store.selection_credentials(&account);
        assert!(usable && proof.is_none());
        let path = store
            .home()
            .join("accounts")
            .join(format!("{}.json", account.id));
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["secret_dpapi_b64"] = "synthetic-invalid-cipher".into();
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let (usable, proof) = store.selection_credentials(&account);
        assert!(!usable);
        let proof = proof.expect("actual failed credential read must issue owner evidence");
        assert!(proof.applies_to(&account.id, Some(&account)));
        assert!(!proof.applies_to("cx-000000", Some(&account)));
        assert!(!proof.applies_to(&account.id, None));
        let mut other_generation = account.clone();
        other_generation.generation += 1;
        assert!(!proof.applies_to(&account.id, Some(&other_generation)));
        store
            .update_metadata(&account.id, |a| a.label = "sample-renamed".into())
            .unwrap();
        assert!(
            !proof.applies_to(&account.id, Some(&account)),
            "a store mutation attempt must revoke the earlier fact"
        );
    }

    #[test]
    fn n6t_store_generation_conflict_and_filtered_presence_do_not_issue_evidence() {
        let (_root, store, _cpa, account) = fixture();
        let mut stale = account.clone();
        stale.generation += 1;
        let (usable, proof) = store.selection_credentials(&stale);
        assert!(
            !usable && proof.is_none(),
            "a metadata race is not credential unavailability"
        );
        assert!(store.selection_absence(&account.id).is_none());
        assert!(store.selection_absence("../sample").is_none());
    }

    #[test]
    fn n6t_store_usage_report_revokes_before_its_specialized_callback() {
        struct Report {
            proof: StoreUnavailableEvidence,
            account: Account,
            called: bool,
        }
        impl crate::accounts::ReportMutation for Report {
            fn before_store_mutation(&mut self, id: &str) {
                self.called = true;
                assert!(
                    !self.proof.applies_to(id, Some(&self.account)),
                    "usage report must revoke selection evidence before its adapter"
                );
            }
        }
        let (_root, store, _cpa, account) = fixture();
        let path = store
            .home()
            .join("accounts")
            .join(format!("{}.json", account.id));
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["secret_dpapi_b64"] = "synthetic-invalid-cipher".into();
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let (_, proof) = store.selection_credentials(&account);
        let mut report = Report {
            proof: proof.unwrap(),
            account: account.clone(),
            called: false,
        };
        assert!(report.proof.applies_to(&account.id, Some(&account)));
        let updated = {
            let _guard = store.transaction().unwrap();
            store
                .update_metadata_with_cause_unlocked(
                    &account.id,
                    super::super::MetadataCause::UsageReport(&mut report),
                    |a| a.label = "sample-report".into(),
                )
                .unwrap()
        };
        assert!(report.called);
        assert!(updated.generation > account.generation);
        assert!(!report.proof.applies_to(&account.id, Some(&account)));
    }

    #[test]
    fn n6t_store_actual_removal_and_replacement_revoke_absence() {
        let (_root, store, cpa, account) = fixture();
        {
            let _guard = store.transaction().unwrap();
            store.remove_login_record_unlocked(&account.id).unwrap();
        }
        let proof = store
            .selection_absence(&account.id)
            .expect("successful absent lookup");
        assert!(proof.applies_to(&account.id, None));
        assert!(!proof.applies_to(&account.id, Some(&account)));
        import::run(&store, &cpa, &["codex:sample@example.test".into()], false).unwrap();
        assert!(
            !proof.applies_to(&account.id, None),
            "replacement must revoke absence before publication"
        );
    }

    #[test]
    fn n6t_store_stop_and_drop_revoke_existing_facts_and_stop_new_admission() {
        let (_root, store, _cpa, account) = fixture();
        let proof = store.selection_absence("cx-000000").unwrap();
        assert!(proof.applies_to("cx-000000", None));
        store.stop_writes();
        assert!(!proof.applies_to("cx-000000", None));
        assert!(store.selection_absence("cx-000000").is_none());
        let (usable, proof) = store.selection_credentials(&account);
        assert!(!usable && proof.is_none());
        drop(store);
        let (_root2, store2, _cpa2, _account2) = fixture();
        let proof = store2.selection_absence("cx-000000").unwrap();
        drop(store2);
        assert!(!proof.applies_to("cx-000000", None));
    }

    #[test]
    fn n6t_store_replacement_vetoes_actual_binding_before_policy_and_budget() {
        use crate::{
            accounts::Provider,
            bindings::{BindingTable, Candidate, Decision, DispatchAdmission, MoveObservations},
        };
        let (_root, store, cpa, account) = fixture();
        let table =
            BindingTable::new(store.home(), crate::config::RoutingConfig::default()).unwrap();
        let source = Candidate {
            account: account.clone(),
            quota: None,
            inflight: 0,
        };
        let mut target = source.clone();
        target.account.id = "sample-target".into();
        let key = "codex:sample-absent";
        match table.choose(
            Provider::Codex,
            Some(key),
            None,
            None,
            std::slice::from_ref(&source),
            None,
            0,
        ) {
            Decision::Selected { lease, .. } => lease.dispatched(),
            other => panic!("initial selection: {other:?}"),
        }
        let before = table.export().unwrap();
        let revision = table.routing_plan(Some(key)).revision;
        for replace in [true, false] {
            {
                let _guard = store.transaction().unwrap();
                store.remove_login_record_unlocked(&account.id).unwrap();
            }
            let proof = store.selection_absence(&account.id).unwrap();
            let candidates = std::slice::from_ref(&target);
            let result = table.choose_with_hold_at_admitted_proof(
                Provider::Codex,
                Some(key),
                None,
                None,
                candidates,
                candidates,
                MoveObservations::untracked(),
                None,
                0,
                false,
                false,
                chrono::Utc::now(),
                None,
                |_| {
                    if replace {
                        import::run(&store, &cpa, &["codex:sample@example.test".into()], false)
                            .unwrap();
                    }
                    Some(DispatchAdmission::ordinary().with_store_evidence(vec![proof]))
                },
            );
            if replace {
                assert!(
                    matches!(result, Decision::None),
                    "revoked absence must precede policy publication"
                );
                assert_eq!(table.export().unwrap(), before);
                assert_eq!(table.routing_plan(Some(key)).revision, revision);
            } else {
                assert!(
                    matches!(result, Decision::Selected { ref account, .. } if account == "sample-target"),
                    "veto must preserve the budget for a real absent source"
                );
            }
        }
    }
}
