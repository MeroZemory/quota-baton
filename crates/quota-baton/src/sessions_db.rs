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
//! Fresh databases retain the source's versioned schema and migration ledger,
//! but never unrelated user rows. Rollback retains these shared schema objects.
//! Official triggers are checked against migrations/0025_thread_timestamps_millis.sql
//! and migrations/0039_threads_recency_at.sql, not trusted by name alone.

use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const STATE: &str = "state_5.sqlite";
const GOALS: &str = "goals_1.sqlite";
const DB_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

// Exact official bodies from Codex state migrations 0025 and 0039 above.
// SQLite omits the final semicolon in sqlite_schema; whitespace is normalized.
const OFFICIAL_TRIGGERS: [(&str, &str); 5] = [
    (
        "threads_created_at_ms_after_insert",
        "CREATE TRIGGER threads_created_at_ms_after_insert
AFTER INSERT ON threads
WHEN NEW.created_at_ms IS NULL
BEGIN
    UPDATE threads
    SET created_at_ms = NEW.created_at * 1000
    WHERE id = NEW.id;
END",
    ),
    (
        "threads_updated_at_ms_after_insert",
        "CREATE TRIGGER threads_updated_at_ms_after_insert
AFTER INSERT ON threads
WHEN NEW.updated_at_ms IS NULL
BEGIN
    UPDATE threads
    SET updated_at_ms = NEW.updated_at * 1000
    WHERE id = NEW.id;
END",
    ),
    (
        "threads_created_at_ms_after_update",
        "CREATE TRIGGER threads_created_at_ms_after_update
AFTER UPDATE OF created_at ON threads
WHEN NEW.created_at != OLD.created_at
 AND NEW.created_at_ms IS OLD.created_at_ms
BEGIN
    UPDATE threads
    SET created_at_ms = NEW.created_at * 1000
    WHERE id = NEW.id;
END",
    ),
    (
        "threads_updated_at_ms_after_update",
        "CREATE TRIGGER threads_updated_at_ms_after_update
AFTER UPDATE OF updated_at ON threads
WHEN NEW.updated_at != OLD.updated_at
 AND NEW.updated_at_ms IS OLD.updated_at_ms
BEGIN
    UPDATE threads
    SET updated_at_ms = NEW.updated_at * 1000
    WHERE id = NEW.id;
END",
    ),
    (
        "threads_recency_at_after_insert",
        "CREATE TRIGGER threads_recency_at_after_insert
AFTER INSERT ON threads
WHEN NEW.recency_at_ms = 0
BEGIN
    UPDATE threads
    SET recency_at = NEW.updated_at,
        recency_at_ms = COALESCE(NEW.updated_at_ms, NEW.updated_at * 1000)
    WHERE id = NEW.id;
END",
    ),
];

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
    /// False for an identical destination row already present when planning.
    /// Source snapshots explicitly own their selected rows for move/restore.
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
    #[serde(default)]
    pub(crate) state_bootstrap: Option<BootstrapPlan>,
    #[serde(default)]
    pub(crate) goals_bootstrap: Option<BootstrapPlan>,
    /// Exact same-home move/restore snapshot, never a transformed import plan.
    #[serde(default)]
    pub(crate) is_source_snapshot: bool,
    /// Return copies current state verbatim, apart from the rollout path and
    /// generated columns depending on it. Import-only normalization is disabled.
    #[serde(default)]
    pub(crate) preserve_current_state: bool,
    pub(crate) threads: Vec<ThreadPlan>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SchemaObject {
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) ddl: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BootstrapPlan {
    pub(crate) objects: Vec<SchemaObject>,
    pub(crate) migrations_schema: TableSchema,
    /// Only successfully applied _sqlx_migrations rows, never application rows.
    pub(crate) migrations: Vec<RowSnapshot>,
}

fn readonly(path: &Path) -> Result<Option<Connection>> {
    let path = local_path(path);
    crate::sessions_guard::safe_path(path.parent().context("metadata path lacks home")?, &path)?;
    match path.try_exists()? {
        false => Ok(None),
        true => {
            let db = with_busy_timeout(Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?)?;
            db.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED")?;
            Ok(Some(db))
        }
    }
}

fn with_busy_timeout(db: Connection) -> Result<Connection> {
    db.busy_timeout(DB_BUSY_TIMEOUT)?;
    Ok(db)
}

