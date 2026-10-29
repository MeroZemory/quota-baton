//! Selection's independent privacy projection. Native Accounts are never serialized.
use super::*;
use crate::bindings::kernel::{
    selection::{
        LaunchInput, PreferenceInput, RootHintInput, SelectionChoice, SelectionDecision,
        SelectionEntry, SelectionInputs, SelectionPolicyInput, SelectionReturnInput,
    },
    selection_affinity::AffinityInput,
    selection_metrics::MetricInputs,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectionRecord {
    pub schema: u32,
    pub ordinal: u64,
    pub inputs: Option<WireSelectionInputs>,
    pub actual: SelectionDecision,
    pub refusal: Option<WireRefusal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectionRow {
    pub account: AccountFacts,
    pub quota: Field<QuotaFacts>,
    pub inflight: usize,
}

/// Only selection reads are archived. Permit-only from/to/permit are not reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireSelectionInputs {
    pub now: DateTime<Utc>,
    pub model: Option<Model>,
    pub key: Option<String>,
    pub accounts: BTreeMap<String, SelectionRow>,
    pub traffic: BTreeMap<String, DateTime<Utc>>,
    pub pending: BTreeMap<String, usize>,
    pub groups: BTreeMap<String, AcceptanceGroup>,
    pub bindings: BTreeMap<String, AcceptanceBinding>,
    pub accepted_unbound: BTreeSet<String>,
    /// Closed provider names, not session IDs.
    pub sticky: BTreeMap<String, String>,
    pub outside_failure: bool,
    pub selection: WireSelectionSnapshot,
}

/// Mirrors the supplied read set with typed, privacy-safe per-row Accounts.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireSelectionSnapshot {
    pub entry: SelectionEntry,
    pub provider: Provider,
    pub parent: Option<String>,
    pub pinned: Option<String>,
    pub hold_allowed: bool,
    pub preference: Option<PreferenceInput>,
    pub root_hint: Option<RootHintInput>,
    pub candidates: Vec<SelectionRow>,
    pub source_accounts: Vec<SelectionRow>,
    pub account_order: Vec<String>,
    pub affinity: AffinityInput,
    pub metrics: MetricInputs,
    pub policy: SelectionPolicyInput,
    pub launches: BTreeMap<String, LaunchInput>,
    pub launch_aliases: BTreeSet<String>,
    pub unknown_cost: BTreeSet<String>,
    pub last_request_bytes: BTreeMap<String, u64>,
    pub outside_unknown: bool,
    pub outside_used: BTreeSet<String>,
    pub entitled_targets: Option<BTreeSet<String>>,
    pub return_guard: Option<SelectionReturnInput>,
}

type Mapper<'a> = dyn FnMut(IdKind, &str) -> Result<String, WireRefusal> + 'a;

fn count(n: usize) -> Result<(), WireRefusal> {
    if n > MAX_ROWS {
        Err(WireRefusal::Bounds)
    } else {
        Ok(())
    }
}

fn optional(v: &mut Option<String>, kind: IdKind, map: &mut Mapper<'_>) -> Result<(), WireRefusal> {
    if let Some(id) = v {
        *id = map(kind, id)?;
    }
    Ok(())
}

fn strings(v: &mut [String], kind: IdKind, map: &mut Mapper<'_>) -> Result<(), WireRefusal> {
    count(v.len())?;
    for id in v {
        *id = map(kind, id)?;
    }
    Ok(())
}

fn set(v: &mut BTreeSet<String>, kind: IdKind, map: &mut Mapper<'_>) -> Result<(), WireRefusal> {
    count(v.len())?;
    *v = v.iter().map(|id| map(kind, id)).collect::<Result<_, _>>()?;
    Ok(())
}

