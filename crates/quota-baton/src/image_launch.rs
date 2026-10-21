//! D23 image mode is explicit and local to the managed child process.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) const KEY_ENV: &str = "QB_IMAGE_CLIENT_KEY";
const EXISTING_SESSION: &str =
    "기존 대화는 이미지 모드로 옮길 수 없습니다. 새 이미지 세션을 여세요";

#[derive(Debug)]
struct ExistingSessionRefusal;
impl std::fmt::Display for ExistingSessionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(EXISTING_SESSION)
    }
}
impl std::error::Error for ExistingSessionRefusal {}

/// Only a leading launcher flag is ours. A prompt, option operand, or text
/// after the native literal boundary must never become a launcher option.
pub(crate) fn split(args: &[OsString]) -> (bool, &[OsString]) {
    if args.first().is_some_and(|arg| arg == "--images") {
        (true, &args[1..])
    } else {
        (false, args)
    }
}

/// Qi admits new sessions only. Check before loading credentials, importing a
/// thread, or running the canary. Native option operands and literal prompts
/// are data, not commands. `exec` (alias `e`) has its own resume/fork commands.
/// Provider overrides must not redirect the child-only local client key.
pub(crate) fn validate_args(args: &[OsString]) -> Result<()> {
    let mut exec = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let arg = arg.to_string_lossy();
        let name = arg.split_once('=').map_or(arg.as_ref(), |(name, _)| name);
        if matches!(
            name,
            "--last"
                | "--resume"
                | "--fork"
                | "--resume-picker"
                | "--fork-picker"
                | "--resume-last"
                | "--fork-last"
                | "--resume-session-id"
                | "--fork-session-id"
                | "--remote"
                | "--remote-auth-token-env"
        ) || matches!(arg.as_ref(), "resume" | "fork")
            || (!exec
                && matches!(
                    arg.as_ref(),
                    "agents" | "daemon" | "app-server" | "app" | "remote-control" | "debug"
                ))
        {
            return Err(anyhow::Error::new(ExistingSessionRefusal));
        }
        ensure!(
            !(arg == "--config"
                || arg.starts_with("--config=")
                || arg.starts_with("-c")
                || arg == "--profile"
                || arg.starts_with("--profile=")
                || arg.starts_with("-p")
                || arg == "--oss"
                || arg == "--local-provider"
                || arg.starts_with("--local-provider=")),
            "이미지 실행에서는 제공자를 바꾸는 config/profile 옵션을 사용할 수 없습니다."
        );
        if matches!(arg.as_ref(), "exec" | "e") {
            exec = true;
        }
        // These native options take a separate operand unless supplied inline.
        // Attached short values (-mMODEL, -iFILE, etc.) need no extra skip.
        if matches!(
            arg.as_ref(),
            "--model"
                | "-m"
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
                | "--output-last-message"
                | "-o"
                | "--output-schema"
                | "--color"
                | "--thread-source"
        ) {
            args.next();
        }
    }
    Ok(())
}

/// Deliberately no Debug: the client key belongs only in the child environment.
pub(crate) struct ImageLaunch {
    pub(crate) home: PathBuf,
    pub(crate) ordinary_home: PathBuf,
    key: zeroize::Zeroizing<String>,
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "이미지 실행 설정이 너무 큽니다"
    );
    Ok(bytes)
}

impl ImageLaunch {
    pub(crate) fn load(qb_home: &Path) -> Result<Self> {
        Self::load_inner(qb_home).map_err(|_| anyhow::Error::new(ImageRefusal))
    }

