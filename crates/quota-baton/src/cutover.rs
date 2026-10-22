//! Synthetic-testable, reversible client wiring for the M8 cutover.
//!
//! This module deliberately keeps the client paths injectable.  The command
//! line uses the normal user locations, while tests use only temporary files.

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use qb_core::accounts::{AccountStore, ImportMode, Source, UserState};
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
    Foundation::{ERROR_FILE_NOT_FOUND, HLOCAL, LocalFree},
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
    System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE, RegCloseKey,
        RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    },
};
use windows::core::PCWSTR;

#[path = "dedicated.rs"]
mod dedicated;

const JOURNAL_ENTROPY: &[u8] = b"quota-baton/v1/wiring-journal";

/// Only fixed stages cross the CLI error boundary; underlying values stay private.
#[derive(Debug)]
pub(crate) struct WireStage(pub(crate) &'static str);

impl std::fmt::Display for WireStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "연결 계획을 확인하지 못했습니다 (wire:{}).", self.0)
    }
}
impl std::error::Error for WireStage {}

#[derive(Clone, Debug)]
pub struct CutoverPaths {
    pub qb_home: PathBuf,
    pub codex_auth: PathBuf,
    pub codex_session_auth: Vec<PathBuf>,
    codex_session_homes: Vec<SessionHome>,
    pub session_homes_file: Option<PathBuf>,
    session_homes_warning: Option<String>,
    pub codex_config: PathBuf,
    pub codex_qb_home: PathBuf,
    pub npm_dir: PathBuf,
    pub claude_settings: PathBuf,
    pub claude_command: Option<PathBuf>,
    pub client_key: PathBuf,
    /// Realtime endpoints intentionally left on CPA because quota-baton does
    /// not serve them.
    codex_realtime_items: Vec<String>,
    /// Injectable report roots; empty in synthetic tests to prevent ambient
    /// profile inspection.
    pub report_roots: Vec<PathBuf>,
    scan_roots: Vec<(String, PathBuf)>,
}

impl CutoverPaths {
    fn npm_dir_for(user: &Path, appdata: Option<PathBuf>) -> PathBuf {
        appdata
            .unwrap_or_else(|| user.join("AppData").join("Roaming"))
            .join("npm")
    }

    pub fn from_environment(qb_home: PathBuf) -> Result<Self> {
        let user = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .context("USERPROFILE 또는 HOME 환경 변수가 없습니다")?;
        let client_key = qb_home.join("client.key");
        let codex_root = user.join(".codex");
        let (session_homes_file, codex_session_homes, session_homes_warning) =
            discover_session_homes(&user);
        let codex_session_auth = codex_session_homes
            .iter()
            .map(|home| home.auth.clone())
            .collect();
        Ok(Self {
            qb_home,
            codex_auth: codex_root.join("auth.json"),
            codex_session_auth,
            codex_session_homes: codex_session_homes.clone(),
            session_homes_file,
            session_homes_warning,
            codex_config: codex_root.join("config.toml"),
            codex_qb_home: user.join(".codex-acct").join("qb"),
            npm_dir: Self::npm_dir_for(&user, std::env::var_os("APPDATA").map(PathBuf::from)),
            codex_realtime_items: realtime_cpa_items(&codex_root.join("config.toml")),
            claude_command: resolve_claude_command(
                std::env::var_os("PATH").as_deref(),
                std::env::var_os("PATHEXT").as_deref(),
            ),
            claude_settings: user.join(".claude").join("settings.json"),
            client_key,
            report_roots: {
                let cpa = user.join(".cli-proxy-api");
                let mut roots = vec![
                    cpa.join("auths"),
                    cpa.join("backups"),
                    cpa.join("client-api-key.txt"),
                ];
                if let Ok(entries) = fs::read_dir(&cpa) {
                    roots.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("config.yaml.bak-"))
                    }));
                }
                roots
            },
            scan_roots: {
                let mut roots = vec![
                    ("pi".into(), user.join(".pi")),
                    ("omp".into(), user.join(".omp")),
                    ("config".into(), user.join(".config")),
                    ("codex".into(), codex_root.clone()),
                ];
                roots.extend(
                    codex_session_homes
                        .iter()
                        .map(|home| ("session".into(), home.home.clone())),
                );
                roots
            },
        })
    }

    fn journal(&self) -> PathBuf {
        self.qb_home.join("journal").join("wiring.json")
    }
}

#[derive(Clone, Debug)]
struct SessionHome {
    home: PathBuf,
    auth: PathBuf,
    sessions: usize,
    shim_exempt: bool,
}

fn session_home_auth_mode(home: &SessionHome) -> Option<String> {
    read_text(&home.auth)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|auth| {
            auth.get("auth_mode")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn session_home_is_detachable(home: &SessionHome) -> bool {
    !home.shim_exempt && session_home_auth_mode(home).as_deref() != Some("apikey")
}

fn realtime_cpa_items(path: &Path) -> Vec<String> {
    let Ok(text) = read_text(path) else {
        return Vec::new();
    };
    [
        "experimental_realtime_ws_base_url",
        "experimental_realtime_webrtc_call_base_url",
    ]
    .into_iter()
    .filter(|key| {
        text.lines().any(|line| {
            line.contains(key)
                && (line.contains("127.0.0.1:10100") || line.contains("localhost:10100"))
        })
    })
    .map(str::to_owned)
    .collect()
}

fn normal_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn session_homes_from_value(value: &Value, user: &Path) -> Vec<SessionHome> {
    let mut homes = std::collections::BTreeMap::<String, (PathBuf, usize)>::new();
    fn add_home(
        homes: &mut std::collections::BTreeMap<String, (PathBuf, usize)>,
        user: &Path,
        path: PathBuf,
    ) {
        let path = if path.is_absolute() {
            path
        } else {
            user.join(path)
        };
        let key = normal_path(&path);
        homes
            .entry(key)
            .and_modify(|(_, sessions)| *sessions += 1)
            .or_insert((path, 1));
    }
    if let Some(sessions) = value.get("sessions").and_then(Value::as_object) {
        for home in sessions.values().filter_map(Value::as_str) {
            add_home(&mut homes, user, PathBuf::from(home));
        }
    }
    fn walk(
        value: &Value,
        homes: &mut std::collections::BTreeMap<String, (PathBuf, usize)>,
        user: &Path,
    ) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key.to_ascii_lowercase().contains("home")
                        && let Some(path) = value.as_str()
                    {
                        add_home(homes, user, PathBuf::from(path));
                    }
                    walk(value, homes, user);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, homes, user)),
            _ => {}
        }
    }
    if homes.is_empty() {
        walk(value, &mut homes, user);
    }
    let default_home = normal_path(&user.join(".codex"));
    homes
        .into_iter()
        .map(|(key, (home, sessions))| SessionHome {
            shim_exempt: key.contains("\\.codex-acct\\"),
            auth: home.join("auth.json"),
            home,
            sessions,
        })
        .filter(|home| normal_path(&home.home) != default_home)
        .collect()
}

fn discover_session_homes(user: &Path) -> (Option<PathBuf>, Vec<SessionHome>, Option<String>) {
    let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) else {
        return (
            None,
            Vec::new(),
            Some("APPDATA가 없어 session-homes.json을 읽지 못했습니다.".into()),
        );
    };
    let path = appdata.join("herdr").join("session-homes.json");
    let Ok(bytes) = fs::read(&path) else {
        return (
            Some(path.clone()),
            Vec::new(),
            Some(format!(
                "session-homes.json을 읽지 못했습니다: {}",
                shorten_path(&path)
            )),
        );
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return (
            Some(path.clone()),
            Vec::new(),
            Some(format!(
                "session-homes.json 형식이 올바르지 않습니다: {}",
                shorten_path(&path)
            )),
        );
    };
    (Some(path), session_homes_from_value(&value, user), None)
}

fn scan_other_clients_10100(roots: &[(String, PathBuf)]) -> Vec<String> {
    let mut groups = std::collections::BTreeMap::<String, Vec<String>>::new();
    for (client, root) in roots {
        if !root.exists() {
            continue;
        }
        let mut pending = vec![(root.clone(), 0usize)];
        while let Some((path, depth)) = pending.pop() {
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if path.is_dir() {
                if depth < 4
                    && let Ok(entries) = fs::read_dir(&path)
                {
                    pending.extend(entries.flatten().map(|entry| (entry.path(), depth + 1)));
                }
                continue;
            }
            if metadata.len() > 1024 * 1024 {
                continue;
            }
            let components = path
                .components()
                .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
                .collect::<Vec<_>>();
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let config_extension = matches!(
                extension.as_str(),
                "json" | "yaml" | "yml" | "toml" | "ini" | "conf" | "ps1"
            );
            if components.iter().any(|component| {
                component == "sessions" || component == "logs" || component.contains(".bak")
            }) || extension == "log"
                || extension == "jsonl"
                || !config_extension
            {
                continue;
            }
            let Ok(text) = read_text(&path) else {
                continue;
            };
            for (line, value) in text.lines().enumerate() {
                if value.contains("127.0.0.1:10100") || value.contains("localhost:10100") {
                    let file = path
                        .file_name()
                        .and_then(|file| file.to_str())
                        .unwrap_or("config");
                    groups
                        .entry(format!("{client}: {file}"))
                        .or_default()
                        .push(format!("{}:{}", shorten_path(&path), line + 1));
                }
            }
        }
    }
    groups
        .into_iter()
        .map(|(group, hits)| format!("10100 참조 {group} ({}곳): {}", hits.len(), hits.join(", ")))
        .collect()
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    String::from_utf8(bytes.to_vec()).context("설정 파일이 UTF-8이 아닙니다")
}

fn shorten_path(path: &Path) -> String {
    let user = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    if let Some(user) = user
        && let Ok(relative) = path.strip_prefix(&user)
    {
        return format!("~\\{}", relative.display());
    }
    path.display().to_string()
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
    #[serde(default)]
    providers: Vec<ProviderJournal>,
    #[serde(default)]
    session_homes_path: Option<String>,
    #[serde(default)]
    session_homes_before: Option<String>,
    #[serde(default)]
    session_homes_after: Option<String>,
    #[serde(default)]
    session_homes_original: Option<String>,
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
    #[serde(default)]
    before_type: u32,
    after: String,
    #[serde(default)]
    after_type: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TaskJournal {
    name: String,
    was_enabled: bool,
    #[serde(default)]
    after_enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProviderJournal {
    name: String,
    base_url: Original,
    model_catalog_url: Original,
    experimental_bearer_token: SecretOriginal,
}

type CodexOriginals = (Original, Original, Original);
type ClaudeOriginals = (Original, SecretOriginal);

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub summary: Vec<String>,
    pub changed: Vec<String>,
    pub warnings: Vec<String>,
    pub diff: Vec<String>,
    pub report_only: Vec<String>,
}

/// CLI-independent mode; main may parse this through `FromStr`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexMode {
    Default,
    Dedicated,
    None,
}

impl std::str::FromStr for CodexMode {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "default" => Ok(Self::Default),
            "dedicated" => Ok(Self::Dedicated),
            "none" => Ok(Self::None),
            _ => bail!("Codex 모드는 default, dedicated, none 중 하나여야 합니다"),
        }
    }
}

