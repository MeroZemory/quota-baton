//! Native-home takeover transactions. Tests inject source and profile adapters.
use super::{Account, AccountStore, Identity, Provider, StoreError, Tokens};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

pub type AsyncResult<'a, T> = Pin<Box<dyn Future<Output = Result<T, TakeoverError>> + Send + 'a>>;

/// Keep the isolated credentials-source test harness independent of oauth's
/// private module. Test builds have no production default adapter.
#[cfg(any(test, feature = "testkit"))]
pub fn synthetic_endpoint(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        })
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && !matches!(url.port_or_known_default(), Some(10100 | 10101))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Inspect,
    Commit,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TakeoverRequest {
    pub provider: Provider,
    pub path: PathBuf,
    pub phase: Phase,
    pub inspect_digest: Option<String>,
    #[serde(default)]
    pub replace: bool,
    #[serde(default)]
    pub keep_source: bool,
}

/// Codes only. Never attach source bytes, provider bodies or parser errors.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum TakeoverError {
    #[error("takeover conflict; inspect again")]
    Conflict,
    #[error("API-key takeover is unsupported")]
    ApiKey,
    #[error("takeover input or journal is invalid")]
    Invalid,
    #[error("takeover is unavailable")]
    Unavailable,
    #[error("takeover refresh result is unknown; no retry was sent")]
    Uncertain,
}
impl From<StoreError> for TakeoverError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::GenerationConflict { .. }
            | StoreError::IdentityCollision
            | StoreError::OlderToken => Self::Conflict,
            StoreError::InvalidData => Self::Invalid,
            _ => Self::Unavailable,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Receipt {
    pub operation: String,
    pub state: ReceiptState,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReceiptState {
    Installed,
    Ready,
    Started,
    Finished { result: Verification },
    Superseded,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Verification {
    pub ok: bool,
    pub token_expires_at: Option<DateTime<Utc>>,
}
impl Receipt {
    pub fn fenced(&self) -> bool {
        !matches!(
            self.state,
            ReceiptState::Finished { .. } | ReceiptState::Superseded
        )
    }
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.operation.len() != 64 || !self.operation.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::InvalidData);
        }
        Ok(())
    }
    pub fn permits(&self, next: &Self) -> bool {
        if !self.fenced() && next.state == ReceiptState::Installed {
            return self.operation != next.operation;
        }
        self.operation == next.operation
            && matches!(
                (&self.state, &next.state),
                (ReceiptState::Installed, ReceiptState::Ready)
                    | (ReceiptState::Ready, ReceiptState::Started)
                    | (ReceiptState::Started, ReceiptState::Finished { .. })
                    | (
                        ReceiptState::Installed | ReceiptState::Ready | ReceiptState::Started,
                        ReceiptState::Superseded
                    )
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SourceProof {
    pub path: PathBuf,
    pub volume: u32,
    pub index: u64,
    pub modified: DateTime<Utc>,
    pub sha256: String,
}
pub struct SourceSnapshot {
    pub proof: SourceProof,
    pub bytes: zeroize::Zeroizing<Vec<u8>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Stopped,
    Running,
    Unknown,
}

/// Production adapter must hold a non-write-shared source handle, enforce
/// reparse/type/ancestor checks, and rename that handle without replacement.
/// The trait never grants permission to follow an arbitrary changed source.
pub trait SourceAccess: Send + Sync {
    fn snapshot(
        &self,
        provider: Provider,
        path: &std::path::Path,
    ) -> Result<SourceSnapshot, TakeoverError>;
    fn owner(&self, provider: Provider, proof: &SourceProof) -> Owner;
    fn retire(
        &self,
        provider: Provider,
        proof: &SourceProof,
        destination: &std::path::Path,
        keep: bool,
    ) -> Result<(), TakeoverError>;
    fn now(&self) -> DateTime<Utc>;
}
pub struct ProfileIdentity {
    pub identity: Identity,
    pub email: String,
    pub plan: Option<String>,
}
pub trait ProfileResolver: Send + Sync {
    fn endpoint(&self) -> &str;
    /// Non-refresh GET only; primary adapter supplies shared client + helper.
    fn claude<'a>(&'a self, access: &'a str) -> AsyncResult<'a, ProfileIdentity>;
}
#[derive(Clone)]
pub struct Dependencies {
    pub source: Arc<dyn SourceAccess>,
    pub profile: Arc<dyn ProfileResolver>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Journal {
    pub schema: u8,
    pub operation: String,
    pub inspect_digest: String,
    pub provider: Provider,
    pub requested_path: PathBuf,
    pub source: SourceProof,
    pub replace: bool,
    pub keep_source: bool,
    pub destination: PathBuf,
    pub account: Account,
    pub expected: Option<u64>,
    /// Existing account DPAPI Tokens format; dropped on durable completion.
    pub protected_tokens: Option<String>,
    pub retired: bool,
    #[serde(default)]
    pub retirement_started: bool,
    #[serde(default)]
    pub superseded: bool,
    pub result: Option<Verification>,
}
impl Journal {
    pub fn validate(&self) -> Result<(), StoreError> {
        Receipt {
            operation: self.operation.clone(),
            state: ReceiptState::Installed,
        }
        .validate()?;
        let filename = if self.provider == Provider::Codex {
            "auth.json"
        } else {
            ".credentials.json"
        };
        let prefix = format!("{filename}.taken-over-by-quota-baton-");
        let suffix = self
            .destination
            .file_name()
            .and_then(|v| v.to_str())
            .and_then(|v| v.strip_prefix(&prefix));
        if self.schema != 1
            || self.inspect_digest.len() != 64
            || !self.inspect_digest.bytes().all(|b| b.is_ascii_hexdigit())
            || self.source.sha256.len() != 64
            || !self.source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.operation != digest(self.inspect_digest.as_bytes())
            || !self.source.path.is_absolute()
            || self.source.path.file_name().is_none_or(|n| n != filename)
            || self.destination.parent() != self.source.path.parent()
            || suffix.is_none_or(|s| s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()))
            || self.account.provider != self.provider
            || self.account.id != self.account.identity.id()
            || self.account.identity.provider() != self.provider
            || !matches!(&self.account.source, super::Source::Takeover {from,..} if from == &self.source.path)
            || (self.result.is_some()
                && ((!self.retired && !self.superseded) || self.protected_tokens.is_some()))
            || (self.superseded && self.result.as_ref().is_none_or(|r| r.ok))
            || (self.result.is_none() && self.protected_tokens.is_none())
        {
            return Err(StoreError::InvalidData);
        }
        Ok(())
    }
}

/// Integration entry points below operate only on the supplied dependencies.
/// Account gate/lifecycle ownership is provided by TokenGates, not HTTP tasks.
pub async fn identify(
    provider: Provider,
    bytes: &[u8],
    resolver: &dyn ProfileResolver,
) -> Result<(ProfileIdentity, Tokens, super::TokenMeta), TakeoverError> {
    use base64::Engine;
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| TakeoverError::Invalid)?;
    let string = |object: &serde_json::Value, key: &str| -> Result<String, TakeoverError> {
        object
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or(TakeoverError::Invalid)
    };
    if provider == Provider::Claude {
        let native = value.get("claudeAiOauth").ok_or(TakeoverError::ApiKey)?;
        let tokens = Tokens {
            access_token: string(native, "accessToken")?,
            refresh_token: string(native, "refreshToken")?,
            id_token: None,
        };
        let millis = native
            .get("expiresAt")
            .and_then(|v| v.as_i64())
            .ok_or(TakeoverError::Invalid)?;
        let expiry =
            DateTime::<Utc>::from_timestamp_millis(millis).ok_or(TakeoverError::Invalid)?;
        let identity = resolver.claude(&tokens.access_token).await?;
        if !matches!(identity.identity, Identity::Claude { .. }) {
            return Err(TakeoverError::Invalid);
        }
        return Ok((
            identity,
            tokens,
            super::TokenMeta {
                expires_at: Some(expiry),
                last_refresh_at: None,
                refresh_uncertain: false,
            },
        ));
    }
    if value.get("auth_mode").and_then(|v| v.as_str()) != Some("chatgpt") {
        return Err(TakeoverError::ApiKey);
    }
    let object = value.get("tokens").ok_or(TakeoverError::Invalid)?;
    let tokens = Tokens {
        access_token: string(object, "access_token")?,
        refresh_token: string(object, "refresh_token")?,
        id_token: Some(string(object, "id_token")?),
    };
    let account_id = string(object, "account_id")?;
    let user_id =
        super::import::codex_user_id(&tokens, &account_id).ok_or(TakeoverError::Invalid)?;
    let parts = tokens
        .id_token
        .as_deref()
        .unwrap()
        .split('.')
        .collect::<Vec<_>>();
    let decoded = zeroize::Zeroizing::new(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|_| TakeoverError::Invalid)?,
    );
    let claims: serde_json::Value =
        serde_json::from_slice(&decoded).map_err(|_| TakeoverError::Invalid)?;
    let email = string(&claims, "email")?;
    let profile = ProfileIdentity {
        identity: Identity::Codex {
            account_id,
            user_id: Some(user_id),
            email: email.clone(),
        },
        email,
        plan: claims["https://api.openai.com/auth"]["chatgpt_plan_type"]
            .as_str()
            .map(str::to_owned),
    };
    let last = value
        .get("last_refresh")
        .and_then(|v| v.as_str())
        .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
        .map(|v| v.with_timezone(&Utc));
    // JWT expiry is evidence, not a locale-dependent source timestamp heuristic.
    let expires_at = claims
        .get("exp")
        .and_then(|v| v.as_i64())
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .ok_or(TakeoverError::Invalid)?;
    Ok((
        profile,
        tokens,
        super::TokenMeta {
            expires_at: Some(expires_at),
            last_refresh_at: last,
            refresh_uncertain: false,
        },
    ))
}

