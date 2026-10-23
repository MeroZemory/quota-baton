//! Conservative syntax change gate for the binding implementation, not an alias/type proof.
//!
//! Scope: bindings.rs and EVERY .rs below bindings/, including unreferenced new files.
//! Only structurally test-required items/modules are skipped. Other cfgs are scanned.
//! Exact token-normalized operations (including occurrence counts) require review.
//! This deliberately overmatches local copies, metadata accesses, and unrelated `account`
//! fields. It cannot establish what arbitrary aliases, imported functions, generated code,
//! proc macros, or renamed types do. The compiler separately seals the account field.
//! Review is still required for aliases/APIs and changes outside this source boundary.
//!
//! Coordinator dependencies (this test does not edit Cargo manifests):
//! syn = { version = "2", features = ["full", "visit"] }, quote = "1",
//! proc-macro2 = { version = "1", features = ["span-locations"] }.
//!
//! Normal gate NEVER writes/refreshes the manifest. To print review candidates explicitly:
//! cargo test -p qb-core --test binding_write_guard print_binding_write_candidates -- --ignored --exact --nocapture
//! Review the printed JSON and copy ONLY its `allowlist` into the fixture after reviewing
//! each new choke point. Do not redirect Cargo's mixed stdout straight into the fixture.

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};
use syn::{
    Attribute, Expr, Item, Meta,
    spanned::Spanned,
    visit::{self, Visit},
};

const ROOT: &str = "crates/qb-core/src/bindings.rs";
const TREE: &str = "crates/qb-core/src/bindings/";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct Operation {
    path: String,
    function: String,
    expression: String,
    count: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema: u32,
    allowlist: Vec<Operation>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    files: BTreeMap<String, String>,
    #[serde(default)]
    baseline: BTreeMap<String, String>,
    reject: bool,
    #[serde(default)]
    diagnostic: String,
}

#[derive(Default)]
struct Inventory {
    // Lines are diagnostic only; never part of the approval key.
    sites: BTreeMap<(String, String, String), Vec<usize>>,
    errors: Vec<String>,
}

impl Inventory {
    fn operations(&self) -> Vec<Operation> {
        self.sites
            .iter()
            .map(|((path, function, expression), lines)| Operation {
                path: path.clone(),
                function: function.clone(),
                expression: expression.clone(),
                count: lines.len(),
            })
            .collect()
    }

    fn compare(&self, allowed: &[Operation]) -> Vec<String> {
        let mut errors = self.errors.clone();
        let mut expected = BTreeMap::new();
        for op in allowed {
            let key = (op.path.clone(), op.function.clone(), op.expression.clone());
            if op.count == 0 || expected.insert(key, op.count).is_some() {
                errors.push(format!(
                    "{}:1: invalid/duplicate allowlist key: {}",
                    op.path, op.function
                ));
            }
        }
        for (key, lines) in &self.sites {
            let count = expected.remove(key).unwrap_or(0);
            if count != lines.len() {
                errors.push(format!(
                    "{}:{}: {}: expected {count}, found {}: {}",
                    key.0,
                    lines[0],
                    key.1,
                    lines.len(),
                    key.2
                ));
            }
        }
        for ((path, function, expression), count) in expected {
            errors.push(format!(
                "{path}:1: missing reviewed operation ({count}) in {function}: {expression}"
            ));
        }
        errors
    }
}

// TokenTree serialization preserves literal contents and token boundaries (including
// punctuation spacing) while ignoring source whitespace/comments. Never strip spaces
// from source text: that would conflate strings and even different tokenizations.
fn normalized(tokens: impl ToTokens) -> String {
    fn walk(stream: TokenStream, out: &mut Vec<String>) {
        for token in stream {
            match token {
                TokenTree::Group(g) => {
                    out.push(format!("{:?}(", g.delimiter()));
                    walk(g.stream(), out);
                    out.push(")".into());
                }
                TokenTree::Punct(p) => out.push(format!("{}{:?}", p.as_char(), p.spacing())),
                token => out.push(token.to_string()),
            }
        }
    }
    let mut parts = Vec::new();
    walk(tokens.to_token_stream(), &mut parts);
    parts.join(" ")
}

