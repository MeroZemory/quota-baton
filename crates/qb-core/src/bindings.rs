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
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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
    pub keys: usize,
    pub debug_pin: bool,
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
#[derive(Clone)]
struct Group {
    keys: BTreeSet<String>,
    account: String,
    last_used: DateTime<Utc>,
}
#[derive(Clone, Default)]
struct AccountIndex {
    count: usize,
    groups: usize,
    recent: BTreeSet<(DateTime<Utc>, String)>,
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
    rate_hits: BTreeMap<String, BTreeMap<String, DateTime<Utc>>>,
    overload_marks: BTreeMap<String, DateTime<Utc>>,
    last_request_bytes: BTreeMap<String, u64>,
    mark_history: BTreeMap<String, Vec<DateTime<Utc>>>,
    pacing: BTreeMap<String, bool>,
    overload_reported: BTreeMap<String, DateTime<Utc>>,
    groups: BTreeMap<String, Group>,
    lru: BTreeSet<(DateTime<Utc>, String)>,
    accounts: BTreeMap<String, AccountIndex>,
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
    thresholds: Mutex<BTreeMap<String, usize>>,
    data: Mutex<Data>,
    writer: Mutex<()>,
    writes_stopped: AtomicBool,
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
        if let Some(group) = &self.group {
            decrement(&mut d.active, group);
            touch_group(&mut d, group, now);
            release_group(&mut d, group);
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
            let mut d = self.table.data.lock().unwrap_or_else(|e| e.into_inner());
            decrement(&mut d.pins, group);
            release_group(&mut d, group);
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
// Index changes visit only the affected group, never the binding table.
fn unindex_group(d: &mut Data, g: &str) -> Option<Group> {
    let entry = d.groups.remove(g)?;
    d.lru.remove(&(entry.last_used, g.to_owned()));
    if let Some(account) = d.accounts.get_mut(&entry.account) {
        account.count -= entry.keys.len();
        account.recent.remove(&(entry.last_used, g.to_owned()));
        if account.count == 0 {
            d.accounts.remove(&entry.account);
        }
        if let Some(account) = d.accounts.get_mut(&entry.account) {
            account.groups = account.groups.saturating_sub(1);
        }
    }
    Some(entry)
}
fn index_group(d: &mut Data, g: String, entry: Group) {
    if !protected(d, &g) {
        d.lru.insert((entry.last_used, g.clone()));
    }
    let account = d.accounts.entry(entry.account.clone()).or_default();
    account.count += entry.keys.len();
    account.groups += 1;
    account.recent.insert((entry.last_used, g.clone()));
    d.groups.insert(g, entry);
}
fn release_group(d: &mut Data, g: &str) {
    if !protected(d, g)
        && let Some(entry) = d.groups.get(g)
    {
        d.lru.insert((entry.last_used, g.to_owned()));
    }
}
fn protect_group(d: &mut Data, g: &str) {
    if let Some(entry) = d.groups.get(g) {
        d.lru.remove(&(entry.last_used, g.to_owned()));
    }
}
fn touch_group(d: &mut Data, g: &str, now: DateTime<Utc>) {
    if let Some(mut entry) = unindex_group(d, g) {
        entry.last_used = entry.last_used.max(now);
        for key in &entry.keys {
            if let Some(b) = d.bindings.get_mut(key) {
                b.last_used = entry.last_used;
            }
        }
        index_group(d, g.to_owned(), entry);
        d.changed();
    }
}
fn rebuild_indexes(d: &mut Data) {
    d.groups.clear();
    d.lru.clear();
    d.accounts.clear();
    let mut groups = BTreeMap::<String, Group>::new();
    for b in d.bindings.values() {
        let entry = groups
            .entry(group(&b.key).to_owned())
            .or_insert_with(|| Group {
                keys: BTreeSet::new(),
                account: b.account.clone(),
                last_used: b.last_used,
            });
        entry.keys.insert(b.key.clone());
        entry.last_used = entry.last_used.max(b.last_used);
    }
    for (g, entry) in groups {
        index_group(d, g, entry);
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
    let mut inconsistent = BTreeSet::new();
    for b in state.bindings {
        if b.key.is_empty() || b.account.is_empty() || d.bindings.contains_key(&b.key) {
            bail!("invalid binding");
        }
        let g = group(&b.key).to_owned();
        if let Some(previous) = groups.insert(g.clone(), b.account.clone())
            && previous != b.account
        {
            inconsistent.insert(g);
        }
        d.bindings.insert(b.key.clone(), b);
    }
    if !inconsistent.is_empty() {
        tracing::warn!(
            groups = inconsistent.len(),
            "Dropping inconsistent binding groups"
        );
        d.bindings
            .retain(|key, _| !inconsistent.contains(group(key)));
        d.changed();
    }
    rebuild_indexes(&mut d);
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
            thresholds: Mutex::new(BTreeMap::new()),
            data: Mutex::new(data),
            writer: Mutex::new(()),
            writes_stopped: AtomicBool::new(false),
        });
        table.prune(
            &mut table.data.lock().unwrap_or_else(|e| e.into_inner()),
            Utc::now(),
        );
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
        while let Some((time, g)) = d.lru.first().cloned() {
            if d.bindings.len() <= CAPACITY
                && idle(now, time) < self.config.session_binding_max_secs
            {
                break;
            }
            if let Some(entry) = unindex_group(d, &g) {
                for key in entry.keys {
                    d.bindings.remove(&key);
                }
                d.after_move.remove(&g);
                d.changed();
            }
        }
    }
    pub fn count(&self) -> usize {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, Utc::now());
        d.bindings.len()
    }
    #[cfg(test)]
    fn has_traffic(&self, id: &str) -> bool {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        d.traffic.contains_key(id) || d.pending.get(id).is_some_and(|count| *count > 0)
    }
    pub fn account_stats(&self, id: &str) -> (usize, Option<DateTime<Utc>>) {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, Utc::now());
        d.accounts.get(id).map_or((0, None), |a| {
            (a.count, a.recent.last().map(|(time, _)| *time))
        })
    }

    pub fn active_sessions(&self, id: &str, now: DateTime<Utc>) -> usize {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.active_sessions_in(&d, id, now)
    }

    fn active_sessions_in(&self, d: &Data, id: &str, now: DateTime<Utc>) -> usize {
        let Some(account) = d.accounts.get(id) else {
            return 0;
        };
        let cutoff = now - chrono::Duration::seconds(self.config.active_window_secs as i64);
        account
            .recent
            .iter()
            .filter(|(last, group)| {
                *last >= cutoff || d.active.contains_key(group) || d.pins.contains_key(group)
            })
            .count()
    }

    pub fn spread_threshold(&self) -> usize {
        self.config.spread_threshold
    }

    pub fn set_account_threshold(&self, id: &str, threshold: Option<usize>) {
        let mut thresholds = self.thresholds.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(threshold) = threshold {
            thresholds.insert(id.to_owned(), threshold.max(1));
        } else {
            thresholds.remove(id);
        }
    }

    pub fn spread_threshold_for(&self, id: &str) -> usize {
        self.thresholds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
            .unwrap_or(self.config.spread_threshold)
    }

    pub fn note_client_rate(
        &self,
        provider: Provider,
        account: &str,
        key: Option<&str>,
        candidates: &[Candidate],
        now: DateTime<Utc>,
    ) -> (usize, usize, usize, usize) {
        let Some(key) = key else { return (0, 0, 0, 0) };
        let group = group(key).to_owned();
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = now - chrono::Duration::seconds(self.config.overload_window_secs as i64);
        let hit_count = {
            let hits = d.rate_hits.entry(account.to_owned()).or_default();
            hits.retain(|_, at| *at >= cutoff);
            hits.insert(group, now);
            hits.len()
        };
        let active = self.active_sessions_in(&d, account, now);
        let threshold = self.spread_threshold_for(account);
        let target = candidates
            .iter()
            .filter(|c| {
                c.account.id != account
                    && c.account.provider == provider
                    && !c.account.drain
                    && effective_account(c, &d, &self.config, None, now).eligible_at(None, now)
                    && metrics(c, &d, &self.config, now).gate
            })
            .map(|c| {
                (
                    c.account.id.clone(),
                    self.active_sessions_in(&d, &c.account.id, now),
                    self.spread_threshold_for(&c.account.id),
                )
            })
            .filter(|(_, active, threshold)| active < threshold)
            .min_by_key(|(_, active, _)| *active);
        let Some((_target, target_active, _)) = target else {
            return (active, threshold, hit_count, 0);
        };
        if hit_count < 3 || active <= threshold || active.saturating_sub(target_active) < 2 {
            return (active, threshold, hit_count, 0);
        }
        d.overload_marks.retain(|_, expires| *expires > now);
        let history = d.mark_history.entry(account.to_owned()).or_default();
        history.retain(|at| *at > cutoff);
        let already = history.len();
        let budget = 2usize.saturating_sub(already);
        let imbalance_budget = active.saturating_sub(target_active).saturating_sub(2) / 2;
        let excess = active
            .saturating_sub(threshold)
            .min(budget)
            .min(imbalance_budget);
        let mut victims: Vec<_> = d
            .groups
            .iter()
            .filter(|(g, entry)| {
                entry.account == account
                    && !d.active.contains_key(*g)
                    && !d.overload_marks.contains_key(*g)
            })
            .map(|(g, entry)| {
                (
                    g.clone(),
                    d.last_request_bytes.get(g).copied().unwrap_or(0),
                    entry.last_used,
                )
            })
            .collect();
        victims.sort_by(|(_, bytes_a, last_a), (_, bytes_b, last_b)| {
            bytes_a.cmp(bytes_b).then_with(|| last_a.cmp(last_b))
        });
        victims.truncate(excess);
        let victims: Vec<_> = victims.into_iter().map(|(g, _, _)| g).collect();
        for victim in &victims {
            d.overload_marks.insert(
                victim.clone(),
                now + chrono::Duration::seconds(self.config.overload_window_secs as i64),
            );
        }
        d.mark_history
            .entry(account.to_owned())
            .or_default()
            .extend(std::iter::repeat_n(now, victims.len()));
        (active, threshold, hit_count, victims.len())
    }

    fn has_overload_mark(&self, d: &Data, key: &str, now: DateTime<Utc>) -> bool {
        let g = group(key);
        d.overload_marks
            .get(g)
            .is_some_and(|expires| *expires > now)
    }
    pub fn overload_marked(&self, key: Option<&str>, now: DateTime<Utc>) -> bool {
        key.and_then(|key| {
            self.data
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .overload_marks
                .get(group(key))
                .copied()
        })
        .is_some_and(|expires| expires > now)
    }
    pub fn overload_report_due(&self, account: &str, now: DateTime<Utc>) -> bool {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let due = d.overload_reported.get(account).is_none_or(|at| {
            *at + chrono::Duration::seconds(self.config.overload_window_secs as i64) <= now
        });
        if due {
            d.overload_reported.insert(account.to_owned(), now);
        }
        due
    }
    pub fn set_pacing(&self, id: &str, pacing: bool) {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pacing
            .insert(id.to_owned(), pacing);
    }
    pub fn request_bytes(&self, key: Option<&str>, bytes: u64) {
        if let Some(key) = key {
            self.data
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_request_bytes
                .insert(group(key).to_owned(), bytes);
        }
    }
    pub fn pending_count(&self, id: &str) -> usize {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .get(id)
            .copied()
            .unwrap_or(0)
    }
    pub fn current(&self, key: Option<&str>) -> Option<String> {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, Utc::now());
        key.and_then(|k| {
            d.bindings.get(k).or_else(|| {
                d.groups
                    .get(group(k))
                    .and_then(|g| g.keys.first())
                    .and_then(|key| d.bindings.get(key))
            })
        })
        .map(|b| b.account.clone())
    }
    pub fn touch(&self, key: Option<&str>) {
        if let Some(k) = key {
            touch_group(
                &mut self.data.lock().unwrap_or_else(|e| e.into_inner()),
                group(k),
                Utc::now(),
            );
        }
    }
    pub fn pin(self: &Arc<Self>, key: Option<&str>) -> SocketPin {
        let g = key.map(|k| group(k).to_owned());
        if let Some(g) = &g {
            let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
            *d.pins.entry(g.clone()).or_default() += 1;
            protect_group(&mut d, g);
        }
        SocketPin {
            table: self.clone(),
            group: g,
        }
    }
    pub fn begin_request(self: &Arc<Self>, key: Option<&str>, account: &str) -> BindingLease {
        self.reserve(
            &mut self.data.lock().unwrap_or_else(|e| e.into_inner()),
            key,
            account,
        )
    }
    fn reserve(self: &Arc<Self>, d: &mut Data, key: Option<&str>, account: &str) -> BindingLease {
        *d.pending.entry(account.into()).or_default() += 1;
        d.traffic.insert(account.into(), Utc::now());
        let g = key.map(|k| group(k).to_owned());
        let after_move = if let Some(g) = &g {
            *d.active.entry(g.clone()).or_default() += 1;
            protect_group(d, g);
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
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.select(
            &d,
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            Utc::now(),
            true,
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
        self.choose_with_hold(
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            request_bytes,
            true,
            false,
        )
    }
    /// Hold state belongs to this request, not to its persisted session group.
    #[allow(clippy::too_many_arguments)]
    pub fn choose_with_hold(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
        hold_allowed: bool,
        hold_timed_out: bool,
    ) -> Decision {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        self.prune(&mut d, now);
        let (choice, reason) = self.select(
            &d,
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            now,
            hold_allowed,
        );
        let account = match choice {
            PreviewDecision::Hold { until } => return Decision::Hold { until },
            PreviewDecision::None => return Decision::None,
            PreviewDecision::Selected { account } => account,
        };
        let mut moves = Vec::new();
        if let Some(k) = key {
            let g = group(k);
            let reason = if hold_timed_out {
                "reset_hold_timeout"
            } else {
                reason
            };
            let previous = unindex_group(&mut d, g);
            let from = previous.as_ref().map(|entry| entry.account.clone());
            let last_used = previous.as_ref().map_or(now, |entry| entry.last_used);
            let mut keys = previous.map_or_else(BTreeSet::new, |entry| entry.keys);
            keys.insert(k.into());
            if let Some(p) = parent
                && !d.bindings.contains_key(p)
                && group(p) == g
            {
                keys.insert(p.into());
            }
            if from.as_deref() != Some(&account) {
                moves.push(MoveEvent {
                    key: g.to_owned(),
                    from: from.clone(),
                    to: account.clone(),
                    reason: if from.is_none() { "new" } else { reason }.into(),
                    bytes: request_bytes,
                    keys: keys.len(),
                    debug_pin: pinned == Some(account.as_str()),
                });
                if from.is_some() {
                    d.after_move.insert(g.into());
                }
            }
            for k in &keys {
                d.bindings.insert(
                    k.clone(),
                    Binding {
                        key: k.clone(),
                        account: account.clone(),
                        last_used,
                    },
                );
            }
            index_group(
                &mut d,
                g.to_owned(),
                Group {
                    keys,
                    account: account.clone(),
                    last_used,
                },
            );
            d.last_request_bytes.insert(g.to_owned(), request_bytes);
            d.overload_marks.remove(g);
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
    /// Read-only counterpart of prune for selection previews. Ordinary lookups
    /// visit one group; only outstanding capacity eviction walks the LRU prefix.
    fn retained_binding<'a>(
        &self,
        d: &'a Data,
        key: &str,
        now: DateTime<Utc>,
    ) -> Option<&'a Binding> {
        let g = group(key);
        let entry = d.groups.get(g)?;
        if !protected(d, g) {
            if idle(now, entry.last_used) >= self.config.session_binding_max_secs {
                return None;
            }
            if d.bindings.len() > CAPACITY {
                let mut size = d.bindings.len();
                for (_, oldest) in &d.lru {
                    if size <= CAPACITY {
                        break;
                    }
                    if oldest == g {
                        return None;
                    }
                    size -= d.groups[oldest].keys.len();
                }
            }
        }
        d.bindings
            .get(key)
            .or_else(|| entry.keys.first().and_then(|key| d.bindings.get(key)))
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
        hold_allowed: bool,
    ) -> (PreviewDecision, &'static str) {
        let entries: Vec<_> = candidates
            .iter()
            .filter(|c| c.account.provider == provider)
            .map(|c| {
                let eligible =
                    effective_account(c, d, &self.config, model, now).eligible_at(model, now);
                (c, eligible)
            })
            .collect();
        if let Some(id) = pinned
            && entries
                .iter()
                .any(|(c, eligible)| c.account.id == id && *eligible)
        {
            return (PreviewDecision::Selected { account: id.into() }, "forced");
        }
        let bound = key.and_then(|k| self.retained_binding(d, k, now)).cloned();
        let mut reason = "new";
        if let Some(b) = bound {
            reason = "forced";
            if let Some((c, eligible)) = entries.iter().find(|(c, _)| c.account.id == b.account) {
                if hold_allowed
                    && c.account.user_state == UserState::Active
                    && let Health::Cooling {
                        reason: HealthReason::Quota,
                        until,
                        scope,
                    } = &c.account.health
                    && *until > now
                    && (scope == "account"
                        || scope
                            .strip_prefix("model:")
                            .is_some_and(|m| model == Some(m)))
                {
                    let reset = c
                        .account
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
                    let skip_reason = if reset <= now {
                        "reset_elapsed"
                    } else if reset.signed_duration_since(now).to_std().is_err() {
                        "reset_before_now"
                    } else {
                        "reset_beyond_hold_bound"
                    };
                    tracing::debug!(
                        account = %b.account,
                        reason = skip_reason,
                        "reset hold skipped"
                    );
                }
                let marked = self.has_overload_mark(d, &b.key, now);
                if marked && *eligible {
                    let room_elsewhere = entries.iter().any(|(candidate, eligible)| {
                        candidate.account.id != b.account
                            && *eligible
                            && !candidate.account.drain
                            && metrics(candidate, d, &self.config, now).gate
                            && self.active_sessions_in(d, &candidate.account.id, now)
                                < self.spread_threshold_for(&candidate.account.id)
                    });
                    if !room_elsewhere
                        || self.active_sessions_in(d, &b.account, now)
                            <= self.spread_threshold_for(&b.account)
                    {
                        // choose_with_hold clears this mark when it reserves
                        // the unchanged binding. Preview remains read-only.
                        return (
                            PreviewDecision::Selected {
                                account: b.account.clone(),
                            },
                            "forced",
                        );
                    }
                    // Use the ordinary C7 ordering below, restricted to real
                    // eligible targets with room rather than candidate id order.
                    reason = "overload";
                }
                if *eligible && !marked {
                    let g = group(&b.key);
                    let last = d.groups.get(g).map_or(b.last_used, |entry| entry.last_used);
                    let at_capacity = self.active_sessions_in(d, &b.account, now)
                        >= self.spread_threshold_for(&b.account);
                    let cold =
                        !protected(d, g) && idle(now, last) >= self.config.rebalance_idle_secs;
                    if !cold
                        || (!at_capacity
                            && !c.account.drain
                            && metrics(c, d, &self.config, now).gate)
                    {
                        return (
                            PreviewDecision::Selected {
                                account: b.account.clone(),
                            },
                            reason,
                        );
                    }
                    reason = if marked { "overload" } else { "cold_rebalance" };
                }
            }
        } else if let Some(p) = parent.and_then(|p| self.retained_binding(d, p, now)) {
            if entries
                .iter()
                .any(|(c, eligible)| c.account.id == p.account && *eligible)
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
            && entries
                .iter()
                .any(|(c, eligible)| &c.account.id == id && *eligible)
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
            .filter(|(c, eligible)| !c.account.drain && *eligible)
            .map(|(c, _)| (c, metrics(c, d, &self.config, now)))
            .collect();
        let has_gate = eligible.iter().any(|(_, m)| m.gate);
        let pool: Vec<_> = eligible
            .iter()
            .filter(|(_, m)| !has_gate || m.gate)
            .collect();
        let has_room = pool.iter().any(|(c, _)| {
            self.active_sessions_in(d, &c.account.id, now)
                < self.spread_threshold_for(&c.account.id)
        });
        let mut partition: Vec<_> = pool
            .into_iter()
            .filter(|(c, _)| {
                !has_room
                    || self.active_sessions_in(d, &c.account.id, now)
                        < self.spread_threshold_for(&c.account.id)
            })
            .collect();
        if reason == "overload" {
            partition.retain(|(c, m)| {
                m.gate
                    && self.active_sessions_in(d, &c.account.id, now)
                        < self.spread_threshold_for(&c.account.id)
            });
        }
        let ratio = |c: &Candidate| {
            self.active_sessions_in(d, &c.account.id, now) as f64
                / self.spread_threshold_for(&c.account.id) as f64
        };
        let min_ratio = partition
            .iter()
            .map(|(c, _)| ratio(c))
            .min_by(f64::total_cmp);
        if !has_room && let Some(minimum) = min_ratio {
            partition.retain(|(c, _)| ratio(c) == minimum);
        }
        let non_pacing = partition
            .iter()
            .any(|(c, _)| !d.pacing.get(&c.account.id).copied().unwrap_or(false));
        if non_pacing {
            partition.retain(|(c, _)| !d.pacing.get(&c.account.id).copied().unwrap_or(false));
        }
        let mut gated: Vec<_> = partition.iter().filter(|(_, m)| m.gate).collect();
        // Anchor the first reset cohort; pairwise approximate comparisons are
        // non-transitive and cannot be used as a sorting comparator.
        let anchor = gated
            .iter()
            .filter(|(_, m)| !m.unknown)
            .filter_map(|(_, m)| m.weekly)
            .min();
        gated.sort_by(|(a, x), (b, y)| {
            let cohort = |t: Option<DateTime<Utc>>| match (anchor, t) {
                (Some(start), Some(t)) if (t - start).num_seconds() <= 10800 => (0, None),
                (_, Some(t)) => (1, Some(t)),
                (_, None) => (2, None),
            };
            x.unknown
                .cmp(&y.unknown)
                .then_with(|| cohort(x.weekly).cmp(&cohort(y.weekly)))
                .then_with(|| x.five.is_none().cmp(&y.five.is_none()))
                .then_with(|| x.five.cmp(&y.five))
                .then_with(|| x.usage.total_cmp(&y.usage))
                .then_with(|| a.account.id.cmp(&b.account.id))
        });
        let chosen = gated.first().map(|(c, _)| *c).or_else(|| {
            partition
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
        let state = snapshot(&self.data.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(serde_json::to_value(state)?)
    }
    /// Takeover only: reject import while any request or socket owns the table.
    pub fn import(&self, value: Value) -> Result<()> {
        let mut next = decode(value)?;
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
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
    /// Finish the last snapshot under the writer lock, then retire this
    /// process as a state-file writer while its streams finish in memory.
    pub fn freeze(&self) -> Result<()> {
        self.flush_with_mode(atomic_write, true)
    }
    fn flush_with(&self, write: impl FnOnce(&Path, &[u8]) -> std::io::Result<()>) -> Result<()> {
        self.flush_with_mode(write, false)
    }
    fn flush_with_mode(
        &self,
        write: impl FnOnce(&Path, &[u8]) -> std::io::Result<()>,
        freeze: bool,
    ) -> Result<()> {
        // Acquire before taking the snapshot so an older flush can never
        // replace a newer file. Selectors and completion never take this lock.
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if self.writes_stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let (state, revision) = {
            let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
            self.prune(&mut d, Utc::now());
            if !d.dirty {
                if freeze {
                    self.writes_stopped.store(true, Ordering::Release);
                }
                return Ok(());
            }
            (snapshot(&d), d.revision)
        };
        write(&self.path, &serde_json::to_vec(&state)?)?;
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if d.revision == revision {
            d.dirty = false;
        }
        if freeze {
            self.writes_stopped.store(true, Ordering::Release);
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
    unknown: bool,
    weekly: Option<DateTime<Utc>>,
    five: Option<DateTime<Utc>>,
    usage: f64,
}
fn date(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}
fn stale_window(c: &Candidate, d: &Data, w: &Value, now: DateTime<Utc>) -> bool {
    date(&w["observed_at"]).is_some_and(|observed| {
        (now - observed).num_seconds() > 900
            && (d.traffic.get(&c.account.id).is_some_and(|t| *t > observed)
                || d.pending
                    .get(&c.account.id)
                    .copied()
                    .unwrap_or(0)
                    .max(c.inflight)
                    > 0)
    })
}
/// Observed limits may construct C5 cooling; projection never constructs health.
fn effective_account(
    c: &Candidate,
    d: &Data,
    _cfg: &RoutingConfig,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Account {
    let mut account = c.account.clone();
    if account.eligible_at(model, now)
        && let Some(snapshot) = &c.quota
    {
        account.health = Health::Ready;
        account.quota_cooling = None;
        let mut snapshot = snapshot.clone();
        if let Some(windows) = snapshot["windows"].as_array_mut() {
            windows.retain(|w| !stale_window(c, d, w, now));
        }
        snapshot["signals"] = Value::Null;
        account.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot,
                model: model.map(str::to_owned),
                projected_allowance: 0.0,
            },
            now,
        );
    }
    account
}
fn account_window(w: &Value) -> Option<bool> {
    match w["name"].as_str()? {
        "7d" | "secondary" => Some(true),
        "5h" | "primary" => Some(false),
        _ => None,
    }
}
fn metrics(c: &Candidate, d: &Data, cfg: &RoutingConfig, now: DateTime<Utc>) -> Metrics {
    let mut m = Metrics {
        gate: true,
        unknown: false,
        weekly: None,
        five: None,
        usage: 0.0,
    };
    let Some(windows) = c.quota.as_ref().and_then(|q| q["windows"].as_array()) else {
        m.unknown = true;
        m.usage = f64::INFINITY;
        return m;
    };
    let pending = d
        .pending
        .get(&c.account.id)
        .copied()
        .unwrap_or(0)
        .max(c.inflight);
    let mut seen = false;
    for w in windows {
        let Some(weekly) = account_window(w) else {
            continue;
        };
        seen = true;
        let reset = date(&w["reset_at"]);
        let expired = reset.is_some_and(|r| r <= now);
        let reset = reset.filter(|r| *r > now);
        let slot = if weekly { &mut m.weekly } else { &mut m.five };
        *slot = match (*slot, reset) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let usage = if expired {
            Some(cfg.inflight_allowance_pct * pending as f64)
        } else if stale_window(c, d, w, now) {
            None
        } else {
            w["used_pct"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n + cfg.inflight_allowance_pct * pending as f64)
        };
        let Some(usage) = usage else {
            m.unknown = true;
            m.usage = f64::INFINITY;
            continue;
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
    if !seen {
        m.unknown = true;
        m.usage = f64::INFINITY;
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
        rebuild_indexes(&mut d);
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
        assert_eq!(preview(&t, &[unknown, known.clone()]), "a");
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
        assert_eq!(preview(&t, &[stale, known]), "a");
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
        let result = t.choose_with_hold(
            Provider::Claude,
            Some("claude:s"),
            None,
            None,
            std::slice::from_ref(&b),
            None,
            42,
            false,
            true,
        );
        assert!(matches!(result, Decision::Selected { account, moves, .. }
            if account == "b" && moves[0].reason == "reset_hold_timeout"));
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
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].keys, 2);
        assert_eq!(moves[0].bytes, 42);
        assert_eq!(moves[0].to, "b");
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
            rebuild_indexes(&mut d);
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
            user_id: None,
            account_id: id.into(),
            email: "synthetic@example.invalid".into(),
        };
        c
    }

    #[test]
    fn projected_credit_usage_preserves_affinity_and_remains_eligible_for_fallback() {
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
        assert!(effective.eligible_at(None, Utc::now()));
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
                    account: "a".into()
                }
            );
        }
        assert!(
            matches!(t.choose(Provider::Codex, Some("codex:s"), None, None, &[a, b], None, 0), Decision::Selected { account, moves, .. } if account == "a" && moves.is_empty())
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
    fn restored_bindings_never_seed_traffic_after_restart_or_import() {
        let (_dir, t) = table();
        let last = Utc::now() - Delta::minutes(20);
        let state = json!({"schema":1,"bindings":[
            {"key":"claude:older","account":"a","last_used":last-Delta::minutes(1)},
            {"key":"claude:newer","account":"a","last_used":last}
        ]});
        t.import(state).unwrap();
        assert!(t.data.lock().unwrap().traffic.is_empty());
        t.flush().unwrap();
        let mut stale = candidate("a", 0.0, 8, 1);
        for w in stale.quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            w["observed_at"] = json!(last);
        }
        let known = candidate("b", 95.0, 30, 2);
        assert_eq!(preview(&t, &[stale.clone(), known.clone()]), "a");
        let home = t.path.parent().unwrap().parent().unwrap().to_path_buf();
        drop(t);
        let restored = BindingTable::new(&home, RoutingConfig::default()).unwrap();
        assert!(restored.data.lock().unwrap().traffic.is_empty());
        assert_eq!(preview(&restored, &[stale, known]), "a");
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
    fn drained_binding_stays_warm_but_rebalances_cold_even_below_gate() {
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
        let (id, lease, moves) = choose(&t, Some("claude:s"), &[a.clone(), b.clone()]);
        assert_eq!(id, "b");
        assert_eq!(moves[0].reason, "cold_rebalance");
        drop(lease);
        a.account.drain = false;
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        a.account.drain = true;
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
            rebuild_indexes(&mut d);
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
    fn observe_at(c: &mut Candidate, time: DateTime<Utc>) {
        for w in c.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["observed_at"] = json!(time);
        }
    }

    #[test]
    fn stale_cold_groups_never_ping_pong_and_fresh_burst_spills_to_unknown() {
        let (_dir, t) = table();
        let mut a = candidate("a", 89.0, 8, 1);
        let mut b = candidate("b", 95.0, 30, 2);
        drop(choose(&t, Some("claude:cold"), std::slice::from_ref(&b)));
        let old = Utc::now() - Delta::minutes(20);
        observe_at(&mut a, old);
        observe_at(&mut b, old);
        drop(t.begin_request(None, "a"));
        for _ in 0..3 {
            age(&t, "claude:cold", 3601);
            let (id, lease, moves) = choose(&t, Some("claude:cold"), &[a.clone(), b.clone()]);
            assert_eq!(id, "b");
            assert!(moves.is_empty());
            drop(lease);
        }
        observe_at(&mut a, Utc::now());
        let (id, first, _) = choose(&t, Some("claude:burst1"), &[a.clone(), b.clone()]);
        assert_eq!(id, "a");
        let (id, second, _) = choose(&t, Some("claude:burst2"), &[a, b]);
        assert_eq!(id, "b");
        drop((first, second));
    }

    #[test]
    fn traffic_before_observation_does_not_make_old_usage_stale() {
        let (_dir, t) = table();
        let now = Utc::now();
        let observed = now - Delta::minutes(20);
        let mut c = candidate("a", 91.0, 30, 2);
        observe_at(&mut c, observed);
        let mut d = Data::default();
        d.traffic.insert("a".into(), observed - Delta::seconds(1));
        assert!(!metrics(&c, &d, &t.config, now).unknown);
        assert!(!metrics(&c, &d, &t.config, now).gate);
        d.traffic.insert("a".into(), observed + Delta::seconds(1));
        assert!(metrics(&c, &d, &t.config, now).unknown);
        assert!(metrics(&c, &d, &t.config, now).gate);
        d.traffic.insert("a".into(), observed - Delta::seconds(1));
        d.pending.insert("a".into(), 1);
        assert!(metrics(&c, &d, &t.config, now).unknown);
        assert!(metrics(&c, &d, &t.config, now).gate);
        d.pending.clear();
        observe_at(&mut c, now - Delta::seconds(900));
        d.traffic.insert("a".into(), now);
        assert!(!metrics(&c, &d, &t.config, now).unknown);
    }

    #[test]
    fn reset_expired_usage_is_zero_and_cold_binding_stays() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 30, 2);
        let b = candidate("b", 5.0, 8, 1);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(100);
            w["reset_at"] = json!(Utc::now() - Delta::seconds(1));
            w["observed_at"] = json!(Utc::now() - Delta::hours(2));
        }
        let m = metrics(&a, &Data::default(), &t.config, Utc::now());
        assert!(m.gate && !m.unknown);
        assert_eq!(m.usage, 0.0);
        assert!(m.weekly.is_none() && m.five.is_none());
        age(&t, "claude:s", 3601);
        let (id, lease, moves) = choose(&t, Some("claude:s"), &[a, b]);
        assert_eq!(id, "a");
        assert!(moves.is_empty());
        drop(lease);
    }

    #[test]
    fn only_account_windows_gate_and_aliases_set_the_correct_threshold() {
        let (_dir, t) = table();
        let now = Utc::now();
        let mut c = candidate("a", 10.0, 30, 2);
        c.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap().extend([
            json!({"name":"7d_oi","used_pct":100,"reset_at":now+Delta::hours(1),"observed_at":now}),
            json!({"name":"60m","used_pct":100,"reset_at":now+Delta::hours(1),"observed_at":now}),
        ]);
        let m = metrics(&c, &Data::default(), &t.config, now);
        assert!(m.gate && !m.unknown);
        assert_eq!(m.usage, 10.0);
        assert_eq!(
            m.weekly,
            date(&c.quota.as_ref().unwrap()["windows"][1]["reset_at"])
        );
        c.quota.as_mut().unwrap()["windows"][0]["name"] = json!("primary");
        c.quota.as_mut().unwrap()["windows"][1] = json!({
            "name":"secondary","used_pct":97,"reset_at":now+Delta::hours(4),"observed_at":now
        });
        assert!(metrics(&c, &Data::default(), &t.config, now).gate);
        c.quota.as_mut().unwrap()["windows"][0]["used_pct"] = Value::Null;
        let m = metrics(&c, &Data::default(), &t.config, now);
        assert!(m.gate && m.unknown);
        c.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(98);
        assert!(!metrics(&c, &Data::default(), &t.config, now).gate);
    }

    #[test]
    fn request_local_hold_timeout_does_not_disable_next_request() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        let until = Utc::now() + Delta::seconds(30);
        a.account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until,
            scope: "account".into(),
        };
        assert!(matches!(
            t.choose_with_hold(
                Provider::Claude,
                Some("claude:s"),
                None,
                None,
                std::slice::from_ref(&a),
                None,
                0,
                false,
                true
            ),
            Decision::None
        ));
        assert!(
            matches!(t.choose(Provider::Claude, Some("claude:s"), None, None,
            &[a], None, 0), Decision::Hold { until: u } if u == until)
        );
    }

    #[test]
    fn projected_credit_usage_never_constructs_cooling_hold_or_affinity_exclusion() {
        let (_dir, t) = table();
        let mut a = codex_candidate("a", 98.0);
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["reset_at"] = json!(Utc::now() + Delta::seconds(30));
        }
        drop(t.choose(
            Provider::Codex,
            Some("codex:s"),
            None,
            None,
            std::slice::from_ref(&a),
            None,
            0,
        ));
        a.inflight = 1;
        assert_eq!(
            effective_account(&a, &Data::default(), &t.config, None, Utc::now()).health,
            Health::Ready
        );
        assert!(matches!(
            t.choose(Provider::Codex, Some("codex:s"), None, None, &[a], None, 0),
            Decision::Selected { account, moves, .. } if account == "a" && moves.is_empty()
        ));
    }

    #[test]
    fn new_group_and_debug_pin_events_have_group_metadata_without_duplicate_bytes() {
        let (_dir, t) = table();
        let a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 10.0, 30, 2);
        let result = t.choose(
            Provider::Claude,
            Some("claude:s:agent:c"),
            Some("claude:s"),
            None,
            std::slice::from_ref(&a),
            None,
            77,
        );
        if let Decision::Selected { lease, moves, .. } = result {
            assert!(!lease.after_move);
            assert_eq!(moves.len(), 1);
            assert_eq!(moves[0].key, "claude:s");
            assert_eq!(moves[0].keys, 2);
            assert_eq!(moves[0].reason, "new");
            assert_eq!(moves[0].bytes, 77);
            assert!(!moves[0].debug_pin);
        } else {
            panic!("new group not selected");
        }
        let result = t.choose(
            Provider::Claude,
            Some("claude:s:agent:c"),
            None,
            None,
            &[a, b],
            Some("b"),
            81,
        );
        if let Decision::Selected { lease, moves, .. } = result {
            assert!(lease.after_move);
            assert_eq!(moves.len(), 1);
            assert_eq!(moves[0].keys, 2);
            assert_eq!(moves[0].bytes, 81);
            assert_eq!(moves[0].reason, "forced");
            assert!(moves[0].debug_pin);
        } else {
            panic!("pinned group not selected");
        }
        assert_eq!(t.account_stats("a").0, 0);
        assert_eq!(t.account_stats("b").0, 2);
    }

    #[test]
    fn inconsistent_group_is_salvaged_without_losing_valid_groups() {
        let now = Utc::now();
        let d = decode(json!({"schema":1,"bindings":[
            {"key":"claude:bad","account":"a","last_used":now},
            {"key":"claude:bad:agent:c","account":"b","last_used":now},
            {"key":"codex:good","account":"a","last_used":now}
        ]}))
        .unwrap();
        assert_eq!(d.bindings.len(), 1);
        assert!(d.bindings.contains_key("codex:good"));
        assert_eq!(d.groups.len(), 1);
        assert_eq!(d.accounts["a"].count, 1);
        assert!(d.traffic.is_empty());
        assert!(d.dirty);
    }

    #[test]
    fn poisoned_mutexes_recover_for_selection_stats_completion_and_flush() {
        let (_dir, t) = table();
        drop(choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = t.data.lock().unwrap();
            panic!("synthetic poison");
        }));
        assert!(panic.is_err());
        assert_eq!(t.current(Some("claude:s")).as_deref(), Some("a"));
        assert_eq!(t.count(), 1);
        assert_eq!(t.account_stats("a").0, 1);
        drop(choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]));
        let pin = t.pin(Some("claude:s"));
        drop(pin);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = t.writer.lock().unwrap();
            panic!("synthetic writer poison");
        }));
        t.flush().unwrap();
    }

    #[test]
    fn pinned_socket_keeps_cold_binding_even_when_gate_fails() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 30, 2);
        let b = candidate("b", 5.0, 8, 1);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        let pin = t.pin(Some("claude:s"));
        age(&t, "claude:s", 3601);
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(91);
        }
        let (id, lease, moves) = choose(&t, Some("claude:s"), &[a.clone(), b.clone()]);
        assert_eq!(id, "a");
        assert!(moves.is_empty());
        drop(lease);
        drop(pin);
        age(&t, "claude:s", 3601);
        assert_eq!(choose(&t, Some("claude:s"), &[a, b]).0, "b");
    }
    fn assert_indexes(d: &Data) {
        let mut expected = d.clone();
        rebuild_indexes(&mut expected);
        assert_eq!(d.lru, expected.lru);
        assert_eq!(d.groups.len(), expected.groups.len());
        assert_eq!(d.accounts.len(), expected.accounts.len());
        for (g, entry) in &d.groups {
            let next = &expected.groups[g];
            assert_eq!(entry.keys, next.keys);
            assert_eq!(entry.account, next.account);
            assert_eq!(entry.last_used, next.last_used);
        }
        for (id, entry) in &d.accounts {
            assert_eq!(entry.count, expected.accounts[id].count);
            assert_eq!(entry.recent, expected.accounts[id].recent);
        }
    }

    #[test]
    fn indexes_track_group_moves_completion_pin_release_and_expiry() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 10.0, 30, 2);
        let (_, old, _) = choose(&t, Some("claude:s"), std::slice::from_ref(&a));
        drop(t.choose(
            Provider::Claude,
            Some("claude:s:agent:c"),
            Some("claude:s"),
            None,
            std::slice::from_ref(&a),
            None,
            0,
        ));
        let pin1 = t.pin(Some("claude:s"));
        let pin2 = t.pin(Some("claude:s"));
        assert_indexes(&t.data.lock().unwrap());
        a.account.user_state = UserState::Paused;
        let (_, new, _) = choose(&t, Some("claude:s"), &[a, b]);
        assert_indexes(&t.data.lock().unwrap());
        drop(old);
        assert_eq!(t.account_stats("a"), (0, None));
        assert_eq!(t.account_stats("b").0, 2);
        drop(new);
        drop(pin1);
        assert_indexes(&t.data.lock().unwrap());
        assert!(t.data.lock().unwrap().lru.is_empty());
        drop(pin2);
        assert_indexes(&t.data.lock().unwrap());
        assert_eq!(t.data.lock().unwrap().lru.len(), 1);
        t.touch(Some("claude:s:agent:unknown"));
        assert_indexes(&t.data.lock().unwrap());
        age(&t, "claude:s", 700000);
        assert_eq!(t.count(), 0);
        let d = t.data.lock().unwrap();
        assert!(d.groups.is_empty() && d.accounts.is_empty() && d.lru.is_empty());
    }
    #[test]
    fn completion_does_not_record_traffic_newer_than_response_observation() {
        let (_dir, t) = table();
        let lease = t.begin_request(None, "a");
        let started = t.data.lock().unwrap().traffic["a"];
        let mut c = candidate("a", 91.0, 30, 2);
        let observed = started + Delta::seconds(1);
        observe_at(&mut c, observed);
        drop(lease);
        let d = t.data.lock().unwrap();
        assert_eq!(d.traffic["a"], started);
        let m = metrics(&c, &d, &t.config, observed + Delta::minutes(20));
        assert!(!m.unknown);
        assert!(!m.gate);
    }

    #[test]
    fn weekly_cohort_includes_exact_three_hours_but_not_one_second_more() {
        let (_dir, t) = table();
        let now = Utc::now();
        let mut a = candidate("a", 10.0, 8, 4);
        let mut b = candidate("b", 10.0, 11, 2);
        let mut c = candidate("c", 10.0, 11, 1);
        a.quota.as_mut().unwrap()["windows"][1]["reset_at"] = json!(now + Delta::hours(8));
        b.quota.as_mut().unwrap()["windows"][1]["reset_at"] = json!(now + Delta::hours(11));
        c.quota.as_mut().unwrap()["windows"][1]["reset_at"] =
            json!(now + Delta::hours(11) + Delta::seconds(1));
        assert_eq!(preview(&t, &[c.clone(), b.clone(), a.clone()]), "b");
        assert_eq!(preview(&t, &[a, c, b]), "b");
    }

    #[test]
    fn missing_weekly_reset_sorts_after_known_then_uses_five_hour_order() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let mut b = candidate("b", 10.0, 30, 2);
        a.quota.as_mut().unwrap()["windows"][1]["reset_at"] = Value::Null;
        assert_eq!(preview(&t, &[a.clone(), b.clone()]), "b");
        b.quota.as_mut().unwrap()["windows"][1]["reset_at"] = Value::Null;
        assert_eq!(preview(&t, &[b, a]), "a");
    }

    #[test]
    fn twenty_thousand_group_lookups_stats_and_completion_keep_indexes_consistent() {
        let (_dir, t) = table();
        let now = Utc::now();
        let mut bindings = Vec::with_capacity(CAPACITY);
        for i in 0..CAPACITY {
            bindings.push(json!({
                "key":format!("claude:g{i}"),
                "account":if i % 2 == 0 { "a" } else { "b" },
                "last_used":now
            }));
        }
        t.import(json!({"schema":1,"bindings":bindings})).unwrap();
        assert_eq!(t.count(), CAPACITY);
        assert_eq!(t.account_stats("a"), (CAPACITY / 2, Some(now)));
        assert_eq!(t.account_stats("b"), (CAPACITY / 2, Some(now)));
        // Missing child key resolves directly through its group index.
        assert_eq!(
            t.current(Some("claude:g0:agent:unknown")).as_deref(),
            Some("a")
        );
        let a = candidate("a", 10.0, 30, 2);
        let (id, lease, moves) = choose(&t, Some("claude:g0"), &[a]);
        assert_eq!(id, "a");
        assert!(moves.is_empty());
        assert_eq!(t.data.lock().unwrap().lru.len(), CAPACITY - 1);
        drop(lease);
        assert_eq!(t.data.lock().unwrap().lru.len(), CAPACITY);
        assert_eq!(t.account_stats("a").0, CAPACITY / 2);
        assert!(t.account_stats("a").1.unwrap() >= now);
        assert_indexes(&t.data.lock().unwrap());
    }
    #[test]
    fn projected_99_or_100_keeps_single_actual_eligible_account_in_2b() {
        let (_dir, t) = table();
        let mut a = codex_candidate("a", 98.0);
        let reservation = t.begin_request(None, "a");
        let m = metrics(&a, &t.data.lock().unwrap(), &t.config, Utc::now());
        assert_eq!(m.usage, 99.0);
        assert!(!m.gate);
        for pending in [1, 2] {
            a.inflight = pending;
            let result = t.choose(
                Provider::Codex,
                Some("codex:2b"),
                None,
                None,
                std::slice::from_ref(&a),
                None,
                0,
            );
            assert!(matches!(result, Decision::Selected { account, .. } if account == "a"));
        }
        // Projection still directs a new group to an account passing the gate.
        let b = codex_candidate("b", 10.0);
        assert_eq!(
            t.preview(
                Provider::Codex,
                Some("codex:new"),
                None,
                None,
                &[a.clone(), b],
                None,
                0
            ),
            PreviewDecision::Selected {
                account: "b".into()
            }
        );
        // Observed credit thresholds remain actual C5 exclusions.
        for observed in [99, 100] {
            for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
                w["used_pct"] = json!(observed);
            }
            assert!(matches!(
                t.choose(
                    Provider::Codex,
                    Some("codex:2b"),
                    None,
                    None,
                    std::slice::from_ref(&a),
                    None,
                    0
                ),
                Decision::None
            ));
        }
        drop(reservation);
    }
    #[test]
    fn preview_at_twenty_thousand_groups_uses_read_only_indexes_and_reservations() {
        let (_dir, t) = table();
        let now = Utc::now();
        let bindings: Vec<_> = (0..CAPACITY)
            .map(|i| {
                json!({
                    "key":format!("claude:g{i}"), "account":"a", "last_used":now
                })
            })
            .collect();
        t.import(json!({"schema":1,"bindings":bindings})).unwrap();
        let a = candidate("a", 89.0, 8, 1);
        let b = candidate("b", 10.0, 30, 2);
        let reservation = t.begin_request(None, "a");
        let before = t.export().unwrap();
        let revision = t.data.lock().unwrap().revision;
        for key in [Some("claude:g0"), Some("claude:g19999:agent:missing")] {
            assert!(matches!(
                t.preview(
                    Provider::Claude,
                    key,
                    None,
                    None,
                    &[a.clone(), b.clone()],
                    None,
                    0
                ),
                PreviewDecision::Selected { .. }
            ));
        }
        assert_eq!(
            t.preview(
                Provider::Claude,
                Some("claude:new"),
                None,
                None,
                &[a.clone(), b.clone()],
                None,
                0
            ),
            PreviewDecision::Selected {
                account: "b".into()
            }
        );
        assert_eq!(t.export().unwrap(), before);
        assert_eq!(t.data.lock().unwrap().revision, revision);
        assert_eq!(t.pending_count("a"), 1);
        // An expired binding is absent in the read view, while its persisted
        // entry and all indexes remain untouched until ordinary pruning.
        age(&t, "claude:g0", 700000);
        let before = t.export().unwrap();
        assert_eq!(
            t.preview(
                Provider::Claude,
                Some("claude:g0"),
                None,
                None,
                &[a, b],
                None,
                0
            ),
            PreviewDecision::Selected {
                account: "b".into()
            }
        );
        assert_eq!(t.export().unwrap(), before);
        assert_eq!(t.data.lock().unwrap().bindings.len(), CAPACITY);
        assert_indexes(&t.data.lock().unwrap());
        drop(reservation);
    }
}
