//! Launch metadata belongs to the binding snapshot. No token or account copy is
//! stored here: anchors and aliases address the ordinary authoritative group.
use super::*;

const IDLE_SECS: u64 = 24 * 60 * 60;
const KEY_BYTES: usize = 256;
const MODEL_BYTES: usize = 256;
const MODEL_COUNT: usize = 2048;
const MODELS_BYTES: usize = 64 * 1024;

/// Only known catalogs are present. Absence is not evidence of no entitlement.
pub type LaunchEntitlements = BTreeMap<String, BTreeSet<String>>;

#[derive(Clone, Copy, Debug)]
pub struct LaunchRequest<'a> {
    /// Unique nonsecret identifier beginning with `launch:`, never a token.
    pub anchor_key: &'a str,
    /// The actual C8 key, not the dashboard's hashed policy/session ID.
    pub resume_key: Option<&'a str>,
    pub model: Option<&'a str>,
    pub model_is_explicit: bool,
    /// Already resolved, unambiguous account ID. A preference, never a pin.
    pub account_pref: Option<&'a str>,
    pub allow_move_for_model: bool,
}

#[derive(Debug)]
pub struct LaunchPlacement {
    pub canonical_key: String,
    pub account: String,
    /// Use this model for native startup, including implicit resumes.
    pub effective_model: Option<String>,
    pub moves: Vec<MoveEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchInfo {
    pub canonical_key: String,
    pub account: String,
    pub current_model: Option<String>,
    pub advertised_models: BTreeSet<String>,
}

/// Called under BindingTable's mutation gate, without its data lock. A consent
/// authorizer must atomically check this revision/from and durably change the
/// Strict anchor, preserving policy and history. Ordinary dispatch is rechecked
/// afterwards. A no-op authorizer cannot bypass Strict.
#[derive(Clone, Debug)]
pub struct LaunchPolicyMove {
    pub key: String,
    pub previous: String,
    pub account: String,
    pub expected_revision: u64,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    #[error("invalid launch metadata")]
    InvalidMetadata,
    #[error("launch anchor already belongs to another session")]
    AnchorConflict,
    #[error("launch is unknown or expired; relaunch codex-qb")]
    UnknownLaunch,
    #[error("no usable account supports the effective model")]
    NoEligibleAccount,
    #[error("계정의 모델 지원 정보를 확인할 수 없습니다. 잠시 후 다시 실행하세요.")]
    EntitlementsUnknown,
    #[error("이 세션의 계정은 요청한 모델을 지원하지 않습니다.")]
    ModelUnsupported,
    #[error(
        "기존 세션 계정이 요청한 모델을 지원하지 않습니다. 계정 이동에 동의하려면 --account 또는 --move-for-model을 사용하세요."
    )]
    ConsentRequired,
    #[error("session has an active request or socket; retry after it finishes")]
    Busy,
    #[error("launch binding capacity exhausted")]
    Capacity,
    #[error("routing policy did not authorize the launch or move")]
    PolicyBlocked,
    #[error("binding changed before model metadata could be recorded")]
    BindingChanged,
    #[error("binding writer retired")]
    Retired,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Metadata {
    current_model: Option<String>,
    advertised_models: BTreeSet<String>,
    // Keep the same policy identity/history when the first real key is adopted.
    policy_key: String,
    idle_placement: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Anchor {
    canonical_key: String,
    last_used: DateTime<Utc>,
}

pub(super) struct SelectionPreference<'a> {
    pub account: Option<&'a str>,
    pub coverage: Option<&'a BTreeSet<String>>,
    pub model_move: bool,
}

pub(super) fn prefer<T>(
    pool: &mut Vec<(&Candidate, T)>,
    preference: Option<&SelectionPreference<'_>>,
) {
    let Some(preference) = preference else { return };
    if let Some(coverage) = preference.coverage
        && pool.iter().any(|(c, _)| coverage.contains(&c.account.id))
    {
        pool.retain(|(c, _)| coverage.contains(&c.account.id));
    }
    if let Some(account) = preference.account
        && pool.iter().any(|(c, _)| c.account.id == account)
    {
        pool.retain(|(c, _)| c.account.id == account);
    }
}

fn identifier(key: &str) -> bool {
    key.len() <= KEY_BYTES
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}
fn is_anchor(key: &str) -> bool {
    key.strip_prefix("launch:").is_some_and(|id| !id.is_empty()) && identifier(key)
}
fn real_key(key: &str) -> bool {
    key.strip_prefix("codex:").is_some_and(|id| !id.is_empty()) && identifier(key)
}
fn valid_model(model: &str) -> bool {
    !model.is_empty() && model.len() <= MODEL_BYTES && !model.chars().any(char::is_control)
}
fn valid_models(models: &BTreeSet<String>) -> bool {
    models.len() <= MODEL_COUNT
        && models.iter().all(|m| valid_model(m))
        && models.iter().map(String::len).sum::<usize>() <= MODELS_BYTES
}

fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_vec(value)
        .expect("binding metadata serialization")
        .len()
}
fn metadata_bound(key: &str, metadata: &Metadata) -> usize {
    json_len(key)
        .saturating_add(json_len(metadata))
        .saturating_add(usize::from(metadata.idle_placement))
        .saturating_add(64)
}
fn anchor_bound(key: &str, anchor: &Anchor) -> usize {
    // Fixed timestamp allowance includes Chrono's longest fractional form.
    json_len(key)
        .saturating_add(json_len(&anchor.canonical_key))
        .saturating_add(128)
}
fn aliases_bound(aliases: &BTreeMap<String, String>) -> usize {
    aliases
        .iter()
        .map(|(k, v)| json_len(k) + json_len(v) + 8)
        .sum()
}
fn metadata_weight(
    launches: &BTreeMap<String, Metadata>,
    anchors: &BTreeMap<String, Anchor>,
    aliases: &BTreeMap<String, String>,
) -> usize {
    if launches.is_empty() && anchors.is_empty() && aliases.is_empty() {
        return 0;
    }
    // Field names, object delimiters, and the legacy snapshot envelope.
    launches
        .iter()
        .map(|(k, v)| metadata_bound(k, v))
        .sum::<usize>()
        .saturating_add(
            anchors
                .iter()
                .map(|(k, v)| anchor_bound(k, v))
                .sum::<usize>(),
        )
        .saturating_add(aliases_bound(aliases))
        .saturating_add(128)
}
pub(super) fn snapshot_bound(state: &Snapshot) -> usize {
    state
        .bindings
        .iter()
        .map(|b| binding_bound(&b.key, &b.account))
        .fold(64usize, usize::saturating_add)
        .saturating_add(metadata_weight(
            &state.launches,
            &state.launch_anchors,
            &state.launch_aliases,
        ))
}
pub(super) fn rebuild(d: &mut Data) {
    // rebuild_indexes() has already recomputed the ordinary binding weight.
    d.launch_weight = metadata_weight(&d.launches, &d.launch_anchors, &d.launch_aliases);
    d.snapshot_weight = d.snapshot_weight.saturating_add(d.launch_weight);
    rebuild_retention(d);
}
fn reweight(d: &mut Data) {
    d.snapshot_weight = d.snapshot_weight.saturating_sub(d.launch_weight);
    rebuild(d);
}
fn rebuild_retention(d: &mut Data) {
    d.launch_retention.clear();
    for anchor in d.launch_anchors.values() {
        let last = d
            .launch_retention
            .entry(anchor.canonical_key.clone())
            .or_insert(anchor.last_used);
        *last = (*last).max(anchor.last_used);
    }
}
pub(super) fn retained(d: &Data, key: &str, now: DateTime<Utc>) -> bool {
    d.launch_retention
        .get(key)
        .is_some_and(|last| idle(now, *last) < IDLE_SECS)
}
pub(super) fn idle_placement(d: &Data, key: &str) -> bool {
    d.launches.get(key).is_some_and(|m| m.idle_placement)
}
// Ownership redirects use only launch anchors, never arbitrary request aliases.
// A provisional anchor can be renamed once; an adopted anchor is never redirected.
// prune retains it while either transferred counter is nonzero. No extra runtime
// map or persisted schema is needed, and stale receipts still require their epoch.
pub(super) fn ownership_group<'a>(d: &'a Data, key: &'a str) -> &'a str {
    d.launch_anchors
        .get(key)
        .map_or(key, |a| a.canonical_key.as_str())
}

pub(super) fn policy_key<'a>(d: &'a Data, key: Option<&'a str>) -> Option<&'a str> {
    key.map(|key| {
        d.launches
            .get(group(key))
            .map_or(key, |m| m.policy_key.as_str())
    })
}
pub(super) fn touch(d: &mut Data, key: &str, now: DateTime<Utc>) {
    if !d.launch_retention.contains_key(key) {
        return;
    }
    for anchor in d
        .launch_anchors
        .values_mut()
        .filter(|a| a.canonical_key == key)
    {
        anchor.last_used = anchor.last_used.max(now);
    }
    if let Some(last) = d.launch_retention.get_mut(key) {
        *last = (*last).max(now);
    }
    d.changed();
}
pub(super) fn request_started(d: &mut Data, key: &str, now: DateTime<Utc>) {
    if let Some(metadata) = d.launches.get_mut(key).filter(|m| m.idle_placement) {
        metadata.idle_placement = false;
        d.changed();
    }
    touch(d, key, now);
}

pub(super) fn validate(d: &Data) -> Result<()> {
    if d.launches.len() > CAPACITY * 2
        || d.launch_anchors.len() > CAPACITY
        || d.launch_aliases.len() > CAPACITY * 2
    {
        return Err(SnapshotCapacity.into());
    }
    let mut policies = BTreeSet::new();
    for key in d.bindings.keys().filter(|k| k.starts_with("launch:")) {
        if !d.launches.contains_key(key)
            || d.launch_anchors
                .get(key)
                .is_none_or(|a| a.canonical_key != *key)
        {
            bail!("orphaned provisional launch binding");
        }
    }
    for (key, m) in &d.launches {
        if !(real_key(key) || is_anchor(key))
            || !d.bindings.contains_key(key)
            || !(real_key(&m.policy_key) || is_anchor(&m.policy_key))
            || !policies.insert(&m.policy_key)
            || m.current_model.as_deref().is_some_and(|m| !valid_model(m))
            || !valid_models(&m.advertised_models)
        {
            bail!("invalid launch binding metadata");
        }
    }
    for (key, anchor) in &d.launch_anchors {
        if !is_anchor(key)
            || !d.launches.contains_key(&anchor.canonical_key)
            || (d.bindings.contains_key(key) && key != &anchor.canonical_key)
            || (is_anchor(&anchor.canonical_key) && key != &anchor.canonical_key)
        {
            bail!("invalid launch anchor");
        }
    }
    for (key, canonical) in &d.launch_aliases {
        // Flat aliases only: no chains, cycles, binding shadowing, or anchors.
        if !real_key(key)
            || !real_key(canonical)
            || key == canonical
            || d.bindings.contains_key(key)
            || d.launch_aliases.contains_key(canonical)
            || !d.launches.contains_key(canonical)
        {
            bail!("invalid launch alias");
        }
    }
    Ok(())
}

pub(super) fn remove_missing(d: &mut Data) {
    d.launches.retain(|key, _| d.bindings.contains_key(key));
    d.launch_anchors
        .retain(|_, a| d.launches.contains_key(&a.canonical_key));
    d.launch_aliases
        .retain(|_, key| d.launches.contains_key(key));
}
pub(super) fn remove_group(d: &mut Data, key: &str) {
    if d.launches.remove(key).is_some() {
        d.launch_anchors.retain(|_, a| a.canonical_key != key);
        d.launch_aliases.retain(|_, canonical| canonical != key);
        reweight(d);
    }
}
fn discard_group(d: &mut Data, key: &str) {
    if let Some(entry) = unindex_group(d, key) {
        for key in entry.keys {
            if let Some(b) = d.bindings.remove(&key) {
                d.snapshot_weight = d
                    .snapshot_weight
                    .saturating_sub(binding_bound(&b.key, &b.account));
            }
        }
    }
    d.epochs.remove(key);
    d.after_move.remove(key);
    d.overload_marks.remove(key);
    d.last_request_bytes.remove(key);
    d.unknown_cost.remove(key);
    for hits in d.rate_hits.values_mut() {
        hits.remove(key);
    }
    remove_group(d, key);
    d.changed();
}
pub(super) fn prune(d: &mut Data, now: DateTime<Utc>) {
    let before = d.launch_anchors.len();
    d.launch_anchors.retain(|_, a| {
        idle(now, a.last_used) < IDLE_SECS
            || d.active.contains_key(&a.canonical_key)
            || d.pins.contains_key(&a.canonical_key)
    });
    if before == d.launch_anchors.len() {
        return;
    }
    reweight(d);
    // Abandoned launches have no request activity and no active-window grace.
    let abandoned: Vec<_> = d
        .launches
        .keys()
        .filter(|k| is_anchor(k) && !d.launch_retention.contains_key(*k) && !protected(d, k))
        .cloned()
        .collect();
    for key in abandoned {
        discard_group(d, &key);
    }
    // D20 charges are conservative placement evidence: adoption/abandonment
    // adds no charge and never rewinds cumulative send tokens. They clear by a
    // strictly newer observation/poll, using the existing bounded deadlines.
    d.changed();
}

