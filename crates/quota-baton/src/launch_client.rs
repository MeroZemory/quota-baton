//! R11 managed Codex issuance, catalog acquisition and contained-launch ownership.
//!
//! Grammar: interactive, `resume [ID]`, `exec|e [resume [ID]]`; known native
//! option operands and everything after `--` are opaque. QB flags are accepted
//! before the first prompt, including after a resume ID. Unknown option arity,
//! profiles, OSS and caller routing overrides are refused before issuance.
//! Model precedence is explicit -m, one -c model assignment, then home model.
//! Value-option arities are listed in `takes_value`; short clusters other than
//! a single valued short option are unsupported. Profiles are never merged.
//! Limits: 4096 arguments, 256-byte model/account text, 1-MiB home config,
//! 16-KiB per config override/admin reply, 1-MiB catalog, ten seconds per request.
//! No persistent configuration is written. All content-bearing Debug is redacted.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, fmt, fs::File, io::Read, path::Path, time::Duration};
use toml_edit::DocumentMut;

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_REPLY_BYTES: usize = 16 * 1024;
const MAX_ARGS: usize = 4096;
const MAX_TEXT_BYTES: usize = 256;
const DEADLINE: Duration = Duration::from_secs(10);

const GRAMMAR: &str = "실행 옵션을 안전하게 해석할 수 없습니다. QB 옵션은 프롬프트 앞에 두고, 프로필·사용자 라우팅 설정 없이 실행해 주세요.";
const HOME: &str = "전용 Codex 홈의 모델 설정을 확인하지 못했습니다.";
const CORE: &str = "실행 계정을 요청하지 못했습니다. quota-baton 코어와 로컬 인증을 확인해 주세요.";
const REPLY: &str = "코어의 실행 응답이 올바르지 않습니다. 코어 버전을 확인해 주세요.";

// Never retain or wrap filesystem/parser/HTTP errors: their source chains may
// contain credentials, response content or token-bearing URLs.
struct LaunchError(&'static str);

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Debug for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for LaunchError {}

fn refusal(message: &'static str) -> anyhow::Error {
    LaunchError(message).into()
}

pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<LaunchError>()
        .map(ToString::to_string)
        .or_else(|| crate::resume_selection::friendly_error(error))
}

/// Ordinary managed Codex only. The caller handles the separate R12 images path.
/// All owned resources are released before returning to main's process::exit.
pub(crate) async fn run_codex(
    program: &Path,
    settings: Option<&Path>,
    args: &[OsString],
) -> Result<i32> {
    if settings.is_some() {
        return Err(refusal(GRAMMAR));
    }
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let mut plan = parse(args, &codex_home)?;
    if plan.passthrough {
        // Native informational commands need no qb home, config, key or core.
        return crate::launcher_label::run(
            crate::launcher_label::Client::Codex,
            program,
            None,
            &plan.native_args,
        )
        .map_err(|_| refusal(NATIVE));
    }
    let home = qb_core::home().map_err(|_| refusal(CORE))?;
    if let Some(request) = plan.selection.take() {
        // Validate the selected CODEX_HOME before reading its session metadata.
        // This is read-only and runs no native version command.
        crate::sessions_admission::preflight_launch(&plan.native_args, program)?;
        let Some(selected) = crate::resume_selection::select(&codex_home, request)? else {
            return Ok(0);
        };
        plan.native_args = selected;
        plan.resume_id = crate::sessions::resume_uuid(&plan.native_args)?;
    }
    // Strip QB flags before every existing R9 resume parser/admission/import.
    let id = crate::sessions::resume_uuid(&plan.native_args)?;
    let progress = crate::sessions::prepare_resume(&home, id.as_deref(), None)?;
    crate::sessions::admission_check(&home, id.as_deref())?;
    crate::sessions_admission::preflight_launch(&plan.native_args, program)?;
    crate::sessions::before_codex_resume(&plan.native_args)?;
    // Signal interception belongs to the move, never to the native child.
    drop(progress);
    let (permit, native_args) =
        crate::sessions_admission::prepare_launch(&plan.native_args, program)?;
    plan.native_args = native_args;
    let outcome = async {
        crate::sessions::admission_check(&home, id.as_deref())?;
        let config = qb_core::config::Config::load(&home).map_err(|_| refusal(CORE))?;
        let issued = issue_launch(&plan, &home, &config).await?;
        if issued.effective_model().is_none() {
            return Err(refusal(REPLY));
        }
        let FetchedCatalog { bytes, envelope } = issued.fetch_catalog().await?;
        // Envelope checking is transport validation only. Store exact wire bytes;
        // pinned-native schema acceptance remains a separate integration gate.
        drop(envelope);
        let snapshot = crate::catalog_snapshot::CatalogSnapshot::create(&home, &bytes)?;
        run_issued(program, &issued, &config, snapshot).await
    }
    .await;
    drop(permit);
    outcome
}

async fn run_issued(
    program: &Path,
    issued: &IssuedLaunch,
    config: &qb_core::config::Config,
    snapshot: crate::catalog_snapshot::CatalogSnapshot,
) -> Result<i32> {
    let status = async {
        let args = issued.native_args(snapshot.path())?;
        #[cfg(windows)]
        if let Some(status) = crate::native_resume::try_run(
            program,
            &args,
            &issued.base_url,
            &issued.client_key,
            account_short(config, issued.account()),
        )
        .await
        .map_err(|_| refusal(NATIVE))?
        {
            return Ok(status);
        }
        crate::launcher_label::run_with_account(
            program,
            &args,
            account_short(config, issued.account()),
        )
        .map_err(|_| refusal(NATIVE))
    }
    .await;
    // The runner returns after the contained process TREE has finished, even
    // on errors. Explicit release also covers argument construction failure.
    drop(snapshot);
    status
}

const NATIVE: &str = "Codex 실행을 완료하지 못했습니다. 실행 파일과 전용 홈을 확인해 주세요.";

fn account_short<'a>(config: &'a qb_core::config::Config, actual_account: &str) -> Option<&'a str> {
    config
        .accounts
        .get(actual_account)
        .and_then(|account| account.short.as_deref())
}

pub(crate) struct LaunchPlan {
    pub native_args: Vec<OsString>,
    pub effective_model: Option<String>,
    pub account_pref: Option<String>,
    pub move_for_model: bool,
    pub urgent: bool,
    pub resume_id: Option<String>,
    /// Help/version/completion: do not perform admission, account or core I/O.
    pub passthrough: bool,
    /// Explicit CLI model (-m/--model or -c model), rather than a home default.
    pub model_is_explicit: bool,
    selection: Option<crate::resume_selection::Request>,
}

impl fmt::Debug for LaunchPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchPlan([REDACTED])")
    }
}

struct Inspected {
    plan: LaunchPlan,
    model_override: Option<String>,
    unsupported: bool,
}

/// Only reads qb_codex_home/config.toml, after inspection identifies a launch.
/// run_codex validates dedicated wiring and holds R9 admission around issuance/run.
pub(crate) fn parse(args: &[OsString], qb_codex_home: &Path) -> Result<LaunchPlan> {
    let inspected = inspect(args)?;
    if inspected.plan.passthrough {
        return Ok(inspected.plan);
    }
    if inspected.unsupported {
        return Err(refusal(GRAMMAR));
    }
    let mut bytes = Vec::new();
    File::open(qb_codex_home.join("config.toml"))
        .map_err(|_| refusal(HOME))?
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| refusal(HOME))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(refusal(HOME));
    }
    let text = std::str::from_utf8(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes))
        .map_err(|_| refusal(HOME))?;
    resolve(inspected, text)
}

fn text_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TEXT_BYTES
        && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn account_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TEXT_BYTES
        && !value.starts_with('-')
        && !value.chars().any(char::is_control)
}

// This deliberately enumerates arities rather than filtering argv by spelling.
pub(crate) fn takes_value(name: &str) -> bool {
    matches!(
        name,
        "--config"
            | "-c"
            | "--model"
            | "-m"
            | "--profile"
            | "-p"
            | "--sandbox"
            | "-s"
            | "--cd"
            | "-C"
            | "--ask-for-approval"
            | "-a"
            | "--image"
            | "-i"
            | "--enable"
            | "--disable"
            | "--add-dir"
            | "--remote"
            | "--remote-auth-token-env"
            | "--local-provider"
            | "--output-last-message"
            | "-o"
            | "--output-schema"
            | "--color"
    )
}

pub(crate) fn switch(name: &str) -> bool {
    matches!(
        name,
        "--all"
            | "--last"
            | "--include-non-interactive"
            | "--search"
            | "--no-alt-screen"
            | "--no-daemon"
            | "--strict-config"
            | "--approve-for-me"
            | "--dangerously-bypass-approvals-and-sandbox"
            | "--dangerously-bypass-hook-trust"
            | "--skip-git-repo-check"
            | "--json"
            | "--ephemeral"
            | "--full-auto"
    )
}

