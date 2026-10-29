//! Conservative syntax boundary for the whole decision kernel, including the
//! raw helpers, permit and selection family. This is a closed allowlist, not a proof of
//! arbitrary Rust. The readset check below closes the captured-value boundary;
//! native differential tests separately establish behavior.
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use syn::{
    Expr, Item, Pat,
    visit::{self, Visit},
};

const SOURCE: &str = include_str!("../src/bindings/kernel.rs");
const PERMIT_SOURCE: &str = include_str!("../src/bindings/kernel/permit.rs");
const HEALTH_SOURCE: &str = include_str!("../src/accounts/health.rs");
const CREDITS_SOURCE: &str = include_str!("../src/credits.rs");
const HISTORY_SOURCE: &str = include_str!("../src/dashboard/history.rs");
const ACCOUNTS_SOURCE: &str = include_str!("../src/accounts/mod.rs");
const MOVE_GUARD_SOURCE: &str = include_str!("../src/bindings/move_guard.rs");

fn tokens(value: &impl ToTokens) -> String {
    value.to_token_stream().to_string()
}

fn compact(value: &str) -> String {
    value.chars().filter(|c| !c.is_whitespace()).collect()
}

const IMPORTS: &[&str] = &[
    "use crate::accounts::{Account, Health, HealthSignal};",
    "use chrono::{DateTime, Utc};",
    "use serde::{Deserialize, Serialize};",
    "use serde_json::Value;",
    "use std::collections::{BTreeMap, BTreeSet};",
    "pub use permit::{PermitDecision, PermitDenied, PermitInputs, PreparedMove, decide_move};",
    "pub use permit::{decide_move, PermitDecision, PermitDenied, PermitInputs, PreparedMove};",
    "use sha2::{Digest, Sha256};",
    "pub use selection::{SelectionInputs, SelectionReturnInput};",
];

const PERMIT_IMPORTS: &[&str] = &[
    "use chrono::{DateTime, Utc};",
    "use serde::{Deserialize, Serialize};",
    "use super::super::move_guard::{GLOBAL_RECORDS, Limits, RECORDS, Refusal, SESSIONS};",
    "use chrono::Duration;",
];

const PERMIT_PATHS: &[&str] = &[
    "Some",
    "None",
    "PermitDecision::NotRequired",
    "PermitDecision::NotComparable",
    "PermitDecision::Denied",
    "PermitDecision::Allowed",
    "Refusal::Capacity",
    "Refusal::GlobalTrip",
    "Refusal::PingPong",
    "Refusal::SessionTenMinutes",
    "Refusal::SessionHour",
    "Duration::minutes",
    "Duration::seconds",
    "Duration::hours",
    "super::effective_account",
    "super::accepted_in",
    "super::semantic_evidence",
    "crate::accounts::health::UnusableEvidence::observe",
    "u64::MAX",
    "GLOBAL_RECORDS",
    "RECORDS",
    "SESSIONS",
];

const PERMIT_METHODS: &[&str] = &[
    "as_deref",
    "filter",
    "as_ref",
    "clone",
    "bounded",
    "is_empty",
    "len",
    "is_some_and",
    "applies_to",
    "is_some",
    "then",
    "to_owned",
    "iter",
    "any",
    "last",
    "rev",
    "find",
    "collect",
    "into",
];

const PERMIT_TYPES: &[&str] = &[
    "DateTime",
    "Utc",
    "String",
    "u64",
    "i64",
    "usize",
    "bool",
    "Option",
    "Vec",
    "Limits",
    "Refusal",
    "MovementInput",
    "IncidentInput",
    "SoleTargetInput",
    "PermitInputs",
    "PreparedMove",
    "PermitDenied",
    "PermitDecision",
    "super::DecisionInputs",
];

#[derive(Default)]
struct Locals(BTreeSet<String>);
impl<'ast> Visit<'ast> for Locals {
    fn visit_pat(&mut self, pat: &'ast Pat) {
        if let Pat::Ident(pat) = pat {
            self.0.insert(pat.ident.to_string());
        }
        visit::visit_pat(self, pat);
    }
}

struct Boundary {
    locals: BTreeSet<String>,
    errors: Vec<String>,
    permit: bool,
}

impl<'ast> Visit<'ast> for Boundary {
    fn visit_item(&mut self, item: &'ast Item) {
        // Nested functions/modules/statics can otherwise hide an owner read.
        if !matches!(item, Item::Use(_)) {
            self.errors.push("nested item / mutable state".into());
            return;
        }
        visit::visit_item(self, item);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let imports = if self.permit { PERMIT_IMPORTS } else { IMPORTS };
        if !imports
            .iter()
            .any(|allowed| compact(allowed) == compact(&tokens(item)))
        {
            self.errors
                .push(format!("external import: {}", tokens(item)));
        }
    }

    fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
        if self.permit {
            let name = ty
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            if ty.qself.is_some()
                || ty.path.leading_colon.is_some()
                || !PERMIT_TYPES.contains(&name.as_str())
            {
                self.errors
                    .push(format!("external permit type: {}", tokens(ty)));
            }
        }
        visit::visit_type_path(self, ty);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Field(field) if matches!(&field.member, syn::Member::Named(name) if name == "data") =>
            {
                self.errors.push("owner data read".into());
            }
            Expr::Path(path) => {
                let path = compact(&tokens(&path.path));
                let allowed = [
                    "Some",
                    "None",
                    "date",
                    "group",
                    "effective_account",
                    "accepted_in",
                    "stale_window",
                    "Health::Ready",
                    "Value::Null",
                    "serde_json::to_vec",
                    "Sha256::digest",
                    "u64::from_be_bytes",
                    "DateTime::parse_from_rfc3339",
                    "str::to_owned",
                    "Utc",
                ];
                let allowed: &[&str] = if self.permit { PERMIT_PATHS } else { &allowed };
                if !self.locals.contains(&path) && !allowed.contains(&path.as_str()) {
                    self.errors
                        .push(format!("external value / native getter: {path}"));
                }
            }
            Expr::MethodCall(call) => {
                let allowed = [
                    "get",
                    "as_deref",
                    "clone",
                    "eligible_at",
                    "as_array_mut",
                    "retain",
                    "apply_health_signal",
                    "is_some_and",
                    "num_seconds",
                    "copied",
                    "unwrap_or",
                    "max",
                    "is_none",
                    "contains",
                    "values",
                    "any",
                    "and_then",
                    "iter",
                    "as_ref",
                    "map",
                    "expect",
                    "try_into",
                    "unwrap",
                    "starts_with",
                    "split_once",
                    "map_or",
                    "as_str",
                    "ok",
                    "with_timezone",
                ];
                let allowed: &[&str] = if self.permit {
                    PERMIT_METHODS
                } else {
                    &allowed
                };
                if !allowed.contains(&call.method.to_string().as_str()) {
                    self.errors
                        .push(format!("unreviewed method / live getter: {}", call.method));
                }
                // These two methods intentionally cross into reviewed native
                // value semantics; pin their receivers rather than allowing
                // arbitrary types with the same method names.
                if self.permit
                    && call.method == "bounded"
                    && compact(&tokens(call)) != "permit.limits.clone().bounded()"
                {
                    self.errors.push("non-input Limits receiver".into());
                }
                if self.permit
                    && call.method == "applies_to"
                    && compact(&tokens(call)) != "proof.applies_to(from,inputs.now)"
                {
                    self.errors.push("non-account health proof receiver".into());
                }
            }
            Expr::Call(call) if !matches!(&*call.func, Expr::Path(_)) => {
                self.errors.push("indirect call".into());
            }
            Expr::Struct(value) => {
                let path = compact(&tokens(&value.path));
                let allowed: &[&str] = if self.permit {
                    &["PermitDenied", "PreparedMove"]
                } else {
                    &["HealthSignal::UsageSnapshot"]
                };
                if !allowed.contains(&path.as_str()) {
                    self.errors.push("external construction".into());
                }
            }
            Expr::Macro(_) | Expr::Unsafe(_) => self.errors.push("macro / unsafe read".into()),
            _ => {}
        }
        visit::visit_expr(self, expr);
    }
}

fn guard(source: &str) -> Vec<String> {
    let file = syn::parse_file(source).expect("kernel syntax");
    let mut errors = Vec::new();
    let signatures = [
        "pub fn stale_window(inputs: &DecisionInputs, id: &str, window: &Value) -> Option<bool>",
        "pub fn effective_account(inputs: &DecisionInputs, id: &str) -> Option<Account>",
        "pub fn accepted_in(inputs: &DecisionInputs, id: &str) -> bool",
        "pub fn semantic_evidence(inputs: &DecisionInputs, id: Option<&str>) -> u64",
        "pub fn move_source_usable(inputs: &DecisionInputs, id: &str) -> bool",
        "fn group(key: &str) -> &str",
        "fn date(value: &Value) -> Option<DateTime<Utc>>",
    ];
    let structs = [
        (
            "CandidateInput",
            "pub account: Account, pub quota: Option<Value>, pub inflight: usize,",
        ),
        (
            "AcceptanceGroup",
            "pub account: String, pub keys: BTreeSet<String>,",
        ),
        ("AcceptanceBinding", "pub accepted: Option<bool>,"),
        (
            "DecisionInputs",
            "pub now: DateTime<Utc>, pub model: Option<String>, pub key: Option<String>, pub from: Option<String>, pub to: String, pub accounts: BTreeMap<String, CandidateInput>, pub traffic: BTreeMap<String, DateTime<Utc>>, pub pending: BTreeMap<String, usize>, pub groups: BTreeMap<String, AcceptanceGroup>, pub bindings: BTreeMap<String, AcceptanceBinding>, pub accepted_unbound: BTreeSet<String>, pub sticky: BTreeMap<String, String>, pub outside_failure: bool, pub permit: Option<PermitInputs>, pub selection: Option<Box<SelectionInputs>>,",
        ),
    ];
    let mut seen = BTreeSet::new();
    for item in file.items {
        match item {
            Item::Use(item) => {
                let mut boundary = Boundary {
                    locals: BTreeSet::new(),
                    errors: Vec::new(),
                    permit: false,
                };
                boundary.visit_item_use(&item);
                errors.extend(boundary.errors);
            }
            Item::Fn(mut item)
                if matches!(
                    item.sig.ident.to_string().as_str(),
                    "facts_with_model" | "helper_facts" | "candidate_facts"
                ) =>
            {
                seen.insert(item.sig.ident.to_string());
                // Closed bridges: pin bodies as well as signatures. A native
                // fallback hidden behind an otherwise pure name must fail.
                item.attrs.retain(|a| !a.path().is_ident("doc"));
                let expected = syn::parse_file(SELECTION_BRIDGES).expect("bridge syntax");
                if !expected
                    .items
                    .iter()
                    .any(|f| compact(&tokens(f)) == compact(&tokens(&item)))
                {
                    errors.push("changed selection facts bridge".into());
                }
            }
            Item::Fn(item) => {
                let signature = compact(&(tokens(&item.vis) + &tokens(&item.sig)));
                if !signatures
                    .iter()
                    .any(|expected| compact(expected) == signature)
                {
                    errors.push(format!("non-input-only signature: {signature}"));
                }
                seen.insert(item.sig.ident.to_string());
                let mut locals = Locals::default();
                locals.visit_item_fn(&item);
                let mut boundary = Boundary {
                    locals: locals.0,
                    errors: Vec::new(),
                    permit: false,
                };
                boundary.visit_block(&item.block);
                errors.extend(boundary.errors);
            }
            Item::Struct(item) => {
                let fields = item
                    .fields
                    .iter()
                    .map(|field| {
                        let mut field = field.clone();
                        field.attrs.clear();
                        tokens(&field)
                    })
                    .collect::<Vec<_>>()
                    .join(",")
                    + ",";
                if !structs.iter().any(|(name, expected)| {
                    item.ident == *name && compact(expected) == compact(&fields)
                }) {
                    errors.push(format!("non-raw input fields: {}", item.ident));
                }
            }
            Item::Mod(item)
                if matches!(
                    item.ident.to_string().as_str(),
                    "permit" | "selection_affinity" | "selection_metrics"
                ) && item.content.is_none()
                    && item.attrs.is_empty()
                    && compact(&tokens(&item.vis)) == "pub(super)" => {}
            Item::Mod(item)
                if item.ident == "selection"
                    && item.content.is_none()
                    && item.attrs.is_empty()
                    && compact(&tokens(&item.vis)) == "pub(crate)" => {}
            Item::Mod(item)
                if item.ident == "raw_tests"
                    && item.content.is_none()
                    && item.attrs.len() == 2
                    && item
                        .attrs
                        .iter()
                        .any(|a| compact(&tokens(a)) == "#[cfg(test)]")
                    && item
                        .attrs
                        .iter()
                        .any(|a| compact(&tokens(a)) == "#[path=\"kernel/raw_tests.rs\"]") => {}
            _ => errors.push("external module / state / item".into()),
        }
    }
    // facts_with_model is an optional unused convenience wrapper; when present,
    // its complete signature/body still must match the reviewed bridge above.
    for name in ["helper_facts", "candidate_facts"] {
        if !seen.contains(name) {
            errors.push(format!("missing selection facts bridge: {name}"));
        }
    }
    for signature in signatures {
        let name = signature
            .split("fn ")
            .nth(1)
            .unwrap()
            .split('(')
            .next()
            .unwrap();
        if !seen.contains(name) {
            errors.push(format!("missing helper: {name}"));
        }
    }
    errors.extend(kernel_readset(source, false));
    errors
}

