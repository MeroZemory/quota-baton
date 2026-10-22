//! Startup selection only: no native command, rollout body, or provisional launch.
//! Native source anchors/policy limits: .sandbox/r11-prep/uuidless-resume-plan.md.
use anyhow::Result;
use std::{
    ffi::OsString,
    io::{self, BufRead, IsTerminal, Read, Write},
    path::{Path, PathBuf},
};
use toml_edit::DocumentMut;

const UUID_REQUIRED: &str = "이 재개 방식의 대화를 안전하게 선택하지 못했습니다. codex-qb resume <UUID>로 대화 ID를 지정해 주세요.";
const TTY_REQUIRED: &str = "대화 선택은 입력과 출력이 모두 터미널일 때만 가능합니다. codex-qb resume <UUID>로 대화 ID를 지정해 주세요.";
const PAGE_SIZE: usize = 10;
const MAX_PAGES: usize = 100;

#[derive(Debug)]
struct SelectionError(&'static str);
impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for SelectionError {}
pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<SelectionError>()
        .map(ToString::to_string)
}
fn refused() -> anyhow::Error {
    SelectionError(UUID_REQUIRED).into()
}

pub(crate) struct Request {
    args: Vec<OsString>,
    resume: usize,
    remove: Vec<usize>,
    last: bool,
    all: bool,
    include_non_interactive: bool,
    cwd: Option<PathBuf>,
    worktrees: bool,
}
impl Request {
    fn rewrite(&self, id: &str) -> Vec<OsString> {
        let mut args = Vec::with_capacity(self.args.len() + 1);
        for (index, arg) in self.args.iter().enumerate() {
            if !self.remove.contains(&index) {
                args.push(arg.clone());
            }
            if index == self.resume {
                args.push(id.into());
            }
        }
        args
    }
}

