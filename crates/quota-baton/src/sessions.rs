//! Journaled Codex session imports. No rollout contents appear in diagnostics.
use crate::{sessions_db as db, sessions_discovery as discovery, sessions_guard as guard};

pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    if let Some(message) = crate::sessions_progress::friendly_error(error) {
        return Some(message);
    }
    if let Some(provider) = error.downcast_ref::<crate::sessions_db::UnsupportedProvider>() {
        return Some(provider.to_string());
    }
    if error.is::<discovery::PhysicalConflict>() {
        return Some("같은 물리 대화 ID의 서로 다른 파일이 있습니다. doctor로 확인하세요".into());
    }
    if error.is::<discovery::RelatedUnreadable>() {
        return Some("관련된 압축 대화 기록을 읽을 수 없습니다. doctor로 확인하세요".into());
    }
    if let Some(conflict) = error.downcast_ref::<InventoryConflict>() {
        return Some(conflict.to_string());
    }
    if let Some(live) = error.downcast_ref::<guard::LiveWriter>() {
        return Some(format!(
            "{live}\n먼저 해당 홈의 대화를 종료하세요. 종료 후에도 잠겨 있으면 그 홈의 app-server-daemon/daemon.pid에서 PID를 확인하세요. 다른 대화를 제공하지 않는 데몬만 해당 PID로 종료한 뒤 다시 시도하세요. qb는 데몬을 자동 종료하지 않습니다."
        ));
    }
    const FIXED: &[&str] = &[
        discovery::ROLLOUT_PATH_ERROR,
        "대화 상태와 기록 경로가 다릅니다",
        "전용 Codex 홈 연결이 없습니다. 먼저 wire를 실행하세요",
        "전용 Codex 홈 연결을 먼저 완료하세요",
        "전용 Codex 홈이 없습니다",
        "대화 ID는 UUID여야 합니다",
        "세션 파일의 마지막 하드링크는 삭제할 수 없습니다",
        "원본에서 대화를 찾지 못했습니다",
        "최근에 사용한 대화입니다. 먼저 대화를 종료하고 120초 후 다시 시도하세요",
        "openai 제공자의 대화만 가져올 수 있습니다",
        "openai 또는 qb 제공자의 대화만 가져올 수 있습니다",
        "대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요",
        "활성 대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요",
        "압축된 대화 기록이 있습니다. sessions unimport 후 다시 가져오세요",
        "대화 제공자가 변경되었습니다. sessions unimport 후 다시 가져오세요",
        "가져온 뒤 대화 기록이 변경되었습니다. 확인 후 --force를 사용하세요",
        "가져온 뒤 기록이 변경되었습니다. 확인 후 --force를 사용하세요",
        "대상 파일 소유권이 달라 되돌릴 수 없습니다",
        "하드링크를 만들 수 없습니다. 같은 드라이브의 홈을 사용하세요. 기록은 복사하지 않았습니다",
        "중단된 가져오기가 있습니다. sessions unimport로 되돌린 뒤 다시 시도하세요",
        "다른 대화 가져오기 작업이 실행 중입니다",
        "다른 작업이 대화 인덱스를 사용 중입니다",
        "기존 홈에서 대화를 찾지 못했습니다. sessions import --from으로 원본을 지정하세요",
        "여러 홈의 대화 기록이 분기되었습니다. 원본을 직접 지정하세요",
        "대화 가져오기가 완료되지 않았습니다",
        "가져온 대화가 아닙니다",
        "기록에서 필요한 대화 메타데이터를 복구하지 못했습니다",
        "재개 옵션의 값이 없습니다",
        "재개 옵션을 확인할 수 없습니다. 지원되는 옵션과 UUID를 사용하세요",
        "다른 원본 대화가 이 기록을 사용합니다. 자식 대화를 먼저 옮기거나 --keep-source를 사용하세요",
        "압축된 rollout 또는 .zst 형제가 있습니다. sessions unimport 후 다시 가져오세요",
        "가져온 대화의 메타데이터가 없습니다",
        "오래된 기록을 다른 대화가 사용합니다. --clean-stale 없이 다시 시도하세요",
        "취소했습니다. 아무것도 바뀌지 않았습니다.",
        "Cancelled. Nothing was changed.",
        "이동 중단 처리를 준비하지 못했습니다",
        "이전 이동 복구를 터미널에서 finish/rollback으로 선택하세요. 스크립트는 --yes를 사용하세요.",
        "게시가 확정된 이동은 되돌릴 수 없습니다. finish로 먼저 복구하세요.",
        "이전 이동을 되돌렸습니다. 원본 홈에서 대화를 다시 열어 주세요.",
        "표준 입력과 출력이 모두 터미널이어야 합니다. 수동 명령을 실행해 주세요.",
        "확인 안내를 출력하지 못했습니다.",
        "확인 입력을 읽지 못해 취소했습니다.",
        "중단된 돌려보내기가 있습니다. sessions return으로 먼저 완료하세요",
        "이 대화에서 갈라진 대화가 있습니다. 가족 이동을 지원하기 전에는 옮길 수 없습니다",
        "조상 전용 대화의 모든 관리 재개 경로를 차단할 수 없어 이 대화의 이동을 거절합니다",
        "대상 홈에 이미 조상 또는 대화 이름이 있습니다. 이동을 거절합니다",
        "대상 qb 홈에 이 대화의 조상 별칭 또는 같은 대화 이름이 있습니다. 먼저 doctor로 확인하세요",
        "이 대화는 이동 복구가 필요합니다. sessions recover로 먼저 복구하세요",
        "이 대화의 이동 journal이 잘못되었습니다",
        "게시 결정 전에는 --resolve-map 없이 sessions recover를 실행하세요",
        "발견하지 못한 하드링크가 있습니다. doctor로 확인하세요",
        "이동 기록과 현재 상태가 다릅니다. 증거를 보존했습니다. doctor로 확인하세요",
        "미완료 이동이 있습니다. sessions recover로 먼저 복구하세요",
        "조상 전용 또는 고아 대화가 있어 관리 실행을 거절합니다. doctor로 확인하세요",
        "이 대화는 다른 홈이 소유합니다. 원래 홈에서 재개하세요",
        "대화 소유권과 목록이 다릅니다. doctor로 확인하세요",
        "여러 관리 홈에 가져오기 기록이 있습니다. doctor로 확인하세요",
        "이동 중 갈라진 대화가 나타나 원본으로 되돌렸습니다",
        "압축된 대화의 후손 관계를 확인할 수 없어 이동을 거절합니다. doctor로 확인하세요",
        "하나의 파일에 서로 다른 물리 대화 ID가 있어 이동을 거절합니다",
        "서로 다른 볼륨에는 하드링크로 대화를 옮길 수 없습니다",
        "압축된 대화의 소유권을 확인할 수 없어 관리 실행을 거절합니다. doctor로 확인하세요",
    ];
    let message = error.to_string();
    FIXED.contains(&message.as_str()).then_some(message)
}
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

// D21 panel chose move; sharing remains an explicit user choice.
const DEFAULT_KEEP_SOURCE: bool = false;

fn progress_english(home: &Path) -> bool {
    qb_core::config::Config::load(home)
        .is_ok_and(|config| config.ui.language == qb_core::config::UiLanguage::En)
}

