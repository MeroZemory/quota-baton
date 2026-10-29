//! Privacy boundary for the bounded, native permit-decision tape.
//! Never serialize `DecisionInputs`: its Account contains identity and paths.
//! Only the UsageSnapshot branch of apply_health_signal is reachable here.
use super::kernel::{
    self, AcceptanceBinding, AcceptanceGroup, CandidateInput, DecisionInputs, PermitDecision,
    PermitInputs,
};
use crate::accounts::{
    Account, Health, HealthReason, Identity, Provider, QuotaCooling, Source, TokenMeta, UserState,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) mod selection;

pub const SCHEMA: u32 = 1;
const MAX_BYTES: usize = 256 * 1024;
const MAX_ROWS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdKind {
    Account,
    Session,
}

/// Refusals contain no offending text or payload and are safe to count/log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireRefusal {
    Schema,
    Shape,
    Bounds,
    Identity,
    Collision,
    Plan,
    Model,
    Health,
    Quota,
    QuotaReference,
    QuotaRoot,
    QuotaWindows,
    QuotaWindow,
    QuotaWindowName,
    QuotaWindowUsed,
    QuotaWindowStatus,
    QuotaWindowReset,
    QuotaWindowObserved,
    QuotaCredits,
    QuotaCreditFlag,
    QuotaCreditBalance,
    QuotaPaidServing,
    MissingPermit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitRecord {
    pub schema: u32,
    pub ordinal: u64,
    pub inputs: Option<WireInputs>,
    pub actual: PermitDecision,
    pub refusal: Option<WireRefusal>,
}

/// Missing and explicit null remain distinct. Unsupported types cause refusal.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Field<T> {
    Missing,
    Null,
    Value(T),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountFacts {
    pub id: String,
    pub generation: u64,
    pub provider: Provider,
    pub plan: Option<Plan>,
    pub user_state: UserState,
    pub health: HealthFacts,
    pub auth_failures: u32,
    pub payment_failures: u32,
    pub payment_until: Option<DateTime<Utc>>,
    pub auth_probe_pending: bool,
    pub quota_cooling: Option<CoolingFacts>,
    pub quota_credit_releasable: bool,
    pub drain: bool,
    pub allow_credits: bool,
}