// Called only after launcher inspection. Reuse its option arities, preserve all
// original OsStrings, and never search arbitrary prompt/option values for UUIDs.
pub(crate) fn request(args: &[OsString], config: &DocumentMut) -> Result<Option<Request>> {
    let mut resume = None;
    let mut exec = false;
    let mut last = false;
    let mut all = false;
    let mut include_non_interactive = false;
    let mut cwd = None;
    let mut remove = Vec::new();
    let mut operands = 0;
    let mut literal = false;
    let mut unsupported = false;
    let mut worktrees_override = None;
    let mut index = 0;
    while index < args.len() {
        let start = index;
        let arg = args[index].to_str();
        index += 1;
        if !literal && arg == Some("--") {
            if resume.is_none() {
                return Ok(None);
            }
            literal = true;
            continue;
        }
        if !literal && arg.is_some_and(|v| v.starts_with('-') && v != "-") {
            let text = arg.unwrap();
            let (mut name, mut attached) = text
                .split_once('=')
                .map_or((text, None), |(n, v)| (n, Some(v)));
            if !text.starts_with("--") && text.len() > 2 {
                for short in ["-c", "-m", "-p", "-s", "-C", "-a", "-i", "-o"] {
                    if let Some(v) = text.strip_prefix(short) {
                        name = short;
                        attached = Some(v.strip_prefix('=').unwrap_or(v));
                        break;
                    }
                }
            }
            if crate::launch_client::takes_value(name) {
                let value = if let Some(value) = attached {
                    OsString::from(value)
                } else {
                    let value = args.get(index).ok_or_else(refused)?.clone();
                    index += 1;
                    value
                };
                match name {
                    "-C" | "--cd" => {
                        if cwd.is_some() {
                            unsupported = true;
                        }
                        cwd = Some(PathBuf::from(value));
                    }
                    "--enable" | "--disable" => {
                        if value == "worktrees" {
                            worktrees_override = Some(name == "--enable");
                        } else {
                            unsupported = true;
                        }
                    }
                    "-c" | "--config" => {
                        let (key, _) = value
                            .to_str()
                            .and_then(|v| v.split_once('='))
                            .ok_or_else(refused)?;
                        // Native layered overrides can change metadata routing. Only
                        // unrelated, already-validated simple model/UI overrides here.
                        if !matches!(
                            key.trim(),
                            "model"
                                | "model_reasoning_effort"
                                | "model_reasoning_summary"
                                | "model_verbosity"
                                | "service_tier"
                        ) {
                            unsupported = true;
                        }
                    }
                    "--remote"
                    | "--remote-auth-token-env"
                    | "--profile"
                    | "-p"
                    | "--local-provider" => unsupported = true,
                    _ => {}
                }
                continue;
            }
            match name {
                "--last" | "--all" | "--include-non-interactive" => {
                    if attached.is_some() || resume.is_none() {
                        unsupported = true;
                    }
                    match name {
                        "--last" => {
                            if last {
                                unsupported = true;
                            }
                            last = true;
                        }
                        "--all" => all = true,
                        _ => include_non_interactive = true,
                    }
                    remove.push(start);
                }
                _ if crate::launch_client::switch(name) => {}
                _ => unsupported = true,
            }
            continue;
        }
        if resume.is_none() {
            match arg {
                Some("exec" | "e") if !exec => exec = true,
                Some("resume") => resume = Some(start),
                _ => return Ok(None), // a fresh prompt is not a resume command
            }
        } else {
            operands += 1;
        }
    }
    let Some(resume) = resume else {
        return Ok(None);
    };
    // Preserve the established explicit UUID path, including exec resume.
    if !last && crate::sessions::resume_uuid(args)?.is_some() {
        return Ok(None);
    }
    // --last's single positional is a prompt, including one shaped like a UUID
    // (native cli/main.rs finalize_resume_interactive). Two positionals select
    // a different native precedence branch; don't guess it here.
    if unsupported || exec || (!last && operands != 0) || operands > 1 {
        return Err(refused());
    }
    let configured_worktrees = config
        .get("features")
        .and_then(|v| v.get("worktrees"))
        .map(|v| v.as_bool().ok_or_else(refused))
        .transpose()?
        .unwrap_or(true);
    let worktrees = worktrees_override.unwrap_or(configured_worktrees);
    for key in [
        "sqlite_home",
        "environments",
        "default_environment",
        "remote",
        "daemon",
        "app_server",
    ] {
        if config.contains_key(key) {
            return Err(refused());
        }
    }
    Ok(Some(Request {
        args: args.to_vec(),
        resume,
        remove,
        last,
        all,
        include_non_interactive,
        cwd,
        worktrees,
    }))
}

fn selected_args(
    home: &Path,
    request: &Request,
    row: &crate::sessions_db::ResumeCandidate,
) -> Result<Vec<OsString>> {
    if !crate::sessions_discovery::valid_id(&row.id) {
        return Err(refused());
    }
    // Resolve Codex's canonical compressed sibling without reading contents.
    // Missing, ambiguous or escaping paths still refuse selection.
    let physical = crate::sessions_discovery::resolve_rollout_path(home, &row.rollout_path)
        .map_err(|_| refused())?;
    if !physical.is_file() {
        return Err(refused());
    }
    Ok(request.rewrite(&row.id))
}

pub(crate) fn select(home: &Path, request: Request) -> Result<Option<Vec<OsString>>> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    require_terminal(request.last, stdin.is_terminal(), stdout.is_terminal())?;
    if std::env::var_os("CODEX_SQLITE_HOME").is_some() {
        return Err(refused());
    }
    let cwd = request
        .cwd
        .clone()
        .unwrap_or(std::env::current_dir().map_err(|_| refused())?);
    let cwd = cwd.canonicalize().map_err(|_| refused())?;
    validate_scope(&cwd, &request)?;
    let filter = (!request.all).then_some(cwd.as_path());
    if request.last {
        return last_args(home, &request, filter).map(Some);
    }
    pick_with_io(
        home,
        &request,
        filter,
        &mut stdin.lock(),
        &mut stdout.lock(),
    )
}

