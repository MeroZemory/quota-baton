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
        .filter(|path| path.exists())
        .collect()
}

fn scan_other_clients_10100() -> Vec<String> {
    let Some(user) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for root in [user.join(".pi"), user.join(".omp"), user.join(".config")] {
        if !root.exists() {
            continue;
        }
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
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
}

type CodexOriginals = (Original, Original, Original);
type ClaudeOriginals = (Original, SecretOriginal);

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub changed: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn wire(paths: &CutoverPaths, apply: bool) -> Result<Report> {
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
    let codex_config_after = hash_bytes(&codex_config_bytes(&paths.codex_config, CODEX_BASE_URL)?);
    let codex_auth_after = hash_bytes(&codex_auth_bytes(&paths.codex_auth, preview_key)?);
    let claude_settings_after = hash_bytes(&claude_bytes(
        &paths.claude_settings,
        CLAUDE_BASE_URL,
        preview_key,
    )?);
    let mut codex_sessions = Vec::new();
    for path in &paths.codex_session_auth {
        if path.exists()
            && let Some(session) = session_journal(path, preview_key)?
        {
            codex_sessions.push(session);
        }
    }
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
    };
    let mut warnings = paths
        .codex_session_auth
        .iter()
        .filter(|path| path.exists())
        .filter_map(|path| {
            let auth = read_text(path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
            (auth.get("auth_mode").and_then(Value::as_str) != Some("apikey")).then(|| {
                format!(
                    "Codex 세션 홈은 ChatGPT 인증이라 변경하지 않습니다: {}",
                    path.display()
                )
            })
        })
        .collect::<Vec<_>>();
    warnings.extend(scan_other_clients_10100());
    let report = Report {
        changed: vec![
            "codex.openai_base_url".into(),
            "codex.openai_api_key".into(),
            "codex.auth.json OPENAI_API_KEY".into(),
            "codex.model_catalog_json (remove)".into(),
            "claude.env.ANTHROPIC_BASE_URL".into(),
            "claude.env.ANTHROPIC_AUTH_TOKEN".into(),
        ],
        warnings,
    };
    if !apply {
        return Ok(report);
    }

    let key = key.expect("apply key");
    atomic_json(&paths.journal(), &journal)?;
    // The journal is intentionally retained if either write fails: it is the
    // recovery record for a partially completed cutover.
    apply_codex(&paths.codex_config, &paths.codex_auth, CODEX_BASE_URL, &key).and_then(|_| {
        for path in &paths.codex_session_auth {
            if path.exists() {
                apply_session(path, &key)?;
            }
        }
        apply_claude(&paths.claude_settings, CLAUDE_BASE_URL, &key)
    })?;
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
            "codex.openai_api_key".into(),
            "codex.auth.json OPENAI_API_KEY".into(),
            "codex.model_catalog_json (restore)".into(),
            "claude.env.ANTHROPIC_BASE_URL".into(),
            "claude.env.ANTHROPIC_AUTH_TOKEN".into(),
        ],
        warnings: Vec::new(),
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
    }
    restore_codex(&paths.codex_config, &paths.codex_auth, &journal)?;
    for session in &journal.codex_sessions {
        restore_session(Path::new(&session.path), session)?;
    }
    restore_claude(&paths.claude_settings, &journal)?;
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
    let after = hash_bytes(&codex_auth_bytes(path, key)?);
    Ok(Some(SessionJournal {
        path: path.display().to_string(),
        before,
        after,
        auth_mode: mode,
        api_key,
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
    atomic_bytes(path, &output)
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
        let codex = fs::read_to_string(&p.codex_config).unwrap();
        let claude = fs::read_to_string(&p.claude_settings).unwrap();
        assert!(codex.contains("old-url") && codex.contains("keep = 'yes'"));
        assert!(claude.contains("old-key") && claude.contains("\"keep\": true"));
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
}