fn keys<T: Clone>(
    v: &mut BTreeMap<String, T>,
    kind: IdKind,
    map: &mut Mapper<'_>,
) -> Result<(), WireRefusal> {
    count(v.len())?;
    let mut mapped = BTreeMap::new();
    for (id, value) in v.iter() {
        if mapped.insert(map(kind, id)?, value.clone()).is_some() {
            return Err(WireRefusal::Collision);
        }
    }
    *v = mapped;
    Ok(())
}

impl SelectionRow {
    fn capture(c: &CandidateInput, map: &mut Mapper<'_>) -> Result<Self, WireRefusal> {
        Ok(Self {
            account: AccountFacts::capture(&c.account, map(IdKind::Account, &c.account.id)?)?,
            quota: field(c.quota.as_ref(), quota)?,
            inflight: c.inflight,
        })
    }

    fn restore(&self) -> Result<CandidateInput, WireRefusal> {
        wire_id(IdKind::Account, &self.account.id)?;
        // Validate even directly constructed DTOs through the same closed parser.
        let account = self.account.account()?;
        AccountFacts::capture(&account, account.id.clone())?;
        let quota = match &self.quota {
            Field::Missing => None,
            Field::Null => Some(Value::Null),
            Field::Value(q) => {
                let value = q.value();
                quota(&value)?;
                Some(value)
            }
        };
        Ok(CandidateInput {
            account,
            quota,
            inflight: self.inflight,
        })
    }
}

fn model(value: &Option<String>) -> Result<(), WireRefusal> {
    value.as_deref().map(Model::parse).transpose()?;
    Ok(())
}

fn remap_actual(actual: &mut SelectionDecision, map: &mut Mapper<'_>) -> Result<(), WireRefusal> {
    if let SelectionChoice::Selected { account } = &mut actual.choice {
        *account = map(IdKind::Account, account)?;
    }
    optional(&mut actual.budget_source, IdKind::Account, map)?;
    if let Some(e) = &mut actual.rebalance {
        e.key = map(IdKind::Session, &e.key)?;
        e.from = map(IdKind::Account, &e.from)?;
        e.to = map(IdKind::Account, &e.to)?;
        model(&e.model)?;
    }
    if let Some(e) = &mut actual.sole_target {
        e.from = map(IdKind::Account, &e.from)?;
        e.to = map(IdKind::Account, &e.to)?;
        model(&e.model)?;
    }
    Ok(())
}