fn test_required(meta: &Meta) -> bool {
    match meta {
        Meta::Path(p) => p.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
            use syn::{Token, parse::Parser, punctuated::Punctuated};
            let Ok(parts) =
                Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone())
            else {
                return false;
            };
            if list.path.is_ident("all") {
                parts.iter().any(test_required)
            } else {
                !parts.is_empty() && parts.iter().all(test_required)
            }
        }
        _ => false,
    }
}

fn only_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg") && a.parse_args::<Meta>().is_ok_and(|m| test_required(&m))
    })
}

fn scoped(path: &str) -> bool {
    path == ROOT || path.starts_with(TREE)
}

fn clean_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

fn module_dir(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.file_name().is_some_and(|n| n == "mod.rs") {
        p.parent().unwrap().to_owned()
    } else {
        p.with_extension("")
    }
}

// Discovery follows production module declarations for test exclusions. Orphan/new files
// remain scanned. Test-only files cannot themselves suppress production siblings.
struct Modules<'a> {
    path: &'a str,
    dir: PathBuf,
    files: &'a BTreeMap<String, String>,
    edges: Vec<(String, bool)>,
    errors: Vec<String>,
    test: bool,
}

impl<'ast> Visit<'ast> for Modules<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let old_test = self.test;
        self.test |= only_test(&item.attrs);
        let old_dir = self.dir.clone();
        let path_attr = item.attrs.iter().find(|a| a.path().is_ident("path"));
        if let Some((_, items)) = &item.content {
            if path_attr.is_some() && !self.test {
                self.errors.push(format!(
                    "{}:{}: #[path] on inline module requires explicit scanner support",
                    self.path,
                    item.span().start().line
                ));
            }
            self.dir.push(item.ident.to_string());
            for item in items {
                self.visit_item(item);
            }
        } else {
            let targets: Vec<Option<String>> = if let Some(attr) = path_attr {
                // #[path] in a file is relative to that file's directory; in inline
                // modules it is relative to the inline module directory.
                let base = if self.dir == module_dir(self.path) {
                    Path::new(self.path).parent().unwrap().to_owned()
                } else {
                    self.dir.clone()
                };
                let target = match &attr.meta {
                    Meta::NameValue(v) => match &v.value {
                        Expr::Lit(lit) => match &lit.lit {
                            syn::Lit::Str(s) => clean_path(&base.join(s.value())),
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                };
                vec![target]
            } else {
                vec![
                    clean_path(&self.dir.join(format!("{}.rs", item.ident))),
                    clean_path(&self.dir.join(item.ident.to_string()).join("mod.rs")),
                ]
            };
            let found: Vec<_> = targets
                .iter()
                .flatten()
                .filter(|p| self.files.contains_key(*p))
                .cloned()
                .collect();
            if !self.test
                && (found.len() != 1
                    || targets
                        .iter()
                        .any(|p| p.as_ref().is_none_or(|p| !scoped(p))))
            {
                self.errors.push(format!("{}:{}: module {} resolves outside scanned sources, is missing, or ambiguous: {:?}", self.path, item.span().start().line, item.ident, targets));
            }
            for target in found {
                self.edges.push((target, self.test));
            }
        }
        self.dir = old_dir;
        self.test = old_test;
    }

    fn visit_item(&mut self, item: &'ast Item) {
        // Preserve test context across enclosing inline modules/impls/functions.
        let old = self.test;
        self.test |= only_test(item_attrs(item));
        visit::visit_item(self, item);
        self.test = old;
    }
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn special(name: &str) -> bool {
    matches!(
        name,
        "account"
            | "bindings"
            | "groups"
            | "Binding"
            | "Group"
            | "Data"
            | "Snapshot"
            | "Identity"
            | "identity"
            | "serde_json"
            | "from_value"
            | "from_str"
            | "from_slice"
            | "deserialize"
            | "d"
            | "data"
    )
}

fn publication(name: &str) -> bool {
    matches!(
        name,
        "index_group"
            | "unindex_group"
            | "rebuild_indexes"
            | "commit_binding"
            | "publish_binding"
            | "rebind"
            | "commit_move"
            | "decode_scoped"
            | "discard_group"
    )
}

// Syntax names, NOT inferred variable types. Recursing through receivers catches
// parenthesized fields, indexed maps, and chained entry/get_mut expressions.
fn sensitive(tokens: impl ToTokens) -> bool {
    fn walk(stream: TokenStream) -> bool {
        stream.into_iter().any(|t| match t {
            TokenTree::Ident(i) => special(&i.to_string()) || publication(&i.to_string()),
            TokenTree::Group(g) => walk(g.stream()),
            _ => false,
        })
    }
    walk(tokens.to_token_stream())
}

fn mentions_alias(tokens: impl ToTokens, aliases: &BTreeSet<String>) -> bool {
    tokens.to_token_stream().into_iter().any(|t| match t {
        TokenTree::Ident(i) => aliases.contains(&i.to_string()),
        TokenTree::Group(g) => mentions_alias(g.stream(), aliases),
        _ => false,
    })
}

// Explicit import/type/function-value aliases are tracked across files as a
// conservative overapproximation. Globs, generated aliases, outside imports and
// arbitrary dataflow remain outside this syntax gate's proof boundary.
struct Aliases<'a>(&'a mut BTreeSet<String>);
impl<'ast> Visit<'ast> for Aliases<'_> {
    fn visit_item(&mut self, i: &'ast Item) {
        if !only_test(item_attrs(i)) {
            visit::visit_item(self, i);
        }
    }
    fn visit_use_tree(&mut self, i: &'ast syn::UseTree) {
        if let syn::UseTree::Rename(r) = i
            && (special(&r.ident.to_string())
                || publication(&r.ident.to_string())
                || self.0.contains(&r.ident.to_string()))
        {
            self.0.insert(r.rename.to_string());
        }
        visit::visit_use_tree(self, i);
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        if sensitive(&i.ty) || mentions_alias(&i.ty, self.0) {
            self.0.insert(i.ident.to_string());
        }
    }
    fn visit_local(&mut self, i: &'ast syn::Local) {
        if only_test(&i.attrs) {
            return;
        }
        if let Some(init) = &i.init
            && matches!(&*init.expr, Expr::Path(_))
            && (sensitive(&init.expr) || mentions_alias(&init.expr, self.0))
            && let syn::Pat::Ident(p) = &i.pat
        {
            self.0.insert(p.ident.to_string());
        }
        visit::visit_local(self, i);
    }
}