#[derive(Subcommand)]
pub(crate) enum SessionsCommand {
    #[command(about = "실행 중인 코어에서 루트 세션의 5시간 제한 긴급 전환을 설정합니다")]
    Urgent {
        id: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    #[command(
        about = "기존 대화를 qb로 가져옵니다",
        override_usage = "quota-baton sessions import --from <HOME> [<UUID> | --all | --list] [--apply]"
    )]
    Import {
        #[arg(long)]
        from: PathBuf,
        #[arg(conflicts_with_all=["all", "list"])]
        id: Vec<String>,
        #[arg(long, conflicts_with = "list")]
        all: bool,
        #[arg(long)]
        list: bool,
        #[arg(long, conflicts_with = "list")]
        apply: bool,
        #[arg(long, conflicts_with = "list")]
        keep_source: bool,
        #[arg(long = "move", conflicts_with_all = ["list", "keep_source"])]
        move_source: bool,
        #[arg(long, conflicts_with = "list")]
        yes: bool,
        #[arg(long, conflicts_with = "list")]
        clean_stale: bool,
        #[arg(long)]
        verbose: bool,
    },
    #[command(about = "가져온 대화의 연결만 되돌립니다")]
    Unimport {
        id: String,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        yes: bool,
    },
    #[command(about = "qb의 현재 대화를 계정 홈으로 돌려보냅니다")]
    Return {
        id: String,
        #[arg(long)]
        to: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    #[command(about = "중단된 대화 이동을 안전한 방향으로 복구합니다")]
    Recover {
        id: String,
        #[arg(long)]
        resolve_map: bool,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct Link {
    source: PathBuf,
    target: PathBuf,
    identity: guard::FileIdentity,
    length: u64,
    owned: bool,
    thread: String,
    moved: bool,
    #[serde(default)]
    retained_ancestor: bool,
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
    root: String,
    keep_source: bool,
    move_started: bool,
    move_done: bool,
    source_database: String,
    stale: Vec<StaleCopy>,
    #[serde(default)]
    session_map: Option<crate::sessions_map::MapPlan>,
}
#[derive(Clone, Serialize, Deserialize)]
struct StaleName {
    original: PathBuf,
    backup: PathBuf,
    identity: guard::FileIdentity,
    bytes: u64,
    #[serde(default)]
    retained_ancestor: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct StaleCopy {
    home: PathBuf,
    names: Vec<StaleName>,
    database: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Journal {
    schema: u8,
    imports: BTreeMap<String, Import>,
}

#[derive(Clone)]
struct Store {
    home: PathBuf,
    target: PathBuf,
    search_homes: Vec<PathBuf>,
    map_path: PathBuf,
}
fn wired_homes(home: &Path) -> Result<Vec<PathBuf>> {
    let path = home.join("journal/wiring.json");
    guard::safe_path(home, &path)?;
    let bytes = fs::read(path).context("전용 Codex 홈 연결이 없습니다. 먼저 wire를 실행하세요")?;
    ensure!(bytes.len() <= 16 * 1024 * 1024, "연결 기록이 너무 큽니다");
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("연결 기록을 읽을 수 없습니다"))?;
    ensure!(
        crate::sessions_admission::completed_wiring(&value),
        "전용 Codex 홈 연결을 먼저 완료하세요"
    );
    let ordinary = PathBuf::from(value["home"].as_str().context("전용 홈 경로가 없습니다")?);
    let image = value["image_home"].as_str().map(PathBuf::from);
    let mut homes = Vec::new();
    for target in std::iter::once(ordinary).chain(image) {
        ensure!(
            target.is_absolute() && target.is_dir(),
            "전용 Codex 홈이 없습니다"
        );
        guard::safe_path(&target, &target)?;
        let target = fs::canonicalize(target)?;
        ensure!(!homes.contains(&target), "연결된 관리 홈이 중복되었습니다");
        homes.push(target);
    }
    Ok(homes)
}
impl Store {
    fn open(home: &Path) -> Result<Self> {
        Self::open_selected(home, None)
    }

    fn open_selected(home: &Path, selected: Option<&Path>) -> Result<Self> {
        let homes = wired_homes(home)?;
        let target = if let Some(selected) = selected {
            let selected = fs::canonicalize(selected)?;
            ensure!(
                homes.contains(&selected),
                "선택한 홈은 연결된 qb 홈이 아닙니다"
            );
            selected
        } else {
            homes[0].clone()
        };
        ensure!(
            target.is_absolute() && target.is_dir(),
            "전용 Codex 홈이 없습니다"
        );
        guard::safe_path(&target, &target)?;
        let user = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        let mut search_homes = user.as_deref().map(known_homes).unwrap_or_default();
        for home in homes {
            if home.is_dir() && !search_homes.contains(&home) {
                search_homes.push(home);
            }
        }
        Ok(Self {
            home: home.to_owned(),
            target: fs::canonicalize(target)?,
            search_homes,
            map_path: std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .context("AppData 경로를 확인할 수 없습니다")?
                .join("herdr/session-homes.json"),
        })
    }

    fn managed_homes(&self) -> Result<Vec<PathBuf>> {
        let journal = self.home.join("journal/wiring.json");
        guard::safe_path(&self.home, &journal)?;
        if journal.try_exists()? {
            wired_homes(&self.home)
        } else {
            Ok(vec![self.target.clone()])
        }
    }

    fn with_target(&self, target: PathBuf) -> Self {
        Self {
            target,
            ..self.clone()
        }
    }

    fn for_session(self, id: &str) -> Result<Self> {
        let homes = self.managed_homes()?;
        if let Some(owner) = staged::owner(&self, id)? {
            return Ok(if homes.contains(&owner) {
                self.with_target(owner)
            } else {
                self
            });
        }
        let mut selected = None;
        for home in homes {
            let candidate = self.with_target(home);
            if candidate
                .load()?
                .imports
                .values()
                .any(|entry| entry.root == id || entry.threads.iter().any(|thread| thread == id))
            {
                ensure!(
                    selected.is_none(),
                    "여러 관리 홈에 가져오기 기록이 있습니다. doctor로 확인하세요"
                );
                selected = Some(candidate);
            }
        }
        Ok(selected.unwrap_or(self))
    }
    fn path(&self) -> PathBuf {
        // Only the journaled Qi path has this fixed basename. Keep legacy
        // share journals separate; staged MOVE ownership remains global by ID.
        self.home.join(
            if self
                .target
                .file_name()
                .is_some_and(|name| name == "qb-images")
            {
                "journal/sessions-import-images.json"
            } else {
                "journal/sessions-import.json"
            },
        )
    }

    fn staging_home(&self) -> Result<PathBuf> {
        let journal = self.home.join("journal/wiring.json");
        guard::safe_path(&self.home, &journal)?;
        if !journal.try_exists()? {
            return Ok(self.target.clone());
        }
        let bytes = fs::read(journal)?;
        ensure!(bytes.len() <= 16 * 1024 * 1024, "연결 기록이 너무 큽니다");
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let home = PathBuf::from(value["home"].as_str().context("전용 홈 경로가 없습니다")?);
        guard::safe_path(&home, &home)?;
        Ok(fs::canonicalize(home)?)
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
            for stale in &entry.stale {
                for name in &stale.names {
                    guard::safe_path(&stale.home, &name.original)?;
                    guard::safe_path(&self.home.join("journal/sessions-stale"), &name.backup)?;
                }
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
        self.probes_with_recorded_homes(source, threads, initial, &[])
    }
    fn probes_with_recorded_homes(
        &self,
        source: &Path,
        threads: &[String],
        initial: &[PathBuf],
        recorded: &[PathBuf],
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let mut homes = vec![source.to_owned(), self.target.clone()];
        // Recovery may have removed active rows before moving paginated names.
        // Their complete probe inventory comes from the encrypted journal.
        homes.extend_from_slice(recorded);
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
                    names.extend(
                        catalog.threads[id]
                            .iter()
                            .map(|physical| catalog.rollouts[physical].path.clone()),
                    );
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

fn source_has_dependents(catalog: &discovery::Catalog, id: &str) -> Result<bool> {
    let own = catalog
        .threads
        .get(id)
        .context("원본에서 대화를 찾지 못했습니다")?;
    Ok(catalog.rollouts.values().any(|f| {
        f.thread != id
            && f.path.exists()
            && (f.forked_from.as_deref() == Some(id)
                || f.history_base
                    .as_ref()
                    .is_some_and(|base| own.contains(base)))
    }))
}

fn migration_files(catalog: &discovery::Catalog, id: &str) -> Result<Vec<discovery::Rollout>> {
    let mut files = catalog.chain(id)?;
    for physical in &catalog.threads[id] {
        let item = &catalog.rollouts[physical];
        if !files.iter().any(|file| file.path == item.path) {
            files.push(item.clone());
        }
    }
    Ok(files)
}

fn checkpoint(store: &Store, id: &str, step: &str) -> Result<()> {
    #[cfg(all(feature = "testkit", debug_assertions))]
    if std::env::var("QB_R9_CHECKPOINT").ok().as_deref() == Some(step) {
        ensure!(
            discovery::valid_id(id)
                && store
                    .home
                    .components()
                    .any(|part| part.as_os_str() == ".sandbox"),
            "중단점은 합성 sandbox 홈에서만 사용할 수 있습니다"
        );
        let address: std::net::SocketAddr = std::env::var("QB_R9_CHECKPOINT_NOTIFY")?.parse()?;
        ensure!(
            address.ip().is_loopback() && !matches!(address.port(), 10100 | 10101),
            "중단점 알림 주소는 테스트 loopback이어야 합니다"
        );
        let mut signal =
            std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2))?;
        signal.write_all(step.as_bytes())?;
        signal.shutdown(std::net::Shutdown::Write)?;
        std::thread::park_timeout(std::time::Duration::from_secs(30));
        bail!("합성 중단점의 종료 대기 시간이 지났습니다");
    }
    let _ = (store, id, step);
    Ok(())
}

#[derive(Debug)]
struct InventoryConflict {
    homes: Vec<PathBuf>,
}
impl std::fmt::Display for InventoryConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "다른 홈의 대화 기록 또는 목표 상태가 다릅니다. 자동으로 고르거나 합치지 않았습니다"
        )?;
        for home in &self.homes {
            write!(f, "\n- {}", home.display())?;
        }
        Ok(())
    }
}
impl std::error::Error for InventoryConflict {}

fn same_contents(left: &Path, right: &Path) -> Result<bool> {
    if guard::identity(left)? == guard::identity(right)? {
        return Ok(true);
    }
    if fs::metadata(left)?.len() != fs::metadata(right)?.len() {
        return Ok(false);
    }
    let mut a = std::io::BufReader::new(File::open(left)?);
    let mut b = std::io::BufReader::new(File::open(right)?);
    let mut aa = [0; 64 * 1024];
    let mut bb = [0; 64 * 1024];
    loop {
        let n = a.read(&mut aa)?;
        if n == 0 {
            return Ok(b.read(&mut bb)? == 0);
        }
        b.read_exact(&mut bb[..n])?;
        if aa[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

fn check_physical_owners(
    source: &Path,
    home: &Path,
    expected: &[discovery::Rollout],
    other: &discovery::Catalog,
) -> Result<()> {
    if other.all().any(|item| {
        expected
            .iter()
            .any(|file| file.physical == item.physical && file.thread != item.thread)
    }) {
        return Err(InventoryConflict {
            homes: vec![source.to_owned(), home.to_owned()],
        }
        .into());
    }
    Ok(())
}

fn stale_plan(store: &Store, catalog: &discovery::Catalog, id: &str) -> Result<Vec<StaleCopy>> {
    let active = catalog.active(id)?;
    let source_chain = migration_files(catalog, id)?;
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    let mut inventory = Vec::new();
    for home in &store.search_homes {
        let home = fs::canonicalize(home)?;
        if home == catalog.home || home == store.target || !seen.insert(home.clone()) {
            continue;
        }
        let other = discovery::catalog(&home)?;
        check_physical_owners(&catalog.home, &home, &source_chain, &other)?;
        if other.threads.contains_key(id) {
            inventory.push((home, other));
        }
    }
    let all_homes = std::iter::once(catalog.home.clone())
        .chain(inventory.iter().map(|(home, _)| home.clone()))
        .collect::<Vec<_>>();
    for (home, other) in inventory {
        let head = other.active(id)?;
        let other_chain = migration_files(&other, id)?;
        let mut equal = active.physical == head.physical && source_chain.len() == other_chain.len();
        for item in &source_chain {
            if let Some(peer) = other_chain
                .iter()
                .find(|peer| peer.physical == item.physical)
            {
                equal &= same_contents(&item.path, &peer.path)?;
            } else {
                equal = false;
            }
        }
        let snapshot = db::source_snapshot(&home, &[id.into()])?;
        let has_rows = snapshot
            .threads
            .iter()
            .any(|thread| thread.thread.is_some() || thread.goal.is_some());
        if has_rows {
            equal &= db::compare_goal_state(&[catalog.home.clone(), home.clone()], id)?;
        }
        if !equal {
            return Err(InventoryConflict {
                homes: all_homes.clone(),
            }
            .into());
        }
        let retained_ancestor = source_has_dependents(&other, id)?;
        let mut names = Vec::new();
        for physical in &other.threads[id] {
            let item = &other.rollouts[physical];
            let directory = store
                .home
                .join("journal/sessions-stale")
                .join(id)
                .join(format!("{}-{}", result.len(), names.len()));
            let backup =
                directory.join(item.path.file_name().context("기록 파일 이름이 없습니다")?);
            guard::safe_path(&store.home, &backup)?;
            ensure!(
                !backup.exists(),
                "오래된 대화의 보존 위치가 이미 사용 중입니다"
            );
            names.push(StaleName {
                original: item.path.clone(),
                backup,
                identity: guard::identity(&item.path)?,
                bytes: item.bytes,
                retained_ancestor,
            });
        }
        let database = crate::cutover::protect(&serde_json::to_vec(&db::source_snapshot(
            &home,
            &[id.into()],
        )?)?)?;
        result.push(StaleCopy {
            home,
            names,
            database,
        });
    }
    Ok(result)
}

fn ensure_readable_inventory(homes: &[PathBuf], seeds: &BTreeSet<String>) -> Result<()> {
    let catalogs = homes
        .iter()
        .map(|h| discovery::catalog(h))
        .collect::<Result<Vec<_>>>()?;
    let mut related = seeds.clone();
    loop {
        let before = related.len();
        for catalog in &catalogs {
            related.extend(catalog.related_ids(&related));
        }
        if related.len() == before {
            break;
        }
    }
    for catalog in &catalogs {
        catalog.ensure_readable_for(&related)?;
    }
    Ok(())
}

fn check_destination_inventory(
    source: &Path,
    target: &Path,
    files: &[discovery::Rollout],
) -> Result<()> {
    let destination = discovery::catalog(target)?;
    let related = files
        .iter()
        .flat_map(|f| [f.thread.clone(), f.physical.clone()])
        .collect();
    destination.ensure_readable_for(&related)?;
    let threads: BTreeSet<_> = files.iter().map(|file| &file.thread).collect();
    let physicals: BTreeSet<_> = files.iter().map(|file| &file.physical).collect();
    for item in destination
        .all()
        .filter(|item| threads.contains(&item.thread) || physicals.contains(&item.physical))
    {
        // Only the exact hardlink we would reuse is harmless. An extra name,
        // even without a threads row, would leave an ambiguous physical id.
        let candidate = files.iter().find(|file| {
            file.thread == item.thread
                && file.physical == item.physical
                && item.path == target.join("sessions").join(&file.relative)
        });
        let compatible = match candidate {
            Some(file) => guard::identity(&file.path)? == guard::identity(&item.path)?,
            None => false,
        };
        if !compatible {
            return Err(InventoryConflict {
                homes: vec![source.to_owned(), target.to_owned()],
            }
            .into());
        }
    }
    Ok(())
}

fn clean_stale_copies(copies: &[StaleCopy], journal_home: &Path) -> Result<()> {
    for copy in copies {
        for name in &copy.names {
            ensure!(
                guard::identity(&name.original)? == name.identity
                    && fs::metadata(&name.original)?.len() == name.bytes,
                "오래된 대화 기록이 정리 전에 변경되었습니다"
            );
        }
        let plan: db::DbPlan = serde_json::from_slice(&crate::cutover::unprotect(&copy.database)?)?;
        db::remove_exact(&plan)?;
        for name in &copy.names {
            if name.retained_ancestor {
                continue;
            }
            if let Some(parent) = name.backup.parent() {
                fs::create_dir_all(parent)?;
            }
            guard::rename_verified(
                &copy.home,
                &name.original,
                journal_home,
                &name.backup,
                name.identity,
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "오래된 기록을 journal에 보존하지 못했습니다. 복사하거나 덮어쓰지 않았습니다"
                )
            })?;
        }
    }
    Ok(())
}

fn restore_stale_copies(copies: &[StaleCopy], journal_home: &Path) -> Result<()> {
    for copy in copies {
        for name in &copy.names {
            if name.original.exists() {
                ensure!(
                    guard::identity(&name.original)? == name.identity,
                    "원본 위치에 다른 기록이 있어 복구하지 못했습니다"
                );
            } else {
                ensure!(
                    guard::identity(&name.backup)? == name.identity,
                    "보존된 오래된 기록이 변경되었습니다"
                );
                if let Some(parent) = name.original.parent() {
                    fs::create_dir_all(parent)?;
                }
                guard::rename_verified(
                    journal_home,
                    &name.backup,
                    &copy.home,
                    &name.original,
                    name.identity,
                )?;
            }
        }
        let plan: db::DbPlan = serde_json::from_slice(&crate::cutover::unprotect(&copy.database)?)?;
        db::apply(&plan)?;
    }
    Ok(())
}

fn index_plan(target: &Path, database: &db::DbPlan) -> Result<Vec<String>> {
    let path = target.join("session_index.jsonl");
    guard::safe_path(target, &path)?;
    let current = match fs::read_to_string(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => bail!("대화 인덱스를 읽을 수 없습니다"),
    };
    let present: BTreeMap<_, _> = current
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|v| {
            let id = v["id"].as_str()?.to_owned();
            Some((id, v))
        })
        .collect();
    let mut result = Vec::new();
    for thread in &database.threads {
        if let Some(entry) = &thread.latest_index_entry
            && present.get(&thread.id) != Some(entry)
        {
            result.push(serde_json::to_string(entry)?);
        }
    }
    Ok(result)
}

fn update_index(target: &Path, entries: &[String], remove: bool) -> Result<()> {
    // Index entries are append-only hints; retiring a thread never rewrites or
    // removes another process's index history, including our former entries.
    if remove || entries.is_empty() {
        return Ok(());
    }
    let path = target.join("session_index.jsonl");
    guard::safe_path(target, &path)?;
    let mut options = OpenOptions::new();
    options.read(true).append(true).create(true);
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(5);
    }
    let mut file = options
        .open(&path)
        .map_err(|_| anyhow::anyhow!("다른 작업이 대화 인덱스를 사용 중입니다"))?;
    ensure!(
        file.metadata()?.len() <= 64 * 1024 * 1024,
        "대화 인덱스가 너무 큽니다"
    );
    let mut current = String::new();
    file.read_to_string(&mut current)
        .map_err(|_| anyhow::anyhow!("대화 인덱스를 읽을 수 없습니다"))?;
    if !current.is_empty() && !current.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    let mut latest: BTreeMap<String, serde_json::Value> = current
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| Some((value["id"].as_str()?.to_owned(), value)))
        .collect();
    for entry in entries {
        let value: serde_json::Value = serde_json::from_str(entry)?;
        let id = value["id"]
            .as_str()
            .context("대화 인덱스 ID가 없습니다")?
            .to_owned();
        if latest.get(&id) != Some(&value) {
            file.write_all(format!("{entry}\n").as_bytes())?;
            latest.insert(id, value);
        }
    }
    file.sync_all()?;
    Ok(())
}