fn canonical<'a>(d: &'a Data, key: &'a str) -> Option<&'a str> {
    if d.groups.contains_key(group(key)) {
        return Some(group(key));
    }
    d.launch_aliases
        .get(key)
        .map(String::as_str)
        .or_else(|| d.launch_anchors.get(key).map(|a| a.canonical_key.as_str()))
}
fn info(d: &Data, key: &str) -> std::result::Result<LaunchInfo, LaunchError> {
    let key = canonical(d, key).ok_or(LaunchError::UnknownLaunch)?;
    let group = d.groups.get(key).ok_or(LaunchError::UnknownLaunch)?;
    let metadata = d.launches.get(key);
    Ok(LaunchInfo {
        canonical_key: key.to_owned(),
        account: group.account.clone(),
        current_model: metadata.and_then(|m| m.current_model.clone()),
        advertised_models: metadata.map_or_else(BTreeSet::new, |m| m.advertised_models.clone()),
    })
}
fn room(weight: usize) -> std::result::Result<(), LaunchError> {
    if weight as u64 + 64 > SNAPSHOT_MAX_BYTES {
        Err(LaunchError::Capacity)
    } else {
        Ok(())
    }
}

impl BindingTable {
    /// Relay admission/hold checks must use these wrappers after adoption so
    /// they consult the same durable policy identity as selection and commit.
    pub fn routing_plan(&self, key: Option<&str>) -> crate::routing::Plan {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.policies.plan(policy_key(&d, key))
    }
    pub fn permits_dispatch(&self, key: Option<&str>, account: &str) -> bool {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.policies.permits_dispatch(policy_key(&d, key), account)
    }
    pub fn place_launch(
        &self,
        request: LaunchRequest<'_>,
        candidates: &[Candidate],
        entitlements: &LaunchEntitlements,
        now: DateTime<Utc>,
    ) -> std::result::Result<LaunchPlacement, LaunchError> {
        self.place_launch_with_authorizer(request, candidates, entitlements, now, |_, _| {
            bail!("explicit Strict model-move authorization required")
        })
    }

    pub fn place_launch_with_authorizer(
        &self,
        request: LaunchRequest<'_>,
        candidates: &[Candidate],
        entitlements: &LaunchEntitlements,
        now: DateTime<Utc>,
        authorize: impl FnOnce(&crate::routing::PolicyStore, &LaunchPolicyMove) -> Result<()>,
    ) -> std::result::Result<LaunchPlacement, LaunchError> {
        self.place_launch_for_issue(request, candidates, entitlements, now, None, authorize)
    }