    fn load_inner(qb_home: &Path) -> Result<Self> {
        let journal = qb_home.join("journal/wiring.json");
        crate::sessions_guard::safe_path(qb_home, &journal)?;
        let value: serde_json::Value =
            serde_json::from_slice(&read_bounded(&journal, 16 * 1024 * 1024)?)
                .map_err(|_| anyhow::anyhow!("이미지 전용 홈 연결 기록을 읽지 못했습니다"))?;
        ensure!(
            crate::sessions_admission::completed_wiring(&value),
            "전용 Codex 홈 연결을 먼저 완료하세요"
        );
        let ordinary_home =
            PathBuf::from(value["home"].as_str().context("전용 홈 경로가 없습니다")?);
        let home = PathBuf::from(
            value["image_home"]
                .as_str()
                .context("이미지 전용 홈이 없습니다. wire 업그레이드를 먼저 실행하세요")?,
        );
        ensure!(
            home.is_absolute() && home.is_dir() && ordinary_home.is_absolute(),
            "이미지 전용 홈 연결이 올바르지 않습니다"
        );
        crate::sessions_guard::safe_path(&home, &home)?;
        ensure!(
            std::fs::canonicalize(&home)? != std::fs::canonicalize(&ordinary_home)?,
            "이미지 홈은 일반 홈과 달라야 합니다"
        );
        let expected = ordinary_home
            .parent()
            .context("전용 홈 부모 경로가 없습니다")?
            .join("qb-images");
        ensure!(
            std::fs::canonicalize(&home)? == std::fs::canonicalize(expected)?,
            "이미지 홈 경로가 연결 기록과 다릅니다"
        );
        let entries = value["entries"]
            .as_array()
            .context("이미지 홈 소유권 기록이 없습니다")?;
        for required in [home.clone(), home.join("config.toml"), home.join("log")] {
            let required = std::fs::canonicalize(required)?;
            ensure!(
                entries
                    .iter()
                    .any(|entry| entry["path"]
                        .as_str()
                        .is_some_and(
                            |path| std::fs::canonicalize(path).ok().as_ref() == Some(&required)
                        )
                        && entry["done"] == true
                        && entry["removed"] != true),
                "이미지 홈 게시가 완료되지 않았습니다"
            );
        }
        let config = home.join("config.toml");
        crate::sessions_guard::safe_path(&home, &config)?;
        let config: toml_edit::DocumentMut =
            std::str::from_utf8(&read_bounded(&config, 1024 * 1024)?)?
                .parse()
                .map_err(|_| anyhow::anyhow!("이미지 전용 홈 설정을 읽지 못했습니다"))?;
        let provider = config
            .get("model_providers")
            .and_then(|providers| providers.get("qb"))
            .context("이미지 공급자 설정이 없습니다")?;
        let local_base = format!(
            "http://127.0.0.1:{}/v1",
            qb_core::config::Config::load(qb_home)?.server.data_port
        );
        ensure!(
            config
                .get("model_provider")
                .and_then(toml_edit::Item::as_str)
                == Some("qb")
                && provider.get("env_key").and_then(toml_edit::Item::as_str) == Some(KEY_ENV)
                && provider.get("base_url").and_then(toml_edit::Item::as_str)
                    == Some(local_base.as_str())
                && provider
                    .get("requires_openai_auth")
                    .and_then(toml_edit::Item::as_bool)
                    == Some(false)
                && provider
                    .get("http_headers")
                    .and_then(|headers| headers.get("x-openai-actor-authorization"))
                    .and_then(toml_edit::Item::as_str)
                    == Some("qb-local"),
            "이미지 전용 공급자 설정이 올바르지 않습니다. wire 업그레이드를 확인하세요"
        );
        let key_path = qb_home.join("client.key");
        crate::sessions_guard::safe_path(qb_home, &key_path)?;
        let bytes = zeroize::Zeroizing::new(read_bounded(&key_path, 4096)?);
        let key = zeroize::Zeroizing::new(std::str::from_utf8(&bytes)?.trim().to_owned());
        ensure!(
            !key.is_empty() && !key.contains(['\r', '\n']),
            "로컬 클라이언트 키를 확인하지 못했습니다"
        );
        Ok(Self {
            home,
            ordinary_home,
            key,
        })
    }

    pub(crate) fn environment(&self) -> [(&str, &std::ffi::OsStr); 2] {
        [
            ("CODEX_HOME", self.home.as_os_str()),
            (KEY_ENV, std::ffi::OsStr::new(self.key.as_str())),
        ]
    }
}

