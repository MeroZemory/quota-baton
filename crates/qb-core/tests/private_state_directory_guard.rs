//! Raw mkdir calls in core/CLI production code require a finite, reviewed entry.
//! Walk both OS/feature branches, including orphan sources. Only proven test-only
//! code is excluded; neither platform directories nor Windows feature roots earn
//! a blanket exemption. This is a source guard, not a replacement for native tests.
use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Expr, ImplItem, Item, Meta, Stmt, Token, TraitItem, UseTree,
    punctuated::Punctuated,
    spanned::Spanned,
    visit::{self, Visit},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Truth {
    Yes,
    No,
    Unknown,
}

// Evaluate only the fact test=false. OS/features stay unknown so the host and
// currently enabled Cargo features cannot make production branches disappear.
fn without_test(meta: &Meta) -> Truth {
    use Truth::*;
    match meta {
        Meta::Path(path) if path.is_ident("test") => No,
        Meta::List(list) => {
            let Ok(parts) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return Unknown;
            };
            let values: Vec<_> = parts.iter().map(without_test).collect();
            if list.path.is_ident("all") {
                if values.contains(&No) {
                    No
                } else if values.iter().all(|v| *v == Yes) {
                    Yes
                } else {
                    Unknown
                }
            } else if list.path.is_ident("any") {
                if values.contains(&Yes) {
                    Yes
                } else if values.iter().all(|v| *v == No) {
                    No
                } else {
                    Unknown
                }
            } else if list.path.is_ident("not") && values.len() == 1 {
                match values[0] {
                    Yes => No,
                    No => Yes,
                    Unknown => Unknown,
                }
            } else {
                Unknown
            }
        }
        _ => Unknown,
    }
}

fn attribute_excludes_production(meta: &Meta) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    if list.path.is_ident("cfg") {
        return list
            .parse_args::<Meta>()
            .is_ok_and(|cfg| without_test(&cfg) == Truth::No);
    }
    if list.path.is_ident("cfg_attr")
        && let Ok(parts) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        && let Some(condition) = parts.first()
        && without_test(condition) == Truth::Yes
    {
        return parts.iter().skip(1).any(attribute_excludes_production);
    }
    false
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let name = attr.path().to_token_stream().to_string();
        name == "test" || name == "tokio :: test" || attribute_excludes_production(&attr.meta)
    })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(v) => &v.attrs,
        Item::Enum(v) => &v.attrs,
        Item::ExternCrate(v) => &v.attrs,
        Item::Fn(v) => &v.attrs,
        Item::ForeignMod(v) => &v.attrs,
        Item::Impl(v) => &v.attrs,
        Item::Macro(v) => &v.attrs,
        Item::Mod(v) => &v.attrs,
        Item::Static(v) => &v.attrs,
        Item::Struct(v) => &v.attrs,
        Item::Trait(v) => &v.attrs,
        Item::TraitAlias(v) => &v.attrs,
        Item::Type(v) => &v.attrs,
        Item::Union(v) => &v.attrs,
        Item::Use(v) => &v.attrs,
        _ => &[],
    }
}

fn expr_attrs(expr: &Expr) -> &[Attribute] {
    macro_rules! attrs {
        ($($variant:ident),* $(,)?) => {
            match expr {
                $(Expr::$variant(v) => &v.attrs,)*
                _ => &[],
            }
        };
    }
    attrs!(
        Array, Assign, Async, Await, Binary, Block, Break, Call, Cast, Closure, Const, Continue,
        Field, ForLoop, Group, If, Index, Infer, Let, Lit, Loop, Macro, Match, MethodCall, Paren,
        Path, Range, RawAddr, Reference, Repeat, Return, Struct, Try, TryBlock, Tuple, Unary,
        Unsafe, While, Yield,
    )
}

fn compact(tokens: impl ToTokens) -> String {
    tokens.to_token_stream().to_string()
}