fn deref(expr: &Expr) -> bool {
    match expr {
        Expr::Unary(u) => matches!(u.op, syn::UnOp::Deref(_)),
        Expr::Paren(p) => deref(&p.expr),
        Expr::Group(g) => deref(&g.expr),
        _ => false,
    }
}

struct Scanner<'a> {
    path: &'a str,
    scope: Vec<String>,
    inventory: &'a mut Inventory,
    aliases: &'a BTreeSet<String>,
}

impl Scanner<'_> {
    fn is_sensitive(&self, tokens: impl ToTokens) -> bool {
        sensitive(&tokens) || mentions_alias(tokens, self.aliases)
    }

    fn record(&mut self, kind: &str, expr: impl ToTokens + Spanned) {
        let key = (
            self.path.to_owned(),
            if self.scope.is_empty() {
                "<module>".into()
            } else {
                self.scope.join("::")
            },
            format!("{kind}: {}", normalized(&expr)),
        );
        self.inventory
            .sites
            .entry(key)
            .or_default()
            .push(expr.span().start().line);
    }
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    fn visit_item(&mut self, i: &'ast Item) {
        if !only_test(item_attrs(i)) {
            visit::visit_item(self, i);
        }
    }
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        if self.is_sensitive(&i.tree) {
            self.record("sensitive-import", i);
        }
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        if self.is_sensitive(&i.ty) {
            self.record("sensitive-type-alias", i);
        }
        visit::visit_item_type(self, i);
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scope.push(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.scope.pop();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let name = i.self_ty.to_token_stream().to_string();
        self.scope.push(if let Some((_, tr, _)) = &i.trait_ {
            format!("{} as {}", name, tr.to_token_stream())
        } else {
            name
        });
        visit::visit_item_impl(self, i);
        self.scope.pop();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.scope.push(i.sig.ident.to_string());
        visit::visit_item_fn(self, i);
        self.scope.pop();
    }
    fn visit_impl_item(&mut self, i: &'ast syn::ImplItem) {
        let attrs = match i {
            syn::ImplItem::Fn(i) => &i.attrs,
            syn::ImplItem::Const(i) => &i.attrs,
            syn::ImplItem::Type(i) => &i.attrs,
            syn::ImplItem::Macro(i) => &i.attrs,
            _ => return visit::visit_impl_item(self, i),
        };
        if !only_test(attrs) {
            visit::visit_impl_item(self, i);
        }
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.scope.push(i.sig.ident.to_string());
        visit::visit_impl_item_fn(self, i);
        self.scope.pop();
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if only_test(&i.attrs) {
            return;
        }
        self.scope.push(i.sig.ident.to_string());
        visit::visit_trait_item_fn(self, i);
        self.scope.pop();
    }
    fn visit_local(&mut self, i: &'ast syn::Local) {
        if !only_test(&i.attrs) {
            visit::visit_local(self, i);
        }
    }
    fn visit_expr(&mut self, expr: &'ast Expr) {
        // syn's expressions carry attributes per variant, not on the enum itself.
        let attrs: &[Attribute] = match expr {
            Expr::Assign(e) => &e.attrs,
            Expr::Block(e) => &e.attrs,
            Expr::Call(e) => &e.attrs,
            Expr::MethodCall(e) => &e.attrs,
            Expr::Macro(e) => &e.attrs,
            Expr::Struct(e) => &e.attrs,
            Expr::Reference(e) => &e.attrs,
            Expr::Binary(e) => &e.attrs,
            _ => &[],
        };
        if only_test(attrs) {
            return;
        }
        match expr {
            Expr::Assign(e) if self.is_sensitive(&e.left) || deref(&e.left) => {
                self.record("assignment", e)
            }
            Expr::Binary(e)
                if matches!(
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
                ) && (self.is_sensitive(&e.left) || deref(&e.left)) =>
            {
                self.record("compound-assignment", e)
            }
            Expr::Struct(e)
                if e.path.segments.last().is_some_and(|s| {
                    matches!(
                        s.ident.to_string().as_str(),
                        "Binding" | "Group" | "Data" | "Snapshot" | "Identity"
                    ) || self.aliases.contains(&s.ident.to_string())
                        || (s.ident == "Self"
                            && self.scope.iter().any(|name| {
                                matches!(
                                    name.as_str(),
                                    "Binding" | "Group" | "Data" | "Snapshot" | "Identity"
                                ) || self.aliases.contains(name)
                            }))
                }) =>
            {
                self.record("construction", e)
            }
            Expr::Reference(e)
                if e.mutability.is_some() && (self.is_sensitive(&e.expr) || deref(&e.expr)) =>
            {
                self.record("mutable-escape", e)
            }
            Expr::MethodCall(e) => {
                let method = e.method.to_string();
                if publication(&method)
                    || (self.is_sensitive(&e.receiver)
                        && matches!(
                            method.as_str(),
                            "insert"
                                | "try_insert"
                                | "insert_entry"
                                | "entry"
                                | "or_insert"
                                | "or_insert_with"
                                | "or_insert_with_key"
                                | "or_default"
                                | "and_modify"
                                | "extend"
                                | "append"
                                | "clear"
                                | "remove"
                                | "remove_entry"
                                | "retain"
                                | "extract_if"
                                | "split_off"
                                | "pop_first"
                                | "pop_last"
                                | "get_mut"
                                | "get_key_value_mut"
                                | "values_mut"
                                | "iter_mut"
                                | "range_mut"
                                | "first_entry"
                                | "last_entry"
                        ))
                {
                    self.record("method", e);
                }
            }
            Expr::Call(e) => {
                let name = if let Expr::Path(p) = &*e.func {
                    p.path
                        .segments
                        .last()
                        .map(|s| s.ident.to_string())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                if publication(&name)
                    || matches!(
                        name.as_str(),
                        "replace"
                            | "take"
                            | "swap"
                            | "from_value"
                            | "from_str"
                            | "from_slice"
                            | "deserialize"
                    )
                    || self.is_sensitive(&e.func)
                    || (name == "new" && e.args.iter().any(|a| self.is_sensitive(a)))
                {
                    self.record("call", e);
                }
            }
            _ => {}
        }
        visit::visit_expr(self, expr);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // Macro bodies are opaque to syn. A sensitive token anywhere (including a
        // definition) or code inclusion requires exact review, never silent expansion.
        if self.is_sensitive(&m.tokens)
            || m.path.segments.last().is_some_and(|s| s.ident == "include")
        {
            self.record("opaque-macro", m);
        }
    }
    fn visit_attribute(&mut self, a: &'ast Attribute) {
        // cfg_attr(path=...) can redirect a module without a direct #[path].
        if a.path().is_ident("cfg_attr")
            && a.to_token_stream().into_iter().any(|t| match t {
                TokenTree::Group(g) => g.stream().to_string().contains("path"),
                _ => false,
            })
        {
            self.inventory.errors.push(format!(
                "{}:{}: conditional module path requires explicit scanner support",
                self.path,
                a.span().start().line
            ));
        }
    }
}

