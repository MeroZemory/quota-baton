//! Durable local routing intent. Display labels and upstream payloads are never selectors.
pub mod forecast;
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_SESSIONS: usize = 20_000;
const MAX_PROJECTS: usize = 1024;
const MAX_FILE: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Strict,
    Recover,
    #[default]
    Rebalance,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChurnLimits {
    pub dwell_secs: u64,
    pub revisit_secs: u64,
    pub window_secs: u64,
    pub max_group_moves: usize,
    pub max_pool_moves: usize,
    pub target_quiet_secs: u64,
    pub target_success_secs: u64,
    pub max_move_bytes: u64,
}
impl Default for ChurnLimits {
    fn default() -> Self {
        Self {
            dwell_secs: 1800,
            revisit_secs: 3600,
            window_secs: 600,
            max_group_moves: 3,
            max_pool_moves: 4,
            target_quiet_secs: 120,
            target_success_secs: 300,
            max_move_bytes: 8 * 1024 * 1024,
        }
    }
}
impl ChurnLimits {
    pub fn validate(&self) -> Result<()> {
        if self.dwell_secs > 86400
            || self.revisit_secs > 86400
            || self.window_secs == 0
            || self.window_secs > 86400
            || !(1..=16).contains(&self.max_group_moves)
            || !(1..=32).contains(&self.max_pool_moves)
            || self.target_quiet_secs > 3600
            || self.target_success_secs == 0
            || self.target_success_secs > 900
            || self.max_move_bytes == 0
            || self.max_move_bytes > 64 * 1024 * 1024
        {
            bail!("invalid routing churn limits");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub mode: Mode,
    pub reset_wait_secs: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Rebalance,
            reset_wait_secs: 60,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.reset_wait_secs > 120 {
            bail!("routing wait exceeds 120 seconds");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: String,
    pub settings: Settings,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Movement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    from: String,
    to: String,
    at: DateTime<Utc>,
    discretionary: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SessionPolicy {
    #[serde(deserialize_with = "required_optional")]
    project: Option<String>,
    baseline: Settings,
    #[serde(deserialize_with = "required_optional")]
    explicit: Option<Settings>,
    #[serde(deserialize_with = "required_optional")]
    anchor: Option<String>,
    movements: Vec<Movement>,
}
impl SessionPolicy {
    fn settings<'a>(&'a self, defaults: &'a Settings) -> &'a Settings {
        self.explicit.as_ref().unwrap_or(if self.project.is_some() {
            &self.baseline
        } else {
            defaults
        })
    }
}
fn required_optional<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    revision: u64,
    projects: BTreeMap<String, Project>,
    sessions: BTreeMap<String, SessionPolicy>,
    legacy_codex_roots: BTreeSet<String>,
    #[serde(skip)]
    pool_moves: BTreeMap<String, Vec<DateTime<Utc>>>,
    #[serde(skip)]
    arrivals: BTreeMap<String, DateTime<Utc>>,
    #[serde(skip)]
    frozen: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            schema: 2,
            revision: 0,
            projects: BTreeMap::new(),
            sessions: BTreeMap::new(),
            legacy_codex_roots: BTreeSet::new(),
            pool_moves: BTreeMap::new(),
            arrivals: BTreeMap::new(),
            frozen: false,
        }
    }
}
impl State {
    fn policy_id(&self, key: &str) -> String {
        let exact = session_id(key);
        if key.starts_with("codex:")
            && key.contains(":agent:")
            && !self.sessions.contains_key(&exact)
        {
            let legacy = session_id(key.split(":agent:").next().unwrap_or(key));
            if self.legacy_codex_roots.contains(&legacy) {
                return legacy;
            }
        }
        exact
    }
    fn reindex(&mut self) {
        self.pool_moves.clear();
        self.arrivals.clear();
        for (root, session) in &mut self.sessions {
            for (index, movement) in session.movements.iter_mut().enumerate() {
                if movement.id.is_none() {
                    // Give old records a stable identity before any alias clones them.
                    let raw = format!(
                        "{root}:{index}:{}:{}:{}:{}",
                        movement.from, movement.to, movement.at, movement.discretionary
                    );
                    movement.id = Some(format!("{:x}", Sha256::digest(raw.as_bytes())));
                }
            }
        }
        let mut seen = BTreeSet::new();
        for movement in self.sessions.values().flat_map(|s| &s.movements) {
            if !seen.insert(movement.id.as_ref().expect("indexed movement identity")) {
                continue;
            }
            self.arrivals
                .entry(movement.to.clone())
                .and_modify(|at| *at = (*at).max(movement.at))
                .or_insert(movement.at);
            if movement.discretionary {
                self.pool_moves
                    .entry(
                        movement
                            .to
                            .split('-')
                            .next()
                            .unwrap_or(&movement.to)
                            .to_owned(),
                    )
                    .or_default()
                    .push(movement.at);
            }
        }
        for times in self.pool_moves.values_mut() {
            times.sort_unstable_by(|a, b| b.cmp(a));
            times.truncate(32);
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub settings: Settings,
    pub anchor: Option<String>,
    pub project: Option<String>,
    pub source: &'static str,
    pub revision: u64,
}
pub struct PolicyStore {
    path: PathBuf,
    defaults: Settings,
    limits: ChurnLimits,
    state: Mutex<Arc<State>>,
    writer: Mutex<()>,
    pub(crate) changed: tokio::sync::Notify,
}

pub fn root(key: &str) -> &str {
    if key.starts_with("claude:") {
        key.split(":agent:").next().unwrap_or(key)
    } else {
        key
    }
}
pub fn session_id(key: &str) -> String {
    format!("{:x}", Sha256::digest(root(key).as_bytes()))
}
pub fn valid_id(id: &str, length: usize) -> bool {
    id.len() == length
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn valid_project(id: &str) -> bool {
    valid_id(id, 32)
}

pub const PROJECT_HEADER: &str = "x-quota-baton-project";
#[derive(Clone)]
pub struct ProjectHint(pub Option<String>);
/// Read once and remove every occurrence, including malformed/duplicate hints.
pub fn take_project(headers: &mut axum::http::HeaderMap) -> Result<Option<String>> {
    let values: Vec<_> = headers.get_all(PROJECT_HEADER).iter().cloned().collect();
    headers.remove(PROJECT_HEADER);
    match values.as_slice() {
        [] => Ok(None),
        [one] => {
            let id = one.to_str()?;
            if !valid_project(id) {
                bail!("invalid routing project hint");
            }
            Ok(Some(id.to_owned()))
        }
        _ => bail!("duplicate routing project hint"),
    }
}

fn load(path: &Path) -> Result<State> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.len() > MAX_FILE => bail!("routing policy file exceeds cap"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(e) => return Err(e.into()),
        Ok(metadata) => metadata,
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("routing policy must be a regular non-link file");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("routing policy reparse point refused");
        }
        crate::local_secrets::restrict(path)?;
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options.open(path)?;
    let actual = file.metadata()?;
    if !actual.is_file() || actual.len() > MAX_FILE {
        bail!("invalid routing policy handle");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if actual.file_attributes() & 0x400 != 0 {
            bail!("routing policy reparse handle refused");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if actual.uid() != unsafe { libc::geteuid() } || actual.mode() & 0o077 != 0 {
            bail!("routing policy requires owner-only permissions");
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if actual.file_attributes() & 0x400 != 0 {
            bail!("routing policy reparse handle refused");
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        bail!("routing policy read cap exceeded");
    }
    // A corrupt intent file must stop startup, never silently remove strict pins.
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value["schema"] == 1 {
        if value.get("legacy_codex_roots").is_some() {
            bail!("invalid legacy policy schema");
        }
        value["legacy_codex_roots"] = value["sessions"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("invalid legacy sessions"))?
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .into();
        value["schema"] = 2.into();
    }
    let mut state: State = serde_json::from_value(value)?;
    if state.schema != 2
        || state.projects.len() > MAX_PROJECTS
        || state.sessions.len() > MAX_SESSIONS
        || state
            .legacy_codex_roots
            .iter()
            .any(|id| !state.sessions.contains_key(id))
    {
        bail!("invalid routing policy state");
    }
    for (id, project) in &state.projects {
        if !valid_project(id)
            || project.name.is_empty()
            || project.name.chars().count() > 80
            || project.name.chars().any(char::is_control)
        {
            bail!("invalid routing project");
        }
        project.settings.validate()?;
    }
    for (id, session) in &state.sessions {
        if !valid_id(id, 64)
            || session.movements.len() > 32
            || session
                .project
                .as_ref()
                .is_some_and(|p| !state.projects.contains_key(p))
        {
            bail!("invalid routing session");
        }
        if session
            .anchor
            .as_deref()
            .is_some_and(|a| crate::observability::safe_identifier(a).is_none())
            || session.movements.iter().any(|m| {
                m.id.as_deref()
                    .is_some_and(|id| !valid_id(id, 32) && !valid_id(id, 64))
                    || crate::observability::safe_identifier(&m.from).is_none()
                    || crate::observability::safe_identifier(&m.to).is_none()
            })
        {
            bail!("invalid routing account identifier");
        }
        session.baseline.validate()?;
        if let Some(settings) = &session.explicit {
            settings.validate()?;
        }
    }
    let mut movement_ids = BTreeMap::new();
    for movement in state
        .sessions
        .values()
        .flat_map(|session| &session.movements)
    {
        if let Some(id) = &movement.id
            && let Some(previous) = movement_ids.insert(id, movement)
            && previous != movement
        {
            bail!("conflicting routing movement identity");
        }
    }
    state.reindex();
    Ok(state)
}

impl PolicyStore {
    pub fn new(home: &Path, defaults: Settings) -> Result<Self> {
        defaults.validate()?;
        let path = home.join("state/routing-policies.json");
        fs::create_dir_all(path.parent().expect("policy parent"))?;
        let state = load(&path)?;
        Ok(Self {
            path,
            defaults,
            limits: ChurnLimits::default(),
            state: Mutex::new(Arc::new(state)),
            writer: Mutex::new(()),
            changed: tokio::sync::Notify::new(),
        })
    }
    pub fn with_limits(mut self, limits: ChurnLimits) -> Result<Self> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }
    pub fn limits(&self) -> &ChurnLimits {
        &self.limits
    }
    fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let state = self.snapshot();
        if state.frozen {
            bail!("routing policy writer retired");
        }
        let mut next = (*state).clone();
        let result = f(&mut next)?;
        if next.sessions.len() > MAX_SESSIONS || next.projects.len() > MAX_PROJECTS {
            bail!("routing policy capacity reached");
        }
        next.revision = next.revision.wrapping_add(1);
        let bytes = serde_json::to_vec(&next)?;
        if bytes.len() as u64 > MAX_FILE {
            bail!("routing policy file exceeds cap");
        }
        if let Err(error) = crate::bindings::atomic_write(&self.path, &bytes) {
            // Replace may have succeeded before a durability failure. Do not
            // keep authorizing from an older in-memory revision afterwards.
            let mut retired = (*state).clone();
            retired.frozen = true;
            *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(retired);
            self.changed.notify_waiters();
            return Err(error.into());
        }
        next.reindex();
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        self.changed.notify_waiters();
        Ok(result)
    }
    pub fn freeze(&self) {
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = (*self.snapshot()).clone();
        next.frozen = true;
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
    }
    pub fn reload(&self) -> Result<()> {
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let state = self.snapshot();
        if state.frozen {
            bail!("routing policy writer retired");
        }
        let next = load(&self.path)?;
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        Ok(())
    }
    fn snapshot(&self) -> Arc<State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Capture existing locks before the disposable binding LRU is pruned.
    pub fn capture_strict_anchors<'a>(
        &self,
        bindings: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<()> {
        let state = self.snapshot();
        let missing: BTreeMap<_, _> = bindings
            .into_iter()
            .filter_map(|(key, account)| {
                let id = state.policy_id(key);
                let plan = self.resolve(&state, &id);
                (plan.settings.mode == Mode::Strict && plan.anchor.is_none())
                    .then_some((id, account.to_owned()))
            })
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        self.update(|state| {
            for (id, account) in missing {
                let entry = state.sessions.entry(id).or_insert_with(|| SessionPolicy {
                    project: None,
                    baseline: self.defaults.clone(),
                    explicit: None,
                    anchor: None,
                    movements: Vec::new(),
                });
                if entry.anchor.is_none() {
                    entry.anchor = Some(account);
                }
            }
            Ok(())
        })
    }
    /// A queued request cannot use an obsolete account after policy changes.
    pub fn permits_dispatch(&self, key: Option<&str>, account: &str) -> bool {
        let state = self.snapshot();
        if state.frozen {
            return false;
        }
        let Some(key) = key else {
            return self.defaults.mode != Mode::Strict;
        };
        let plan = self.resolve(&state, &state.policy_id(key));
        plan.settings.mode != Mode::Strict || plan.anchor.as_deref() == Some(account)
    }
    fn resolve(&self, state: &State, id: &str) -> Plan {
        let session = state.sessions.get(id);
        Plan {
            settings: session.map_or_else(
                || self.defaults.clone(),
                |s| s.settings(&self.defaults).clone(),
            ),
            anchor: session.and_then(|s| s.anchor.clone()),
            project: session.and_then(|s| s.project.clone()),
            source: session.map_or("global", |s| {
                if s.explicit.is_some() {
                    "session"
                } else if s.project.is_some() {
                    "project"
                } else {
                    "global"
                }
            }),
            revision: state.revision,
        }
    }
    pub fn plan(&self, key: Option<&str>) -> Plan {
        let state = self.snapshot();
        self.resolve(&state, &key.map(|k| state.policy_id(k)).unwrap_or_default())
    }
    pub fn list(&self) -> serde_json::Value {
        let state = self.snapshot();
        serde_json::json!({"revision":state.revision,"defaults":self.defaults,"projects":state.projects,"sessions":state.sessions.keys().map(|id| (id.clone(), self.resolve(&state,id))).collect::<BTreeMap<_,_>>()})
    }
    pub fn put_project(&self, id: Option<&str>, project: Project) -> Result<String> {
        project.settings.validate()?;
        if project.name.is_empty()
            || project.name.chars().count() > 80
            || project.name.chars().any(char::is_control)
        {
            bail!("invalid routing project name");
        }
        let id = id
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{:032x}", rand::random::<u128>()));
        if !valid_project(&id) {
            bail!("invalid routing project id");
        }
        self.update(|state| {
            state.projects.insert(id.clone(), project);
            Ok(id)
        })
    }
    pub fn set_session(
        &self,
        id: &str,
        explicit: Option<Settings>,
        project: Option<&str>,
        anchor: Option<&str>,
    ) -> Result<()> {
        self.edit_session(id, explicit, project, anchor, true)
    }
    pub fn edit_session(
        &self,
        id: &str,
        explicit: Option<Settings>,
        project: Option<&str>,
        anchor: Option<&str>,
        inherit: bool,
    ) -> Result<()> {
        self.edit_session_for_key(id, explicit, project, anchor, inherit, None)
    }
    pub fn edit_session_for_key(
        &self,
        id: &str,
        explicit: Option<Settings>,
        project: Option<&str>,
        anchor: Option<&str>,
        inherit: bool,
        key: Option<&str>,
    ) -> Result<()> {
        if explicit.is_none() && project.is_none() && !inherit {
            bail!("routing edit requires policy, project or explicit inherit");
        }
        if !valid_id(id, 64) {
            bail!("invalid routing session id");
        }
        if let Some(settings) = &explicit {
            settings.validate()?;
        }
        self.update(|state| {
            if !state.sessions.contains_key(id) {
                if let Some(key) = key {
                    if session_id(key) != id {
                        bail!("routing handle does not match observed root");
                    }
                    let effective = state.policy_id(key);
                    if effective != id
                        && let Some(legacy) = state.sessions.get(&effective).cloned()
                    {
                        state.sessions.insert(id.to_owned(), legacy);
                    }
                } else if project.is_some()
                    && explicit.is_none()
                    && !inherit
                    && !state.legacy_codex_roots.is_empty()
                {
                    bail!(
                        "unobserved legacy root needs an explicit policy or an observed identity"
                    );
                }
            }
            let baseline = match project {
                Some(p) => state
                    .projects
                    .get(p)
                    .ok_or_else(|| anyhow::anyhow!("unknown routing project"))?
                    .settings
                    .clone(),
                None => state
                    .sessions
                    .get(id)
                    .map_or_else(|| self.defaults.clone(), |s| s.baseline.clone()),
            };
            let entry = state
                .sessions
                .entry(id.to_owned())
                .or_insert(SessionPolicy {
                    project: None,
                    baseline: baseline.clone(),
                    explicit: None,
                    anchor: anchor.map(str::to_owned),
                    movements: Vec::new(),
                });
            if let Some(project) = project {
                entry.project = Some(project.to_owned());
                entry.baseline = baseline;
            }
            if explicit.is_some() || inherit {
                entry.explicit = explicit;
            }
            // Editing policy never silently chooses another account.
            if entry.anchor.is_none() {
                entry.anchor = anchor.map(str::to_owned);
            }
            Ok(())
        })
    }
    /// Ingress association is explicit, stable, and cannot reassign an existing root.
    pub fn associate(&self, key: &str, project: &str) -> Result<()> {
        self.associate_with_anchor(key, project, None)
    }
    pub fn associate_with_anchor(
        &self,
        key: &str,
        project: &str,
        anchor: Option<&str>,
    ) -> Result<()> {
        let id = self.snapshot().policy_id(key);
        {
            let state = self.snapshot();
            if let Some(session) = state.sessions.get(&id) {
                if session.project.as_deref() == Some(project)
                    && (session.anchor.is_some() || anchor.is_none())
                {
                    return Ok(());
                }
                if session.project.as_deref() != Some(project) {
                    bail!("routing project conflicts with existing session");
                }
            }
        }
        self.update(|state| {
            if let Some(session) = state.sessions.get_mut(&id) {
                if session.project.as_deref() == Some(project) {
                    if session.anchor.is_none() {
                        session.anchor = anchor.map(str::to_owned);
                    }
                    return Ok(());
                }
                bail!("routing project conflicts with existing session");
            }
            let baseline = state
                .projects
                .get(project)
                .ok_or_else(|| anyhow::anyhow!("unknown routing project"))?
                .settings
                .clone();
            state.sessions.insert(
                id,
                SessionPolicy {
                    project: Some(project.to_owned()),
                    baseline,
                    explicit: None,
                    anchor: anchor.map(str::to_owned),
                    movements: Vec::new(),
                },
            );
            Ok(())
        })
    }
    pub fn discretionary_allowed(
        &self,
        key: &str,
        target: Option<&str>,
        now: DateTime<Utc>,
    ) -> bool {
        let state = self.snapshot();
        let prefix = if key.starts_with("claude:") {
            "cl"
        } else {
            "cx"
        };
        let pool_moves = state
            .pool_moves
            .get(prefix)
            .into_iter()
            .flatten()
            .filter(|at| (now - **at).num_seconds() < self.limits.window_secs as i64)
            .count();
        if pool_moves >= self.limits.max_pool_moves
            || target.is_some_and(|target| {
                state.arrivals.get(target).is_some_and(|at| {
                    (now - *at).num_seconds() < self.limits.target_quiet_secs as i64
                })
            })
        {
            return false;
        }
        let Some(session) = state.sessions.get(&state.policy_id(key)) else {
            return self.defaults.mode == Mode::Rebalance;
        };
        session.settings(&self.defaults).mode == Mode::Rebalance
            && session
                .movements
                .iter()
                .filter(|m| (now - m.at).num_seconds() < self.limits.window_secs as i64)
                .count()
                < self.limits.max_group_moves
            && !session.movements.iter().any(|m| {
                (m.discretionary && (now - m.at).num_seconds() < self.limits.dwell_secs as i64)
                    || target.is_some_and(|target| {
                        target == m.from
                            && (now - m.at).num_seconds() < self.limits.revisit_secs as i64
                    })
            })
    }
    /// Persist before a strict first dispatch or any account change. No partial memory commit.
    pub fn dispatch(
        &self,
        key: Option<&str>,
        account: &str,
        previous: Option<&str>,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let Some(key) = key else {
            return Ok(());
        };
        let id = self.snapshot().policy_id(key);
        let discretionary = matches!(reason, "overload" | "cold_rebalance");
        {
            let state = self.snapshot();
            if state.frozen {
                bail!("routing policy writer retired");
            }
            let plan = self.resolve(&state, &id);
            let anchor = plan.anchor.as_deref().or(previous);
            if plan.settings.mode == Mode::Strict && anchor.is_some_and(|a| a != account) {
                bail!("routing session is locked");
            }
            if anchor == Some(account)
                && (plan.settings.mode != Mode::Strict
                    || state.sessions.get(&id).is_some_and(|s| s.anchor.is_some()))
            {
                return Ok(());
            }
            if anchor.is_none()
                && plan.settings.mode != Mode::Strict
                && !state.sessions.contains_key(&id)
            {
                return Ok(());
            }
        }
        self.update(|state| {
            if discretionary {
                let prefix = account.split('-').next().unwrap_or(account);
                // The immutable index de-duplicates inherited legacy history.
                let pool_moves = state
                    .pool_moves
                    .get(prefix)
                    .into_iter()
                    .flatten()
                    .filter(|at| (now - **at).num_seconds() < self.limits.window_secs as i64)
                    .count();
                if pool_moves >= self.limits.max_pool_moves {
                    bail!("routing pool movement budget exhausted");
                }
                if state
                    .sessions
                    .values()
                    .flat_map(|s| s.movements.iter())
                    .any(|m| {
                        m.to == account
                            && (now - m.at).num_seconds() < self.limits.target_quiet_secs as i64
                    })
                {
                    bail!("routing destination inflow budget exhausted");
                }
            }
            let entry = state.sessions.entry(id).or_insert_with(|| SessionPolicy {
                project: None,
                baseline: self.defaults.clone(),
                explicit: None,
                anchor: previous.map(str::to_owned),
                movements: Vec::new(),
            });
            let from = entry.anchor.as_deref().or(previous).map(str::to_owned);
            if entry.settings(&self.defaults).mode == Mode::Strict
                && from.as_deref().is_some_and(|a| a != account)
            {
                bail!("routing session is locked");
            }
            if let Some(from) = from.filter(|from| from != account) {
                if discretionary && entry.settings(&self.defaults).mode != Mode::Rebalance {
                    bail!("routing discretionary move forbidden");
                }
                entry
                    .movements
                    .retain(|m| (now - m.at).num_seconds() < 24 * 3600);
                if discretionary
                    && entry.movements.iter().any(|m| {
                        (m.discretionary
                            && (now - m.at).num_seconds() < self.limits.dwell_secs as i64)
                            || (m.from == account
                                && (now - m.at).num_seconds() < self.limits.revisit_secs as i64)
                    })
                {
                    bail!("routing dwell or revisit budget");
                }
                let recent: Vec<_> = entry
                    .movements
                    .iter()
                    .filter(|m| (now - m.at).num_seconds() < self.limits.window_secs as i64)
                    .collect();
                if recent.len() >= self.limits.max_group_moves {
                    bail!("routing movement budget exhausted");
                }
                entry.movements.push(Movement {
                    id: Some(format!("{:032x}", rand::random::<u128>())),
                    from,
                    to: account.to_owned(),
                    at: now,
                    discretionary,
                });
                if entry.movements.len() > 32 {
                    bail!("routing history capacity reached");
                }
            }
            entry.anchor = Some(account.to_owned());
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn home() -> tempfile::TempDir {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/routing-tests");
        fs::create_dir_all(&path).unwrap();
        tempfile::tempdir_in(path).unwrap()
    }
    fn settings(mode: Mode) -> Settings {
        Settings {
            mode,
            reset_wait_secs: 7,
        }
    }
    #[test]
    fn same_project_empty_anchor_is_completed_without_reassignment() {
        let dir = home();
        let store = PolicyStore::new(dir.path(), settings(Mode::Rebalance)).unwrap();
        let project = store
            .put_project(
                None,
                Project {
                    name: "fixture".into(),
                    settings: settings(Mode::Recover),
                },
            )
            .unwrap();
        store.associate("codex:anchor-fill", &project).unwrap();
        store
            .associate_with_anchor("codex:anchor-fill", &project, Some("cx-a"))
            .unwrap();
        store.reload().unwrap();
        assert_eq!(
            store.plan(Some("codex:anchor-fill")).anchor.as_deref(),
            Some("cx-a")
        );
        assert!(
            store
                .associate_with_anchor("codex:anchor-fill", "other", Some("cx-b"))
                .is_err()
        );
    }
    #[test]
    fn legacy_alias_copy_keeps_pool_count_and_distinct_real_movements_after_restart() {
        let dir = home();
        let store = PolicyStore::new(dir.path(), settings(Mode::Rebalance)).unwrap();
        let now = Utc::now();
        store
            .update(|state| {
                for key in ["codex:legacy", "codex:other1", "codex:other2"] {
                    state.sessions.insert(
                        session_id(key),
                        SessionPolicy {
                            project: None,
                            baseline: settings(Mode::Rebalance),
                            explicit: Some(settings(Mode::Rebalance)),
                            anchor: Some("cx-b".into()),
                            movements: vec![Movement {
                                id: None,
                                from: "cx-a".into(),
                                to: "cx-b".into(),
                                at: now,
                                discretionary: true,
                            }],
                        },
                    );
                }
                state.legacy_codex_roots.insert(session_id("codex:legacy"));
                Ok(())
            })
            .unwrap();
        assert_eq!(store.snapshot().pool_moves["cx"].len(), 3);
        let project = store
            .put_project(
                None,
                Project {
                    name: "legacy".into(),
                    settings: settings(Mode::Rebalance),
                },
            )
            .unwrap();
        let key = "codex:legacy:agent:one";
        store
            .edit_session_for_key(
                &session_id(key),
                None,
                Some(&project),
                Some("cx-b"),
                false,
                Some(key),
            )
            .unwrap();
        assert_eq!(store.snapshot().pool_moves["cx"].len(), 3);
        store.reload().unwrap();
        assert_eq!(store.snapshot().pool_moves["cx"].len(), 3);
        assert!(store.discretionary_allowed("codex:new", None, now));
        let mut bad: serde_json::Value =
            serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
        let id = bad["sessions"][session_id("codex:legacy")]["movements"][0]["id"].clone();
        bad["sessions"][session_id("codex:other2")]["movements"][0]["id"] = id;
        bad["sessions"][session_id("codex:other2")]["movements"][0]["from"] = "cx-conflict".into();
        fs::write(&store.path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(
            store.reload().is_err(),
            "conflicting movement IDs were allowed to erase pool history"
        );
    }
    #[test]
    fn strict_anchor_survives_restart_and_child_inherits() {
        let home = home();
        let key = "claude:synthetic-root";
        let now = Utc::now();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .set_session(&session_id(key), Some(settings(Mode::Strict)), None, None)
            .unwrap();
        store
            .dispatch(Some(key), "cl-synthetic-a", None, "new", now)
            .unwrap();
        drop(store);
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let plan = store.plan(Some("claude:synthetic-root:agent:child"));
        assert_eq!(plan.settings.mode, Mode::Strict);
        assert_eq!(plan.anchor.as_deref(), Some("cl-synthetic-a"));
        assert!(
            store
                .dispatch(
                    Some(key),
                    "cl-synthetic-b",
                    Some("cl-synthetic-a"),
                    "forced",
                    now
                )
                .is_err()
        );
        assert!(
            store
                .dispatch(Some(key), "cl-synthetic-a", None, "forced", now)
                .is_ok()
        );
    }
    #[test]
    fn project_defaults_are_snapshotted_and_explicit_session_wins() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let id = store
            .put_project(
                None,
                Project {
                    name: "같은 이름".into(),
                    settings: settings(Mode::Strict),
                },
            )
            .unwrap();
        let other = store
            .put_project(
                None,
                Project {
                    name: "같은 이름".into(),
                    settings: settings(Mode::Recover),
                },
            )
            .unwrap();
        assert_ne!(id, other);
        store.associate("codex:old", &id).unwrap();
        store
            .put_project(
                Some(&id),
                Project {
                    name: "변경".into(),
                    settings: settings(Mode::Recover),
                },
            )
            .unwrap();
        store.associate("codex:new", &id).unwrap();
        assert_eq!(store.plan(Some("codex:old")).settings.mode, Mode::Strict);
        assert_eq!(store.plan(Some("codex:new")).settings.mode, Mode::Recover);
        assert!(store.associate("codex:old", &other).is_err());
        store
            .set_session(
                &session_id("codex:new"),
                Some(settings(Mode::Strict)),
                None,
                None,
            )
            .unwrap();
        assert_eq!(store.plan(Some("codex:new")).source, "session");
    }
    #[test]
    fn movement_budget_dwell_and_revisit_survive_restart() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let now = Utc::now();
        store
            .dispatch(Some("codex:moving"), "cx-b", Some("cx-a"), "overload", now)
            .unwrap();
        assert!(!store.discretionary_allowed(
            "codex:moving",
            Some("cx-c"),
            now + chrono::Duration::seconds(1799)
        ));
        assert!(!store.discretionary_allowed(
            "codex:moving",
            Some("cx-a"),
            now + chrono::Duration::seconds(1801)
        ));
        assert!(store.discretionary_allowed(
            "codex:moving",
            Some("cx-c"),
            now + chrono::Duration::seconds(1801)
        ));
        store
            .dispatch(Some("codex:moving"), "cx-c", Some("cx-b"), "forced", now)
            .unwrap();
        store
            .dispatch(Some("codex:moving"), "cx-d", Some("cx-c"), "forced", now)
            .unwrap();
        assert!(
            store
                .dispatch(Some("codex:moving"), "cx-e", Some("cx-d"), "forced", now)
                .is_err()
        );
        drop(store);
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert_eq!(
            store.plan(Some("codex:moving")).anchor.as_deref(),
            Some("cx-d")
        );
        assert!(!store.discretionary_allowed("codex:moving", None, now));
    }
    #[test]
    fn failed_persistence_never_commits_memory_and_freeze_stops_writes() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        fs::create_dir(&store.path).unwrap();
        assert!(
            store
                .put_project(
                    None,
                    Project {
                        name: "test".into(),
                        settings: Settings::default()
                    }
                )
                .is_err()
        );
        assert_eq!(store.list()["revision"], 0);
        assert_eq!(store.list()["projects"], serde_json::json!({}));
        store.freeze();
        assert!(
            store
                .set_session(
                    &session_id("codex:s"),
                    Some(settings(Mode::Strict)),
                    None,
                    None
                )
                .is_err()
        );
        assert!(store.reload().is_err());
    }
    #[test]
    fn corrupt_policy_refuses_startup_instead_of_forgetting_strict_intent() {
        let home = home();
        fs::create_dir_all(home.path().join("state")).unwrap();
        fs::write(home.path().join("state/routing-policies.json"), b"not-json").unwrap();
        assert!(PolicyStore::new(home.path(), Settings::default()).is_err());
    }
    #[test]
    fn indexed_policy_checks_remain_bounded_at_maximum_session_capacity() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .update(|state| {
                for n in 0..MAX_SESSIONS {
                    state.sessions.insert(
                        session_id(&format!("codex:capacity-{n}")),
                        SessionPolicy {
                            project: None,
                            baseline: Settings::default(),
                            explicit: None,
                            anchor: Some("cx-a".into()),
                            movements: Vec::new(),
                        },
                    );
                }
                Ok(())
            })
            .unwrap();
        let start = std::time::Instant::now();
        for n in 0..MAX_SESSIONS {
            assert!(store.discretionary_allowed(
                &format!("codex:capacity-{n}"),
                Some("cx-b"),
                Utc::now()
            ));
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "policy lookup revisited the whole table for every root"
        );
    }
    #[cfg(windows)]
    #[test]
    fn policy_create_replace_and_load_have_one_protected_owner_ace() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        for n in 0..2 {
            store
                .put_project(
                    None,
                    Project {
                        name: format!("fixture-{n}"),
                        settings: Settings::default(),
                    },
                )
                .unwrap();
            let saved = home.path().join("acl.txt");
            let output = std::process::Command::new("icacls.exe")
                .arg(&store.path)
                .arg("/save")
                .arg(&saved)
                .arg("/q")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "synthetic policy ACL inspection failed"
            );
            let bytes = fs::read(&saved).unwrap();
            let text = if bytes.starts_with(&[255, 254]) || bytes.contains(&0) {
                let offset = usize::from(bytes.starts_with(&[255, 254])) * 2;
                String::from_utf16(
                    &bytes[offset..]
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect::<Vec<_>>(),
                )
                .unwrap()
            } else {
                String::from_utf8(bytes).unwrap()
            };
            assert!(text.contains("D:P"), "policy ACL must be protected");
            assert_eq!(
                text.matches("(A;").count(),
                1,
                "only the current owner may access intent"
            );
            assert!(PolicyStore::new(home.path(), Settings::default()).is_ok());
        }
    }
    #[test]
    fn legacy_codex_agent_hash_keeps_strict_intent_through_schema_upgrade() {
        let home = home();
        fs::create_dir_all(home.path().join("state")).unwrap();
        let id = session_id("codex:legacy");
        let original = serde_json::json!({"schema":1,"revision":7,"projects":{},"sessions":{
            id:{"project":null,"baseline":{"mode":"rebalance","reset_wait_secs":60},
            "explicit":{"mode":"strict","reset_wait_secs":7},"anchor":"cx-a","movements":[]}}});
        fs::write(
            home.path().join("state/routing-policies.json"),
            serde_json::to_vec(&original).unwrap(),
        )
        .unwrap();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert_eq!(
            store.plan(Some("codex:legacy:agent:one")).settings.mode,
            Mode::Strict
        );
        assert!(!store.permits_dispatch(Some("codex:legacy:agent:one"), "cx-b"));
        store
            .put_project(
                None,
                Project {
                    name: "unrelated edit".into(),
                    settings: Settings::default(),
                },
            )
            .unwrap();
        drop(store);
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert!(!store.permits_dispatch(Some("codex:legacy:agent:two"), "cx-b"));
        assert_eq!(
            store.plan(Some("codex:legacy:agent:two")).anchor.as_deref(),
            Some("cx-a")
        );
    }
    #[test]
    fn implicit_history_follows_current_global_defaults_after_restart() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .dispatch(
                Some("codex:global"),
                "cx-b",
                Some("cx-a"),
                "forced",
                Utc::now(),
            )
            .unwrap();
        drop(store);
        let store = PolicyStore::new(home.path(), settings(Mode::Strict)).unwrap();
        assert_eq!(store.plan(Some("codex:global")).source, "global");
        assert_eq!(store.plan(Some("codex:global")).settings.mode, Mode::Strict);
        assert!(!store.permits_dispatch(Some("codex:global"), "cx-a"));
        assert!(store.permits_dispatch(Some("codex:global"), "cx-b"));
    }
    #[test]
    fn project_assignment_does_not_clear_explicit_policy_without_inherit() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let id = session_id("codex:explicit");
        store
            .edit_session(&id, Some(settings(Mode::Strict)), None, Some("cx-a"), false)
            .unwrap();
        let project = store
            .put_project(
                None,
                Project {
                    name: "project".into(),
                    settings: settings(Mode::Rebalance),
                },
            )
            .unwrap();
        store
            .edit_session(&id, None, Some(&project), None, false)
            .unwrap();
        assert_eq!(
            store.plan(Some("codex:explicit")).settings.mode,
            Mode::Strict
        );
        store.edit_session(&id, None, None, None, true).unwrap();
        assert_eq!(
            store.plan(Some("codex:explicit")).settings.mode,
            Mode::Rebalance
        );
    }
    #[test]
    fn strict_capture_preserves_lru_anchor_but_never_overwrites_durable_anchor() {
        let home = home();
        let store = PolicyStore::new(home.path(), settings(Mode::Strict)).unwrap();
        store
            .capture_strict_anchors([("codex:old", "cx-a")])
            .unwrap();
        store
            .capture_strict_anchors([("codex:old", "cx-b")])
            .unwrap();
        assert!(store.permits_dispatch(Some("codex:old"), "cx-a"));
        assert!(!store.permits_dispatch(Some("codex:old"), "cx-b"));
        assert!(!store.permits_dispatch(None, "cx-a"));
    }
    #[test]
    fn persisted_schema_rejects_missing_or_misspelled_safety_fields() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let id = session_id("codex:schema");
        store
            .set_session(&id, Some(settings(Mode::Strict)), None, Some("cx-a"))
            .unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
        for field in ["explicit", "anchor", "project", "movements"] {
            let mut bad = original.clone();
            bad["sessions"][&id].as_object_mut().unwrap().remove(field);
            fs::write(&store.path, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(
                PolicyStore::new(home.path(), Settings::default()).is_err(),
                "missing {field}"
            );
        }
        let mut bad = original.clone();
        bad["sessions"][&id]["explict"] = bad["sessions"][&id]["explicit"].clone();
        fs::write(&store.path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(PolicyStore::new(home.path(), Settings::default()).is_err());
        let mut bad = original;
        bad["sessions"][&id]["baseline"]
            .as_object_mut()
            .unwrap()
            .remove("mode");
        fs::write(&store.path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(PolicyStore::new(home.path(), Settings::default()).is_err());
    }
    #[test]
    fn policy_load_rejects_nonregular_and_oversized_files() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        fs::create_dir(&store.path).unwrap();
        assert!(PolicyStore::new(home.path(), Settings::default()).is_err());
        fs::remove_dir(&store.path).unwrap();
        fs::File::create(&store.path)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        assert!(PolicyStore::new(home.path(), Settings::default()).is_err());
    }
    #[test]
    fn slow_policy_writer_does_not_block_policy_readers() {
        let home = home();
        let store = Arc::new(PolicyStore::new(home.path(), Settings::default()).unwrap());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = store.clone();
        let writer = std::thread::spawn(move || {
            worker.update(|_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        entered_rx.recv().unwrap();
        let reader = store.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread =
            std::thread::spawn(move || tx.send(reader.plan(Some("codex:read")).revision).unwrap());
        let result = rx.recv_timeout(std::time::Duration::from_secs(1));
        release_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
        thread.join().unwrap();
        assert_eq!(result.unwrap(), 0);
    }
    #[test]
    fn duplicate_and_invalid_project_headers_are_removed_on_error() {
        assert_ne!(
            session_id("codex:root:agent:independent"),
            session_id("codex:root")
        );
        assert_eq!(
            session_id("claude:root:agent:child"),
            session_id("claude:root")
        );
        let mut headers = axum::http::HeaderMap::new();
        headers.append(PROJECT_HEADER, "a".repeat(32).parse().unwrap());
        headers.append(PROJECT_HEADER, "b".repeat(32).parse().unwrap());
        assert!(take_project(&mut headers).is_err());
        assert!(!headers.contains_key(PROJECT_HEADER));
        headers.insert(PROJECT_HEADER, "unknown".parse().unwrap());
        assert!(take_project(&mut headers).is_err());
        assert!(!headers.contains_key(PROJECT_HEADER));
        assert!(
            Settings {
                mode: Mode::Strict,
                reset_wait_secs: 121
            }
            .validate()
            .is_err()
        );
    }
}
