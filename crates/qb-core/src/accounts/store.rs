use super::{Account, Tokens, crypto, lease::Lease};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
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
    _lease: Lease,
}

impl AccountStore {
    // 조회와 미리보기는 실행 중인 코어의 lease를 빼앗지 않는다.
    pub fn list_metadata(home: &Path) -> Result<Vec<Account>, StoreError> {
        Self::read_metadata_dir(&home.join("accounts"))
    }
    pub fn acquire(home: &Path) -> Result<Self, StoreError> {
        let dir = home.join("accounts");
        fs::create_dir_all(&dir).map_err(|_| StoreError::Io)?;
        let lease = Lease::acquire(&dir)?;
        Ok(Self {
            dir,
            gate: Mutex::new(()),
            _lease: lease,
        })
    }

    pub(crate) fn transaction(&self) -> Result<MutexGuard<'_, ()>, StoreError> {
        self.gate.lock().map_err(|_| StoreError::Synchronization)
    }

    pub fn list(&self) -> Result<Vec<Account>, StoreError> {
        let _guard = self.transaction()?;
        self.list_unlocked()
    }

    pub(crate) fn list_unlocked(&self) -> Result<Vec<Account>, StoreError> {
        Self::read_metadata_dir(&self.dir)
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
        Ok(self.read(id)?.account)
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
        let mut record: Record =
            serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
        Self::validate(&record.account)?;
        if matches!(&record.account.health, super::Health::Cooling { until, .. } if *until <= chrono::Utc::now())
        {
            record.account.health = super::Health::Ready;
        }
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