fn permit_guard(source: &str) -> Vec<String> {
    let file = syn::parse_file(source).expect("permit syntax");
    let structs = [
        (
            "MovementInput",
            "pub at: DateTime<Utc>, pub committed_age_ns: u64, pub from: String, pub departure_evidence: u64, pub to: String, pub destination_evidence: u64, pub emergency_return: bool,",
        ),
        (
            "IncidentInput",
            "pub at: DateTime<Utc>, pub shared: Option<(String, u64)>,",
        ),
        (
            "SoleTargetInput",
            "pub from: String, pub to: String, pub model: Option<String>, pub at: DateTime<Utc>,",
        ),
        (
            "PermitInputs",
            "pub limits: Limits, pub now: DateTime<Utc>, pub history: Vec<MovementInput>, pub global: Vec<IncidentInput>, pub session_count: usize, pub session_exists: bool, pub tripped_until: Option<DateTime<Utc>>, pub revision: u64, pub source_observation_age_ns: Option<u64>, pub target_observation_age_ns: Option<u64>, pub sole_target: Option<SoleTargetInput>,",
        ),
        (
            "PreparedMove",
            "pub revision: u64, pub shared_incident: Option<(String, u64)>, pub key: String, pub at: DateTime<Utc>, pub from: String, pub to: String, pub departure_evidence: u64, pub destination_evidence: u64, pub emergency_return: bool,",
        ),
        (
            "PermitDenied",
            "pub reason: Refusal, pub until: DateTime<Utc>, pub newly_tripped: bool,",
        ),
    ];
    let mut boundary = Boundary {
        locals: BTreeSet::new(),
        errors: Vec::new(),
        permit: true,
    };
    let mut decisions = 0;
    let mut seen = BTreeSet::new();
    for item in file.items {
        match item {
            Item::Use(item) => boundary.visit_item_use(&item),
            Item::Struct(item) => {
                let fields = item
                    .fields
                    .iter()
                    .map(|field| {
                        let mut field = field.clone();
                        field.attrs.clear();
                        tokens(&field)
                    })
                    .collect::<Vec<_>>()
                    .join(",")
                    + ",";
                if !structs.iter().any(|(name, expected)| {
                    item.ident == *name && compact(expected) == compact(&fields)
                }) || !item.generics.params.is_empty()
                    || item.generics.where_clause.is_some()
                {
                    boundary
                        .errors
                        .push(format!("non-raw permit fields: {}", item.ident));
                }
                if !seen.insert(item.ident.to_string()) {
                    boundary.errors.push("duplicate permit type".into());
                }
                boundary.visit_item_struct(&item);
            }
            Item::Enum(item) if item.ident == "PermitDecision" => {
                let variants = item
                    .variants
                    .iter()
                    .map(tokens)
                    .collect::<Vec<_>>()
                    .join(",")
                    + ",";
                if compact(&variants)
                    != compact(
                        "NotRequired, Allowed(PreparedMove), Denied(PermitDenied), NotComparable,",
                    )
                    || !item.generics.params.is_empty()
                    || item.generics.where_clause.is_some()
                {
                    boundary
                        .errors
                        .push("non-value permit decision type".into());
                }
                if !seen.insert(item.ident.to_string()) {
                    boundary.errors.push("duplicate permit type".into());
                }
                boundary.visit_item_enum(&item);
            }
            Item::Fn(item) => {
                decisions += 1;
                let signature = compact(&(tokens(&item.vis) + &tokens(&item.sig)));
                if signature
                    != compact(
                        "pub fn decide_move(inputs: &super::DecisionInputs) -> PermitDecision",
                    )
                {
                    boundary
                        .errors
                        .push(format!("non-input-only permit signature: {signature}"));
                }
                let mut locals = Locals::default();
                locals.visit_item_fn(&item);
                boundary.locals = locals.0;
                boundary.visit_signature(&item.sig);
                boundary.visit_block(&item.block);
            }
            _ => boundary
                .errors
                .push("external permit module / state / item".into()),
        }
    }
    if decisions != 1 {
        boundary
            .errors
            .push("require exactly one decide_move".into());
    }
    for name in structs
        .iter()
        .map(|(name, _)| *name)
        .chain(["PermitDecision"])
    {
        if !seen.contains(name) {
            boundary.errors.push(format!("missing permit type: {name}"));
        }
    }
    boundary.errors.extend(kernel_readset(source, true));
    boundary.errors
}

#[test]
fn permit_kernel_has_only_captured_inputs_and_native_value_semantics() {
    assert_eq!(permit_guard(PERMIT_SOURCE), Vec::<String>::new());
}

#[test]
fn planted_permit_owner_reads_and_getters_fail_the_guard() {
    for planted in [
        "let now = OWNER.now;",
        "let now = inputs.data.now;",
        "let now = super::super::live_now();",
        "let now = chrono::Utc::now();",
        "let now = permit.live_now();",
        "let now = permit.now; use super::super::Data;",
        "let now = permit.now; static mut OWNER: bool = false;",
        "let now = super::effective_account_live(inputs, from);",
    ] {
        let needle = "let now = permit.now;";
        assert!(PERMIT_SOURCE.contains(needle), "stale permit read fixture");
        let fixture = PERMIT_SOURCE.replacen(needle, planted, 1);
        assert!(
            !permit_guard(&fixture).is_empty(),
            "planted permit violation escaped: {planted}"
        );
    }
    let needle = "pub fn decide_move(inputs: &super::DecisionInputs)";
    assert!(
        PERMIT_SOURCE.contains(needle),
        "stale permit signature fixture"
    );
    let fixture = PERMIT_SOURCE.replacen(
        needle,
        "pub fn decide_move(inputs: &super::DecisionInputs, data: &super::super::Data)",
        1,
    );
    assert!(!permit_guard(&fixture).is_empty());
    let fixture = PERMIT_SOURCE.replacen(
        "permit.limits.clone().bounded()",
        "permit.other.clone().bounded()",
        1,
    );
    assert_ne!(fixture, PERMIT_SOURCE, "stale bounded Limits fixture");
    assert!(!permit_guard(&fixture).is_empty());
}

#[test]
fn raw_kernel_has_no_owner_state_or_live_getters() {
    assert_eq!(guard(SOURCE), Vec::<String>::new());
}

#[test]
fn planted_external_reads_and_computed_inputs_fail_the_guard() {
    for (needle, planted) in [
        (
            "let c = inputs.accounts.get(id)?;",
            "let c = super::live_candidate(id)?;",
        ),
        (
            "let c = inputs.accounts.get(id)?;",
            "let c = OWNER.accounts.get(id)?;",
        ),
        (
            "let c = inputs.accounts.get(id)?;",
            "let c = inputs.accounts.live_get(id)?;",
        ),
        (
            "let c = inputs.accounts.get(id)?;",
            "let c = inputs.data.get(id)?;",
        ),
        (
            "pub fn accepted_in(inputs: &DecisionInputs, id: &str)",
            "pub fn accepted_in(inputs: &DecisionInputs, data: &Data, id: &str)",
        ),
        (
            "pub inflight: usize,",
            "pub inflight: usize, pub eligible: bool,",
        ),
        (
            "use serde_json::Value;",
            "use serde_json::Value; use super::Data;",
        ),
        (
            "use serde_json::Value;",
            "use serde_json::Value; static mut OWNER: bool = false;",
        ),
    ] {
        assert!(SOURCE.contains(needle), "stale negative fixture: {needle}");
        let fixture = SOURCE.replacen(needle, planted, 1);
        assert!(
            !guard(&fixture).is_empty(),
            "planted violation escaped: {planted}"
        );
    }
}

#[test]
fn planted_unrecorded_account_label_read_fails_the_guard() {
    let needle = "let c = inputs.accounts.get(id)?;";
    assert!(SOURCE.contains(needle), "stale label read fixture");
    let fixture = SOURCE.replacen(
        needle,
        "let c = inputs.accounts.get(id)?; if c.account.label.as_str() == \"unrecorded\" { return Some(false); }",
        1,
    );
    assert!(
        !guard(&fixture).is_empty(),
        "unrecorded_account_label_read_escaped_guard"
    );
}

// A small abstract interpreter, not a Rust type checker. Unknown shapes, calls,
// fields, patterns and JSON probes fail closed. References preserve shape; local
// aliases and closure arguments therefore cannot turn Account/Value into an
// unrestricted scalar. No production implementation is copied or body-pinned.
#[derive(Clone, PartialEq, Eq)]
enum Shape {
    Scalar,
    JsonScalar,
    Object(&'static str),
    Optional(Box<Shape>),
    Rows(Box<Shape>),
    Map(Box<Shape>),
    Tuple(Vec<Shape>),
    Closure(String, BTreeMap<String, Shape>),
    Unknown,
}

impl Shape {
    fn optional(self) -> Self {
        Self::Optional(Box::new(self))
    }

    fn rows(self) -> Self {
        Self::Rows(Box::new(self))
    }

    fn inner(&self) -> Self {
        match self {
            Self::Optional(s) | Self::Rows(s) | Self::Map(s) => *s.clone(),
            _ => Self::Unknown,
        }
    }

    fn bulk(&self) -> bool {
        match self {
            Self::Object(
                "Account" | "CandidateInput" | "DecisionInputs" | "Quota" | "NormalizedQuota"
                | "Window" | "Credits" | "Signals",
            ) => true,
            Self::Optional(s) | Self::Rows(s) | Self::Map(s) => s.bulk(),
            Self::Tuple(s) => s.iter().any(Self::bulk),
            Self::Unknown | Self::Closure(_, _) => true,
            _ => false,
        }
    }
}

const ACCOUNT_FIELDS: &[&str] = &[
    "id",
    "generation",
    "provider",
    "plan",
    "user_state",
    "health",
    "auth_failures",
    "payment_failures",
    "payment_until",
    "auth_probe_pending",
    "quota_cooling",
    "quota_credit_releasable",
    "drain",
    "allow_credits",
];
const INPUT_FIELDS: &[&str] = &[
    "now",
    "model",
    "key",
    "from",
    "to",
    "accounts",
    "traffic",
    "pending",
    "groups",
    "bindings",
    "accepted_unbound",
    "sticky",
    "outside_failure",
    "permit",
];
const PERMIT_FIELDS: &[&str] = &[
    "limits",
    "now",
    "history",
    "global",
    "session_count",
    "session_exists",
    "tripped_until",
    "revision",
    "source_observation_age_ns",
    "target_observation_age_ns",
    "sole_target",
];

fn field_shape(base: &Shape, member: &syn::Member) -> Shape {
    use Shape::{Map, Object, Scalar, Tuple, Unknown};
    if let (Tuple(parts), syn::Member::Unnamed(n)) = (base, member) {
        return parts.get(n.index as usize).cloned().unwrap_or(Unknown);
    }
    let syn::Member::Named(name) = member else {
        return Unknown;
    };
    let name = name.to_string();
    match (base, name.as_str()) {
        (Object("Account"), "health") => Object("Health"),
        (Object("Account"), "quota_cooling") => Object("QuotaCooling").optional(),
        (Object("Account"), "plan" | "payment_until") => Scalar.optional(),
        (Object("Account"), n) if ACCOUNT_FIELDS.contains(&n) => Scalar,
        (Object("CandidateInput"), "account") => Object("Account"),
        (Object("CandidateInput"), "quota") => Object("Quota").optional(),
        (Object("CandidateInput"), "inflight") => Scalar,
        (Object("DecisionInputs"), "accounts") => Map(Box::new(Object("CandidateInput"))),
        (Object("DecisionInputs"), "groups") => Map(Box::new(Object("AcceptanceGroup"))),
        (Object("DecisionInputs"), "bindings") => Map(Box::new(Object("AcceptanceBinding"))),
        (Object("DecisionInputs"), "traffic" | "pending" | "sticky") => Map(Box::new(Scalar)),
        (Object("DecisionInputs"), "permit") => Object("PermitInputs").optional(),
        (Object("DecisionInputs"), "model" | "key" | "from") => Scalar.optional(),
        (Object("DecisionInputs"), "accepted_unbound") => Scalar.rows(),
        (Object("DecisionInputs"), n) if INPUT_FIELDS.contains(&n) => Scalar,
        (Object("PermitInputs"), "limits") => Object("Limits"),
        (Object("PermitInputs"), "history") => Object("MovementInput").rows(),
        (Object("PermitInputs"), "global") => Object("IncidentInput").rows(),
        (Object("PermitInputs"), "sole_target") => Object("SoleTargetInput").optional(),
        (
            Object("PermitInputs"),
            "tripped_until" | "source_observation_age_ns" | "target_observation_age_ns",
        ) => Scalar.optional(),
        (Object("PermitInputs"), n) if PERMIT_FIELDS.contains(&n) => Scalar,
        (Object("AcceptanceGroup"), "account") => Scalar,
        (Object("AcceptanceGroup"), "keys") => Scalar.rows(),
        (Object("AcceptanceBinding"), "accepted") => Scalar.optional(),
        (Object("QuotaCooling"), "window" | "reset_at" | "started_at") => Scalar,
        (
            Object("MovementInput"),
            "at"
            | "committed_age_ns"
            | "from"
            | "departure_evidence"
            | "to"
            | "destination_evidence"
            | "emergency_return",
        ) => Scalar,
        (Object("IncidentInput"), "at") => Scalar,
        (Object("IncidentInput"), "shared") => Tuple(vec![Scalar, Scalar]).optional(),
        (Object("SoleTargetInput"), "model") => Scalar.optional(),
        (Object("SoleTargetInput"), "from" | "to" | "at") => Scalar,
        (
            Object("Limits"),
            "global_ten_minutes" | "trip_seconds" | "session_ten_minutes" | "session_hour",
        ) => Scalar,
        (Object("UnusableEvidence"), "account" | "at") => Scalar,
        (Object("UnusableEvidence"), "model") => Scalar.optional(),
        _ => Unknown,
    }
}

// Signals are a synthetic Null at the only permitted UsageSnapshot call. They
// are NOT captured quota facts. Kernel reads are forbidden; the one reset write
// and native reads of this normalized value are checked separately below.
fn json_key(base: &Shape, key: &str, native: bool) -> Shape {
    use Shape::{JsonScalar, Object, Unknown};
    match (base, key) {
        (Object("Quota" | "NormalizedQuota"), "windows") => Object("Window").rows(),
        (Object("Quota" | "NormalizedQuota"), "credits") => Object("Credits"),
        (Object("Quota" | "NormalizedQuota"), "paid_serving_observed_at") => JsonScalar,
        (Object("NormalizedQuota"), "signals") if native => Object("Signals"),
        (Object("Window"), "name" | "used_pct" | "status" | "reset_at" | "observed_at") => {
            JsonScalar
        }
        (Object("Credits"), "has_credits" | "unlimited" | "balance") => JsonScalar,
        (
            Object("Signals"),
            "active_limit"
            | "representative_claim"
            | "serving_from_credits"
            | "unified_status"
            | "limit_reached",
        ) if native => JsonScalar,
        _ => Unknown,
    }
}

struct Readset {
    values: BTreeMap<String, Shape>,
    callback_depth: usize,
    errors: Vec<String>,
    native: bool,
    owner: &'static str,
}

impl Readset {
    fn reject(&mut self, what: &str) -> Shape {
        self.errors.push(format!("unrecorded readset: {what}"));
        Shape::Unknown
    }

