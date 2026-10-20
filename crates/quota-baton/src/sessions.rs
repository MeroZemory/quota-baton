//! Journaled Codex session imports. No rollout contents appear in diagnostics.
use crate::{sessions_db as db, sessions_discovery as discovery, sessions_guard as guard};
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Subcommand)]
pub(crate) enum SessionsCommand {
    #[command(
        about = "기존 대화를 qb로 가져옵니다",
        override_usage = "quota-baton sessions import --from <HOME> [<UUID> | --all | --list] [--apply]"
    )]
    Import {
        #[arg(long)]
        from: PathBuf,
        #[arg(conflicts_with_all=["all", "list"])]
        id: Option<String>,
        #[arg(long, conflicts_with = "list")]
        all: bool,
        #[arg(long)]
        list: bool,
        #[arg(long, conflicts_with = "list")]
        apply: bool,
    },
    #[command(about = "가져온 대화의 연결만 되돌립니다")]
    Unimport {
        id: String,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct Link {
    source: PathBuf,
    target: PathBuf,
    identity: guard::FileIdentity,
    length: u64,
    owned: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Import {
    source_home: PathBuf,
    target_home: PathBuf,
    files: Vec<Link>,
    threads: Vec<String>,
    database: String,
    index: Vec<String>,
    complete: bool,
}
#[derive(Default, Serialize, Deserialize)]
struct Journal {
    schema: u8,
    imports: BTreeMap<String, Import>,
}

struct Store {
    home: PathBuf,
    target: PathBuf,
    search_homes: Vec<PathBuf>,
}
impl Store {
    fn open(home: &Path) -> Result<Self> {
        let path = home.join("journal/wiring.json");
        guard::safe_path(home, &path)?;
        let bytes =
            fs::read(path).context("전용 Codex 홈 연결이 없습니다. 먼저 wire를 실행하세요")?;
        ensure!(bytes.len() <= 16 * 1024 * 1024, "연결 기록이 너무 큽니다");
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("연결 기록을 읽을 수 없습니다"))?;
        ensure!(
            value["mode"] == "dedicated"
                && value["install_completed"] == true
                && value["schema"] == 3,
            "전용 Codex 홈 연결을 먼저 완료하세요"
        );
        let target = PathBuf::from(value["home"].as_str().context("전용 홈 경로가 없습니다")?);
        ensure!(
            target.is_absolute() && target.is_dir(),
            "전용 Codex 홈이 없습니다"
        );
        guard::safe_path(&target, &target)?;
        let user = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        let search_homes = user.as_deref().map(known_homes).unwrap_or_default();
        Ok(Self {
            home: home.to_owned(),
            target: fs::canonicalize(target)?,
            search_homes,
        })
    }
    fn path(&self) -> PathBuf {
        self.home.join("journal/sessions-import.json")
    }
    fn load(&self) -> Result<Journal> {
        let path = self.path();
        guard::safe_path(&self.home, &path)?;
        let bytes = match fs::read(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Journal {
                    schema: 1,
                    ..Default::default()
                });
            }
            Err(e) => return Err(e.into()),
        };
        ensure!(
            bytes.len() <= 64 * 1024 * 1024,
            "가져오기 기록이 너무 큽니다"
        );
        let envelope: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("가져오기 기록이 올바르지 않습니다"))?;
        let payload = crate::cutover::unprotect(
            envelope["payload"]
                .as_str()
                .context("가져오기 보호 기록이 없습니다")?,
        )?;
        let journal: Journal = serde_json::from_slice(&payload)
            .map_err(|_| anyhow::anyhow!("가져오기 기록이 올바르지 않습니다"))?;
        ensure!(journal.schema == 1, "지원하지 않는 가져오기 기록입니다");
        for (id, entry) in &journal.imports {
            ensure!(
                discovery::valid_id(id) && entry.target_home == self.target,
                "가져오기 기록의 범위가 다릅니다"
            );
            for link in &entry.files {
                guard::safe_path(&self.target.join("sessions"), &link.target)?;
                guard::safe_path(&entry.source_home, &link.source)?;
            }
        }
        Ok(journal)
    }
    fn save(&self, journal: &Journal) -> Result<()> {
        guard::safe_path(&self.home, &self.path())?;
        let payload = crate::cutover::protect(&serde_json::to_vec(journal)?)?;
        crate::cutover::atomic_bytes(
            &self.path(),
            &serde_json::to_vec_pretty(&serde_json::json!({"schema":1,"payload":payload}))?,
        )
    }
    fn lock(&self) -> Result<File> {
        let path = self.home.join("journal/sessions-import.lock");
        guard::safe_path(&self.home, &path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock()
            .map_err(|_| anyhow::anyhow!("다른 대화 가져오기 작업이 실행 중입니다"))?;
        Ok(file)
    }
    fn probes(
        &self,
        source: &Path,
        threads: &[String],
        initial: &[PathBuf],
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let mut homes = vec![source.to_owned(), self.target.clone()];
        let mut names = initial.to_vec();
        for home in &self.search_homes {
            let canonical = fs::canonicalize(home)?;
            if homes.contains(&canonical) {
                continue;
            }
            let catalog = discovery::catalog(&canonical)?;
            let mut found = false;
            for id in threads {
                if catalog.threads.contains_key(id) {
                    names.extend(catalog.chain(id)?.iter().map(|f| f.path.clone()));
                    found = true;
                }
            }
            if found {
                homes.push(canonical);
            }
        }
        homes.sort();
        homes.dedup();
        names.sort();
        names.dedup();
        Ok((homes, names))
    }
}