pub fn validate_source(
    deps: &Dependencies,
    provider: Provider,
    proof: &SourceProof,
) -> Result<(), TakeoverError> {
    let age = deps.source.now().signed_duration_since(proof.modified);
    if age < chrono::Duration::seconds(120) || deps.source.owner(provider, proof) != Owner::Stopped
    {
        return Err(TakeoverError::Conflict);
    }
    Ok(())
}

fn digest(value: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(value)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub enum Preparation {
    Inspect(serde_json::Value),
    Commit(Box<Journal>),
}

fn same_person(
    store: &AccountStore,
    old: &Account,
    incoming: &Identity,
) -> Result<bool, TakeoverError> {
    match (&old.identity, incoming) {
        (
            Identity::Codex {
                account_id: a,
                user_id,
                ..
            },
            Identity::Codex {
                account_id: b,
                user_id: Some(user),
                ..
            },
        ) if a == b => {
            let known = if let Some(known) = user_id {
                Some(known.clone())
            } else {
                super::import::codex_user_id(&store.tokens(old)?, a)
            };
            Ok(known.ok_or(TakeoverError::Conflict)? == *user)
        }
        (
            Identity::Claude {
                account_uuid: a,
                organization_uuid: x,
                ..
            },
            Identity::Claude {
                account_uuid: b,
                organization_uuid: y,
                ..
            },
        ) => Ok(a == b && (x.is_none() || x == y)),
        _ => Ok(old.identity.canonical_key() == incoming.canonical_key()),
    }
}

pub async fn prepare(
    store: Arc<AccountStore>,
    deps: Dependencies,
    request: TakeoverRequest,
) -> Result<Preparation, TakeoverError> {
    if !request.path.is_absolute() {
        return Err(TakeoverError::Invalid);
    }
    if request.phase == Phase::Commit {
        let inspected = request
            .inspect_digest
            .as_deref()
            .ok_or(TakeoverError::Conflict)?;
        let operation = digest(inspected.as_bytes());
        let lookup_store = store.clone();
        let saved =
            tokio::task::spawn_blocking(move || lookup_store.read_takeover_journal(&operation))
                .await
                .map_err(|_| TakeoverError::Unavailable)??;
        if let Some(saved) = saved {
            if saved.provider != request.provider
                || saved.requested_path != request.path
                || saved.replace != request.replace
                || saved.keep_source != request.keep_source
                || saved.inspect_digest != inspected
            {
                return Err(TakeoverError::Conflict);
            }
            return Ok(Preparation::Commit(Box::new(saved)));
        }
    }
    let source = deps.source.clone();
    let path = request.path.clone();
    let provider = request.provider;
    let snapshot = tokio::task::spawn_blocking(move || source.snapshot(provider, &path))
        .await
        .map_err(|_| TakeoverError::Unavailable)??;
    if snapshot.bytes.len() > 1024 * 1024
        || snapshot.proof.sha256 != digest(&snapshot.bytes)
        || !snapshot.proof.path.is_absolute()
    {
        return Err(TakeoverError::Invalid);
    }
    // A terminal source snapshot already has durable identity evidence. Never
    // require its consumed/expired access token to authorize another profile GET.
    let saved_store = store.clone();
    let saved_deps = deps.clone();
    let saved_request = request.clone();
    let saved_proof = snapshot.proof.clone();
    if let Some(saved) = tokio::task::spawn_blocking(move || {
        terminal_snapshot(&saved_store, &saved_deps, &saved_request, &saved_proof)
    })
    .await
    .map_err(|_| TakeoverError::Unavailable)??
    {
        return Ok(saved);
    }
    let (profile, tokens, meta) =
        identify(request.provider, &snapshot.bytes, deps.profile.as_ref()).await?;
    tokio::task::spawn_blocking(move || {
        finish_prepare(&store, &deps, request, snapshot, profile, tokens, meta)
    })
    .await
    .map_err(|_| TakeoverError::Unavailable)?
}

fn terminal_snapshot(
    store: &AccountStore,
    deps: &Dependencies,
    request: &TakeoverRequest,
    proof: &SourceProof,
) -> Result<Option<Preparation>, TakeoverError> {
    for journal in store.takeover_journals()? {
        if journal.provider != request.provider
            || journal.source.path != proof.path
            || journal.source.volume != proof.volume
            || journal.source.index != proof.index
            || journal.source.sha256 != proof.sha256
        {
            continue;
        }
        let terminal = journal.result.is_some()
            || match store.takeover_receipt(&journal.account.id) {
                Ok(Some(receipt)) => receipt.operation == journal.operation && !receipt.fenced(),
                Ok(None) | Err(StoreError::NotFound) => false,
                Err(error) => return Err(error.into()),
            };
        if !terminal {
            continue;
        }
        if journal.replace != request.replace
            || journal.keep_source != request.keep_source
            || journal.requested_path != request.path
        {
            return Err(TakeoverError::Conflict);
        }
        if request.phase == Phase::Commit {
            if request.inspect_digest.as_deref() != Some(journal.inspect_digest.as_str()) {
                return Err(TakeoverError::Conflict);
            }
            return Ok(Some(Preparation::Commit(Box::new(journal))));
        }
        let provider_account_id = match &journal.account.identity {
            Identity::Codex { account_id, .. } => account_id,
            Identity::Claude { account_uuid, .. } => account_uuid,
        };
        let exists = AccountStore::list_metadata(store.home())?
            .iter()
            .any(|a| a.id == journal.account.id);
        return Ok(Some(Preparation::Inspect(serde_json::json!({
            "email":journal.account.email,"plan":journal.account.plan,"provider_account_id":provider_account_id,
            "source_path":proof.path,"source_last_refresh":journal.account.token_meta.last_refresh_at,
            "source_mtime":proof.modified,"owner_alive":deps.source.owner(request.provider, proof) != Owner::Stopped,
            "existing_id":exists.then_some(&journal.account.id),"inspect_digest":journal.inspect_digest,
        }))));
    }
    Ok(None)
}

fn finish_prepare(
    store: &AccountStore,
    deps: &Dependencies,
    request: TakeoverRequest,
    snapshot: SourceSnapshot,
    profile: ProfileIdentity,
    tokens: Tokens,
    meta: super::TokenMeta,
) -> Result<Preparation, TakeoverError> {
    // Profile GET ran without the store mutex. Check the exact source again.
    let rechecked = deps.source.snapshot(request.provider, &request.path)?;
    if rechecked.proof != snapshot.proof {
        return Err(TakeoverError::Conflict);
    }
    let accounts = AccountStore::list_metadata(store.home())?;
    let mut matches = Vec::new();
    for account in &accounts {
        if same_person(store, account, &profile.identity)? {
            matches.push(account.clone());
        }
    }
    if matches.len() > 1 {
        return Err(TakeoverError::Conflict);
    }
    let existing = matches.pop();
    // A kept, unchanged source must reuse its original operation, not reinstall
    // an old pair under a digest that merely has a newer target generation.
    let mut prior = None;
    for journal in store.takeover_journals()? {
        if journal.provider == request.provider
            && journal.source.path == snapshot.proof.path
            && journal.source.volume == snapshot.proof.volume
            && journal.source.index == snapshot.proof.index
            && journal.source.sha256 == snapshot.proof.sha256
            && store.takeover_intent_reusable(&journal)?
        {
            prior = Some(journal);
            break;
        }
    }
    let inspect_digest = if let Some(prior) = &prior {
        if prior.keep_source != request.keep_source || prior.replace != request.replace {
            return Err(TakeoverError::Conflict);
        }
        prior.inspect_digest.clone()
    } else {
        digest(
            &serde_json::to_vec(&(
                "quota-baton/takeover/v1",
                request.provider,
                &snapshot.proof,
                &profile.identity,
                existing.as_ref().map(|a| (&a.id, a.generation)),
                request.keep_source,
            ))
            .map_err(|_| TakeoverError::Invalid)?,
        )
    };
    if request.phase == Phase::Inspect {
        let provider_account_id = match &profile.identity {
            Identity::Codex { account_id, .. } => account_id,
            Identity::Claude { account_uuid, .. } => account_uuid,
        };
        return Ok(Preparation::Inspect(serde_json::json!({
            "email":profile.email,"plan":profile.plan,"provider_account_id":provider_account_id,
            "source_path":snapshot.proof.path,"source_last_refresh":meta.last_refresh_at,
            "source_mtime":snapshot.proof.modified,
            "owner_alive":deps.source.owner(request.provider, &snapshot.proof) != Owner::Stopped,
            "existing_id":existing.as_ref().map(|a| &a.id),"inspect_digest":inspect_digest,
        })));
    }
    validate_source(deps, request.provider, &snapshot.proof)?;
    // Inspection discovers whether this identity already exists. Replacement
    // consent follows that read; it is not a different source proof. The bound
    // target generation still rejects a concurrent identity/settings change.
    if request.inspect_digest.as_deref() != Some(inspect_digest.as_str())
        || (existing.is_some() && !request.replace)
    {
        return Err(TakeoverError::Conflict);
    }
    if let Some(prior) = prior {
        return Ok(Preparation::Commit(Box::new(prior)));
    }
    let expected = existing.as_ref().map(|a| a.generation);
    let mut account = if let Some(existing) = existing {
        existing
    } else {
        Account {
            schema: 1,
            id: profile.identity.id(),
            provider: request.provider,
            label: profile.email.clone(),
            email: profile.email.clone(),
            plan: profile.plan.clone(),
            identity: profile.identity,
            source: super::Source::Oauth,
            user_state: super::UserState::Active,
            health: super::Health::Ready,
            auth_failures: 0,
            payment_failures: 0,
            payment_until: None,
            auth_probe_pending: false,
            quota_cooling: None,
            drain: false,
            allow_credits: false,
            quota_credit_releasable: false,
            generation: 0,
            token_meta: meta.clone(),
        }
    };
    // Drain is an explicit user setting; takeover never silently clears it.
    if account.drain || account.token_meta.refresh_uncertain {
        return Err(TakeoverError::Conflict);
    }
    // Never roll back a known newer pair, including an unknown-order source.
    if expected.is_some()
        && meta
            .compare(&account.token_meta)
            .is_none_or(|order| order.is_lt())
    {
        return Err(TakeoverError::Conflict);
    }
    account.token_meta = meta;
    account.email = profile.email;
    account.plan = profile.plan;
    account.source = super::Source::Takeover {
        from: snapshot.proof.path.clone(),
        at: deps.source.now(),
    };
    if matches!(
        account.health,
        super::Health::NeedsRelogin { .. }
            | super::Health::Cooling {
                reason: super::HealthReason::Auth
                    | super::HealthReason::StoreError
                    | super::HealthReason::LinkStale,
                ..
            }
    ) {
        account.apply_health_signal(super::HealthSignal::ReloginCompleted, deps.source.now());
    }
    let destination = snapshot.proof.path.with_file_name(format!(
        "{}.taken-over-by-quota-baton-{}",
        snapshot
            .proof
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(TakeoverError::Invalid)?,
        deps.source.now().format("%Y%m%d")
    ));
    Ok(Preparation::Commit(Box::new(Journal {
        schema: 1,
        operation: digest(inspect_digest.as_bytes()),
        inspect_digest,
        provider: request.provider,
        requested_path: request.path,
        source: snapshot.proof,
        replace: request.replace,
        keep_source: request.keep_source,
        destination,
        account,
        expected,
        protected_tokens: Some(AccountStore::protect_takeover_tokens(&tokens)?),
        retired: false,
        retirement_started: false,
        superseded: false,
        result: None,
    })))
}

#[cfg(test)]
#[path = "takeover_tests.rs"]
mod tests;

/// Must run inside TokenGates' tracked blocking work with the account gate held.
pub fn install(
    store: &AccountStore,
    deps: &Dependencies,
    mut journal: Journal,
) -> Result<Journal, TakeoverError> {
    if store.writes_stopped() {
        return Err(TakeoverError::Unavailable);
    }
    if let Some(current) = store.read_takeover_journal(&journal.operation)? {
        journal = current;
    }
    let receipt = match store.takeover_receipt(&journal.account.id) {
        Ok(receipt) => receipt,
        Err(StoreError::NotFound) => None,
        Err(e) => return Err(e.into()),
    };
    let ours = receipt
        .as_ref()
        .is_some_and(|r| r.operation == journal.operation);
    if receipt.as_ref().is_some_and(|r| r.fenced()) && !ours {
        return Err(TakeoverError::Conflict);
    }
    if ours
        && receipt
            .as_ref()
            .is_some_and(|r| r.state == ReceiptState::Superseded)
    {
        finalize(store, journal.clone())?;
        return store
            .read_takeover_journal(&journal.operation)?
            .ok_or(TakeoverError::Invalid);
    }
    if !ours {
        if journal.result.is_some() {
            return Ok(journal);
        }
        let snapshot = deps
            .source
            .snapshot(journal.provider, &journal.requested_path)?;
        if snapshot.proof != journal.source {
            return Err(TakeoverError::Conflict);
        }
        validate_source(deps, journal.provider, &snapshot.proof)?;
        // Journal first. Store CAS prevents overwriting an account that raced
        // after profile/preflight; no retirement or request precedes that CAS.
        journal.account = store.publish_takeover(&journal)?;
    }
    if !journal.retired {
        validate_source(deps, journal.provider, &journal.source)?;
    }
    store.retire_takeover(&journal, deps)
}

pub fn finalize(
    store: &AccountStore,
    mut journal: Journal,
) -> Result<serde_json::Value, TakeoverError> {
    if journal.result.is_some() {
        return Ok(result_value(&journal));
    }
    let receipt = store
        .takeover_receipt(&journal.account.id)?
        .ok_or(TakeoverError::Invalid)?;
    if receipt.operation != journal.operation {
        return Err(TakeoverError::Conflict);
    }
    let result = match receipt.state {
        ReceiptState::Finished { result } => result,
        ReceiptState::Superseded => {
            let terminal =
                store.finalize_superseded_takeover(&journal.account.id, &journal.operation)?;
            return Ok(result_value(&terminal));
        }
        _ => return Err(TakeoverError::Uncertain),
    };
    journal.result = Some(result.clone());
    journal.protected_tokens = None;
    store.write_takeover_journal(&journal)?;
    Ok(result_value(&journal))
}

/// A completed tombstone remains replayable after deletion or a later takeover.
pub fn result_value(journal: &Journal) -> serde_json::Value {
    serde_json::json!({"id":journal.account.id,
        "renamed_to":(journal.retired && !journal.keep_source).then_some(&journal.destination),"refresh":journal.result})
}