fn remap(w: &mut WireSelectionInputs, map: &mut Mapper<'_>) -> Result<(), WireRefusal> {
    use IdKind::{Account as A, Session as S};
    optional(&mut w.key, S, map)?;
    keys(&mut w.accounts, A, map)?;
    for row in w.accounts.values_mut() {
        row.account.id = map(A, &row.account.id)?;
    }
    keys(&mut w.traffic, A, map)?;
    keys(&mut w.pending, A, map)?;
    keys(&mut w.groups, S, map)?;
    for g in w.groups.values_mut() {
        g.account = map(A, &g.account)?;
        set(&mut g.keys, S, map)?;
    }
    keys(&mut w.bindings, S, map)?;
    set(&mut w.accepted_unbound, A, map)?;
    for (provider, id) in &mut w.sticky {
        if !matches!(provider.as_str(), "claude" | "codex") {
            return Err(WireRefusal::Shape);
        }
        *id = map(A, id)?;
    }
    let s = &mut w.selection;
    optional(&mut s.parent, S, map)?;
    optional(&mut s.pinned, A, map)?;
    if let Some(p) = &mut s.preference {
        optional(&mut p.account, A, map)?;
        if let Some(coverage) = &mut p.coverage {
            set(coverage, A, map)?;
        }
    }
    if let Some(h) = &mut s.root_hint {
        h.key = map(S, &h.key)?;
    }
    count(s.candidates.len())?;
    count(s.source_accounts.len())?;
    for row in s.candidates.iter_mut().chain(&mut s.source_accounts) {
        row.account.id = map(A, &row.account.id)?;
    }
    strings(&mut s.account_order, A, map)?;
    let a = &mut s.affinity;
    keys(&mut a.bindings, S, map)?;
    for b in a.bindings.values_mut() {
        b.key = map(S, &b.key)?;
        b.account = map(A, &b.account)?;
    }
    keys(&mut a.groups, S, map)?;
    for g in a.groups.values_mut() {
        strings(&mut g.keys, S, map)?;
        g.account = map(A, &g.account)?;
    }
    count(a.lru.len())?;
    for (_, id) in &mut a.lru {
        *id = map(S, id)?;
    }
    set(&mut a.active, S, map)?;
    set(&mut a.pins, S, map)?;
    keys(&mut a.launch_retention, S, map)?;
    keys(&mut a.launch_anchors, S, map)?;
    for id in a.launch_anchors.values_mut() {
        *id = map(S, id)?;
    }
    let m = &mut s.metrics;
    if [
        m.config.gate_5h_pct,
        m.config.gate_weekly_pct,
        m.config.gate_weekly_near_reset_pct,
        m.config.inflight_allowance_pct,
        m.config.placement_allowance_pct,
    ]
    .iter()
    .any(|v| !v.is_finite())
    {
        return Err(WireRefusal::Shape);
    }
    keys(&mut m.recent, A, map)?;
    for recent in m.recent.values_mut() {
        count(recent.len())?;
        for (_, id) in recent {
            *id = map(S, id)?;
        }
    }
    set(&mut m.idle_placements, S, map)?;
    keys(&mut m.thresholds, A, map)?;
    keys(&mut m.overload_marks, S, map)?;
    for mark in m.overload_marks.values_mut() {
        mark.from = map(A, &mark.from)?;
        mark.target = map(A, &mark.target)?;
    }
    keys(&mut m.pacing, A, map)?;
    keys(&mut m.placements, A, map)?;
    for placement in m.placements.values_mut() {
        if placement.windows.len() > 32 {
            return Err(WireRefusal::Bounds);
        }
        for (name, row) in &mut placement.windows {
            Window::parse(name)?;
            // Placement values are raw quota windows, never arbitrary JSON.
            let projected = QuotaFacts {
                windows: Field::Value(vec![window(&row.value)?]),
                credits: Field::Missing,
                paid_serving_observed_at: Field::Missing,
            }
            .value();
            row.value = projected["windows"][0].clone();
        }
    }
    let p = &mut s.policy;
    optional(&mut p.scope.key, S, map)?;
    // Empty exact is the native no-key sentinel. Preserve it literally.
    if !p.scope.exact.is_empty() {
        p.scope.exact = map(S, &p.scope.exact)?;
    }
    optional(&mut p.scope.legacy, S, map)?;
    for rows in [&mut p.state, &mut p.witness] {
        keys(&mut rows.sessions, S, map)?;
        set(&mut rows.legacy_codex_roots, S, map)?;
        for row in rows.sessions.values_mut() {
            optional(&mut row.project, S, map)?;
            optional(&mut row.anchor, A, map)?;
            count(row.movements.len())?;
            for movement in &mut row.movements {
                movement.from = map(A, &movement.from)?;
                movement.to = map(A, &movement.to)?;
            }
        }
    }
    keys(&mut p.pool_moves, A, map)?;
    for times in p.pool_moves.values() {
        count(times.len())?;
    }
    keys(&mut p.target_pools, A, map)?;
    for pool in p.target_pools.values_mut() {
        *pool = map(A, pool)?;
    }
    keys(&mut s.launches, S, map)?;
    for launch in s.launches.values_mut() {
        launch.policy_key = map(S, &launch.policy_key)?;
    }
    set(&mut s.launch_aliases, S, map)?;
    set(&mut s.unknown_cost, S, map)?;
    keys(&mut s.last_request_bytes, S, map)?;
    set(&mut s.outside_used, A, map)?;
    if let Some(targets) = &mut s.entitled_targets {
        set(targets, A, map)?;
    }
    if let Some(guard) = &mut s.return_guard {
        if guard.history.len() > 128 {
            return Err(WireRefusal::Bounds);
        }
        for movement in &mut guard.history {
            movement.from = map(A, &movement.from)?;
            movement.to = map(A, &movement.to)?;
        }
    }
    Ok(())
}

