//! Conservative syntax boundary for the whole decision kernel, including the
//! raw helpers and permit module. This is an exact allowlist, not a proof of
//! arbitrary Account internals. Native differential tests are outside it.
use quote::ToTokens;
use std::collections::BTreeSet;
use syn::{
    Expr, Item, Pat,
    visit::{self, Visit},
};

const SOURCE: &str = include_str!("../src/bindings/kernel.rs");
const PERMIT_SOURCE: &str = include_str!("../src/bindings/kernel/permit.rs");

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
];

const PERMIT_IMPORTS: &[&str] = &[
    "use chrono::{DateTime, Utc};",
    "use serde::{Deserialize, Serialize};",
    "use super::super::move_guard::{Limits, Refusal};",
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
            "pub now: DateTime<Utc>, pub model: Option<String>, pub key: Option<String>, pub from: Option<String>, pub to: String, pub accounts: BTreeMap<String, CandidateInput>, pub traffic: BTreeMap<String, DateTime<Utc>>, pub pending: BTreeMap<String, usize>, pub groups: BTreeMap<String, AcceptanceGroup>, pub bindings: BTreeMap<String, AcceptanceBinding>, pub accepted_unbound: BTreeSet<String>, pub sticky: BTreeMap<String, String>, pub outside_failure: bool, pub permit: Option<PermitInputs>,",
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
                if item.ident == "permit"
                    && item.content.is_none()
                    && item.attrs.is_empty()
                    && compact(&tokens(&item.vis)) == "pub(super)" => {}
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