/// The serde default is legacy; fresh wire defaults to Dedicated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeMode {
    Default,
    #[default]
    Dedicated,
    None,
}
fn legacy_claude_mode() -> ClaudeMode {
    ClaudeMode::Default
}
impl std::str::FromStr for ClaudeMode {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "default" => Ok(Self::Default),
            "dedicated" => Ok(Self::Dedicated),
            "none" => Ok(Self::None),
            _ => bail!("Claude 모드는 default, dedicated, none 중 하나여야 합니다"),
        }
    }
}
pub fn resolve_claude_mode(paths: &CutoverPaths, options: &WireOptions) -> Result<ClaudeMode> {
    if let Some(mode) = options.claude_mode {
        return Ok(mode);
    }
    match fs::read(paths.journal()) {
        Ok(bytes) => {
            #[derive(Deserialize)]
            struct Header {
                schema: u8,
                #[serde(default = "legacy_claude_mode")]
                claude_mode: ClaudeMode,
            }
            let invalid = || anyhow::anyhow!("연결 journal 형식이 올바르지 않습니다");
            let header: Header = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
            match header.schema {
                1 => Ok(ClaudeMode::Default),
                3 => Ok(header.claude_mode),
                _ => Err(invalid()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ClaudeMode::Dedicated),
        Err(_) => bail!("연결 journal을 읽지 못했습니다"),
    }
}
fn safe_command_path(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        && path.to_str().is_some_and(|s| {
            !s.chars().any(|c| {
                matches!(
                    c,
                    '\r' | '\n' | '"' | '%' | '!' | '^' | '&' | '|' | '<' | '>'
                )
            })
        })
}
/// PATH then PATHEXT order, with no execution, CWD search or relative entries.
pub fn resolve_claude_command(
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let extensions = pathext
        .and_then(|s| s.to_str())
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .filter(|s| [".com", ".exe", ".bat", ".cmd"].contains(&s.to_ascii_lowercase().as_str()))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for directory in std::env::split_paths(path?) {
        if !safe_command_path(&directory) {
            continue;
        }
        for extension in &extensions {
            let command = directory.join(format!("claude{extension}"));
            if safe_command_path(&command)
                && fs::symlink_metadata(&command).is_ok_and(|m| {
                    use std::os::windows::fs::MetadataExt;
                    m.is_file() && m.file_attributes() & 0x400 == 0
                })
            {
                // Canonical spelling matters for the recorded launcher (PATHEXT may be uppercase).
                let canonical = fs::canonicalize(command).ok()?;
                let text = canonical.to_str()?;
                let command = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
                    PathBuf::from(format!(r"\\{rest}"))
                } else {
                    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
                };
                if safe_command_path(&command) {
                    return Some(command);
                }
            }
        }
    }
    None
}

/// Reads only injected paths; persisted mode wins over auto-detection on retry.
pub fn resolve_codex_mode(paths: &CutoverPaths, options: &WireOptions) -> Result<CodexMode> {
    if let Some(mode) = options.codex_mode {
        return Ok(mode);
    }
    // Routing by the journal header does not replace the wire path's full
    // journal validation. Explicit overrides above retain their precedence.
    match fs::read(paths.journal()) {
        Ok(bytes) => {
            #[derive(Deserialize)]
            struct ModeJournal {
                schema: u8,
                #[serde(default)]
                mode: Option<CodexMode>,
            }
            let invalid = || anyhow::anyhow!("연결 journal 형식이 올바르지 않습니다");
            let saved: ModeJournal = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
            return match saved.schema {
                1 => Ok(CodexMode::Default),
                3 => saved.mode.ok_or_else(invalid),
                _ => Err(invalid()),
            };
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => bail!("연결 journal을 읽지 못했습니다"),
    }
    if !paths.codex_config.exists() {
        return Ok(CodexMode::Dedicated);
    }
    let doc = read_text(&paths.codex_config)?
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("Codex config 형식이 올바르지 않습니다"))?;
    let selected = doc
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        .unwrap_or("openai");
    let provider = doc
        .get("model_providers")
        .and_then(|providers| providers.get(selected));
    let endpoint = if let Some(provider) = provider {
        provider.get("base_url").and_then(toml_edit::Item::as_str)
    } else if selected == "openai" {
        doc.get("openai_base_url").and_then(toml_edit::Item::as_str)
    } else {
        None
    };
    if !paths.codex_auth.exists() {
        return Ok(CodexMode::Dedicated);
    }
    let auth: Value = serde_json::from_str(&read_text(&paths.codex_auth)?)
        .map_err(|_| anyhow::anyhow!("Codex auth.json 형식이 올바르지 않습니다"))?;
    Ok(
        if auth.get("auth_mode").and_then(Value::as_str) == Some("apikey")
            && endpoint.is_some_and(dedicated::is_cpa_endpoint)
        {
            CodexMode::Default
        } else {
            CodexMode::Dedicated
        },
    )
}

#[derive(Clone, Copy, Debug)]
pub struct WireOptions {
    pub codex_mode: Option<CodexMode>,
    pub claude_mode: Option<ClaudeMode>,
    pub data_port: u16,
    /// Content override for the same owned dedicated upgrade object only.
    pub force: bool,
    pub autostart: bool,
    pub disable_cpa_tasks: bool,
    pub detach_chatgpt_session_homes: bool,
}

impl Default for WireOptions {
    fn default() -> Self {
        Self {
            codex_mode: None,
            claude_mode: None,
            data_port: 10200,
            force: false,
            autostart: false,
            disable_cpa_tasks: false,
            detach_chatgpt_session_homes: false,
        }
    }
}

fn codex_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

fn claude_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn secret_diff_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "key", "secret", "bearer", "password", "auth"]
        .iter()
        .any(|word| key.contains(word))
}

fn redacted_diff_line(line: &str, secrets: &[String]) -> String {
    fn redact_json(value: &mut Value, secret: bool) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    redact_json(value, secret || secret_diff_key(key));
                }
            }
            Value::Array(values) => {
                for value in values {
                    redact_json(value, secret);
                }
            }
            _ if secret && value.as_str() != Some("<client key>") => {
                *value = Value::String("<redacted>".into());
            }
            _ => {}
        }
    }
    fn redact_toml(item: &mut toml_edit::Item) {
        if let Some(table) = item.as_table_like_mut() {
            for (key, item) in table.iter_mut() {
                if secret_diff_key(key.get()) && item.is_value() {
                    let value = item.as_value_mut().expect("value checked");
                    if value.as_str() != Some("<client key>") {
                        let decor = value.decor().clone();
                        *value = toml_edit::Value::from("<redacted>");
                        *value.decor_mut() = decor;
                    }
                } else {
                    redact_toml(item);
                }
            }
        }
    }
    let mut line = line.to_owned();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        line = line.replace(secret, "<redacted>");
    }
    let trimmed = line.trim();
    // Parse complete compact JSON, or one complete pretty-printed member.
    // An env object is structural; keywords in its children cannot redact env.
    let member = trimmed.strip_suffix(',').unwrap_or(trimmed);
    let wrapped = format!("{{{member}}}");
    let json = serde_json::from_str::<Value>(trimmed)
        .map(|value| (value, false))
        .or_else(|_| serde_json::from_str::<Value>(&wrapped).map(|value| (value, true)));
    if let Ok((mut value, is_member)) = json {
        let before = value.clone();
        redact_json(&mut value, false);
        if value == before {
            return line;
        }
        let rendered = serde_json::to_string(&value).expect("JSON value serializes");
        let rendered = if is_member {
            &rendered[1..rendered.len() - 1]
        } else {
            &rendered
        };
        let leading = &line[..line.len() - line.trim_start().len()];
        let trailing = &line[line.trim_end().len()..];
        let comma = if is_member && trimmed.ends_with(',') {
            ","
        } else {
            ""
        };
        return format!("{leading}{rendered}{comma}{trailing}");
    }
    if let Ok(mut document) = line.parse::<DocumentMut>() {
        let before = document.to_string();
        redact_toml(document.as_item_mut());
        let rendered = document.to_string();
        if rendered == before {
            return line;
        }
        // DocumentMut appends a line terminator to a standalone assignment.
        // Diff input is already split into lines; do not inject blank lines.
        return if line.ends_with('\n') {
            rendered
        } else {
            rendered.strip_suffix('\n').unwrap_or(&rendered).to_owned()
        };
    }
    line
}

fn unified_diff(path: &Path, before: &[u8], after: &[u8], secrets: &[String]) -> Vec<String> {
    let before = String::from_utf8_lossy(before)
        .trim_start_matches('\u{feff}')
        .lines()
        .map(|line| redacted_diff_line(line, secrets))
        .collect::<Vec<_>>();
    let after = String::from_utf8_lossy(after)
        .trim_start_matches('\u{feff}')
        .lines()
        .map(|line| redacted_diff_line(line, secrets))
        .collect::<Vec<_>>();
    let mut result = vec![
        format!("--- {} (현재)", path.display()),
        format!("+++ {} (연결 후)", path.display()),
    ];
    let n = before.len();
    let m = after.len();
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if before[i] == after[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut entries = Vec::<(char, String)>::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && before[i] == after[j] {
            entries.push((' ', before[i].clone()));
            i += 1;
            j += 1;
        } else if j == m || (i < n && lcs[i + 1][j] >= lcs[i][j + 1]) {
            entries.push(('-', before[i].clone()));
            i += 1;
        } else {
            entries.push(('+', after[j].clone()));
            j += 1;
        }
    }
    let changes = entries
        .iter()
        .enumerate()
        .filter_map(|(index, (kind, _))| (*kind != ' ').then_some(index))
        .collect::<Vec<_>>();
    let mut cursor = 0;
    while cursor < changes.len() {
        let first = changes[cursor].saturating_sub(3);
        let mut last = (changes[cursor] + 4).min(entries.len());
        cursor += 1;
        while cursor < changes.len() && changes[cursor] <= last + 3 {
            last = (changes[cursor] + 4).min(entries.len());
            cursor += 1;
        }
        let before_start = entries[..first]
            .iter()
            .filter(|(kind, _)| *kind != '+')
            .count();
        let after_start = entries[..first]
            .iter()
            .filter(|(kind, _)| *kind != '-')
            .count();
        let before_count = entries[first..last]
            .iter()
            .filter(|(kind, _)| *kind != '+')
            .count();
        let after_count = entries[first..last]
            .iter()
            .filter(|(kind, _)| *kind != '-')
            .count();
        result.push(format!(
            "@@ -{},{} +{},{} @@",
            before_start + 1,
            before_count,
            after_start + 1,
            after_count
        ));
        result.extend(
            entries[first..last]
                .iter()
                .map(|(kind, line)| format!("{kind}{line}")),
        );
    }
    result
}

fn plaintext_report(roots: &[PathBuf]) -> Vec<String> {
    roots
        .iter()
        .map(|root| {
            if !root.exists() {
                return format!("평문 자격 증명 후보 경로 없음: {}", root.display());
            }
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

fn home_matches(path: &str, homes: &[SessionHome]) -> bool {
    let path = normal_path(&PathBuf::from(path));
    homes
        .iter()
        .any(|home| normal_path(&home.home) == path || normal_path(&home.auth) == path)
}

fn detach_chatgpt_session_homes(path: &Path, homes: &[SessionHome]) -> Result<Vec<u8>> {
    let original = fs::read(path)?;
    let text = read_text(path)?;
    let mut value: Value =
        serde_json::from_str(&text).context("session-homes.json을 해석하지 못했습니다")?;
    fn prune(value: &mut Value, homes: &[SessionHome]) {
        match value {
            Value::Object(map) => {
                let keys = map
                    .iter()
                    .filter_map(|(key, child)| {
                        let direct = child.as_str().is_some_and(|path| home_matches(path, homes));
                        let nested = child.as_object().is_some_and(|object| {
                            object.iter().any(|(key, value)| {
                                key.to_ascii_lowercase().contains("home")
                                    && value.as_str().is_some_and(|path| home_matches(path, homes))
                            })
                        });
                        (direct || nested).then(|| key.clone())
                    })
                    .collect::<Vec<_>>();
                for key in keys {
                    map.remove(&key);
                }
                for child in map.values_mut() {
                    prune(child, homes);
                }
            }
            Value::Array(items) => {
                items.retain(|child| {
                    !child.as_object().is_some_and(|object| {
                        object.iter().any(|(key, value)| {
                            key.to_ascii_lowercase().contains("home")
                                && value.as_str().is_some_and(|path| home_matches(path, homes))
                        })
                    })
                });
                for child in items {
                    prune(child, homes);
                }
            }
            _ => {}
        }
    }
    prune(&mut value, homes);
    let rendered = serde_json::to_vec_pretty(&value)?;
    Ok(preserved_bytes_from_original(&original, &rendered))
}

fn preserved_bytes_from_original(original: &[u8], bytes: &[u8]) -> Vec<u8> {
    let bom = original.starts_with(&[0xef, 0xbb, 0xbf]);
    let crlf = original.windows(2).any(|pair| pair == b"\r\n");
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if crlf {
        text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    let mut output = Vec::with_capacity(text.len() + usize::from(bom) * 3);
    if bom {
        output.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    output.extend_from_slice(text.as_bytes());
    output
}

fn tray_autostart_command() -> Result<String> {
    let exe = std::env::current_exe()
        .context("현재 실행 파일 경로를 확인할 수 없습니다")?
        .with_file_name("quota-baton-tray.exe");
    Ok(format!("\"{}\"", exe.display()))
}

fn registry_wide(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn open_run_key(
    access: windows::Win32::System::Registry::REG_SAM_FLAGS,
) -> Result<windows::Win32::System::Registry::HKEY> {
    let subkey = registry_wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let mut key = windows::Win32::System::Registry::HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            access,
            &mut key,
        )
    };
    if !status.is_ok() {
        bail!("자동 시작 레지스트리를 열 수 없습니다")
    }
    Ok(key)
}

fn read_autostart() -> Result<(Original, u32)> {
    let subkey = registry_wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let name = registry_wide("quota-baton-tray");
    let mut key = windows::Win32::System::Registry::HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok((Original::default(), REG_SZ.0));
    }
    if !status.is_ok() {
        bail!("자동 시작 레지스트리를 읽지 못했습니다")
    }
    let mut value_type = REG_VALUE_TYPE(0);
    let mut length = 0u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut length),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        unsafe {
            let _ = RegCloseKey(key);
        };
        return Ok((Original::default(), REG_SZ.0));
    }
    if !status.is_ok() {
        unsafe {
            let _ = RegCloseKey(key);
        };
        bail!("자동 시작 값을 읽지 못했습니다")
    }
    let mut bytes = vec![0u8; length as usize];
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(bytes.as_mut_ptr()),
            Some(&mut length),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if !status.is_ok() {
        bail!("자동 시작 값을 읽지 못했습니다")
    }
    let words =
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u16, (length as usize) / 2) };
    let value = String::from_utf16_lossy(words)
        .trim_end_matches('\0')
        .to_owned();
    Ok((
        Original {
            present: true,
            value: Some(value),
        },
        value_type.0,
    ))
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
struct OptionalMutationState {
    autostart: String,
    tasks: std::collections::BTreeMap<String, bool>,
    calls: usize,
}
#[cfg(test)]
thread_local! {
    static OPTIONAL_MUTATIONS: std::cell::RefCell<Option<OptionalMutationState>> = const { std::cell::RefCell::new(None) };
}

fn set_autostart(action: &OptionalActionJournal) -> Result<()> {
    #[cfg(test)]
    if OPTIONAL_MUTATIONS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(state) = slot.as_mut() {
            state.calls += 1;
            state.autostart = action.after.clone();
            true
        } else {
            false
        }
    }) {
        return Ok(());
    }
    let key = open_run_key(KEY_SET_VALUE)?;
    let name = registry_wide("quota-baton-tray");
    let value = registry_wide(&action.after);
    let value_type = REG_VALUE_TYPE(action.after_type);
    let status = unsafe {
        RegSetValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            value_type,
            Some(bytemuck_u8(&value)),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if !status.is_ok() {
        bail!("자동 시작 레지스트리 변경을 거부했습니다")
    }
    Ok(())
}

