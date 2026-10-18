//! R3 implementation, deliberately private behind the cutover module boundary.
use super::*;
use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};

const SHARED: &[&str] = &[
    "agents",
    "skills",
    "rules",
    "plugins",
    "mcp",
    "hooks",
    "memories",
    "vendor_imports",
];
const USAGE: &str = "`codex-qb`로 시작한 세션만 quota-baton을 거칩니다. 기존 `codex`·`codex-ib`·`codex-mz`·`codex-cpa`는 바뀌지 않습니다.";
const HERDR: &str = "herdr 창에서 `codex-qb` 세션을 재시작 후 이어 열려면 그 세션 ID를 `session-homes.json`에 `~/.codex-acct/qb`로 등록해야 합니다.";

pub(super) fn is_cpa_endpoint(url: &str) -> bool {
    // Deliberately small, strict authority parser; no DNS or ambient state.
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]"
    ) && port == "10100"
}

fn parse_config(paths: &CutoverPaths) -> Result<DocumentMut> {
    read_text(&paths.codex_config)?
        .parse()
        .map_err(|_| anyhow::anyhow!("Codex config 형식이 올바르지 않습니다"))
}

fn is_cpa_realtime_endpoint(url: &str) -> bool {
    is_cpa_endpoint(url)
        || url
            .strip_prefix("ws://")
            .is_some_and(|rest| is_cpa_endpoint(&format!("http://{rest}")))
        || url
            .strip_prefix("wss://")
            .is_some_and(|rest| is_cpa_endpoint(&format!("https://{rest}")))
}

/// R3.2 dedicated-only merge. Also usable on CURRENT journal-owned file bytes.
/// Never regenerate upgrade inputs from the default home.
pub(super) fn dedicated_codex_title_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(strip_utf8_bom(bytes))
        .map_err(|_| anyhow::anyhow!("전용 Codex config 인코딩이 올바르지 않습니다"))?;
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("전용 Codex config 형식이 올바르지 않습니다"))?;
    if !doc.contains_key("tui") {
        doc["tui"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let tui = doc
        .get_mut("tui")
        .and_then(toml_edit::Item::as_table_like_mut)
        .context("전용 Codex config의 tui가 테이블이 아닙니다")?;
    if tui
        .get("terminal_title")
        .and_then(toml_edit::Item::as_array)
        .is_some_and(|array| array.is_empty())
    {
        return Ok(bytes.to_vec());
    }
    let mut title = toml_edit::value(toml_edit::Array::new());
    if let Some(old) = tui
        .get("terminal_title")
        .and_then(toml_edit::Item::as_value)
    {
        *title.as_value_mut().expect("array value").decor_mut() = old.decor().clone();
    }
    tui.insert("terminal_title", title);
    let mut result = Vec::new();
    if bytes.starts_with(b"\xef\xbb\xbf") {
        result.extend_from_slice(b"\xef\xbb\xbf");
    }
    result.extend_from_slice(doc.to_string().as_bytes());
    Ok(result)
}

/// Patch only the owned root.env member; preserve all other bytes and ordering.
pub(super) fn dedicated_claude_title_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let invalid = || anyhow::anyhow!("전용 Claude settings 형식이 올바르지 않습니다");
    let text = std::str::from_utf8(strip_utf8_bom(bytes)).map_err(|_| invalid())?;
    let mut expected: Value = serde_json::from_str(text).map_err(|_| invalid())?;
    let root = expected
        .as_object_mut()
        .context("전용 Claude settings가 객체가 아닙니다")?;
    let env = root
        .entry("env")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("전용 Claude settings의 env가 객체가 아닙니다")?;
    if env
        .get("CLAUDE_CODE_DISABLE_TERMINAL_TITLE")
        .and_then(Value::as_str)
        == Some("1")
    {
        return Ok(bytes.to_vec());
    }
    env.insert(
        "CLAUDE_CODE_DISABLE_TERMINAL_TITLE".into(),
        Value::String("1".into()),
    );
    let root_start = text
        .bytes()
        .position(|byte| !byte.is_ascii_whitespace())
        .ok_or_else(invalid)?;
    let members = json_member_spans(&text[root_start..]).ok_or_else(invalid)?;
    let mut patched = text.to_owned();
    if let Some(member) = members.iter().find(|member| member.0 == "env") {
        let start = root_start + member.2;
        let object = &text[start..root_start + member.3];
        let env_members = json_member_spans(object).ok_or_else(invalid)?;
        if let Some(member) = env_members
            .iter()
            .find(|member| member.0 == "CLAUDE_CODE_DISABLE_TERMINAL_TITLE")
        {
            patched.replace_range(start + member.2..start + member.3, "\"1\"");
        } else {
            let insertion = format!(
                "{}\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"1\"",
                if env_members.is_empty() { "" } else { "," }
            );
            let position = start + env_members.last().map_or(1, |member| member.3);
            patched.insert_str(position, &insertion);
        }
    } else {
        let insertion = format!(
            "{}\"env\":{{\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"1\"}}",
            if members.is_empty() { "" } else { "," }
        );
        let position = root_start + members.last().map_or(1, |member| member.3);
        patched.insert_str(position, &insertion);
    }
    // Check semantics as well as spans, without serializing any unrelated value.
    let actual: Value = serde_json::from_str(&patched).map_err(|_| invalid())?;
    if actual != expected {
        return Err(invalid());
    }
    let mut result = Vec::new();
    if bytes.starts_with(b"\xef\xbb\xbf") {
        result.extend_from_slice(b"\xef\xbb\xbf");
    }
    result.extend_from_slice(patched.as_bytes());
    Ok(result)
}

fn config_bytes(paths: &CutoverPaths, port: u16) -> Result<Vec<u8>> {
    let mut doc = parse_config(paths)?;
    doc.remove("model_catalog_json");
    for key in [
        "realtime_base_url",
        "realtime_api_base_url",
        "realtime_ws_base_url",
        "experimental_realtime_ws_base_url",
        "experimental_realtime_webrtc_call_base_url",
    ] {
        if doc
            .get(key)
            .and_then(toml_edit::Item::as_str)
            .is_some_and(is_cpa_realtime_endpoint)
        {
            doc.remove(key);
        }
    }
    let selected = doc
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        .map(str::to_owned);
    let mut removed = Vec::new();
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        removed.extend(
            providers
                .iter()
                .filter(|(_, provider)| {
                    provider
                        .get("base_url")
                        .and_then(toml_edit::Item::as_str)
                        .is_some_and(is_cpa_endpoint)
                })
                .map(|(name, _)| name.to_owned()),
        );
        for name in &removed {
            providers.remove(name);
        }
    }
    // A retained provider could override the new top-level URL. Fail rather
    // than silently launching a dedicated home that bypasses quota-baton.
    if let Some(selected) = selected
        && selected != "openai"
    {
        if removed.contains(&selected) {
            doc.remove("model_provider");
        } else {
            bail!("선택된 비 CPA model_provider는 별도 홈 모드와 함께 사용할 수 없습니다");
        }
    }
    let effective = doc
        .get("model_provider")
        .and_then(toml_edit::Item::as_str)
        .unwrap_or("openai");
    if doc
        .get("model_providers")
        .and_then(|providers| providers.get(effective))
        .is_some()
    {
        bail!("선택된 사용자 정의 model_provider가 별도 홈 연결을 우회할 수 있어 중단했습니다");
    }
    doc["log_dir"] = toml_edit::value(paths.codex_qb_home.join("log").to_string_lossy().as_ref());
    doc["openai_base_url"] = toml_edit::value(codex_base_url(port));
    let trust = doc
        .get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|state| {
            state.iter().find_map(|(key, item)| {
                key.ends_with("hooks.json:session_start:0:0")
                    .then(|| item.get("trusted_hash").cloned())
                    .flatten()
            })
        });
    if let Some(hash) = trust {
        let key = format!(
            "{}:session_start:0:0",
            paths.codex_qb_home.join("hooks.json").display()
        );
        doc["hooks"]["state"][key.as_str()]["trusted_hash"] = hash;
    }
    let text = doc.to_string();
    let reparsed = text
        .parse::<DocumentMut>()
        .map_err(|_| anyhow::anyhow!("dedicated config 생성에 실패했습니다"))?;
    if reparsed
        .get("openai_base_url")
        .and_then(toml_edit::Item::as_str)
        != Some(codex_base_url(port).as_str())
    {
        bail!("dedicated config의 최상위 endpoint가 올바르지 않습니다");
    }
    dedicated_codex_title_bytes(text.as_bytes())
}

fn launcher() -> Vec<u8> {
    // CALL reparses percent/carets. Protect only the child command's outer
    // quotes from this parser, retaining the forwarded argument quotes.
    concat!(
        "@echo off\r\n",
        "setlocal DisableDelayedExpansion\r\n",
        "rem quota-baton dedicated Codex home\r\n",
        "set \"ERRORLEVEL=\"\r\n",
        "for /f \"tokens=2 delims=:\" %%P in ('chcp') do set \"QB_CODEPAGE=%%P\"\r\n",
        "chcp 65001 >nul\r\n",
        "for %%I in (.) do set \"QB_LAUNCHER_FOLDER=%%~nxI\"\r\n",
        "setlocal EnableDelayedExpansion\r\n",
        "title codex-qb · !QB_LAUNCHER_FOLDER!\r\n",
        "endlocal\r\n",
        "set \"CODEX_HOME=%USERPROFILE%\\.codex-acct\\qb\"\r\n",
        "\"%SystemRoot%\\System32\\cmd.exe\" /d /v:off /s /c ^\"\"%APPDATA%\\npm\\codex.cmd\" %*^\"\r\n",
        "set \"QB_EXIT=%errorlevel%\"\r\n",
        "chcp %QB_CODEPAGE% >nul\r\n",
        "endlocal & exit /b %QB_EXIT%\r\n",
    )
    .as_bytes()
    .to_vec()
}

// Collect credentials by key, then redact their known values everywhere they recur.
// Parsed sources never appear in parser error chains or reports.
fn string_patterns(value: &str, out: &mut Vec<String>) {
    let quoted = serde_json::to_string(value).expect("JSON string serialization");
    if value.len() >= 8 {
        out.push(value.to_owned());
        out.push(quoted[1..quoted.len() - 1].to_owned());
    }
    out.push(quoted);
}

fn toml_strings(root: &toml_edit::Item, out: &mut Vec<String>) {
    fn value(v: &toml_edit::Value, secret: bool, out: &mut Vec<String>) {
        if secret && let Some(s) = v.as_str() {
            string_patterns(s, out);
            out.push(v.to_string().trim().to_owned());
        }
        if let Some(a) = v.as_array() {
            for v in a {
                value(v, secret, out);
            }
        }
        if let Some(t) = v.as_inline_table() {
            for (key, v) in t {
                value(v, secret || secret_diff_key(key), out);
            }
        }
    }
    fn walk(item: &toml_edit::Item, secret: bool, out: &mut Vec<String>) {
        if let Some(v) = item.as_value() {
            value(v, secret, out);
        }
        if let Some(t) = item.as_table_like() {
            for (key, v) in t.iter() {
                if key == "projects"
                    && let Some(projects) = v.as_table_like()
                {
                    // Project paths are structural identifiers, not credential keys.
                    for (_, project) in projects.iter() {
                        walk(project, secret, out);
                    }
                } else {
                    walk(v, secret || secret_diff_key(key), out);
                }
            }
        }
        if let Some(a) = item.as_array_of_tables() {
            for t in a {
                for (key, v) in t {
                    walk(v, secret || secret_diff_key(key), out);
                }
            }
        }
    }
    walk(root, false, out);
}
fn json_strings(value: &Value, out: &mut Vec<String>) {
    fn walk(value: &Value, secret: bool, out: &mut Vec<String>) {
        match value {
            Value::String(s) if secret => string_patterns(s, out),
            Value::Array(a) => {
                for v in a {
                    walk(v, secret, out);
                }
            }
            Value::Object(o) => {
                for (key, v) in o {
                    walk(v, secret || secret_diff_key(key), out);
                }
            }
            _ => {}
        }
    }
    walk(value, false, out);
}
fn redact_once(input: &str, patterns: &[String]) -> String {
    let mut patterns = patterns
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    patterns.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    patterns.dedup();
    let mut result = String::with_capacity(input.len());
    let mut position = 0;
    while position < input.len() {
        // Read only the original input. Generated markers are never searched.
        if let Some(pattern) = patterns
            .iter()
            .find(|pattern| input[position..].starts_with(**pattern))
        {
            // Quoted lexical patterns must remain valid JSON/TOML values.
            if (pattern.starts_with('"') && pattern.ends_with('"'))
                || (pattern.starts_with('\'') && pattern.ends_with('\''))
            {
                result.push_str("\"<redacted>\"");
            } else {
                result.push_str("<redacted>");
            }
            position += pattern.len();
        } else {
            let character = input[position..]
                .chars()
                .next()
                .expect("remaining UTF-8 character");
            result.push(character);
            position += character.len_utf8();
        }
    }
    result
}

