//! Synthetic-testable, reversible client wiring for the M8 cutover.
//!
//! This module deliberately keeps the client paths injectable.  The command
//! line uses the normal user locations, while tests use only temporary files.

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use toml_edit::DocumentMut;
use windows::Win32::{
    Foundation::{HLOCAL, LocalFree},
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};
use windows::core::PCWSTR;

const CODEX_BASE_URL: &str = "http://127.0.0.1:10200/v1";
const CLAUDE_BASE_URL: &str = "http://127.0.0.1:10200";
const JOURNAL_ENTROPY: &[u8] = b"quota-baton/v1/wiring-journal";

#[derive(Clone, Debug)]
pub struct CutoverPaths {
    pub qb_home: PathBuf,
    pub codex_auth: PathBuf,
    pub codex_session_auth: Vec<PathBuf>,
    pub codex_config: PathBuf,
    pub claude_settings: PathBuf,
    pub client_key: PathBuf,
    /// Injectable report roots; empty in synthetic tests to prevent ambient
    /// profile inspection.
    pub report_roots: Vec<PathBuf>,
}

impl CutoverPaths {
    pub fn from_environment(qb_home: PathBuf) -> Result<Self> {
        let user = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .context("USERPROFILE/HOME is missing")?;
        let client_key = qb_home.join("client.key");
        let codex_root = user.join(".codex");
        let codex_session_auth = discover_session_auth(&user);
        Ok(Self {
            qb_home,
            codex_auth: codex_root.join("auth.json"),
            codex_session_auth,
            codex_config: codex_root.join("config.toml"),
            claude_settings: user.join(".claude").join("settings.json"),
            client_key,
            report_roots: vec![
                user.join(".cli-proxy-api").join("auths"),
            ],
        })
    }

    fn journal(&self) -> PathBuf {
        self.qb_home.join("journal").join("wiring.json")
    }
}

