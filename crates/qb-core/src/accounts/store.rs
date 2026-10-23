use super::{Account, Health, Tokens, crypto, lease::Lease};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
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
    #[error("credential save requires an ordinary native health transition")]
    CredentialHealthChange,
    #[error("account store synchronization failed")]
    Synchronization,
}

#[derive(Serialize, Deserialize)]
struct Record {
    #[serde(flatten)]
    account: Account,
    secret_dpapi_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    takeover_receipt: Option<super::takeover::Receipt>,
}

pub struct AccountStore {
    dir: PathBuf,
    gate: Mutex<()>,
    observer: Mutex<Option<AccountObserver>>,
    pre_mutation: Mutex<Option<PreMutationHook>>,
    lease: Mutex<Option<Lease>>,
    writes_stopped: AtomicBool,
}

pub type AccountObserver = Arc<dyn Fn(&Account, &Health) + Send + Sync>;

/// Internal revocation notification, never permission to publish a proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetadataMutation {
    Ordinary,
    Release,
    QuotaRejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BeforeAccountMutation<'a> {
    Metadata {
        id: &'a str,
        cause: MetadataMutation,
    },
    HealthExpiry {
        id: &'a str,
    },
    Settings {
        id: &'a str,
    },
    FullSave {
        id: &'a str,
    },
    /// A restricted save ATTEMPT, not a continuity certificate or success event.
    /// Current proof invalidation is identical to FullSave, before any read.
    CredentialSave {
        id: &'a str,
        expected_generation: u64,
        refresh_uncertain: bool,
    },
    Retire {
        id: &'a str,
    },
    Stop,
}

/// The runtime adapter owns an Option<AcceptPermit> plus a weak proof owner.
/// Consume/replace only that permit through its own invalidation; failure must
/// leave it empty. No store access, I/O, awaits, panics, or retained references.
/// This callback conveys no success authority to the store.
pub(crate) trait ReportMutation {
    fn before_store_mutation(&mut self, id: &str);
}

pub(crate) enum MetadataCause<'a> {
    Mutation(MetadataMutation),
    UsageReport(&'a mut dyn ReportMutation),
}