fn safe_diff(path: &Path, before: &[u8], after: &[u8], secrets: &[String]) -> Vec<String> {
    let before = redact_once(&String::from_utf8_lossy(before), secrets);
    let after = redact_once(&String::from_utf8_lossy(after), secrets);
    let mut diff = unified_diff(path, before.as_bytes(), after.as_bytes(), &[]);
    if before.is_empty() {
        diff[0] = format!("--- {} (없음)", path.display());
        diff[1] = format!("+++ {} (새로 만듦)", path.display());
    }
    diff
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum Kind {
    Directory,
    File,
    Junction,
    Hardlink,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Identity {
    volume: u32,
    index: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    kind: Kind,
    target: Option<PathBuf>,
    hash: String,
    identity: Option<Identity>,
    pending_identity: Option<Identity>,
    pending: Option<PathBuf>,
    /// Unproven scratch entries are reported, never adopted or removed.
    #[serde(default)]
    abandoned: Vec<PathBuf>,
    /// Protected bytes support retry without rereading a changed source/key.
    payload: Option<String>,
    done: bool,
    removed: bool,
    #[serde(default)]
    original: Option<ExistingFile>,
    /// Independent from the original unwire backup; absent in R3 journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upgrade: Option<Upgrade>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Upgrade {
    old_hash: String,
    old_identity: Identity,
    hash: String,
    payload: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExistingFile {
    hash: String,
    identity: Identity,
    payload: String,
    #[serde(default)]
    restoring: bool,
}
#[derive(Serialize, Deserialize)]
struct CommandRecord {
    path: PathBuf,
    hash: String,
    identity: Identity,
}
fn claude_home(paths: &CutoverPaths) -> Result<PathBuf> {
    Ok(paths
        .codex_qb_home
        .parent()
        .and_then(Path::parent)
        .context("사용자 홈 경로가 없습니다")?
        .join(".claude-qb"))
}
fn claude_launcher(command: &Path) -> Result<Vec<u8>> {
    if !safe_command_path(command) {
        bail!("PATH에서 안전한 Claude 실행 파일을 찾지 못했습니다. Claude 설치를 확인하세요");
    }
    Ok(format!(
        concat!(
            "@echo off\r\n",
            "setlocal DisableDelayedExpansion\r\n",
            "rem quota-baton Claude launcher\r\n",
            "set \"ERRORLEVEL=\"\r\n",
            "for /f \"tokens=2 delims=:\" %%P in ('chcp') do set \"QB_CODEPAGE=%%P\"\r\n",
            "chcp 65001 >nul\r\n",
            "for %%I in (.) do set \"QB_LAUNCHER_FOLDER=%%~nxI\"\r\n",
            "setlocal EnableDelayedExpansion\r\n",
            "title claude-qb · !QB_LAUNCHER_FOLDER!\r\n",
            "endlocal\r\n",
            "\"%SystemRoot%\\System32\\cmd.exe\" /d /v:off /s /c ^\"\"{}\" --settings \"%USERPROFILE%\\.claude-qb\\settings.json\" %*^\"\r\n",
            "set \"QB_EXIT=%errorlevel%\"\r\n",
            "chcp %QB_CODEPAGE% >nul\r\n",
            "endlocal & exit /b %QB_EXIT%\r\n",
        ),
        command.display()
    )
    .into_bytes())
}
fn check_command(record: &CommandRecord) -> Result<()> {
    if !safe_command_path(&record.path)
        || !record.path.is_file()
        || reparse(&record.path)?
        || identity(&record.path)? != record.identity
        || hash_file(&record.path)? != record.hash
    {
        bail!("Claude 실행 파일이 변경되었습니다. 먼저 unwire를 실행하세요");
    }
    ancestors_safe(&record.path)?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    schema: u8,
    mode: CodexMode,
    #[serde(default = "legacy_claude_mode")]
    claude_mode: ClaudeMode,
    #[serde(default)]
    command: Option<CommandRecord>,
    home: PathBuf,
    npm: PathBuf,
    claude: PathBuf,
    claude_before: String,
    claude_after: String,
    claude_original: String,
    claude_payload: String,
    claude_identity: Option<Identity>,
    claude_restored: bool,
    autostart: Option<OptionalActionJournal>,
    tasks: Vec<TaskJournal>,
    optional_restored: bool,
    retained: bool,
    /// None is legacy R3; false is an incomplete new install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    install_completed: Option<bool>,
    entries: Vec<Entry>,
}

// Windows handles are opened with OPEN_REPARSE_POINT: never fingerprint a
// junction target when deciding whether its directory entry belongs to us.
#[repr(C)]
#[derive(Default)]
struct FileInfo {
    attributes: u32,
    creation: [u32; 2],
    access: [u32; 2],
    write: [u32; 2],
    volume: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandle(handle: *mut std::ffi::c_void, info: *mut FileInfo) -> i32;
    fn MoveFileExW(existing: *const u16, destination: *const u16, flags: u32) -> i32;
    fn DeviceIoControl(
        handle: *mut std::ffi::c_void,
        code: u32,
        input: *const u8,
        input_size: u32,
        output: *mut u8,
        output_size: u32,
        returned: *mut u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
}
fn identity(path: &Path) -> Result<Identity> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(0x02200000)
        .open(path)?;
    let mut info = FileInfo::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(Identity {
        volume: info.volume,
        index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
    })
}
fn reparse(path: &Path) -> Result<bool> {
    use std::os::windows::fs::MetadataExt;
    Ok(fs::symlink_metadata(path)?.file_attributes() & 0x400 != 0)
}
fn ancestors_safe(path: &Path) -> Result<()> {
    for parent in path.ancestors().skip(1) {
        match fs::symlink_metadata(parent) {
            Ok(m) if !m.is_dir() || reparse(parent)? => {
                bail!("상위 경로가 일반 디렉터리가 아닙니다: {}", parent.display())
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn junction(path: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    // Caller has journaled and exclusively created the empty directory.
    // Shared sources are read-only targets and may themselves be junctions.
    // Keep the logical source path, matching the builder's mklink /J pattern.
    if !target.is_absolute() || !target.is_dir() {
        bail!("공유 디렉터리 원본이 올바르지 않습니다");
    }
    let printed = target
        .to_string_lossy()
        .trim_start_matches("\\\\?\\")
        .to_owned();
    if printed.starts_with("UNC\\") || printed.starts_with("\\\\") {
        bail!("UNC 공유 경로는 지원하지 않습니다");
    }
    let substitute = format!("\\??\\{printed}");
    let sub: Vec<u16> = std::ffi::OsStr::new(&substitute).encode_wide().collect();
    let print: Vec<u16> = std::ffi::OsStr::new(&printed).encode_wide().collect();
    let mut data = Vec::new();
    let path_bytes = (sub.len() + print.len() + 2) * 2;
    data.extend_from_slice(&0xA0000003u32.to_le_bytes());
    data.extend_from_slice(&u16::try_from(8 + path_bytes)?.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    for n in [0, sub.len() * 2, (sub.len() + 1) * 2, print.len() * 2] {
        data.extend_from_slice(&u16::try_from(n)?.to_le_bytes());
    }
    for word in sub.into_iter().chain([0]).chain(print).chain([0]) {
        data.extend_from_slice(&word.to_le_bytes());
    }
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(0x02200000)
        .open(path)?;
    let mut returned = 0;
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            0x000900A4,
            data.as_ptr(),
            data.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn target_same(a: &Path, b: &Path) -> bool {
    let normalize = |p: &Path| {
        p.to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .replace('/', "\\")
            .to_lowercase()
    };
    normalize(a) == normalize(b)
}
fn descriptor(kind: &Kind, target: Option<&Path>) -> String {
    hash_bytes(format!("{kind:?}:{}", target.map(normal_path).unwrap_or_default()).as_bytes())
}
fn new_entry(
    path: PathBuf,
    kind: Kind,
    target: Option<PathBuf>,
    bytes: Option<&[u8]>,
    apply: bool,
) -> Result<Entry> {
    Ok(Entry {
        hash: bytes
            .map(hash_bytes)
            .unwrap_or_else(|| descriptor(&kind, target.as_deref())),
        pending: None,
        abandoned: Vec::new(),
        path,
        kind,
        target,
        identity: None,
        pending_identity: None,
        payload: if apply {
            bytes.map(protect).transpose()?
        } else {
            None
        },
        done: false,
        removed: false,
        original: None,
        upgrade: None,
    })
}
fn add_dirs(entries: &mut Vec<Entry>, path: &Path, apply: bool) -> Result<()> {
    if exists(path)? {
        ancestors_safe(&path.join("probe"))?;
        return Ok(());
    }
    if entries.iter().any(|e| e.path == path) {
        return Ok(());
    }
    let parent = path.parent().context("전용 홈 부모 경로가 없습니다")?;
    add_dirs(entries, parent, apply)?;
    entries.push(new_entry(
        path.to_path_buf(),
        Kind::Directory,
        None,
        None,
        apply,
    )?);
    Ok(())
}

fn validate_manifest(paths: &CutoverPaths, m: &Manifest) -> Result<()> {
    if m.mode == CodexMode::None && m.claude_mode == ClaudeMode::None {
        bail!("Codex와 Claude를 모두 연결 안 함으로 선택할 수 없습니다");
    }
    if m.schema != 3
        || m.home != paths.codex_qb_home
        || m.npm != paths.npm_dir
        || m.claude != paths.claude_settings
        || (m.mode == CodexMode::Default && m.claude_mode == ClaudeMode::Default)
    {
        bail!("wiring journal의 경로/모드가 현재 홈과 일치하지 않습니다");
    }
    let mut tasks = std::collections::HashSet::new();
    for task in &m.tasks {
        if !matches!(
            task.name.as_str(),
            "cliproxyapi-proxy" | "cliproxyapi-quota-watch"
        ) || !tasks.insert(task.name.as_str())
            || task.after_enabled
        {
            bail!("journal의 CPA 예약 작업 범위/상태가 올바르지 않습니다");
        }
    }
    let ch = claude_home(paths)?;
    if m.claude_mode == ClaudeMode::Dedicated {
        let command = m
            .command
            .as_ref()
            .context("Claude 실행 파일 journal이 없습니다")?;
        if !safe_command_path(&command.path) {
            bail!("Claude 실행 파일 journal 경로가 올바르지 않습니다");
        }
    } else if m.command.is_some() {
        bail!("Claude 실행 파일 journal 모드가 다릅니다");
    }
    let mut seen = std::collections::HashSet::new();
    let mut scratch_seen = std::collections::HashSet::new();
    for e in &m.entries {
        if e.pending.as_ref().is_some_and(|p| {
            !(scratch_scoped(e, p)
                || (e.kind == Kind::File && *p == e.path.with_extension("qb-pending")))
        }) || (e.kind == Kind::Hardlink && e.pending.is_some())
            || e.abandoned.iter().any(|p| !scratch_scoped(e, p))
            || e.pending
                .iter()
                .chain(&e.abandoned)
                .any(|p| !scratch_seen.insert(p.clone()))
        {
            bail!("journal 임시 경로가 올바르지 않습니다");
        }
        let allowed_claude = m.claude_mode == ClaudeMode::Dedicated
            && (e.path == ch
                || e.path == ch.join("settings.json")
                || e.path == m.npm.join("claude-qb.cmd"));
        let allowed_existing = m.mode == CodexMode::Default
            && e.original.is_some()
            && (e.path == paths.codex_config
                || e.path == paths.codex_auth
                || paths
                    .codex_session_auth
                    .iter()
                    .any(|p| *p == e.path && !normal_path(p).contains("\\.codex-acct\\"))
                || paths.session_homes_file.as_ref() == Some(&e.path));
        if e.original.is_some() && (!allowed_existing || e.kind != Kind::File || e.target.is_some())
        {
            bail!("journal의 기존 파일 범위가 올바르지 않습니다");
        }
        let allowed_home = m.mode == CodexMode::Dedicated
            && (e.path == m.home
                || e.path == m.home.join("log")
                || ["config.toml", "auth.json", "AGENTS.md", "hooks.json"]
                    .iter()
                    .any(|n| e.path == m.home.join(n))
                || SHARED.iter().any(|n| e.path == m.home.join(n)));
        let allowed_parent = m.mode == CodexMode::Dedicated
            && e.kind == Kind::Directory
            && m.home.parent() == Some(e.path.as_path());
        if !e.path.is_absolute()
            || e.path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || !(allowed_existing
                || allowed_claude
                || allowed_home
                || allowed_parent
                || (m.mode == CodexMode::Dedicated && e.path == m.npm.join("codex-qb.cmd")))
            || !seen.insert(e.path.clone())
        {
            bail!("journal에 허용되지 않은 생성 경로가 있습니다");
        }
        if m.mode == CodexMode::None && !allowed_claude {
            bail!("none journal에 Codex 생성 경로가 있습니다");
        }
        let expected_kind =
            if e.path == ch || e.path == m.home || e.path == m.home.join("log") || allowed_parent {
                Kind::Directory
            } else if SHARED.iter().any(|name| e.path == m.home.join(name)) {
                Kind::Junction
            } else if ["AGENTS.md", "hooks.json"]
                .iter()
                .any(|name| e.path == m.home.join(name))
            {
                Kind::Hardlink
            } else {
                Kind::File
            };
        if e.kind != expected_kind {
            bail!("journal 경로의 파일 종류가 올바르지 않습니다");
        }
        if let Some(u) = &e.upgrade {
            if !upgrade_path(m, &e.path)?
                || e.kind != Kind::File
                || !e.done
                || e.removed
                || e.original.is_some()
                || e.identity.as_ref() != Some(&u.old_identity)
                || u.hash != hash_bytes(&unprotect(&u.payload)?)
            {
                bail!("업그레이드 journal 범위/내용이 올바르지 않습니다");
            }
        }
        ancestors_safe(&e.path)?;
        if e.kind == Kind::Junction || e.kind == Kind::Hardlink {
            let source = paths
                .codex_config
                .parent()
                .context("Codex 원본 홈이 없습니다")?
                .join(e.path.file_name().context("링크 이름이 없습니다")?);
            if e.target.as_ref() != Some(&source) {
                bail!("journal 링크 대상이 원본과 일치하지 않습니다");
            }
        }
    }
    if m.claude_mode == ClaudeMode::Default {
        ancestors_safe(&paths.claude_settings)?;
        if reparse(&paths.claude_settings)? || !paths.claude_settings.is_file() {
            bail!("Claude 설정이 일반 파일이 아닙니다");
        }
    }
    ancestors_safe(&paths.journal())?;
    if exists(&paths.journal())? && reparse(&paths.journal())? {
        bail!("journal이 일반 파일이 아닙니다");
    }
    Ok(())
}

fn matches_entry(e: &Entry, force: bool) -> Result<bool> {
    use std::os::windows::fs::MetadataExt;
    let meta = fs::symlink_metadata(&e.path)?;
    let same_identity = e
        .identity
        .as_ref()
        .or(e.pending_identity.as_ref())
        .is_some_and(|expected| identity(&e.path).ok().as_ref() == Some(expected));
    // Force applies only to the same owned object, never a substituted file,
    // reparse point, directory or hardlink relationship.
    match e.kind {
        Kind::Directory => Ok(meta.is_dir()
            && !reparse(&e.path)?
            && same_identity
            && e.hash == descriptor(&e.kind, None)),
        // symlink_metadata does not classify a mount-point reparse entry as
        // a regular directory. Check its native directory attribute without
        // following the target (which may itself be a junction).
        Kind::Junction => Ok(meta.file_attributes() & 0x10 != 0
            && reparse(&e.path)?
            && same_identity
            && e.target.as_deref().is_some_and(|target| {
                fs::read_link(&e.path)
                    .ok()
                    .is_some_and(|actual| target_same(&actual, target))
            })
            && e.hash == descriptor(&e.kind, e.target.as_deref())),
        Kind::File => Ok(meta.is_file()
            && !reparse(&e.path)?
            && e.identity
                .as_ref()
                .or(e.pending_identity.as_ref())
                .is_some_and(|expected| identity(&e.path).ok().as_ref() == Some(expected))
            && (force || hash_file(&e.path)? == e.hash)),
        Kind::Hardlink => Ok(meta.is_file()
            && !reparse(&e.path)?
            && same_identity
            && e.target
                .as_deref()
                .is_some_and(|target| identity(target).ok().as_ref() == e.identity.as_ref())
            && (hash_file(&e.path)? == e.hash || force)),
    }
}

fn pending_path(e: &Entry) -> PathBuf {
    e.pending
        .clone()
        .unwrap_or_else(|| e.path.with_extension("qb-pending"))
}

fn scratch_scoped(e: &Entry, path: &Path) -> bool {
    if e.kind == Kind::Hardlink || path.parent() != e.path.parent() {
        return false;
    }
    let Some(name) = e.path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let prefix = format!(".qb-scratch-{name}-");
    let Some(suffix) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix(&prefix))
    else {
        return false;
    };
    let parts = suffix.split('-').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|s| !s.is_empty() && s.len() <= 39 && s.bytes().all(|b| b.is_ascii_digit()))
}

fn fresh_scratch(e: &Entry) -> Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = e
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .context("scratch 이름이 올바르지 않습니다")?;
    Ok(e.path.with_file_name(format!(
        ".qb-scratch-{name}-{}-{time}-{nonce}",
        std::process::id()
    )))
}

#[cfg(test)]
thread_local! {
    static INTERRUPT: std::cell::RefCell<Option<(PathBuf, &'static str)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn interrupt(path: &Path, stage: &str) -> Result<()> {
    INTERRUPT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(p, s)| p == path && *s == stage) {
            *slot = None;
            bail!("synthetic creation interruption");
        }
        Ok(())
    })
}

fn pending_matches(e: &Entry, force: bool) -> Result<bool> {
    let tmp = pending_path(e);
    if !fs::symlink_metadata(&tmp)?.is_file() || reparse(&tmp)? {
        return Ok(false);
    }
    let same = e
        .pending_identity
        .as_ref()
        .is_some_and(|id| identity(&tmp).ok().as_ref() == Some(id));
    Ok(same && (force || hash_file(&tmp)? == e.hash))
}

/// Journal intent, exclusively create a sibling, then journal its identity.
/// A crash before the second journal write leaves an unproven sibling only.
fn prepare_scratch(paths: &CutoverPaths, m: &mut Manifest, i: usize) -> Result<()> {
    loop {
        if let Some(tmp) = m.entries[i].pending.clone() {
            if exists(&tmp)? {
                if m.entries[i].pending_identity.is_some() {
                    return Ok(()); // Caller checks identity, type and contents before using it.
                }
                if !scratch_scoped(&m.entries[i], &tmp) {
                    bail!("기존 임시 파일의 identity가 없습니다");
                }
                m.entries[i].abandoned.push(tmp);
            } else if m.entries[i].pending_identity.is_some() {
                bail!("기록된 임시 경로가 사라졌습니다");
            }
        }
        m.entries[i].pending = Some(fresh_scratch(&m.entries[i])?);
        m.entries[i].pending_identity = None;
        atomic_json(&paths.journal(), m)?;
        let tmp = pending_path(&m.entries[i]);
        let result = if m.entries[i].kind == Kind::File {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map(|_| ())
        } else {
            fs::create_dir(&tmp)
        };
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
        #[cfg(test)]
        interrupt(&m.entries[i].path, "after-create")?;
        m.entries[i].pending_identity = Some(identity(&tmp)?);
        atomic_json(&paths.journal(), m)?;
        return Ok(());
    }
}

fn restrict_claude_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
            Security::{
                Authorization::{
                    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                    SDDL_REVISION_1,
                },
                DACL_SECURITY_INFORMATION, GetTokenInformation,
                PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SetFileSecurityW,
                TOKEN_QUERY, TOKEN_USER, TokenUser,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        },
        core::{PCWSTR, PWSTR},
    };
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    struct Allocation(HLOCAL);
    impl Drop for Allocation {
        fn drop(&mut self) {
            unsafe {
                let _ = LocalFree(Some(self.0));
            }
        }
    }
    #[cfg(test)]
    interrupt(path, "before-acl")?;
    // 토큰 버퍼는 TOKEN_USER 정렬을 보장하고 모든 Windows 할당은 RAII로 해제한다.
    unsafe {
        let mut handle = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle)?;
        let token = Token(handle);
        let mut size = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
        anyhow::ensure!(size > 0, "cannot determine current user token size");
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
        let _sid_allocation = Allocation(HLOCAL(sid.0.cast()));
        let sddl = format!("D:P(A;OICI;FA;;;{})", sid.to_string()?);
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let _descriptor_allocation = Allocation(HLOCAL(descriptor.0));
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        SetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
        .ok()?;
    }
    Ok(())
}

fn install_directory(paths: &CutoverPaths, m: &mut Manifest, i: usize) -> Result<()> {
    prepare_scratch(paths, m, i)?;
    let e = &m.entries[i];
    let tmp = pending_path(e);
    ancestors_safe(&tmp)?;
    if e.pending_identity.as_ref() != Some(&identity(&tmp)?) {
        bail!("임시 디렉터리 identity가 다릅니다");
    }
    if reparse(&tmp)? {
        let mut scratch = e.clone();
        scratch.path = tmp.clone();
        scratch.identity = e.pending_identity.clone();
        if e.kind != Kind::Junction || !matches_entry(&scratch, false)? {
            bail!("임시 링크가 변경되었습니다");
        }
    } else {
        if !tmp.is_dir() || fs::read_dir(&tmp)?.next().is_some() {
            bail!("임시 디렉터리가 변경되었습니다");
        }
        if e.kind == Kind::Junction {
            junction(&tmp, e.target.as_deref().context("링크 대상이 없습니다")?)?;
        }
    }
    if exists(&e.path)? {
        bail!("생성 경로 충돌: {}", e.path.display());
    }
    // No MOVEFILE_REPLACE_EXISTING: even a collision after the check is refused.
    // The source identity was made durable before this publication.
    if e.kind == Kind::Directory && e.path == claude_home(paths)? {
        restrict_claude_directory(&tmp).map_err(|_| {
            anyhow::anyhow!("Claude 전용 설정 디렉터리 접근 권한을 제한하지 못했습니다")
        })?;
    }
    publish_directory(&tmp, &e.path)?;
    #[cfg(test)]
    interrupt(&e.path, "after-publish")?;
    Ok(())
}

fn publish_directory(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let source = source
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn install_file(paths: &CutoverPaths, m: &mut Manifest, i: usize) -> Result<()> {
    let e = &m.entries[i];
    if e.path == claude_home(paths)?.join("settings.json") {
        let home = claude_home(paths)?;
        let directory = m
            .entries
            .iter()
            .find(|e| e.path == home)
            .context("Claude 홈 journal이 없습니다")?;
        if !matches_entry(directory, false)? {
            bail!("Claude 홈 identity가 다릅니다");
        }
        restrict_claude_directory(&home).map_err(|_| {
            anyhow::anyhow!("Claude 전용 설정 디렉터리 접근 권한을 제한하지 못했습니다")
        })?;
    }
    let bytes = unprotect(e.payload.as_deref().context("파일 payload가 없습니다")?)?;
    if hash_bytes(&bytes) != e.hash {
        bail!("journal payload hash가 다릅니다");
    }
    prepare_scratch(paths, m, i)?;
    let e = &m.entries[i];
    let tmp = pending_path(e);
    if !pending_matches(e, true)? {
        bail!("임시 파일이 변경되었습니다: {}", tmp.display());
    }
    if !pending_matches(e, false)? {
        let mut f = OpenOptions::new().write(true).truncate(true).open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    // Exclusive, atomic publication. The manifest contains this sibling's
    // intent/hash before it exists; recovery checks its identity and bytes.
    fs::hard_link(&tmp, &m.entries[i].path)?;
    #[cfg(test)]
    interrupt(&m.entries[i].path, "after-publish")?;
    fs::remove_file(&tmp)?;
    Ok(())
}

fn install_existing(paths: &CutoverPaths, m: &mut Manifest, i: usize) -> Result<()> {
    if matches_entry(&m.entries[i], false)? {
        return Ok(());
    }
    let e = &m.entries[i];
    let before = e
        .original
        .as_ref()
        .context("기존 파일 journal이 없습니다")?;
    if reparse(&e.path)?
        || identity(&e.path)? != before.identity
        || hash_file(&e.path)? != before.hash
    {
        bail!("기존 Codex 파일이 변경되었습니다");
    }
    prepare_scratch(paths, m, i)?;
    let e = &m.entries[i];
    if !pending_matches(e, true)? {
        bail!("기존 파일 임시 identity가 다릅니다");
    }
    let bytes = unprotect(e.payload.as_deref().context("payload가 없습니다")?)?;
    if hash_bytes(&bytes) != e.hash {
        bail!("journal payload hash가 다릅니다");
    }
    let mut file = OpenOptions::new()
        .write(true)
        .access_mode(0x40000000 | 0x00010000)
        .open(pending_path(e))?;
    file.set_len(0)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    let before = e.original.as_ref().unwrap();
    if identity(&e.path)? != before.identity || hash_file(&e.path)? != before.hash {
        bail!("기존 Codex 파일이 변경되었습니다");
    }
    replace_owned_scratch(&file, &e.path)?;
    #[cfg(test)]
    interrupt(&e.path, "after-publish")?;
    Ok(())
}
fn restore_existing_text(paths: &CutoverPaths, e: &Entry) -> Result<Vec<u8>> {
    let original = unprotect(&e.original.as_ref().context("원본이 없습니다")?.payload)?;
    if e.path == paths.codex_config {
        let text = std::str::from_utf8(strip_utf8_bom(&original))?.parse::<DocumentMut>()?;
        let mut current = read_text(&e.path)?.parse::<DocumentMut>()?;
        for key in ["openai_base_url", "model_catalog_json"] {
            restore_toml(&mut current, key, &original_toml(text.get(key)));
        }
        let providers = text
            .get("model_providers")
            .and_then(toml_edit::Item::as_table_like);
        if let Some(providers) = providers {
            for (name, old) in providers.iter() {
                if !old
                    .get("base_url")
                    .and_then(toml_edit::Item::as_str)
                    .is_some_and(is_cpa_url)
                {
                    continue;
                }
                if let Some(table) = current
                    .get_mut("model_providers")
                    .and_then(|v| v.get_mut(name))
                    .and_then(toml_edit::Item::as_table_like_mut)
                {
                    for key in ["base_url", "model_catalog_url", "experimental_bearer_token"] {
                        restore_toml_in_table(table, key, &original_toml(old.get(key)));
                    }
                }
            }
        }
        preserved_bytes(&e.path, current.to_string().as_bytes())
    } else if e.path == paths.codex_auth || paths.codex_session_auth.contains(&e.path) {
        let text = restore_codex_auth_text(&original, &read_text(&e.path)?)?;
        preserved_bytes(&e.path, text.as_bytes())
    } else {
        bail!("변경된 세션 홈 매핑은 보존합니다");
    }
}
/// Restore only owned root members; nested values and unrelated text survive force.
fn restore_codex_auth_text(original: &[u8], current: &str) -> Result<String> {
    let invalid = || anyhow::anyhow!("Codex auth 복구 형식이 올바르지 않습니다");
    let before: Value = serde_json::from_slice(strip_utf8_bom(original)).map_err(|_| invalid())?;
    let mut expected: Value = serde_json::from_str(current).map_err(|_| invalid())?;
    let root = expected.as_object_mut().ok_or_else(invalid)?;
    let mut text = current.to_owned();
    for key in ["auth_mode", "OPENAI_API_KEY"] {
        let value = before.get(key);
        if let Some(value) = value {
            root.insert(key.into(), value.clone());
        } else {
            root.remove(key);
        }
        let start = text
            .bytes()
            .position(|byte| !byte.is_ascii_whitespace())
            .ok_or_else(invalid)?;
        let members = json_member_spans(&text[start..])
            .ok_or_else(invalid)?
            .into_iter()
            .map(|(key, member, value, end)| (key, member + start, value + start, end + start))
            .collect::<Vec<_>>();
        if let Some((index, member)) = members
            .iter()
            .enumerate()
            .find(|(_, member)| member.0 == key)
        {
            if let Some(value) = value {
                text.replace_range(member.2..member.3, &serde_json::to_string(value)?);
            } else {
                let range = if index + 1 < members.len() {
                    let comma = text[member.3..members[index + 1].1]
                        .find(',')
                        .ok_or_else(invalid)?
                        + member.3;
                    member.1..comma + 1
                } else if index > 0 {
                    let comma = text[members[index - 1].3..member.1]
                        .rfind(',')
                        .ok_or_else(invalid)?
                        + members[index - 1].3;
                    comma..member.3
                } else {
                    member.1..member.3
                };
                text.replace_range(range, "");
            }
        } else if let Some(value) = value {
            let position = members
                .last()
                .map(|member| member.3)
                .or_else(|| text.find('{').map(|position| position + 1))
                .ok_or_else(invalid)?;
            text.insert_str(
                position,
                &format!(
                    "{}{}:{}",
                    if members.is_empty() { "" } else { "," },
                    serde_json::to_string(key)?,
                    serde_json::to_string(value)?
                ),
            );
        }
    }
    let actual: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    if actual != expected {
        return Err(invalid());
    }
    Ok(text)
}
fn existing_entry(path: PathBuf, after: &[u8], apply: bool) -> Result<Entry> {
    ancestors_safe(&path)?;
    if reparse(&path)? || !path.is_file() {
        bail!("기존 Codex 파일이 일반 파일이 아닙니다");
    }
    let mut e = new_entry(path.clone(), Kind::File, None, Some(after), apply)?;
    e.original = Some(ExistingFile {
        hash: hash_file(&path)?,
        identity: identity(&path)?,
        payload: if apply {
            protect(&fs::read(&path)?)?
        } else {
            String::new()
        },
        restoring: false,
    });
    Ok(e)
}

fn install(paths: &CutoverPaths, m: &mut Manifest) -> Result<()> {
    validate_manifest(paths, m)?;
    for i in 0..m.entries.len() {
        let e = &m.entries[i];
        if e.removed {
            bail!("부분 해제 journal입니다. unwire를 먼저 완료하세요");
        }
        if e.original.is_some() {
            if e.original.as_ref().is_some_and(|o| o.restoring) {
                bail!("부분 해제 journal입니다. unwire를 먼저 완료하세요");
            }
            install_existing(paths, m, i)?;
        } else if exists(&e.path)? {
            // Recover the precise file installed before its completion write.
            if e.kind == Kind::Junction
                && !reparse(&e.path)?
                && e.path.is_dir()
                && e.identity.as_ref() == Some(&identity(&e.path)?)
                && fs::read_dir(&e.path)?.next().is_none()
            {
                junction(
                    &e.path,
                    e.target.as_deref().context("링크 대상이 없습니다")?,
                )?;
            } else if !matches_entry(e, false)? {
                bail!("생성 경로 충돌 또는 변경: {}", e.path.display());
            }
        } else {
            if e.done {
                bail!("생성한 경로가 사라졌습니다: {}", e.path.display());
            }
            match e.kind {
                Kind::Directory | Kind::Junction => install_directory(paths, m, i)?,
                Kind::File => install_file(paths, m, i)?,
                Kind::Hardlink => {
                    let target = e.target.as_ref().context("링크 대상이 없습니다")?;
                    ancestors_safe(target)?;
                    if reparse(target)? || identity(target).ok().as_ref() != e.identity.as_ref() {
                        bail!("공유 파일 원본 identity가 변경되었습니다");
                    }
                    fs::hard_link(target, &e.path)?;
                    if !matches_entry(e, false)? {
                        bail!("생성한 hardlink가 원본과 다릅니다");
                    }
                }
            }
        }
        m.entries[i].identity = Some(identity(&m.entries[i].path)?);
        m.entries[i].done = true;
        atomic_json(&paths.journal(), m)?;
        if m.entries[i].kind == Kind::File
            && m.entries[i].pending.is_some()
            && exists(&pending_path(&m.entries[i]))?
        {
            if !pending_matches(&m.entries[i], false)? {
                bail!("임시 파일 identity가 다릅니다");
            }
            fs::remove_file(pending_path(&m.entries[i]))?;
        }
    }
    if m.claude_mode == ClaudeMode::Default {
        let current = hash_file(&m.claude)?;
        if current == m.claude_before {
            // Payload already preserves the source BOM/line endings. A second
            // preservation wrapper would prepend another BOM.
            atomic_bytes(&m.claude, &unprotect(&m.claude_payload)?)?;
            m.claude_identity = Some(identity(&m.claude)?);
            atomic_json(&paths.journal(), m)?;
        } else if current != m.claude_after {
            bail!("Claude 설정이 변경되어 적용을 중단했습니다");
        }
    }
    // A completed journal can still need file-boundary recovery. Its optional
    // actions already finished; recovering files must not apply them again.
    if m.install_completed != Some(true) {
        if let Some(action) = &m.autostart {
            set_autostart(action)?;
        }
        disable_cpa_tasks(&m.tasks)?;
    }
    m.install_completed = Some(true);
    atomic_json(&paths.journal(), m)?;
    Ok(())
}

fn scratch_warnings(m: &Manifest) -> Vec<String> {
    m.entries
        .iter()
        .flat_map(|e| &e.abandoned)
        .map(|p| {
            format!(
                "identity가 기록되지 않은 임시 경로를 보존합니다: {}",
                p.display()
            )
        })
        .collect()
}

fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

// R3.2: only the four dedicated title surfaces; never auth/default homes.
fn upgrade_path(m: &Manifest, path: &Path) -> Result<bool> {
    let ch = m
        .home
        .parent()
        .and_then(Path::parent)
        .context("사용자 홈 경로가 없습니다")?
        .join(".claude-qb");
    Ok((m.mode == CodexMode::Dedicated
        && (path == m.home.join("config.toml") || path == m.npm.join("codex-qb.cmd")))
        || (m.claude_mode == ClaudeMode::Dedicated
            && (path == ch.join("settings.json") || path == m.npm.join("claude-qb.cmd"))))
}

fn completed_install(m: &Manifest) -> Result<bool> {
    // The terminal marker cannot override an unfinished object boundary,
    // including default Claude still at the exact journaled pre-install hash.
    if m.entries.iter().any(|e| !e.done || e.removed)
        || (m.claude_mode == ClaudeMode::Default
            && m.claude_before != m.claude_after
            && hash_file(&m.claude)? == m.claude_before)
    {
        return Ok(false);
    }
    if let Some(completed) = m.install_completed {
        return Ok(completed);
    }
    // Legacy R3 has no terminal marker. Incomplete file installs still resume.
    // All-done legacy journals must not replay optional actions, even if a crash
    // occurred just before those actions; R3 cannot distinguish that last window.
    Ok(m.entries.iter().all(|e| e.done && !e.removed)
        && (m.claude_mode != ClaudeMode::Default
            || hash_file(&m.claude)? != m.claude_before
            || m.claude_before == m.claude_after))
}

/// true = the exact staged new object is already at the destination.
fn upgrade_state(e: &Entry) -> Result<bool> {
    ancestors_safe(&e.path)?;
    let u = e.upgrade.as_ref().context("업그레이드 intent가 없습니다")?;
    let mut probe = e.clone();
    probe.identity = e.pending_identity.clone();
    probe.pending_identity = None;
    probe.hash = u.hash.clone();
    if matches_entry(&probe, false)? {
        return Ok(true);
    }
    probe.identity = Some(u.old_identity.clone());
    probe.hash = u.old_hash.clone();
    if matches_entry(&probe, false)? {
        return Ok(false);
    }
    bail!("업그레이드 파일 identity/내용이 변경되었습니다. journal을 보존합니다");
}

fn upgrade_directories(m: &Manifest) -> Result<()> {
    for e in &m.entries {
        if upgrade_path(m, &e.path)? {
            for parent in &m.entries {
                if parent.kind == Kind::Directory
                    && e.path.starts_with(&parent.path)
                    && !matches_entry(parent, false)?
                {
                    bail!("업그레이드 홈 identity가 다릅니다");
                }
            }
        }
    }
    Ok(())
}

fn resume_upgrade(paths: &CutoverPaths, m: &mut Manifest, i: usize) -> Result<()> {
    validate_manifest(paths, m)?;
    upgrade_directories(m)?;
    if !upgrade_state(&m.entries[i])? {
        #[cfg(test)]
        interrupt(&m.entries[i].path, "upgrade-before-scratch")?;
        prepare_scratch(paths, m, i)?;
        #[cfg(test)]
        interrupt(&m.entries[i].path, "upgrade-after-identity")?;
        let e = &m.entries[i];
        if !pending_matches(e, true)? {
            bail!("업그레이드 임시 파일 identity가 다릅니다");
        }
        let u = e.upgrade.as_ref().unwrap();
        let bytes = unprotect(&u.payload)?;
        if hash_bytes(&bytes) != u.hash {
            bail!("업그레이드 journal payload hash가 다릅니다");
        }
        let mut file = OpenOptions::new()
            .write(true)
            .access_mode(0x40000000 | 0x00010000)
            .custom_flags(0x00200000)
            .open(pending_path(e))?;
        // Bind writes and rename to the journaled scratch handle, not just path.
        let mut info = FileInfo::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let held = Identity {
            volume: info.volume,
            index: (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        };
        if e.pending_identity.as_ref() != Some(&held) || info.attributes & 0x410 != 0 {
            bail!("업그레이드 임시 파일 identity가 다릅니다");
        }
        file.set_len(0)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(test)]
        interrupt(&e.path, "upgrade-after-write")?;
        let mut staged = e.clone();
        staged.hash = u.hash.clone();
        if !pending_matches(&staged, false)? || upgrade_state(e)? {
            bail!("업그레이드 교체 대상이 변경되었습니다");
        }
        replace_owned_scratch(&file, &e.path)?;
        #[cfg(test)]
        interrupt(&e.path, "upgrade-after-replace")?;
    }
    if !upgrade_state(&m.entries[i])? {
        bail!("업그레이드 교체가 완료되지 않았습니다");
    }
    #[cfg(test)]
    interrupt(&m.entries[i].path, "upgrade-before-final-journal")?;
    let e = &mut m.entries[i];
    let u = e.upgrade.take().unwrap();
    e.hash = u.hash;
    e.payload = Some(u.payload);
    e.identity = e.pending_identity.take();
    e.pending = None;
    // e.original is deliberately untouched: it is the unwire backup anchor.
    atomic_json(&paths.journal(), m)?;
    Ok(())
}

fn upgrade_completed(
    paths: &CutoverPaths,
    m: &mut Manifest,
    apply: bool,
    force: bool,
) -> Result<Report> {
    upgrade_directories(m)?;
    // Title upgrades do not reinstall the other objects, but still enforce
    // their existing ownership/content boundaries before any upgrade write.
    for e in &m.entries {
        if !upgrade_path(m, &e.path)? && !matches_entry(e, false)? {
            bail!("생성 경로 충돌 또는 변경: {}", e.path.display());
        }
    }
    if m.claude_mode == ClaudeMode::Default && hash_file(&m.claude)? != m.claude_after {
        bail!("Claude 설정이 변경되어 적용을 중단했습니다");
    }
    let mut report = Report {
        summary: vec!["요약: 별도 홈 제목 설정 업그레이드를 확인합니다".into()],
        ..Report::default()
    };
    let mut plans = Vec::new();
    let mut secrets = Vec::new();
    // Preflight all candidates before mutation. No default source/key reads.
    for e in &m.entries {
        if !upgrade_path(m, &e.path)? {
            continue;
        }
        if e.upgrade.is_some() {
            upgrade_state(e)?;
        } else {
            ancestors_safe(&e.path)?;
            if !matches_entry(e, force)? {
                bail!(
                    "전용 파일이 변경되었습니다. 같은 파일의 내용 변경만 --force로 허용할 수 있습니다"
                );
            }
        }
        let before = fs::read(&e.path)?;
        let after = if let Some(u) = &e.upgrade {
            unprotect(&u.payload)?
        } else if e.path == m.home.join("config.toml") {
            dedicated_codex_title_bytes(&before)?
        } else if e.path.file_name().is_some_and(|n| n == "settings.json") {
            dedicated_claude_title_bytes(&before)?
        } else if e.path == m.npm.join("codex-qb.cmd") {
            launcher()
        } else {
            claude_launcher(
                &m.command
                    .as_ref()
                    .context("Claude 실행 파일 journal이 없습니다")?
                    .path,
            )?
        };
        for bytes in [&before, &after] {
            if let Ok(doc) = std::str::from_utf8(strip_utf8_bom(bytes))
                .unwrap_or("")
                .parse::<DocumentMut>()
            {
                toml_strings(doc.as_item(), &mut secrets);
            }
            if let Ok(json) = serde_json::from_slice::<Value>(strip_utf8_bom(bytes)) {
                json_strings(&json, &mut secrets);
            }
        }
        plans.push((e.path.clone(), before, after));
    }
    // Known journal credentials are redacted wherever values recur in the diff.
    for e in &m.entries {
        if let Some(payload) = &e.payload {
            let bytes = unprotect(payload)?;
            if let Ok(json) = serde_json::from_slice::<Value>(strip_utf8_bom(&bytes)) {
                json_strings(&json, &mut secrets);
            }
        }
    }
    for (path, before, after) in plans {
        let i = m.entries.iter().position(|e| e.path == path).unwrap();
        if before != after || m.entries[i].upgrade.is_some() {
            report.changed.push(path.display().to_string());
            report
                .diff
                .extend(safe_diff(&path, &before, &after, &secrets));
            if !apply {
                continue;
            }
            if m.entries[i].upgrade.is_none() {
                let e = &mut m.entries[i];
                if !matches_entry(e, force)? || hash_file(&e.path)? != hash_bytes(&before) {
                    bail!("업그레이드 교체 대상이 변경되었습니다");
                }
                if let Some(tmp) = &e.pending {
                    if exists(tmp)? {
                        bail!("기존 임시 파일을 먼저 확인하세요");
                    }
                }
                e.upgrade = Some(Upgrade {
                    old_hash: hash_bytes(&before),
                    old_identity: e
                        .identity
                        .clone()
                        .context("업그레이드 identity가 없습니다")?,
                    hash: hash_bytes(&after),
                    payload: protect(&after)?,
                });
                e.pending = None;
                e.pending_identity = None;
                atomic_json(&paths.journal(), m)?;
            }
            resume_upgrade(paths, m, i)?;
        }
    }
    report.warnings.extend(scratch_warnings(m));
    Ok(report)
}

/// Check the saved plan before any resume write; retained homes build a fresh plan.
fn validate_resume_inputs(paths: &CutoverPaths, options: &WireOptions, m: &Manifest) -> Result<()> {
    let mismatch =
        || anyhow::anyhow!("기존 연결의 포트/키가 현재 설정과 다릅니다. 먼저 unwire를 실행하세요");
    let invalid = || anyhow::anyhow!("기존 연결 journal 형식이 올바르지 않습니다");
    let key = read_key(&paths.client_key)?;
    if m.claude_mode != ClaudeMode::None {
        let home = claude_home(paths)?;
        let claude_payload = if m.claude_mode == ClaudeMode::Dedicated {
            m.entries
                .iter()
                .find(|e| e.path == home.join("settings.json"))
                .and_then(|e| e.payload.as_deref())
                .ok_or_else(invalid)?
        } else {
            &m.claude_payload
        };
        let claude = unprotect(claude_payload).map_err(|_| invalid())?;
        let claude: Value =
            serde_json::from_slice(strip_utf8_bom(&claude)).map_err(|_| invalid())?;
        let env = claude.get("env").ok_or_else(invalid)?;
        if env.get("ANTHROPIC_AUTH_TOKEN").and_then(Value::as_str) != Some(key.as_str())
            || env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str)
                != Some(claude_base_url(options.data_port).as_str())
        {
            return Err(mismatch());
        }
    }
    if m.mode != CodexMode::None {
        let payload = |name: &str| -> Result<Vec<u8>> {
            let e = m
                .entries
                .iter()
                .find(|e| {
                    e.path
                        == if m.mode == CodexMode::Dedicated {
                            m.home.join(name)
                        } else if name == "auth.json" {
                            paths.codex_auth.clone()
                        } else {
                            paths.codex_config.clone()
                        }
                })
                .ok_or_else(invalid)?;
            unprotect(e.payload.as_deref().ok_or_else(invalid)?).map_err(|_| invalid())
        };
        let auth = payload("auth.json")?;
        let auth: Value = serde_json::from_slice(strip_utf8_bom(&auth)).map_err(|_| invalid())?;
        let config = payload("config.toml")?;
        let config = std::str::from_utf8(strip_utf8_bom(&config))
            .map_err(|_| invalid())?
            .parse::<DocumentMut>()
            .map_err(|_| invalid())?;
        if auth.get("auth_mode").and_then(Value::as_str) != Some("apikey")
            || auth.get("OPENAI_API_KEY").and_then(Value::as_str) != Some(key.as_str())
            || config
                .get("openai_base_url")
                .and_then(toml_edit::Item::as_str)
                != Some(codex_base_url(options.data_port).as_str())
        {
            return Err(mismatch());
        }
    }
    // Validate every protected file payload even if no file needs rewriting.
    // Keep semantic key/port diagnostics above, without exposing payload text.
    for e in m.entries.iter().filter(|e| e.kind == Kind::File) {
        let bytes = unprotect(e.payload.as_deref().ok_or_else(invalid)?).map_err(|_| invalid())?;
        if hash_bytes(&bytes) != e.hash {
            return Err(invalid());
        }
    }
    if m.claude_mode == ClaudeMode::Default
        && hash_bytes(&unprotect(&m.claude_payload).map_err(|_| invalid())?) != m.claude_after
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn wire(
    paths: &CutoverPaths,
    apply: bool,
    options: WireOptions,
    mode: CodexMode,
    claude_mode: ClaudeMode,
    task_query: &dyn Fn(&str) -> Option<bool>,
) -> Result<Report> {
    let count = active_linked_count(&paths.qb_home)?;
    check_cpa_dependency(count, &options)?;
    let mut retained_home = false;
    let mut retained_entries = Vec::new();
    if paths.journal().exists() {
        let mut old: Manifest =
            serde_json::from_slice(&fs::read(paths.journal())?).map_err(|_| {
                anyhow::anyhow!("이미 연결 journal이 있습니다. 먼저 unwire를 실행하세요")
            })?;
        validate_manifest(paths, &old)?;
        if old.mode != mode {
            bail!("journal의 Codex 모드가 다릅니다. 먼저 unwire를 실행하세요");
        }
        if old.claude_mode != claude_mode {
            bail!("journal의 Claude 모드가 다릅니다. 먼저 unwire를 실행하세요");
        }
        if claude_mode == ClaudeMode::Dedicated
            && paths.claude_command.as_ref() != old.command.as_ref().map(|c| &c.path)
        {
            bail!("Claude 실행 파일이 변경되었습니다. 먼저 unwire를 실행하세요");
        }
        if let Some(command) = &old.command {
            check_command(command)?;
        }
        if !old.retained {
            if old
                .entries
                .iter()
                .any(|e| e.removed || e.original.as_ref().is_some_and(|o| o.restoring))
                || old.optional_restored
            {
                bail!("부분 해제 journal입니다. unwire를 먼저 완료하세요");
            }
            if completed_install(&old)? {
                if apply {
                    validate_resume_inputs(paths, &options, &old)?;
                }
                return upgrade_completed(paths, &mut old, apply, options.force);
            }
            let mut report = Report {
                summary: vec!["요약: 기존 journal에서 연결을 재개합니다".into()],
                ..Report::default()
            };
            if apply {
                validate_resume_inputs(paths, &options, &old)?;
                install(paths, &mut old)?;
            }
            report.warnings.extend(scratch_warnings(&old));
            return Ok(report);
        }
        retained_home = true;
        for e in old.entries.iter().filter(|e| !e.removed) {
            if e.kind != Kind::Directory || !matches_entry(e, false)? {
                bail!("보존된 홈의 identity가 다릅니다");
            }
        }
        retained_entries.extend(old.entries.into_iter().filter(|e| !e.removed));
    }
    let key = if apply {
        read_key(&paths.client_key)?
    } else {
        "<client key>".into()
    };
    let claude_before = if claude_mode == ClaudeMode::Default {
        fs::read(&paths.claude_settings).context("Claude 설정을 읽지 못했습니다")?
    } else {
        Vec::new()
    };
    let json: Value = if claude_mode == ClaudeMode::Default {
        serde_json::from_slice(strip_utf8_bom(&claude_before))
            .map_err(|_| anyhow::anyhow!("Claude 설정 형식이 올바르지 않습니다"))?
    } else {
        Value::Null
    };
    let claude_after = if claude_mode == ClaudeMode::Default {
        preserved_bytes_from_original(
            &claude_before,
            &claude_bytes(
                &paths.claude_settings,
                &claude_base_url(options.data_port),
                &key,
            )?,
        )
    } else {
        Vec::new()
    };
    let mut secrets = Vec::new();
    if apply {
        string_patterns(&key, &mut secrets);
    }
    json_strings(&json, &mut secrets);
    let mut entries = retained_entries;
    let mode_label = match mode {
        CodexMode::Default => "기본 홈",
        CodexMode::Dedicated => "별도 홈",
        CodexMode::None => "연결 안 함",
    };
    let mut report = Report {
        summary: vec![format!(
            "요약: Codex {mode_label}, Claude {}",
            match claude_mode {
                ClaudeMode::Default => "연결 계획",
                ClaudeMode::Dedicated => "별도 설정 연결 계획",
                ClaudeMode::None => "연결 안 함",
            }
        )],
        ..Report::default()
    };
    if claude_mode == ClaudeMode::Default {
        report.diff.extend(safe_diff(
            &paths.claude_settings,
            &claude_before,
            &claude_after,
            &secrets,
        ));
    }
    let command = if claude_mode == ClaudeMode::Dedicated {
        if !paths.npm_dir.is_dir() {
            bail!("npm 디렉터리가 없습니다. Claude 설치를 확인하세요");
        }
        let command = paths.claude_command.as_ref().context(
            "PATH에서 안전한 Claude 실행 파일을 찾지 못했습니다. Claude 설치를 확인하세요",
        )?;
        let record = CommandRecord {
            path: command.clone(),
            hash: hash_file(command)?,
            identity: identity(command)?,
        };
        check_command(&record)?;
        let home = claude_home(paths)?;
        if exists(&home)?
            && !(retained_home
                && entries.iter().any(|e| {
                    e.path == home
                        && e.kind == Kind::Directory
                        && matches_entry(e, false).unwrap_or(false)
                }))
        {
            bail!("전용 Claude 홈이 이미 존재합니다. 덮어쓰지 않습니다");
        }
        let settings = dedicated_claude_title_bytes(&serde_json::to_vec(
            &serde_json::json!({"env": {
                "ANTHROPIC_BASE_URL": claude_base_url(options.data_port), "ANTHROPIC_AUTH_TOKEN": key
            }}),
        )?)?;
        add_dirs(&mut entries, &home, apply)?;
        entries.push(new_entry(
            home.join("settings.json"),
            Kind::File,
            None,
            Some(&settings),
            apply,
        )?);
        entries.push(new_entry(
            paths.npm_dir.join("claude-qb.cmd"),
            Kind::File,
            None,
            Some(&claude_launcher(command)?),
            apply,
        )?);
        report.summary.push(format!(
            "Claude 별도 설정: {}",
            home.join("settings.json").display()
        ));
        report.summary.push(format!(
            "실행 파일: {}",
            paths.npm_dir.join("claude-qb.cmd").display()
        ));
        report.diff.extend(safe_diff(
            &home.join("settings.json"),
            &[],
            &settings,
            &secrets,
        ));
        report.diff.extend(safe_diff(
            &paths.npm_dir.join("claude-qb.cmd"),
            &[],
            &claude_launcher(command)?,
            &secrets,
        ));
        report.warnings.push("claude-qb로 시작한 세션만 quota-baton을 거칩니다. 기본 Claude 설정은 그대로이며 기록과 이어 열기는 양방향으로 공유합니다.".into());
        Some(record)
    } else {
        None
    };
    if mode == CodexMode::Dedicated {
        if !paths.npm_dir.is_dir() || !paths.npm_dir.join("codex.cmd").is_file() {
            bail!("npm 디렉터리 또는 codex.cmd가 없습니다. Codex 설치를 확인하세요");
        }
        if exists(&paths.codex_qb_home)? && !retained_home {
            bail!("전용 Codex 홈이 이미 존재합니다. 덮어쓰지 않습니다");
        }
        let config = config_bytes(paths, options.data_port)?;
        let doc = parse_config(paths)?;
        toml_strings(doc.as_item(), &mut secrets);
        let mut config_diff = safe_diff(
            &paths.codex_qb_home.join("config.toml"),
            &fs::read(&paths.codex_config)?,
            &config,
            &secrets,
        );
        config_diff[0] = format!("--- {} (원본, 바뀌지 않음)", paths.codex_config.display());
        config_diff[1] = format!(
            "+++ {} (새로 만듦)",
            paths.codex_qb_home.join("config.toml").display()
        );
        report.diff.extend(config_diff);
        report.diff.extend([
            "auth.json: {\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"<client key>\"}".into(),
        ]);
        report
            .summary
            .push(format!("전용 홈: {}", paths.codex_qb_home.display()));
        report.summary.push(format!(
            "실행 파일: {}",
            paths.npm_dir.join("codex-qb.cmd").display()
        ));
        report.warnings.extend([USAGE.into(), HERDR.into()]);
        add_dirs(&mut entries, &paths.codex_qb_home, apply)?;
        add_dirs(&mut entries, &paths.codex_qb_home.join("log"), apply)?;
        entries.push(new_entry(
            paths.codex_qb_home.join("config.toml"),
            Kind::File,
            None,
            Some(&config),
            apply,
        )?);
        let auth = serde_json::to_vec_pretty(
            &serde_json::json!({"auth_mode":"apikey", "OPENAI_API_KEY":key}),
        )?;
        entries.push(new_entry(
            paths.codex_qb_home.join("auth.json"),
            Kind::File,
            None,
            Some(&auth),
            apply,
        )?);
        let source = paths
            .codex_config
            .parent()
            .context("Codex 원본 홈이 없습니다")?;
        for name in SHARED {
            let target = source.join(name);
            if exists(&target)? {
                if !target.is_dir() {
                    bail!("공유 디렉터리 원본이 아닙니다");
                }
                entries.push(new_entry(
                    paths.codex_qb_home.join(name),
                    Kind::Junction,
                    Some(target),
                    None,
                    apply,
                )?);
            }
        }
        for name in ["AGENTS.md", "hooks.json"] {
            let target = source.join(name);
            if exists(&target)? {
                if reparse(&target)? || !target.is_file() {
                    bail!("공유 원본이 일반 파일이 아닙니다");
                }
                let mut e = new_entry(
                    paths.codex_qb_home.join(name),
                    Kind::Hardlink,
                    Some(target.clone()),
                    None,
                    apply,
                )?;
                e.hash = hash_file(&target)?;
                e.identity = Some(identity(&target)?);
                entries.push(e);
            }
        }
        entries.push(new_entry(
            paths.npm_dir.join("codex-qb.cmd"),
            Kind::File,
            None,
            Some(&launcher()),
            apply,
        )?);
        for e in &entries {
            if e.original.is_none()
                && exists(&e.path)?
                && !(e.done && e.kind == Kind::Directory && matches_entry(e, false)?)
            {
                bail!("생성 경로가 이미 존재합니다: {}", e.path.display());
            }
            if e.kind == Kind::File && exists(&e.path.with_extension("qb-pending"))? {
                bail!("임시 경로가 이미 존재합니다");
            }
        }
    }
    for e in &entries {
        if e.original.is_none()
            && exists(&e.path)?
            && !(e.done && e.kind == Kind::Directory && matches_entry(e, false)?)
        {
            bail!("생성 경로가 이미 존재합니다: {}", e.path.display());
        }
    }
    if mode == CodexMode::Default {
        let auth: Value = serde_json::from_str(&read_text(&paths.codex_auth)?)
            .map_err(|_| anyhow::anyhow!("Codex auth.json 형식이 올바르지 않습니다"))?;
        if apply && auth.get("auth_mode").and_then(Value::as_str) != Some("apikey") {
            bail!("기본 Codex auth.json이 apikey 인증이 아니어서 연결을 중단했습니다.");
        }
        json_strings(&auth, &mut secrets);
        let config = preserved_bytes(
            &paths.codex_config,
            &codex_config_bytes(
                &paths.codex_config,
                &codex_base_url(options.data_port),
                &key,
            )?,
        )?;
        let auth = preserved_bytes(
            &paths.codex_auth,
            &codex_auth_bytes(&paths.codex_auth, &key)?,
        )?;
        for (path, bytes) in [(&paths.codex_config, config), (&paths.codex_auth, auth)] {
            let before = fs::read(path)?;
            if path == &paths.codex_config {
                toml_strings(
                    read_text(path)?
                        .parse::<DocumentMut>()
                        .map_err(|_| anyhow::anyhow!("Codex config 형식이 올바르지 않습니다"))?
                        .as_item(),
                    &mut secrets,
                );
            } else {
                json_strings(
                    &serde_json::from_slice::<Value>(strip_utf8_bom(&before))?,
                    &mut secrets,
                );
            }
            report
                .diff
                .extend(safe_diff(path, &before, &bytes, &secrets));
            entries.push(existing_entry(path.clone(), &bytes, apply)?);
        }
        for path in &paths.codex_session_auth {
            if normal_path(path).contains("\\.codex-acct\\") || !path.exists() {
                continue;
            }
            if session_journal(path, &key)?.is_some() {
                let bytes = preserved_bytes(path, &codex_auth_bytes(path, &key)?)?;
                entries.push(existing_entry(path.clone(), &bytes, apply)?);
            }
        }
        if options.detach_chatgpt_session_homes
            && let Some(path) = paths.session_homes_file.as_ref().filter(|p| p.exists())
        {
            let homes = paths
                .codex_session_homes
                .iter()
                .filter(|h| session_home_is_detachable(h))
                .cloned()
                .collect::<Vec<_>>();
            let bytes = detach_chatgpt_session_homes(path, &homes)?;
            entries.push(existing_entry(path.clone(), &bytes, apply)?);
        }
    }
    report
        .changed
        .extend(entries.iter().map(|e| e.path.display().to_string()));
    if claude_mode == ClaudeMode::Default {
        report
            .changed
            .push("claude.env.ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN".into());
    }
    let mut manifest = Manifest {
        schema: 3,
        mode,
        claude_mode,
        command,
        home: paths.codex_qb_home.clone(),
        npm: paths.npm_dir.clone(),
        claude: paths.claude_settings.clone(),
        claude_before: hash_bytes(&claude_before),
        claude_after: hash_bytes(&claude_after),
        claude_original: if apply {
            protect(&claude_before)?
        } else {
            String::new()
        },
        claude_payload: if apply {
            protect(&claude_after)?
        } else {
            String::new()
        },
        claude_identity: if claude_mode == ClaudeMode::Default {
            Some(identity(&paths.claude_settings)?)
        } else {
            None
        },
        claude_restored: claude_mode != ClaudeMode::Default,
        autostart: if options.autostart {
            let (before, before_type) = read_autostart()?;
            Some(OptionalActionJournal {
                before,
                before_type,
                after: tray_autostart_command()?,
                after_type: REG_SZ.0,
            })
        } else {
            None
        },
        tasks: if options.disable_cpa_tasks {
            cpa_task_journal(task_query)?
        } else {
            Vec::new()
        },
        optional_restored: false,
        retained: false,
        install_completed: Some(false),
        entries,
    };
    validate_manifest(paths, &manifest)?;
    if !apply {
        return Ok(report);
    }
    atomic_json(&paths.journal(), &manifest)?;
    install(paths, &mut manifest)?;
    report.warnings.extend(scratch_warnings(&manifest));
    Ok(report)
}

/// Patch only the two owned env members; keep all other text and ordering.
fn restore_claude_env_text(original: &[u8], current: &str) -> Result<String> {
    let invalid = || anyhow::anyhow!("Claude 설정 복구 형식이 올바르지 않습니다");
    let original = std::str::from_utf8(original)
        .map_err(|_| invalid())?
        .trim_start_matches('\u{feff}');
    let before: Value = serde_json::from_str(original).map_err(|_| invalid())?;
    let mut expected: Value = serde_json::from_str(current).map_err(|_| invalid())?;
    let env = expected
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .context("Claude env가 없습니다")?;
    let mut text = current.to_owned();
    for key in ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"] {
        let value = before.get("env").and_then(|env| env.get(key));
        if let Some(v) = value {
            env.insert(key.into(), v.clone());
        } else {
            env.remove(key);
        }
        // Use the established string/span helper whenever the key is present.
        if let Some(updated) = value
            .and_then(Value::as_str)
            .and_then(|v| replace_json_string_in_env(&text, key, v))
        {
            text = updated;
            continue;
        }
        let (start, end) =
            json_object_span(&text, "env").context("Claude env 위치를 찾지 못했습니다")?;
        let mut object = text[start..end].to_owned();
        let members = json_member_spans(&object).ok_or_else(invalid)?;
        if let Some((index, member)) = members
            .iter()
            .enumerate()
            .find(|(_, member)| member.0 == key)
        {
            if let Some(value) = value {
                object.replace_range(member.2..member.3, &serde_json::to_string(value)?);
            } else {
                // Drop only the member and one separator, preserving surrounding whitespace.
                let range = if index + 1 < members.len() {
                    let comma = object[member.3..members[index + 1].1]
                        .find(',')
                        .ok_or_else(invalid)?
                        + member.3;
                    member.1..comma + 1
                } else if index > 0 {
                    let previous = &members[index - 1];
                    let comma =
                        object[previous.3..member.1].find(',').ok_or_else(invalid)? + previous.3;
                    comma..member.3
                } else {
                    member.1..member.3
                };
                object.replace_range(range, "");
            }
        } else if let Some(value) = value {
            let insertion = format!(
                "{}{}:{}",
                if members.is_empty() { "" } else { "," },
                serde_json::to_string(key)?,
                serde_json::to_string(value)?
            );
            let position = members.last().map_or(1, |member| member.3);
            object.insert_str(position, &insertion);
        }
        text.replace_range(start..end, &object);
    }
    // The old span helper can encounter a similarly named nested object first.
    // A semantic equality check prevents writing anything outside root.env.
    let actual: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    if actual != expected {
        return Err(invalid());
    }
    Ok(text)
}

/// Direct object members as (decoded key, key start, value start, value end).
fn json_member_spans(text: &str) -> Option<Vec<(String, usize, usize, usize)>> {
    let mut members = Vec::new();
    let bytes = text.as_bytes();
    let mut position = 1;
    loop {
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        if bytes.get(position) == Some(&b'}') {
            return Some(members);
        }
        let key_start = position;
        let mut key = serde_json::Deserializer::from_str(&text[position..]).into_iter::<String>();
        let name = key.next()?.ok()?;
        position += key.byte_offset();
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        if bytes.get(position) != Some(&b':') {
            return None;
        }
        position += 1;
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        let value_start = position;
        let mut value = serde_json::Deserializer::from_str(&text[position..]).into_iter::<Value>();
        value.next()?.ok()?;
        position += value.byte_offset();
        members.push((name, key_start, value_start, position));
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        match bytes.get(position) {
            Some(b',') => position += 1,
            Some(b'}') => return Some(members),
            _ => return None,
        }
    }
}

pub(super) fn unwire(paths: &CutoverPaths, apply: bool, force: bool) -> Result<Report> {
    let mut m: Manifest = serde_json::from_slice(&fs::read(paths.journal())?)
        .map_err(|_| anyhow::anyhow!("dedicated journal 형식이 올바르지 않습니다"))?;
    validate_manifest(paths, &m)?;
    let mut report = Report {
        summary: vec!["요약: journal에 기록된 연결만 되돌립니다".into()],
        ..Report::default()
    };
    report.warnings.extend(scratch_warnings(&m));
    report.changed.extend(
        m.entries
            .iter()
            .filter(|e| !e.removed)
            .map(|e| e.path.display().to_string()),
    );
    if m.mode == CodexMode::Dedicated && exists(&m.home.join("sessions"))? {
        report.warnings.push(format!(
            "세션 기록을 보존합니다: {}",
            m.home.join("sessions").display()
        ));
    }
    if !apply {
        return Ok(report);
    }
    // Resolve only durable upgrade intents before deleting anything.
    // Force cannot bless substituted or edited in-flight old/new objects.
    for e in m.entries.iter().filter(|e| e.upgrade.is_some()) {
        upgrade_state(e)?;
    }
    for i in 0..m.entries.len() {
        if m.entries[i].upgrade.is_some() {
            resume_upgrade(paths, &mut m, i)?;
        }
    }
    let mut refused = Vec::new();
    for i in (0..m.entries.len()).rev() {
        let e = &m.entries[i];
        if e.removed {
            continue;
        }
        ancestors_safe(&e.path)?;
        if let Some(original) = e.original.clone() {
            if !original.restoring {
                if identity(&e.path)? == original.identity && hash_file(&e.path)? == original.hash {
                    if let Some(tmp) = &e.pending
                        && exists(tmp)?
                    {
                        if e.pending_identity.is_some() && pending_matches(e, true)? {
                            fs::remove_file(tmp)?;
                        } else if scratch_scoped(e, tmp) {
                            report.warnings.push(format!(
                                "identity가 기록되지 않은 임시 경로를 보존합니다: {}",
                                tmp.display()
                            ));
                        } else {
                            refused.push(tmp.display().to_string());
                            continue;
                        }
                    }
                    m.entries[i].removed = true;
                    atomic_json(&paths.journal(), &m)?;
                    continue;
                }
                if !matches_entry(e, force)? {
                    refused.push(e.path.display().to_string());
                    continue;
                }
                let bytes = if hash_file(&e.path)? == e.hash {
                    unprotect(&original.payload)?
                } else {
                    restore_existing_text(paths, e)?
                };
                let path = e.path.clone();
                let mut reverse = existing_entry(path, &bytes, true)?;
                reverse.original.as_mut().unwrap().restoring = true;
                // Keep any old unproven scratch visible in the durable record.
                reverse.abandoned.extend(e.abandoned.clone());
                if let Some(tmp) = &e.pending
                    && exists(tmp)?
                {
                    if pending_matches(e, false)? {
                        fs::remove_file(tmp)?;
                    } else {
                        refused.push(tmp.display().to_string());
                        continue;
                    }
                }
                m.entries[i] = reverse;
                atomic_json(&paths.journal(), &m)?;
            }
            install_existing(paths, &mut m, i)?;
            m.entries[i].identity = Some(identity(&m.entries[i].path)?);
            m.entries[i].removed = true;
            atomic_json(&paths.journal(), &m)?;
            continue;
        }
        if let Some(tmp) = e.pending.as_ref().filter(|_| !e.removed)
            && exists(tmp)?
        {
            if e.pending_identity.is_none() && scratch_scoped(e, tmp) {
                report.warnings.push(format!(
                    "identity가 기록되지 않은 임시 경로를 보존합니다: {}",
                    tmp.display()
                ));
            } else if e.kind == Kind::File && pending_matches(e, force || !e.done)? {
                fs::remove_file(tmp)?;
            } else if e.kind == Kind::Directory || e.kind == Kind::Junction {
                let mut scratch = e.clone();
                scratch.path = tmp.clone();
                scratch.identity = e.pending_identity.clone();
                let empty_backing = e.kind == Kind::Junction
                    && !reparse(tmp)?
                    && tmp.is_dir()
                    && e.pending_identity.as_ref() == Some(&identity(tmp)?)
                    && fs::read_dir(tmp)?.next().is_none();
                if matches_entry(&scratch, false)? || empty_backing {
                    // No recursion, including for owned scratch directories.
                    fs::remove_dir(tmp)?;
                } else {
                    refused.push(tmp.display().to_string());
                    continue;
                }
            } else {
                refused.push(tmp.display().to_string());
                continue;
            }
        }
        if !exists(&e.path)? {
            m.entries[i].removed = true;
            atomic_json(&paths.journal(), &m)?;
            continue;
        }
        let unfinished_junction = e.kind == Kind::Junction
            && !e.done
            && !reparse(&e.path)?
            && e.path.is_dir()
            && e.identity.as_ref() == Some(&identity(&e.path)?)
            && fs::read_dir(&e.path)?.next().is_none();
        if !matches_entry(e, force)? && !unfinished_junction {
            refused.push(e.path.display().to_string());
            continue;
        }
        match e.kind {
            Kind::Directory => {
                // remove_dir never recurses and cannot remove sessions or unknown content.
                if fs::read_dir(&e.path)?.next().is_some() {
                    report.warnings.push(format!(
                        "비어 있지 않아 보존합니다 (sessions 포함): {}",
                        e.path.display()
                    ));
                    continue;
                }
                fs::remove_dir(&e.path)?;
            }
            Kind::Junction => fs::remove_dir(&e.path)?,
            Kind::File | Kind::Hardlink => fs::remove_file(&e.path)?,
        }
        m.entries[i].removed = true;
        atomic_json(&paths.journal(), &m)?;
    }
    if !m.claude_restored {
        let current = hash_file(&m.claude)?;
        if current == m.claude_before {
            m.claude_restored = true;
        } else if current == m.claude_after
            || (force && Some(identity(&m.claude)?) == m.claude_identity && !reparse(&m.claude)?)
        {
            // Exact restore only while unchanged. Forced cleanup preserves unrelated keys.
            if current == m.claude_after {
                restore_exact(&m.claude, &m.claude_original)?;
            } else {
                let original = unprotect(&m.claude_original)?;
                let restored = restore_claude_env_text(&original, &read_text(&m.claude)?)?;
                atomic_preserved(&m.claude, restored.as_bytes())?;
            }
            m.claude_restored = true;
        } else {
            refused.push(m.claude.display().to_string());
        }
        atomic_json(&paths.journal(), &m)?;
    }
    if !m.optional_restored {
        if let Some(a) = &m.autostart {
            let current = read_autostart()?.0;
            if current.value != a.before.value && current.value != Some(a.after.clone()) && !force {
                bail!("autostart 설정이 변경되었습니다. journal을 보존합니다");
            }
            restore_autostart(&a.before, a.before_type)?;
        }
        for task in &m.tasks {
            let current =
                task_enabled(&task.name).context("CPA 예약 작업 상태를 확인할 수 없습니다")?;
            if current != task.was_enabled && current != task.after_enabled && !force {
                bail!("CPA 예약 작업 상태가 변경되었습니다. journal을 보존합니다");
            }
        }
        restore_cpa_tasks(&m.tasks)?;
        m.optional_restored = true;
        atomic_json(&paths.journal(), &m)?;
    }
    if !refused.is_empty() {
        bail!(
            "변경/identity 불일치로 보존했습니다: {}. journal을 보존합니다",
            refused.join(", ")
        );
    }
    if m.entries.iter().any(|e| !e.removed) {
        m.retained = true;
        atomic_json(&paths.journal(), &m)?;
    } else {
        fs::remove_file(paths.journal())?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Synthetic preparation only: these tests have NOT been executed.
    #[test]
    fn r32_codex_title_merge_preserves_unknown_keys_comments_and_inline_tables() {
        for input in [
            "# current dedicated config\nmodel = 'keep'\n",
            "# current dedicated config\nmodel = 'keep'\n[tui]\n# keep this comment\nnotifications = false\nterminal_title = ['model'] # title comment\n",
            "# current dedicated config\nmodel = 'keep'\ntui = { notifications = false, terminal_title = ['model'] }\n",
        ] {
            let result = dedicated_codex_title_bytes(input.as_bytes()).unwrap();
            let text = std::str::from_utf8(&result).unwrap();
            let doc: DocumentMut = text.parse().unwrap();
            assert_eq!(doc["model"].as_str(), Some("keep"));
            assert!(doc["tui"]["terminal_title"].as_array().unwrap().is_empty());
            assert!(text.contains("# current dedicated config"));
            if input.contains("notifications") {
                assert_eq!(doc["tui"]["notifications"].as_bool(), Some(false));
            }
            for comment in ["# keep this comment", "# title comment"] {
                if input.contains(comment) {
                    assert!(text.contains(comment));
                }
            }
            assert_eq!(dedicated_codex_title_bytes(&result).unwrap(), result);
        }
        let already = b"\xef\xbb\xbf[tui]\r\nterminal_title = [ ] # untouched\r\n";
        assert_eq!(dedicated_codex_title_bytes(already).unwrap(), already);
        let bom = b"\xef\xbb\xbf[tui]\nnotifications = false\n";
        assert!(
            dedicated_codex_title_bytes(bom)
                .unwrap()
                .starts_with(b"\xef\xbb\xbf")
        );
        for invalid in [
            "tui = false",
            "tui = []",
            "not TOML = [",
            "tui = 'private-do-not-print'",
        ] {
            let error = dedicated_codex_title_bytes(invalid.as_bytes()).unwrap_err();
            assert!(!error.to_string().contains("private-do-not-print"));
        }
        assert!(dedicated_codex_title_bytes(&[0xff]).is_err());
    }

    #[test]
    fn r32_claude_title_merge_preserves_current_env_and_unknown_root_values() {
        for input in [
            r#"{}"#,
            r#"{"env":{"KEEP":"unchanged","ANTHROPIC_AUTH_TOKEN":"synthetic-current-key","ANTHROPIC_CUSTOM_HEADERS":"x-existing: keep","CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"0"},"permissions":{"allow":["synthetic"]},"unknown":[1,true]}"#,
        ] {
            let before: Value = serde_json::from_str(input).unwrap();
            let result = dedicated_claude_title_bytes(input.as_bytes()).unwrap();
            let after: Value = serde_json::from_slice(&result).unwrap();
            assert_eq!(after["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"], "1");
            for (key, value) in before.as_object().unwrap() {
                if key == "env" {
                    for (key, value) in value.as_object().unwrap() {
                        if key != "CLAUDE_CODE_DISABLE_TERMINAL_TITLE" {
                            assert_eq!(&after["env"][key], value);
                        }
                    }
                } else {
                    assert_eq!(&after[key], value);
                }
            }
            assert_eq!(dedicated_claude_title_bytes(&result).unwrap(), result);
        }
        let already =
            b"\xef\xbb\xbf{\r\n \"env\": {\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"1\"}\r\n}\r\n";
        assert_eq!(dedicated_claude_title_bytes(already).unwrap(), already);
        let result = dedicated_claude_title_bytes(b"\xef\xbb\xbf{}").unwrap();
        assert!(result.starts_with(b"\xef\xbb\xbf"));
        for invalid in [
            "[]",
            "null",
            r#"{"env":null}"#,
            r#"{"env":[]}"#,
            "private-do-not-print",
        ] {
            let error = dedicated_claude_title_bytes(invalid.as_bytes()).unwrap_err();
            assert!(!error.to_string().contains("private-do-not-print"));
        }
    }

    #[test]
    fn r32_claude_title_merge_preserves_noncanonical_bytes_and_is_idempotent() {
        let noncanonical = concat!(
            "\u{feff} \r\n{\r\n",
            "  \"nested-first\" : {\"env\":{\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"nested-old\"}},\r\n",
            "\t\"env\" : { \"Z_LAST\" : [ 1,  2 ], \"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\" : \"0\" , \"A_FIRST\" : \"한글\" },\r\n",
            "  \"tail\" : { \"spacing\" : true }\r\n}\r\n"
        );
        let replaced = noncanonical.replace(
            "\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\" : \"0\"",
            "\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\" : \"1\"",
        );
        let existing_env = "\u{feff}{\r\n \"z\":0,\r\n \"env\" : {\r\n  \"Z\" : 1, \"A\" : [ 2,  3 ] \r\n },\r\n \"a\":true\r\n}\r\n";
        let inserted = existing_env.replace(
            "[ 2,  3 ]",
            "[ 2,  3 ],\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"1\"",
        );
        for (input, expected) in [
            (noncanonical, replaced.as_str()),
            (existing_env, inserted.as_str()),
            (
                " \r\n{ \"z\" : [ 1, 2 ], \"a\" : false \r\n}\r\n",
                " \r\n{ \"z\" : [ 1, 2 ], \"a\" : false,\"env\":{\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"1\"} \r\n}\r\n",
            ),
            (
                "\u{feff}{ \"env\" : { \r\n\t } , \"last\":true }\r\n",
                "\u{feff}{ \"env\" : {\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"1\" \r\n\t } , \"last\":true }\r\n",
            ),
            (
                "\u{feff}{ \r\n\t }\r\n",
                "\u{feff}{\"env\":{\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"1\"} \r\n\t }\r\n",
            ),
            (
                r#"{ "before": {"CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"nested"}, "env" : { "CLAUDE_CODE_DISABLE_TERMINAL_TITLE" : [ false, { "keep":true } ] , "tail" : 1 } }"#,
                r#"{ "before": {"CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"nested"}, "env" : { "CLAUDE_CODE_DISABLE_TERMINAL_TITLE" : "1" , "tail" : 1 } }"#,
            ),
            (
                r#"{ "\u0065nv" : { "\u0043LAUDE_CODE_DISABLE_TERMINAL_TITLE" : false }, "last":null }"#,
                r#"{ "\u0065nv" : { "\u0043LAUDE_CODE_DISABLE_TERMINAL_TITLE" : "1" }, "last":null }"#,
            ),
        ] {
            let result = dedicated_claude_title_bytes(input.as_bytes()).unwrap();
            assert_eq!(
                result,
                expected.as_bytes(),
                "unrelated settings bytes changed"
            );
            assert_eq!(dedicated_claude_title_bytes(&result).unwrap(), result);
        }
    }

    #[test]
    fn r32_launcher_bytes_scope_delayed_expansion_and_capture_exit_before_restore() {
        let codex = String::from_utf8(launcher()).unwrap();
        let claude =
            String::from_utf8(claude_launcher(Path::new(r"C:\synthetic bin\claude.cmd")).unwrap())
                .unwrap();
        for (script, client) in [(&codex, "codex"), (&claude, "claude")] {
            assert!(script.starts_with("@echo off\r\nsetlocal DisableDelayedExpansion\r\n"));
            assert!(script.contains("for /f \"tokens=2 delims=:\" %%P in ('chcp')"));
            assert!(script.contains("for %%I in (.) do set \"QB_LAUNCHER_FOLDER=%%~nxI\"\r\n"));
            assert!(script.contains(&format!(
                "setlocal EnableDelayedExpansion\r\ntitle {client}-qb · !QB_LAUNCHER_FOLDER!\r\nendlocal\r\n"
            )));
            let dispatch = script
                .find("\"%SystemRoot%\\System32\\cmd.exe\" /d /v:off /s /c ^\"\"")
                .unwrap();
            assert!(script.find("chcp 65001 >nul").unwrap() < script.find("title ").unwrap());
            assert!(script.find("endlocal\r\n").unwrap() < dispatch);
            assert!(script[dispatch..].contains(" %*^\"\r\nset \"QB_EXIT=%errorlevel%\"\r\nchcp %QB_CODEPAGE% >nul\r\nendlocal & exit /b %QB_EXIT%\r\n"));
            assert!(
                script
                    .as_bytes()
                    .windows(2)
                    .all(|pair| pair[1] != b'\n' || pair[0] == b'\r')
            );
        }
        assert!(codex.contains("set \"CODEX_HOME=%USERPROFILE%\\.codex-acct\\qb\"\r\n"));
        assert!(claude.contains("--settings \"%USERPROFILE%\\.claude-qb\\settings.json\" %*"));
        // Opt-in coordinator fixture: exercise the actual generated payloads in
        // separate hidden consoles, without adding a product capture command.
        if let Some(out) = std::env::var_os("QB_R32_NATIVE_OUT") {
            let out = PathBuf::from(out);
            let project = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap();
            let canonical = out.canonicalize().unwrap();
            assert!(canonical.starts_with(project.canonicalize().unwrap()));
            assert!(canonical.components().any(|c| c.as_os_str() == ".sandbox"));
            fs::write(out.join("codex-qb.cmd"), launcher()).unwrap();
            fs::write(
                out.join("claude-qb.cmd"),
                claude_launcher(&out.join("fake-claude.cmd")).unwrap(),
            )
            .unwrap();
            fs::write(
                out.join("claude-qb-exe.cmd"),
                claude_launcher(&project.join("target").join("debug").join("r32-capture.exe"))
                    .unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn r32_default_modes_do_not_add_dedicated_title_settings() {
        let (_dir, p) = fixture();
        fs::write(
            &p.codex_auth,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-old"}"#,
        )
        .unwrap();
        let before = snapshot(&p);
        let report = claude_run(&p, false, CodexMode::Default, Some(ClaudeMode::Default)).unwrap();
        let text = output(&report);
        assert!(!text.contains("terminal_title"));
        assert!(!text.contains("CLAUDE_CODE_DISABLE_TERMINAL_TITLE"));
        assert_eq!(snapshot(&p), before);
    }

    #[test]
    fn r32_dedicated_config_and_fresh_plan_do_not_mutate_default_inputs() {
        let (_dir, p) = fixture();
        let before = snapshot(&p);
        let config = config_bytes(&p, 10200).unwrap();
        let doc: DocumentMut = std::str::from_utf8(&config).unwrap().parse().unwrap();
        assert!(doc["tui"]["terminal_title"].as_array().unwrap().is_empty());
        let report =
            claude_run(&p, false, CodexMode::Dedicated, Some(ClaudeMode::Dedicated)).unwrap();
        assert!(output(&report).contains("CLAUDE_CODE_DISABLE_TERMINAL_TITLE"));
        assert_eq!(snapshot(&p), before);
        assert!(!p.codex_qb_home.exists());
        assert!(!claude_home(&p).unwrap().exists());
    }

    const KEY: &str = "synthetic-client-KEY-9281";
    const SECRET: &str = "synthetic-nested-SECRET-7126";
    fn fixture() -> (tempfile::TempDir, CutoverPaths) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r3-cutover-tests");
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let source = dir.path().join(".codex");
        let npm = dir.path().join("App Data/npm");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&npm).unwrap();
        fs::write(npm.join("codex.cmd"), "synthetic").unwrap();
        fs::write(npm.join("claude.cmd"), "synthetic-never-executed").unwrap();
        fs::write(
            source.join("config.toml"),
            format!(
                r#"# synthetic direct source
model = "synthetic-model"
model_provider = "cliproxyapi"
model_catalog_json = "catalog.json"
experimental_realtime_ws_base_url = "ws://127.0.0.1:10100/ws"
experimental_realtime_webrtc_call_base_url = "http://localhost:10100/realtime"
unrelated = "preserve-me"
[model_providers.cliproxyapi]
base_url = "http://127.0.0.1:10100/v1"
experimental_bearer_token = "{SECRET}"
[model_providers.unused]
base_url = "https://example.invalid/v1"
[mcp_servers.synthetic.env]
ARBITRARY_CREDENTIAL = "{SECRET}"
[hooks.state.'C:\synthetic\hooks.json:session_start:0:0']
trusted_hash = "synthetic-hook-hash"
"#
            ),
        )
        .unwrap();
        fs::write(
            source.join("auth.json"),
            format!(r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{SECRET}"}}}}"#),
        )
        .unwrap();
        for name in SHARED {
            fs::create_dir(source.join(name)).unwrap();
            fs::write(source.join(name).join("source-marker"), name).unwrap();
        }
        fs::write(source.join("AGENTS.md"), "synthetic instructions").unwrap();
        fs::write(source.join("hooks.json"), "{}").unwrap();
        let claude_settings = dir.path().join("claude/settings.json");
        fs::create_dir(claude_settings.parent().unwrap()).unwrap();
        fs::write(&claude_settings, format!(r#"{{"env":{{"ANTHROPIC_AUTH_TOKEN":"{SECRET}"}},"nested":{{"innocent":"{SECRET}"}}}}"#)).unwrap();
        let qb_home = dir.path().join("quota-baton");
        fs::create_dir(&qb_home).unwrap();
        let client_key = qb_home.join("client.key");
        fs::write(&client_key, KEY).unwrap();
        let p = CutoverPaths {
            qb_home,
            codex_auth: source.join("auth.json"),
            codex_config: source.join("config.toml"),
            codex_qb_home: dir.path().join(".codex-acct/qb"),
            npm_dir: npm.clone(),
            codex_session_auth: Vec::new(),
            codex_session_homes: Vec::new(),
            session_homes_file: None,
            session_homes_warning: None,
            codex_realtime_items: Vec::new(),
            claude_command: Some(npm.join("claude.cmd")),
            claude_settings,
            client_key,
            report_roots: Vec::new(),
            scan_roots: Vec::new(),
        };
        (dir, p)
    }
    fn opts(mode: Option<CodexMode>) -> WireOptions {
        WireOptions {
            codex_mode: mode,
            claude_mode: Some(ClaudeMode::Default),
            ..WireOptions::default()
        }
    }
    fn run(p: &CutoverPaths, apply: bool, mode: Option<CodexMode>) -> Result<Report> {
        wire_with_task_query(p, apply, opts(mode), &|_| {
            panic!("unrequested task backend")
        })
    }
    fn output(r: &Report) -> String {
        r.summary
            .iter()
            .chain(&r.changed)
            .chain(&r.warnings)
            .chain(&r.diff)
            .chain(&r.report_only)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn manifest(p: &CutoverPaths) -> Manifest {
        serde_json::from_slice(&fs::read(p.journal()).unwrap()).unwrap()
    }
    fn snapshot(p: &CutoverPaths) -> Vec<Vec<u8>> {
        let source = p.codex_config.parent().unwrap();
        ["config.toml", "auth.json", "AGENTS.md", "hooks.json"]
            .iter()
            .map(|n| source.join(n))
            .chain(SHARED.iter().map(|n| source.join(n).join("source-marker")))
            .map(|path| fs::read(path).unwrap())
            .collect()
    }

    #[test]
    fn detection_uses_auth_effective_endpoint_and_explicit_overrides() {
        let (_dir, p) = fixture();
        assert_eq!(
            resolve_codex_mode(&p, &opts(None)).unwrap(),
            CodexMode::Dedicated
        );
        fs::write(&p.codex_auth, r#"{"auth_mode":"apikey"}"#).unwrap();
        assert_eq!(
            resolve_codex_mode(&p, &opts(None)).unwrap(),
            CodexMode::Default
        );
        for url in [
            "http://127.0.0.1:101000/v1",
            "https://127.0.0.1:10100.evil.invalid",
            "http://127.0.0.1:10100@evil.invalid",
            "https://example.invalid/v1",
        ] {
            fs::write(&p.codex_config, format!("openai_base_url = '{url}'\n[model_providers.unused]\nbase_url = 'http://127.0.0.1:10100/v1'\n")).unwrap();
            assert_eq!(
                resolve_codex_mode(&p, &opts(None)).unwrap(),
                CodexMode::Dedicated
            );
        }
        fs::write(
            &p.codex_config,
            "model_provider = 'openai'\nopenai_base_url = 'http://[::1]:10100/v1'\n",
        )
        .unwrap();
        assert_eq!(
            resolve_codex_mode(&p, &opts(None)).unwrap(),
            CodexMode::Default
        );
        fs::write(&p.codex_config, "model_provider = 'other'\nopenai_base_url = 'http://127.0.0.1:10100/v1'\n[model_providers.other]\nbase_url = 'https://example.invalid'\n").unwrap();
        assert_eq!(
            resolve_codex_mode(&p, &opts(None)).unwrap(),
            CodexMode::Dedicated
        );
        for (name, mode) in [
            ("default", CodexMode::Default),
            ("dedicated", CodexMode::Dedicated),
            ("none", CodexMode::None),
        ] {
            assert_eq!(name.parse::<CodexMode>().unwrap(), mode);
            assert_eq!(resolve_codex_mode(&p, &opts(Some(mode))).unwrap(), mode);
        }
        fs::remove_file(&p.codex_auth).unwrap();
        assert_eq!(
            resolve_codex_mode(&p, &opts(None)).unwrap(),
            CodexMode::Dedicated
        );
    }
    #[test]
    fn malformed_input_errors_have_no_secret_or_parser_source_chain() {
        let (_dir, p) = fixture();
        fs::write(&p.codex_config, format!("secret = '{SECRET}\n")).unwrap();
        for error in [
            resolve_codex_mode(&p, &opts(None)).unwrap_err(),
            run(&p, false, Some(CodexMode::Dedicated)).unwrap_err(),
        ] {
            let text = format!("{error:#}\n{error:?}");
            assert!(!text.contains(SECRET));
            assert!(!text.contains("secret ="));
        }
        fs::write(
            &p.codex_config,
            "openai_base_url = 'http://localhost:10100/v1'\n",
        )
        .unwrap();
        fs::write(&p.codex_auth, format!("{{\"secret\":\"{SECRET}")).unwrap();
        let error = resolve_codex_mode(&p, &opts(None)).unwrap_err();
        assert!(!format!("{error:#}\n{error:?}").contains(SECRET));
    }
    #[test]
    fn preview_writes_nothing_and_redacts_every_report_stream() {
        let (_dir, p) = fixture();
        let before = snapshot(&p);
        fs::remove_file(&p.client_key).unwrap();
        let text = output(&run(&p, false, None).unwrap());
        assert!(!text.contains(SECRET));
        assert!(!text.contains(KEY));
        assert!(text.contains("<client key>"));
        assert!(text.contains("@@ "));
        assert!(text.contains(USAGE));
        assert!(text.contains(HERDR));
        assert!(!p.codex_qb_home.exists());
        assert!(!p.journal().exists());
        assert!(!p.npm_dir.join("codex-qb.cmd").exists());
        assert_eq!(before, snapshot(&p));
    }
    #[test]
    fn transform_preserves_unrelated_keys_copies_trust_and_places_endpoint_before_tables() {
        let (_dir, p) = fixture();
        let text = String::from_utf8(config_bytes(&p, 10277).unwrap()).unwrap();
        let doc = text.parse::<DocumentMut>().unwrap();
        assert_eq!(
            doc["openai_base_url"].as_str(),
            Some("http://127.0.0.1:10277/v1")
        );
        assert!(text.find("openai_base_url").unwrap() < text.find('[').unwrap());
        for key in [
            "model_provider",
            "model_catalog_json",
            "experimental_realtime_ws_base_url",
            "experimental_realtime_webrtc_call_base_url",
        ] {
            assert!(doc.get(key).is_none());
        }
        assert!(doc["model_providers"].get("cliproxyapi").is_none());
        assert_eq!(
            doc["model_providers"]["unused"]["base_url"].as_str(),
            Some("https://example.invalid/v1")
        );
        assert_eq!(doc["unrelated"].as_str(), Some("preserve-me"));
        let key = format!(
            "{}:session_start:0:0",
            p.codex_qb_home.join("hooks.json").display()
        );
        assert_eq!(
            doc["hooks"]["state"][key.as_str()]["trusted_hash"].as_str(),
            Some("synthetic-hook-hash")
        );
        let launcher = String::from_utf8(launcher()).unwrap();
        assert!(launcher.starts_with("@echo off\r\nsetlocal DisableDelayedExpansion\r\nrem "));
        assert!(launcher.contains("set \"CODEX_HOME=%USERPROFILE%\\.codex-acct\\qb\"\r\n"));
        assert!(launcher.contains("/s /c ^\"\"%APPDATA%\\npm\\codex.cmd\" %*^\"\r\n"));
        assert!(launcher.ends_with("chcp %QB_CODEPAGE% >nul\r\nendlocal & exit /b %QB_EXIT%\r\n"));
        assert!(normal_path(&p.codex_qb_home).contains(".codex-acct"));
    }
    #[test]
    fn none_without_codex_inputs_and_no_detach_or_task_side_effects() {
        let (_dir, mut p) = fixture();
        p.codex_config = p.qb_home.join("missing/config.toml");
        p.codex_auth = p.qb_home.join("missing/auth.json");
        p.npm_dir = p.qb_home.join("missing/npm");
        let original = fs::read(&p.claude_settings).unwrap();
        for apply in [false, true] {
            assert!(!output(&run(&p, apply, Some(CodexMode::None)).unwrap()).contains(KEY));
        }
        unwire(&p, true, false).unwrap();
        assert_eq!(original, fs::read(&p.claude_settings).unwrap());
        assert!(!p.codex_qb_home.exists());
        for mode in [CodexMode::None, CodexMode::Dedicated] {
            let options = WireOptions {
                detach_chatgpt_session_homes: true,
                ..opts(Some(mode))
            };
            assert!(wire_with_task_query(&p, false, options, &|_| panic!("task backend")).is_err());
        }
    }
    #[test]
    fn roundtrip_keeps_sources_and_sessions_and_retained_home_can_rewire() {
        let (_dir, p) = fixture();
        let before = snapshot(&p);
        let claude = fs::read(&p.claude_settings).unwrap();
        for pass in 0..2 {
            assert!(!output(&run(&p, true, None).unwrap()).contains(KEY));
            for name in SHARED {
                let link = p.codex_qb_home.join(name);
                assert!(reparse(&link).unwrap());
                assert!(target_same(
                    &fs::read_link(link).unwrap(),
                    &p.codex_config.parent().unwrap().join(name)
                ));
            }
            for name in ["AGENTS.md", "hooks.json"] {
                assert_eq!(
                    identity(&p.codex_qb_home.join(name)).unwrap(),
                    identity(&p.codex_config.parent().unwrap().join(name)).unwrap()
                );
            }
            let sessions = p.codex_qb_home.join("sessions");
            fs::create_dir_all(&sessions).unwrap();
            fs::write(
                sessions.join(format!("synthetic-{pass}.jsonl")),
                "synthetic session",
            )
            .unwrap();
            unwire(&p, true, false).unwrap();
            assert!(sessions.join("synthetic-0.jsonl").exists());
            assert!(manifest(&p).retained);
            assert!(!p.codex_qb_home.join("auth.json").exists());
            assert!(!p.npm_dir.join("codex-qb.cmd").exists());
            assert_eq!(before, snapshot(&p));
            assert_eq!(claude, fs::read(&p.claude_settings).unwrap());
        }
    }
    #[test]
    fn changed_file_refuses_per_file_force_removes_only_owned_identity() {
        let (_dir, p) = fixture();
        let before = snapshot(&p);
        run(&p, true, None).unwrap();
        let launcher = p.npm_dir.join("codex-qb.cmd");
        fs::write(&launcher, "edited launcher").unwrap();
        assert!(
            unwire(&p, true, false)
                .unwrap_err()
                .to_string()
                .contains("codex-qb.cmd")
        );
        assert!(launcher.exists());
        assert!(p.journal().exists());
        assert!(!p.codex_qb_home.join("auth.json").exists());
        unwire(&p, true, true).unwrap();
        assert!(!launcher.exists());
        assert!(!p.journal().exists());
        assert_eq!(before, snapshot(&p));
    }
    #[test]
    fn replacement_identity_and_retargeted_junction_survive_force() {
        let (_dir, p) = fixture();
        run(&p, true, None).unwrap();
        let launcher = p.npm_dir.join("codex-qb.cmd");
        fs::rename(&launcher, p.npm_dir.join("old-launcher")).unwrap();
        fs::write(&launcher, "unowned replacement").unwrap();
        let link = p.codex_qb_home.join("skills");
        fs::remove_dir(&link).unwrap();
        fs::create_dir(&link).unwrap();
        junction(&link, &p.npm_dir).unwrap();
        assert!(unwire(&p, true, true).is_err());
        assert_eq!(fs::read_to_string(launcher).unwrap(), "unowned replacement");
        assert!(reparse(&link).unwrap());
        assert!(p.npm_dir.join("codex.cmd").exists());
        fs::remove_dir(link).unwrap();
    }
    #[test]
    fn missing_launcher_and_unowned_collisions_are_refused_before_journal() {
        let (_dir, p) = fixture();
        fs::remove_file(p.npm_dir.join("codex.cmd")).unwrap();
        assert!(run(&p, false, None).is_err());
        assert!(!p.journal().exists());
        fs::write(p.npm_dir.join("codex.cmd"), "synthetic").unwrap();
        fs::write(p.npm_dir.join("codex-qb.cmd"), "unowned").unwrap();
        assert!(run(&p, true, None).is_err());
        assert!(!p.journal().exists());
        fs::remove_file(p.npm_dir.join("codex-qb.cmd")).unwrap();
        fs::create_dir_all(&p.codex_qb_home).unwrap();
        assert!(run(&p, false, None).is_err());
        fs::remove_dir(&p.codex_qb_home).unwrap();
        for name in ["AGENTS.md", "hooks.json"] {
            fs::remove_file(p.codex_config.parent().unwrap().join(name)).unwrap();
        }
        run(&p, true, None).unwrap();
        assert!(!p.codex_qb_home.join("hooks.json").exists());
        unwire(&p, true, false).unwrap();
    }
    #[test]
    fn invalid_task_scope_is_rejected_before_any_mutation() {
        for mode in [CodexMode::Dedicated, CodexMode::None] {
            for invalid in 0..3 {
                let (_dir, p) = fixture();
                run(&p, true, Some(mode)).unwrap();
                let mut m = manifest(&p);
                m.tasks = vec![TaskJournal {
                    name: if invalid == 0 {
                        "unrelated-synthetic-task"
                    } else {
                        "cliproxyapi-proxy"
                    }
                    .into(),
                    was_enabled: true,
                    after_enabled: invalid == 2,
                }];
                if invalid == 1 {
                    m.tasks.push(m.tasks[0].clone());
                }
                atomic_json(&p.journal(), &m).unwrap();
                let before = resume_snapshot(&p);
                let sources = snapshot(&p);
                for apply in [false, true] {
                    let error = run(&p, apply, Some(mode)).unwrap_err();
                    assert!(error.to_string().contains("CPA 예약 작업 범위/상태"));
                    for force in [false, true] {
                        // Public dispatcher must validate schema 3 before touching any backend.
                        let error = super::super::unwire(&p, apply, force).unwrap_err();
                        assert!(error.to_string().contains("CPA 예약 작업 범위/상태"));
                        assert_eq!(resume_snapshot(&p), before);
                        assert_eq!(snapshot(&p), sources);
                    }
                }
            }
        }
    }

    #[test]
    fn creation_and_publication_interruptions_recover_without_adopting_unknown_scratch() {
        for target in ["parent", "home", "log", "agents", "auth.json"] {
            for stage in ["after-create", "after-publish"] {
                for resume in [false, true] {
                    let (_dir, p) = fixture();
                    let sources = snapshot(&p);
                    let path = match target {
                        "parent" => p.codex_qb_home.parent().unwrap().to_path_buf(),
                        "home" => p.codex_qb_home.clone(),
                        name => p.codex_qb_home.join(name),
                    };
                    INTERRUPT.with(|slot| *slot.borrow_mut() = Some((path.clone(), stage)));
                    assert!(
                        run(&p, true, None)
                            .unwrap_err()
                            .to_string()
                            .contains("synthetic creation interruption")
                    );
                    let m = manifest(&p);
                    let e = m.entries.iter().find(|e| e.path == path).unwrap();
                    assert!(!e.done);
                    assert!(e.identity.is_none());
                    let scratch = pending_path(e);
                    let mut unproven = None;
                    if stage == "after-create" {
                        assert!(!exists(&path).unwrap());
                        assert!(e.pending_identity.is_none());
                        let marker = if e.kind == Kind::File {
                            scratch.clone()
                        } else {
                            fs::create_dir(scratch.join("sessions")).unwrap();
                            scratch.join("sessions/unowned.jsonl")
                        };
                        fs::write(&marker, "unowned scratch content").unwrap();
                        unproven = Some((identity(&scratch).unwrap(), marker));
                    } else {
                        assert!(e.pending_identity.is_some());
                        assert!(matches_entry(e, false).unwrap());
                    }
                    let report = if resume {
                        let report = run(&p, true, None).unwrap();
                        assert!(manifest(&p).entries.iter().all(|e| e.done));
                        report
                    } else {
                        super::super::unwire(&p, true, true).unwrap()
                    };
                    if let Some((id, marker)) = &unproven {
                        assert_eq!(identity(&scratch).unwrap(), *id);
                        assert_eq!(
                            fs::read_to_string(marker).unwrap(),
                            "unowned scratch content"
                        );
                        assert!(output(&report).contains(scratch.to_str().unwrap()));
                    }
                    if resume {
                        let sessions = p.codex_qb_home.join("sessions");
                        fs::create_dir(&sessions).unwrap();
                        fs::write(sessions.join("kept.jsonl"), "resumable history").unwrap();
                        super::super::unwire(&p, true, true).unwrap();
                        assert_eq!(
                            fs::read_to_string(sessions.join("kept.jsonl")).unwrap(),
                            "resumable history"
                        );
                    }
                    if let Some((id, marker)) = unproven {
                        assert_eq!(identity(&scratch).unwrap(), id);
                        assert_eq!(
                            fs::read_to_string(marker).unwrap(),
                            "unowned scratch content"
                        );
                    }
                    assert_eq!(snapshot(&p), sources);
                }
            }
        }
    }

    #[test]
    fn scratch_scope_rejects_unknown_and_session_paths_without_mutation() {
        for abandoned in [false, true] {
            for invalid in [
                "sessions",
                "../escape",
                ".qb-scratch-config.toml-1-2-3:stream",
                ".qb-scratch-auth.json-1-2-3",
            ] {
                let (_dir, p) = fixture();
                run(&p, true, None).unwrap();
                let mut m = manifest(&p);
                let e = m
                    .entries
                    .iter_mut()
                    .find(|e| e.path == p.codex_qb_home.join("config.toml"))
                    .unwrap();
                let bad = p.codex_qb_home.join(invalid);
                if abandoned {
                    e.abandoned.push(bad);
                } else {
                    e.pending = Some(bad);
                }
                atomic_json(&p.journal(), &m).unwrap();
                let before = resume_snapshot(&p);
                assert!(run(&p, true, None).is_err());
                assert!(super::super::unwire(&p, true, true).is_err());
                assert_eq!(resume_snapshot(&p), before);
            }
        }
    }

    #[test]
    fn publication_refuses_existing_final_objects_without_clobbering() {
        for name in ["log", "agents", "auth.json"] {
            let (_dir, p) = fixture();
            let path = p.codex_qb_home.join(name);
            INTERRUPT.with(|slot| *slot.borrow_mut() = Some((path.clone(), "after-create")));
            assert!(run(&p, true, None).is_err());
            let mut m = manifest(&p);
            let i = m.entries.iter().position(|e| e.path == path).unwrap();
            let scratch = pending_path(&m.entries[i]);
            // Synthetic state immediately after the durable identity write.
            m.entries[i].pending_identity = Some(identity(&scratch).unwrap());
            atomic_json(&p.journal(), &m).unwrap();
            let marker = if m.entries[i].kind == Kind::File {
                fs::write(&path, "unknown final file").unwrap();
                path.clone()
            } else {
                fs::create_dir(&path).unwrap();
                fs::create_dir(path.join("sessions")).unwrap();
                let marker = path.join("sessions/unknown.jsonl");
                fs::write(&marker, "unknown final directory").unwrap();
                marker
            };
            let id = identity(&path).unwrap();
            let content = fs::read(&marker).unwrap();
            if m.entries[i].kind != Kind::File {
                // Exercise the native no-replace primitive too, independently
                // of install_directory's earlier exists check.
                assert!(publish_directory(&scratch, &path).is_err());
            }
            let result = if m.entries[i].kind == Kind::File {
                install_file(&p, &mut m, i)
            } else {
                install_directory(&p, &mut m, i)
            };
            assert!(result.is_err());
            assert_eq!(identity(&path).unwrap(), id);
            assert_eq!(fs::read(&marker).unwrap(), content);
            assert!(run(&p, true, None).is_err());
            assert!(super::super::unwire(&p, true, true).is_err());
            assert_eq!(identity(&path).unwrap(), id);
            assert_eq!(fs::read(&marker).unwrap(), content);
        }
    }

    #[test]
    fn completed_entry_boundaries_resume_and_unwire_is_idempotent() {
        let (_dir, p) = fixture();
        run(&p, true, None).unwrap();
        let template = manifest(&p);
        unwire(&p, true, false).unwrap();
        for boundary in 0..=template.entries.len() {
            let mut m: Manifest =
                serde_json::from_slice(&serde_json::to_vec(&template).unwrap()).unwrap();
            for e in &mut m.entries {
                e.done = false;
                e.removed = false;
                if e.kind != Kind::Hardlink {
                    e.identity = None;
                }
                e.pending_identity = None;
            }
            let rest = m.entries.split_off(boundary);
            atomic_json(&p.journal(), &m).unwrap();
            install(&p, &mut m).unwrap();
            m.entries.extend(rest);
            atomic_json(&p.journal(), &m).unwrap();
            run(&p, true, None).unwrap();
            unwire(&p, true, false).unwrap();
            assert!(!p.journal().exists());
            assert!(!p.codex_qb_home.exists());
            // Once the journal is gone, the public dispatcher owns the no-op;
            // it cannot select this schema-specific helper without a journal.
            assert!(super::super::unwire(&p, true, false).is_ok());
        }
    }
    #[test]
    fn unrecorded_directory_identity_and_sessions_manifest_paths_are_refused() {
        let (_dir, p) = fixture();
        run(&p, true, None).unwrap();
        let mut m = manifest(&p);
        let root = m
            .entries
            .iter_mut()
            .find(|e| e.path == p.codex_qb_home)
            .unwrap();
        root.identity = None;
        root.pending_identity = None;
        root.done = false;
        atomic_json(&p.journal(), &m).unwrap();
        assert!(unwire(&p, true, true).is_err());
        assert!(p.codex_qb_home.exists());
        let mut m = manifest(&p);
        m.entries.push(
            new_entry(
                p.codex_qb_home.join("sessions"),
                Kind::Directory,
                None,
                None,
                true,
            )
            .unwrap(),
        );
        assert!(validate_manifest(&p, &m).is_err());
    }
    #[test]
    fn interrupted_pending_write_resumes_by_exact_identity_and_publishes_atomic_file() {
        let (_dir, p) = fixture();
        run(&p, true, None).unwrap();
        let mut m = manifest(&p);
        unwire(&p, true, false).unwrap();
        for e in &mut m.entries {
            e.done = false;
            e.removed = false;
            if e.kind != Kind::Hardlink {
                e.identity = None;
            }
            e.pending_identity = None;
        }
        let first_file = m.entries.iter().position(|e| e.kind == Kind::File).unwrap();
        let rest = m.entries.split_off(first_file);
        atomic_json(&p.journal(), &m).unwrap();
        install(&p, &mut m).unwrap();
        m.entries.extend(rest);
        let tmp = pending_path(&m.entries[first_file]);
        fs::write(&tmp, "interrupted incomplete synthetic bytes").unwrap();
        m.entries[first_file].pending_identity = Some(identity(&tmp).unwrap());
        atomic_json(&p.journal(), &m).unwrap();
        run(&p, true, None).unwrap();
        assert!(!tmp.exists());
        assert_eq!(
            hash_file(&m.entries[first_file].path).unwrap(),
            m.entries[first_file].hash
        );
        unwire(&p, true, false).unwrap();
        assert!(!p.journal().exists());
    }
    #[test]
    fn junction_conversion_interruption_and_nonempty_log_are_safe() {
        let (_dir, p) = fixture();
        run(&p, true, None).unwrap();
        let mut m = manifest(&p);
        let entry = m
            .entries
            .iter_mut()
            .find(|e| e.kind == Kind::Junction)
            .unwrap();
        fs::remove_dir(&entry.path).unwrap();
        fs::create_dir(&entry.path).unwrap();
        entry.identity = Some(identity(&entry.path).unwrap());
        entry.done = false;
        atomic_json(&p.journal(), &m).unwrap();
        run(&p, true, None).unwrap();
        let log = p.codex_qb_home.join("log/runtime.log");
        fs::write(&log, "synthetic runtime data").unwrap();
        let report = unwire(&p, true, false).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "synthetic runtime data");
        assert!(manifest(&p).retained);
        assert!(report.warnings.iter().any(|line| line.contains("log")));
    }
    #[test]
    fn unproven_matching_file_or_pending_is_preserved_even_with_force() {
        for force in [false, true] {
            for pending in [false, true] {
                let (_dir, p) = fixture();
                run(&p, true, None).unwrap();
                let mut m = manifest(&p);
                let e = m
                    .entries
                    .iter_mut()
                    .find(|e| e.path == p.npm_dir.join("codex-qb.cmd"))
                    .unwrap();
                let bytes = fs::read(&e.path).unwrap();
                // Keep the owned inode alive while creating a byte-identical substitute.
                fs::rename(&e.path, p.npm_dir.join("owned-launcher-kept")).unwrap();
                let substitute = if pending {
                    pending_path(e)
                } else {
                    e.path.clone()
                };
                fs::write(&substitute, &bytes).unwrap();
                e.identity = None;
                e.pending_identity = None;
                e.done = false;
                assert_eq!(hash_file(&substitute).unwrap(), e.hash);
                assert!(
                    !if pending {
                        pending_matches(e, force)
                    } else {
                        matches_entry(e, force)
                    }
                    .unwrap()
                );
                atomic_json(&p.journal(), &m).unwrap();
                let result = unwire(&p, true, force);
                if pending {
                    assert!(
                        output(&result.unwrap()).contains("identity가 기록되지 않은 임시 경로")
                    );
                } else {
                    assert!(result.is_err());
                    assert!(p.journal().exists());
                }
                assert_eq!(fs::read(&substitute).unwrap(), bytes);
            }
        }
    }
    #[test]
    fn published_file_recovers_from_pending_identity_before_final_identity_save() {
        let (_dir, p) = fixture();
        run(&p, true, None).unwrap();
        let mut m = manifest(&p);
        let path = p.npm_dir.join("codex-qb.cmd");
        let e = m.entries.iter_mut().find(|e| e.path == path).unwrap();
        assert_eq!(e.pending_identity.as_ref(), Some(&identity(&path).unwrap()));
        assert!(!pending_path(e).exists()); // publication completed; final state write did not
        e.identity = None;
        e.done = false;
        assert!(matches_entry(e, false).unwrap());
        atomic_json(&p.journal(), &m).unwrap();
        run(&p, true, None).unwrap();
        let recovered = manifest(&p);
        let e = recovered.entries.iter().find(|e| e.path == path).unwrap();
        assert!(e.done);
        assert_eq!(e.identity.as_ref(), Some(&identity(&path).unwrap()));
        unwire(&p, true, false).unwrap();
        assert!(!path.exists());
        assert!(!p.journal().exists());
    }
    #[test]
    fn forced_claude_restore_keeps_bom_crlf_order_spacing_and_user_edits_byte_for_byte() {
        let (_dir, p) = fixture();
        let original = concat!(
            "{\r\n\t\"z-first\" : [1, 2],\r\n",
            "\t\"env\" : { \"KEEP\" : \"untouched\", \"ANTHROPIC_BASE_URL\" : \"old-url\", ",
            "\"ANTHROPIC_AUTH_TOKEN\" : \"old-token\" },\r\n\t\"a-last\" : {\"edited\" : false}\r\n}\r\n"
        );
        let original_bytes = [b"\xef\xbb\xbf".as_slice(), original.as_bytes()].concat();
        fs::write(&p.claude_settings, &original_bytes).unwrap();
        run(&p, true, None).unwrap();
        let installed = fs::read(&p.claude_settings).unwrap();
        let edited = String::from_utf8(installed)
            .unwrap()
            .replace("\"edited\" : false", "\"edited\" : true");
        fs::write(&p.claude_settings, edited.as_bytes()).unwrap();
        assert!(unwire(&p, true, false).is_err());
        unwire(&p, true, true).unwrap();
        let expected = String::from_utf8(original_bytes)
            .unwrap()
            .replace("\"edited\" : false", "\"edited\" : true");
        assert_eq!(fs::read(&p.claude_settings).unwrap(), expected.as_bytes());
    }
    #[test]
    fn forced_env_removal_preserves_other_member_text_and_handles_absent_owned_keys() {
        for (current, expected) in [
            (
                r#"{ "env" : { "ANTHROPIC_BASE_URL" : "qb", "KEEP" : [1, 2] }, "last":true }"#,
                r#"{ "env" : {  "KEEP" : [1, 2] }, "last":true }"#,
            ),
            (
                r#"{ "env" : { "KEEP" : [1, 2], "ANTHROPIC_BASE_URL" : "qb" }, "last":true }"#,
                r#"{ "env" : { "KEEP" : [1, 2] }, "last":true }"#,
            ),
            (
                r#"{ "env" : { "ANTHROPIC_BASE_URL" : "qb" }, "last":true }"#,
                r#"{ "env" : {  }, "last":true }"#,
            ),
        ] {
            assert_eq!(
                restore_claude_env_text(br#"{"env":{"KEEP":[1,2]}}"#, current).unwrap(),
                expected
            );
        }
    }
    #[test]
    fn shared_directory_source_junction_keeps_logical_target_and_survives_unwire() {
        let (dir, p) = fixture();
        let source = p.codex_config.parent().unwrap().join("skills");
        let target = dir.path().join("fixture-shared-skills");
        fs::create_dir(&target).unwrap();
        let marker = target.join("source-marker");
        fs::write(&marker, "skills").unwrap();
        fs::remove_file(source.join("source-marker")).unwrap();
        fs::remove_dir(&source).unwrap();
        fs::create_dir(&source).unwrap();
        junction(&source, &target).unwrap();
        let source_identity = identity(&source).unwrap();
        let source_link = fs::read_link(&source).unwrap();
        let before = snapshot(&p);
        run(&p, true, None).unwrap();
        let own = p.codex_qb_home.join("skills");
        assert!(reparse(&own).unwrap());
        let logical = fs::read_link(&own).unwrap();
        assert!(target_same(&logical, &source));
        assert!(!target_same(&logical, &target));
        let m = manifest(&p);
        assert_eq!(
            m.entries
                .iter()
                .find(|e| e.path == own)
                .unwrap()
                .target
                .as_ref(),
            Some(&source)
        );
        unwire(&p, true, false).unwrap();
        assert!(!own.exists());
        assert!(reparse(&source).unwrap());
        assert_eq!(identity(&source).unwrap(), source_identity);
        assert_eq!(fs::read_link(&source).unwrap(), source_link);
        assert_eq!(fs::read(&marker).unwrap(), b"skills");
        assert_eq!(snapshot(&p), before);
        fs::remove_dir(source).unwrap(); // unlink only the fixture's original junction
    }
    #[test]
    fn r31_dry_run_names_created_home_preserves_settings_and_exact_claude_json() {
        let (_dir, p) = fixture();
        let source = fs::read_to_string(&p.codex_config).unwrap();
        let harmless = "service_tier = 'fast'\nlog_dir = 'synthetic-log'\nmodel_reasoning_effort = 'high'\napprovals_reviewer = 'user'\nnotify = ['synthetic-notifier']\n";
        fs::write(
            &p.codex_config,
            format!(
                "{harmless}{source}\n[projects.'C:/synthetic-project/auth-demo']\ntrust_level = 'trusted'\n"
            ),
        )
        .unwrap();
        let before = snapshot(&p);
        let report =
            claude_run(&p, false, CodexMode::Dedicated, Some(ClaudeMode::Dedicated)).unwrap();
        let text = output(&report);
        assert!(report.diff.contains(&format!(
            "--- {} (원본, 바뀌지 않음)",
            p.codex_config.display()
        )));
        assert!(report.diff.contains(&format!(
            "+++ {} (새로 만듦)",
            p.codex_qb_home.join("config.toml").display()
        )));
        assert!(
            !report
                .diff
                .contains(&format!("+++ {} (연결 후)", p.codex_config.display()))
        );
        let expected = "+{\"env\":{\"ANTHROPIC_BASE_URL\":\"http://127.0.0.1:10200\",\"ANTHROPIC_AUTH_TOKEN\":\"<client key>\",\"CLAUDE_CODE_DISABLE_TERMINAL_TITLE\":\"1\"}}";
        assert!(report.diff.iter().any(|line| line == expected), "{text}");
        let preview: Value = serde_json::from_str(&expected[1..]).unwrap();
        assert_eq!(preview["env"]["ANTHROPIC_AUTH_TOKEN"], "<client key>");
        assert_eq!(
            preview["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:10200"
        );
        for line in harmless.lines() {
            assert!(text.contains(line), "hidden harmless setting: {line}");
        }
        // Unchanged project settings may fall outside the diff's context.
        let config = String::from_utf8(config_bytes(&p, 10200).unwrap()).unwrap();
        assert!(config.contains("[projects.'C:/synthetic-project/auth-demo']"));
        assert!(config.contains("trust_level = 'trusted'"));
        for secret in [KEY, SECRET] {
            assert!(!text.contains(secret));
        }
        assert_eq!(before, snapshot(&p));
        assert!(!p.journal().exists());
        assert!(!p.codex_qb_home.exists());
        assert!(!claude_home(&p).unwrap().exists());
        println!("R31_SAMPLE_BEGIN\n{text}\nR31_SAMPLE_END");
    }

    #[test]
    fn short_strings_do_not_corrupt_preview_keys_endpoint_or_redaction_markers() {
        let (_dir, p) = fixture();
        let long = format!("{SECRET}-quote\"-backslash\\-line\n한글-secret-end");
        let extra = format!(
            "short = ['a', 'e', 'd', 'v1']\npayload_secret = {}\n# {SECRET}\n",
            toml_edit::value(long.as_str())
        );
        let source = fs::read_to_string(&p.codex_config).unwrap();
        fs::write(&p.codex_config, format!("{extra}{source}")).unwrap();
        let json = serde_json::json!({"env":{"ANTHROPIC_AUTH_TOKEN":"a"},
            "nested":{"short":["a","e","d","v1"],"payload_secret":long},"unchanged":"e"});
        fs::write(
            &p.claude_settings,
            serde_json::to_vec_pretty(&json).unwrap(),
        )
        .unwrap();
        let source_before = snapshot(&p);
        let report = run(&p, false, None).unwrap();
        let text = output(&report);
        assert!(text.contains("openai_base_url"));
        assert!(text.contains("log_dir"));
        assert!(text.contains("http://127.0.0.1:10200/v1"));
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("<red<"));
        assert!(!text.contains("<redacted><redacted>"));
        assert!(!text.contains(SECRET));
        assert!(!text.contains(&long));
        let escaped = serde_json::to_string(&long).unwrap();
        assert!(!text.contains(&escaped[1..escaped.len() - 1]));
        assert!(text.len() < 32 * 1024);
        // Only short credentials are hidden. Harmless short values remain visible.
        let mut patterns = Vec::new();
        json_strings(&json, &mut patterns);
        toml_strings(parse_config(&p).unwrap().as_item(), &mut patterns);
        assert!(!patterns.iter().any(|pattern| pattern == "a"));
        assert_eq!(redact_once("\"a\"", &patterns), "\"<redacted>\"");
        assert_eq!(redact_once("'a'", &patterns), "'a'");
        for short in ["e", "d", "v1"] {
            let quoted = serde_json::to_string(short).unwrap();
            assert_eq!(redact_once(&quoted, &patterns), quoted);
        }
        assert!(!p.journal().exists());
        assert!(!p.codex_qb_home.exists());
        assert_eq!(source_before, snapshot(&p));
    }
    #[test]
    fn redaction_is_single_pass_longest_first_and_preserves_utf8_boundaries() {
        let patterns = ["secret", "secret-longer", "redacted"].map(str::to_owned);
        assert_eq!(
            redact_once("한글é secret-longer secret 😀", &patterns),
            "한글é <redacted> <redacted> 😀"
        );
        let mut collected = Vec::new();
        let multiline = "long-multiline\ncredential-한글";
        string_patterns(multiline, &mut collected);
        let quoted = serde_json::to_string(multiline).unwrap();
        let escaped = &quoted[1..quoted.len() - 1];
        assert_eq!(
            redact_once(&format!("{multiline}|{quoted}|{escaped}"), &collected),
            "<redacted>|\"<redacted>\"|<redacted>"
        );
    }
    fn resume_snapshot(p: &CutoverPaths) -> Vec<(PathBuf, String)> {
        let m = manifest(p);
        let paths = std::iter::once(p.journal())
            .chain(std::iter::once(p.claude_settings.clone()))
            .chain(
                m.entries
                    .iter()
                    .flat_map(|e| std::iter::once(e.path.clone()).chain(e.pending.clone())),
            );
        paths
            .map(|path| {
                let state = if !exists(&path).unwrap() {
                    "absent".to_owned()
                } else {
                    let content = if reparse(&path).unwrap() {
                        format!("link:{:?}", fs::read_link(&path).unwrap())
                    } else if path.is_file() {
                        hash_file(&path).unwrap()
                    } else {
                        "directory".to_owned()
                    };
                    format!("{:?}:{content}", identity(&path).unwrap())
                };
                (path, state)
            })
            .collect()
    }
    #[test]
    fn resume_key_or_port_mismatch_refuses_without_mutations_active_or_partial() {
        for mode in [CodexMode::Dedicated, CodexMode::None] {
            for partial in [false, true] {
                for rotate_key in [false, true] {
                    let (_dir, p) = fixture();
                    run(&p, true, Some(mode)).unwrap();
                    if partial {
                        let mut m = manifest(&p);
                        atomic_bytes(&p.claude_settings, &unprotect(&m.claude_original).unwrap())
                            .unwrap();
                        if mode == CodexMode::Dedicated {
                            let e = m
                                .entries
                                .iter_mut()
                                .find(|e| e.path == m.home.join("auth.json"))
                                .unwrap();
                            fs::remove_file(&e.path).unwrap();
                            e.identity = None;
                            e.pending_identity = None;
                            e.done = false;
                        }
                        atomic_json(&p.journal(), &m).unwrap();
                    }
                    let mut options = opts(Some(mode));
                    if rotate_key {
                        fs::write(&p.client_key, "synthetic-rotated-KEY-7738").unwrap();
                    } else {
                        options.data_port = 10291;
                    }
                    let before = resume_snapshot(&p);
                    let error =
                        wire_with_task_query(&p, true, options, &|_| panic!("task backend"))
                            .unwrap_err();
                    let text = format!("{error:#}\n{error:?}");
                    assert!(text.contains("기존 연결의 포트/키"));
                    assert!(text.contains("unwire"));
                    assert!(!text.contains(KEY));
                    assert!(!text.contains("synthetic-rotated-KEY-7738"));
                    assert_eq!(resume_snapshot(&p), before);
                    fs::write(&p.client_key, KEY).unwrap();
                    run(&p, true, Some(mode)).unwrap(); // unchanged saved plan still resumes
                    unwire(&p, true, false).unwrap();
                }
            }
        }
    }
    #[test]
    fn resume_checks_dedicated_payloads_as_well_as_claude_payload() {
        for config in [false, true] {
            let (_dir, p) = fixture();
            run(&p, true, None).unwrap();
            let mut m = manifest(&p);
            let name = if config { "config.toml" } else { "auth.json" };
            let path = m.home.join(name);
            let e = m.entries.iter_mut().find(|e| e.path == path).unwrap();
            let bytes = if config {
                config_bytes(&p, 10291).unwrap()
            } else {
                br#"{"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-other-key"}"#.to_vec()
            };
            e.payload = Some(protect(&bytes).unwrap());
            atomic_json(&p.journal(), &m).unwrap();
            let before = resume_snapshot(&p);
            let error = run(&p, true, None).unwrap_err();
            assert!(error.to_string().contains("기존 연결의 포트/키"));
            assert!(!format!("{error:#}").contains("synthetic-other-key"));
            assert_eq!(resume_snapshot(&p), before);
        }
    }
    #[test]
    fn retained_home_fresh_plan_accepts_new_key_and_port() {
        let (_dir, p) = fixture();
        run(&p, true, None).unwrap();
        fs::create_dir(p.codex_qb_home.join("sessions")).unwrap();
        fs::write(
            p.codex_qb_home.join("sessions/synthetic.jsonl"),
            "keep history",
        )
        .unwrap();
        unwire(&p, true, false).unwrap();
        assert!(manifest(&p).retained);
        fs::write(&p.client_key, "synthetic-fresh-key-7738").unwrap();
        let options = WireOptions {
            data_port: 10291,
            ..opts(Some(CodexMode::Dedicated))
        };
        wire_with_task_query(&p, true, options, &|_| panic!("task backend")).unwrap();
        let auth: Value =
            serde_json::from_slice(&fs::read(p.codex_qb_home.join("auth.json")).unwrap()).unwrap();
        assert_eq!(
            auth["OPENAI_API_KEY"].as_str(),
            Some("synthetic-fresh-key-7738")
        );
        let config = fs::read_to_string(p.codex_qb_home.join("config.toml"))
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            config["openai_base_url"].as_str(),
            Some("http://127.0.0.1:10291/v1")
        );
        unwire(&p, true, false).unwrap();
        assert_eq!(
            fs::read_to_string(p.codex_qb_home.join("sessions/synthetic.jsonl")).unwrap(),
            "keep history"
        );
    }
    #[test]
    fn initial_claude_bom_crlf_wire_resume_unwire_roundtrip_is_byte_exact() {
        for mode in [CodexMode::Dedicated, CodexMode::None] {
            let (_dir, p) = fixture();
            let original = b"\xef\xbb\xbf{\r\n  \"keep-first\" : true,\r\n  \"env\" : {\"ANTHROPIC_BASE_URL\":\"old-url\", \"ANTHROPIC_AUTH_TOKEN\":\"old-token\"},\r\n  \"keep-last\" : [1, 2]\r\n}\r\n";
            fs::write(&p.claude_settings, original).unwrap();
            run(&p, true, Some(mode)).unwrap();
            let m = manifest(&p);
            assert_eq!(unprotect(&m.claude_original).unwrap(), original);
            let installed = fs::read(&p.claude_settings).unwrap();
            assert!(installed.starts_with(b"\xef\xbb\xbf"));
            assert!(!installed[3..].starts_with(b"\xef\xbb\xbf"));
            assert!(installed.ends_with(b"\r\n"));
            assert_eq!(hash_bytes(&installed), m.claude_after);
            run(&p, true, Some(mode)).unwrap(); // saved BOM payload is also semantically valid
            unwire(&p, true, false).unwrap();
            assert_eq!(fs::read(&p.claude_settings).unwrap(), original);
        }
    }
    #[test]
    fn npm_appdata_fallback_is_path_only_and_none_default_do_not_require_npm() {
        let (dir, mut p) = fixture();
        let fallback = CutoverPaths::npm_dir_for(dir.path(), None);
        assert_eq!(fallback, dir.path().join("AppData/Roaming/npm"));
        let custom = dir.path().join("custom-roaming");
        assert_eq!(
            CutoverPaths::npm_dir_for(dir.path(), Some(custom.clone())),
            custom.join("npm")
        );
        p.npm_dir = fallback.clone();
        assert!(!fallback.exists());
        run(&p, false, Some(CodexMode::None)).unwrap();
        fs::write(
            &p.codex_auth,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-default-key"}"#,
        )
        .unwrap();
        run(&p, false, Some(CodexMode::Default)).unwrap();
        assert!(!fallback.exists());
        assert!(!p.journal().exists());
        assert!(run(&p, false, Some(CodexMode::Dedicated)).is_err());
    }
    #[test]
    fn new_summary_mode_labels_are_korean() {
        let (_dir, p) = fixture();
        for (mode, label) in [
            (CodexMode::Dedicated, "별도 홈"),
            (CodexMode::None, "연결 안 함"),
        ] {
            let report = run(&p, false, Some(mode)).unwrap();
            let summary = report.summary.join("\n");
            assert!(summary.contains(&format!("Codex {label}, Claude 연결 계획")));
            assert!(!summary.contains("Dedicated"));
            assert!(!summary.contains("None"));
        }
    }
    #[test]
    fn builtin_explicit_openai_selector_is_preserved_without_a_custom_override() {
        let (_dir, p) = fixture();
        for custom_cpa in [false, true] {
            let extra = if custom_cpa {
                "[model_providers.openai]\nbase_url = 'http://localhost:10100/v1'\n"
            } else {
                ""
            };
            let source = format!(
                "# selector stays explicit\nmodel_provider = 'openai' # keep selector comment\nopenai_base_url = 'http://127.0.0.1:10100/v1'\n{extra}"
            );
            fs::write(&p.codex_config, &source).unwrap();
            fs::write(&p.codex_auth, r#"{"auth_mode":"apikey"}"#).unwrap();
            assert_eq!(
                resolve_codex_mode(&p, &opts(None)).unwrap(),
                CodexMode::Default
            );
            let derived = String::from_utf8(config_bytes(&p, 10200).unwrap()).unwrap();
            let doc = derived.parse::<DocumentMut>().unwrap();
            assert_eq!(doc["model_provider"].as_str(), Some("openai"));
            assert!(derived.contains("# keep selector comment"));
            assert_eq!(
                doc["openai_base_url"].as_str(),
                Some("http://127.0.0.1:10200/v1")
            );
            assert!(
                doc.get("model_providers")
                    .and_then(|providers| providers.get("openai"))
                    .is_none()
            );
            assert_eq!(fs::read_to_string(&p.codex_config).unwrap(), source);
        }
    }
    #[test]
    fn selected_openai_override_is_checked_for_explicit_implicit_and_removed_cpa_selectors() {
        let (_dir, p) = fixture();
        fs::write(&p.codex_auth, r#"{"auth_mode":"apikey"}"#).unwrap();
        for explicit in [false, true] {
            for missing_base in [false, true] {
                let selector = if explicit {
                    "model_provider = 'openai'\n"
                } else {
                    ""
                };
                let override_value = if missing_base {
                    "name = 'custom openai'\n"
                } else {
                    "base_url = 'https://example.invalid/v1'\n"
                };
                let source = format!(
                    "{selector}openai_base_url = 'http://127.0.0.1:10100/v1'\n[model_providers.openai]\n{override_value}"
                );
                fs::write(&p.codex_config, &source).unwrap();
                assert_eq!(
                    resolve_codex_mode(&p, &opts(None)).unwrap(),
                    CodexMode::Dedicated
                );
                let error = run(&p, false, Some(CodexMode::Dedicated)).unwrap_err();
                assert!(error.to_string().contains("우회"));
                assert_eq!(fs::read_to_string(&p.codex_config).unwrap(), source);
                assert!(!p.journal().exists());
                assert!(!p.codex_qb_home.exists());
            }
        }
        fs::write(&p.codex_config, "model_provider = 'cliproxyapi'\n[model_providers.cliproxyapi]\nbase_url = 'http://127.0.0.1:10100/v1'\n[model_providers.openai]\nbase_url = 'https://example.invalid/v1'\n").unwrap();
        assert!(config_bytes(&p, 10200).is_err()); // removing CPA would select the conflicting implicit openai
        fs::write(&p.codex_config, "openai_base_url = 'https://example.invalid/v1'\n[model_providers.openai]\nbase_url = 'http://127.0.0.1:10100/v1'\n").unwrap();
        assert_eq!(
            resolve_codex_mode(&p, &opts(None)).unwrap(),
            CodexMode::Default
        );
        assert!(config_bytes(&p, 10200).is_ok()); // implicit CPA override is removed, restoring builtin openai
    }
    fn claude_options(codex: CodexMode, claude: Option<ClaudeMode>) -> WireOptions {
        WireOptions {
            codex_mode: Some(codex),
            claude_mode: claude,
            ..WireOptions::default()
        }
    }
    fn claude_run(
        p: &CutoverPaths,
        apply: bool,
        codex: CodexMode,
        claude: Option<ClaudeMode>,
    ) -> Result<Report> {
        wire_with_task_query(p, apply, claude_options(codex, claude), &|_| {
            panic!("unrequested task backend")
        })
    }
    #[test]
    fn claude_default_is_dedicated_but_old_journals_are_legacy() {
        let (_dir, p) = fixture();
        assert_eq!(ClaudeMode::default(), ClaudeMode::Dedicated);
        assert_eq!(
            resolve_claude_mode(&p, &WireOptions::default()).unwrap(),
            ClaudeMode::Dedicated
        );
        fs::create_dir_all(p.journal().parent().unwrap()).unwrap();
        for bytes in [r#"{"schema":1}"#, r#"{"schema":3,"mode":"none"}"#] {
            fs::write(p.journal(), bytes).unwrap();
            assert_eq!(
                resolve_claude_mode(&p, &WireOptions::default()).unwrap(),
                ClaudeMode::Default
            );
        }
        fs::write(p.journal(), r#"{"schema":3,"claude_mode":"none"}"#).unwrap();
        assert_eq!(
            resolve_claude_mode(&p, &WireOptions::default()).unwrap(),
            ClaudeMode::None
        );
        fs::write(
            p.journal(),
            r#"{"schema":3,"claude_mode":"private-bad-mode"}"#,
        )
        .unwrap();
        assert!(
            !resolve_claude_mode(&p, &WireOptions::default())
                .unwrap_err()
                .to_string()
                .contains("private-bad-mode")
        );
        assert_eq!(
            resolve_claude_mode(
                &p,
                &claude_options(CodexMode::None, Some(ClaudeMode::Dedicated))
            )
            .unwrap(),
            ClaudeMode::Dedicated
        );
    }
    #[test]
    fn claude_path_lookup_is_injected_ordered_and_never_executes() {
        let (dir, p) = fixture();
        let first = dir.path().join("first bin");
        fs::create_dir(&first).unwrap();
        fs::write(first.join("claude.exe"), "not executable").unwrap();
        fs::write(first.join("claude.cmd"), "not executable").unwrap();
        let path = std::env::join_paths([&first, &p.npm_dir]).unwrap();
        assert_eq!(
            resolve_claude_command(Some(&path), Some(std::ffi::OsStr::new(".CMD;.EXE")))
                .map(|p| normal_path(&p)),
            Some(
                normal_path(&first.join("claude.cmd"))
                    .trim_start_matches(r"\\?\")
                    .to_owned()
            )
        );
        assert_eq!(
            resolve_claude_command(Some(&path), Some(std::ffi::OsStr::new(".EXE;.CMD")))
                .map(|p| normal_path(&p)),
            Some(
                normal_path(&first.join("claude.exe"))
                    .trim_start_matches(r"\\?\")
                    .to_owned()
            )
        );
        assert!(resolve_claude_command(None, None).is_none());
        assert!(resolve_claude_command(Some(std::ffi::OsStr::new(".;relative")), None).is_none());
        assert!(claude_launcher(&dir.path().join("bad%name/claude.cmd")).is_err());
        assert!(claude_launcher(&dir.path().join("bad!name/claude.cmd")).is_err());
    }
    #[test]
    fn claude_dedicated_exact_payload_launcher_preview_roundtrip_and_untouched_sources() {
        let (_dir, p) = fixture();
        let before = snapshot(&p);
        let home = claude_home(&p).unwrap();
        let claude_before = fs::read(&p.claude_settings).unwrap();
        let preview = claude_run(&p, false, CodexMode::None, None).unwrap();
        assert!(!home.exists());
        assert!(!p.journal().exists());
        assert!(output(&preview).contains("--settings"));
        for secret in [KEY, SECRET] {
            assert!(!output(&preview).contains(secret));
        }
        let report = claude_run(&p, true, CodexMode::None, None).unwrap();
        assert!(!output(&report).contains(KEY));
        let expected = serde_json::json!({"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:10200","ANTHROPIC_AUTH_TOKEN":KEY,"CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"1"}});
        assert_eq!(
            fs::read(home.join("settings.json")).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        let launcher = fs::read_to_string(p.npm_dir.join("claude-qb.cmd")).unwrap();
        assert!(launcher.contains("setlocal DisableDelayedExpansion\r\n"));
        assert!(launcher.contains(&format!(
            "/s /c ^\"\"{}\" --settings \"%USERPROFILE%\\.claude-qb\\settings.json\" %*^\"\r\n",
            p.claude_command.as_ref().unwrap().display()
        )));
        assert_eq!(before, snapshot(&p));
        assert_eq!(claude_before, fs::read(&p.claude_settings).unwrap());
        assert_private_claude_acl(&home, true);
        assert_private_claude_acl(&home.join("settings.json"), false);
        claude_run(&p, true, CodexMode::None, None).unwrap();
        unwire(&p, true, false).unwrap();
        assert!(!home.exists());
        assert!(!p.npm_dir.join("claude-qb.cmd").exists());
        assert_eq!(before, snapshot(&p));
        assert_eq!(claude_before, fs::read(&p.claude_settings).unwrap());
    }
    #[test]
    fn claude_modes_are_independent_and_none_never_reads_claude_inputs() {
        for codex in [CodexMode::Default, CodexMode::Dedicated, CodexMode::None] {
            for claude in [ClaudeMode::Default, ClaudeMode::Dedicated, ClaudeMode::None] {
                let (_dir, mut p) = fixture();
                if codex == CodexMode::Default {
                    fs::write(
                        &p.codex_auth,
                        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"old"}"#,
                    )
                    .unwrap();
                }
                if claude == ClaudeMode::None {
                    fs::remove_file(&p.claude_settings).unwrap();
                    p.claude_command = None;
                }
                if codex == CodexMode::None && claude == ClaudeMode::None {
                    assert!(claude_run(&p, true, codex, Some(claude)).is_err());
                    assert!(!p.journal().exists());
                    continue;
                }
                let config = fs::read(&p.codex_config).unwrap();
                let auth = fs::read(&p.codex_auth).unwrap();
                claude_run(&p, false, codex, Some(claude)).unwrap();
                claude_run(&p, true, codex, Some(claude)).unwrap();
                if codex != CodexMode::Default || claude != ClaudeMode::Default {
                    claude_run(&p, true, codex, None).unwrap();
                }
                super::super::unwire(&p, true, false).unwrap();
                assert_eq!(fs::read(&p.codex_config).unwrap(), config);
                assert_eq!(fs::read(&p.codex_auth).unwrap(), auth);
                if claude == ClaudeMode::None {
                    assert!(!p.claude_settings.exists());
                }
            }
        }
    }
    #[test]
    fn claude_creation_and_publication_crashes_recover_with_unknown_scratch_preserved() {
        for name in ["home", "settings.json", "launcher"] {
            for stage in ["after-create", "after-publish"] {
                for resume in [false, true] {
                    let (_dir, p) = fixture();
                    let home = claude_home(&p).unwrap();
                    let path = match name {
                        "home" => home.clone(),
                        "launcher" => p.npm_dir.join("claude-qb.cmd"),
                        n => home.join(n),
                    };
                    INTERRUPT.with(|s| *s.borrow_mut() = Some((path.clone(), stage)));
                    assert!(claude_run(&p, true, CodexMode::None, None).is_err());
                    let m = manifest(&p);
                    let e = m.entries.iter().find(|e| e.path == path).unwrap();
                    let scratch = pending_path(e);
                    let marker = if stage == "after-create" {
                        let marker = if name == "home" {
                            scratch.join("unknown")
                        } else {
                            scratch.clone()
                        };
                        fs::write(&marker, "unowned").unwrap();
                        Some(marker)
                    } else {
                        None
                    };
                    if resume {
                        claude_run(&p, true, CodexMode::None, None).unwrap();
                    }
                    unwire(&p, true, false).unwrap();
                    if let Some(marker) = marker {
                        assert_eq!(fs::read_to_string(marker).unwrap(), "unowned");
                    }
                }
            }
        }
    }
    #[test]
    fn claude_unknown_paths_modes_command_changes_and_resume_key_mismatch_refuse() {
        let (_dir, p) = fixture();
        claude_run(&p, true, CodexMode::None, None).unwrap();
        assert!(claude_run(&p, true, CodexMode::None, Some(ClaudeMode::None)).is_err());
        fs::write(&p.client_key, "different-key").unwrap();
        assert!(claude_run(&p, true, CodexMode::None, None).is_err());
        fs::write(&p.client_key, KEY).unwrap();
        fs::write(p.claude_command.as_ref().unwrap(), "changed-command").unwrap();
        assert!(claude_run(&p, true, CodexMode::None, None).is_err());
        let mut m = manifest(&p);
        let original = fs::read(p.journal()).unwrap();
        m.entries[1].path = p.claude_settings.clone();
        atomic_json(&p.journal(), &m).unwrap();
        assert!(unwire(&p, true, true).is_err());
        fs::write(p.journal(), original).unwrap();
        // Removal does not depend on the continued availability of Claude.
        unwire(&p, true, false).unwrap();
    }
    #[test]
    fn claude_changed_payload_requires_force_and_replacement_identity_is_never_adopted() {
        for replace in [false, true] {
            let (_dir, p) = fixture();
            claude_run(&p, true, CodexMode::None, None).unwrap();
            let path = claude_home(&p).unwrap().join("settings.json");
            if replace {
                fs::rename(&path, path.with_extension("kept")).unwrap();
            }
            fs::write(&path, "edited").unwrap();
            assert!(unwire(&p, true, false).is_err());
            if replace {
                assert!(unwire(&p, true, true).is_err());
                assert_eq!(fs::read_to_string(path).unwrap(), "edited");
            } else {
                unwire(&p, true, true).unwrap();
                assert!(!path.exists());
            }
        }
    }
    /// Native DACL inspection only: no external command and no ambient profile.
    fn assert_private_claude_acl(path: &Path, directory: bool) {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::{
                Foundation::{CloseHandle, HANDLE},
                Security::{
                    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
                    GetFileSecurityW, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
                    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
                    TOKEN_QUERY, TOKEN_USER, TokenUser,
                },
                System::Threading::{GetCurrentProcess, OpenProcessToken},
            },
            core::{BOOL, PCWSTR},
        };
        let path = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        unsafe {
            let mut size = 0;
            let _ = GetFileSecurityW(
                PCWSTR(path.as_ptr()),
                DACL_SECURITY_INFORMATION.0,
                None,
                0,
                &mut size,
            );
            assert!(size > 0);
            let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            let descriptor = PSECURITY_DESCRIPTOR(storage.as_mut_ptr().cast());
            GetFileSecurityW(
                PCWSTR(path.as_ptr()),
                DACL_SECURITY_INFORMATION.0,
                Some(descriptor),
                size,
                &mut size,
            )
            .ok()
            .unwrap();
            let mut control = 0;
            let mut revision = 0;
            GetSecurityDescriptorControl(descriptor, &mut control, &mut revision).unwrap();
            if directory {
                assert_ne!(control & SE_DACL_PROTECTED.0, 0);
            }
            let mut present = BOOL::default();
            let mut defaulted = BOOL::default();
            let mut acl: *mut ACL = std::ptr::null_mut();
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted).unwrap();
            assert!(present.as_bool() && !acl.is_null());
            assert_eq!((*acl).AceCount, 1);
            let mut ace = std::ptr::null_mut();
            GetAce(acl, 0, &mut ace).unwrap();
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0);
            if directory {
                assert_eq!(ace.Header.AceFlags & 3, 3);
            }
            assert_eq!(ace.Mask & 0x001f01ff, 0x001f01ff);
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).unwrap();
            let mut size = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
            let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            let result = GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            );
            CloseHandle(token).unwrap();
            result.unwrap();
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            EqualSid(
                PSID((&ace.SidStart as *const u32).cast_mut().cast()),
                user.User.Sid,
            )
            .unwrap();
        }
    }

    #[test]
    fn claude_acl_failure_stops_before_any_secret_scratch_or_launcher() {
        let (_dir, p) = fixture();
        let home = claude_home(&p).unwrap();
        INTERRUPT.with(|s| *s.borrow_mut() = Some((home.clone(), "before-acl")));
        let error = claude_run(&p, true, CodexMode::None, None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Claude 전용 설정 디렉터리 접근 권한을 제한하지 못했습니다"
        );
        assert!(!home.join("settings.json").exists());
        assert!(!p.npm_dir.join("claude-qb.cmd").exists());
        assert!(fs::read_dir(&home).unwrap().next().is_none());
        claude_run(&p, true, CodexMode::None, None).unwrap();
        assert_private_claude_acl(&home, true);
        unwire(&p, true, false).unwrap();
    }
    #[test]
    fn codex_default_claude_none_replacement_publication_recovers_and_restores_sessions() {
        for resume in [false, true] {
            let (dir, mut p) = fixture();
            fs::write(
                &p.codex_auth,
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"old"}"#,
            )
            .unwrap();
            let session = dir.path().join("session-auth.json");
            fs::write(
                &session,
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"old-session","keep":1}"#,
            )
            .unwrap();
            p.codex_session_auth.push(session.clone());
            let before = snapshot(&p);
            let original_session = fs::read(&session).unwrap();
            INTERRUPT.with(|s| *s.borrow_mut() = Some((p.codex_config.clone(), "after-publish")));
            assert!(claude_run(&p, true, CodexMode::Default, Some(ClaudeMode::None)).is_err());
            if resume {
                claude_run(&p, true, CodexMode::Default, None).unwrap();
            }
            unwire(&p, true, false).unwrap();
            assert_eq!(snapshot(&p), before);
            assert_eq!(fs::read(session).unwrap(), original_session);
        }
    }
    #[test]
    fn codex_default_claude_none_force_restores_owned_values_preserving_user_edits() {
        let (_dir, p) = fixture();
        fs::write(
            &p.codex_auth,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"old"}"#,
        )
        .unwrap();
        claude_run(&p, true, CodexMode::Default, Some(ClaudeMode::None)).unwrap();
        let mut config = fs::read_to_string(&p.codex_config).unwrap();
        config.push_str("\nuser_added = true\n");
        fs::write(&p.codex_config, config).unwrap();
        assert!(unwire(&p, true, false).is_err());
        unwire(&p, true, true).unwrap();
        let text = fs::read_to_string(&p.codex_config).unwrap();
        assert!(text.contains("user_added = true"));
        assert!(!text.contains("10200"));
        assert!(text.contains(SECRET));
    }
    #[test]
    fn forced_codex_auth_restore_preserves_bom_crlf_spacing_order_and_nested_keys() {
        let (_dir, p) = fixture();
        let original = b"\xef\xbb\xbf{\r\n  \"keep\" : {\"OPENAI_API_KEY\":\"nested-old\"},\r\n  \"auth_mode\" : \"apikey\",\r\n  \"OPENAI_API_KEY\" : \"old-root\"\r\n}\r\n";
        fs::write(&p.codex_auth, original).unwrap();
        claude_run(&p, true, CodexMode::Default, Some(ClaudeMode::None)).unwrap();
        let changed = b"\xef\xbb\xbf{\r\n  \"keep\" : {\"OPENAI_API_KEY\":\"nested-user-edit\"},\r\n  \"auth_mode\" : \"apikey\",\r\n  \"OPENAI_API_KEY\" : \"changed-root\",\r\n  \"user_added\" : [ 1,  2 ]\r\n}\r\n";
        fs::write(&p.codex_auth, changed).unwrap();
        let expected = String::from_utf8(changed.to_vec())
            .unwrap()
            .replace("changed-root", "old-root")
            .into_bytes();
        unwire(&p, true, true).unwrap();
        assert!(
            fs::read(&p.codex_auth).unwrap() == expected,
            "forced auth restore changed unrelated bytes"
        );
    }
    #[test]
    fn forced_codex_auth_root_patch_handles_missing_and_nonstring_owned_members() {
        for (before, current, expected) in [
            (
                r#"{}"#,
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"added","keep" : [ 1, 2 ]}"#,
                r#"{"keep" : [ 1, 2 ]}"#,
            ),
            (
                r#"{"auth_mode":"apikey"}"#,
                r#"{"keep" : [ 1, 2 ],"OPENAI_API_KEY":"added","auth_mode":"apikey"}"#,
                r#"{"keep" : [ 1, 2 ],"auth_mode":"apikey"}"#,
            ),
            (
                r#"{}"#,
                r#"{"keep":true,"auth_mode":"apikey","OPENAI_API_KEY":"added"}"#,
                r#"{"keep":true}"#,
            ),
            (
                r#"{}"#,
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"added"}"#,
                r#"{}"#,
            ),
            (
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"before"}"#,
                " \r\n{ \"keep\" : { \"auth_mode\":\"nested\" } }\r\n",
                " \r\n{ \"keep\" : { \"auth_mode\":\"nested\" },\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"before\" }\r\n",
            ),
            (
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":null}"#,
                r#"{"auth_mode":false,"OPENAI_API_KEY":[1],"keep":"same"}"#,
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":null,"keep":"same"}"#,
            ),
        ] {
            assert!(
                restore_codex_auth_text(before.as_bytes(), current).unwrap() == expected,
                "unrelated auth text changed"
            );
        }
    }
    #[test]
    fn claude_dedicated_does_not_require_default_settings_and_unknown_home_content_survives() {
        let (_dir, p) = fixture();
        fs::remove_file(&p.claude_settings).unwrap();
        claude_run(&p, true, CodexMode::None, None).unwrap();
        let home = claude_home(&p).unwrap();
        let unknown = home.join("unknown");
        fs::write(&unknown, "keep").unwrap();
        unwire(&p, true, false).unwrap();
        assert_eq!(fs::read_to_string(&unknown).unwrap(), "keep");
        claude_run(&p, true, CodexMode::None, None).unwrap();
        unwire(&p, true, false).unwrap();
        assert_eq!(fs::read_to_string(unknown).unwrap(), "keep");
        assert!(!p.claude_settings.exists());
    }

    // R3.2 candidate tests: SYNTHETIC, UNRUN. Merge the helper/launcher worker
    // first. Tests call only fixture paths; they never launch the wrappers.
    fn r32_old_install(p: &CutoverPaths) {
        claude_run(p, true, CodexMode::Dedicated, Some(ClaudeMode::Dedicated)).unwrap();
        let config = p.codex_qb_home.join("config.toml");
        let mut doc = fs::read_to_string(&config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        if let Some(tui) = doc
            .get_mut("tui")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            tui.remove("terminal_title");
        }
        // These extras existed in the owned R3 file, not in its default source.
        doc["r32_extra"] = toml_edit::value("current-dedicated-only");
        fs::write(&config, doc.to_string()).unwrap();
        let settings = claude_home(p).unwrap().join("settings.json");
        let mut json: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        json["env"]
            .as_object_mut()
            .unwrap()
            .remove("CLAUDE_CODE_DISABLE_TERMINAL_TITLE");
        json["r32_extra"] = serde_json::json!({"current": true});
        fs::write(&settings, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
        fs::write(
            p.npm_dir.join("codex-qb.cmd"),
            b"@echo off\r\nrem synthetic old R3 Codex wrapper\r\n",
        )
        .unwrap();
        fs::write(
            p.npm_dir.join("claude-qb.cmd"),
            b"@echo off\r\nrem synthetic old R3 Claude wrapper\r\n",
        )
        .unwrap();
        let mut m = manifest(p);
        for e in &mut m.entries {
            if e.kind == Kind::File {
                let bytes = fs::read(&e.path).unwrap();
                e.hash = hash_bytes(&bytes);
                e.payload = Some(protect(&bytes).unwrap());
                e.identity = Some(identity(&e.path).unwrap());
            }
        }
        // Emit the old schema-3 shape, exercising serde defaults explicitly.
        let mut old = serde_json::to_value(m).unwrap();
        old.as_object_mut().unwrap().remove("install_completed");
        for e in old["entries"].as_array_mut().unwrap() {
            e.as_object_mut().unwrap().remove("upgrade");
        }
        atomic_json(&p.journal(), &old).unwrap();
    }

    fn r32_run(p: &CutoverPaths, apply: bool, force: bool) -> Result<Report> {
        let mut options = claude_options(CodexMode::Dedicated, Some(ClaudeMode::Dedicated));
        options.force = force;
        wire_with_task_query(p, apply, options, &|_| panic!("upgrade queried tasks"))
    }

    fn r32_owned(p: &CutoverPaths) -> Vec<(PathBuf, Vec<u8>, Identity)> {
        let m = manifest(p);
        m.entries
            .iter()
            .filter(|e| upgrade_path(&m, &e.path).unwrap())
            .map(|e| {
                (
                    e.path.clone(),
                    fs::read(&e.path).unwrap(),
                    identity(&e.path).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn r32_upgrade_old_r3_preserves_current_extras_and_is_idempotent() {
        let (_dir, p) = fixture();
        r32_old_install(&p);
        let sources = snapshot(&p);
        let auth = fs::read(p.codex_qb_home.join("auth.json")).unwrap();
        let auth_id = identity(&p.codex_qb_home.join("auth.json")).unwrap();
        let before = manifest(&p);
        // The existing key is validated, but default sources are not regenerated.
        fs::write(&p.codex_config, "invalid default-source TOML [").unwrap();
        let report = r32_run(&p, true, false).unwrap();
        assert_eq!(report.changed.len(), 4);
        let m = manifest(&p);
        for e in &m.entries {
            let prior = before
                .entries
                .iter()
                .find(|old| old.path == e.path)
                .unwrap();
            assert_eq!(
                serde_json::to_value(&e.original).unwrap(),
                serde_json::to_value(&prior.original).unwrap()
            );
            if !upgrade_path(&m, &e.path).unwrap() {
                assert_eq!(
                    serde_json::to_value(e).unwrap(),
                    serde_json::to_value(prior).unwrap()
                );
            }
        }
        let config = fs::read_to_string(p.codex_qb_home.join("config.toml")).unwrap();
        let doc = config.parse::<DocumentMut>().unwrap();
        assert!(doc["tui"]["terminal_title"].as_array().unwrap().is_empty());
        assert_eq!(doc["r32_extra"].as_str(), Some("current-dedicated-only"));
        let json: Value = serde_json::from_slice(
            &fs::read(claude_home(&p).unwrap().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(json["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"], "1");
        assert_eq!(json["r32_extra"]["current"], true);
        assert_eq!(fs::read(p.codex_qb_home.join("auth.json")).unwrap(), auth);
        assert_eq!(
            identity(&p.codex_qb_home.join("auth.json")).unwrap(),
            auth_id
        );
        let owned = r32_owned(&p);
        let journal = fs::read(p.journal()).unwrap();
        assert!(r32_run(&p, true, false).unwrap().changed.is_empty());
        assert_eq!(r32_owned(&p), owned);
        assert_eq!(fs::read(p.journal()).unwrap(), journal);
        // Restore synthetic source only so its unchanged companions can be compared.
        fs::write(&p.codex_config, &sources[0]).unwrap();
        assert_eq!(snapshot(&p), sources);
        super::unwire(&p, true, false).unwrap();
        assert!(!p.journal().exists());
        assert!(!p.npm_dir.join("codex-qb.cmd").exists());
        assert!(!p.npm_dir.join("claude-qb.cmd").exists());
    }

    #[test]
    fn r32_upgrade_dry_run_diff_has_no_mutation_or_scratch() {
        let (_dir, p) = fixture();
        r32_old_install(&p);
        let before = r32_owned(&p);
        let journal = fs::read(p.journal()).unwrap();
        let siblings = |dir: &Path| {
            let mut paths = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect::<Vec<_>>();
            paths.sort();
            paths
        };
        let codex_siblings = siblings(&p.codex_qb_home);
        let claude_siblings = siblings(&claude_home(&p).unwrap());
        let npm_siblings = siblings(&p.npm_dir);
        let report = r32_run(&p, false, false).unwrap();
        assert_eq!(report.changed.len(), 4);
        let diff = report.diff.join("\n");
        assert!(diff.contains("terminal_title"));
        assert!(diff.contains("CLAUDE_CODE_DISABLE_TERMINAL_TITLE"));
        assert!(!diff.contains(KEY));
        assert_eq!(r32_owned(&p), before);
        assert_eq!(fs::read(p.journal()).unwrap(), journal);
        assert_eq!(siblings(&p.codex_qb_home), codex_siblings);
        assert_eq!(siblings(&claude_home(&p).unwrap()), claude_siblings);
        assert_eq!(siblings(&p.npm_dir), npm_siblings);
    }

    #[test]
    fn r32_upgrade_content_refuses_then_force_merges_current_owned_file() {
        let (_dir, p) = fixture();
        r32_old_install(&p);
        let config = p.codex_qb_home.join("config.toml");
        let before_id = identity(&config).unwrap();
        let mut text = fs::read_to_string(&config).unwrap();
        text.insert_str(0, "user_extra = 'keep-force-edit'\n");
        fs::write(&config, text).unwrap();
        assert_eq!(identity(&config).unwrap(), before_id);
        let journal = fs::read(p.journal()).unwrap();
        assert!(
            r32_run(&p, true, false)
                .unwrap_err()
                .to_string()
                .contains("--force")
        );
        assert_eq!(fs::read(p.journal()).unwrap(), journal);
        r32_run(&p, true, true).unwrap();
        assert!(
            fs::read_to_string(config)
                .unwrap()
                .contains("keep-force-edit")
        );
        super::unwire(&p, true, false).unwrap();
    }

    #[test]
    fn r32_upgrade_force_refuses_same_bytes_substitute() {
        let (_dir, p) = fixture();
        r32_old_install(&p);
        let path = p.codex_qb_home.join("config.toml");
        let bytes = fs::read(&path).unwrap();
        // Keep the old object alive, avoiding file-ID reuse in this test.
        fs::rename(&path, path.with_extension("held-old")).unwrap();
        fs::write(&path, &bytes).unwrap();
        let journal = fs::read(p.journal()).unwrap();
        assert!(r32_run(&p, true, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read(p.journal()).unwrap(), journal);
    }

    #[test]
    fn r32_upgrade_interruptions_resume_and_unwire_exact_owned_objects() {
        for stage in [
            "upgrade-before-scratch",
            "after-create",
            "upgrade-after-identity",
            "upgrade-after-write",
            "upgrade-after-replace",
            "upgrade-before-final-journal",
        ] {
            for reverse in [false, true] {
                let (_dir, p) = fixture();
                r32_old_install(&p);
                let path = p.codex_qb_home.join("config.toml");
                let unrelated = p.codex_qb_home.join("unrelated.txt");
                fs::write(&unrelated, "preserve unknown data").unwrap();
                INTERRUPT.with(|slot| *slot.borrow_mut() = Some((path.clone(), stage)));
                assert!(
                    r32_run(&p, true, false)
                        .unwrap_err()
                        .to_string()
                        .contains("synthetic creation interruption")
                );
                let m = manifest(&p);
                let e = m.entries.iter().find(|e| e.path == path).unwrap();
                assert!(e.upgrade.is_some());
                let published = identity(&path).unwrap();
                if reverse {
                    super::unwire(&p, true, false).unwrap();
                    assert!(!path.exists());
                } else {
                    r32_run(&p, true, false).unwrap();
                    if stage == "upgrade-after-replace" || stage == "upgrade-before-final-journal" {
                        assert_eq!(identity(&path).unwrap(), published);
                    }
                    assert!(manifest(&p).entries.iter().all(|e| e.upgrade.is_none()));
                    super::unwire(&p, true, false).unwrap();
                }
                assert_eq!(
                    fs::read_to_string(&unrelated).unwrap(),
                    "preserve unknown data"
                );
            }
        }
    }

    #[test]
    fn r32_upgrade_unwire_refuses_substituted_new_object_even_with_force() {
        let (_dir, p) = fixture();
        r32_old_install(&p);
        let path = p.codex_qb_home.join("config.toml");
        INTERRUPT.with(|slot| *slot.borrow_mut() = Some((path.clone(), "upgrade-after-replace")));
        assert!(r32_run(&p, true, false).is_err());
        let bytes = fs::read(&path).unwrap();
        fs::rename(&path, path.with_extension("held-staged-new")).unwrap();
        fs::write(&path, &bytes).unwrap();
        let journal = fs::read(p.journal()).unwrap();
        assert!(super::unwire(&p, true, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read(p.journal()).unwrap(), journal);
        assert!(p.npm_dir.join("claude-qb.cmd").exists());
    }

    #[test]
    fn r32_upgrade_mixed_modes_keep_real_original_backup_anchor() {
        let (_dir, p) = fixture();
        fs::write(
            &p.codex_auth,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-old"}"#,
        )
        .unwrap();
        let original_sources = snapshot(&p);
        claude_run(&p, true, CodexMode::Default, Some(ClaudeMode::Dedicated)).unwrap();
        let launcher_path = p.npm_dir.join("claude-qb.cmd");
        fs::write(&launcher_path, "@echo off\r\nrem old R3 synthetic\r\n").unwrap();
        let mut m = manifest(&p);
        let e = m
            .entries
            .iter_mut()
            .find(|e| e.path == launcher_path)
            .unwrap();
        e.hash = hash_file(&launcher_path).unwrap();
        e.payload = Some(protect(&fs::read(&launcher_path).unwrap()).unwrap());
        atomic_json(&p.journal(), &m).unwrap();
        let anchors = m
            .entries
            .iter()
            .filter(|e| e.original.is_some())
            .map(|e| (e.path.clone(), serde_json::to_value(e).unwrap()))
            .collect::<Vec<_>>();
        assert!(!anchors.is_empty());
        claude_run(&p, true, CodexMode::Default, Some(ClaudeMode::Dedicated)).unwrap();
        let upgraded = manifest(&p);
        for (path, value) in anchors {
            assert_eq!(
                serde_json::to_value(upgraded.entries.iter().find(|e| e.path == path).unwrap())
                    .unwrap(),
                value
            );
        }
        super::unwire(&p, true, false).unwrap();
        assert_eq!(snapshot(&p), original_sources);
    }

    #[test]
    fn r32_upgrade_leaves_already_current_config_and_settings_identity_unchanged() {
        let (_dir, p) = fixture();
        r32_old_install(&p);
        r32_run(&p, true, false).unwrap();
        let prior = r32_owned(&p);
        let path = p.npm_dir.join("codex-qb.cmd");
        fs::write(&path, "@echo off\r\nrem old synthetic launcher\r\n").unwrap();
        let mut m = manifest(&p);
        let e = m.entries.iter_mut().find(|e| e.path == path).unwrap();
        e.hash = hash_file(&path).unwrap();
        e.payload = Some(protect(&fs::read(&path).unwrap()).unwrap());
        atomic_json(&p.journal(), &m).unwrap();
        assert_eq!(
            r32_run(&p, true, false).unwrap().changed,
            vec![path.display().to_string()]
        );
        for (name, bytes, id) in prior {
            if name != path {
                assert_eq!(fs::read(&name).unwrap(), bytes);
                assert_eq!(identity(&name).unwrap(), id);
            }
        }
    }

    #[test]
    fn r32_upgrade_legacy_partial_install_keeps_resume_path() {
        let (_dir, p) = fixture();
        let path = p.codex_qb_home.join("config.toml");
        INTERRUPT.with(|slot| *slot.borrow_mut() = Some((path.clone(), "after-publish")));
        assert!(claude_run(&p, true, CodexMode::Dedicated, Some(ClaudeMode::Dedicated)).is_err());
        let mut old = serde_json::to_value(manifest(&p)).unwrap();
        old.as_object_mut().unwrap().remove("install_completed");
        atomic_json(&p.journal(), &old).unwrap();
        assert!(!completed_install(&manifest(&p)).unwrap());
        r32_run(&p, true, false).unwrap();
        assert_eq!(manifest(&p).install_completed, Some(true));
        assert!(
            manifest(&p)
                .entries
                .iter()
                .all(|e| e.done && e.upgrade.is_none())
        );
        super::unwire(&p, true, false).unwrap();
    }

    #[test]
    fn r32_upgrade_rejects_missing_key_and_saved_payload_hash_changes_before_mutation() {
        for target in [
            "missing-key",
            "config.toml",
            "settings.json",
            "codex-qb.cmd",
        ] {
            let (_dir, p) = fixture();
            r32_old_install(&p);
            if target == "missing-key" {
                fs::remove_file(&p.client_key).unwrap();
            } else {
                let mut m = manifest(&p);
                let e = m
                    .entries
                    .iter_mut()
                    .find(|e| e.path.file_name().is_some_and(|name| name == target))
                    .unwrap();
                let mut bytes = unprotect(e.payload.as_deref().unwrap()).unwrap();
                // Keep parsed key/port values unchanged: the digest must reject.
                bytes.push(b'\n');
                e.payload = Some(protect(&bytes).unwrap());
                atomic_json(&p.journal(), &m).unwrap();
            }
            let before = resume_snapshot(&p);
            assert!(r32_run(&p, true, true).is_err());
            assert_eq!(resume_snapshot(&p), before);
        }
    }
}
