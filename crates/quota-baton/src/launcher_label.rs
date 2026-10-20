//! Process-local launcher label. No settings file or argument contains the label.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::path::Path;

const NAME: &str = "x-quota-baton-label";
const MAX_HEADERS: usize = 64 * 1024;

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Client {
    Claude,
    Codex,
}

/// Encoding is deliberately ASCII: Node's header APIs reject raw Hangul/emoji.
pub fn folder_value(cwd: &Path) -> Option<String> {
    let folder: String = cwd
        .file_name()?
        .to_str()?
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    if folder.trim().is_empty() || folder == "." || folder == ".." {
        return None;
    }
    Some(format!("qb1:{}", URL_SAFE_NO_PAD.encode(folder.as_bytes())))
}

/// Keep unrelated lines byte-for-byte; replace only the header addressed to us.
/// Oversized inherited state stays the client's responsibility: omit our label
/// rather than truncating or logging somebody else's headers.
pub fn merge_headers(existing: &str, value: &str) -> Option<String> {
    if existing.len() > MAX_HEADERS || !value.is_ascii() || value.contains(['\r', '\n']) {
        return None;
    }
    let mut lines: Vec<_> = existing
        .split('\n')
        .filter(|line| {
            !line
                .split_once(':')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case(NAME))
        })
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut merged = lines.join("\n");
    if !merged.is_empty() {
        merged.push('\n');
    }
    merged.push_str(NAME);
    merged.push_str(": ");
    merged.push_str(value);
    (merged.len() <= MAX_HEADERS).then_some(merged)
}

/// The generated batch launcher delegates only its final child spawn here.
/// Keep the dedicated settings file unchanged and out of per-launch scratch.
/// Client settings can override this environment; the core's filename lookup
/// remains the fallback when no launcher header reaches it.
pub fn run(
    client: Client,
    program: &Path,
    settings: Option<&Path>,
    args: &[std::ffi::OsString],
) -> std::io::Result<i32> {
    if !program.is_absolute()
        || settings.is_some_and(|p| !p.is_absolute())
        || matches!(client, Client::Claude) && settings.is_none()
        || matches!(client, Client::Codex) && settings.is_some()
    {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut child = client_command(client, program);
    if let Some(settings) = settings {
        child.arg("--settings").arg(settings);
    }
    child.args(args);
    let headers = std::env::var_os("ANTHROPIC_CUSTOM_HEADERS");
    if matches!(client, Client::Claude)
        && let Some(value) = folder_value(&std::env::current_dir()?)
    {
        let existing = headers
            .as_deref()
            .map(std::ffi::OsStr::to_str)
            .unwrap_or(Some(""));
        if let Some(merged) = existing.and_then(|existing| merge_headers(existing, &value)) {
            child.env("ANTHROPIC_CUSTOM_HEADERS", merged);
        }
    }
    // Rust applies its batch-specific escaping for .cmd/.bat. Do not change
    // this into an interpolated `cmd /c` string or CALL reparse.
    #[cfg(windows)]
    let title = matches!(client, Client::Codex)
        .then(|| ConsoleTitle::new(program))
        .transpose()?;
    #[cfg(windows)]
    let needs_title_repair = child.get_program() == program.as_os_str()
        && program
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
    let mut child = child.spawn()?;
    #[cfg(windows)]
    let repair = title
        .filter(|_| needs_title_repair)
        .map(ConsoleTitle::watch);
    let status = child.wait();
    #[cfg(windows)]
    if let Some((stop, worker)) = repair {
        let _ = stop.send(());
        let _ = worker.join();
    }
    Ok(status?.code().unwrap_or(1))
}

fn client_command(client: Client, program: &Path) -> std::process::Command {
    if matches!(client, Client::Codex)
        && let Some((node, script)) = resolve_codex_node(program)
    {
        let mut command = std::process::Command::new(node);
        command.arg(script);
        return command;
    }
    std::process::Command::new(program)
}

fn resolve_codex_node(program: &Path) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    if !program
        .file_name()?
        .to_str()?
        .eq_ignore_ascii_case("codex.cmd")
    {
        return None;
    }
    let npm = program.parent()?;
    let script = npm.join("node_modules/@openai/codex/bin/codex.js");
    if !script.is_file() {
        return None;
    }
    let local = npm.join("node.exe");
    let node = if local.is_file() {
        local
    } else {
        unambiguous_path_node(
            &std::env::var_os("PATH")?,
            &std::env::current_dir().ok()?,
            &std::env::var_os("PATHEXT")?,
        )?
    };
    Some((node, script))
}