fn preview(
    id: &str,
    files: &[discovery::Rollout],
    target: &Path,
    database: &db::DbPlan,
) -> Result<()> {
    use crate::sessions_presentation::{
        self as presentation, FileLink, Language, Preview, Verbose,
    };
    crate::sessions_progress::phase(crate::sessions_progress::Phase::Preview, None)?;
    let thread = database.threads.iter().find(|t| t.id == id);
    let text = |row: Option<&db::RowSnapshot>, key: &str| -> Option<String> {
        match row?.values.get(key)? {
            db::DbValue::Text(bytes) => String::from_utf8(bytes.clone()).ok(),
            _ => None,
        }
    };
    let db_title = thread.and_then(|t| text(t.thread.as_ref(), "title"));
    let index_name = thread
        .and_then(|t| t.latest_index_entry.as_ref())
        .and_then(|entry| entry["thread_name"].as_str());
    let title = crate::sessions_preview_title::resolve(
        db_title.as_deref(),
        index_name,
        files
            .iter()
            .rev()
            .find(|file| file.thread == id)
            .map(|file| file.path.as_path()),
    );
    let before = thread.and_then(|t| t.source_goal_status.as_deref());
    let after = thread.and_then(|t| text(t.goal.as_ref(), "status"));
    let names = files
        .iter()
        .map(|file| {
            (
                file.path.to_string_lossy().into_owned(),
                target
                    .join("sessions")
                    .join(&file.relative)
                    .to_string_lossy()
                    .into_owned(),
            )
        })
        .collect::<Vec<_>>();
    let links = names
        .iter()
        .map(|(source, target)| FileLink { source, target })
        .collect::<Vec<_>>();
    let ancestors = files
        .iter()
        .filter(|f| f.thread != id)
        .map(|f| f.thread.as_str())
        .collect::<Vec<_>>();
    let profile = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let source = database.source_home.to_string_lossy();
    let target = target.to_string_lossy();
    let body = presentation::format_body(
        if crate::sessions_progress::english() {
            Language::En
        } else {
            Language::Ko
        },
        &Preview {
            id,
            title: &title,
            bytes: files.iter().map(|f| f.bytes).sum(),
            source_home: &source,
            target_home: &target,
            profile_home: &profile,
            goal_before: before,
            goal_after: after.as_deref(),
            verbose: crate::sessions_progress::verbose().then_some(Verbose {
                files: &links,
                ancestors: &ancestors,
            }),
        },
    );
    crate::print_stdout(format_args!("{body}"))?;
    Ok(())
}

