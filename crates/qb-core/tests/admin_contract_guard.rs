//! Candidate integration test: literal JSON/default policy, not a type checker.
//! Install this and its adjacent manifest together; do not replace the prototype.
use proc_macro2::{Span, TokenStream, TokenTree};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Meta, Token,
    parse::Parser,
    punctuated::Punctuated,
    spanned::Spanned,
    visit::{self, Visit},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    file: String,
    json: bool,
    defaults: bool,
    coverage: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct Key {
    file: String,
    line: usize,
    rule: String,
    snippet: String,
    field: String,
    field_type: String,
    owner: String,
    attribute: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allowance {
    key: Key,
    count: usize,
    reason: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    sources: Vec<Source>,
    baseline: Vec<Allowance>,
}
#[derive(Clone, Debug)]
struct Finding {
    key: Key,
    column: usize,
}
impl Finding {
    fn diagnostic(&self, reason: &str) -> String {
        // Do not print source snippets or possible payloads in gate output.
        format!(
            "{}:{}:{}: {} {} {}: {reason}",
            self.key.file,
            self.key.line,
            self.column,
            self.key.rule,
            self.key.field,
            self.key.field_type
        )
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Path>()
                    .is_ok_and(|p| p.is_ident("test")))
    })
}
fn item_attrs(item: &syn::Item) -> &[Attribute] {
    match item {
        syn::Item::Const(x) => &x.attrs,
        syn::Item::Enum(x) => &x.attrs,
        syn::Item::ExternCrate(x) => &x.attrs,
        syn::Item::Fn(x) => &x.attrs,
        syn::Item::ForeignMod(x) => &x.attrs,
        syn::Item::Impl(x) => &x.attrs,
        syn::Item::Macro(x) => &x.attrs,
        syn::Item::Mod(x) => &x.attrs,
        syn::Item::Static(x) => &x.attrs,
        syn::Item::Struct(x) => &x.attrs,
        syn::Item::Trait(x) => &x.attrs,
        syn::Item::TraitAlias(x) => &x.attrs,
        syn::Item::Type(x) => &x.attrs,
        syn::Item::Union(x) => &x.attrs,
        syn::Item::Use(x) => &x.attrs,
        _ => &[],
    }
}
fn join_text(first: Span, last: Span) -> Option<String> {
    first.join(last).and_then(|span| span.source_text())
}
struct Scanner<'a> {
    spec: &'a Source,
    aliases: BTreeSet<String>,
    field: String,
    field_type: String,
    owners: Vec<String>,
    attribute: String,
    findings: Vec<Finding>,
    errors: Vec<String>,
}
impl Scanner<'_> {
    fn emit(&mut self, span: Span, rule: &str, snippet: Option<String>) {
        let Some(snippet) = snippet else {
            self.errors.push(format!(
                "{}:{}: cannot recover exact {rule} occurrence",
                self.spec.file,
                span.start().line
            ));
            return;
        };
        self.findings.push(Finding {
            key: Key {
                file: self.spec.file.clone(),
                line: span.start().line,
                rule: rule.into(),
                snippet,
                field: if rule == "serde-default" {
                    self.field.clone()
                } else {
                    String::new()
                },
                field_type: if rule == "serde-default" {
                    self.field_type.clone()
                } else {
                    String::new()
                },
                owner: if rule == "serde-default" {
                    self.owners.join("/")
                } else {
                    String::new()
                },
                attribute: if rule == "serde-default" {
                    self.attribute.clone()
                } else {
                    String::new()
                },
            },
            column: span.start().column + 1,
        });
    }
    fn meta(&mut self, meta: &Meta) {
        let Meta::List(list) = meta else {
            return;
        };
        if !list.path.is_ident("serde") && !list.path.is_ident("cfg_attr") {
            return;
        }
        let entries =
            match Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone()) {
                Ok(entries) => entries,
                Err(error) => {
                    self.errors.push(format!(
                        "{}:{}: malformed contract attribute: {error}",
                        self.spec.file,
                        list.span().start().line
                    ));
                    return;
                }
            };
        if list.path.is_ident("serde") {
            for entry in entries {
                if entry.path().is_ident("default") {
                    self.emit(entry.span(), "serde-default", entry.span().source_text());
                }
            }
        } else {
            // Include every non-excluded cfg branch. No host/feature evaluation.
            for entry in entries.into_iter().skip(1) {
                self.meta(&entry);
            }
        }
    }
    fn tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Ident(name) = token {
                if self.spec.json
                    && self.aliases.contains(&name.to_string())
                    && let (Some(TokenTree::Punct(bang)), Some(TokenTree::Group(group))) =
                        (tokens.get(index + 1), tokens.get(index + 2))
                    && bang.as_char() == '!'
                {
                    self.emit(
                        name.span(),
                        "admin-json",
                        join_text(name.span(), group.span_close()),
                    );
                }
                if self.spec.defaults
                    && name == "serde"
                    && let Some(TokenTree::Group(group)) = tokens.get(index + 1)
                    && group.delimiter() == proc_macro2::Delimiter::Parenthesis
                {
                    // A serde(default) hidden in a macro token body has no
                    // compiled field identity. Report it; it cannot be baselined.
                    match Punctuated::<Meta, Token![,]>::parse_terminated.parse2(group.stream()) {
                        Ok(entries) if entries.iter().any(|m| m.path().is_ident("default")) => {
                            self.errors.push(format!(
                                "{}:{}: macro-hidden serde(default); expose a real contract field",
                                self.spec.file,
                                name.span().start().line
                            ))
                        }
                        Err(error) => self.errors.push(format!(
                            "{}:{}: malformed macro serde attribute: {error}",
                            self.spec.file,
                            name.span().start().line
                        )),
                        _ => {}
                    }
                }
            }
            if let TokenTree::Group(group) = token {
                self.tokens(group.stream());
            }
        }
    }
}
impl<'ast> Visit<'ast> for Scanner<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if test_only(item_attrs(item)) {
            return;
        }
        let owner = match item {
            syn::Item::Struct(x) => Some(format!("struct:{}", x.ident)),
            syn::Item::Enum(x) => Some(format!("enum:{}", x.ident)),
            syn::Item::Union(x) => Some(format!("union:{}", x.ident)),
            syn::Item::Mod(x) => Some(format!("mod:{}", x.ident)),
            syn::Item::Fn(x) => Some(format!("fn:{}", x.sig.ident)),
            _ => None,
        };
        if let Some(owner) = &owner {
            self.owners.push(owner.clone());
        }
        visit::visit_item(self, item);
        if owner.is_some() {
            self.owners.pop();
        }
    }
    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        if test_only(&variant.attrs) {
            return;
        }
        self.owners.push(format!("variant:{}", variant.ident));
        visit::visit_variant(self, variant);
        self.owners.pop();
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !test_only(&item.attrs) {
            visit::visit_impl_item_fn(self, item);
        }
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if !test_only(&item.attrs) {
            visit::visit_trait_item_fn(self, item);
        }
    }
    fn visit_field(&mut self, field: &'ast syn::Field) {
        if test_only(&field.attrs) {
            return;
        }
        let previous = (self.field.clone(), self.field_type.clone());
        self.field = field
            .ident
            .as_ref()
            .map_or_else(|| "<tuple>".into(), ToString::to_string);
        if let Some(text) = field.ty.span().source_text() {
            self.field_type = text;
        } else {
            self.errors.push(format!(
                "{}:{}: cannot recover field type",
                self.spec.file,
                field.span().start().line
            ));
        }
        visit::visit_field(self, field);
        (self.field, self.field_type) = previous;
    }
    fn visit_attribute(&mut self, attribute: &'ast Attribute) {
        if self.spec.defaults
            && (attribute.path().is_ident("serde") || attribute.path().is_ident("cfg_attr"))
        {
            let previous = self.attribute.clone();
            if let Some(text) = attribute.span().source_text() {
                self.attribute = text;
                self.meta(&attribute.meta);
            } else {
                self.errors.push(format!(
                    "{}:{}: cannot recover contract attribute",
                    self.spec.file,
                    attribute.span().start().line
                ));
            }
            self.attribute = previous;
        }
        // Ordinary imports/derives/serde annotations are not unsupported syntax.
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = &mac
            .path
            .segments
            .last()
            .expect("syn macro has a path")
            .ident;
        if self.spec.json && self.aliases.contains(&name.to_string()) {
            let close = match &mac.delimiter {
                syn::MacroDelimiter::Paren(x) => x.span.close(),
                syn::MacroDelimiter::Brace(x) => x.span.close(),
                syn::MacroDelimiter::Bracket(x) => x.span.close(),
            };
            self.emit(name.span(), "admin-json", join_text(name.span(), close));
        }
        // syn does not visit token bodies of select!, stream!, macro_rules!, etc.
        // Descend literals safely as tokens, not strings. Do not reject the macro.
        self.tokens(mac.tokens.clone());
    }
}
fn collect_aliases(tree: &syn::UseTree, path: &mut Vec<String>, names: &mut BTreeSet<String>) {
    match tree {
        syn::UseTree::Path(x) => {
            path.push(x.ident.to_string());
            collect_aliases(&x.tree, path, names);
            path.pop();
        }
        syn::UseTree::Name(x) if x.ident == "json" => {
            names.insert("json".into());
        }
        syn::UseTree::Rename(x) if x.ident == "json" => {
            names.insert(x.rename.to_string());
        }
        syn::UseTree::Group(x) => {
            for item in &x.items {
                collect_aliases(item, path, names);
            }
        }
        _ => {}
    }
}
fn scan(spec: &Source, source: &str) -> Result<Vec<Finding>, Vec<String>> {
    let ast = syn::parse_file(source).map_err(|error| {
        vec![format!(
            "{}:{}:{}: syn parse: {error}",
            spec.file,
            error.span().start().line,
            error.span().start().column + 1
        )]
    })?;
    if ast.items.is_empty() {
        return Err(vec![format!("{}: empty source", spec.file)]);
    }
    struct Imports(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Imports {
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            collect_aliases(&item.tree, &mut Vec::new(), &mut self.0);
        }
    }
    let mut imports = Imports(BTreeSet::from(["json".into()]));
    imports.visit_file(&ast);
    let mut scanner = Scanner {
        spec,
        aliases: imports.0,
        field: String::new(),
        field_type: String::new(),
        owners: Vec::new(),
        attribute: String::new(),
        findings: Vec::new(),
        errors: Vec::new(),
    };
    scanner.visit_file(&ast);
    if scanner.errors.is_empty() {
        Ok(scanner.findings)
    } else {
        Err(scanner.errors)
    }
}
fn valid_path(file: &str) -> bool {
    !file.is_empty()
        && !file.contains(['\\', ':'])
        && file.ends_with(".rs")
        && file.split('/').all(|x| !matches!(x, "" | "." | ".."))
}
fn enforce(manifest: &Manifest, sources: &BTreeMap<String, String>) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let mut paths = BTreeSet::new();
    if manifest.version != 1 || manifest.sources.is_empty() {
        errors.push("invalid manifest version or empty sources".into());
    }
    let mut findings = Vec::new();
    for spec in &manifest.sources {
        if !valid_path(&spec.file)
            || !paths.insert(spec.file.clone())
            || (!spec.json && !spec.defaults)
            || spec.coverage.trim().is_empty()
        {
            errors.push(format!(
                "{}: invalid/duplicate source specification",
                spec.file
            ));
        }
        match sources.get(&spec.file) {
            None => errors.push(format!("{}: missing source", spec.file)),
            Some(source) => match scan(spec, source) {
                Ok(rows) => findings.extend(rows),
                Err(rows) => errors.extend(rows),
            },
        }
    }
    for extra in sources.keys().filter(|file| !paths.contains(*file)) {
        errors.push(format!("{extra}: unmanifested supplied source"));
    }
    let mut approvals = BTreeMap::new();
    for allowance in &manifest.baseline {
        let key = &allowance.key;
        let source = manifest.sources.iter().find(|s| s.file == key.file);
        let scoped = source.is_some_and(|s| match key.rule.as_str() {
            "admin-json" => s.json,
            "serde-default" => {
                s.defaults
                    && !key.field.is_empty()
                    && !key.field_type.is_empty()
                    && !key.owner.is_empty()
                    && !key.attribute.is_empty()
            }
            _ => false,
        });
        if !scoped
            || key.line == 0
            || key.snippet.is_empty()
            || allowance.count == 0
            || allowance.reason.trim().is_empty()
            || approvals.insert(key.clone(), allowance).is_some()
        {
            errors.push(format!(
                "{}:{}: invalid/duplicate baseline entry",
                key.file, key.line
            ));
        }
    }
    let mut counts = BTreeMap::new();
    for finding in &findings {
        *counts.entry(finding.key.clone()).or_insert(0usize) += 1;
        if !approvals.contains_key(&finding.key) {
            errors.push(finding.diagnostic("new/unapproved occurrence"));
        }
    }
    for (key, allowance) in approvals {
        let actual = counts.get(&key).copied().unwrap_or(0);
        if actual != allowance.count {
            errors.push(format!(
                "{}:{}: {} baseline count {} != actual {actual}; review stale/added occurrence",
                key.file, key.line, key.rule, allowance.count
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// Explicit response-producing graph roots/helpers from the route inventory.
// Entire production files are scanned; this list is not an item-name wildcard.
const REQUIRED_PRODUCERS: &[&str] = &[
    "crates/qb-core/src/admin.rs",
    "crates/qb-core/src/http.rs",
    "crates/qb-core/src/account_runtime.rs",
    "crates/qb-core/src/account_runtime/admin_view.rs",
    "crates/qb-core/src/observability.rs",
    "crates/qb-core/src/relay.rs",
    "crates/qb-core/src/debug_pin.rs",
    "crates/qb-core/src/bindings.rs",
    "crates/qb-core/src/bindings/affinity.rs",
    "crates/qb-core/src/routing.rs",
    "crates/qb-core/src/routing/forecast.rs",
    "crates/qb-core/src/relay/launch.rs",
    "crates/qb-core/src/relay/credentials.rs",
    "crates/qb-core/src/oauth.rs",
    "crates/qb-core/src/accounts/import.rs",
    "crates/qb-core/src/accounts/takeover.rs",
    "crates/qb-core/src/accounts/adopt.rs",
    "crates/qb-core/src/dashboard.rs",
    "crates/qb-core/src/dashboard/history.rs",
    "crates/qb-core/src/dashboard/runtime.rs",
    "crates/qb-core/src/dashboard/labels.rs",
    "crates/qb-core/src/observability/reread_accounting.rs",
];
fn proto_files(root: &Path, directory: &Path, files: &mut BTreeSet<String>) -> Result<(), String> {
    for entry in
        std::fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?
    {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path
            .symlink_metadata()
            .map_err(|error| error.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err(format!("{}: symlink in proto source tree", path.display()));
        }
        if path.is_dir() {
            proto_files(root, &path, files)?;
        } else if path.extension().is_some_and(|x| x == "rs") {
            files.insert(
                path.strip_prefix(root)
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}
fn proto_coverage(manifest: &Manifest, files: &BTreeSet<String>) -> Vec<String> {
    files
        .iter()
        .filter(|file| {
            !manifest
                .sources
                .iter()
                .any(|s| s.file.as_str() == file.as_str() && s.defaults && s.json)
        })
        .map(|file| format!("{file}: shared proto source not covered"))
        .collect()
}
fn load_repository(
    root: &Path,
    manifest: &Manifest,
) -> Result<BTreeMap<String, String>, Vec<String>> {
    let root = root
        .canonicalize()
        .map_err(|error| vec![error.to_string()])?;
    let mut errors = Vec::new();
    let mut sources = BTreeMap::new();
    let mut canonical = BTreeSet::new();
    for spec in &manifest.sources {
        if !valid_path(&spec.file) {
            errors.push(format!("{}: invalid manifest path", spec.file));
            continue;
        }
        let path = root.join(&spec.file);
        match path.canonicalize() {
            Ok(path)
                if path.starts_with(&root) && path.is_file() && canonical.insert(path.clone()) =>
            {
                match std::fs::read_to_string(&path) {
                    Ok(source) => {
                        sources.insert(spec.file.clone(), source);
                    }
                    Err(error) => errors.push(format!("{}: {error}", spec.file)),
                }
            }
            _ => errors.push(format!(
                "{}: missing/outside/duplicate source file",
                spec.file
            )),
        }
    }
    for file in REQUIRED_PRODUCERS {
        if !manifest.sources.iter().any(|s| s.file == *file && s.json) {
            errors.push(format!("{file}: required producer not covered"));
        }
    }
    for file in [
        "crates/qb-core/src/admin.rs",
        "crates/qb-core/src/account_runtime/admin_view.rs",
    ] {
        if !manifest
            .sources
            .iter()
            .any(|s| s.file == file && s.defaults)
        {
            errors.push(format!(
                "{file}: contract fields must retain explicit default policy"
            ));
        }
    }
    let mut proto = BTreeSet::new();
    if let Err(error) = proto_files(&root, &root.join("crates/qb-proto/src"), &mut proto) {
        errors.push(error);
    }
    errors.extend(proto_coverage(manifest, &proto));
    if errors.is_empty() {
        Ok(sources)
    } else {
        Err(errors)
    }
}
#[test]
fn repository_admin_contract_policy() {
    let manifest: Manifest = serde_json::from_str(include_str!("admin-contract-manifest.json"))
        .expect("strict manifest JSON");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let result = load_repository(&root, &manifest).and_then(|sources| enforce(&manifest, &sources));
    if let Err(errors) = result {
        panic!("{}", errors.join("\n"));
    }
}

#[cfg(test)]
mod selftests {
    use super::*;
    fn spec() -> Source {
        Source {
            file: "synthetic.rs".into(),
            json: true,
            defaults: true,
            coverage: "synthetic policy fixture".into(),
        }
    }
    fn manifest() -> Manifest {
        Manifest {
            version: 1,
            sources: vec![spec()],
            baseline: vec![],
        }
    }
    fn check(source: &str, manifest: &Manifest) -> Result<(), Vec<String>> {
        enforce(
            manifest,
            &BTreeMap::from([("synthetic.rs".into(), source.into())]),
        )
    }
    fn approved(source: &str) -> Manifest {
        let mut manifest = manifest();
        let mut counts = BTreeMap::new();
        for finding in scan(&spec(), source).unwrap() {
            *counts.entry(finding.key).or_insert(0usize) += 1;
        }
        manifest.baseline = counts
            .into_iter()
            .map(|(key, count)| Allowance {
                key,
                count,
                reason: "synthetic test approval only".into(),
            })
            .collect();
        manifest
    }
    #[test]
    fn ordinary_rust_is_accepted_and_real_parse_errors_are_not() {
        assert!(check("use x::{self as alias, *}; #[derive(custom::Derive)] #[serde(rename_all=\"snake_case\")] struct S { n:u64 } fn f(){ tokio::select! { _=ready()=>{} }; }", &manifest()).is_ok());
        assert!(check("fn broken( {", &manifest()).is_err());
        assert!(check("// empty", &manifest()).is_err());
        assert!(check("struct S { #[serde(default =)] n:u64 }", &manifest()).is_err());
    }
    #[test]
    fn added_changed_and_copied_json_do_not_reuse_exact_baseline() {
        let old = "fn handler(){ serde_json::json!({\"ok\":true}); }";
        let policy = approved(old);
        assert!(check(old, &policy).is_ok());
        assert!(
            check(
                "fn handler(){ serde_json::json!({\"ok\":false}); }",
                &policy
            )
            .is_err()
        );
        assert!(check(&format!("{old}\nfn other(){{json!({{}});}}"), &policy).is_err());
        assert!(check("fn handler(){ }", &policy).is_err());
        assert!(check(&format!("{old}\n{old}"), &policy).is_err());
        let twice = "fn f(){json!({});json!({});}";
        assert!(check(twice, &approved("fn f(){json!({});}")).is_err());
    }
    #[test]
    fn aliases_and_nested_macros_are_scanned_without_rejecting_ordinary_macros() {
        for raw in [
            "use serde_json::json as reply; fn f(){ reply!({}); }",
            "fn f(){ tokio::select! { _=f()=>{ json!({}); } } }",
            "macro_rules! reply {()=>{serde_json::json!({})}}",
        ] {
            assert!(
                scan(&spec(), raw)
                    .unwrap()
                    .iter()
                    .any(|f| f.key.rule == "admin-json")
            );
        }
        assert!(
            scan(&spec(), "fn f(){ let _=\"json!({})\"; /* json!({}) */ } ")
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn only_exact_field_default_is_approved() {
        let old = "struct S { #[serde(default)] legacy:Option<String>, n:u64 }";
        let policy = approved(old);
        assert!(check(old, &policy).is_ok());
        for raw in [
            "struct T { #[serde(default)] legacy:Option<String>, n:u64 }",
            "struct S { #[serde(default)] legacy:Option<String>, #[serde(default)] n:u64 }",
            "struct S { legacy:Option<String>, #[serde(default)] n:u64 }",
            "struct S { #[serde(default)] legacy:String, n:u64 }",
            "#[serde(default)] struct S { legacy:Option<String>, n:u64 }",
            "struct S { #[cfg_attr(feature=\"x\",serde(default))] legacy:Option<String>, n:u64 }",
        ] {
            assert!(check(raw, &policy).is_err());
        }
        let variant = "enum E { A { #[serde(default)] legacy:Option<String> } }";
        assert!(check(&variant.replace("A {", "B {"), &approved(variant)).is_err());
        assert!(
            check(
                "macro_rules! dto {()=>{struct S{#[serde(default)] n:u64}}}",
                &manifest()
            )
            .is_err()
        );
    }
    #[test]
    fn test_exclusion_is_explicit_and_production_cfg_stays_visible() {
        assert!(scan(&spec(), "#[cfg(test)] mod tests { fn f(){json!({})} struct S{#[serde(default)] n:u64} } #[test] fn fixture(){json!({});} #[cfg(test)] struct T{#[serde(default)] n:u64}").unwrap().is_empty());
        assert!(
            !scan(
                &spec(),
                "#[cfg(any(test,feature=\"live\"))] fn f(){json!({});}"
            )
            .unwrap()
            .is_empty()
        );
    }
    #[test]
    fn newly_discovered_admin_dto_requires_manifest_registration() {
        let file = "crates/qb-proto/src/nested/admin_launch.rs".to_string();
        let files = BTreeSet::from([file.clone()]);
        let mut policy = manifest();
        assert_eq!(proto_coverage(&policy, &files).len(), 1);
        let mut source = spec();
        source.file = file;
        policy.sources.push(source);
        assert!(proto_coverage(&policy, &files).is_empty());
        policy.sources.last_mut().unwrap().defaults = false;
        assert_eq!(proto_coverage(&policy, &files).len(), 1);
    }
    #[test]
    fn manifest_and_allowances_fail_closed() {
        let raw = "fn f(){json!({});}";
        let mut policy = approved(raw);
        policy.baseline[0].count += 1;
        assert!(check(raw, &policy).is_err());
        let mut policy = approved(raw);
        policy.baseline.push(policy.baseline[0].clone());
        assert!(check(raw, &policy).is_err());
        let mut policy = approved(raw);
        policy.baseline[0].reason.clear();
        assert!(check(raw, &policy).is_err());
        let mut policy = manifest();
        policy.sources.push(spec());
        assert!(check(raw, &policy).is_err());
        assert!(enforce(&manifest(), &BTreeMap::new()).is_err());
        assert!(!valid_path("../outside.rs"));
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let errors = load_repository(&root, &manifest()).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("synthetic.rs: missing/outside/duplicate source file"))
        );
    }
}