// cmd also searches the current directory and accepts relative PATH entries.
// If those could affect resolution, preserve npm's own lookup via the fallback.
fn unambiguous_path_node(
    path: &std::ffi::OsStr,
    cwd: &Path,
    pathext: &std::ffi::OsStr,
) -> Option<std::path::PathBuf> {
    // npm removes ;.JS; from PATHEXT before invoking `node`.
    let pathext = pathext.to_str()?.replace(";.JS;", ";");
    let extensions: Vec<_> = pathext.split(';').map(str::to_ascii_lowercase).collect();
    if extensions.is_empty()
        || extensions.iter().any(|ext| {
            !ext.starts_with('.') || !ext[1..].bytes().all(|b| b.is_ascii_alphanumeric())
        })
    {
        return None;
    }
    let has_candidate = |dir: &Path| {
        dir.join("node").is_file()
            || extensions
                .iter()
                .any(|ext| dir.join(format!("node{ext}")).is_file())
    };
    if has_candidate(cwd) {
        return None;
    }
    for dir in std::env::split_paths(path) {
        if !dir.is_absolute() || dir.join("node").is_file() {
            return None;
        }
        for extension in &extensions {
            let candidate = dir.join(format!("node{extension}"));
            if candidate.is_file() {
                return (extension == ".exe").then_some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
struct ConsoleTitle {
    desired: String,
    cmd_titles: Vec<String>,
    program_title: String,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleTitleW(title: *const u16) -> i32;
    fn GetConsoleTitleW(title: *mut u16, size: u32) -> u32;
}

#[cfg(windows)]
fn console_title() -> String {
    let mut buffer = vec![0u16; 32768];
    let n = unsafe { GetConsoleTitleW(buffer.as_mut_ptr(), buffer.len() as u32) };
    String::from_utf16_lossy(&buffer[..n as usize])
}

#[cfg(windows)]
fn set_console_title(title: &str) {
    let title: Vec<_> = title.encode_utf16().chain(Some(0)).collect();
    // A redirected/non-console invocation has no title; that is not a launch failure.
    unsafe {
        SetConsoleTitleW(title.as_ptr());
    }
}

#[cfg(windows)]
impl ConsoleTitle {
    fn new(program: &Path) -> std::io::Result<Self> {
        let cwd = std::env::current_dir()?;
        let folder: String = cwd
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let desired = format!("codex-qb · {folder}");
        set_console_title(&desired);
        // Exact known shell/program titles only: never fight a client's own title.
        let program_title = program.to_string_lossy().into_owned();
        let mut cmd_titles = Vec::new();
        if let Some(root) = std::env::var_os("SystemRoot") {
            cmd_titles.push(
                Path::new(&root)
                    .join("System32/cmd.exe")
                    .to_string_lossy()
                    .replace('/', "\\"),
            );
        }
        if let Some(cmd) = std::env::var_os("ComSpec") {
            cmd_titles.push(cmd.to_string_lossy().into_owned());
        }
        Ok(Self {
            desired,
            cmd_titles,
            program_title,
        })
    }

    fn watch(self) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (stop, stopped) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // Bound fallback repair to the first twenty seconds. Exit promptly
            // with the child, and stop permanently at a client's own title.
            for delay in std::iter::once(300)
                .chain(std::iter::once(700))
                .chain(std::iter::repeat_n(1000, 19))
            {
                if !matches!(
                    stopped.recv_timeout(std::time::Duration::from_millis(delay)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
                let current = console_title();
                if current
                    .trim()
                    .eq_ignore_ascii_case(self.program_title.trim())
                    || is_cmd_title(&current, &self.cmd_titles)
                {
                    set_console_title(&self.desired);
                } else if current != self.desired {
                    break;
                }
            }
        });
        (stop, worker)
    }
}

#[cfg(windows)]
fn is_cmd_title(current: &str, cmd_titles: &[String]) -> bool {
    let current = current.trim().to_ascii_lowercase();
    cmd_titles.iter().any(|title| {
        let title = title.trim().to_ascii_lowercase();
        current == title
            || current
                .strip_prefix(&title)
                .is_some_and(|rest| rest.starts_with(" - "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn r81_ambiguous_relative_or_current_directory_node_uses_shim_fallback() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r81-path-resolution");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let cwd = dir.path().join("cwd");
        let absolute = dir.path().join("node-b");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&absolute).unwrap();
        std::fs::write(absolute.join("node.exe"), "synthetic, never executed").unwrap();
        let path = std::env::join_paths([Path::new("node-a"), absolute.as_path()]).unwrap();
        assert!(
            unambiguous_path_node(&path, &cwd, std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD"))
                .is_none()
        );
        let path = std::env::join_paths([absolute.as_path()]).unwrap();
        assert_eq!(
            unambiguous_path_node(&path, &cwd, std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD")),
            Some(absolute.join("node.exe"))
        );
        std::fs::write(cwd.join("node.cmd"), "synthetic wrapper, never executed").unwrap();
        assert!(
            unambiguous_path_node(&path, &cwd, std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD"))
                .is_none()
        );
        std::fs::remove_file(cwd.join("node.cmd")).unwrap();
        std::fs::write(cwd.join("node.exe"), "synthetic, never executed").unwrap();
        assert!(
            unambiguous_path_node(&path, &cwd, std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD"))
                .is_none()
        );
    }
    #[test]
    fn r81_node_resolution_uses_npm_script_and_sibling_node_without_a_shell() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r81-resolution");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let shim = dir.path().join("codex.cmd");
        std::fs::write(&shim, "synthetic shim, never executed").unwrap();
        assert!(resolve_codex_node(&shim).is_none());
        let script = dir.path().join("node_modules/@openai/codex/bin/codex.js");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "// synthetic").unwrap();
        let node = dir.path().join("node.exe");
        std::fs::write(&node, "synthetic placeholder, never executed").unwrap();
        assert_eq!(
            resolve_codex_node(&shim),
            Some((node.clone(), script.clone()))
        );
        let command = client_command(Client::Codex, &shim);
        assert_eq!(command.get_program(), node);
        assert_eq!(command.get_args().collect::<Vec<_>>(), [script.as_os_str()]);
        assert_eq!(client_command(Client::Claude, &shim).get_program(), shim);
    }
    #[cfg(windows)]
    #[test]
    fn r81_known_cmd_title_trims_space_and_matches_command_suffix_only() {
        let cmds = vec![r"C:\Windows\System32\cmd.exe".into()];
        for title in [
            r"C:\WINDOWS\system32\cmd.exe ",
            r" C:\Windows\System32\cmd.exe - node script ",
        ] {
            assert!(is_cmd_title(title, &cmds));
        }
        for title in [
            "client owns this title",
            r"C:\Windows\System32\cmd.exe.other",
            r"prefix C:\Windows\System32\cmd.exe",
        ] {
            assert!(!is_cmd_title(title, &cmds));
        }
    }
    #[test]
    fn unicode_basename_is_ascii_and_round_trips_without_path() {
        let value = folder_value(Path::new(r"C:\synthetic\한글 & 테스트 ! ^ % 🐬")).unwrap();
        assert!(value.is_ascii());
        let decoded = URL_SAFE_NO_PAD
            .decode(value.strip_prefix("qb1:").unwrap())
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "한글 & 테스트 ! ^ % 🐬"
        );
        assert!(folder_value(Path::new(r"C:\")).is_none());
    }
    #[test]
    fn preserves_colons_newlines_and_unrelated_values_replaces_owned_duplicates_only() {
        let original = "X-Other: alpha:beta\r\nX-Quota-Baton-Label: old\r\nX-Aux: retain\nx-quota-baton-label: older";
        assert_eq!(
            merge_headers(original, "qb1:bmV3").unwrap(),
            "X-Other: alpha:beta\r\nX-Aux: retain\nx-quota-baton-label: qb1:bmV3"
        );
        assert_eq!(
            merge_headers("", "qb1:YQ").unwrap(),
            "x-quota-baton-label: qb1:YQ"
        );
        assert!(merge_headers("preserve", "bad\nInjected: yes").is_none());
        assert!(merge_headers(&"a".repeat(MAX_HEADERS + 1), "qb1:YQ").is_none());
    }
}
