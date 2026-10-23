//! D23 discovery only: no real home, account, image, or upstream request.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SCHEME: &str = "d23-image-discovery-1";
const LIMIT: usize = 2 * 1024 * 1024;
const REFUSAL: &str = "현재 Codex의 이미지 도구(image_gen__imagegen)를 확인하지 못했습니다. 이미지 작업은 codex-mz를 사용하세요.";
const FAKE_KEY: &str = "qb-image-canary-not-a-credential";
const PATH_ENV: &[&str] = &[
    "CODEX_HOME",
    "USERPROFILE",
    "HOME",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "TEMP",
    "TMP",
    "TMPDIR",
];
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeStage {
    Version,
    CachePublication,
    Worker,
    Setup,
    Cleanup,
    NativeSpawn,
    NativeWait,
    NativeExit,
    ServerTimeout,
    Server,
    ToolAbsent,
}

impl ProbeStage {
    fn name(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::CachePublication => "cache-publication",
            Self::Worker => "worker",
            Self::Setup => "setup",
            Self::Cleanup => "cleanup",
            Self::NativeSpawn => "native-spawn",
            Self::NativeWait => "native-wait",
            Self::NativeExit => "native-exit",
            Self::ServerTimeout => "server-timeout",
            Self::Server => "server",
            Self::ToolAbsent => "tool-absent",
        }
    }

    fn retryable(self) -> bool {
        matches!(self, Self::NativeWait | Self::ServerTimeout)
    }
}

impl std::fmt::Display for ProbeStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
impl std::error::Error for ProbeStage {}

fn probe_with_retry(mut attempt: impl FnMut() -> Result<()>) -> Result<()> {
    for retry in [true, false] {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) => {
                let stage = error
                    .downcast_ref::<ProbeStage>()
                    .copied()
                    .unwrap_or(ProbeStage::Setup);
                if !retry || !stage.retryable() {
                    return Err(error);
                }
                diagnostic(stage.name(), true);
            }
        }
    }
    unreachable!()
}

#[derive(Debug)]
struct CanaryRefusal;

impl std::fmt::Display for CanaryRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REFUSAL)
    }
}
impl std::error::Error for CanaryRefusal {}

pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<CanaryRefusal>()
        .map(ToString::to_string)
}

/// Normal image launch only. The coordinator must not call this from --check.
/// Keep the blocking helper and server lifecycle together even if cancelled.
pub(crate) async fn check(program: &Path, qb_home: &Path) -> Result<()> {
    let program = program.to_owned();
    let qb_home = qb_home.to_owned();
    tokio::task::spawn_blocking(move || check_inner(&program, &qb_home))
        .await
        .map_err(|_| {
            diagnostic(ProbeStage::Worker.name(), true);
            CanaryRefusal
        })?
        .map_err(|_| CanaryRefusal.into())
}

fn check_inner(program: &Path, qb_home: &Path) -> Result<()> {
    let result = check_inner_impl(program, qb_home);
    if let Err(error) = &result {
        let stage = error
            .downcast_ref::<ProbeStage>()
            .copied()
            .unwrap_or(ProbeStage::Setup);
        diagnostic(stage.name(), true);
    }
    result
}

fn check_inner_impl(program: &Path, qb_home: &Path) -> Result<()> {
    let qb_home = fs::canonicalize(qb_home)?;
    let base = qb_home.join("state/image-canary");
    // Pins the no-reparse ancestors for cache publication and scratch lifetime.
    let _guard = crate::sessions_guard::prepare_parent(&qb_home, &base)?;
    let version = match version_gate(
        program,
        &qb_home,
        &base,
        || crate::launcher_label::version_output(program),
        SystemTime::now,
    )
    .context(ProbeStage::Version)?
    {
        VersionGate::Known(version) => version,
        VersionGate::TimeoutCache => return Ok(()),
    };
    let cache = base.join(cache_name(&version));
    crate::sessions_guard::safe_path(&qb_home, &cache)?;
    if cached_pass(&cache, &version) {
        return Ok(());
    }

    probe_with_retry(|| {
        let scratch = Scratch::create(&qb_home, &base)?;
        let result = probe(program, &scratch.path);
        // Every attempt is isolated and cleaned before retry or publication.
        scratch.cleanup().context(ProbeStage::Cleanup)?;
        result
    })?;
    publish_pass(&qb_home, &cache, &version).context(ProbeStage::CachePublication)
}

const VERSION_RECEIPT: &str = "last-version-read.json";
const VERSION_LOCK: &str = "last-version-read.lock";
const VERSION_LOCK_WAIT: Duration = Duration::from_secs(35);
const VERSION_MAX_AGE_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

#[derive(Debug, PartialEq, Eq)]
enum VersionGate {
    Known(String),
    TimeoutCache,
}