// Import aliases and type aliases must not hide a constructor or function value.
// Collect conservatively across a file; shadowing may require review but cannot
// silently introduce a bypass. Literal strings/comments are never API matches.
struct Names {
    mkdir: BTreeSet<String>,
    builders: BTreeSet<String>,
}
impl Default for Names {
    fn default() -> Self {
        Self {
            mkdir: [
                "create_dir",
                "create_dir_all",
                "mkdirat",
                "mkdir",
                "CreateDirectoryW",
                "CreateDirectoryA",
            ]
            .map(str::to_owned)
            .into(),
            builders: ["DirBuilder"].map(str::to_owned).into(),
        }
    }
}
impl Names {
    fn alias(&mut self, old: &str, new: &str) {
        if self.mkdir.contains(old) {
            self.mkdir.insert(new.to_owned());
        }
        if self.builders.contains(old) {
            self.builders.insert(new.to_owned());
        }
    }
    fn path_is_mkdir(&self, path: &syn::Path) -> bool {
        path.segments
            .last()
            .is_some_and(|s| self.mkdir.contains(&s.ident.to_string()))
    }
    fn path_is_builder(&self, path: &syn::Path) -> bool {
        path.segments
            .iter()
            .any(|s| self.builders.contains(&s.ident.to_string()))
    }
}
impl<'ast> Visit<'ast> for Names {
    fn visit_item(&mut self, item: &'ast Item) {
        if !test_only(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if !test_only(expr_attrs(expr)) {
            visit::visit_expr(self, expr);
        }
    }
    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        if !test_only(&function.attrs) {
            visit::visit_impl_item_fn(self, function);
        }
    }
    fn visit_trait_item_fn(&mut self, function: &'ast syn::TraitItemFn) {
        if !test_only(&function.attrs) {
            visit::visit_trait_item_fn(self, function);
        }
    }
    fn visit_use_tree(&mut self, tree: &'ast UseTree) {
        if let UseTree::Rename(rename) = tree {
            self.alias(&rename.ident.to_string(), &rename.rename.to_string());
        }
        visit::visit_use_tree(self, tree);
    }
    fn visit_item_type(&mut self, alias: &'ast syn::ItemType) {
        if let syn::Type::Path(path) = &*alias.ty
            && self.path_is_builder(&path.path)
        {
            self.builders.insert(alias.ident.to_string());
        }
        visit::visit_item_type(self, alias);
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Site {
    file: String,
    context: String,
    expression: String,
    line: usize,
}

struct Scanner {
    file: String,
    context: Vec<String>,
    names: Names,
    sites: Vec<Site>,
}
impl Scanner {
    fn record(&mut self, expression: impl ToTokens, line: usize) {
        self.sites.push(Site {
            file: self.file.clone(),
            context: self.context.join("::"),
            expression: compact(expression),
            line,
        });
    }
    fn raw_import(&self, tree: &UseTree, plain: bool) -> bool {
        match tree {
            UseTree::Path(v) => self.raw_import(&v.tree, plain),
            UseTree::Group(v) => v.items.iter().any(|v| self.raw_import(v, plain)),
            UseTree::Name(v) => {
                plain
                    && (self.names.mkdir.contains(&v.ident.to_string())
                        || self.names.builders.contains(&v.ident.to_string()))
            }
            UseTree::Rename(v) => {
                self.names.mkdir.contains(&v.ident.to_string())
                    || self.names.builders.contains(&v.ident.to_string())
            }
            UseTree::Glob(_) => false,
        }
    }
    fn builder_chain(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Path(path) => self.names.path_is_builder(&path.path),
            Expr::Call(call) => self.builder_chain(&call.func),
            Expr::MethodCall(call) => self.builder_chain(&call.receiver),
            Expr::Paren(v) => self.builder_chain(&v.expr),
            Expr::Group(v) => self.builder_chain(&v.expr),
            _ => false,
        }
    }
    fn macro_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for token in &tokens {
            if let TokenTree::Ident(id) = token {
                let name = id.to_string();
                if self.names.builders.contains(&name) || self.names.mkdir.contains(&name) {
                    // Macro inputs are deliberately fail-closed. Never give a
                    // raw operation hidden in opaque tokens a platform exemption.
                    self.record(id, id.span().start().line);
                }
            }
            if let TokenTree::Group(group) = token {
                self.macro_tokens(group.stream());
            }
        }
    }
}
impl<'ast> Visit<'ast> for Scanner {
    fn visit_item(&mut self, item: &'ast Item) {
        if !test_only(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        // Also reject new re-exported aliases: callers in a different source
        // file could otherwise hide the raw API's original name entirely.
        if self.raw_import(&item.tree, !matches!(item.vis, syn::Visibility::Inherited)) {
            self.record(item, item.span().start().line);
        }
    }
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        self.context.push(module.ident.to_string());
        if let Some((_, items)) = &module.content {
            for item in items {
                self.visit_item(item);
            }
        }
        self.context.pop();
    }
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        self.context.push(function.sig.ident.to_string());
        visit::visit_item_fn(self, function);
        self.context.pop();
    }
    fn visit_item_impl(&mut self, implementation: &'ast syn::ItemImpl) {
        self.context
            .push(format!("impl {}", compact(&implementation.self_ty)));
        visit::visit_item_impl(self, implementation);
        self.context.pop();
    }
    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        let attrs = match item {
            ImplItem::Const(v) => &v.attrs,
            ImplItem::Fn(v) => &v.attrs,
            ImplItem::Type(v) => &v.attrs,
            ImplItem::Macro(v) => &v.attrs,
            _ => return visit::visit_impl_item(self, item),
        };
        if !test_only(attrs) {
            visit::visit_impl_item(self, item);
        }
    }
    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        self.context.push(function.sig.ident.to_string());
        visit::visit_impl_item_fn(self, function);
        self.context.pop();
    }
    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        let attrs = match item {
            TraitItem::Const(v) => &v.attrs,
            TraitItem::Fn(v) => &v.attrs,
            TraitItem::Type(v) => &v.attrs,
            TraitItem::Macro(v) => &v.attrs,
            _ => return visit::visit_trait_item(self, item),
        };
        if !test_only(attrs) {
            visit::visit_trait_item(self, item);
        }
    }
    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if !test_only(&arm.attrs) {
            visit::visit_arm(self, arm);
        }
    }
    fn visit_field(&mut self, field: &'ast syn::Field) {
        if !test_only(&field.attrs) {
            visit::visit_field(self, field);
        }
    }
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::Local(v) if test_only(&v.attrs) => return,
            Stmt::Macro(v) if test_only(&v.attrs) => return,
            _ => {}
        }
        visit::visit_stmt(self, stmt);
    }
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if !test_only(expr_attrs(expr)) {
            visit::visit_expr(self, expr);
        }
    }
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = &*call.func
            && (self.names.path_is_mkdir(&path.path) || self.names.path_is_builder(&path.path))
        {
            self.record(call, call.span().start().line);
            for arg in &call.args {
                self.visit_expr(arg);
            }
        } else {
            visit::visit_expr_call(self, call);
        }
    }
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if self.builder_chain(&call.receiver) || self.names.mkdir.contains(&call.method.to_string())
        {
            self.record(call, call.span().start().line);
            for arg in &call.args {
                self.visit_expr(arg);
            }
        } else {
            visit::visit_expr_method_call(self, call);
        }
    }
    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if self.names.path_is_mkdir(&path.path) || self.names.path_is_builder(&path.path) {
            self.record(path, path.span().start().line);
        }
        visit::visit_expr_path(self, path);
    }
    fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
        if self.names.path_is_builder(&path.path) {
            self.record(path, path.span().start().line);
        }
        visit::visit_type_path(self, path);
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.macro_tokens(mac.tokens.clone());
    }
}

