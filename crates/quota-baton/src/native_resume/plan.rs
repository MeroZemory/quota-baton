//! Pure prelaunch classification of ALREADY ISSUED argv. No runtime activation,
//! config reads, credential validation or reset authorization. ExistingPath means
//! preserve the existing runner/refusal, never bypass managed launch validation.
//!
//! Source: local Codex 092d3acd6bec3e3a14bdc7e7a2810ab628ab759d (0.162.1):
//! cli/src/main.rs:129-145, 146-255, 571-605, 976-991, 1237-1302;
//! utils/cli/src/config_override.rs:25-89; tui/src/cli.rs:12-22, 75-85;
//! config/src/types.rs:831,847; features/src/lib.rs:1824. App-server takes root
//! -c/feature overrides, NOT arbitrary TUI flags. Its strict flag is separate.
//! This source pin is not installed 0.163 parity or transport-delivery evidence.

use std::ffi::OsString;

use toml_edit::DocumentMut;

const MAX_ARGS: usize = 256;
const MAX_TOKEN: usize = 16 * 1024;
const MAX_TOTAL: usize = 64 * 1024;

/// No Debug/serialization: argument values may contain private user content.
pub(crate) enum Plan {
    ExistingPath,
    OwnedRemoteTui {
        tui_args: Vec<OsString>,
        server_args: Vec<OsString>,
    },
}

/// Consuming attachment gives one configured argv pair, without bearer values.
pub(crate) struct AttachedRemoteTui {
    pub(crate) tui_args: Vec<OsString>,
    pub(crate) server_args: Vec<OsString>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttachError {
    ExistingPath,
    InvalidEndpoint,
    InvalidEnvironmentName,
}

impl Plan {
    /// Accept only the generated nonsecret numeric-loopback endpoint. The parent
    /// owns listener binding, token entropy, environment isolation and auth. Never
    /// supply a URL-path/query secret here; bearer values are not API inputs.
    pub(crate) fn attach_remote(
        self,
        url: &str,
        env_name: &str,
    ) -> Result<AttachedRemoteTui, AttachError> {
        let Self::OwnedRemoteTui {
            tui_args,
            server_args,
        } = self
        else {
            return Err(AttachError::ExistingPath);
        };
        let port = url
            .strip_prefix("ws://127.0.0.1:")
            .and_then(|s| s.strip_suffix("/qb-native"))
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|p| *p != 0);
        if port.is_none() || url.len() > 64 {
            return Err(AttachError::InvalidEndpoint);
        }
        if env_name.is_empty()
            || env_name.len() > 128
            || !env_name
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
        {
            return Err(AttachError::InvalidEnvironmentName);
        }
        let mut attached = Vec::with_capacity(tui_args.len() + 4);
        attached.extend([
            OsString::from("--remote"),
            OsString::from(url),
            OsString::from("--remote-auth-token-env"),
            OsString::from(env_name),
        ]);
        attached.extend(tui_args);
        Ok(AttachedRemoteTui {
            tui_args: attached,
            server_args,
        })
    }
}

/// Conservative new-session subset. The original TUI token vector is cloned
/// byte-for-byte; server token spans are copied, never reserialized or reordered.
/// A prompt is one opaque positional token, optionally after `--`. Options after
/// that prompt fall back rather than guessing native positional/option grammar.
pub(crate) fn from_issued_args(args: &[OsString]) -> Plan {
    build(args).unwrap_or(Plan::ExistingPath)
}