fn restore_autostart(original: &Original, value_type: u32) -> Result<()> {
    let key = open_run_key(KEY_SET_VALUE)?;
    let name = registry_wide("quota-baton-tray");
    let status = if original.present {
        let value = registry_wide(original.value.as_deref().unwrap_or_default());
        unsafe {
            RegSetValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                REG_VALUE_TYPE(value_type),
                Some(bytemuck_u8(&value)),
            )
        }
    } else {
        unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) }
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if !status.is_ok() {
        bail!("자동 시작 복구를 거부했습니다")
    }
    Ok(())
}

fn bytemuck_u8(words: &[u16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 2) }
}

#[cfg(test)]
fn task_enabled(_name: &str) -> Option<bool> {
    panic!("unit test attempted the real scheduled-task backend")
}
#[cfg(not(test))]
fn task_enabled(name: &str) -> Option<bool> {
    let output = std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", name, "/V", "/FO", "LIST"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_task_enabled(&String::from_utf8_lossy(&output.stdout))
}

fn parse_task_enabled(output: &str) -> Option<bool> {
    let state = output.lines().find_map(|line| {
        let (label, value) = line.split_once(':')?;
        let label = label.trim();
        (label.eq_ignore_ascii_case("Scheduled Task State") || label == "예약된 작업 상태")
            .then_some(value.trim())
    })?;

    match state.to_ascii_lowercase().as_str() {
        "enabled" | "사용" | "activé" | "aktiviert" | "habilitado" | "有効" | "已启用" => {
            Some(true)
        }
        "disabled" | "사용 안 함" | "désactivé" | "deaktiviert" | "deshabilitado" | "無効"
        | "已禁用" => Some(false),
        _ => None,
    }
}

fn cpa_task_journal(task_query: &dyn Fn(&str) -> Option<bool>) -> Result<Vec<TaskJournal>> {
    ["cliproxyapi-proxy", "cliproxyapi-quota-watch"]
        .into_iter()
        .map(|name| {
            let was_enabled = task_query(name)
                .with_context(|| format!("CPA 예약 작업 상태를 확인할 수 없습니다: {name}"))?;
            Ok(TaskJournal {
                name: name.into(),
                was_enabled,
                after_enabled: false,
            })
        })
        .collect()
}

fn disable_cpa_tasks(tasks: &[TaskJournal]) -> Result<()> {
    #[cfg(test)]
    if OPTIONAL_MUTATIONS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(state) = slot.as_mut() {
            for task in tasks.iter().filter(|task| task.was_enabled) {
                state.calls += 1;
                state.tasks.insert(task.name.clone(), false);
            }
            true
        } else {
            false
        }
    }) {
        return Ok(());
    }
    #[cfg(test)]
    assert!(
        tasks.iter().all(|task| !task.was_enabled),
        "unit test attempted the real scheduled-task backend"
    );
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
    #[cfg(test)]
    assert!(
        tasks.is_empty(),
        "unit test attempted the real scheduled-task backend"
    );
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

fn changed_file_list(paths: &CutoverPaths, journal: &Journal) -> Vec<String> {
    let mut changed = Vec::new();
    for (label, path, before) in [
        (
            "codex config",
            &paths.codex_config,
            &journal.codex_config_before,
        ),
        ("codex auth", &paths.codex_auth, &journal.codex_auth_before),
        (
            "Claude settings",
            &paths.claude_settings,
            &journal.claude_settings_before,
        ),
    ] {
        if !before.is_empty()
            && hash_file(path)
                .ok()
                .is_some_and(|current| current != *before)
        {
            changed.push(format!("{label}: {}", path.display()));
        }
    }
    for session in &journal.codex_sessions {
        let path = Path::new(&session.path);
        if hash_file(path)
            .ok()
            .is_some_and(|current| current != session.before)
        {
            changed.push(format!("Codex session auth: {}", path.display()));
        }
    }
    if let (Some(path), Some(before)) = (
        journal.session_homes_path.as_deref().map(Path::new),
        journal.session_homes_before.as_deref(),
    ) && hash_file(path)
        .ok()
        .is_some_and(|current| current != before)
    {
        changed.push(format!("session-homes.json: {}", path.display()));
    }
    changed
}

pub(crate) const CPA_DEPENDENCY_REFUSAL: &str = "CPA가 연결 계정의 토큰을 갱신하고 있어 CPA 작업을 끌 수 없습니다. 먼저 계정을 직접 로그인으로 전환하세요.";

fn active_linked_count(home: &Path) -> Result<usize> {
    Ok(AccountStore::list_metadata(home)?
        .iter()
        .filter(|account| {
            account.user_state == UserState::Active
                && matches!(
                    account.source,
                    Source::Cpa {
                        mode: ImportMode::Linked,
                        ..
                    }
                )
        })
        .count())
}

fn dependency_note(count: usize) -> Option<String> {
    (count > 0).then(|| format!("CPA는 계속 실행되어야 합니다 (연결 계정 {count}개의 토큰 갱신)"))
}

pub(crate) fn cpa_dependency_note(home: &Path) -> Result<Option<String>> {
    Ok(dependency_note(active_linked_count(home)?))
}

fn check_cpa_dependency(count: usize, options: &WireOptions) -> Result<()> {
    if options.disable_cpa_tasks && count > 0 {
        bail!(CPA_DEPENDENCY_REFUSAL);
    }
    Ok(())
}

pub fn check_optional_preconditions(home: &Path, options: &WireOptions) -> Result<()> {
    check_optional_preconditions_with_task_query(home, options, &task_enabled)
}

