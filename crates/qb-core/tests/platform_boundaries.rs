//! L1: OS selection belongs at module boundaries, implementations under platform/.
//! Scans every source file, including orphan files and disabled feature branches.
//! Windows-only exceptions come from actual module declarations, not filename globs.
//! Literal include! edges are followed; aliases and generated code still need review.
use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Item, Meta, Token, UseTree,
    punctuated::Punctuated,
    spanned::Spanned,
    visit::{self, Visit},
};

fn contains_os(tokens: TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(id) => matches!(
            id.to_string().as_str(),
            "windows"
                | "unix"
                | "target_os"
                | "target_family"
                | "target_env"
                | "target_vendor"
                | "target_abi"
        ),
        TokenTree::Group(group) => contains_os(group.stream()),
        _ => false,
    })
}

// A disjunction containing Windows is not necessarily Windows-only.
fn requires_windows(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("windows"),
        Meta::NameValue(value) => {
            value.path.is_ident("target_os")
                && matches!(&value.value, syn::Expr::Lit(lit)
                if matches!(&lit.lit, syn::Lit::Str(s) if s.value() == "windows"))
        }
        Meta::List(list) => {
            let Ok(parts) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return false;
            };
            if list.path.is_ident("all") {
                parts.iter().any(requires_windows)
            } else if list.path.is_ident("any") {
                !parts.is_empty() && parts.iter().all(requires_windows)
            } else {
                false
            }
        }
    }
}

fn windows_module(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|meta| requires_windows(&meta))
    })
}

#[derive(Default)]
struct Scanner {
    findings: BTreeSet<(usize, String)>,
}
impl Scanner {
    fn reject(&mut self, span: Span, message: &str) {
        self.findings
            .insert((span.start().line, message.to_owned()));
    }

    // Also examine macro input: syn deliberately leaves macro bodies opaque.
    fn tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Ident(id) = token {
                let name = id.to_string();
                if matches!(name.as_str(), "cfg" | "cfg_attr") {
                    let mut next = index + 1;
                    if matches!(tokens.get(next), Some(TokenTree::Punct(p)) if p.as_char() == '!') {
                        next += 1;
                    }
                    if let Some(TokenTree::Group(group)) = tokens.get(next)
                        && contains_os(group.stream())
                    {
                        self.reject(id.span(), "OS cfg/cfg! outside a module declaration");
                    }
                }
                if name == "windows"
                    && matches!(tokens.get(index + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                    && matches!(tokens.get(index + 2), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                {
                    self.reject(
                        id.span(),
                        "windows:: outside a platform or Windows feature module",
                    );
                }
                let tail: String = tokens[index..]
                    .iter()
                    .take(9)
                    .map(ToString::to_string)
                    .collect();
                if tail.starts_with("std::os::unix") || tail.starts_with("std::os::windows") {
                    self.reject(
                        id.span(),
                        "std::os platform API outside a platform or Windows feature module",
                    );
                }
            }
            if let TokenTree::Group(group) = token {
                self.tokens(group.stream());
            }
        }
    }

    fn use_tree(&mut self, tree: &UseTree, prefix: &mut Vec<String>) {
        match tree {
            UseTree::Path(path) => {
                if prefix.is_empty() && path.ident == "windows" {
                    self.reject(
                        path.ident.span(),
                        "windows:: outside a platform or Windows feature module",
                    );
                }
                prefix.push(path.ident.to_string());
                self.use_tree(&path.tree, prefix);
                prefix.pop();
            }
            UseTree::Group(group) => {
                for item in &group.items {
                    self.use_tree(item, prefix);
                }
            }
            UseTree::Name(name) => self.use_leaf(prefix, &name.ident),
            UseTree::Rename(rename) => self.use_leaf(prefix, &rename.ident),
            UseTree::Glob(glob) => self.check_path(prefix, glob.span()),
        }
    }
    fn use_leaf(&mut self, prefix: &[String], leaf: &syn::Ident) {
        if prefix.is_empty() && leaf == "windows" {
            self.reject(
                leaf.span(),
                "windows:: outside a platform or Windows feature module",
            );
        }
        let mut path = prefix.to_vec();
        path.push(leaf.to_string());
        self.check_path(&path, leaf.span());
    }
    fn check_path(&mut self, parts: &[String], span: Span) {
        if parts
            .iter()
            .take(parts.len().saturating_sub(1))
            .any(|p| p == "windows")
        {
            self.reject(
                span,
                "windows:: outside a platform or Windows feature module",
            );
        }
        if parts.len() >= 3
            && parts[0] == "std"
            && parts[1] == "os"
            && matches!(parts[2].as_str(), "windows" | "unix")
        {
            self.reject(
                span,
                "std::os platform API outside a platform or Windows feature module",
            );
        }
    }
}
impl<'ast> Visit<'ast> for Scanner {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        // Only out-of-line declarations may select an OS implementation or
        // feature root. Inline modules remain common code, even on Windows.
        for attr in &module.attrs {
            if module.content.is_some()
                || (!attr.path().is_ident("cfg") && !attr.path().is_ident("cfg_attr"))
            {
                self.visit_attribute(attr);
            }
        }
        if let Some((_, items)) = &module.content {
            for item in items {
                self.visit_item(item);
            }
        }
    }
    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        self.tokens(attr.to_token_stream());
    }
    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.check_path(
            &path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>(),
            path.span(),
        );
        visit::visit_path(self, path);
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
        self.use_tree(&item.tree, &mut Vec::new());
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // Include the macro's own path so cfg!(...) is checked too.
        self.tokens(mac.to_token_stream());
    }
}