fn scan(file: &str, source: &str) -> Vec<Site> {
    let syntax = syn::parse_file(source).expect("mkdir guard fails closed on invalid Rust");
    if test_only(&syntax.attrs) {
        return Vec::new();
    }
    let mut names = Names::default();
    // Aliases can be declared in either order. Iterate until the finite sets stop growing.
    loop {
        let count = names.mkdir.len() + names.builders.len();
        names.visit_file(&syntax);
        if names.mkdir.len() + names.builders.len() == count {
            break;
        }
    }
    let mut scanner = Scanner {
        file: file.to_owned(),
        context: Vec::new(),
        names,
        sites: Vec::new(),
    };
    scanner.visit_file(&syntax);
    scanner.sites
}

// Build test reachability from real declarations rather than filenames. A file
// that has any production importer remains production even if tests import it too.
struct Edges {
    base: PathBuf,
    module_dir: PathBuf,
    include_base: PathBuf,
    test: bool,
    edges: Vec<(PathBuf, bool)>,
}
impl Edges {
    fn add(&mut self, path: PathBuf) {
        self.edges.push((
            path.canonicalize()
                .unwrap_or_else(|e| panic!("declared source {}: {e}", path.display())),
            self.test,
        ));
    }
}
impl<'ast> Visit<'ast> for Edges {
    fn visit_item(&mut self, item: &'ast Item) {
        let previous = self.test;
        self.test |= test_only(item_attrs(item));
        visit::visit_item(self, item);
        self.test = previous;
    }
    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        let previous = self.test;
        self.test |= test_only(&arm.attrs);
        visit::visit_arm(self, arm);
        self.test = previous;
    }
    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        let previous = self.test;
        let attrs = match item {
            TraitItem::Const(v) => &v.attrs,
            TraitItem::Fn(v) => &v.attrs,
            TraitItem::Type(v) => &v.attrs,
            TraitItem::Macro(v) => &v.attrs,
            _ => return visit::visit_trait_item(self, item),
        };
        self.test |= test_only(attrs);
        visit::visit_trait_item(self, item);
        self.test = previous;
    }
    fn visit_expr(&mut self, expr: &'ast Expr) {
        let previous = self.test;
        self.test |= test_only(expr_attrs(expr));
        visit::visit_expr(self, expr);
        self.test = previous;
    }
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let previous = self.test;
        match stmt {
            Stmt::Local(v) => self.test |= test_only(&v.attrs),
            Stmt::Macro(v) => self.test |= test_only(&v.attrs),
            _ => {}
        }
        visit::visit_stmt(self, stmt);
        self.test = previous;
    }
    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        let previous = self.test;
        let attrs = match item {
            ImplItem::Const(v) => &v.attrs,
            ImplItem::Fn(v) => &v.attrs,
            ImplItem::Type(v) => &v.attrs,
            ImplItem::Macro(v) => &v.attrs,
            _ => return visit::visit_impl_item(self, item),
        };
        self.test |= test_only(attrs);
        visit::visit_impl_item(self, item);
        self.test = previous;
    }
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if let Some((_, items)) = &module.content {
            let previous_base = self.base.clone();
            let previous_dir = self.module_dir.clone();
            self.module_dir.push(module.ident.to_string());
            self.base = self.module_dir.clone();
            for item in items {
                self.visit_item(item);
            }
            self.base = previous_base;
            self.module_dir = previous_dir;
        } else {
            let explicit = module.attrs.iter().find_map(|attr| {
                if !attr.path().is_ident("path") {
                    return None;
                }
                let Meta::NameValue(value) = &attr.meta else {
                    return None;
                };
                let syn::Expr::Lit(value) = &value.value else {
                    return None;
                };
                let syn::Lit::Str(path) = &value.lit else {
                    return None;
                };
                Some(self.base.join(path.value()))
            });
            let path = explicit.unwrap_or_else(|| {
                let flat = self.module_dir.join(format!("{}.rs", module.ident));
                if flat.is_file() {
                    flat
                } else {
                    self.module_dir
                        .join(module.ident.to_string())
                        .join("mod.rs")
                }
            });
            self.add(path);
        }
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.is_ident("include") {
            let literal = syn::parse2::<syn::LitStr>(mac.tokens.clone())
                .expect("nonliteral include! needs explicit mkdir boundary review");
            self.add(self.include_base.join(literal.value()));
        }
    }
}