fn check_optional_preconditions_with_task_query(
    home: &Path,
    options: &WireOptions,
    task_query: &dyn Fn(&str) -> Option<bool>,
) -> Result<()> {
    // Guard before registry checks and every scheduled-task backend read.
    if options.disable_cpa_tasks {
        check_cpa_dependency(active_linked_count(home)?, options)?;
    }
    if options.autostart {
        let _ = read_autostart()?;
        let key = open_run_key(KEY_SET_VALUE)?;
        unsafe {
            let _ = RegCloseKey(key);
        }
    }
    if options.disable_cpa_tasks {
        for name in ["cliproxyapi-proxy", "cliproxyapi-quota-watch"] {
            task_query(name)
                .with_context(|| format!("CPA 예약 작업 상태를 확인할 수 없습니다: {name}"))?;
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
    wire_with_task_query(paths, apply, options, &task_enabled)
}

fn wire_with_task_query(
    paths: &CutoverPaths,
    apply: bool,
    options: WireOptions,
    task_query: &dyn Fn(&str) -> Option<bool>,
) -> Result<Report> {
    let mode = resolve_codex_mode(paths, &options)?;
    let claude_mode = resolve_claude_mode(paths, &options)?;
    if mode == CodexMode::None && claude_mode == ClaudeMode::None {
        bail!("Codex와 Claude를 모두 연결 안 함으로 선택할 수 없습니다");
    }
    if mode != CodexMode::Default || claude_mode != ClaudeMode::Default {
        if options.detach_chatgpt_session_homes && mode != CodexMode::Default {
            bail!("세션 홈 분리 옵션은 Codex default 모드에서만 사용할 수 있습니다");
        }
        return dedicated::wire(paths, apply, options, mode, claude_mode, task_query);
    }
    let linked_active = active_linked_count(&paths.qb_home)?;
    check_cpa_dependency(linked_active, &options)?;
    if paths.journal().exists() {
        bail!("이미 연결 journal이 있습니다. 먼저 `quota-baton unwire --apply`를 실행하세요")
    }
    let key = if apply {
        Some(read_key(&paths.client_key)?)
    } else {
        None
    };
    let (codex, codex_auth, codex_auth_mode, claude) = read_inputs(paths)?;
    if apply && codex_auth_mode.value.as_deref() != Some("apikey") {
        bail!("기본 Codex auth.json이 apikey 인증이 아니어서 연결을 중단했습니다.");
    }
    let codex_config_before = hash_file(&paths.codex_config)?;
    let codex_auth_before = hash_file(&paths.codex_auth)?;
    let claude_settings_before = hash_file(&paths.claude_settings)?;
    let preview_key = key.as_deref().unwrap_or("<client key>");
    let codex_base = codex_base_url(options.data_port);
    let claude_base = claude_base_url(options.data_port);
    let codex_config_preview = codex_config_bytes(&paths.codex_config, &codex_base, preview_key)?;
    let codex_auth_preview = codex_auth_bytes(&paths.codex_auth, preview_key)?;
    let claude_preview = claude_bytes(&paths.claude_settings, &claude_base, preview_key)?;
    let providers = provider_journal(&paths.codex_config)?;
    let mut diff_secrets = Vec::new();
    for original in [&codex_auth, &claude.1] {
        if let Some(encoded) = original.protected_b64.as_deref() {
            diff_secrets.push(
                String::from_utf8(unprotect(encoded)?)
                    .context("journal secret이 올바르지 않습니다")?,
            );
        }
    }
    for provider in &providers {
        if let Some(encoded) = provider.experimental_bearer_token.protected_b64.as_deref() {
            diff_secrets.push(
                String::from_utf8(unprotect(encoded)?)
                    .context("journal secret이 올바르지 않습니다")?,
            );
        }
    }
    let codex_config_after = hash_bytes(&preserved_bytes(
        &paths.codex_config,
        &codex_config_preview,
    )?);
    let codex_auth_after = hash_bytes(&preserved_bytes(&paths.codex_auth, &codex_auth_preview)?);
    let claude_settings_after =
        hash_bytes(&preserved_bytes(&paths.claude_settings, &claude_preview)?);
    let mut codex_sessions = Vec::new();
    let mut session_diffs = Vec::new();
    let session_auth_paths = if paths.codex_session_homes.is_empty() {
        paths.codex_session_auth.iter().collect::<Vec<_>>()
    } else {
        paths
            .codex_session_homes
            .iter()
            .filter(|home| !home.shim_exempt)
            .map(|home| &home.auth)
            .collect::<Vec<_>>()
    };
    for path in session_auth_paths {
        if path.exists()
            && let Some(session) = session_journal(path, preview_key)?
        {
            let before = fs::read(path)?;
            let after = codex_auth_bytes(path, preview_key)?;
            session_diffs.extend(unified_diff(path, &before, &after, &diff_secrets));
            codex_sessions.push(session);
        }
    }
    let autostart = if options.autostart {
        let (before, before_type) = read_autostart()?;
        Some(OptionalActionJournal {
            before,
            before_type,
            after: tray_autostart_command()?,
            after_type: REG_SZ.0,
        })
    } else {
        None
    };
    let cpa_tasks = if options.disable_cpa_tasks {
        cpa_task_journal(task_query)?
    } else {
        Vec::new()
    };
    let session_homes_preview = if options.detach_chatgpt_session_homes {
        paths
            .session_homes_file
            .as_deref()
            .filter(|path| path.exists())
            .map(|path| {
                let homes = paths
                    .codex_session_homes
                    .iter()
                    .filter(|home| session_home_is_detachable(home))
                    .cloned()
                    .collect::<Vec<_>>();
                detach_chatgpt_session_homes(path, &homes)
            })
            .transpose()?
    } else {
        None
    };
    let session_homes_original = if options.detach_chatgpt_session_homes {
        paths
            .session_homes_file
            .as_deref()
            .filter(|path| path.exists())
            .map(|path| protect(&fs::read(path)?))
            .transpose()?
    } else {
        None
    };
    let mut journal = Journal {
        schema: 1,
        codex_config: paths.codex_config.display().to_string(),
        codex_auth: paths.codex_auth.display().to_string(),
        claude_settings: paths.claude_settings.display().to_string(),
        codex_base_url: codex.0.clone(),
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
        codex_sessions: codex_sessions.clone(),
        codex_config_original: Some(protect(&fs::read(&paths.codex_config)?)?),
        codex_auth_original: Some(protect(&fs::read(&paths.codex_auth)?)?),
        claude_settings_original: Some(protect(&fs::read(&paths.claude_settings)?)?),
        autostart: autostart.clone(),
        cpa_tasks: cpa_tasks.clone(),
        providers: providers.clone(),
        session_homes_path: paths
            .session_homes_file
            .as_ref()
            .map(|path| path.display().to_string()),
        session_homes_before: paths
            .session_homes_file
            .as_ref()
            .filter(|path| path.exists())
            .map(|path| hash_file(path))
            .transpose()?,
        session_homes_after: session_homes_preview
            .as_ref()
            .map(|bytes| hash_bytes(bytes)),
        session_homes_original,
    };
    let mut warnings = paths
        .codex_session_homes
        .iter()
        .map(|home| {
            let mode = session_home_auth_mode(home).unwrap_or_else(|| "missing".into());
            if home.shim_exempt {
                format!(
                    "Codex 세션 홈: {} (auth_mode={}, 세션 {}개). herdr shim이 그대로 두며 직접 ChatGPT로 연결합니다.",
                    shorten_path(&home.home),
                    mode,
                    home.sessions
                )
            } else if mode == "apikey" {
                format!(
                    "Codex 세션 홈: {} (auth_mode={}, 세션 {}개, 연결 대상)",
                    shorten_path(&home.home), mode, home.sessions
                )
            } else {
                format!(
                    "Codex 세션 홈: {} (auth_mode={}, 세션 {}개). herdr shim을 통해 자체 ChatGPT 토큰으로 quota-baton에 연결되어 재개 시 401이 됩니다. 그대로 두거나 session-homes.json에서 분리하거나 CPA를 계속 실행할 수 있습니다{}",
                    shorten_path(&home.home),
                    mode,
                    home.sessions,
                    if options.detach_chatgpt_session_homes {
                        "; 이번 실행에서 분리를 선택했습니다."
                    } else {
                        "."
                    }
                )
            }
        })
        .collect::<Vec<_>>();
    if options.disable_cpa_tasks {
        for name in ["cliproxyapi-proxy", "cliproxyapi-quota-watch"] {
            if task_query(name).is_none() {
                warnings.push(format!("CPA 예약 작업 상태를 확인할 수 없습니다: {name}"));
            }
        }
    }
    if let Some(warning) = &paths.session_homes_warning {
        warnings.push(warning.clone());
    }
    if paths.codex_session_homes.is_empty() {
        warnings.extend(paths.codex_session_auth.iter().filter_map(|path| {
            if !path.exists() {
                return Some(format!(
                    "Codex 세션 홈에 auth.json이 없어 변경하지 않습니다. 재개 시 401이 될 수 있으며 사용자 결정이 필요합니다: {}",
                    shorten_path(path)
                ));
            }
            let auth = read_text(path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
            (auth.get("auth_mode").and_then(Value::as_str) != Some("apikey")).then(|| {
                format!(
                    "Codex 세션 홈은 ChatGPT 인증이라 변경하지 않습니다. 재개 시 401이 될 수 있으며 사용자 결정이 필요합니다: {}",
                    shorten_path(path)
                )
            })
        }));
    }
    warnings.extend(scan_other_clients_10100(&paths.scan_roots));
    warnings.extend(paths.codex_realtime_items.iter().map(|key| {
        format!(
            "CPA에 남는 항목: Codex {}. quota-baton은 realtime/voice 경로를 제공하지 않으므로 CPA를 중지하면 해당 기능도 중지됩니다.",
            key
        )
    }));
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
    if options.detach_chatgpt_session_homes && session_homes_preview.is_some() {
        changed.push("herdr/session-homes.json (ChatGPT 홈 분리)".into());
    }
    changed.extend(providers.iter().flat_map(|provider| {
        [
            format!("codex.model_providers.{}.base_url", provider.name),
            format!("codex.model_providers.{}.model_catalog_url", provider.name),
            format!(
                "codex.model_providers.{}.experimental_bearer_token",
                provider.name
            ),
        ]
    }));
    changed.extend(codex_sessions.iter().map(|session| {
        format!(
            "codex.session-auth:{}",
            shorten_path(Path::new(&session.path))
        )
    }));
    let mut changed_files = std::collections::BTreeSet::from([
        paths.codex_config.display().to_string(),
        paths.codex_auth.display().to_string(),
        paths.claude_settings.display().to_string(),
    ]);
    changed_files.extend(codex_sessions.iter().map(|session| session.path.clone()));
    if let Some(path) = paths
        .session_homes_file
        .as_ref()
        .filter(|_| session_homes_preview.is_some())
    {
        changed_files.insert(path.display().to_string());
    }
    let report = Report {
        summary: vec![
            format!(
                "요약: 연결 대상 {}개 파일, {}개 항목",
                changed_files.len(),
                changed.len()
            ),
            format!(
                "CPA에 남는 세션 홈: {}개",
                paths
                    .codex_session_homes
                    .iter()
                    .filter(|home| {
                        !home.shim_exempt
                            && session_home_auth_mode(home).as_deref() != Some("apikey")
                    })
                    .count()
            ),
            format!(
                "선택 단계: 자동 시작={}, CPA 예약 작업 중지={}, ChatGPT 세션 홈 분리={}",
                if options.autostart {
                    "선택"
                } else {
                    "없음"
                },
                if options.disable_cpa_tasks {
                    "선택"
                } else {
                    "없음"
                },
                if options.detach_chatgpt_session_homes {
                    "선택"
                } else {
                    "없음"
                }
            ),
        ]
        .into_iter()
        .chain(dependency_note(linked_active))
        .collect(),
        changed,
        warnings,
        diff: unified_diff(
            &paths.codex_config,
            &fs::read(&paths.codex_config)?,
            &codex_config_preview,
            &diff_secrets,
        )
        .into_iter()
        .chain(unified_diff(
            &paths.codex_auth,
            &fs::read(&paths.codex_auth)?,
            &codex_auth_preview,
            &diff_secrets,
        ))
        .chain(unified_diff(
            &paths.claude_settings,
            &fs::read(&paths.claude_settings)?,
            &claude_preview,
            &diff_secrets,
        ))
        .chain(session_diffs)
        .chain(
            session_homes_preview
                .as_ref()
                .zip(paths.session_homes_file.as_ref())
                .into_iter()
                .flat_map(|(after, path)| {
                    unified_diff(
                        path,
                        &fs::read(path).unwrap_or_default(),
                        after,
                        &diff_secrets,
                    )
                }),
        )
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
    let apply_result = apply_codex(&paths.codex_config, &paths.codex_auth, &codex_base, &key)
        .and_then(|_| {
            for session in &journal.codex_sessions {
                apply_session(Path::new(&session.path), &key)?;
            }
            apply_claude(&paths.claude_settings, &claude_base, &key)?;
            if let (Some(path), Some(bytes)) = (&paths.session_homes_file, &session_homes_preview) {
                atomic_preserved(path, bytes)?;
            }
            Ok(())
        });
    if let Err(error) = apply_result {
        let changed_files = changed_file_list(paths, &journal);
        bail!(
            "적용 중 실패했습니다: {error}. 실제로 변경된 파일: {}. journal: {}. 복구 명령: quota-baton unwire --apply",
            if changed_files.is_empty() {
                "없음".into()
            } else {
                changed_files.join(", ")
            },
            paths.journal().display(),
        );
    }
    if options.autostart {
        if let Err(error) = set_autostart(&autostart.expect("autostart journal")) {
            let changed_files = changed_file_list(paths, &journal);
            bail!(
                "자동 시작 설정 중 실패했습니다: {error}. 실제로 변경된 파일: {}. journal: {}. 복구 명령: quota-baton unwire --apply",
                if changed_files.is_empty() {
                    "없음".into()
                } else {
                    changed_files.join(", ")
                },
                paths.journal().display()
            );
        }
        if let Some(action) = journal.autostart.as_mut() {
            let (observed, observed_type) = read_autostart()?;
            if let Some(value) = observed.value {
                action.after = value;
                action.after_type = observed_type;
            }
        }
        atomic_json(&paths.journal(), &journal)?;
    }
    if options.disable_cpa_tasks {
        if let Err(error) = disable_cpa_tasks(&cpa_tasks) {
            let changed_files = changed_file_list(paths, &journal);
            bail!(
                "CPA 예약 작업 처리 중 실패했습니다: {error}. 실제로 변경된 파일: {}. journal: {}. 복구 명령: quota-baton unwire --apply",
                if changed_files.is_empty() {
                    "없음".into()
                } else {
                    changed_files.join(", ")
                },
                paths.journal().display()
            );
        }
        for task in &mut journal.cpa_tasks {
            if let Some(enabled) = task_query(&task.name) {
                task.after_enabled = enabled;
            }
        }
        atomic_json(&paths.journal(), &journal)?;
    }
    Ok(report)
}

pub fn report_only(paths: &CutoverPaths) -> Report {
    let mut warnings = scan_other_clients_10100(&paths.scan_roots);
    warnings.extend(paths.codex_realtime_items.iter().map(|key| {
        format!(
            "CPA에 남는 항목: Codex {}. quota-baton은 realtime/voice 경로를 제공하지 않으므로 CPA를 중지하면 해당 기능도 중지됩니다.",
            key
        )
    }));
    if let Some(warning) = &paths.session_homes_warning {
        warnings.push(warning.clone());
    }
    Report {
        summary: vec!["요약: 다른 클라이언트와 평문 자격 증명 후보를 확인합니다".into()],
        changed: Vec::new(),
        warnings,
        diff: Vec::new(),
        report_only: plaintext_report(&paths.report_roots),
    }
}

pub fn unwire(paths: &CutoverPaths, apply: bool, force: bool) -> Result<Report> {
    if !paths.journal().exists() {
        return Ok(Report {
            summary: vec!["요약: 되돌릴 연결 설정이 없습니다".into()],
            changed: Vec::new(),
            warnings: Vec::new(),
            diff: Vec::new(),
            report_only: Vec::new(),
        });
    }
    let header: Value = serde_json::from_slice(&fs::read(paths.journal())?)
        .map_err(|_| anyhow::anyhow!("wiring journal 형식이 올바르지 않습니다"))?;
    if header.get("schema").and_then(Value::as_u64) == Some(3) {
        return dedicated::unwire(paths, apply, force);
    }
    let journal: Journal =
        serde_json::from_slice(&fs::read(paths.journal()).context("journal을 읽지 못했습니다")?)
            .context("wiring journal 형식이 올바르지 않습니다")?;
    if journal.codex_config != paths.codex_config.display().to_string()
        || journal.codex_auth != paths.codex_auth.display().to_string()
        || journal.claude_settings != paths.claude_settings.display().to_string()
    {
        bail!("wiring journal의 경로가 현재 홈과 일치하지 않습니다")
    }
    let mut report = Report {
        summary: vec!["요약: 연결 설정을 되돌립니다".into()],
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
        for (path, before, after) in [
            (
                &paths.codex_config,
                &journal.codex_config_before,
                &journal.codex_config_after,
            ),
            (
                &paths.codex_auth,
                &journal.codex_auth_before,
                &journal.codex_auth_after,
            ),
            (
                &paths.claude_settings,
                &journal.claude_settings_before,
                &journal.claude_settings_after,
            ),
        ] {
            let current = hash_file(path)?;
            if !before.is_empty() && current == *before {
                continue;
            }
            if !after.is_empty() && current != *after {
                bail!("파일이 변경되어 되돌릴 수 없습니다: {}", path.display());
            }
        }
        for session in &journal.codex_sessions {
            let path = Path::new(&session.path);
            if !path.exists() {
                report.warnings.push(format!(
                    "세션 auth.json이 없어 복구를 건너뜁니다: {}",
                    shorten_path(path)
                ));
                continue;
            }
            let current = hash_file(path)?;
            if !session.before.is_empty() && current == session.before {
                continue;
            }
            if current != session.after {
                bail!("파일이 변경되어 되돌릴 수 없습니다: {}", path.display());
            }
        }
        if let Some(autostart) = &journal.autostart {
            let current = read_autostart()?.0;
            if current.value != autostart.before.value
                && current.value != Some(autostart.after.clone())
            {
                bail!("autostart 설정이 변경되어 되돌릴 수 없습니다");
            }
        }
        for task in &journal.cpa_tasks {
            let Some(current) = task_enabled(&task.name) else {
                bail!("{} 예약 작업 상태를 확인할 수 없습니다", task.name);
            };
            if current == task.was_enabled {
                continue;
            }
            if current != task.after_enabled {
                bail!("{} 예약 작업이 변경되어 되돌릴 수 없습니다", task.name);
            }
        }
        if let (Some(path), Some(expected)) = (
            journal.session_homes_path.as_deref().map(Path::new),
            journal.session_homes_after.as_deref(),
        ) {
            let current = hash_file(path)?;
            let untouched = journal
                .session_homes_before
                .as_deref()
                .is_some_and(|before| current == before);
            if !untouched && current != expected {
                bail!("session-homes.json이 변경되어 되돌릴 수 없습니다");
            }
        }
    }
    if journal.codex_config_before.is_empty()
        || hash_file(&paths.codex_config)? != journal.codex_config_before
    {
        let config_exact = journal.codex_config_original.as_deref().is_some_and(|_| {
            hash_file(&paths.codex_config).ok().as_deref()
                == Some(journal.codex_config_after.as_str())
        });
        if config_exact {
            restore_exact(
                &paths.codex_config,
                journal.codex_config_original.as_deref().unwrap(),
            )?;
        } else {
            restore_codex_config(&paths.codex_config, &journal)?;
        }
    }
    if journal.codex_auth_before.is_empty()
        || hash_file(&paths.codex_auth)? != journal.codex_auth_before
    {
        let auth_exact = journal.codex_auth_original.as_deref().is_some_and(|_| {
            hash_file(&paths.codex_auth).ok().as_deref() == Some(journal.codex_auth_after.as_str())
        });
        if auth_exact {
            restore_exact(
                &paths.codex_auth,
                journal.codex_auth_original.as_deref().unwrap(),
            )?;
        } else {
            restore_codex_auth(&paths.codex_auth, &journal)?;
        }
    }
    if journal.claude_settings_before.is_empty()
        || hash_file(&paths.claude_settings)? != journal.claude_settings_before
    {
        let claude_exact = journal
            .claude_settings_original
            .as_deref()
            .is_some_and(|_| {
                hash_file(&paths.claude_settings).ok().as_deref()
                    == Some(&journal.claude_settings_after)
            });
        if claude_exact {
            restore_exact(
                &paths.claude_settings,
                journal.claude_settings_original.as_deref().unwrap(),
            )?;
        } else {
            restore_claude(&paths.claude_settings, &journal)?;
        }
    }
    for session in &journal.codex_sessions {
        let path = Path::new(&session.path);
        if !path.exists() {
            report.warnings.push(format!(
                "세션 auth.json이 없어 복구를 건너뜁니다: {}",
                shorten_path(path)
            ));
        } else if !session.before.is_empty()
            && hash_file(path).ok().as_deref() == Some(&session.before)
        {
            continue;
        } else if session.original.is_some()
            && hash_file(path).ok().as_deref() == Some(session.after.as_str())
        {
            restore_exact(path, session.original.as_deref().unwrap())?;
        } else {
            restore_session(path, session)?;
        }
    }
    if let Some(autostart) = &journal.autostart {
        let current = read_autostart()?.0;
        if current.value != autostart.before.value {
            restore_autostart(&autostart.before, autostart.before_type)?;
        }
    }
    let mut tasks_to_restore = Vec::new();
    for task in &journal.cpa_tasks {
        let current = task_enabled(&task.name)
            .with_context(|| format!("{} 예약 작업 상태를 확인할 수 없습니다", task.name))?;
        if current != task.was_enabled {
            tasks_to_restore.push(task.clone());
        }
    }
    restore_cpa_tasks(&tasks_to_restore)?;
    if let (Some(path), Some(original)) = (
        journal.session_homes_path.as_deref().map(Path::new),
        journal.session_homes_original.as_deref(),
    ) {
        let should_restore = journal
            .session_homes_before
            .as_deref()
            .and_then(|before| hash_file(path).ok().map(|current| current != before))
            .unwrap_or(true);
        if should_restore {
            restore_exact(path, original)?;
        }
    }
    fs::remove_file(paths.journal()).context("wiring journal을 삭제하지 못했습니다")?;
    Ok(report)
}

fn read_inputs(
    paths: &CutoverPaths,
) -> Result<(CodexOriginals, SecretOriginal, Original, ClaudeOriginals)> {
    let codex_text = read_text(&paths.codex_config).context("Codex config을 읽지 못했습니다")?;
    let doc = codex_text
        .parse::<DocumentMut>()
        .context("Codex config 형식이 올바르지 않습니다")?;
    let codex = (
        original_toml(doc.get("openai_base_url")),
        original_toml(doc.get("openai_api_key")),
        original_toml(doc.get("model_catalog_json")),
    );
    let auth_text = read_text(&paths.codex_auth).context("Codex auth.json을 읽지 못했습니다")?;
    let auth: Value =
        serde_json::from_str(&auth_text).context("Codex auth.json 형식이 올바르지 않습니다")?;
    let codex_auth = secret_json(auth.get("OPENAI_API_KEY"))?;
    let codex_auth_mode = original_json(auth.get("auth_mode"));
    let claude_text =
        read_text(&paths.claude_settings).context("Claude settings.json을 읽지 못했습니다")?;
    let json: Value = serde_json::from_str(&claude_text)
        .context("Claude settings.json 형식이 올바르지 않습니다")?;
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
    let text = value
        .as_str()
        .context("secret 설정 값이 문자열이 아닙니다")?;
    Ok(SecretOriginal {
        present: true,
        protected_b64: Some(protect(text.as_bytes())?),
    })
}
fn read_key(path: &Path) -> Result<String> {
    let key = fs::read_to_string(path)
        .with_context(|| format!("client.key를 읽지 못했습니다: {}", path.display()))?;
    let key = key.trim();
    if key.is_empty() {
        bail!("client.key가 비어 있습니다")
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
    atomic_preserved(path, &codex_config_bytes(path, base, key)?)?;
    atomic_preserved(auth_path, &codex_auth_bytes(auth_path, key)?)
}
fn codex_config_bytes(path: &Path, base: &str, key: &str) -> Result<Vec<u8>> {
    let mut doc = read_text(path)?
        .parse::<DocumentMut>()
        .context("Codex 설정을 해석하지 못했습니다")?;
    doc["openai_base_url"] = toml_edit::value(base);
    doc.remove("model_catalog_json");
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        for (_, item) in providers.iter_mut() {
            let Some(provider) = item.as_table_like_mut() else {
                continue;
            };
            let points_at_cpa = provider
                .get("base_url")
                .and_then(toml_edit::Item::as_str)
                .is_some_and(is_cpa_url);
            if points_at_cpa {
                provider.insert("base_url", toml_edit::value(base));
                provider.insert(
                    "model_catalog_url",
                    toml_edit::value(format!("{base}/models")),
                );
                provider.insert("experimental_bearer_token", toml_edit::value(key));
            }
        }
    }
    Ok(doc.to_string().into_bytes())
}

fn is_cpa_url(url: &str) -> bool {
    url.contains("127.0.0.1:10100") || url.contains("localhost:10100")
}

fn provider_journal(path: &Path) -> Result<Vec<ProviderJournal>> {
    let doc = read_text(path)?.parse::<DocumentMut>()?;
    let Some(providers) = doc
        .get("model_providers")
        .and_then(toml_edit::Item::as_table_like)
    else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for (name, item) in providers.iter() {
        let Some(provider) = item.as_table_like() else {
            continue;
        };
        let Some(base_url) = provider.get("base_url").and_then(toml_edit::Item::as_str) else {
            continue;
        };
        if !is_cpa_url(base_url) {
            continue;
        }
        result.push(ProviderJournal {
            name: name.to_owned(),
            base_url: original_toml(provider.get("base_url")),
            model_catalog_url: original_toml(provider.get("model_catalog_url")),
            experimental_bearer_token: secret_toml(provider.get("experimental_bearer_token"))?,
        });
    }
    Ok(result)
}

fn secret_toml(item: Option<&toml_edit::Item>) -> Result<SecretOriginal> {
    let Some(item) = item.filter(|item| !item.is_none()) else {
        return Ok(SecretOriginal {
            present: false,
            protected_b64: None,
        });
    };
    let value = item
        .as_str()
        .context("Codex provider secret이 문자열이 아닙니다")?;
    Ok(SecretOriginal {
        present: true,
        protected_b64: Some(protect(value.as_bytes())?),
    })
}
fn codex_auth_bytes(path: &Path, key: &str) -> Result<Vec<u8>> {
    let text = read_text(path)?;
    if let Some(updated) = replace_json_string(&text, "auth_mode", "apikey")
        .and_then(|updated| replace_json_string(&updated, "OPENAI_API_KEY", key))
    {
        return Ok(updated.into_bytes());
    }
    let mut auth: Value = serde_json::from_str(&text)?;
    let root = auth
        .as_object_mut()
        .context("Codex auth의 최상위 값이 객체가 아닙니다")?;
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
        .context("Codex session auth의 최상위 값이 객체가 아닙니다")?;
    restore_secret_json(root, "OPENAI_API_KEY", &journal.api_key)?;
    restore_json(root, "auth_mode", &journal.auth_mode);
    atomic_preserved(path, &serde_json::to_vec_pretty(&auth)?)
}

fn restore_exact(path: &Path, protected: &str) -> Result<()> {
    atomic_bytes(path, &unprotect(protected)?)
}
fn restore_codex_config(path: &Path, journal: &Journal) -> Result<()> {
    let mut doc = read_text(path)?
        .parse::<DocumentMut>()
        .context("Codex 설정을 해석하지 못했습니다")?;
    restore_toml(&mut doc, "openai_base_url", &journal.codex_base_url);
    restore_toml(&mut doc, "model_catalog_json", &journal.codex_model_catalog);
    for provider in &journal.providers {
        if let Some(table) = doc
            .get_mut("model_providers")
            .and_then(toml_edit::Item::as_table_like_mut)
            .and_then(|providers| providers.get_mut(&provider.name))
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            restore_toml_in_table(table, "base_url", &provider.base_url);
            restore_toml_in_table(table, "model_catalog_url", &provider.model_catalog_url);
            restore_secret_toml(
                table,
                "experimental_bearer_token",
                &provider.experimental_bearer_token,
            )?;
        }
    }
    atomic_preserved(path, doc.to_string().as_bytes())?;
    Ok(())
}
fn restore_codex_auth(auth_path: &Path, journal: &Journal) -> Result<()> {
    let mut auth: Value = serde_json::from_str(&read_text(auth_path)?)?;
    let root = auth
        .as_object_mut()
        .context("Codex auth의 최상위 값이 객체가 아닙니다")?;
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

fn restore_toml_in_table(table: &mut dyn toml_edit::TableLike, key: &str, original: &Original) {
    if original.present {
        table.insert(
            key,
            toml_edit::value(original.value.as_deref().unwrap_or_default()),
        );
    } else {
        table.remove(key);
    }
}

fn restore_secret_toml(
    table: &mut dyn toml_edit::TableLike,
    key: &str,
    original: &SecretOriginal,
) -> Result<()> {
    if original.present {
        let encoded = original
            .protected_b64
            .as_deref()
            .context("암호화된 provider secret이 없습니다")?;
        let value = String::from_utf8(unprotect(encoded)?)
            .context("provider secret이 올바르지 않습니다")?;
        table.insert(key, toml_edit::value(value));
    } else {
        table.remove(key);
    }
    Ok(())
}
fn apply_claude(path: &Path, base: &str, key: &str) -> Result<()> {
    atomic_preserved(path, &claude_bytes(path, base, key)?)
}
fn claude_bytes(path: &Path, base: &str, key: &str) -> Result<Vec<u8>> {
    let text = read_text(path)?;
    if let Some(updated) = replace_json_string_in_env(&text, "ANTHROPIC_BASE_URL", base)
        .and_then(|updated| replace_json_string_in_env(&updated, "ANTHROPIC_AUTH_TOKEN", key))
    {
        return Ok(updated.into_bytes());
    }
    let mut json: Value = serde_json::from_str(&text)?;
    let env = json
        .as_object_mut()
        .context("Claude settings의 최상위 값이 객체가 아닙니다")?
        .entry("env")
        .or_insert_with(|| Value::Object(Default::default()));
    let env = env
        .as_object_mut()
        .context("Claude settings의 env가 객체가 아닙니다")?;
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(base.into()));
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), Value::String(key.into()));
    Ok(serde_json::to_vec_pretty(&json)?)
}

fn replace_json_string(text: &str, key: &str, value: &str) -> Option<String> {
    let result = replace_json_string_raw(text, key, value)?;
    let parsed: Value = serde_json::from_str(&result).ok()?;
    (parsed.get(key).and_then(Value::as_str) == Some(value)).then_some(result)
}

fn replace_json_string_in_env(text: &str, key: &str, value: &str) -> Option<String> {
    let (start, end) = json_object_span(text, "env")?;
    let updated_env = replace_json_string_raw(&text[start..end], key, value)?;
    let mut result = text.to_owned();
    result.replace_range(start..end, &updated_env);
    let parsed: Value = serde_json::from_str(&result).ok()?;
    (parsed
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(key))
        .and_then(Value::as_str)
        == Some(value))
    .then_some(result)
}