/// Capture supplied facts and supplied output independently; never run selection.
pub(crate) fn capture(
    inputs: &DecisionInputs,
    actual: &SelectionDecision,
    mapper: impl FnMut(IdKind, &str) -> String,
) -> Result<SelectionRecord, WireRefusal> {
    let s = inputs.selection.as_deref().ok_or(WireRefusal::Shape)?;
    count(inputs.accounts.len())?;
    count(s.candidates.len())?;
    count(s.source_accounts.len())?;
    // Projection precedes serialization, including all three independent row sets.
    // Leave IDs raw until the single shared remapping pass below.
    let mut identity = |_: IdKind, id: &str| Ok(id.to_owned());
    let mut w = WireSelectionInputs {
        now: inputs.now,
        model: inputs.model.as_deref().map(Model::parse).transpose()?,
        key: inputs.key.clone(),
        accounts: inputs
            .accounts
            .iter()
            .map(|(id, c)| Ok((id.clone(), SelectionRow::capture(c, &mut identity)?)))
            .collect::<Result<_, WireRefusal>>()?,
        traffic: inputs.traffic.clone(),
        pending: inputs.pending.clone(),
        groups: inputs.groups.clone(),
        bindings: inputs.bindings.clone(),
        accepted_unbound: inputs.accepted_unbound.clone(),
        sticky: inputs.sticky.clone(),
        outside_failure: inputs.outside_failure,
        selection: WireSelectionSnapshot {
            entry: s.entry,
            provider: s.provider,
            parent: s.parent.clone(),
            pinned: s.pinned.clone(),
            hold_allowed: s.hold_allowed,
            preference: s.preference.clone(),
            root_hint: s.root_hint.clone(),
            candidates: s
                .candidates
                .iter()
                .map(|c| SelectionRow::capture(c, &mut identity))
                .collect::<Result<_, _>>()?,
            source_accounts: s
                .source_accounts
                .iter()
                .map(|c| SelectionRow::capture(c, &mut identity))
                .collect::<Result<_, _>>()?,
            account_order: s.account_order.clone(),
            affinity: s.affinity.clone(),
            metrics: s.metrics.clone(),
            policy: s.policy.clone(),
            launches: s.launches.clone(),
            launch_aliases: s.launch_aliases.clone(),
            unknown_cost: s.unknown_cost.clone(),
            last_request_bytes: s.last_request_bytes.clone(),
            outside_unknown: s.outside_unknown,
            outside_used: s.outside_used.clone(),
            entitled_targets: s.entitled_targets.clone(),
            return_guard: s.return_guard.clone(),
        },
    };
    let mut ids = Ids {
        mapper,
        cache: BTreeMap::new(),
        reverse: BTreeMap::new(),
    };
    let mut map = |kind, id: &str| ids.id(kind, id);
    remap(&mut w, &mut map)?;
    let mut actual = actual.clone();
    remap_actual(&mut actual, &mut map)?;
    let record = SelectionRecord {
        schema: SCHEMA,
        ordinal: 0,
        inputs: Some(w),
        actual,
        refusal: None,
    };
    validate_record(&record)?;
    Ok(record)
}

pub(crate) fn refused_record(
    actual: &SelectionDecision,
    refusal: WireRefusal,
    mapper: impl FnMut(IdKind, &str) -> String,
) -> Result<SelectionRecord, WireRefusal> {
    let mut ids = Ids {
        mapper,
        cache: BTreeMap::new(),
        reverse: BTreeMap::new(),
    };
    let mut actual = actual.clone();
    remap_actual(&mut actual, &mut |kind, id| ids.id(kind, id))?;
    let record = SelectionRecord {
        schema: SCHEMA,
        ordinal: 0,
        inputs: None,
        actual,
        refusal: Some(refusal),
    };
    validate_record(&record)?;
    Ok(record)
}