fn index_plan(target: &Path, database: &db::DbPlan) -> Result<Vec<String>> {
    let path = target.join("session_index.jsonl");
    guard::safe_path(target, &path)?;
    let current = match fs::read_to_string(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => bail!("대화 인덱스를 읽을 수 없습니다"),
    };
    let present: BTreeSet<_> = current
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|v| v["id"].as_str().map(str::to_owned))
        .collect();
    let mut result = Vec::new();
    for thread in &database.threads {
        if !present.contains(&thread.id)
            && let Some(entry) = &thread.latest_index_entry
        {
            result.push(serde_json::to_string(entry)?);
        }
    }
    Ok(result)
}

fn update_index(target: &Path, entries: &[String], remove: bool) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let path = target.join("session_index.jsonl");
    guard::safe_path(target, &path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if !remove {
        options.create(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(5);
    }
    let mut file = match options.open(&path) {
        Ok(f) => f,
        Err(e) if remove && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => bail!("다른 작업이 대화 인덱스를 사용 중입니다"),
    };
    ensure!(
        file.metadata()?.len() <= 64 * 1024 * 1024,
        "대화 인덱스가 너무 큽니다"
    );
    let mut bytes = String::new();
    file.read_to_string(&mut bytes)
        .map_err(|_| anyhow::anyhow!("대화 인덱스를 읽을 수 없습니다"))?;
    let output = if remove {
        bytes
            .split_inclusive('\n')
            .filter(|line| {
                !entries
                    .iter()
                    .any(|s| s == line.trim_end_matches(['\r', '\n']))
            })
            .collect::<String>()
    } else {
        let mut output = bytes.clone();
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        for entry in entries {
            if !bytes.lines().any(|line| line == entry) {
                output.push_str(entry);
                output.push('\n');
            }
        }
        output
    };
    atomic_index(&path, output.as_bytes())
}

// FILE_RENAME_POSIX_SEMANTICS permits replacing the old index while our guard
// still denies other writers. A classic ReplaceIfExists rename refuses open
// targets on Windows. Keep the old handle alive until publication completes.
#[cfg(windows)]
fn atomic_index(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use windows::Win32::{
        Foundation::{GENERIC_WRITE, HANDLE},
        Storage::FileSystem::{
            DELETE, FILE_RENAME_INFO, FileRenameInfoEx, SetFileInformationByHandle,
        },
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("qb-index-{}-{serial}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE.0 | DELETE.0)
        .share_mode(0)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let name = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let length = u32::try_from(name.len() * 2)?;
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = offset + length as usize + 2;
    let mut buffer = vec![
        std::mem::MaybeUninit::<FILE_RENAME_INFO>::zeroed();
        size.div_ceil(std::mem::size_of::<FILE_RENAME_INFO>())
    ];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.Flags = 3; // REPLACE_IF_EXISTS | POSIX_SEMANTICS
        (*info).RootDirectory = HANDLE::default();
        (*info).FileNameLength = length;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            info.cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfoEx,
            info.cast(),
            u32::try_from(size)?,
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "대화 인덱스를 원자적으로 교체하지 못했습니다. 원본 인덱스는 보존했습니다"
            )
        })?;
    }
    Ok(())
}
#[cfg(not(windows))]
fn atomic_index(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::cutover::atomic_bytes(path, bytes)
}