    fn require(&mut self, shape: Shape, what: &str) -> Shape {
        if shape == Shape::Unknown {
            self.reject(what)
        } else {
            shape
        }
    }

    fn bind(&mut self, pat: &Pat, shape: Shape) {
        match pat {
            Pat::Ident(p) => {
                if let Some((_, sub)) = &p.subpat {
                    self.bind(sub, shape.clone());
                }
                self.values.insert(p.ident.to_string(), shape);
            }
            Pat::Type(p) => self.bind(&p.pat, shape),
            Pat::Reference(p) => self.bind(&p.pat, shape),
            Pat::Wild(_) | Pat::Rest(_) | Pat::Lit(_) => {}
            Pat::Tuple(p) => {
                for (i, pat) in p.elems.iter().enumerate() {
                    let part = match &shape {
                        Shape::Tuple(v) => v.get(i).cloned().unwrap_or(Shape::Unknown),
                        _ => Shape::Unknown,
                    };
                    self.bind(pat, part);
                }
            }
            Pat::TupleStruct(p)
                if matches!(compact(&tokens(&p.path)).as_str(), "Some" | "Ok")
                    && p.elems.len() == 1 =>
            {
                self.bind(&p.elems[0], shape.inner());
            }
            Pat::Struct(p) => {
                let path = compact(&tokens(&p.path));
                // Account destructuring uses the same captured vocabulary as
                // field expressions; enum patterns have a finite native schema.
                for f in &p.fields {
                    let part = if matches!(&shape, Shape::Object("Health")) {
                        if matches!(path.as_str(), "Health::Cooling" | "Self::Cooling")
                            && matches!(
                                compact(&tokens(&f.member)).as_str(),
                                "reason" | "until" | "scope"
                            )
                        {
                            Shape::Scalar
                        } else {
                            Shape::Unknown
                        }
                    } else if shape == Shape::Object("UsageSnapshot")
                        && path == "HealthSignal::UsageSnapshot"
                    {
                        match compact(&tokens(&f.member)).as_str() {
                            "snapshot" => Shape::Object("NormalizedQuota"),
                            "model" => Shape::Scalar.optional(),
                            "projected_allowance" => Shape::Scalar,
                            _ => Shape::Unknown,
                        }
                    } else {
                        field_shape(&shape, &f.member)
                    };
                    let part =
                        self.require(part, &format!("pattern {}.{}", path, tokens(&f.member)));
                    self.bind(&f.pat, part);
                }
                if !matches!(
                    path.as_str(),
                    "Account"
                        | "Health::Ready"
                        | "Health::Cooling"
                        | "Health::NeedsRelogin"
                        | "Health::PlanWithoutClaudeCode"
                        | "Self::Cooling"
                        | "HealthSignal::UsageSnapshot"
                ) {
                    self.reject("unreviewed struct pattern");
                }
            }
            Pat::Or(p) => {
                for p in &p.cases {
                    self.bind(p, shape.clone());
                }
            }
            Pat::Path(p)
                if matches!(
                    compact(&tokens(&p.path)).as_str(),
                    "None"
                        | "Health::Ready"
                        | "Health::PlanWithoutClaudeCode"
                        | "Self::Ready"
                        | "HealthReason::Payment"
                        | "HealthReason::Auth"
                        | "HealthReason::Quota"
                        | "HealthReason::Permission"
                        | "HealthReason::Rate"
                ) => {}
            _ => {
                self.reject("unreviewed pattern");
            }
        }
    }

    fn callback(&mut self, expr: &Expr, args: &[Shape]) -> Shape {
        let callable = self.expr(expr);
        match callable {
            Shape::Closure(source, captured) => {
                let closure: syn::ExprClosure = syn::parse_str(&source).expect("closure syntax");
                if closure.inputs.len() != args.len() {
                    return self.reject("callback arity");
                }
                // Resolve the body against its lexical creation environment,
                // never against bindings introduced or shadowed by the caller.
                let saved = std::mem::replace(&mut self.values, captured);
                self.callback_depth += 1;
                for (pat, shape) in closure.inputs.iter().zip(args) {
                    self.bind(pat, shape.clone());
                }
                let result = self.expr(&closure.body);
                self.callback_depth -= 1;
                self.values = saved;
                result
            }
            Shape::Scalar
                if compact(&tokens(expr)) == "str::to_owned" && args == [Shape::Scalar] =>
            {
                Shape::Scalar
            }
            _ => self.reject("unreviewed callback"),
        }
    }

    fn block(&mut self, block: &syn::Block) -> Shape {
        let saved = self.values.clone();
        let mut result = Shape::Scalar;
        for stmt in &block.stmts {
            match stmt {
                syn::Stmt::Local(local) => {
                    let shape = local
                        .init
                        .as_ref()
                        .map(|i| self.expr(&i.expr))
                        .unwrap_or(Shape::Unknown);
                    if let Some(init) = &local.init
                        && let Some((_, diverge)) = &init.diverge
                    {
                        self.expr(diverge);
                    }
                    self.bind(&local.pat, shape);
                }
                syn::Stmt::Expr(expr, semi) => {
                    let shape = self.expr(expr);
                    result = if semi.is_none() { shape } else { Shape::Scalar };
                }
                syn::Stmt::Item(Item::Use(u))
                    if IMPORTS
                        .iter()
                        .chain(PERMIT_IMPORTS)
                        .any(|i| compact(i) == compact(&tokens(u))) => {}
                syn::Stmt::Item(_) => {
                    self.reject("nested native item");
                }
                syn::Stmt::Macro(_) => {
                    self.reject("statement macro");
                }
            }
        }
        self.values = saved;
        result
    }