fn check(kind: IdKind, id: &str) -> Result<String, WireRefusal> {
    wire_id(kind, id)?;
    Ok(id.to_owned())
}

impl WireSelectionInputs {
    pub(crate) fn to_inputs(&self) -> Result<DecisionInputs, WireRefusal> {
        bounded(self)?;
        let mut w = self.clone();
        remap(&mut w, &mut check)?;
        // The only open JSON leaf is a placement window. A reader must reject
        // unprojected fields rather than silently accepting arbitrary payloads.
        if serde_json::to_value(self).map_err(|_| WireRefusal::Shape)?
            != serde_json::to_value(&w).map_err(|_| WireRefusal::Shape)?
        {
            return Err(WireRefusal::Shape);
        }
        let s = w.selection;
        Ok(DecisionInputs {
            now: w.now,
            model: w.model.map(|m| m.text().to_owned()),
            key: w.key,
            from: None,
            to: String::new(),
            permit: None,
            accounts: w
                .accounts
                .iter()
                .map(|(id, row)| Ok((id.clone(), row.restore()?)))
                .collect::<Result<_, WireRefusal>>()?,
            traffic: w.traffic,
            pending: w.pending,
            groups: w.groups,
            bindings: w.bindings,
            accepted_unbound: w.accepted_unbound,
            sticky: w.sticky,
            outside_failure: w.outside_failure,
            selection: Some(Box::new(SelectionInputs {
                entry: s.entry,
                provider: s.provider,
                parent: s.parent,
                pinned: s.pinned,
                hold_allowed: s.hold_allowed,
                preference: s.preference,
                root_hint: s.root_hint,
                candidates: s
                    .candidates
                    .iter()
                    .map(SelectionRow::restore)
                    .collect::<Result<_, _>>()?,
                source_accounts: s
                    .source_accounts
                    .iter()
                    .map(SelectionRow::restore)
                    .collect::<Result<_, _>>()?,
                account_order: s.account_order,
                affinity: s.affinity,
                metrics: s.metrics,
                policy: s.policy,
                launches: s.launches,
                launch_aliases: s.launch_aliases,
                unknown_cost: s.unknown_cost,
                last_request_bytes: s.last_request_bytes,
                outside_unknown: s.outside_unknown,
                outside_used: s.outside_used,
                entitled_targets: s.entitled_targets,
                return_guard: s.return_guard,
            })),
        })
    }
}

pub(crate) fn validate_record(record: &SelectionRecord) -> Result<(), WireRefusal> {
    if record.schema != SCHEMA {
        return Err(WireRefusal::Schema);
    }
    bounded(record)?;
    remap_actual(&mut record.actual.clone(), &mut check)?;
    match (&record.inputs, record.refusal) {
        (Some(inputs), None) => {
            inputs.to_inputs()?;
        }
        (None, Some(_)) => {}
        _ => return Err(WireRefusal::Shape),
    }
    Ok(())
}

pub(crate) fn validate(record: &SelectionRecord) -> Result<(), WireRefusal> {
    validate_record(record)
}

/// Re-evaluate only the restored input, never use recorded output as an oracle.
pub(crate) fn replay(record: &SelectionRecord) -> Result<SelectionDecision, WireRefusal> {
    validate_record(record)?;
    if let Some(refusal) = record.refusal {
        return Err(refusal);
    }
    let inputs = record
        .inputs
        .as_ref()
        .ok_or(WireRefusal::Shape)?
        .to_inputs()?;
    kernel::selection::select(&inputs)
        .map(|evaluation| evaluation.decision)
        .map_err(|_| WireRefusal::Shape)
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