fn inventory(files: &BTreeMap<String, String>) -> Inventory {
    let mut out = Inventory::default();
    let mut parsed = BTreeMap::new();
    for (path, text) in files {
        if !scoped(path) {
            out.errors
                .push(format!("{path}:1: outside scanned sources"));
            continue;
        }
        match syn::parse_file(text) {
            Ok(file) => {
                parsed.insert(path.clone(), file);
            }
            Err(e) => out.errors.push(format!(
                "{path}:{}: parse error: {e}",
                e.span().start().line
            )),
        }
    }
    if !parsed.contains_key(ROOT) {
        out.errors.push(format!("{ROOT}:1: missing scanner root"));
    }
    let mut production = BTreeSet::new();
    let mut tests = BTreeSet::new();
    let mut queue = vec![ROOT.to_owned()];
    // Afterwards scan all unreferenced sources too, with their own module discovery.
    loop {
        while let Some(path) = queue.pop() {
            if !production.insert(path.clone()) {
                continue;
            }
            let Some(ast) = parsed.get(&path) else {
                continue;
            };
            let mut mods = Modules {
                path: &path,
                dir: module_dir(&path),
                files,
                edges: Vec::new(),
                errors: Vec::new(),
                test: false,
            };
            mods.visit_file(ast);
            out.errors.extend(mods.errors);
            for (target, test) in mods.edges {
                if test {
                    tests.insert(target);
                } else {
                    queue.push(target);
                }
            }
        }
        let orphan = parsed
            .keys()
            .find(|p| {
                !production.contains(*p)
                    && !tests.iter().any(|t| {
                        *p == t
                            || p.starts_with(&format!(
                                "{}/",
                                module_dir(t).to_string_lossy().replace('\\', "/")
                            ))
                    })
            })
            .cloned();
        if let Some(path) = orphan {
            queue.push(path);
        } else {
            break;
        }
    }
    let mut aliases = BTreeSet::new();
    loop {
        let old = aliases.len();
        for path in &production {
            if let Some(ast) = parsed.get(path) {
                Aliases(&mut aliases).visit_file(ast);
            }
        }
        if old == aliases.len() {
            break;
        }
    }
    for path in production {
        if let Some(ast) = parsed.get(&path) {
            Scanner {
                path: &path,
                scope: Vec::new(),
                aliases: &aliases,
                inventory: &mut out,
            }
            .visit_file(ast);
        }
    }
    out
}

