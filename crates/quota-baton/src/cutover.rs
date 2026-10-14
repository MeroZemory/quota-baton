//! Synthetic-testable, reversible client wiring for the M8 cutover.
//!
//! This module deliberately keeps the client paths injectable.  The command
//! line uses the normal user locations, while tests use only temporary files.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use toml_edit::DocumentMut;

const CODEX_BASE_URL: &str = "http://127.0.0.1:10200/v1";
const CLAUDE_BASE_URL: &str = "http://127.0.0.1:10200";

#[derive(Clone, Debug)]
pub struct CutoverPaths {
    pub qb_home: PathBuf,
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
        Ok(Self {
            qb_home,
            codex_config: user.join(".codex").join("config.toml"),
            claude_settings: user.join(".claude").join("settings.json"),
            client_key,
        })
    }

    fn journal(&self) -> PathBuf {
        self.qb_home.join("journal").join("wiring.json")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Original {
    present: bool,
    value: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    schema: u8,
    codex_config: String,
    claude_settings: String,
    codex_base_url: Original,
    codex_api_key: Original,
    codex_model_catalog: Original,
    claude_base_url: Original,
    claude_api_key: Original,
}

type CodexOriginals = (Original, Original, Original);
type ClaudeOriginals = (Original, Original);

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub changed: Vec<String>,
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
    let (codex, claude) = read_inputs(paths)?;
    let journal = Journal {
        schema: 1,
        codex_config: paths.codex_config.display().to_string(),
        claude_settings: paths.claude_settings.display().to_string(),
        codex_base_url: codex.0.clone(),
        codex_api_key: codex.1.clone(),
        codex_model_catalog: codex.2.clone(),
        claude_base_url: claude.0.clone(),
        claude_api_key: claude.1.clone(),
    };
    let report = Report {
        changed: vec![
            "codex.openai_base_url".into(),
            "codex.openai_api_key".into(),
            "codex.model_catalog_json (remove)".into(),
            "claude.env.ANTHROPIC_BASE_URL".into(),
            "claude.env.ANTHROPIC_API_KEY".into(),
        ],
    };
    if !apply {
        return Ok(report);
    }

    let key = key.expect("apply key");
    atomic_json(&paths.journal(), &journal)?;
    // The journal is intentionally retained if either write fails: it is the
    // recovery record for a partially completed cutover.
    apply_codex(&paths.codex_config, CODEX_BASE_URL, &key)
        .and_then(|_| apply_claude(&paths.claude_settings, CLAUDE_BASE_URL, &key))?;
    Ok(report)
}

pub fn unwire(paths: &CutoverPaths, apply: bool) -> Result<Report> {
    let journal: Journal =
        serde_json::from_slice(&fs::read(paths.journal()).context("read wiring journal")?)
            .context("parse wiring journal")?;
    if journal.codex_config != paths.codex_config.display().to_string()
        || journal.claude_settings != paths.claude_settings.display().to_string()
    {
        bail!("wiring journal paths do not match this home")
    }
    let report = Report {
        changed: vec![
            "codex.openai_base_url".into(),
            "codex.openai_api_key".into(),
            "codex.model_catalog_json (restore)".into(),
            "claude.env.ANTHROPIC_BASE_URL".into(),
            "claude.env.ANTHROPIC_API_KEY".into(),
        ],
    };
    if !apply {
        return Ok(report);
    }
    restore_codex(&paths.codex_config, &journal)?;
    restore_claude(&paths.claude_settings, &journal)?;
    fs::remove_file(paths.journal()).context("remove wiring journal")?;
    Ok(report)
}

