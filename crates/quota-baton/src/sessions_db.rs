//! Selected Codex metadata only; never opens rollouts or writes session_index.
//!
//! Source citations (repository .sandbox/src/codex/codex-rs/state/):
//! - src/sqlite.rs:35-38: goals_1.sqlite and state_5.sqlite.
//! - migrations/0001_threads.sql: thread identity, rollout_path and archived fields.
//! - src/audit.rs:28-41: read-only id/path/provider discovery.
//! - goals_migrations/0001_thread_goals.sql and src/runtime/goals.rs:44-61:
//!   independent goal snapshots, status and updated_at_ms.
//!
//! D21 / R9 concurrency addendum explicitly excludes thread_history_1.
//!
//! The coordinator must hold its import lock from planning through journal save
//! and apply. Persist the *original* plan before apply; do not re-plan a retry.
//! Ownership means absent at planning time under that lock, not merely equal.
//! Each DB commits atomically; a failure between DB commits is recovered by
//! replaying or removing the saved plan. Empty created DBs/schemas are retained.
//! Fresh schemas deliberately contain no migrations ledger or global tables.

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

const STATE: &str = "state_5.sqlite";
const GOALS: &str = "goals_1.sqlite";

// Codex state/migrations/0001_threads.sql. Used only for explicit synthesis in
// rollout-only homes; callers must supply the required verified metadata.
const MINIMUM_THREADS_DDL: &str = "CREATE TABLE threads (
    id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    source TEXT NOT NULL, model_provider TEXT NOT NULL, cwd TEXT NOT NULL,
    title TEXT NOT NULL, sandbox_policy TEXT NOT NULL, approval_mode TEXT NOT NULL,
    tokens_used INTEGER NOT NULL DEFAULT 0, has_user_event INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0, archived_at INTEGER,
    git_sha TEXT, git_branch TEXT, git_origin_url TEXT
)";

/// Bytes preserve even non-UTF8 SQLite TEXT; REAL uses bits for lossless JSON.
/// Do not log these values: metadata can include titles and goal objectives.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum DbValue {
    Null,
    Integer(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl DbValue {
    fn text(value: &str) -> Self {
        Self::Text(value.as_bytes().to_vec())
    }

    fn as_text(&self) -> Result<&str> {
        match self {
            Self::Text(value) => Ok(std::str::from_utf8(value)?),
            _ => bail!("expected SQLite TEXT metadata"),
        }
    }

    fn from_ref(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(value) => Self::Integer(value),
            ValueRef::Real(value) => Self::Real(value.to_bits()),
            ValueRef::Text(value) => Self::Text(value.to_vec()),
            ValueRef::Blob(value) => Self::Blob(value.to_vec()),
        }
    }
}