fn version_attempts(mut attempt: impl FnMut() -> std::io::Result<Vec<u8>>) -> Result<VersionGate> {
    use crate::launcher_label::{VersionFailure, version_failure};
    let mut first_timeout = false;
    for index in 0..2 {
        match attempt() {
            // Parse failures are terminal, even when a valid legacy pass exists.
            Ok(output) => return version(&output).map(VersionGate::Known),
            Err(error) => {
                let failure = version_failure(&error);
                if index == 1 {
                    if first_timeout && failure == VersionFailure::Timeout {
                        return Ok(VersionGate::TimeoutCache);
                    }
                    return Err(error.into());
                }
                if !matches!(failure, VersionFailure::Spawn | VersionFailure::Timeout) {
                    return Err(error.into());
                }
                first_timeout = failure == VersionFailure::Timeout;
            }
        }
    }
    unreachable!()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionReceipt {
    schema: u8,
    version: String,
    read_utc_ns: u64,
    fingerprint: Option<String>,
}
impl VersionReceipt {
    fn valid(&self) -> bool {
        self.schema == 1
            && version(self.version.as_bytes()).is_ok_and(|v| v == self.version)
            && self.fingerprint.as_ref().is_none_or(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
    }
}

fn utc_ns(now: SystemTime) -> Result<u64> {
    Ok(u64::try_from(now.duration_since(UNIX_EPOCH)?.as_nanos())?)
}

// Metadata identity, not code attestation: the fallback still uses a last-known
// version. The hash avoids persisting an executable pathname in the receipt.
fn program_fingerprint(program: &Path) -> Option<String> {
    let program = fs::canonicalize(program).ok()?;
    let metadata = fs::metadata(&program).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let path = program.as_os_str().as_encoded_bytes();
    let mut digest = Sha256::new();
    digest.update(b"qb-version-program-metadata-v1");
    digest.update(u64::try_from(path.len()).ok()?.to_be_bytes());
    digest.update(path);
    digest.update(metadata.len().to_be_bytes());
    digest.update(modified.to_be_bytes());
    Some(format!("{:x}", digest.finalize()))
}

fn wait_version_lock(
    file: &File,
    bound: Duration,
    mut contended: impl FnMut(),
) -> std::io::Result<()> {
    let began = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if began.elapsed() >= bound {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                contended();
                thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

// One receipt + one lock per managed home, not a directory scan or growing index.
// All cooperating readers hold this lock through the two bounded version attempts.
struct VersionMemo {
    _lock: File,
    path: PathBuf,
    prior: Option<VersionReceipt>,
}
impl VersionMemo {
    fn begin(root: &Path, base: &Path) -> Result<Self> {
        let lock = base.join(VERSION_LOCK);
        crate::sessions_guard::safe_path(root, &lock)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock)?;
        ensure!(lock.metadata()?.is_file(), "version lock type");
        // Two 15s probes plus 5s cleanup/publication headroom. Never wait
        // indefinitely for a hung peer; no lock is retained during tool probing.
        wait_version_lock(&lock, VERSION_LOCK_WAIT, || {})?;
        let path = base.join(VERSION_RECEIPT);
        crate::sessions_guard::safe_path(root, &path)?;
        let prior = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(metadata.is_file(), "version receipt type");
                let read = || -> Result<VersionReceipt> {
                    let mut bytes = Vec::new();
                    File::open(&path)?.take(1025).read_to_end(&mut bytes)?;
                    ensure!(bytes.len() <= 1024, "version receipt limit");
                    let receipt: VersionReceipt = serde_json::from_slice(&bytes)?;
                    ensure!(receipt.valid(), "version receipt format");
                    Ok(receipt)
                };
                let prior = read().ok();
                // Invalidate BEFORE reading a potentially newer version. A failed
                // new receipt write/crash cannot leave the old pass eligible.
                // Unlink instead of truncating a possibly hardlinked old record.
                fs::remove_file(&path)?;
                prior
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            _lock: lock,
            path,
            prior,
        })
    }
    fn publish(&self, receipt: &VersionReceipt) -> Result<()> {
        ensure!(receipt.valid(), "version receipt format");
        let bytes = serde_json::to_vec(receipt)?;
        ensure!(bytes.len() <= 1024, "version receipt limit");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    }
    fn restore(&self) -> Result<()> {
        if let Some(prior) = &self.prior {
            self.publish(prior)?;
        }
        Ok(())
    }
}

fn version_gate(
    program: &Path,
    root: &Path,
    base: &Path,
    attempt: impl FnMut() -> std::io::Result<Vec<u8>>,
    now: impl Fn() -> SystemTime,
) -> Result<VersionGate> {
    let memo = VersionMemo::begin(root, base)?;
    let before = program_fingerprint(program);
    let result = version_attempts(attempt);
    // This samples the actual read completion, not cache-use time for a retained
    // version. Failed parses never publish a new version or advance its age.
    let read_at = now();
    let after = program_fingerprint(program);
    match result {
        Ok(VersionGate::Known(version)) => {
            memo.publish(&VersionReceipt {
                schema: 1,
                version: version.clone(),
                read_utc_ns: utc_ns(read_at)?,
                fingerprint: if before == after { after } else { None },
            })?;
            Ok(VersionGate::Known(version))
        }
        Ok(VersionGate::TimeoutCache) => {
            memo.restore()?; // Retain original timestamp; a cache hit never renews it.
            let prior = memo.prior.as_ref().context("version receipt missing")?;
            let age = utc_ns(read_at)?
                .checked_sub(prior.read_utc_ns)
                .context("version receipt future")?;
            ensure!(age <= VERSION_MAX_AGE_NS, "version receipt expired");
            ensure!(
                before.is_some() && before == after && prior.fingerprint == after,
                "version receipt program changed"
            );
            let cache = base.join(cache_name(&prior.version));
            crate::sessions_guard::safe_path(root, &cache)?;
            ensure!(
                cached_pass(&cache, &prior.version),
                "version cached pass missing"
            );
            diagnostic("version-timeout-cache", true);
            Ok(VersionGate::TimeoutCache)
        }
        Err(error) => {
            memo.restore()?;
            Err(error)
        }
    }
}

fn diagnostic(stage: &'static str, failed: bool) {
    // Fixed stage names only: never native output, request bodies or error chains.
    if failed {
        eprintln!("qb-image-canary: {stage}");
    }
}

fn version(output: &[u8]) -> Result<String> {
    crate::launcher_label::parse_version_output(output).context("version format")
}

fn cache_name(version: &str) -> String {
    let hash = Sha256::digest(format!("{SCHEME}\0{version}"));
    format!("{SCHEME}-{hash:x}.json")
}

fn pass_matches(bytes: &[u8], version: &str) -> bool {
    bytes.len() <= 1024
        && serde_json::from_slice::<Value>(bytes)
            .is_ok_and(|value| value == json!({"version": version, "pass": true}))
}

fn cached_pass(path: &Path, version: &str) -> bool {
    let read = || -> Result<Vec<u8>> {
        ensure!(fs::symlink_metadata(path)?.is_file(), "cache type");
        let mut bytes = Vec::new();
        File::open(path)?.take(1025).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    read().is_ok_and(|bytes| pass_matches(&bytes, version))
}

fn publish_pass(root: &Path, path: &Path, version: &str) -> Result<()> {
    crate::sessions_guard::safe_path(root, path)?;
    if path.try_exists()? {
        if cached_pass(path, version) {
            return Ok(());
        }
        ensure!(fs::symlink_metadata(path)?.is_file(), "cache type");
        // Unlink, never truncate a possibly hardlinked existing file.
        fs::remove_file(path)?;
    }
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            crate::sessions_guard::safe_path(root, path)?;
            if cached_pass(path, version) {
                return Ok(());
            }
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    file.write_all(&serde_json::to_vec(
        &json!({"version": version, "pass": true}),
    )?)?;
    file.sync_all()?;
    Ok(())
}

struct Scratch {
    root: PathBuf,
    path: PathBuf,
}

impl Scratch {
    fn create(root: &Path, base: &Path) -> Result<Self> {
        // create_dir is exclusive. Never adopt or clean an older probe directory.
        for _ in 0..32 {
            let path = base.join(format!(
                "probe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            crate::sessions_guard::safe_path(root, &path)?;
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        root: root.to_owned(),
                        path,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("scratch collision")
    }

    fn cleanup(self) -> Result<()> {
        self.remove()
    }

    fn remove(&self) -> Result<()> {
        ensure!(
            self.path.parent() == Some(self.root.join("state/image-canary").as_path())
                && self
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("probe-")),
            "scratch anchor"
        );
        match self.path.try_exists()? {
            false => Ok(()),
            true => remove_owned(&self.root, &self.path, 0),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn remove_owned(root: &Path, path: &Path, depth: usize) -> Result<()> {
    ensure!(depth < 64, "scratch depth");
    crate::sessions_guard::safe_path(root, path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(!metadata.file_type().is_symlink(), "scratch link");
    if metadata.is_dir() {
        // Validate every entry; never recursively delete an unchecked subtree.
        for entry in fs::read_dir(path)? {
            remove_owned(root, &entry?.path(), depth + 1)?;
        }
        crate::sessions_guard::safe_path(root, path)?;
        fs::remove_dir(path)?;
    } else {
        ensure!(metadata.is_file(), "scratch type");
        fs::remove_file(path)?;
    }
    Ok(())
}

fn settings(port: u16) -> Vec<String> {
    let base = format!("http://127.0.0.1:{port}");
    vec![
        "model=\"gpt-6.1-sol\"".into(),
        "model_reasoning_effort=\"low\"".into(),
        "model_provider=\"qb\"".into(),
        format!("openai_base_url=\"{base}/v1\""),
        format!("chatgpt_base_url=\"{base}/denied\""),
        "check_for_update_on_startup=false".into(),
        "approval_policy=\"never\"".into(),
        "sandbox_mode=\"read-only\"".into(),
        "web_search=\"disabled\"".into(),
        "project_doc_max_bytes=0".into(),
        "project_root_markers=[\".qb-canary-root\"]".into(),
        "cli_auth_credentials_store=\"file\"".into(),
        "mcp_oauth_credentials_store=\"file\"".into(),
        "mcp_servers={}".into(),
        "orchestrator.mcp.enabled=false".into(),
        "orchestrator.skills.enabled=false".into(),
        "notify=[]".into(),
        "analytics.enabled=false".into(),
        "feedback.enabled=false".into(),
        "otel.exporter=\"none\"".into(),
        "otel.trace_exporter=\"none\"".into(),
        "otel.metrics_exporter=\"none\"".into(),
        "otel.log_user_prompt=false".into(),
        "features.image_generation=true".into(),
        "features.hooks=false".into(),
        "features.plugins=false".into(),
        "features.apps=false".into(),
        "features.shell_snapshot=false".into(),
        "features.skip_host_skill_discovery=true".into(),
        "model_providers.qb.name=\"OpenAI\"".into(),
        format!("model_providers.qb.base_url=\"{base}/v1\""),
        "model_providers.qb.env_key=\"QB_IMAGE_CANARY_KEY\"".into(),
        "model_providers.qb.requires_openai_auth=false".into(),
        "model_providers.qb.wire_api=\"responses\"".into(),
        "model_providers.qb.supports_websockets=false".into(),
        "model_providers.qb.request_max_retries=0".into(),
        "model_providers.qb.stream_max_retries=0".into(),
        "model_providers.qb.http_headers.x-openai-actor-authorization=\"qb-local\"".into(),
    ]
}

fn environment(path: &Path, port: u16) -> Vec<(&'static str, OsString)> {
    let proxy = OsString::from(format!("http://127.0.0.1:{port}"));
    let mut env = vec![];
    for &key in PATH_ENV {
        env.push((key, path.join(key).into_os_string()));
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        env.push((key, proxy.clone()));
    }
    for key in ["NO_PROXY", "no_proxy"] {
        env.push((key, "127.0.0.1".into()));
    }
    for key in [
        "QB_IMAGE_CANARY_KEY",
        "QB_IMAGE_CLIENT_KEY",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
    ] {
        env.push((key, FAKE_KEY.into()));
    }
    env.extend([
        (
            "OPENAI_BASE_URL",
            format!("http://127.0.0.1:{port}/v1").into(),
        ),
        ("CODEX_DISABLE_ANALYTICS", "1".into()),
        ("DO_NOT_TRACK", "1".into()),
        ("OTEL_SDK_DISABLED", "true".into()),
        ("RUST_LOG", "off".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        (
            "GIT_CONFIG_GLOBAL",
            path.join("empty-git-config").into_os_string(),
        ),
        ("GIT_CONFIG_COUNT", "0".into()),
        ("GIT_CONFIG_PARAMETERS", "".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
    ]);
    env
}

fn probe(program: &Path, path: &Path) -> Result<()> {
    let _pin = crate::sessions_guard::header_directory(path, path)?;
    fs::write(path.join(".qb-canary-root"), [])?;
    fs::write(path.join("empty-git-config"), [])?;
    let server = Server::start()?;
    let env = environment(path, server.port);
    for (key, value) in &env {
        if PATH_ENV.contains(key) {
            fs::create_dir(Path::new(value))?;
        }
    }
    let config = settings(server.port);
    // Generate from fixed values only. The user config is ignored by native;
    // the same settings are explicit CLI overrides.
    fs::write(path.join("CODEX_HOME/config.toml"), config.join("\n"))?;
    let mut args: Vec<OsString> = [
        "--no-daemon",
        "exec",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    for setting in config {
        args.extend(["-c".into(), setting.into()]);
    }
    args.extend(["--".into(), "Reply OK without calling any tools.".into()]);
    let native = crate::launcher_label::canary_run(program, &args, &env, path);
    let found = server.finish();
    probe_outcome(native, found)
}

fn probe_outcome(
    native: std::result::Result<(), crate::launcher_label::CanaryRunError>,
    found: Result<bool>,
) -> Result<()> {
    let native = native.map_err(|error| match error {
        crate::launcher_label::CanaryRunError::Spawn(_) => ProbeStage::NativeSpawn,
        crate::launcher_label::CanaryRunError::Wait(_) => ProbeStage::NativeWait,
        crate::launcher_label::CanaryRunError::Exit => ProbeStage::NativeExit,
    });
    if let Err(stage @ (ProbeStage::NativeSpawn | ProbeStage::NativeWait)) = native {
        return Err(stage.into());
    }
    // A server read/write timeout may itself cause a native nonzero exit.
    // Preserve that typed timeout instead of losing its single retry.
    let found = found.map_err(server_failure)?;
    native?;
    if !found {
        return Err(ProbeStage::ToolAbsent.into());
    }
    Ok(())
}

fn server_failure(error: anyhow::Error) -> ProbeStage {
    if error.downcast_ref::<ProbeStage>() == Some(&ProbeStage::ServerTimeout)
        || error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        })
    {
        ProbeStage::ServerTimeout
    } else {
        ProbeStage::Server
    }
}

struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<Result<bool>>>,
}

impl Server {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(17);
            let mut found = false;
            let mut count = 0;
            while !signal.load(Ordering::Acquire) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, peer)) => {
                        count += 1;
                        ensure!(
                            count <= 16 && peer.ip().is_loopback(),
                            "probe request limit"
                        );
                        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
                        stream.set_write_timeout(Some(Duration::from_millis(250)))?;
                        found |= serve(&mut stream, port, deadline)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            if !signal.load(Ordering::Acquire) && Instant::now() >= deadline {
                return Err(ProbeStage::ServerTimeout.into());
            }
            Ok(found)
        });
        Ok(Self {
            port,
            stop,
            worker: Some(worker),
        })
    }

    fn finish(mut self) -> Result<bool> {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .unwrap()
            .join()
            .map_err(|_| CanaryRefusal)?
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_deadline(reader: &mut impl Read, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err(ProbeStage::ServerTimeout.into());
        }
        let count = reader.read(bytes)?;
        ensure!(count > 0, "probe EOF");
        bytes = &mut bytes[count..];
    }
    Ok(())
}

fn serve(stream: &mut TcpStream, port: u16, deadline: Instant) -> Result<bool> {
    let mut reader = BufReader::new(&mut *stream);
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        ensure!(headers.len() < 16 * 1024, "header limit");
        let mut byte = [0];
        read_deadline(&mut reader, &mut byte, deadline)?;
        headers.push(byte[0]);
    }
    let text = std::str::from_utf8(&headers)?;
    let mut lines = text.split("\r\n");
    if lines.next() != Some("POST /v1/responses HTTP/1.1") {
        drop(reader);
        reply(stream, "403 Forbidden", "application/json", b"{}")?;
        // Native may attempt an ancillary catalog/update lookup through the
        // sink proxy. Deny it locally; only an advertised tool on the allowed
        // Responses request can make the probe pass.
        return Ok(false);
    }
    let mut length = None;
    let mut encoding = None;
    let mut host = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (key, value) = line.split_once(':').ok_or(CanaryRefusal)?;
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "content-length" => {
                ensure!(length.is_none(), "duplicate length");
                length = Some(value.parse::<usize>()?);
            }
            "content-encoding" => {
                ensure!(encoding.is_none(), "duplicate encoding");
                encoding = Some(value.to_ascii_lowercase());
            }
            "host" => {
                ensure!(host.is_none(), "duplicate host");
                host = Some(value);
            }
            "transfer-encoding" => bail!("unsupported transfer encoding"),
            _ => {}
        }
    }
    ensure!(
        host == Some(format!("127.0.0.1:{port}").as_str()),
        "nonlocal host"
    );
    let length = length.ok_or(CanaryRefusal)?;
    ensure!(length <= LIMIT, "body limit");
    let mut body = vec![0; length];
    read_deadline(&mut reader, &mut body, deadline)?;
    drop(reader);
    let found = advertised(&body, encoding.as_deref())?;
    // No tool call is ever returned, including when the image tool is present.
    let item = json!({"id":"msg_canary", "type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"OK", "annotations":[]}]});
    let events = [
        (
            "response.created",
            json!({"response":{"id":"resp_canary","status":"in_progress"}}),
        ),
        (
            "response.output_item.added",
            json!({"output_index":0,"item":{"id":"msg_canary","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        ),
        (
            "response.output_text.delta",
            json!({"item_id":"msg_canary","output_index":0,"content_index":0,"delta":"OK"}),
        ),
        (
            "response.output_item.done",
            json!({"output_index":0,"item":item}),
        ),
        (
            "response.completed",
            json!({"response":{"id":"resp_canary","status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
        ),
    ];
    let mut sse = String::new();
    for (name, mut payload) in events {
        payload["type"] = name.into();
        sse.push_str(&format!("event: {name}\ndata: {payload}\n\n"));
    }
    reply(stream, "200 OK", "text/event-stream", sse.as_bytes())?;
    Ok(found)
}

fn reply(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}

fn advertised(body: &[u8], encoding: Option<&str>) -> Result<bool> {
    ensure!(body.len() <= LIMIT, "encoded limit");
    let decoded = match encoding {
        None | Some("identity") => body.to_vec(),
        Some("zstd") => {
            let mut decoded = Vec::new();
            let mut decoder = zstd::stream::read::Decoder::new(body)?;
            decoder.window_log_max(23)?;
            decoder.take((LIMIT + 1) as u64).read_to_end(&mut decoded)?;
            decoded
        }
        _ => bail!("unsupported content encoding"),
    };
    ensure!(decoded.len() <= LIMIT, "decoded limit");
    let value: Value = serde_json::from_slice(&decoded)?;
    // Lite puts actual tool metadata in a typed additional_tools input item.
    // Never scan message text, instructions, or schemas for names.
    Ok(value
        .get("tools")
        .is_some_and(|tools| tool_match(tools, "", 0))
        || value
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"] == "additional_tools"
                        && item["role"] == "developer"
                        && item
                            .get("tools")
                            .is_some_and(|tools| tool_match(tools, "", 0))
                })
            }))
}

fn tool_match(value: &Value, prefix: &str, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    if let Some(values) = value.as_array() {
        return values
            .iter()
            .any(|value| tool_match(value, prefix, depth + 1));
    }
    let Some(object) = value.as_object() else {
        return false;
    };
    let kind = object.get("type").and_then(Value::as_str);
    let name = object.get("name").and_then(Value::as_str);
    let qualified = name.map(|name| {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}__{name}")
        }
    });
    if matches!(kind, Some("function" | "custom")) {
        if qualified.as_deref() == Some("image_gen__imagegen") {
            return true;
        }
        if name == Some("exec")
            && object
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(code_declaration)
        {
            return true;
        }
    }
    if kind == Some("function")
        && let Some(name) = object
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        && (if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}__{name}")
        }) == "image_gen__imagegen"
    {
        return true;
    }
    // Responses namespaces, nested function wrappers (older function shape),
    // and code-mode namespaces are the only traversed metadata containers.
    let next = if kind == Some("namespace") {
        qualified.as_deref().unwrap_or(prefix)
    } else {
        prefix
    };
    ["tools", "functions", "function"].iter().any(|key| {
        object
            .get(*key)
            .is_some_and(|child| tool_match(child, next, depth + 1))
    })
}

fn code_declaration(description: &str) -> bool {
    // Exact declaration observed in r12-provider-probe.py's captured metadata.
    // A prose mention of the name is insufficient.
    let compact: String = description.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .split("declareconsttools:{")
        .skip(1)
        .any(|declaration| {
            declaration.starts_with("image_gen__imagegen(args:{")
                && declaration
                    .split("};")
                    .next()
                    .is_some_and(|signature| signature.ends_with("}):Promise<unknown>;"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_fixture() -> Result<(tempfile::TempDir, PathBuf)> {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r1211-fixtures");
        fs::create_dir_all(&base)?;
        let home = tempfile::tempdir_in(fs::canonicalize(base)?)?;
        let program = home.path().join("synthetic-codex.exe");
        fs::write(&program, b"synthetic fixture, never executed")?;
        Ok((home, program))
    }
    fn gate(
        home: &Path,
        program: &Path,
        now: SystemTime,
        attempt: impl FnMut() -> std::io::Result<Vec<u8>>,
    ) -> Result<VersionGate> {
        let base = home.join("state/image-canary");
        let _guard = crate::sessions_guard::prepare_parent(home, &base)?;
        version_gate(program, home, &base, attempt, || now)
    }
    fn version_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }
    fn timeout() -> std::io::Error {
        crate::launcher_label::VersionFailure::Timeout.wrap(std::io::ErrorKind::TimedOut.into())
    }
    fn seed_version(home: &Path, program: &Path, now: SystemTime) -> Result<PathBuf> {
        assert_eq!(
            gate(home, program, now, || Ok(b"codex-cli 0.163.0".to_vec()))?,
            VersionGate::Known("0.163.0".into())
        );
        let pass = home.join("state/image-canary").join(cache_name("0.163.0"));
        publish_pass(home, &pass, "0.163.0")?;
        Ok(pass)
    }
    #[test]
    fn r1211_version_lock_waits_for_a_peer_but_has_a_finite_bound() -> Result<()> {
        let (home, _) = version_fixture()?;
        let path = home.path().join("manual-version.lock");
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        held.lock()?;
        let peer = OpenOptions::new().read(true).write(true).open(&path)?;
        assert_eq!(
            wait_version_lock(&peer, Duration::ZERO, || unreachable!())
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        let (entered, seen) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let mut notified = false;
            wait_version_lock(&peer, VERSION_LOCK_WAIT, || {
                if !notified {
                    notified = true;
                    entered.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                }
            })
            .unwrap();
        });
        seen.recv_timeout(Duration::from_secs(5))?;
        drop(held);
        release.send(())?;
        worker.join().unwrap();
        assert_eq!(VERSION_LOCK_WAIT, Duration::from_secs(35));
        Ok(())
    }

    #[test]
    fn r1211_two_wait_timeouts_use_valid_last_version_without_refreshing_age() -> Result<()> {
        let (home, program) = version_fixture()?;
        let at = version_time();
        seed_version(home.path(), &program, at)?;
        let receipt = home.path().join("state/image-canary").join(VERSION_RECEIPT);
        let original = fs::read(&receipt)?;
        let mut calls = 0;
        assert_eq!(
            gate(home.path(), &program, at + Duration::from_secs(60), || {
                calls += 1;
                Err(timeout())
            })?,
            VersionGate::TimeoutCache
        );
        assert_eq!(calls, 2);
        assert_eq!(fs::read(receipt)?, original);
        Ok(())
    }
    #[test]
    fn r1211_version_cache_age_exact_24h_and_clock_rollback() -> Result<()> {
        // Windows SystemTime is represented in 100 ns ticks; a 1 ns change
        // can round back to the same instant and cannot test either edge.
        const CLOCK_TICK_NS: u64 = 100;
        for (elapsed, accepted) in [
            (
                Duration::from_nanos(VERSION_MAX_AGE_NS - CLOCK_TICK_NS),
                true,
            ),
            (Duration::from_nanos(VERSION_MAX_AGE_NS), true),
            (
                Duration::from_nanos(VERSION_MAX_AGE_NS + CLOCK_TICK_NS),
                false,
            ),
        ] {
            let (home, program) = version_fixture()?;
            let at = version_time();
            assert_eq!(
                utc_ns(at + elapsed)? - utc_ns(at)?,
                elapsed.as_nanos() as u64
            );
            seed_version(home.path(), &program, at)?;
            assert_eq!(
                gate(home.path(), &program, at + elapsed, || Err(timeout())).is_ok(),
                accepted
            );
            assert!(
                gate(
                    home.path(),
                    &program,
                    at - Duration::from_nanos(CLOCK_TICK_NS),
                    || Err(timeout())
                )
                .is_err()
            );
        }
        Ok(())
    }
    #[test]
    fn r1211_missing_failed_wrong_version_and_changed_program_cache_reject() -> Result<()> {
        for case in 0..6 {
            let (home, program) = version_fixture()?;
            let at = version_time();
            let pass = seed_version(home.path(), &program, at)?;
            match case {
                0 => fs::remove_file(home.path().join("state/image-canary").join(VERSION_RECEIPT))?,
                1 => fs::remove_file(&pass)?,
                2 => fs::write(&pass, br#"{"version":"0.163.0","pass":false}"#)?,
                3 => fs::write(&pass, br#"{"version":"0.164.0","pass":true}"#)?,
                4 => fs::write(&program, b"different synthetic file length, never executed")?,
                _ => fs::write(
                    home.path().join("state/image-canary").join(VERSION_RECEIPT),
                    b"malformed",
                )?,
            }
            let mut calls = 0;
            assert!(
                gate(home.path(), &program, at, || {
                    calls += 1;
                    Err(timeout())
                })
                .is_err()
            );
            assert_eq!(calls, 2);
        }
        Ok(())
    }
    #[test]
    fn r1211_unparseable_version_is_never_retried_or_retained() -> Result<()> {
        let (home, program) = version_fixture()?;
        let at = version_time();
        let pass = seed_version(home.path(), &program, at)?;
        let receipt = home.path().join("state/image-canary").join(VERSION_RECEIPT);
        let original = fs::read(&receipt)?;
        for output in [
            b"invalid".to_vec(),
            b"codex-cli 0.164.0\nextra".to_vec(),
            vec![b'1'; 513],
            vec![255],
        ] {
            let mut calls = 0;
            assert!(
                gate(home.path(), &program, at + Duration::from_secs(1), || {
                    calls += 1;
                    Ok(output.clone())
                })
                .is_err()
            );
            assert_eq!(calls, 1);
            assert_eq!(fs::read(&receipt)?, original);
            assert!(cached_pass(&pass, "0.163.0"));
        }
        Ok(())
    }
    #[test]
    fn r1211_spawn_retry_and_mixed_errors_never_grant_timeout_fallback() -> Result<()> {
        use crate::launcher_label::VersionFailure;
        for failures in [
            [VersionFailure::Spawn, VersionFailure::Timeout],
            [VersionFailure::Timeout, VersionFailure::Spawn],
            [VersionFailure::Spawn, VersionFailure::Spawn],
        ] {
            let (home, program) = version_fixture()?;
            let at = version_time();
            seed_version(home.path(), &program, at)?;
            let mut index = 0;
            assert!(
                gate(home.path(), &program, at, || {
                    let failure = failures[index];
                    index += 1;
                    Err(failure.wrap(std::io::ErrorKind::TimedOut.into()))
                })
                .is_err()
            );
            assert_eq!(index, 2);
        }
        for failure in [VersionFailure::Spawn, VersionFailure::Timeout] {
            let mut calls = 0;
            assert_eq!(
                version_attempts(|| {
                    calls += 1;
                    if calls == 1 {
                        Err(failure.wrap(std::io::ErrorKind::Other.into()))
                    } else {
                        Ok(b"0.164.0".to_vec())
                    }
                })?,
                VersionGate::Known("0.164.0".into())
            );
            assert_eq!(calls, 2);
        }
        Ok(())
    }
    #[test]
    fn r1211_nonspawn_io_and_nonzero_class_are_terminal_even_if_timed_out_kind() {
        for kind in [
            std::io::ErrorKind::Other,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::InvalidData,
        ] {
            let mut calls = 0;
            assert!(
                version_attempts(|| {
                    calls += 1;
                    Err(kind.into())
                })
                .is_err()
            );
            assert_eq!(calls, 1);
        }
    }
    #[test]
    fn r1211_latest_parsed_version_without_pass_cannot_fall_back_to_older_pass() -> Result<()> {
        let (home, program) = version_fixture()?;
        let at = version_time();
        let pass = seed_version(home.path(), &program, at)?;
        assert_eq!(
            gate(home.path(), &program, at + Duration::from_secs(1), || Ok(
                b"0.164.0".to_vec()
            ))?,
            VersionGate::Known("0.164.0".into())
        );
        assert!(cached_pass(&pass, "0.163.0"));
        assert!(
            gate(home.path(), &program, at + Duration::from_secs(2), || Err(
                timeout()
            ))
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn r1211_legacy_operator_pass_keeps_exact_format_with_fresh_version_receipt() -> Result<()> {
        let (home, program) = version_fixture()?;
        let base = home.path().join("state/image-canary");
        fs::create_dir_all(&base)?;
        let pass = base.join(cache_name("0.163.0"));
        let original = b"{\"version\":\"0.163.0\",\"pass\":true}\n";
        fs::write(&pass, original)?;
        // Legacy pass alone cannot establish when a version was actually read.
        assert!(gate(home.path(), &program, version_time(), || Err(timeout())).is_err());
        assert_eq!(
            gate(home.path(), &program, version_time(), || Ok(
                b"0.163.0".to_vec()
            ))?,
            VersionGate::Known("0.163.0".into())
        );
        assert!(cached_pass(&pass, "0.163.0"));
        assert_eq!(fs::read(pass)?, original);
        Ok(())
    }

    #[test]
    fn retries_native_wait_and_server_timeout_once_then_succeeds() -> Result<()> {
        for stage in [ProbeStage::NativeWait, ProbeStage::ServerTimeout] {
            let mut attempts = 0;
            probe_with_retry(|| {
                attempts += 1;
                if attempts == 1 {
                    Err(stage.into())
                } else {
                    Ok(())
                }
            })?;
            assert_eq!(attempts, 2);
        }
        Ok(())
    }

    #[test]
    fn retry_stops_after_second_wait_or_timeout_failure() {
        for stage in [ProbeStage::NativeWait, ProbeStage::ServerTimeout] {
            let mut attempts = 0;
            let error = probe_with_retry(|| {
                attempts += 1;
                Err(stage.into())
            })
            .unwrap_err();
            assert_eq!(attempts, 2);
            assert_eq!(error.downcast_ref::<ProbeStage>(), Some(&stage));
        }
    }

    #[test]
    fn confirmed_absence_and_other_failures_never_retry() {
        for stage in [
            ProbeStage::ToolAbsent,
            ProbeStage::NativeSpawn,
            ProbeStage::NativeExit,
            ProbeStage::Server,
            ProbeStage::Setup,
            ProbeStage::Cleanup,
        ] {
            let mut attempts = 0;
            let error = probe_with_retry(|| {
                attempts += 1;
                Err(stage.into())
            })
            .unwrap_err();
            assert_eq!(attempts, 1, "{stage}");
            assert_eq!(error.downcast_ref::<ProbeStage>(), Some(&stage));
        }
        let mut attempts = 0;
        let error = probe_with_retry(|| {
            attempts += 1;
            Err::<(), anyhow::Error>(ProbeStage::NativeWait.into()).context(ProbeStage::Cleanup)
        })
        .unwrap_err();
        assert_eq!(attempts, 1, "cleanup failure must prevent retry");
        assert_eq!(
            error.downcast_ref::<ProbeStage>(),
            Some(&ProbeStage::Cleanup)
        );
    }

    #[test]
    fn confirmed_absence_after_timeout_never_gets_a_third_attempt() {
        let mut attempts = 0;
        let error = probe_with_retry(|| {
            attempts += 1;
            Err(if attempts == 1 {
                ProbeStage::ServerTimeout
            } else {
                ProbeStage::ToolAbsent
            }
            .into())
        })
        .unwrap_err();
        assert_eq!(attempts, 2);
        assert_eq!(
            error.downcast_ref::<ProbeStage>(),
            Some(&ProbeStage::ToolAbsent)
        );
    }

    #[test]
    fn server_timeouts_are_typed_and_protocol_errors_are_terminal() {
        for kind in [std::io::ErrorKind::TimedOut, std::io::ErrorKind::WouldBlock] {
            assert_eq!(
                server_failure(std::io::Error::from(kind).into()),
                ProbeStage::ServerTimeout
            );
        }
        let mut byte = [0];
        let error = read_deadline(&mut &b"x"[..], &mut byte, Instant::now()).unwrap_err();
        assert_eq!(server_failure(error), ProbeStage::ServerTimeout);
        assert_eq!(
            server_failure(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into()),
            ProbeStage::Server
        );
        assert_eq!(
            server_failure(advertised(b"broken", None).unwrap_err()),
            ProbeStage::Server
        );
    }

    #[test]
    fn server_timeout_survives_native_nonzero_exit_and_absence_requires_success() {
        use crate::launcher_label::CanaryRunError;
        let error = probe_outcome(
            Err(CanaryRunError::Exit),
            Err(ProbeStage::ServerTimeout.into()),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProbeStage>(),
            Some(&ProbeStage::ServerTimeout)
        );
        let error = probe_outcome(Err(CanaryRunError::Exit), Ok(false)).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProbeStage>(),
            Some(&ProbeStage::NativeExit)
        );
        let error = probe_outcome(Ok(()), Ok(false)).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProbeStage>(),
            Some(&ProbeStage::ToolAbsent)
        );
        let error = probe_outcome(
            Err(CanaryRunError::Wait(std::io::ErrorKind::TimedOut.into())),
            Ok(true),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProbeStage>(),
            Some(&ProbeStage::NativeWait)
        );
        assert!(probe_outcome(Ok(()), Ok(true)).is_ok());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "runs pinned native 0.163 with isolated homes and loopback only"]
    fn fresh_native_0163_canary_publishes_legacy_cache() -> Result<()> {
        let program = Path::new(
            "C:/Users/SampleUser/AppData/Roaming/npm/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe",
        );
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r12.1/canary");
        fs::create_dir_all(&base)?;
        let base = fs::canonicalize(base)?;
        let home = tempfile::Builder::new()
            .prefix("native-check-")
            .tempdir_in(base)?;
        assert_eq!(
            version(&crate::launcher_label::version_output(program)?)?,
            "0.163.0"
        );
        check_inner(program, home.path())?;
        let cache = home
            .path()
            .join("state/image-canary")
            .join(cache_name("0.163.0"));
        assert!(cached_pass(&cache, "0.163.0"));
        let base = cache.parent().unwrap();
        let receipt: VersionReceipt =
            serde_json::from_slice(&fs::read(base.join(VERSION_RECEIPT))?)?;
        assert!(receipt.valid());
        assert_eq!(receipt.version, "0.163.0");
        assert!(base.join(VERSION_LOCK).is_file());
        assert_eq!(
            fs::read_dir(base)?.count(),
            3,
            "only legacy pass, version receipt and version lock remain"
        );
        Ok(())
    }

    #[test]
    fn operator_cache_keeps_exact_legacy_name_and_record() -> Result<()> {
        assert_eq!(
            cache_name("0.163.0"),
            "d23-image-discovery-1-5897d33ae539eec6ed68de926547f2a7b57aae9f90823c91e13be20a07a96184.json"
        );
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r12.1/canary");
        fs::create_dir_all(&base)?;
        let base = fs::canonicalize(base)?;
        let home = tempfile::Builder::new()
            .prefix("operator-cache-")
            .tempdir_in(base)?;
        let cache = home.path().join(cache_name("0.163.0"));
        let record = b"{\"version\":\"0.163.0\",\"pass\":true}\n";
        fs::write(&cache, record)?;
        assert!(cached_pass(&cache, "0.163.0"));
        publish_pass(home.path(), &cache, "0.163.0")?;
        assert_eq!(fs::read(&cache)?, record);
        Ok(())
    }

    #[test]
    fn denied_ancillary_proxy_requests_never_forward_or_replace_tool_evidence() -> Result<()> {
        let server = Server::start()?;
        let exchange = |request: &[u8]| -> Result<Vec<u8>> {
            let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, server.port))?;
            socket.set_read_timeout(Some(Duration::from_secs(2)))?;
            socket.write_all(request)?;
            let mut response = Vec::new();
            socket.read_to_end(&mut response)?;
            Ok(response)
        };
        let denied =
            exchange(b"CONNECT example.invalid:443 HTTP/1.1\r\nHost: example.invalid:443\r\n\r\n")?;
        assert!(denied.starts_with(b"HTTP/1.1 403"));
        let body = br#"{"tools":[{"type":"function","name":"image_gen__imagegen"}]}"#;
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n\r\n{}",
            server.port,
            body.len(),
            std::str::from_utf8(body)?
        );
        assert!(exchange(request.as_bytes())?.starts_with(b"HTTP/1.1 200"));
        assert!(server.finish()?);
        let denied_only = Server::start()?;
        let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, denied_only.port))?;
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        socket.write_all(b"CONNECT example.invalid:443 HTTP/1.1\r\n\r\n")?;
        let mut response = Vec::new();
        socket.read_to_end(&mut response)?;
        assert!(response.starts_with(b"HTTP/1.1 403"));
        assert!(
            !denied_only.finish()?,
            "denial alone never proves tool availability"
        );
        Ok(())
    }

    #[test]
    fn qualified_and_nested_captured_tools_match() {
        for tools in [
            json!([{"type":"function","name":"image_gen__imagegen"}]),
            json!([{"type":"namespace","name":"image_gen","tools":[{"type":"function","name":"imagegen"}]}]),
            json!([{"type":"function","function":{"name":"image_gen__imagegen"}}]),
        ] {
            assert!(
                advertised(&serde_json::to_vec(&json!({"tools":tools})).unwrap(), None).unwrap()
            );
        }
    }

    #[test]
    fn captured_code_mode_declaration_matches() {
        let tools = json!([{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec","description":"exec tool declaration:\n```ts\ndeclare const tools: { image_gen__imagegen(args: {\n  num_last_images_to_include?: number | null;\n  prompt: string;\n  referenced_image_paths?: Array<string> | null;\n  // Whether the output should have a transparent background. Defaults to false.\n  transparent_background?: boolean;\n}): Promise<unknown>; };\n```"}]}]);
        assert!(tool_match(&tools, "", 0));
        let lite = json!({"input":[{"type":"additional_tools","role":"developer","tools":tools}]});
        assert!(advertised(&serde_json::to_vec(&lite).unwrap(), None).unwrap());
    }

    #[test]
    fn prompt_schema_mentions_and_wrong_namespace_do_not_match() {
        for payload in [
            json!({"input":"image_gen__imagegen", "tools":[]}),
            json!({"tools":[{"type":"function","name":"image_gen__imagegen_extra"}]}),
            json!({"tools":[{"type":"namespace","name":"other","tools":[{"type":"function","name":"imagegen"}]}]}),
            json!({"tools":[{"type":"function","name":"exec","description":"image_gen__imagegen(args: {})"}]}),
            json!({"tools":[{"type":"function","name":"other","parameters":{"type":"function","name":"image_gen__imagegen"}}]}),
            json!({"input":[{"type":"message","role":"user","tools":[{"type":"function","name":"image_gen__imagegen"}]}]}),
        ] {
            assert!(!advertised(&serde_json::to_vec(&payload).unwrap(), None).unwrap());
        }
    }

    #[test]
    fn bounded_zstd_side_copy_matches_and_rejects_expansion() {
        let body = br#"{"tools":[{"type":"function","name":"image_gen__imagegen"}]}"#;
        let compressed = zstd::stream::encode_all(&body[..], 1).unwrap();
        assert!(advertised(&compressed, Some("zstd")).unwrap());
        let huge = zstd::stream::encode_all(vec![b' '; LIMIT + 1].as_slice(), 1).unwrap();
        assert!(advertised(&huge, Some("zstd")).is_err());
        assert!(advertised(body, Some("gzip")).is_err());
        assert!(advertised(b"broken", None).is_err());
    }

    #[test]
    fn cache_requires_same_version_and_pass_only() {
        let record = br#"{"version":"0.163.0","pass":true}"#;
        assert!(pass_matches(record, "0.163.0"));
        assert!(!pass_matches(record, "0.164.0"));
        assert!(!pass_matches(
            br#"{"version":"0.163.0","pass":false}"#,
            "0.163.0"
        ));
        assert!(!pass_matches(b"partial", "0.163.0"));
        assert_ne!(cache_name("0.163.0"), cache_name("0.164.0"));
        assert!(cache_name("0.163.0").starts_with(SCHEME));
    }

    #[test]
    fn version_normalizes_and_never_accepts_extra_output() {
        assert_eq!(version(b"codex-cli 0.163.0\r\n").unwrap(), "0.163.0");
        assert_eq!(version(b"0.999.0-beta.1").unwrap(), "0.999.0-beta.1");
        assert!(version(b"codex-cli 0.163.0\nsecret").is_err());
        assert!(version(&vec![b'1'; 513]).is_err());
    }

    #[test]
    fn refusal_is_fixed_and_has_fallback() {
        let error: anyhow::Error = CanaryRefusal.into();
        assert_eq!(format!("{error:#}"), REFUSAL);
        assert!(REFUSAL.contains("codex-mz"));
    }

    #[test]
    fn stalled_or_closed_request_cannot_outlive_deadline() {
        let mut byte = [0];
        assert!(read_deadline(&mut &b"x"[..], &mut byte, Instant::now()).is_err());
        assert!(
            read_deadline(
                &mut &b""[..],
                &mut byte,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn settings_are_toml_and_only_use_numeric_loopback() {
        for setting in settings(12345) {
            assert!(
                setting.parse::<toml_edit::DocumentMut>().is_ok(),
                "{setting}"
            );
            assert!(!setting.contains("https://"));
        }
        let env = environment(Path::new("isolated"), 12345);
        assert!(env.iter().any(|(key, value)| *key == "CODEX_HOME"
            && value == Path::new("isolated").join("CODEX_HOME").as_os_str()));
        assert!(
            env.iter()
                .any(|(key, value)| *key == "NO_PROXY" && value == "127.0.0.1")
        );
    }
}