fn inspect(args: &[OsString]) -> Result<Inspected> {
    if args.len() > MAX_ARGS {
        return Err(refusal(GRAMMAR));
    }
    let mut result = Inspected {
        plan: LaunchPlan {
            native_args: Vec::new(),
            effective_model: None,
            account_pref: None,
            move_for_model: false,
            urgent: false,
            resume_id: None,
            passthrough: false,
            model_is_explicit: false,
            selection: None,
        },
        model_override: None,
        unsupported: false,
    };
    let mut index = 0;
    let mut exec = false;
    let mut resume = false;
    let mut resume_positional = false;
    let mut prompt = false;
    while index < args.len() {
        let start = index;
        index += 1;
        let Some(arg) = args[start].to_str() else {
            // Non-Unicode positional prompts are opaque. Option operands are
            // consumed below without attempting Unicode conversion.
            if args[start].to_string_lossy().starts_with('-') {
                return Err(refusal(GRAMMAR));
            }
            prompt = true;
            result.plan.native_args.push(args[start].clone());
            continue;
        };
        if arg == "--" {
            result.plan.native_args.extend_from_slice(&args[start..]);
            break;
        }
        if matches!(arg, "--help" | "-h" | "--version" | "-V") {
            result.plan.native_args.extend_from_slice(&args[start..]);
            result.plan.passthrough = true;
            return Ok(result);
        }
        if prompt && arg == "--urgent" {
            result.plan.native_args.push(args[start].clone());
            continue;
        }
        if !prompt && (arg == "--account" || arg.starts_with("--account=")) {
            if result.plan.account_pref.is_some() {
                return Err(refusal(GRAMMAR));
            }
            let value = if let Some(value) = arg.strip_prefix("--account=") {
                value
            } else {
                let value = args
                    .get(index)
                    .and_then(|v| v.to_str())
                    .ok_or_else(|| refusal(GRAMMAR))?;
                index += 1;
                value
            };
            if !account_name(value) {
                return Err(refusal(GRAMMAR));
            }
            result.plan.account_pref = Some(value.to_owned());
            continue;
        }
        if !prompt && arg == "--urgent" {
            if result.plan.urgent {
                return Err(refusal(GRAMMAR));
            }
            result.plan.urgent = true;
            continue;
        }
        if !prompt && arg == "--move-for-model" {
            if result.plan.move_for_model {
                return Err(refusal(GRAMMAR));
            }
            result.plan.move_for_model = true;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            let (mut name, mut attached) = arg
                .split_once('=')
                .map_or((arg, None), |(n, v)| (n, Some(v)));
            if !arg.starts_with("--") && arg.len() > 2 {
                for short in ["-c", "-m", "-p", "-s", "-C", "-a", "-i", "-o"] {
                    if let Some(value) = arg.strip_prefix(short) {
                        name = short;
                        attached = Some(value.strip_prefix('=').unwrap_or(value));
                        break;
                    }
                }
            }
            if takes_value(name) {
                let value = if let Some(value) = attached {
                    Some(value)
                } else {
                    let value = args.get(index).ok_or_else(|| refusal(GRAMMAR))?;
                    if value == "--" {
                        return Err(refusal(GRAMMAR));
                    }
                    index += 1;
                    value.to_str()
                };
                match name {
                    "-m" | "--model" => {
                        let value = value
                            .filter(|v| text_id(v))
                            .ok_or_else(|| refusal(GRAMMAR))?;
                        if result.plan.model_is_explicit {
                            return Err(refusal(GRAMMAR));
                        }
                        result.plan.model_is_explicit = true;
                        result.plan.effective_model = Some(value.to_owned());
                    }
                    "-c" | "--config" => {
                        inspect_override(&mut result, value.ok_or_else(|| refusal(GRAMMAR))?)?
                    }
                    "-p" | "--profile" | "--local-provider" | "--remote" => {
                        result.unsupported = true
                    }
                    _ => {}
                }
            } else if name == "--oss" && attached.is_none() {
                result.unsupported = true;
            } else if !switch(name) || attached.is_some() {
                // No guessing which subsequent argument might be an operand.
                return Err(refusal(GRAMMAR));
            }
        } else if !prompt {
            match arg {
                "help" | "completion" if !exec && !resume => {
                    result.plan.native_args.extend_from_slice(&args[start..]);
                    result.plan.passthrough = true;
                    return Ok(result);
                }
                "exec" | "e" if !exec && !resume => exec = true,
                "resume" if !resume => resume = true,
                _ if resume && !resume_positional => resume_positional = true,
                "fork" | "review" | "login" | "logout" | "mcp" | "mcp-server" | "app-server"
                | "daemon" | "debug" | "features" | "cloud" | "apply" | "a" | "sandbox" | "s"
                    if !exec && !resume =>
                {
                    result.unsupported = true
                }
                _ => prompt = true,
            }
        }
        result
            .plan
            .native_args
            .extend_from_slice(&args[start..index]);
    }
    result.plan.resume_id =
        crate::sessions::resume_uuid(&result.plan.native_args).map_err(|_| refusal(GRAMMAR))?;
    Ok(result)
}

fn inspect_override(result: &mut Inspected, value: &str) -> Result<()> {
    if value.len() > MAX_REPLY_BYTES {
        return Err(refusal(GRAMMAR));
    }
    let (key, value) = value.split_once('=').ok_or_else(|| refusal(GRAMMAR))?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err(refusal(GRAMMAR));
    }
    let root = key.split('.').next().unwrap_or("");
    if matches!(
        root,
        "profile"
            | "default_profile"
            | "profiles"
            | "model_provider"
            | "model_providers"
            | "openai_base_url"
            | "model_catalog_url"
            | "model_catalog_json"
    ) {
        result.unsupported = true;
    }
    if root == "model" {
        if key != "model" || result.model_override.is_some() {
            return Err(refusal(GRAMMAR));
        }
        let value = value.trim();
        let model = match format!("v = {value}").parse::<DocumentMut>() {
            Ok(doc) if doc.len() == 1 => doc.get("v").and_then(|v| v.as_str()).map(str::to_owned),
            // Native CLI supports bare string values. Only an unambiguous ID
            // subset is accepted here; other TOML/override grammar is refused.
            Err(_)
                if value.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/')
                }) =>
            {
                Some(value.to_owned())
            }
            _ => None,
        }
        .filter(|v| text_id(v))
        .ok_or_else(|| refusal(GRAMMAR))?;
        result.model_override = Some(model);
    }
    Ok(())
}

fn resolve(mut inspected: Inspected, text: &str) -> Result<LaunchPlan> {
    if inspected.unsupported {
        return Err(refusal(GRAMMAR));
    }
    let doc = text.parse::<DocumentMut>().map_err(|_| refusal(HOME))?;
    if ["profile", "default_profile", "model_catalog_json"]
        .iter()
        .any(|key| doc.contains_key(key))
        || doc
            .get("model_provider")
            .is_some_and(|v| v.as_str() != Some("openai"))
        || doc
            .get("model_providers")
            .and_then(|v| v.get("openai"))
            .is_some()
    {
        return Err(refusal(GRAMMAR));
    }
    let root_model = doc.get("model").and_then(|v| v.as_str());
    inspected.plan.model_is_explicit |= inspected.model_override.is_some();
    let model = inspected
        .plan
        .effective_model
        .take()
        .or(inspected.model_override)
        .or_else(|| root_model.map(str::to_owned))
        .filter(|v| text_id(v))
        .ok_or_else(|| refusal(HOME))?;
    inspected.plan.effective_model = Some(model);
    inspected.plan.selection = crate::resume_selection::request(&inspected.plan.native_args, &doc)?;
    Ok(inspected.plan)
}

#[derive(Serialize)]
struct LaunchRequest<'a> {
    model: &'a str,
    model_is_explicit: bool,
    account_pref: Option<&'a str>,
    resume_id: Option<&'a str>,
    move_for_model: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    urgent: bool,
}

#[derive(Deserialize)]
struct LaunchReply {
    base_url: String,
    catalog_url: String,
    account: String,
    model: Option<String>,
}

/// Authenticated, validated issuance. No Debug/Display: routing URLs are secrets.
/// Caller retains admission and owns snapshot validation and child lifetime.
pub(crate) struct IssuedLaunch {
    base_url: String,
    catalog_url: String,
    account: String,
    effective_model: Option<String>,
    native_args: Vec<OsString>,
    model_is_explicit: bool,
    client: reqwest::Client,
    client_key: String,
}

/// Transport/envelope validation only, never native schema validation.
/// Raw bytes preserve unknown fields, formatting and all native capabilities.
pub(crate) struct FetchedCatalog {
    pub bytes: Vec<u8>,
    pub envelope: serde_json::Value,
}

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const CATALOG: &str =
    "모델 목록을 안전하게 가져오지 못했습니다. 코어를 확인한 뒤 다시 실행해 주세요.";
const ACCOUNT_GUARD: &str =
    "실행 계정과 모델 목록의 계정이 달라졌습니다. codex-qb를 다시 실행해 주세요.";
const SNAPSHOT_PATH: &str =
    "검증된 모델 목록 파일의 절대 경로를 전달해 주세요. 경로를 손실 없이 변환할 수 없습니다.";

fn local_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        // Preserve wire JSON; refuse encoded bodies rather than implicitly decode.
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .timeout(DEADLINE)
        .build()
        .map_err(|_| refusal(CORE))
}

async fn bounded_body(
    mut response: reqwest::Response,
    max: usize,
    message: &'static str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(refusal(message));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| refusal(message))? {
        if chunk.len() > max - bytes.len() {
            return Err(refusal(message));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn admin_failure(bytes: &[u8]) -> &'static str {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return CORE;
    };
    match value.pointer("/error/code").and_then(|v| v.as_str()) {
        Some("invalid_launch") => "실행 옵션이 올바르지 않습니다.",
        Some("launch_unavailable") => {
            "모델 목록을 확인한 사용 가능 계정이 없습니다. 잠시 후 다시 실행해 주세요."
        }
        Some("model_move_consent_required") => {
            "기존 세션의 계정에서 요청한 모델을 사용할 수 없습니다. 다른 계정으로 옮기려면 --account 또는 --move-for-model을 지정해 주세요."
        }
        Some("model_not_supported") => {
            "이 계정은 요청한 모델을 지원하지 않습니다. 지원하는 모델을 선택해 주세요."
        }
        Some("launch_capacity") => {
            "실행 연결을 더 만들 수 없습니다. 사용하지 않는 실행을 종료한 뒤 다시 시도해 주세요."
        }
        Some("launch_store_error") => {
            "계정 연결을 안전하게 저장하지 못했습니다. 코어 로그를 확인해 주세요."
        }
        _ => CORE,
    }
}