impl ToSql for DbValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Self::Integer(value) => ToSqlOutput::Borrowed(ValueRef::Integer(*value)),
            Self::Real(bits) => ToSqlOutput::Borrowed(ValueRef::Real(f64::from_bits(*bits))),
            Self::Text(value) => ToSqlOutput::Borrowed(ValueRef::Text(value)),
            Self::Blob(value) => ToSqlOutput::Borrowed(ValueRef::Blob(value)),
        })
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RowSnapshot {
    pub(crate) values: BTreeMap<String, DbValue>,
    /// False for an identical row already present when planning.
    pub(crate) owned: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TableSchema {
    pub(crate) ddl: String,
    pub(crate) indexes: Vec<String>,
    /// All stored/generated columns, with generated columns marked nonwritable.
    pub(crate) columns: BTreeMap<String, bool>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ThreadInfo {
    pub(crate) id: String,
    pub(crate) rollout_path: PathBuf,
    pub(crate) model_provider: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ThreadPlan {
    pub(crate) id: String,
    pub(crate) source: Option<ThreadInfo>,
    /// None if the source DB/row is absent: caller's rollout discovery can proceed.
    pub(crate) thread: Option<RowSnapshot>,
    pub(crate) goal: Option<RowSnapshot>,
    pub(crate) source_goal_status: Option<String>,
    /// Last matching JSONL record in file order, including unknown fields.
    pub(crate) latest_index_entry: Option<serde_json::Value>,
}

/// Status-only output; never format the goal row or its objective.
pub(crate) fn summaries(plan: &DbPlan) -> Vec<(String, String, String)> {
    plan.threads
        .iter()
        .filter_map(|thread| {
            let from = thread.source_goal_status.as_ref()?;
            let to = field(thread.goal.as_ref()?, "status").ok()?;
            Some((thread.id.clone(), from.clone(), to.to_owned()))
        })
        .collect()
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DbPlan {
    pub(crate) source_home: PathBuf,
    pub(crate) target_home: PathBuf,
    pub(crate) state_path: PathBuf,
    pub(crate) goals_path: PathBuf,
    pub(crate) threads_schema: Option<TableSchema>,
    pub(crate) goals_schema: Option<TableSchema>,
    pub(crate) threads: Vec<ThreadPlan>,
}

fn readonly(path: &Path) -> Result<Option<Connection>> {
    let path = local_path(path);
    crate::sessions_guard::safe_path(path.parent().context("metadata path lacks home")?, &path)?;
    match path.try_exists()? {
        false => Ok(None),
        true => {
            let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED")?;
            Ok(Some(db))
        }
    }
}

/// Convert only canonical Windows verbatim-disk prefixes for the local guard.
/// Leave other device/UNC namespaces intact so the guard rejects them.
fn local_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next()
            && let Prefix::VerbatimDisk(drive) = prefix.kind()
        {
            let mut local = PathBuf::from(format!("{}:\\", char::from(drive)));
            for component in components {
                if !matches!(component, Component::RootDir) {
                    local.push(component.as_os_str());
                }
            }
            return local;
        }
    }
    path.to_owned()
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn columns(db: &Connection, table: &str) -> Result<BTreeMap<String, bool>> {
    let mut statement = db.prepare(&format!("PRAGMA table_xinfo({})", quoted(table)))?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, i64>(6)? == 0))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn schema(db: &Connection, table: &str) -> Result<Option<TableSchema>> {
    let ddl = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(ddl) = ddl else { return Ok(None) };
    let mut statement = db.prepare(
        "SELECT sql FROM sqlite_schema WHERE type='index' AND tbl_name=?1 AND sql IS NOT NULL ORDER BY name",
    )?;
    let indexes = statement
        .query_map([table], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(Some(TableSchema {
        ddl,
        indexes,
        columns: columns(db, table)?,
    }))
}

fn selected_row(db: &Connection, table: &str, key: &str, id: &str) -> Result<Option<RowSnapshot>> {
    if schema(db, table)?.is_none() {
        return Ok(None);
    }
    let mut statement = db.prepare(&format!(
        "SELECT * FROM {} WHERE {}=?1",
        quoted(table),
        quoted(key)
    ))?;
    let names = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut rows = statement.query([id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut values = BTreeMap::new();
    for (index, name) in names.into_iter().enumerate() {
        values.insert(name, DbValue::from_ref(row.get_ref(index)?));
    }
    ensure!(rows.next()?.is_none(), "metadata identity is not unique");
    Ok(Some(RowSnapshot {
        values,
        owned: false,
    }))
}

fn field<'a>(row: &'a RowSnapshot, name: &str) -> Result<&'a str> {
    row.values
        .get(name)
        .context("required metadata column absent")?
        .as_text()
}

fn thread_info(row: &RowSnapshot) -> Result<ThreadInfo> {
    Ok(ThreadInfo {
        id: field(row, "id")?.to_owned(),
        rollout_path: PathBuf::from(field(row, "rollout_path")?),
        model_provider: field(row, "model_provider")?.to_owned(),
    })
}

/// Discovery reads only id, rollout_path and model_provider, never titles/goals.
pub(crate) fn threads(home: &Path) -> Result<Vec<ThreadInfo>> {
    let Some(db) = readonly(&home.join(STATE))? else {
        return Ok(Vec::new());
    };
    if schema(&db, "threads")?.is_none() {
        return Ok(Vec::new());
    }
    let mut statement =
        db.prepare("SELECT id, rollout_path, model_provider FROM threads ORDER BY id")?;
    let rows = statement.query_map([], |row| {
        Ok(ThreadInfo {
            id: row.get(0)?,
            rollout_path: PathBuf::from(row.get::<_, String>(1)?),
            model_provider: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn thread_path(home: &Path, id: &str) -> Result<Option<PathBuf>> {
    let Some(db) = readonly(&home.join(STATE))? else {
        return Ok(None);
    };
    if schema(&db, "threads")?.is_none() {
        return Ok(None);
    }
    Ok(db
        .query_row(
            "SELECT rollout_path FROM threads WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0).map(PathBuf::from),
        )
        .optional()?)
}

pub(crate) fn thread_provider(home: &Path, id: &str) -> Result<Option<String>> {
    let Some(db) = readonly(&home.join(STATE))? else {
        return Ok(None);
    };
    if schema(&db, "threads")?.is_none() {
        return Ok(None);
    }
    Ok(db
        .query_row(
            "SELECT model_provider FROM threads WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?)
}

fn mapped_path(source_home: &Path, target_home: &Path, source: &Path) -> Result<PathBuf> {
    let home = local_path(source_home);
    crate::sessions_guard::safe_path(&home, &home)?;
    let candidate = if source.is_absolute() {
        local_path(source)
    } else {
        home.join(source)
    };
    // Check before canonicalization can hide reparse points or parent traversal.
    crate::sessions_guard::safe_path(&candidate, &candidate)?;
    let relative = if candidate.try_exists()? {
        let home = home.canonicalize()?;
        candidate.canonicalize()?.strip_prefix(home)?.to_owned()
    } else {
        candidate.strip_prefix(&home)?.to_owned()
    };
    let mut parts = relative.components();
    ensure!(
        matches!(parts.next(), Some(Component::Normal(name)) if name == "sessions" || name == "archived_sessions"),
        "rollout_path is outside source session directories"
    );
    let suffix = parts.collect::<PathBuf>();
    ensure!(
        !suffix.as_os_str().is_empty()
            && suffix
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "unsafe rollout_path suffix"
    );
    Ok(target_home.join("sessions").join(suffix))
}

fn latest_index(
    home: &Path,
    ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, serde_json::Value>> {
    let home = local_path(home);
    let path = home.join("session_index.jsonl");
    crate::sessions_guard::safe_path(&home, &path)?;
    if !path.try_exists()? {
        return Ok(BTreeMap::new());
    }
    let mut selected = BTreeMap::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Avoid including any line payload in parse errors.
        let value: serde_json::Value =
            serde_json::from_str(&line).context("invalid session_index JSON record")?;
        if let Some(id) = value.get("id").and_then(serde_json::Value::as_str)
            && ids.contains(id)
        {
            selected.insert(id.to_owned(), value);
        }
    }
    Ok(selected)
}

fn set_ownership(
    target: Option<&Connection>,
    table: &str,
    key: &str,
    id: &str,
    row: &mut RowSnapshot,
) -> Result<()> {
    let existing = target
        .map(|db| selected_row(db, table, key, id))
        .transpose()?
        .flatten();
    match existing {
        Some(existing) => ensure!(
            existing.values == row.values,
            "destination {table} row differs for {id}"
        ),
        None => row.owned = true,
    }
    Ok(())
}

fn destination_file(path: &Path) -> Result<()> {
    let path = local_path(path);
    crate::sessions_guard::safe_path(path.parent().context("metadata path lacks home")?, &path)?;
    if !path.try_exists()? {
        return Ok(());
    }
    ensure!(
        !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
        "destination SQLite file is a symlink"
    );
    let file = File::open(&path)?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: file owns a live handle; info is a valid output buffer.
        unsafe {
            GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)?;
        }
        ensure!(
            info.nNumberOfLinks == 1,
            "destination SQLite file is hardlinked"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            file.metadata()?.nlink() == 1,
            "destination SQLite file is hardlinked"
        );
    }
    Ok(())
}

pub(crate) fn plan(source_home: &Path, target_home: &Path, ids: &[String]) -> Result<DbPlan> {
    ensure!(
        source_home.is_absolute() && target_home.is_absolute(),
        "metadata homes must be absolute"
    );
    // Canonical ancestors also reject same-home aliases when the target exists.
    if target_home.try_exists()? {
        ensure!(
            source_home.canonicalize()? != target_home.canonicalize()?,
            "source and target homes coincide"
        );
    }
    destination_file(&target_home.join(STATE))?;
    destination_file(&target_home.join(GOALS))?;
    let state = readonly(&source_home.join(STATE))?;
    let goals = readonly(&source_home.join(GOALS))?;
    let target_state = readonly(&target_home.join(STATE))?;
    let target_goals = readonly(&target_home.join(GOALS))?;
    let threads_schema = state
        .as_ref()
        .map(|db| schema(db, "threads"))
        .transpose()?
        .flatten();
    let goals_schema = goals
        .as_ref()
        .map(|db| schema(db, "thread_goals"))
        .transpose()?
        .flatten();
    let ids = ids.iter().cloned().collect::<BTreeSet<_>>();
    let mut index = latest_index(source_home, &ids)?;
    let mut selected = Vec::new();
    for id in ids {
        let mut thread = state
            .as_ref()
            .map(|db| selected_row(db, "threads", "id", &id))
            .transpose()?
            .flatten();
        let source = thread.as_ref().map(thread_info).transpose()?;
        if let Some(row) = &mut thread {
            ensure!(
                field(row, "model_provider")? == "openai",
                "unsupported model provider for {id}"
            );
            let path = mapped_path(
                source_home,
                target_home,
                &source
                    .as_ref()
                    .context("missing thread metadata")?
                    .rollout_path,
            )?;
            row.values.insert(
                "rollout_path".into(),
                DbValue::text(path.to_str().context("non-UTF8 rollout path")?),
            );
            ensure!(
                row.values.contains_key("archived"),
                "missing archived column"
            );
            row.values.insert("archived".into(), DbValue::Integer(0));
            recompute_generated(
                threads_schema.as_ref().context("missing threads schema")?,
                "threads",
                "id",
                &id,
                row,
            )?;
            set_ownership(target_state.as_ref(), "threads", "id", &id, row)?;
        }
        let mut goal = goals
            .as_ref()
            .map(|db| selected_row(db, "thread_goals", "thread_id", &id))
            .transpose()?
            .flatten();
        let source_goal_status = goal
            .as_ref()
            .map(|row| field(row, "status").map(str::to_owned))
            .transpose()?;
        if let Some(row) = &mut goal {
            if matches!(field(row, "status")?, "active" | "usage_limited") {
                row.values.insert("status".into(), DbValue::text("paused"));
            }
            ensure!(
                row.values.contains_key("updated_at_ms"),
                "missing goal update timestamp"
            );
            row.values.insert(
                "updated_at_ms".into(),
                DbValue::Integer(chrono::Utc::now().timestamp_millis()),
            );
            recompute_generated(
                goals_schema.as_ref().context("missing goals schema")?,
                "thread_goals",
                "thread_id",
                &id,
                row,
            )?;
            // A second fork can share an already imported ancestor. A timestamp
            // refresh alone must not turn that identical logical goal into a
            // collision or claim ownership of the first import's row.
            if let Some(target) = &target_goals
                && let Some(existing) = selected_row(target, "thread_goals", "thread_id", &id)?
            {
                let expected = goals_schema.as_ref().context("missing goals schema")?;
                if expected
                    .columns
                    .iter()
                    .filter(|(name, writable)| **writable && name.as_str() != "updated_at_ms")
                    .all(|(name, _)| existing.values.get(name) == row.values.get(name))
                {
                    row.values = existing.values;
                }
            }
            set_ownership(target_goals.as_ref(), "thread_goals", "thread_id", &id, row)?;
        }
        selected.push(ThreadPlan {
            id: id.clone(),
            source,
            thread,
            goal,
            source_goal_status,
            latest_index_entry: index.remove(&id),
        });
    }
    if selected.iter().any(|t| t.thread.is_some())
        && let (Some(db), Some(expected)) = (target_state.as_ref(), threads_schema.as_ref())
        && schema(db, "threads")?.is_some()
    {
        validate_schema(db, "threads", expected)?;
    }
    if selected.iter().any(|t| t.goal.is_some())
        && let (Some(db), Some(expected)) = (target_goals.as_ref(), goals_schema.as_ref())
        && schema(db, "thread_goals")?.is_some()
    {
        validate_schema(db, "thread_goals", expected)?;
    }
    Ok(DbPlan {
        source_home: source_home.to_owned(),
        target_home: target_home.to_owned(),
        state_path: target_home.join(STATE),
        goals_path: target_home.join(GOALS),
        threads_schema,
        goals_schema,
        threads: selected,
    })
}

/// For a rollout-only home, the filesystem planner can supply metadata it has
/// already verified. No guessed provider, timestamps, cwd or sandbox settings.
/// Required fields lacking defaults must be supplied in `fields`; optional and
/// future columns receive their schema defaults and are captured in the plan.
/// This modifies only the in-memory plan; save the completed plan before apply.
pub(crate) fn add_synthetic_thread(
    plan: &mut DbPlan,
    source: &ThreadInfo,
    mut fields: BTreeMap<String, DbValue>,
) -> Result<()> {
    ensure!(
        source.model_provider == "openai",
        "unsupported model provider for {}",
        source.id
    );
    let position = plan
        .threads
        .iter()
        .position(|thread| thread.id == source.id)
        .context("synthetic thread is not selected")?;
    ensure!(
        plan.threads[position].thread.is_none(),
        "refusing to replace existing thread metadata"
    );
    destination_file(&plan.state_path)?;
    let target = readonly(&plan.state_path)?;
    let expected = match &plan.threads_schema {
        Some(schema) => schema.clone(),
        None => match target
            .as_ref()
            .map(|db| schema(db, "threads"))
            .transpose()?
            .flatten()
        {
            Some(schema) => schema,
            None => {
                let db = Connection::open_in_memory()?;
                db.execute(MINIMUM_THREADS_DDL, [])?;
                schema(&db, "threads")?.context("missing synthetic schema")?
            }
        },
    };
    let destination = mapped_path(&plan.source_home, &plan.target_home, &source.rollout_path)?;
    fields.insert("id".into(), DbValue::text(&source.id));
    fields.insert(
        "rollout_path".into(),
        DbValue::text(destination.to_str().context("non-UTF8 rollout path")?),
    );
    fields.insert(
        "model_provider".into(),
        DbValue::text(&source.model_provider),
    );
    fields.insert("archived".into(), DbValue::Integer(0));
    ensure!(
        fields
            .keys()
            .all(|name| expected.columns.get(name) == Some(&true)),
        "unknown or generated synthetic field"
    );
    let db = Connection::open_in_memory()?;
    db.execute_batch("PRAGMA foreign_keys=OFF")?;
    db.execute(&expected.ddl, [])?;
    let names = fields
        .keys()
        .map(|name| quoted(name))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "INSERT INTO threads ({names}) VALUES ({})",
        vec!["?"; fields.len()].join(",")
    );
    db.execute(&sql, params_from_iter(fields.values()))
        .context("synthetic metadata lacks required fields or violates schema")?;
    let mut row =
        selected_row(&db, "threads", "id", &source.id)?.context("missing synthesized row")?;
    if let Some(target) = &target
        && schema(target, "threads")?.is_some()
    {
        validate_schema(target, "threads", &expected)?;
    }
    set_ownership(target.as_ref(), "threads", "id", &source.id, &mut row)?;
    plan.threads_schema = Some(expected);
    plan.threads[position].source = Some(source.clone());
    plan.threads[position].thread = Some(row);
    Ok(())
}

fn validate_schema(db: &Connection, table: &str, expected: &TableSchema) -> Result<()> {
    ensure!(
        columns(db, table)? == expected.columns,
        "incompatible destination {table} columns"
    );
    let triggers: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND tbl_name=?1",
        [table],
        |row| row.get(0),
    )?;
    ensure!(
        triggers == 0,
        "destination {table} has triggers; refusing indirect writes"
    );
    Ok(())
}

struct Pending<'a> {
    db: Connection,
    table: &'static str,
    key: &'static str,
    schema: &'a TableSchema,
    rows: Vec<(&'a str, &'a RowSnapshot)>,
}

fn prepare<'a>(plan: &'a DbPlan, remove: bool) -> Result<Vec<Pending<'a>>> {
    ensure!(
        plan.state_path == plan.target_home.join(STATE)
            && plan.goals_path == plan.target_home.join(GOALS),
        "invalid metadata plan paths"
    );
    if plan.target_home.try_exists()? && plan.source_home.try_exists()? {
        ensure!(
            plan.source_home.canonicalize()? != plan.target_home.canonicalize()?,
            "source and target homes coincide"
        );
    }
    let mut pending = Vec::new();
    for (path, table, key, expected, is_goal) in [
        (
            &plan.state_path,
            "threads",
            "id",
            plan.threads_schema.as_ref(),
            false,
        ),
        (
            &plan.goals_path,
            "thread_goals",
            "thread_id",
            plan.goals_schema.as_ref(),
            true,
        ),
    ] {
        let rows = plan
            .threads
            .iter()
            .filter_map(|thread| {
                let row = if is_goal {
                    thread.goal.as_ref()
                } else {
                    thread.thread.as_ref()
                };
                row.map(|row| (thread.id.as_str(), row))
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            continue;
        }
        let expected = expected.context("row plan lacks source schema")?;
        if remove && !path.try_exists()? {
            continue;
        }
        destination_file(path)?;
        if !remove {
            std::fs::create_dir_all(&plan.target_home)?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | if remove {
                OpenFlags::empty()
            } else {
                OpenFlags::SQLITE_OPEN_CREATE
            };
        let db = Connection::open_with_flags(local_path(path), flags)?;
        // Preserve foreign-key columns without cloning referenced global tables;
        // disable cascades so exact-row removal cannot delete unrelated metadata.
        db.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE")?;
        if schema(&db, table)?.is_none() {
            if remove {
                continue;
            }
            // execute (not execute_batch) rejects multiple SQL statements.
            db.execute(&expected.ddl, [])?;
            for index in &expected.indexes {
                db.execute(index, [])?;
            }
        }
        validate_schema(&db, table, expected)?;
        for (id, row) in &rows {
            ensure!(field(row, key)? == *id, "row plan identity mismatch");
            let current = selected_row(&db, table, key, id)?;
            match current {
                Some(current) => ensure!(
                    current.values == row.values,
                    "destination {table} row changed for {id}"
                ),
                None => ensure!(
                    row.owned,
                    "unowned destination {table} row disappeared for {id}"
                ),
            }
        }
        pending.push(Pending {
            db,
            table,
            key,
            schema: expected,
            rows,
        });
    }
    Ok(pending)
}

pub(crate) fn apply(plan: &DbPlan) -> Result<()> {
    let pending = prepare(plan, false)?;
    for item in &pending {
        for (id, row) in &item.rows {
            if selected_row(&item.db, item.table, item.key, id)?.is_some() {
                continue;
            }
            ensure!(row.owned, "refusing to insert unowned metadata");
            insert_row(&item.db, item.table, item.schema, row)?;
            let inserted = selected_row(&item.db, item.table, item.key, id)?
                .context("inserted metadata disappeared")?;
            ensure!(
                inserted.values == row.values,
                "destination coerced imported metadata for {id}"
            );
        }
    }
    for item in pending {
        item.db.execute_batch("COMMIT")?;
    }
    Ok(())
}

fn insert_row(db: &Connection, table: &str, schema: &TableSchema, row: &RowSnapshot) -> Result<()> {
    let names = schema
        .columns
        .iter()
        .filter(|(_, writable)| **writable)
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    let values = names
        .iter()
        .map(|name| row.values.get(*name).context("planned column absent"))
        .collect::<Result<Vec<_>>>()?;
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quoted(table),
        names
            .iter()
            .map(|name| quoted(name))
            .collect::<Vec<_>>()
            .join(","),
        vec!["?"; names.len()].join(",")
    );
    db.execute(&sql, params_from_iter(values))?;
    Ok(())
}

fn recompute_generated(
    schema: &TableSchema,
    table: &str,
    key: &str,
    id: &str,
    row: &mut RowSnapshot,
) -> Result<()> {
    if schema.columns.values().all(|writable| *writable) {
        return Ok(());
    }
    let db = Connection::open_in_memory()?;
    db.execute_batch("PRAGMA foreign_keys=OFF")?;
    db.execute(&schema.ddl, [])?;
    insert_row(&db, table, schema, row)?;
    let computed = selected_row(&db, table, key, id)?.context("generated row missing")?;
    row.values = computed.values;
    Ok(())
}

/// Validate *all* selected rows before removing any. Missing rows are harmless
/// for crash recovery; matching pre-existing (unowned) rows are never deleted.
pub(crate) fn remove_exact(plan: &DbPlan) -> Result<()> {
    let pending = prepare(plan, true)?;
    for item in &pending {
        for (id, row) in &item.rows {
            if row.owned {
                item.db.execute(
                    &format!(
                        "DELETE FROM {} WHERE {}=?1",
                        quoted(item.table),
                        quoted(item.key)
                    ),
                    [id],
                )?;
            }
        }
    }
    for item in pending {
        item.db.execute_batch("COMMIT")?;
    }
    Ok(())
}

pub(crate) fn rollback(plan: &DbPlan) -> Result<()> {
    remove_exact(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn homes() -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let sandbox = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox");
        std::fs::create_dir_all(&sandbox)?;
        let sandbox = sandbox.canonicalize()?;
        let temp = tempfile::Builder::new()
            .prefix("sessions-db-")
            .tempdir_in(sandbox)?;
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        std::fs::create_dir(&source)?;
        Ok((temp, source, target))
    }

    fn fixture(source: &Path, statuses: &[&str]) -> Result<Vec<String>> {
        let state = Connection::open(source.join(STATE))?;
        state.execute_batch("PRAGMA foreign_keys=OFF")?;
        state.execute(MINIMUM_THREADS_DDL, [])?;
        state.execute_batch(
            "ALTER TABLE threads ADD COLUMN future_blob BLOB;
             ALTER TABLE threads ADD COLUMN future_real REAL;
             ALTER TABLE threads ADD COLUMN future_text TEXT;
             ALTER TABLE threads ADD COLUMN future_null TEXT;
             ALTER TABLE threads ADD COLUMN future_derived TEXT GENERATED ALWAYS AS (rollout_path) VIRTUAL;
             ALTER TABLE threads ADD COLUMN project_id TEXT REFERENCES projects(id);
             CREATE INDEX future_index ON threads(future_text) WHERE archived=0;
             CREATE TABLE thread_history(thread_id TEXT, content TEXT);
             CREATE TABLE audit(id TEXT);
             CREATE TRIGGER source_only AFTER INSERT ON threads BEGIN INSERT INTO audit VALUES (NEW.id); END;"
        )?;
        let goals = Connection::open(source.join(GOALS))?;
        goals.execute_batch(
            "CREATE TABLE thread_goals (
                 thread_id TEXT PRIMARY KEY NOT NULL, goal_id TEXT NOT NULL,
                 objective TEXT NOT NULL, status TEXT NOT NULL,
                 token_budget INTEGER, tokens_used INTEGER NOT NULL DEFAULT 0,
                 time_used_seconds INTEGER NOT NULL DEFAULT 0,
                 created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,
                 future_goal BLOB
             );
             CREATE INDEX goal_status ON thread_goals(status);
             CREATE TABLE unrelated(value TEXT);",
        )?;
        let ids = statuses
            .iter()
            .enumerate()
            .map(|(i, _)| format!("thread-{i}"))
            .collect::<Vec<_>>();
        for (i, (id, status)) in ids.iter().zip(statuses).enumerate() {
            let directory = if i == 0 {
                "archived_sessions"
            } else {
                "sessions"
            };
            let path = source
                .join(directory)
                .join("2026/10/21")
                .join(format!("rollout-{id}.jsonl"));
            state.execute(
                "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider,
                 cwd, title, sandbox_policy, approval_mode, archived, archived_at, future_blob,
                 future_real, future_text, project_id)
                 VALUES (?1, ?2, 1, 2, 'cli', 'openai', 'synthetic', 'private title',
                 'read-only', 'on-request', 1, 5, X'00FF', 1.25, CAST(X'80FF' AS TEXT), 'uncopied-project')",
                [id.as_str(), path.to_str().context("fixture path")?],
            )?;
            goals.execute(
                "INSERT INTO thread_goals (thread_id,goal_id,objective,status,created_at_ms,updated_at_ms,future_goal)
                 VALUES (?1,?2,'private objective',?3,1,2,X'0102')",
                [id.as_str(), format!("goal-{i}").as_str(), *status],
            )?;
        }
        std::fs::write(
            source.join("session_index.jsonl"),
            "{\"id\":\"thread-0\",\"thread_name\":\"old\"}\n{\"id\":\"other\",\"extra\":true}\n{\"id\":\"thread-0\",\"thread_name\":\"new\",\"unknown\":17}\n",
        )?;
        Ok(ids)
    }

    #[test]
    fn selected_fidelity_mapping_serialization_and_idempotence() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["active", "usage_limited", "complete"])?;
        let source_bytes = std::fs::read(source.join(STATE))?;
        let source_goals = std::fs::read(source.join(GOALS))?;
        let source_index = std::fs::read(source.join("session_index.jsonl"))?;
        let planned = plan(&source, &target, &ids)?;
        ensure!(!target.exists(), "planning created destination");
        let saved = serde_json::to_vec(&planned)?;
        let planned: DbPlan = serde_json::from_slice(&saved)?;
        let statuses = summaries(&planned);
        assert_eq!(
            statuses[0],
            (ids[0].clone(), "active".into(), "paused".into())
        );
        assert_eq!(statuses[1].2, "paused");
        assert_eq!(statuses[2].2, "complete");
        assert_eq!(
            planned.threads[0]
                .latest_index_entry
                .as_ref()
                .context("index")?["unknown"],
            17
        );
        apply(&planned)?;
        apply(&planned)?;
        assert_eq!(threads(&target)?.len(), 3);
        assert_eq!(
            thread_provider(&target, &ids[0])?.as_deref(),
            Some("openai")
        );
        let path = target.join("sessions/2026/10/21/rollout-thread-0.jsonl");
        assert_eq!(thread_path(&target, &ids[0])?, Some(path.clone()));
        let db = readonly(&target.join(STATE))?.context("target")?;
        let row = selected_row(&db, "threads", "id", &ids[0])?.context("row")?;
        assert!(
            row.values
                == planned.threads[0]
                    .thread
                    .as_ref()
                    .context("plan row")?
                    .values
        );
        assert!(row.values["future_blob"] == DbValue::Blob(vec![0, 255]));
        assert!(row.values["future_text"] == DbValue::Text(vec![128, 255]));
        assert!(row.values["future_real"] == DbValue::Real(1.25_f64.to_bits()));
        assert!(row.values["future_null"] == DbValue::Null);
        assert!(row.values["archived"] == DbValue::Integer(0));
        assert_eq!(
            field(&row, "future_derived")?,
            path.to_str().context("path")?
        );
        let tables: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(tables, 1);
        let triggers: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='trigger'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(triggers, 0);
        let indexes: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='future_index'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(indexes, 1);
        drop(db);
        ensure!(
            !target.join("session_index.jsonl").exists(),
            "adapter wrote index"
        );
        ensure!(
            !target.join("thread_history_1.sqlite").exists(),
            "adapter wrote history"
        );
        assert!(std::fs::read(source.join(STATE))? == source_bytes);
        assert!(std::fs::read(source.join(GOALS))? == source_goals);
        assert!(std::fs::read(source.join("session_index.jsonl"))? == source_index);
        rollback(&planned)?;
        rollback(&planned)?;
        assert!(thread_path(&target, &ids[0])?.is_none());
        Ok(())
    }

    #[test]
    fn changed_goal_refuses_before_removing_any_thread() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["active", "complete"])?;
        let planned = plan(&source, &target, &ids)?;
        apply(&planned)?;
        let goals = Connection::open(target.join(GOALS))?;
        goals.execute(
            "UPDATE thread_goals SET tokens_used=7 WHERE thread_id=?1",
            [&ids[1]],
        )?;
        assert!(remove_exact(&planned).is_err());
        assert_eq!(threads(&target)?.len(), 2);
        assert!(apply(&planned).is_err());
        Ok(())
    }

    #[test]
    fn borrowed_rows_survive_and_conflicts_never_overwrite() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["complete"])?;
        let original = plan(&source, &target, &ids)?;
        apply(&original)?;
        // Model an exact pre-existing row without relying on clock equality.
        let mut borrowed = original.clone();
        borrowed.threads[0].thread.as_mut().context("thread")?.owned = false;
        borrowed.threads[0].goal.as_mut().context("goal")?.owned = false;
        apply(&borrowed)?;
        remove_exact(&borrowed)?;
        assert_eq!(threads(&target)?.len(), 1);
        let db = Connection::open(target.join(STATE))?;
        db.execute("UPDATE threads SET title='changed' WHERE id=?1", [&ids[0]])?;
        assert!(plan(&source, &target, &ids).is_err());
        assert!(remove_exact(&original).is_err());
        assert!(apply(&original).is_err());
        assert_eq!(threads(&target)?.len(), 1);
        Ok(())
    }

    #[test]
    fn incompatible_columns_and_destination_triggers_are_refused() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["blocked"])?;
        let planned = plan(&source, &target, &ids)?;
        apply(&planned)?;
        let db = Connection::open(target.join(STATE))?;
        db.execute("ALTER TABLE threads ADD COLUMN destination_only TEXT", [])?;
        assert!(remove_exact(&planned).is_err());
        db.execute("ALTER TABLE threads DROP COLUMN destination_only", [])?;
        db.execute_batch("CREATE TABLE touched(id TEXT); CREATE TRIGGER unsafe_delete AFTER DELETE ON threads BEGIN INSERT INTO touched VALUES(OLD.id); END;")?;
        assert!(remove_exact(&planned).is_err());
        let touched: i64 = db.query_row("SELECT count(*) FROM touched", [], |r| r.get(0))?;
        assert_eq!(touched, 0);
        Ok(())
    }

    #[test]
    fn partial_commit_can_replay_or_remove_saved_plan() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["active"])?;
        let planned = plan(&source, &target, &ids)?;
        let mut state_only = planned.clone();
        state_only.threads[0].goal = None;
        apply(&state_only)?;
        ensure!(!target.join(GOALS).exists(), "unexpected goals DB");
        apply(&planned)?;
        remove_exact(&planned)?;
        assert!(threads(&target)?.is_empty());
        Ok(())
    }

    #[test]
    fn source_missing_db_or_row_allows_explicit_verified_synthesis() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = vec!["old-thread".to_owned()];
        let mut planned = plan(&source, &target, &ids)?;
        assert!(planned.threads[0].thread.is_none());
        assert!(thread_path(&source, &ids[0])?.is_none());
        assert!(threads(&source)?.is_empty());
        let info = ThreadInfo {
            id: ids[0].clone(),
            rollout_path: source.join("archived_sessions/2020/rollout-old-thread.jsonl"),
            model_provider: "openai".into(),
        };
        let mut fields = BTreeMap::new();
        fields.insert("created_at".into(), DbValue::Integer(1));
        fields.insert("updated_at".into(), DbValue::Integer(2));
        for (key, value) in [
            ("source", "cli"),
            ("cwd", "synthetic"),
            ("title", ""),
            ("sandbox_policy", "read-only"),
            ("approval_mode", "on-request"),
        ] {
            fields.insert(key.into(), DbValue::text(value));
        }
        add_synthetic_thread(&mut planned, &info, fields)?;
        ensure!(!target.exists(), "synthesis created target");
        apply(&planned)?;
        assert_eq!(
            thread_path(&target, &ids[0])?,
            Some(target.join("sessions/2020/rollout-old-thread.jsonl"))
        );
        assert!(!source.join(STATE).exists());
        let missing = plan(&target, &source, &["absent".into()])?;
        assert!(missing.threads[0].thread.is_none());
        Ok(())
    }

    #[test]
    fn unsafe_paths_providers_same_home_and_hardlinked_dbs_are_refused() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["budget_limited"])?;
        assert!(plan(&source, &source, &ids).is_err());
        let db = Connection::open(source.join(STATE))?;
        db.execute(
            "UPDATE threads SET rollout_path='sessions/../outside.jsonl'",
            [],
        )?;
        assert!(plan(&source, &target, &ids).is_err());
        db.execute(
            "UPDATE threads SET rollout_path='sessions/inside.jsonl',model_provider='qb'",
            [],
        )?;
        assert!(plan(&source, &target, &ids).is_err());
        drop(db);
        std::fs::create_dir(&target)?;
        std::fs::hard_link(source.join(STATE), target.join(STATE))?;
        assert!(plan(&source, &target, &ids).is_err());
        Ok(())
    }

    #[test]
    fn unrelated_threads_are_not_exported_or_removed() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["paused", "complete"])?;
        let planned = plan(&source, &target, &ids[..1])?;
        assert_eq!(planned.threads.len(), 1);
        apply(&planned)?;
        assert!(thread_path(&target, &ids[1])?.is_none());
        let second = plan(&source, &target, &ids[1..])?;
        apply(&second)?;
        remove_exact(&planned)?;
        assert!(thread_path(&target, &ids[0])?.is_none());
        assert!(thread_path(&target, &ids[1])?.is_some());
        Ok(())
    }

    #[test]
    fn overlapping_ancestor_keeps_target_goal_timestamp_and_is_unowned() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["active", "complete"])?;
        let first = plan(&source, &target, &ids[..1])?;
        apply(&first)?;
        let db = Connection::open(target.join(GOALS))?;
        db.execute(
            "UPDATE thread_goals SET updated_at_ms=123 WHERE thread_id=?1",
            [&ids[0]],
        )?;
        let shared = plan(&source, &target, &ids)?;
        let ancestor = shared.threads[0].goal.as_ref().context("ancestor")?;
        assert!(!ancestor.owned);
        assert!(ancestor.values["updated_at_ms"] == DbValue::Integer(123));
        assert!(
            !shared.threads[0]
                .thread
                .as_ref()
                .context("ancestor thread")?
                .owned
        );
        apply(&shared)?;
        remove_exact(&shared)?;
        assert_eq!(threads(&target)?.len(), 1);
        Ok(())
    }

    #[test]
    fn canonical_home_maps_existing_ordinary_rollout_path() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let rollout = source.join("archived_sessions/2026/rollout-existing.jsonl");
        std::fs::create_dir_all(rollout.parent().context("parent")?)?;
        std::fs::write(&rollout, [])?;
        let canonical_home = source.canonicalize()?;
        assert_eq!(
            mapped_path(&canonical_home, &target, &rollout)?,
            target.join("sessions/2026/rollout-existing.jsonl")
        );
        Ok(())
    }
}
