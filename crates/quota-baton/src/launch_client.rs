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
    crate::sessions::admission_check(&home, id.as_deref())?;
    crate::sessions_admission::preflight_launch(&plan.native_args, program)?;
    crate::sessions::before_codex_resume(&plan.native_args)?;
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
        run_issued(program, &issued, &config, snapshot)
    }
    .await;
    drop(permit);
    outcome
}

fn run_issued(
    program: &Path,
    issued: &IssuedLaunch,
    config: &qb_core::config::Config,
    snapshot: crate::catalog_snapshot::CatalogSnapshot,
) -> Result<i32> {
    let status = issued.native_args(snapshot.path()).and_then(|args| {
        crate::launcher_label::run_with_account(
            program,
            &args,
            account_short(config, issued.account()),
        )
        .map_err(|_| refusal(NATIVE))
    });
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
    #[test]
    fn snapshot_is_released_on_argument_and_runner_errors() {
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
                format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: /trap\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
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
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 5);
        assert_eq!(value["model"], "synthetic");
        assert_eq!(value["model_is_explicit"], true);
        assert_eq!(value["account_pref"], "alpha");
        assert_eq!(value["resume_id"], UUID);
        assert_eq!(value["move_for_model"], true);
    }
}