    fn expr(&mut self, expr: &Expr) -> Shape {
        use Shape::{Object, Scalar, Unknown};
        match expr {
            Expr::Path(p) => {
                if p.qself.is_some() || p.path.leading_colon.is_some() {
                    return self.reject("qualified value escape");
                }
                let path = compact(&tokens(&p.path));
                if let Some(shape) = self.values.get(&path) {
                    return shape.clone();
                }
                if matches!(path.as_str(), "None") {
                    return Scalar.optional();
                }
                if matches!(
                    path.as_str(),
                    "Health::Ready" | "Health::PlanWithoutClaudeCode" | "Self::Ready"
                ) {
                    return Object("Health");
                }
                if matches!(
                    path.as_str(),
                    "UserState::Active"
                        | "Provider::Claude"
                        | "Provider::Codex"
                        | "HealthReason::Payment"
                        | "HealthReason::Auth"
                        | "HealthReason::Quota"
                        | "HealthReason::Permission"
                        | "HealthReason::Rate"
                        | "Value::Null"
                        | "u64::MAX"
                        | "GLOBAL_RECORDS"
                        | "RECORDS"
                        | "SESSIONS"
                        | "PermitDecision::NotRequired"
                        | "PermitDecision::NotComparable"
                        | "str::to_owned"
                        | "Utc"
                ) || path.starts_with("Refusal::")
                {
                    Scalar
                } else {
                    self.reject(&format!("external value {path}"))
                }
            }
            Expr::Lit(_) => Scalar,
            Expr::Reference(e) => self.expr(&e.expr),
            Expr::Paren(e) => self.expr(&e.expr),
            Expr::Group(e) => self.expr(&e.expr),
            Expr::Unary(e) => {
                let shape = self.expr(&e.expr);
                if !matches!(e.op, syn::UnOp::Deref(_)) && shape.bulk() {
                    self.reject("whole value unary observation")
                } else {
                    shape
                }
            }
            Expr::Try(e) => self.expr(&e.expr).inner(),
            Expr::Cast(e) => {
                let s = self.expr(&e.expr);
                if s.bulk() {
                    self.reject("whole value cast")
                } else {
                    Scalar
                }
            }
            Expr::Closure(_) => Shape::Closure(tokens(expr), self.values.clone()),
            Expr::Field(e) => {
                let base = self.expr(&e.base);
                let shape = field_shape(&base, &e.member);
                self.require(shape, &format!("field {}", tokens(expr)))
            }
            Expr::Index(e) => {
                let base = self.expr(&e.expr);
                match base {
                    Shape::Rows(s) => {
                        let index = self.expr(&e.index);
                        if index.bulk() {
                            self.reject("dynamic aggregate index")
                        } else {
                            *s
                        }
                    }
                    _ => {
                        let Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(key),
                            ..
                        }) = &*e.index
                        else {
                            return self.reject("dynamic raw JSON index");
                        };
                        let shape = json_key(&base, &key.value(), self.native);
                        self.require(shape, &format!("JSON key {}", tokens(expr)))
                    }
                }
            }
            Expr::Tuple(e) => Shape::Tuple(e.elems.iter().map(|e| self.expr(e)).collect()),
            Expr::Array(e) => {
                let mut item = None;
                for e in &e.elems {
                    let s = self.expr(e);
                    if item.as_ref().is_some_and(|i| i != &s) {
                        self.reject("mixed array");
                    }
                    item = Some(s);
                }
                item.unwrap_or(Unknown).rows()
            }
            Expr::Range(e) => {
                for e in e.start.iter().chain(e.end.iter()) {
                    let s = self.expr(e);
                    if s.bulk() {
                        self.reject("aggregate range");
                    }
                }
                Scalar
            }
            Expr::Binary(e) => {
                if self.callback_depth > 0
                    && matches!(
                        e.op,
                        syn::BinOp::AddAssign(_)
                            | syn::BinOp::SubAssign(_)
                            | syn::BinOp::MulAssign(_)
                            | syn::BinOp::DivAssign(_)
                            | syn::BinOp::RemAssign(_)
                            | syn::BinOp::BitXorAssign(_)
                            | syn::BinOp::BitAndAssign(_)
                            | syn::BinOp::BitOrAssign(_)
                            | syn::BinOp::ShlAssign(_)
                            | syn::BinOp::ShrAssign(_)
                    )
                {
                    return self.reject("unsupported callback mutation");
                }
                let a = self.expr(&e.left);
                let b = self.expr(&e.right);
                if a.bulk() || b.bulk() {
                    self.reject("whole value binary observation");
                }
                Scalar
            }
            Expr::Assign(e) => {
                if self.callback_depth > 0 {
                    return self.reject("unsupported callback mutation");
                }
                if !self.native && compact(&tokens(expr)) == "snapshot[\"signals\"]=Value::Null" {
                    if self.values.get("snapshot") != Some(&Object("Quota")) {
                        self.reject("non-quota signals reset");
                    }
                    self.values
                        .insert("snapshot".into(), Object("NormalizedQuota"));
                    return Scalar;
                }
                let a = self.expr(&e.left);
                let b = self.expr(&e.right);
                if let Expr::Path(p) = &*e.left {
                    let key = compact(&tokens(&p.path));
                    if a != b {
                        self.reject("alias shape reassignment");
                    }
                    self.values.insert(key, b);
                } else if a.bulk() || b.bulk() {
                    self.reject("whole value store");
                }
                Scalar
            }
            Expr::Block(e) => self.block(&e.block),
            Expr::If(e) => {
                let saved = self.values.clone();
                self.expr(&e.cond);
                let a = self.block(&e.then_branch);
                self.values = saved.clone();
                let b = e.else_branch.as_ref().map(|(_, e)| self.expr(e));
                self.values = saved;
                match b {
                    Some(b) if b != a => self.reject("branch shape mismatch"),
                    Some(_) => a,
                    None => Scalar,
                }
            }
            Expr::Let(e) => {
                let shape = self.expr(&e.expr);
                self.bind(&e.pat, shape);
                Scalar
            }
            Expr::Match(e) => {
                let shape = self.expr(&e.expr);
                let saved = self.values.clone();
                let mut result = None;
                for arm in &e.arms {
                    self.values = saved.clone();
                    self.bind(&arm.pat, shape.clone());
                    if let Some((_, guard)) = &arm.guard {
                        self.expr(guard);
                    }
                    let s = self.expr(&arm.body);
                    if result.as_ref().is_some_and(|r| r != &s) {
                        self.reject("match shape mismatch");
                    }
                    result = Some(s);
                }
                self.values = saved;
                result.unwrap_or(Scalar)
            }
            Expr::Return(e) => {
                if let Some(e) = &e.expr {
                    let returned = self.expr(e);
                    if returned.bulk() {
                        return self.reject("whole value explicit return");
                    }
                }
                Scalar
            }
            Expr::Struct(e) => {
                let path = compact(&tokens(&e.path));
                let shape = match path.as_str() {
                    "Health::Cooling" => Object("Health"),
                    "QuotaCooling" => Object("QuotaCooling"),
                    "Self" if self.owner == "UnusableEvidence" => Object("UnusableEvidence"),
                    "HealthSignal::UsageSnapshot" => Object("UsageSnapshot"),
                    "PermitDenied" | "PreparedMove" => Scalar,
                    _ => return self.reject("whole/unreviewed value construction"),
                };
                if e.rest.is_some() {
                    self.reject("struct update escape");
                }
                for f in &e.fields {
                    let name = compact(&tokens(&f.member));
                    let s = self.expr(&f.expr);
                    let allowed = match &shape {
                        Object("Health") => {
                            matches!(name.as_str(), "reason" | "until" | "scope") && !s.bulk()
                        }
                        Object("QuotaCooling") | Object("UnusableEvidence") => {
                            field_shape(&shape, &f.member) != Unknown && !s.bulk()
                        }
                        Object("UsageSnapshot") => match name.as_str() {
                            "snapshot" => s == Object("NormalizedQuota"),
                            "model" => s == Scalar.optional(),
                            "projected_allowance" => s == Scalar,
                            _ => false,
                        },
                        _ => !s.bulk(),
                    };
                    if !allowed {
                        self.reject("unreviewed constructed field");
                    }
                }
                shape
            }
            Expr::Call(e) => self.call(e),
            Expr::MethodCall(e) => self.method(e),
            // One reviewed formatting seam: captured model text only. Keep
            // arbitrary formatting macros and whole-value Display/Debug closed.
            Expr::Macro(e)
                if self.native
                    && e.mac.path.is_ident("format")
                    && compact(&tokens(&e.mac)) == "format!(\"model:{m}\")" =>
            {
                if self.values.get("m") == Some(&Scalar) {
                    Scalar
                } else {
                    self.reject("non-scalar model format capture")
                }
            }
            Expr::Macro(e) if self.native && e.mac.path.is_ident("matches") => {
                use syn::parse::Parser;
                let parser = |input: syn::parse::ParseStream<'_>| {
                    let value: Expr = input.parse()?;
                    input.parse::<syn::Token![,]>()?;
                    let pat = Pat::parse_multi_with_leading_vert(input)?;
                    let guard = if input.peek(syn::Token![if]) {
                        input.parse::<syn::Token![if]>()?;
                        Some(input.parse::<Expr>()?)
                    } else {
                        None
                    };
                    if input.peek(syn::Token![,]) {
                        input.parse::<syn::Token![,]>()?;
                    }
                    Ok((value, pat, guard))
                };
                match parser.parse2(e.mac.tokens.clone()) {
                    Ok((value, pat, guard)) => {
                        let saved = self.values.clone();
                        let s = self.expr(&value);
                        self.bind(&pat, s);
                        if let Some(g) = guard {
                            self.expr(&g);
                        }
                        self.values = saved;
                    }
                    Err(_) => {
                        self.reject("unreviewed matches syntax");
                    }
                }
                Scalar
            }
            _ => self.reject(&format!("unreviewed expression {}", tokens(expr))),
        }
    }

    fn call(&mut self, call: &syn::ExprCall) -> Shape {
        use Shape::{Object, Scalar};
        let Expr::Path(path) = &*call.func else {
            return self.reject("indirect value escape");
        };
        if path.qself.is_some() || path.path.leading_colon.is_some() {
            return self.reject("qualified callee escape");
        }
        let name = compact(&tokens(&path.path));
        if let Some(Shape::Closure(_, _)) = self.values.get(&name) {
            let args: Vec<_> = call.args.iter().map(|e| self.expr(e)).collect();
            return self.callback(&call.func, &args);
        }
        let args: Vec<_> = call.args.iter().map(|e| self.expr(e)).collect();
        match (name.as_str(), args.as_slice()) {
            ("Some", [s]) => s.clone().optional(),
            (
                "effective_account" | "super::effective_account",
                [Object("DecisionInputs"), Scalar],
            ) => Object("Account").optional(),
            ("accepted_in" | "super::accepted_in", [Object("DecisionInputs"), Scalar]) => Scalar,
            (
                "semantic_evidence" | "super::semantic_evidence",
                [Object("DecisionInputs"), Shape::Optional(s)],
            ) if **s == Scalar => Scalar,
            ("stale_window", [Object("DecisionInputs"), Scalar, Object("Window")]) => {
                Scalar.optional()
            }
            ("date", [Shape::JsonScalar]) => Scalar.optional(),
            ("group", [Scalar]) => Scalar,
            ("timestamp", [Shape::JsonScalar]) if self.native => Scalar.optional(),
            (
                "crate::accounts::health::UnusableEvidence::observe",
                [Object("Account"), Shape::Optional(s), Scalar, Scalar],
            ) if **s == Scalar => Object("UnusableEvidence").optional(),
            ("crate::credits::entitled", [Object("NormalizedQuota")]) if self.native => Scalar,
            ("available_in" | "explicitly_absent", [Object("Credits")]) if self.native => Scalar,
            ("serving_ended", [Object("NormalizedQuota")]) if self.native => Scalar,
            ("available", [a, b, c]) if self.native && [a, b, c].iter().all(|s| !s.bulk()) => {
                Scalar
            }
            ("serde_json::to_vec", [_])
                if !self.native
                    && args
                        == [Shape::Tuple(vec![
                            Scalar,
                            Object("Health"),
                            Scalar.optional(),
                            Scalar,
                            Scalar,
                            Shape::Tuple(vec![Scalar, Scalar]).optional(),
                        ])]
                    && compact(&tokens(call))
                        == "serde_json::to_vec(&(&c.user_state,&c.health,c.payment_until,c.drain,c.allow_credits,reset,))" =>
            {
                Scalar.optional()
            }
            ("DateTime::parse_from_rfc3339", [Scalar]) => Scalar.optional(),
            ("Sha256::digest", [Scalar]) => Scalar.rows(),
            ("u64::from_be_bytes", [Scalar]) => Scalar,
            ("Duration::minutes" | "Duration::seconds" | "Duration::hours", [Scalar]) => Scalar,
            ("crate::dashboard::history::WindowName::try_from", [Scalar]) if self.native => {
                Scalar.optional()
            }
            ("PermitDecision::Denied" | "PermitDecision::Allowed", [Scalar]) => Scalar,
            _ => self.reject(&format!("callee / arguments {}", tokens(call))),
        }
    }

    fn method(&mut self, call: &syn::ExprMethodCall) -> Shape {
        use Shape::{JsonScalar, Object, Scalar};
        let base = self.expr(&call.receiver);
        let name = call.method.to_string();
        if self.callback_depth > 0
            && matches!(
                name.as_str(),
                "apply_health_signal"
                    | "apply_snapshot"
                    | "expire_health"
                    | "expire"
                    | "cool"
                    | "as_array_mut"
                    | "retain"
            )
        {
            return self.reject("unsupported callback mutation");
        }
        // Closure bodies are evaluated with the receiver's item shape below.
        let args: Vec<_> = call.args.iter().map(|e| self.expr(e)).collect();
        match (name.as_str(), &base, args.as_slice()) {
            ("clone", _, []) => base,
            ("as_ref" | "as_deref", Shape::Optional(_), []) => base,
            ("get", Shape::Map(s), [Scalar]) => (*s.clone()).optional(),
            (
                "get",
                Object("Quota" | "NormalizedQuota" | "Window" | "Credits" | "Signals"),
                [_],
            ) => {
                let Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(key),
                    ..
                }) = &call.args[0]
                else {
                    return self.reject("dynamic raw JSON get");
                };
                let s = json_key(&base, &key.value(), self.native);
                self.require(s, "unrecorded raw JSON get").optional()
            }
            ("values", Shape::Map(s), []) => (*s.clone()).rows(),
            ("iter", Shape::Rows(_), []) => base,
            ("last" | "next", Shape::Rows(s), []) => (*s.clone()).optional(),
            ("min", Shape::Rows(s), []) if !s.bulk() => (*s.clone()).optional(),
            ("min_by_key" | "max_by_key", Shape::Rows(s), [_]) => {
                let key = self.callback(&call.args[0], &[*s.clone()]);
                if key.bulk() {
                    self.reject("aggregate ordering key");
                }
                (*s.clone()).optional()
            }
            ("rev" | "copied" | "cloned" | "collect", Shape::Rows(_), []) => base,
            ("copied" | "cloned", Shape::Optional(_), []) => base,
            ("as_array" | "as_array_mut", Shape::Rows(_), []) => base.optional(),
            ("unwrap" | "expect" | "unwrap_or_default", Shape::Optional(s), _)
                if args.iter().all(|s| !s.bulk()) =>
            {
                *s.clone()
            }
            ("unwrap_or", Shape::Optional(s), [other]) if **s == *other => *s.clone(),
            ("is_some" | "is_none", Shape::Optional(_), []) => Scalar,
            ("or", Shape::Optional(_), [other]) if base == *other => base,
            ("or_else", Shape::Optional(_), [_]) => {
                let other = self.callback(&call.args[0], &[]);
                if other == base {
                    base
                } else {
                    self.reject("or_else shape mismatch")
                }
            }
            ("is_empty" | "len", Shape::Rows(_) | Shape::Map(_) | Scalar, []) => Scalar,
            ("contains", Shape::Rows(s), [other]) if **s == *other => Scalar,
            (
                "is_some_and" | "is_none_or" | "map" | "and_then" | "filter",
                Shape::Optional(s),
                [_],
            ) => {
                let result = self.callback(&call.args[0], &[*s.clone()]);
                match name.as_str() {
                    "map" => result.optional(),
                    "and_then" => result,
                    "filter" => base,
                    _ => Scalar,
                }
            }
            ("any" | "filter" | "find" | "retain" | "map" | "filter_map", Shape::Rows(s), [_]) => {
                let result = self.callback(&call.args[0], &[*s.clone()]);
                match name.as_str() {
                    "map" => result.rows(),
                    "filter_map" => result.inner().rows(),
                    "find" => (*s.clone()).optional(),
                    "filter" => base,
                    _ => Scalar,
                }
            }
            ("map_or", Shape::Optional(s), [default, _]) => {
                let result = self.callback(&call.args[1], &[*s.clone()]);
                if result == *default {
                    result
                } else {
                    self.reject("map_or shape mismatch")
                }
            }
            ("then", Scalar, [_]) => self.callback(&call.args[0], &[]).optional(),
            ("eligible_at", Object("Account"), [Shape::Optional(s), Scalar]) if **s == Scalar => {
                Scalar
            }
            ("claude_free_plan", Object("Account"), []) if self.native => Scalar,
            ("expire_health", Object("Account"), [Scalar]) if self.native => Scalar,
            ("expire", Object("Health"), [Scalar]) if self.native => Scalar,
            ("apply_health_signal", Object("Account"), [Object("UsageSnapshot"), Scalar])
                if !self.native =>
            {
                // Preserve the normalization seam, not native method bodies.
                if compact(&tokens(call))
                    != "account.apply_health_signal(HealthSignal::UsageSnapshot{snapshot,model:model.map(str::to_owned),projected_allowance:0.0,},now,)"
                {
                    self.reject("unreviewed UsageSnapshot seam");
                }
                Scalar
            }
            (
                "apply_snapshot",
                Object("Account"),
                [
                    Object("NormalizedQuota"),
                    Shape::Optional(s),
                    Scalar,
                    Scalar,
                ],
            ) if self.native && **s == Scalar => Scalar,
            ("cool", Object("Account"), [Scalar, Scalar, Scalar]) if self.native => Scalar,
            ("applies_to", Object("UnusableEvidence"), [Scalar, Scalar]) => Scalar,
            ("bounded", Object("Limits"), []) => base,
            ("as_str" | "as_bool" | "as_f64", JsonScalar, []) => Scalar.optional(),
            ("as_str", Scalar, []) => Scalar,
            ("split", Scalar, [Scalar]) if self.native => Scalar.rows(),
            ("strip_prefix" | "split_once", Scalar, [Scalar]) => {
                if name == "split_once" {
                    Shape::Tuple(vec![Scalar, Scalar]).optional()
                } else {
                    Scalar.optional()
                }
            }
            ("try_into", Scalar, []) => Scalar.optional(),
            ("num_seconds" | "abs" | "is_finite" | "to_owned" | "into", Scalar, []) => Scalar,
            ("ok" | "is_ok", Shape::Optional(_), []) => {
                if name == "ok" {
                    base
                } else {
                    Scalar
                }
            }
            ("with_timezone", Scalar, [Scalar]) => Scalar,
            ("max" | "min" | "starts_with" | "contains", Scalar, [Scalar]) => Scalar,
            _ => self.reject(&format!("method / receiver {}", tokens(call))),
        }
    }
}

fn check_function(
    block: &syn::Block,
    bindings: &[(&str, Shape)],
    native: bool,
    owner: &'static str,
) -> Vec<String> {
    let mut readset = Readset {
        values: bindings
            .iter()
            .map(|(n, s)| (n.to_string(), s.clone()))
            .collect(),
        callback_depth: 0,
        errors: Vec::new(),
        native,
        owner,
    };
    readset.block(block);
    readset.errors
}

fn kernel_readset(source: &str, permit: bool) -> Vec<String> {
    use Shape::{Object, Scalar};
    let file = syn::parse_file(source).expect("kernel readset syntax");
    let mut errors = Vec::new();
    for item in file.items {
        if let Item::Fn(f) = item {
            let name = f.sig.ident.to_string();
            let bindings = match name.as_str() {
                "stale_window" => vec![
                    ("inputs", Object("DecisionInputs")),
                    ("id", Scalar),
                    ("window", Object("Window")),
                ],
                "effective_account" | "accepted_in" | "move_source_usable" => {
                    vec![("inputs", Object("DecisionInputs")), ("id", Scalar)]
                }
                "semantic_evidence" => vec![
                    ("inputs", Object("DecisionInputs")),
                    ("id", Scalar.optional()),
                ],
                "group" => vec![("key", Scalar)],
                "date" => vec![("value", Shape::JsonScalar)],
                "facts_with_model" | "helper_facts" | "candidate_facts" if !permit => {
                    // guard() pins these complete ASTs to SELECTION_BRIDGES.
                    continue;
                }
                "decide_move" if permit => vec![("inputs", Object("DecisionInputs"))],
                _ => {
                    errors.push("unreviewed kernel readset root".into());
                    continue;
                }
            };
            errors.extend(check_function(&f.block, &bindings, false, "kernel"));
        }
    }
    errors
}