fn json_object_span(text: &str, object_key: &str) -> Option<(usize, usize)> {
    let needle = format!("\"{object_key}\"");
    let key_start = text.find(&needle)?;
    let colon = text[key_start + needle.len()..].find(':')? + key_start + needle.len();
    let value_start = text[colon + 1..]
        .bytes()
        .position(|byte| !byte.is_ascii_whitespace())?
        + colon
        + 1;
    if text.as_bytes().get(value_start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in text.as_bytes()[value_start..].iter().copied().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((value_start, value_start + offset + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn replace_json_string_raw(text: &str, key: &str, value: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let key_start = text.find(&needle)?;
    let colon = text[key_start + needle.len()..].find(':')? + key_start + needle.len();
    let value_start = text[colon + 1..]
        .bytes()
        .position(|byte| !byte.is_ascii_whitespace())?
        + colon
        + 1;
    if text.as_bytes().get(value_start) != Some(&b'\"') {
        return None;
    }
    let mut escaped = false;
    let mut end = None;
    for (offset, byte) in text.as_bytes()[value_start + 1..].iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'\"' {
            end = Some(value_start + 1 + offset);
            break;
        }
    }
    let end = end?;
    let encoded = serde_json::to_string(value).ok()?;
    let mut result = text.to_owned();
    result.replace_range(value_start..=end, &encoded);
    Some(result)
}
fn restore_claude(path: &Path, journal: &Journal) -> Result<()> {
    let mut json: Value = serde_json::from_str(&read_text(path)?)?;
    let root = json
        .as_object_mut()
        .context("Claude settings의 최상위 값이 객체가 아닙니다")?;
    let env = root
        .entry("env")
        .or_insert_with(|| Value::Object(Default::default()));
    let env = env
        .as_object_mut()
        .context("Claude settings의 env가 객체가 아닙니다")?;
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
            .context("암호화된 secret이 없습니다")?;
        let value = String::from_utf8(unprotect(encoded)?)
            .context("암호화된 secret이 올바르지 않습니다")?;
        env.insert(key.into(), Value::String(value));
    } else {
        env.remove(key);
    }
    Ok(())
}

pub(crate) fn protect(input: &[u8]) -> Result<String> {
    transform(input, true).map(|bytes| STANDARD.encode(bytes))
}

pub(crate) fn unprotect(encoded: &str) -> Result<Vec<u8>> {
    let bytes = STANDARD
        .decode(encoded)
        .context("암호화된 journal 값이 올바르지 않습니다")?;
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
    result.context("DPAPI journal 변환에 실패했습니다")?;
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
    let mut text = String::from_utf8(bytes.to_vec()).context("설정 파일이 UTF-8이 아닙니다")?;
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
// Handle-based rename/deletion keep cleanup bound to the exclusively created
// scratch inode, even if other processes are active in the same directory.
fn replace_owned_scratch(file: &fs::File, path: &Path) -> std::io::Result<()> {
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle},
    };
    let path = std::path::absolute(path)?;
    let name = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let length = u32::try_from(name.len() * 2).map_err(std::io::Error::other)?;
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = offset + length as usize + 2; // include the zeroed UTF-16 terminator
    // SDK-typed storage supplies the authoritative header layout/alignment.
    let count = size.div_ceil(std::mem::size_of::<FILE_RENAME_INFO>());
    let mut buffer = vec![std::mem::MaybeUninit::<FILE_RENAME_INFO>::zeroed(); count];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = HANDLE::default();
        (*info).FileNameLength = length;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            info.cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfo,
            info.cast(),
            u32::try_from(size).map_err(std::io::Error::other)?,
        )
        .map_err(std::io::Error::other)?;
    }
    Ok(())
}

