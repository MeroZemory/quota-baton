//! C7 placement and schema-1 affinity persistence. Callers filter tried accounts,
//! credentials, probes and volatile failures, but retain quota cooling for holds.
use crate::{
    accounts::{Account, Health, HealthReason, HealthSignal, Provider, UserState},
    config::RoutingConfig,
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const CAPACITY: usize = 20_000;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub account: Account,
    pub quota: Option<Value>,
    pub inflight: usize,
}
#[derive(Debug)]
pub enum Decision {
    Selected {
        account: String,
        lease: BindingLease,
        moves: Vec<MoveEvent>,
    },
    Hold {
        until: DateTime<Utc>,
    },
    None,
}
#[derive(Debug, PartialEq, Eq)]
pub enum PreviewDecision {
    Selected { account: String },
    Hold { until: DateTime<Utc> },
    None,
}
#[derive(Clone, Debug, Serialize)]
pub struct MoveEvent {
    pub key: String,
    pub from: Option<String>,
    pub to: String,
    pub reason: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Binding {
    key: String,
    account: String,
    last_used: DateTime<Utc>,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    schema: u32,
    bindings: Vec<Binding>,
}
#[derive(Clone, Default)]
struct Data {
    bindings: BTreeMap<String, Binding>,
    active: BTreeMap<String, usize>,
    pins: BTreeMap<String, usize>,
    pending: BTreeMap<String, usize>,
    traffic: BTreeMap<String, DateTime<Utc>>,
    sticky: BTreeMap<String, String>,
    after_move: BTreeSet<String>,
    hold_timeout: BTreeSet<String>,
    dirty: bool,
    revision: u64,
}
impl Data {
    fn changed(&mut self) {
        self.dirty = true;
        self.revision = self.revision.wrapping_add(1);
    }
}
pub struct BindingTable {
    path: PathBuf,
    config: RoutingConfig,
    data: Mutex<Data>,
    writer: Mutex<()>,
}

/// A reservation belongs to the actual upstream account, even after a forced
/// remap. Completion never restores an old account into the binding table.
pub struct BindingLease {
    table: Arc<BindingTable>,
    group: Option<String>,
    account: String,
    pub after_move: bool,
    defer: bool,
}
impl std::fmt::Debug for BindingLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindingLease")
            .field("account", &self.account)
            .field("after_move", &self.after_move)
            .finish()
    }
}
impl BindingLease {
    /// Upgrade handshakes have no cache usage. Carry this marker to response.create.
    pub fn defer_after_move(&mut self) {
        self.defer = true;
    }
}
impl Drop for BindingLease {
    fn drop(&mut self) {
        let mut d = self.table.data.lock().unwrap_or_else(|e| e.into_inner());
        decrement(&mut d.pending, &self.account);
        let now = Utc::now();
        d.traffic.insert(self.account.clone(), now);
        if let Some(group) = &self.group {
            decrement(&mut d.active, group);
            touch_group(&mut d, group, now);
            if self.defer && self.after_move {
                d.after_move.insert(group.clone());
            }
        }
    }
}
pub struct SocketPin {
    table: Arc<BindingTable>,
    group: Option<String>,
}
impl Drop for SocketPin {
    fn drop(&mut self) {
        if let Some(group) = &self.group {
            decrement(
                &mut self
                    .table
                    .data
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pins,
                group,
            );
        }
    }
}
fn group(key: &str) -> &str {
    if key.starts_with("claude:") {
        key.split_once(":agent:").map_or(key, |(root, _)| root)
    } else {
        key
    }
}
fn decrement(map: &mut BTreeMap<String, usize>, key: &str) {
    if let Some(n) = map.get_mut(key) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            map.remove(key);
        }
    }
}
fn protected(d: &Data, g: &str) -> bool {
    d.active.contains_key(g) || d.pins.contains_key(g)
}
fn touch_group(d: &mut Data, g: &str, now: DateTime<Utc>) {
    let mut touched = false;
    for b in d.bindings.values_mut().filter(|b| group(&b.key) == g) {
        b.last_used = b.last_used.max(now);
        touched = true;
    }
    if touched {
        d.changed();
    }
}
fn snapshot(d: &Data) -> Snapshot {
    Snapshot {
        schema: 1,
        bindings: d.bindings.values().cloned().collect(),
    }
}
fn decode(value: Value) -> Result<Data> {
    let state: Snapshot = serde_json::from_value(value)?;
    if state.schema != 1 {
        bail!("unsupported binding schema");
    }
    let mut d = Data::default();
    let mut groups = BTreeMap::new();
    for b in state.bindings {
        if b.key.is_empty() || b.account.is_empty() || d.bindings.contains_key(&b.key) {
            bail!("invalid binding");
        }
        if let Some(previous) = groups.insert(group(&b.key).to_owned(), b.account.clone())
            && previous != b.account
        {
            bail!("inconsistent binding group");
        }
        d.traffic
            .entry(b.account.clone())
            .and_modify(|last| *last = (*last).max(b.last_used))
            .or_insert(b.last_used);
        d.bindings.insert(b.key.clone(), b);
    }
    Ok(d)
}
impl BindingTable {
    pub fn new(home: &Path, config: RoutingConfig) -> Result<Arc<Self>> {
        config.validate()?;
        let path = home.join("state/bindings.json");
        fs::create_dir_all(path.parent().context("binding state directory")?)?;
        let data = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(decode)
            {
                Ok(data) => data,
                Err(_) => {
                    let quarantine = path.with_file_name(format!(
                        "bindings.json.corrupt-{}-{:08x}",
                        Utc::now().timestamp_millis(),
                        rand::random::<u32>()
                    ));
                    fs::rename(&path, quarantine).context("quarantine corrupt bindings")?;
                    tracing::warn!("Corrupt bindings quarantined; starting empty");
                    Data::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e.into()),
        };
        let table = Arc::new(Self {
            path,
            config,
            data: Mutex::new(data),
            writer: Mutex::new(()),
        });
        table.prune(&mut table.data.lock().unwrap(), Utc::now());
        let weak = Arc::downgrade(&table);
        std::thread::Builder::new()
            .name("qb-bindings".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    let Some(table) = weak.upgrade() else {
                        break;
                    };
                    if let Err(e) = table.flush() {
                        tracing::warn!(error = %e, "Binding write-behind failed; will retry");
                    }
                }
            })?;
        Ok(table)
    }
    fn prune(&self, d: &mut Data, now: DateTime<Utc>) {
        let mut last = BTreeMap::<String, DateTime<Utc>>::new();
        for b in d.bindings.values() {
            let t = last.entry(group(&b.key).into()).or_insert(b.last_used);
            *t = (*t).max(b.last_used);
        }
        let mut remove = BTreeSet::new();
        for (g, time) in &last {
            if !protected(d, g) && idle(now, *time) >= self.config.session_binding_max_secs {
                remove.insert(g.clone());
            }
        }
        let mut size = d
            .bindings
            .values()
            .filter(|b| !remove.contains(group(&b.key)))
            .count();
        let mut lru: Vec<_> = last
            .into_iter()
            .filter(|(g, _)| !protected(d, g) && !remove.contains(g))
            .collect();
        lru.sort_by_key(|(g, t)| (*t, g.clone()));
        for (g, _) in lru {
            if size <= CAPACITY {
                break;
            }
            size -= d.bindings.values().filter(|b| group(&b.key) == g).count();
            remove.insert(g);
        }
        if !remove.is_empty() {
            d.bindings.retain(|k, _| !remove.contains(group(k)));
            d.after_move.retain(|g| !remove.contains(g));
            d.hold_timeout.retain(|g| !remove.contains(g));
            d.changed();
        }
    }
    pub fn count(&self) -> usize {
        let mut d = self.data.lock().unwrap();
        self.prune(&mut d, Utc::now());
        d.bindings.len()
    }
    pub(crate) fn has_traffic(&self, id: &str) -> bool {
        let d = self.data.lock().unwrap();
        d.traffic.contains_key(id) || d.pending.get(id).is_some_and(|count| *count > 0)
    }
    pub fn account_stats(&self, id: &str) -> (usize, Option<DateTime<Utc>>) {
        let mut d = self.data.lock().unwrap();
        self.prune(&mut d, Utc::now());
        let entries: Vec<_> = d.bindings.values().filter(|b| b.account == id).collect();
        (entries.len(), entries.iter().map(|b| b.last_used).max())
    }
    pub fn pending_count(&self, id: &str) -> usize {
        self.data
            .lock()
            .unwrap()
            .pending
            .get(id)
            .copied()
            .unwrap_or(0)
    }
    pub fn current(&self, key: Option<&str>) -> Option<String> {
        let mut d = self.data.lock().unwrap();
        self.prune(&mut d, Utc::now());
        key.and_then(|k| {
            d.bindings
                .get(k)
                .or_else(|| d.bindings.values().find(|b| group(&b.key) == group(k)))
        })
        .map(|b| b.account.clone())
    }
    pub fn touch(&self, key: Option<&str>) {
        if let Some(k) = key {
            touch_group(&mut self.data.lock().unwrap(), group(k), Utc::now());
        }
    }
    pub fn mark_reset_hold_timeout(&self, key: Option<&str>) {
        if let Some(k) = key {
            self.data
                .lock()
                .unwrap()
                .hold_timeout
                .insert(group(k).into());
        }
    }
    pub fn pin(self: &Arc<Self>, key: Option<&str>) -> SocketPin {
        let g = key.map(|k| group(k).to_owned());
        if let Some(g) = &g {
            *self.data.lock().unwrap().pins.entry(g.clone()).or_default() += 1;
        }
        SocketPin {
            table: self.clone(),
            group: g,
        }
    }
    pub fn begin_request(self: &Arc<Self>, key: Option<&str>, account: &str) -> BindingLease {
        self.reserve(&mut self.data.lock().unwrap(), key, account)
    }
    fn reserve(self: &Arc<Self>, d: &mut Data, key: Option<&str>, account: &str) -> BindingLease {
        *d.pending.entry(account.into()).or_default() += 1;
        d.traffic.insert(account.into(), Utc::now());
        let g = key.map(|k| group(k).to_owned());
        let after_move = if let Some(g) = &g {
            *d.active.entry(g.clone()).or_default() += 1;
            d.after_move.remove(g)
        } else {
            false
        };
        BindingLease {
            table: self.clone(),
            group: g,
            account: account.into(),
            after_move,
            defer: false,
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn preview(
        &self,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        _request_bytes: u64,
    ) -> PreviewDecision {
        let mut d = self.data.lock().unwrap().clone();
        self.prune(&mut d, Utc::now());
        self.select(
            &d,
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            Utc::now(),
        )
        .0
    }
    #[allow(clippy::too_many_arguments)]
    pub fn choose(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
    ) -> Decision {
        let mut d = self.data.lock().unwrap();
        let now = Utc::now();
        self.prune(&mut d, now);
        let (choice, reason) =
            self.select(&d, provider, key, parent, model, candidates, pinned, now);
        let account = match choice {
            PreviewDecision::Hold { until } => return Decision::Hold { until },
            PreviewDecision::None => return Decision::None,
            PreviewDecision::Selected { account } => account,
        };
        let mut moves = Vec::new();
        if let Some(k) = key {
            let g = group(k);
            let reason = if d.hold_timeout.contains(g) {
                "reset_hold_timeout"
            } else {
                reason
            };
            let mut keys: BTreeSet<String> = d
                .bindings
                .keys()
                .filter(|k| group(k) == g)
                .cloned()
                .collect();
            keys.insert(k.into());
            if let Some(p) = parent
                && !d.bindings.contains_key(p)
                && group(p) == g
            {
                keys.insert(p.into());
            }
            for k in keys {
                let from = d.bindings.get(&k).map(|b| b.account.clone());
                if from.as_deref() != Some(&account) {
                    moves.push(MoveEvent {
                        key: k.clone(),
                        from: from.clone(),
                        to: account.clone(),
                        reason: if from.is_none() { "new" } else { reason }.into(),
                        bytes: request_bytes,
                    });
                    if from.is_some() {
                        d.after_move.insert(g.into());
                    }
                }
                let last_used = d.bindings.get(&k).map_or(now, |b| b.last_used);
                d.bindings.insert(
                    k.clone(),
                    Binding {
                        key: k,
                        account: account.clone(),
                        last_used,
                    },
                );
            }
            if moves.iter().any(|m| m.from.is_some()) {
                d.hold_timeout.remove(g);
            }
            d.changed();
        } else {
            d.sticky.insert(provider.as_str().into(), account.clone());
        }
        let lease = self.reserve(&mut d, key, &account);
        self.prune(&mut d, now);
        Decision::Selected {
            account,
            lease,
            moves,
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn select(
        &self,
        d: &Data,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        now: DateTime<Utc>,
    ) -> (PreviewDecision, &'static str) {
        let entries: Vec<_> = candidates
            .iter()
            .filter(|c| c.account.provider == provider)
            .collect();
        let eligible = |c: &Candidate| {
            effective_account(c, d, &self.config, model, now).eligible_at(model, now)
        };
        if let Some(id) = pinned
            && entries.iter().any(|c| c.account.id == id && eligible(c))
        {
            return (PreviewDecision::Selected { account: id.into() }, "forced");
        }
        let bound = key.and_then(|k| {
            d.bindings
                .get(k)
                .or_else(|| d.bindings.values().find(|b| group(&b.key) == group(k)))
        });
        let mut reason = "new";
        if let Some(b) = bound {
            reason = "forced";
            if let Some(c) = entries.iter().find(|c| c.account.id == b.account) {
                let effective = effective_account(c, d, &self.config, model, now);
                if !d.hold_timeout.contains(group(&b.key))
                    && effective.user_state == UserState::Active
                    && let Health::Cooling {
                        reason: HealthReason::Quota,
                        until,
                        scope,
                    } = &effective.health
                    && *until > now
                    && (scope == "account"
                        || scope
                            .strip_prefix("model:")
                            .is_some_and(|m| model == Some(m)))
                {
                    let reset = effective
                        .quota_cooling
                        .as_ref()
                        .map_or(*until, |q| q.reset_at);
                    if reset > now
                        && (reset - now).to_std().is_ok_and(|wait| {
                            wait <= Duration::from_secs(self.config.reset_hold_secs)
                        })
                    {
                        return (PreviewDecision::Hold { until: reset }, reason);
                    }
                }
                if effective.eligible_at(model, now) {
                    let g = group(&b.key);
                    let last = d
                        .bindings
                        .values()
                        .filter(|b| group(&b.key) == g)
                        .map(|b| b.last_used)
                        .max()
                        .unwrap_or(b.last_used);
                    let cold =
                        !protected(d, g) && idle(now, last) >= self.config.rebalance_idle_secs;
                    if !cold || metrics(c, d, &self.config, now).gate {
                        return (
                            PreviewDecision::Selected {
                                account: b.account.clone(),
                            },
                            reason,
                        );
                    }
                    reason = "cold_rebalance";
                }
            }
        } else if let Some(p) = parent.and_then(|p| d.bindings.get(p)) {
            if entries
                .iter()
                .any(|c| c.account.id == p.account && eligible(c))
            {
                return (
                    PreviewDecision::Selected {
                        account: p.account.clone(),
                    },
                    reason,
                );
            }
        } else if key.is_none()
            && let Some(id) = d.sticky.get(provider.as_str())
            && entries.iter().any(|c| &c.account.id == id && eligible(c))
        {
            return (
                PreviewDecision::Selected {
                    account: id.clone(),
                },
                reason,
            );
        }
        let eligible: Vec<_> = entries
            .into_iter()
            .filter(|c| !c.account.drain && eligible(c))
            .map(|c| (c, metrics(c, d, &self.config, now)))
            .collect();
        let mut gated: Vec<_> = eligible.iter().filter(|(_, m)| m.gate).collect();
        // Anchor the first reset cohort; pairwise approximate comparisons are
        // non-transitive and cannot be used as a sorting comparator.
        let anchor = gated.iter().filter_map(|(_, m)| m.weekly).min();
        gated.sort_by(|(a, x), (b, y)| {
            let cohort = |t: Option<DateTime<Utc>>| match (anchor, t) {
                (Some(start), Some(t)) if (t - start).num_seconds() <= 10800 => (0, None),
                (_, Some(t)) => (1, Some(t)),
                (_, None) => (2, None),
            };
            cohort(x.weekly)
                .cmp(&cohort(y.weekly))
                .then_with(|| x.five.is_none().cmp(&y.five.is_none()))
                .then_with(|| x.five.cmp(&y.five))
                .then_with(|| x.usage.total_cmp(&y.usage))
                .then_with(|| a.account.id.cmp(&b.account.id))
        });
        let chosen = gated.first().map(|(c, _)| *c).or_else(|| {
            eligible
                .iter()
                .min_by(|(a, x), (b, y)| {
                    x.usage
                        .total_cmp(&y.usage)
                        .then_with(|| a.account.id.cmp(&b.account.id))
                })
                .map(|(c, _)| *c)
        });
        (
            chosen.map_or(PreviewDecision::None, |c| PreviewDecision::Selected {
                account: c.account.id.clone(),
            }),
            reason,
        )
    }
    pub fn export(&self) -> Result<Value> {
        let state = snapshot(&self.data.lock().unwrap());
        Ok(serde_json::to_value(state)?)
    }
    /// Takeover only: reject import while any request or socket owns the table.
    pub fn import(&self, value: Value) -> Result<()> {
        let mut next = decode(value)?;
        let mut d = self.data.lock().unwrap();
        if !d.active.is_empty() || !d.pins.is_empty() || !d.pending.is_empty() {
            bail!("cannot import active binding table");
        }
        self.prune(&mut next, Utc::now());
        // Import must advance the existing table's revision, rather than use
        // the decoded default: a flush may already own an older snapshot.
        next.revision = d.revision;
        next.changed();
        *d = next;
        Ok(())
    }
    pub fn flush(&self) -> Result<()> {
        self.flush_with(atomic_write)
    }
    fn flush_with(&self, write: impl FnOnce(&Path, &[u8]) -> std::io::Result<()>) -> Result<()> {
        // Acquire before taking the snapshot so an older flush can never
        // replace a newer file. Selectors and completion never take this lock.
        let _writer = self.writer.lock().unwrap();
        let (state, revision) = {
            let mut d = self.data.lock().unwrap();
            self.prune(&mut d, Utc::now());
            if !d.dirty {
                return Ok(());
            }
            (snapshot(&d), d.revision)
        };
        write(&self.path, &serde_json::to_vec(&state)?)?;
        let mut d = self.data.lock().unwrap();
        if d.revision == revision {
            d.dirty = false;
        }
        Ok(())
    }
}
impl Drop for BindingTable {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            tracing::warn!(error = %e, "Final binding flush failed");
        }
    }
}
fn idle(now: DateTime<Utc>, last: DateTime<Utc>) -> u64 {
    (now - last).num_seconds().max(0) as u64
}
struct Metrics {
    gate: bool,
    weekly: Option<DateTime<Utc>>,
    five: Option<DateTime<Utc>>,
    usage: f64,
}
fn date(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}
/// Re-evaluate C5 against reservations that arrived since the last persisted
/// snapshot. The clone is request-local: selection does not persist health.
fn effective_account(
    c: &Candidate,
    d: &Data,
    cfg: &RoutingConfig,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Account {
    let mut account = c.account.clone();
    if account.eligible_at(model, now)
        && let Some(snapshot) = &c.quota
    {
        // An expired cooldown or a permission cooldown for another model must
        // not prevent C5 from checking this request's independent quota limit.
        account.health = Health::Ready;
        account.quota_cooling = None;
        let pending = d
            .pending
            .get(&account.id)
            .copied()
            .unwrap_or(0)
            .max(c.inflight);
        let mut snapshot = snapshot.clone();
        if (d.traffic.contains_key(&account.id) || pending > 0)
            && let Some(windows) = snapshot["windows"].as_array_mut()
        {
            windows.retain(|w| {
                date(&w["observed_at"]).is_some_and(|t| (now - t).num_seconds() <= 900)
            });
            if windows.is_empty() {
                return account;
            }
        }
        // Persisted rejection flags belong to the observation, not this projection.
        snapshot["signals"] = Value::Null;
        account.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot,
                model: model.map(str::to_owned),
                projected_allowance: cfg.inflight_allowance_pct * pending as f64,
            },
            now,
        );
    }
    account
}
fn metrics(c: &Candidate, d: &Data, cfg: &RoutingConfig, now: DateTime<Utc>) -> Metrics {
    let mut m = Metrics {
        gate: false,
        weekly: None,
        five: None,
        usage: f64::INFINITY,
    };
    let Some(windows) = c.quota.as_ref().and_then(|q| q["windows"].as_array()) else {
        return m;
    };
    if windows.is_empty() {
        return m;
    }
    m.gate = true;
    m.usage = 0.0;
    let pending = d
        .pending
        .get(&c.account.id)
        .copied()
        .unwrap_or(0)
        .max(c.inflight);
    let traffic = d.traffic.contains_key(&c.account.id) || pending > 0;
    for w in windows {
        let reset = date(&w["reset_at"]).filter(|t| *t > now);
        let name = w["name"].as_str().unwrap_or("");
        let weekly = name == "7d" || name.starts_with("7d_") || name.starts_with("seven_day");
        let five = name == "5h" || name == "five_hour";
        if weekly {
            m.weekly = match (m.weekly, reset) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        if five {
            m.five = match (m.five, reset) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        let stale =
            traffic && date(&w["observed_at"]).is_none_or(|t| (now - t).num_seconds() > 900);
        let usage = if stale {
            f64::INFINITY
        } else {
            w["used_pct"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map_or(f64::INFINITY, |n| {
                    n + cfg.inflight_allowance_pct * pending as f64
                })
        };
        m.usage = m.usage.max(usage);
        let near = reset.is_some_and(|r| (r - now).num_seconds() as u64 <= cfg.near_reset_secs);
        let gate = if weekly {
            if near {
                cfg.gate_weekly_near_reset_pct
            } else {
                cfg.gate_weekly_pct
            }
        } else {
            cfg.gate_5h_pct
        };
        if usage >= gate {
            m.gate = false;
        }
    }
    m
}
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{:032x}.tmp", rand::random::<u128>()));
    let result = (|| {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::{
                Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW},
                core::PCWSTR,
            };
            let src: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
            let dst: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(src.as_ptr()),
                    PCWSTR(dst.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING,
                )
            }
            .map_err(std::io::Error::other)
        }
        #[cfg(not(windows))]
        {
            fs::rename(&tmp, path)
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Identity, Source, TokenMeta};
    use chrono::Duration as Delta;
    use serde_json::json;

    fn candidate(id: &str, used: f64, weekly_hours: i64, five_hours: i64) -> Candidate {
        let now = Utc::now();
        Candidate {
            account: Account {
                schema: 1,
                id: id.into(),
                provider: Provider::Claude,
                label: id.into(),
                email: "synthetic@example.invalid".into(),
                plan: None,
                identity: Identity::Claude {
                    account_uuid: id.into(),
                    organization_uuid: None,
                    email: "synthetic@example.invalid".into(),
                },
                source: Source::Oauth,
                user_state: UserState::Active,
                health: Health::Ready,
                auth_failures: 0,
                auth_probe_pending: false,
                quota_cooling: None,
                drain: false,
                allow_credits: false,
                generation: 0,
                token_meta: TokenMeta::default(),
            },
            quota: Some(json!({"windows":[
                {"name":"5h","used_pct":used,"reset_at":now+Delta::hours(five_hours),"observed_at":now},
                {"name":"7d","used_pct":used,"reset_at":now+Delta::hours(weekly_hours),"observed_at":now}
            ]})),
            inflight: 0,
        }
    }
    fn fixture_dir() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/bindings-tests");
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("bindings-")
            .tempdir_in(root)
            .unwrap()
    }
    fn table() -> (tempfile::TempDir, Arc<BindingTable>) {
        let dir = fixture_dir();
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        (dir, t)
    }
    fn choose(
        t: &Arc<BindingTable>,
        key: Option<&str>,
        cs: &[Candidate],
    ) -> (String, BindingLease, Vec<MoveEvent>) {
        match t.choose(Provider::Claude, key, None, None, cs, None, 42) {
            Decision::Selected {
                account,
                lease,
                moves,
            } => (account, lease, moves),
            other => panic!("{other:?}"),
        }
    }
    fn preview(t: &BindingTable, cs: &[Candidate]) -> String {
        match t.preview(Provider::Claude, None, None, None, cs, None, 0) {
            PreviewDecision::Selected { account } => account,
            other => panic!("{other:?}"),
        }
    }
    fn age(t: &BindingTable, key: &str, seconds: i64) {
        let mut d = t.data.lock().unwrap();
        for b in d
            .bindings
            .values_mut()
            .filter(|b| group(&b.key) == group(key))
        {
            b.last_used = Utc::now() - Delta::seconds(seconds);
        }
    }
    #[test]
    fn gates_project_all_windows_and_near_reset() {
        let (_dir, t) = table();
        let d = Data::default();
        let now = Utc::now();
        let mut c = candidate("a", 89.0, 8, 2);
        assert!(metrics(&c, &d, &t.config, now).gate);
        c.inflight = 1;
        assert!(!metrics(&c, &d, &t.config, now).gate);
        c.inflight = 0;
        c.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(97);
        assert!(!metrics(&c, &d, &t.config, now).gate);
        c.quota.as_mut().unwrap()["windows"][1]["reset_at"] = json!(now + Delta::hours(4));
        assert!(metrics(&c, &d, &t.config, now).gate);
        c.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(98);
        assert!(!metrics(&c, &d, &t.config, now).gate);
    }
    #[test]
    fn reservation_uses_max_not_sum_and_burst_spreads() {
        let (_dir, t) = table();
        let mut cs = vec![candidate("a", 89.0, 8, 1), candidate("b", 10.0, 30, 2)];
        let (id, lease, _) = choose(&t, Some("claude:one"), &cs);
        assert_eq!(id, "a");
        cs[0].inflight = 1;
        assert_eq!(
            metrics(&cs[0], &t.data.lock().unwrap(), &t.config, Utc::now()).usage,
            90.0
        );
        let (id, second, _) = choose(&t, Some("claude:two"), &cs);
        assert_eq!(id, "b");
        drop(lease);
        drop(second);
        assert!(t.data.lock().unwrap().pending.is_empty());
    }
    #[test]
    fn ordering_anchored_weekly_cohort_then_five_usage_id() {
        let (_dir, t) = table();
        let a = candidate("a", 10.0, 8, 4);
        let b = candidate("b", 20.0, 10, 2);
        let c = candidate("c", 1.0, 12, 1);
        assert_eq!(preview(&t, &[c.clone(), b.clone(), a.clone()]), "b");
        assert_eq!(preview(&t, &[a.clone(), c, b]), "b");
        let mut b = a.clone();
        b.account.id = "b".into();
        b.quota.as_mut().unwrap()["windows"][0]["used_pct"] = json!(5);
        b.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(5);
        assert_eq!(preview(&t, &[a.clone(), b.clone()]), "b");
        b.quota = a.quota.clone();
        assert_eq!(preview(&t, &[b.clone(), a.clone()]), "a");
        b.quota.as_mut().unwrap()["windows"][0]["reset_at"] = Value::Null;
        assert_eq!(preview(&t, &[b, a]), "a");
    }
    #[test]
    fn unknown_and_traffic_stale_sort_last_in_fallback() {
        let (_dir, t) = table();
        let mut unknown = candidate("a", 0.0, 8, 1);
        unknown.quota = None;
        let known = candidate("b", 95.0, 20, 2);
        assert_eq!(preview(&t, &[unknown, known.clone()]), "b");
        let mut stale = candidate("a", 0.0, 8, 1);
        for w in stale.quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            w["observed_at"] = json!(Utc::now() - Delta::minutes(16));
        }
        assert_eq!(preview(&t, &[stale.clone(), known.clone()]), "a");
        let lease = t.begin_request(None, "a");
        drop(lease);
        assert_eq!(preview(&t, &[stale, known]), "b");
    }
    #[test]
    fn fallback_headroom_and_drain_exclusion() {
        let (_dir, t) = table();
        let mut a = candidate("a", 91.0, 8, 1);
        let b = candidate("b", 95.0, 30, 2);
        assert_eq!(preview(&t, &[a.clone(), b.clone()]), "a");
        a.account.drain = true;
        assert_eq!(preview(&t, &[a, b]), "b");
    }
    #[test]
    fn warm_binding_and_unbound_stay_despite_thresholds_and_drain() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        drop(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]));
        drop(choose(&t, None, &[a.clone(), b.clone()]));
        a.account.drain = true;
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(99);
        }
        assert_eq!(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]).0, "a");
        assert_eq!(choose(&t, None, &[a, b]).0, "a");
    }
    #[test]
    fn cold_stays_when_gate_passes_and_moves_only_when_gate_fails() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 30, 2);
        let b = candidate("b", 5.0, 8, 1);
        drop(choose(&t, Some("claude:s"), &[a.clone()]));
        age(&t, "claude:s", 3601);
        assert_eq!(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]).0, "a");
        age(&t, "claude:s", 3601);
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(91);
        }
        let (id, lease, moves) = choose(&t, Some("claude:s"), &[a, b]);
        assert_eq!(id, "b");
        assert!(lease.after_move);
        assert_eq!(moves[0].reason, "cold_rebalance");
    }
    #[test]
    fn hold_bound_and_timeout_reason() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        drop(choose(&t, Some("claude:s"), &[a.clone()]));
        let until = Utc::now() + Delta::seconds(30);
        a.account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until,
            scope: "account".into(),
        };
        assert!(
            matches!(t.choose(Provider::Claude, Some("claude:s"), None, None, &[a.clone(), b.clone()], None, 1), Decision::Hold { until: u } if u == until)
        );
        t.mark_reset_hold_timeout(Some("claude:s"));
        let (id, _, moves) = choose(&t, Some("claude:s"), std::slice::from_ref(&b));
        assert_eq!(id, "b");
        assert_eq!(moves[0].reason, "reset_hold_timeout");
        drop(choose(
            &t,
            Some("claude:far"),
            &[candidate("a", 10.0, 8, 1)],
        ));
        a.account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: Utc::now() + Delta::seconds(120),
            scope: "account".into(),
        };
        assert_eq!(choose(&t, Some("claude:far"), &[a, b]).0, "b");
    }
    #[test]
    fn groups_inherit_move_atomically_and_old_completion_does_not_undo() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        let (_, old, _) = choose(&t, Some("claude:s"), &[a.clone()]);
        let result = t.choose(
            Provider::Claude,
            Some("claude:s:agent:c"),
            Some("claude:s"),
            None,
            &[a.clone(), b.clone()],
            None,
            9,
        );
        assert!(matches!(&result, Decision::Selected { account, .. } if account == "a"));
        drop(result);
        a.account.user_state = UserState::Paused;
        let (_, lease, moves) = choose(&t, Some("claude:s:agent:c"), &[a, b]);
        assert_eq!(moves.len(), 2);
        assert!(moves.iter().all(|m| m.to == "b" && m.bytes == 42));
        assert!(lease.after_move);
        drop(old);
        assert_eq!(t.current(Some("claude:s")).as_deref(), Some("b"));
    }
    #[test]
    fn pins_and_active_requests_protect_cold_expiry_and_eviction() {
        let (_dir, t) = table();
        let a = candidate("a", 10.0, 8, 1);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        let pin = t.pin(Some("claude:s"));
        age(&t, "claude:s", 700000);
        assert_eq!(t.current(Some("claude:s")).as_deref(), Some("a"));
        {
            let mut d = t.data.lock().unwrap();
            for i in 0..CAPACITY {
                let key = format!("claude:other{i}");
                d.bindings.insert(
                    key.clone(),
                    Binding {
                        key,
                        account: "a".into(),
                        last_used: Utc::now(),
                    },
                );
            }
            t.prune(&mut d, Utc::now());
            assert_eq!(d.bindings.len(), CAPACITY);
            assert!(d.bindings.contains_key("claude:s"));
        }
        drop(pin);
        assert_eq!(t.current(Some("claude:s")), None);
        let (_, lease, _) = choose(&t, Some("claude:active"), &[a]);
        age(&t, "claude:active", 700000);
        assert!(t.current(Some("claude:active")).is_some());
        drop(lease);
        assert!(t.current(Some("claude:active")).is_some());
        drop(t.pin(None));
    }
    #[test]
    fn preview_does_not_mutate_and_pin_never_overrides_health() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        let before = t.export().unwrap();
        assert!(matches!(
            t.preview(
                Provider::Claude,
                Some("claude:s"),
                None,
                None,
                &[a.clone()],
                None,
                0
            ),
            PreviewDecision::Selected { .. }
        ));
        assert_eq!(before, t.export().unwrap());
        assert!(t.data.lock().unwrap().pending.is_empty());
        a.account.user_state = UserState::Paused;
        assert!(
            matches!(t.preview(Provider::Claude, None, None, None, &[a, b], Some("a"), 0), PreviewDecision::Selected { account } if account == "b")
        );
    }
    #[test]
    fn model_cooling_filters_only_known_affected_model() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        a.account.health = Health::Cooling {
            reason: HealthReason::Permission,
            until: Utc::now() + Delta::hours(1),
            scope: "model:opus".into(),
        };
        assert_eq!(preview(&t, &[a.clone(), b.clone()]), "a");
        assert!(
            matches!(t.preview(Provider::Claude, None, None, Some("opus"), &[a, b], None, 0), PreviewDecision::Selected { account } if account == "b")
        );
    }
    #[test]
    fn handshake_preserves_after_move_for_first_response_create() {
        let (_dir, t) = table();
        let a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        drop(choose(&t, Some("claude:s"), &[a]));
        let (_, mut lease, _) = choose(&t, Some("claude:s"), &[b]);
        assert!(lease.after_move);
        lease.defer_after_move();
        drop(lease);
        let lease = t.begin_request(Some("claude:s"), "b");
        assert!(lease.after_move);
        drop(lease);
        assert!(!t.begin_request(Some("claude:s"), "b").after_move);
    }
    #[test]
    fn persistence_restart_import_and_corrupt_quarantine() {
        let (dir, t) = table();
        drop(choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]));
        t.flush().unwrap();
        t.flush().unwrap();
        let value = t.export().unwrap();
        assert_eq!(value["schema"], 1);
        drop(t);
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        assert_eq!(t.current(Some("claude:s")).as_deref(), Some("a"));
        assert_eq!(t.account_stats("a").0, 1);
        t.import(value).unwrap();
        let pin = t.pin(Some("claude:s"));
        assert!(t.import(json!({"schema":1,"bindings":[]})).is_err());
        drop(pin);
        drop(t);
        fs::write(dir.path().join("state/bindings.json"), "{broken").unwrap();
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        assert_eq!(t.count(), 0);
        assert!(fs::read_dir(dir.path().join("state")).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("bindings.json.corrupt-")
        }));
    }

    #[test]
    fn concurrent_placements_see_every_prior_reservation() {
        let (_dir, t) = table();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let cs = vec![candidate("a", 89.0, 8, 1), candidate("b", 10.0, 30, 2)];
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let t = t.clone();
                let barrier = barrier.clone();
                let cs = cs.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    choose(&t, Some(&format!("claude:burst{i}")), &cs)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|(id, _, _)| id == "a").count(), 1);
        assert_eq!(results.iter().filter(|(id, _, _)| id == "b").count(), 7);
        drop(results);
        assert!(t.data.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn constructor_rejects_invalid_config_and_schema_is_quarantined() {
        let dir = fixture_dir();
        let config = RoutingConfig {
            gate_5h_pct: f64::NAN,
            ..RoutingConfig::default()
        };
        assert!(BindingTable::new(dir.path(), config).is_err());
        fs::create_dir_all(dir.path().join("state")).unwrap();
        fs::write(
            dir.path().join("state/bindings.json"),
            r#"{"schema":2,"bindings":[]}"#,
        )
        .unwrap();
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        assert_eq!(t.count(), 0);
        assert!(matches!(
            t.choose(Provider::Claude, None, None, None, &[], None, 0),
            Decision::None
        ));
    }

    #[test]
    fn flush_releases_data_lock_and_preserves_mutations_until_next_write() {
        use std::sync::mpsc;
        let (_dir, t) = table();
        let (_, lease, _) = choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]);
        let (captured_tx, captured_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = t.clone();
        let flush = std::thread::spawn(move || {
            writer.flush_with(|path, bytes| {
                captured_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(std::io::Error::other)?;
                atomic_write(path, bytes)
            })
        });
        captured_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (mutated_tx, mutated_rx) = mpsc::channel();
        let mutation = t.clone();
        let mutate = std::thread::spawn(move || {
            // Completion, import, placement and touch must all proceed while
            // the first writer is stopped after capturing its old snapshot.
            drop(lease);
            mutation
                .import(json!({"schema":1,"bindings":[{
                    "key":"claude:s", "account":"b", "last_used":Utc::now()
                }]}))
                .unwrap();
            drop(choose(
                &mutation,
                Some("claude:new"),
                &[candidate("b", 10.0, 8, 1)],
            ));
            mutation.touch(Some("claude:s"));
            mutated_tx.send(()).unwrap();
        });
        let progressed = mutated_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        flush.join().unwrap().unwrap();
        mutate.join().unwrap();
        assert!(
            progressed.is_ok(),
            "completion and selection blocked behind disk writer"
        );
        assert!(
            t.data.lock().unwrap().dirty,
            "old flush erased newer mutations"
        );
        let old: Snapshot = serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap();
        assert_eq!(old.bindings.len(), 1);
        assert_eq!(old.bindings[0].account, "a");
        t.flush().unwrap();
        let latest: Snapshot = serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap();
        assert_eq!(latest.bindings.len(), 2);
        assert!(latest.bindings.iter().all(|b| b.account == "b"));
        assert!(!t.data.lock().unwrap().dirty);
    }

    #[test]
    fn failed_flush_keeps_dirty_and_import_advances_existing_revision() {
        let (_dir, t) = table();
        drop(choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]));
        assert!(
            t.flush_with(|_, _| Err(std::io::Error::other("synthetic write failure")))
                .is_err()
        );
        let revision = t.data.lock().unwrap().revision;
        assert!(t.data.lock().unwrap().dirty);
        t.import(json!({"schema":1,"bindings":[]})).unwrap();
        assert!(t.data.lock().unwrap().revision > revision);
        t.flush().unwrap();
        let state: Snapshot = serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap();
        assert!(state.bindings.is_empty());
    }

    fn codex_candidate(id: &str, used: f64) -> Candidate {
        let mut c = candidate(id, used, 30, 2);
        c.account.provider = Provider::Codex;
        c.account.identity = Identity::Codex {
            account_id: id.into(),
            email: "synthetic@example.invalid".into(),
        };
        c
    }

    #[test]
    fn projected_credit_limit_excludes_every_selection_path() {
        let (_dir, t) = table();
        let mut a = codex_candidate("a", 98.0);
        let b = codex_candidate("b", 99.0);
        // Establish both bound and unbound sticky placement below the limit.
        drop(t.choose(
            Provider::Codex,
            Some("codex:s"),
            None,
            None,
            std::slice::from_ref(&a),
            None,
            0,
        ));
        drop(t.choose(
            Provider::Codex,
            None,
            None,
            None,
            std::slice::from_ref(&a),
            None,
            0,
        ));
        let reservation = t.begin_request(Some("codex:s"), "a");
        assert_eq!(t.pending_count("a"), 1);
        a.inflight = 1;
        let cfg = &t.config;
        let d = t.data.lock().unwrap();
        let effective = effective_account(&a, &d, cfg, None, Utc::now());
        assert!(!effective.eligible_at(None, Utc::now()));
        assert_eq!(a.account.health, Health::Ready);
        drop(d);
        let mut b = b;
        b.account.allow_credits = true;
        for (key, parent, pinned) in [
            (Some("codex:new"), None, None),
            (Some("codex:s"), None, None),
            (Some("codex:child"), Some("codex:s"), None),
            (None, None, None),
            (Some("codex:pin"), None, Some("a")),
        ] {
            assert_eq!(
                t.preview(
                    Provider::Codex,
                    key,
                    parent,
                    None,
                    &[a.clone(), b.clone()],
                    pinned,
                    0
                ),
                PreviewDecision::Selected {
                    account: "b".into()
                }
            );
        }
        assert!(
            matches!(t.choose(Provider::Codex, Some("codex:s"), None, None, &[a, b], None, 0), Decision::Selected { account, moves, .. } if account == "b" && moves[0].reason == "forced")
        );
        drop(reservation);
        assert_eq!(t.pending_count("a"), 0);
    }

    #[test]
    fn quota_limit_uses_observed_100_and_codex_credit_guard_only() {
        let (_dir, t) = table();
        let mut c = codex_candidate("a", 98.0);
        c.account.allow_credits = true;
        c.inflight = 2; // Projected 100 is still below the observed limit.
        assert!(matches!(
            t.preview(
                Provider::Codex,
                None,
                None,
                None,
                std::slice::from_ref(&c),
                None,
                0
            ),
            PreviewDecision::Selected { .. }
        ));
        c.quota.as_mut().unwrap()["windows"][0]["used_pct"] = json!(100);
        assert_eq!(
            t.preview(
                Provider::Codex,
                None,
                None,
                None,
                std::slice::from_ref(&c),
                None,
                0
            ),
            PreviewDecision::None
        );
        c.quota = None;
        assert!(matches!(
            t.preview(Provider::Codex, None, None, None, &[c], None, 0),
            PreviewDecision::Selected { .. }
        ));
        let mut claude = candidate("cl", 98.0, 30, 2);
        claude.inflight = 2;
        assert!(matches!(
            t.preview(
                Provider::Claude,
                None,
                None,
                None,
                std::slice::from_ref(&claude),
                None,
                0
            ),
            PreviewDecision::Selected { .. }
        ));
        claude.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(100);
        assert_eq!(
            t.preview(Provider::Claude, None, None, None, &[claude], None, 0),
            PreviewDecision::None
        );
    }

    #[test]
    fn restored_traffic_keeps_stale_quota_last_after_restart_and_import() {
        let (_dir, t) = table();
        let last = Utc::now() - Delta::minutes(20);
        let state = json!({"schema":1,"bindings":[
            {"key":"claude:older","account":"a","last_used":last-Delta::minutes(1)},
            {"key":"claude:newer","account":"a","last_used":last}
        ]});
        t.import(state).unwrap();
        assert_eq!(t.data.lock().unwrap().traffic.get("a"), Some(&last));
        t.flush().unwrap();
        let mut stale = candidate("a", 0.0, 8, 1);
        for w in stale.quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            w["observed_at"] = json!(last);
        }
        let known = candidate("b", 95.0, 30, 2);
        assert_eq!(preview(&t, &[stale.clone(), known.clone()]), "b");
        let home = t.path.parent().unwrap().parent().unwrap().to_path_buf();
        drop(t);
        let restored = BindingTable::new(&home, RoutingConfig::default()).unwrap();
        assert_eq!(restored.data.lock().unwrap().traffic.get("a"), Some(&last));
        assert_eq!(preview(&restored, &[stale, known]), "b");
    }

    #[test]
    fn stale_exhaustion_does_not_move_warm_binding_or_exclude_unbound_fallback() {
        let (_dir, t) = table();
        let mut c = codex_candidate("a", 10.0);
        drop(t.choose(
            Provider::Codex,
            Some("codex:warm"),
            None,
            None,
            std::slice::from_ref(&c),
            None,
            0,
        ));
        assert!(t.has_traffic("a"));
        for w in c.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(100);
            w["observed_at"] = json!(Utc::now() - Delta::minutes(20));
        }
        c.quota.as_mut().unwrap()["signals"] = json!({"limit_reached":true});
        for key in [Some("codex:warm"), None] {
            assert_eq!(
                t.preview(
                    Provider::Codex,
                    key,
                    None,
                    None,
                    std::slice::from_ref(&c),
                    None,
                    0
                ),
                PreviewDecision::Selected {
                    account: "a".into()
                }
            );
        }
        drop(t.choose(
            Provider::Codex,
            None,
            None,
            None,
            std::slice::from_ref(&c),
            None,
            0,
        ));
        assert!(t.has_traffic("a"));
    }

    #[test]
    fn drained_binding_stays_warm_or_cold_below_gate_but_rebalances_cold_above_gate() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 10.0, 30, 2);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        a.account.drain = true;
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(91);
        }
        assert_eq!(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]).0, "a");
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(10);
        }
        age(&t, "claude:s", 3601);
        assert_eq!(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]).0, "a");
        assert_eq!(
            choose(&t, Some("claude:new"), &[a.clone(), b.clone()]).0,
            "b"
        );
        age(&t, "claude:s", 3601);
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(91);
        }
        let (account, _, moves) = choose(&t, Some("claude:s"), &[a, b]);
        assert_eq!(account, "b");
        assert_eq!(moves[0].reason, "cold_rebalance");
    }

    #[test]
    fn flush_prunes_oversized_group_after_its_last_pin_is_released() {
        let (_dir, t) = table();
        let pin = t.pin(Some("claude:oversized"));
        {
            let mut d = t.data.lock().unwrap();
            let now = Utc::now();
            for i in 0..=CAPACITY {
                let key = format!("claude:oversized:agent:{i}");
                d.bindings.insert(
                    key.clone(),
                    Binding {
                        key,
                        account: "a".into(),
                        last_used: now,
                    },
                );
            }
            d.changed();
        }
        t.flush().unwrap();
        assert_eq!(t.data.lock().unwrap().bindings.len(), CAPACITY + 1);
        assert!(!t.data.lock().unwrap().dirty);
        // Pin release itself does not mark persistence dirty or scan the table.
        drop(pin);
        t.flush().unwrap();
        assert!(t.data.lock().unwrap().bindings.len() <= CAPACITY);
        let saved: Snapshot = serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap();
        assert!(saved.bindings.len() <= CAPACITY);
        assert!(
            saved.bindings.is_empty(),
            "eviction must preserve group atomicity"
        );
    }
}