// These are the native entry roots and their complete reachable local callees.
// Adding a call requires an argument/receiver rule AND a checked root here.
// Unselected HealthSignal arms are outside this UsageSnapshot-only boundary.
const HEALTH_ROOTS: &[(&str, &str, &str)] = &[
    (
        "Account",
        "eligible_at",
        "fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool",
    ),
    (
        "Account",
        "claude_free_plan",
        "fn claude_free_plan(&self) -> bool",
    ),
    (
        "Account",
        "expire_health",
        "fn expire_health(&mut self, now: DateTime<Utc>) -> bool",
    ),
    (
        "Account",
        "apply_health_signal",
        "fn apply_health_signal(&mut self, signal: HealthSignal, now: DateTime<Utc>) -> bool",
    ),
    (
        "Account",
        "apply_snapshot",
        "fn apply_snapshot(&mut self, snapshot: &serde_json::Value, model: Option<&str>, _allowance: f64, now: DateTime<Utc>)",
    ),
    (
        "Account",
        "cool",
        "fn cool(&mut self, reason: HealthReason, until: DateTime<Utc>, scope: String) -> bool",
    ),
    (
        "Health",
        "expire",
        "fn expire(&mut self, now: DateTime<Utc>) -> bool",
    ),
    (
        "UnusableEvidence",
        "observe",
        "fn observe(account: &Account, model: Option<&str>, accepted: bool, now: DateTime<Utc>) -> Option<Self>",
    ),
    (
        "UnusableEvidence",
        "applies_to",
        "fn applies_to(&self, account: &str, now: DateTime<Utc>) -> bool",
    ),
    (
        "",
        "timestamp",
        "fn timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>>",
    ),
];
const CREDIT_ROOTS: &[(&str, &str, &str)] = &[
    ("", "entitled", "fn entitled(snapshot: &Value) -> bool"),
    (
        "",
        "available_in",
        "fn available_in(credits: &Value) -> bool",
    ),
    (
        "",
        "available",
        "fn available(has: Option<bool>, unlimited: Option<bool>, balance: Option<f64>) -> bool",
    ),
    (
        "",
        "serving_ended",
        "fn serving_ended(snapshot: &Value) -> bool",
    ),
    (
        "",
        "explicitly_absent",
        "fn explicitly_absent(credits: &Value) -> bool",
    ),
];

fn usage_snapshot_block(block: &syn::Block, errors: &mut Vec<String>) -> syn::Block {
    let mut block = block.clone();
    let mut matches = 0;
    for stmt in &mut block.stmts {
        if let syn::Stmt::Expr(Expr::Match(m), _) = stmt {
            if compact(&tokens(&m.expr)) != "signal" {
                continue;
            }
            matches += 1;
            m.arms.retain(|arm| {
                compact(&tokens(&arm.pat))
                    == "HealthSignal::UsageSnapshot{snapshot,model,projected_allowance,}"
            });
            if m.arms.len() != 1 || m.arms[0].guard.is_some() {
                errors.push("unreviewed UsageSnapshot dispatch".into());
            }
        }
    }
    if matches != 1 {
        errors.push("missing/duplicate UsageSnapshot dispatch".into());
    }
    block
}

fn native_bindings(owner: &'static str, name: &str) -> Vec<(&'static str, Shape)> {
    use Shape::{JsonScalar, Object, Scalar};
    let mut bindings = if owner.is_empty() || name == "observe" {
        vec![]
    } else {
        vec![("self", Object(owner))]
    };
    let mut args = match name {
        "eligible_at" => vec![("model", Scalar.optional()), ("now", Scalar)],
        "claude_free_plan" => vec![],
        "expire" | "expire_health" => vec![("now", Scalar)],
        "apply_health_signal" => vec![("signal", Object("UsageSnapshot")), ("now", Scalar)],
        "apply_snapshot" => vec![
            ("snapshot", Object("NormalizedQuota")),
            ("model", Scalar.optional()),
            ("_allowance", Scalar),
            ("now", Scalar),
        ],
        "cool" => vec![("reason", Scalar), ("until", Scalar), ("scope", Scalar)],
        "observe" => vec![
            ("account", Object("Account")),
            ("model", Scalar.optional()),
            ("accepted", Scalar),
            ("now", Scalar),
        ],
        "applies_to" => vec![("account", Scalar), ("now", Scalar)],
        "timestamp" => vec![("value", JsonScalar)],
        "entitled" | "serving_ended" => vec![("snapshot", Object("NormalizedQuota"))],
        "available_in" | "explicitly_absent" => vec![("credits", Object("Credits"))],
        "available" => vec![
            ("has", Scalar.optional()),
            ("unlimited", Scalar.optional()),
            ("balance", Scalar.optional()),
        ],
        _ => vec![],
    };
    bindings.append(&mut args);
    bindings
}