fn fixture() -> Fixture {
    let f: Fixture = serde_json::from_str(include_str!("fixtures/binding_write_guard.json"))
        .expect("valid binding write fixture");
    assert_eq!(f.schema, 1);
    f
}

fn real_sources() -> BTreeMap<String, String> {
    fn descend(dir: &Path, repo: &Path, files: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}:1: {e}", dir.display())) {
            let entry = entry.unwrap();
            let path = entry.path();
            let ty = entry.file_type().unwrap();
            assert!(
                !ty.is_symlink(),
                "{}:1: symlink in scanned tree requires review",
                path.display()
            );
            if ty.is_dir() {
                descend(&path, repo, files);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.insert(
                    path.strip_prefix(repo)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    fs::read_to_string(&path).unwrap(),
                );
            }
        }
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let root = repo.join(ROOT);
    assert!(
        !fs::symlink_metadata(&root)
            .unwrap()
            .file_type()
            .is_symlink(),
        "{}:1: symlink scanner root",
        root.display()
    );
    let mut files = BTreeMap::from([(ROOT.to_owned(), fs::read_to_string(root).unwrap())]);
    descend(&repo.join(TREE), repo, &mut files);
    files
}

#[test]
fn canonical_binding_write_surface_is_reviewed() {
    let current = inventory(&real_sources());
    let errors = current.compare(&fixture().allowlist);
    assert!(
        errors.is_empty(),
        "Binding write surface changed; review each operation, then explicitly print candidates.\n{}",
        errors.join("\n")
    );
}