fn discard_owned_scratch(file: &fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
        },
    };
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
        .map_err(std::io::Error::other)?;
    }
    Ok(())
}

pub(crate) fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    atomic_bytes_with_counter(path, bytes, &SEQUENCE)
}

fn atomic_bytes_with_counter(
    path: &Path,
    bytes: &[u8],
    sequence: &std::sync::atomic::AtomicU64,
) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::{
        Foundation::GENERIC_WRITE,
        Storage::FileSystem::{DELETE, FILE_SHARE_NONE},
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    for _ in 0..64 {
        let serial = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp-{}-{serial}", std::process::id()));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_WRITE.0 | DELETE.0)
            .share_mode(FILE_SHARE_NONE.0) // retain exclusive ownership through rename/cleanup
            .open(&tmp)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("임시 파일을 만들지 못했습니다: {}", tmp.display()));
            }
        };
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            for attempt in 0..5 {
                match replace_owned_scratch(&file, path) {
                    Ok(()) => return Ok(()),
                    Err(_) if attempt < 4 => {
                        thread::sleep(Duration::from_millis(20 * (attempt + 1)))
                    }
                    Err(error) => return Err(error),
                }
            }
            unreachable!()
        })();
        if let Err(error) = result {
            if let Err(cleanup) = discard_owned_scratch(&file) {
                bail!(
                    "파일을 원자적으로 쓰지 못했습니다: {}: {error}. 소유한 임시 파일 정리 실패: {}: {cleanup}",
                    path.display(),
                    tmp.display()
                );
            }
            return Err(error)
                .with_context(|| format!("파일을 원자적으로 쓰지 못했습니다: {}", path.display()));
        }
        return Ok(());
    }
    bail!(
        "기존 임시 파일과 충돌하여 쓰기를 중단했습니다: {}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tempdir() -> std::io::Result<tempfile::TempDir> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r3-cutover-tests");
        fs::create_dir_all(&root)?;
        tempfile::tempdir_in(root)
    }
    fn wire(paths: &CutoverPaths, apply: bool) -> Result<Report> {
        super::wire_with_options(
            paths,
            apply,
            WireOptions {
                codex_mode: Some(CodexMode::Default),
                ..WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                }
            },
        )
    }

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
                qb_home: qb_home.clone(),
                codex_auth: auth,
                codex_session_auth: Vec::new(),
                codex_session_homes: Vec::new(),
                session_homes_file: None,
                session_homes_warning: None,
                codex_qb_home: qb_home.parent().unwrap().join(".codex-acct/qb"),
                npm_dir: qb_home.parent().unwrap().join("npm"),
                codex_config: codex,
                codex_realtime_items: Vec::new(),
                claude_command: None,
                claude_settings: claude,
                client_key,
                report_roots: Vec::new(),
                scan_roots: Vec::new(),
            },
        )
    }

    fn r3_retry_paths() -> (tempfile::TempDir, CutoverPaths) {
        let (dir, mut p) = paths();
        // Only this synthetic root is normalized, never an ambient profile.
        let root = fs::canonicalize(dir.path()).unwrap();
        p.qb_home = root.join("qb");
        p.codex_config = root.join("codex.toml");
        p.codex_auth = root.join("auth.json");
        p.claude_settings = root.join("settings.json");
        p.client_key = root.join("key");
        p.codex_qb_home = root.join(".codex-acct/qb");
        p.npm_dir = root.join("npm");
        (dir, p)
    }

    #[test]
    fn r3_schema1_flagless_retry_keeps_default_after_wired_endpoint_rewrite() {
        let (_dir, p) = r3_retry_paths();
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        fs::write(
            &p.codex_config,
            "openai_base_url = 'http://127.0.0.1:10100/v1'\n",
        )
        .unwrap();
        assert_eq!(
            resolve_codex_mode(
                &p,
                &WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                }
            )
            .unwrap(),
            CodexMode::Default
        );
        // Seed a legacy journal explicitly; only its retry is flagless.
        wire(&p, true).unwrap();
        assert!(
            fs::read_to_string(&p.codex_config)
                .unwrap()
                .contains("127.0.0.1:10200/v1")
        );
        let before = [
            p.journal(),
            p.codex_config.clone(),
            p.codex_auth.clone(),
            p.claude_settings.clone(),
        ]
        .map(|path| fs::read(path).unwrap());
        assert_eq!(
            resolve_codex_mode(
                &p,
                &WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                }
            )
            .unwrap(),
            CodexMode::Default
        );
        // Schema 1 retains its existing already-wired refusal; never reroute
        // to schema 3 or reinterpret the rewritten QB URL as a new home.
        let error = super::wire(&p, true).unwrap_err();
        assert!(error.to_string().contains("이미 연결 journal"));
        let after = [
            p.journal(),
            p.codex_config.clone(),
            p.codex_auth.clone(),
            p.claude_settings.clone(),
        ]
        .map(|path| fs::read(path).unwrap());
        assert_eq!(before, after);
        unwire(&p, true, false).unwrap();
    }

    #[test]
    fn r3_none_journal_flagless_retry_resumes_without_codex_inputs() {
        let (_dir, mut p) = r3_retry_paths();
        p.codex_config = p.qb_home.join("missing-config.toml");
        p.codex_auth = p.qb_home.join("missing-auth.json");
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        let original = fs::read(&p.claude_settings).unwrap();
        super::wire_with_options(
            &p,
            true,
            WireOptions {
                codex_mode: Some(CodexMode::None),
                ..WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                }
            },
        )
        .unwrap();
        assert_eq!(
            resolve_codex_mode(
                &p,
                &WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                }
            )
            .unwrap(),
            CodexMode::None
        );
        super::wire(&p, true).unwrap();
        assert!(!p.codex_config.exists());
        assert!(!p.codex_auth.exists());
        assert!(!p.codex_qb_home.exists());
        assert!(!p.npm_dir.exists());
        unwire(&p, true, false).unwrap();
        assert_eq!(fs::read(&p.claude_settings).unwrap(), original);
    }

    #[test]
    fn r3_dedicated_journal_wins_over_changed_source_and_full_validation_still_runs() {
        let (_dir, p) = r3_retry_paths();
        fs::create_dir_all(p.journal().parent().unwrap()).unwrap();
        let header = br#"{"schema":3,"mode":"dedicated"}"#;
        fs::write(p.journal(), header).unwrap();
        for source in [
            "openai_base_url = 'http://127.0.0.1:10100/v1'\n",
            "secret = 'synthetic-malformed-secret\n",
        ] {
            fs::write(&p.codex_config, source).unwrap();
            assert_eq!(
                resolve_codex_mode(
                    &p,
                    &WireOptions {
                        claude_mode: Some(ClaudeMode::Default),
                        ..WireOptions::default()
                    }
                )
                .unwrap(),
                CodexMode::Dedicated
            );
            // Header selection alone must not authorize an incomplete manifest.
            let error = super::wire(&p, true).unwrap_err();
            assert!(!format!("{error:#}\n{error:?}").contains("synthetic-malformed-secret"));
            assert_eq!(fs::read(p.journal()).unwrap(), header);
            assert_eq!(fs::read_to_string(&p.codex_config).unwrap(), source);
            assert!(!p.codex_qb_home.exists());
        }
    }

    #[test]
    fn r3_journal_mode_errors_are_generic_and_explicit_overrides_still_win() {
        let (_dir, p) = r3_retry_paths();
        fs::create_dir_all(p.journal().parent().unwrap()).unwrap();
        for header in [
            "{\"schema\":3,\"mode\":\"synthetic-mode-secret\"}",
            "{\"schema\":3,\"secret\":\"synthetic-mode-secret\"}",
            "{\"schema\":99,\"mode\":\"none\"}",
            "{\"schema\":\"synthetic-mode-secret\"}",
            "{\"schema\":3,\"mode\":\"none\",\"secret\":\"synthetic-mode-secret",
        ] {
            fs::write(p.journal(), header).unwrap();
            let error = resolve_codex_mode(
                &p,
                &WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                },
            )
            .unwrap_err();
            assert_eq!(error.to_string(), "연결 journal 형식이 올바르지 않습니다");
            assert!(!format!("{error:#}\n{error:?}").contains("synthetic-mode-secret"));
            for mode in [CodexMode::Default, CodexMode::Dedicated, CodexMode::None] {
                assert_eq!(
                    resolve_codex_mode(
                        &p,
                        &WireOptions {
                            codex_mode: Some(mode),
                            ..WireOptions {
                                claude_mode: Some(ClaudeMode::Default),
                                ..WireOptions::default()
                            }
                        }
                    )
                    .unwrap(),
                    mode
                );
            }
            assert_eq!(fs::read_to_string(p.journal()).unwrap(), header);
        }
    }

    #[test]
    fn r3_atomic_journal_and_settings_scratch_hardlink_collisions_preserve_sentinels() {
        for settings in [false, true] {
            let (dir, _p) = r3_retry_paths();
            let root = fs::canonicalize(dir.path()).unwrap();
            let target = root.join(if settings {
                "settings.json"
            } else {
                "wiring.json"
            });
            let destination_sentinel = root.join("destination-sentinel.json");
            let original = if settings {
                b"\xef\xbb\xbf{\r\n  \"keep\" : true\r\n}\r\n".as_slice()
            } else {
                b"{\"original\":true}".as_slice()
            };
            fs::write(&destination_sentinel, original).unwrap();
            if target.exists() {
                fs::remove_file(&target).unwrap();
            }
            fs::hard_link(&destination_sentinel, &target).unwrap();
            let scratch_sentinel = root.join("scratch-sentinel.json");
            fs::write(&scratch_sentinel, b"unrelated scratch hardlink sentinel").unwrap();
            let legacy_collision = target.with_extension(format!("tmp-{}", std::process::id()));
            let collision = target.with_extension(format!("tmp-{}-0", std::process::id()));
            fs::hard_link(&scratch_sentinel, &legacy_collision).unwrap();
            fs::hard_link(&scratch_sentinel, &collision).unwrap();
            let requested = b"{\n  \"updated\" : true\n}\n";
            let bytes = if settings {
                preserved_bytes(&target, requested).unwrap()
            } else {
                requested.to_vec()
            };
            let counter = std::sync::atomic::AtomicU64::new(0);
            atomic_bytes_with_counter(&target, &bytes, &counter).unwrap();
            assert_eq!(fs::read(&target).unwrap(), bytes);
            assert_eq!(fs::read(&destination_sentinel).unwrap(), original);
            for path in [&scratch_sentinel, &legacy_collision, &collision] {
                assert_eq!(
                    fs::read(path).unwrap(),
                    b"unrelated scratch hardlink sentinel"
                );
            }
            assert!(
                !target
                    .with_extension(format!("tmp-{}-1", std::process::id()))
                    .exists()
            );
            if settings {
                assert_eq!(bytes, b"\xef\xbb\xbf{\r\n  \"updated\" : true\r\n}\r\n");
            }
        }
    }

    #[test]
    fn r3_atomic_collision_exhaustion_preserves_target_and_all_unowned_hardlinks() {
        let (dir, _p) = r3_retry_paths();
        let root = fs::canonicalize(dir.path()).unwrap();
        for name in ["wiring.json", "settings.json"] {
            let target = root.join(name);
            fs::write(&target, b"unchanged destination").unwrap();
            let sentinel = root.join(format!("{name}-sentinel"));
            fs::write(&sentinel, b"unrelated hardlink bytes").unwrap();
            for serial in 0..64 {
                fs::hard_link(
                    &sentinel,
                    target.with_extension(format!("tmp-{}-{serial}", std::process::id())),
                )
                .unwrap();
            }
            let counter = std::sync::atomic::AtomicU64::new(0);
            assert!(atomic_bytes_with_counter(&target, b"must not install", &counter).is_err());
            assert_eq!(fs::read(&target).unwrap(), b"unchanged destination");
            for serial in 0..64 {
                assert_eq!(
                    fs::read(target.with_extension(format!("tmp-{}-{serial}", std::process::id())))
                        .unwrap(),
                    b"unrelated hardlink bytes"
                );
            }
            assert_eq!(fs::read(&sentinel).unwrap(), b"unrelated hardlink bytes");
        }
    }

    #[test]
    fn r3_atomic_failed_replace_discards_only_the_exclusively_owned_scratch() {
        let (dir, _p) = r3_retry_paths();
        let root = fs::canonicalize(dir.path()).unwrap();
        let target = root.join("blocked.json");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"destination directory stays").unwrap();
        let sentinel = root.join("unowned-sentinel");
        fs::write(&sentinel, b"unowned bytes").unwrap();
        let collision = target.with_extension(format!("tmp-{}-0", std::process::id()));
        fs::hard_link(&sentinel, &collision).unwrap();
        let names = || {
            let mut paths = fs::read_dir(&root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect::<Vec<_>>();
            paths.sort();
            paths
        };
        let before = names();
        let counter = std::sync::atomic::AtomicU64::new(0);
        assert!(atomic_bytes_with_counter(&target, b"cannot replace directory", &counter).is_err());
        assert_eq!(names(), before);
        assert_eq!(fs::read(&sentinel).unwrap(), b"unowned bytes");
        assert_eq!(fs::read(&collision).unwrap(), b"unowned bytes");
        assert_eq!(
            fs::read(target.join("keep")).unwrap(),
            b"destination directory stays"
        );
    }

    #[test]
    fn scheduled_task_parser_ignores_disabled_idle_time() {
        let output = "Scheduled Task State: Enabled\nIdle Time: Disabled\n";
        assert_eq!(parse_task_enabled(output), Some(true));
    }

    #[test]
    fn scheduled_task_parser_accepts_localized_state() {
        let output = "예약된 작업 상태: 사용\n유휴 시간: 사용 안 함\n";
        assert_eq!(parse_task_enabled(output), Some(true));
    }

    #[test]
    fn scheduled_task_parser_rejects_unknown_state() {
        assert_eq!(
            parse_task_enabled("Scheduled Task State: Paused\nIdle Time: Disabled\n"),
            None
        );
    }

    #[test]
    fn dry_run_reads_and_writes_nothing_or_secrets() {
        let (_dir, p) = paths();
        let before = fs::read(&p.codex_config).unwrap();
        let report = wire(&p, false).unwrap();
        assert_eq!(before, fs::read(&p.codex_config).unwrap());
        assert!(!p.journal().exists());
        assert!(report.diff.iter().any(|line| line.starts_with("@@ ")));
        assert!(report.diff.iter().any(|line| line.starts_with(' ')));
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
    fn session_home_discovery_reads_uuid_to_path_map_and_deduplicates_default() {
        let (dir, _p) = paths();
        let user = dir.path().join("user");
        let home = user.join("chatgpt-home");
        let value = serde_json::json!({
            "sessions": {
                "uuid-a": home.display().to_string(),
                "uuid-b": format!("{}\\", home.display()),
                "uuid-default": user.join(".codex").display().to_string()
            }
        });
        let homes = session_homes_from_value(&value, &user);
        assert_eq!(homes.len(), 1);
        assert_eq!(homes[0].sessions, 2);
        assert_eq!(homes[0].home, home);
        assert!(!homes[0].shim_exempt);
    }

    #[test]
    fn detach_keeps_exempt_and_apikey_homes_on_real_session_map_shape() {
        let (dir, _p) = paths();
        let exempt = dir.path().join(".codex-acct").join("one");
        let apikey = dir.path().join("apikey-home");
        let chatgpt = dir.path().join("chatgpt-home");
        for home in [&exempt, &apikey, &chatgpt] {
            fs::create_dir_all(home).unwrap();
        }
        fs::write(exempt.join("auth.json"), r#"{"auth_mode":"chatgpt"}"#).unwrap();
        fs::write(
            apikey.join("auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"key"}"#,
        )
        .unwrap();
        fs::write(chatgpt.join("auth.json"), r#"{"auth_mode":"chatgpt"}"#).unwrap();
        let map = dir.path().join("session-homes.json");
        let value = serde_json::json!({
            "sessions": {
                "exempt": exempt.display().to_string(),
                "apikey": apikey.display().to_string(),
                "chatgpt": chatgpt.display().to_string()
            }
        });
        fs::write(&map, serde_json::to_vec(&value).unwrap()).unwrap();
        let homes = [
            SessionHome {
                home: exempt.clone(),
                auth: exempt.join("auth.json"),
                sessions: 1,
                shim_exempt: true,
            },
            SessionHome {
                home: apikey.clone(),
                auth: apikey.join("auth.json"),
                sessions: 1,
                shim_exempt: false,
            },
            SessionHome {
                home: chatgpt.clone(),
                auth: chatgpt.join("auth.json"),
                sessions: 1,
                shim_exempt: false,
            },
        ];
        let after = detach_chatgpt_session_homes(&map, &[homes[2].clone()]).unwrap();
        let after: Value = serde_json::from_slice(&after).unwrap();
        let sessions = after["sessions"].as_object().unwrap();
        assert!(sessions.contains_key("exempt"));
        assert!(sessions.contains_key("apikey"));
        assert!(!sessions.contains_key("chatgpt"));
    }

    #[test]
    fn exempt_apikey_home_is_reported_but_never_wired() {
        let (dir, mut p) = paths();
        let home = dir.path().join(".codex-acct").join("apikey");
        fs::create_dir_all(&home).unwrap();
        let auth = home.join("auth.json");
        fs::write(&auth, r#"{"auth_mode":"apikey","OPENAI_API_KEY":"old"}"#).unwrap();
        p.codex_session_homes.push(SessionHome {
            home,
            auth: auth.clone(),
            sessions: 1,
            shim_exempt: true,
        });
        p.codex_session_auth.push(auth.clone());
        let before = fs::read(&auth).unwrap();
        let report = wire(&p, false).unwrap();
        assert_eq!(before, fs::read(&auth).unwrap());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("herdr shim이 그대로"))
        );
        assert!(
            !report
                .changed
                .iter()
                .any(|item| item.contains("session-auth"))
        );
    }

    #[test]
    fn unwire_succeeds_when_journaled_session_auth_disappears() {
        let (dir, mut p) = paths();
        let home = dir.path().join("session-home");
        fs::create_dir_all(&home).unwrap();
        let auth = home.join("auth.json");
        fs::write(&auth, r#"{"auth_mode":"apikey","OPENAI_API_KEY":"old"}"#).unwrap();
        p.codex_session_auth.push(auth.clone());
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        wire(&p, true).unwrap();
        fs::remove_file(&auth).unwrap();
        unwire(&p, true, false).unwrap();
        assert!(!p.journal().exists());
    }

    #[test]
    fn partial_apply_recovery_accepts_an_untouched_file() {
        let (_dir, p) = paths();
        let original_config = fs::read(&p.codex_config).unwrap();
        let original_auth = fs::read(&p.codex_auth).unwrap();
        let original_claude = fs::read(&p.claude_settings).unwrap();
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        wire(&p, true).unwrap();
        // Simulate a failure before the first file write while later files had
        // already been changed by the apply sequence.
        fs::write(&p.codex_config, &original_config).unwrap();
        unwire(&p, true, false).unwrap();
        assert_eq!(original_config, fs::read(&p.codex_config).unwrap());
        assert_eq!(original_auth, fs::read(&p.codex_auth).unwrap());
        assert_eq!(original_claude, fs::read(&p.claude_settings).unwrap());
        assert!(!p.journal().exists());
    }

    #[test]
    fn apply_refuses_chatgpt_default_home() {
        let (_dir, p) = paths();
        fs::write(
            &p.codex_auth,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":"old"}"#,
        )
        .unwrap();
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        let error = wire(&p, true).unwrap_err().to_string();
        assert!(error.contains("apikey"));
        assert!(!p.journal().exists());
    }

    #[test]
    fn r31_secret_diff_key_matches_only_the_keyword_list_case_insensitively() {
        for key in [
            "API_KEY",
            "access_TOKEN",
            "FOO_SECRET",
            "Bearer",
            "Password",
            "Authorization",
        ] {
            assert!(secret_diff_key(key), "{key}");
        }
        for key in [
            "env",
            "service_tier",
            "log_dir",
            "model_reasoning_effort",
            "approvals_reviewer",
            "notify",
            "projects",
        ] {
            assert!(!secret_diff_key(key), "{key}");
        }
    }

    #[test]
    fn r31_compact_json_keeps_nested_env_and_base_url_with_only_token_redacted() {
        let line = r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"<client key>","ANTHROPIC_BASE_URL":"http://127.0.0.1:10200"}}"#;
        assert_eq!(redacted_diff_line(line, &[]), line);
        assert_eq!(
            redacted_diff_line(
                r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"synthetic-private-token","ANTHROPIC_BASE_URL":"http://127.0.0.1:10200"}}"#,
                &[]
            ),
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"<redacted>","ANTHROPIC_BASE_URL":"http://127.0.0.1:10200"}}"#
        );
        assert_eq!(
            redacted_diff_line(r#"  "access_token": "synthetic-private-token","#, &[]),
            r#"  "access_token":"<redacted>","#
        );
        assert_eq!(
            redacted_diff_line(
                r#"{"env":{"API_KEYS":["synthetic-private-token","<client key>"]}}"#,
                &[]
            ),
            r#"{"env":{"API_KEYS":["<redacted>","<client key>"]}}"#
        );
    }

    #[test]
    fn r31_toml_preserves_harmless_values_and_project_headers() {
        for line in [
            "service_tier = 'fast'",
            "log_dir = 'C:/logs'",
            "model_reasoning_effort = 'high'",
            "approvals_reviewer = 'user'",
            "notify = ['notify-auth-helper', '--token-option']",
            "[projects.'C:/projects/key-tool']",
            "trust_level = 'trusted'",
            "base_url = 'http://127.0.0.1:10200'",
            "note = 'a # token-shaped harmless value'",
            "# auth setup",
            "# token options are documented here",
            "notify = 'visible' # auth setup",
        ] {
            assert_eq!(redacted_diff_line(line, &[]), line);
        }
    }

    #[test]
    fn r31_mcp_inline_secrets_bearer_comments_and_known_substrings_stay_redacted() {
        for line in [
            "FOO_SECRET = 'synthetic-mcp-secret'",
            "experimental_bearer_token = 'synthetic-bearer'",
            "env = { FOO_SECRET = 'synthetic-mcp-secret', SAFE = 'visible' }",
        ] {
            let redacted = redacted_diff_line(line, &[]);
            for secret in [
                "synthetic-mcp-secret",
                "synthetic-bearer",
                "synthetic-comment-secret",
            ] {
                assert!(!redacted.contains(secret), "{redacted}");
            }
            if line.starts_with("env") {
                assert!(redacted.contains("SAFE = 'visible'"));
            }
        }
        assert_eq!(
            redacted_diff_line(
                "log_dir = 'prefix-known-private-suffix'",
                &["known-private".into()]
            ),
            "log_dir = 'prefix-<redacted>-suffix'"
        );
        for line in [
            "# token = synthetic-comment-secret",
            "notify = 'visible' # password: synthetic-comment-secret",
        ] {
            assert_eq!(
                redacted_diff_line(line, &["synthetic-comment-secret".into()]),
                line.replace("synthetic-comment-secret", "<redacted>")
            );
        }
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

    #[test]
    fn provider_table_is_wired_without_changing_provider_identity() {
        let (_dir, p) = paths();
        fs::write(
            &p.codex_config,
            "openai_base_url = \"https://old.invalid/v1\"\n\n[mcp_servers.a.env]\nFOO_SECRET = \"mcp-secret\"\n\n[model_providers.cliproxyapi]\nname = \"CPA\"\nbase_url = \"http://127.0.0.1:10100/v1\"\nmodel_catalog_url = \"http://127.0.0.1:10100/v1/models\"\nexperimental_bearer_token = \"old-cpa-key\"\nwire_api = \"responses\"\nmodel_provider = \"cliproxyapi\"\n",
        )
        .unwrap();
        let preview = wire(&p, false).unwrap();
        assert!(
            preview
                .diff
                .iter()
                .all(|line| !line.contains("old-cpa-key"))
        );
        assert!(preview.diff.iter().all(|line| !line.contains("mcp-secret")));
        assert!(
            preview
                .diff
                .iter()
                .any(|line| line.contains("experimental_bearer_token"))
        );
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        wire(&p, true).unwrap();
        let config = fs::read_to_string(&p.codex_config).unwrap();
        assert!(config.contains("base_url = \"http://127.0.0.1:10200/v1\""));
        assert!(config.contains("model_catalog_url = \"http://127.0.0.1:10200/v1/models\""));
        assert!(config.contains("experimental_bearer_token = \"synthetic-key\""));
        assert!(config.contains("model_provider = \"cliproxyapi\""));
        unwire(&p, true, false).unwrap();
        assert!(
            fs::read_to_string(&p.codex_config)
                .unwrap()
                .contains("127.0.0.1:10100/v1")
        );
    }

    #[test]
    fn realistic_toml_keeps_unrelated_lines_when_previewing() {
        let (_dir, p) = paths();
        let mut source = String::from("openai_base_url = \"https://old.invalid/v1\"\n");
        for index in 0..120 {
            source.push_str(&format!("filler_{index} = \"value_{index}\"\n"));
        }
        fs::write(&p.codex_config, &source).unwrap();
        let preview =
            codex_config_bytes(&p.codex_config, "http://127.0.0.1:10200/v1", "<client key>")
                .unwrap();
        let preview = String::from_utf8(preview).unwrap();
        assert_eq!(preview.lines().count(), source.lines().count());
        assert!(preview.contains("filler_119 = \"value_119\""));
        assert!(preview.contains("openai_base_url = \"http://127.0.0.1:10200/v1\""));
    }

    #[test]
    fn realtime_cpa_items_are_reported_without_wiring() {
        let (_dir, mut p) = paths();
        p.codex_realtime_items = vec!["experimental_realtime_ws_base_url".into()];
        let report = wire(&p, false).unwrap();
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("realtime/voice")
                    && warning.contains("experimental_realtime_ws_base_url"))
        );
        assert!(
            !report
                .changed
                .iter()
                .any(|item| item.contains("realtime_ws"))
        );
    }

    #[test]
    fn pretty_json_files_keep_layout_and_unrelated_order_when_wired() {
        let (_dir, p) = paths();
        let settings = r#"{
  "ANTHROPIC_AUTH_TOKEN": "old-token-outside-env",
  "env": {
    "ANTHROPIC_BASE_URL": "https://old.invalid",
    "ANTHROPIC_AUTH_TOKEN": "old-token",
    "CLAUDE_CODE_SUBAGENT_MODEL": "keep-model"
  },
  "array": [1, 2, {"unicode": "한글"}],
  "last": true
}
"#;
        fs::write(&p.claude_settings, settings).unwrap();
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        wire(&p, true).unwrap();
        let wired = fs::read_to_string(&p.claude_settings).unwrap();
        assert_eq!(wired.lines().count(), settings.lines().count());
        assert!(
            wired.contains("\"array\": [1, 2, {\"unicode\": \"한글\"}]")
                && wired.contains("\"last\": true")
        );
        assert!(wired.contains("\"CLAUDE_CODE_SUBAGENT_MODEL\": \"keep-model\""));
        let wired_json: Value = serde_json::from_str(&wired).unwrap();
        assert_eq!(
            wired_json["ANTHROPIC_AUTH_TOKEN"],
            Value::String("old-token-outside-env".into())
        );
        assert_eq!(
            wired_json["env"]["ANTHROPIC_AUTH_TOKEN"],
            Value::String("synthetic-key".into())
        );
        unwire(&p, true, false).unwrap();
        assert_eq!(
            settings.as_bytes(),
            fs::read(&p.claude_settings).unwrap().as_slice()
        );
    }

    #[test]
    fn chatgpt_session_home_detach_is_explicit_and_reversible() {
        let (dir, mut p) = paths();
        let home = dir.path().join("chatgpt-home");
        fs::create_dir_all(&home).unwrap();
        let auth = home.join("auth.json");
        fs::write(&auth, r#"{"auth_mode":"chatgpt"}"#).unwrap();
        let session_map = dir.path().join("session-homes.json");
        fs::write(
            &session_map,
            serde_json::to_vec(&serde_json::json!({
                "s1": {"home": home.display().to_string()},
                "s2": {"home": home.display().to_string()}
            }))
            .unwrap(),
        )
        .unwrap();
        p.session_homes_file = Some(session_map.clone());
        p.codex_session_homes = vec![SessionHome {
            home: home.clone(),
            auth,
            sessions: 2,
            shim_exempt: false,
        }];
        let before = fs::read(&session_map).unwrap();
        let report = wire_with_options(
            &p,
            false,
            WireOptions {
                detach_chatgpt_session_homes: true,
                codex_mode: Some(CodexMode::Default),
                ..WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                }
            },
        )
        .unwrap();
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("세션 2개"))
        );
        assert_eq!(before, fs::read(&session_map).unwrap());
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        wire_with_options(
            &p,
            true,
            WireOptions {
                detach_chatgpt_session_homes: true,
                codex_mode: Some(CodexMode::Default),
                ..WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                }
            },
        )
        .unwrap();
        assert!(
            !String::from_utf8(fs::read(&session_map).unwrap())
                .unwrap()
                .contains("s1")
        );
        unwire(&p, true, false).unwrap();
        assert_eq!(before, fs::read(&session_map).unwrap());
    }

    #[test]
    fn edited_file_refuses_unwire_without_force_and_force_restores_keys() {
        let (_dir, p) = paths();
        fs::write(&p.client_key, "synthetic-key\n").unwrap();
        wire(&p, true).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&p.codex_config)
            .unwrap()
            .write_all(b"\nuser_edit = true\n")
            .unwrap();
        assert!(unwire(&p, true, false).is_err());
        assert!(p.journal().exists());
        unwire(&p, true, true).unwrap();
        assert!(!p.journal().exists());
        assert!(
            fs::read_to_string(&p.codex_config)
                .unwrap()
                .contains("old-url")
        );
    }

    #[test]
    #[ignore = "regenerates the cited synthetic M8.2 dry-run artifact"]
    fn write_synthetic_dry_run_sample() {
        let (dir, mut p) = paths();
        // Keep the cited artifact representative of the reviewed live shape:
        // a CPA provider table, an MCP secret in the surrounding hunk, and a
        // ChatGPT session-home entry.  Everything remains synthetic and
        // isolated in this temporary directory.
        fs::write(
            &p.codex_config,
            "openai_base_url = \"https://old.invalid/v1\"\n\n[mcp_servers.synthetic.env]\nFOO_SECRET = \"synthetic-mcp-secret\"\n\n[model_providers.cliproxyapi]\nname = \"CPA\"\nbase_url = \"http://127.0.0.1:10100/v1\"\nmodel_catalog_url = \"http://127.0.0.1:10100/v1/models\"\nexperimental_bearer_token = \"synthetic-cpa-key\"\nwire_api = \"responses\"\nmodel_provider = \"cliproxyapi\"\n",
        )
        .unwrap();
        let session_home = dir.path().join("chatgpt-session-home");
        fs::create_dir_all(&session_home).unwrap();
        let session_auth = session_home.join("auth.json");
        fs::write(
            &session_auth,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":"synthetic-session-key"}"#,
        )
        .unwrap();
        let session_homes_file = dir.path().join("session-homes.json");
        fs::write(
            &session_homes_file,
            serde_json::to_vec(&serde_json::json!({
                "sessions": {
                    "synthetic-session": session_home.display().to_string()
                }
            }))
            .unwrap(),
        )
        .unwrap();
        p.codex_session_auth.push(session_auth.clone());
        p.codex_session_homes.push(SessionHome {
            home: session_home.clone(),
            auth: session_auth,
            sessions: 1,
            shim_exempt: false,
        });
        p.session_homes_file = Some(session_homes_file);
        let report = wire(&p, false).unwrap();
        let mut output = String::new();
        for line in &report.summary {
            output.push_str(line);
            output.push('\n');
        }
        output.push_str("미리보기: ");
        output.push_str(&report.changed.join(", "));
        output.push('\n');
        for warning in &report.warnings {
            output.push_str("경고: ");
            output.push_str(warning);
            output.push('\n');
        }
        output.extend(report.diff.iter().map(|line| format!("{line}\n")));
        for item in &report.report_only {
            output.push_str("보고: ");
            output.push_str(item);
            output.push('\n');
        }
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../quota-baton/.sandbox/reports/M8.2-synthetic-dry-run.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, output).unwrap();
    }
}