fn read_inputs(paths: &CutoverPaths) -> Result<(CodexOriginals, ClaudeOriginals)> {
    let codex_text = fs::read_to_string(&paths.codex_config).context("read Codex config")?;
    let doc = codex_text
        .parse::<DocumentMut>()
        .context("parse Codex config")?;
    let codex = (
        original_toml(doc.get("openai_base_url")),
        original_toml(doc.get("openai_api_key")),
        original_toml(doc.get("model_catalog_json")),
    );
    let claude_text = fs::read_to_string(&paths.claude_settings).context("read Claude settings")?;
    let json: Value = serde_json::from_str(&claude_text).context("parse Claude settings")?;
    let env = json.get("env").and_then(Value::as_object);
    let claude = (
        original_json(env.and_then(|m| m.get("ANTHROPIC_BASE_URL"))),
        original_json(env.and_then(|m| m.get("ANTHROPIC_API_KEY"))),
    );
    Ok((codex, claude))
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
fn read_key(path: &Path) -> Result<String> {
    let key = fs::read_to_string(path).context("read client key")?;
    let key = key.trim();
    if key.is_empty() {
        bail!("client key is empty")
    }
    Ok(key.to_owned())
}

fn apply_codex(path: &Path, base: &str, key: &str) -> Result<()> {
    let mut doc = fs::read_to_string(path)?
        .parse::<DocumentMut>()
        .context("parse Codex config")?;
    doc["openai_base_url"] = toml_edit::value(base);
    doc["openai_api_key"] = toml_edit::value(key);
    doc.remove("model_catalog_json");
    atomic_bytes(path, doc.to_string().as_bytes())
}
fn restore_codex(path: &Path, journal: &Journal) -> Result<()> {
    let mut doc = fs::read_to_string(path)?
        .parse::<DocumentMut>()
        .context("parse Codex config")?;
    restore_toml(&mut doc, "openai_base_url", &journal.codex_base_url);
    restore_toml(&mut doc, "openai_api_key", &journal.codex_api_key);
    restore_toml(&mut doc, "model_catalog_json", &journal.codex_model_catalog);
    atomic_bytes(path, doc.to_string().as_bytes())
}
fn restore_toml(doc: &mut DocumentMut, key: &str, original: &Original) {
    if original.present {
        doc[key] = toml_edit::value(original.value.as_deref().unwrap_or_default());
    } else {
        doc.remove(key);
    }
}
fn apply_claude(path: &Path, base: &str, key: &str) -> Result<()> {
    let mut json: Value = serde_json::from_slice(&fs::read(path)?)?;
    let env = json
        .as_object_mut()
        .context("Claude settings root is not an object")?
        .entry("env")
        .or_insert_with(|| Value::Object(Default::default()));
    let env = env
        .as_object_mut()
        .context("Claude settings env is not an object")?;
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(base.into()));
    env.insert("ANTHROPIC_API_KEY".into(), Value::String(key.into()));
    atomic_json(path, &json)
}
fn restore_claude(path: &Path, journal: &Journal) -> Result<()> {
    let mut json: Value = serde_json::from_slice(&fs::read(path)?)?;
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
    restore_json(env, "ANTHROPIC_API_KEY", &journal.claude_api_key);
    if env.is_empty() {
        root.remove("env");
    }
    atomic_json(path, &json)
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
        let claude = dir.path().join("settings.json");
        fs::write(&codex, "openai_base_url = 'old-url'\nkeep = 'yes'\n").unwrap();
        fs::write(
            &claude,
            r#"{"env":{"ANTHROPIC_API_KEY":"old-key"},"keep":true}"#,
        )
        .unwrap();
        let qb_home = dir.path().join("qb");
        let client_key = dir.path().join("key");
        (
            dir,
            CutoverPaths {
                qb_home,
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
        unwire(&p, true).unwrap();
        let codex = fs::read_to_string(&p.codex_config).unwrap();
        let claude = fs::read_to_string(&p.claude_settings).unwrap();
        assert!(codex.contains("old-url") && codex.contains("keep = 'yes'"));
        assert!(claude.contains("old-key") && claude.contains("\"keep\": true"));
        assert!(!p.journal().exists());
    }
}
