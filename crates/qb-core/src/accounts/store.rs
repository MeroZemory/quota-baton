use super::{Account, Health, Tokens, crypto, lease::Lease};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FlushFileBuffers, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
    },
    core::PCWSTR,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("credential lease is already held")]
    LeaseBusy,
    #[error("credential lease is unavailable")]
    LeaseUnavailable,
    #[error("account storage I/O failed")]
    Io,
    #[error("invalid account data")]
    InvalidData,
    #[error("account secret protection failed")]
    Crypto,
    #[error("account was not found")]
    NotFound,
    #[error("account generation conflict (expected {expected:?}, actual {actual:?})")]
    GenerationConflict {
        expected: Option<u64>,
        actual: Option<u64>,
    },
    #[error("account identity hash collision")]
    IdentityCollision,
    #[error("account token metadata is older than stored metadata")]
    OlderToken,
    #[error("account store synchronization failed")]
    Synchronization,
    #[error("adopt is a cutover step (M8)")]
    AdoptUnavailable,
}

#[derive(Serialize, Deserialize)]
struct Record {
    #[serde(flatten)]
    account: Account,
    secret_dpapi_b64: String,
}

pub struct AccountStore {
    dir: PathBuf,
    gate: Mutex<()>,
    observer: Mutex<Option<AccountObserver>>,
    _lease: Lease,
}

pub type AccountObserver = Arc<dyn Fn(&Account, &Health) + Send + Sync>;

impl AccountStore {
    pub fn home(&self) -> &Path {
        self.dir.parent().expect("account store has a home")
    }

    fn write_settings_unlocked(
        &self,
        id: &str,
        drain: Option<bool>,
        allow_credits: Option<bool>,
    ) -> Result<(), StoreError> {
        self.path(id)?;
        let path = self.home().join("config.toml");
        let mut value: toml_edit::DocumentMut = match fs::read_to_string(&path) {
            Ok(text) => text.parse().map_err(|_| StoreError::InvalidData)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::new(),
            Err(_) => return Err(StoreError::Io),
        };
        let accounts = value
            .entry("accounts")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_like_mut()
            .ok_or(StoreError::InvalidData)?;
        let settings = accounts
            .entry(id)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_like_mut()
            .ok_or(StoreError::InvalidData)?;
        for (key, flag) in [("drain", drain), ("allow_credits", allow_credits)] {
            if let Some(flag) = flag {
                let decor = settings
                    .get(key)
                    .and_then(toml_edit::Item::as_value)
                    .map(|v| v.decor().clone())
                    .unwrap_or_default();
                let mut replacement = toml_edit::Value::from(flag);
                *replacement.decor_mut() = decor;
                settings.insert(key, toml_edit::Item::Value(replacement));
            }
        }
        let bytes = value.to_string();
        self.atomic_write(&path, bytes.as_bytes())
    }
    // 조회와 미리보기는 실행 중인 코어의 lease를 빼앗지 않는다.
    pub fn list_metadata(home: &Path) -> Result<Vec<Account>, StoreError> {
        let mut accounts = Self::read_metadata_dir(&home.join("accounts"))?;
        for account in &mut accounts {
            account.health.expire(chrono::Utc::now());
        }
        Ok(accounts)
    }
    pub fn acquire(home: &Path) -> Result<Self, StoreError> {
        let dir = home.join("accounts");
        fs::create_dir_all(&dir).map_err(|_| StoreError::Io)?;
        let lease = Lease::acquire(&dir)?;
        Ok(Self {
            dir,
            gate: Mutex::new(()),
            observer: Mutex::new(None),
            _lease: lease,
        })
    }

    /// Called after each successful account write, under the transaction gate.
    /// Observers must not query the store, perform I/O, or panic.
    pub fn set_observer(&self, observer: AccountObserver) {
        *self
            .observer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(observer);
    }

    fn notify(&self, account: &Account, previous: &Health) {
        let observer = self
            .observer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(observer) = observer {
            observer(account, previous);
        }
    }