#[derive(Debug)]
struct ImageRefusal;
impl std::fmt::Display for ImageRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("이미지 전용 홈 연결을 확인하지 못했습니다. wire 업그레이드와 이미지 설정을 확인하세요.")
    }
}
impl std::error::Error for ImageRefusal {}
pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    if error.downcast_ref::<ExistingSessionRefusal>().is_some() {
        return Some(EXISTING_SESSION.to_owned());
    }
    error
        .downcast_ref::<ImageRefusal>()
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn image_flag_is_leading_and_never_a_prompt_or_operand() {
        for values in [
            vec!["exec", "--images"],
            vec!["--", "--images"],
            vec!["-m", "--images"],
        ] {
            let args = args(&values);
            let (images, remaining) = split(&args);
            assert!(!images);
            assert_eq!(remaining, args);
        }
        let args = args(&["--images", "resume", "fixture"]);
        let (images, remaining) = split(&args);
        assert!(images);
        assert_eq!(remaining, &args[1..]);
    }

    #[test]
    fn r12_existing_image_sessions_refuse_before_wiring_or_admission() {
        // Deliberately nonexistent wiring/command: the new-session refusal
        // must win without reading configuration or acquiring any home gate.
        let image = ImageLaunch {
            home: PathBuf::from("missing-image-home"),
            ordinary_home: PathBuf::from("missing-ordinary-home"),
            key: zeroize::Zeroizing::new(String::new()),
        };
        let command = Path::new("missing-codex");
        for values in [
            vec!["resume"],
            vec!["resume", "11111111-1111-1111-1111-111111111111"],
            vec!["resume", "thread-name"],
            vec!["resume", "--last"],
            vec!["resume", "--all"],
            vec!["resume", "--", "thread-name"],
            vec!["fork"],
            vec!["fork", "--last"],
            vec!["exec", "resume", "--last"],
            vec!["e", "--model", "model", "resume", "thread-name"],
            vec!["exec", "fork", "thread-name"],
            vec!["--last"],
            vec!["--resume-picker"],
            vec!["--fork-picker"],
            vec!["--cd", "project", "resume"],
            vec!["daemon"],
            vec!["app-server", "daemon", "start"],
            vec!["agents"],
            vec!["--remote=unix://codex.sock"],
        ] {
            let input = args(&values);
            for error in [
                validate_args(&input).unwrap_err(),
                crate::sessions_admission::preflight_images(&input, command, &image).unwrap_err(),
                crate::sessions_admission::prepare_images(&input, command, &image)
                    .err()
                    .expect("existing session admitted"),
            ] {
                assert_eq!(
                    friendly_error(&error).as_deref(),
                    Some("기존 대화는 이미지 모드로 옮길 수 없습니다. 새 이미지 세션을 여세요"),
                    "{values:?}"
                );
            }
        }
    }

    #[test]
    fn r12_new_image_prompts_operands_and_literal_boundary_are_preserved() {
        for values in [
            vec![],
            vec!["draw an image"],
            vec!["exec", "draw an image"],
            vec!["e", "--full-auto", "draw an image"],
            vec!["exec", "daemon"],
            vec!["--model", "resume", "draw an image"],
            vec!["--cd", "fork", "exec", "draw an image"],
            vec!["-mresume", "exec", "-o", "app-server", "draw an image"],
            vec!["--image", "resume", "--", "draw an image"],
            vec!["--", "resume", "--last", "--config=literal"],
            vec!["exec", "--", "resume", "fork", "--last"],
        ] {
            let input = args(&values);
            let original = input.clone();
            validate_args(&input).unwrap_or_else(|error| panic!("{values:?}: {error}"));
            assert_eq!(input, original);
        }
    }

    #[test]
    fn r12_image_mode_refuses_provider_overrides_but_preserves_literal_prompt() {
        for values in [
            vec![
                "-c",
                "model_providers.qb.base_url='https://example.invalid'",
            ],
            vec!["exec", "--config=model_provider='other'"],
            vec!["resume", "-pother"],
            vec!["--oss"],
        ] {
            assert!(validate_args(&args(&values)).is_err());
        }
        assert!(
            validate_args(&args(&[
                "-m",
                "gpt-6.1-sol",
                "exec",
                "--",
                "--config=literal prompt"
            ]))
            .is_ok()
        );
    }

    #[test]
    fn r12_image_launch_refuses_unpublished_or_remote_or_missing_provider_without_leaking_key() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r12-launch-tests");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let qb = root.join("core");
        let ordinary = root.join("qb");
        let image = root.join("qb-images");
        for path in [qb.join("journal"), ordinary.clone(), image.join("log")] {
            std::fs::create_dir_all(path).unwrap();
        }
        let key = "synthetic-r12-local-key-never-in-error";
        std::fs::write(qb.join("client.key"), key).unwrap();
        let valid = "model_provider='qb'\n[model_providers.qb]\nbase_url='http://127.0.0.1:10200/v1'\nenv_key='QB_IMAGE_CLIENT_KEY'\nrequires_openai_auth=false\n[model_providers.qb.http_headers]\nx-openai-actor-authorization='qb-local'\n";
        std::fs::write(image.join("config.toml"), valid).unwrap();
        let mut manifest = serde_json::json!({"schema":3,"mode":"dedicated","install_completed":true,"home":ordinary,"image_home":image,"entries":[
            {"path":image,"done":true},{"path":image.join("log"),"done":true},{"path":image.join("config.toml"),"done":true}
        ]});
        std::fs::write(
            qb.join("journal/wiring.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let existed = std::env::var_os(KEY_ENV).is_some();
        let context = ImageLaunch::load(&qb).unwrap();
        assert_eq!(context.environment()[1].1, std::ffi::OsStr::new(key));
        assert_eq!(std::env::var_os(KEY_ENV).is_some(), existed);
        for invalid in [
            "",
            "model_provider='qb'\n",
            &valid.replace("127.0.0.1:10200", "example.invalid:10200"),
        ] {
            std::fs::write(image.join("config.toml"), invalid).unwrap();
            let error = ImageLaunch::load(&qb)
                .err()
                .expect("invalid config accepted");
            assert!(!format!("{error:#}").contains(key));
        }
        std::fs::write(image.join("config.toml"), valid).unwrap();
        manifest["entries"][2]["done"] = false.into();
        std::fs::write(
            qb.join("journal/wiring.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(ImageLaunch::load(&qb).is_err());
    }
}