fn classes(roots: &[PathBuf]) -> BTreeMap<PathBuf, BTreeSet<bool>> {
    let mut found: BTreeMap<PathBuf, BTreeSet<bool>> = BTreeMap::new();
    let mut pending: Vec<_> = roots
        .iter()
        .map(|p| (p.canonicalize().unwrap(), false))
        .collect();
    while let Some((path, inherited_test)) = pending.pop() {
        let source = fs::read_to_string(&path).unwrap();
        let syntax = syn::parse_file(&source).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let test = inherited_test || test_only(&syntax.attrs);
        if !found.entry(path.clone()).or_default().insert(test) {
            continue;
        }
        let parent = path.parent().unwrap();
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let module_dir = if matches!(stem, "lib" | "main" | "mod") {
            parent.to_path_buf()
        } else {
            parent.join(stem)
        };
        let mut edges = Edges {
            base: parent.to_path_buf(),
            module_dir,
            include_base: parent.to_path_buf(),
            test,
            edges: Vec::new(),
        };
        edges.visit_file(&syntax);
        pending.extend(edges.edges);
    }
    found
}

fn collect(directory: &Path, files: &mut Vec<PathBuf>) {
    let mut paths: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|v| v.unwrap().path())
        .collect();
    paths.sort();
    for path in paths {
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "source symlink requires explicit mkdir review: {}",
            path.display()
        );
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}