fn discover_session_auth(user: &Path) -> Vec<PathBuf> {
    let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) else {
        return Vec::new();
    };
    let path = appdata.join("herdr").join("session-homes.json");
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    let mut homes = Vec::new();
    fn walk(value: &Value, homes: &mut Vec<PathBuf>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key.to_ascii_lowercase().contains("home")
                        && let Some(path) = value.as_str()
                    {
                        homes.push(PathBuf::from(path));
                    }
                    walk(value, homes);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, homes)),
            _ => {}
        }
    }
    walk(&value, &mut homes);
    homes
        .into_iter()
        .map(|home| {
            if home.is_absolute() {
                home
            } else {
                user.join(home)
            }
        })
        .map(|home| home.join("auth.json"))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn scan_other_clients_10100(session_auth: &[PathBuf]) -> Vec<String> {
    let Some(user) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    let mut roots = vec![user.join(".pi"), user.join(".omp"), user.join(".config")];
    roots.extend(
        session_auth
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf)),
    );
    for root in roots {
        if !root.exists() {
            continue;
        }
        let mut pending = vec![root];
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                if let Ok(entries) = fs::read_dir(&path) {
                    pending.extend(entries.flatten().map(|entry| entry.path()));
                }
                continue;
            }
            let Ok(text) = read_text(&path) else {
                continue;
            };
            for (line, value) in text.lines().enumerate() {
                if value.contains("127.0.0.1:10100") || value.contains("localhost:10100") {
                    warnings.push(format!("10100 참조: {}:{}", path.display(), line + 1));
                }
            }
        }
    }
    warnings
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    String::from_utf8(bytes.to_vec()).context("configuration is not UTF-8")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Original {
    present: bool,
    value: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    schema: u8,
    codex_config: String,
    codex_auth: String,
    claude_settings: String,
    codex_base_url: Original,
    codex_api_key: Original,
    codex_auth_key: SecretOriginal,
    #[serde(default)]
    codex_auth_mode: Original,
    codex_model_catalog: Original,
    claude_base_url: Original,
    claude_api_key: SecretOriginal,
    #[serde(default)]
    codex_config_before: String,
    #[serde(default)]
    codex_config_after: String,
    #[serde(default)]
    codex_auth_before: String,
    #[serde(default)]
    codex_auth_after: String,
    #[serde(default)]
    claude_settings_before: String,
    #[serde(default)]
    claude_settings_after: String,
    #[serde(default)]
    codex_sessions: Vec<SessionJournal>,
    #[serde(default)]
    codex_config_original: Option<String>,
    #[serde(default)]
    codex_auth_original: Option<String>,
    #[serde(default)]
    claude_settings_original: Option<String>,
    #[serde(default)]
    autostart: Option<OptionalActionJournal>,
    #[serde(default)]
    cpa_tasks: Vec<TaskJournal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SecretOriginal {
    present: bool,
    protected_b64: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionJournal {
    path: String,
    before: String,
    after: String,
    auth_mode: Original,
    api_key: SecretOriginal,
    #[serde(default)]
    original: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OptionalActionJournal {
    before: Original,
    after: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TaskJournal {
    name: String,
    was_enabled: bool,
    #[serde(default)]
    after_enabled: bool,
}

type CodexOriginals = (Original, Original, Original);
type ClaudeOriginals = (Original, SecretOriginal);

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub changed: Vec<String>,
    pub warnings: Vec<String>,
    pub diff: Vec<String>,
    pub report_only: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WireOptions {
    pub autostart: bool,
    pub disable_cpa_tasks: bool,
}

fn redacted_diff_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let replacement = if line.contains("<client key>") {
        Some("<client key>")
    } else if lower.contains("openai_api_key") || lower.contains("api_key") {
        Some("<previous key: redacted>")
    } else if lower.contains("anthropic_auth_token")
        || lower.contains("refresh_token")
        || lower.contains("access_token")
        || lower.contains("token")
    {
        Some("<token: redacted>")
    } else {
        None
    };
    if let Some(replacement) = replacement {
        let prefix = line.find(':').map(|index| &line[..index]).unwrap_or(line);
        return format!("{prefix}: {replacement}");
    }
    line.to_owned()
}

fn unified_diff(path: &Path, before: &[u8], after: &[u8]) -> Vec<String> {
    let before = String::from_utf8_lossy(before);
    let after = String::from_utf8_lossy(after);
    let before = before
        .trim_start_matches('\u{feff}')
        .lines()
        .collect::<Vec<_>>();
    let after = after
        .trim_start_matches('\u{feff}')
        .lines()
        .collect::<Vec<_>>();
    let mut diff = vec![
        format!("--- {} (현재)", path.display()),
        format!("+++ {} (연결 후)", path.display()),
        format!("@@ -1,{} +1,{} @@", before.len(), after.len()),
    ];
    for line in before {
        diff.push(format!("-{}", redacted_diff_line(line)));
    }
    for line in after {
        diff.push(format!("+{}", redacted_diff_line(line)));
    }
    diff
}

fn plaintext_report(roots: &[PathBuf]) -> Vec<String> {
    roots
        .iter()
        .map(|root| {
            let mut count = 0;
            let mut pending = vec![root.clone()];
            while let Some(path) = pending.pop() {
                if path.is_dir() {
                    if let Ok(entries) = fs::read_dir(&path) {
                        pending.extend(entries.flatten().map(|entry| entry.path()));
                    }
                } else if path.is_file() {
                    count += 1;
                }
            }
            format!(
                "평문 자격 증명 후보: {}개 (확인 경로: {})",
                count,
                root.display()
            )
        })
        .collect()
}

fn tray_autostart_command() -> Result<String> {
    let exe = std::env::current_exe()
        .context("현재 실행 파일 경로를 확인할 수 없습니다")?
        .with_file_name("quota-baton-tray.exe");
    Ok(format!("\"{}\"", exe.display()))
}

fn read_autostart() -> Result<Original> {
    let output = std::process::Command::new("reg.exe")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "quota-baton-tray",
        ])
        .output()
        .context("autostart 레지스트리를 읽지 못했습니다")?;
    if !output.status.success() {
        return Ok(Original::default());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let value = text
        .lines()
        .find(|line| line.contains("quota-baton-tray"))
        .and_then(|line| line.split("REG_SZ").nth(1))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Ok(Original {
        present: value.is_some(),
        value,
    })
}

fn set_autostart(action: &OptionalActionJournal) -> Result<()> {
    let status = std::process::Command::new("reg.exe")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "quota-baton-tray",
            "/t",
            "REG_SZ",
            "/d",
            &action.after,
            "/f",
        ])
        .status()
        .context("autostart 레지스트리를 변경하지 못했습니다")?;
    if !status.success() {
        bail!("autostart 레지스트리 변경을 거부했습니다")
    }
    Ok(())
}

fn restore_autostart(original: &Original) -> Result<()> {
    let mut command = std::process::Command::new("reg.exe");
    if original.present {
        command.args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "quota-baton-tray",
            "/t",
            "REG_SZ",
            "/d",
            original.value.as_deref().unwrap_or_default(),
            "/f",
        ]);
    } else {
        command.args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "quota-baton-tray",
            "/f",
        ]);
    }
    let status = command
        .status()
        .context("autostart를 되돌리지 못했습니다")?;
    if !status.success() {
        bail!("autostart 복구를 거부했습니다")
    }
    Ok(())
}