    pub(crate) fn place_launch_for_issue(
        &self,
        request: LaunchRequest<'_>,
        candidates: &[Candidate],
        entitlements: &LaunchEntitlements,
        now: DateTime<Utc>,
        operation: Option<&crate::issue_operation::IssueOperation>,
        authorize: impl FnOnce(&crate::routing::PolicyStore, &LaunchPolicyMove) -> Result<()>,
    ) -> std::result::Result<LaunchPlacement, LaunchError> {
        if !is_anchor(request.anchor_key)
            || request.resume_key.is_some_and(|k| !real_key(k))
            || request.model.is_some_and(|m| !valid_model(m))
            || (request.model_is_explicit && request.model.is_none())
        {
            return Err(LaunchError::InvalidMetadata);
        }
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        if self.writes_stopped.load(Ordering::Acquire) {
            return Err(LaunchError::Retired);
        }
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        // Both binding gates are held; this remains a precommit check.
        if let Some(operation) = operation {
            operation.check().map_err(|_| LaunchError::PolicyBlocked)?;
        }
        self.prune(&mut d, now);
        if let Some(anchor) = d.launch_anchors.get(request.anchor_key) {
            if request
                .resume_key
                .is_some_and(|k| canonical(&d, k) != Some(anchor.canonical_key.as_str()))
            {
                return Err(LaunchError::AnchorConflict);
            }
            let key = anchor.canonical_key.clone();
            let prior = info(&d, &key)?;
            if let Some(operation) = operation {
                operation.admit().map_err(|_| LaunchError::PolicyBlocked)?;
            }
            touch(&mut d, &key, now);
            return Ok(LaunchPlacement {
                canonical_key: key,
                account: prior.account,
                effective_model: prior.current_model,
                moves: Vec::new(),
            });
        }
        if d.launches
            .values()
            .any(|m| m.policy_key == request.anchor_key)
        {
            return Err(LaunchError::AnchorConflict);
        }
        if d.bindings.contains_key(request.anchor_key) || d.launch_anchors.len() >= CAPACITY {
            return Err(LaunchError::Capacity);
        }
        let key = request
            .resume_key
            .and_then(|k| canonical(&d, k))
            .or(request.resume_key)
            .unwrap_or(request.anchor_key)
            .to_owned();
        let policy_key = policy_key(&d, Some(&key)).unwrap().to_owned();
        let plan = self.policies.plan(Some(&policy_key));
        let previous = plan
            .anchor
            .clone()
            .or_else(|| d.groups.get(&key).map(|g| g.account.clone()));
        let old_metadata = d.launches.get(&key).cloned();
        let old_model = old_metadata.as_ref().and_then(|m| m.current_model.clone());
        let mut model = if previous.is_some() && !request.model_is_explicit {
            old_model
                .clone()
                .or_else(|| request.model.map(str::to_owned))
        } else {
            request.model.map(str::to_owned).or(old_model.clone())
        };
        let current = previous
            .as_deref()
            .and_then(|id| candidates.iter().find(|c| c.account.id == id));
        let usable = current.is_some_and(|c| {
            effective_account(c, &d, &self.config, None, now).eligible_at(None, now)
                && (!c.account.drain || Self::accepted_in(&d, Some(&key), &c.account.id))
        });
        let mut model_move = false;
        if let (Some(id), Some(model)) = (
            previous.as_deref(),
            request.model.filter(|_| request.model_is_explicit),
        ) {
            let supported = entitlements
                .get(id)
                .ok_or(LaunchError::EntitlementsUnknown)?
                .contains(model)
                && current.is_none_or(|c| {
                    effective_account(c, &d, &self.config, Some(model), now)
                        .eligible_at(Some(model), now)
                });
            if !supported {
                if !request.allow_move_for_model && request.account_pref.is_none() {
                    return Err(LaunchError::ConsentRequired);
                }
                model_move = true;
            }
        }
        if usable && !model_move && !request.model_is_explicit && old_model.is_none() {
            // A legacy binding has no proved current model. An incompatible
            // implicit default never turns that into permission to relocate.
            if let (Some(id), Some(default)) = (previous.as_deref(), model.as_deref()) {
                if !entitlements.get(id).is_some_and(|s| s.contains(default)) {
                    model = None;
                }
            }
        }
        for candidate in candidates {
            if let Some(snapshot) = &candidate.quota {
                d.placements
                    .entry(candidate.account.id.clone())
                    .or_default()
                    .observe(snapshot);
            }
        }
        let filtered: Vec<_> = candidates
            .iter()
            .filter(|c| c.account.provider == Provider::Codex)
            .filter(|c| {
                model.as_deref().is_none_or(|m| {
                    entitlements
                        .get(&c.account.id)
                        .is_some_and(|s| s.contains(m))
                })
            })
            .cloned()
            .collect();
        let coverage: BTreeSet<_> = old_metadata.as_ref().map_or_else(BTreeSet::new, |m| {
            entitlements
                .iter()
                .filter(|(_, s)| m.advertised_models.is_subset(s))
                .map(|(id, _)| id.clone())
                .collect()
        });
        let preference = SelectionPreference {
            account: request.account_pref,
            coverage: previous.as_ref().filter(|_| !model_move).map(|_| &coverage),
            model_move,
        };
        let (account, reason) = if usable && !model_move {
            (previous.clone().unwrap(), "launch_resume")
        } else {
            let (choice, _) = self.select_ordered(
                &d,
                Provider::Codex,
                Some(&key),
                None,
                model.as_deref(),
                &filtered,
                None,
                now,
                false,
                Some(&preference),
            );
            match choice {
                PreviewDecision::Selected { account } => (
                    account,
                    if model_move {
                        "model_choice"
                    } else if previous.is_some() {
                        "forced"
                    } else {
                        "launch_model"
                    },
                ),
                _ if plan.settings.mode == crate::routing::Mode::Strict && !model_move => {
                    return Err(LaunchError::PolicyBlocked);
                }
                _ if model.is_some()
                    && candidates.iter().any(|c| {
                        c.account.provider == Provider::Codex
                            && !entitlements.contains_key(&c.account.id)
                    }) =>
                {
                    return Err(LaunchError::EntitlementsUnknown);
                }
                _ => return Err(LaunchError::NoEligibleAccount),
            }
        };
        if previous.as_deref().is_some_and(|id| id != account) && protected(&d, &key) {
            return Err(LaunchError::Busy);
        }
        let metadata = Metadata {
            current_model: model.clone(),
            advertised_models: old_metadata
                .as_ref()
                .map_or_else(BTreeSet::new, |m| m.advertised_models.clone()),
            policy_key: policy_key.clone(),
            idle_placement: old_metadata
                .as_ref()
                .map_or(!d.groups.contains_key(&key), |m| m.idle_placement),
        };
        let anchor = Anchor {
            canonical_key: key.clone(),
            last_used: now,
        };
        let mut weight = d
            .snapshot_weight
            .saturating_add(if d.launch_weight == 0 { 128 } else { 0 })
            .saturating_sub(old_metadata.as_ref().map_or(0, |m| metadata_bound(&key, m)))
            .saturating_add(metadata_bound(&key, &metadata))
            .saturating_add(anchor_bound(request.anchor_key, &anchor));
        if let Some(entry) = d.groups.get(&key) {
            for key in &entry.keys {
                weight = weight
                    .saturating_sub(binding_bound(key, &entry.account))
                    .saturating_add(binding_bound(key, &account));
            }
        } else {
            if d.bindings.len() >= CAPACITY * 2 {
                return Err(LaunchError::Capacity);
            }
            weight = weight.saturating_add(binding_bound(&key, &account));
        }
        room(weight)?;
        let consent = LaunchPolicyMove {
            key: policy_key.clone(),
            previous: previous.clone().unwrap_or_default(),
            account: account.clone(),
            expected_revision: plan.revision,
            at: now,
        };
        drop(d);
        if model_move && plan.settings.mode == crate::routing::Mode::Strict {
            authorize(&self.policies, &consent).map_err(|_| LaunchError::PolicyBlocked)?;
        }
        self.policies
            .dispatch_for_issue(
                Some(&policy_key),
                &account,
                previous.as_deref(),
                reason,
                now,
                operation,
            )
            .map_err(|_| LaunchError::PolicyBlocked)?;
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let moves = self.commit_binding(
            &mut d,
            Provider::Codex,
            Some(&key),
            None,
            candidates,
            None,
            0,
            false,
            now,
            &account,
            reason,
        );
        d.launches.insert(key.clone(), metadata);
        d.launch_anchors
            .insert(request.anchor_key.to_owned(), anchor);
        reweight(&mut d);
        d.changed();
        Ok(LaunchPlacement {
            canonical_key: key,
            account,
            effective_model: model,
            moves,
        })
    }

