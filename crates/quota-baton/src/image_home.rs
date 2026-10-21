//! Qi uses the dedicated journal's publication, recovery and removal machinery.
use super::*;

pub(super) fn path(home: &Path) -> Result<PathBuf> {
    let image = home
        .parent()
        .context("Qi 부모 경로가 없습니다")?
        .join("qb-images");
    if image == home {
        bail!("Q와 Qi 홈 경로가 같습니다");
    }
    Ok(image)
}

pub(super) fn allowed(m: &Manifest, candidate: &Path) -> bool {
    m.mode == CodexMode::Dedicated
        && m.image_home.as_ref().is_some_and(|home| {
            candidate == home
                || ["log", "config.toml", "AGENTS.md", "hooks.json"]
                    .iter()
                    .chain(SHARED.iter())
                    .any(|name| candidate == home.join(name))
        })
}

pub(super) fn validate_metadata(m: &Manifest) -> Result<()> {
    if m.image_install_pending && (m.image_home.is_none() || m.install_completed != Some(false)) {
        bail!("Qi 확장 완료 상태가 올바르지 않습니다");
    }
    if let Some(home) = &m.image_home {
        if m.mode != CodexMode::Dedicated || *home != path(&m.home)? {
            bail!("Qi journal 경로/모드가 올바르지 않습니다");
        }
        // Metadata alone never proves ownership of an already-existing home.
        for required in [home.clone(), home.join("log"), home.join("config.toml")] {
            if !m.entries.iter().any(|e| e.path == required) {
                bail!("Qi 소유권 journal 항목이 없습니다");
            }
        }
    }
    Ok(())
}

fn parse(bytes: &[u8]) -> Result<DocumentMut> {
    std::str::from_utf8(strip_utf8_bom(bytes))
        .ok()
        .and_then(|text| text.parse().ok())
        .context("Qi 원본 config 형식이 올바르지 않습니다")
}