macro_rules! vocabulary {
    ($name:ident, $refusal:ident, {$($variant:ident => $text:literal),+ $(,)?}) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $text)] $variant),+ }
        impl $name {
            fn parse(text: &str) -> Result<Self, WireRefusal> {
                match text { $($text => Ok(Self::$variant),)+ _ => Err(WireRefusal::$refusal) }
            }
            fn text(self) -> &'static str { match self { $(Self::$variant => $text,)+ } }
        }
    };
}
vocabulary!(Plan, Plan, {
    Free => "free", Go => "go", Plus => "plus", Pro => "pro", Prolite => "prolite",
    Promax => "promax", Max => "max", Max5 => "max_5x", Max20 => "max_20x",
    Team => "team", Business => "business", Enterprise => "enterprise", Edu => "edu"
});
/// Exact native model identifiers, with bounded family/version vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Model(String);
impl Model {
    fn parse(text: &str) -> Result<Self, WireRefusal> {
        let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let version = |s: &str| s.split('.').all(numeric);
        let suffix = |s: &str| {
            numeric(s)
                || matches!(
                    s,
                    "codex"
                        | "mini"
                        | "nano"
                        | "max"
                        | "latest"
                        | "preview"
                        | "pro"
                        | "chat"
                        | "luna"
                        | "turbo"
                        | "thinking"
                        | "fast"
                        | "spark"
                        | "lite"
                )
        };
        let rest = text.strip_prefix("claude-").unwrap_or(text);
        let family = ["opus", "sonnet", "haiku"].iter().find_map(|family| {
            if rest == *family {
                Some("")
            } else {
                rest.strip_prefix(*family).and_then(|s| s.strip_prefix('-'))
            }
        });
        let valid = if let Some(rest) = family {
            rest.is_empty() || rest.split('-').all(suffix)
        } else if let Some(rest) = text.strip_prefix("gpt-").or_else(|| text.strip_prefix('o')) {
            let mut parts = rest.split('-');
            parts.next().is_some_and(version) && parts.all(suffix)
        } else {
            text.strip_prefix("codex-")
                .is_some_and(|s| !s.is_empty() && s.split('-').all(suffix))
        };
        if text.len() > 96 || text.ends_with('-') || !valid {
            return Err(WireRefusal::Model);
        }
        Ok(Self(text.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    fn text(&self) -> &str {
        self.as_str()
    }
}
impl TryFrom<String> for Model {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value).map_err(|_| "unsupported native model")
    }
}
impl From<Model> for String {
    fn from(value: Model) -> Self {
        value.0
    }
}

/// Keep literal duration identities, including 0m/43200m and native seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Window(String);
impl Window {
    fn parse(text: &str) -> Result<Self, WireRefusal> {
        let named = matches!(
            text,
            "primary" | "secondary" | "five_hour" | "seven_day" | "seven_day_opus"
        );
        let (duration, suffix) = text
            .split_once('_')
            .map_or((text, None), |(d, s)| (d, Some(s)));
        let numeric = duration.len() >= 2
            && duration.len() <= 21
            && matches!(duration.as_bytes().last(), Some(b'm' | b's' | b'h' | b'd'))
            && duration[..duration.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_digit())
            && duration[..duration.len() - 1].parse::<u64>().is_ok();
        let known_suffix = suffix.is_none_or(|s| matches!(s, "oi" | "sonnet" | "opus" | "haiku"));
        if !(named || numeric && known_suffix) {
            return Err(WireRefusal::Quota);
        }
        Ok(Self(text.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    fn text(&self) -> &str {
        self.as_str()
    }
}
impl TryFrom<String> for Window {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value).map_err(|_| "unsupported native window")
    }
}
impl From<Window> for String {
    fn from(value: Window) -> Self {
        value.0
    }
}
// Native Codex header snapshots retain the boolean header as literal text.
// These are not aliases for allowed/rejected: native health reads distinguish
// those strings, so the archive must preserve them without interpretation.
vocabulary!(Status, Quota, {
    Rejected => "rejected", Allowed => "allowed", Warning => "allowed_warning",
    True => "true", False => "false"
});
vocabulary!(ReloginReason, Health, {
    InvalidGrant => "invalid_grant", TokenRevoked => "token_revoked", Revoked => "revoked",
    AuthAfterRefresh => "auth_after_refresh", RefreshReused => "refresh_token_reused",
    RefreshExpired => "refresh_token_expired", RefreshInvalidated => "refresh_token_invalidated"
});

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "model",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Scope {
    Account,
    Model(Model),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HealthFacts {
    Ready,
    PlanWithoutClaudeCode,
    Cooling {
        reason: HealthReason,
        until: DateTime<Utc>,
        scope: Scope,
    },
    NeedsRelogin {
        reason: ReloginReason,
        since: DateTime<Utc>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoolingFacts {
    pub window: Window,
    pub reset_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowFacts {
    pub name: Field<Window>,
    pub used_pct: Field<Number>,
    pub status: Field<Status>,
    pub reset_at: Field<DateTime<Utc>>,
    pub observed_at: Field<DateTime<Utc>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreditFacts {
    pub has_credits: Field<bool>,
    pub unlimited: Field<bool>,
    pub balance: Field<Number>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaFacts {
    pub windows: Field<Vec<WindowFacts>>,
    pub credits: Field<CreditFacts>,
    pub paid_serving_observed_at: Field<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QuotaDigest(pub [u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaKey {
    pub account: String,
    pub revision: u64,
    pub digest: QuotaDigest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "facts",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuotaInput {
    Missing,
    Null,
    Inline(QuotaFacts),
    Ref { digest: QuotaDigest },
}

/// Storage owns emission/caching; this DTO contains only projected native facts.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaSnapshot {
    pub account: String,
    pub revision: u64,
    pub digest: QuotaDigest,
    pub quota: QuotaFacts,
}
impl QuotaSnapshot {
    pub fn key(&self) -> QuotaKey {
        QuotaKey {
            account: self.account.clone(),
            revision: self.revision,
            digest: self.digest,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCandidate {
    pub account: AccountFacts,
    pub revision: u64,
    pub quota: QuotaInput,
    pub inflight: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireInputs {
    pub now: DateTime<Utc>,
    pub model: Option<Model>,
    pub key: Option<String>,
    pub from: Option<String>,
    pub to: String,
    pub accounts: BTreeMap<String, WireCandidate>,
    pub traffic: BTreeMap<String, DateTime<Utc>>,
    pub pending: BTreeMap<String, usize>,
    pub groups: BTreeMap<String, AcceptanceGroup>,
    pub bindings: BTreeMap<String, AcceptanceBinding>,
    pub accepted_unbound: BTreeSet<String>,
    pub sticky: BTreeMap<String, String>,
    pub outside_failure: bool,
    pub permit: Option<PermitInputs>,
}

impl HealthFacts {
    fn capture(health: &Health) -> Result<Self, WireRefusal> {
        Ok(match health {
            Health::Ready => Self::Ready,
            Health::PlanWithoutClaudeCode => Self::PlanWithoutClaudeCode,
            Health::Cooling {
                reason,
                until,
                scope,
            } => Self::Cooling {
                reason: *reason,
                until: *until,
                scope: if scope == "account" {
                    Scope::Account
                } else {
                    Scope::Model(
                        Model::parse(scope.strip_prefix("model:").ok_or(WireRefusal::Health)?)
                            .map_err(|_| WireRefusal::Health)?,
                    )
                },
            },
            Health::NeedsRelogin { reason, since } => Self::NeedsRelogin {
                reason: ReloginReason::parse(reason)?,
                since: *since,
            },
        })
    }
    fn health(&self) -> Health {
        match self {
            Self::Ready => Health::Ready,
            Self::PlanWithoutClaudeCode => Health::PlanWithoutClaudeCode,
            Self::Cooling {
                reason,
                until,
                scope,
            } => Health::Cooling {
                reason: *reason,
                until: *until,
                scope: match scope {
                    Scope::Account => "account".into(),
                    Scope::Model(m) => format!("model:{}", m.text()),
                },
            },
            Self::NeedsRelogin { reason, since } => Health::NeedsRelogin {
                reason: reason.text().into(),
                since: *since,
            },
        }
    }
}

impl AccountFacts {
    fn capture(a: &Account, id: String) -> Result<Self, WireRefusal> {
        Ok(Self {
            id,
            generation: a.generation,
            provider: a.provider,
            plan: a.plan.as_deref().map(Plan::parse).transpose()?,
            user_state: a.user_state,
            health: HealthFacts::capture(&a.health)?,
            auth_failures: a.auth_failures,
            payment_failures: a.payment_failures,
            payment_until: a.payment_until,
            auth_probe_pending: a.auth_probe_pending,
            quota_cooling: a
                .quota_cooling
                .as_ref()
                .map(|q| {
                    Ok::<_, WireRefusal>(CoolingFacts {
                        window: Window::parse(&q.window)?,
                        reset_at: q.reset_at,
                        started_at: q.started_at,
                    })
                })
                .transpose()?,
            quota_credit_releasable: a.quota_credit_releasable,
            drain: a.drain,
            allow_credits: a.allow_credits,
        })
    }
    fn account(&self) -> Result<Account, WireRefusal> {
        // These fields are unread by eligible_at, semantic_evidence and the
        // UsageSnapshot transition. They are explicitly synthetic, never facts.
        let identity = match self.provider {
            Provider::Claude => Identity::Claude {
                account_uuid: String::new(),
                organization_uuid: None,
                email: String::new(),
            },
            Provider::Codex => Identity::Codex {
                account_id: String::new(),
                email: String::new(),
                user_id: None,
            },
        };
        Ok(Account {
            schema: 1,
            id: self.id.clone(),
            provider: self.provider,
            label: String::new(),
            email: String::new(),
            plan: self.plan.map(|p| p.text().to_owned()),
            plan_observed_at: None,
            subscription: None,
            identity,
            source: Source::Oauth,
            user_state: self.user_state,
            health: self.health.health(),
            auth_failures: self.auth_failures,
            payment_failures: self.payment_failures,
            payment_until: self.payment_until,
            auth_probe_pending: self.auth_probe_pending,
            quota_cooling: self.quota_cooling.as_ref().map(|q| QuotaCooling {
                window: q.window.text().into(),
                reset_at: q.reset_at,
                started_at: q.started_at,
            }),
            quota_credit_releasable: self.quota_credit_releasable,
            drain: self.drain,
            allow_credits: self.allow_credits,
            generation: self.generation,
            token_meta: TokenMeta::default(),
        })
    }
}

fn field<T>(
    v: Option<&Value>,
    parse: impl FnOnce(&Value) -> Result<T, WireRefusal>,
) -> Result<Field<T>, WireRefusal> {
    match v {
        None => Ok(Field::Missing),
        Some(Value::Null) => Ok(Field::Null),
        Some(v) => parse(v).map(Field::Value),
    }
}
fn object(v: &Value) -> Result<&Map<String, Value>, WireRefusal> {
    v.as_object().ok_or(WireRefusal::Quota)
}
fn number(v: &Value) -> Result<Number, WireRefusal> {
    v.as_number().cloned().ok_or(WireRefusal::Quota)
}
fn boolean(v: &Value) -> Result<bool, WireRefusal> {
    v.as_bool().ok_or(WireRefusal::Quota)
}
fn date(v: &Value) -> Result<DateTime<Utc>, WireRefusal> {
    DateTime::parse_from_rfc3339(v.as_str().ok_or(WireRefusal::Quota)?)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| WireRefusal::Quota)
}
fn window(v: &Value) -> Result<WindowFacts, WireRefusal> {
    let o = object(v).map_err(|_| WireRefusal::QuotaWindow)?;
    Ok(WindowFacts {
        name: field(o.get("name"), |v| {
            Window::parse(v.as_str().ok_or(WireRefusal::Quota)?)
        })
        .map_err(|_| WireRefusal::QuotaWindowName)?,
        used_pct: field(o.get("used_pct"), number).map_err(|_| WireRefusal::QuotaWindowUsed)?,
        status: field(o.get("status"), |v| {
            Status::parse(v.as_str().ok_or(WireRefusal::Quota)?)
        })
        .map_err(|_| WireRefusal::QuotaWindowStatus)?,
        reset_at: field(o.get("reset_at"), date).map_err(|_| WireRefusal::QuotaWindowReset)?,
        observed_at: field(o.get("observed_at"), date)
            .map_err(|_| WireRefusal::QuotaWindowObserved)?,
    })
}
fn quota(v: &Value) -> Result<QuotaFacts, WireRefusal> {
    let o = object(v).map_err(|_| WireRefusal::QuotaRoot)?;
    Ok(QuotaFacts {
        windows: field(o.get("windows"), |v| {
            let rows = v.as_array().ok_or(WireRefusal::QuotaWindows)?;
            if rows.len() > 32 {
                return Err(WireRefusal::Bounds);
            }
            rows.iter().map(window).collect()
        })?,
        credits: field(o.get("credits"), |v| {
            let c = object(v).map_err(|_| WireRefusal::QuotaCredits)?;
            Ok(CreditFacts {
                has_credits: field(c.get("has_credits"), boolean)
                    .map_err(|_| WireRefusal::QuotaCreditFlag)?,
                unlimited: field(c.get("unlimited"), boolean)
                    .map_err(|_| WireRefusal::QuotaCreditFlag)?,
                balance: field(c.get("balance"), number)
                    .map_err(|_| WireRefusal::QuotaCreditBalance)?,
            })
        })?,
        paid_serving_observed_at: field(o.get("paid_serving_observed_at"), date)
            .map_err(|_| WireRefusal::QuotaPaidServing)?,
        // effective_account unconditionally replaces signals with Null.
    })
}
fn put<T>(o: &mut Map<String, Value>, key: &str, f: &Field<T>, encode: impl FnOnce(&T) -> Value) {
    match f {
        Field::Missing => {}
        Field::Null => {
            o.insert(key.into(), Value::Null);
        }
        Field::Value(v) => {
            o.insert(key.into(), encode(v));
        }
    }
}
fn date_value(v: &DateTime<Utc>) -> Value {
    Value::String(v.to_rfc3339())
}
impl QuotaFacts {
    fn value(&self) -> Value {
        let mut o = Map::new();
        put(&mut o, "windows", &self.windows, |rows| {
            Value::Array(
                rows.iter()
                    .map(|w| {
                        let mut o = Map::new();
                        put(&mut o, "name", &w.name, |n| n.text().into());
                        put(&mut o, "used_pct", &w.used_pct, |n| {
                            Value::Number(n.clone())
                        });
                        put(&mut o, "status", &w.status, |s| s.text().into());
                        put(&mut o, "reset_at", &w.reset_at, date_value);
                        put(&mut o, "observed_at", &w.observed_at, date_value);
                        Value::Object(o)
                    })
                    .collect(),
            )
        });
        put(&mut o, "credits", &self.credits, |c| {
            let mut o = Map::new();
            put(&mut o, "has_credits", &c.has_credits, |b| (*b).into());
            put(&mut o, "unlimited", &c.unlimited, |b| (*b).into());
            put(&mut o, "balance", &c.balance, |n| Value::Number(n.clone()));
            Value::Object(o)
        });
        put(
            &mut o,
            "paid_serving_observed_at",
            &self.paid_serving_observed_at,
            date_value,
        );
        Value::Object(o)
    }
}

struct Ids<F> {
    mapper: F,
    cache: BTreeMap<(IdKind, String), String>,
    reverse: BTreeMap<(IdKind, String), String>,
}
impl<F: FnMut(IdKind, &str) -> String> Ids<F> {
    fn token(&mut self, kind: IdKind, raw: &str) -> Result<String, WireRefusal> {
        let key = (kind, raw.to_owned());
        if let Some(id) = self.cache.get(&key) {
            return Ok(id.clone());
        }
        let token = (self.mapper)(kind, raw);
        if !compact(&token) {
            return Err(WireRefusal::Identity);
        }
        if self
            .reverse
            .insert((kind, token.clone()), raw.into())
            .is_some()
        {
            return Err(WireRefusal::Collision);
        }
        self.cache.insert(key, token.clone());
        Ok(token)
    }
    fn id(&mut self, kind: IdKind, raw: &str) -> Result<String, WireRefusal> {
        let cap = if kind == IdKind::Account { 128 } else { 256 };
        // Remapping malformed native IDs would alter the Capacity branch.
        if raw.is_empty() || raw.len() > cap {
            return Err(WireRefusal::Identity);
        }
        if kind == IdKind::Account {
            return Ok(format!("a:{}", self.token(kind, raw)?));
        }
        if raw.starts_with("claude:") {
            if let Some((root, _)) = raw.split_once(":agent:") {
                return Ok(format!(
                    "claude:{}:agent:{}",
                    self.token(kind, root)?,
                    self.token(kind, raw)?
                ));
            }
            return Ok(format!("claude:{}", self.token(kind, raw)?));
        }
        Ok(format!("s:{}", self.token(kind, raw)?))
    }
}
fn compact(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
fn wire_id(kind: IdKind, id: &str) -> Result<(), WireRefusal> {
    let valid = if kind == IdKind::Account {
        id.strip_prefix("a:").is_some_and(compact)
    } else if let Some(root) = id.strip_prefix("claude:") {
        root.split_once(":agent:").map_or_else(
            || compact(root),
            |(root, child)| compact(root) && compact(child),
        )
    } else {
        id.strip_prefix("s:").is_some_and(compact)
    };
    if valid {
        Ok(())
    } else {
        Err(WireRefusal::Identity)
    }
}
fn remap_permit(
    p: &mut PermitInputs,
    map: &mut impl FnMut(IdKind, &str) -> Result<String, WireRefusal>,
) -> Result<(), WireRefusal> {
    if p.history.len() > 128 || p.global.len() > MAX_ROWS {
        return Err(WireRefusal::Bounds);
    }
    for m in &mut p.history {
        m.from = map(IdKind::Account, &m.from)?;
        m.to = map(IdKind::Account, &m.to)?;
    }
    for i in &mut p.global {
        if let Some((id, _)) = &mut i.shared {
            *id = map(IdKind::Account, id)?;
        }
    }
    if let Some(s) = &mut p.sole_target {
        s.from = map(IdKind::Account, &s.from)?;
        s.to = map(IdKind::Account, &s.to)?;
        if let Some(m) = &s.model {
            Model::parse(m)?;
        }
    }
    Ok(())
}
fn remap_actual(
    a: &mut PermitDecision,
    map: &mut impl FnMut(IdKind, &str) -> Result<String, WireRefusal>,
) -> Result<(), WireRefusal> {
    if let PermitDecision::Allowed(p) = a {
        p.from = map(IdKind::Account, &p.from)?;
        p.to = map(IdKind::Account, &p.to)?;
        p.key = map(IdKind::Session, &p.key)?;
        if let Some((id, _)) = &mut p.shared_incident {
            *id = map(IdKind::Account, id)?;
        }
    }
    Ok(())
}

/// Mapper supplies epoch-private tokens (1..=64 ASCII alphanumerics, '_' or '-').
/// Mapping is cached per kind/raw ID and collisions refuse the record. Claude
/// roots and complete child keys are mapped separately to preserve group().
pub fn capture(
    inputs: &DecisionInputs,
    actual: &PermitDecision,
    mapper: impl FnMut(IdKind, &str) -> String,
) -> Result<PermitRecord, WireRefusal> {
    let mut ids = Ids {
        mapper,
        cache: BTreeMap::new(),
        reverse: BTreeMap::new(),
    };
    let mut map = |kind, id: &str| ids.id(kind, id);
    if inputs.accounts.len() > 128
        || [
            inputs.traffic.len(),
            inputs.pending.len(),
            inputs.groups.len(),
            inputs.bindings.len(),
            inputs.accepted_unbound.len(),
            inputs.sticky.len(),
        ]
        .into_iter()
        .any(|n| n > MAX_ROWS)
    {
        return Err(WireRefusal::Bounds);
    }
    if inputs
        .groups
        .values()
        .try_fold(0usize, |n, g| n.checked_add(g.keys.len()))
        .is_none_or(|n| n > MAX_ROWS)
        || inputs
            .permit
            .as_ref()
            .is_some_and(|p| p.history.len() > 128 || p.global.len() > MAX_ROWS)
    {
        return Err(WireRefusal::Bounds);
    }
    let mut accounts = BTreeMap::new();
    for (id, c) in &inputs.accounts {
        accounts.insert(
            map(IdKind::Account, id)?,
            WireCandidate {
                account: AccountFacts::capture(&c.account, map(IdKind::Account, &c.account.id)?)?,
                revision: c.account.generation,
                quota: match field(c.quota.as_ref(), quota)? {
                    Field::Missing => QuotaInput::Missing,
                    Field::Null => QuotaInput::Null,
                    Field::Value(q) => QuotaInput::Inline(q),
                },
                inflight: c.inflight,
            },
        );
    }
    let mut groups = BTreeMap::new();
    for (key, g) in &inputs.groups {
        if g.keys.len() > MAX_ROWS {
            return Err(WireRefusal::Bounds);
        }
        groups.insert(
            map(IdKind::Session, key)?,
            AcceptanceGroup {
                account: map(IdKind::Account, &g.account)?,
                keys: g
                    .keys
                    .iter()
                    .map(|k| map(IdKind::Session, k))
                    .collect::<Result<_, _>>()?,
            },
        );
    }
    let mut permit = inputs.permit.clone();
    if let Some(p) = &mut permit {
        remap_permit(p, &mut map)?;
    }
    let mut actual = actual.clone();
    remap_actual(&mut actual, &mut map)?;
    let wire = WireInputs {
        now: inputs.now,
        model: inputs.model.as_deref().map(Model::parse).transpose()?,
        key: inputs
            .key
            .as_deref()
            .map(|id| map(IdKind::Session, id))
            .transpose()?,
        from: inputs
            .from
            .as_deref()
            .map(|id| map(IdKind::Account, id))
            .transpose()?,
        to: map(IdKind::Account, &inputs.to)?,
        accounts,
        groups,
        traffic: inputs
            .traffic
            .iter()
            .map(|(id, t)| Ok((map(IdKind::Account, id)?, *t)))
            .collect::<Result<_, WireRefusal>>()?,
        pending: inputs
            .pending
            .iter()
            .map(|(id, n)| Ok((map(IdKind::Account, id)?, *n)))
            .collect::<Result<_, WireRefusal>>()?,
        bindings: inputs
            .bindings
            .iter()
            .map(|(id, b)| Ok((map(IdKind::Session, id)?, b.clone())))
            .collect::<Result<_, WireRefusal>>()?,
        accepted_unbound: inputs
            .accepted_unbound
            .iter()
            .map(|id| map(IdKind::Account, id))
            .collect::<Result<_, _>>()?,
        sticky: inputs
            .sticky
            .iter()
            .map(|(k, id)| Ok((map(IdKind::Session, k)?, map(IdKind::Account, id)?)))
            .collect::<Result<_, WireRefusal>>()?,
        outside_failure: inputs.outside_failure,
        permit,
    };
    let record = PermitRecord {
        schema: SCHEMA,
        ordinal: 0,
        inputs: Some(wire),
        actual,
        refusal: None,
    };
    bounded(&record)?;
    Ok(record)
}

/// Retain a non-comparable decision, including its privacy-safe actual outcome.
/// A failing mapper still returns Err; the archive must count that as loss.
pub fn refused_record(
    actual: &PermitDecision,
    reason: WireRefusal,
    mapper: impl FnMut(IdKind, &str) -> String,
) -> Result<PermitRecord, WireRefusal> {
    let mut ids = Ids {
        mapper,
        cache: BTreeMap::new(),
        reverse: BTreeMap::new(),
    };
    let mut actual = actual.clone();
    remap_actual(&mut actual, &mut |kind, raw| ids.id(kind, raw))?;
    let record = PermitRecord {
        schema: SCHEMA,
        ordinal: 0,
        inputs: None,
        actual,
        refusal: Some(reason),
    };
    bounded(&record)?;
    Ok(record)
}

fn bounded(value: &impl Serialize) -> Result<(), WireRefusal> {
    // Fail while counting, without allocating an oversized serialized record.
    // Only privacy-reviewed DTOs reach this serializer, never native Account.
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "permit record bound")
            })?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(MAX_BYTES), value).map_err(|_| WireRefusal::Bounds)
}
impl WireInputs {
    /// Replace inline facts with hash references, returning snapshots for Main's
    /// cache to emit once per (account, revision, digest). Missing/null stay raw.
    /// Main commits returned keys to `seen` only after the envelope is queued.
    pub fn intern_quotas(
        &mut self,
        seen: &BTreeSet<QuotaKey>,
    ) -> Result<Vec<QuotaSnapshot>, WireRefusal> {
        bounded(self)?;
        let mut snapshots = BTreeMap::new();
        let mut replacements = Vec::new();
        for (key, candidate) in &self.accounts {
            if candidate.revision != candidate.account.generation {
                return Err(WireRefusal::QuotaReference);
            }
            if let QuotaInput::Inline(quota) = &candidate.quota {
                let account = &candidate.account.id;
                let digest = quota_digest(account, candidate.revision, quota)?;
                let snapshot = QuotaSnapshot {
                    account: account.clone(),
                    revision: candidate.revision,
                    digest,
                    quota: quota.clone(),
                };
                bounded(&snapshot)?;
                if !seen.contains(&snapshot.key()) {
                    snapshots.insert(snapshot.key(), snapshot);
                }
                replacements.push((key.clone(), digest));
            } else if let QuotaInput::Ref { digest } = &candidate.quota {
                let key = QuotaKey {
                    account: candidate.account.id.clone(),
                    revision: candidate.revision,
                    digest: *digest,
                };
                if !seen.contains(&key) {
                    return Err(WireRefusal::QuotaReference);
                }
            }
        }
        let mut interned = self.clone();
        for (key, digest) in replacements {
            interned
                .accounts
                .get_mut(&key)
                .expect("existing candidate")
                .quota = QuotaInput::Ref { digest };
        }
        bounded(&interned)?;
        *self = interned;
        Ok(snapshots.into_values().collect())
    }

    /// Resolve atomically: missing, misbound or changed snapshots refuse without
    /// partially replacing references. Hashes bind schema/account/revision/facts.
    pub fn resolve_quotas(
        &mut self,
        snapshots: &BTreeMap<QuotaKey, QuotaSnapshot>,
    ) -> Result<(), WireRefusal> {
        bounded(self)?;
        let mut resolved = self.clone();
        for candidate in resolved.accounts.values_mut() {
            if candidate.revision != candidate.account.generation {
                return Err(WireRefusal::QuotaReference);
            }
            if let QuotaInput::Ref { digest } = &candidate.quota {
                let account = &candidate.account.id;
                let key = QuotaKey {
                    account: account.clone(),
                    revision: candidate.revision,
                    digest: *digest,
                };
                let snapshot = snapshots.get(&key).ok_or(WireRefusal::QuotaReference)?;
                bounded(&snapshot)?;
                if snapshot.account != *account
                    || snapshot.revision != candidate.revision
                    || snapshot.digest != *digest
                    || quota_digest(account, candidate.revision, &snapshot.quota)? != *digest
                {
                    return Err(WireRefusal::QuotaReference);
                }
                candidate.quota = QuotaInput::Inline(snapshot.quota.clone());
            }
        }
        bounded(&resolved)?;
        *self = resolved;
        Ok(())
    }

    pub fn to_inputs(&self) -> Result<DecisionInputs, WireRefusal> {
        bounded(self)?;
        let check = |kind, id: &str| {
            wire_id(kind, id)?;
            Ok(id.to_owned())
        };
        let mut check = check;
        let mut permit = self.permit.clone();
        if let Some(p) = &mut permit {
            remap_permit(p, &mut check)?;
        }
        let mut accounts = BTreeMap::new();
        for (id, c) in &self.accounts {
            if c.revision != c.account.generation {
                return Err(WireRefusal::QuotaReference);
            }
            wire_id(IdKind::Account, id)?;
            wire_id(IdKind::Account, &c.account.id)?;
            let quota = match &c.quota {
                QuotaInput::Missing => None,
                QuotaInput::Null => Some(Value::Null),
                QuotaInput::Inline(q) => Some(q.value()),
                QuotaInput::Ref { .. } => return Err(WireRefusal::QuotaReference),
            };
            let mut account = c.account.account()?;
            account.generation = c.revision;
            accounts.insert(
                id.clone(),
                CandidateInput {
                    account,
                    quota,
                    inflight: c.inflight,
                },
            );
        }
        for id in self
            .from
            .iter()
            .chain(std::iter::once(&self.to))
            .chain(self.traffic.keys())
            .chain(self.pending.keys())
            .chain(self.accepted_unbound.iter())
            .chain(self.sticky.values())
        {
            wire_id(IdKind::Account, id)?;
        }
        for id in self
            .key
            .iter()
            .chain(self.groups.keys())
            .chain(self.bindings.keys())
            .chain(self.sticky.keys())
        {
            wire_id(IdKind::Session, id)?;
        }
        for g in self.groups.values() {
            wire_id(IdKind::Account, &g.account)?;
            for k in &g.keys {
                wire_id(IdKind::Session, k)?;
            }
        }
        Ok(DecisionInputs {
            selection: None,
            now: self.now,
            model: self.model.as_ref().map(|m| m.text().into()),
            key: self.key.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
            accounts,
            traffic: self.traffic.clone(),
            pending: self.pending.clone(),
            groups: self.groups.clone(),
            bindings: self.bindings.clone(),
            accepted_unbound: self.accepted_unbound.clone(),
            sticky: self.sticky.clone(),
            outside_failure: self.outside_failure,
            permit,
        })
    }
}

fn quota_digest(
    account: &str,
    revision: u64,
    quota: &QuotaFacts,
) -> Result<QuotaDigest, WireRefusal> {
    use sha2::{Digest, Sha256};
    wire_id(IdKind::Account, account)?;
    bounded(quota)?;
    let bytes =
        serde_json::to_vec(&(SCHEMA, account, revision, quota)).map_err(|_| WireRefusal::Quota)?;
    Ok(QuotaDigest(Sha256::digest(bytes).into()))
}

/// Validate an archive record after resolving quota references. This checks
/// structure and facts only; it neither evaluates policy nor compares outcomes.
/// A well-formed retained refusal is valid, even though replay returns its reason.
pub fn validate_record(record: &PermitRecord) -> Result<(), WireRefusal> {
    if record.schema != SCHEMA {
        return Err(WireRefusal::Schema);
    }
    bounded(record)?;
    let mut actual = record.actual.clone();
    remap_actual(&mut actual, &mut |kind, id| {
        wire_id(kind, id)?;
        Ok(id.into())
    })?;
    match (&record.inputs, record.refusal) {
        (Some(inputs), None) => inputs.to_inputs().map(|_| ()),
        (None, Some(_)) => Ok(()),
        _ => Err(WireRefusal::Shape),
    }
}

pub fn replay(record: &PermitRecord) -> Result<PermitDecision, WireRefusal> {
    validate_record(record)?;
    match (&record.inputs, record.refusal) {
        (Some(inputs), None) => Ok(kernel::decide_move(&inputs.to_inputs()?)),
        (None, Some(reason)) => Err(reason),
        _ => Err(WireRefusal::Shape),
    }
}

#[cfg(test)]
#[path = "permit_tape/tests.rs"]
mod tests;