    /// Call on EVERY managed data request before routing, including unprefixed
    /// existing keys. Bound-key precedence does not depend on token validity.
    /// UnknownLaunch on an unprefixed, unbound request permits legacy routing;
    /// a token-scoped request must instead fail explicitly.
    pub fn resolve_launch(
        &self,
        anchor: Option<&str>,
        request_key: Option<&str>,
        now: DateTime<Utc>,
    ) -> std::result::Result<LaunchInfo, LaunchError> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, now);
        if let Some(key) = request_key
            .and_then(|k| canonical(&d, k))
            .map(str::to_owned)
        {
            // Only a still-provisional losing launch can be coalesced. An
            // already adopted anchor is never redirected by another bound key.
            if let Some(source) = anchor
                .and_then(|a| d.launch_anchors.get(a))
                .map(|a| a.canonical_key.clone())
            {
                if source != key && is_anchor(&source) && protected(&d, &source) {
                    // The socket still uses its original account's credentials.
                    // Never report a bound winner while a losing owner is live.
                    // Even same-account live merges need a separate epoch/policy
                    // merge protocol; only a fresh-key rename is supported here.
                    return Err(LaunchError::Busy);
                }
                if source != key
                    && is_anchor(&source)
                    && !self.writes_stopped.load(Ordering::Acquire)
                {
                    // Bound routing remains available even if coalescing an
                    // optional provisional handle would exceed metadata caps.
                    if let Err(error) = self.merge_provisional(&mut d, &source, &key) {
                        if error != LaunchError::Capacity {
                            return Err(error);
                        }
                    }
                }
            }
            touch(&mut d, &key, now);
            return info(&d, &key);
        }
        if self.writes_stopped.load(Ordering::Acquire) {
            return Err(LaunchError::Retired);
        }
        let source = anchor
            .and_then(|a| d.launch_anchors.get(a))
            .map(|a| a.canonical_key.clone())
            .ok_or(LaunchError::UnknownLaunch)?;
        let mut key = source.clone();
        if let Some(request_key) = request_key {
            if !real_key(request_key) {
                return Err(LaunchError::InvalidMetadata);
            }
            if is_anchor(&source) {
                let live = protected(&d, &source);
                // Unbound keys can still have owners through pin/begin_request.
                // Renaming is not a merge with those independent owners either.
                if protected(&d, request_key) {
                    return Err(LaunchError::Busy);
                }
                let mut metadata = d
                    .launches
                    .get(&source)
                    .cloned()
                    .ok_or(LaunchError::UnknownLaunch)?;
                let target_plan = self.policies.plan(Some(request_key));
                let old_plan = self.policies.plan(Some(&metadata.policy_key));
                if target_plan
                    .anchor
                    .as_deref()
                    .is_some_and(|a| a != d.groups[&source].account)
                    || (target_plan.source != "global" && old_plan.source != "global")
                    // Live owners/receipts retain their original policy identity.
                    // A target-specific policy requires reconnect, even if it
                    // currently permits the same account. Idle behavior is unchanged.
                    || (live && target_plan.source != "global")
                {
                    return Err(LaunchError::PolicyBlocked);
                }
                if target_plan.source != "global" {
                    metadata.policy_key = request_key.to_owned();
                }
                let entry = d.groups.get(&source).ok_or(LaunchError::UnknownLaunch)?;
                let weight = d
                    .snapshot_weight
                    .saturating_sub(binding_bound(&source, &entry.account))
                    .saturating_add(binding_bound(request_key, &entry.account))
                    .saturating_sub(metadata_bound(&source, &d.launches[&source]))
                    .saturating_add(metadata_bound(request_key, &metadata))
                    .saturating_add(d.launch_anchors.len().saturating_mul(KEY_BYTES));
                room(weight)?;
                let mut entry = unindex_group(&mut d, &source).unwrap();
                let mut binding = d
                    .bindings
                    .remove(&source)
                    .ok_or(LaunchError::UnknownLaunch)?;
                d.snapshot_weight = d
                    .snapshot_weight
                    .saturating_sub(binding_bound(&source, &binding.account));
                binding.key = request_key.to_owned();
                d.snapshot_weight = d
                    .snapshot_weight
                    .saturating_add(binding_bound(request_key, &binding.account));
                d.bindings.insert(request_key.to_owned(), binding);
                entry.keys = BTreeSet::from([request_key.to_owned()]);
                d.launches.remove(&source);
                d.launches.insert(request_key.to_owned(), metadata);
                for a in d
                    .launch_anchors
                    .values_mut()
                    .filter(|a| a.canonical_key == source)
                {
                    a.canonical_key = request_key.to_owned();
                }
                if d.after_move.remove(&source) {
                    d.after_move.insert(request_key.to_owned());
                }
                if d.unknown_cost.remove(&source) {
                    d.unknown_cost.insert(request_key.to_owned());
                }
                if let Some(bytes) = d.last_request_bytes.remove(&source) {
                    d.last_request_bytes.insert(request_key.to_owned(), bytes);
                }
                // The mutation gate plus data lock makes this a single rename:
                // no unpinned interval, new placement, or account/policy move.
                // Existing owners keep source and resolve its immutable anchor.
                if let Some(count) = d.active.remove(&source) {
                    *d.active.entry(request_key.to_owned()).or_default() += count;
                }
                if let Some(count) = d.pins.remove(&source) {
                    *d.pins.entry(request_key.to_owned()).or_default() += count;
                }
                if let Some(epoch) = d.epochs.remove(&source) {
                    d.epochs.insert(request_key.to_owned(), epoch);
                }
                d.overload_marks.remove(&source);
                for hits in d.rate_hits.values_mut() {
                    hits.remove(&source);
                }
                index_group(&mut d, request_key.to_owned(), entry);
                reweight(&mut d);
                d.changed();
                key = request_key.to_owned();
            } else {
                if d.launch_aliases.len() >= CAPACITY * 2 {
                    return Err(LaunchError::Capacity);
                }
                room(
                    d.snapshot_weight
                        .saturating_add(json_len(request_key) + json_len(&source) + 8),
                )?;
                d.launch_aliases.insert(request_key.to_owned(), source);
                reweight(&mut d);
                d.changed();
            }
        }
        touch(&mut d, &key, now);
        info(&d, &key)
    }

    fn merge_provisional(
        &self,
        d: &mut Data,
        source: &str,
        target: &str,
    ) -> std::result::Result<(), LaunchError> {
        let source_metadata = d
            .launches
            .get(source)
            .cloned()
            .ok_or(LaunchError::UnknownLaunch)?;
        let old = d.launches.get(target).cloned();
        let mut metadata = old.clone().unwrap_or(Metadata {
            current_model: None,
            advertised_models: BTreeSet::new(),
            policy_key: target.to_owned(),
            idle_placement: false,
        });
        metadata
            .advertised_models
            .extend(source_metadata.advertised_models);
        if !valid_models(&metadata.advertised_models) {
            return Err(LaunchError::Capacity);
        }
        room(
            d.snapshot_weight
                .saturating_add(metadata_bound(target, &metadata))
                .saturating_sub(old.as_ref().map_or(0, |m| metadata_bound(target, m)))
                .saturating_add(d.launch_anchors.len().saturating_mul(KEY_BYTES)),
        )?;
        // Remap before discard so the winner keeps every valid losing handle.
        for a in d
            .launch_anchors
            .values_mut()
            .filter(|a| a.canonical_key == source)
        {
            a.canonical_key = target.to_owned();
        }
        discard_group(d, source);
        d.launches.insert(target.to_owned(), metadata);
        reweight(d);
        d.changed();
        Ok(())
    }

    /// Read/touch metadata only; never places, moves, reserves, or uses sticky.
    pub fn launch_info(
        &self,
        key: &str,
        now: DateTime<Utc>,
    ) -> std::result::Result<LaunchInfo, LaunchError> {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, now);
        let result = info(&d, key)?;
        touch(&mut d, &result.canonical_key, now);
        Ok(result)
    }

    /// Account guards reject an observation captured before a concurrent move.
    pub fn record_launch_model(
        &self,
        key: &str,
        account: &str,
        model: &str,
        now: DateTime<Utc>,
    ) -> std::result::Result<LaunchInfo, LaunchError> {
        if !valid_model(model) {
            return Err(LaunchError::InvalidMetadata);
        }
        self.record_launch_metadata(key, account, now, |m| {
            m.current_model = Some(model.to_owned())
        })
    }
    /// Record only models actually advertised to this session, not a union of
    /// account entitlements. Retaining the union of attached launches makes a
    /// forced destination's whole-catalog preference conservative.
    pub fn record_launch_advertised(
        &self,
        key: &str,
        account: &str,
        models: &BTreeSet<String>,
        now: DateTime<Utc>,
    ) -> std::result::Result<LaunchInfo, LaunchError> {
        if !valid_models(models) {
            return Err(LaunchError::InvalidMetadata);
        }
        self.record_launch_metadata(key, account, now, |m| {
            m.advertised_models.extend(models.iter().cloned())
        })
    }
    fn record_launch_metadata(
        &self,
        key: &str,
        account: &str,
        now: DateTime<Utc>,
        update: impl FnOnce(&mut Metadata),
    ) -> std::result::Result<LaunchInfo, LaunchError> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        if self.writes_stopped.load(Ordering::Acquire) {
            return Err(LaunchError::Retired);
        }
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, now);
        let before = info(&d, key)?;
        if before.account != account {
            return Err(LaunchError::BindingChanged);
        }
        let key = before.canonical_key;
        let old = d.launches.get(&key).ok_or(LaunchError::UnknownLaunch)?;
        let mut next = old.clone();
        update(&mut next);
        if !valid_models(&next.advertised_models) {
            return Err(LaunchError::Capacity);
        }
        room(
            d.snapshot_weight
                .saturating_sub(metadata_bound(&key, old))
                .saturating_add(metadata_bound(&key, &next)),
        )?;
        d.launches.insert(key.clone(), next);
        reweight(&mut d);
        touch(&mut d, &key, now);
        d.changed();
        info(&d, &key)
    }
}

