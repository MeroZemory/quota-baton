//! Staged pure argv planning only; no native, transport, config or authority I/O.
#[path = "../src/native_resume/plan.rs"]
mod plan;

use std::ffi::OsString;

use plan::{AttachError, Plan, from_issued_args};

fn argv(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn issued(tail: &[&str]) -> Vec<OsString> {
    let mut args = argv(&[
        "-c",
        "openai_base_url='http://127.0.0.1:10200/l/synthetic/v1'",
        "-c",
        "model_catalog_json='C:/owned/catalog with spaces.json'",
        "--model",
        "M",
    ]);
    args.extend(argv(tail));
    args
}

fn owned(args: &[OsString]) -> (Vec<OsString>, Vec<OsString>) {
    match from_issued_args(args) {
        Plan::OwnedRemoteTui {
            tui_args,
            server_args,
        } => (tui_args, server_args),
        Plan::ExistingPath => panic!("expected supported synthetic argv"),
    }
}

fn existing(args: &[OsString]) {
    assert!(matches!(from_issued_args(args), Plan::ExistingPath));
}

#[test]
fn golden_issued_config_order_model_features_and_strict() {
    let args = issued(&[
        "--config=model_reasoning_effort=low",
        "--disable=goals",
        "--enable",
        "goals",
        "-cmodel_reasoning_effort='high'",
        "-c",
        "features.goals=true",
        "--config",
        "tui.animations=false",
        "-ctui.show_tooltips=true",
        "--strict-config",
        "--no-alt-screen",
        "a prompt",
    ]);
    let (tui, server) = owned(&args);
    assert_eq!(tui, args);
    let mut expected = args[..4].to_vec();
    expected.extend(argv(&[
        "--config=model_reasoning_effort=low",
        "--disable=goals",
        "--enable",
        "goals",
        "-cmodel_reasoning_effort='high'",
        "-c",
        "features.goals=true",
        "--config",
        "tui.animations=false",
        "-ctui.show_tooltips=true",
        "app-server",
        "--listen",
        "stdio://",
        "--strict-config",
    ]));
    assert_eq!(server, expected, "copy spans without sorting or flattening");
}

#[test]
fn spelling_and_duplicate_config_spans_are_preserved() {
    let args = argv(&[
        "--config=openai_base_url='http://127.0.0.1:10200/exec/resume'",
        "-cmodel_catalog_json='C:/resume/exec.json'",
        "-mmodel-exec-resume",
        "-c=model_reasoning_effort=medium",
        "--model=other-model",
        "--config",
        "model='third-model'",
        "--strict-config",
        "--strict-config",
    ]);
    let (tui, server) = owned(&args);
    assert_eq!(tui, args);
    assert_eq!(
        server,
        argv(&[
            "--config=openai_base_url='http://127.0.0.1:10200/exec/resume'",
            "-cmodel_catalog_json='C:/resume/exec.json'",
            "-c=model_reasoning_effort=medium",
            "--config",
            "model='third-model'",
            "app-server",
            "--listen",
            "stdio://",
            "--strict-config",
        ])
    );
}

#[test]
fn operands_containing_verbs_are_opaque_and_delimiter_is_preserved() {
    for prompt in [
        "please resume exec --remote",
        "resume something",
        "",
        "한글 😀",
    ] {
        let args = issued(&[prompt]);
        assert_eq!(owned(&args).0, args);
    }
    for prompt in [
        "resume",
        "exec",
        "--remote",
        "--profile",
        "--strict-config",
        "-cunknown=1",
    ] {
        let args = issued(&["--", prompt]);
        let (tui, server) = owned(&args);
        assert_eq!(tui, args);
        let mut expected = args[..4].to_vec();
        expected.extend(argv(&["app-server", "--listen", "stdio://"]));
        assert_eq!(server, expected);
    }
    let args = issued(&["--"]);
    assert_eq!(owned(&args).0, args);
    existing(&issued(&["--", "one", "two"]));
    existing(&issued(&["prompt", "--strict-config"]));
}

#[test]
fn exec_resume_cwd_sandbox_profiles_and_other_modes_keep_existing_path() {
    for tail in [
        vec!["exec", "prompt"],
        vec!["e", "resume", "id"],
        vec!["resume", "id"],
        vec!["--resume", "id"],
        vec!["resume", "--last"],
        vec!["fork"],
        vec!["-C", "C:/owned"],
        vec!["--cd=C:/owned"],
        vec!["-s", "read-only"],
        vec!["--sandbox=workspace-write"],
        vec!["--profile", "work"],
        vec!["-pwork"],
        vec!["--ask-for-approval", "never"],
        vec!["--add-dir", "C:/other"],
        vec!["--search"],
        vec!["--worktree"],
        vec!["--no-daemon"],
        vec!["--oss"],
        vec!["--image", "picture.png"],
        vec!["--dangerously-bypass-approvals-and-sandbox"],
        vec!["--remote", "ws://127.0.0.1:1234"],
        vec!["--remote-auth-token-env", "EXISTING"],
        vec!["--json"],
        vec!["--help"],
        vec!["--version"],
        vec!["app-server"],
        vec!["help"],
    ] {
        existing(&issued(&tail));
    }
}

#[test]
fn unknown_config_and_value_shapes_never_activate_remote() {
    for config in [
        "unknown=1",
        "model_provider='other'",
        "profiles.work.model='M'",
        "profile='work'",
        "features.unknown=true",
        "features.goals='true'",
        "features.goals={ enabled=true }",
        "features={goals=true}",
        "tui.unknown=true",
        "tui.animations='false'",
        "tui.show_tooltips=1",
        "tui.resume_cwd='current'",
        "sandbox_workspace_write.writable_roots=['C:/other']",
        "sandbox_mode='danger-full-access'",
        "approval_policy='never'",
        "model=[]",
        "model=true",
        "model=123",
        "model=''",
        "model='M with spaces'",
        "model_reasoning_effort='ultra'",
        "'features.goals'=true",
        "features..goals=true",
        "features.goals=",
        "model='M'\nextra=true",
        "model='M'\rextra=true",
        "model",
        "=true",
        "openai_base_url='http://other'",
        "model_catalog_json='C:/other'",
    ] {
        existing(&issued(&["-c", config]));
    }
    for tail in [
        vec!["--enable", "unknown"],
        vec!["--disable="],
        vec!["--enable"],
        vec!["--config"],
        vec!["--model"],
        vec!["--model="],
        vec!["--unknown", "operand"],
        vec!["--strict-config=false"],
        vec!["--no-alt-screen=true"],
        vec!["-c", "--"],
    ] {
        existing(&issued(&tail));
    }
}

#[test]
fn issued_markers_are_required_but_not_claimed_as_authorization() {
    existing(&[]);
    existing(&argv(&["--model", "M", "prompt"]));
    existing(&argv(&[
        "-c",
        "openai_base_url='synthetic'",
        "--model",
        "M",
    ]));
    existing(&argv(&[
        "-c",
        "model_catalog_json='C:/synthetic'",
        "--model",
        "M",
    ]));
    let mut args = issued(&[]);
    args.truncate(4);
    existing(&args);
    args.extend(argv(&["-c", "model=M"]));
    assert_eq!(owned(&args).0, args);
}

#[test]
fn attachment_consumes_plan_and_only_adds_nonsecret_remote_options_to_tui() {
    let args = issued(&["--strict-config", "--", "resume"]);
    let (_, expected_server) = owned(&args);
    let attached = from_issued_args(&args)
        .attach_remote("ws://127.0.0.1:43210/qb-native", "QB_REMOTE_SYNTHETIC")
        .unwrap();
    let mut expected_tui = argv(&[
        "--remote",
        "ws://127.0.0.1:43210/qb-native",
        "--remote-auth-token-env",
        "QB_REMOTE_SYNTHETIC",
    ]);
    expected_tui.extend(args);
    assert_eq!(attached.tui_args, expected_tui);
    assert_eq!(attached.server_args, expected_server);
}

fn attach_error(result: Result<plan::AttachedRemoteTui, AttachError>) -> AttachError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected fixed attachment error"),
    }
}