/// After admission/import only; never starts core or creates/changes secrets.
/// Passthrough help/version/completion must bypass this API at the caller.
pub(crate) async fn issue_launch(
    plan: &LaunchPlan,
    qb_home: &Path,
    config: &qb_core::config::Config,
) -> Result<IssuedLaunch> {
    if plan.passthrough || plan.selection.is_some() || config.server.admin_port == 0 {
        return Err(refusal(GRAMMAR));
    }
    let model = plan
        .effective_model
        .as_deref()
        .filter(|v| text_id(v))
        .ok_or_else(|| refusal(GRAMMAR))?;
    let secrets = qb_core::local_secrets::load_existing(qb_home).map_err(|_| refusal(CORE))?;
    let client = local_client()?;
    let request = LaunchRequest {
        model,
        model_is_explicit: plan.model_is_explicit,
        account_pref: plan.account_pref.as_deref(),
        resume_id: plan.resume_id.as_deref(),
        move_for_model: plan.move_for_model,
        urgent: plan.urgent,
    };
    let response = client
        .post(format!(
            "http://127.0.0.1:{}/admin/v1/launches",
            config.server.admin_port
        ))
        .bearer_auth(&secrets.admin_token)
        .json(&request)
        .send()
        .await
        .map_err(|_| refusal(CORE))?;
    let success = response.status().is_success();
    let bytes = bounded_body(response, MAX_REPLY_BYTES, REPLY).await?;
    if !success {
        return Err(refusal(admin_failure(&bytes)));
    }
    let reply: LaunchReply = serde_json::from_slice(&bytes).map_err(|_| refusal(REPLY))?;
    validated_launch(
        plan,
        reply,
        config.server.data_port,
        client,
        secrets.client_key,
    )
}

fn validated_launch(
    plan: &LaunchPlan,
    reply: LaunchReply,
    data_port: u16,
    client: reqwest::Client,
    client_key: String,
) -> Result<IssuedLaunch> {
    let prefix = format!("http://127.0.0.1:{data_port}/l/");
    let token = reply
        .base_url
        .strip_prefix(&prefix)
        .and_then(|v| v.strip_suffix("/v1"))
        .filter(|v| {
            v.len() == 43
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        })
        .ok_or_else(|| refusal(REPLY))?;
    // Entropy is an issuer guarantee; a client can check only the handle syntax.
    if data_port == 0
        || token.is_empty()
        || reply.catalog_url != format!("{}/models", reply.base_url)
        || !account_name(&reply.account)
        || reply.model.as_deref().is_some_and(|m| !text_id(m))
        || (plan.model_is_explicit && reply.model != plan.effective_model)
    {
        return Err(refusal(REPLY));
    }
    Ok(IssuedLaunch {
        base_url: reply.base_url,
        catalog_url: reply.catalog_url,
        account: reply.account,
        effective_model: reply.model,
        native_args: plan.native_args.clone(),
        model_is_explicit: plan.model_is_explicit,
        client,
        client_key,
    })
}

impl IssuedLaunch {
    pub(crate) fn account(&self) -> &str {
        &self.account
    }
    pub(crate) fn effective_model(&self) -> Option<&str> {
        self.effective_model.as_deref()
    }

    /// A mismatch is terminal for this acquisition: never retry into a wrong menu.
    pub(crate) async fn fetch_catalog(&self) -> Result<FetchedCatalog> {
        self.fetch_catalog_with_deadline(DEADLINE).await
    }