#[cfg(test)]
mod r11_active_adoption_tests {
    use super::*;
    use crate::routing::{Mode, Project, Settings, session_id};

    const SOURCE: &str = "launch:active-adoption";
    const REAL: &str = "codex:active-adoption";

    fn fixture(mode: Mode) -> (tempfile::TempDir, Arc<BindingTable>, Vec<Candidate>, LaunchEntitlements) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().parent().unwrap()
            .join(".sandbox/r11-prep/active-adoption-test-runs");
        fs::create_dir_all(&root).unwrap();
        let home = tempfile::Builder::new().prefix("case-").tempdir_in(root).unwrap();
        let mut config = RoutingConfig::default();
        config.mode = mode;
        let table = BindingTable::new(home.path(), config).unwrap();
        let mut candidates = Vec::new();
        for id in ["a", "b"] {
            let mut c = super::super::tests::candidate(id, 10.0, 30, 2);
            c.account.provider = Provider::Codex;
            c.account.identity = crate::accounts::Identity::Codex {
                user_id: None,
                account_id: id.into(),
                email: "synthetic@example.invalid".into(),
            };
            candidates.push(c);
        }
        let entitlements = BTreeMap::from([
            ("a".to_owned(), BTreeSet::from(["m1".to_owned()])),
            ("b".to_owned(), BTreeSet::from(["m1".to_owned(), "m2".to_owned()])),
        ]);
        (home, table, candidates, entitlements)
    }

    fn request(anchor: &str) -> LaunchRequest<'_> {
        LaunchRequest {
            anchor_key: anchor,
            resume_key: None,
            model: Some("m1"),
            model_is_explicit: true,
            account_pref: Some("a"),
            allow_move_for_model: false,
        }
    }

    fn place(t: &BindingTable, cs: &[Candidate], es: &LaunchEntitlements) {
        assert_eq!(t.place_launch(request(SOURCE), cs, es, Utc::now()).unwrap().account, "a");
    }

    fn counts(t: &BindingTable, key: &str, active: usize, pins: usize, pending: usize) {
        let d = t.data.lock().unwrap();
        assert_eq!(d.active.get(key).copied().unwrap_or(0), active);
        assert_eq!(d.pins.get(key).copied().unwrap_or(0), pins);
        assert_eq!(d.pending.get("a").copied().unwrap_or(0), pending);
        if key == REAL {
            assert!(!d.active.contains_key(SOURCE));
            assert!(!d.pins.contains_key(SOURCE));
        }
    }

    #[test]
    fn r11_active_adoption_live_pin_first_actual_key_and_old_receipt() {
        let (_home, t, cs, es) = fixture(Mode::Rebalance);
        place(&t, &cs, &es);
        let pin = t.pin(Some(SOURCE));
        let mut lease = t.begin_request(Some(SOURCE), "a");
        lease.after_move = true;
        lease.defer_after_move();
        let receipt = lease.receipt();
        let pending = t.pending_count("a");
        let policy_before = t.policies.list();
        let placement_before = t.data.lock().unwrap().placements["a"].total;
        let resolved = t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()).unwrap();
        assert_eq!(resolved.canonical_key, REAL);
        assert_eq!(resolved.account, "a");
        counts(&t, REAL, 1, 1, pending);
        receipt.dispatched();
        {
            let d = t.data.lock().unwrap();
            assert_eq!(d.groups.len(), 1);
            assert_eq!(d.bindings.len(), 1);
            assert_eq!(d.bindings[REAL].accepted, Some(true));
            assert_eq!(d.launches[REAL].policy_key, SOURCE);
            assert_eq!(d.placements["a"].total, placement_before);
            assert!(!d.lru.iter().any(|(_, key)| key == REAL));
            assert!(Arc::ptr_eq(&d.epochs[REAL], &receipt.epoch));
        }
        assert_eq!(t.policies.list(), policy_before);
        drop(lease);
        counts(&t, REAL, 0, 1, 0);
        assert!(t.data.lock().unwrap().after_move.contains(REAL));
        drop(pin);
        counts(&t, REAL, 0, 0, 0);
        let d = t.data.lock().unwrap();
        assert!(d.active.is_empty() && d.pins.is_empty() && d.pending.is_empty());
        assert!(d.lru.iter().any(|(_, key)| key == REAL));
    }

    #[test]
    fn r11_active_adoption_pre_adoption_drops_and_stale_source_reservations() {
        let (_home, t, cs, es) = fixture(Mode::Rebalance);
        place(&t, &cs, &es);
        let keep_pin = t.pin(Some(SOURCE));
        let gone_pin = t.pin(Some(SOURCE));
        let gone_lease = t.begin_request(Some(SOURCE), "a");
        drop(gone_pin);
        drop(gone_lease);
        counts(&t, SOURCE, 0, 1, 0);
        t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()).unwrap();
        // A caller that captured the anchor before the rename still reserves
        // the adopted group, rather than resurrecting counters on source.
        let later_pin = t.pin(Some(SOURCE));
        let later_lease = t.begin_request(Some(SOURCE), "a");
        counts(&t, REAL, 1, 2, 1);
        drop(keep_pin);
        counts(&t, REAL, 1, 1, 1);
        drop(later_pin);
        counts(&t, REAL, 1, 0, 1);
        drop(later_lease);
        counts(&t, REAL, 0, 0, 0);
    }

    #[test]
    fn r11_active_adoption_socket_blocks_concurrent_resume_model_move() {
        let (_home, t, cs, es) = fixture(Mode::Rebalance);
        place(&t, &cs, &es);
        let pin = t.pin(Some(SOURCE));
        t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker = {
            let t = t.clone();
            let barrier = barrier.clone();
            let cs = cs.clone();
            let es = es.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut r = request("launch:concurrent-resume");
                r.resume_key = Some(REAL);
                r.model = Some("m2");
                r.account_pref = Some("b");
                r.allow_move_for_model = true;
                t.place_launch(r, &cs, &es, Utc::now()).map(|p| p.account)
            })
        };
        barrier.wait();
        assert_eq!(t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()).unwrap().account, "a");
        assert_eq!(worker.join().unwrap(), Err(LaunchError::Busy));
        counts(&t, REAL, 0, 1, 0);
        assert_eq!(t.current(Some(REAL)).as_deref(), Some("a"));
        drop(pin);
        let mut r = request("launch:resume-after-close");
        r.resume_key = Some(REAL);
        r.model = Some("m2");
        r.account_pref = Some("b");
        r.allow_move_for_model = true;
        assert_eq!(t.place_launch(r, &cs, &es, Utc::now()).unwrap().account, "b");
    }

    #[test]
    fn r11_active_adoption_live_bound_winner_is_busy_without_redirect() {
        for target_account in ["a", "b"] {
            let (_home, t, cs, es) = fixture(Mode::Rebalance);
            place(&t, &cs, &es);
            let mut r = request("launch:bound-winner");
            r.resume_key = Some(REAL);
            r.account_pref = Some(target_account);
            assert_eq!(t.place_launch(r, &cs, &es, Utc::now()).unwrap().account, target_account);
            let pin = t.pin(Some(SOURCE));
            let lease = t.begin_request(Some(SOURCE), "a");
            assert_eq!(t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()), Err(LaunchError::Busy));
            counts(&t, SOURCE, 1, 1, 1);
            assert_eq!(t.launch_info(SOURCE, Utc::now()).unwrap().account, "a");
            assert_eq!(t.current(Some(REAL)).as_deref(), Some(target_account));
            drop(pin);
            drop(lease);
            counts(&t, SOURCE, 0, 0, 0);
        }
    }

    fn strict() -> Settings {
        Settings { mode: Mode::Strict, reset_wait_secs: 0 }
    }

    #[test]
    fn r11_active_adoption_unbound_target_owners_are_not_merged() {
        let (_home, t, cs, es) = fixture(Mode::Rebalance);
        place(&t, &cs, &es);
        let source_pin = t.pin(Some(SOURCE));
        let target_pin = t.pin(Some(REAL));
        let target_lease = t.begin_request(Some(REAL), "b");
        assert_eq!(t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()), Err(LaunchError::Busy));
        {
            let d = t.data.lock().unwrap();
            assert_eq!(d.pins.get(SOURCE), Some(&1));
            assert_eq!(d.pins.get(REAL), Some(&1));
            assert_eq!(d.active.get(REAL), Some(&1));
            assert_eq!(d.pending.get("b"), Some(&1));
            assert!(!d.bindings.contains_key(REAL));
        }
        drop(target_lease);
        drop(target_pin);
        t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()).unwrap();
        drop(source_pin);
        let d = t.data.lock().unwrap();
        assert!(d.active.is_empty() && d.pins.is_empty() && d.pending.is_empty());
    }

    #[test]
    fn r11_active_adoption_strict_target_anchor_conflict_is_unchanged() {
        let (_home, t, cs, es) = fixture(Mode::Rebalance);
        place(&t, &cs, &es);
        t.policies.set_session(&session_id(REAL), Some(strict()), None, Some("b")).unwrap();
        let pin = t.pin(Some(SOURCE));
        let before = t.policies.list();
        assert_eq!(t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()), Err(LaunchError::PolicyBlocked));
        assert_eq!(t.policies.list(), before);
        counts(&t, SOURCE, 0, 1, 0);
        assert!(!t.data.lock().unwrap().bindings.contains_key(REAL));
        drop(pin);
    }

    #[test]
    fn r11_active_adoption_project_policy_conflicts_and_live_identity_guard() {
        for source_project in [false, true] {
            let (_home, t, cs, es) = fixture(Mode::Rebalance);
            place(&t, &cs, &es);
            t.policies.put_project(Some("adoption"), Project { name: "adoption".into(), settings: strict() }).unwrap();
            if source_project {
                t.associate_project(SOURCE, "adoption").unwrap();
            }
            // Same-account target policy must not change a live owner's identity.
            t.policies.associate_with_anchor(REAL, "adoption", Some("a")).unwrap();
            let pin = t.pin(Some(SOURCE));
            let before = t.policies.list();
            assert_eq!(t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()), Err(LaunchError::PolicyBlocked));
            counts(&t, SOURCE, 0, 1, 0);
            assert_eq!(t.policies.list(), before);
            assert_eq!(t.data.lock().unwrap().launches[SOURCE].policy_key, SOURCE);
            drop(pin);
        }
    }

    #[test]
    fn r11_active_adoption_strict_identity_history_and_snapshot_reload() {
        let (home, t, cs, es) = fixture(Mode::Strict);
        place(&t, &cs, &es);
        let pin = t.pin(Some(SOURCE));
        let lease = t.begin_request(Some(SOURCE), "a");
        let policy_path = home.path().join("state/routing-policies.json");
        let policy_bytes = fs::read(&policy_path).unwrap();
        let before = t.policies.list();
        t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()).unwrap();
        lease.dispatched();
        t.record_launch_advertised(REAL, "a", &es["a"], Utc::now()).unwrap();
        t.flush().unwrap();
        assert_eq!(fs::read(&policy_path).unwrap(), policy_bytes);
        assert_eq!(t.policies.list(), before);
        // An independent reload does not restore runtime counters, but its
        // persisted anchor and policy key still identify the adopted binding.
        let mut config = RoutingConfig::default();
        config.mode = Mode::Strict;
        let restored = BindingTable::new(home.path(), config).unwrap();
        let resolved = restored.resolve_launch(Some(SOURCE), None, Utc::now()).unwrap();
        assert_eq!(resolved.canonical_key, REAL);
        assert_eq!(resolved.account, "a");
        assert_eq!(resolved.advertised_models, es["a"]);
        assert_eq!(resolved.current_model.as_deref(), Some("m1"));
        assert!(restored.permits_dispatch(Some(REAL), "a"));
        assert!(!restored.permits_dispatch(Some(REAL), "b"));
        assert_eq!(restored.routing_plan(Some(REAL)).settings.mode, Mode::Strict);
        counts(&restored, REAL, 0, 0, 0);
        assert_eq!(restored.data.lock().unwrap().launches[REAL].policy_key, SOURCE);
        // The live original still owns its pin/lease after its snapshot.
        counts(&t, REAL, 1, 1, 1);
        drop(pin);
        drop(lease);
        counts(&t, REAL, 0, 0, 0);
    }

    #[test]
    fn r11_active_adoption_gate_orders_drop_and_retains_idle_anchor_while_pinned() {
        let (_home, t, cs, es) = fixture(Mode::Rebalance);
        place(&t, &cs, &es);
        let pin = t.pin(Some(SOURCE));
        t.resolve_launch(Some(SOURCE), Some(REAL), Utc::now()).unwrap();
        let gate = t.mutations.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            drop(pin);
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.try_recv().is_err());
        {
            let mut d = t.data.lock().unwrap();
            prune(&mut d, Utc::now() + chrono::Duration::days(2));
            assert_eq!(d.launch_anchors[SOURCE].canonical_key, REAL);
            assert_eq!(d.pins.get(REAL), Some(&1));
        }
        drop(gate);
        done_rx.recv().unwrap();
        worker.join().unwrap();
        counts(&t, REAL, 0, 0, 0);
    }
}


