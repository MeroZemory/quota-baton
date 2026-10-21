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
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

const MAX_SESSIONS: usize = 20_000;
const MAX_PROJECTS: usize = 1024;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_FORCED_HISTORY: usize = 32;

fn pool_id(account: &str) -> &str {
    account.split('-').next().unwrap_or(account)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Strict,
    Recover,
    #[default]
    Rebalance,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ChurnLimits {
    pub dwell_secs: u64,
    pub revisit_secs: u64,
    pub window_secs: u64,
    pub max_group_moves: usize,
    pub max_pool_moves: usize,
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
    fn durable(&self, defaults: &Settings) -> bool {
        self.explicit.is_some()
            || self.project.is_some()
            || (self.anchor.is_some() && self.settings(defaults).mode == Mode::Strict)
    }
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
    frozen: bool,
    #[serde(skip)]
    history_overflow_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    durable_overflow_until: Option<DateTime<Utc>>,
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
            frozen: false,
            history_overflow_until: None,
            durable_overflow_until: None,
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
        if let Some(until) = self.durable_overflow_until {
            self.history_overflow_until = Some(
                self.history_overflow_until
                    .map_or(until, |old| old.max(until)),
            );
        }
        self.pool_moves.clear();
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
            if movement.discretionary {
                self.pool_moves
                    .entry(pool_id(&movement.to).to_owned())
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
    /// Readers hold this only during in-memory authority lookup. Writers acquire
    /// it only after I/O, and publish state, witness and blocking flags together.
    publication: RwLock<()>,
    #[cfg(test)]
    publication_hook: Mutex<Option<Arc<PublicationHook>>>,
    file_seen: AtomicBool,
    recovery_blocked: AtomicBool,
    lifecycle_recovery: AtomicBool,
    persistence_failed: AtomicBool,
    recovery_intent: Mutex<State>,
    durable_generation: Mutex<Option<(u64, String)>>,
    warnings: Mutex<WarningState>,
    pub(crate) changed: tokio::sync::Notify,
}

#[cfg(test)]
struct PublicationHook {
    entered: std::sync::mpsc::SyncSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
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

fn load(path: &Path) -> Result<Option<State>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.len() > MAX_FILE => bail!("routing policy file exceeds cap"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
    let mut value: serde_json::Value = crate::json_unique::from_slice(&bytes)?;
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
    validate_state(&state)?;
    state.reindex();
    Ok(Some(state))
}

fn validate_state(state: &State) -> Result<()> {
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
            || session.movements.iter().filter(|m| m.discretionary).count() > 32
            || session
                .movements
                .iter()
                .filter(|m| !m.discretionary)
                .count()
                > MAX_FORCED_HISTORY
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
    Ok(())
}

#[derive(Default)]
struct WarningState {
    pending: Vec<PolicyWarning>,
    sink: Option<std::sync::Weak<crate::observability::Observability>>,
    seen: BTreeSet<&'static str>,
}

/// Bounded, content-free warnings drained by BindingTable's observability owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyWarning {
    pub code: &'static str,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFence {
    schema: u32,
    revision: u64,
    digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strict: Option<State>,
    #[serde(default)]
    pending: bool,
}
fn fence_path(path: &Path) -> PathBuf {
    path.with_file_name("routing-policies.fence.json")
}
fn policy_digest(state: &State) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(state)?)))
}
fn strict_intent(state: &State, defaults: &Settings) -> State {
    let mut strict = State {
        projects: state
            .projects
            .iter()
            .filter(|(_, p)| p.settings.mode == Mode::Strict)
            .map(|(id, p)| {
                (
                    id.clone(),
                    Project {
                        name: "strict".into(),
                        settings: p.settings.clone(),
                    },
                )
            })
            .collect(),
        ..State::default()
    };
    let legacy_strict = state.legacy_codex_roots.iter().any(|id| {
        state
            .sessions
            .get(id)
            .is_some_and(|session| session.settings(defaults).mode == Mode::Strict)
    });
    for (id, session) in &state.sessions {
        let mode = session.settings(defaults).mode;
        if mode == Mode::Strict
            || ((legacy_strict || defaults.mode == Mode::Strict) && session.durable(defaults))
        {
            strict.sessions.insert(
                id.clone(),
                SessionPolicy {
                    project: None,
                    baseline: session.settings(defaults).clone(),
                    explicit: Some(session.settings(defaults).clone()),
                    anchor: if mode == Mode::Strict {
                        session.anchor.clone()
                    } else {
                        None
                    },
                    movements: Vec::new(),
                },
            );
            if mode == Mode::Strict && state.legacy_codex_roots.contains(id) {
                strict.legacy_codex_roots.insert(id.clone());
            }
        }
    }
    strict
}
fn merge_intent(into: &mut State, other: State) {
    // Old known anchors win on an incomplete edit; every affected root is blocked.
    for (id, session) in other.sessions {
        match into.sessions.entry(id) {
            std::collections::btree_map::Entry::Occupied(mut entry)
                if entry
                    .get()
                    .explicit
                    .as_ref()
                    .is_none_or(|s| s.mode != Mode::Strict)
                    && session
                        .explicit
                        .as_ref()
                        .is_some_and(|s| s.mode == Mode::Strict) =>
            {
                entry.insert(session);
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(session);
            }
            _ => {}
        }
    }
    for (id, project) in other.projects {
        into.projects.entry(id).or_insert(project);
    }
    into.legacy_codex_roots.extend(other.legacy_codex_roots);
}
fn has_intent(state: &State) -> bool {
    state.sessions.values().any(|session| {
        session
            .explicit
            .as_ref()
            .is_some_and(|s| s.mode == Mode::Strict)
    }) || !state.projects.is_empty()
}
fn has_durable_rows(state: &State) -> bool {
    !state.sessions.is_empty() || !state.projects.is_empty()
}
fn durable_state(state: &State, defaults: &Settings) -> State {
    let mut durable = state.clone();
    durable.sessions.retain(|_, s| s.durable(defaults));
    durable
        .legacy_codex_roots
        .retain(|id| durable.sessions.contains_key(id));
    if !has_durable_rows(&durable) {
        durable.durable_overflow_until = None;
    }
    durable
}
fn write_fence_with_intent(path: &Path, state: &State, strict: State) -> Result<()> {
    write_intent_fence(path, state, strict, false)
}
fn write_pending_fence(path: &Path, state: &State, strict: State) -> Result<()> {
    write_intent_fence(path, state, strict, true)
}
fn write_intent_fence(path: &Path, state: &State, strict: State, pending: bool) -> Result<()> {
    validate_state(&strict)?;
    let fence = PolicyFence {
        schema: 2,
        revision: state.revision,
        digest: policy_digest(state)?,
        strict: Some(strict),
        pending,
    };
    let bytes = serde_json::to_vec(&fence)?;
    if bytes.len() as u64 > MAX_FILE {
        bail!("routing policy fence exceeds cap");
    }
    crate::bindings::atomic_write(&fence_path(path), &bytes)?;
    Ok(())
}
#[cfg(test)]
fn write_fence(path: &Path, state: &State) -> Result<()> {
    write_fence_with_intent(path, state, strict_intent(state, &Settings::default()))
}
fn read_fence(path: &Path) -> Result<Option<PolicyFence>> {
    match crate::bindings::read_binding_snapshot(&fence_path(path)) {
        Ok(bytes) => {
            if bytes.len() as u64 > MAX_FILE {
                bail!("routing policy fence exceeds cap");
            }
            let fence: PolicyFence =
                serde_json::from_value(crate::json_unique::from_slice(&bytes)?)?;
            if !matches!(fence.schema, 1 | 2)
                || !valid_id(&fence.digest, 64)
                || (fence.schema == 2 && fence.strict.is_none())
            {
                bail!("invalid routing policy fence");
            }
            if let Some(strict) = &fence.strict {
                validate_state(strict)?;
                if strict.sessions.values().any(|s| {
                    s.explicit.is_none()
                        || (s.explicit.as_ref().is_some_and(|v| v.mode != Mode::Strict)
                            && s.anchor.is_some())
                        || s.project.is_some()
                        || !s.movements.is_empty()
                }) || strict
                    .projects
                    .values()
                    .any(|p| p.settings.mode != Mode::Strict)
                {
                    bail!("invalid strict intent witness");
                }
            }
            Ok(Some(fence))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
// Best-effort positive evidence only. Unparseable legacy bytes cannot establish
// absence of strict intent; that pre-witness limitation is recorded at the gate.
fn legacy_strict_evidence(path: &Path, defaults: &Settings) -> State {
    let mut strict = State::default();
    let Ok(bytes) = crate::bindings::read_binding_snapshot(path) else {
        return strict;
    };
    if bytes.len() as u64 > MAX_FILE {
        return strict;
    }
    let Ok(value) = crate::json_unique::from_slice(&bytes) else {
        return strict;
    };
    if let Some(projects) = value.get("projects").and_then(|v| v.as_object()) {
        for (id, value) in projects.iter() {
            if valid_project(id)
                && value.pointer("/settings/mode").and_then(|v| v.as_str()) == Some("strict")
            {
                strict.projects.insert(
                    id.clone(),
                    Project {
                        name: "recovered-strict".into(),
                        settings: Settings {
                            mode: Mode::Strict,
                            ..Settings::default()
                        },
                    },
                );
            }
        }
    }
    if let Some(sessions) = value.get("sessions").and_then(|v| v.as_object()) {
        for (id, value) in sessions.iter() {
            let explicit = value.get("explicit").filter(|v| !v.is_null());
            let project = value.get("project").and_then(|v| v.as_str());
            // Project association freezes the session baseline. Current project
            // settings cannot change the mode of an already-associated root.
            let effective = explicit.or_else(|| project.and_then(|_| value.get("baseline")));
            let mode = effective
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str());
            let settings = match mode {
                Some("strict") => Some(Settings {
                    mode: Mode::Strict,
                    ..Settings::default()
                }),
                Some("recover" | "rebalance") => effective
                    .and_then(|v| serde_json::from_value::<Settings>(v.clone()).ok())
                    .filter(|v| v.validate().is_ok()),
                None if explicit.is_none()
                    && project.is_none()
                    && defaults.mode == Mode::Strict =>
                {
                    Some(defaults.clone())
                }
                _ => None,
            };
            if !valid_id(id, 64) {
                continue;
            }
            if let Some(settings) = settings {
                let anchor = value
                    .get("anchor")
                    .and_then(|v| v.as_str())
                    .filter(|a| crate::observability::safe_identifier(a).is_some())
                    .map(str::to_owned);
                strict.sessions.insert(
                    id.clone(),
                    SessionPolicy {
                        project: None,
                        baseline: settings.clone(),
                        explicit: Some(settings.clone()),
                        anchor: if settings.mode == Mode::Strict {
                            anchor
                        } else {
                            None
                        },
                        movements: Vec::new(),
                    },
                );
            }
        }
    }
    if value.get("schema").and_then(|v| v.as_u64()) == Some(1) {
        strict.legacy_codex_roots.extend(
            strict
                .sessions
                .iter()
                .filter(|(_, session)| {
                    session
                        .explicit
                        .as_ref()
                        .is_some_and(|settings| settings.mode == Mode::Strict)
                })
                .map(|(id, _)| id.clone()),
        );
    } else if let Some(aliases) = value.get("legacy_codex_roots").and_then(|v| v.as_array()) {
        for alias in aliases.iter().filter_map(|v| v.as_str()) {
            if strict.sessions.get(alias).is_some_and(|session| {
                session
                    .explicit
                    .as_ref()
                    .is_some_and(|settings| settings.mode == Mode::Strict)
            }) {
                strict.legacy_codex_roots.insert(alias.to_owned());
            }
        }
    }
    strict
}
struct Recovery {
    state: State,
    blocked: bool,
    degraded: bool,
    seen: bool,
    warning: Option<&'static str>,
}
fn recover_policy(
    path: &Path,
    defaults: &Settings,
    known: Option<&State>,
    witness: Option<&State>,
    generation: Option<&(u64, String)>,
) -> Recovery {
    // Read both independently: a bad policy must not hide a good strict witness.
    let loaded = load(path);
    let fence = read_fence(path);
    let salvage = if loaded.is_err() {
        legacy_strict_evidence(path, defaults)
    } else {
        State::default()
    };
    let mut strict = known
        .map(|s| strict_intent(s, defaults))
        .unwrap_or_default();
    merge_intent(&mut strict, salvage);
    if let Some(witness) = witness {
        merge_intent(&mut strict, witness.clone());
    }
    if let Ok(Some(state)) = &loaded {
        merge_intent(&mut strict, strict_intent(state, defaults));
    }
    if let Ok(Some(fence)) = &fence
        && let Some(witness) = &fence.strict
    {
        merge_intent(&mut strict, witness.clone());
    }
    let adopt = matches!(&fence, Ok(None)) || matches!(&fence, Ok(Some(f)) if f.schema == 1);
    let checked = (|| -> Result<Option<State>> {
        let state = loaded?;
        if let Some(fence) = fence? {
            let state = state
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("routing policy missing"))?;
            if fence.revision != state.revision || fence.digest != policy_digest(state)? {
                bail!("routing policy generation mismatch");
            }
            if fence.pending {
                bail!("routing policy transaction incomplete");
            }
            // A union witness from an interrupted transaction may be wider.
            // Even matching generations cannot release those pending locks.
            if let Some(witness) = fence.strict
                && policy_digest(&strict_intent(&witness, defaults))?
                    != policy_digest(&strict_intent(state, defaults))?
            {
                bail!("routing strict witness reconciliation incomplete");
            }
        }
        if let (Some((revision, digest)), Some(next)) = (generation, state.as_ref()) {
            let next = durable_state(next, defaults);
            if next.revision < *revision
                || (next.revision == *revision && policy_digest(&next)? != *digest)
            {
                bail!("routing policy revision rollback refused");
            }
        }
        if state.is_none() && known.is_some_and(|s| has_durable_rows(&durable_state(s, defaults))) {
            bail!("routing policy disappeared");
        }
        if let Some(state) = &state {
            let durable = durable_state(state, defaults);
            if adopt && has_durable_rows(&durable) {
                // Also upgrades schema-1 digest-only fences and pre-fence files.
                write_fence_with_intent(path, state, strict_intent(state, defaults))?;
            }
        }
        Ok(state)
    })();
    match checked {
        Ok(state) => Recovery {
            seen: state.is_some(),
            state: state.unwrap_or_default(),
            blocked: false,
            degraded: false,
            warning: None,
        },
        Err(_) => {
            strict.revision = known.map_or(0, |state| state.revision);
            let blocked = defaults.mode == Mode::Strict || has_intent(&strict);
            if !blocked {
                // Rename evidence, never replace/delete it. Failure disables writes,
                // but cannot deny unrelated non-strict traffic.
                let suffix = format!(
                    "corrupt-{}-{:016x}",
                    Utc::now().timestamp_millis(),
                    rand::random::<u64>()
                );
                for original in [path.to_path_buf(), fence_path(path)] {
                    if fs::symlink_metadata(&original).is_ok() {
                        let name = original
                            .file_name()
                            .expect("policy filename")
                            .to_string_lossy();
                        if fs::rename(
                            &original,
                            original.with_file_name(format!("{name}.{suffix}")),
                        )
                        .is_err()
                        {
                            return Recovery {
                                state: strict,
                                blocked: false,
                                degraded: true,
                                seen: false,
                                warning: Some("routing_policy_quarantine_failed"),
                            };
                        }
                    }
                }
            }
            Recovery {
                state: strict,
                blocked,
                degraded: true,
                seen: blocked,
                warning: Some(if blocked {
                    "routing_policy_strict_recovery_required"
                } else {
                    "routing_policy_quarantined"
                }),
            }
        }
    }
}

impl PolicyStore {
    pub fn new(home: &Path, defaults: Settings) -> Result<Self> {
        defaults.validate()?;
        let path = home.join("state/routing-policies.json");
        let directory_failed = fs::create_dir_all(path.parent().expect("policy parent")).is_err();
        let recovered = recover_policy(&path, &defaults, None, None, None);
        let file_seen = recovered.seen;
        let intent = strict_intent(&recovered.state, &defaults);
        let generation = if !recovered.degraded && recovered.seen {
            Some((
                recovered.state.revision,
                policy_digest(&durable_state(&recovered.state, &defaults))?,
            ))
        } else {
            None
        };
        let state = recovered.state;
        let mut warnings = Vec::new();
        if let Some(code) = recovered.warning {
            warnings.push(PolicyWarning { code });
        }
        if directory_failed {
            warnings.push(PolicyWarning {
                code: "routing_policy_directory_failed",
            });
        }
        Ok(Self {
            path,
            defaults,
            limits: ChurnLimits::default(),
            state: Mutex::new(Arc::new(state)),
            writer: Mutex::new(()),
            publication: RwLock::new(()),
            #[cfg(test)]
            publication_hook: Mutex::new(None),
            file_seen: AtomicBool::new(file_seen),
            recovery_blocked: AtomicBool::new(recovered.blocked),
            lifecycle_recovery: AtomicBool::new(false),
            persistence_failed: AtomicBool::new(recovered.degraded || directory_failed),
            recovery_intent: Mutex::new(intent),
            durable_generation: Mutex::new(generation),
            warnings: Mutex::new(WarningState {
                seen: warnings.iter().map(|w| w.code).collect(),
                pending: warnings,
                sink: None,
            }),
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
    /// The integration owner drains startup warnings after attaching observability,
    /// and drains subsequent warnings after routing operations. No raw file data.
    pub fn take_warnings(&self) -> Vec<PolicyWarning> {
        std::mem::take(
            &mut self
                .warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending,
        )
    }
    pub fn attach_observability(&self, observability: &Arc<crate::observability::Observability>) {
        let pending = {
            let mut state = self.warnings.lock().unwrap_or_else(|e| e.into_inner());
            state.sink = Some(Arc::downgrade(observability));
            std::mem::take(&mut state.pending)
        };
        for warning in pending {
            observability.core_warning("routing", warning.code);
        }
    }
    pub fn emit_pending_warnings(&self, observability: &crate::observability::Observability) {
        for warning in self.take_warnings() {
            observability.core_warning("routing", warning.code);
        }
    }
    fn warn(&self, code: &'static str) {
        let sink = {
            let mut state = self.warnings.lock().unwrap_or_else(|e| e.into_inner());
            // Codes come from a fixed set. Report each once per store lifetime,
            // rather than flooding SSE/ledger during a persistent disk failure.
            if !state.seen.insert(code) {
                return;
            }
            let sink = state.sink.as_ref().and_then(std::sync::Weak::upgrade);
            if sink.is_none() {
                state.pending.push(PolicyWarning { code });
            }
            sink
        };
        if let Some(sink) = sink {
            sink.core_warning("routing", code);
        }
    }
    #[cfg(test)]
    fn pause_publication(&self) {
        let hook = self
            .publication_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook.entered.send(()).unwrap();
            hook.release
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv()
                .unwrap();
        }
    }
    fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.update_inner(false, f)
    }
    fn update_inner<T>(
        &self,
        volatile_on_failure: bool,
        f: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        self.update_inner_for_issue(volatile_on_failure, None, f)
    }

    fn update_inner_for_issue<T>(
        &self,
        volatile_on_failure: bool,
        operation: Option<&crate::issue_operation::IssueOperation>,
        f: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(operation) = operation {
            operation.check().map_err(|_| anyhow::anyhow!("launch admission cancelled"))?;
        }
        let state = self.snapshot();
        if state.frozen {
            bail!("routing policy writer retired");
        }
        let mut next = (*state).clone();
        let result = f(&mut next)?;
        // A volatile ring may already have evicted live guard evidence. When
        // its history becomes explicit intent, persist that summary with it.
        // Merely moving a default root beside other durable intent does not
        // promote the summary or cause a policy write.
        if let Some(until) = state.history_overflow_until
            && next.sessions.iter().any(|(id, session)| {
                session.durable(&self.defaults)
                    && !session.movements.is_empty()
                    && state
                        .sessions
                        .get(id)
                        .is_none_or(|previous| !previous.durable(&self.defaults))
            })
        {
            next.durable_overflow_until = Some(
                next.durable_overflow_until
                    .map_or(until, |previous| previous.max(until)),
            );
        }
        let mut published_intent = None;
        if next.sessions.len() > MAX_SESSIONS || next.projects.len() > MAX_PROJECTS {
            bail!("routing policy capacity reached");
        }
        next.reindex();
        validate_state(&next)?;
        // Writer gate is held, f only changed a private clone. Cancellation
        // competes here before durable writes/publication; never after them.
        if let Some(operation) = operation {
            operation.admit().map_err(|_| anyhow::anyhow!("launch admission cancelled"))?;
        }
        let before = durable_state(&state, &self.defaults);
        let mut durable = durable_state(&next, &self.defaults);
        // Volatile movement history never advances the durable generation.
        durable.revision = before.revision;
        if policy_digest(&before)? != policy_digest(&durable)? {
            next.revision = state
                .revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("routing revision exhausted"))?;
            durable.revision = next.revision;
            let mut union = strict_intent(&before, &self.defaults);
            merge_intent(&mut union, strict_intent(&durable, &self.defaults));
            merge_intent(
                &mut union,
                self.recovery_intent
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            );
            let write = (|| -> Result<()> {
                if self.persistence_failed.load(Ordering::Acquire) {
                    bail!("routing persistence requires recovery");
                }
                let bytes = serde_json::to_vec(&durable)?;
                if bytes.len() as u64 > MAX_FILE {
                    bail!("routing policy file exceeds cap");
                }
                write_pending_fence(&self.path, &durable, union.clone())?;
                crate::bindings::atomic_write(&self.path, &bytes)?;
                write_fence_with_intent(
                    &self.path,
                    &durable,
                    strict_intent(&durable, &self.defaults),
                )?;
                Ok(())
            })();
            if let Err(error) = write {
                {
                    let _publication = self.publication.write().unwrap_or_else(|e| e.into_inner());
                    self.persistence_failed.store(true, Ordering::Release);
                    self.recovery_blocked.store(true, Ordering::Release);
                    #[cfg(test)]
                    self.pause_publication();
                    *self
                        .recovery_intent
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = union;
                    if volatile_on_failure {
                        next.revision = state.revision;
                        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
                    }
                }
                self.warn("routing_policy_write_failed");
                self.changed.notify_waiters();
                return if volatile_on_failure {
                    Ok(result)
                } else {
                    Err(error)
                };
            }
            self.file_seen.store(true, Ordering::Release);
            *self
                .durable_generation
                .lock()
                .unwrap_or_else(|e| e.into_inner()) =
                Some((durable.revision, policy_digest(&durable)?));
            published_intent = Some(strict_intent(&durable, &self.defaults));
        }
        next.reindex();
        {
            let _publication = self.publication.write().unwrap_or_else(|e| e.into_inner());
            if let Some(intent) = published_intent {
                *self
                    .recovery_intent
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = intent;
            }
            *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        }
        self.changed.notify_waiters();
        Ok(result)
    }
    pub fn freeze(&self) {
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = (*self.snapshot()).clone();
        next.frozen = true;
        let _publication = self.publication.write().unwrap_or_else(|e| e.into_inner());
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
    }
    pub(crate) fn requires_strict_recovery(&self) -> bool {
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        self.defaults.mode == Mode::Strict
            || has_intent(
                &self
                    .recovery_intent
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            )
            || self
                .snapshot()
                .sessions
                .values()
                .any(|session| session.settings(&self.defaults).mode == Mode::Strict)
    }
    pub fn reload(&self) -> Result<()> {
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let state = self.snapshot();
        if state.frozen {
            bail!("routing policy writer retired");
        }
        let generation = self
            .durable_generation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let witness = self
            .recovery_intent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let recovered = recover_policy(
            &self.path,
            &self.defaults,
            Some(&state),
            Some(&witness),
            generation.as_ref(),
        );
        if let Some(code) = recovered.warning {
            self.warn(code);
        }
        let intent = strict_intent(&recovered.state, &self.defaults);
        let mut next = recovered.state;
        if !recovered.degraded {
            *self
                .durable_generation
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some((
                next.revision,
                policy_digest(&durable_state(&next, &self.defaults))?,
            ));
        }
        // In-memory implicit histories can be newer than legacy disk histories.
        for (id, session) in &state.sessions {
            if !session.durable(&self.defaults)
                && next
                    .sessions
                    .get(id)
                    .is_none_or(|disk| !disk.durable(&self.defaults))
            {
                next.sessions.insert(id.clone(), session.clone());
            }
        }
        next.history_overflow_until =
            match (next.history_overflow_until, state.history_overflow_until) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
        next.reindex();
        {
            let _publication = self.publication.write().unwrap_or_else(|e| e.into_inner());
            self.recovery_blocked
                .store(recovered.blocked, Ordering::Release);
            #[cfg(test)]
            self.pause_publication();
            self.persistence_failed
                .store(recovered.degraded, Ordering::Release);
            self.file_seen.store(recovered.seen, Ordering::Release);
            *self
                .recovery_intent
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = intent;
            *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        }
        self.changed.notify_waiters();
        if recovered.blocked {
            bail!("routing strict recovery required");
        }
        Ok(())
    }
    /// Import recovery blocks strict dispatch until validation and anchor capture finish.
    /// Unrelated non-strict sessions continue serving.
    pub(crate) fn begin_recovery(&self) {
        let _publication = self.publication.write().unwrap_or_else(|e| e.into_inner());
        self.lifecycle_recovery.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    pub(crate) fn finish_recovery(&self) {
        let _publication = self.publication.write().unwrap_or_else(|e| e.into_inner());
        self.lifecycle_recovery.store(false, Ordering::Release);
        self.changed.notify_waiters();
    }
    pub(crate) fn strict_dispatch_blocked(&self) -> bool {
        self.recovery_blocked.load(Ordering::Acquire)
            || self.lifecycle_recovery.load(Ordering::Acquire)
    }
    fn snapshot(&self) -> Arc<State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Capture existing locks before the disposable binding LRU is pruned.
    pub fn capture_strict_anchors<'a>(
        &self,
        bindings: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<()> {
        if self.persistence_failed.load(Ordering::Acquire) {
            self.warn("routing_policy_strict_recovery_required");
            return Ok(());
        }
        let state = self.snapshot();
        let mut missing = BTreeMap::<String, String>::new();
        for (key, account) in bindings {
            let id = state.policy_id(key);
            let plan = self.resolve(&state, &id);
            if plan.settings.mode == Mode::Strict && plan.anchor.is_none() {
                if let Some(previous) = missing.get(&id) {
                    if previous != account {
                        bail!("strict policy alias has conflicting binding accounts");
                    }
                } else {
                    missing.insert(id, account.to_owned());
                }
            }
        }
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
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        let state = self.snapshot();
        if state.frozen {
            return false;
        }
        let Some(key) = key else {
            return self.defaults.mode != Mode::Strict;
        };
        let plan = self.plan_published(Some(key));
        plan.settings.mode != Mode::Strict
            || (!self.strict_dispatch_blocked() && plan.anchor.as_deref() == Some(account))
    }
    fn resolve(&self, state: &State, id: &str) -> Plan {
        let witness = self
            .recovery_intent
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = if self.recovery_blocked.load(Ordering::Acquire) {
            witness.sessions.get(id).or_else(|| state.sessions.get(id))
        } else {
            state.sessions.get(id)
        };
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
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        self.plan_published(key)
    }
    // Caller holds publication read/write access; never reacquire it here.
    fn plan_published(&self, key: Option<&str>) -> Plan {
        let state = self.snapshot();
        let id = key
            .map(|key| {
                let current = state.policy_id(key);
                let witness = self
                    .recovery_intent
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if self.recovery_blocked.load(Ordering::Acquire) {
                    let guarded = witness.policy_id(key);
                    if witness.sessions.contains_key(&guarded) {
                        guarded
                    } else {
                        current
                    }
                } else {
                    current
                }
            })
            .unwrap_or_default();
        self.resolve(&state, &id)
    }
    pub fn list(&self) -> serde_json::Value {
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
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
        if self.recovery_blocked.load(Ordering::Acquire)
            && self
                .recovery_intent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .projects
                .contains_key(project)
        {
            bail!("strict project recovery incomplete");
        }
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
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        let state = self.snapshot();
        if state
            .history_overflow_until
            .is_some_and(|until| now < until)
        {
            return false;
        }
        let pool_moves = target
            .and_then(|account| state.pool_moves.get(pool_id(account)))
            .into_iter()
            .flatten()
            .filter(|at| (now - **at).num_seconds() < self.limits.window_secs as i64)
            .count();
        if pool_moves >= self.limits.max_pool_moves {
            return false;
        }
        let Some(session) = state.sessions.get(&state.policy_id(key)) else {
            return self.defaults.mode == Mode::Rebalance;
        };
        session.settings(&self.defaults).mode == Mode::Rebalance
            && session
                .movements
                .iter()
                .filter(|m| {
                    m.discretionary && (now - m.at).num_seconds() < self.limits.window_secs as i64
                })
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
    /// Authorize an explicitly consented Strict model move before binding commit.
    /// The caller holds BindingTable's mutation gate and has checked consent,
    /// entitlement, eligibility and inactivity. Ordinary dispatch still rechecks
    /// authority afterwards. This does not relax policy or replace its history.
    pub fn authorize_model_move(&self, consent: &crate::bindings::LaunchPolicyMove) -> Result<()> {
        self.authorize_model_move_for_issue(consent, None)
    }

    pub(crate) fn authorize_model_move_for_issue(
        &self,
        consent: &crate::bindings::LaunchPolicyMove,
        operation: Option<&crate::issue_operation::IssueOperation>,
    ) -> Result<()> {
        if consent.previous == consent.account
            || crate::observability::safe_identifier(&consent.previous).is_none()
            || crate::observability::safe_identifier(&consent.account).is_none()
        {
            bail!("invalid explicit model move");
        }
        self.update_inner_for_issue(false, operation, |state| {
            // update holds the writer gate through persistence and publication.
            // Resolve aliases here too, rather than against a pre-lock snapshot.
            if self.persistence_failed.load(Ordering::Acquire) || self.strict_dispatch_blocked() {
                bail!("routing policy recovery incomplete");
            }
            if state.revision != consent.expected_revision {
                bail!("routing policy changed before explicit model move");
            }
            let id = state.policy_id(&consent.key);
            let entry = state.sessions.get_mut(&id).ok_or_else(|| {
                anyhow::anyhow!("explicit model move requires an anchored policy")
            })?;
            if entry.settings(&self.defaults).mode != Mode::Strict
                || entry.anchor.as_deref() != Some(consent.previous.as_str())
            {
                bail!("routing anchor changed before explicit model move");
            }
            // Consent is not an automatic rebalance: retain all existing budget
            // evidence and add a non-discretionary revisit guard. Dispatch will
            // see the new anchor and must not record this move a second time.
            // A consented move may fail closed at capacity; never erase live
            // evidence to make room or fall back to volatile publication.
            if entry.movements.iter().filter(|m| !m.discretionary).count() >= MAX_FORCED_HISTORY {
                bail!("routing explicit model move history capacity reached");
            }
            entry.movements.push(Movement {
                id: Some(format!("{:032x}", rand::random::<u128>())),
                from: consent.previous.clone(),
                to: consent.account.clone(),
                at: consent.at,
                discretionary: false,
            });
            entry.anchor = Some(consent.account.clone());
            Ok(())
        })
    }
    /// Persist explicit intent before strict dispatch. Default movement guards are volatile.
    /// Non-strict movement remains available when persistence fails.
    pub fn dispatch(
        &self,
        key: Option<&str>,
        account: &str,
        previous: Option<&str>,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.dispatch_for_issue(key, account, previous, reason, now, None)
    }

    pub(crate) fn dispatch_for_issue(
        &self,
        key: Option<&str>,
        account: &str,
        previous: Option<&str>,
        reason: &str,
        now: DateTime<Utc>,
        operation: Option<&crate::issue_operation::IssueOperation>,
    ) -> Result<()> {
        if self.snapshot().frozen {
            bail!("routing policy writer retired");
        }
        let plan = self.plan(key);
        if plan.settings.mode == Mode::Strict && self.strict_dispatch_blocked() {
            bail!("routing policy recovery incomplete");
        }
        let Some(key) = key else {
            if let Some(operation) = operation {
                let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
                if self.snapshot().frozen { bail!("routing policy writer retired"); }
                operation.admit().map_err(|_| anyhow::anyhow!("launch admission cancelled"))?;
            }
            return Ok(());
        };
        let id = self.snapshot().policy_id(key);
        let discretionary = matches!(reason, "overload" | "cold_rebalance");
        {
            let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
            let state = self.snapshot();
            if state.frozen {
                bail!("routing policy writer retired");
            }
            let plan = self.plan_published(Some(key));
            if plan.settings.mode == Mode::Strict && self.strict_dispatch_blocked() {
                bail!("routing policy recovery incomplete");
            }
            let anchor = plan.anchor.as_deref().or(previous);
            if plan.settings.mode == Mode::Strict && anchor.is_some_and(|a| a != account) {
                bail!("routing session is locked");
            }
            if anchor == Some(account)
                && (plan.settings.mode != Mode::Strict
                    || state.sessions.get(&id).is_some_and(|s| s.anchor.is_some()))
            {
                if let Some(operation) = operation {
                    operation.admit().map_err(|_| anyhow::anyhow!("launch admission cancelled"))?;
                }
                return Ok(());
            }
            if anchor.is_none()
                && plan.settings.mode != Mode::Strict
                && !state.sessions.contains_key(&id)
            {
                if let Some(operation) = operation {
                    operation.admit().map_err(|_| anyhow::anyhow!("launch admission cancelled"))?;
                }
                return Ok(());
            }
        }
        if !discretionary
            && plan.settings.mode != Mode::Strict
            && self.persistence_failed.load(Ordering::Acquire)
        {
            let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            let state = self.snapshot();
            if state.frozen {
                bail!("routing policy writer retired");
            }
            if state.sessions.len() > MAX_SESSIONS || state.projects.len() > MAX_PROJECTS {
                if self.plan(Some(key)).settings.mode == Mode::Strict {
                    bail!("routing policy recovery incomplete");
                }
                if let Some(operation) = operation {
                    operation.admit().map_err(|_| anyhow::anyhow!("launch admission cancelled"))?;
                }
                let mut next = (*state).clone();
                let until = now + chrono::Duration::seconds(self.limits.revisit_secs as i64);
                next.history_overflow_until = Some(
                    next.history_overflow_until
                        .map_or(until, |old| old.max(until)),
                );
                {
                    let _publication = self.publication.write().unwrap_or_else(|e| e.into_inner());
                    *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
                }
                self.warn("routing_forced_history_overflow");
                self.changed.notify_waiters();
                // Metadata only: no rows added, no evidence pruned or persisted.
                // BindingTable emits the successful forced MoveEvent as usual.
                return Ok(());
            }
        }
        self.update_inner_for_issue(plan.settings.mode != Mode::Strict, operation, |state| {
            let current = self.plan(Some(key));
            if current.settings.mode == Mode::Strict
                && (self.strict_dispatch_blocked() || plan.settings.mode != Mode::Strict)
            {
                bail!("routing strict authority changed before dispatch");
            }
            if discretionary
                && state
                    .history_overflow_until
                    .is_some_and(|until| now < until)
            {
                bail!("routing volatile history overflow guard");
            }
            if !state.sessions.contains_key(&id) && state.sessions.len() >= MAX_SESSIONS {
                let forced_horizon = self.limits.revisit_secs;
                let discretionary_horizon = 86400_u64
                    .max(self.limits.window_secs)
                    .max(self.limits.dwell_secs)
                    .max(self.limits.revisit_secs);
                state.sessions.retain(|root, session| {
                    session.durable(&self.defaults)
                        || session.settings(&self.defaults).mode == Mode::Strict
                        || state.legacy_codex_roots.contains(root)
                        || session.movements.iter().any(|movement| {
                            let horizon = if movement.discretionary {
                                discretionary_horizon
                            } else {
                                forced_horizon
                            };
                            (now - movement.at).num_seconds() < horizon as i64
                        })
                });
                if state.sessions.len() >= MAX_SESSIONS
                    && !discretionary
                    && plan.settings.mode != Mode::Strict
                {
                    let until = now + chrono::Duration::seconds(forced_horizon as i64);
                    state.history_overflow_until = Some(
                        state
                            .history_overflow_until
                            .map_or(until, |old| old.max(until)),
                    );
                    self.warn("routing_forced_history_overflow");
                    // BindingTable still records its successful MoveEvent/ledger.
                    return Ok(());
                }
            }
            if discretionary {
                let prefix = pool_id(account);
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
            }
            let mut evicted_forced_until: Option<DateTime<Utc>> = None;
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
                if discretionary
                    && recent.iter().filter(|m| m.discretionary).count()
                        >= self.limits.max_group_moves
                {
                    bail!("routing movement budget exhausted");
                }
                entry.movements.push(Movement {
                    id: Some(format!("{:032x}", rand::random::<u128>())),
                    from,
                    to: account.to_owned(),
                    at: now,
                    discretionary,
                });
                if discretionary && entry.movements.iter().filter(|m| m.discretionary).count() > 32
                {
                    bail!("routing history capacity reached");
                }
                if !discretionary {
                    while entry.movements.iter().filter(|m| !m.discretionary).count()
                        > MAX_FORCED_HISTORY
                    {
                        let index = entry
                            .movements
                            .iter()
                            .position(|m| !m.discretionary)
                            .expect("forced history overflow");
                        let evicted = entry.movements.remove(index);
                        let until =
                            evicted.at + chrono::Duration::seconds(self.limits.revisit_secs as i64);
                        if now < until {
                            evicted_forced_until =
                                Some(evicted_forced_until.map_or(until, |old| old.max(until)));
                        }
                    }
                }
            }
            entry.anchor = Some(account.to_owned());
            let durable_eviction = entry.durable(&self.defaults);
            if let Some(until) = evicted_forced_until {
                if durable_eviction {
                    state.durable_overflow_until = Some(
                        state
                            .durable_overflow_until
                            .map_or(until, |old| old.max(until)),
                    );
                }
                state.history_overflow_until = Some(
                    state
                        .history_overflow_until
                        .map_or(until, |old| old.max(until)),
                );
                self.warn("routing_forced_history_overflow");
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    fn settings(mode: Mode) -> Settings {
        Settings {
            mode,
            reset_wait_secs: 7,
        }
    }
    fn r11_consent(
        store: &PolicyStore,
        key: &str,
        account: &str,
    ) -> crate::bindings::LaunchPolicyMove {
        let plan = store.plan(Some(key));
        crate::bindings::LaunchPolicyMove {
            key: key.to_owned(),
            previous: plan.anchor.unwrap(),
            account: account.to_owned(),
            expected_revision: plan.revision,
            at: clock(),
        }
    }

    #[test]
    fn r11_model_cas_preserves_strict_policy_inheritance_and_history_on_restart() {
        for kind in 0..3 {
            let dir = home();
            let defaults = settings(if kind == 0 {
                Mode::Strict
            } else {
                Mode::Rebalance
            });
            let store = PolicyStore::new(dir.path(), defaults.clone()).unwrap();
            let key = "codex:r11-policy";
            let id = session_id(key);
            let project = store
                .put_project(
                    None,
                    Project {
                        name: "r11".into(),
                        settings: settings(Mode::Strict),
                    },
                )
                .unwrap();
            if kind == 0 {
                store.capture_strict_anchors([(key, "cx-a")]).unwrap();
            } else {
                store
                    .set_session(
                        &id,
                        (kind == 2).then(|| settings(Mode::Strict)),
                        Some(&project),
                        Some("cx-a"),
                    )
                    .unwrap();
                // The inherited baseline must not follow later project edits.
                store
                    .put_project(
                        Some(&project),
                        Project {
                            name: "changed".into(),
                            settings: settings(Mode::Recover),
                        },
                    )
                    .unwrap();
            }
            store
                .update(|state| {
                    state
                        .sessions
                        .get_mut(&id)
                        .unwrap()
                        .movements
                        .push(Movement {
                            id: Some(format!("{:032x}", 1)),
                            from: "cx-old".into(),
                            to: "cx-a".into(),
                            at: clock(),
                            discretionary: true,
                        });
                    Ok(())
                })
                .unwrap();
            let before = store.snapshot().sessions[&id].clone();
            let consent = r11_consent(&store, key, "cx-b");
            store.authorize_model_move(&consent).unwrap();
            assert_eq!(
                store.plan(Some(key)).revision,
                consent.expected_revision + 1
            );
            assert!(!store.permits_dispatch(Some(key), "cx-a"));
            assert!(store.permits_dispatch(Some(key), "cx-b"));
            assert!(
                store
                    .dispatch(Some(key), "cx-a", Some("cx-b"), "forced", clock())
                    .is_err()
            );
            store
                .dispatch(Some(key), "cx-b", Some("cx-a"), "model_choice", clock())
                .unwrap();
            drop(store);
            let store = PolicyStore::new(dir.path(), defaults).unwrap();
            let state = store.snapshot();
            let after = &state.sessions[&id];
            assert_eq!(after.settings(&store.defaults).mode, Mode::Strict);
            assert_eq!(after.project, before.project);
            assert_eq!(after.baseline, before.baseline);
            assert_eq!(after.explicit, before.explicit);
            assert_eq!(after.anchor.as_deref(), Some("cx-b"));
            assert_eq!(after.movements.len(), before.movements.len() + 1);
            assert_eq!(
                &after.movements[..before.movements.len()],
                before.movements.as_slice()
            );
            let movement = after.movements.last().unwrap();
            assert_eq!((&*movement.from, &*movement.to), ("cx-a", "cx-b"));
            assert_eq!(movement.at, consent.at);
            assert!(!movement.discretionary);
            assert_eq!(state.pool_moves["cx"].len(), 1);
            // If policy later permits rebalance, the explicit move still guards
            // revisiting its source after the older discretionary dwell expires.
            store
                .set_session(&id, Some(settings(Mode::Rebalance)), None, None)
                .unwrap();
            let later = clock() + chrono::Duration::seconds(store.limits.dwell_secs as i64);
            assert!(store.discretionary_allowed(key, Some("cx-fresh"), later));
            assert!(!store.discretionary_allowed(key, Some("cx-a"), later));
        }
    }

    #[test]
    fn r11_model_cas_checks_revision_and_anchor_without_publishing_on_conflict() {
        let dir = home();
        let store = PolicyStore::new(dir.path(), Settings::default()).unwrap();
        let key = "codex:r11-conflict";
        store
            .set_session(
                &session_id(key),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        let stale = r11_consent(&store, key, "cx-b");
        store
            .put_project(
                None,
                Project {
                    name: "unrelated".into(),
                    settings: Settings::default(),
                },
            )
            .unwrap();
        let before = fs::read(&store.path).unwrap();
        assert!(store.authorize_model_move(&stale).is_err());
        let mut wrong = r11_consent(&store, key, "cx-b");
        wrong.previous = "cx-wrong".into();
        assert!(store.authorize_model_move(&wrong).is_err());
        let mut noop = r11_consent(&store, key, "cx-a");
        assert!(store.authorize_model_move(&noop).is_err());
        noop.account = "bad/account".into();
        assert!(store.authorize_model_move(&noop).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), before);
        assert_eq!(store.plan(Some(key)).anchor.as_deref(), Some("cx-a"));
        let valid = r11_consent(&store, key, "cx-b");
        store.authorize_model_move(&valid).unwrap();
        assert!(store.authorize_model_move(&valid).is_err());
    }

    #[test]
    fn r11_model_cas_simultaneous_moves_have_one_durable_winner() {
        let dir = home();
        let store = Arc::new(PolicyStore::new(dir.path(), Settings::default()).unwrap());
        let key = "codex:r11-race";
        store
            .set_session(
                &session_id(key),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for account in ["cx-b", "cx-c"] {
            let consent = r11_consent(&store, key, account);
            let store = store.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                store.authorize_model_move(&consent).is_ok()
            }));
        }
        barrier.wait();
        let won = workers
            .into_iter()
            .map(|w| usize::from(w.join().unwrap()))
            .sum::<usize>();
        assert_eq!(won, 1);
        let plan = store.plan(Some(key));
        drop(store);
        let store = PolicyStore::new(dir.path(), Settings::default()).unwrap();
        assert_eq!(store.plan(Some(key)).revision, plan.revision);
        assert_eq!(store.plan(Some(key)).anchor, plan.anchor);
        assert_eq!(
            store.snapshot().sessions[&session_id(key)].movements.len(),
            1
        );
    }

    #[test]
    fn r11_model_cas_rejects_persist_failure_recovery_and_freeze() {
        for frozen in [false, true] {
            let dir = home();
            let store = PolicyStore::new(dir.path(), Settings::default()).unwrap();
            let key = "codex:r11-failure";
            store
                .set_session(
                    &session_id(key),
                    Some(settings(Mode::Strict)),
                    None,
                    Some("cx-a"),
                )
                .unwrap();
            let consent = r11_consent(&store, key, "cx-b");
            let before = serde_json::to_vec(&*store.snapshot()).unwrap();
            if frozen {
                store.freeze();
            } else {
                // Leave the policy intact but make the fence destination unwritable.
                let fence = fence_path(&store.path);
                fs::remove_file(&fence).unwrap();
                fs::create_dir(&fence).unwrap();
            }
            assert!(store.authorize_model_move(&consent).is_err());
            assert_eq!(serde_json::to_vec(&*store.snapshot()).unwrap(), before);
            assert!(!store.permits_dispatch(Some(key), "cx-b"));
            if !frozen {
                assert!(store.persistence_failed.load(Ordering::Acquire));
                // Restoring the destination alone cannot bypass failed-persistence state.
                fs::remove_dir(fence_path(&store.path)).unwrap();
                assert!(store.authorize_model_move(&consent).is_err());
                assert_eq!(store.plan(Some(key)).revision, consent.expected_revision);
            }
        }
    }

    #[test]
    fn r11_model_cas_preserves_legacy_alias_authority_and_fails_closed_at_capacity() {
        let dir = home();
        let store = PolicyStore::new(dir.path(), Settings::default()).unwrap();
        let parent = "codex:r11-legacy";
        let child = "codex:r11-legacy:agent:one";
        let id = session_id(parent);
        store
            .set_session(&id, Some(settings(Mode::Strict)), None, Some("cx-a"))
            .unwrap();
        store
            .update(|state| {
                state.legacy_codex_roots.insert(id.clone());
                Ok(())
            })
            .unwrap();
        store
            .authorize_model_move(&r11_consent(&store, child, "cx-b"))
            .unwrap();
        assert_eq!(store.plan(Some(parent)).anchor.as_deref(), Some("cx-b"));
        assert!(!store.snapshot().sessions.contains_key(&session_id(child)));
        store
            .update(|state| {
                let entry = state.sessions.get_mut(&id).unwrap();
                for n in 1..MAX_FORCED_HISTORY {
                    entry.movements.push(Movement {
                        id: Some(format!("{:032x}", n)),
                        from: "cx-old".into(),
                        to: "cx-b".into(),
                        at: clock(),
                        discretionary: false,
                    });
                }
                Ok(())
            })
            .unwrap();
        let before = fs::read(&store.path).unwrap();
        assert!(
            store
                .authorize_model_move(&r11_consent(&store, child, "cx-c"))
                .is_err()
        );
        assert_eq!(fs::read(&store.path).unwrap(), before);
        drop(store);
        let store = PolicyStore::new(dir.path(), Settings::default()).unwrap();
        assert_eq!(store.plan(Some(child)).anchor.as_deref(), Some("cx-b"));
        assert_eq!(
            store.snapshot().sessions[&id].movements.len(),
            MAX_FORCED_HISTORY
        );
    }

    #[test]
    fn r11_model_cas_rejects_missing_anchor_and_non_strict_policy() {
        let dir = home();
        let store = PolicyStore::new(dir.path(), Settings::default()).unwrap();
        let key = "codex:r11-unanchored";
        store
            .set_session(&session_id(key), Some(settings(Mode::Strict)), None, None)
            .unwrap();
        let mut consent = crate::bindings::LaunchPolicyMove {
            key: key.into(),
            previous: "cx-a".into(),
            account: "cx-b".into(),
            expected_revision: store.plan(Some(key)).revision,
            at: clock(),
        };
        assert!(store.authorize_model_move(&consent).is_err());
        store
            .set_session(
                &session_id(key),
                Some(settings(Mode::Recover)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        consent.expected_revision = store.plan(Some(key)).revision;
        let before = fs::read(&store.path).unwrap();
        assert!(store.authorize_model_move(&consent).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), before);
    }

    #[test]
    fn duplicate_policy_fields_and_session_map_keys_fail_closed_before_migration() {
        let dir = home();
        let store = PolicyStore::new(dir.path(), settings(Mode::Rebalance)).unwrap();
        let id = session_id("codex:duplicate");
        store
            .set_session(&id, Some(settings(Mode::Strict)), None, Some("cx-a"))
            .unwrap();
        let original = fs::read_to_string(&store.path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&original).unwrap();
        let entry = serde_json::to_string(&value["sessions"][&id]).unwrap();
        let duplicate_map = format!(
            r#"{{"schema":2,"revision":1,"projects":{{}},"sessions":{{"{id}":{entry},"{id}":{entry}}},"legacy_codex_roots":[]}}"#
        );
        for raw in [
            original.replacen(
                r#""mode":"strict""#,
                r#""mode":"recover","mode":"strict""#,
                1,
            ),
            original.replacen(
                r#""anchor":"cx-a""#,
                r#""anchor":"cx-a","anchor":"cx-b""#,
                1,
            ),
            duplicate_map,
        ] {
            assert_ne!(raw, original);
            fs::write(&store.path, raw.as_bytes()).unwrap();
            crate::local_secrets::restrict(&store.path).unwrap();
            assert!(store.reload().is_err());
            assert_eq!(fs::read_to_string(&store.path).unwrap(), raw);
            assert_eq!(
                store.plan(Some("codex:duplicate")).anchor.as_deref(),
                Some("cx-a")
            );
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
        store.reload().unwrap();
        assert!(PolicyStore::new(dir.path(), settings(Mode::Rebalance)).is_ok());
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
        store.reload().unwrap();
        assert!(
            store
                .take_warnings()
                .iter()
                .any(|w| w.code == "routing_policy_quarantined")
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
    fn round2_churn_ignores_removed_and_unknown_keys() {
        let limits: ChurnLimits = serde_json::from_value(serde_json::json!({
            "target_quiet_secs": 120, "target_success_secs": 300,
            "future_option": true, "max_pool_moves": 7
        }))
        .unwrap();
        assert_eq!(limits.max_pool_moves, 7);
        limits.validate().unwrap();
        let value = serde_json::to_value(limits).unwrap();
        assert!(value.get("target_quiet_secs").is_none());
        assert!(value.get("target_success_secs").is_none());
        assert!(value.get("future_option").is_none());
    }

    #[test]
    fn round2_distinct_groups_can_arrive_without_destination_quiet() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        for n in 0..2 {
            let key = format!("codex:arrival{n}");
            assert!(store.discretionary_allowed(&key, Some("cx-b"), clock()));
            store
                .dispatch(Some(&key), "cx-b", Some("cx-a"), "overload", clock())
                .unwrap();
        }
        assert_eq!(store.snapshot().pool_moves["cx"].len(), 2);
        assert!(!store.path.exists());
    }

    #[test]
    fn round2_zero_revisit_forced_ring_does_not_invent_arrival_guard() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default())
            .unwrap()
            .with_limits(ChurnLimits {
                revisit_secs: 0,
                ..ChurnLimits::default()
            })
            .unwrap();
        let mut previous = "cx-original".to_owned();
        for n in 0..33 {
            let to = format!("cx-ring-{n}");
            store
                .dispatch(Some("codex:ring"), &to, Some(&previous), "forced", clock())
                .unwrap();
            previous = to;
        }
        assert!(store.snapshot().history_overflow_until.is_none());
        assert!(store.discretionary_allowed("codex:other", Some("cx-ring-0"), clock()));
        assert!(store.discretionary_allowed("codex:ring", Some("cx-original"), clock()));
        store
            .dispatch(
                Some("codex:other"),
                "cx-ring-0",
                Some("cx-before"),
                "overload",
                clock(),
            )
            .unwrap();
    }

    #[test]
    fn movement_budget_dwell_and_revisit_survive_restart() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let now = Utc::now();
        // Explicit policy makes this group's discretionary history durable.
        store
            .set_session(
                &session_id("codex:moving"),
                Some(settings(Mode::Rebalance)),
                None,
                Some("cx-a"),
            )
            .unwrap();
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
        for (from, to) in [
            ("cx-b", "cx-c"),
            ("cx-c", "cx-d"),
            ("cx-d", "cx-e"),
            ("cx-e", "cx-f"),
        ] {
            store
                .dispatch(Some("codex:moving"), to, Some(from), "forced", now)
                .unwrap();
        }
        drop(store);
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert_eq!(
            store.plan(Some("codex:moving")).anchor.as_deref(),
            Some("cx-f")
        );
        let snapshot = store.snapshot();
        let moves = &snapshot.sessions[&session_id("codex:moving")].movements;
        assert_eq!(moves.iter().filter(|m| !m.discretionary).count(), 4);
        assert_eq!(snapshot.pool_moves["cx"].len(), 1);
        assert!(!store.discretionary_allowed("codex:moving", None, now));
    }

    #[test]
    fn durable_fence_blocks_only_strict_on_missing_rollback_and_same_revision_changes() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .dispatch(Some("codex:fenced"), "cx-a", None, "first", Utc::now())
            .unwrap();
        store
            .set_session(
                &session_id("codex:fenced"),
                Some(settings(Mode::Recover)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        let old = fs::read(&store.path).unwrap();
        let old_fence = fs::read(fence_path(&store.path)).unwrap();
        store
            .set_session(
                &session_id("codex:fenced"),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        let latest = fs::read(&store.path).unwrap();
        let latest_fence = fs::read(fence_path(&store.path)).unwrap();
        for bytes in [
            None,
            Some(old.clone()),
            Some({
                let mut value: serde_json::Value = serde_json::from_slice(&latest).unwrap();
                value["sessions"][session_id("codex:fenced")]["explicit"]["mode"] =
                    "rebalance".into();
                serde_json::to_vec(&value).unwrap()
            }),
        ] {
            if let Some(bytes) = bytes {
                fs::write(&store.path, bytes).unwrap();
            } else {
                fs::remove_file(&store.path).unwrap();
            }
            assert!(store.reload().is_err());
            for _ in 0..2 {
                assert!(
                    !PolicyStore::new(home.path(), Settings::default())
                        .unwrap()
                        .permits_dispatch(Some("codex:fenced"), "cx-a")
                );
            }
            assert_eq!(store.plan(Some("codex:fenced")).settings.mode, Mode::Strict);
            assert_eq!(fs::read(fence_path(&store.path)).unwrap(), latest_fence);
            fs::write(&store.path, &latest).unwrap();
        }
        // While alive, even a coordinated rollback of both files is rejected.
        fs::write(&store.path, &old).unwrap();
        fs::write(fence_path(&store.path), old_fence).unwrap();
        assert!(store.reload().is_err());
        fs::write(&store.path, &latest).unwrap();
        fs::write(fence_path(&store.path), latest_fence).unwrap();
        store.reload().unwrap();
        store
            .set_session(
                &session_id("codex:fenced"),
                Some(settings(Mode::Recover)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        store.reload().unwrap();
        assert_eq!(
            store.plan(Some("codex:fenced")).settings.mode,
            Mode::Recover
        );
        assert!(PolicyStore::new(home.path(), Settings::default()).is_ok());
    }
    #[test]
    fn interrupted_fence_before_policy_replace_blocks_strict_and_keeps_evidence() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .set_session(
                &session_id("codex:crash"),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        let original = fs::read(&store.path).unwrap();
        let mut pending = (*store.snapshot()).clone();
        pending.revision += 1;
        write_fence(&store.path, &pending).unwrap();
        for _ in 0..2 {
            let restarted = PolicyStore::new(home.path(), Settings::default()).unwrap();
            assert!(!restarted.permits_dispatch(Some("codex:crash"), "cx-a"));
            assert!(restarted.permits_dispatch(Some("codex:unrelated"), "cx-b"));
        }
        assert_eq!(fs::read(&store.path).unwrap(), original);
    }
    #[test]
    fn recovered_anchor_writer_and_reader_share_identifier_validation() {
        let home = home();
        let store = PolicyStore::new(home.path(), settings(Mode::Strict)).unwrap();
        for invalid in ["bad/account", "sk-secret", "bad:account"] {
            assert!(
                store
                    .capture_strict_anchors([("codex:recover", invalid)])
                    .is_err()
            );
            assert!(store.plan(Some("codex:recover")).anchor.is_none());
            assert!(!store.path.exists());
        }
        store
            .capture_strict_anchors([("codex:recover", "cx-safe")])
            .unwrap();
        store.reload().unwrap();
        assert!(store.permits_dispatch(Some("codex:recover"), "cx-safe"));
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
    fn corrupt_legacy_without_known_strict_evidence_quarantines_and_serves_defaults() {
        let home = home();
        fs::create_dir_all(home.path().join("state")).unwrap();
        let path = home.path().join("state/routing-policies.json");
        fs::write(&path, b"not-json").unwrap();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert!(store.permits_dispatch(Some("codex:unrelated"), "cx-a"));
        assert!(!path.exists());
        assert_eq!(store.take_warnings()[0].code, "routing_policy_quarantined");
        let files: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|f| f.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(fs::read(&files[0]).unwrap(), b"not-json");
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
    fn implicit_history_is_volatile_and_survives_in_process_reload() {
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
        assert!(!store.path.exists());
        assert!(!fence_path(&store.path).exists());
        store.reload().unwrap();
        assert_eq!(
            store.snapshot().sessions[&session_id("codex:global")]
                .movements
                .len(),
            1
        );
        drop(store);
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert!(store.snapshot().sessions.is_empty());
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
                !PolicyStore::new(home.path(), Settings::default())
                    .unwrap()
                    .permits_dispatch(Some("codex:schema"), "cx-a"),
                "missing {field}"
            );
        }
        let mut bad = original.clone();
        bad["sessions"][&id]["explict"] = bad["sessions"][&id]["explicit"].clone();
        fs::write(&store.path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(
            !PolicyStore::new(home.path(), Settings::default())
                .unwrap()
                .permits_dispatch(Some("codex:schema"), "cx-a")
        );
        let mut bad = original;
        bad["sessions"][&id]["baseline"]
            .as_object_mut()
            .unwrap()
            .remove("mode");
        fs::write(&store.path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(
            !PolicyStore::new(home.path(), Settings::default())
                .unwrap()
                .permits_dispatch(Some("codex:schema"), "cx-a")
        );
    }
    #[test]
    fn policy_load_quarantines_nonregular_and_oversized_files_without_default_denial() {
        for directory in [true, false] {
            let home = home();
            let path = home.path().join("state/routing-policies.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if directory {
                fs::create_dir(&path).unwrap();
            } else {
                fs::File::create(&path)
                    .unwrap()
                    .set_len(MAX_FILE + 1)
                    .unwrap();
            }
            let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
            assert!(store.permits_dispatch(Some("codex:default"), "cx-a"));
            assert!(!store.take_warnings().is_empty());
        }
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
    fn clock() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn r8_default_history_guards_survive_reload_without_policy_writes() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .dispatch(
                Some("codex:volatile"),
                "cx-b",
                Some("cx-a"),
                "overload",
                clock(),
            )
            .unwrap();
        assert!(!store.discretionary_allowed("codex:volatile", Some("cx-c"), clock()));
        assert_eq!(store.snapshot().pool_moves["cx"].len(), 1);
        assert!(!store.path.exists());
        assert!(!fence_path(&store.path).exists());
        store.reload().unwrap();
        assert!(!store.discretionary_allowed("codex:volatile", Some("cx-c"), clock()));
        assert_eq!(store.snapshot().pool_moves["cx"].len(), 1);
        assert!(!store.path.exists());
    }

    #[test]
    fn r8_io_failure_blocks_only_strict_and_freeze_blocks_everyone() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .set_session(
                &session_id("codex:strict"),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        store
            .set_session(
                &session_id("codex:recover"),
                Some(settings(Mode::Recover)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        fs::remove_file(&store.path).unwrap();
        fs::create_dir(&store.path).unwrap();
        assert!(
            store
                .put_project(
                    None,
                    Project {
                        name: "failure".into(),
                        settings: Settings::default()
                    }
                )
                .is_err()
        );
        assert!(!store.permits_dispatch(Some("codex:strict"), "cx-a"));
        assert!(store.permits_dispatch(Some("codex:recover"), "cx-a"));
        assert!(store.permits_dispatch(None, "cx-a"));
        store
            .dispatch(
                Some("codex:recover"),
                "cx-b",
                Some("cx-a"),
                "forced",
                clock(),
            )
            .unwrap();
        store
            .dispatch(
                Some("codex:default"),
                "cx-b",
                Some("cx-a"),
                "forced",
                clock(),
            )
            .unwrap();
        assert_eq!(
            store.plan(Some("codex:recover")).anchor.as_deref(),
            Some("cx-b")
        );
        assert!(store.list()["projects"].as_object().unwrap().is_empty());
        assert!(
            store
                .take_warnings()
                .iter()
                .any(|w| w.code == "routing_policy_write_failed")
        );
        store.freeze();
        assert!(!store.permits_dispatch(Some("codex:default"), "cx-b"));
        assert!(
            store
                .dispatch(
                    Some("codex:default"),
                    "cx-c",
                    Some("cx-b"),
                    "forced",
                    clock()
                )
                .is_err()
        );
        assert!(!store.permits_dispatch(None, "cx-a"));
    }

    #[test]
    fn r8_corrupt_policy_uses_witness_for_selective_recovery_and_aliases() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let project = store
            .put_project(
                None,
                Project {
                    name: "strict".into(),
                    settings: settings(Mode::Strict),
                },
            )
            .unwrap();
        store.associate("claude:strict-root", &project).unwrap();
        store
            .set_session(
                &session_id("codex:legacy"),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        store
            .update(|state| {
                state.legacy_codex_roots.insert(session_id("codex:legacy"));
                Ok(())
            })
            .unwrap();
        let fence = fs::read(fence_path(&store.path)).unwrap();
        fs::write(&store.path, b"broken-json").unwrap();
        for _ in 0..2 {
            let restarted = PolicyStore::new(home.path(), Settings::default()).unwrap();
            assert!(!restarted.permits_dispatch(Some("claude:strict-root:agent:child"), "cl-a"));
            assert!(!restarted.permits_dispatch(Some("codex:legacy:agent:child"), "cx-a"));
            assert!(restarted.permits_dispatch(Some("codex:unrelated"), "cx-a"));
            assert!(restarted.permits_dispatch(None, "cx-a"));
            assert!(
                restarted
                    .associate("codex:new-project-root", &project)
                    .is_err()
            );
            assert!(
                restarted
                    .dispatch(Some("claude:strict-root"), "cl-a", None, "first", clock())
                    .is_err()
            );
        }
        assert_eq!(fs::read(&store.path).unwrap(), b"broken-json");
        assert_eq!(fs::read(fence_path(&store.path)).unwrap(), fence);
    }

    #[test]
    fn r8_pending_strict_creation_and_removal_keep_union_witness() {
        for removal in [false, true] {
            let home = home();
            let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
            store
                .set_session(
                    &session_id("codex:pending"),
                    Some(settings(if removal { Mode::Strict } else { Mode::Recover })),
                    None,
                    Some("cx-a"),
                )
                .unwrap();
            let previous = (*store.snapshot()).clone();
            let mut next = previous.clone();
            next.revision += 1;
            next.sessions
                .get_mut(&session_id("codex:pending"))
                .unwrap()
                .explicit = Some(settings(if removal { Mode::Recover } else { Mode::Strict }));
            let mut union = strict_intent(&previous, &Settings::default());
            merge_intent(&mut union, strict_intent(&next, &Settings::default()));
            write_pending_fence(&store.path, &next, union).unwrap();
            // Test both crash boundaries: before and after policy replacement.
            for replace_policy in [false, true] {
                if replace_policy {
                    crate::bindings::atomic_write(&store.path, &serde_json::to_vec(&next).unwrap())
                        .unwrap();
                }
                let restarted = PolicyStore::new(home.path(), Settings::default()).unwrap();
                assert!(!restarted.permits_dispatch(Some("codex:pending"), "cx-a"));
                assert!(restarted.permits_dispatch(Some("codex:unrelated"), "cx-b"));
            }
        }
    }

    #[test]
    fn r8_forced_ring_never_consumes_discretionary_budgets() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .dispatch(
                Some("codex:ring"),
                "cx-b",
                Some("cx-a"),
                "overload",
                clock(),
            )
            .unwrap();
        let mut previous = "cx-b".to_owned();
        for n in 0..80 {
            let target = format!("cx-forced-{n}");
            store
                .dispatch(
                    Some("codex:ring"),
                    &target,
                    Some(&previous),
                    "forced",
                    clock(),
                )
                .unwrap();
            previous = target;
        }
        let state = store.snapshot();
        let movements = &state.sessions[&session_id("codex:ring")].movements;
        assert_eq!(
            movements.iter().filter(|m| !m.discretionary).count(),
            MAX_FORCED_HISTORY
        );
        assert_eq!(movements.iter().filter(|m| m.discretionary).count(), 1);
        assert_eq!(state.pool_moves["cx"].len(), 1);
        assert!(!store.discretionary_allowed("codex:ring", Some("cx-new"), clock()));
        assert!(!store.discretionary_allowed("codex:other", Some("cx-new"), clock()));
        assert!(!store.path.exists());
    }

    #[test]
    fn r8_pool_budget_uses_account_id_not_session_prefix() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        for n in 0..4 {
            // Intentionally different from the session's usual provider prefix.
            store
                .dispatch(
                    Some(&format!("claude:pool-{n}")),
                    &format!("cx-target-{n}"),
                    Some("cx-source"),
                    "overload",
                    clock(),
                )
                .unwrap();
        }
        assert!(!store.discretionary_allowed("claude:next", Some("cx-unused"), clock()));
        assert!(store.discretionary_allowed("codex:next", Some("cl-unused"), clock()));
        assert!(store.discretionary_allowed("claude:next", None, clock()));
        store.reload().unwrap();
        assert!(!store.discretionary_allowed("claude:next", Some("cx-unused"), clock()));
    }

    #[test]
    fn r8_failed_strict_first_placement_never_authorizes_send() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .set_session(
                &session_id("codex:first"),
                Some(settings(Mode::Strict)),
                None,
                None,
            )
            .unwrap();
        fs::remove_file(&store.path).unwrap();
        fs::create_dir(&store.path).unwrap();
        assert!(
            store
                .dispatch(Some("codex:first"), "cx-a", None, "first", clock())
                .is_err()
        );
        assert!(!store.permits_dispatch(Some("codex:first"), "cx-a"));
        assert!(store.permits_dispatch(Some("codex:default"), "cx-a"));
        assert!(store.plan(Some("codex:first")).anchor.is_none());
    }

    #[test]
    fn r8_legacy_digest_only_corruption_does_not_block_unprovable_strict_intent() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .set_session(
                &session_id("codex:old"),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        let mut fence: serde_json::Value =
            serde_json::from_slice(&fs::read(fence_path(&store.path)).unwrap()).unwrap();
        fence["schema"] = 1.into();
        fence.as_object_mut().unwrap().remove("strict");
        fence.as_object_mut().unwrap().remove("pending");
        crate::bindings::atomic_write(
            &fence_path(&store.path),
            &serde_json::to_vec(&fence).unwrap(),
        )
        .unwrap();
        fs::write(&store.path, b"unparseable legacy data").unwrap();
        let restarted = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert!(restarted.permits_dispatch(Some("codex:unrelated"), "cx-a"));
        assert_eq!(
            restarted.plan(Some("codex:old")).settings.mode,
            Mode::Rebalance
        );
        assert!(
            restarted
                .take_warnings()
                .iter()
                .any(|w| w.code == "routing_policy_quarantined")
        );
        assert_eq!(fs::read_dir(home.path().join("state")).unwrap().count(), 2);
    }

    #[test]
    fn r8_legacy_positive_strict_evidence_survives_invalid_unrelated_fields() {
        let home = home();
        let path = home.path().join("state/routing-policies.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let value = serde_json::json!({"schema":1,"revision":3,"projects":{},"sessions":{
            session_id("codex:known"): {"explicit":{"mode":"strict"},"anchor":"cx-a"},
            session_id("codex:broken"): {"unrelated":"invalid"}
        }});
        crate::bindings::atomic_write(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        let restarted = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert!(!restarted.permits_dispatch(Some("codex:known:agent:child"), "cx-a"));
        assert!(restarted.permits_dispatch(Some("codex:unrelated"), "cx-a"));
        assert!(path.exists());
    }

    #[test]
    fn r8_witness_rejects_over_capacity_without_unrelated_denial() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let mut witness = State::default();
        for n in 0..=MAX_PROJECTS {
            witness.projects.insert(
                format!("{n:032x}"),
                Project {
                    name: "strict".into(),
                    settings: settings(Mode::Strict),
                },
            );
        }
        assert!(write_pending_fence(&store.path, &State::default(), witness).is_err());
        assert!(!fence_path(&store.path).exists());
        assert!(store.permits_dispatch(Some("codex:default"), "cx-a"));
    }
    #[test]
    fn sol1_lifecycle_import_captures_anchor_before_releasing_strict_block() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .set_session(
                &session_id("codex:import"),
                Some(settings(Mode::Strict)),
                None,
                None,
            )
            .unwrap();
        store.begin_recovery();
        // Import calls reload between begin_recovery and anchor capture.
        store.reload().unwrap();
        assert!(!store.permits_dispatch(Some("codex:import"), "cx-a"));
        assert!(store.permits_dispatch(Some("codex:other"), "cx-a"));
        store
            .capture_strict_anchors([("codex:import", "cx-a")])
            .unwrap();
        assert_eq!(
            store.plan(Some("codex:import")).anchor.as_deref(),
            Some("cx-a")
        );
        assert!(!store.permits_dispatch(Some("codex:import"), "cx-a"));
        store.finish_recovery();
        assert!(store.permits_dispatch(Some("codex:import"), "cx-a"));
        assert!(
            store
                .dispatch(Some("codex:import"), "cx-b", None, "first", clock())
                .is_err()
        );
        let restarted = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert!(restarted.permits_dispatch(Some("codex:import"), "cx-a"));
        assert!(!restarted.permits_dispatch(Some("codex:import"), "cx-b"));
    }

    #[test]
    fn sol2_legacy_negative_exception_survives_policy_or_fence_corruption() {
        for corrupt_policy in [false, true] {
            let home = home();
            let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
            let parent = "codex:legacy-exception";
            let child = "codex:legacy-exception:agent:independent";
            store
                .set_session(
                    &session_id(parent),
                    Some(settings(Mode::Strict)),
                    None,
                    Some("cx-a"),
                )
                .unwrap();
            store
                .set_session(
                    &session_id(child),
                    Some(settings(Mode::Rebalance)),
                    None,
                    Some("cx-b"),
                )
                .unwrap();
            store
                .update(|state| {
                    state.legacy_codex_roots.insert(session_id(parent));
                    Ok(())
                })
                .unwrap();
            let fence = read_fence(&store.path).unwrap().unwrap();
            let witness = fence.strict.unwrap();
            assert_eq!(
                witness.sessions[&session_id(child)]
                    .explicit
                    .as_ref()
                    .unwrap()
                    .mode,
                Mode::Rebalance
            );
            assert!(witness.sessions[&session_id(child)].anchor.is_none());
            assert!(witness.sessions[&session_id(child)].movements.is_empty());
            let broken = if corrupt_policy {
                store.path.clone()
            } else {
                fence_path(&store.path)
            };
            fs::write(&broken, b"broken").unwrap();
            assert!(store.reload().is_err());
            assert!(!store.permits_dispatch(Some(parent), "cx-a"));
            assert!(
                !store.permits_dispatch(Some("codex:legacy-exception:agent:inherited"), "cx-a")
            );
            assert!(store.permits_dispatch(Some(child), "cx-b"));
            store
                .dispatch(Some(child), "cx-c", Some("cx-b"), "forced", clock())
                .unwrap();
            for _ in 0..2 {
                let restarted = PolicyStore::new(home.path(), Settings::default()).unwrap();
                assert!(!restarted.permits_dispatch(Some(parent), "cx-a"));
                assert!(restarted.permits_dispatch(Some(child), "cx-b"));
                assert!(restarted.permits_dispatch(Some("codex:unrelated"), "cx-b"));
            }
        }
    }

    #[test]
    fn sol2_volatile_child_cannot_shadow_recovered_strict_alias() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .set_session(
                &session_id("codex:aliased"),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-a"),
            )
            .unwrap();
        store
            .update(|state| {
                state.legacy_codex_roots.insert(session_id("codex:aliased"));
                Ok(())
            })
            .unwrap();
        // Pre-existing volatile history is not an explicit alias exception.
        let mut state = (*store.snapshot()).clone();
        state.sessions.insert(
            session_id("codex:aliased:agent:child"),
            SessionPolicy {
                project: None,
                baseline: Settings::default(),
                explicit: None,
                anchor: Some("cx-b".into()),
                movements: Vec::new(),
            },
        );
        *store.state.lock().unwrap() = Arc::new(state);
        fs::write(&store.path, b"broken").unwrap();
        assert!(store.reload().is_err());
        assert_eq!(
            store.plan(Some("codex:aliased:agent:child")).settings.mode,
            Mode::Strict
        );
        assert!(!store.permits_dispatch(Some("codex:aliased:agent:child"), "cx-b"));
    }

    #[test]
    fn sol3_reload_keeps_newer_volatile_history_over_legacy_disk_row() {
        let home = home();
        let path = home.path().join("state/routing-policies.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut legacy = State {
            revision: 7,
            ..State::default()
        };
        legacy.sessions.insert(
            session_id("codex:legacy-history"),
            SessionPolicy {
                project: None,
                baseline: Settings::default(),
                explicit: None,
                anchor: Some("cx-a".into()),
                movements: Vec::new(),
            },
        );
        let original = serde_json::to_vec(&legacy).unwrap();
        crate::bindings::atomic_write(&path, &original).unwrap();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        store
            .dispatch(
                Some("codex:legacy-history"),
                "cx-b",
                Some("cx-a"),
                "overload",
                clock(),
            )
            .unwrap();
        store.reload().unwrap();
        assert_eq!(
            store.plan(Some("codex:legacy-history")).anchor.as_deref(),
            Some("cx-b")
        );
        assert_eq!(
            store.snapshot().sessions[&session_id("codex:legacy-history")]
                .movements
                .len(),
            1
        );
        assert_eq!(store.snapshot().pool_moves["cx"].len(), 1);
        assert!(!store.discretionary_allowed("codex:legacy-history", Some("cx-c"), clock()));
        assert!(!store.discretionary_allowed(
            "codex:legacy-history",
            Some("cx-a"),
            clock() + chrono::Duration::seconds(1801)
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!fence_path(&path).exists());
    }

    #[test]
    fn sol4_committed_witness_matches_durable_projection_with_global_strict() {
        let home = home();
        let store = PolicyStore::new(home.path(), settings(Mode::Strict)).unwrap();
        store
            .set_session(&session_id("codex:phantom"), None, None, None)
            .unwrap();
        assert!(!store.path.exists());
        store
            .put_project(
                None,
                Project {
                    name: "explicit".into(),
                    settings: settings(Mode::Recover),
                },
            )
            .unwrap();
        let loaded = load(&store.path).unwrap().unwrap();
        let fence = read_fence(&store.path).unwrap().unwrap();
        assert_eq!(
            policy_digest(&fence.strict.unwrap()).unwrap(),
            policy_digest(&strict_intent(&loaded, &settings(Mode::Strict))).unwrap()
        );
        assert!(!fence.pending);
        store.reload().unwrap();
        store
            .dispatch(Some("codex:phantom"), "cx-a", None, "first", clock())
            .unwrap();
        assert!(store.permits_dispatch(Some("codex:phantom"), "cx-a"));
        let restarted = PolicyStore::new(home.path(), settings(Mode::Strict)).unwrap();
        assert!(restarted.permits_dispatch(Some("codex:phantom"), "cx-a"));
        restarted
            .dispatch(Some("codex:another"), "cx-b", None, "first", clock())
            .unwrap();
    }

    fn capacity_state(explicit: bool, expired: bool) -> State {
        let mut state = State::default();
        let at = clock() - chrono::Duration::seconds(if expired { 86401 } else { 0 });
        for n in 0..MAX_SESSIONS {
            state.sessions.insert(
                format!("{n:064x}"),
                SessionPolicy {
                    project: None,
                    baseline: Settings::default(),
                    explicit: explicit.then(Settings::default),
                    anchor: Some("cx-b".into()),
                    movements: vec![Movement {
                        id: Some(format!("{n:032x}")),
                        from: "cx-a".into(),
                        to: "cx-b".into(),
                        at,
                        discretionary: false,
                    }],
                },
            );
        }
        state.reindex();
        state
    }

    #[test]
    fn sol5_full_expired_volatile_history_prunes_and_records_new_forced_move() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let mut state = capacity_state(false, true);
        // A live discretionary guard must survive the capacity sweep.
        let protected = state.sessions.get_mut(&format!("{:064x}", 0)).unwrap();
        protected.movements[0].at = clock();
        protected.movements[0].discretionary = true;
        *store.state.lock().unwrap() = Arc::new(state);
        store
            .dispatch(
                Some("codex:capacity-new"),
                "cx-c",
                Some("cx-a"),
                "forced",
                clock(),
            )
            .unwrap();
        let state = store.snapshot();
        assert_eq!(state.sessions.len(), 2);
        assert!(state.sessions.contains_key(&format!("{:064x}", 0)));
        assert_eq!(
            state.sessions[&session_id("codex:capacity-new")]
                .movements
                .len(),
            1
        );
        assert_eq!(state.pool_moves["cx"].len(), 1);
        assert!(state.history_overflow_until.is_none());
        assert!(!store.path.exists());
        assert!(!fence_path(&store.path).exists());
    }

    #[test]
    fn sol5_full_live_volatile_history_falls_back_and_preserves_overflow_on_reload() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        *store.state.lock().unwrap() = Arc::new(capacity_state(false, false));
        for n in 0..4 {
            store
                .dispatch(
                    Some(&format!("codex:overflow-{n}")),
                    "cx-c",
                    Some("cx-a"),
                    "forced",
                    clock(),
                )
                .unwrap();
        }
        assert_eq!(store.snapshot().sessions.len(), MAX_SESSIONS);
        assert!(store.permits_dispatch(Some("codex:overflow-0"), "cx-c"));
        assert_eq!(
            store
                .take_warnings()
                .iter()
                .filter(|w| w.code == "routing_forced_history_overflow")
                .count(),
            1
        );
        assert!(!store.discretionary_allowed("codex:next", Some("cx-unused"), clock()));
        assert!(
            store
                .dispatch(
                    Some("codex:next"),
                    "cx-unused",
                    Some("cx-a"),
                    "overload",
                    clock()
                )
                .is_err()
        );
        let deadline = store.snapshot().history_overflow_until.unwrap();
        store.reload().unwrap();
        assert_eq!(store.snapshot().history_overflow_until, Some(deadline));
        assert!(!store.discretionary_allowed(
            "codex:next",
            Some("cx-unused"),
            deadline - chrono::Duration::seconds(1)
        ));
        assert!(store.discretionary_allowed("codex:next", Some("cx-unused"), deadline));
        assert!(!store.path.exists());
        assert!(!fence_path(&store.path).exists());
    }

    #[test]
    fn sol5_full_durable_intent_never_blocks_distinct_default_forced_move() {
        let home = home();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        let state = capacity_state(true, false);
        let bytes = serde_json::to_vec(&state).unwrap();
        // One fixture write, no 20,000-update/fsync loop.
        crate::bindings::atomic_write(&store.path, &bytes).unwrap();
        write_fence(&store.path, &state).unwrap();
        *store.state.lock().unwrap() = Arc::new(state);
        for n in 0..4 {
            store
                .dispatch(
                    Some(&format!("codex:durable-overflow-{n}")),
                    "cx-c",
                    Some("cx-a"),
                    "forced",
                    clock(),
                )
                .unwrap();
        }
        assert_eq!(store.snapshot().sessions.len(), MAX_SESSIONS);
        assert!(
            store
                .snapshot()
                .sessions
                .values()
                .all(|s| s.explicit.is_some())
        );
        assert!(store.snapshot().history_overflow_until.is_some());
        assert_eq!(fs::read(&store.path).unwrap(), bytes);
        store.reload().unwrap();
        assert_eq!(store.snapshot().sessions.len(), MAX_SESSIONS);
        assert!(store.snapshot().history_overflow_until.is_some());
    }
    #[test]
    fn close1_global_strict_keeps_explicit_recover_exception_on_corruption() {
        let home = home();
        let store = PolicyStore::new(home.path(), settings(Mode::Strict)).unwrap();
        store
            .set_session(
                &session_id("codex:recover-exception"),
                Some(settings(Mode::Recover)),
                None,
                Some("cx-r"),
            )
            .unwrap();
        store
            .set_session(
                &session_id("codex:actual-strict"),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-s"),
            )
            .unwrap();
        fs::write(&store.path, b"broken").unwrap();
        assert!(store.reload().is_err());
        assert_eq!(
            store.plan(Some("codex:recover-exception")).settings.mode,
            Mode::Recover
        );
        assert!(store.permits_dispatch(Some("codex:recover-exception"), "cx-r"));
        assert!(!store.permits_dispatch(Some("codex:actual-strict"), "cx-s"));
        assert!(!store.permits_dispatch(Some("codex:unknown-strict"), "cx-s"));
        let restarted = PolicyStore::new(home.path(), settings(Mode::Strict)).unwrap();
        assert_eq!(
            restarted
                .plan(Some("codex:recover-exception"))
                .settings
                .mode,
            Mode::Recover
        );
        assert!(restarted.permits_dispatch(Some("codex:recover-exception"), "cx-r"));
        assert!(!restarted.permits_dispatch(Some("codex:actual-strict"), "cx-s"));
    }

    #[test]
    fn close2_salvage_keeps_valid_legacy_child_exception_with_unrelated_invalid_row() {
        let home = home();
        let path = home.path().join("state/routing-policies.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let value = serde_json::json!({"schema":1,"revision":4,"projects":{},"sessions":{
            session_id("codex:salvage-parent"): {"project":null,"baseline":Settings::default(),"explicit":settings(Mode::Strict),"anchor":"cx-a","movements":[]},
            session_id("codex:salvage-parent:agent:exception"): {"project":null,"baseline":Settings::default(),"explicit":settings(Mode::Recover),"anchor":"cx-b","movements":[]},
            session_id("codex:invalid"): {"unrelated":"invalid"}
        }});
        crate::bindings::atomic_write(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        for _ in 0..2 {
            let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
            assert!(!store.permits_dispatch(Some("codex:salvage-parent"), "cx-a"));
            assert!(!store.permits_dispatch(Some("codex:salvage-parent:agent:inherited"), "cx-a"));
            assert_eq!(
                store
                    .plan(Some("codex:salvage-parent:agent:exception"))
                    .settings
                    .mode,
                Mode::Recover
            );
            assert!(store.permits_dispatch(Some("codex:salvage-parent:agent:exception"), "cx-b"));
        }
        assert!(path.exists());
        assert!(!fence_path(&path).exists());
    }

    #[test]
    fn close3_salvage_uses_frozen_baseline_not_current_project_mode() {
        for baseline_mode in [Mode::Rebalance, Mode::Strict] {
            let home = home();
            let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
            let project = store
                .put_project(
                    None,
                    Project {
                        name: "baseline".into(),
                        settings: settings(baseline_mode),
                    },
                )
                .unwrap();
            store
                .associate_with_anchor("codex:baseline-root", &project, Some("cx-a"))
                .unwrap();
            let current_mode = if baseline_mode == Mode::Strict {
                Mode::Rebalance
            } else {
                Mode::Strict
            };
            store
                .put_project(
                    Some(&project),
                    Project {
                        name: "changed".into(),
                        settings: settings(current_mode),
                    },
                )
                .unwrap();
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
            value["sessions"][session_id("codex:invalid")] =
                serde_json::json!({"unrelated":"invalid"});
            fs::write(&store.path, serde_json::to_vec(&value).unwrap()).unwrap();
            // Exercise salvage itself, without a valid witness masking its result.
            fs::remove_file(fence_path(&store.path)).unwrap();
            let restarted = PolicyStore::new(home.path(), Settings::default()).unwrap();
            assert_eq!(
                restarted.plan(Some("codex:baseline-root")).settings.mode,
                baseline_mode
            );
            assert_eq!(
                restarted.permits_dispatch(Some("codex:baseline-root"), "cx-a"),
                baseline_mode != Mode::Strict
            );
            assert!(restarted.permits_dispatch(Some("codex:unrelated"), "cx-a"));
        }
    }

    #[test]
    fn close4_forced_ring_eviction_preserves_live_revisit_guards() {
        for revisit in [true, false] {
            let home = home();
            let limits = ChurnLimits {
                revisit_secs: if revisit { 3600 } else { 120 },
                ..ChurnLimits::default()
            };
            let store = PolicyStore::new(home.path(), Settings::default())
                .unwrap()
                .with_limits(limits)
                .unwrap();
            let mut from = "cx-original".to_owned();
            for n in 0..33 {
                let to = format!("cx-ring-{n}");
                store
                    .dispatch(
                        Some("codex:evict-ring"),
                        &to,
                        Some(&from),
                        "forced",
                        clock(),
                    )
                    .unwrap();
                from = to;
            }
            assert_eq!(
                store.snapshot().sessions[&session_id("codex:evict-ring")]
                    .movements
                    .len(),
                MAX_FORCED_HISTORY
            );
            let target = "cx-original";
            let key = "codex:evict-ring";
            let deadline = clock() + chrono::Duration::seconds(if revisit { 3600 } else { 120 });
            assert_eq!(store.snapshot().history_overflow_until, Some(deadline));
            assert!(!store.discretionary_allowed(key, Some(target), clock()));
            assert!(
                store
                    .dispatch(Some(key), target, Some("cx-ring-32"), "overload", clock())
                    .is_err()
            );
            store.reload().unwrap();
            assert!(!store.discretionary_allowed(
                key,
                Some(target),
                deadline - chrono::Duration::seconds(1)
            ));
            assert!(store.discretionary_allowed(key, Some(target), deadline));
            // No discretionary budget was charged, and another forced move still succeeds.
            assert!(store.snapshot().pool_moves.is_empty());
            store
                .dispatch(
                    Some("codex:evict-ring"),
                    "cx-forced-again",
                    Some("cx-ring-32"),
                    "forced",
                    clock(),
                )
                .unwrap();
            assert!(!store.path.exists());
        }
    }

    #[test]
    fn close5_oversized_two_file_recovery_keeps_both_strict_sets_and_allows_forced() {
        let home = home();
        let path = home.path().join("state/routing-policies.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut old = capacity_state(true, false);
        for session in old.sessions.values_mut() {
            session.explicit = Some(settings(Mode::Strict));
        }
        let removed = session_id("codex:old-strict-generation");
        let old_row = old.sessions.remove(&format!("{:064x}", 0)).unwrap();
        old.sessions.insert(removed.clone(), old_row);
        write_fence(&path, &old).unwrap();
        let added = session_id("codex:new-strict-generation");
        let mut new = old.clone();
        let row = new.sessions.remove(&removed).unwrap();
        new.sessions.insert(added.clone(), row);
        new.revision = 1;
        validate_state(&old).unwrap();
        validate_state(&new).unwrap();
        let bytes = serde_json::to_vec(&new).unwrap();
        crate::bindings::atomic_write(&path, &bytes).unwrap();
        let fence = fs::read(fence_path(&path)).unwrap();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert_eq!(store.snapshot().sessions.len(), MAX_SESSIONS + 1);
        assert!(!store.permits_dispatch(Some("codex:new-strict-generation"), "cx-b"));
        assert!(!store.permits_dispatch(Some("codex:old-strict-generation"), "cx-b"));
        assert_eq!(
            store.resolve(&store.snapshot(), &removed).settings.mode,
            Mode::Strict
        );
        assert!(store.recovery_blocked.load(Ordering::Acquire));
        for n in 0..4 {
            store
                .dispatch(
                    Some(&format!("codex:oversize-default-{n}")),
                    "cx-c",
                    Some("cx-a"),
                    "forced",
                    clock(),
                )
                .unwrap();
        }
        let state = store.snapshot();
        assert_eq!(state.sessions.len(), MAX_SESSIONS + 1);
        assert!(state.sessions.contains_key(&removed));
        assert!(state.sessions.contains_key(&added));
        assert!(!store.permits_dispatch(Some("codex:old-strict-generation"), "cx-b"));
        assert!(!store.permits_dispatch(Some("codex:new-strict-generation"), "cx-b"));
        assert_eq!(
            state.history_overflow_until,
            Some(clock() + chrono::Duration::seconds(3600))
        );
        assert!(store.permits_dispatch(Some("codex:oversize-default-0"), "cx-c"));
        assert!(!store.discretionary_allowed("codex:oversize-default-0", Some("cx-c"), clock()));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read(fence_path(&path)).unwrap(), fence);
        assert_eq!(
            store
                .take_warnings()
                .iter()
                .filter(|w| w.code == "routing_forced_history_overflow")
                .count(),
            1
        );
        store.freeze();
        assert!(
            store
                .dispatch(
                    Some("codex:oversize-default-0"),
                    "cx-d",
                    Some("cx-c"),
                    "forced",
                    clock()
                )
                .is_err()
        );
    }

    fn publication_pause(
        store: &PolicyStore,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        *store.publication_hook.lock().unwrap() = Some(Arc::new(PublicationHook {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }));
        (entered_rx, release_tx)
    }

    fn assert_publication_reader_blocked(
        store: &Arc<PolicyStore>,
        key: &'static str,
        entered: std::sync::mpsc::Receiver<()>,
        release: std::sync::mpsc::SyncSender<()>,
    ) {
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // The writer hook is between flag and authority writes. This assertion
        // proves that no reader can inspect the deliberately split internal state.
        let gated = store.publication.try_read().is_err();
        let start = Arc::new(std::sync::Barrier::new(2));
        let reader = store.clone();
        let reader_start = start.clone();
        let task = std::thread::spawn(move || {
            reader_start.wait();
            (
                reader.plan(Some(key)).settings.mode,
                reader.permits_dispatch(Some(key), "cx-wrong"),
                reader.permits_dispatch(Some("codex:existing-nonstrict"), "cx-nonstrict"),
            )
        });
        start.wait();
        release.send(()).unwrap();
        let (mode, wrong_allowed, nonstrict_allowed) = task.join().unwrap();
        assert!(gated);
        assert_eq!(mode, Mode::Strict);
        assert!(!wrong_allowed);
        assert!(nonstrict_allowed);
        *store.publication_hook.lock().unwrap() = None;
    }

    #[test]
    fn integrated1_failed_new_strict_edit_publishes_authority_atomically() {
        let home = home();
        let store = Arc::new(PolicyStore::new(home.path(), Settings::default()).unwrap());
        store
            .set_session(
                &session_id("codex:existing-nonstrict"),
                Some(settings(Mode::Recover)),
                None,
                Some("cx-nonstrict"),
            )
            .unwrap();
        fs::remove_file(&store.path).unwrap();
        fs::create_dir(&store.path).unwrap();
        assert!(store.permits_dispatch(Some("codex:existing-nonstrict"), "cx-nonstrict"));
        let (entered, release) = publication_pause(&store);
        let writer = store.clone();
        let task = std::thread::spawn(move || {
            writer.set_session(
                &session_id("codex:new-strict-race"),
                Some(settings(Mode::Strict)),
                None,
                Some("cx-right"),
            )
        });
        assert_publication_reader_blocked(&store, "codex:new-strict-race", entered, release);
        assert!(task.join().unwrap().is_err());
        assert!(!store.permits_dispatch(Some("codex:new-strict-race"), "cx-wrong"));
        assert!(
            store
                .dispatch(
                    Some("codex:new-strict-race"),
                    "cx-wrong",
                    None,
                    "first",
                    clock()
                )
                .is_err()
        );
    }

    #[test]
    fn integrated1_restored_strict_generation_never_exposes_previous_recover_plan() {
        let home = home();
        let store = Arc::new(PolicyStore::new(home.path(), Settings::default()).unwrap());
        store
            .set_session(
                &session_id("codex:restored-strict-race"),
                Some(settings(Mode::Recover)),
                None,
                Some("cx-right"),
            )
            .unwrap();
        store
            .set_session(
                &session_id("codex:existing-nonstrict"),
                Some(settings(Mode::Recover)),
                None,
                Some("cx-nonstrict"),
            )
            .unwrap();
        fs::remove_file(&store.path).unwrap();
        fs::create_dir(&store.path).unwrap();
        assert!(
            store
                .set_session(
                    &session_id("codex:restored-strict-race"),
                    Some(settings(Mode::Strict)),
                    None,
                    Some("cx-right")
                )
                .is_err()
        );
        assert_eq!(
            store.snapshot().sessions[&session_id("codex:restored-strict-race")]
                .explicit
                .as_ref()
                .unwrap()
                .mode,
            Mode::Recover
        );
        assert!(!store.permits_dispatch(Some("codex:restored-strict-race"), "cx-wrong"));
        let mut restored = (*store.snapshot()).clone();
        restored.revision += 1;
        restored
            .sessions
            .get_mut(&session_id("codex:restored-strict-race"))
            .unwrap()
            .explicit = Some(settings(Mode::Strict));
        fs::remove_dir(&store.path).unwrap();
        crate::bindings::atomic_write(&store.path, &serde_json::to_vec(&restored).unwrap())
            .unwrap();
        write_fence(&store.path, &restored).unwrap();
        let (entered, release) = publication_pause(&store);
        let writer = store.clone();
        let task = std::thread::spawn(move || writer.reload());
        assert_publication_reader_blocked(&store, "codex:restored-strict-race", entered, release);
        task.join().unwrap().unwrap();
        assert!(store.permits_dispatch(Some("codex:restored-strict-race"), "cx-right"));
        assert!(!store.permits_dispatch(Some("codex:restored-strict-race"), "cx-wrong"));
    }

    #[test]
    fn integrated2_duplicate_or_invalid_alias_prefix_cannot_hide_later_strict_root() {
        let home = home();
        let path = home.path().join("state/routing-policies.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let a = session_id("codex:alias-a");
        let b = session_id("codex:alias-b");
        let row = serde_json::json!({"project":null,"baseline":Settings::default(),"explicit":settings(Mode::Strict),"anchor":"cx-right","movements":[]});
        let mut aliases = vec![serde_json::Value::String(a.clone()); MAX_SESSIONS];
        aliases.push(serde_json::Value::Null);
        aliases.push("invalid".into());
        aliases.push(b.clone().into());
        let value = serde_json::json!({"schema":2,"revision":1,"projects":{},"sessions":{
            a:row.clone(), b:row, session_id("codex:malformed"): {"invalid":"row"}
        },"legacy_codex_roots":aliases});
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(bytes.len() as u64 <= MAX_FILE);
        crate::bindings::atomic_write(&path, &bytes).unwrap();
        for _ in 0..2 {
            let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
            for root in [
                "codex:alias-a",
                "codex:alias-b",
                "codex:alias-b:agent:child",
            ] {
                assert!(!store.permits_dispatch(Some(root), "cx-wrong"));
                assert_eq!(store.plan(Some(root)).settings.mode, Mode::Strict);
            }
            assert!(store.permits_dispatch(Some("codex:unrelated"), "cx-right"));
        }
    }

    #[test]
    fn integrated2_invalid_map_prefix_cannot_hide_positive_strict_evidence() {
        let home = home();
        let path = home.path().join("state/routing-policies.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut projects = serde_json::Map::new();
        for n in 0..MAX_PROJECTS {
            projects.insert(format!("!invalid-{n:05}"), serde_json::Value::Null);
        }
        let project = "f".repeat(32);
        projects.insert(
            project.clone(),
            serde_json::json!({"name":"strict","settings":settings(Mode::Strict)}),
        );
        let mut sessions = serde_json::Map::new();
        for n in 0..MAX_SESSIONS {
            sessions.insert(format!("!invalid-{n:05}"), serde_json::Value::Null);
        }
        let root = session_id("codex:late-strict");
        sessions.insert(root.clone(), serde_json::json!({"project":null,"baseline":Settings::default(),"explicit":settings(Mode::Strict),"anchor":"cx-right","movements":[]}));
        let bytes = serde_json::to_vec(&serde_json::json!({"schema":2,"revision":1,"projects":projects,"sessions":sessions,"legacy_codex_roots":[root]})).unwrap();
        assert!(bytes.len() as u64 <= MAX_FILE);
        crate::bindings::atomic_write(&path, &bytes).unwrap();
        let store = PolicyStore::new(home.path(), Settings::default()).unwrap();
        assert_eq!(
            store
                .plan(Some("codex:late-strict:agent:child"))
                .settings
                .mode,
            Mode::Strict
        );
        assert!(!store.permits_dispatch(Some("codex:late-strict:agent:child"), "cx-right"));
        assert!(store.associate("codex:late-project", &project).is_err());
        assert!(store.permits_dispatch(Some("codex:unrelated"), "cx-right"));
    }

    #[test]
    fn integrated3_durable_forced_eviction_guard_survives_restart_for_both_horizons() {
        for revisit in [true, false] {
            let home = home();
            let limits = ChurnLimits {
                revisit_secs: if revisit { 3600 } else { 120 },
                ..ChurnLimits::default()
            };
            let store = PolicyStore::new(home.path(), Settings::default())
                .unwrap()
                .with_limits(limits.clone())
                .unwrap();
            let key = "codex:durable-ring";
            store
                .set_session(
                    &session_id(key),
                    Some(settings(Mode::Rebalance)),
                    None,
                    Some("cx-original"),
                )
                .unwrap();
            // Seed the first 32 durable moves in one update, then exercise the
            // real 33rd-move eviction/write path without a 32-fsync fixture loop.
            store
                .update(|state| {
                    let entry = state.sessions.get_mut(&session_id(key)).unwrap();
                    for n in 0..MAX_FORCED_HISTORY {
                        entry.movements.push(Movement {
                            id: Some(format!("{n:032x}")),
                            from: if n == 0 {
                                "cx-original".into()
                            } else {
                                format!("cx-ring-{}", n - 1)
                            },
                            to: format!("cx-ring-{n}"),
                            at: clock(),
                            discretionary: false,
                        });
                    }
                    entry.anchor = Some("cx-ring-31".into());
                    Ok(())
                })
                .unwrap();
            store
                .dispatch(
                    Some(key),
                    "cx-ring-32",
                    Some("cx-ring-31"),
                    "forced",
                    clock(),
                )
                .unwrap();
            let deadline = clock() + chrono::Duration::seconds(if revisit { 3600 } else { 120 });
            assert_eq!(
                load(&store.path).unwrap().unwrap().durable_overflow_until,
                Some(deadline)
            );
            drop(store);
            let store = PolicyStore::new(home.path(), Settings::default())
                .unwrap()
                .with_limits(limits)
                .unwrap();
            assert_eq!(store.snapshot().history_overflow_until, Some(deadline));
            let (group, target) = (key, "cx-original");
            assert!(!store.discretionary_allowed(
                group,
                Some(target),
                deadline - chrono::Duration::seconds(1)
            ));
            assert!(
                store
                    .dispatch(Some(group), target, Some("cx-ring-32"), "overload", clock())
                    .is_err()
            );
            assert!(store.discretionary_allowed(group, Some(target), deadline));
            store
                .dispatch(
                    Some(key),
                    "cx-forced-after-restart",
                    Some("cx-ring-32"),
                    "forced",
                    clock(),
                )
                .unwrap();
            assert!(store.permits_dispatch(Some(key), "cx-forced-after-restart"));
        }
    }

    #[test]
    fn final_policy_promotion_persists_preexisting_volatile_eviction_guard() {
        for project_policy in [false, true] {
            for revisit in [true, false] {
                let home = home();
                let limits = ChurnLimits {
                    revisit_secs: if revisit { 3600 } else { 120 },
                    ..ChurnLimits::default()
                };
                let store = PolicyStore::new(home.path(), Settings::default())
                    .unwrap()
                    .with_limits(limits.clone())
                    .unwrap();
                let key = "codex:promoted-ring";
                let mut previous = "cx-original".to_owned();
                for n in 0..33 {
                    let target = format!("cx-ring-{n}");
                    store
                        .dispatch(Some(key), &target, Some(&previous), "forced", clock())
                        .unwrap();
                    previous = target;
                }
                let deadline =
                    clock() + chrono::Duration::seconds(if revisit { 3600 } else { 120 });
                assert_eq!(store.snapshot().history_overflow_until, Some(deadline));
                assert!(store.snapshot().durable_overflow_until.is_none());
                assert!(!store.path.exists());
                assert!(!fence_path(&store.path).exists());
                if project_policy {
                    let project = store
                        .put_project(
                            None,
                            Project {
                                name: "promotion".into(),
                                settings: settings(Mode::Rebalance),
                            },
                        )
                        .unwrap();
                    // Unrelated durable project creation must not persist the
                    // default root or its still-volatile eviction summary.
                    let loaded = load(&store.path).unwrap().unwrap();
                    assert!(loaded.sessions.is_empty());
                    assert!(loaded.durable_overflow_until.is_none());
                    let before = fs::read(&store.path).unwrap();
                    let before_fence = fs::read(fence_path(&store.path)).unwrap();
                    store
                        .dispatch(
                            Some("codex:other-default"),
                            "cx-other",
                            Some("cx-before"),
                            "forced",
                            clock(),
                        )
                        .unwrap();
                    assert_eq!(fs::read(&store.path).unwrap(), before);
                    assert_eq!(fs::read(fence_path(&store.path)).unwrap(), before_fence);
                    store
                        .edit_session(
                            &session_id(key),
                            None,
                            Some(&project),
                            Some("cx-ring-32"),
                            true,
                        )
                        .unwrap();
                } else {
                    store
                        .set_session(
                            &session_id(key),
                            Some(settings(Mode::Rebalance)),
                            None,
                            Some("cx-ring-32"),
                        )
                        .unwrap();
                }
                assert_eq!(
                    load(&store.path).unwrap().unwrap().durable_overflow_until,
                    Some(deadline)
                );
                drop(store);
                let store = PolicyStore::new(home.path(), Settings::default())
                    .unwrap()
                    .with_limits(limits)
                    .unwrap();
                let (group, target) = (key, "cx-original");
                assert_eq!(store.snapshot().history_overflow_until, Some(deadline));
                assert!(!store.discretionary_allowed(
                    group,
                    Some(target),
                    clock() + chrono::Duration::seconds(1)
                ));
                assert!(!store.discretionary_allowed(
                    group,
                    Some(target),
                    deadline - chrono::Duration::seconds(1)
                ));
                assert!(
                    store
                        .dispatch(Some(group), target, Some("cx-ring-32"), "overload", clock())
                        .is_err()
                );
                // Guard preservation must not turn into a forced-move cap.
                store
                    .dispatch(
                        Some(key),
                        "cx-forced-after-promotion",
                        Some("cx-ring-32"),
                        "forced",
                        clock(),
                    )
                    .unwrap();
                assert!(store.permits_dispatch(Some(key), "cx-forced-after-promotion"));
                assert!(store.discretionary_allowed(group, Some(target), deadline));
            }
        }
    }

    #[test]
    fn final_legacy_history_copy_promotes_eviction_guard_for_new_exact_policy() {
        for project_policy in [false, true] {
            for revisit in [true, false] {
                let home = home();
                let path = home.path().join("state/routing-policies.json");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let legacy = session_id("codex:legacy-promotion");
                let key = "codex:legacy-promotion:agent:new";
                let exact = session_id(key);
                let fixture = State {
                    sessions: BTreeMap::from([(
                        legacy.clone(),
                        SessionPolicy {
                            project: None,
                            baseline: Settings::default(),
                            explicit: None,
                            anchor: Some("cx-original".into()),
                            movements: Vec::new(),
                        },
                    )]),
                    legacy_codex_roots: BTreeSet::from([legacy.clone()]),
                    ..State::default()
                };
                let original = serde_json::to_vec(&fixture).unwrap();
                crate::bindings::atomic_write(&path, &original).unwrap();
                let limits = ChurnLimits {
                    revisit_secs: if revisit { 3600 } else { 120 },
                    ..ChurnLimits::default()
                };
                let store = PolicyStore::new(home.path(), Settings::default())
                    .unwrap()
                    .with_limits(limits.clone())
                    .unwrap();
                let mut previous = "cx-original".to_owned();
                for n in 0..33 {
                    let target = format!("cx-ring-{n}");
                    store
                        .dispatch(Some(key), &target, Some(&previous), "forced", clock())
                        .unwrap();
                    previous = target;
                }
                let deadline =
                    clock() + chrono::Duration::seconds(if revisit { 3600 } else { 120 });
                assert!(!store.snapshot().sessions.contains_key(&exact));
                assert_eq!(
                    store.snapshot().sessions[&legacy].movements.len(),
                    MAX_FORCED_HISTORY
                );
                assert_eq!(store.snapshot().history_overflow_until, Some(deadline));
                assert!(store.snapshot().durable_overflow_until.is_none());
                // Existing legacy evidence is not rewritten by default moves.
                assert_eq!(fs::read(&path).unwrap(), original);
                assert!(!fence_path(&path).exists());
                let project = if project_policy {
                    Some(
                        store
                            .put_project(
                                None,
                                Project {
                                    name: "legacy-promotion".into(),
                                    settings: settings(Mode::Rebalance),
                                },
                            )
                            .unwrap(),
                    )
                } else {
                    None
                };
                if project_policy {
                    let loaded = load(&path).unwrap().unwrap();
                    assert!(loaded.sessions.is_empty());
                    assert!(loaded.durable_overflow_until.is_none());
                }
                store
                    .edit_session_for_key(
                        &exact,
                        if project_policy {
                            None
                        } else {
                            Some(settings(Mode::Rebalance))
                        },
                        project.as_deref(),
                        Some("cx-ring-32"),
                        true,
                        Some(key),
                    )
                    .unwrap();
                let loaded = load(&path).unwrap().unwrap();
                assert_eq!(loaded.durable_overflow_until, Some(deadline));
                assert_eq!(loaded.sessions[&exact].movements.len(), MAX_FORCED_HISTORY);
                assert!(!loaded.sessions.contains_key(&legacy));
                drop(store);
                let store = PolicyStore::new(home.path(), Settings::default())
                    .unwrap()
                    .with_limits(limits)
                    .unwrap();
                let (group, target) = (key, "cx-original");
                assert_eq!(store.snapshot().history_overflow_until, Some(deadline));
                assert!(!store.discretionary_allowed(
                    group,
                    Some(target),
                    clock() + chrono::Duration::seconds(1)
                ));
                assert!(!store.discretionary_allowed(
                    group,
                    Some(target),
                    deadline - chrono::Duration::seconds(1)
                ));
                assert!(
                    store
                        .dispatch(Some(group), target, Some("cx-ring-32"), "overload", clock())
                        .is_err()
                );
                store
                    .dispatch(
                        Some(key),
                        "cx-forced-after-copy",
                        Some("cx-ring-32"),
                        "forced",
                        clock(),
                    )
                    .unwrap();
                assert!(store.permits_dispatch(Some(key), "cx-forced-after-copy"));
                assert!(store.discretionary_allowed(group, Some(target), deadline));
            }
        }
    }
}

#[cfg(test)]
mod issue_operation_writer_tests {
    use super::*;
    use crate::issue_operation::IssueOperation;
    use std::sync::{Barrier, mpsc};

    #[test]
    fn cancellation_behind_actual_policy_writer_prevents_cas() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r11-operation-tests");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let store = Arc::new(PolicyStore::new(dir.path(), Settings {
            mode: Mode::Strict, reset_wait_secs: 7,
        }).unwrap());
        let key = "codex:operation-cancel";
        store.capture_strict_anchors([(key, "cx-a")]).unwrap();
        let plan = store.plan(Some(key));
        let consent = crate::bindings::LaunchPolicyMove {
            key: key.into(), previous: "cx-a".into(), account: "cx-b".into(),
            expected_revision: plan.revision, at: chrono::Utc::now(),
        };
        let before = std::fs::read(&store.path).unwrap();
        let operation = IssueOperation::new(tokio::time::Instant::now()
            + std::time::Duration::from_secs(3600));
        let caller = operation.caller_guard();
        let ready = Arc::new(Barrier::new(2));
        let held = store.writer.lock().unwrap();
        let (send, recv) = mpsc::channel();
        let task = {
            let store = store.clone();
            let operation = operation.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                ready.wait();
                send.send(store.authorize_model_move_for_issue(&consent, Some(&operation)).is_err())
                    .unwrap();
            })
        };
        ready.wait();
        drop(caller);
        drop(held);
        assert!(recv.recv().unwrap());
        task.join().unwrap();
        assert_eq!(std::fs::read(&store.path).unwrap(), before);
        assert_eq!(store.plan(Some(key)).anchor.as_deref(), Some("cx-a"));
        assert_eq!(store.plan(Some(key)).revision, plan.revision);
    }

    #[test]
    fn admitted_operation_still_obeys_late_dispatch_recheck() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r11-operation-tests");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let store = PolicyStore::new(dir.path(), Settings {
            mode: Mode::Strict, reset_wait_secs: 7,
        }).unwrap();
        let key = "codex:operation-recheck";
        store.capture_strict_anchors([(key, "cx-a")]).unwrap();
        let operation = IssueOperation::new(tokio::time::Instant::now()
            + std::time::Duration::from_secs(3600));
        let caller = operation.caller_guard();
        operation.admit().unwrap();
        drop(caller);
        assert!(store.dispatch_for_issue(Some(key), "cx-b", Some("cx-a"),
            "model_choice", chrono::Utc::now(), Some(&operation)).is_err());
        assert_eq!(store.plan(Some(key)).anchor.as_deref(), Some("cx-a"));
    }
}