fn validate_scope(cwd: &Path, request: &Request) -> Result<()> {
    // Project layers and repository-wide worktree expansion require the native
    // config/repository resolver. Don't silently substitute a single-cwd filter.
    let mut found_repo = false;
    for ancestor in cwd.ancestors() {
        if ancestor
            .join(".codex/config.toml")
            .try_exists()
            .map_err(|_| refused())?
        {
            return Err(refused());
        }
        if !request.all
            && request.worktrees
            && !found_repo
            && ancestor.join(".git").try_exists().map_err(|_| refused())?
        {
            let git = ancestor.join(".git");
            crate::sessions_guard::safe_path(ancestor, &git).map_err(|_| refused())?;
            // Native linked_worktree_cwds adds no distinct cwd for an ordinary
            // primary checkout with no worktree registrations. Keep ordinary
            // --last working; only linked/registered layouts need expansion.
            if !git.is_dir() || git.join("worktrees").try_exists().map_err(|_| refused())? {
                return Err(refused());
            }
            found_repo = true;
        }
    }
    Ok(())
}

fn require_terminal(last: bool, input: bool, output: bool) -> Result<()> {
    if !(last || input && output) {
        return Err(SelectionError(TTY_REQUIRED).into());
    }
    Ok(())
}

fn last_args(home: &Path, request: &Request, cwd: Option<&Path>) -> Result<Vec<OsString>> {
    let rows = crate::sessions_db::resume_page(home, cwd, request.include_non_interactive, None, 2)
        .map_err(|_| refused())?;
    let row = rows.first().ok_or_else(refused)?;
    // Native UpdatedAt has no UUID tiebreaker. Refuse an indeterminate last.
    if rows
        .get(1)
        .is_some_and(|next| next.updated_at_ms == row.updated_at_ms)
    {
        return Err(refused());
    }
    selected_args(home, request, row)
}