fn task_enabled(name: &str) -> Option<bool> {
    let output = std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", name, "/FO", "LIST"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if text.contains("disabled") {
        Some(false)
    } else if text.contains("ready") || text.contains("running") {
        Some(true)
    } else {
        None
    }
}

fn cpa_task_journal() -> Vec<TaskJournal> {
    ["cliproxyapi-proxy", "cliproxyapi-quota-watch"]
        .into_iter()
        .filter_map(|name| {
            task_enabled(name).map(|was_enabled| TaskJournal {
                name: name.into(),
                was_enabled,
                after_enabled: false,
            })
        })
        .collect()
}

fn disable_cpa_tasks(tasks: &[TaskJournal]) -> Result<()> {
    for task in tasks.iter().filter(|task| task.was_enabled) {
        let status = std::process::Command::new("schtasks.exe")
            .args(["/Change", "/TN", &task.name, "/DISABLE"])
            .status()
            .with_context(|| format!("{} 작업을 중지하지 못했습니다", task.name))?;
        if !status.success() {
            bail!("{} 작업 중지를 거부했습니다", task.name);
        }
    }
    Ok(())
}

fn restore_cpa_tasks(tasks: &[TaskJournal]) -> Result<()> {
    for task in tasks {
        let action = if task.was_enabled {
            "/ENABLE"
        } else {
            "/DISABLE"
        };
        let status = std::process::Command::new("schtasks.exe")
            .args(["/Change", "/TN", &task.name, action])
            .status()
            .with_context(|| format!("{} 작업을 되돌리지 못했습니다", task.name))?;
        if !status.success() {
            bail!("{} 작업 복구를 거부했습니다", task.name);
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub fn wire(paths: &CutoverPaths, apply: bool) -> Result<Report> {
    wire_with_options(paths, apply, WireOptions::default())
}

pub fn wire_with_options(
    paths: &CutoverPaths,
    apply: bool,
    options: WireOptions,
) -> Result<Report> {
    if paths.journal().exists() {
        bail!("wiring journal already exists; run unwire first")
    }
    let key = if apply {
        Some(read_key(&paths.client_key)?)
    } else {
        None
    };
    let (codex, codex_auth, codex_auth_mode, claude) = read_inputs(paths)?;
    let codex_config_before = hash_file(&paths.codex_config)?;
    let codex_auth_before = hash_file(&paths.codex_auth)?;
    let claude_settings_before = hash_file(&paths.claude_settings)?;
    let preview_key = key.as_deref().unwrap_or("<client key>");
    let codex_config_preview = codex_config_bytes(&paths.codex_config, CODEX_BASE_URL)?;
    let codex_auth_preview = codex_auth_bytes(&paths.codex_auth, preview_key)?;
    let claude_preview = claude_bytes(&paths.claude_settings, CLAUDE_BASE_URL, preview_key)?;
    let codex_config_after = hash_bytes(&preserved_bytes(
        &paths.codex_config,
        &codex_config_preview,
    )?);
    let codex_auth_after = hash_bytes(&preserved_bytes(&paths.codex_auth, &codex_auth_preview)?);
    let claude_settings_after =
        hash_bytes(&preserved_bytes(&paths.claude_settings, &claude_preview)?);
    let mut codex_sessions = Vec::new();
    let mut session_diffs = Vec::new();
    for path in &paths.codex_session_auth {
        if path.exists()
            && let Some(session) = session_journal(path, preview_key)?
        {
            let before = fs::read(path)?;
            let after = codex_auth_bytes(path, preview_key)?;
            session_diffs.extend(unified_diff(path, &before, &after));
            codex_sessions.push(session);
        }
    }
    let autostart = if options.autostart {
        Some(OptionalActionJournal {
            before: read_autostart()?,
            after: tray_autostart_command()?,
        })
    } else {
        None
    };
    let cpa_tasks = if options.disable_cpa_tasks {
        cpa_task_journal()
    } else {
        Vec::new()
    };
    let journal = Journal {
        schema: 1,
        codex_config: paths.codex_config.display().to_string(),
        codex_auth: paths.codex_auth.display().to_string(),
        claude_settings: paths.claude_settings.display().to_string(),
        codex_base_url: codex.0.clone(),
        codex_api_key: codex.1.clone(),
        codex_auth_key: codex_auth.clone(),
        codex_auth_mode,
        codex_model_catalog: codex.2.clone(),
        claude_base_url: claude.0.clone(),
        claude_api_key: claude.1.clone(),
        codex_config_before,
        codex_config_after,
        codex_auth_before,
        codex_auth_after,
        claude_settings_before,
        claude_settings_after,
        codex_sessions,
        codex_config_original: Some(protect(&fs::read(&paths.codex_config)?)?),
        codex_auth_original: Some(protect(&fs::read(&paths.codex_auth)?)?),
        claude_settings_original: Some(protect(&fs::read(&paths.claude_settings)?)?),
        autostart: autostart.clone(),
        cpa_tasks: cpa_tasks.clone(),
    };
    let mut warnings = paths
        .codex_session_auth
        .iter()
        .filter_map(|path| {
            if !path.exists() {
                return Some(format!(
                    "Codex 세션 홈에 auth.json이 없어 변경하지 않습니다. 재개 시 401이 될 수 있으며 사용자 결정이 필요합니다: {}",
                    path.display()
                ));
            }
            let auth = read_text(path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
            (auth.get("auth_mode").and_then(Value::as_str) != Some("apikey")).then(|| {
                format!(
                    "Codex 세션 홈은 ChatGPT 인증이라 변경하지 않습니다. 재개 시 401이 될 수 있으며 사용자 결정이 필요합니다: {}",
                    path.display()
                )
            })
        })
        .collect::<Vec<_>>();
    warnings.extend(scan_other_clients_10100(&paths.codex_session_auth));
    let mut changed = vec![
        "codex.openai_base_url".into(),
        "codex.auth.json OPENAI_API_KEY".into(),
        "codex.model_catalog_json (remove)".into(),
        "claude.env.ANTHROPIC_BASE_URL".into(),
        "claude.env.ANTHROPIC_AUTH_TOKEN".into(),
    ];
    if options.autostart {
        changed.push(
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\\quota-baton-tray".into(),
        );
    }
    if options.disable_cpa_tasks {
        changed.extend(
            cpa_tasks
                .iter()
                .map(|task| format!("schtasks:{}", task.name)),
        );
    }
    let report = Report {
        changed,
        warnings,
        diff: unified_diff(
            &paths.codex_config,
            &fs::read(&paths.codex_config)?,
            &codex_config_preview,
        )
        .into_iter()
        .chain(unified_diff(
            &paths.codex_auth,
            &fs::read(&paths.codex_auth)?,
            &codex_auth_preview,
        ))
        .chain(unified_diff(
            &paths.claude_settings,
            &fs::read(&paths.claude_settings)?,
            &claude_preview,
        ))
        .chain(session_diffs)
        .collect(),
        report_only: plaintext_report(&paths.report_roots),
    };
    if !apply {
        return Ok(report);
    }

    let key = key.expect("apply key");
    atomic_json(&paths.journal(), &journal)?;
    // The journal is intentionally retained if either write fails: it is the
    // recovery record for a partially completed cutover.
    apply_codex(&paths.codex_config, &paths.codex_auth, CODEX_BASE_URL, &key).and_then(|_| {
        for session in &journal.codex_sessions {
            apply_session(Path::new(&session.path), &key)?;
        }
        apply_claude(&paths.claude_settings, CLAUDE_BASE_URL, &key)
    })?;
    if options.autostart {
        set_autostart(&autostart.expect("autostart journal"))?;
    }
    if options.disable_cpa_tasks {
        disable_cpa_tasks(&cpa_tasks)?;
    }
    Ok(report)
}

pub fn unwire(paths: &CutoverPaths, apply: bool, force: bool) -> Result<Report> {
    let journal: Journal =
        serde_json::from_slice(&fs::read(paths.journal()).context("read wiring journal")?)
            .context("parse wiring journal")?;
    if journal.codex_config != paths.codex_config.display().to_string()
        || journal.codex_auth != paths.codex_auth.display().to_string()
        || journal.claude_settings != paths.claude_settings.display().to_string()
    {
        bail!("wiring journal paths do not match this home")
    }
    let report = Report {
        changed: vec![
            "codex.openai_base_url".into(),
            "codex.auth.json OPENAI_API_KEY".into(),
            "codex.model_catalog_json (restore)".into(),
            "claude.env.ANTHROPIC_BASE_URL".into(),
            "claude.env.ANTHROPIC_AUTH_TOKEN".into(),
        ],
        warnings: Vec::new(),
        diff: Vec::new(),
        report_only: Vec::new(),
    };
    if !apply {
        return Ok(report);
    }
    if !force {
        for (path, expected) in [
            (&paths.codex_config, &journal.codex_config_after),
            (&paths.codex_auth, &journal.codex_auth_after),
            (&paths.claude_settings, &journal.claude_settings_after),
        ] {
            if !expected.is_empty() && hash_file(path)? != *expected {
                bail!("파일이 변경되어 되돌릴 수 없습니다: {}", path.display());
            }
        }
        for session in &journal.codex_sessions {
            let path = Path::new(&session.path);
            if hash_file(path)? != session.after {
                bail!("파일이 변경되어 되돌릴 수 없습니다: {}", path.display());
            }
        }
        if let Some(autostart) = &journal.autostart {
            let current = read_autostart()?;
            if current.value != Some(autostart.after.clone()) {
                bail!("autostart 설정이 변경되어 되돌릴 수 없습니다");
            }
        }
        for task in &journal.cpa_tasks {
            if task_enabled(&task.name) != Some(task.after_enabled) {
                bail!("{} 예약 작업이 변경되어 되돌릴 수 없습니다", task.name);
            }
        }
    }
    if !force
        && let (Some(config), Some(auth), Some(claude)) = (
            journal.codex_config_original.as_deref(),
            journal.codex_auth_original.as_deref(),
            journal.claude_settings_original.as_deref(),
        )
    {
        restore_exact(&paths.codex_config, config)?;
        restore_exact(&paths.codex_auth, auth)?;
        restore_exact(&paths.claude_settings, claude)?;
        for session in &journal.codex_sessions {
            if let Some(original) = session.original.as_deref() {
                restore_exact(Path::new(&session.path), original)?;
            } else {
                restore_session(Path::new(&session.path), session)?;
            }
        }
    } else {
        restore_codex(&paths.codex_config, &paths.codex_auth, &journal)?;
        for session in &journal.codex_sessions {
            restore_session(Path::new(&session.path), session)?;
        }
        restore_claude(&paths.claude_settings, &journal)?;
    }
    if let Some(autostart) = &journal.autostart {
        restore_autostart(&autostart.before)?;
    }
    restore_cpa_tasks(&journal.cpa_tasks)?;
    fs::remove_file(paths.journal()).context("remove wiring journal")?;
    Ok(report)
}

fn read_inputs(
    paths: &CutoverPaths,
) -> Result<(CodexOriginals, SecretOriginal, Original, ClaudeOriginals)> {
    let codex_text = read_text(&paths.codex_config).context("read Codex config")?;
    let doc = codex_text
        .parse::<DocumentMut>()
        .context("parse Codex config")?;
    let codex = (
        original_toml(doc.get("openai_base_url")),
        original_toml(doc.get("openai_api_key")),
        original_toml(doc.get("model_catalog_json")),
    );
    let auth_text = read_text(&paths.codex_auth).context("read Codex auth")?;
    let auth: Value = serde_json::from_str(&auth_text).context("parse Codex auth")?;
    let codex_auth = secret_json(auth.get("OPENAI_API_KEY"))?;
    let codex_auth_mode = original_json(auth.get("auth_mode"));
    let claude_text = read_text(&paths.claude_settings).context("read Claude settings")?;
    let json: Value = serde_json::from_str(&claude_text).context("parse Claude settings")?;
    let env = json.get("env").and_then(Value::as_object);
    let claude = (
        original_json(env.and_then(|m| m.get("ANTHROPIC_BASE_URL"))),
        secret_json(env.and_then(|m| m.get("ANTHROPIC_AUTH_TOKEN")))?,
    );
    Ok((codex, codex_auth, codex_auth_mode, claude))
}

fn original_toml(item: Option<&toml_edit::Item>) -> Original {
    item.filter(|v| !v.is_none())
        .map(|v| Original {
            present: true,
            value: v.as_str().map(str::to_owned),
        })
        .unwrap_or(Original {
            present: false,
            value: None,
        })
}
fn original_json(value: Option<&Value>) -> Original {
    value
        .map(|v| Original {
            present: true,
            value: v.as_str().map(str::to_owned),
        })
        .unwrap_or(Original {
            present: false,
            value: None,
        })
}
fn secret_json(value: Option<&Value>) -> Result<SecretOriginal> {
    let Some(value) = value else {
        return Ok(SecretOriginal {
            present: false,
            protected_b64: None,
        });
    };
    let text = value.as_str().context("secret setting is not a string")?;
    Ok(SecretOriginal {
        present: true,
        protected_b64: Some(protect(text.as_bytes())?),
    })
}
fn read_key(path: &Path) -> Result<String> {
    let key = fs::read_to_string(path).context("read client key")?;
    let key = key.trim();
    if key.is_empty() {
        bail!("client key is empty")
    }
    Ok(key.to_owned())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash_file(path: &Path) -> Result<String> {
    Ok(hash_bytes(
        &fs::read(path).with_context(|| format!("read {}", path.display()))?,
    ))
}

fn apply_codex(path: &Path, auth_path: &Path, base: &str, key: &str) -> Result<()> {
    atomic_preserved(path, &codex_config_bytes(path, base)?)?;
    atomic_preserved(auth_path, &codex_auth_bytes(auth_path, key)?)
}
fn codex_config_bytes(path: &Path, base: &str) -> Result<Vec<u8>> {
    let mut doc = read_text(path)?
        .parse::<DocumentMut>()
        .context("parse Codex config")?;
    doc["openai_base_url"] = toml_edit::value(base);
    doc.remove("model_catalog_json");
    Ok(doc.to_string().into_bytes())
}
fn codex_auth_bytes(path: &Path, key: &str) -> Result<Vec<u8>> {
    let mut auth: Value = serde_json::from_str(&read_text(path)?)?;
    let root = auth
        .as_object_mut()
        .context("Codex auth root is not an object")?;
    root.insert("auth_mode".into(), Value::String("apikey".into()));
    root.insert("OPENAI_API_KEY".into(), Value::String(key.into()));
    Ok(serde_json::to_vec_pretty(&auth)?)
}
fn session_journal(path: &Path, key: &str) -> Result<Option<SessionJournal>> {
    let auth: Value = serde_json::from_str(&read_text(path)?)?;
    if auth.get("auth_mode").and_then(Value::as_str) != Some("apikey") {
        return Ok(None);
    }
    let mode = original_json(auth.get("auth_mode"));
    let api_key = secret_json(auth.get("OPENAI_API_KEY"))?;
    let before = hash_file(path)?;
    let after = hash_bytes(&preserved_bytes(path, &codex_auth_bytes(path, key)?)?);
    Ok(Some(SessionJournal {
        path: path.display().to_string(),
        before,
        after,
        auth_mode: mode,
        api_key,
        original: Some(protect(&fs::read(path)?)?),
    }))
}
fn apply_session(path: &Path, key: &str) -> Result<()> {
    atomic_preserved(path, &codex_auth_bytes(path, key)?)
}
fn restore_session(path: &Path, journal: &SessionJournal) -> Result<()> {
    let mut auth: Value = serde_json::from_str(&read_text(path)?)?;
    let root = auth
        .as_object_mut()
        .context("Codex session auth root is not an object")?;
    restore_secret_json(root, "OPENAI_API_KEY", &journal.api_key)?;
    restore_json(root, "auth_mode", &journal.auth_mode);
    atomic_preserved(path, &serde_json::to_vec_pretty(&auth)?)
}

fn restore_exact(path: &Path, protected: &str) -> Result<()> {
    atomic_bytes(path, &unprotect(protected)?)
}
fn restore_codex(path: &Path, auth_path: &Path, journal: &Journal) -> Result<()> {
    let mut doc = read_text(path)?
        .parse::<DocumentMut>()
        .context("parse Codex config")?;
    restore_toml(&mut doc, "openai_base_url", &journal.codex_base_url);
    restore_toml(&mut doc, "model_catalog_json", &journal.codex_model_catalog);
    atomic_preserved(path, doc.to_string().as_bytes())?;
    let mut auth: Value = serde_json::from_str(&read_text(auth_path)?)?;
    let root = auth
        .as_object_mut()
        .context("Codex auth root is not an object")?;
    restore_secret_json(root, "OPENAI_API_KEY", &journal.codex_auth_key)?;
    restore_json(root, "auth_mode", &journal.codex_auth_mode);
    atomic_preserved(auth_path, &serde_json::to_vec_pretty(&auth)?)
}
fn restore_toml(doc: &mut DocumentMut, key: &str, original: &Original) {
    if original.present {
        doc[key] = toml_edit::value(original.value.as_deref().unwrap_or_default());
    } else {
        doc.remove(key);
    }
}
fn apply_claude(path: &Path, base: &str, key: &str) -> Result<()> {
    atomic_preserved(path, &claude_bytes(path, base, key)?)
}
fn claude_bytes(path: &Path, base: &str, key: &str) -> Result<Vec<u8>> {
    let mut json: Value = serde_json::from_str(&read_text(path)?)?;
    let env = json
        .as_object_mut()
        .context("Claude settings root is not an object")?
        .entry("env")
        .or_insert_with(|| Value::Object(Default::default()));
    let env = env
        .as_object_mut()
        .context("Claude settings env is not an object")?;
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(base.into()));
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), Value::String(key.into()));
    Ok(serde_json::to_vec_pretty(&json)?)
}
fn restore_claude(path: &Path, journal: &Journal) -> Result<()> {
    let mut json: Value = serde_json::from_str(&read_text(path)?)?;
    let root = json
        .as_object_mut()
        .context("Claude settings root is not an object")?;
    let env = root
        .entry("env")
        .or_insert_with(|| Value::Object(Default::default()));
    let env = env
        .as_object_mut()
        .context("Claude settings env is not an object")?;
    restore_json(env, "ANTHROPIC_BASE_URL", &journal.claude_base_url);
    restore_secret_json(env, "ANTHROPIC_AUTH_TOKEN", &journal.claude_api_key)?;
    if env.is_empty() {
        root.remove("env");
    }
    atomic_preserved(path, &serde_json::to_vec_pretty(&json)?)
}
fn restore_json(env: &mut serde_json::Map<String, Value>, key: &str, original: &Original) {
    if original.present {
        env.insert(
            key.into(),
            Value::String(original.value.clone().unwrap_or_default()),
        );
    } else {
        env.remove(key);
    }
}
fn restore_secret_json(
    env: &mut serde_json::Map<String, Value>,
    key: &str,
    original: &SecretOriginal,
) -> Result<()> {
    if original.present {
        let encoded = original
            .protected_b64
            .as_deref()
            .context("missing encrypted secret")?;
        let value = String::from_utf8(unprotect(encoded)?).context("invalid encrypted secret")?;
        env.insert(key.into(), Value::String(value));
    } else {
        env.remove(key);
    }
    Ok(())
}

fn protect(input: &[u8]) -> Result<String> {
    transform(input, true).map(|bytes| STANDARD.encode(bytes))
}

fn unprotect(encoded: &str) -> Result<Vec<u8>> {
    let bytes = STANDARD
        .decode(encoded)
        .context("invalid encrypted journal value")?;
    transform(&bytes, false)
}

fn transform(input: &[u8], protect: bool) -> Result<Vec<u8>> {
    let mut input = input.to_vec();
    let data = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_mut_ptr(),
    };
    let mut entropy = JOURNAL_ENTROPY.to_vec();
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let result = unsafe {
        if protect {
            CryptProtectData(
                &data,
                PCWSTR::null(),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &data,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    result.context("DPAPI journal transform failed")?;
    let copied =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
    }
    Ok(copied)
}

fn atomic_preserved(path: &Path, bytes: &[u8]) -> Result<()> {
    let output = preserved_bytes(path, bytes)?;
    atomic_bytes(path, &output)
}

fn preserved_bytes(path: &Path, bytes: &[u8]) -> Result<Vec<u8>> {
    let original = fs::read(path).unwrap_or_default();
    let bom = original.starts_with(&[0xef, 0xbb, 0xbf]);
    let crlf = original.windows(2).any(|pair| pair == b"\r\n");
    let mut text = String::from_utf8(bytes.to_vec()).context("configuration is not UTF-8")?;
    if crlf {
        text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    let mut output = Vec::with_capacity(text.len() + usize::from(bom) * 3);
    if bom {
        output.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    output.extend_from_slice(text.as_bytes());
    Ok(output)
}

fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_bytes(path, serde_json::to_vec_pretty(value)?.as_slice())
}
fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    for attempt in 0..5 {
        let result = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, path)
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 4 => thread::sleep(Duration::from_millis(20 * (attempt + 1))),
            Err(error) => {
                return Err(error).with_context(|| format!("atomic write {}", path.display()));
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn paths() -> (tempfile::TempDir, CutoverPaths) {
        let dir = tempdir().unwrap();
        let codex = dir.path().join("codex.toml");
        let auth = dir.path().join("auth.json");
        let claude = dir.path().join("settings.json");
        fs::write(&codex, "openai_base_url = 'old-url'\nkeep = 'yes'\n").unwrap();
        fs::write(
            &auth,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"old-key"}"#,
        )
        .unwrap();
        fs::write(
            &claude,
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"old-key"},"keep":true}"#,
        )
        .unwrap();
        let qb_home = dir.path().join("qb");
        let client_key = dir.path().join("key");
        (
            dir,
            CutoverPaths {
                qb_home,
                codex_auth: auth,
                codex_session_auth: Vec::new(),
                codex_config: codex,
                claude_settings: claude,
                client_key,
                report_roots: Vec::new(),
            },
        )
    }

    #[test]
    fn dry_run_reads_and_writes_nothing_or_secrets() {
        let (_dir, p) = paths();
        let before = fs::read(&p.codex_config).unwrap();
        let _report = wire(&p, false).unwrap();
        assert_eq!(before, fs::read(&p.codex_config).unwrap());
        assert!(!p.journal().exists());
    }

    #[test]
    fn wire_and_unwire_restore_only_target_keys() {
        let (_dir, p) = paths();
        let original_codex = fs::read(&p.codex_config).unwrap();
        let original_claude = fs::read(&p.claude_settings).unwrap();
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        wire(&p, true).unwrap();
        assert!(
            fs::read_to_string(&p.codex_config)
                .unwrap()
                .contains("127.0.0.1:10200/v1")
        );
        assert!(
            fs::read_to_string(&p.claude_settings)
                .unwrap()
                .contains("synthetic-key")
        );
        unwire(&p, true, false).unwrap();
        assert_eq!(original_codex, fs::read(&p.codex_config).unwrap());
        assert_eq!(original_claude, fs::read(&p.claude_settings).unwrap());
        assert!(!p.journal().exists());
    }

    #[test]
    fn bom_prefixed_client_files_are_read_without_exposing_the_bom_to_parsers() {
        let (_dir, p) = paths();
        let codex = fs::read(&p.codex_config).unwrap();
        let auth = fs::read(&p.codex_auth).unwrap();
        let claude = fs::read(&p.claude_settings).unwrap();
        fs::write(
            &p.codex_config,
            [b"\xef\xbb\xbf".as_slice(), codex.as_slice()].concat(),
        )
        .unwrap();
        fs::write(
            &p.codex_auth,
            [b"\xef\xbb\xbf".as_slice(), auth.as_slice()].concat(),
        )
        .unwrap();
        fs::write(
            &p.claude_settings,
            [b"\xef\xbb\xbf".as_slice(), claude.as_slice()].concat(),
        )
        .unwrap();
        assert!(wire(&p, false).is_ok());
    }

    #[test]
    fn chatgpt_session_is_untouched_and_diff_redacts_secrets() {
        let (dir, mut p) = paths();
        let session = dir.path().join("chatgpt-auth.json");
        fs::write(
            &session,
            br#"{"auth_mode":"chatgpt","OPENAI_API_KEY":"synthetic-chatgpt"}"#,
        )
        .unwrap();
        let before = fs::read(&session).unwrap();
        p.codex_session_auth.push(session.clone());
        let report = wire(&p, false).unwrap();
        assert_eq!(before, fs::read(&session).unwrap());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("사용자 결정"))
        );
        assert!(
            report
                .diff
                .iter()
                .all(|line| !line.contains("synthetic-chatgpt"))
        );
    }

    #[test]
    fn apply_unwire_restores_bom_and_line_endings_byte_for_byte() {
        let (_dir, p) = paths();
        let originals = [
            (
                &p.codex_config,
                b"\xef\xbb\xbfopenai_base_url = 'old-url'\r\nkeep = 'yes'\r\n".as_slice(),
            ),
            (
                &p.codex_auth,
                b"\xef\xbb\xbf{\r\n  \"auth_mode\": \"apikey\",\r\n  \"OPENAI_API_KEY\": \"old-key\"\r\n}\r\n".as_slice(),
            ),
            (
                &p.claude_settings,
                b"\xef\xbb\xbf{\r\n  \"env\": {\r\n    \"ANTHROPIC_AUTH_TOKEN\": \"old-key\"\r\n  },\r\n  \"keep\": true\r\n}\r\n".as_slice(),
            ),
        ];
        for (path, bytes) in originals {
            fs::write(path, bytes).unwrap();
        }
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        let before = [
            fs::read(&p.codex_config).unwrap(),
            fs::read(&p.codex_auth).unwrap(),
            fs::read(&p.claude_settings).unwrap(),
        ];
        wire(&p, true).unwrap();
        unwire(&p, true, false).unwrap();
        assert_eq!(before[0], fs::read(&p.codex_config).unwrap());
        assert_eq!(before[1], fs::read(&p.codex_auth).unwrap());
        assert_eq!(before[2], fs::read(&p.claude_settings).unwrap());
    }
}
