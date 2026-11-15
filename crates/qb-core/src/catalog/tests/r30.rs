use super::*;

fn ids(accounts: &[&str]) -> Vec<String> {
    accounts.iter().map(|id| (*id).to_owned()).collect()
}

#[test]
fn entitlements_distinguish_unknown_empty_and_retained_catalogs() {
    let f = Fixture::new();
    f.begin("empty").finish(Ok(json!({"data":[]}))).unwrap();
    f.begin("known")
        .finish(Ok(json!({"data":[{"id":"model-a"}]})))
        .unwrap();
    f.begin("known")
        .finish(Err(RefreshFailure::Upstream))
        .unwrap();
    f.begin("failed")
        .finish(Err(RefreshFailure::Transport))
        .unwrap();
    let accounts = ids(&["unknown", "empty", "known", "failed", "known"]);
    assert_eq!(
        f.cache
            .entitled_accounts(&accounts, "model-a", false)
            .unwrap(),
        BTreeSet::from(["known".to_owned()])
    );
    assert_eq!(
        f.cache
            .entitled_accounts(&accounts, "model-a", true)
            .unwrap(),
        ids(&["unknown", "known", "failed"])
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        f.cache
            .entitled_accounts(&accounts, "absent", true)
            .unwrap(),
        ids(&["unknown", "failed"])
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    assert!(
        f.cache
            .entitled_accounts(&accounts, "absent", false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn denials_override_unknown_and_survive_failure_until_valid_success() {
    let f = Fixture::new();
    f.cache.deny_model("a", "model-a").unwrap();
    f.cache.deny_model("a", "model-b").unwrap();
    assert!(f.cache.view("a").unwrap().refresh_due);
    assert!(
        f.cache
            .entitled_accounts(&ids(&["a"]), "model-a", true)
            .unwrap()
            .is_empty()
    );
    assert!(
        !f.cache
            .entitled_accounts(&ids(&["a"]), "model-c", true)
            .unwrap()
            .is_empty()
    );
    let guard = f.begin("a");
    assert!(f.cache.is_model_denied("a", "model-a").unwrap());
    guard.finish(Err(RefreshFailure::Transport)).unwrap();
    drop(f.begin("a"));
    assert!(f.begin("a").finish(Ok(json!({"data":[{}]}))).is_err());
    assert!(f.cache.is_model_denied("a", "model-a").unwrap());
    f.begin("a")
        .finish(Ok(json!({"data":[{"id":"model-a"}]})))
        .unwrap();
    assert!(!f.cache.is_model_denied("a", "model-a").unwrap());
    assert!(!f.cache.is_model_denied("a", "model-b").unwrap());
    assert_eq!(
        f.cache
            .entitled_accounts(&ids(&["a"]), "model-a", false)
            .unwrap(),
        ids(&["a"]).into_iter().collect::<BTreeSet<_>>()
    );
    // A successful empty catalog clears overrides without granting entitlement.
    f.cache.deny_model("a", "model-a").unwrap();
    f.begin("a").finish(Ok(json!({"data":[]}))).unwrap();
    assert!(!f.cache.is_model_denied("a", "model-a").unwrap());
    assert!(
        f.cache
            .entitled_accounts(&ids(&["a"]), "model-a", true)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn union_uses_only_supplied_contributors_and_filters_denials_without_changing_raw() {
    let f = Fixture::new();
    let raw = json!({"data":[{"id":"shared"},{"id":"a-only"}],"extension":{"preserve":true}});
    f.begin("a").finish(Ok(raw.clone())).unwrap();
    f.begin("b")
        .finish(Ok(json!({"data":[{"id":"shared"}]})))
        .unwrap();
    f.begin("disabled")
        .finish(Ok(json!({"data":[{"id":"disabled-only"}]})))
        .unwrap();
    f.cache.deny_model("a", "shared").unwrap();
    let models = f
        .cache
        .union_models(&ids(&["b", "a", "unknown", "b"]))
        .unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["a-only", "shared"]
    );
    assert_eq!(models[0].accounts, ids(&["a"]));
    assert_eq!(models[1].accounts, ids(&["b"]));
    assert_eq!(
        f.cache
            .view("a")
            .unwrap()
            .observation
            .last_known()
            .unwrap()
            .raw(),
        &raw
    );
    assert!(f.cache.union_models(&[]).unwrap().is_empty());
}

#[test]
fn union_freshness_uses_newest_contributor_and_strict_two_interval_boundary() {
    let f = Fixture::new();
    f.begin("a")
        .finish(Ok(json!({"data":[{"id":"shared"},{"id":"old-only"}]})))
        .unwrap();
    let original = f
        .cache
        .view("a")
        .unwrap()
        .observation
        .last_known()
        .unwrap()
        .clone();
    let wallclock = original.observed_at_wallclock();
    assert_eq!(
        wallclock,
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    );
    f.advance(REFRESH_INTERVAL.as_secs() * 2);
    assert!(
        f.cache
            .union_models(&ids(&["a"]))
            .unwrap()
            .iter()
            .all(|model| !model.stale)
    );
    f.advance(1);
    f.begin("a").finish(Err(RefreshFailure::Transport)).unwrap();
    let retained = f
        .cache
        .view("a")
        .unwrap()
        .observation
        .last_known()
        .unwrap()
        .clone();
    assert!(Arc::ptr_eq(&original, &retained));
    assert_eq!(retained.observed_at_wallclock(), wallclock);
    f.begin("b")
        .finish(Ok(json!({"data":[{"id":"shared"}]})))
        .unwrap();
    let models = f.cache.union_models(&ids(&["a", "b"])).unwrap();
    assert_eq!(models[0].id, "old-only");
    assert!(models[0].stale);
    assert_eq!(models[0].observed_at, wallclock);
    assert_eq!(models[1].id, "shared");
    assert!(!models[1].stale);
    assert_eq!(
        models[1].observed_at,
        wallclock + chrono::Duration::seconds(601)
    );
    f.begin("b").finish(Err(RefreshFailure::Upstream)).unwrap();
    assert!(
        !f.cache.union_models(&ids(&["b"])).unwrap()[0].stale,
        "refresh failure is distinct from the admin age policy"
    );
    f.cache.deny_model("b", "shared").unwrap();
    let models = f.cache.union_models(&ids(&["a", "b"])).unwrap();
    assert!(models[1].stale);
    assert_eq!(models[1].accounts, ids(&["a"]));
    assert_eq!(models[1].observed_at, wallclock);
}

#[test]
fn metadata_projects_only_reported_provider_fields_from_newest_contributor() {
    let f = Fixture::new();
    let raw = json!({"models":[{"slug":"codex-model", "display_name":"Codex Model",
        "supported_reasoning_levels":[{"effort":"high","description":"High"},{"effort":"future"}],
        "default_reasoning_level":"future", "extension":{"kept":true}}]});
    f.begin("codex").finish(Ok(raw.clone())).unwrap();
    let models = f.cache.union_models(&ids(&["codex"])).unwrap();
    assert_eq!(models[0].display_name.as_deref(), Some("Codex Model"));
    assert_eq!(models[0].efforts, Some(ids(&["high", "future"])));
    assert_eq!(models[0].default_effort.as_deref(), Some("future"));
    assert_eq!(
        f.cache
            .view("codex")
            .unwrap()
            .observation
            .last_known()
            .unwrap()
            .raw(),
        &raw
    );
    f.begin("claude").finish(Ok(json!({"data":[{"id":"claude-model","display_name":"Claude Model",
        "efforts":["invented"],"default_effort":"invented",
        "supported_reasoning_levels":[{"effort":"invented"}],"default_reasoning_level":"invented"}]}))).unwrap();
    let models = f.cache.union_models(&ids(&["claude"])).unwrap();
    assert_eq!(models[0].display_name.as_deref(), Some("Claude Model"));
    assert_eq!(models[0].efforts, None);
    assert_eq!(models[0].default_effort, None);
    f.advance(1);
    f.begin("newer")
        .finish(Ok(json!({"models":[{"slug":"codex-model"}]})))
        .unwrap();
    let models = f.cache.union_models(&ids(&["codex", "newer"])).unwrap();
    assert_eq!(models[0].display_name, None);
    assert_eq!(models[0].efforts, None);
    assert_eq!(models[0].default_effort, None);
}

#[test]
fn malformed_optional_metadata_does_not_discard_catalog_or_invent_values() {
    let f = Fixture::new();
    f.begin("a")
        .finish(Ok(json!({"models":[{"slug":"a","display_name":4,
        "supported_reasoning_levels":[{"effort":"high"},{}],"default_reasoning_level":false},
        {"slug":"b","supported_reasoning_levels":[]}]})))
        .unwrap();
    let models = f.cache.union_models(&ids(&["a"])).unwrap();
    assert_eq!(models[0].display_name, None);
    assert_eq!(models[0].efforts, None);
    assert_eq!(models[0].default_effort, None);
    assert_eq!(models[1].efforts, Some(Vec::new()));
}

#[test]
fn denial_limits_validation_and_lifecycle_cleanup_are_explicit() {
    let f = Fixture::new();
    assert_eq!(
        f.cache.deny_model("", "m"),
        Err(CatalogError::InvalidAccount)
    );
    assert_eq!(
        f.cache.deny_model("a", "bad model"),
        Err(CatalogError::InvalidModelId)
    );
    assert_eq!(
        f.cache.is_model_denied("a", ""),
        Err(CatalogError::InvalidModelId)
    );
    assert_eq!(
        f.cache.entitled_accounts(&ids(&[""]), "m", true),
        Err(CatalogError::InvalidAccount)
    );
    assert!(matches!(
        f.cache.union_models(&ids(&[""])),
        Err(CatalogError::InvalidAccount)
    ));
    for index in 0..MAX_MODELS {
        f.cache.deny_model("a", &format!("model-{index}")).unwrap();
    }
    f.cache.deny_model("a", "model-0").unwrap();
    assert_eq!(
        f.cache.deny_model("a", "overflow"),
        Err(CatalogError::TooManyModels)
    );
    assert!(f.cache.remove("a").unwrap());
    assert!(!f.cache.is_model_denied("a", "model-0").unwrap());
    for index in 0..MAX_ACCOUNTS {
        f.cache
            .deny_model(&format!("account-{index}"), "m")
            .unwrap();
    }
    assert_eq!(
        f.cache.deny_model("overflow", "m"),
        Err(CatalogError::AccountCapacity)
    );
}