fn verify_links(entry: &Import, growth: bool) -> Result<()> {
    for link in &entry.files {
        guard::safe_path(&entry.source_home, &link.source)?;
        guard::safe_path(&entry.target_home, &link.target)?;
        let source = if link.moved && !link.source.exists() {
            link.identity
        } else {
            guard::identity(&link.source)?
        };
        ensure!(
            source == link.identity && guard::identity(&link.target)? == source,
            "대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요"
        );
        if growth {
            ensure!(
                fs::metadata(&link.target)?.len() == link.length,
                "가져온 뒤 대화 기록이 변경되었습니다. 확인 후 --force를 사용하세요"
            );
        }
    }
    ensure_readable_inventory(
        &[entry.source_home.clone(), entry.target_home.clone()],
        &entry.threads.iter().cloned().collect(),
    )?;
    for id in &entry.threads {
        ensure!(
            db::thread_provider(&entry.target_home, id)?
                .is_none_or(|p| db::destination_provider(&entry.target_home)
                    .is_ok_and(|expected| p == expected)),
            "대화 제공자가 변경되었습니다. sessions unimport 후 다시 가져오세요"
        );
        let source = db::thread_path(&entry.source_home, id)?;
        let target = match db::thread_path(&entry.target_home, id)? {
            Some(path) => discovery::resolve_rollout_path(&entry.target_home, &path)?,
            None if id != &entry.root => continue,
            None => bail!("가져온 대화의 메타데이터가 없습니다"),
        };
        guard::safe_path(&entry.target_home, &target)?;
        let expected = entry
            .files
            .iter()
            .find(|l| l.thread == *id && l.target.file_name() == target.file_name())
            .context("활성 대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요")?;
        ensure!(
            fs::canonicalize(&target)? == fs::canonicalize(&expected.target)?,
            "활성 대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요"
        );
        if let Some(source) = source {
            let source = discovery::resolve_rollout_path(&entry.source_home, &source)?;
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

fn import_refusal_class(error: &anyhow::Error) -> usize {
    let message = error.to_string();
    if error.is::<guard::LiveWriter>()
        || message == "최근에 사용한 대화입니다. 먼저 대화를 종료하고 120초 후 다시 시도하세요"
    {
        0
    } else if error.is::<InventoryConflict>()
        || error.is::<discovery::PhysicalConflict>()
        || message.contains("분기")
        || message == "대상에 다른 대화 기록이 있습니다"
    {
        1
    } else {
        2
    }
}
fn import_refusal_label(class: usize) -> &'static str {
    match class {
        0 => "실행 중/최근 사용으로 건너뜀",
        1 => "기록 분기로 건너뜀",
        _ => "실패 (원본과 journal을 확인하세요)",
    }
}

fn preview_imports(
    store: &Store,
    catalog: &discovery::Catalog,
    ids: &[String],
    bulk: bool,
    keep_source: bool,
    clean_stale: bool,
    skip_existing: bool,
) -> Result<(Vec<String>, [usize; 3])> {
    let mut included = Vec::new();
    let mut refused = [0; 3];
    for id in ids {
        match import_one(
            store,
            catalog,
            id,
            ImportOptions {
                apply: false,
                skip_existing,
                announce: true,
                keep_source,
                clean_stale,
            },
        ) {
            Ok(_) => included.push(id.clone()),
            Err(error) if bulk => {
                crate::sessions_progress::check()?;
                let class = import_refusal_class(&error);
                refused[class] += 1;
                crate::print_stdout(format_args!(
                    "{id}: {} · 사전 검사에서 제외됨",
                    import_refusal_label(class)
                ))?
            }
            Err(error) => return Err(error),
        }
    }
    Ok((included, refused))
}

pub(crate) fn run(home: &Path, command: SessionsCommand) -> Result<()> {
    ensure!(
        !matches!(&command, SessionsCommand::Urgent { .. }),
        "긴급 정책은 실행 중인 코어에 요청해야 합니다"
    );
    let id = match &command {
        SessionsCommand::Import { id, .. } => id.first().map(String::as_str).unwrap_or("all"),
        SessionsCommand::Return { id, .. }
        | SessionsCommand::Recover { id, .. }
        | SessionsCommand::Unimport { id, .. } => id,
        SessionsCommand::Urgent { .. } => unreachable!(),
    };
    let _progress = if matches!(&command, SessionsCommand::Import { .. }) {
        crate::sessions_progress::Operation::begin(id, progress_english(home))?
    } else {
        crate::sessions_progress::Operation::begin_existing(id, progress_english(home))?
    };
    run_with_store(Store::open(home)?, command)
}

fn run_with_store(mut store: Store, command: SessionsCommand) -> Result<()> {
    // MOVE ownership is shared by Q and Qi. Return/recovery must operate on
    // the current managed owner instead of assuming every thread lives in Q.
    let requested = match &command {
        SessionsCommand::Return { id, .. }
        | SessionsCommand::Unimport { id, .. }
        | SessionsCommand::Recover { id, .. } => Some(id.as_str()),
        _ => None,
    };
    if let Some(id) = requested {
        store = store.for_session(id)?;
    }
    match command {
        SessionsCommand::Urgent { .. } => bail!("긴급 정책은 실행 중인 코어에 요청해야 합니다"),
        SessionsCommand::Import {
            from,
            mut id,
            all,
            list,
            apply,
            keep_source,
            move_source,
            yes,
            clean_stale,
            verbose,
        } => {
            crate::sessions_progress::set_verbose(verbose);
            ensure!(
                !id.is_empty() || all || list,
                "대화 ID, --all 또는 --list를 지정하세요"
            );
            let _read_only_cache = if !apply {
                Some(discovery::read_only_cache())
            } else {
                None
            };
            let mut keep_source = !move_source && (keep_source || DEFAULT_KEEP_SOURCE);
            crate::sessions_progress::phase(crate::sessions_progress::Phase::Searching, None)?;
            // Resolve an interrupted exact-id move before scanning its now
            // partially removed source home. Recovery still owns all fences.
            if apply {
                let mut remaining = Vec::new();
                for requested in &id {
                    match staged::offer_recovery(&store, requested, yes)? {
                        Some(false) => return Ok(()),
                        Some(true) => {}
                        None => remaining.push(requested.clone()),
                    }
                }
                if !id.is_empty() && remaining.is_empty() {
                    return Ok(());
                }
                id = remaining;
            }
            let catalog = discovery::catalog(&from)?;
            if list {
                let providers = db::threads(&catalog.home)?
                    .into_iter()
                    .map(|t| (t.id, t.model_provider))
                    .collect::<BTreeMap<_, _>>();
                for id in catalog.threads.keys() {
                    let refusal = catalog
                        .ensure_readable_for(&BTreeSet::from([id.clone()]))
                        .err();
                    crate::print_stdout(format_args!(
                        "{id}{}",
                        if refusal
                            .as_ref()
                            .is_some_and(|e| e.is::<discovery::PhysicalConflict>())
                        {
                            " · 물리 ID 중복 (가져오기 거절)"
                        } else if refusal
                            .as_ref()
                            .is_some_and(|e| e.is::<discovery::RelatedUnreadable>())
                        {
                            " · 관련 압축 기록을 읽을 수 없음"
                        } else if providers.get(id).is_some_and(|p| p != "openai") {
                            " · openai 제공자가 아님"
                        } else {
                            ""
                        }
                    ))?;
                }
                return Ok(());
            }
            #[cfg(not(test))]
            crate::sessions_admission::verify_installed_version()?;
            let mut ids: Vec<String> = if all {
                catalog.threads.keys().cloned().collect()
            } else {
                id.into_iter().map(|s| s.to_ascii_lowercase()).collect()
            };
            ids.sort_by_cached_key(|id| {
                std::cmp::Reverse(catalog.chain(id).map(|c| c.len()).unwrap_or(0))
            });
            let bulk = all || ids.len() > 1;
            let show_summary = ids.len() > 1 || verbose;
            let mut preflight_refused = [0; 3];
            if apply {
                crate::print_stdout(format_args!("가져오기 계획 · 대화 {}개", ids.len()))?;
                (ids, preflight_refused) =
                    preview_imports(&store, &catalog, &ids, bulk, keep_source, clean_stale, all)?;
                use crate::sessions_confirm::{self, Mode};
                if !ids.is_empty() {
                    let mode = sessions_confirm::ask(
                        yes,
                        if keep_source { Mode::Share } else { Mode::Move },
                        true,
                        "quota-baton sessions import --from <HOME> <UUID> --apply",
                    )?;
                    keep_source = matches!(mode, Mode::Share);
                }
            }
            let mut imported = 0;
            let mut skipped = 0;
            let [mut live, mut diverged, mut failed] = preflight_refused;
            for id in ids {
                match import_one(
                    &store,
                    &catalog,
                    &id,
                    ImportOptions {
                        apply,
                        skip_existing: all,
                        announce: !apply,
                        keep_source,
                        clean_stale,
                    },
                ) {
                    Ok(true) => imported += 1,
                    Ok(false) => skipped += 1,
                    Err(error) if bulk => {
                        crate::sessions_progress::check()?;
                        let class = import_refusal_class(&error);
                        match class {
                            0 => live += 1,
                            1 => diverged += 1,
                            _ => failed += 1,
                        }
                        let outcome = import_refusal_label(class);
                        crate::print_stdout(format_args!("{id}: {outcome}"))?;
                    }
                    Err(error) => return Err(error),
                }
            }
            if show_summary {
                crate::print_stdout(format_args!(
                    "결과 | {} | 이미 가져옴 | 실행 중 건너뜀 | 분기 건너뜀 | 실패",
                    if !apply {
                        "처리 예정"
                    } else if keep_source {
                        "공유"
                    } else {
                        "이동"
                    }
                ))?;
                crate::print_stdout(format_args!(
                    "개수 | {imported} | {skipped} | {live} | {diverged} | {failed}"
                ))?;
            }
            Ok(())
        }
        SessionsCommand::Unimport {
            id,
            apply,
            force,
            yes,
        } => {
            if staged::recorded(&store, &id)? {
                if staged::pending(&store)? {
                    ensure!(
                        apply,
                        "미완료 이동은 sessions recover로 복구하세요. 미리보기에서는 변경하지 않습니다"
                    );
                    staged::recover(&store, &id)
                } else {
                    staged::return_current(&store, &id, None, yes, apply)
                }
            } else {
                unimport(&store, &id, apply, force)
            }
        }
        SessionsCommand::Return { id, to, yes } => {
            staged::return_current(&store, &id, to.as_deref(), yes, true)
        }
        SessionsCommand::Recover { id, resolve_map } => {
            if resolve_map {
                crate::sessions_confirm::ask_resolve(
                    "quota-baton sessions recover <UUID> --resolve-map",
                )?;
                staged::recover_resolve(&store, &id)
            } else {
                staged::recover(&store, &id)
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ImportOptions {
    apply: bool,
    skip_existing: bool,
    announce: bool,
    keep_source: bool,
    clean_stale: bool,
}

fn import_one(
    store: &Store,
    catalog: &discovery::Catalog,
    id: &str,
    options: ImportOptions,
) -> Result<bool> {
    if !options.keep_source {
        return staged::import(store, catalog, id, options);
    }
    import_one_using(store, catalog, id, options, |a, b| fs::hard_link(a, b))
}

fn import_one_using(
    store: &Store,
    catalog: &discovery::Catalog,
    id: &str,
    options: ImportOptions,
    linker: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<bool> {
    returning::ensure_not_pending(store, id)?;
    let ImportOptions {
        apply,
        skip_existing,
        announce,
        keep_source,
        clean_stale,
    } = options;
    admission_check(&store.home, Some(id))?;
    let initial_files = migration_files(catalog, id)?;
    let fenced_ids: BTreeSet<_> = std::iter::once(id.to_owned())
        .chain(
            initial_files
                .iter()
                .flat_map(|f| [f.thread.clone(), f.physical.clone()]),
        )
        .collect();
    let mut admission_guards = Vec::new();
    for selected in &fenced_ids {
        admission_check(&store.home, Some(selected))?;
        if apply {
            admission_guards.push(crate::sessions_admission::exclusive(
                &[catalog.home.clone(), store.target.clone()],
                selected,
            )?);
        }
    }
    let _lock = if apply { Some(store.lock()?) } else { None };
    // Recheck durable admission under exclusion/store lock. Include ancestors
    // and physical aliases that sharing would make discoverable in Q.
    for selected in &fenced_ids {
        admission_check(&store.home, Some(selected))?;
    }
    let mut journal = store.load()?;
    if let Some(entry) = journal.imports.get(id) {
        let names = entry
            .files
            .iter()
            .flat_map(|f| [f.source.clone(), f.target.clone()])
            .filter(|p| p.exists())
            .collect::<Vec<_>>();
        let (homes, names) = store.probes(&entry.source_home, &entry.threads, &names)?;
        let _guards = guard::probe_decoded(&homes, &entry.threads, &names)?;
        verify_links(entry, false)?;
        if entry.complete {
            if announce {
                crate::print_stdout(format_args!("{id}: 이미 가져온 대화입니다"))?;
            }
            return Ok(!skip_existing);
        }
        bail!("중단된 가져오기가 있습니다. sessions unimport로 되돌린 뒤 다시 시도하세요");
    }
    let files = migration_files(catalog, id)?;
    ensure!(catalog.home != store.target, "원본과 대상 홈이 같습니다");
    discovery::refuse_recent(&files, SystemTime::now())?;
    let threads: Vec<_> = files
        .iter()
        .map(|f| f.thread.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ensure!(
        files
            .iter()
            .all(|f| fenced_ids.contains(&f.thread) && fenced_ids.contains(&f.physical)),
        "대화 관계가 사전 검사 뒤 변경되었습니다"
    );
    let mut inventory_homes = vec![catalog.home.clone(), store.target.clone()];
    inventory_homes.extend(store.search_homes.iter().cloned());
    ensure_readable_inventory(&inventory_homes, &fenced_ids)?;
    let names: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
    let (homes, names) = store.probes(&catalog.home, &threads, &names)?;
    let _guards = if apply {
        guard::reserve_decoded(&homes, &threads, &names)?
    } else {
        guard::probe_decoded(&homes, &threads, &names)?
    };
    let retain_ancestor =
        !keep_source && source_has_dependents(&discovery::catalog(&catalog.home)?, id)?;
    check_destination_inventory(&catalog.home, &store.target, &files)?;
    let stale = stale_plan(store, catalog, id)?;
    if announce {
        for copy in &stale {
            let bytes: u64 = copy.names.iter().map(|n| n.bytes).sum();
            crate::print_stdout(format_args!(
                "다른 홈의 같은 기록: {} · {}개 · {} · {}",
                crate::sessions_presentation::readable_path(
                    &copy.home.to_string_lossy(),
                    &std::env::var("USERPROFILE").unwrap_or_default()
                ),
                copy.names.len(),
                crate::sessions_presentation::size(bytes),
                if clean_stale || !keep_source {
                    "정리 후 journal에서 복구 가능"
                } else {
                    "유지 (--clean-stale로만 정리)"
                }
            ))?;
        }
    }
    let metadata_ids = if keep_source {
        threads.clone()
    } else {
        vec![id.to_owned()]
    };
    let mut database = db::plan(&catalog.home, &store.target, &metadata_ids)?;
    for id in &metadata_ids {
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
                model_provider: item.provider.clone().context("대화 제공자가 없습니다")?,
            };
            db::add_synthetic_thread(&mut database, &info, fields).map_err(|_| {
                anyhow::anyhow!("기록에서 필요한 대화 메타데이터를 복구하지 못했습니다")
            })?;
        }
    }
    if announce {
        preview(id, &files, &store.target, &database)?;
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
            thread: file.thread.clone(),
            moved: false,
            retained_ancestor: retain_ancestor && file.thread == id,
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
        root: id.into(),
        keep_source,
        move_started: false,
        move_done: false,
        source_database: crate::cutover::protect(&serde_json::to_vec(&db::source_snapshot(
            &catalog.home,
            &[id.into()],
        )?)?)?,
        stale: if clean_stale || !keep_source {
            stale
        } else {
            Vec::new()
        },
        session_map: if keep_source {
            None
        } else {
            Some(crate::sessions_map::plan(
                &store.map_path,
                id,
                &store.target,
                &store.home.join("journal/session-map-backups"),
            )?)
        },
    };
    let _commit = crate::sessions_progress::commit()?;
    crate::sessions_progress::phase(crate::sessions_progress::Phase::Linking, None)?;
    journal.imports.insert(id.into(), record.clone());
    store.save(&journal)?;
    checkpoint(store, id, "prepared")?;
    for link in &record.files {
        if link.owned {
            if let Some(parent) = link.target.parent() {
                fs::create_dir_all(parent)?;
            }
            guard::safe_path(&store.target, &link.target)?;
            linker(&link.source,&link.target).map_err(|_|anyhow::anyhow!("하드링크를 만들 수 없습니다. 같은 드라이브의 홈을 사용하세요. 기록은 복사하지 않았습니다"))?;
        }
    }
    checkpoint(store, id, "linked")?;
    crate::sessions_progress::phase(crate::sessions_progress::Phase::Records, None)?;
    db::apply(&database)?;
    checkpoint(store, id, "target_rows")?;
    update_index(&store.target, &record.index, false)?;
    checkpoint(store, id, "index_appended")?;
    verify_links(&record, false)?;
    db::verify(&database)?;
    checkpoint(store, id, "verified")?;
    if !keep_source {
        journal.imports.get_mut(id).unwrap().move_started = true;
        for removed in record
            .files
            .iter()
            .filter(|l| l.thread == id && !l.retained_ancestor)
        {
            for entry in journal.imports.values_mut() {
                for alias in &mut entry.files {
                    if alias.source == removed.source && alias.identity == removed.identity {
                        alias.moved = true;
                    }
                }
            }
        }
        store.save(&journal)?;
        checkpoint(store, id, "retiring")?;
        let source: db::DbPlan =
            serde_json::from_slice(&crate::cutover::unprotect(&record.source_database)?)?;
        // Verify completed while all writer guards remain held, then remove only
        // this thread's own names/rows. Shared fork ancestors remain untouched.
        for link in record.files.iter().filter(|l| l.thread == id) {
            ensure!(
                guard::identity(&link.source)? == link.identity
                    && guard::identity(&link.target)? == link.identity,
                "이동 검증 중 대화 기록이 변경되었습니다"
            );
        }
        db::remove_exact(&source)?;
        checkpoint(store, id, "source_rows_removed")?;
        for link in record
            .files
            .iter()
            .filter(|l| l.thread == id && !l.retained_ancestor)
        {
            guard::remove_verified(&record.source_home, &link.source, link.identity)?;
            checkpoint(store, id, "source_name_removed")?;
        }
        journal.imports.get_mut(id).unwrap().move_done = true;
        store.save(&journal)?;
        checkpoint(store, id, "source_retired")?;
    }
    clean_stale_copies(&record.stale, &store.home)?;
    checkpoint(store, id, "aliases_retired")?;
    if let Some(map) = &record.session_map {
        crate::sessions_map::apply(map)?;
        crate::sessions_map::verify(map)?;
    }
    checkpoint(store, id, "map_committed")?;
    journal.imports.get_mut(id).unwrap().complete = true;
    store.save(&journal)?;
    checkpoint(store, id, "committed")?;
    // Legacy sharing keeps its transaction as one safe step. Cancellation may
    // stop here only after the durable journal reports the share complete.
    crate::sessions_progress::boundary()?;
    if announce && keep_source {
        crate::print_stdout(format_args!(
            "주의: 두 홈의 대화 기록은 같은 파일입니다. 양쪽에서 동시에 실행하면 기록이 손상될 수 있습니다."
        ))?;
    }
    Ok(true)
}

fn unimport(store: &Store, id: &str, apply: bool, force: bool) -> Result<()> {
    returning::ensure_not_pending(store, id)?;
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    let _lock = if apply { Some(store.lock()?) } else { None };
    let mut journal = store.load()?;
    let entry = journal
        .imports
        .get(id)
        .context("가져온 대화가 아닙니다")?
        .clone();
    let mut names: Vec<_> = entry
        .files
        .iter()
        .flat_map(|f| [f.source.clone(), f.target.clone()])
        .filter(|p| p.exists())
        .collect();
    for copy in &entry.stale {
        for name in &copy.names {
            names.extend(
                [name.original.clone(), name.backup.clone()]
                    .into_iter()
                    .filter(|p| p.exists()),
            );
        }
    }
    let recorded = entry
        .stale
        .iter()
        .map(|c| c.home.clone())
        .collect::<Vec<_>>();
    let (mut homes, names) =
        store.probes_with_recorded_homes(&entry.source_home, &entry.threads, &names, &recorded)?;
    homes.extend(entry.stale.iter().map(|c| c.home.clone()));
    homes.push(store.home.clone());
    homes.sort();
    homes.dedup();
    // Decoded imports may own compressed names. Journal identity authorizes
    // unlink-only cleanup; this path does not publish or reinterpret headers.
    let compressed_names = names
        .iter()
        .any(|p| p.extension().is_some_and(|ext| ext == "zst"));
    let mut guards = match (apply, compressed_names) {
        (true, true) => guard::reserve_decoded(&homes, &entry.threads, &names)?,
        (false, true) => guard::probe_decoded(&homes, &entry.threads, &names)?,
        (true, false) => guard::reserve_cleanup(&homes, &entry.threads, &names)?,
        (false, false) => guard::probe_cleanup(&homes, &entry.threads, &names)?,
    };
    for link in &entry.files {
        if link.target.exists() {
            ensure!(
                guard::identity(&link.target)? == link.identity,
                "대상 파일 소유권이 달라 되돌릴 수 없습니다"
            );
            let retained_by_other = journal.imports.iter().any(|(other_id, other)| {
                other_id != id
                    && other
                        .files
                        .iter()
                        .any(|peer| peer.target == link.target && peer.identity == link.identity)
            });
            let restores_source = entry.move_started
                && (link.moved || link.thread == entry.root)
                && !link.source.exists();
            if link.owned && !retained_by_other && !restores_source {
                ensure!(
                    guard::hardlink_count(&link.target)? > 1,
                    "세션 파일의 마지막 하드링크는 삭제할 수 없습니다"
                );
            }
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
    journal.imports.get_mut(id).unwrap().complete = false;
    store.save(&journal)?;
    if entry.move_started {
        // Restore original names/rows first. A crash is retryable and never leaves
        // this half-unimport advertised as a usable completed import.
        for link in entry
            .files
            .iter()
            .filter(|l| l.moved || l.thread == entry.root)
        {
            if link.source.exists() {
                ensure!(
                    guard::identity(&link.source)? == link.identity,
                    "원본 위치에 다른 기록이 있어 복구하지 못했습니다"
                );
            } else {
                ensure!(
                    guard::identity(&link.target)? == link.identity,
                    "이동한 기록을 복구할 수 없습니다"
                );
                if let Some(parent) = link.source.parent() {
                    fs::create_dir_all(parent)?;
                }
                guard::safe_path(&entry.source_home, &link.source)?;
                fs::hard_link(&link.target, &link.source)?;
            }
            for other in journal.imports.values_mut() {
                for alias in &mut other.files {
                    if alias.source == link.source && alias.identity == link.identity {
                        alias.moved = false;
                    }
                }
            }
        }
        let source: db::DbPlan =
            serde_json::from_slice(&crate::cutover::unprotect(&entry.source_database)?)?;
        db::apply(&source)?;
    }
    restore_stale_copies(&entry.stale, &store.home)?;
    if let Some(map) = &entry.session_map {
        crate::sessions_map::restore(map)?;
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
    journal.imports.get_mut(id).unwrap().complete = false;
    store.save(&journal)?;
    db::rollback_after_resume(&database, force)?;
    let remove_index = entry
        .index
        .iter()
        .filter(|s| !retained_index.contains(*s))
        .cloned()
        .collect::<Vec<_>>();
    update_index(&store.target, &remove_index, true)?;
    guards.release_rollouts_for_unlink();
    for link in &entry.files {
        if link.owned && !retained.contains(&link.target) && link.target.exists() {
            guard::unlink_validated(
                &entry.target_home,
                &link.target,
                link.identity,
                guard::hardlink_count(&link.target)?,
                |file| {
                    ensure!(
                        force || file.metadata()?.len() == link.length,
                        "가져온 뒤 기록이 변경되었습니다. 확인 후 --force를 사용하세요"
                    );
                    Ok(())
                },
            )?;
        }
    }
    journal.imports.remove(id);
    store.save(&journal)
}

pub(crate) fn resume_uuid(args: &[OsString]) -> Result<Option<String>> {
    let mut index = 0;
    let mut resume = false;
    let mut literal = false;
    let mut exec = false;
    let mut requested = None;
    let mut positional_seen = false;
    let mut last = false;
    while let Some(arg) = args.get(index).and_then(|v| v.to_str()) {
        index += 1;
        if !literal && arg == "--" {
            if !resume {
                return Ok(None);
            }
            literal = true;
            continue;
        }
        if !literal && matches!(arg, "--help" | "-h" | "--version" | "-V") {
            return Ok(None);
        }
        if !literal && arg.starts_with('-') {
            let (name, inline) = arg
                .split_once('=')
                .map_or((arg, false), |(name, _)| (name, true));
            let valued = matches!(
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
            );
            if valued {
                if !inline {
                    ensure!(index < args.len(), "재개 옵션의 값이 없습니다");
                    index += 1;
                }
                continue;
            }
            if name == "--last" {
                last = true;
                continue;
            }
            if arg.len() > 2
                && ["-c", "-m", "-p", "-s", "-C", "-a", "-i", "-o"]
                    .iter()
                    .any(|p| arg.starts_with(p))
            {
                continue;
            }
            if matches!(
                name,
                "--all"
                    | "--last"
                    | "--include-non-interactive"
                    | "--search"
                    | "--no-alt-screen"
                    | "--no-daemon"
                    | "--strict-config"
                    | "--oss"
                    | "--approve-for-me"
                    | "--dangerously-bypass-approvals-and-sandbox"
                    | "--dangerously-bypass-hook-trust"
                    | "--skip-git-repo-check"
                    | "--json"
                    | "--ephemeral"
                    | "--full-auto"
            ) {
                continue;
            }
            // Native options are opaque. Keep a UUID already parsed as the
            // resume positional, but never guess an unknown option's arity.
            return Ok(if last { None } else { requested });
        }
        if !resume {
            match arg {
                "resume" => resume = true,
                "exec" | "e" if !exec => exec = true,
                _ => return Ok(None),
            }
        } else if !positional_seen {
            positional_seen = true;
            requested = discovery::valid_id(arg).then(|| arg.to_ascii_lowercase());
        }
    }
    Ok(if last { None } else { requested })
}

fn resume_entry<'a>(journal: &'a Journal, id: &str) -> Result<Option<&'a Import>> {
    ensure!(
        !journal
            .imports
            .values()
            .any(|entry| !entry.complete && entry.threads.iter().any(|t| t == id)),
        "대화 가져오기가 완료되지 않았습니다"
    );
    Ok(journal.imports.get(id).or_else(|| {
        journal
            .imports
            .values()
            .find(|entry| entry.threads.iter().any(|t| t == id))
    }))
}

pub(crate) fn before_codex_resume(args: &[OsString]) -> Result<()> {
    before_codex_resume_at(args, None)
}

fn resume_in_target(store: &Store, id: &str, previous_owner: Option<&Path>) -> Result<bool> {
    returning::ensure_not_pending(store, id)?;
    let journal = store.load()?;
    // Pending legacy work still fences the ID. Completed shares cannot
    // override the newer staged move's authoritative owner.
    let entry = resume_entry(&journal, id)?;
    let entry = if let Some(owner) = previous_owner {
        if owner != store.target.as_path() {
            ensure!(
                db::thread_path(&store.target, id)?.is_none(),
                "대화 소유권과 목록이 다릅니다. doctor로 확인하세요"
            );
            return Ok(false);
        }
        ensure!(
            db::thread_path(&store.target, id)?.is_some(),
            "대화 소유권과 목록이 다릅니다. doctor로 확인하세요"
        );
        None
    } else {
        entry
    };
    if let Some(entry) = entry
        && (entry.root == id || db::thread_path(&store.target, id)?.is_some())
    {
        ensure!(entry.complete, "대화 가져오기가 완료되지 않았습니다");
        let names = entry
            .files
            .iter()
            .flat_map(|f| [f.source.clone(), f.target.clone()])
            .filter(|p| p.exists())
            .collect::<Vec<_>>();
        let (homes, names) = store.probes(&entry.source_home, &entry.threads, &names)?;
        let _guards = guard::probe_decoded(&homes, &entry.threads, &names)?;
        verify_links(entry, false)?;
        return Ok(true);
    }
    if db::thread_path(&store.target, id)?.is_some() {
        let mut catalog = discovery::catalog(&store.target)?;
        // Q already owns this row. Bypass only import provider eligibility in
        // this disposable catalog; retain chain/path validation and writer probes.
        // Historical CPA ancestors are valid local history, not new imports.
        for file in catalog.rollouts.values_mut().chain(&mut catalog.aliases) {
            file.provider = Some("openai".into());
        }
        let files = catalog.chain(id)?;
        let names = files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        let threads = files.iter().map(|f| f.thread.clone()).collect::<Vec<_>>();
        // Probe Q directly: rollout sharing detects writers through hardlinks
        // in other homes without external import discovery.
        let _guards = guard::probe_decoded(std::slice::from_ref(&store.target), &threads, &names)?;
        return Ok(true);
    }
    Ok(false)
}

/// Resolve only the requested journal before the ordinary admission veto.
/// The returned scope must end before starting a native child.
pub(crate) fn prepare_resume(
    home: &Path,
    id: Option<&str>,
    target: Option<&Path>,
) -> Result<Option<crate::sessions_progress::Operation>> {
    let Some(id) = id else {
        return Ok(None);
    };
    let progress = crate::sessions_progress::Operation::begin(id, progress_english(home))?;
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    let store = Store::open_selected(home, target)?;
    ensure!(
        staged::offer_recovery(&store, id, false)? != Some(false),
        "이전 이동을 되돌렸습니다. 원본 홈에서 대화를 다시 열어 주세요."
    );
    Ok(Some(progress))
}

pub(crate) fn before_codex_resume_at(args: &[OsString], target: Option<&Path>) -> Result<()> {
    let requested = resume_uuid(args)?;
    let _progress = prepare_resume(&qb_core::home()?, requested.as_deref(), target)?;
    admission_check(&qb_core::home()?, requested.as_deref())?;
    let Some(id) = requested else {
        return Ok(());
    };
    let id = id.as_str();
    let store = Store::open_selected(&qb_core::home()?, target)?;
    let previous_owner = staged::owner(&store, id)?;
    if resume_in_target(&store, id, previous_owner.as_deref())? {
        return Ok(());
    }
    let user = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .context("사용자 홈 경로가 없습니다")?;
    let mut matches = Vec::new();
    let mut candidates = known_homes(&user);
    if let Some(owner) = previous_owner
        && !candidates.contains(&owner)
    {
        candidates.push(owner);
    }
    for home in candidates {
        crate::sessions_progress::check()?;
        if fs::canonicalize(&home).ok().as_ref() == Some(&store.target) {
            continue;
        }
        if let Ok(catalog) = discovery::catalog(&home)
            && catalog.threads.contains_key(id)
        {
            matches.push(catalog);
        }
    }
    crate::sessions_progress::check()?;
    ensure!(
        !matches.is_empty(),
        "기존 홈에서 대화를 찾지 못했습니다. sessions import --from으로 원본을 지정하세요"
    );
    // Several homes may contain the same linked thread. Do not pick a forked copy.
    let first = matches[0].chain(id)?;
    for other in matches.iter().skip(1) {
        let files = other.chain(id)?;
        let mut equal = files.len() == first.len();
        for file in &files {
            if let Some(peer) = first.iter().find(|peer| peer.physical == file.physical) {
                equal &= same_contents(&file.path, &peer.path)?;
            } else {
                equal = false;
            }
        }
        equal &= db::compare_goal_state(&[matches[0].home.clone(), other.home.clone()], id)?;
        if !equal {
            return Err(InventoryConflict {
                homes: matches.iter().map(|catalog| catalog.home.clone()).collect(),
            }
            .into());
        }
    }
    crate::sessions_admission::verify_installed_version()?;
    import_one(
        &store,
        &matches[0],
        id,
        ImportOptions {
            apply: false,
            skip_existing: false,
            announce: true,
            keep_source: DEFAULT_KEEP_SOURCE,
            clean_stale: false,
        },
    )?;
    use crate::sessions_confirm::{self, Mode};
    let manual = format!(
        "quota-baton sessions import --from \"{}\" {id} --apply",
        matches[0].home.display()
    );
    let mode = sessions_confirm::ask(false, Mode::Move, true, &manual)?;
    let keep_source = matches!(mode, Mode::Share);
    if keep_source {
        crate::print_stdout(format_args!(
            "경고: 공유한 대화를 두 홈에서 동시에 열지 마세요. 같은 기록을 수정합니다."
        ))?;
    }
    import_one(
        &store,
        &matches[0],
        id,
        ImportOptions {
            apply: true,
            skip_existing: false,
            announce: false,
            keep_source,
            clean_stale: false,
        },
    )?;
    let action = if keep_source {
        "연결했습니다"
    } else {
        "옮겼습니다"
    };
    crate::print_stdout(format_args!(
        "기존 대화를 qb로 {action} ({} → qb)",
        matches[0].home.display()
    ))?;
    crate::sessions_progress::phase(crate::sessions_progress::Phase::Starting, None)?;
    Ok(())
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;

#[path = "sessions_return_compat.rs"]
mod return_compat;
use return_compat as returning;

#[path = "sessions_doctor.rs"]
mod doctor_impl;
#[path = "sessions_move.rs"]
mod staged;

pub(crate) fn doctor(home: &Path) -> Result<()> {
    doctor_impl::run(&Store::open(home)?)
}

/// Journals fence their own IDs, not the whole dedicated home. Unnamed fresh
/// launches/pickers do not request one of those IDs and must remain available.
pub(crate) fn admission_check(home: &Path, requested: Option<&str>) -> Result<()> {
    let Some(id) = requested else {
        return Ok(());
    };
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    let dir = home.join("journal/session-moves");
    guard::safe_path(home, &dir)?;
    if dir.exists() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            guard::safe_path(home, &path)?;
            let exact = path.file_stem().and_then(|v| v.to_str()) == Some(id);
            if fs::metadata(&path)?.len() >= 64 * 1024 * 1024 {
                ensure!(!exact, "이 대화의 이동 journal이 잘못되었습니다");
                continue;
            }
            let value: serde_json::Value = match serde_json::from_slice(&fs::read(&path)?) {
                Ok(value) => value,
                Err(_) if !exact => continue,
                Err(_) => bail!("이 대화의 이동 journal이 잘못되었습니다"),
            };
            let mentions = exact
                || value["ids"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|v| v.as_str() == Some(id)));
            if mentions {
                ensure!(
                    value["schema"] == 2 && value["open"].as_bool() == Some(false),
                    "이 대화는 이동 복구가 필요합니다. sessions recover로 먼저 복구하세요"
                );
            }
        }
    }
    if !home.join("journal/wiring.json").exists() {
        return Ok(());
    }
    // Admission reads journal scopes only; it must not discover user profiles
    // or resolve a session map merely to check either managed home's journal.
    let homes = wired_homes(home)?;
    let mut store = Store {
        home: home.to_owned(),
        target: homes[0].clone(),
        search_homes: Vec::new(),
        map_path: PathBuf::new(),
    };
    return_compat::ensure_not_pending(&store, id)?;
    for target in homes {
        store.target = target;
        let legacy = store.load()?;
        resume_entry(&legacy, id)?;
    }
    Ok(())
}

#[cfg(test)]
mod n5c_urgent_args_tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Args {
        #[command(subcommand)]
        command: SessionsCommand,
    }

    #[test]
    fn n5c_sessions_urgent_accepts_only_explicit_on_or_off() {
        for state in ["on", "off"] {
            let args = Args::try_parse_from(["qb", "urgent", "synthetic", state]).unwrap();
            assert!(
                matches!(args.command, SessionsCommand::Urgent { id, state: value }
                if id == "synthetic" && value == state)
            );
        }
        for args in [
            vec!["qb", "urgent", "synthetic"],
            vec!["qb", "urgent", "synthetic", "move"],
            vec!["qb", "urgent", "synthetic", "true"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
    }
}