#[test]
fn planted_binding_write_changes_are_rejected() {
    for case in fixture().cases {
        let baseline = if case.baseline.is_empty() {
            BTreeMap::from([(ROOT.to_owned(), String::new())])
        } else {
            case.baseline
        };
        let old = inventory(&baseline);
        assert!(
            old.errors.is_empty(),
            "{}: invalid fixture baseline: {:?}",
            case.name,
            old.errors
        );
        let errors = inventory(&case.files).compare(&old.operations());
        assert_eq!(
            !errors.is_empty(),
            case.reject,
            "{}: {:?}",
            case.name,
            errors
        );
        if case.reject {
            assert!(
                !case.diagnostic.is_empty(),
                "{}: negative must assert a diagnostic",
                case.name
            );
            assert!(
                errors.iter().any(|e| e.contains(&case.diagnostic)),
                "{}: expected {:?}, got {:?}",
                case.name,
                case.diagnostic,
                errors
            );
            assert!(
                errors.iter().all(|e| e.contains(":1:")
                    || e.split(':')
                        .nth(1)
                        .is_some_and(|s| s.parse::<usize>().is_ok())),
                "{}: file:line required",
                case.name
            );
        }
    }
}

#[test]
#[ignore = "Explicit review helper: prints candidates, never edits the fixture"]
fn print_binding_write_candidates() {
    let current = inventory(&real_sources());
    assert!(current.errors.is_empty(), "{}", current.errors.join("\n"));
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"schema": 1, "allowlist": current.operations()})
        )
        .unwrap()
    );
}