#[test]
fn attachment_rejects_secret_urls_external_endpoints_and_invalid_env_names() {
    for url in [
        "ws://localhost:1234/qb-native",
        "wss://127.0.0.1:1234/qb-native",
        "ws://127.0.0.1:0/qb-native",
        "ws://127.0.0.1:65536/qb-native",
        "ws://user:secret@127.0.0.1:1234/qb-native",
        "ws://127.0.0.1:1234/secret",
        "ws://127.0.0.1:1234/qb-native?token=secret",
        "ws://127.0.0.1:1234/qb-native#secret",
        "ws://127.0.0.1:1234/qb-native\n",
    ] {
        assert_eq!(
            attach_error(from_issued_args(&issued(&[])).attach_remote(url, "QB_REMOTE")),
            AttachError::InvalidEndpoint
        );
    }
    for name in [
        "",
        "1NAME",
        "NAME=value",
        "NAME-with-dash",
        "NAME\n",
        "한글",
    ] {
        assert_eq!(
            attach_error(
                from_issued_args(&issued(&[])).attach_remote("ws://127.0.0.1:1/qb-native", name)
            ),
            AttachError::InvalidEnvironmentName
        );
    }
    assert_eq!(
        attach_error(Plan::ExistingPath.attach_remote("ws://127.0.0.1:1/qb-native", "QB_REMOTE")),
        AttachError::ExistingPath
    );
}

#[test]
fn oversized_and_nul_args_fall_back_without_payload_errors() {
    let mut args = issued(&[]);
    args.push(OsString::from("x".repeat(16 * 1024 + 1)));
    existing(&args);
    let mut args = issued(&[]);
    args.extend(std::iter::repeat_n(OsString::from("--no-alt-screen"), 257));
    existing(&args);
    let mut args = issued(&[]);
    args.push(OsString::from("private\0prompt"));
    existing(&args);
}

#[cfg(windows)]
#[test]
fn delimiter_preserves_non_unicode_prompt_osstring() {
    use std::os::windows::ffi::OsStringExt;
    let mut args = issued(&["--"]);
    args.push(OsString::from_wide(&[0xd800]));
    assert_eq!(owned(&args).0, args);
}