struct Allowed {
    file: &'static str,
    context: &'static str,
    expression: &'static str,
    reason: &'static str,
}

// Exactly one occurrence per entry. No module, filename-glob, platform-directory
// or function-wide exemption: another call in the same function still fails.
const ALLOWED: &[Allowed] = &[
    Allowed {
        file: "qb-core/src/platform/windows/private_directory.rs",
        context: "create_leaf",
        expression: "CreateDirectoryW(PCWSTR(wide_path.as_ptr()), Some(&attributes))",
        reason: "Atomic Windows private leaf creation supplies an explicit current-user owner and protected current-user-only DACL through SECURITY_ATTRIBUTES; it never creates with an inherited/default broad ACL.",
    },
    Allowed {
        file: "qb-core/src/platform/unix/atomic.rs",
        context: "create_private_directory_new",
        expression: "std::fs::DirBuilder::new().mode(0o700).create(path)",
        reason: "Exclusive Unix leaf uses mode 0700, then checks directory type, current owner and exact permissions.",
    },
    Allowed {
        file: "qb-core/src/platform/unix/private_fs.rs",
        context: "mkdir_private",
        expression: "libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700)",
        reason: "Existing descriptor-anchored Unix private primitive uses mode 0700 and opens/verifies the child via the checked parent descriptor; no general libc exemption.",
    },
    Allowed {
        file: "qb-core/src/platform/unix/box_sockets/transport.rs",
        context: "impl SocketDirectory::open",
        expression: "libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o750)",
        reason: "Runtime IPC socket directory under /run intentionally shares traversal with the socket group (0750); this descriptor-anchored native socket primitive does not create private data directories.",
    },
];