pub(crate) type PreMutationHook = Arc<dyn for<'a> Fn(BeforeAccountMutation<'a>) + Send + Sync>;

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
            account.expire_health(chrono::Utc::now());
        }
        Ok(accounts)
    }
    pub fn acquire(home: &Path) -> Result<Self, StoreError> {
        let dir = home.join("accounts");
        fs::create_dir_all(&dir).map_err(|_| StoreError::Io)?;
        let lease = Lease::acquire(&dir)?;
        let store = Self {
            dir,
            gate: Mutex::new(()),
            observer: Mutex::new(None),
            pre_mutation: Mutex::new(None),
            lease: Mutex::new(Some(lease)),
            writes_stopped: AtomicBool::new(false),
        };
        // Validate durable fences before any runtime job can read credentials.
        // Recovery never touches the native source or presents a token.
        store.recover_takeovers()?;
        store.recover_login_migration()?;
        Ok(store)
    }

    /// Release the home lease while read-only runtime objects are still
    /// alive.  Takeover uses this after all writers and refresh tasks stop;
    /// ordinary shutdown still releases it through Drop.
    pub fn release_lease(&self) {
        self.stop_writes();
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        drop(lease);
    }

    /// Serialize the final write boundary with all account transactions.
    pub fn stop_writes(&self) {
        let _guard = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        self.before_mutation(BeforeAccountMutation::Stop);
        self.writes_stopped.store(true, Ordering::Release);
    }

    pub fn writes_stopped(&self) -> bool {
        self.writes_stopped.load(Ordering::Acquire)
    }

    /// Called after each successful account write, under the transaction gate.
    /// Observers must not query the store, perform I/O, or panic.
    pub fn set_observer(&self, observer: AccountObserver) {
        *self
            .observer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(observer);
    }

    /// Caller holds transaction(), before any runtime producer is published.
    /// Capture only Weak proof-owner state; replacing a live hook is unsupported.
    pub(crate) fn set_pre_mutation_hook_unlocked(&self, hook: PreMutationHook) {
        *self
            .pre_mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(hook);
    }

    fn before_mutation(&self, mutation: BeforeAccountMutation<'_>) {
        // Bound identifiers without moving any native error/return boundary.
        let id = match mutation {
            BeforeAccountMutation::Metadata { id, .. }
            | BeforeAccountMutation::HealthExpiry { id }
            | BeforeAccountMutation::Settings { id }
            | BeforeAccountMutation::FullSave { id }
            | BeforeAccountMutation::CredentialSave { id, .. }
            | BeforeAccountMutation::Retire { id } => Some(id),
            BeforeAccountMutation::Stop => None,
        };
        if id.is_some_and(|id| self.path(id).is_err()) {
            return;
        }
        let hook = self
            .pre_mutation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        // The registration lock is released before invoking a leaf callback.
        if let Some(hook) = hook {
            hook(mutation);
        }
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

    // Caller holds transaction(); adoption validates before any expiry mutation.
    pub(crate) fn read_account_unlocked(&self, id: &str) -> Result<Account, StoreError> {
        Ok(self.read(id)?.account)
    }

    /// Caller holds transaction() through exact captured-hold validation, this
    /// native expiry, and fresh eligibility checks. This never acquires the gate.
    /// Unlike ordinary load, stopped writes cannot return an unexpired account
    /// as a successful expiry inspection. No result here grants resume authority.
    pub(crate) fn expire_health_unlocked(&self, id: &str) -> Result<Account, StoreError> {
        if self.writes_stopped() {
            return Err(StoreError::LeaseUnavailable);
        }
        // Reuse the native id-only HealthExpiry hook, generation fence, durable
        // metadata write and post-observer. Any failed persistence returns Err.
        self.load_unlocked(id)
    }

    fn load_unlocked(&self, id: &str) -> Result<Account, StoreError> {
        let mut record = self.read(id)?;
        if !self.writes_stopped() && record.account.expire_health(chrono::Utc::now()) {
            self.before_mutation(BeforeAccountMutation::HealthExpiry { id });
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

    /// Compare the failed access token under the metadata transaction. Metadata
    /// generations may advance while a refresh is in flight without rotating it.
    pub fn update_metadata_for_token(
        &self,
        id: &str,
        failed: &str,
        update: impl FnOnce(&mut Account),
    ) -> Result<Account, StoreError> {
        let _guard = self.transaction()?;
        self.before_mutation(BeforeAccountMutation::Metadata {
            id,
            cause: MetadataMutation::Ordinary,
        });
        // Expire even when a stale token suppresses the requested transition.
        self.load_unlocked(id)?;
        let record = self.read(id)?;
        let tokens = crypto::decrypt(&record.secret_dpapi_b64)?;
        if tokens.access_token != failed {
            return Ok(record.account);
        }
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
        self.before_mutation(BeforeAccountMutation::Settings { id });
        self.read(id)?;
        if drain.is_some() || allow_credits.is_some() {
            self.write_settings_unlocked(id, drain, allow_credits)?;
        }
        self.update_metadata_unlocked(id, update)
    }

    pub(crate) fn update_metadata_unlocked(
        &self,
        id: &str,
        update: impl FnOnce(&mut Account),
    ) -> Result<Account, StoreError> {
        self.update_metadata_with_cause_unlocked(
            id,
            MetadataCause::Mutation(MetadataMutation::Ordinary),
            update,
        )
    }

    /// Caller holds transaction() through classification, persistence and mint.
    pub(crate) fn update_metadata_with_cause_unlocked(
        &self,
        id: &str,
        cause: MetadataCause<'_>,
        update: impl FnOnce(&mut Account),
    ) -> Result<Account, StoreError> {
        match cause {
            MetadataCause::Mutation(cause) => {
                self.before_mutation(BeforeAccountMutation::Metadata { id, cause });
            }
            MetadataCause::UsageReport(report) => {
                if self.path(id).is_ok() {
                    report.before_store_mutation(id);
                }
            }
        }
        let mut record = self.read(id)?;
        if self.writes_stopped() {
            return Ok(record.account);
        }
        let current = record.account.clone();
        if record.account.expire_health(chrono::Utc::now()) {
            self.before_mutation(BeforeAccountMutation::HealthExpiry { id });
        }
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
        if record.account == current {
            return Ok(current);
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
        if record.takeover_receipt.as_ref().is_some_and(|r| r.fenced()) {
            return Err(StoreError::InvalidData);
        }
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

    /// Replace existing credentials under an exact generation fence. No identity,
    /// source, settings or health edits are accepted, except the existing explicit
    /// StoreError -> Ready recovery used after a successfully persisted rotation.
    /// A prehook is an attempt notification only, including on rejection/failure.
    pub fn save_credentials(
        &self,
        account: &Account,
        tokens: &Tokens,
        expected_generation: u64,
    ) -> Result<Account, StoreError> {
        let _guard = self.transaction()?;
        self.save_record_with_credentials_unlocked(
            account,
            tokens,
            Some(expected_generation),
            None,
            Some(expected_generation),
        )
    }

    pub(crate) fn save_unlocked(
        &self,
        account: &Account,
        tokens: &Tokens,
        expected: Option<u64>,
    ) -> Result<Account, StoreError> {
        self.save_record_unlocked(account, tokens, expected, None)
    }

    /// Only OAuth completion calls this, after same-user matching under the
    /// account transaction. The atomic receipt is the old operation's tombstone
    /// even if the following journal compaction cannot be written immediately.
    pub(super) fn save_login_unlocked(
        &self,
        account: &Account,
        tokens: &Tokens,
        expected: Option<u64>,
    ) -> Result<Account, StoreError> {
        use super::takeover::{Receipt, ReceiptState};
        self.before_mutation(BeforeAccountMutation::FullSave { id: &account.id });
        let previous = match self.read(&account.id) {
            Ok(record) => record.takeover_receipt,
            Err(StoreError::NotFound) => None,
            Err(error) => return Err(error),
        };
        if let Some(before) = previous.filter(|r| r.fenced()) {
            if account.source != super::Source::Oauth || account.token_meta.refresh_uncertain {
                return Err(StoreError::InvalidData);
            }
            self.reconcile_retirement_unlocked(&before)?;
            let after = Receipt {
                operation: before.operation.clone(),
                state: ReceiptState::Superseded,
            };
            let saved = self.save_record_unlocked(
                account,
                tokens,
                expected,
                Some((Some(&before), &after)),
            )?;
            // Account+new family+Superseded are already durable. Startup and old
            // commit replay finish this same journal without source/token use.
            let _ = self.complete_takeover_journal_unlocked(&saved.id, &after);
            Ok(saved)
        } else {
            self.save_unlocked(account, tokens, expected)
        }
    }

    /// Identity evidence for fresh OAuth matching only. Return no credentials
    /// and keep all general token-read fences intact, including for legacy IDs.
    pub(super) fn login_codex_user_unlocked(
        &self,
        account: &Account,
        workspace: &str,
    ) -> Result<Option<String>, StoreError> {
        let record = self.read(&account.id)?;
        Self::check_generation(Some(account.generation), Some(record.account.generation))?;
        if matches!(
            record.account.source,
            super::Source::Cpa {
                mode: super::ImportMode::Linked,
                ..
            }
        ) {
            return Err(StoreError::InvalidData);
        }
        let tokens = crypto::decrypt(&record.secret_dpapi_b64)?;
        Ok(super::import::codex_user_id(&tokens, workspace))
    }

    pub(crate) fn takeover_intent_reusable(
        &self,
        journal: &super::takeover::Journal,
    ) -> Result<bool, StoreError> {
        let _guard = self.transaction()?;
        if journal.result.is_some() || journal.retired {
            return Ok(true);
        }
        match self.read(&journal.account.id) {
            Ok(record) => Ok(record
                .takeover_receipt
                .as_ref()
                .is_some_and(|r| r.operation == journal.operation)
                || journal.expected == Some(record.account.generation)),
            Err(StoreError::NotFound) => Ok(journal.expected.is_none()),
            Err(error) => Err(error),
        }
    }

    fn read_takeover_unlocked(
        &self,
        operation: &str,
    ) -> Result<super::takeover::Journal, StoreError> {
        let path = self.takeover_journal_path(operation)?;
        let bytes = self
            .read_takeover_bytes(&path)?
            .ok_or(StoreError::InvalidData)?;
        let journal: super::takeover::Journal =
            serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
        journal.validate()?;
        if journal.operation != operation {
            return Err(StoreError::InvalidData);
        }
        Ok(journal)
    }

    fn reconcile_retirement_unlocked(
        &self,
        receipt: &super::takeover::Receipt,
    ) -> Result<(), StoreError> {
        let mut journal = self.read_takeover_unlocked(&receipt.operation)?;
        if journal.retirement_started && !journal.retired && !journal.keep_source {
            let retired =
                super::takeover_native::retirement_evidence(&journal.source, &journal.destination)
                    .unwrap_or(false);
            if retired {
                journal.retired = true;
                // New OAuth is independent evidence. Failure to refine the old
                // journal must not prevent its atomic supersession tombstone.
                let _ = self.write_takeover_journal_unlocked(&journal);
            }
        }
        Ok(())
    }

    /// Synchronous file-only boundary shared with fresh-login supersession.
    /// Caller holds the async account gate; no HTTP/profile work occurs here.
    pub(crate) fn retire_takeover(
        &self,
        supplied: &super::takeover::Journal,
        deps: &super::takeover::Dependencies,
    ) -> Result<super::takeover::Journal, super::takeover::TakeoverError> {
        use super::takeover::{Receipt, ReceiptState, TakeoverError};
        let _guard = self.transaction()?;
        self.before_mutation(BeforeAccountMutation::Retire {
            id: &supplied.account.id,
        });
        if self.writes_stopped() {
            return Err(TakeoverError::Unavailable);
        }
        let mut journal = self.read_takeover_unlocked(&supplied.operation)?;
        self.before_mutation(BeforeAccountMutation::Retire {
            id: &journal.account.id,
        });
        let record = self.read(&journal.account.id)?;
        let receipt = record
            .takeover_receipt
            .as_ref()
            .ok_or(TakeoverError::Invalid)?;
        if receipt.operation != journal.operation {
            return Err(TakeoverError::Conflict);
        }
        if !receipt.fenced() {
            self.complete_takeover_journal_unlocked(&journal.account.id, receipt)?;
            return Ok(self.read_takeover_unlocked(&journal.operation)?);
        }
        if !journal.retired {
            if receipt.state != ReceiptState::Installed {
                return Err(TakeoverError::Invalid);
            }
            super::takeover::validate_source(deps, journal.provider, &journal.source)?;
            journal.retirement_started = true;
            self.write_takeover_journal_unlocked(&journal)?;
            deps.source.retire(
                journal.provider,
                &journal.source,
                &journal.destination,
                journal.keep_source,
            )?;
            journal.retired = true;
            self.write_takeover_journal_unlocked(&journal)?;
        }
        if receipt.state == ReceiptState::Installed {
            let tokens = crypto::decrypt(&record.secret_dpapi_b64)?;
            let ready = Receipt {
                operation: journal.operation.clone(),
                state: ReceiptState::Ready,
            };
            self.save_record_unlocked(
                &record.account,
                &tokens,
                Some(record.account.generation),
                Some((Some(receipt), &ready)),
            )?;
        }
        Ok(journal)
    }

    fn save_record_unlocked(
        &self,
        account: &Account,
        tokens: &Tokens,
        expected: Option<u64>,
        receipt_change: Option<(Option<&super::takeover::Receipt>, &super::takeover::Receipt)>,
    ) -> Result<Account, StoreError> {
        self.save_record_with_credentials_unlocked(account, tokens, expected, receipt_change, None)
    }

    fn save_record_with_credentials_unlocked(
        &self,
        account: &Account,
        tokens: &Tokens,
        expected: Option<u64>,
        receipt_change: Option<(Option<&super::takeover::Receipt>, &super::takeover::Receipt)>,
        credential_generation: Option<u64>,
    ) -> Result<Account, StoreError> {
        match credential_generation {
            Some(expected_generation) => {
                self.before_mutation(BeforeAccountMutation::CredentialSave {
                    id: &account.id,
                    expected_generation,
                    refresh_uncertain: account.token_meta.refresh_uncertain,
                })
            }
            None => self.before_mutation(BeforeAccountMutation::FullSave { id: &account.id }),
        }
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
        // A response to this exact fenced write-ahead operation is independent
        // proof of rotation. Native Claude has no prior refresh timestamp and
        // a legitimate refreshed expiry may be shorter than its imported one.
        let takeover_rotation = receipt_change.is_some_and(|(before, next)| {
            before.is_some_and(|before| before.operation == next.operation
                && before.state == super::takeover::ReceiptState::Started)
                && matches!(&next.state, super::takeover::ReceiptState::Finished { result } if result.ok)
        });
        let fresh_login = receipt_change.is_some_and(|(before, next)| {
            before.is_some_and(|r| r.fenced() && r.operation == next.operation)
                && next.state == super::takeover::ReceiptState::Superseded
                && account.source == super::Source::Oauth
                && !account.token_meta.refresh_uncertain
        });
        if !takeover_rotation
            && !fresh_login
            && old.as_ref().is_some_and(|old| {
                account
                    .token_meta
                    .compare(&old.account.token_meta)
                    .is_some_and(|order| order.is_lt())
            })
        {
            return Err(StoreError::OlderToken);
        }
        if expected.is_some_and(|n| n != account.generation) {
            return Err(StoreError::InvalidData);
        }
        if credential_generation.is_some()
            && !old
                .as_ref()
                .is_some_and(|old| Self::credential_metadata_matches(&old.account, account))
        {
            return Err(StoreError::InvalidData);
        }
        let previous_receipt = old.as_ref().and_then(|r| r.takeover_receipt.as_ref());
        let receipt = if let Some((expected_receipt, next)) = receipt_change {
            next.validate()?;
            if previous_receipt != expected_receipt
                || previous_receipt.is_some_and(|old| !old.permits(next))
                || (previous_receipt.is_none()
                    && next.state != super::takeover::ReceiptState::Installed)
            {
                return Err(StoreError::InvalidData);
            }
            if next.state == super::takeover::ReceiptState::Started
                && !account.token_meta.refresh_uncertain
            {
                return Err(StoreError::InvalidData);
            }
            Some(next.clone())
        } else {
            if previous_receipt.is_some_and(|r| r.fenced()) {
                return Err(StoreError::InvalidData);
            }
            previous_receipt.cloned()
        };
        let mut updated = account.clone();
        let adopting = old.as_ref().is_some_and(|old| {
            matches!(
                &old.account.source,
                super::Source::Cpa {
                    mode: super::ImportMode::Linked,
                    ..
                }
            )
        }) && matches!(
            &account.source,
            super::Source::Cpa {
                mode: super::ImportMode::Adopted,
                ..
            }
        );
        if !adopting
            && old
                .as_ref()
                .is_some_and(|old| account.token_meta.newer_than(&old.account.token_meta))
        {
            updated.apply_health_signal(super::HealthSignal::NewerLinkedToken, chrono::Utc::now());
        }
        updated.generation = expected
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(StoreError::InvalidData)?;
        // NewerLinkedToken can itself normalize health (including auth/payment
        // expiry). Such a save must use the ordinary FullSave path, not acquire
        // a credential-only classification by hiding the normalized post-state.
        if credential_generation.is_some()
            && !old
                .as_ref()
                .is_some_and(|old| Self::credential_metadata_matches(&old.account, &updated))
        {
            return Err(StoreError::CredentialHealthChange);
        }
        let record = Record {
            account: updated.clone(),
            secret_dpapi_b64: crypto::encrypt(tokens)?,
            takeover_receipt: receipt,
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

    fn credential_metadata_matches(before: &Account, after: &Account) -> bool {
        let mut permitted = before.clone();
        permitted.token_meta = after.token_meta.clone();
        // CAS validation checks the requested generation; persistence advances it.
        permitted.generation = after.generation;
        if matches!(
            before.health,
            Health::Cooling {
                reason: super::HealthReason::StoreError,
                ..
            }
        ) && after.health == Health::Ready
            && !after.token_meta.refresh_uncertain
            && after.token_meta.newer_than(&before.token_meta)
        {
            permitted.health = Health::Ready;
        }
        // Compare the complete metadata: newly added settings default to denied.
        permitted == *after
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

    pub(crate) fn remove_login_record_unlocked(&self, id: &str) -> Result<(), StoreError> {
        self.before_mutation(BeforeAccountMutation::Retire { id });
        if self.writes_stopped() {
            return Err(StoreError::LeaseUnavailable);
        }
        if let Some(receipt) = self.read(id)?.takeover_receipt {
            if receipt.fenced() {
                return Err(StoreError::InvalidData);
            }
            self.complete_takeover_journal_unlocked(id, &receipt)?;
        }
        fs::remove_file(self.path(id)?).map_err(|_| StoreError::Io)
    }

    pub(crate) fn begin_login_migration(
        &self,
        old: &str,
        new: &str,
        resolved_identity: &super::Identity,
    ) -> Result<(), StoreError> {
        // Revoke both even if preparation fails before either save/remove.
        for id in [old, new] {
            self.before_mutation(BeforeAccountMutation::Retire { id });
        }
        self.check_login_migration_clear()?;
        for id in [old, new] {
            match self.read(id) {
                Ok(record) if record.takeover_receipt.as_ref().is_some_and(|r| r.fenced()) => {
                    return Err(StoreError::InvalidData);
                }
                Ok(_) | Err(StoreError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        self.path(old)?;
        self.path(new)?;
        let old_identity = self.read(old)?.account.identity;
        self.atomic_write(
            &self.dir.join(".login-migration"),
            serde_json::json!({"old":old,"new":new,"proof":{
                "old_identity":old_identity,"resolved_identity":resolved_identity
            }})
            .to_string()
            .as_bytes(),
        )
    }
    pub(crate) fn check_login_migration_clear(&self) -> Result<(), StoreError> {
        match fs::symlink_metadata(self.dir.join(".login-migration")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(StoreError::InvalidData),
            Err(_) => Err(StoreError::Io),
        }
    }
    pub(crate) fn finish_login_migration(&self) -> Result<(), StoreError> {
        if self.writes_stopped() {
            return Err(StoreError::LeaseUnavailable);
        }
        fs::remove_file(self.dir.join(".login-migration")).map_err(|_| StoreError::Io)
    }
    fn recover_login_migration(&self) -> Result<(), StoreError> {
        let bytes = match fs::read(self.dir.join(".login-migration")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(StoreError::Io),
        };
        #[derive(Deserialize)]
        struct Proof {
            old_identity: super::Identity,
            resolved_identity: super::Identity,
        }
        #[derive(Deserialize)]
        struct Migration {
            old: String,
            new: String,
            #[serde(default)]
            proof: Option<Proof>,
        }
        let migration: Migration =
            serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
        if migration.old == migration.new {
            return Err(StoreError::InvalidData);
        }
        self.path(&migration.old)?;
        self.path(&migration.new)?;
        match self.read(&migration.new) {
            Ok(new) if matches!(new.account.source, super::Source::Oauth) => {
                // A metadata-valid replacement must also contain usable credentials
                // before recovery removes the only old token record.
                let _tokens = crypto::decrypt(&new.secret_dpapi_b64)?;
                match self.read(&migration.old) {
                    Ok(mut old) => {
                        // The transaction authorized this replacement before journaling.
                        // Recovery uses immutable identity evidence, never live CPA state.
                        let authorized = if let Some(proof) = &migration.proof {
                            proof.old_identity == old.account.identity
                                && (super::login::matches(
                                    &proof.old_identity,
                                    &proof.resolved_identity,
                                ) || matches!(
                                    (&proof.old_identity, &proof.resolved_identity),
                                    (super::Identity::Codex { account_id: a, user_id: None, .. },
                                     super::Identity::Codex { account_id: b, user_id: Some(_), .. }) if a == b
                                ))
                                && super::login::matches(
                                    &proof.resolved_identity,
                                    &new.account.identity,
                                )
                        } else {
                            super::login::matches(&old.account.identity, &new.account.identity)
                        };
                        if authorized {
                            self.remove_login_record_unlocked(&migration.old)?;
                        } else if migration.proof.is_none()
                            && matches!(
                                old.account.source,
                                super::Source::Cpa {
                                    mode: super::ImportMode::Linked,
                                    ..
                                }
                            )
                            && matches!(
                                (&old.account.identity, &new.account.identity),
                                (super::Identity::Codex { account_id: a, user_id: None, .. },
                                 super::Identity::Codex { account_id: b, user_id: Some(_), .. }) if a == b
                            )
                        {
                            // Old journals lack the proof needed to delete either family.
                            // Preserve both and stop placing requests on the legacy record.
                            old.account.user_state = super::UserState::Disabled;
                            let generation = old.account.generation;
                            self.write_metadata_record(old, generation)?;
                            tracing::warn!(
                                code = "login_migration_legacy_retained",
                                "사용자 근거가 없는 이전 로그인 복구 기록: 두 계정을 보존하고 기존 연결 계정을 비활성화했습니다."
                            );
                        } else {
                            return Err(StoreError::IdentityCollision);
                        }
                    }
                    Err(StoreError::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(StoreError::NotFound) => {}
            _ => return Err(StoreError::InvalidData),
        }
        self.finish_login_migration()?;
        tracing::warn!(
            code = "login_migration_recovered",
            "중단된 로그인 변경을 저장된 복구 기록으로 복구했습니다."
        );
        Ok(())
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
        if let Some(receipt) = &record.takeover_receipt {
            receipt.validate()?;
            if receipt.state == super::takeover::ReceiptState::Started
                && !record.account.token_meta.refresh_uncertain
            {
                return Err(StoreError::InvalidData);
            }
        }
        Ok(record)
    }

    pub(crate) fn finalize_superseded_takeover(
        &self,
        id: &str,
        operation: &str,
    ) -> Result<super::takeover::Journal, StoreError> {
        let _guard = self.transaction()?;
        let receipt = self
            .read(id)?
            .takeover_receipt
            .ok_or(StoreError::InvalidData)?;
        if receipt.operation != operation
            || receipt.state != super::takeover::ReceiptState::Superseded
        {
            return Err(StoreError::InvalidData);
        }
        self.complete_takeover_journal_unlocked(id, &receipt)?;
        self.read_takeover_unlocked(operation)
    }

    fn complete_takeover_journal_unlocked(
        &self,
        id: &str,
        receipt: &super::takeover::Receipt,
    ) -> Result<(), StoreError> {
        use super::takeover::{Journal, ReceiptState};
        let (result, superseded) = match &receipt.state {
            ReceiptState::Finished { result } => (result.clone(), false),
            ReceiptState::Superseded => (
                super::takeover::Verification {
                    ok: false,
                    token_expires_at: None,
                },
                true,
            ),
            _ => return Err(StoreError::InvalidData),
        };
        let path = self.takeover_journal_path(&receipt.operation)?;
        let bytes = self
            .read_takeover_bytes(&path)?
            .ok_or(StoreError::InvalidData)?;
        let mut journal: Journal =
            serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
        journal.validate()?;
        let refined = superseded
            && journal.retirement_started
            && !journal.retired
            && !journal.keep_source
            && super::takeover_native::retirement_evidence(&journal.source, &journal.destination)
                .unwrap_or(false);
        if refined {
            journal.retired = true;
        }
        if journal.operation != receipt.operation
            || journal.account.id != id
            || (!journal.retired && !superseded)
        {
            return Err(StoreError::InvalidData);
        }
        if let Some(saved) = &journal.result {
            return if *saved == result && journal.superseded == superseded {
                if refined {
                    self.write_takeover_journal_unlocked(&journal)
                } else {
                    Ok(())
                }
            } else {
                Err(StoreError::InvalidData)
            };
        }
        journal.result = Some(result);
        journal.superseded = superseded;
        journal.protected_tokens = None;
        self.write_takeover_journal_unlocked(&journal)
    }

    fn recover_takeovers(&self) -> Result<(), StoreError> {
        let journals = self.takeover_journals()?;
        let _guard = self.transaction()?;
        for account in Self::read_metadata_dir(&self.dir)? {
            let record = self.read(&account.id)?;
            let Some(receipt) = record.takeover_receipt else {
                continue;
            };
            let journal = journals
                .iter()
                .find(|j| j.operation == receipt.operation)
                .ok_or(StoreError::InvalidData)?;
            if journal.account.id != account.id
                || journal.account.identity != account.identity
                || (!matches!(
                    receipt.state,
                    super::takeover::ReceiptState::Installed
                        | super::takeover::ReceiptState::Superseded
                ) && !journal.retired)
                || (journal.result.is_some() && receipt.fenced())
            {
                return Err(StoreError::InvalidData);
            }
            if !receipt.fenced() {
                self.complete_takeover_journal_unlocked(&account.id, &receipt)?;
            }
        }
        // A published but incomplete operation cannot lose its marker. Prepared
        // intents without a marker have not consumed tokens; the original store
        // generation must still pass CAS if explicitly resumed later.
        for journal in journals {
            if journal.retired && journal.result.is_none() {
                let receipt = self
                    .read(&journal.account.id)?
                    .takeover_receipt
                    .ok_or(StoreError::InvalidData)?;
                if receipt.operation != journal.operation {
                    return Err(StoreError::InvalidData);
                }
            }
        }
        Ok(())
    }

    fn atomic_write(&self, destination: &Path, bytes: &[u8]) -> Result<(), StoreError> {
        if self.writes_stopped() {
            return Err(StoreError::LeaseUnavailable);
        }
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

// The existing isolated m1b harness compiles credentials.rs as source. Keep
// these internal in production and expose only to explicit testkit builds.
macro_rules! takeover_store_api {
    ($visibility:vis) => {
        #[doc(hidden)]
        impl AccountStore {
    $visibility fn takeover_receipt(&self, id: &str) -> Result<Option<super::takeover::Receipt>, StoreError> {
        let _guard = self.transaction()?;
        Ok(self.read(id)?.takeover_receipt)
    }

    $visibility fn takeover_fenced(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.takeover_receipt(id)?.is_some_and(|r| r.fenced()))
    }
    $visibility fn takeover_fenced_ids(&self) -> Result<std::collections::BTreeSet<String>, StoreError> {
        let _guard = self.transaction()?;
        let mut ids = std::collections::BTreeSet::new();
        for account in Self::read_metadata_dir(&self.dir)? {
            if self.read(&account.id)?.takeover_receipt.as_ref().is_some_and(|r| r.fenced()) { ids.insert(account.id); }
        }
        Ok(ids)
    }

    $visibility fn takeover_tokens(&self, account: &Account, operation: &str) -> Result<Tokens, StoreError> {
        let _guard = self.transaction()?;
        let record = self.read(&account.id)?;
        Self::check_generation(Some(account.generation), Some(record.account.generation))?;
        if record.takeover_receipt.as_ref().is_none_or(|r| r.operation != operation || r.state == super::takeover::ReceiptState::Superseded) { return Err(StoreError::InvalidData); }
        crypto::decrypt(&record.secret_dpapi_b64)
    }

    $visibility fn save_takeover_receipt(
        &self, account: &Account, tokens: &Tokens, expected: Option<u64>,
        before: Option<&super::takeover::Receipt>, after: &super::takeover::Receipt,
    ) -> Result<Account, StoreError> {
        let _guard = self.transaction()?;
        self.save_record_unlocked(account, tokens, expected, Some((before, after)))
    }

    $visibility fn publish_takeover(&self, journal: &super::takeover::Journal) -> Result<Account, StoreError> {
        use super::takeover::{Receipt, ReceiptState};
        let _guard = self.transaction()?;
        self.before_mutation(BeforeAccountMutation::FullSave { id: &journal.account.id });
        journal.validate()?;
        let old = match self.read(&journal.account.id) {
            Ok(record) => Some(record), Err(StoreError::NotFound) => None, Err(e) => return Err(e),
        };
        Self::check_generation(journal.expected, old.as_ref().map(|r| r.account.generation))?;
        Self::validate(&journal.account)?;
        if old.as_ref().is_some_and(|r| r.account.identity.canonical_key() != journal.account.identity.canonical_key()) {
            return Err(StoreError::IdentityCollision);
        }
        if old.as_ref().is_some_and(|r| journal.account.token_meta.compare(&r.account.token_meta).is_none_or(|o| o.is_lt())) {
            return Err(StoreError::OlderToken);
        }
        for other in Self::read_metadata_dir(&self.dir)? {
            if other.id == journal.account.id { continue; }
            let ambiguous = match (&other.identity, &journal.account.identity) {
                (super::Identity::Codex { account_id:a,user_id:x,.. }, super::Identity::Codex { account_id:b,user_id:y,.. }) =>
                    a == b && (x.is_none() || y.is_none() || x == y),
                (super::Identity::Claude { account_uuid:a,organization_uuid:x,.. }, super::Identity::Claude { account_uuid:b,organization_uuid:y,.. }) =>
                    a == b && (x.is_none() || y.is_none() || x == y),
                _ => false,
            };
            if ambiguous { return Err(StoreError::IdentityCollision); }
        }
        let before = old.as_ref().and_then(|r| r.takeover_receipt.as_ref());
        if before.is_some_and(|r| r.fenced()) { return Err(StoreError::InvalidData); }
        if let Some(before) = before {
            // Preserve the previous operation's dedupe proof before replacing
            // its account receipt. A failed tombstone write blocks publication.
            self.complete_takeover_journal_unlocked(&journal.account.id, before)?;
        }
        let tokens = crypto::decrypt(journal.protected_tokens.as_deref().ok_or(StoreError::InvalidData)?)?;
        // Keep identity/store CAS and intent publication in one short transaction.
        // The existing account gate excludes competing token-consuming work.
        self.write_takeover_journal_unlocked(journal)?;
        let after = Receipt { operation:journal.operation.clone(), state:ReceiptState::Installed };
        self.save_record_unlocked(&journal.account, &tokens, journal.expected, Some((before, &after)))
    }

    /// Caller holds the existing per-account token gate; store lock is short.
    $visibility fn finish_takeover_attempt(
        &self, id: &str, operation: &str, result: super::takeover::Verification,
        terminal: Option<String>,
    ) -> Result<Account, StoreError> {
        use super::takeover::{Receipt, ReceiptState};
        let _guard = self.transaction()?;
        self.before_mutation(BeforeAccountMutation::FullSave { id });
        let record = self.read(id)?;
        let before = record.takeover_receipt.as_ref().ok_or(StoreError::InvalidData)?;
        if before.operation != operation || before.state != ReceiptState::Started {
            return Err(StoreError::InvalidData);
        }
        let mut account = record.account;
        let tokens = crypto::decrypt(&record.secret_dpapi_b64)?;
        account.token_meta.refresh_uncertain = false;
        if let Some(reason) = terminal {
            account.apply_health_signal(super::HealthSignal::TerminalRefresh { reason }, chrono::Utc::now());
        }
        let next = Receipt { operation: operation.to_owned(), state: ReceiptState::Finished { result } };
        self.save_record_unlocked(&account, &tokens, Some(account.generation), Some((Some(before), &next)))
    }

    /// Journal extension is deliberately not .json: account enumeration ignores it.
    fn takeover_journal_path(&self, operation: &str) -> Result<PathBuf, StoreError> {
        super::takeover::Receipt { operation: operation.to_owned(), state: super::takeover::ReceiptState::Installed }.validate()?;
        Ok(self.dir.join(format!(".takeover-{operation}.journal")))
    }
    $visibility fn read_takeover_journal(&self, operation: &str) -> Result<Option<super::takeover::Journal>, StoreError> {
        let _guard = self.transaction()?;
        let path = self.takeover_journal_path(operation)?;
        let Some(bytes) = self.read_takeover_bytes(&path)? else { return Ok(None); };
        let journal: super::takeover::Journal = serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
        if journal.operation != operation || journal.schema != 1 { return Err(StoreError::InvalidData); }
        journal.validate()?;
        Ok(Some(journal))
    }
    $visibility fn write_takeover_journal(&self, journal: &super::takeover::Journal) -> Result<(), StoreError> {
        let _guard = self.transaction()?;
        self.write_takeover_journal_unlocked(journal)
    }
    fn write_takeover_journal_unlocked(&self, journal: &super::takeover::Journal) -> Result<(), StoreError> {
        let path = self.takeover_journal_path(&journal.operation)?;
        journal.validate()?;
        if let Some(bytes) = self.read_takeover_bytes(&path)? {
            let old: super::takeover::Journal = serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
            old.validate()?;
            let mut old_account = old.account.clone();
            old_account.generation = journal.account.generation;
            if old.inspect_digest != journal.inspect_digest || old.source != journal.source
                || old.account.id != journal.account.id || old.provider != journal.provider
                || old.operation != journal.operation || old.expected != journal.expected
                || old_account != journal.account
                || old.replace != journal.replace || old.keep_source != journal.keep_source
                || old.destination != journal.destination || old.requested_path != journal.requested_path
                || (old.retired && !journal.retired) || (old.result.is_some() && old.result != journal.result)
                || (old.retirement_started && !journal.retirement_started)
                || (old.superseded && !journal.superseded)
                || (journal.protected_tokens.is_some() && old.protected_tokens != journal.protected_tokens)
            { return Err(StoreError::InvalidData); }
        }
        let bytes = serde_json::to_vec_pretty(journal).map_err(|_| StoreError::InvalidData)?;
        self.atomic_write(&path, &bytes)
    }

    $visibility fn takeover_journals(&self) -> Result<Vec<super::takeover::Journal>, StoreError> {
        let _guard = self.transaction()?;
        let mut journals = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|_| StoreError::Io)? {
            let entry = entry.map_err(|_| StoreError::Io)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue; };
            if !name.starts_with(".takeover-") || !name.ends_with(".journal") { continue; }
            if !entry.file_type().map_err(|_| StoreError::Io)?.is_file() { return Err(StoreError::InvalidData); }
            let bytes = self.read_takeover_bytes(&entry.path())?.ok_or(StoreError::Io)?;
            let journal: super::takeover::Journal = serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
            journal.validate()?;
            if self.takeover_journal_path(&journal.operation)? != entry.path() { return Err(StoreError::InvalidData); }
            journals.push(journal);
        }
        Ok(journals)
    }

    fn read_takeover_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
        use std::{io::Read, os::windows::fs::{MetadataExt, OpenOptionsExt}};
        let file = match OpenOptions::new().read(true).custom_flags(0x00200000).open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(StoreError::Io),
        };
        let meta = file.metadata().map_err(|_| StoreError::Io)?;
        if !meta.is_file() || meta.file_attributes() & 0x410 != 0 || meta.len() > 4 * 1024 * 1024 {
            return Err(StoreError::InvalidData);
        }
        let mut bytes = Vec::new();
        file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| StoreError::Io)?;
        if bytes.len() > 4 * 1024 * 1024 { return Err(StoreError::InvalidData); }
        Ok(Some(bytes))
    }

    $visibility fn protect_takeover_tokens(tokens: &Tokens) -> Result<String, StoreError> { crypto::encrypt(tokens) }
        }
    };
}
#[cfg(feature = "testkit")]
takeover_store_api!(pub);
#[cfg(not(feature = "testkit"))]
takeover_store_api!(pub(crate));

#[cfg(test)]
mod tests {
    use super::super::{HealthReason, HealthSignal};
    use super::*;
    use chrono::{Duration, Utc};

    // No credentials or native secret operations are needed for these failed
    // store transitions. Runtime authority/persistence tests remain integration work.
    fn n5a_hook_store() -> (tempfile::TempDir, AccountStore, Arc<Mutex<Vec<String>>>) {
        let sandbox =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/n5a-store-fixtures");
        fs::create_dir_all(&sandbox).unwrap();
        let root = tempfile::tempdir_in(sandbox).unwrap();
        let store = AccountStore {
            dir: root.path().join("accounts"),
            gate: Mutex::new(()),
            observer: Mutex::new(None),
            pre_mutation: Mutex::new(None),
            lease: Mutex::new(None),
            writes_stopped: AtomicBool::new(false),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&events);
        {
            let _gate = store.transaction().unwrap();
            store.set_pre_mutation_hook_unlocked(Arc::new(move |event| {
                output.lock().unwrap().push(format!("{event:?}"));
            }));
        }
        (root, store, events)
    }

    #[test]
    fn n5d_credential_attempt_revokes_before_failed_read_and_represents_writeahead() {
        let (_root, store, events) = n5a_hook_store();
        let account = super::super::health::tests::fixture();
        let tokens = Tokens {
            access_token: "synthetic-credential-access".into(),
            refresh_token: "synthetic-credential-refresh".into(),
            id_token: None,
        };
        // The missing record must not skip the typed pre-mutation notification.
        for uncertain in [false, true] {
            let mut candidate = account.clone();
            candidate.token_meta.refresh_uncertain = uncertain;
            assert!(matches!(
                store.save_credentials(&candidate, &tokens, candidate.generation),
                Err(StoreError::GenerationConflict { actual: None, .. })
            ));
        }
        assert!(matches!(
            store.save(&account, &tokens, Some(account.generation)),
            Err(StoreError::GenerationConflict { actual: None, .. })
        ));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(events[0].starts_with("CredentialSave"));
        assert!(events[0].contains("refresh_uncertain: false"));
        assert!(events[1].starts_with("CredentialSave"));
        assert!(events[1].contains("refresh_uncertain: true"));
        assert!(events[1].contains(&format!("expected_generation: {}", account.generation)));
        assert!(events[2].starts_with("FullSave"));
    }

    #[test]
    fn n5d_credential_projection_does_not_allow_auth_or_payment_recovery() {
        let before = super::super::health::tests::fixture();
        let now = Utc::now();
        for reason in [
            HealthReason::Auth,
            HealthReason::Payment,
            HealthReason::Quota,
        ] {
            let mut held = before.clone();
            held.health = Health::Cooling {
                reason,
                until: now + Duration::hours(1),
                scope: "account".into(),
            };
            let mut candidate = held.clone();
            candidate.token_meta.last_refresh_at = Some(now);
            candidate.health = Health::Ready;
            assert!(!AccountStore::credential_metadata_matches(
                &held, &candidate
            ));
        }
    }

    #[test]
    fn n5a_prehook_precedes_missing_metadata_and_keeps_native_error() {
        let (_root, store, events) = n5a_hook_store();
        store.set_observer(Arc::new(|_, _| panic!("failed write must not notify")));
        let result = store.update_metadata("cx-000001", |_| panic!("missing account"));
        assert!(matches!(result, Err(StoreError::NotFound)));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].starts_with("Metadata"));
        assert!(events[0].contains("cx-000001"));
        assert!(events[0].contains("Ordinary"));
    }

    #[test]
    fn n5a_settings_and_retire_failures_revoke_before_native_return() {
        let (_root, store, events) = n5a_hook_store();
        assert!(matches!(
            store.update_settings("cx-000001", Some(true), None, |_| panic!("missing account")),
            Err(StoreError::NotFound)
        ));
        assert!(!store.home().join("config.toml").exists());
        store.stop_writes();
        {
            let _gate = store.transaction().unwrap();
            assert!(matches!(
                store.remove_login_record_unlocked("cx-000001"),
                Err(StoreError::LeaseUnavailable)
            ));
        }
        let events = events.lock().unwrap();
        assert!(events[0].starts_with("Settings"));
        assert_eq!(events[1], "Stop");
        assert!(events[2].starts_with("Retire"));
    }

    #[test]
    fn n5a_migration_revokes_both_ids_before_missing_old_failure() {
        let (_root, store, events) = n5a_hook_store();
        let account = super::super::health::tests::fixture();
        let _gate = store.transaction().unwrap();
        assert!(matches!(
            store.begin_login_migration("cx-000001", "cx-000002", &account.identity),
            Err(StoreError::NotFound)
        ));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("cx-000001"));
        assert!(events[1].contains("cx-000002"));
        assert!(events.iter().all(|event| event.starts_with("Retire")));
    }

    #[test]
    fn n5a_fullsave_revokes_before_invalid_account_without_encrypting() {
        let (_root, store, events) = n5a_hook_store();
        let mut account = super::super::health::tests::fixture();
        account.schema = 0;
        let tokens = Tokens {
            access_token: "synthetic".into(),
            refresh_token: "synthetic".into(),
            id_token: None,
        };
        assert!(matches!(
            store.save(&account, &tokens, None),
            Err(StoreError::InvalidData)
        ));
        assert_eq!(events.lock().unwrap().len(), 1);
        assert!(events.lock().unwrap()[0].starts_with("FullSave"));
    }

    #[test]
    fn n5a_report_adapter_is_per_call_and_not_a_transaction_exemption() {
        struct Report<'a>(&'a mut bool);
        impl ReportMutation for Report<'_> {
            fn before_store_mutation(&mut self, id: &str) {
                assert_eq!(id, "cx-000001");
                assert!(!*self.0);
                *self.0 = true;
            }
        }
        let (_root, store, events) = n5a_hook_store();
        let _gate = store.transaction().unwrap();
        let mut used = false;
        assert!(matches!(
            store.update_metadata_with_cause_unlocked(
                "cx-000001",
                MetadataCause::UsageReport(&mut Report(&mut used)),
                |_| panic!("missing")
            ),
            Err(StoreError::NotFound)
        ));
        assert!(used);
        assert!(events.lock().unwrap().is_empty());
        assert!(matches!(
            store.update_metadata_with_cause_unlocked(
                "cx-000001",
                MetadataCause::Mutation(MetadataMutation::Release),
                |_| panic!("missing")
            ),
            Err(StoreError::NotFound)
        ));
        assert!(events.lock().unwrap()[0].contains("Release"));
    }

    fn n5a_seed_metadata(store: &AccountStore, account: &Account) {
        // Metadata-only tests never decrypt or use this synthetic placeholder.
        fs::create_dir_all(&store.dir).unwrap();
        let record = Record {
            account: account.clone(),
            secret_dpapi_b64: "synthetic-unused-secret".into(),
            takeover_receipt: None,
        };
        fs::write(
            store.path(&account.id).unwrap(),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn n5a_noop_and_stopped_metadata_revoke_without_changing_native_result() {
        let (_root, store, events) = n5a_hook_store();
        let account = super::super::health::tests::fixture();
        n5a_seed_metadata(&store, &account);
        assert_eq!(store.update_metadata(&account.id, |_| {}).unwrap(), account);
        assert_eq!(events.lock().unwrap().len(), 1);
        store.stop_writes();
        assert_eq!(
            store
                .update_metadata(&account.id, |_| panic!("stopped closure"))
                .unwrap(),
            account
        );
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1], "Stop");
        assert!(events[2].starts_with("Metadata"));
        assert_eq!(store.read(&account.id).unwrap().account, account);
    }

    #[test]
    fn n5d_caller_held_expiry_persists_before_return_and_keeps_native_load_semantics() {
        let (_root, store, events) = n5a_hook_store();
        let mut account = super::super::health::tests::fixture();
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: Utc::now() - Duration::seconds(1),
            scope: "account".into(),
        };
        n5a_seed_metadata(&store, &account);
        let output = events.clone();
        store.set_observer(Arc::new(move |current, _| {
            let mut events = output.lock().unwrap();
            assert!(events.last().unwrap().starts_with("HealthExpiry"));
            assert_eq!(current.health, Health::Ready);
            events.push("PostObserver".into());
        }));
        {
            let _gate = store.transaction().unwrap();
            let expired = store.expire_health_unlocked(&account.id).unwrap();
            assert_eq!(expired.health, Health::Ready);
            assert_eq!(expired.generation, account.generation + 1);
            assert_eq!(store.read(&account.id).unwrap().account, expired);
            assert_eq!(events.lock().unwrap().len(), 2);
            // No expiry transition is synthesized on a subsequent inspection.
            assert_eq!(store.expire_health_unlocked(&account.id).unwrap(), expired);
            assert_eq!(events.lock().unwrap().len(), 2);
        }
        store.stop_writes();
        let current = store.load(&account.id).unwrap();
        let _gate = store.transaction().unwrap();
        assert!(matches!(
            store.expire_health_unlocked(&account.id),
            Err(StoreError::LeaseUnavailable)
        ));
        assert_eq!(store.read(&account.id).unwrap().account, current);
    }

    #[test]
    fn n5d_caller_held_expiry_failure_returns_no_account_after_revocation() {
        let (_root, store, events) = n5a_hook_store();
        let mut account = super::super::health::tests::fixture();
        account.generation = u64::MAX;
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: Utc::now() - Duration::seconds(1),
            scope: "account".into(),
        };
        n5a_seed_metadata(&store, &account);
        store.set_observer(Arc::new(|_, _| panic!("failed expiry must not publish")));
        let _gate = store.transaction().unwrap();
        assert!(matches!(
            store.expire_health_unlocked(&account.id),
            Err(StoreError::InvalidData)
        ));
        assert_eq!(store.read(&account.id).unwrap().account, account);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].starts_with("HealthExpiry"));
    }

    #[test]
    fn n5a_lazy_expiry_revokes_before_persist_and_post_observer() {
        let (_root, store, events) = n5a_hook_store();
        let mut account = super::super::health::tests::fixture();
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: Utc::now() - Duration::seconds(1),
            scope: "account".into(),
        };
        n5a_seed_metadata(&store, &account);
        let output = events.clone();
        store.set_observer(Arc::new(move |current, _| {
            let mut events = output.lock().unwrap();
            assert!(events.last().unwrap().starts_with("HealthExpiry"));
            assert_eq!(current.health, Health::Ready);
            events.push("PostObserver".into());
        }));
        let loaded = store.load(&account.id).unwrap();
        assert_eq!(loaded.health, Health::Ready);
        assert_eq!(loaded.generation, account.generation + 1);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1], "PostObserver");
    }

    #[test]
    fn n5a_settings_config_success_does_not_hide_metadata_failure() {
        let (_root, store, events) = n5a_hook_store();
        let account = super::super::health::tests::fixture();
        n5a_seed_metadata(&store, &account);
        let result = store.update_settings(&account.id, None, Some(true), |a| a.schema = 0);
        assert!(matches!(result, Err(StoreError::InvalidData)));
        let config: toml_edit::DocumentMut = fs::read_to_string(store.home().join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            config["accounts"][&account.id]["allow_credits"].as_bool(),
            Some(true)
        );
        assert_eq!(store.read(&account.id).unwrap().account, account);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].starts_with("Settings"));
        assert!(events[1].starts_with("Metadata"));
    }

    #[test]
    fn n5a_takeover_and_login_wrapper_failures_revoke_before_shared_save() {
        use super::super::takeover::{Journal, SourceProof, Verification};
        let (_root, store, events) = n5a_hook_store();
        let account = super::super::health::tests::fixture();
        n5a_seed_metadata(&store, &account);
        let journal = Journal {
            schema: 0,
            operation: "invalid-synthetic-operation".into(),
            inspect_digest: String::new(),
            provider: account.provider,
            requested_path: store.home().join("synthetic-source/auth.json"),
            source: SourceProof {
                path: store.home().join("synthetic-source/auth.json"),
                volume: 0,
                index: 0,
                modified: Utc::now(),
                sha256: String::new(),
            },
            replace: false,
            keep_source: true,
            destination: store.home().join("synthetic-destination"),
            account: account.clone(),
            expected: None,
            protected_tokens: None,
            retired: false,
            retirement_started: false,
            superseded: false,
            result: None,
        };
        assert!(matches!(
            store.publish_takeover(&journal),
            Err(StoreError::InvalidData)
        ));
        assert!(matches!(
            store.finish_takeover_attempt(
                &account.id,
                "synthetic-operation",
                Verification {
                    ok: false,
                    token_expires_at: None
                },
                None
            ),
            Err(StoreError::InvalidData)
        ));
        // A corrupt synthetic metadata row fails before login reaches shared save.
        fs::write(store.path(&account.id).unwrap(), b"{").unwrap();
        let tokens = Tokens {
            access_token: "synthetic".into(),
            refresh_token: "synthetic".into(),
            id_token: None,
        };
        let _gate = store.transaction().unwrap();
        assert!(matches!(
            store.save_login_unlocked(&account, &tokens, None),
            Err(StoreError::InvalidData)
        ));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| event.starts_with("FullSave")));
    }

    #[test]
    fn r1_metadata_and_token_guarded_transitions_expire_before_read() {
        let sandbox =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/hardening-fixtures");
        fs::create_dir_all(&sandbox).unwrap();
        let root = tempfile::tempdir_in(sandbox).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let store = loop {
            match AccountStore::acquire(root.path()) {
                Ok(store) => break store,
                Err(StoreError::LeaseBusy) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("synthetic fixture lease: {error}"),
            }
        };
        let tokens = Tokens {
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            id_token: None,
        };
        let now = Utc::now();
        for guarded in [false, true] {
            let mut account = super::super::health::tests::fixture();
            account.health = Health::Cooling {
                reason: HealthReason::Auth,
                until: now - Duration::seconds(1),
                scope: "account".into(),
            };
            account.auth_failures = 2;
            let expected = store.read(&account.id).ok().map(|r| r.account.generation);
            account.generation = expected.unwrap_or(0);
            let saved = store.save(&account, &tokens, expected).unwrap();
            let update = |account: &mut Account| {
                assert_eq!(account.health, Health::Ready);
                assert!(account.auth_probe_pending);
                account.apply_health_signal(
                    HealthSignal::Cooling {
                        reason: HealthReason::Permission,
                        until: now + Duration::hours(1),
                        scope: "model:X".into(),
                    },
                    now,
                );
            };
            let updated = if guarded {
                store
                    .update_metadata_for_token(&saved.id, &tokens.access_token, update)
                    .unwrap()
            } else {
                store.update_metadata(&saved.id, update).unwrap()
            };
            assert!(matches!(&updated.health, Health::Cooling { scope, .. } if scope == "model:X"));
            assert_eq!(updated.auth_failures, 2);
            assert_eq!(store.read(&saved.id).unwrap().account, updated);
        }
        let mut account = store
            .read(&super::super::health::tests::fixture().id)
            .unwrap()
            .account;
        account.health = Health::Cooling {
            reason: HealthReason::Auth,
            until: now - Duration::seconds(1),
            scope: "account".into(),
        };
        let saved = store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
        let updated = store
            .update_metadata_for_token(&saved.id, "synthetic-stale-token", |_| {
                panic!("stale token must suppress callback")
            })
            .unwrap();
        assert_eq!(updated.health, Health::Ready);
        assert!(updated.auth_probe_pending);
        assert_eq!(store.read(&saved.id).unwrap().account, updated);
    }
}