fn enable_images(doc: &mut DocumentMut) -> Result<()> {
    if !doc.contains_key("features") {
        doc["features"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc.get_mut("features")
        .and_then(toml_edit::Item::as_table_like_mut)
        .context("Qi features 형식이 올바르지 않습니다")?
        .insert("image_generation", toml_edit::value(true));
    Ok(())
}

pub(super) fn config_bytes(q_config: &[u8], q_home: &Path) -> Result<Vec<u8>> {
    let mut doc = parse(q_config)?;
    let home = path(q_home)?;
    let endpoint = doc
        .get("openai_base_url")
        .and_then(toml_edit::Item::as_str)
        .context("Q endpoint가 없습니다")?
        .to_owned();
    if doc
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        .is_some_and(|provider| provider != "openai")
    {
        bail!("Q provider가 openai가 아닙니다");
    }
    if !doc.contains_key("model_providers") {
        doc["model_providers"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let providers = doc
        .get_mut("model_providers")
        .and_then(toml_edit::Item::as_table_like_mut)
        .context("Q model_providers 형식이 올바르지 않습니다")?;
    // Replace the entire provider, including any inherited bearer token.
    let mut provider = toml_edit::Table::new();
    for (key, value) in [
        ("name", "OpenAI"),
        ("base_url", endpoint.as_str()),
        ("env_key", "QB_IMAGE_CLIENT_KEY"),
        ("wire_api", "responses"),
    ] {
        provider[key] = toml_edit::value(value);
    }
    provider["supports_websockets"] = toml_edit::value(true);
    provider["requires_openai_auth"] = toml_edit::value(false);
    provider["http_headers"]["x-openai-actor-authorization"] = toml_edit::value("qb-local");
    // Explicit parity with the current shared-home study.
    provider["capabilities"]["remote_compaction"] = toml_edit::value("v2");
    providers.insert("qb", toml_edit::Item::Table(provider));
    doc["model_provider"] = toml_edit::value("qb");
    enable_images(&mut doc)?;
    doc["log_dir"] = toml_edit::value(home.join("log").to_string_lossy().as_ref());
    let source_hook = format!("{}:session_start:0:0", q_home.join("hooks.json").display());
    let trust = doc
        .get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(|state| state.get(source_hook.as_str()))
        .and_then(|state| state.get("trusted_hash"))
        .cloned();
    if let Some(hash) = trust {
        let hook = format!("{}:session_start:0:0", home.join("hooks.json").display());
        doc["hooks"]["state"][hook.as_str()]["trusted_hash"] = hash;
    }
    dedicated_codex_title_bytes(doc.to_string().as_bytes())
}

/// Apply Qi's local edits over newly generated Q settings. The caller validates
/// the controlled connection fields before publication; no-op saves stay unowned.
pub(super) fn sync_config(
    q_config: &[u8],
    current: &[u8],
    saved: &[u8],
    q_home: &Path,
) -> Result<Vec<u8>> {
    let generated = config_bytes(q_config, q_home)?;
    if generated == saved {
        // No inherited change: preserve a native atomic save byte for byte.
        return dedicated_codex_title_bytes(current);
    }
    let mut desired = parse(&generated)?;
    let current = parse(current)?;
    let saved = parse(saved)?;
    fn unchanged(current: &toml_edit::Item, saved: &toml_edit::Item) -> bool {
        match (current.as_table_like(), saved.as_table_like()) {
            // Table Display omits ordinary subtables, including project trust.
            (Some(current), Some(saved)) => {
                current.len() == saved.len()
                    && current
                        .iter()
                        .all(|(key, value)| saved.get(key).is_some_and(|old| unchanged(value, old)))
            }
            (None, None) => current.to_string() == saved.to_string(),
            _ => false,
        }
    }
    fn merge(desired: &mut toml_edit::Item, current: &toml_edit::Item, saved: &toml_edit::Item) {
        if unchanged(current, saved) {
            return;
        }
        if let (Some(current), Some(saved)) = (current.as_table_like(), saved.as_table_like()) {
            if desired.as_table_like().is_none() {
                *desired = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let desired = desired.as_table_like_mut().unwrap();
            for (key, _) in saved.iter() {
                if !current.contains_key(key) {
                    desired.remove(key);
                }
            }
            for (key, value) in current.iter() {
                if saved.get(key).is_some_and(|old| unchanged(value, old)) {
                    continue;
                }
                // A new Qi table can coexist with new Q entries in that table.
                // A table removed from Q must not resurrect its unchanged rows.
                let empty = toml_edit::Item::Table(toml_edit::Table::new());
                let original = saved.get(key).unwrap_or(&empty);
                if let Some(destination) = desired.get_mut(key) {
                    merge(destination, value, original);
                } else {
                    let mut destination = empty.clone();
                    merge(&mut destination, value, original);
                    desired.insert(key, destination);
                }
            }
        } else {
            *desired = current.clone();
        }
    }
    merge(desired.as_item_mut(), current.as_item(), saved.as_item());
    enable_images(&mut desired)?;
    dedicated_codex_title_bytes(desired.to_string().as_bytes())
}

pub(super) fn validate_config(m: &Manifest, bytes: &[u8]) -> Result<()> {
    let entry = m
        .entries
        .iter()
        .find(|e| e.path == m.home.join("config.toml"))
        .context("Q config journal이 없습니다")?;
    let saved = unprotect(
        entry
            .payload
            .as_deref()
            .context("Q config payload가 없습니다")?,
    )?;
    let expected = parse(&config_bytes(&saved, &m.home)?)?;
    let actual = parse(bytes)?;
    if actual
        .get("features")
        .and_then(|features| features.get("image_generation"))
        .and_then(toml_edit::Item::as_bool)
        != Some(true)
    {
        bail!("Qi image_generation 기능이 활성화되어 있지 않습니다");
    }
    for key in ["model_provider", "openai_base_url", "log_dir"] {
        if actual.get(key).and_then(toml_edit::Item::as_str)
            != expected.get(key).and_then(toml_edit::Item::as_str)
        {
            bail!("Qi 연결 설정이 journal과 다릅니다");
        }
    }
    // Compare controlled fields recursively, independent of TOML formatting.
    fn equal(a: &toml_edit::Item, b: &toml_edit::Item) -> bool {
        if let Some(value) = b.as_str() {
            return a.as_str() == Some(value);
        }
        if let Some(value) = b.as_bool() {
            return a.as_bool() == Some(value);
        }
        match (a.as_table_like(), b.as_table_like()) {
            (Some(a), Some(b)) => {
                a.len() == b.len()
                    && b.iter()
                        .all(|(key, value)| a.get(key).is_some_and(|actual| equal(actual, value)))
            }
            _ => false,
        }
    }
    let a = actual.get("model_providers").and_then(|p| p.get("qb"));
    let b = expected.get("model_providers").and_then(|p| p.get("qb"));
    if !a.zip(b).is_some_and(|(a, b)| equal(a, b)) {
        bail!("Qi provider 설정이 journal과 다릅니다");
    }
    Ok(())
}

pub(super) fn plan(
    paths: &CutoverPaths,
    existing: &[Entry],
    q_config: &[u8],
    apply: bool,
) -> Result<(PathBuf, Vec<Entry>)> {
    let home = path(&paths.codex_qb_home)?;
    // A retained, journal-owned directory is the only adoption path.
    if exists(&home)?
        && !existing.iter().any(|e| {
            e.path == home
                && e.kind == Kind::Directory
                && e.done
                && !e.removed
                && matches_entry(e, false).unwrap_or(false)
        })
    {
        bail!("Qi 홈이 이미 존재하며 journal 소유가 아닙니다. 덮어쓰지 않습니다");
    }
    let config = config_bytes(q_config, &paths.codex_qb_home)?;
    let mut entries = Vec::new();
    if !exists(&home)? {
        entries.push(new_entry(home.clone(), Kind::Directory, None, None, apply)?);
    }
    // Retained log directories must retain their identity rather than be adopted.
    let log = home.join("log");
    if !existing.iter().any(|e| {
        e.path == log
            && e.kind == Kind::Directory
            && e.done
            && !e.removed
            && matches_entry(e, false).unwrap_or(false)
    }) {
        entries.push(new_entry(log, Kind::Directory, None, None, apply)?);
    }
    let mut config_entry = new_entry(
        home.join("config.toml"),
        Kind::File,
        None,
        Some(&config),
        apply,
    )?;
    if apply {
        config_entry.image_base = Some(protect(&config)?);
    }
    entries.push(config_entry);
    // Reuse Q's verified sharing plan, never read/copy source credentials.
    for source in existing.iter().filter(|e| {
        e.path.parent() == Some(paths.codex_qb_home.as_path())
            && matches!(e.kind, Kind::Junction | Kind::Hardlink)
    }) {
        let mut entry = new_entry(
            home.join(source.path.file_name().context("공유 이름이 없습니다")?),
            source.kind.clone(),
            source.target.clone(),
            None,
            apply,
        )?;
        entry.hash = source.hash.clone();
        entry.identity = source.identity.clone();
        entries.push(entry);
    }
    for entry in &entries {
        ancestors_safe(&entry.path)?;
        if exists(&entry.path)? || exists(&entry.path.with_extension("qb-pending"))? {
            bail!("Qi 생성 경로가 이미 존재합니다: {}", entry.path.display());
        }
    }
    Ok((home, entries))
}