fn unreviewed(sites: &[Site], allowed: &[Allowed]) -> Vec<String> {
    let normalized: Vec<_> = allowed
        .iter()
        .map(|entry| {
            assert!(
                !entry.reason.trim().is_empty(),
                "mkdir exemption needs a reason"
            );
            entry.expression.parse::<TokenStream>().unwrap().to_string()
        })
        .collect();
    let mut used = vec![0; allowed.len()];
    let mut errors = Vec::new();
    for site in sites {
        if let Some(i) = allowed.iter().enumerate().position(|(i, entry)| {
            site.file == entry.file
                && site.context == entry.context
                && site.expression == normalized[i]
        }) {
            used[i] += 1;
        } else {
            errors.push(format!(
                "{}:{} [{}] {}",
                site.file, site.line, site.context, site.expression
            ));
        }
    }
    for (entry, count) in allowed.iter().zip(used) {
        if count != 1 {
            errors.push(format!(
                "reviewed inventory entry {} [{}] {}: expected once, found {count}; {}",
                entry.file, entry.context, entry.expression, entry.reason
            ));
        }
    }
    errors
}

#[test]
fn production_data_directories_have_no_unreviewed_raw_mkdir() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let roots = [
        crates.join("qb-core/src/lib.rs"),
        crates.join("quota-baton/src/main.rs"),
    ];
    let reached = classes(&roots);
    let mut files = Vec::new();
    for root in &roots {
        collect(root.parent().unwrap(), &mut files);
    }
    let mut sites = Vec::new();
    for path in files {
        if reached
            .get(&path.canonicalize().unwrap())
            .is_some_and(|set| set.contains(&true) && !set.contains(&false))
        {
            continue;
        }
        let relative = path
            .strip_prefix(crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        sites.extend(scan(&relative, &fs::read_to_string(&path).unwrap()));
    }
    sites.sort();
    let errors = unreviewed(&sites, ALLOWED);
    assert!(
        errors.is_empty(),
        "Unreviewed production mkdir or stale reviewed inventory:\n{}\nUse the secure platform primitive, or add a precise reviewed site with a reason.\nObserved production sites: {sites:#?}",
        errors.join("\n")
    );
}

#[test]
fn planted_state_mkdir_aliases_builders_and_macro_bypasses_are_rejected() {
    for source in [
        "fn planted(home: &Path) { std::fs::create_dir_all(home.join(\"state\")).unwrap(); }",
        "fn planted() { fs::create_dir(path)?; }",
        "fn planted() { unsafe { libc::mkdirat(fd, name, 0o755) }; }",
        "macro_rules! planted { () => { use std::fs::create_dir_all as make; make(state)?; }; }",
        "use libc::mkdirat as make; fn planted() { unsafe { make(fd, name, 0o700) }; }",
        "use std::fs::create_dir_all as mkdir; fn planted() { mkdir(state)?; }",
        "pub use std::fs::create_dir_all as exported_mkdir;",
        "fn planted() { let mkdir = fs::create_dir_all; mkdir(state)?; }",
        "use std::fs::DirBuilder as Builder; fn planted() { Builder::new().recursive(true).create(state)?; }",
        "type Builder = std::fs::DirBuilder; fn planted(builder: Builder) { builder.create(state)?; }",
        "fn planted() { std::fs::DirBuilder::new().mode(0o755).create(state)?; }",
        "macro_rules! planted { () => { std::fs::create_dir_all(state)?; }; }",
        "#[cfg(any(test, feature = \"serve\"))] fn planted() { fs::create_dir_all(state)?; }",
        "#[cfg(windows)] mod feature { fn planted() { fs::create_dir_all(state)?; } }",
        "#[cfg(not(test))] fn planted() { fs::create_dir_all(state)?; }",
        "fn planted() { unsafe { CreateDirectoryW(path, None) }; }",
        "use windows::Win32::Storage::FileSystem::CreateDirectoryW as make; fn planted() { unsafe { make(path, None) }; }",
        "macro_rules! planted { () => { CreateDirectoryW(path, None) }; }",
        "fn planted() { unsafe { CreateDirectoryA(path, None) }; }",
        "use windows::Win32::Storage::FileSystem::CreateDirectoryA as make; fn planted() { unsafe { make(path, None) }; }",
        "macro_rules! planted { () => { CreateDirectoryA(path, None) }; }",
        "fn planted() { unsafe { libc::mkdir(path, 0o755) }; }",
        "use libc::mkdir as make; fn planted() { unsafe { make(path, 0o700) }; }",
    ] {
        let sites = scan("qb-core/src/planted.rs", source);
        assert!(!sites.is_empty(), "missed mkdir: {source}");
        assert!(
            !unreviewed(&sites, &[]).is_empty(),
            "accepted bypass: {source}"
        );
    }
}