#[cfg(test)]
pub(crate) mod cpa_dependency_tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;

    pub(crate) fn scratch() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r1-cpa-tests");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    // Deliberately invalid DPAPI payload: these paths must only read metadata.
    // This is a new synthetic record, never a mutation of a leased real store.
    pub(crate) fn account(
        home: &Path,
        label: &str,
        mode: &str,
        state: &str,
        health: Value,
    ) -> PathBuf {
        let identity = qb_core::accounts::Identity::Codex {
            account_id: format!("synthetic-{label}"),
            email: format!("{label}@example.invalid"),
            user_id: None,
        };
        let source = if mode == "owned" {
            json!({"kind":"oauth"})
        } else {
            json!({"kind":"cpa", "mode":mode, "dir":home.join("nonexistent-cpa"),
                "match":{"account_id":format!("synthetic-{label}"), "email":format!("{label}@example.invalid")}})
        };
        fs::create_dir_all(home.join("accounts")).unwrap();
        let path = home
            .join("accounts")
            .join(format!("{}.json", identity.id()));
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "schema":1, "id":identity.id(), "provider":"codex", "label":label,
                "email":format!("{label}@example.invalid"), "plan":null, "identity":identity,
                "source":source, "user_state":state, "health":health, "generation":1,
                "token_meta":{"expires_at":null,"last_refresh_at":null,"refresh_uncertain":false},
                "secret_dpapi_b64":"intentionally-not-dpapi"
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    fn disable_options() -> WireOptions {
        WireOptions {
            disable_cpa_tasks: true,
            codex_mode: Some(CodexMode::Default),
            ..WireOptions {
                claude_mode: Some(ClaudeMode::Default),
                ..WireOptions::default()
            }
        }
    }

    #[test]
    fn r1_cpa_active_linked_refuses_dry_run_and_apply_before_task_backend() {
        for apply in [false, true] {
            for health in [
                json!({"kind":"ready"}),
                json!({"kind":"cooling","reason":"auth","until":"2099-01-01T00:00:00Z","scope":"account"}),
                json!({"kind":"needs_relogin","reason":"synthetic","since":"2026-01-01T00:00:00Z"}),
            ] {
                let (_dir, p) = paths();
                let record = account(&p.qb_home, "linked", "linked", "active", health);
                let before = fs::read(&record).unwrap();
                let config_before = fs::read(&p.codex_config).unwrap();
                // No client key: refusal must also precede apply credential reads.
                let error = wire_with_task_query(&p, apply, disable_options(), &|_| {
                    panic!("task query before dependency refusal")
                })
                .unwrap_err();
                assert_eq!(error.to_string(), CPA_DEPENDENCY_REFUSAL);
                assert_eq!(fs::read(&record).unwrap(), before);
                assert_eq!(fs::read(&p.codex_config).unwrap(), config_before);
                assert!(!p.journal().exists());
                assert!(!p.client_key.exists());
            }
        }
    }

    #[test]
    fn r1_cpa_apply_precheck_refuses_before_optional_backends() {
        let dir = scratch();
        account(
            dir.path(),
            "linked",
            "linked",
            "active",
            json!({"kind":"ready"}),
        );
        let mut options = disable_options();
        options.autostart = true;
        let error = check_optional_preconditions_with_task_query(dir.path(), &options, &|_| {
            panic!("task query before precheck refusal")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), CPA_DEPENDENCY_REFUSAL);
    }

    #[test]
    fn r1_cpa_disabled_paused_owned_and_adopted_allow_dry_run_and_apply() {
        for apply in [false, true] {
            for (mode, state) in [
                ("linked", "disabled"),
                ("linked", "paused"),
                ("owned", "active"),
                ("adopted", "active"),
            ] {
                let (_dir, p) = paths();
                let record = account(&p.qb_home, "allowed", mode, state, json!({"kind":"ready"}));
                let before = fs::read(&record).unwrap();
                fs::write(&p.client_key, "synthetic-client-key\n").unwrap();
                let queries = Cell::new(0);
                let query = |_: &str| {
                    queries.set(queries.get() + 1);
                    Some(false)
                };
                check_optional_preconditions_with_task_query(
                    &p.qb_home,
                    &disable_options(),
                    &query,
                )
                .unwrap();
                // Tasks are already disabled: no real /Change backend can run.
                let report = wire_with_task_query(&p, apply, disable_options(), &query).unwrap();
                assert!(queries.get() >= 4);
                assert!(
                    !report
                        .summary
                        .iter()
                        .any(|line| line.contains("CPA는 계속 실행되어야"))
                );
                assert_eq!(fs::read(&record).unwrap(), before);
                assert_eq!(p.journal().exists(), apply);
            }
        }
    }

    #[test]
    fn r1_cpa_summary_counts_only_active_linked_metadata_with_lease_held() {
        let (_dir, p) = paths();
        for (label, mode, state) in [
            ("one", "linked", "active"),
            ("two", "linked", "active"),
            ("paused", "linked", "paused"),
            ("disabled", "linked", "disabled"),
            ("owned", "owned", "active"),
            ("adopted", "adopted", "active"),
        ] {
            account(&p.qb_home, label, mode, state, json!({"kind":"ready"}));
        }
        let _lease = AccountStore::acquire(&p.qb_home).unwrap();
        let report = wire_with_task_query(
            &p,
            false,
            WireOptions {
                codex_mode: Some(CodexMode::Default),
                ..WireOptions {
                    claude_mode: Some(ClaudeMode::Default),
                    ..WireOptions::default()
                }
            },
            &|_| panic!("flagless wire queried tasks"),
        )
        .unwrap();
        assert_eq!(
            report
                .summary
                .iter()
                .filter(|line| line.as_str()
                    == "CPA는 계속 실행되어야 합니다 (연결 계정 2개의 토큰 갱신)")
                .count(),
            1
        );
        assert!(!p.client_key.exists());
        assert!(!p.journal().exists());
    }

    #[test]
    fn r1_cpa_invalid_metadata_fails_closed_before_task_query() {
        let (_dir, p) = paths();
        let record = account(
            &p.qb_home,
            "broken",
            "linked",
            "active",
            json!({"kind":"ready"}),
        );
        fs::write(record, b"not json").unwrap();
        for apply in [false, true] {
            assert!(
                wire_with_task_query(&p, apply, disable_options(), &|_| panic!(
                    "task query after unreadable metadata"
                ))
                .is_err()
            );
        }
        assert!(
            check_optional_preconditions_with_task_query(
                &p.qb_home,
                &disable_options(),
                &|_| panic!("task query after unreadable metadata")
            )
            .is_err()
        );
    }
    fn paths() -> (tempfile::TempDir, CutoverPaths) {
        let dir = scratch();
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
                qb_home: qb_home.clone(),
                codex_auth: auth,
                codex_session_auth: Vec::new(),
                codex_session_homes: Vec::new(),
                session_homes_file: None,
                session_homes_warning: None,
                codex_qb_home: qb_home.parent().unwrap().join(".codex-acct/qb"),
                npm_dir: qb_home.parent().unwrap().join("npm"),
                codex_config: codex,
                codex_realtime_items: Vec::new(),
                claude_command: None,
                claude_settings: claude,
                client_key,
                report_roots: Vec::new(),
                scan_roots: Vec::new(),
            },
        )
    }
}