fn scan(source: &str) -> BTreeSet<(usize, String)> {
    let file = syn::parse_file(source).expect("source must parse; boundary guard fails closed");
    let mut scanner = Scanner::default();
    scanner.visit_file(&file);
    scanner.findings
}

fn collect_rs(directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "source symlink needs explicit boundary review: {}",
            path.display()
        );
        if path.is_dir() {
            collect_rs(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

fn module_edges(
    items: &[Item],
    base: &Path,
    module_dir: &Path,
    include_base: &Path,
    windows: bool,
    edges: &mut Vec<(PathBuf, bool)>,
) {
    for item in items {
        if let Item::Macro(item) = item
            && item.mac.path.is_ident("include")
        {
            let literal = syn::parse2::<syn::LitStr>(item.mac.tokens.clone())
                .expect("nonliteral include! needs explicit boundary review");
            edges.push((
                include_base.join(literal.value()).canonicalize().unwrap(),
                windows,
            ));
        }
        let Item::Mod(module) = item else { continue };
        let windows = windows || windows_module(&module.attrs);
        let name = module.ident.to_string();
        if let Some((_, items)) = &module.content {
            let child_dir = module_dir.join(&name);
            module_edges(items, &child_dir, &child_dir, include_base, windows, edges);
            continue;
        }
        let explicit = module.attrs.iter().find_map(|attr| {
            if !attr.path().is_ident("path") {
                return None;
            }
            let Meta::NameValue(value) = &attr.meta else {
                return None;
            };
            let syn::Expr::Lit(lit) = &value.value else {
                return None;
            };
            let syn::Lit::Str(path) = &lit.lit else {
                return None;
            };
            Some(base.join(path.value()))
        });
        let path = explicit.unwrap_or_else(|| {
            let flat = module_dir.join(format!("{name}.rs"));
            if flat.is_file() {
                flat
            } else {
                module_dir.join(name).join("mod.rs")
            }
        });
        assert!(
            path.is_file(),
            "declared module not found: {}",
            path.display()
        );
        edges.push((path.canonicalize().unwrap(), windows));
    }
}

// Track both reachability classes. A file imported by common code never earns
// an exemption merely because a Windows-only feature imports it as well.
fn reachability(roots: &[PathBuf]) -> BTreeMap<PathBuf, BTreeSet<bool>> {
    let mut reached: BTreeMap<PathBuf, BTreeSet<bool>> = BTreeMap::new();
    let mut pending: Vec<_> = roots
        .iter()
        .map(|p| (p.canonicalize().unwrap(), false))
        .collect();
    while let Some((path, windows)) = pending.pop() {
        if !reached.entry(path.clone()).or_default().insert(windows) {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        let file = syn::parse_file(&source).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let parent = path.parent().unwrap();
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let module_dir = if matches!(stem, "lib" | "main" | "mod") {
            parent.to_path_buf()
        } else {
            parent.join(stem)
        };
        module_edges(
            &file.items,
            parent,
            &module_dir,
            parent,
            windows,
            &mut pending,
        );
    }
    reached
}

#[test]
fn core_and_cli_keep_os_details_at_module_boundaries() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let roots = [
        crates.join("qb-core/src/lib.rs"),
        crates.join("quota-baton/src/main.rs"),
    ];
    let reached = reachability(&roots);
    let mut files = Vec::new();
    for root in &roots {
        collect_rs(root.parent().unwrap(), &mut files);
    }
    let mut findings = Vec::new();
    for path in files {
        let canonical = path.canonicalize().unwrap();
        let relative = path.strip_prefix(crates).unwrap();
        if relative
            .components()
            .any(|part| part.as_os_str() == "platform")
        {
            continue;
        }
        if reached
            .get(&canonical)
            .is_some_and(|classes| classes.contains(&true) && !classes.contains(&false))
        {
            continue;
        }
        for (line, message) in scan(&fs::read_to_string(&path).unwrap()) {
            findings.push(format!("{}:{line}: {message}", relative.display()));
        }
    }
    assert!(
        findings.is_empty(),
        "L1 platform boundary violations:\n{}",
        findings.join("\n")
    );
}

#[test]
fn scanner_rejects_os_cfg_in_common_items_and_logic() {
    for source in [
        "#[cfg(windows)] fn f() {}",
        "#[cfg(all(test, unix))] fn f() {}",
        "#[cfg(target_os = \"linux\")] struct S;",
        "#[cfg_attr(feature = \"x\", cfg(windows))] fn f() {}",
        "fn f() { if cfg!(windows) {} }",
        "fn f() { let _ = cfg!(any(unix, target_family = \"unix\")); }",
        "enum E { #[cfg(windows)] W }",
        "#[cfg(windows)] use crate::x;",
        "#![cfg(windows)] fn f() {}",
        "struct S { #[cfg(unix)] field: u8 }",
        "fn f() { #[cfg(target_family = \"unix\")] { work(); } }",
        "macro_rules! hidden { () => { #[cfg(unix)] fn f() {} }; }",
        "macro_rules! hidden { () => { if cfg!(target_os = \"linux\") {} }; }",
        "#[cfg(any(test, windows))] mod mixed { fn f() { let _ = cfg!(windows); } }",
    ] {
        assert!(!scan(source).is_empty(), "missed violation: {source}");
    }
}

#[test]
fn scanner_rejects_platform_paths_and_grouped_or_renamed_imports() {
    for source in [
        "use windows::Win32::Foundation::HANDLE;",
        "fn f() { windows::foo(); }",
        "fn f() { crate::platform::windows::foo(); }",
        "use crate::platform::windows::foo;",
        "use std::os::unix::fs::MetadataExt;",
        "use std::{os::{unix::fs::MetadataExt}};",
        "use std::os::{windows as native};",
        "use windows as native;",
        "fn f(_: std::os::windows::io::RawHandle) {}",
        "macro_rules! hidden { () => { windows::foo() }; }",
    ] {
        assert!(!scan(source).is_empty(), "missed platform API: {source}");
    }
}

#[test]
fn scanner_allows_feature_cfg_comments_strings_and_module_roots() {
    for source in [
        "#[cfg(feature = \"testkit\")] fn f() {}",
        "#[cfg(test)] mod tests { #[test] fn t() {} }",
        "fn f(windows: Vec<u8>) { let _ = windows.iter(); }",
        "struct S { windows: usize }",
        "fn f() { let _ = serde_json::json!({\"windows\": 1}); }",
        "// cfg!(windows) windows::x\nfn f() { let _ = \"std::os::unix cfg!(unix)\"; }",
        "fn f() { let _ = r#\"windows:: cfg!(windows)\"#; }",
        "#[cfg(windows)] mod feature;",
        "#[cfg(unix)] mod feature;",
        "#[cfg(all(test, windows))] mod tests;",
        "#[cfg(target_os = \"windows\")] mod feature;",
    ] {
        assert!(scan(source).is_empty(), "false positive: {source}");
    }
}

#[test]
fn scanner_rejects_inline_os_modules_including_windows_feature_roots() {
    for source in [
        "#[cfg(windows)] mod feature {}",
        "#[cfg(unix)] pub mod feature {}",
        "#[cfg(all(test, windows))] mod tests { #[test] fn t() {} }",
        "#[cfg(target_os = \"linux\")] mod feature { fn f() {} }",
        "#[cfg(target_os = \"windows\")] mod feature { fn f() {} }",
        "#[cfg_attr(feature = \"x\", cfg(windows))] mod feature {}",
        "mod common { #[cfg(windows)] mod feature {} }",
        "#[cfg(windows)] mod feature { #[cfg(unix)] fn hidden() {} }",
    ] {
        assert!(
            !scan(source).is_empty(),
            "inline OS module escaped: {source}"
        );
    }
}

#[test]
fn windows_only_detection_does_not_exempt_disjunctions_or_unix_logic() {
    for source in [
        "#[cfg(any(windows, unix))] mod shared { use windows::Win32; }",
        "#[cfg(unix)] mod misplaced { use std::os::unix::fs::MetadataExt; }",
        "#[cfg_attr(feature = \"x\", cfg(windows))] mod conditional { use windows::Win32; }",
    ] {
        assert!(
            !scan(source).is_empty(),
            "overbroad feature exemption: {source}"
        );
    }
}