    pub(crate) fn transaction(&self) -> Result<MutexGuard<'_, ()>, StoreError> {
        self.gate.lock().map_err(|_| StoreError::Synchronization)
    }

    pub fn list(&self) -> Result<Vec<Account>, StoreError> {
        let _guard = self.transaction()?;
        self.list_unlocked()
    }

    pub(crate) fn list_unlocked(&self) -> Result<Vec<Account>, StoreError> {
        Self::read_metadata_dir(&self.dir)?
            .into_iter()
            .map(|account| self.load_unlocked(&account.id))
            .collect()
    }

    fn read_metadata_dir(dir: &Path) -> Result<Vec<Account>, StoreError> {
        let mut accounts = Vec::new();
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(accounts),
            Err(_) => return Err(StoreError::Io),
        };
        for entry in entries {
            let path = entry.map_err(|_| StoreError::Io)?.path();
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                let record = Self::read_path(&path)?;
                if path.file_stem().and_then(|s| s.to_str()) != Some(&record.account.id) {
                    return Err(StoreError::InvalidData);
                }
                accounts.push(record.account);
            }
        }
        accounts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(accounts)
    }

    pub fn load(&self, id: &str) -> Result<Account, StoreError> {
        let _guard = self.transaction()?;
        self.load_unlocked(id)
    }

    fn load_unlocked(&self, id: &str) -> Result<Account, StoreError> {
        let mut record = self.read(id)?;
        if record.account.health.expire(chrono::Utc::now()) {
            let generation = record.account.generation;
            self.write_metadata_record(record, generation)
        } else {
            Ok(record.account)
        }
    }

    /// Mutates current metadata while holding the credential lease and transaction.
    /// No token decryption is performed. Async callers must use spawn_blocking.
    pub fn update_metadata(
        &self,
        id: &str,
        update: impl FnOnce(&mut Account),
    ) -> Result<Account, StoreError> {
        let _guard = self.transaction()?;
        self.update_metadata_unlocked(id, update)
    }

    pub fn update_settings(
        &self,
        id: &str,
        drain: Option<bool>,
        allow_credits: Option<bool>,
        update: impl FnOnce(&mut Account),
    ) -> Result<Account, StoreError> {
        let _guard = self.transaction()?;
        self.read(id)?;
        if drain.is_some() || allow_credits.is_some() {
            self.write_settings_unlocked(id, drain, allow_credits)?;
        }
        self.update_metadata_unlocked(id, update)
    }

    fn update_metadata_unlocked(
        &self,
        id: &str,
        update: impl FnOnce(&mut Account),
    ) -> Result<Account, StoreError> {
        let mut record = self.read(id)?;
        let current = record.account.clone();
        update(&mut record.account);
        Self::validate(&record.account)?;
        if record.account.id != current.id
            || record.account.identity != current.identity
            || record.account.source != current.source
            || record.account.token_meta != current.token_meta
            || record.account.generation != current.generation
        {
            return Err(StoreError::InvalidData);
        }
        self.write_metadata_record(record, current.generation)
    }

    fn write_metadata_record(
        &self,
        mut record: Record,
        generation: u64,
    ) -> Result<Account, StoreError> {
        let previous = self.read(&record.account.id)?.account;
        Self::check_generation(Some(generation), Some(previous.generation))?;
        record.account.generation = generation.checked_add(1).ok_or(StoreError::InvalidData)?;
        let bytes = serde_json::to_vec_pretty(&record).map_err(|_| StoreError::InvalidData)?;
        self.atomic_write(&self.path(&record.account.id)?, &bytes)?;
        self.notify(&record.account, &previous.health);
        Ok(record.account)
    }

    pub fn tokens(&self, account: &Account) -> Result<Tokens, StoreError> {
        let _guard = self.transaction()?;
        self.tokens_unlocked(account)
    }

    pub(crate) fn tokens_unlocked(&self, account: &Account) -> Result<Tokens, StoreError> {
        let record = self.read(&account.id)?;
        Self::check_generation(Some(account.generation), Some(record.account.generation))?;
        crypto::decrypt(&record.secret_dpapi_b64)
    }

    // None은 신규 생성, Some(N)은 N에서 읽은 메타데이터의 갱신이다.
    pub fn save(
        &self,
        account: &Account,
        tokens: &Tokens,
        expected: Option<u64>,
    ) -> Result<Account, StoreError> {
        let _guard = self.transaction()?;
        self.save_unlocked(account, tokens, expected)
    }

    pub(crate) fn save_unlocked(
        &self,
        account: &Account,
        tokens: &Tokens,
        expected: Option<u64>,
    ) -> Result<Account, StoreError> {
        Self::validate(account)?;
        let old = match self.read(&account.id) {
            Ok(record) => Some(record),
            Err(StoreError::NotFound) => None,
            Err(error) => return Err(error),
        };
        if old.as_ref().is_some_and(|old| {
            old.account.identity.canonical_key() != account.identity.canonical_key()
        }) {
            return Err(StoreError::IdentityCollision);
        }
        Self::check_generation(expected, old.as_ref().map(|old| old.account.generation))?;
        if old.as_ref().is_some_and(|old| {
            account
                .token_meta
                .compare(&old.account.token_meta)
                .is_some_and(|order| order.is_lt())
        }) {
            return Err(StoreError::OlderToken);
        }
        if expected.is_some_and(|n| n != account.generation) {
            return Err(StoreError::InvalidData);
        }
        let mut updated = account.clone();
        if old
            .as_ref()
            .is_some_and(|old| account.token_meta.newer_than(&old.account.token_meta))
        {
            updated.apply_health_signal(super::HealthSignal::NewerLinkedToken, chrono::Utc::now());
        }
        updated.generation = expected
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(StoreError::InvalidData)?;
        let record = Record {
            account: updated.clone(),
            secret_dpapi_b64: crypto::encrypt(tokens)?,
        };
        let bytes = serde_json::to_vec_pretty(&record).map_err(|_| StoreError::InvalidData)?;
        self.atomic_write(&self.path(&account.id)?, &bytes)?;
        self.notify(
            &updated,
            old.as_ref()
                .map(|old| &old.account.health)
                .unwrap_or(&Health::Ready),
        );
        Ok(updated)
    }

    fn check_generation(expected: Option<u64>, actual: Option<u64>) -> Result<(), StoreError> {
        if expected == actual {
            Ok(())
        } else {
            Err(StoreError::GenerationConflict { expected, actual })
        }
    }

    fn validate(account: &Account) -> Result<(), StoreError> {
        if account.schema != 1
            || account.id != account.identity.id()
            || account.provider != account.identity.provider()
            || matches!(&account.health, super::Health::Cooling { scope, .. }
                if scope != "account" && scope.strip_prefix("model:").is_none_or(|model| model.is_empty()))
        {
            return Err(StoreError::InvalidData);
        }
        Ok(())
    }

    fn path(&self, id: &str) -> Result<PathBuf, StoreError> {
        let valid = (id.starts_with("cl-") || id.starts_with("cx-"))
            && id.len() == 9
            && id[3..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if !valid {
            return Err(StoreError::InvalidData);
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    fn read(&self, id: &str) -> Result<Record, StoreError> {
        let record = Self::read_path(&self.path(id)?)?;
        if record.account.id != id {
            return Err(StoreError::InvalidData);
        }
        Ok(record)
    }

    fn read_path(path: &Path) -> Result<Record, StoreError> {
        let bytes = fs::read(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound
            } else {
                StoreError::Io
            }
        })?;
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
        Self::validate(&record.account)?;
        Ok(record)
    }

    fn atomic_write(&self, destination: &Path, bytes: &[u8]) -> Result<(), StoreError> {
        let path = self
            .dir
            .join(format!(".write-{:032x}.tmp", rand::random::<u128>()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| StoreError::Io)?;
        let result = (|| {
            file.write_all(bytes).map_err(|_| StoreError::Io)?;
            // 이동 전 파일 핸들을 명시적으로 flush한다.
            unsafe {
                FlushFileBuffers(HANDLE(file.as_raw_handle())).map_err(|_| StoreError::Io)?;
            }
            drop(file);
            let source: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let target: Vec<u16> = destination
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(source.as_ptr()),
                    PCWSTR(target.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
                .map_err(|_| StoreError::Io)
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&path);
        }
        result
    }
}
