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

const JOURNAL_ENTROPY: &[u8] = b"quota-baton/v1/wiring-journal";

#[derive(Clone, Debug)]
pub struct CutoverPaths {
    pub qb_home: PathBuf,
    pub codex_auth: PathBuf,
    pub codex_session_auth: Vec<PathBuf>,
    codex_session_homes: Vec<SessionHome>,
    pub session_homes_file: Option<PathBuf>,
    session_homes_warning: Option<String>,
    pub codex_config: PathBuf,
    pub claude_settings: PathBuf,
    pub client_key: PathBuf,
    /// Injectable report roots; empty in synthetic tests to prevent ambient
    /// profile inspection.
    pub report_roots: Vec<PathBuf>,
    scan_roots: Vec<(String, PathBuf)>,
}

impl CutoverPaths {
    pub fn from_environment(qb_home: PathBuf) -> Result<Self> {
        let user = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .context("USERPROFILE/HOME is missing")?;
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
    String::from_utf8(bytes.to_vec()).context("configuration is not UTF-8")
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

#[derive(Clone, Copy, Debug)]
pub struct WireOptions {
    pub data_port: u16,
    pub autostart: bool,
    pub disable_cpa_tasks: bool,
    pub detach_chatgpt_session_homes: bool,
}

impl Default for WireOptions {
    fn default() -> Self {
        Self {
            data_port: 10200,
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

fn redacted_diff_line(line: &str, secrets: &[String]) -> String {
    let mut line = line.to_owned();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        line = line.replace(secret, "<redacted>");
    }
    let lower = line.to_ascii_lowercase();
    let replacement = if line.contains("<client key>") {
        Some("<client key>")
    } else if lower.contains("openai_api_key")
        || lower.contains("api_key")
        || lower.contains("experimental_bearer_token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("authorization")
        || lower.contains("bearer")
    {
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
        let delimiter = line.find('=').or_else(|| line.find(':'));
        if let Some(index) = delimiter {
            let whitespace = line[index + 1..]
                .chars()
                .take_while(|character| character.is_whitespace())
                .collect::<String>();
            // Keep the diff parseable while replacing the complete value.  Both
            // TOML and JSON use quoted string values for the secrets we wire;
            // quoting the marker also prevents a TOML line without `:` from
            // falling through to a verbatim secret-bearing line.
            let replacement = format!("\"{replacement}\"");
            let suffix = line
                .chars()
                .rev()
                .take_while(|character| matches!(character, ',' | '}' | ']'))
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            return format!("{}{}{}{}", &line[..=index], whitespace, replacement, suffix);
        }
        return replacement.into();
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
    let mut value: Value = serde_json::from_str(&text).context("parse session-homes.json")?;
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

fn set_autostart(action: &OptionalActionJournal) -> Result<()> {
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

fn task_enabled(name: &str) -> Option<bool> {
    let output = std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", name, "/V", "/FO", "LIST"])
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
            diff_secrets
                .push(String::from_utf8(unprotect(encoded)?).context("invalid journal secret")?);
        }
    }
    for provider in &providers {
        if let Some(encoded) = provider.experimental_bearer_token.protected_b64.as_deref() {
            diff_secrets
                .push(String::from_utf8(unprotect(encoded)?).context("invalid journal secret")?);
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
    for path in &paths.codex_session_auth {
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
        cpa_task_journal()
    } else {
        Vec::new()
    };
    let session_homes_preview = if options.detach_chatgpt_session_homes {
        paths
            .session_homes_file
            .as_deref()
            .filter(|path| path.exists())
            .map(|path| detach_chatgpt_session_homes(path, &paths.codex_session_homes))
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
        session_homes_after: session_homes_preview
            .as_ref()
            .map(|bytes| hash_bytes(bytes)),
        session_homes_original,
    };
    let mut warnings = paths
        .codex_session_homes
        .iter()
        .map(|home| {
            let mode = read_text(&home.auth)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|auth| auth.get("auth_mode").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "missing".into());
            if mode == "apikey" {
                format!(
                    "Codex 세션 홈: {} (auth_mode={}, 세션 {}개, 연결 대상)",
                    shorten_path(&home.home), mode, home.sessions
                )
            } else {
                let consequence = if home.shim_exempt {
                    "herdr shim이 이 홈에는 proxy override를 적용하지 않아 직접 ChatGPT로 연결됩니다"
                } else {
                    "herdr shim을 통해 자체 ChatGPT 토큰으로 quota-baton에 연결되어 재개 시 401이 됩니다"
                };
                format!(
                    "Codex 세션 홈: {} (auth_mode={}, 세션 {}개). {}. 그대로 두거나 session-homes.json에서 분리하거나 CPA를 계속 실행하는 선택이 필요합니다{}",
                    shorten_path(&home.home),
                    mode,
                    home.sessions,
                    consequence,
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
            if task_enabled(name).is_none() {
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
    let report = Report {
        summary: vec![
            format!("요약: 연결 대상 {}개 파일", changed.len()),
            format!(
                "CPA에 남는 세션 홈: {}개",
                paths
                    .codex_session_homes
                    .iter()
                    .filter(|home| {
                        read_text(&home.auth)
                            .ok()
                            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                            .and_then(|auth| {
                                auth.get("auth_mode")
                                    .and_then(Value::as_str)
                                    .map(|mode| mode != "apikey")
                            })
                            .unwrap_or(true)
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
        ],
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
            apply_claude(&paths.claude_settings, &claude_base, &key)
        });
    if let Err(error) = apply_result {
        bail!(
            "적용 중 실패했습니다: {error}. 변경된 파일은 journal에 기록되어 있습니다: {}. 복구 명령: quota-baton unwire --apply",
            paths.journal().display()
        );
    }
    if options.autostart {
        if let Err(error) = set_autostart(&autostart.expect("autostart journal")) {
            bail!(
                "자동 시작 설정 중 실패했습니다: {error}. 변경된 파일은 journal에 기록되어 있습니다: {}. 복구 명령: quota-baton unwire --apply",
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
            bail!(
                "CPA 예약 작업 처리 중 실패했습니다: {error}. 변경된 파일은 journal에 기록되어 있습니다: {}. 복구 명령: quota-baton unwire --apply",
                paths.journal().display()
            );
        }
        for task in &mut journal.cpa_tasks {
            if let Some(enabled) = task_enabled(&task.name) {
                task.after_enabled = enabled;
            }
        }
        atomic_json(&paths.journal(), &journal)?;
    }
    if let (Some(path), Some(bytes)) = (&paths.session_homes_file, &session_homes_preview) {
        atomic_preserved(path, bytes)?;
    }
    Ok(report)
}

pub fn report_only(paths: &CutoverPaths) -> Report {
    let mut warnings = scan_other_clients_10100(&paths.scan_roots);
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
            if !path.exists() {
                report.warnings.push(format!(
                    "세션 auth.json이 없어 복구를 건너뜁니다: {}",
                    shorten_path(path)
                ));
                continue;
            }
            if hash_file(path)? != session.after {
                bail!("파일이 변경되어 되돌릴 수 없습니다: {}", path.display());
            }
        }
        if let Some(autostart) = &journal.autostart {
            let current = read_autostart()?.0;
            if current.value != Some(autostart.after.clone()) {
                bail!("autostart 설정이 변경되어 되돌릴 수 없습니다");
            }
        }
        for task in &journal.cpa_tasks {
            if task_enabled(&task.name) != Some(task.after_enabled) {
                bail!("{} 예약 작업이 변경되어 되돌릴 수 없습니다", task.name);
            }
        }
        if let (Some(path), Some(expected)) = (
            journal.session_homes_path.as_deref().map(Path::new),
            journal.session_homes_after.as_deref(),
        ) && hash_file(path)? != expected
        {
            bail!("session-homes.json이 변경되어 되돌릴 수 없습니다");
        }
    }
    let config_exact = journal.codex_config_original.as_deref().is_some_and(|_| {
        hash_file(&paths.codex_config).ok().as_deref() == Some(journal.codex_config_after.as_str())
    });
    if config_exact {
        restore_exact(
            &paths.codex_config,
            journal.codex_config_original.as_deref().unwrap(),
        )?;
    } else {
        restore_codex_config(&paths.codex_config, &journal)?;
    }
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
    for session in &journal.codex_sessions {
        let path = Path::new(&session.path);
        if !path.exists() {
            report.warnings.push(format!(
                "세션 auth.json이 없어 복구를 건너뜁니다: {}",
                shorten_path(path)
            ));
        } else if session.original.is_some()
            && hash_file(path).ok().as_deref() == Some(session.after.as_str())
        {
            restore_exact(path, session.original.as_deref().unwrap())?;
        } else {
            restore_session(path, session)?;
        }
    }
    if let Some(autostart) = &journal.autostart {
        restore_autostart(&autostart.before, autostart.before_type)?;
    }
    restore_cpa_tasks(&journal.cpa_tasks)?;
    if let (Some(path), Some(original)) = (
        journal.session_homes_path.as_deref().map(Path::new),
        journal.session_homes_original.as_deref(),
    ) {
        restore_exact(path, original)?;
    }
    fs::remove_file(paths.journal()).context("remove wiring journal")?;
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
    let text = value.as_str().context("secret setting is not a string")?;
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
        .context("parse Codex config")?;
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
        .context("Codex provider secret is not a string")?;
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
fn restore_codex_config(path: &Path, journal: &Journal) -> Result<()> {
    let mut doc = read_text(path)?
        .parse::<DocumentMut>()
        .context("parse Codex config")?;
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
            .context("missing encrypted provider secret")?;
        let value = String::from_utf8(unprotect(encoded)?).context("invalid provider secret")?;
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
    if let Some(updated) = replace_json_string(&text, "ANTHROPIC_BASE_URL", base)
        .and_then(|updated| replace_json_string(&updated, "ANTHROPIC_AUTH_TOKEN", key))
    {
        return Ok(updated.into_bytes());
    }
    let mut json: Value = serde_json::from_str(&text)?;
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

fn replace_json_string(text: &str, key: &str, value: &str) -> Option<String> {
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
                let _ = fs::remove_file(&tmp);
                return Err(error).with_context(|| {
                    format!("파일을 원자적으로 쓰지 못했습니다: {}", path.display())
                });
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
                codex_session_homes: Vec::new(),
                session_homes_file: None,
                session_homes_warning: None,
                codex_config: codex,
                claude_settings: claude,
                client_key,
                report_roots: Vec::new(),
                scan_roots: Vec::new(),
            },
        )
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
    fn pretty_json_files_keep_layout_and_unrelated_order_when_wired() {
        let (_dir, p) = paths();
        let settings = r#"{
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
                ..WireOptions::default()
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
                ..WireOptions::default()
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
        let (_dir, p) = paths();
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