fn preview(id: &str, files: &[discovery::Rollout], target: &Path) -> Result<()> {
    crate::print_stdout(format_args!(
        "대화 {id}: 기록 {}개를 연결합니다",
        files.len()
    ))?;
    for item in files {
        crate::print_stdout(format_args!(
            "{} → {}",
            item.path.display(),
            target.join("sessions").join(&item.relative).display()
        ))?;
    }
    crate::print_stdout(format_args!("계속하기: codex-qb resume {id}"))?;
    Ok(())
}

fn goal_status(value: &str) -> &'static str {
    match value {
        "active" => "진행 중",
        "usage_limited" => "사용량 한도 대기",
        "paused" => "일시정지",
        "blocked" => "외부 조건 대기",
        "budget_limited" => "예산 소진",
        "complete" => "완료",
        _ => "알 수 없음",
    }
}

fn verify_links(entry: &Import, growth: bool) -> Result<()> {
    for link in &entry.files {
        guard::safe_path(&entry.source_home, &link.source)?;
        guard::safe_path(&entry.target_home, &link.target)?;
        let source = guard::identity(&link.source)?;
        ensure!(
            source == link.identity && guard::identity(&link.target)? == source,
            "대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요"
        );
        ensure!(
            !link.source.with_extension("jsonl.zst").exists()
                && !link.target.with_extension("jsonl.zst").exists(),
            "압축된 대화 기록이 있습니다. sessions unimport 후 다시 가져오세요"
        );
        if growth {
            ensure!(
                fs::metadata(&link.target)?.len() == link.length,
                "가져온 뒤 대화 기록이 변경되었습니다. 확인 후 --force를 사용하세요"
            );
        }
    }
    for id in &entry.threads {
        ensure!(
            db::thread_provider(&entry.target_home, id)?.is_none_or(|p| p == "openai"),
            "대화 제공자가 변경되었습니다. sessions unimport 후 다시 가져오세요"
        );
        let source = db::thread_path(&entry.source_home, id)?;
        let target = db::thread_path(&entry.target_home, id)?;
        if let (Some(source), Some(target)) = (source, target) {
            ensure!(
                source.file_name() == target.file_name(),
                "활성 대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요"
            );
        }
    }
    Ok(())
}

fn known_homes(user: &Path) -> Vec<PathBuf> {
    let mut result = vec![user.join(".codex")];
    for root in [
        user.join(".codex-acct"),
        user.join("codex-accounts"),
        user.join(".orca/codex-accounts"),
    ] {
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    result.push(if root.ends_with("codex-accounts") {
                        entry.path().join("home")
                    } else {
                        entry.path()
                    });
                }
            }
        }
    }
    result.retain(|p| p.is_dir());
    result.sort();
    result.dedup();
    result
}

pub(crate) fn run(home: &Path, command: SessionsCommand) -> Result<()> {
    let store = Store::open(home)?;
    match command {
        SessionsCommand::Import {
            from,
            id,
            all,
            list,
            apply,
        } => {
            ensure!(
                id.is_some() || all || list,
                "대화 ID, --all 또는 --list를 지정하세요"
            );
            let catalog = discovery::catalog(&from)?;
            if list {
                let providers = db::threads(&catalog.home)?
                    .into_iter()
                    .map(|t| (t.id, t.model_provider))
                    .collect::<BTreeMap<_, _>>();
                for id in catalog.threads.keys() {
                    crate::print_stdout(format_args!(
                        "{id}{}",
                        if providers.get(id).is_some_and(|p| p != "openai") {
                            " · openai 제공자가 아님"
                        } else {
                            ""
                        }
                    ))?;
                }
                return Ok(());
            }
            let ids = if all {
                catalog.threads.keys().cloned().collect()
            } else {
                vec![id.unwrap().to_ascii_lowercase()]
            };
            let mut imported = 0;
            let mut skipped = 0;
            for id in ids {
                match import_one(&store, &catalog, &id, apply, all, true) {
                    Ok(true) => imported += 1,
                    Ok(false) => skipped += 1,
                    Err(error) if all => {
                        skipped += 1;
                        // Internal failures may include sqlite contents. Never echo them.
                        let _ = error;
                        crate::print_stdout(format_args!(
                            "{id}: 건너뜀 (사용 중이거나 가져오기 조건을 만족하지 않습니다)"
                        ))?;
                    }
                    Err(error) => return Err(error),
                }
            }
            crate::print_stdout(format_args!(
                "{} {imported}개 · 건너뜀 {skipped}개",
                if apply {
                    "가져옴"
                } else {
                    "가져올 대화"
                }
            ))?;
            Ok(())
        }
        SessionsCommand::Unimport { id, apply, force } => unimport(&store, &id, apply, force),
    }
}