#[test]
fn test_only_setup_comments_and_strings_are_not_production_mkdirs() {
    for source in [
        "#[cfg(test)] mod tests { fn fixture() { fs::create_dir_all(state)?; } }",
        "#[cfg(test)] mod tests { use std::fs::create_dir_all as new; } fn f() { Type::new(); }",
        "#[cfg(all(test, feature = \"testkit\"))] fn fixture() { fs::create_dir_all(state)?; }",
        "#![cfg(test)] fn fixture() { fs::create_dir_all(state)?; }",
        "#[test] fn fixture() { fs::create_dir_all(state)?; }",
        "fn f() { #[cfg(test)] { fs::create_dir_all(state)?; } }",
        "fn f() { match value { #[cfg(test)] 0 => { fs::create_dir_all(state)?; }, _ => {} } }",
        "#[cfg_attr(not(test), cfg(test))] fn fixture() { fs::create_dir_all(state)?; }",
        "fn f() { let _ = \"fs::create_dir_all(state) DirBuilder\"; /* fs::create_dir(state) */ }",
    ] {
        assert!(
            scan("qb-core/src/fixture.rs", source).is_empty(),
            "fixture counted as product: {source}"
        );
    }
}

#[test]
fn platform_allowance_is_exact_and_cannot_hide_new_or_duplicate_calls() {
    let source = "use windows::Win32::Storage::FileSystem::CreateDirectoryW; fn create_leaf() { CreateDirectoryW(PCWSTR(wide_path.as_ptr()), Some(&attributes)); }";
    let file = "qb-core/src/platform/windows/private_directory.rs";
    assert!(unreviewed(&scan(file, source), &ALLOWED[..1]).is_empty());
    assert!(
        !unreviewed(
            &scan("qb-core/src/platform/windows/new.rs", source),
            &ALLOWED[..1]
        )
        .is_empty()
    );
    let duplicate = "fn create_leaf() { CreateDirectoryW(PCWSTR(wide_path.as_ptr()), Some(&attributes)); CreateDirectoryW(PCWSTR(wide_path.as_ptr()), Some(&attributes)); }";
    assert!(!unreviewed(&scan(file, duplicate), &ALLOWED[..1]).is_empty());
    let new_state = "fn create_leaf() { CreateDirectoryW(PCWSTR(wide_path.as_ptr()), Some(&attributes)); fs::create_dir_all(path.join(\"state\")); }";
    assert!(!unreviewed(&scan(file, new_state), &ALLOWED[..1]).is_empty());
    assert!(
        !unreviewed(&[], &ALLOWED[..1]).is_empty(),
        "stale exemption silently retained"
    );
}

#[test]
fn descriptor_primitive_and_private_builder_exemptions_keep_exact_modes() {
    for (index, function, good, bad) in [
        (
            1,
            "create_private_directory_new",
            "std::fs::DirBuilder::new().mode(0o700).create(path)",
            "std::fs::DirBuilder::new().mode(0o755).create(path)",
        ),
        (
            2,
            "mkdir_private",
            "libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700)",
            "libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755)",
        ),
    ] {
        let allowance = &ALLOWED[index..=index];
        let source = format!("fn {function}() {{ {good}; }}");
        assert!(unreviewed(&scan(allowance[0].file, &source), allowance).is_empty());
        let source = format!("fn {function}() {{ {bad}; }}");
        assert!(!unreviewed(&scan(allowance[0].file, &source), allowance).is_empty());
    }
}