/// All homes must agree on the optional goal, excluding only updated_at_ms.
/// Goal identity, creation time, status, objective, budgets, both usage counters
/// and every unknown column remain exact. Missing-vs-present goals conflict.
/// No goal values are emitted; callers retain the relevant writer guards when
/// using this inventory decision to commit a move or return.
pub(crate) fn compare_goal_state(homes: &[PathBuf], id: &str) -> Result<bool> {
    let mut states = Vec::new();
    for home in homes {
        let db = readonly(&home.join(GOALS))?;
        let row = db
            .as_ref()
            .map(|db| selected_row(db, "thread_goals", "thread_id", id))
            .transpose()?
            .flatten();
        states.push(row.map(|row| {
            let mut values = row.values;
            values.remove("updated_at_ms");
            values
        }));
    }
    Ok(states.windows(2).all(|pair| pair[0] == pair[1]))
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

fn normalized_sql(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn schema_objects(db: &Connection) -> Result<Vec<SchemaObject>> {
    let mut statement = db.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema
         WHERE sql IS NOT NULL AND name NOT GLOB 'sqlite_*'
         ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, name",
    )?;
    let objects = statement
        .query_map([], |row| {
            Ok(SchemaObject {
                kind: row.get(0)?,
                name: row.get(1)?,
                table: row.get(2)?,
                ddl: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(objects)
}

fn validate_trigger(object: &SchemaObject) -> Result<()> {
    ensure!(
        object.kind == "trigger"
            && object.table == "threads"
            && OFFICIAL_TRIGGERS
                .iter()
                .any(|(name, ddl)| *name == object.name
                    && normalized_sql(ddl) == normalized_sql(&object.ddl)),
        "unsupported SQLite trigger; refusing indirect writes"
    );
    Ok(())
}

fn validate_triggers(db: &Connection) -> Result<()> {
    for object in schema_objects(db)?
        .iter()
        .filter(|object| object.kind == "trigger")
    {
        validate_trigger(object)?;
    }
    Ok(())
}

fn migration_rows(db: &Connection) -> Result<Vec<RowSnapshot>> {
    if schema(db, "_sqlx_migrations")?.is_none() {
        return Ok(Vec::new());
    }
    let mut statement =
        db.prepare("SELECT version, success FROM _sqlx_migrations ORDER BY version")?;
    let versions = statement
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut rows = Vec::new();
    for (version, success) in versions {
        ensure!(
            success == 1,
            "SQLite migration ledger contains an unsuccessful migration"
        );
        rows.push(
            selected_row(db, "_sqlx_migrations", "version", &version.to_string())?
                .context("migration snapshot disappeared")?,
        );
    }
    Ok(rows)
}

fn bootstrap_snapshot(db: &Connection) -> Result<Option<BootstrapPlan>> {
    // Even unversioned sources must not carry arbitrary executable triggers.
    validate_triggers(db)?;
    let migrations = migration_rows(db)?;
    if migrations.is_empty() {
        return Ok(None);
    }
    let objects = schema_objects(db)?;
    ensure!(
        objects.iter().all(
            |object| matches!(object.kind.as_str(), "table" | "index" | "trigger")
                && !object
                    .ddl
                    .to_ascii_uppercase()
                    .starts_with("CREATE VIRTUAL")
        ),
        "unsupported versioned SQLite schema object"
    );
    Ok(Some(BootstrapPlan {
        objects,
        migrations,
        migrations_schema: schema(db, "_sqlx_migrations")?.context("missing migrations schema")?,
    }))
}

/// Repair only an uninitialized database whose existing objects are an exact
/// subset of the source schema. A nonempty ledger belongs to an initialized
/// destination and is never replaced or extended by this adapter.
fn bootstrap(db: &Connection, snapshot: &BootstrapPlan) -> Result<()> {
    validate_triggers(db)?;
    if !migration_rows(db)?.is_empty() {
        return Ok(());
    }
    for object in &snapshot.objects {
        if object.kind == "trigger" {
            validate_trigger(object)?;
        }
    }
    let existing = schema_objects(db)?;
    for object in &existing {
        ensure!(
            snapshot
                .objects
                .iter()
                .any(|source| source.kind == object.kind
                    && source.name == object.name
                    && source.table == object.table
                    && normalized_sql(&source.ddl) == normalized_sql(&object.ddl)),
            "uninitialized destination has incompatible schema objects"
        );
    }
    // Tables, then indexes, then allowlisted triggers. The caller's IMMEDIATE
    // transaction includes all schema, ledger and selected row writes.
    for kind in ["table", "index", "trigger"] {
        for object in snapshot.objects.iter().filter(|object| object.kind == kind) {
            if !existing
                .iter()
                .any(|current| current.kind == object.kind && current.name == object.name)
            {
                db.execute(&object.ddl, [])?;
            }
        }
    }
    ensure!(
        columns(db, "_sqlx_migrations")? == snapshot.migrations_schema.columns,
        "incompatible migration ledger columns"
    );
    for row in &snapshot.migrations {
        ensure!(
            row.values.get("success") == Some(&DbValue::Integer(1)),
            "invalid migration snapshot"
        );
        insert_row(db, "_sqlx_migrations", &snapshot.migrations_schema, row)?;
    }
    ensure!(
        migration_rows(db)?
            .iter()
            .map(|row| &row.values)
            .eq(snapshot.migrations.iter().map(|row| &row.values)),
        "destination coerced migration metadata"
    );
    Ok(())
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
    plan_with_mode(source_home, target_home, ids, false)
}

/// Read the authoritative home's CURRENT rows, never an old import snapshot.
/// Preserve goal status, objective, counters and timestamps, and thread archive
/// state. Map only rollout_path (and its generated dependents). Capture the
/// root-only source_snapshot separately under the same guards before removal.
pub(crate) fn plan_return(
    source_home: &Path,
    target_home: &Path,
    ids: &[String],
) -> Result<DbPlan> {
    plan_with_mode(source_home, target_home, ids, true)
}

fn plan_with_mode(
    source_home: &Path,
    target_home: &Path,
    ids: &[String],
    preserve_current_state: bool,
) -> Result<DbPlan> {
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
    let state_bootstrap = state
        .as_ref()
        .map(bootstrap_snapshot)
        .transpose()?
        .flatten();
    let goals_bootstrap = goals
        .as_ref()
        .map(bootstrap_snapshot)
        .transpose()?
        .flatten();
    let state_triggers = state
        .as_ref()
        .map(schema_objects)
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .filter(|object| object.kind == "trigger")
        .collect::<Vec<_>>();
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
            if !preserve_current_state {
                row.values.insert("archived".into(), DbValue::Integer(0));
            }
            recompute_generated(
                threads_schema.as_ref().context("missing threads schema")?,
                "threads",
                "id",
                &id,
                row,
                if preserve_current_state {
                    &[]
                } else {
                    &state_triggers
                },
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
            if !preserve_current_state {
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
                    &[],
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
        state_bootstrap,
        goals_bootstrap,
        is_source_snapshot: false,
        preserve_current_state,
        threads: selected,
    })
}

/// Capture only explicitly selected ROOT ids, without traversing fork ancestry.
/// Persist/encrypt this entire plan before deleting any source rows or names.
/// `remove_exact` deletes these original rows; `apply` restores their exact
/// values, including archived state, paths, active status and original times.
/// No index file is read or written. Caller holds source/destination guards
/// through destination verification and source removal, and restores names
/// before restoring source rows on unimport.
pub(crate) fn source_snapshot(home: &Path, root_ids: &[String]) -> Result<DbPlan> {
    ensure!(home.is_absolute(), "metadata home must be absolute");
    let state = readonly(&home.join(STATE))?;
    let goals = readonly(&home.join(GOALS))?;
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
    let state_bootstrap = state
        .as_ref()
        .map(bootstrap_snapshot)
        .transpose()?
        .flatten();
    let goals_bootstrap = goals
        .as_ref()
        .map(bootstrap_snapshot)
        .transpose()?
        .flatten();
    let mut selected = Vec::new();
    for id in root_ids.iter().cloned().collect::<BTreeSet<_>>() {
        let mut thread = state
            .as_ref()
            .map(|db| selected_row(db, "threads", "id", &id))
            .transpose()?
            .flatten();
        let mut goal = goals
            .as_ref()
            .map(|db| selected_row(db, "thread_goals", "thread_id", &id))
            .transpose()?
            .flatten();
        let source = thread.as_ref().map(thread_info).transpose()?;
        let source_goal_status = goal
            .as_ref()
            .map(|row| field(row, "status").map(str::to_owned))
            .transpose()?;
        if let Some(row) = &mut thread {
            row.owned = true;
        }
        if let Some(row) = &mut goal {
            row.owned = true;
        }
        selected.push(ThreadPlan {
            id,
            source,
            thread,
            goal,
            source_goal_status,
            latest_index_entry: None,
        });
    }
    Ok(DbPlan {
        source_home: home.to_owned(),
        target_home: home.to_owned(),
        state_path: home.join(STATE),
        goals_path: home.join(GOALS),
        threads_schema,
        goals_schema,
        state_bootstrap,
        goals_bootstrap,
        is_source_snapshot: true,
        preserve_current_state: false,
        threads: selected,
    })
}

/// Read-only post-apply verification before a move deletes its source. A thread
/// row must exactly match the full snapshot at its planned path/provider, and
/// its optional goal must match (or remain absent). Includes borrowed rows;
/// verification does not change ownership or reuse removal's borrowed-row skip.
pub(crate) fn verify(plan: &DbPlan) -> Result<()> {
    ensure!(
        plan.state_path == plan.target_home.join(STATE)
            && plan.goals_path == plan.target_home.join(GOALS),
        "invalid metadata plan paths"
    );
    let state = readonly(&plan.state_path)?;
    let goals = readonly(&plan.goals_path)?;
    for thread in &plan.threads {
        let expected = thread
            .thread
            .as_ref()
            .context("cannot verify destination without planned thread metadata")?;
        let actual = state
            .as_ref()
            .map(|db| selected_row(db, "threads", "id", &thread.id))
            .transpose()?
            .flatten()
            .context("destination thread row is missing")?;
        ensure!(
            field(&actual, "rollout_path")? == field(expected, "rollout_path")?
                && field(&actual, "model_provider")? == field(expected, "model_provider")?,
            "destination thread path or provider differs for {}",
            thread.id
        );
        ensure!(
            actual.values == expected.values,
            "destination thread metadata differs for {}",
            thread.id
        );
        let actual_goal = goals
            .as_ref()
            .map(|db| selected_row(db, "thread_goals", "thread_id", &thread.id))
            .transpose()?
            .flatten();
        ensure!(
            actual_goal.as_ref().map(|row| &row.values)
                == thread.goal.as_ref().map(|row| &row.values),
            "destination goal differs for {}",
            thread.id
        );
    }
    Ok(())
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
        !plan.is_source_snapshot,
        "cannot synthesize rows into an exact source snapshot"
    );
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
                let db = with_busy_timeout(Connection::open_in_memory()?)?;
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
    let db = with_busy_timeout(Connection::open_in_memory()?)?;
    db.execute_batch("PRAGMA foreign_keys=OFF")?;
    db.execute(&expected.ddl, [])?;
    let synthesis_objects = if let Some(target) = &target {
        schema_objects(target)?
    } else {
        plan.state_bootstrap
            .as_ref()
            .map(|snapshot| snapshot.objects.clone())
            .unwrap_or_default()
    };
    for object in synthesis_objects
        .iter()
        .filter(|object| object.kind == "trigger")
    {
        validate_trigger(object)?;
        db.execute(&object.ddl, [])?;
    }
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
    validate_triggers(db)?;
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
    prepare_with_operational_rebase(plan, remove, false)
}

fn prepare_with_operational_rebase<'a>(
    plan: &'a DbPlan,
    remove: bool,
    after_resume: bool,
) -> Result<Vec<Pending<'a>>> {
    ensure!(
        plan.state_path == plan.target_home.join(STATE)
            && plan.goals_path == plan.target_home.join(GOALS),
        "invalid metadata plan paths"
    );
    if plan.is_source_snapshot {
        ensure!(
            plan.source_home == plan.target_home,
            "source snapshot homes differ"
        );
    } else if plan.target_home.try_exists()? && plan.source_home.try_exists()? {
        ensure!(
            plan.source_home.canonicalize()? != plan.target_home.canonicalize()?,
            "source and target homes coincide"
        );
    }
    let mut pending = Vec::new();
    if plan.threads.is_empty() {
        return Ok(pending);
    }
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
                row.filter(|row| !remove || row.owned)
                    .map(|row| (thread.id.as_str(), row))
            })
            .collect::<Vec<_>>();
        let snapshot = if is_goal {
            plan.goals_bootstrap.as_ref()
        } else {
            plan.state_bootstrap.as_ref()
        };
        if rows.is_empty() && (remove || snapshot.is_none()) {
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
        let db = with_busy_timeout(Connection::open_with_flags(local_path(path), flags)?)?;
        // Preserve foreign-key columns without cloning referenced global tables;
        // disable cascades so exact-row removal cannot delete unrelated metadata.
        db.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE")?;
        validate_triggers(&db)?;
        if !remove && let Some(snapshot) = snapshot {
            bootstrap(&db, snapshot)?;
        }
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
                Some(current) => {
                    // Rebase only the comparison snapshot while both database
                    // write transactions are held. Never rewrite the journal or
                    // relax identity, rollout path, provider, goals or unknown
                    // fields. Borrowed rows were excluded above for removal.
                    let mut values = row.values.clone();
                    if after_resume && remove && !is_goal && row.owned {
                        for name in [
                            "created_at",
                            "updated_at",
                            "tokens_used",
                            "created_at_ms",
                            "updated_at_ms",
                            "recency_at",
                            "recency_at_ms",
                        ] {
                            if values.contains_key(name)
                                && let Some(value) = current.values.get(name)
                            {
                                ensure!(
                                    matches!(value, DbValue::Integer(_))
                                        || (matches!(name, "created_at_ms" | "updated_at_ms")
                                            && *value == DbValue::Null),
                                    "invalid operational metadata for {id}"
                                );
                                values.insert(name.to_owned(), value.clone());
                            }
                        }
                    }
                    ensure!(
                        current.values == values,
                        "destination {table} row changed for {id}"
                    );
                }
                None => {
                    ensure!(
                        row.owned,
                        "unowned destination {table} row disappeared for {id}"
                    );
                    if after_resume && remove && !is_goal {
                        let replacement: bool = db.query_row(
                            "SELECT EXISTS(SELECT 1 FROM threads WHERE rollout_path=?1 AND id<>?2)",
                            (field(row, "rollout_path")?, *id),
                            |row| row.get(0),
                        )?;
                        ensure!(!replacement, "destination thread identity changed for {id}");
                    }
                }
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
        // Exact restoration and current-state return must not let official
        // INSERT triggers rewrite a captured legacy NULL/zero timestamp.
        // Suspend only the allowlisted triggers in this transaction, restoring their
        // original definitions before commit. Failed inserts roll back DDL too.
        let mut restore_triggers = Vec::new();
        if plan.is_source_snapshot || plan.preserve_current_state {
            let mut has_missing = false;
            for (id, _) in &item.rows {
                has_missing |= selected_row(&item.db, item.table, item.key, id)?.is_none();
            }
            if has_missing {
                restore_triggers = schema_objects(&item.db)?
                    .into_iter()
                    .filter(|object| object.kind == "trigger")
                    .collect();
                for trigger in &restore_triggers {
                    validate_trigger(trigger)?;
                    item.db
                        .execute(&format!("DROP TRIGGER {}", quoted(&trigger.name)), [])?;
                }
            }
        }
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
        for trigger in &restore_triggers {
            item.db.execute(&trigger.ddl, [])?;
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
    triggers: &[SchemaObject],
) -> Result<()> {
    if schema.columns.values().all(|writable| *writable) && triggers.is_empty() {
        return Ok(());
    }
    let db = with_busy_timeout(Connection::open_in_memory()?)?;
    db.execute_batch("PRAGMA foreign_keys=OFF")?;
    db.execute(&schema.ddl, [])?;
    for trigger in triggers {
        validate_trigger(trigger)?;
        db.execute(&trigger.ddl, [])?;
    }
    insert_row(&db, table, schema, row)?;
    let computed = selected_row(&db, table, key, id)?.context("generated row missing")?;
    row.values = computed.values;
    Ok(())
}

/// Validate all owned rows before removing any. Missing owned rows are harmless
/// for crash recovery. Borrowed rows are neither inspected nor deleted: another
/// import owns their current snapshots and may legitimately have changed them.
pub(crate) fn remove_exact(plan: &DbPlan) -> Result<()> {
    remove_prepared(prepare(plan, true)?)
}

fn remove_prepared(pending: Vec<Pending<'_>>) -> Result<()> {
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

/// Force permits only Codex's known operational thread time/token changes.
/// Every owned row in both databases is validated under IMMEDIATE transactions
/// before any deletion. Goals remain exact; borrowed rows remain untouched.
/// The original encrypted plan is not rebased or modified by this operation.
pub(crate) fn rollback_after_resume(plan: &DbPlan, force: bool) -> Result<()> {
    if !force {
        return rollback(plan);
    }
    ensure!(
        !plan.is_source_snapshot,
        "forced rollback requires a destination import plan"
    );
    remove_prepared(prepare_with_operational_rebase(plan, true, true)?)
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
             CREATE TABLE audit(id TEXT);"
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

    fn add_ledger(db: &Connection, versions: &[i64]) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        )",
        )?;
        for version in versions {
            db.execute("INSERT INTO _sqlx_migrations VALUES (?1,'synthetic migration','2026-10-21 00:00:00',1,X'00FF1020',17)", [version])?;
        }
        Ok(())
    }

    fn versioned_fixture(source: &Path) -> Result<Vec<String>> {
        let ids = fixture(source, &["active", "complete"])?;
        let db = Connection::open(source.join(STATE))?;
        db.execute_batch("PRAGMA foreign_keys=OFF;
            ALTER TABLE threads ADD COLUMN created_at_ms INTEGER;
            ALTER TABLE threads ADD COLUMN updated_at_ms INTEGER;
            ALTER TABLE threads ADD COLUMN recency_at INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE threads ADD COLUMN recency_at_ms INTEGER NOT NULL DEFAULT 0;
            UPDATE threads SET created_at_ms=1000, updated_at_ms=2000, recency_at=2, recency_at_ms=2000;
            CREATE TABLE projects(id TEXT PRIMARY KEY, value TEXT);
            INSERT INTO projects VALUES('uncopied-project','unrelated project');
            CREATE TABLE thread_dynamic_tools(id INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT, value BLOB);
            INSERT INTO thread_dynamic_tools(thread_id,value) VALUES('thread-0', X'01');
            CREATE INDEX dynamic_thread ON thread_dynamic_tools(thread_id);")?;
        for (_, ddl) in OFFICIAL_TRIGGERS {
            db.execute(ddl, [])?;
        }
        add_ledger(&db, &[1, 25, 39])?;
        let goals = Connection::open(source.join(GOALS))?;
        add_ledger(&goals, &[1, 2])?;
        goals.execute("INSERT INTO unrelated VALUES('unrelated user row')", [])?;
        Ok(ids)
    }

    #[test]
    fn versioned_bootstrap_prevents_first_migration_collision_and_keeps_only_selected_data()
    -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let planned = plan(&source, &target, &ids[..1])?;
        assert_eq!(
            planned
                .state_bootstrap
                .as_ref()
                .context("bootstrap")?
                .migrations
                .len(),
            3
        );
        let planned: DbPlan = serde_json::from_slice(&serde_json::to_vec(&planned)?)?;
        apply(&planned)?;
        apply(&planned)?;
        let db = Connection::open(target.join(STATE))?;
        let applied: i64 = db.query_row(
            "SELECT count(*) FROM _sqlx_migrations WHERE version=1 AND success=1",
            [],
            |row| row.get(0),
        )?;
        // Model SQLx's first migration gate: without this ledger row the same
        // CREATE TABLE statement fails with 'table threads already exists'.
        if applied == 0 {
            db.execute(MINIMUM_THREADS_DDL, [])?;
        }
        assert_eq!(applied, 1);
        let source_db = readonly(&source.join(STATE))?.context("source DB")?;
        assert!(schema_objects(&db)? == schema_objects(&source_db)?);
        assert!(
            migration_rows(&db)?
                .iter()
                .map(|r| &r.values)
                .eq(migration_rows(&source_db)?.iter().map(|r| &r.values))
        );
        assert_eq!(threads(&target)?.len(), 1);
        for table in [
            "projects",
            "thread_dynamic_tools",
            "thread_history",
            "audit",
        ] {
            let count: i64 = db.query_row(
                &format!("SELECT count(*) FROM {}", quoted(table)),
                [],
                |row| row.get(0),
            )?;
            assert_eq!(count, 0);
        }
        let target_goals = Connection::open(target.join(GOALS))?;
        let unrelated: i64 =
            target_goals.query_row("SELECT count(*) FROM unrelated", [], |row| row.get(0))?;
        assert_eq!(unrelated, 0);
        drop(source_db);
        drop(target_goals);
        drop(db);
        remove_exact(&planned)?;
        let db = Connection::open(target.join(STATE))?;
        assert_eq!(migration_rows(&db)?.len(), 3);
        assert!(schema(&db, "thread_dynamic_tools")?.is_some());
        assert!(threads(&target)?.is_empty());
        Ok(())
    }

    #[test]
    fn bootstrap_repairs_qb_partial_schema_with_empty_sqlx_ledger() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let planned = plan(&source, &target, &ids[..1])?;
        std::fs::create_dir(&target)?;
        let db = Connection::open(target.join(STATE))?;
        db.execute_batch("PRAGMA foreign_keys=OFF")?;
        let threads_schema = planned.threads_schema.as_ref().context("threads schema")?;
        db.execute(&threads_schema.ddl, [])?;
        for ddl in &threads_schema.indexes {
            db.execute(ddl, [])?;
        }
        add_ledger(&db, &[])?;
        insert_row(
            &db,
            "threads",
            threads_schema,
            planned.threads[0].thread.as_ref().context("thread")?,
        )?;
        drop(db);
        apply(&planned)?;
        let db = Connection::open(target.join(STATE))?;
        assert_eq!(migration_rows(&db)?.len(), 3);
        validate_triggers(&db)?;
        assert_eq!(
            schema_objects(&db)?
                .iter()
                .filter(|o| o.kind == "trigger")
                .count(),
            5
        );
        assert_eq!(threads(&target)?.len(), 1);
        Ok(())
    }

    #[test]
    fn initialized_official_triggers_are_accepted_but_modified_bodies_are_refused() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let first = plan(&source, &target, &ids[..1])?;
        apply(&first)?;
        let second = plan(&source, &target, &ids[1..])?;
        apply(&second)?;
        assert_eq!(threads(&target)?.len(), 2);
        let db = Connection::open(target.join(STATE))?;
        db.execute_batch(
            "DROP TRIGGER threads_created_at_ms_after_insert;
            CREATE TRIGGER threads_created_at_ms_after_insert AFTER INSERT ON threads
            BEGIN DELETE FROM projects; END;",
        )?;
        assert!(apply(&second).is_err());
        assert!(remove_exact(&second).is_err());
        assert!(plan(&target, &source, &ids).is_err());
        Ok(())
    }

    #[test]
    fn arbitrary_source_triggers_are_refused_even_without_a_ledger() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["paused"])?;
        let db = Connection::open(source.join(STATE))?;
        db.execute_batch("CREATE TRIGGER source_only AFTER INSERT ON threads BEGIN INSERT INTO audit VALUES(NEW.id); END;")?;
        assert!(plan(&source, &target, &ids).is_err());
        assert!(!target.exists());
        Ok(())
    }

    #[test]
    fn bootstrap_conflicts_rollback_schema_and_never_adopt_unversioned_tables() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let planned = plan(&source, &target, &ids[..1])?;
        std::fs::create_dir(&target)?;
        let db = Connection::open(target.join(STATE))?;
        db.execute("CREATE TABLE unknown_user_data(value TEXT)", [])?;
        assert!(apply(&planned).is_err());
        assert!(schema(&db, "threads")?.is_none());
        db.execute("DROP TABLE unknown_user_data", [])?;
        // A deliberately changed planned snapshot fails after bootstrap creates
        // the schema, which must still roll back within the same transaction.
        let mut invalid = planned.clone();
        invalid.threads[0].thread.as_mut().context("thread")?.owned = false;
        assert!(apply(&invalid).is_err());
        assert!(schema(&db, "threads")?.is_none());
        assert!(schema(&db, "_sqlx_migrations")?.is_none());
        Ok(())
    }

    #[test]
    fn changed_borrowed_ancestor_does_not_block_child_removal() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["active", "complete"])?;
        let mut successor = plan(&source, &target, &ids[..1])?;
        apply(&successor)?;
        let child = plan(&source, &target, &ids)?;
        apply(&child)?;
        verify(&child)?;
        assert!(
            !child.threads[0]
                .thread
                .as_ref()
                .context("borrowed thread")?
                .owned
        );
        assert!(
            !child.threads[0]
                .goal
                .as_ref()
                .context("borrowed goal")?
                .owned
        );
        let state = Connection::open(target.join(STATE))?;
        let goals = Connection::open(target.join(GOALS))?;
        state.execute(
            "UPDATE threads SET title='ancestor changed' WHERE id=?1",
            [&ids[0]],
        )?;
        // Verification includes a borrowed row's non-path metadata, even
        // though child removal deliberately ignores that borrowed snapshot.
        assert!(verify(&child).is_err());
        goals.execute(
            "UPDATE thread_goals SET updated_at_ms=456,tokens_used=9 WHERE thread_id=?1",
            [&ids[0]],
        )?;
        // The surviving journal owns the current snapshots; child removal must
        // neither replace those snapshots nor insist on its stale borrowed copy.
        let mut current_thread =
            selected_row(&state, "threads", "id", &ids[0])?.context("current thread")?;
        let mut current_goal =
            selected_row(&goals, "thread_goals", "thread_id", &ids[0])?.context("current goal")?;
        current_thread.owned = true;
        current_goal.owned = true;
        successor.threads[0].thread = Some(current_thread.clone());
        successor.threads[0].goal = Some(current_goal.clone());
        remove_exact(&child)?;
        assert!(thread_path(&target, &ids[1])?.is_none());
        assert!(
            selected_row(&state, "threads", "id", &ids[0])?
                .context("retained thread")?
                .values
                == current_thread.values
        );
        assert!(
            selected_row(&goals, "thread_goals", "thread_id", &ids[0])?
                .context("retained goal")?
                .values
                == current_goal.values
        );
        remove_exact(&successor)?;
        assert!(threads(&target)?.is_empty());
        Ok(())
    }

    #[test]
    fn move_snapshot_removes_only_explicit_roots_and_restores_original_rows() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["active", "complete"])?;
        let source_index = std::fs::read(source.join("session_index.jsonl"))?;
        let snapshot = source_snapshot(&source, &ids[..1])?;
        let snapshot: DbPlan = serde_json::from_slice(&serde_json::to_vec(&snapshot)?)?;
        assert!(snapshot.is_source_snapshot);
        assert_eq!(snapshot.source_home, snapshot.target_home);
        assert_eq!(snapshot.threads.len(), 1);
        assert!(snapshot.threads[0].latest_index_entry.is_none());
        let root = snapshot.threads[0].thread.as_ref().context("root")?;
        let goal = snapshot.threads[0].goal.as_ref().context("goal")?;
        assert!(root.owned && goal.owned);
        assert!(root.values["archived"] == DbValue::Integer(1));
        assert_eq!(field(goal, "status")?, "active");
        assert!(goal.values["updated_at_ms"] == DbValue::Integer(2));
        let destination = plan(&source, &target, &ids)?;
        apply(&destination)?;
        verify(&destination)?;
        remove_exact(&snapshot)?;
        remove_exact(&snapshot)?;
        assert!(thread_path(&source, &ids[0])?.is_none());
        assert!(thread_path(&source, &ids[1])?.is_some());
        let goals = readonly(&source.join(GOALS))?.context("source goals")?;
        assert!(selected_row(&goals, "thread_goals", "thread_id", &ids[0])?.is_none());
        assert!(selected_row(&goals, "thread_goals", "thread_id", &ids[1])?.is_some());
        drop(goals);
        apply(&snapshot)?;
        apply(&snapshot)?;
        verify(&snapshot)?;
        let state = readonly(&source.join(STATE))?.context("source state")?;
        let goals = readonly(&source.join(GOALS))?.context("source goals")?;
        assert!(
            selected_row(&state, "threads", "id", &ids[0])?
                .context("restored root")?
                .values
                == root.values
        );
        assert!(
            selected_row(&goals, "thread_goals", "thread_id", &ids[0])?
                .context("restored goal")?
                .values
                == goal.values
        );
        assert!(std::fs::read(source.join("session_index.jsonl"))? == source_index);
        // A normal import must never acquire source-write authority merely by
        // constructing a reverse plan with identical home paths.
        let mut invalid = snapshot.clone();
        invalid.is_source_snapshot = false;
        assert!(remove_exact(&invalid).is_err());
        Ok(())
    }

    #[test]
    fn source_snapshot_refuses_changed_owned_rows_before_deleting_anything() -> Result<()> {
        let (_temp, source, _target) = homes()?;
        let ids = fixture(&source, &["active", "complete"])?;
        let snapshot = source_snapshot(&source, &ids)?;
        let goals = Connection::open(source.join(GOALS))?;
        goals.execute(
            "UPDATE thread_goals SET tokens_used=17 WHERE thread_id=?1",
            [&ids[1]],
        )?;
        assert!(remove_exact(&snapshot).is_err());
        assert_eq!(threads(&source)?.len(), 2);
        assert!(apply(&snapshot).is_err());
        let changed =
            selected_row(&goals, "thread_goals", "thread_id", &ids[1])?.context("changed goal")?;
        assert!(changed.values["tokens_used"] == DbValue::Integer(17));
        Ok(())
    }

    #[test]
    fn destination_verify_failure_leaves_source_rows_intact() -> Result<()> {
        let (_temp, source, target) = homes()?;
        // This test deletes a row through a normal SQLite connection. Use a
        // versioned source so bootstrap supplies the referenced projects schema;
        // the legacy fixture deliberately has a dangling project-table reference.
        let ids = versioned_fixture(&source)?;
        let destination = plan(&source, &target, &ids[..1])?;
        apply(&destination)?;
        verify(&destination)?;
        let goals = Connection::open(target.join(GOALS))?;
        goals.execute("UPDATE thread_goals SET tokens_used=1", [])?;
        assert!(verify(&destination).is_err());
        assert!(thread_path(&source, &ids[0])?.is_some());
        goals.execute("UPDATE thread_goals SET tokens_used=0", [])?;
        verify(&destination)?;
        let state = Connection::open(target.join(STATE))?;
        let projects: i64 =
            state.query_row("SELECT count(*) FROM projects", [], |row| row.get(0))?;
        assert_eq!(projects, 0);
        state.execute("UPDATE threads SET rollout_path='sessions/wrong.jsonl'", [])?;
        assert!(verify(&destination).is_err());
        assert!(thread_path(&source, &ids[0])?.is_some());
        state.execute("DELETE FROM threads", [])?;
        assert!(verify(&destination).is_err());
        Ok(())
    }

    #[test]
    fn source_restore_preserves_legacy_null_times_and_official_trigger_schema() -> Result<()> {
        let (_temp, source, _target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let state = Connection::open(source.join(STATE))?;
        state.execute("UPDATE threads SET created_at_ms=NULL,updated_at_ms=NULL,recency_at=0,recency_at_ms=0 WHERE id=?1", [&ids[0]])?;
        let original_schema = schema_objects(&state)?;
        let snapshot = source_snapshot(&source, &ids[..1])?;
        let expected = snapshot.threads[0]
            .thread
            .as_ref()
            .context("original thread")?;
        assert!(expected.values["created_at_ms"] == DbValue::Null);
        assert!(expected.values["recency_at_ms"] == DbValue::Integer(0));
        remove_exact(&snapshot)?;
        apply(&snapshot)?;
        let restored = selected_row(&state, "threads", "id", &ids[0])?.context("restored row")?;
        assert!(restored.values == expected.values);
        assert!(schema_objects(&state)? == original_schema);
        validate_triggers(&state)?;
        Ok(())
    }

    #[test]
    fn source_snapshot_without_rows_does_not_read_index_or_create_databases() -> Result<()> {
        let (_temp, source, _target) = homes()?;
        let index = b"not JSON; snapshot must never open this file";
        std::fs::write(source.join("session_index.jsonl"), index)?;
        let snapshot = source_snapshot(&source, &["rollout-only-root".into()])?;
        assert!(snapshot.threads[0].thread.is_none());
        assert!(snapshot.threads[0].goal.is_none());
        remove_exact(&snapshot)?;
        apply(&snapshot)?;
        assert!(!source.join(STATE).exists());
        assert!(!source.join(GOALS).exists());
        assert!(std::fs::read(source.join("session_index.jsonl"))? == index);
        Ok(())
    }

    #[test]
    fn forced_rollback_allows_only_owned_operational_thread_changes() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let planned = plan(&source, &target, &ids)?;
        let journal = serde_json::to_vec(&planned)?;
        apply(&planned)?;
        let state = Connection::open(target.join(STATE))?;
        state.execute(
            "UPDATE threads SET created_at=42,updated_at=77,tokens_used=88,created_at_ms=999,
            updated_at_ms=77000,recency_at=77,recency_at_ms=77000 WHERE id=?1",
            [&ids[0]],
        )?;
        assert!(rollback(&planned).is_err());
        assert!(rollback_after_resume(&planned, false).is_err());
        assert_eq!(threads(&target)?.len(), 2);
        rollback_after_resume(&planned, true)?;
        rollback_after_resume(&planned, true)?;
        assert!(threads(&target)?.is_empty());
        let goals = Connection::open(target.join(GOALS))?;
        let count: i64 =
            goals.query_row("SELECT count(*) FROM thread_goals", [], |row| row.get(0))?;
        assert_eq!(count, 0);
        assert_eq!(threads(&source)?.len(), 2);
        assert!(serde_json::to_vec(&planned)? == journal);
        Ok(())
    }

    #[test]
    fn forced_rollback_refuses_owned_identity_path_provider_and_other_semantic_changes()
    -> Result<()> {
        for change in [
            "UPDATE threads SET rollout_path='sessions/wrong.jsonl' WHERE id=?1",
            "UPDATE threads SET model_provider='qb' WHERE id=?1",
            "UPDATE threads SET title='changed title' WHERE id=?1",
            "UPDATE threads SET future_blob=X'AB' WHERE id=?1",
            "UPDATE threads SET id='different-id' WHERE id=?1",
        ] {
            let (_temp, source, target) = homes()?;
            let ids = versioned_fixture(&source)?;
            let planned = plan(&source, &target, &ids)?;
            apply(&planned)?;
            let state = Connection::open(target.join(STATE))?;
            state.execute(
                "UPDATE threads SET updated_at=77,tokens_used=88 WHERE id=?1",
                [&ids[0]],
            )?;
            state.execute(change, [&ids[1]])?;
            assert!(rollback_after_resume(&planned, true).is_err());
            assert_eq!(threads(&target)?.len(), 2);
            let goals = Connection::open(target.join(GOALS))?;
            let count: i64 =
                goals.query_row("SELECT count(*) FROM thread_goals", [], |row| row.get(0))?;
            assert_eq!(count, 2);
        }
        Ok(())
    }

    #[test]
    fn forced_rollback_keeps_goal_rows_exact_and_validates_both_dbs_before_delete() -> Result<()> {
        for change in [
            "UPDATE thread_goals SET status='paused' WHERE thread_id=?1",
            "UPDATE thread_goals SET objective='changed objective' WHERE thread_id=?1",
            "UPDATE thread_goals SET updated_at_ms=updated_at_ms+1 WHERE thread_id=?1",
            "UPDATE thread_goals SET tokens_used=9 WHERE thread_id=?1",
        ] {
            let (_temp, source, target) = homes()?;
            let ids = versioned_fixture(&source)?;
            let planned = plan(&source, &target, &ids)?;
            apply(&planned)?;
            let state = Connection::open(target.join(STATE))?;
            state.execute(
                "UPDATE threads SET updated_at=77,tokens_used=88 WHERE id=?1",
                [&ids[0]],
            )?;
            let goals = Connection::open(target.join(GOALS))?;
            goals.execute(change, [&ids[1]])?;
            assert!(rollback_after_resume(&planned, true).is_err());
            assert_eq!(threads(&target)?.len(), 2);
            let count: i64 =
                goals.query_row("SELECT count(*) FROM thread_goals", [], |row| row.get(0))?;
            assert_eq!(count, 2);
        }
        Ok(())
    }

    #[test]
    fn forced_rollback_does_not_rebase_validate_or_delete_borrowed_rows() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let ancestor = plan(&source, &target, &ids[..1])?;
        apply(&ancestor)?;
        let child = plan(&source, &target, &ids)?;
        apply(&child)?;
        assert!(
            !child.threads[0]
                .thread
                .as_ref()
                .context("borrowed thread")?
                .owned
        );
        assert!(
            !child.threads[0]
                .goal
                .as_ref()
                .context("borrowed goal")?
                .owned
        );
        let state = Connection::open(target.join(STATE))?;
        state.execute("UPDATE threads SET updated_at=77,tokens_used=88", [])?;
        let goals = Connection::open(target.join(GOALS))?;
        goals.execute(
            "UPDATE thread_goals SET objective='surviving ancestor goal' WHERE thread_id=?1",
            [&ids[0]],
        )?;
        let current_thread =
            selected_row(&state, "threads", "id", &ids[0])?.context("current ancestor")?;
        let current_goal =
            selected_row(&goals, "thread_goals", "thread_id", &ids[0])?.context("current goal")?;
        rollback_after_resume(&child, true)?;
        assert!(
            selected_row(&state, "threads", "id", &ids[0])?
                .context("surviving ancestor")?
                .values
                == current_thread.values
        );
        assert!(
            selected_row(&goals, "thread_goals", "thread_id", &ids[0])?
                .context("surviving goal")?
                .values
                == current_goal.values
        );
        assert!(thread_path(&target, &ids[1])?.is_none());
        Ok(())
    }

    #[test]
    fn goal_inventory_ignores_only_update_time_and_compares_all_semantic_fields() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["active"])?;
        let current = plan_return(&source, &target, &ids)?;
        apply(&current)?;
        let homes = vec![source.clone(), target.clone()];
        assert!(compare_goal_state(&homes, &ids[0])?);
        let target_goals = Connection::open(target.join(GOALS))?;
        target_goals.execute("UPDATE thread_goals SET updated_at_ms=987654321", [])?;
        assert!(compare_goal_state(&homes, &ids[0])?);
        for change in [
            "UPDATE thread_goals SET status='paused' WHERE thread_id=?1",
            "UPDATE thread_goals SET objective='different synthetic objective' WHERE thread_id=?1",
            "UPDATE thread_goals SET goal_id='another-goal' WHERE thread_id=?1",
            "UPDATE thread_goals SET token_budget=1000 WHERE thread_id=?1",
            "UPDATE thread_goals SET tokens_used=99 WHERE thread_id=?1",
            "UPDATE thread_goals SET time_used_seconds=33 WHERE thread_id=?1",
            "UPDATE thread_goals SET created_at_ms=42 WHERE thread_id=?1",
            "UPDATE thread_goals SET future_goal=X'0304' WHERE thread_id=?1",
        ] {
            target_goals.execute(change, [&ids[0]])?;
            assert!(!compare_goal_state(&homes, &ids[0])?);
            target_goals.execute("DELETE FROM thread_goals WHERE thread_id=?1", [&ids[0]])?;
            insert_row(
                &target_goals,
                "thread_goals",
                current.goals_schema.as_ref().context("goal schema")?,
                current.threads[0].goal.as_ref().context("goal snapshot")?,
            )?;
            assert!(compare_goal_state(&homes, &ids[0])?);
        }
        target_goals.execute("DELETE FROM thread_goals WHERE thread_id=?1", [&ids[0]])?;
        assert!(!compare_goal_state(&homes, &ids[0])?);
        assert!(compare_goal_state(&homes, "absent-id")?);
        assert!(compare_goal_state(&[], &ids[0])?);
        Ok(())
    }

    #[test]
    fn goal_inventory_missing_home_is_read_only_and_differs_from_present_goal() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = fixture(&source, &["active"])?;
        assert!(compare_goal_state(std::slice::from_ref(&target), &ids[0])?);
        assert!(!compare_goal_state(&[source, target.clone()], &ids[0])?);
        assert!(!target.exists());
        Ok(())
    }

    #[test]
    fn return_moves_current_goal_and_thread_state_without_restoring_old_snapshots() -> Result<()> {
        let (_temp, account, qb) = homes()?;
        let ids = versioned_fixture(&account)?;
        let old_source = source_snapshot(&account, &ids[..1])?;
        let imported = plan(&account, &qb, &ids[..1])?;
        apply(&imported)?;
        verify(&imported)?;
        remove_exact(&old_source)?;
        let qb_state = Connection::open(qb.join(STATE))?;
        qb_state.execute(
            "UPDATE threads SET updated_at=88,tokens_used=450,created_at_ms=NULL,
            updated_at_ms=NULL,recency_at=0,recency_at_ms=0,archived=1 WHERE id=?1",
            [&ids[0]],
        )?;
        let qb_goals = Connection::open(qb.join(GOALS))?;
        qb_goals.execute("UPDATE thread_goals SET objective='current synthetic objective',status='usage_limited',
            tokens_used=123,token_budget=1000,time_used_seconds=45,updated_at_ms=9876543 WHERE thread_id=?1", [&ids[0]])?;
        let current_source = source_snapshot(&qb, &ids[..1])?;
        let returned = plan_return(&qb, &account, &ids[..1])?;
        let returned: DbPlan = serde_json::from_slice(&serde_json::to_vec(&returned)?)?;
        assert!(returned.preserve_current_state);
        let current_thread = current_source.threads[0]
            .thread
            .as_ref()
            .context("current thread")?;
        let returned_thread = returned.threads[0]
            .thread
            .as_ref()
            .context("return thread")?;
        for (name, value) in &current_thread.values {
            if !matches!(name.as_str(), "rollout_path" | "future_derived") {
                assert!(returned_thread.values.get(name) == Some(value));
            }
        }
        assert!(returned_thread.values["archived"] == DbValue::Integer(1));
        assert!(returned_thread.values["created_at_ms"] == DbValue::Null);
        let current_goal = current_source.threads[0]
            .goal
            .as_ref()
            .context("current goal")?;
        assert!(
            returned.threads[0]
                .goal
                .as_ref()
                .context("return goal")?
                .values
                == current_goal.values
        );
        assert_eq!(field(current_goal, "status")?, "usage_limited");
        assert_eq!(
            field(current_goal, "objective")?,
            "current synthetic objective"
        );
        assert!(current_goal.values["tokens_used"] == DbValue::Integer(123));
        assert!(current_goal.values["updated_at_ms"] == DbValue::Integer(9876543));
        apply(&returned)?;
        verify(&returned)?;
        remove_exact(&current_source)?;
        assert!(thread_path(&qb, &ids[0])?.is_none());
        assert!(thread_path(&account, &ids[0])?.is_some());
        assert!(thread_path(&account, &ids[1])?.is_some());
        let account_goals = readonly(&account.join(GOALS))?.context("returned goals")?;
        assert!(
            selected_row(&account_goals, "thread_goals", "thread_id", &ids[0])?
                .context("returned goal")?
                .values
                == current_goal.values
        );
        for home in [&account, &qb] {
            for filename in [STATE, GOALS] {
                let db = readonly(&home.join(filename))?.context("integrity database")?;
                let integrity: String =
                    db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                assert_eq!(integrity, "ok");
            }
        }
        Ok(())
    }

    #[test]
    fn adapter_read_write_and_memory_connections_have_explicit_two_second_timeout() -> Result<()> {
        fn check(db: &Connection) -> Result<()> {
            let timeout: i64 = db.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            assert_eq!(timeout, 2000);
            Ok(())
        }
        let (_temp, source, target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let planned = plan(&source, &target, &ids)?;
        for name in [STATE, GOALS] {
            check(&readonly(&source.join(name))?.context("source database")?)?;
        }
        let pending = prepare(&planned, false)?;
        assert_eq!(pending.len(), 2);
        for item in &pending {
            check(&item.db)?;
        }
        drop(pending);
        apply(&planned)?;
        let pending = prepare(&planned, true)?;
        for item in &pending {
            check(&item.db)?;
        }
        drop(pending);
        check(&with_busy_timeout(Connection::open_in_memory()?)?)?;
        Ok(())
    }

    #[test]
    fn adapter_waits_for_busy_writer_then_times_out_without_deleting_rows() -> Result<()> {
        let (_temp, source, target) = homes()?;
        let ids = versioned_fixture(&source)?;
        let planned = plan(&source, &target, &ids)?;
        apply(&planned)?;
        let writer = Connection::open(target.join(STATE))?;
        writer.execute_batch("BEGIN IMMEDIATE")?;
        let started = std::time::Instant::now();
        let error = remove_exact(&planned)
            .err()
            .context("competing writer did not cause timeout")?;
        let elapsed = started.elapsed();
        let sqlite = error
            .downcast_ref::<rusqlite::Error>()
            .context("expected SQLite busy error")?;
        assert_eq!(
            sqlite.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        );
        assert!(elapsed >= Duration::from_millis(1500));
        assert!(elapsed < Duration::from_secs(10));
        writer.execute_batch("ROLLBACK")?;
        verify(&planned)?;
        assert_eq!(threads(&target)?.len(), 2);
        remove_exact(&planned)?;
        assert!(threads(&target)?.is_empty());
        Ok(())
    }
}
