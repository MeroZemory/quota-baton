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
    run_with_environment(client, program, settings, args, &[])
}

/// D23: image-home selection and its local client key affect only the managed
/// process tree, never this process or an installed/global configuration.
pub(crate) fn run_with_environment(
    client: Client,
    program: &Path,
    settings: Option<&Path>,
    args: &[std::ffi::OsString],
    environment: &[(&str, &std::ffi::OsStr)],
) -> std::io::Result<i32> {
    if !program.is_absolute()
        || settings.is_some_and(|p| !p.is_absolute())
        || matches!(client, Client::Claude) && settings.is_none()
        || matches!(client, Client::Codex) && settings.is_some()
    {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut child = client_command(client, program);
    child.envs(environment.iter().copied());
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
    #[cfg(windows)]
    if matches!(client, Client::Codex) {
        // Job membership is installed by CreateProcess itself, not after spawn.
        // The native process and every descendant die if this launcher dies.
        let mut child = contained::Child::spawn(&child, None)?;
        let repair = title
            .filter(|_| needs_title_repair)
            .map(ConsoleTitle::watch);
        let status = child.wait(None);
        // Kill and wait for descendants before the caller releases admission.
        drop(child);
        if let Some((stop, worker)) = repair {
            let _ = stop.send(());
            let _ = worker.join();
        }
        return status;
    }
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

/// The version probe uses the same npm/node resolution and batch quoting as a
/// launch, but has bounded output and a five-second process-tree lifetime.
pub(crate) fn version_output(program: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(windows)]
    {
        let mut command = client_command(Client::Codex, program);
        command.arg("--version");
        contained::capture(&command)
    }
    #[cfg(not(windows))]
    {
        let _ = program;
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

/// Isolated D23 capability probe. The job owns the complete process tree and
/// output goes to NUL; callers retain only the loopback tool-presence boolean.
pub(crate) fn canary_run(
    program: &Path,
    args: &[std::ffi::OsString],
    environment: &[(&str, std::ffi::OsString)],
    cwd: &Path,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let mut command = client_command(Client::Codex, program);
        command
            .args(args)
            .envs(environment.iter().map(|(key, value)| (*key, value)))
            .current_dir(cwd);
        let null = std::fs::OpenOptions::new().write(true).open("NUL")?;
        let mut child = contained::Child::spawn(&command, Some(&null))?;
        let status = child.wait(Some(std::time::Duration::from_secs(15)));
        drop(child);
        #[cfg(all(feature = "testkit", debug_assertions))]
        match &status {
            Ok(code) if *code != 0 => eprintln!("qb-image-canary: native-exit={code}"),
            Err(error) => eprintln!("qb-image-canary: native-wait={:?}", error.kind()),
            _ => {}
        }
        if status? == 0 {
            Ok(())
        } else {
            Err(std::io::Error::other("이미지 도구 검사 실패"))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (program, args, environment, cwd);
        Err(std::io::ErrorKind::Unsupported.into())
    }
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
mod contained {
    use std::{
        ffi::{OsStr, OsString, c_void},
        fs::File,
        io::{self, Read},
        marker::PhantomData,
        os::windows::{
            ffi::{OsStrExt, OsStringExt},
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        path::{Component, Path, Prefix},
        process::Command,
        time::{Duration, Instant},
    };
    use windows::{
        Win32::{
            Foundation::{
                DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
            },
            System::{
                Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
                Threading::{
                    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
                    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
                    GetExitCodeProcess, IO_COUNTERS, InitializeProcThreadAttributeList,
                    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread,
                    STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
                    WaitForSingleObject,
                },
            },
        },
        core::{PCWSTR, PWSTR},
    };

    // JobObjects is not a workspace windows-crate feature. Keep these four ABI
    // declarations local instead of changing the shared dependency feature set.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: i32,
            info: *const c_void,
            size: u32,
        ) -> i32;
        fn TerminateJobObject(job: *mut c_void, code: u32) -> i32;
        fn QueryInformationJobObject(
            job: *mut c_void,
            class: i32,
            info: *mut c_void,
            size: u32,
            returned: *mut u32,
        ) -> i32;
        fn CreatePipe(
            read: *mut *mut c_void,
            write: *mut *mut c_void,
            attributes: *const c_void,
            size: u32,
        ) -> i32;
    }

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        process_time: i64,
        job_time: i64,
        flags: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_limit: u32,
        affinity: usize,
        priority: u32,
        scheduling: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: IO_COUNTERS,
        process_memory: usize,
        job_memory: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Accounting {
        times: [i64; 4],
        page_faults: u32,
        total: u32,
        active: u32,
        terminated: u32,
    }

    fn os_error(_: windows::core::Error) -> io::Error {
        io::Error::last_os_error()
    }

    fn owned(raw: *mut c_void) -> io::Result<OwnedHandle> {
        if raw.is_null() || raw as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful creation/duplication transfers this unique handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }

    fn inherited(handle: HANDLE) -> io::Result<OwnedHandle> {
        let fallback;
        let handle = if handle.is_invalid() {
            fallback = File::options().read(true).write(true).open("NUL")?;
            HANDLE(fallback.as_raw_handle())
        } else {
            handle
        };
        let mut duplicated = HANDLE::default();
        // Only these explicit duplicates are inheritable; notably not the job.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                handle,
                GetCurrentProcess(),
                &mut duplicated,
                0,
                true,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .map_err(os_error)?;
        owned(duplicated.0)
    }

    struct Attributes<'a> {
        _storage: Vec<usize>,
        list: LPPROC_THREAD_ATTRIBUTE_LIST,
        _values: PhantomData<&'a [HANDLE]>,
    }
    impl<'a> Attributes<'a> {
        fn new(job: &'a [HANDLE], handles: &'a [HANDLE]) -> io::Result<Self> {
            let mut bytes = 0;
            // Expected sizing failure, with a nonzero required allocation.
            let _ = unsafe { InitializeProcThreadAttributeList(None, 2, None, &mut bytes) };
            if bytes == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
            let list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
            unsafe { InitializeProcThreadAttributeList(Some(list), 2, None, &mut bytes) }
                .map_err(os_error)?;
            let attributes = Self {
                _storage: storage,
                list,
                _values: PhantomData,
            };
            for (kind, values) in [
                (PROC_THREAD_ATTRIBUTE_JOB_LIST, job),
                (PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles),
            ] {
                unsafe {
                    UpdateProcThreadAttribute(
                        list,
                        0,
                        kind as usize,
                        Some(values.as_ptr().cast()),
                        size_of_val(values),
                        None,
                        None,
                    )
                }
                .map_err(os_error)?;
            }
            Ok(attributes)
        }
    }
    impl Drop for Attributes<'_> {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.list) };
        }
    }

    pub(super) struct Child {
        job: OwnedHandle,
        process: OwnedHandle,
    }

    impl Child {
        pub(super) fn spawn(command: &Command, output: Option<&File>) -> io::Result<Self> {
            let environment = environment_block(command)?;
            let (program, mut line) = command_line(command)?;
            let job = owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
            let mut limits = ExtendedLimits::default();
            limits.basic.flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            if unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    9,
                    (&limits as *const ExtendedLimits).cast(),
                    size_of::<ExtendedLimits>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let null_input;
            let input = inherited(if output.is_some() {
                null_input = File::open("NUL")?;
                HANDLE(null_input.as_raw_handle())
            } else {
                unsafe { GetStdHandle(STD_INPUT_HANDLE) }.unwrap_or_default()
            })?;
            let stdout = inherited(match output {
                Some(file) => HANDLE(file.as_raw_handle()),
                None => unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.unwrap_or_default(),
            })?;
            let stderr = inherited(match output {
                Some(file) => HANDLE(file.as_raw_handle()),
                None => unsafe { GetStdHandle(STD_ERROR_HANDLE) }.unwrap_or_default(),
            })?;
            let handles = [
                HANDLE(input.as_raw_handle()),
                HANDLE(stdout.as_raw_handle()),
                HANDLE(stderr.as_raw_handle()),
            ];
            let jobs = [HANDLE(job.as_raw_handle())];
            let attributes = Attributes::new(&jobs, &handles)?;
            let mut startup = STARTUPINFOEXW::default();
            startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = handles[0];
            startup.StartupInfo.hStdOutput = handles[1];
            startup.StartupInfo.hStdError = handles[2];
            startup.lpAttributeList = attributes.list;
            let cwd = command
                .get_current_dir()
                .map(|path| wide(path.as_os_str()))
                .transpose()?;
            let mut information = PROCESS_INFORMATION::default();
            let mut flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;
            if output.is_some() {
                flags |= CREATE_NO_WINDOW;
            }
            if environment.is_some() {
                flags |= CREATE_UNICODE_ENVIRONMENT;
            }
            // The job-list attribute assigns membership atomically at creation.
            // Even a launcher crash before ResumeThread cannot leave an orphan.
            unsafe {
                CreateProcessW(
                    PCWSTR(program.as_ptr()),
                    Some(PWSTR(line.as_mut_ptr())),
                    None,
                    None,
                    true,
                    flags,
                    environment.as_ref().map(|block| block.as_ptr().cast()),
                    PCWSTR(cwd.as_ref().map_or(std::ptr::null(), |v| v.as_ptr())),
                    &startup.StartupInfo,
                    &mut information,
                )
            }
            .map_err(os_error)?;
            let child = Self {
                job,
                process: owned(information.hProcess.0)?,
            };
            let thread = owned(information.hThread.0)?;
            if unsafe { ResumeThread(HANDLE(thread.as_raw_handle())) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            Ok(child)
        }

        pub(super) fn wait(&mut self, timeout: Option<Duration>) -> io::Result<i32> {
            let deadline = timeout.map(|duration| Instant::now() + duration);
            loop {
                match unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 10) } {
                    WAIT_OBJECT_0 => {
                        let mut code = 1;
                        unsafe {
                            GetExitCodeProcess(HANDLE(self.process.as_raw_handle()), &mut code)
                        }
                        .map_err(os_error)?;
                        return Ok(code as i32);
                    }
                    WAIT_TIMEOUT => {}
                    _ => return Err(io::Error::last_os_error()),
                }
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(io::ErrorKind::TimedOut.into());
                }
            }
        }
    }

    /// This path inherits the current environment and applies Command's
    /// explicit set/remove operations. Callers never use env_clear here.
    /// Values stay private and the native UTF-16 block is erased after spawn.
    fn environment_block(command: &Command) -> io::Result<Option<zeroize::Zeroizing<Vec<u16>>>> {
        if command.get_envs().next().is_none() {
            return Ok(None);
        }
        fn key(value: &OsStr) -> Vec<u16> {
            value
                .encode_wide()
                .map(|unit| {
                    if (97..=122).contains(&unit) {
                        unit - 32
                    } else {
                        unit
                    }
                })
                .collect()
        }
        let mut values: std::collections::BTreeMap<_, _> = std::env::vars_os()
            .map(|(name, value)| (key(&name), (name, value)))
            .collect();
        for (name, value) in command.get_envs() {
            if let Some(value) = value {
                values.insert(key(name), (name.to_owned(), value.to_owned()));
            } else {
                values.remove(&key(name));
            }
        }
        let mut block = zeroize::Zeroizing::new(Vec::new());
        for (_, (name, value)) in values {
            let name: Vec<_> = name.encode_wide().collect();
            let value = zeroize::Zeroizing::new(value.encode_wide().collect::<Vec<_>>());
            if name.contains(&0) || value.contains(&0) {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            block.extend_from_slice(&name);
            block.push(61);
            block.extend_from_slice(&value);
            block.push(0);
        }
        block.push(0);
        if block.len() == 1 {
            block.push(0);
        }
        Ok(Some(block))
    }

    impl Drop for Child {
        fn drop(&mut self) {
            unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
            // Do not return to the caller (and release admission) while a native
            // child or inherited descendant can still access the managed home.
            loop {
                let mut accounting = Accounting::default();
                if unsafe {
                    QueryInformationJobObject(
                        self.job.as_raw_handle(),
                        1,
                        (&mut accounting as *mut Accounting).cast(),
                        size_of::<Accounting>() as u32,
                        std::ptr::null_mut(),
                    )
                } == 0
                {
                    // A broken job handle cannot safely return an admission
                    // permit. Process teardown still closes the kill-on-close job.
                    std::process::abort();
                }
                if accounting.active == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    pub(super) fn capture(command: &Command) -> io::Result<Vec<u8>> {
        let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
        if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let read = File::from(owned(read)?);
        let write = File::from(owned(write)?);
        let mut child = Child::spawn(command, Some(&write)).inspect_err(|_error| {
            #[cfg(all(feature = "testkit", debug_assertions))]
            eprintln!(
                "qb-version-probe stage=spawn kind={:?} os={:?}",
                _error.kind(),
                _error.raw_os_error()
            );
        })?;
        drop(write);
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            read.take(4097).read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        });
        let status = child.wait(Some(Duration::from_secs(5)));
        drop(child); // Closes every inherited pipe writer, even on timeout.
        let output = reader
            .join()
            .map_err(|_| io::Error::other("버전 출력 검사 실패"))??;
        #[cfg(all(feature = "testkit", debug_assertions))]
        match &status {
            Err(error) => eprintln!(
                "qb-version-probe stage=wait kind={:?} os={:?} bytes={}",
                error.kind(),
                error.raw_os_error(),
                output.len()
            ),
            Ok(code) if *code != 0 || output.len() > 4096 => eprintln!(
                "qb-version-probe stage=result exit={code} bytes={}",
                output.len()
            ),
            _ => {}
        }
        if status? != 0 || output.len() > 4096 {
            return Err(io::Error::other("Codex 버전을 확인하지 못했습니다"));
        }
        Ok(output)
    }

    fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
        let mut value: Vec<_> = value.encode_wide().collect();
        if value.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        value.push(0);
        Ok(value)
    }

    // Match Rust's Windows CRT and batch escaping. These algorithms follow the
    // Rust 1.93 std implementation (MIT/Apache-2.0), including its percent hack;
    // /v:OFF and /d prevent delayed expansion and cmd AutoRun execution.
    fn quoted(output: &mut Vec<u16>, value: &OsStr, batch: bool) -> io::Result<()> {
        let units = wide(value)?;
        if batch && units.iter().any(|&unit| unit == 10 || unit == 13) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        output.push(34);
        let mut slashes = 0;
        for &unit in &units[..units.len() - 1] {
            if unit == 92 {
                slashes += 1;
            } else {
                if unit == 34 {
                    output.extend(std::iter::repeat_n(92, slashes + usize::from(!batch)));
                    if batch {
                        output.push(34);
                    }
                } else if batch && unit == 37 {
                    output.extend("%%cd:~,".encode_utf16());
                }
                slashes = 0;
            }
            output.push(unit);
        }
        output.extend(std::iter::repeat_n(92, slashes));
        output.push(34);
        Ok(())
    }

    fn command_line(command: &Command) -> io::Result<(Vec<u16>, Vec<u16>)> {
        let original = Path::new(command.get_program());
        if !original.is_absolute() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let batch = original
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
        let shell;
        let program = if batch {
            shell = std::path::PathBuf::from(
                std::env::var_os("SystemRoot").ok_or(io::ErrorKind::NotFound)?,
            )
            // cmd scans its own raw argv[0] for switches: `/cmd.exe` can
            // become /c followed by md.exe. Keep shell separators native.
            .join("System32")
            .join("cmd.exe");
            shell.as_path()
        } else {
            original
        };
        let mut line = Vec::new();
        quoted(&mut line, program.as_os_str(), false)?;
        if batch {
            line.extend(" /e:ON /v:OFF /d /c \"".encode_utf16());
            quoted(&mut line, &batch_path(original)?, true)?;
        }
        for argument in command.get_args() {
            line.push(32);
            quoted(&mut line, argument, batch)?;
        }
        if batch {
            line.push(34);
        }
        line.push(0);
        if line.len() > 32767 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok((wide(program.as_os_str())?, line))
    }

    /// cmd does not execute a drive-verbatim batch pathname. Convert only that
    /// namespace, preserving native UTF-16, and verify that ordinary Win32 path
    /// semantics resolve to the same file before passing it to the shell.
    fn batch_path(path: &Path) -> io::Result<OsString> {
        match path.components().next() {
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_)) => {
                let units: Vec<_> = path.as_os_str().encode_wide().skip(4).collect();
                let ordinary = OsString::from_wide(&units);
                if std::fs::canonicalize(Path::new(&ordinary))? != std::fs::canonicalize(path)? {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                Ok(ordinary)
            }
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)) => {
                Ok(path.as_os_str().to_owned())
            }
            _ => Err(io::ErrorKind::Unsupported.into()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
        };

        #[repr(C)]
        #[derive(Default)]
        struct ProcessIds {
            assigned: u32,
            count: u32,
            ids: [usize; 8],
        }

        #[test]
        fn job_drop_and_last_handle_close_kill_owned_child_and_descendant() {
            let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/session-admission-tests");
            std::fs::create_dir_all(&scratch).unwrap();
            let root = tempfile::tempdir_in(scratch).unwrap();
            std::fs::write(
                root.path().join("hold.cmd"),
                b"@echo off\r\n:hold\r\ngoto hold\r\n",
            )
            .unwrap();
            let script = root.path().join("parent.cmd");
            std::fs::write(&script, b"@echo off\r\nstart \"\" /b \"%SystemRoot%\\System32\\cmd.exe\" /d /c \"\"%~dp0hold.cmd\"\"\r\n:hold\r\ngoto hold\r\n").unwrap();
            let sink = File::options().write(true).open("NUL").unwrap();
            for abrupt in [false, true] {
                let child = Child::spawn(&Command::new(&script), Some(&sink)).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                let ids = loop {
                    let mut ids = ProcessIds::default();
                    assert_ne!(
                        unsafe {
                            QueryInformationJobObject(
                                child.job.as_raw_handle(),
                                3,
                                (&mut ids as *mut ProcessIds).cast(),
                                size_of::<ProcessIds>() as u32,
                                std::ptr::null_mut(),
                            )
                        },
                        0
                    );
                    if ids.count >= 2 {
                        break ids;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "synthetic descendant did not join its parent's job"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                };
                assert!(ids.count as usize <= ids.ids.len());
                let observed: Vec<_> = ids.ids[..ids.count as usize]
                    .iter()
                    .map(|&id| {
                        let handle = unsafe {
                            OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, false, id as u32)
                        }
                        .unwrap();
                        owned(handle.0).unwrap()
                    })
                    .collect();
                let process = if abrupt {
                    // Simulate OS handle teardown after launcher death without
                    // invoking Child::drop/TerminateJobObject. This must exercise
                    // KILL_ON_JOB_CLOSE itself, not just explicit cleanup.
                    let child = std::mem::ManuallyDrop::new(child);
                    // SAFETY: ManuallyDrop suppresses both field destructors;
                    // each unique owned handle is moved out exactly once.
                    let job = unsafe { std::ptr::read(&child.job) };
                    let process = unsafe { std::ptr::read(&child.process) };
                    drop(job);
                    Some(process)
                } else {
                    drop(child);
                    None
                };
                let mut all_exited = true;
                for handle in observed {
                    let exited =
                        unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), 5000) }
                            == WAIT_OBJECT_0;
                    if !exited {
                        all_exited = false;
                        // A failed containment test must not leave busy-loop
                        // fixtures behind. These handles came only from our job.
                        unsafe { TerminateProcess(HANDLE(handle.as_raw_handle()), 1) }.unwrap();
                        assert_eq!(
                            unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), 5000) },
                            WAIT_OBJECT_0
                        );
                    }
                }
                drop(process);
                assert!(
                    all_exited,
                    "owned process survived job teardown (abrupt={abrupt})"
                );
            }
        }
    }
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
#[cfg(windows)]
#[test]
fn r12_contained_environment_sets_and_removes_only_child_variables() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r12-environment-tests");
    std::fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let script = root.path().join("environment.cmd");
    std::fs::write(&script, b"@echo off\r\nif defined QB_SYNTHETIC_IMAGE_KEY (echo present:%QB_SYNTHETIC_IMAGE_KEY%) else (echo absent)\r\nexit /b 0\r\n").unwrap();
    let mut command = std::process::Command::new(script);
    command.env("QB_SYNTHETIC_IMAGE_KEY", "r12-synthetic-only");
    let existed = std::env::var_os("QB_SYNTHETIC_IMAGE_KEY").is_some();
    assert_eq!(
        contained::capture(&command).unwrap(),
        b"present:r12-synthetic-only\r\n"
    );
    assert_eq!(
        std::env::var_os("QB_SYNTHETIC_IMAGE_KEY").is_some(),
        existed
    );
    command.env_remove("QB_SYNTHETIC_IMAGE_KEY");
    assert_eq!(contained::capture(&command).unwrap(), b"absent\r\n");
}
