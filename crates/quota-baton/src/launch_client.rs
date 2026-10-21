//! R11 launch planning only; admission/import and process lifetime stay with callers.
//!
//! Grammar: interactive, `resume [ID]`, `exec|e [resume [ID]]`; known native
//! option operands and everything after `--` are opaque. QB flags are accepted
//! before the first prompt, including after a resume ID. Unknown option arity,
//! profiles, OSS and caller routing overrides are refused before issuance.
//! Model precedence is explicit -m, one -c model assignment, then home model.
//! Value-option arities are listed in `takes_value`; short clusters other than
//! a single valued short option are unsupported. Profiles are never merged.
//! Limits: 4096 arguments, 256-byte model/account text, 1-MiB home config,
//! 16-KiB per config override/admin reply, ten seconds per admin request.
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
/// The coordinator must validate dedicated wiring and hold R9 admission itself.
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
fn takes_value(name: &str) -> bool {
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

fn switch(name: &str) -> bool {
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
}

/// Called only after R9 admission/import; returns args for the existing runner.
/// Never auto-starts a core, creates secrets, or changes persistent config.
pub(crate) async fn issue_and_override(
    plan: &LaunchPlan,
    qb_home: &Path,
    config: &qb_core::config::Config,
) -> Result<Vec<OsString>> {
    if plan.passthrough {
        return Ok(plan.native_args.clone());
    }
    let model = plan
        .effective_model
        .as_deref()
        .filter(|v| text_id(v))
        .ok_or_else(|| refusal(GRAMMAR))?;
    let secrets = qb_core::local_secrets::load_existing(qb_home).map_err(|_| refusal(CORE))?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DEADLINE)
        .build()
        .map_err(|_| refusal(CORE))?;
    let request = LaunchRequest {
        model,
        model_is_explicit: plan.model_is_explicit,
        account_pref: plan.account_pref.as_deref(),
        resume_id: plan.resume_id.as_deref(),
        move_for_model: plan.move_for_model,
    };
    let mut response = client
        .post(format!(
            "http://127.0.0.1:{}/admin/v1/launches",
            config.server.admin_port
        ))
        .bearer_auth(&secrets.admin_token)
        .json(&request)
        .send()
        .await
        .map_err(|_| refusal(CORE))?;
    if !response.status().is_success() {
        return Err(refusal(CORE));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_REPLY_BYTES as u64)
    {
        return Err(refusal(REPLY));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| refusal(CORE))? {
        if chunk.len() > MAX_REPLY_BYTES - bytes.len() {
            return Err(refusal(REPLY));
        }
        bytes.extend_from_slice(&chunk);
    }
    let reply: LaunchReply = serde_json::from_slice(&bytes).map_err(|_| refusal(REPLY))?;
    overrides(plan, reply, config.server.data_port)
}

fn overrides(plan: &LaunchPlan, reply: LaunchReply, data_port: u16) -> Result<Vec<OsString>> {
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
    if data_port == 0
        || token.is_empty()
        || reply.catalog_url != format!("{}/models", reply.base_url)
        || !account_name(&reply.account)
    {
        return Err(refusal(REPLY));
    }
    let mut args = Vec::with_capacity(plan.native_args.len() + 4);
    for (key, value) in [
        ("openai_base_url", reply.base_url),
        ("model_catalog_url", reply.catalog_url),
    ] {
        // TOML encoding, not shell quoting. Values are separate Command args.
        let quoted = toml_edit::Value::from(value).to_string();
        args.push("-c".into());
        args.push(format!("{key}={quoted}").into());
    }
    args.extend_from_slice(&plan.native_args);
    Ok(args)
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
        }
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
        }
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
        assert_eq!(&args[4..], plan.native_args.as_slice());
        for (offset, key) in [(0, "openai_base_url"), (2, "model_catalog_url")] {
            assert_eq!(args[offset], OsString::from("-c"));
            let doc = args[offset + 1]
                .to_str()
                .unwrap()
                .parse::<DocumentMut>()
                .unwrap();
            assert!(doc[key].as_str().unwrap().contains(TOKEN));
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
    async fn passthrough_issue_never_loads_secrets_or_contacts_a_core() {
        let plan = inspect(&argv(&["--help"])).unwrap().plan;
        let args = issue_and_override(
            &plan,
            Path::new("must-not-be-opened"),
            &qb_core::config::Config::default(),
        )
        .await
        .unwrap();
        assert_eq!(args, argv(&["--help"]));
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
        })
        .to_string();
        for (expected_ok, status, body) in [
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
        ] {
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
            let result = issue_and_override(&plan, home.path(), &config).await;
            server.abort();
            let _ = server.await;
            assert_eq!(seen.load(Ordering::SeqCst), 1);
            assert_eq!(result.is_ok(), expected_ok);
            if expected_ok {
                assert_eq!(&result.unwrap()[4..], plan.native_args.as_slice());
            } else {
                let error = result.unwrap_err();
                assert!(!format!("{error:?}").contains(TOKEN));
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