    async fn fetch_catalog_with_deadline(&self, deadline: Duration) -> Result<FetchedCatalog> {
        tokio::time::timeout(deadline, async {
            let response = self
                .client
                .get(&self.catalog_url)
                .bearer_auth(&self.client_key)
                .header("accept-encoding", "identity")
                .send()
                .await
                .map_err(|_| refusal(CATALOG))?;
            if !response.status().is_success() {
                return Err(refusal(CATALOG));
            }
            let mut accounts = response.headers().get_all("x-quota-baton-account").iter();
            if accounts.next().and_then(|v| v.to_str().ok()) != Some(self.account.as_str())
                || accounts.next().is_some()
            {
                return Err(refusal(ACCOUNT_GUARD));
            }
            if response.headers().contains_key("content-encoding") {
                return Err(refusal(CATALOG));
            }
            let bytes = bounded_body(response, MAX_CATALOG_BYTES, CATALOG).await?;
            let envelope: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| refusal(CATALOG))?;
            if !envelope.is_object() || !envelope.get("models").is_some_and(|v| v.is_array()) {
                return Err(refusal(CATALOG));
            }
            Ok(FetchedCatalog { bytes, envelope })
        })
        .await
        .map_err(|_| refusal(CATALOG))?
    }

    /// Caller supplies an immutable bounded snapshot and retains its owner.
    /// This conversion does not inspect a file or certify native schema acceptance.
    pub(crate) fn native_args(&self, snapshot_path: &Path) -> Result<Vec<OsString>> {
        let path = snapshot_path
            .to_str()
            .filter(|value| snapshot_path.is_absolute() && !value.contains('\0'))
            .ok_or_else(|| refusal(SNAPSHOT_PATH))?;
        let mut args = Vec::with_capacity(self.native_args.len() + 6);
        for (key, value) in [
            ("openai_base_url", self.base_url.as_str()),
            ("model_catalog_json", path),
        ] {
            // Encode a TOML string, never shell quoting or lossy path conversion.
            let quoted = toml_edit::Value::from(value).to_string();
            args.push("-c".into());
            args.push(format!("{key}={quoted}").into());
        }
        // Only implicit defaults can differ: explicit requests were checked above.
        // A CLI model overrides home configuration, including on resume.
        if let Some(model) = &self.effective_model {
            if !self.model_is_explicit {
                args.push("--model".into());
                args.push(model.into());
            }
        } else {
            // No authoritative model cannot safely inherit a different home default.
            return Err(refusal(REPLY));
        }
        args.extend_from_slice(&self.native_args);
        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "12345678-1234-4234-8234-123456789abc";
    const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn planned(args: &[&str]) -> LaunchPlan {
        resolve(inspect(&argv(args)).unwrap(), "model = 'configured'\n").unwrap()
    }

    fn reply() -> LaunchReply {
        LaunchReply {
            base_url: format!("http://127.0.0.1:10200/l/{TOKEN}/v1"),
            catalog_url: format!("http://127.0.0.1:10200/l/{TOKEN}/v1/models"),
            account: "synthetic".into(),
            model: Some("configured".into()),
        }
    }

    fn overrides(plan: &LaunchPlan, reply: LaunchReply, port: u16) -> Result<Vec<OsString>> {
        validated_launch(plan, reply, port, local_client()?, "fixture-key".into())?.native_args(
            &std::env::current_dir()
                .unwrap()
                .join("fixture-catalog.json"),
        )
    }

    #[tokio::test]
    async fn informational_caller_reaches_runner_without_home_or_core_preflight() {
        // Relative program fails the runner's initial validation without spawn.
        // HOME/CORE errors would expose accidental setup work on this bypass.
        for args in [
            vec!["--help"],
            vec!["--version"],
            vec!["completion", "powershell"],
            vec!["--account=alpha", "--help"],
        ] {
            let error = run_codex(Path::new("synthetic-relative-program"), None, &argv(&args))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), NATIVE);
            assert_eq!(error.chain().count(), 1);
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn snapshot_is_released_on_argument_and_runner_errors() {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r11-catalog-snapshot-tests");
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        for model in [None, Some("configured".to_owned())] {
            let expected = if model.is_none() { REPLY } else { NATIVE };
            let mut response = reply();
            response.model = model;
            let issued = validated_launch(
                &planned(&[]),
                response,
                10200,
                local_client().unwrap(),
                "fixture-key".into(),
            )
            .ok()
            .unwrap();
            let snapshot = crate::catalog_snapshot::CatalogSnapshot::create(
                home.path(),
                br#"{ "models": [], "future": true }"#,
            )
            .unwrap();
            let path = snapshot.path().to_owned();
            let error = run_issued(
                Path::new("synthetic-relative-program"),
                &issued,
                &qb_core::config::Config::default(),
                snapshot,
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert!(!path.exists());
        }
    }

    #[test]
    fn title_short_comes_from_issued_actual_account_not_preference() {
        let mut config = qb_core::config::Config::default();
        config.accounts.insert(
            "preferred".into(),
            qb_core::config::AccountConfig {
                short: Some("aa".into()),
                ..Default::default()
            },
        );
        config.accounts.insert(
            "actual".into(),
            qb_core::config::AccountConfig {
                short: Some("bb".into()),
                ..Default::default()
            },
        );
        assert_eq!(account_short(&config, "actual"), Some("bb"));
        assert_eq!(account_short(&config, "unknown"), None);
    }

    #[test]
    fn qb_options_before_native_and_after_resume_are_removed_only() {
        for input in [
            vec![
                "--account",
                "alpha",
                "--move-for-model",
                "-m",
                "chosen",
                "resume",
                UUID,
            ],
            vec![
                "-m",
                "chosen",
                "resume",
                UUID,
                "--account=alpha",
                "--move-for-model",
            ],
        ] {
            let plan = planned(&input);
            assert_eq!(plan.native_args, argv(&["-m", "chosen", "resume", UUID]));
            assert_eq!(plan.account_pref.as_deref(), Some("alpha"));
            assert!(plan.move_for_model && plan.model_is_explicit);
            assert_eq!(plan.effective_model.as_deref(), Some("chosen"));
            assert_eq!(plan.resume_id.as_deref(), Some(UUID));
        }
    }

    #[test]
    fn native_operands_and_literal_prompts_are_preserved() {
        for input in [
            vec!["--image", "--account", "--", "--move-for-model"],
            vec!["-C", "--account", "--", "--account", "alpha"],
            vec!["resume", UUID, "--", "--account"],
            vec!["--", "--account", "alpha", "--model", "not-an-option"],
            vec!["--image=--account", "prompt"],
        ] {
            let plan = planned(&input);
            assert_eq!(plan.native_args, argv(&input));
            assert!(plan.account_pref.is_none());
            assert!(!plan.move_for_model);
            assert_eq!(plan.effective_model.as_deref(), Some("configured"));
        }
    }

    #[test]
    fn model_spellings_and_precedence() {
        for input in [
            vec!["-m", "chosen"],
            vec!["--model", "chosen"],
            vec!["--model=chosen"],
            vec!["-mchosen"],
            vec!["-m=chosen"],
        ] {
            assert_eq!(planned(&input).effective_model.as_deref(), Some("chosen"));
        }
        for input in [
            vec!["-c", "model='override'"],
            vec!["--config", "model=override"],
            vec!["--config=model=override"],
            vec!["-cmodel=override"],
            vec!["-c=model=override"],
        ] {
            let plan = planned(&input);
            assert_eq!(plan.effective_model.as_deref(), Some("override"));
            assert!(plan.model_is_explicit);
            assert_eq!(plan.native_args, argv(&input));
        }
        assert_eq!(
            planned(&["-c", "model='override'", "-m", "chosen"])
                .effective_model
                .as_deref(),
            Some("chosen")
        );
    }

    #[test]
    fn ambiguous_profiles_overrides_and_unknown_arities_refuse() {
        for input in [
            vec!["--account"],
            vec!["--account="],
            vec!["--account", "--move-for-model"],
            vec!["--account=a", "--account=b"],
            vec!["--move-for-model", "--move-for-model"],
            vec!["--move-for-model=true"],
            vec!["-m", "one", "-m", "two"],
            vec!["-c", "model=one", "-c", "model=two"],
            vec!["-c", "model=true"],
            vec!["-c", "model=[]"],
            vec!["-c", "'model'=one"],
            vec!["--new-unknown", "--account", "operand"],
            vec!["--profile", "p"],
            vec!["-pp"],
            vec!["--oss"],
            vec!["-c", "model_provider='other'"],
            vec!["-c", "model_providers.openai.base_url='secret'"],
            vec!["-c", "openai_base_url='secret'"],
            vec!["-c", "model_catalog_url='secret'"],
            vec!["-c", "model_catalog_json=[]"],
            vec!["-c", "profiles.p.model='other'"],
        ] {
            assert!(
                inspect(&argv(&input))
                    .and_then(|p| resolve(p, "model='root'\n"))
                    .is_err()
            );
        }
    }

    #[test]
    fn resume_exec_last_and_picker_use_existing_uuid_parser() {
        assert_eq!(
            planned(&["exec", "resume", UUID, "--account", "alpha"])
                .resume_id
                .as_deref(),
            Some(UUID)
        );
        assert_eq!(
            planned(&["e", "resume", UUID]).resume_id.as_deref(),
            Some(UUID)
        );
        for input in [
            vec!["resume"],
            vec!["resume", "--last"],
            vec!["resume", UUID, "--last"],
        ] {
            let plan = planned(&input);
            assert_eq!(plan.native_args, argv(&input));
            assert!(plan.resume_id.is_none());
            assert!(plan.selection.is_some());
        }
    }

    #[tokio::test]
    async fn unresolved_selection_cannot_issue_or_read_local_keys() {
        let plan = planned(&["resume", "--last"]);
        let error = issue_launch(
            &plan,
            Path::new("missing-synthetic-qb-home"),
            &qb_core::config::Config::default(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.to_string(), GRAMMAR);
        assert_eq!(error.chain().count(), 1);
    }

    #[test]
    fn help_version_completion_need_no_home_and_do_not_consume_operands() {
        let home = Path::new("this-path-must-not-be-read-for-native-help");
        for input in [
            vec!["--help"],
            vec!["-V"],
            vec!["resume", "--help"],
            vec!["help", "resume"],
            vec!["completion", "powershell"],
            vec!["--account=alpha", "completion", "powershell"],
            vec!["-p", "unsupported", "--help"],
        ] {
            assert!(parse(&argv(&input), home).unwrap().passthrough);
        }
        for input in [
            vec!["--image", "--help"],
            vec!["--", "--help"],
            vec!["exec", "help"],
        ] {
            assert!(!inspect(&argv(&input)).unwrap().plan.passthrough);
        }
    }

    #[test]
    fn home_config_and_relevant_overrides_are_checked_without_real_files() {
        for text in [
            "",
            "model = 1",
            "model = 'bad model'",
            "[broken",
            "model='root'\nprofile='p'",
            "model='root'\nmodel_provider='other'",
            "model='root'\n[model_providers.openai]\nbase_url='secret'",
            "model='root'\nmodel_catalog_json=[]",
        ] {
            assert!(resolve(inspect(&[]).unwrap(), text).is_err());
        }
        assert!(resolve(inspect(&argv(&["-m", "chosen"])).unwrap(), "").is_ok());
        assert!(
            resolve(
                inspect(&[]).unwrap(),
                "model='root'\n[profiles.unselected]\nmodel='unused'"
            )
            .is_ok()
        );
        assert!(
            planned(&["-c", "model_reasoning_effort='high'"])
                .effective_model
                .is_some()
        );
    }

    #[test]
    fn parse_reads_only_a_bounded_synthetic_home_config_without_writing() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let original = b"\xef\xbb\xbfmodel='fixture-model'\n";
        std::fs::write(&path, original).unwrap();
        let plan = parse(&argv(&["resume", "--last"]), home.path()).unwrap();
        assert_eq!(plan.effective_model.as_deref(), Some("fixture-model"));
        assert!(!plan.model_is_explicit);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES + 1]).unwrap();
        assert!(parse(&[], home.path()).is_err());
    }

    #[test]
    fn process_overrides_are_toml_strings_before_unchanged_native_args() {
        let plan = planned(&["resume", UUID, "--", "--account"]);
        let args = overrides(&plan, reply(), 10200).unwrap();
        assert_eq!(&args[6..], plan.native_args.as_slice());
        for (offset, key) in [(0, "openai_base_url"), (2, "model_catalog_json")] {
            assert_eq!(args[offset], OsString::from("-c"));
            let doc = args[offset + 1]
                .to_str()
                .unwrap()
                .parse::<DocumentMut>()
                .unwrap();
            assert!(doc[key].as_str().is_some());
        }
    }

    #[test]
    fn response_urls_cannot_escape_the_configured_numeric_loopback_or_launch() {
        let plan = planned(&[]);
        for base in [
            format!("http://localhost:10200/l/{TOKEN}/v1"),
            format!("http://127.0.0.1:10201/l/{TOKEN}/v1"),
            format!("https://127.0.0.1:10200/l/{TOKEN}/v1"),
            format!("http://127.0.0.1:10200/l/{TOKEN}/v1?secret=yes"),
            "http://127.0.0.1:10200/l/short/v1".into(),
            format!("http://127.0.0.1:10200/l/{}/v1", "%".repeat(43)),
            format!("http://127.0.0.1:10200/l/{TOKEN}/v1/"),
            format!("http://user@127.0.0.1:10200/l/{TOKEN}/v1"),
        ] {
            let mut response = reply();
            response.base_url = base;
            assert!(overrides(&plan, response, 10200).is_err());
        }
        let mut response = reply();
        response.catalog_url = "http://127.0.0.1:10200/v1/models".into();
        assert!(overrides(&plan, response, 10200).is_err());
    }

    #[test]
    fn errors_and_debug_retain_no_input_or_parser_source_chain() {
        let plan = planned(&["-c", "model_reasoning_effort='sensitive'"]);
        assert_eq!(format!("{plan:?}"), "LaunchPlan([REDACTED])");
        let mut response = reply();
        response.catalog_url.push_str("?sensitive");
        let error = overrides(&plan, response, 10200).unwrap_err();
        for display in [
            format!("{error}"),
            format!("{error:?}"),
            format!("{error:#}"),
        ] {
            assert!(!display.contains(TOKEN));
            assert!(!display.contains("sensitive"));
            assert!(!display.contains("http"));
        }
        assert_eq!(error.chain().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_native_operands_survive_unchanged() {
        use std::os::windows::ffi::OsStringExt;
        let opaque = OsString::from_wide(&[0xd800]);
        let args = vec!["--image".into(), opaque, "--".into(), "--account".into()];
        let plan = resolve(inspect(&args).unwrap(), "model='root'").unwrap();
        assert_eq!(plan.native_args, args);
        assert!(plan.account_pref.is_none());
    }

    #[tokio::test]
    async fn synthetic_admin_authentication_and_bounded_failures() {
        use axum::{Json, Router, http::HeaderMap, response::IntoResponse, routing::post};
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let home = tempfile::tempdir().unwrap();
        let admin_token = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let client_key = URL_SAFE_NO_PAD.encode([2u8; 32]);
        std::fs::write(home.path().join("admin.token"), &admin_token).unwrap();
        std::fs::write(home.path().join("client.key"), &client_key).unwrap();
        let good_body = serde_json::json!({
            "base_url": format!("http://127.0.0.1:10200/l/{TOKEN}/v1"),
            "catalog_url": format!("http://127.0.0.1:10200/l/{TOKEN}/v1/models"),
            "account": "synthetic",
            "model": "chosen",
        })
        .to_string();
        let mut cases = vec![
            (true, axum::http::StatusCode::OK, good_body),
            (
                false,
                axum::http::StatusCode::TEMPORARY_REDIRECT,
                "not followed".into(),
            ),
            (
                false,
                axum::http::StatusCode::OK,
                "x".repeat(MAX_REPLY_BYTES + 1),
            ),
            (
                false,
                axum::http::StatusCode::OK,
                format!("not-json-{TOKEN}"),
            ),
        ];
        for code in [
            "model_move_consent_required",
            "model_not_supported",
            "launch_capacity",
            "launch_store_error",
            "launch_unavailable",
            "invalid_launch",
            "unknown-secret",
        ] {
            cases.push((false, axum::http::StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({"error":{"code":code,"message":"arbitrary secret upstream body"}}).to_string()));
        }
        for (expected_ok, status, body) in cases {
            let expected_message = if !status.is_success() {
                Some(admin_failure(body.as_bytes()))
            } else {
                None
            };
            let seen = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&seen);
            let expected_auth = format!("Bearer {admin_token}");
            let app = Router::new().route(
                "/admin/v1/launches",
                post(
                    move |headers: HeaderMap, Json(request): Json<serde_json::Value>| {
                        let count = Arc::clone(&count);
                        let expected_auth = expected_auth.clone();
                        let body = body.clone();
                        async move {
                            assert_eq!(
                                headers.get("authorization").unwrap().to_str().unwrap(),
                                expected_auth
                            );
                            assert_eq!(request["model"], "chosen");
                            assert_eq!(request["model_is_explicit"], true);
                            assert_eq!(request["account_pref"], "alpha");
                            assert_eq!(request["resume_id"], UUID);
                            assert_eq!(request["move_for_model"], true);
                            assert_eq!(request.as_object().unwrap().len(), 5);
                            count.fetch_add(1, Ordering::SeqCst);
                            let mut response = (status, body).into_response();
                            response
                                .headers_mut()
                                .insert("location", "/admin/v1/launches".parse().unwrap());
                            response
                        }
                    },
                ),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut config = qb_core::config::Config::default();
            config.server.admin_port = listener.local_addr().unwrap().port();
            config.server.data_port = 10200;
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let plan = planned(&[
                "-m",
                "chosen",
                "resume",
                UUID,
                "--account",
                "alpha",
                "--move-for-model",
            ]);
            let result = issue_launch(&plan, home.path(), &config).await;
            server.abort();
            let _ = server.await;
            assert_eq!(seen.load(Ordering::SeqCst), 1);
            assert_eq!(result.is_ok(), expected_ok);
            if expected_ok {
                let issued = result.unwrap();
                let args = issued
                    .native_args(&home.path().join("catalog.json"))
                    .unwrap();
                assert_eq!(&args[4..], plan.native_args.as_slice());
            } else {
                let error = result.err().unwrap();
                if let Some(message) = expected_message {
                    assert_eq!(error.to_string(), message);
                }
                assert!(!format!("{error:?}").contains(TOKEN));
                assert!(!format!("{error:?}").contains("arbitrary secret"));
                assert_eq!(error.chain().count(), 1);
            }
        }
        assert_eq!(
            std::fs::read_to_string(home.path().join("admin.token")).unwrap(),
            admin_token
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("client.key")).unwrap(),
            client_key
        );
        assert!(!home.path().join("config.toml").exists());
    }

    #[test]
    fn resume_reply_wins_over_implicit_default_and_explicit_mismatch_refuses() {
        let plan = planned(&["resume", UUID, "--", "opaque"]);
        let mut response = reply();
        response.model = Some("binding-current".into());
        let issued = validated_launch(
            &plan,
            response,
            10200,
            local_client().unwrap(),
            "fixture-key".into(),
        )
        .ok()
        .unwrap();
        assert_eq!(issued.effective_model(), Some("binding-current"));
        assert_eq!(issued.account(), "synthetic");
        let path = std::env::current_dir()
            .unwrap()
            .join("한글 \"catalog\".json");
        let args = issued.native_args(&path).unwrap();
        assert_eq!(args[4], "--model");
        assert_eq!(args[5], "binding-current");
        assert_eq!(&args[6..], plan.native_args.as_slice());
        let doc = args[3].to_str().unwrap().parse::<DocumentMut>().unwrap();
        assert_eq!(doc["model_catalog_json"].as_str(), path.to_str());
        assert!(
            !args
                .iter()
                .any(|a| a.to_string_lossy().contains("model_catalog_url"))
        );
        assert!(issued.native_args(Path::new("relative.json")).is_err());
        let explicit = planned(&["-m", "chosen"]);
        assert!(
            validated_launch(
                &explicit,
                reply(),
                10200,
                local_client().unwrap(),
                "fixture-key".into()
            )
            .is_err()
        );
        let mut matching = reply();
        matching.model = Some("chosen".into());
        let issued = validated_launch(
            &explicit,
            matching,
            10200,
            local_client().unwrap(),
            "fixture-key".into(),
        )
        .ok()
        .unwrap();
        let args = issued.native_args(&path).unwrap();
        assert_eq!(&args[4..], explicit.native_args.as_slice()); // no duplicate CLI model
        let mut absent = reply();
        absent.model = None;
        let issued = validated_launch(
            &plan,
            absent,
            10200,
            local_client().unwrap(),
            "fixture-key".into(),
        )
        .ok()
        .unwrap();
        assert!(issued.native_args(&path).is_err()); // do not invent a default
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_path_rejects_invalid_utf16() {
        use std::os::windows::ffi::OsStringExt;
        let path = std::env::current_dir()
            .unwrap()
            .join(OsString::from_wide(&[0xd800]));
        let issued = validated_launch(
            &planned(&[]),
            reply(),
            10200,
            local_client().unwrap(),
            "fixture-key".into(),
        )
        .ok()
        .unwrap();
        assert!(issued.native_args(&path).is_err());
    }

    // Raw HTTP fixture covers both declared lengths and chunked bodies; it never
    // touches a real home or core. These tests are authored, not run in prep.
    async fn catalog_fixture(
        response: String,
        delay_body: bool,
    ) -> (
        IssuedLaunch,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if socket.read(&mut byte).await.unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                    assert!(request.len() < MAX_REPLY_BYTES);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with(&format!(
                    "get /l/{}/v1/models http/1.1",
                    TOKEN.to_ascii_lowercase()
                )));
                assert!(request.contains("authorization: bearer fixture-key\r\n"));
                assert!(request.contains("accept-encoding: identity\r\n"));
                assert!(!request.contains("admin-token"));
                let (headers, body) = response.split_once("\r\n\r\n").unwrap();
                if socket
                    .write_all(format!("{headers}\r\n\r\n").as_bytes())
                    .await
                    .is_err()
                {
                    continue;
                }
                if delay_body {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                let _ = socket.write_all(body.as_bytes()).await;
            }
        });
        let mut response = reply();
        response.base_url = format!("http://127.0.0.1:{port}/l/{TOKEN}/v1");
        response.catalog_url = format!("{}/models", response.base_url);
        let issued = validated_launch(
            &planned(&[]),
            response,
            port,
            local_client().unwrap(),
            "fixture-key".into(),
        )
        .ok()
        .unwrap();
        (issued, server, seen)
    }

    #[tokio::test]
    async fn catalog_auth_bounds_redirect_account_guard_envelope_and_deadline() {
        use std::sync::atomic::Ordering;
        let raw = "{ \"models\": [], \"capabilities\": {\"opaque\":true} }";
        let at_limit = format!("{}{}", raw, " ".repeat(MAX_CATALOG_BYTES - raw.len()));
        let overflow = format!("{at_limit} ");
        let chunked = |body: &str| {
            format!(
                "HTTP/1.1 200 OK\r\nx-quota-baton-account: synthetic\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            )
        };
        for (response, expected, delay) in [
            (chunked(&at_limit), "ok", false),
            (chunked(&overflow), "catalog", false),
            (
                format!(
                    "HTTP/1.1 200 OK\r\nx-quota-baton-account: synthetic\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    MAX_CATALOG_BYTES + 1
                ),
                "catalog",
                false,
            ),
            (
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: /trap\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                "catalog",
                false,
            ),
            (
                chunked(raw).replace("account: synthetic", "account: moved"),
                "guard",
                false,
            ),
            (
                chunked(raw).replace("x-quota-baton-account: synthetic\r\n", ""),
                "guard",
                false,
            ),
            (
                chunked(raw).replace(
                    "account: synthetic",
                    "account: synthetic\r\nx-quota-baton-account: synthetic",
                ),
                "guard",
                false,
            ),
            (chunked("{\"data\":[]}"), "catalog", false),
            (chunked("invalid secret body"), "catalog", false),
            (
                chunked(raw).replace(
                    "Transfer-Encoding:",
                    "Content-Encoding: gzip\r\nTransfer-Encoding:",
                ),
                "catalog",
                false,
            ),
            (chunked(raw), "catalog", true),
        ] {
            let (issued, server, seen) = catalog_fixture(response, delay).await;
            let result = if delay {
                issued
                    .fetch_catalog_with_deadline(Duration::from_millis(40))
                    .await
            } else {
                issued.fetch_catalog().await
            };
            server.abort();
            let _ = server.await;
            assert_eq!(seen.load(Ordering::SeqCst), 1); // redirect cannot cause a second GET
            if expected == "ok" {
                let fetched = result.ok().unwrap();
                assert_eq!(fetched.bytes, at_limit.as_bytes());
                assert_eq!(fetched.envelope["capabilities"]["opaque"], true);
            } else {
                let error = result.err().unwrap();
                assert_eq!(
                    error.to_string(),
                    if expected == "guard" {
                        ACCOUNT_GUARD
                    } else {
                        CATALOG
                    }
                );
                assert_eq!(error.chain().count(), 1);
            }
        }
    }

    #[test]
    fn admin_code_allowlist_ignores_arbitrary_messages_and_unknown_codes() {
        for (code, fragment) in [
            ("model_move_consent_required", "--move-for-model"),
            ("model_not_supported", "지원하지"),
            ("launch_capacity", "종료"),
            ("launch_store_error", "저장"),
            ("launch_unavailable", "사용 가능"),
            ("invalid_launch", "옵션"),
        ] {
            let body = serde_json::json!({"error":{"code":code,"message":"sensitive arbitrary upstream URL/token"}}).to_string();
            let message = admin_failure(body.as_bytes());
            assert!(message.contains(fragment));
            assert!(!message.contains("sensitive"));
        }
        assert_eq!(
            admin_failure(br#"{"error":{"code":"secret","message":"secret"}}"#),
            CORE
        );
        assert_eq!(admin_failure(b"secret invalid JSON"), CORE);
    }

    #[test]
    fn wire_request_contains_the_five_agreed_fields_including_explicit_intent() {
        let request = LaunchRequest {
            model: "synthetic",
            model_is_explicit: true,
            account_pref: Some("alpha"),
            resume_id: Some(UUID),
            move_for_model: true,
            urgent: false,
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 5);
        assert_eq!(value["model"], "synthetic");
        assert_eq!(value["model_is_explicit"], true);
        assert_eq!(value["account_pref"], "alpha");
        assert_eq!(value["resume_id"], UUID);
        assert_eq!(value["move_for_model"], true);
    }

    #[test]
    fn n5c_codex_urgent_interactive_exec_and_resume_strip_only_launcher_intent() {
        for (input, expected) in [
            (vec!["--urgent", "-m", "chosen"], vec!["-m", "chosen"]),
            (
                vec!["exec", "--urgent", "-m", "chosen", "synthetic prompt"],
                vec!["exec", "-m", "chosen", "synthetic prompt"],
            ),
            (
                vec!["e", "resume", UUID, "--urgent", "-m", "chosen"],
                vec!["e", "resume", UUID, "-m", "chosen"],
            ),
            (
                vec!["--urgent", "exec", "--", "--urgent"],
                vec!["exec", "--", "--urgent"],
            ),
        ] {
            let plan = planned(&input);
            assert!(plan.urgent);
            assert_eq!(plan.native_args, argv(&expected));
            let args = overrides(&plan, reply_for(&plan), 10200).unwrap();
            let prefix = if plan.model_is_explicit { 4 } else { 6 };
            assert_eq!(&args[prefix..], plan.native_args.as_slice());
        }
        assert!(inspect(&argv(&["--urgent", "--urgent"])).is_err());
        assert!(inspect(&argv(&["--urgent=true"])).is_err());
    }

    fn reply_for(plan: &LaunchPlan) -> LaunchReply {
        LaunchReply {
            model: plan.effective_model.clone(),
            ..reply()
        }
    }

    #[tokio::test]
    async fn n5c_codex_urgent_uses_real_authenticated_issue_request() {
        use axum::{Json, Router, http::HeaderMap, routing::post};
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let admin = URL_SAFE_NO_PAD.encode([1u8; 32]);
        std::fs::write(home.path().join("admin.token"), &admin).unwrap();
        std::fs::write(
            home.path().join("client.key"),
            URL_SAFE_NO_PAD.encode([2u8; 32]),
        )
        .unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        let app = Router::new().route(
            "/admin/v1/launches",
            post(
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let admin = admin.clone();
                    let count = count.clone();
                    async move {
                        assert_eq!(headers["authorization"], format!("Bearer {admin}"));
                        assert_eq!(
                            body,
                            serde_json::json!({
                                "model":"chosen", "model_is_explicit":true,
                                "account_pref":null, "resume_id":null,
                                "move_for_model":false, "urgent":true,
                            })
                        );
                        count.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "base_url":format!("http://127.0.0.1:10200/l/{TOKEN}/v1"),
                            "catalog_url":format!("http://127.0.0.1:10200/l/{TOKEN}/v1/models"),
                            "account":"synthetic", "model":"chosen",
                        }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = qb_core::config::Config::default();
        config.server.admin_port = listener.local_addr().unwrap().port();
        config.server.data_port = 10200;
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let plan = planned(&["--urgent", "exec", "-m", "chosen", "synthetic prompt"]);
        let issued = issue_launch(&plan, home.path(), &config).await.unwrap();
        let native = issued
            .native_args(&home.path().join("synthetic-catalog.json"))
            .unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(
            &native[4..],
            argv(&["exec", "-m", "chosen", "synthetic prompt"])
        );
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn n5c_codex_urgent_operands_prompts_and_plain_requests_are_unchanged() {
        for input in [
            vec!["--image", "--urgent"],
            vec!["--image=--urgent", "synthetic prompt"],
            vec!["--", "--urgent"],
            vec!["exec", "synthetic prompt", "--urgent"],
        ] {
            let plan = planned(&input);
            assert!(!plan.urgent);
            assert_eq!(plan.native_args, argv(&input));
        }
        let home = Path::new("synthetic-nonexistent-help-home");
        for input in [
            vec!["--urgent", "--help"],
            vec!["exec", "--urgent", "--help"],
        ] {
            let plan = parse(&argv(&input), home).unwrap();
            assert!(plan.passthrough);
            assert!(!plan.native_args.iter().any(|arg| arg == "--urgent"));
        }
        let request = LaunchRequest {
            model: "synthetic",
            model_is_explicit: true,
            account_pref: None,
            resume_id: None,
            move_for_model: false,
            urgent: false,
        };
        assert!(
            serde_json::to_value(&request)
                .unwrap()
                .get("urgent")
                .is_none()
        );
        let urgent = LaunchRequest {
            urgent: true,
            ..request
        };
        assert_eq!(serde_json::to_value(urgent).unwrap()["urgent"], true);
    }
}

pub(crate) async fn run_claude(
    program: &Path,
    settings: Option<&Path>,
    args: &[OsString],
) -> Result<i32> {
    claude_urgent::run(program, settings, args).await
}

mod claude_urgent {
    //! Bounded Claude launch planning and one-time authenticated urgency intent.
    //! Options are recognized only before the first positional prompt or `--`.
    //! Unknown grammar is passed through ordinarily, but refuses an urgent launch.

    use std::{ffi::OsString, path::Path, time::Duration};

    const REGISTER: &str =
        "긴급 실행 정책을 저장하지 못했습니다. 코어를 실행한 뒤 다시 시도해 주세요.";

    fn fresh_uuid() -> String {
        let mut bytes: [u8; 16] = rand::random();
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )
    }

    /// Explicit launch intent is stored once, before starting the child. Ordinary
    /// launches perform no admin I/O and never overwrite a later dashboard toggle.
    pub(crate) async fn run(
        program: &Path,
        settings: Option<&Path>,
        args: &[OsString],
    ) -> anyhow::Result<i32> {
        let plan = parse(args, fresh_uuid).map_err(crate::accounts_cli::AccountCliError)?;
        if plan.urgent {
            let failure = || crate::accounts_cli::AccountCliError(REGISTER);
            let home = qb_core::home().map_err(|_| failure())?;
            let config = qb_core::config::Config::load(&home).map_err(|_| failure())?;
            register(&home, &config, &plan).await?;
        }
        Ok(crate::launcher_label::run(
            crate::launcher_label::Client::Claude,
            program,
            settings,
            &plan.native_args,
        )?)
    }

    async fn register(
        home: &Path,
        config: &qb_core::config::Config,
        plan: &Plan,
    ) -> anyhow::Result<()> {
        let failure = || crate::accounts_cli::AccountCliError(REGISTER);
        let id = plan
            .expected_root
            .as_deref()
            .and_then(|root| root.strip_prefix("claude:"))
            .filter(|id| uuid(id).is_some())
            .ok_or_else(failure)?;
        anyhow::ensure!(plan.urgent && config.server.admin_port != 0, failure());
        let secrets = qb_core::local_secrets::load_existing(home).map_err(|_| failure())?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| failure())?;
        let mut response = client
            .post(format!(
                "http://127.0.0.1:{}/admin/v1/routing",
                config.server.admin_port
            ))
            .bearer_auth(&secrets.admin_token)
            .json(&serde_json::json!({"kind":"claude_urgent_launch","session_id":id}))
            .send()
            .await
            .map_err(|_| failure())?;
        // No upstream error body or credential value crosses this CLI boundary.
        anyhow::ensure!(response.status().is_success(), failure());
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| failure())? {
            anyhow::ensure!(bytes.len().saturating_add(chunk.len()) <= 4096, failure());
            bytes.extend_from_slice(&chunk);
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            updated: bool,
        }
        let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| failure())?;
        anyhow::ensure!(reply.updated, failure());
        Ok(())
    }

    #[derive(PartialEq, Eq)]
    pub(crate) struct Plan {
        pub native_args: Vec<OsString>,
        /// C8 policy root, never a display label or a managed Codex launch handle.
        pub expected_root: Option<String>,
        pub urgent: bool,
    }

    impl std::fmt::Debug for Plan {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("ClaudeLaunchPlan([REDACTED])")
        }
    }

    const GRAMMAR: &str = "긴급 실행에서 확인되지 않은 Claude 옵션입니다. 명시적인 세션 ID와 지원되는 옵션을 사용해 주세요.";
    const SELECTOR: &str = "긴급 실행은 새 세션 또는 정확한 UUID 재개만 지원합니다. 세션 선택기·계속·분기·원격 재개는 사용할 수 없습니다.";
    const UUID: &str = "긴급 실행의 세션 ID는 하이픈을 포함한 정확한 UUID여야 합니다.";
    const DUPLICATE: &str = "긴급 실행 옵션이나 세션 선택 옵션을 중복 지정할 수 없습니다.";
    const OPERAND: &str = "긴급 실행의 Claude 옵션에 필요한 값이 없습니다.";

    fn uuid(value: &str) -> Option<String> {
        (value.len() == 36
            && value.bytes().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            }))
        // C8 hashes the native string, not a parsed UUID. Do not invent a casing
        // conversion here; the eventual ingress must verify the emitted ID exactly.
        .then(|| value.to_owned())
    }

    // Single-operand forms only. In particular -p is Claude's print switch, not
    // Codex's profile option. These operands are used by the dedicated launcher or
    // the existing Claude hold fixture. Variadic tool/directory options are not
    // guessed from their spelling or from a fixture passing one empty value.
    fn takes_value(name: &str) -> bool {
        matches!(
            name,
            "--model"
                | "--settings"
                | "--setting-sources"
                | "--max-turns"
                | "--output-format"
                | "--input-format"
                | "--permission-mode"
                | "--system-prompt"
        )
    }

    fn switch(name: &str) -> bool {
        matches!(
            name,
            "--print" | "-p" | "--no-session-persistence" | "--strict-mcp-config" | "--verbose"
        )
    }

    /// The coordinator supplies UUID generation lazily. It is called exactly once
    /// only for a valid fresh urgent launch without an explicit native session ID.
    /// Ordinary launches return the original arguments and never call it.
    ///
    /// This supports a bounded option vocabulary, not Claude's complete grammar.
    /// No selector or prompt is rewritten; fresh ID injection is the only addition.
    pub(crate) fn parse(
        args: &[OsString],
        fresh_uuid: impl FnOnce() -> String,
    ) -> Result<Plan, &'static str> {
        let mut native_args = Vec::with_capacity(args.len());
        let mut index = 0;
        let mut urgent = false;
        let mut informational = false;
        let mut error = None;
        let mut resume = None;
        let mut session = None;
        let mut resume_seen = false;
        let mut session_seen = false;
        while index < args.len() {
            let start = index;
            index += 1;
            let Some(arg) = args[start].to_str() else {
                // An opaque positional prompt ends our options region. An opaque
                // option cannot be assigned a guessed arity on the urgent path.
                if args[start].to_string_lossy().starts_with('-') {
                    error.get_or_insert(GRAMMAR);
                    native_args.push(args[start].clone());
                    continue;
                }
                native_args.extend_from_slice(&args[start..]);
                break;
            };
            if arg == "--" || !arg.starts_with('-') || arg == "-" {
                native_args.extend_from_slice(&args[start..]);
                break;
            }
            // Informational native options are recognized only in the option
            // region, never as an opaque option operand or literal prompt.
            if matches!(arg, "--help" | "-h" | "--version" | "-v") {
                informational = true;
                native_args.push(args[start].clone());
                continue;
            }
            if arg == "--urgent" {
                if urgent {
                    error.get_or_insert(DUPLICATE);
                }
                urgent = true;
                continue;
            }
            let (name, attached) = arg
                .split_once('=')
                .map_or((arg, None), |(name, value)| (name, Some(value)));
            native_args.push(args[start].clone());
            if name == "--resume" || name == "-r" || name == "--session-id" {
                if name == "-r" && attached.is_some() {
                    error.get_or_insert(GRAMMAR);
                }
                let is_resume = name != "--session-id";
                let seen = if is_resume {
                    &mut resume_seen
                } else {
                    &mut session_seen
                };
                if *seen {
                    error.get_or_insert(DUPLICATE);
                }
                *seen = true;
                // Resume's optional operand is absent when the next token is an
                // option. Never swallow --urgent as a picker/name operand.
                let value = if let Some(value) = attached {
                    Some(value)
                } else if let Some(value) = args.get(index).and_then(|value| value.to_str())
                    && !value.starts_with('-')
                {
                    native_args.push(args[index].clone());
                    index += 1;
                    Some(value)
                } else {
                    None
                };
                match value.and_then(uuid) {
                    Some(id) if is_resume => resume = Some(id),
                    Some(id) => session = Some(id),
                    None => {
                        error.get_or_insert(if is_resume { SELECTOR } else { UUID });
                    }
                }
            } else if matches!(
                name,
                "--continue" | "-c" | "--fork-session" | "--teleport" | "--from-pr"
            ) {
                error.get_or_insert(SELECTOR);
                // A remote/PR selector may carry an ID. Recognizing that token
                // only lets us reject following urgency; it cannot authorize a
                // launch or change ordinary native arguments.
                if matches!(name, "--teleport" | "--from-pr")
                    && attached.is_none()
                    && args
                        .get(index)
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| !value.starts_with('-'))
                {
                    native_args.push(args[index].clone());
                    index += 1;
                }
            } else if matches!(name, "--tools" | "--mcp-config") && attached.is_some() {
                // Native variadic flags are unambiguous only in one-token form.
            } else if takes_value(name) {
                if attached.is_none() {
                    if let Some(value) = args.get(index) {
                        // Values are opaque, including a literal --urgent or --.
                        native_args.push(value.clone());
                        index += 1;
                    } else {
                        error.get_or_insert(OPERAND);
                    }
                }
            } else if !switch(name) || attached.is_some() {
                error.get_or_insert(GRAMMAR);
            }
        }
        if !urgent {
            return Ok(Plan {
                native_args: args.to_vec(),
                expected_root: None,
                urgent: false,
            });
        }
        if informational {
            return Ok(Plan {
                native_args,
                expected_root: None,
                urgent: false,
            });
        }
        if args.len() > super::MAX_ARGS {
            return Err(GRAMMAR);
        }
        if let Some(error) = error {
            return Err(error);
        }
        if resume_seen && session_seen {
            return Err(SELECTOR);
        }
        let id = if let Some(id) = resume.or(session) {
            id
        } else {
            let id = uuid(&fresh_uuid()).ok_or(UUID)?;
            // Prepend before the preserved prompt/-- tail; never append an option
            // where native parsing would treat it as user input.
            native_args.splice(0..0, [OsString::from("--session-id"), OsString::from(&id)]);
            id
        };
        Ok(Plan {
            native_args,
            expected_root: Some(format!("claude:{id}")),
            urgent: true,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::Cell;

        const ID: &str = "12345678-1234-4321-abcd-123456789abc";
        const FRESH: &str = "87654321-4321-4321-abcd-cba987654321";

        #[test]
        fn n5c_claude_urgent_generated_id_is_v4_with_canonical_native_shape() {
            let first = fresh_uuid();
            let second = fresh_uuid();
            assert_eq!(uuid(&first).as_deref(), Some(first.as_str()));
            assert_eq!(first.as_bytes()[14], b'4');
            assert!(matches!(first.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
            assert_ne!(first, second);
        }

        #[test]
        fn n5c_claude_urgent_print_transport_options_preserve_exact_operands() {
            let input = args(&[
                "--urgent",
                "--session-id",
                ID,
                "-p",
                "--verbose",
                "--output-format",
                "stream-json",
                "--tools=",
                "--mcp-config={\"mcpServers\":{}}",
                "--system-prompt",
                "--urgent",
                "synthetic prompt",
            ]);
            let plan = parse(&input, || panic!("explicit id must not allocate")).unwrap();
            assert!(plan.urgent);
            assert_eq!(plan.native_args, input[1..]);
        }

        #[tokio::test]
        async fn n5c_claude_urgent_registration_checks_auth_and_bounded_success() {
            use axum::{
                Json, Router,
                http::{HeaderMap, StatusCode},
                routing::post,
            };
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            use std::sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            };
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox");
            std::fs::create_dir_all(&dir).unwrap();
            let home = tempfile::tempdir_in(dir).unwrap();
            let admin = URL_SAFE_NO_PAD.encode([1u8; 32]);
            std::fs::write(home.path().join("admin.token"), &admin).unwrap();
            std::fs::write(
                home.path().join("client.key"),
                URL_SAFE_NO_PAD.encode([2u8; 32]),
            )
            .unwrap();
            let mode = Arc::new(AtomicUsize::new(0));
            let seen = Arc::new(AtomicUsize::new(0));
            let handler_mode = mode.clone();
            let handler_seen = seen.clone();
            let app = Router::new().route(
                "/admin/v1/routing",
                post(
                    move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                        let mode = handler_mode.clone();
                        let seen = handler_seen.clone();
                        let admin = admin.clone();
                        async move {
                            assert_eq!(headers["authorization"], format!("Bearer {admin}"));
                            assert_eq!(
                                body,
                                serde_json::json!({"kind":"claude_urgent_launch","session_id":ID})
                            );
                            seen.fetch_add(1, Ordering::SeqCst);
                            match mode.load(Ordering::SeqCst) {
                                0 => (StatusCode::OK, "{\"updated\":true}".to_owned()),
                                1 => (StatusCode::OK, "{\"updated\":false}".to_owned()),
                                2 => (StatusCode::OK, "private-value".repeat(4096)),
                                _ => (StatusCode::TEMPORARY_REDIRECT, "private-value".to_owned()),
                            }
                        }
                    },
                ),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            assert!(![10100, 10101].contains(&port));
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut config = qb_core::config::Config::default();
            config.server.admin_port = port;
            let plan = planned(&["--urgent", "--session-id", ID]).unwrap();
            for case in 0..4 {
                mode.store(case, Ordering::SeqCst);
                let result = register(home.path(), &config, &plan).await;
                if case == 0 {
                    assert!(result.is_ok());
                } else {
                    assert_eq!(result.unwrap_err().to_string(), REGISTER);
                }
            }
            assert_eq!(seen.load(Ordering::SeqCst), 4);
            server.abort();
        }

        fn args(values: &[&str]) -> Vec<OsString> {
            values.iter().map(OsString::from).collect()
        }

        fn planned(values: &[&str]) -> Result<Plan, &'static str> {
            parse(&args(values), || FRESH.into())
        }

        #[test]
        fn n5c_claude_urgent_fresh_injects_before_prompt_without_changing_input() {
            let plan = planned(&["--urgent", "-p", "hello", "--urgent"]).unwrap();
            assert_eq!(
                plan.native_args,
                args(&["--session-id", FRESH, "-p", "hello", "--urgent"])
            );
            assert_eq!(plan.expected_root, Some(format!("claude:{FRESH}")));
            assert!(plan.urgent);
        }

        #[test]
        fn n5c_claude_urgent_ordinary_is_exact_and_never_allocates() {
            for input in [
                vec![],
                vec!["--unknown", "value"],
                vec!["--continue"],
                vec!["--resume"],
                vec!["--session-id", "invalid"],
                vec!["--model", "--urgent", "hello"],
                vec!["--", "--urgent"],
                vec!["literal prompt", "--urgent"],
                vec!["--urgent=true"],
            ] {
                let original = args(&input);
                let plan = parse(&original, || panic!("ordinary launch allocated ID")).unwrap();
                assert_eq!(plan.native_args, original);
                assert_eq!(plan.expected_root, None);
                assert!(!plan.urgent);
            }
        }

        #[test]
        fn n5c_claude_urgent_option_operands_are_opaque() {
            for option in ["--model", "--settings", "--setting-sources", "--max-turns"] {
                let plan = planned(&[option, "--urgent", "--urgent", "hello"]).unwrap();
                assert_eq!(
                    plan.native_args,
                    args(&["--session-id", FRESH, option, "--urgent", "hello"])
                );
            }
            let plan = planned(&["--urgent", "--model", "--", "hello"]).unwrap();
            assert_eq!(
                plan.native_args,
                args(&["--session-id", FRESH, "--model", "--", "hello"])
            );
        }

        #[test]
        fn n5c_claude_urgent_tail_is_preserved_after_consumed_intent() {
            let plan = planned(&["--urgent", "--", "--urgent", "--continue"]).unwrap();
            assert_eq!(
                plan.native_args,
                args(&["--session-id", FRESH, "--", "--urgent", "--continue"])
            );
        }

        #[test]
        fn n5c_claude_urgent_attached_values_and_unknown_before_intent() {
            let plan = planned(&["--model=--urgent", "--urgent", "hello"]).unwrap();
            assert_eq!(
                plan.native_args,
                args(&["--session-id", FRESH, "--model=--urgent", "hello"])
            );
            assert!(!planned(&["--model=--urgent", "hello"]).unwrap().urgent);
            for input in [
                vec!["--unknown", "--urgent"],
                vec!["--print=true", "--urgent"],
            ] {
                assert_eq!(
                    parse(&args(&input), || panic!("invalid grammar allocated ID")),
                    Err(GRAMMAR)
                );
            }
        }

        #[test]
        fn n5c_claude_urgent_exact_resume_and_explicit_fresh_never_allocate() {
            for input in [
                vec!["--urgent".into(), "--resume".into(), ID.into()],
                vec!["-r".into(), ID.into(), "--urgent".into()],
                vec![format!("--resume={ID}"), "--urgent".into()],
                vec!["--session-id".into(), ID.into(), "--urgent".into()],
                vec![format!("--session-id={ID}"), "--urgent".into()],
            ] {
                let original: Vec<OsString> = input.iter().map(OsString::from).collect();
                let plan = parse(&original, || panic!("explicit ID allocated another ID")).unwrap();
                assert_eq!(plan.expected_root, Some(format!("claude:{ID}")));
                assert_eq!(
                    plan.native_args,
                    original
                        .into_iter()
                        .filter(|v| v != "--urgent")
                        .collect::<Vec<_>>()
                );
            }
        }

        #[test]
        fn n5c_claude_urgent_ambiguous_selectors_refused_without_allocation() {
            for input in [
                vec!["--urgent", "--resume"],
                vec!["--resume", "--urgent"],
                vec!["--urgent", "--resume", "named-session"],
                vec!["--urgent", "-c"],
                vec!["--urgent", "--continue"],
                vec!["--urgent", "--fork-session"],
                vec!["--urgent", "--teleport"],
                vec!["--urgent", "--from-pr=12"],
                vec!["--teleport", ID, "--urgent"],
                vec!["--from-pr", "12", "--urgent"],
                vec!["--urgent", "--resume", ID, "--session-id", ID],
            ] {
                assert!(parse(&args(&input), || panic!("refused launch allocated ID")).is_err());
            }
        }

        #[test]
        fn n5c_claude_urgent_unknown_grammar_refuses_only_urgent_path() {
            for option in [
                "--unknown",
                "--allowedTools",
                "--tools",
                "--add-dir",
                "-pr",
                "--urgent=true",
            ] {
                assert!(planned(&["--urgent", option]).is_err());
                assert!(!planned(&[option]).unwrap().urgent);
            }
        }

        #[test]
        fn n5c_claude_urgent_duplicate_and_missing_operands_refused() {
            for input in [
                vec!["--urgent", "--urgent"],
                vec!["--urgent", "--session-id", ID, "--session-id", ID],
                vec!["--urgent", "--resume", ID, "-r", ID],
                vec!["--urgent", "--model"],
                vec!["--urgent", "--session-id"],
            ] {
                assert!(planned(&input).is_err());
            }
        }

        #[test]
        fn n5c_claude_urgent_uuid_validation_and_canonical_root() {
            for id in [
                "",
                "not-a-uuid",
                "1234567812344321abcd123456789abc",
                "12345678-1234-4321-abcd-123456789abz",
            ] {
                assert_eq!(planned(&["--urgent", "--resume", id]), Err(SELECTOR));
                assert_eq!(planned(&["--urgent", "--session-id", id]), Err(UUID));
            }
            let upper = ID.to_ascii_uppercase();
            let plan = planned(&["--urgent", "--resume", &upper]).unwrap();
            assert_eq!(plan.expected_root, Some(format!("claude:{upper}")));
            assert_eq!(plan.native_args, args(&["--resume", &upper]));
            assert_eq!(parse(&args(&["--urgent"]), || "invalid".into()), Err(UUID));
        }

        #[test]
        fn n5c_claude_urgent_fresh_id_supplier_called_exactly_once() {
            let calls = Cell::new(0);
            let plan = parse(&args(&["--urgent"]), || {
                calls.set(calls.get() + 1);
                FRESH.into()
            })
            .unwrap();
            assert_eq!(calls.get(), 1);
            assert!(plan.urgent);
        }
    }

    #[cfg(test)]
    mod informational_tests {
        use super::*;

        #[test]
        fn n5c_claude_urgent_help_consumes_only_intent_without_allocating() {
            for input in [
                vec!["--urgent", "--help"],
                vec!["--help", "--urgent"],
                vec!["--urgent", "--version"],
                vec!["--urgent", "--unknown", "-h"],
            ] {
                let args: Vec<_> = input.iter().map(OsString::from).collect();
                let plan = parse(&args, || panic!("help must not allocate a native UUID")).unwrap();
                let expected: Vec<_> = input
                    .iter()
                    .filter(|arg| **arg != "--urgent")
                    .map(OsString::from)
                    .collect();
                assert_eq!(plan.native_args, expected);
                assert!(!plan.urgent);
                assert!(plan.expected_root.is_none());
            }
            // These help-looking values must not bypass actual urgent registration.
            for input in [
                vec!["--urgent", "--model", "--help"],
                vec!["--urgent", "--", "--help"],
                vec!["--urgent", "synthetic prompt", "--help"],
            ] {
                let args: Vec<_> = input.iter().map(OsString::from).collect();
                assert!(
                    parse(&args, || "87654321-4321-4321-abcd-cba987654321".into())
                        .unwrap()
                        .urgent
                );
            }
        }

        #[tokio::test]
        async fn n5c_claude_urgent_plain_and_help_reach_native_validation_without_core() {
            // A relative program fails existing runner validation before any spawn.
            // A registration/setup error instead would reveal accidental core I/O.
            for values in [
                vec!["--help"],
                vec!["--urgent", "--help"],
                vec!["--unknown", "value"],
            ] {
                let args: Vec<_> = values.iter().map(OsString::from).collect();
                let expected = crate::launcher_label::run(
                    crate::launcher_label::Client::Claude,
                    Path::new("synthetic-relative-native"),
                    None,
                    &args,
                )
                .unwrap_err();
                let error = run(Path::new("synthetic-relative-native"), None, &args)
                    .await
                    .unwrap_err();
                assert_eq!(error.to_string(), expected.to_string());
                assert!(error.downcast_ref::<std::io::Error>().is_some());
            }
        }

        #[test]
        fn n5c_claude_urgent_debug_does_not_retain_prompt_or_native_id() {
            let args = [
                OsString::from("--urgent"),
                OsString::from("synthetic-private-prompt"),
            ];
            let plan = parse(&args, || "87654321-4321-4321-abcd-cba987654321".into()).unwrap();
            assert_eq!(format!("{plan:?}"), "ClaudeLaunchPlan([REDACTED])");
        }
    }
}