fn import_one(
    store: &Store,
    catalog: &discovery::Catalog,
    id: &str,
    apply: bool,
    skip_existing: bool,
    announce: bool,
) -> Result<bool> {
    import_one_using(
        store,
        catalog,
        id,
        apply,
        skip_existing,
        announce,
        |a, b| fs::hard_link(a, b),
    )
}

fn import_one_using(
    store: &Store,
    catalog: &discovery::Catalog,
    id: &str,
    apply: bool,
    skip_existing: bool,
    announce: bool,
    linker: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<bool> {
    let _lock = if apply { Some(store.lock()?) } else { None };
    let mut journal = store.load()?;
    if let Some(entry) = journal.imports.get(id) {
        let names = entry
            .files
            .iter()
            .flat_map(|f| [f.source.clone(), f.target.clone()])
            .filter(|p| p.exists())
            .collect::<Vec<_>>();
        let (homes, names) = store.probes(&entry.source_home, &entry.threads, &names)?;
        let _guards = guard::probe_all(&homes, &entry.threads, &names)?;
        verify_links(entry, false)?;
        if entry.complete {
            if announce {
                crate::print_stdout(format_args!("{id}: 이미 가져온 대화입니다"))?;
            }
            return Ok(!skip_existing);
        }
        bail!("중단된 가져오기가 있습니다. sessions unimport로 되돌린 뒤 다시 시도하세요");
    }
    let files = catalog.chain(id)?;
    ensure!(catalog.home != store.target, "원본과 대상 홈이 같습니다");
    discovery::refuse_recent(&files, SystemTime::now())?;
    let threads: Vec<_> = files
        .iter()
        .map(|f| f.thread.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let names: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
    let (homes, names) = store.probes(&catalog.home, &threads, &names)?;
    let _guards = guard::probe_all(&homes, &threads, &names)?;
    if announce {
        preview(id, &files, &store.target)?;
    }
    let mut database = db::plan(&catalog.home, &store.target, &threads)?;
    for id in &threads {
        if database
            .threads
            .iter()
            .any(|t| &t.id == id && t.thread.is_none())
        {
            let item = files
                .iter()
                .rev()
                .find(|f| &f.thread == id)
                .context("대화 메타데이터가 없습니다")?;
            let mut fields = item.metadata.clone();
            let title = database
                .threads
                .iter()
                .find(|t| &t.id == id)
                .and_then(|t| t.latest_index_entry.as_ref())
                .and_then(|v| v["thread_name"].as_str())
                .unwrap_or("");
            fields.insert("title".into(), db::DbValue::Text(title.as_bytes().to_vec()));
            let info = db::ThreadInfo {
                id: id.clone(),
                rollout_path: item.path.clone(),
                model_provider: "openai".into(),
            };
            db::add_synthetic_thread(&mut database, &info, fields).map_err(|_| {
                anyhow::anyhow!("기록에서 필요한 대화 메타데이터를 복구하지 못했습니다")
            })?;
        }
    }
    if announce {
        for (thread, from, to) in db::summaries(&database) {
            crate::print_stdout(format_args!(
                "목표 {thread}: {} → {}",
                goal_status(&from),
                goal_status(&to)
            ))?;
        }
    }
    if !apply {
        return Ok(true);
    }
    let mut links = Vec::new();
    for file in &files {
        let target = store.target.join("sessions").join(&file.relative);
        guard::safe_path(&store.target, &target)?;
        let identity = guard::identity(&file.path)?;
        let owned = if target.exists() {
            ensure!(
                guard::identity(&target)? == identity,
                "대상에 다른 대화 기록이 있습니다"
            );
            false
        } else {
            true
        };
        links.push(Link {
            source: file.path.clone(),
            target,
            identity,
            length: file.bytes,
            owned,
        });
    }
    let index = index_plan(&store.target, &database)?;
    let record = Import {
        source_home: catalog.home.clone(),
        target_home: store.target.clone(),
        files: links,
        threads,
        database: crate::cutover::protect(&serde_json::to_vec(&database)?)?,
        index,
        complete: false,
    };
    journal.imports.insert(id.into(), record.clone());
    store.save(&journal)?;
    for link in &record.files {
        if link.owned {
            if let Some(parent) = link.target.parent() {
                fs::create_dir_all(parent)?;
            }
            guard::safe_path(&store.target, &link.target)?;
            linker(&link.source,&link.target).map_err(|_|anyhow::anyhow!("하드링크를 만들 수 없습니다. 같은 드라이브의 홈을 사용하세요. 기록은 복사하지 않았습니다"))?;
        }
    }
    db::apply(&database)?;
    update_index(&store.target, &record.index, false)?;
    journal.imports.get_mut(id).unwrap().complete = true;
    store.save(&journal)?;
    if announce {
        crate::print_stdout(format_args!(
            "주의: 두 홈의 대화 기록은 같은 파일입니다. 양쪽에서 동시에 실행하면 기록이 손상될 수 있습니다."
        ))?;
    }
    Ok(true)
}

fn unimport(store: &Store, id: &str, apply: bool, force: bool) -> Result<()> {
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    let _lock = if apply { Some(store.lock()?) } else { None };
    let mut journal = store.load()?;
    let entry = journal
        .imports
        .get(id)
        .context("가져온 대화가 아닙니다")?
        .clone();
    let names: Vec<_> = entry
        .files
        .iter()
        .flat_map(|f| [f.source.clone(), f.target.clone()])
        .filter(|p| p.exists())
        .collect();
    let (homes, names) = store.probes(&entry.source_home, &entry.threads, &names)?;
    let _guards = guard::probe_all(&homes, &entry.threads, &names)?;
    for link in &entry.files {
        if link.target.exists() {
            ensure!(
                guard::identity(&link.target)? == link.identity,
                "대상 파일 소유권이 달라 되돌릴 수 없습니다"
            );
            if !force {
                ensure!(
                    fs::metadata(&link.target)?.len() == link.length,
                    "가져온 뒤 기록이 변경되었습니다. 확인 후 --force를 사용하세요"
                );
            }
        }
    }
    crate::print_stdout(format_args!(
        "대화 {id}: 가져온 연결 {}개를 되돌립니다. 원본 기록은 남습니다.",
        entry.files.iter().filter(|f| f.owned).count()
    ))?;
    if !apply {
        return Ok(());
    }
    let mut database: db::DbPlan =
        serde_json::from_slice(&crate::cutover::unprotect(&entry.database)?)
            .map_err(|_| anyhow::anyhow!("가져오기 메타데이터를 읽을 수 없습니다"))?;
    let mut retained = BTreeSet::new();
    let mut retained_index = BTreeSet::new();
    // Fork imports can share ancestors. Transfer journal ownership to a surviving
    // import before removing this one; never strand a still-used linked prefix.
    for (other_id, other) in journal
        .imports
        .iter_mut()
        .filter(|(other_id, _)| other_id.as_str() != id)
    {
        let _ = other_id;
        for link in entry.files.iter().filter(|f| f.owned) {
            if let Some(successor) = other.files.iter_mut().find(|f| f.target == link.target) {
                ensure!(
                    successor.identity == link.identity,
                    "공유한 대화 기록 소유권이 다릅니다"
                );
                successor.owned = true;
                retained.insert(link.target.clone());
            }
        }
        let shared: BTreeSet<_> = other
            .threads
            .iter()
            .filter(|t| entry.threads.contains(t))
            .cloned()
            .collect();
        if !shared.is_empty() {
            let mut other_db: db::DbPlan =
                serde_json::from_slice(&crate::cutover::unprotect(&other.database)?)?;
            for original in &mut database.threads {
                if shared.contains(&original.id)
                    && let Some(next) = other_db.threads.iter_mut().find(|t| t.id == original.id)
                {
                    if let (Some(before), Some(after)) = (&mut original.thread, &mut next.thread)
                        && before.owned
                    {
                        after.owned = true;
                        before.owned = false;
                    }
                    if let (Some(before), Some(after)) = (&mut original.goal, &mut next.goal)
                        && before.owned
                    {
                        after.owned = true;
                        before.owned = false;
                    }
                }
            }
            other.database = crate::cutover::protect(&serde_json::to_vec(&other_db)?)?;
            for line in &entry.index {
                let value: serde_json::Value = serde_json::from_str(line)?;
                if value["id"].as_str().is_some_and(|id| shared.contains(id)) {
                    if !other.index.contains(line) {
                        other.index.push(line.clone());
                    }
                    retained_index.insert(line.clone());
                }
            }
        }
    }
    store.save(&journal)?;
    db::rollback(&database)?;
    let remove_index = entry
        .index
        .iter()
        .filter(|s| !retained_index.contains(*s))
        .cloned()
        .collect::<Vec<_>>();
    update_index(&store.target, &remove_index, true)?;
    for link in &entry.files {
        if link.owned && !retained.contains(&link.target) && link.target.exists() {
            fs::remove_file(&link.target)?;
        }
    }
    journal.imports.remove(id);
    store.save(&journal)
}

pub(crate) fn before_codex_resume(args: &[OsString]) -> Result<()> {
    if args.first().and_then(|s| s.to_str()) != Some("resume") {
        return Ok(());
    }
    let Some(id) = args
        .get(1)
        .and_then(|s| s.to_str())
        .filter(|s| discovery::valid_id(s))
    else {
        return Ok(());
    };
    let store = Store::open(&qb_core::home()?)?;
    let journal = store.load()?;
    if let Some(entry) = journal.imports.get(id) {
        ensure!(entry.complete, "대화 가져오기가 완료되지 않았습니다");
        verify_links(entry, false)?;
        let names = entry
            .files
            .iter()
            .flat_map(|f| [f.source.clone(), f.target.clone()])
            .collect::<Vec<_>>();
        let (homes, names) = store.probes(&entry.source_home, &entry.threads, &names)?;
        let _guards = guard::probe_all(&homes, &entry.threads, &names)?;
        return Ok(());
    }
    if db::thread_path(&store.target, id)?.is_some() {
        let catalog = discovery::catalog(&store.target)?;
        let files = catalog.chain(id)?;
        let names = files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        let threads = files.iter().map(|f| f.thread.clone()).collect::<Vec<_>>();
        let (homes, names) = store.probes(&store.target, &threads, &names)?;
        let _guards = guard::probe_all(&homes, &threads, &names)?;
        return Ok(());
    }
    let user = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .context("사용자 홈 경로가 없습니다")?;
    let mut matches = Vec::new();
    for home in known_homes(&user) {
        if fs::canonicalize(&home).ok().as_ref() == Some(&store.target) {
            continue;
        }
        if let Ok(catalog) = discovery::catalog(&home)
            && catalog.threads.contains_key(id)
        {
            matches.push(catalog);
        }
    }
    ensure!(
        !matches.is_empty(),
        "기존 홈에서 대화를 찾지 못했습니다. sessions import --from으로 원본을 지정하세요"
    );
    // Several homes may contain the same linked thread. Do not pick a forked copy.
    let first = matches[0].chain(id)?;
    for other in matches.iter().skip(1) {
        let files = other.chain(id)?;
        ensure!(
            files.len() == first.len()
                && files
                    .iter()
                    .zip(&first)
                    .all(|(a, b)| guard::identity(&a.path).ok() == guard::identity(&b.path).ok()),
            "여러 홈의 대화 기록이 분기되었습니다. 원본을 직접 지정하세요"
        );
    }
    import_one(&store, &matches[0], id, true, false, false)?;
    crate::print_stdout(format_args!(
        "기존 대화를 qb로 가져왔습니다 ({}) · 두 홈에서 동시에 열지 마세요",
        matches[0].home.display()
    ))?;
    Ok(())
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