#[cfg(test)]
mod issue_operation_binding_tests {
    use super::*;
    use crate::issue_operation::IssueOperation;
    use std::sync::{Arc, Barrier};

    #[test]
    fn cancellation_behind_actual_binding_gate_prevents_placement() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r11-operation-tests");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let table = BindingTable::new(dir.path(), crate::config::RoutingConfig::default()).unwrap();
        let operation = IssueOperation::new(tokio::time::Instant::now()
            + std::time::Duration::from_secs(3600));
        let caller = operation.caller_guard();
        let ready = Arc::new(Barrier::new(2));
        let held = table.mutations.lock().unwrap();
        let task = {
            let table = table.clone();
            let operation = operation.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                ready.wait();
                table.place_launch_for_issue(LaunchRequest {
                    anchor_key: "launch:operation-cancel", resume_key: None,
                    model: Some("m"), model_is_explicit: true,
                    account_pref: None, allow_move_for_model: false,
                }, &[], &LaunchEntitlements::new(), chrono::Utc::now(), Some(&operation),
                    |_, _| panic!("cancelled placement must not authorize"))
            })
        };
        ready.wait();
        drop(caller);
        drop(held);
        assert!(matches!(task.join().unwrap(), Err(LaunchError::PolicyBlocked)));
        let data = table.data.lock().unwrap();
        assert!(data.launches.is_empty());
        assert!(data.launch_anchors.is_empty());
        assert!(data.bindings.is_empty());
    }
}