fn native_file_readset(
    source: &str,
    roots: &[(&'static str, &str, &str)],
    imports: &[&str],
) -> Vec<String> {
    let file = syn::parse_file(source).expect("native readset syntax");
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    let mut functions = Vec::new();
    for item in file.items {
        match item {
            Item::Use(u) => {
                if !imports.iter().any(|i| compact(i) == compact(&tokens(&u))) {
                    errors.push("changed native root imports".into());
                }
            }
            Item::Fn(f) => functions.push((String::new(), f.sig, f.attrs, f.block)),
            Item::Impl(i) => {
                if i.trait_.is_some() {
                    continue;
                }
                let owner = compact(&tokens(&i.self_ty));
                for item in i.items {
                    if let syn::ImplItem::Fn(f) = item {
                        functions.push((owner.clone(), f.sig, f.attrs, Box::new(f.block)));
                    }
                }
            }
            _ => {}
        }
    }
    for (found_owner, sig, attrs, block) in functions {
        let name = sig.ident.to_string();
        let Some((owner, _, expected)) = roots
            .iter()
            .find(|(o, n, _)| *o == found_owner && *n == name)
        else {
            continue;
        };
        if !seen.insert(format!("{owner}::{name}"))
            || attrs.iter().any(|a| !a.path().is_ident("doc"))
            || compact(&tokens(&sig)).replace(",)", ")") != compact(expected).replace(",)", ")")
        {
            errors.push(format!("changed native root signature {owner}::{name}"));
        }
        let selected;
        let block = if name == "apply_health_signal" {
            selected = usage_snapshot_block(&block, &mut errors);
            &selected
        } else {
            &block
        };
        errors.extend(check_function(
            block,
            &native_bindings(owner, &name),
            true,
            owner,
        ));
    }
    for (owner, name, _) in roots {
        if !seen.contains(&format!("{owner}::{name}")) {
            errors.push(format!("missing native root {owner}::{name}"));
        }
    }
    errors
}

fn native_readset(health: &str, credits: &str, history: &str) -> Vec<String> {
    let mut errors = native_file_readset(
        health,
        HEALTH_ROOTS,
        &[
            "use super::{Account, Health, HealthReason, Provider, QuotaCooling, UserState};",
            "use chrono::{DateTime, Duration, Utc};",
        ],
    );
    errors.extend(native_file_readset(
        credits,
        CREDIT_ROOTS,
        &["use serde_json::Value;"],
    ));
    // Bounded trusted seam: pure String -> WindowName validation; it receives
    // neither Account nor raw Value. Pin this small conversion, not health bodies.
    let expected: syn::ItemImpl = syn::parse_str(r#"
        impl TryFrom<String> for WindowName {
            type Error = &'static str;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                if matches!(s.as_str(), "primary" | "secondary") { return Ok(Self(s)); }
                if s.len() >= 2 && s.len() <= 10 && matches!(s.as_bytes().last(), Some(b'm' | b'h' | b'd')) {
                    let n = &s[..s.len() - 1];
                    if n.bytes().all(|b| b.is_ascii_digit()) && n.parse::<u32>().is_ok_and(|n| n > 0) { return Ok(Self(s)); }
                }
                Err("invalid account window")
            }
        }
    "#).expect("WindowName seam syntax");
    let file = syn::parse_file(history).expect("WindowName source syntax");
    let seams: Vec<_> = file
        .items
        .iter()
        .filter_map(|i| {
            if let Item::Impl(i) = i
                && compact(&tokens(&i.self_ty)) == "WindowName"
                && i.trait_
                    .as_ref()
                    .is_some_and(|(_, p, _)| compact(&tokens(p)) == "TryFrom<String>")
            {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    if seams.len() != 1 || compact(&tokens(seams[0])) != compact(&tokens(&expected)) {
        errors.push("changed WindowName trusted seam".into());
    }
    errors
}

fn native_value_seams(accounts: &str, move_guard: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let accounts = syn::parse_file(accounts).expect("native value syntax");
    let mut found = BTreeSet::new();
    for item in accounts.items {
        match item {
            Item::Struct(s) if s.ident == "Account" || s.ident == "QuotaCooling" => {
                found.insert(s.ident.to_string());
                if !s.attrs.iter().any(|a| {
                    compact(&tokens(a))
                        == "#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]"
                }) {
                    errors.push("changed derived native value traits".into());
                }
                if s.attrs
                    .iter()
                    .any(|a| !a.path().is_ident("doc") && !a.path().is_ident("derive"))
                {
                    errors.push("unreviewed native value attribute".into());
                }
                if s.ident == "QuotaCooling"
                    && s.fields
                        .iter()
                        .map(|f| {
                            f.ident
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>()
                        != ["window", "reset_at", "started_at"]
                {
                    errors.push("changed QuotaCooling value fields".into());
                }
            }
            Item::Enum(e) if e.ident == "Health" => {
                found.insert("Health".into());
                if !e.attrs.iter().any(|a| {
                    compact(&tokens(a))
                        == "#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]"
                }) {
                    errors.push("changed derived Health traits".into());
                }
                if e.attrs.iter().any(|a| {
                    !a.path().is_ident("doc")
                        && compact(&tokens(a))
                            != "#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]"
                        && compact(&tokens(a)) != "#[serde(tag=\"kind\",rename_all=\"snake_case\")]"
                }) || e.variants.iter().any(|v| {
                    v.attrs.iter().any(|a| !a.path().is_ident("doc"))
                        || v.fields
                            .iter()
                            .any(|f| f.attrs.iter().any(|a| !a.path().is_ident("doc")))
                }) {
                    errors.push("unreviewed projected Health attribute".into());
                }
                let actual: Vec<_> = e
                    .variants
                    .iter()
                    .map(|v| {
                        (
                            v.ident.to_string(),
                            v.fields
                                .iter()
                                .map(|f| {
                                    f.ident
                                        .as_ref()
                                        .map(ToString::to_string)
                                        .unwrap_or_default()
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                let expected = vec![
                    ("Ready".to_owned(), vec![]),
                    ("PlanWithoutClaudeCode".to_owned(), vec![]),
                    (
                        "Cooling".to_owned(),
                        vec!["reason".to_owned(), "until".to_owned(), "scope".to_owned()],
                    ),
                    (
                        "NeedsRelogin".to_owned(),
                        vec!["reason".to_owned(), "since".to_owned()],
                    ),
                ];
                if actual != expected {
                    errors.push("changed projected Health fields".into());
                }
            }
            _ => {}
        }
    }
    if found
        != [
            "Account".to_owned(),
            "Health".to_owned(),
            "QuotaCooling".to_owned(),
        ]
        .into_iter()
        .collect()
    {
        errors.push("missing derived native value seam".into());
    }
    let file = syn::parse_file(move_guard).expect("Limits seam syntax");
    let expected: syn::ImplItemFn = syn::parse_str(
        r#"
        pub(crate) fn bounded(mut self) -> Self {
            self.session_ten_minutes = self.session_ten_minutes.clamp(1, 16);
            self.session_hour = self.session_hour.clamp(1, 64);
            self.global_ten_minutes = self.global_ten_minutes.clamp(1, 128);
            self.trip_seconds = self.trip_seconds.clamp(1, 3600);
            self.reread_notice_tokens = self.reread_notice_tokens.clamp(1, 1_000_000_000);
            self
        }
    "#,
    )
    .expect("bounded Limits seam syntax");
    let mut bounded = 0;
    for item in file.items {
        if let Item::Impl(i) = item
            && i.trait_.is_none()
            && compact(&tokens(&i.self_ty)) == "Limits"
        {
            for item in i.items {
                if let syn::ImplItem::Fn(f) = item
                    && f.sig.ident == "bounded"
                {
                    bounded += 1;
                    if compact(&tokens(&f)) != compact(&tokens(&expected)) {
                        errors.push("changed bounded Limits trusted seam".into());
                    }
                }
            }
        }
    }
    if bounded != 1 {
        errors.push("missing/duplicate bounded Limits seam".into());
    }
    errors
}

fn plant_after(source: &str, needle: &str, planted: &str) -> String {
    assert!(source.contains(needle), "stale readset fixture: {needle}");
    source.replacen(needle, &format!("{needle}\n{planted}"), 1)
}

fn assert_readset_rejected(errors: Vec<String>, planted: &str) {
    assert!(
        errors.iter().any(|e| e.starts_with("unrecorded readset:")),
        "readset plant escaped or failed for another reason: {planted}: {errors:?}"
    );
}

#[test]
fn captured_account_fields_patterns_and_aliases_are_closed() {
    for planted in [
        "let a = &c.account; let _label = &a.label;",
        "let Account { label, .. } = &c.account;",
        "let a = &c.account; let Account { token_meta: _, .. } = a;",
        "let _future = &c.account.future_unrecorded_field;",
        "let _email = &c.account.email;",
        "let _identity = &c.account.identity;",
        "let _subscription = &c.account.subscription;",
        "let _source = &c.account.source;",
    ] {
        let fixture = plant_after(SOURCE, "let c = inputs.accounts.get(id)?;", planted);
        assert_readset_rejected(guard(&fixture), planted);
    }
    for planted in [
        "let _label = &account.label;",
        "let alias = account; let _email = &alias.email;",
        "let crate::accounts::Account { label, .. } = account;",
    ] {
        let fixture = plant_after(
            PERMIT_SOURCE,
            "let source_unusable = source_account.as_ref().is_some_and(|account| {",
            planted,
        );
        assert_readset_rejected(permit_guard(&fixture), planted);
    }
}

#[test]
fn serialization_accepts_only_the_reviewed_semantic_tuple() {
    for planted in [
        "let _bytes = serde_json::to_vec(&c.account);",
        "let alias = &c.account; let _bytes = serde_json::to_vec(alias);",
        "let _bytes = serde_json::to_vec(c);",
        "let _bytes = serde_json::to_vec(inputs);",
        "let _different = c.account == inputs.accounts.get(id)?.account;",
    ] {
        let fixture = plant_after(SOURCE, "let c = inputs.accounts.get(id)?;", planted);
        assert_readset_rejected(guard(&fixture), planted);
    }
    let fixture = SOURCE.replace("&c.user_state,", "&c,");
    assert_ne!(fixture, SOURCE, "stale semantic tuple fixture");
    assert_readset_rejected(guard(&fixture), "whole account inside tuple");
    let fixture = SOURCE.replacen(
        "let reset = c.quota_cooling.as_ref().map(|q| (&q.window, q.reset_at));",
        "let reset = &c;",
        1,
    );
    assert_ne!(fixture, SOURCE, "stale tuple alias fixture");
    assert_readset_rejected(guard(&fixture), "whole account aliased as reset");
    let fixture = plant_after(
        HEALTH_SOURCE,
        "pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {",
        "let alias = self; let _bytes = serde_json::to_vec(alias);",
    );
    assert_readset_rejected(
        native_readset(&fixture, CREDITS_SOURCE, HISTORY_SOURCE),
        "native alias serialization",
    );
}

#[test]
fn raw_quota_keys_dynamic_probes_and_wrong_levels_are_closed() {
    for planted in [
        "let _unknown = &window[\"unrecorded\"];",
        "let key = \"observed_at\"; let _dynamic = &window[key];",
        "let _unknown = window.get(\"unrecorded\");",
        "let key = \"observed_at\"; let _dynamic = window.get(key);",
        "let alias = window; let _unknown = alias.get(\"unrecorded\");",
        "let _object = window.as_object();",
        "let _whole = window == window;",
        "let _signals = c.quota.as_ref().map(|q| &q[\"signals\"]);",
        "let _wrong_level = c.quota.as_ref().map(|q| &q[\"name\"]);",
        "let _wrong_level = c.quota.as_ref().map(|q| q[\"credits\"][\"name\"].as_str());",
        "let _wrong_origin = [c.quota.as_ref().unwrap()].iter().map(|w| &w[\"name\"]);",
    ] {
        let fixture = plant_after(SOURCE, "let c = inputs.accounts.get(id)?;", planted);
        assert_readset_rejected(guard(&fixture), planted);
    }
    for planted in [
        "let _unknown = &snapshot[\"unrecorded\"];",
        "let key = \"windows\"; let _dynamic = snapshot.get(key);",
        "let _wrong_level = &snapshot[\"name\"];",
        "let alias = snapshot; let _length = alias.as_object().map(|o| o.len());",
    ] {
        let fixture = plant_after(HEALTH_SOURCE, "use serde_json::Value;", planted);
        assert_readset_rejected(
            native_readset(&fixture, CREDITS_SOURCE, HISTORY_SOURCE),
            planted,
        );
    }
}

#[test]
fn native_readset_contains_only_captured_fields_and_closed_callees() {
    assert_eq!(
        native_readset(HEALTH_SOURCE, CREDITS_SOURCE, HISTORY_SOURCE),
        Vec::<String>::new()
    );
    assert_eq!(
        native_value_seams(ACCOUNTS_SOURCE, MOVE_GUARD_SOURCE),
        Vec::<String>::new()
    );
}

#[test]
fn planted_native_fields_patterns_helpers_and_escapes_fail_readset() {
    for (needle, planted) in [
        (
            "pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {",
            "let _escape = format!(\"{:?}\", self);",
        ),
        (
            "use serde_json::Value;",
            "let m = self; let _escape = format!(\"model:{m}\");",
        ),
        (
            "pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {",
            "if self.label.as_str() == \"unrecorded\" { return false; }",
        ),
        (
            "fn claude_free_plan(&self) -> bool {",
            "let a = self; let _email = &a.email;",
        ),
        (
            "pub(crate) fn expire_health(&mut self, now: DateTime<Utc>) -> bool {",
            "let Account { identity: _, .. } = &*self;",
        ),
        (
            "pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {",
            "let _new = self.unreviewed_helper();",
        ),
        (
            "pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {",
            "let _new = unreviewed_helper(self);",
        ),
        (
            "pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {",
            "let a = self; let _new = a.unreviewed_helper();",
        ),
        (
            "pub fn apply_health_signal(&mut self, signal: HealthSignal, now: DateTime<Utc>) -> bool {",
            "let _omitted = &self.subscription;",
        ),
        (
            "self.apply_snapshot(&snapshot, model.as_deref(), projected_allowance, now);",
            "let _omitted = &self.token_meta;",
        ),
        (
            "use serde_json::Value;",
            "let _omitted = &self.plan_observed_at;",
        ),
        (
            "fn cool(&mut self, reason: HealthReason, until: DateTime<Utc>, scope: String) -> bool {",
            "let _omitted = &self.source;",
        ),
        (
            "(!account.eligible_at(model, now) || (account.drain && !accepted)).then(|| Self {",
            "account: { let _omitted = &account.label; account.id.clone() },",
        ),
    ] {
        // The final case replaces rather than duplicates the account field.
        let fixture = if needle.ends_with("then(|| Self {") {
            assert!(HEALTH_SOURCE.contains("account: account.id.clone(),"));
            HEALTH_SOURCE.replacen("account: account.id.clone(),", planted, 1)
        } else {
            plant_after(HEALTH_SOURCE, needle, planted)
        };
        assert_readset_rejected(
            native_readset(&fixture, CREDITS_SOURCE, HISTORY_SOURCE),
            planted,
        );
    }
    let fixture = plant_after(
        HEALTH_SOURCE,
        "pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {",
        "let _new = self.unreviewed_helper();",
    )
    .replacen(
        "impl Account {",
        "impl Account { fn unreviewed_helper(&self) -> bool { self.label == \"unrecorded\" }",
        1,
    );
    assert_readset_rejected(
        native_readset(&fixture, CREDITS_SOURCE, HISTORY_SOURCE),
        "defined but unreviewed helper",
    );
}

#[test]
fn free_rule_behavior_fault_remains_an_allowed_readset_mutation() {
    let needle = "&& !self.claude_free_plan()";
    assert!(
        HEALTH_SOURCE.contains(needle),
        "stale Free-rule fault fixture"
    );
    let fault = HEALTH_SOURCE.replacen(needle, "", 1);
    assert_ne!(fault, HEALTH_SOURCE);
    assert_eq!(
        native_readset(&fault, CREDITS_SOURCE, HISTORY_SOURCE),
        Vec::<String>::new()
    );
}

#[test]
fn usage_snapshot_normalization_and_external_pure_seams_are_closed() {
    let fixture = SOURCE.replacen("snapshot[\"signals\"] = Value::Null;", "", 1);
    assert_ne!(fixture, SOURCE, "stale signals reset fixture");
    assert_readset_rejected(guard(&fixture), "removed signals reset");
    let fixture = CREDITS_SOURCE.replacen(
        "credits[\"balance\"].as_f64()",
        "credits[\"unrecorded\"].as_f64()",
        1,
    );
    assert_ne!(fixture, CREDITS_SOURCE, "stale credit read fixture");
    assert_readset_rejected(
        native_readset(HEALTH_SOURCE, &fixture, HISTORY_SOURCE),
        "credit helper omitted key",
    );
    let fixture = plant_after(
        CREDITS_SOURCE,
        "pub fn available(has: Option<bool>, unlimited: Option<bool>, balance: Option<f64>) -> bool {",
        "let _new = unreviewed_helper();",
    );
    assert_readset_rejected(
        native_readset(HEALTH_SOURCE, &fixture, HISTORY_SOURCE),
        "credit helper new callee",
    );
    let fixture = HISTORY_SOURCE.replacen(
        "Err(\"invalid account window\")",
        "Err(\"changed seam\")",
        1,
    );
    assert_ne!(fixture, HISTORY_SOURCE, "stale WindowName seam fixture");
    assert!(
        native_readset(HEALTH_SOURCE, CREDITS_SOURCE, &fixture)
            .iter()
            .any(|e| e == "changed WindowName trusted seam")
    );
    let fixture = MOVE_GUARD_SOURCE.replacen(
        "self.session_ten_minutes.clamp(1, 16)",
        "unreviewed_helper()",
        1,
    );
    assert_ne!(fixture, MOVE_GUARD_SOURCE, "stale Limits seam fixture");
    assert!(
        native_value_seams(ACCOUNTS_SOURCE, &fixture)
            .iter()
            .any(|e| e == "changed bounded Limits trusted seam")
    );
    let fixture =
        ACCOUNTS_SOURCE.replacen("pub struct Account {", "pub struct RenamedAccount {", 1);
    assert_ne!(fixture, ACCOUNTS_SOURCE, "stale Account clone seam fixture");
    assert!(!native_value_seams(&fixture, MOVE_GUARD_SOURCE).is_empty());
}

#[test]
fn captured_aliases_and_known_quota_probes_remain_allowed() {
    let fixture = plant_after(
        SOURCE,
        "let c = inputs.accounts.get(id)?;",
        "let alias = &c.account; let _id = &alias.id; let quota = c.quota.as_ref(); let _windows = quota.and_then(|q| q.get(\"windows\")).is_none();",
    );
    assert_eq!(guard(&fixture), Vec::<String>::new());
    let fixture = plant_after(
        HEALTH_SOURCE,
        "pub fn eligible_at(&self, model: Option<&str>, now: DateTime<Utc>) -> bool {",
        "let alias = self; let _id = &alias.id;",
    );
    assert_eq!(
        native_readset(&fixture, CREDITS_SOURCE, HISTORY_SOURCE),
        Vec::<String>::new()
    );
}

#[test]
fn planted_explicit_return_closure_account_serialization_fails_readset() {
    let needle = "let reset = c.quota_cooling.as_ref().map(|q| (&q.window, q.reset_at));";
    assert!(
        SOURCE.contains(needle),
        "stale explicit-return reset fixture"
    );
    let fixture = SOURCE.replacen(
        needle,
        "let expose=||{return c;}; let reset=Some((expose(),expose()));",
        1,
    );
    assert!(
        !guard(&fixture).is_empty(),
        "explicit_closure_return_account_serialization_escaped_guard"
    );
}

#[test]
fn planted_shadowed_capture_closure_account_serialization_fails_readset() {
    let needle = "let reset = c.quota_cooling.as_ref().map(|q| (&q.window, q.reset_at));";
    assert!(
        SOURCE.contains(needle),
        "stale shadowed-capture reset fixture"
    );
    let fixture = SOURCE.replacen(
        needle,
        "let captured=c;let expose=||captured;let captured=0;let reset=Some((expose(),expose()));",
        1,
    );
    assert!(
        !guard(&fixture).is_empty(),
        "shadowed_closure_capture_account_serialization_escaped_guard"
    );
}

// Selection uses a separate closed syntax vocabulary. Public entry signatures
// accept DecisionInputs plus scalar/value arguments, never native owners.
// PolicyStore::selection_inputs and PolicyRowsInput::capture are snapshot code
// outside evaluation; their locks are intentionally outside this guard.
const SELECTION_SOURCE: &str = include_str!("../src/bindings/kernel/selection.rs");

const SELECTION_SIGNATURES: &[&str] = &[
    "pub fn select(inputs: &DecisionInputs) -> Result<SelectionEvaluation, SelectionInputError>",
    "fn return_blocked(inputs: &DecisionInputs, row: &CandidateInput) -> bool",
    "fn rebind_budget_source(inputs: &DecisionInputs, to: &str) -> Option<String>",
    "fn select_ordered(inputs: &DecisionInputs, candidates: &[CandidateInput]) -> SelectionEvaluation",
    "fn prefer<T>(pool: &mut Vec<(&CandidateInput, T)>, preference: Option<&PreferenceInput>)",
    "fn prefer_root_hint<T>(pool: &mut Vec<(&CandidateInput, T)>, account: Option<&str>)",
    "fn prefer_pacing_and_evidence( pool: &mut Vec<(&CandidateInput, &m::Metrics)>, inputs: &DecisionInputs, )",
    "fn effective_for(inputs: &DecisionInputs, row: &CandidateInput, model: Option<&str>) -> Account",
    "fn policy_key(inputs: &DecisionInputs) -> Option<&str>",
    "fn account_cmp(inputs: &DecisionInputs, a: &str, b: &str) -> std::cmp::Ordering",
    "fn idle(now: DateTime<Utc>, last: DateTime<Utc>) -> u64",
    "fn protected(inputs: &DecisionInputs, key: &str) -> bool",
    "fn select_ranked( inputs: &DecisionInputs, candidates: &[CandidateInput], plan: &crate::routing::Plan, hinted_account: Option<&str>, diagnostics: &mut Vec<HoldDiagnostic>, ) -> (SelectionChoice, SelectionLabel)",
];

const SELECTION_IMPORTS: &[&str] = &[
    "use super::selection_metrics::MetricInputs;",
    "use super::{CandidateInput, permit::MovementInput, selection_affinity::AffinityInput};",
    "use crate::accounts::Provider;",
    "pub use crate::routing::selection_inputs::SelectionPolicyInput;",
    "use chrono::{DateTime, Utc};",
    "use serde::{Deserialize, Serialize};",
    "use std::collections::{BTreeMap, BTreeSet};",
    "use super::{DecisionInputs, group, selection_affinity as affinity, selection_metrics as m};",
    "use crate::accounts::{Account, Health, HealthReason, UserState};",
    "use std::time::Duration;",
];

const AFFINITY_SOURCE: &str = include_str!("../src/bindings/kernel/selection_affinity.rs");

const AFFINITY_SIGNATURES: &[&str] = &[
    "pub fn retained_binding<'a>( inputs: &'a super::DecisionInputs, key: &str, ) -> Option<&'a BindingInput>",
    "pub fn protected_source( inputs: &super::DecisionInputs, key: Option<&str>, anchor: Option<&str>, ) -> Option<String>",
    "fn idle(now: DateTime<Utc>, last: DateTime<Utc>) -> u64",
    "fn launch_retained(inputs: &AffinityInput, key: &str, now: DateTime<Utc>) -> bool",
    "fn retained<'a>( inputs: &'a AffinityInput, key: &str, now: DateTime<Utc>, ) -> Option<&'a BindingInput>",
    "fn source( inputs: &AffinityInput, key: Option<&str>, anchor: Option<&str>, now: DateTime<Utc>, ) -> Option<String>",
];

const AFFINITY_IMPORTS: &[&str] = &[
    "use crate::bindings::{CAPACITY, launch::IDLE_SECS};",
    "use chrono::{DateTime, Utc};",
    "use serde::{Deserialize, Serialize};",
    "use std::collections::{BTreeMap, BTreeSet};",
];

const METRICS_SOURCE: &str = include_str!("../src/bindings/kernel/selection_metrics.rs");

const METRICS_SIGNATURES: &[&str] = &[
    "pub fn metrics(inputs: &DecisionInputs, id: &str) -> Option<Metrics>",
    "pub(super) fn metrics_for( inputs: &DecisionInputs, c: &CandidateInput, without_charge: bool, ) -> Option<Metrics>",
    "fn placement_state(c: &CandidateInput, raw: &MetricInputs) -> PlacementState",
    "fn evidence( c: &CandidateInput, raw: &MetricInputs, now: DateTime<Utc>, ) -> (PlacementEvidence, usize)",
    "pub(super) fn placement_evidence_for( inputs: &DecisionInputs, c: &CandidateInput, ) -> Option<(PlacementEvidence, usize)>",
    "pub(super) fn placement_capped_for(inputs: &DecisionInputs, c: &CandidateInput) -> Option<bool>",
    "fn activity(raw: &MetricInputs, active: &BTreeSet<String>, id: &str, now: DateTime<Utc>) -> usize",
    "pub fn active_sessions(inputs: &DecisionInputs, id: &str) -> Option<usize>",
    "fn reservations(raw: &MetricInputs, id: &str, except: Option<&str>, now: DateTime<Utc>) -> usize",
    "pub fn reserved(inputs: &DecisionInputs, id: &str, except: Option<&str>) -> Option<usize>",
    "fn threshold(raw: &MetricInputs, id: &str) -> usize",
    "pub fn spread_threshold(inputs: &DecisionInputs, id: &str) -> Option<usize>",
    "pub fn has_overload_mark(inputs: &DecisionInputs, key: &str) -> Option<bool>",
    "pub(super) fn rebalance_target_waiting_for( inputs: &DecisionInputs, source: &str, c: &CandidateInput, except: Option<&str>, ) -> Option<bool>",
    "fn waiting( raw: &MetricInputs, c: &CandidateInput, m: &Metrics, eligible: bool, target: usize, source_count: usize, except: Option<&str>, now: DateTime<Utc>, ) -> bool",
    "pub(super) fn rebalance_target_base_for( inputs: &DecisionInputs, source: &str, c: &CandidateInput, except: Option<&str>, ) -> Option<bool>",
    "pub(super) fn rebalance_target_for( inputs: &DecisionInputs, source: &str, c: &CandidateInput, except: Option<&str>, ) -> Option<bool>",
    "pub fn compare_gated( inputs: &DecisionInputs, a: &str, x: &Metrics, b: &str, y: &Metrics, anchor: Option<DateTime<Utc>>, ) -> Option<Ordering>",
    "fn compare_in_order( order: &[String], a: &str, x: &Metrics, b: &str, y: &Metrics, anchor: Option<DateTime<Utc>>, ) -> Option<Ordering>",
    "fn calculate( c: &CandidateInput, raw: &MetricInputs, cfg: &RoutingConfig, pending: usize, now: DateTime<Utc>, stale: impl Fn(&Value) -> bool, ) -> Metrics",
];

const METRICS_IMPORTS: &[&str] = &[
    "use super::{CandidateInput, DecisionInputs, date};",
    "use crate::bindings::{ PlacementEvidence, PlacementState, PlacementWindow, UsageObservationAsOf, account_window, };",
    "use crate::config::RoutingConfig;",
    "use chrono::{DateTime, Utc};",
    "use serde::{Deserialize, Serialize};",
    "use serde_json::Value;",
    "use std::{ cmp::Ordering, collections::{BTreeMap, BTreeSet}, };",
];

const POLICY_SOURCE: &str = include_str!("../src/routing/selection_inputs.rs");

const POLICY_SIGNATURES: &[&str] = &[
    "fn settings(&self, defaults: &Settings) -> Settings",
    "fn policy_id<'a>(&self, scope: &'a PolicyKeyInput) -> &'a str",
    "fn selected_id(&self) -> &str",
    "pub fn plan(&self) -> Plan",
    "pub fn placement_plan(&self) -> (Plan, bool)",
    "pub fn permits_dispatch(&self, account: &str) -> bool",
    "pub fn discretionary_allowed(&self, target: Option<&str>, now: DateTime<Utc>,) -> Result<bool, PolicyInputRefusal>",
];

const POLICY_IMPORTS: &[&str] = &[
    "use super::{ ChurnLimits, Mode, On5hLimit, Plan, PolicyStore, Settings, State, pool_id, session_id, };",
    "use chrono::{DateTime, Utc};",
    "use serde::{Deserialize, Serialize};",
    "use std::{ collections::{BTreeMap, BTreeSet}, sync::atomic::Ordering, };",
];

const SELECTION_BRIDGES: &str = r#"
pub(super) fn facts_with_model(inputs: &DecisionInputs, model: Option<&str>) -> DecisionInputs {
    helper_facts(inputs, inputs.accounts.clone(), model)
}

fn helper_facts(
    inputs: &DecisionInputs,
    accounts: BTreeMap<String, CandidateInput>,
    model: Option<&str>,
) -> DecisionInputs {
    DecisionInputs {
        now: inputs.now,
        model: model.map(str::to_owned),
        key: inputs.key.clone(),
        from: inputs.from.clone(),
        to: inputs.to.clone(),
        accounts,
        traffic: inputs.traffic.clone(),
        pending: inputs.pending.clone(),
        groups: inputs.groups.clone(),
        bindings: inputs.bindings.clone(),
        accepted_unbound: inputs.accepted_unbound.clone(),
        sticky: inputs.sticky.clone(),
        outside_failure: inputs.outside_failure,
        permit: None,
        selection: None,
    }
}

pub(super) fn candidate_facts(
    inputs: &DecisionInputs,
    candidate: &CandidateInput,
    model: Option<&str>,
) -> DecisionInputs {
    helper_facts(
        inputs,
        BTreeMap::from([(candidate.account.id.clone(), candidate.clone())]),
        model,
    )
}
"#;

// Closed reviewed external value seams: Account health eligibility / evidence
// (checked above), Provider::as_str, routing Settings/Plan/ChurnLimits values,
// RoutingConfig values, account_window, PlacementState::{default, observe,
// weak, count, charge_count}, PlacementWindow and UsageObservationAsOf values,
// and canonical CAPACITY / launch::IDLE_SECS constants. PlacementState's
// methods operate on a locally reconstructed ownerless value. Their transitive
// implementation and std/chrono/serde value semantics remain trusted here;
// this syntax guard does NOT prove purity of infinitely many possible callees.
// No native BindingTable/Data/PolicyStore type is allowed inside evaluation.
const SELECTION_TYPES: &str = "Account AffinityInput BindingInput BTreeMap BTreeSet CandidateInput ChurnLimits DateTime DecisionInputs GroupInput Health HealthReason HoldDiagnostic LaunchInput MetricInputs Metrics Mode MovementInput On5hLimit Option Ordering OverloadInput PlacementAsOfInput PlacementEvidence PlacementInput PlacementState PlacementWindow PlacementWindowInput Plan PolicyInputRefusal PolicyKeyInput PolicyRowsInput PreferenceInput Provider Result RootHintInput RoutingConfig SelectionBinding SelectionChoice SelectionDecision SelectionEntry SelectionEvaluation SelectionEvidence SelectionInputError SelectionInputs SelectionLabel SelectionPolicyInput SelectionReturnInput SessionInput Settings String T UsageObservationAsOf UserState Utc Value Vec bool f64 i64 str u64 usize super::DecisionInputs super::permit::SoleTargetInput m::Metrics crate::routing::Plan std::cmp::Ordering";
const SELECTION_PATHS: &str = "Some None Ok Err self CAPACITY IDLE_SECS Duration::from_secs Health::Cooling HealthReason::Quota SelectionChoice::Hold SelectionChoice::None SelectionChoice::Selected SelectionEntry::Ordered SelectionEntry::ReturnGuard SelectionInputError::AccountOrder SelectionInputError::MissingPool SelectionInputError::MissingReturnHistory SelectionInputError::MissingSelection SelectionInputError::PolicyScope SelectionLabel::ColdRebalance SelectionLabel::Forced SelectionLabel::Inherited SelectionLabel::MarkWait SelectionLabel::New SelectionLabel::Overload UserState::Active Vec::new affinity::protected_source affinity::retained_binding chrono::Duration::minutes chrono::Duration::seconds crate::accounts::health::UnusableEvidence::observe crate::routing::Mode::Rebalance crate::routing::Mode::Strict f64::total_cmp f64::INFINITY m::active_sessions m::compare_gated m::has_overload_mark m::metrics_for m::placement_capped_for m::placement_evidence_for m::rebalance_target_base_for m::rebalance_target_for m::rebalance_target_waiting_for m::reserved m::spread_threshold super::accepted_in super::candidate_facts super::effective_account super::semantic_evidence super::stale_window super::group String::as_str str::to_owned PlacementEvidence::Fresh PlacementEvidence::Weak PlacementState::default usize::from Mode::Rebalance Mode::Strict PolicyInputRefusal::MissingTargetPool";
const SELECTION_METHODS: &str = "and_then any append as_array as_deref as_f64 as_ref as_str checked_sub clone cloned cmp collect contains contains_key copied count discretionary_allowed eligible_at expect filter filter_map find first flatten get get_or_insert_with into into_iter is_empty is_err is_finite is_none is_none_or is_ok_and is_some is_some_and iter len map map_or map_or_else max min min_by num_seconds observe charge_count weak ok_or or or_else permits_dispatch placement_plan plan policy_id position push retain rev saturating_add selected_id settings signed_duration_since sort_by strip_prefix then then_with to_owned to_std total_cmp transpose unwrap_or";

fn listed(list: &str, name: &str) -> bool {
    list.split_whitespace().any(|allowed| allowed == name)
}

fn path_name(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

struct SelectionBoundary<'a> {
    signatures: &'a [&'a str],
    imports: &'a [&'a str],
    locals: BTreeSet<String>,
    errors: Vec<String>,
    seen: BTreeSet<String>,
    pure_policy_receiver: bool,
}

impl SelectionBoundary<'_> {
    fn function(
        &mut self,
        vis: &syn::Visibility,
        sig: &syn::Signature,
        block: &syn::Block,
        attrs: &[syn::Attribute],
    ) {
        let signature = compact(&(tokens(vis) + &tokens(sig)));
        if !self
            .signatures
            .iter()
            .any(|expected| compact(expected) == signature)
        {
            self.errors
                .push(format!("non-input selection signature: {signature}"));
        }
        if !self.seen.insert(signature) {
            self.errors.push("duplicate selection function".into());
        }
        let mut locals = Locals::default();
        locals.visit_signature(sig);
        locals.visit_block(block);
        self.locals = locals.0;
        for attr in attrs {
            // The metrics ID wrapper may be test-only; its signature and body
            // are still checked. No general cfg or attribute admission.
            if self.imports == METRICS_IMPORTS
                && sig.ident == "metrics"
                && compact(&tokens(attr)) == "#[cfg(test)]"
            {
                continue;
            }
            self.visit_attribute(attr);
        }
        self.visit_signature(sig);
        self.visit_block(block);
    }
}

impl<'ast> Visit<'ast> for SelectionBoundary<'_> {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // Also covers Stmt::Macro: a standalone macro is not Expr::Macro.
        use syn::parse::Parser;
        if !mac.path.is_ident("matches") {
            self.errors.push("unreviewed selection macro".into());
            return;
        }
        let parser = |input: syn::parse::ParseStream<'_>| {
            let value: Expr = input.parse()?;
            input.parse::<syn::Token![,]>()?;
            let pattern = Pat::parse_multi_with_leading_vert(input)?;
            if !input.is_empty() {
                return Err(input.error("unreviewed matches guard"));
            }
            Ok((value, pattern))
        };
        match parser.parse2(mac.tokens.clone()) {
            Ok((value, pattern)) => {
                self.visit_expr(&value);
                self.visit_pat(&pattern);
            }
            Err(_) => self.errors.push("unreviewed selection macro".into()),
        }
    }

    fn visit_pat(&mut self, pat: &'ast Pat) {
        let path = match pat {
            Pat::Path(p) => Some(&p.path),
            Pat::Struct(p) => Some(&p.path),
            Pat::TupleStruct(p) => Some(&p.path),
            _ => None,
        };
        if let Some(path) = path
            && (path.leading_colon.is_some() || !listed(SELECTION_PATHS, &path_name(path)))
        {
            self.errors
                .push("unreviewed selection pattern value".into());
        }
        visit::visit_pat(self, pat);
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr.path().is_ident("doc") {
            return;
        }
        let allowed = [
            "#[derive(Clone, Debug, Serialize, Deserialize)]",
            "#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]",
            "#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]",
            "#[derive(Clone, Debug, PartialEq, Eq)]",
            "#[derive(Clone, Copy, Debug)]",
            "#[serde(deny_unknown_fields)]",
            "#[serde(rename_all = \"snake_case\")]",
            "#[serde(tag = \"kind\", rename_all = \"snake_case\", deny_unknown_fields)]",
            "#[allow(clippy::too_many_arguments)]",
        ];
        if !allowed.iter().any(|a| compact(a) == compact(&tokens(attr))) {
            self.errors.push("unreviewed selection attribute".into());
        }
    }

    fn visit_item(&mut self, item: &'ast Item) {
        // Only this reviewed local value declaration is allowed in a body.
        let expected: Item = syn::parse_str(
            "struct SelectionBinding { key: String, account: String, last_used: DateTime<Utc>, }",
        )
        .unwrap();
        if compact(&tokens(item)) != compact(&tokens(&expected)) {
            self.errors
                .push("nested selection item / state / import".into());
        }
    }

    fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
        if ty.qself.is_some()
            || ty.path.leading_colon.is_some()
            || !(listed(SELECTION_TYPES, &path_name(&ty.path))
                || (self.pure_policy_receiver && ty.path.is_ident("Self")))
        {
            self.errors.push(format!(
                "native / unreviewed selection type: {}",
                tokens(ty)
            ));
        }
        visit::visit_type_path(self, ty);
    }

    fn visit_type(&mut self, ty: &'ast syn::Type) {
        if matches!(
            ty,
            syn::Type::BareFn(_)
                | syn::Type::Macro(_)
                | syn::Type::Ptr(_)
                | syn::Type::TraitObject(_)
        ) {
            self.errors.push("selection capability type".into());
        }
        visit::visit_type(self, ty);
    }

    fn visit_trait_bound(&mut self, bound: &'ast syn::TraitBound) {
        // The sole callback seam is calculate's supplied stale-window closure.
        if path_name(&bound.path) != "Fn" {
            self.errors.push("unreviewed selection trait bound".into());
        }
        visit::visit_trait_bound(self, bound);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Path(p) => {
                let name = path_name(&p.path);
                let helpers = "select return_blocked rebind_budget_source select_ordered prefer prefer_root_hint prefer_pacing_and_evidence effective_for policy_key account_cmp idle protected select_ranked retained_binding protected_source launch_retained retained source metrics metrics_for placement_state evidence placement_evidence_for placement_capped_for activity active_sessions reservations reserved threshold spread_threshold has_overload_mark rebalance_target_waiting_for waiting rebalance_target_base_for rebalance_target_for compare_gated compare_in_order calculate account_window date group";
                if p.qself.is_some()
                    || p.path.leading_colon.is_some()
                    || (!self.locals.contains(&name)
                        && !listed(SELECTION_PATHS, &name)
                        && !listed(helpers, &name))
                {
                    self.errors
                        .push(format!("external selection value / getter: {}", tokens(p)));
                }
            }
            Expr::MethodCall(c) => {
                if !listed(SELECTION_METHODS, &c.method.to_string()) {
                    self.errors
                        .push(format!("unreviewed selection method: {}", c.method));
                }
                // Pin the native value seam to the locally reconstructed value.
                let receiver = compact(&tokens(&c.receiver));
                if listed("observe weak charge_count", &c.method.to_string()) && receiver != "state"
                {
                    self.errors.push("nonlocal PlacementState receiver".into());
                }
            }
            Expr::Struct(s) => {
                if s.qself.is_some()
                    || s.path.leading_colon.is_some()
                    || !listed(
                        "SelectionEvaluation SelectionDecision SelectionEvidence SelectionBinding HoldDiagnostic SelectionChoice::Selected SelectionChoice::Hold super::permit::SoleTargetInput PlacementState PlacementWindow UsageObservationAsOf Metrics Plan",
                        &path_name(&s.path),
                    )
                {
                    self.errors.push("unreviewed selection construction".into());
                }
            }
            Expr::Field(f) if matches!(&f.member, syn::Member::Named(n) if n == "data") => {
                self.errors.push("selection owner data read".into());
            }
            Expr::Call(c) => {
                // Local function values are not a generic escape hatch. These
                // four names are the reviewed inline closure seams only.
                match &*c.func {
                    Expr::Path(p)
                        if !self.locals.contains(&path_name(&p.path))
                            || listed("at ratio cohort stale", &path_name(&p.path)) => {}
                    _ => self.errors.push("indirect selection call".into()),
                }
            }
            Expr::Macro(m) => {
                self.visit_macro(&m.mac);
                return;
            }
            Expr::Unsafe(_) => self.errors.push("selection unsafe read".into()),
            _ => {}
        }
        visit::visit_expr(self, expr);
    }
}

fn selection_guard(
    source: &str,
    signatures: &[&str],
    imports: &[&str],
    policy: bool,
) -> Vec<String> {
    let file = syn::parse_file(source).expect("selection guard syntax");
    let mut boundary = SelectionBoundary {
        signatures,
        imports,
        locals: BTreeSet::new(),
        errors: Vec::new(),
        seen: BTreeSet::new(),
        pure_policy_receiver: false,
    };
    for item in &file.items {
        match item {
            Item::Use(u) => {
                if !boundary
                    .imports
                    .iter()
                    .any(|expected| compact(expected) == compact(&tokens(u)))
                {
                    boundary.errors.push("unreviewed selection import".into());
                }
            }
            Item::Fn(f) if !policy => boundary.function(&f.vis, &f.sig, &f.block, &f.attrs),
            Item::Struct(s) => {
                if !listed(SELECTION_TYPES, &s.ident.to_string()) || !s.generics.params.is_empty() {
                    boundary
                        .errors
                        .push("unreviewed selection value declaration".into());
                }
                boundary.visit_item_struct(s);
            }
            Item::Enum(e) => {
                if !listed(SELECTION_TYPES, &e.ident.to_string()) || !e.generics.params.is_empty() {
                    boundary.errors.push("unreviewed selection enum".into());
                }
                boundary.visit_item_enum(e);
            }
            Item::Impl(i) if policy && i.trait_.is_none() && i.generics.params.is_empty() => {
                let owner = compact(&tokens(&i.self_ty));
                // Snapshot ownership is excluded by exact owner + method name,
                // never by broadly skipping an impl containing native locks.
                for member in &i.items {
                    match member {
                        syn::ImplItem::Fn(f)
                            if (owner == "PolicyStore" && f.sig.ident == "selection_inputs")
                                || (owner == "PolicyRowsInput" && f.sig.ident == "capture") => {}
                        syn::ImplItem::Fn(f)
                            if listed(
                                "SessionInput PolicyRowsInput SelectionPolicyInput",
                                &owner,
                            ) =>
                        {
                            // syn represents an implicit &self receiver using
                            // Type::Path(Self). Only these value impls own it.
                            boundary.pure_policy_receiver = true;
                            boundary.function(&f.vis, &f.sig, &f.block, &f.attrs);
                            boundary.pure_policy_receiver = false;
                        }
                        _ => boundary
                            .errors
                            .push("unreviewed policy evaluation item".into()),
                    }
                }
            }
            Item::Mod(m)
                if m.ident == "tests"
                    && m.content.is_none()
                    && matches!(m.vis, syn::Visibility::Inherited)
                    && m.attrs
                        .iter()
                        .any(|a| compact(&tokens(a)) == "#[cfg(test)]")
                    && m.attrs.iter().all(|a| {
                        [
                            "#[cfg(test)]",
                            "#[path=\"selection/tests.rs\"]",
                            "#[path=\"selection_affinity/tests.rs\"]",
                        ]
                        .contains(&compact(&tokens(a)).as_str())
                    }) => {}
            _ => boundary
                .errors
                .push("selection external module / state / item".into()),
        }
    }
    for expected in signatures {
        if !boundary.seen.contains(&compact(expected)) {
            boundary
                .errors
                .push(format!("missing selection signature: {expected}"));
        }
    }
    boundary.errors
}

fn selection_sources() -> [(
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    bool,
); 4] {
    [
        (
            SELECTION_SOURCE,
            SELECTION_SIGNATURES,
            SELECTION_IMPORTS,
            false,
        ),
        (
            AFFINITY_SOURCE,
            AFFINITY_SIGNATURES,
            AFFINITY_IMPORTS,
            false,
        ),
        (METRICS_SOURCE, METRICS_SIGNATURES, METRICS_IMPORTS, false),
        (POLICY_SOURCE, POLICY_SIGNATURES, POLICY_IMPORTS, true),
    ]
}

#[test]
fn selection_family_has_only_reviewed_value_inputs_and_callees() {
    for (source, signatures, imports, policy) in selection_sources() {
        assert_eq!(
            selection_guard(source, signatures, imports, policy),
            Vec::<String>::new()
        );
    }
}

#[test]
fn planted_selection_native_global_clock_and_io_reads_fail_guard() {
    for (source, signatures, imports, policy) in selection_sources() {
        let needle = if policy {
            "fn selected_id(&self) -> &str {"
        } else if source == SELECTION_SOURCE {
            "pub fn select(inputs: &DecisionInputs) -> Result<SelectionEvaluation, SelectionInputError> {"
        } else if source == AFFINITY_SOURCE {
            "fn idle(now: DateTime<Utc>, last: DateTime<Utc>) -> u64 {"
        } else {
            "pub fn metrics(inputs: &DecisionInputs, id: &str) -> Option<Metrics> {"
        };
        for planted in [
            "let _x = crate::bindings::native_snapshot();",
            "let _x = GLOBAL.load(std::sync::atomic::Ordering::Relaxed);",
            "let _x = Utc::now();",
            "let _x = std::fs::read(\"live\");",
            "let _x = self.data.lock();",
            "let _x = inputs.native_getter();",
            "let _x: &crate::bindings::BindingTable = inputs;",
            "let _x: &Data = inputs;",
            "let _x: &PolicyStore = inputs;",
            "let _x = matches!(Utc::now(), Some(_));",
            "native_read!();",
            "let _x = matches!(inputs.now, crate::GLOBAL);",
            "fn escape() { let _x = GLOBAL; }",
            "let escape = crate::bindings::native_snapshot; escape();",
            "let _x = <PolicyStore>::default();",
        ] {
            let fixture = plant_after(source, needle, planted);
            assert!(
                !selection_guard(&fixture, signatures, imports, policy).is_empty(),
                "selection plant escaped: {planted}"
            );
        }
    }
}

#[test]
fn selection_signatures_and_value_fields_reject_native_capabilities() {
    let fixture = plant_after(
        METRICS_SOURCE,
        "pub fn metrics(inputs: &DecisionInputs, id: &str) -> Option<Metrics> {",
        "let _owner: &Self = inputs;",
    );
    assert!(!selection_guard(&fixture, METRICS_SIGNATURES, METRICS_IMPORTS, false).is_empty());
    let fixture = SELECTION_SOURCE.replacen("pub fn select(", "#[cfg(test)] pub fn select(", 1);
    assert_ne!(fixture, SELECTION_SOURCE);
    assert!(!selection_guard(&fixture, SELECTION_SIGNATURES, SELECTION_IMPORTS, false).is_empty());
    let fixture = SELECTION_SOURCE.replacen(
        "pub fn select(inputs: &DecisionInputs)",
        "pub fn select(inputs: &BindingTable)",
        1,
    );
    assert_ne!(fixture, SELECTION_SOURCE);
    assert!(!selection_guard(&fixture, SELECTION_SIGNATURES, SELECTION_IMPORTS, false).is_empty());
    let fixture = SELECTION_SOURCE.replacen(
        "pub struct SelectionInputs {",
        "pub struct SelectionInputs { pub owner: std::sync::Mutex<Data>,",
        1,
    );
    assert_ne!(fixture, SELECTION_SOURCE);
    assert!(!selection_guard(&fixture, SELECTION_SIGNATURES, SELECTION_IMPORTS, false).is_empty());
}

#[test]
fn selection_facts_bridge_rejects_native_fallback_and_signature_escape() {
    for (needle, planted) in [
        (
            "BTreeMap::from([(candidate.account.id.clone(), candidate.clone())]),",
            "crate::bindings::native_snapshot(),",
        ),
        ("        accounts,", "        accounts: GLOBAL.snapshot(),"),
        ("candidate: &CandidateInput,", "candidate: &BindingTable,"),
    ] {
        let fixture = SOURCE.replacen(needle, planted, 1);
        assert_ne!(fixture, SOURCE, "stale bridge fixture: {needle}");
        assert!(
            guard(&fixture)
                .iter()
                .any(|e| e == "changed selection facts bridge"),
            "selection bridge escaped: {planted}"
        );
    }
}

#[test]
fn selection_bridge_and_ownerless_placement_constants_remain_allowed() {
    // The whole current family is positive evidence, including native snapshot
    // locks outside the boundary and local PlacementState value helper calls.
    assert_eq!(guard(SOURCE), Vec::<String>::new());
    // Both forms of the reviewed ID wrapper remain fully inspected.
    let fixture = METRICS_SOURCE.replacen("pub fn metrics(", "#[cfg(test)] pub fn metrics(", 1);
    assert_ne!(fixture, METRICS_SOURCE);
    assert_eq!(
        selection_guard(&fixture, METRICS_SIGNATURES, METRICS_IMPORTS, false),
        Vec::<String>::new()
    );
    let fixture = plant_after(
        AFFINITY_SOURCE,
        "fn idle(now: DateTime<Utc>, last: DateTime<Utc>) -> u64 {",
        "let _capacity = CAPACITY; let _idle = IDLE_SECS;",
    );
    assert_eq!(
        selection_guard(&fixture, AFFINITY_SIGNATURES, AFFINITY_IMPORTS, false),
        Vec::<String>::new()
    );
    let fixture = plant_after(
        METRICS_SOURCE,
        "fn placement_state(c: &CandidateInput, raw: &MetricInputs) -> PlacementState {",
        "let mut state = PlacementState::default(); if let Some(snapshot) = &c.quota { state.observe(snapshot); }",
    );
    assert_eq!(
        selection_guard(&fixture, METRICS_SIGNATURES, METRICS_IMPORTS, false),
        Vec::<String>::new()
    );
}