fn pick_with_io(
    home: &Path,
    request: &Request,
    cwd: Option<&Path>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<Vec<OsString>>> {
    let mut cursors = vec![None];
    let mut page = 0;
    loop {
        let rows = crate::sessions_db::resume_page(
            home,
            cwd,
            request.include_non_interactive,
            cursors[page].as_ref(),
            PAGE_SIZE + 1,
        )
        .map_err(|_| refused())?;
        if rows.is_empty() {
            return Err(refused());
        }
        writeln!(output, "이어갈 대화 ID를 선택하세요 (페이지 {}).", page + 1)
            .map_err(|_| refused())?;
        for (index, row) in rows.iter().take(PAGE_SIZE).enumerate() {
            writeln!(output, "{}: {} ({})", index + 1, row.id, row.updated_at_ms)
                .map_err(|_| refused())?;
        }
        write!(output, "번호 / n 다음 / p 이전 / q 취소: ").map_err(|_| refused())?;
        output.flush().map_err(|_| refused())?;
        let mut answer = String::new();
        let size = (&mut *input)
            .take(1025)
            .read_line(&mut answer)
            .map_err(|_| refused())?;
        if size == 0 {
            return Ok(None);
        }
        if size > 1024 {
            return Err(refused());
        }
        match answer.trim() {
            "q" => return Ok(None),
            "p" if page > 0 => page -= 1,
            "n" if rows.len() > PAGE_SIZE && page + 1 < MAX_PAGES => {
                let row = &rows[PAGE_SIZE - 1];
                let cursor = crate::sessions_db::ResumeCursor {
                    updated_at_ms: row.updated_at_ms,
                    id: row.id.clone(),
                };
                cursors.truncate(page + 1);
                cursors.push(Some(cursor));
                page += 1;
            }
            value => {
                if let Ok(index) = value.parse::<usize>()
                    && index > 0
                    && index <= rows.len().min(PAGE_SIZE)
                {
                    return selected_args(home, request, &rows[index - 1]).map(Some);
                }
                writeln!(output, "표시된 번호나 이동 명령을 입력해 주세요.")
                    .map_err(|_| refused())?;
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::sessions_db::{ResumeCursor, resume_page};
    use rusqlite::{Connection, params};

    const FIRST: &str = "11111111-1111-1111-1111-111111111111";
    const SECOND: &str = "22222222-2222-2222-2222-222222222222";
    fn argv(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }
    fn requested(values: &[&str]) -> Request {
        request(&argv(values), &"model='configured'".parse().unwrap())
            .unwrap()
            .unwrap()
    }
    fn fixture() -> tempfile::TempDir {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r11-resume-selection-tests");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base.canonicalize().unwrap()).unwrap();
        let db = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE threads (
            id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, model_provider TEXT NOT NULL,
            source TEXT NOT NULL, cwd TEXT NOT NULL, archived INTEGER NOT NULL,
            preview TEXT NOT NULL, updated_at_ms INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL, recency_at_ms INTEGER NOT NULL);",
        )
        .unwrap();
        home
    }
    fn row(home: &Path, id: &str, time: i64, provider: &str, source: &str, cwd: &Path) {
        let path = home.join("sessions").join(format!("{id}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"synthetic opaque fixture: never read").unwrap();
        let db = Connection::open(home.join("state_5.sqlite")).unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1,?2,?3,?4,?5,0,'synthetic preview',?6,?7,?7)",
            params![
                id,
                path.to_str().unwrap(),
                provider,
                source,
                cwd.to_str().unwrap(),
                time,
                100_000 - time
            ],
        )
        .unwrap();
    }

    #[test]
    fn fresh_explicit_exec_and_option_values_are_not_picker_intents() {
        let config = "model='configured'".parse().unwrap();
        for values in [
            vec![],
            vec!["a fresh prompt", "resume"],
            vec!["--", "resume"],
            vec!["--config", "model='resume'"],
            vec!["resume", FIRST],
            vec!["exec", "resume", FIRST],
            vec!["-m", "--last", "resume", FIRST],
        ] {
            assert!(request(&argv(&values), &config).unwrap().is_none());
        }
    }
    #[test]
    fn last_rewrite_preserves_prompt_and_literal_boundary() {
        let selection = requested(&["-m", "chosen", "resume", "--all", "--last", "--", FIRST]);
        assert_eq!(
            selection.rewrite(SECOND),
            argv(&["-m", "chosen", "resume", SECOND, "--", FIRST])
        );
        assert_eq!(
            crate::sessions::resume_uuid(&selection.rewrite(SECOND))
                .unwrap()
                .as_deref(),
            Some(SECOND)
        );
        let selection = requested(&["resume", "--last", "prompt", "--include-non-interactive"]);
        assert_eq!(selection.rewrite(FIRST), argv(&["resume", FIRST, "prompt"]));
    }
    #[test]
    fn scoped_cd_spellings_and_worktrees_override_are_parsed_without_io() {
        for values in [
            vec!["-Cfolder", "resume"],
            vec!["--cd=folder", "resume"],
            vec!["resume", "-C", "folder"],
        ] {
            assert_eq!(requested(&values).cwd.as_deref(), Some(Path::new("folder")));
        }
        assert!(!requested(&["resume", "--disable", "worktrees"]).worktrees);
        assert!(requested(&["resume", "--all"]).all);
    }
    #[test]
    fn unproven_grammar_and_configuration_refuse_with_fixed_uuid_instruction() {
        for values in [
            vec!["resume", "a-name"],
            vec!["exec", "resume", "--last"],
            vec!["resume", "--last", FIRST, "prompt"],
            vec!["resume", "--last=true"],
            vec!["resume", "--unknown"],
            vec!["-c", "sqlite_home='elsewhere'", "resume"],
        ] {
            let error = request(&argv(&values), &"model='configured'".parse().unwrap())
                .err()
                .unwrap();
            assert_eq!(error.to_string(), UUID_REQUIRED);
            assert_eq!(error.chain().count(), 1);
        }
        assert!(
            request(
                &argv(&["resume"]),
                &"sqlite_home='private'".parse().unwrap()
            )
            .is_err()
        );
    }
    #[test]
    fn picker_requires_both_terminals_but_last_does_not() {
        for (input, output) in [(false, false), (false, true), (true, false)] {
            assert_eq!(
                require_terminal(false, input, output)
                    .unwrap_err()
                    .to_string(),
                TTY_REQUIRED
            );
            assert!(require_terminal(true, input, output).is_ok());
        }
        assert!(require_terminal(false, true, true).is_ok());
    }

    #[test]
    fn project_layers_and_unexpanded_worktrees_refuse_without_reading_config() {
        let home = fixture();
        std::fs::create_dir(home.path().join(".git")).unwrap();
        assert!(validate_scope(home.path(), &requested(&["resume"])).is_ok());
        std::fs::create_dir(home.path().join(".git/worktrees")).unwrap();
        assert!(validate_scope(home.path(), &requested(&["resume"])).is_err());
        assert!(validate_scope(home.path(), &requested(&["resume", "--all"])).is_ok());
        assert!(
            validate_scope(
                home.path(),
                &requested(&["resume", "--disable", "worktrees"])
            )
            .is_ok()
        );
        std::fs::create_dir(home.path().join(".codex")).unwrap();
        std::fs::write(
            home.path().join(".codex/config.toml"),
            b"unread synthetic contents",
        )
        .unwrap();
        assert!(validate_scope(home.path(), &requested(&["resume", "--all"])).is_err());
    }
    #[test]
    fn native_db_order_is_updated_not_created_recency_or_uuid() {
        let home = fixture();
        row(home.path(), FIRST, 20, "openai", "cli", home.path());
        row(home.path(), SECOND, 10, "openai", "vscode", home.path());
        let bytes = std::fs::read(home.path().join("state_5.sqlite")).unwrap();
        let args = last_args(
            home.path(),
            &requested(&["resume", "--last"]),
            Some(home.path()),
        )
        .unwrap();
        assert_eq!(args, argv(&["resume", FIRST]));
        assert_eq!(
            std::fs::read(home.path().join("state_5.sqlite")).unwrap(),
            bytes
        );
    }
    #[test]
    fn native_db_filters_provider_cwd_sources_archive_and_preview() {
        let home = fixture();
        row(home.path(), FIRST, 10, "openai", "cli", home.path());
        let other = home.path().join("other");
        std::fs::create_dir(&other).unwrap();
        for (n, provider, source, cwd) in [
            (2, "other", "cli", home.path()),
            (3, "openai", "cli", other.as_path()),
            (4, "openai", "exec", home.path()),
            (5, "openai", "mcp", home.path()),
            (6, "openai", "unknown", home.path()),
            (7, "openai", "cli", home.path()),
            (8, "openai", "cli", home.path()),
        ] {
            row(
                home.path(),
                &format!("{n:08}-1111-1111-1111-111111111111"),
                n * 100,
                provider,
                source,
                cwd,
            );
        }
        let db = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        db.execute(
            "UPDATE threads SET archived=1 WHERE id LIKE '00000007%'",
            [],
        )
        .unwrap();
        db.execute(
            "UPDATE threads SET preview='' WHERE id LIKE '00000008%'",
            [],
        )
        .unwrap();
        let page = resume_page(home.path(), Some(home.path()), false, None, 10).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, FIRST);
        let page = resume_page(home.path(), Some(home.path()), true, None, 10).unwrap();
        assert_eq!(page.len(), 3);
        assert!(page[0].id.starts_with("00000005"));
        let page = resume_page(home.path(), None, false, None, 10).unwrap();
        assert_eq!(page.len(), 2);
        assert!(page[0].id.starts_with("00000003"));
    }
    #[test]
    fn last_tie_missing_metadata_or_stale_rollout_refuses_without_fresh_launch() {
        let home = fixture();
        let request = requested(&["resume", "--last"]);
        assert!(last_args(home.path(), &request, None).is_err());
        row(home.path(), FIRST, 20, "openai", "cli", home.path());
        row(home.path(), SECOND, 20, "openai", "cli", home.path());
        assert!(last_args(home.path(), &request, None).is_err());
        let db = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        db.execute(
            "UPDATE threads SET rollout_path='missing', updated_at_ms=30 WHERE id=?1",
            [FIRST],
        )
        .unwrap();
        assert!(last_args(home.path(), &request, None).is_err());
        let absent = home.path().join("absent");
        std::fs::create_dir(&absent).unwrap();
        assert!(resume_page(&absent, None, false, None, 10).is_err());
        assert!(!absent.join("state_5.sqlite").exists());
    }
    #[test]
    fn picker_paginates_equal_timestamps_and_rewrites_selected_id() {
        let home = fixture();
        for n in 1..=12 {
            row(
                home.path(),
                &format!("{n:08}-1111-1111-1111-111111111111"),
                20,
                "openai",
                "cli",
                home.path(),
            );
        }
        let request = requested(&["resume", "--all"]);
        let mut output = Vec::new();
        let args = pick_with_io(
            home.path(),
            &request,
            None,
            &mut io::Cursor::new(b"n\np\nn\n2\n"),
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            args,
            argv(&["resume", "00000001-1111-1111-1111-111111111111"])
        );
        assert!(String::from_utf8(output).unwrap().contains("페이지 2"));
        let page = resume_page(home.path(), None, false, None, 10).unwrap();
        let last = page.last().unwrap();
        let cursor = ResumeCursor {
            updated_at_ms: last.updated_at_ms,
            id: last.id.clone(),
        };
        assert_eq!(
            resume_page(home.path(), None, false, Some(&cursor), 10)
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn picker_cancels_or_refuses_oversized_input_without_selecting() {
        let home = fixture();
        row(home.path(), FIRST, 20, "openai", "cli", home.path());
        let request = requested(&["resume"]);
        for input in [b"q\n".as_slice(), b"".as_slice()] {
            assert!(
                pick_with_io(
                    home.path(),
                    &request,
                    None,
                    &mut io::Cursor::new(input),
                    &mut Vec::new()
                )
                .unwrap()
                .is_none()
            );
        }
        assert!(
            pick_with_io(
                home.path(),
                &request,
                None,
                &mut io::Cursor::new(vec![b'x'; 1025]),
                &mut Vec::new()
            )
            .is_err()
        );
    }
    #[test]
    fn db_bounds_schema_and_path_refusals_do_not_publish_or_escape() {
        let home = fixture();
        row(home.path(), FIRST, 20, "openai", "cli", home.path());
        assert!(resume_page(home.path(), None, false, None, 0).is_err());
        assert!(resume_page(home.path(), None, false, None, 102).is_err());
        let db = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        db.execute(
            "UPDATE threads SET rollout_path=?1",
            [home.path().join("../outside").to_str().unwrap()],
        )
        .unwrap();
        assert!(last_args(home.path(), &requested(&["resume", "--last"]), None).is_err());
        db.execute_batch("ALTER TABLE threads RENAME COLUMN updated_at_ms TO unsupported_time")
            .unwrap();
        assert!(resume_page(home.path(), None, false, None, 10).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn r94_last_accepts_only_compressed_sibling_of_canonical_database_path() {
        let root = fixture();
        let home = root.path();
        let cwd = home.canonicalize().unwrap();
        row(home, FIRST, 30, "openai", "cli", &cwd);
        let path = crate::sessions_db::thread_path(home, FIRST)
            .unwrap()
            .unwrap();
        let compressed = path.with_extension("jsonl.zst");
        std::fs::write(
            &compressed,
            zstd::stream::encode_all(std::fs::read(&path).unwrap().as_slice(), 1).unwrap(),
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();
        let request = requested(&["resume", "--last"]);
        assert_eq!(
            last_args(home, &request, None).unwrap(),
            argv(&["resume", FIRST])
        );
        std::fs::write(&path, b"synthetic duplicate").unwrap();
        assert!(last_args(home, &request, None).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn last_prompt_utf16_is_preserved_losslessly() {
        use std::os::windows::ffi::OsStringExt;
        let opaque = OsString::from_wide(&[0xd800]);
        let mut args = argv(&["resume", "--last", "--"]);
        args.push(opaque.clone());
        let request = request(&args, &"model='configured'".parse().unwrap())
            .unwrap()
            .unwrap();
        let mut expected = argv(&["resume", FIRST, "--"]);
        expected.push(opaque);
        assert_eq!(request.rewrite(FIRST), expected);
    }
}