fn build(args: &[OsString]) -> Option<Plan> {
    if args.len() > MAX_ARGS {
        return None;
    }
    let mut total = 0usize;
    for arg in args {
        let len = arg.as_encoded_bytes().len();
        if len > MAX_TOKEN || arg.as_encoded_bytes().contains(&0) {
            return None;
        }
        total = total.checked_add(len)?;
        if total > MAX_TOTAL {
            return None;
        }
    }
    let mut server_args = Vec::new();
    let (mut route, mut catalog, mut model, mut strict) = (false, false, false, false);
    let mut index = 0;
    while index < args.len() {
        let start = index;
        let token = args[index].to_str()?;
        index += 1;
        if token == "--" {
            // Nothing after this delimiter is interpreted as an option or verb.
            if args.len() - index > 1 {
                return None;
            }
            break;
        }
        if !token.starts_with('-') || token == "-" {
            if subcommand(token) || index != args.len() {
                return None;
            }
            break;
        }
        if matches!(token, "--no-alt-screen" | "--strict-config") {
            strict |= token == "--strict-config";
            continue;
        }
        let (name, attached) = option(token)?;
        let value = match attached {
            Some(value) => value,
            None => {
                let value = args.get(index)?.to_str()?;
                if value.starts_with('-') {
                    return None;
                }
                index += 1;
                value
            }
        };
        match name {
            "-c" | "--config" => {
                let kind = config_kind(value)?;
                match kind {
                    ConfigKind::Route if !route => route = true,
                    ConfigKind::Catalog if !catalog => catalog = true,
                    ConfigKind::Model => model = true,
                    ConfigKind::Ordinary => {}
                    _ => return None,
                }
                server_args.extend_from_slice(&args[start..index]);
            }
            "-m" | "--model" => {
                if !text_id(value) {
                    return None;
                }
                model = true;
            }
            "--enable" | "--disable" => {
                // Deliberately only this source-confirmed feature in v1. Native
                // folds enable then disable; retaining tokens preserves that rule.
                if value != "goals" {
                    return None;
                }
                server_args.extend_from_slice(&args[start..index]);
            }
            _ => return None,
        }
    }
    if !route || !catalog || !model {
        return None;
    }
    server_args.extend(["app-server".into(), "--listen".into(), "stdio://".into()]);
    if strict {
        server_args.push("--strict-config".into());
    }
    Some(Plan::OwnedRemoteTui {
        tui_args: args.to_vec(),
        server_args,
    })
}

fn option(token: &str) -> Option<(&str, Option<&str>)> {
    for long in ["--config", "--model", "--enable", "--disable"] {
        if token == long {
            return Some((long, None));
        }
        if let Some(value) = token.strip_prefix(long).and_then(|s| s.strip_prefix('=')) {
            return Some((long, Some(value)));
        }
    }
    for short in ["-c", "-m"] {
        if let Some(value) = token.strip_prefix(short) {
            return Some((
                short,
                if value.is_empty() {
                    None
                } else {
                    Some(value.strip_prefix('=').unwrap_or(value))
                },
            ));
        }
    }
    None
}

enum ConfigKind {
    Route,
    Catalog,
    Model,
    Ordinary,
}

fn config_kind(raw: &str) -> Option<ConfigKind> {
    let (key, value) = raw.split_once('=')?;
    let (key, value) = (key.trim(), value.trim());
    // Parse only the bounded value, never quoted/dynamic keys or extra lines.
    if raw.contains(['\n', '\r']) {
        return None;
    }
    let doc = format!("v = {value}").parse::<DocumentMut>().ok();
    let item = doc
        .as_ref()
        .filter(|d| d.len() == 1)
        .and_then(|d| d.get("v"));
    let string = item.and_then(|i| i.as_str()).or_else(|| {
        // Native accepts bare strings after TOML failure. Keep this subset small;
        // malformed quoted values, inline comments and assignments cannot enter.
        (doc.is_none() && text_id(value)).then_some(value)
    });
    match key {
        "openai_base_url"
            if string.is_some_and(|s| !s.is_empty() && !s.chars().any(char::is_control)) =>
        {
            Some(ConfigKind::Route)
        }
        "model_catalog_json"
            if string.is_some_and(|s| !s.is_empty() && !s.chars().any(char::is_control)) =>
        {
            Some(ConfigKind::Catalog)
        }
        "model" if string.is_some_and(text_id) => Some(ConfigKind::Model),
        "model_reasoning_effort"
            if string.is_some_and(|s| {
                matches!(s, "none" | "minimal" | "low" | "medium" | "high" | "xhigh")
            }) =>
        {
            Some(ConfigKind::Ordinary)
        }
        "features.goals" | "tui.animations" | "tui.show_tooltips"
            if item.and_then(|i| i.as_bool()).is_some() =>
        {
            Some(ConfigKind::Ordinary)
        }
        _ => None,
    }
}

fn text_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

fn subcommand(value: &str) -> bool {
    matches!(
        value,
        "agents"
            | "tcp-tunnel"
            | "exec"
            | "e"
            | "review"
            | "login"
            | "logout"
            | "mcp"
            | "plugin"
            | "app-server"
            | "remote-control"
            | "app"
            | "completion"
            | "update"
            | "doctor"
            | "sandbox"
            | "debug"
            | "execpolicy"
            | "apply"
            | "a"
            | "resume"
            | "queue"
            | "archive"
            | "delete"
            | "migrate-rollouts"
            | "unarchive"
            | "fork"
            | "cloud"
            | "cloud-tasks"
            | "responses-api-proxy"
            | "stdio-to-uds"
            | "exec-server"
            | "features"
            | "help"
    )
}
