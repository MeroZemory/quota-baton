//! Durable local routing intent. Display labels and upstream payloads are never selectors.
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
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
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Movement {
    from: String,
    to: String,
    at: DateTime<Utc>,
    discretionary: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct SessionPolicy {
    project: Option<String>,
    baseline: Settings,
    explicit: Option<Settings>,
    anchor: Option<String>,
    #[serde(default)]
    movements: Vec<Movement>,
}
impl SessionPolicy {
    fn settings(&self) -> &Settings {
        self.explicit.as_ref().unwrap_or(&self.baseline)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct State {
    schema: u32,
    revision: u64,
    projects: BTreeMap<String, Project>,
    sessions: BTreeMap<String, SessionPolicy>,
    #[serde(skip)]
    frozen: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            schema: 1,
            revision: 0,
            projects: BTreeMap::new(),
            sessions: BTreeMap::new(),
            frozen: false,
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
    state: Mutex<State>,
}

pub fn root(key: &str) -> &str {
    key.split(":agent:").next().unwrap_or(key)
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
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_FILE => bail!("routing policy file exceeds cap"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(e) => return Err(e.into()),
        _ => {}
    }
    // A corrupt intent file must stop startup, never silently remove strict pins.
    let state: State = serde_json::from_slice(&fs::read(path)?)?;
    if state.schema != 1
        || state.projects.len() > MAX_PROJECTS
        || state.sessions.len() > MAX_SESSIONS
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
        session.baseline.validate()?;
        if let Some(settings) = &session.explicit {
            settings.validate()?;
        }
    }
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
            state: Mutex::new(state),
        })
    }
    fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.frozen {
            bail!("routing policy writer retired");
        }
        let mut next = state.clone();
        let result = f(&mut next)?;
        if next.sessions.len() > MAX_SESSIONS || next.projects.len() > MAX_PROJECTS {
            bail!("routing policy capacity reached");
        }
        next.revision = next.revision.wrapping_add(1);
        let bytes = serde_json::to_vec(&next)?;
        if bytes.len() as u64 > MAX_FILE {
            bail!("routing policy file exceeds cap");
        }
        crate::bindings::atomic_write(&self.path, &bytes)?;
        *state = next;
        Ok(result)
    }
    pub fn freeze(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).frozen = true;
    }
    pub fn reload(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.frozen {
            bail!("routing policy writer retired");
        }
        *state = load(&self.path)?;
        Ok(())
    }
    fn resolve(&self, state: &State, id: &str) -> Plan {
        let session = state.sessions.get(id);
        Plan {
            settings: session.map_or_else(|| self.defaults.clone(), |s| s.settings().clone()),
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
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.resolve(&state, &key.map(session_id).unwrap_or_default())
    }
    pub fn list(&self) -> serde_json::Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        if !valid_id(id, 64) {
            bail!("invalid routing session id");
        }
        if let Some(settings) = &explicit {
            settings.validate()?;
        }
        self.update(|state| {
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
            entry.explicit = explicit;
            // Editing policy never silently chooses another account.
            if entry.anchor.is_none() {
                entry.anchor = anchor.map(str::to_owned);
            }
            Ok(())
        })
    }
    /// Ingress association is explicit, stable, and cannot reassign an existing root.
    pub fn associate(&self, key: &str, project: &str) -> Result<()> {
        let id = session_id(key);
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(session) = state.sessions.get(&id) {
                if session.project.as_deref() == Some(project) {
                    return Ok(());
                }
                bail!("routing project conflicts with existing session");
            }
        }
        self.update(|state| {
            if let Some(session) = state.sessions.get(&id) {
                if session.project.as_deref() == Some(project) {
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
                    anchor: None,
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
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(session) = state.sessions.get(&session_id(key)) else {
            return self.defaults.mode == Mode::Rebalance;
        };
        session.settings().mode == Mode::Rebalance
            && !session.movements.iter().any(|m| {
                (m.discretionary && (now - m.at).num_seconds() < 1800)
                    || target
                        .is_some_and(|target| target == m.from && (now - m.at).num_seconds() < 3600)
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
        let id = session_id(key);
        let discretionary = matches!(reason, "overload" | "cold_rebalance");
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            let entry = state.sessions.entry(id).or_insert_with(|| SessionPolicy {
                project: None,
                baseline: self.defaults.clone(),
                explicit: None,
                anchor: previous.map(str::to_owned),
                movements: Vec::new(),
            });
            let from = entry.anchor.as_deref().or(previous).map(str::to_owned);
            if entry.settings().mode == Mode::Strict
                && from.as_deref().is_some_and(|a| a != account)
            {
                bail!("routing session is locked");
            }
            if let Some(from) = from.filter(|from| from != account) {
                if discretionary && entry.settings().mode != Mode::Rebalance {
                    bail!("routing discretionary move forbidden");
                }
                entry
                    .movements
                    .retain(|m| (now - m.at).num_seconds() < 24 * 3600);
                if discretionary
                    && entry.movements.iter().any(|m| {
                        (m.discretionary && (now - m.at).num_seconds() < 1800)
                            || (m.from == account && (now - m.at).num_seconds() < 3600)
                    })
                {
                    bail!("routing dwell or revisit budget");
                }
                let recent: Vec<_> = entry
                    .movements
                    .iter()
                    .filter(|m| (now - m.at).num_seconds() < 600)
                    .collect();
                if recent.len() >= 3 {
                    bail!("routing movement budget exhausted");
                }
                entry.movements.push(Movement {
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
    fn duplicate_and_invalid_project_headers_are_removed_on_error() {
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
