//! Bounded source regression, reviewed at db557c076aa3e468eec7cc6ca9cb62af9ef4e8dc.
//! This is syntax analysis, not an interpreter: unknown dynamic producers fail.
//! Production module traversal includes all feature/platform variants, skips
//! test-required modules, and fails on unresolved/ambiguous module placement.
//! This bounded walker does not resolve Rust names or expand macros: native
//! producer aliases and opaque macro references require explicit classification.
//! Ordinary imports and composite types containing Window are not constructor
//! aliases; only direct type aliases and renamed imports are rejected.

use std::{collections::BTreeSet, path::Path};

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::{Expr, Item, Pat, visit::Visit};

const HEAD: &str = "db557c076aa3e468eec7cc6ca9cb62af9ef4e8dc";
const HEADERS: &str = include_str!("../../observability.rs");
const POLL: &str = include_str!("../../quota/parse.rs");
const HEALTH: &str = include_str!("../../accounts/health.rs");
const RELAY: &str = include_str!("../../relay.rs");
const PLACEMENT: &str = include_str!("../../bindings.rs");

fn tokens(value: &impl ToTokens) -> String {
    value.to_token_stream().to_string()
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    fn requires_test(meta: &syn::Meta) -> bool {
        match meta {
            syn::Meta::Path(path) => path.is_ident("test"),
            syn::Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
                let children = list
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                    )
                    .expect("cfg child predicates parse");
                if list.path.is_ident("all") {
                    children.iter().any(requires_test)
                } else {
                    !children.is_empty() && children.iter().all(requires_test)
                }
            }
            // Feature/target predicates and negation do not prove test is
            // required. In particular any(test, feature = ...) stays scanned.
            _ => false,
        }
    }
    attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        requires_test(&a.parse_args::<syn::Meta>().expect("cfg predicate parses"))
    })
}

#[derive(Default)]
struct Vocabulary {
    values: BTreeSet<String>,
    sites: Vec<String>,
    families: BTreeSet<String>,
    forwarded: usize,
}

impl Vocabulary {
    fn value(&mut self, value: String, origin: &str) {
        self.sites.push(format!("{origin}: {value}"));
        self.values.insert(value);
    }

    fn expression(&mut self, expr: &Expr, origin: &str) {
        match expr {
            Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => {
                self.value(s.value(), origin);
            }
            Expr::Reference(e) => self.expression(&e.expr, origin),
            Expr::MethodCall(e)
                if e.args.is_empty()
                    && matches!(e.method.to_string().as_str(), "into" | "to_owned") =>
            {
                self.expression(&e.receiver, origin);
            }
            Expr::Match(e) => {
                for arm in &e.arms {
                    self.expression(&arm.body, origin);
                }
            }
            Expr::Continue(_) => {}
            Expr::Macro(e) if e.mac.path.is_ident("format") => {
                let actual = tokens(&e.mac.tokens);
                // Exact expression classification prevents accepting a new format,
                // arbitrary interpolation, or a changed divisor without review.
                let (family, samples): (&str, &[&str]) = match actual.as_str() {
                    "\"{n}m\"" => ("u32 minutes", &["0m", "1m", "43200m", "4294967295m"]),
                    "\"{}m\" , n / 60" => ("u64 seconds / 60", &["1m", "307445734561825860m"]),
                    "\"{n}s\"" => ("u64 seconds", &["1s", "59s", "18446744073709551615s"]),
                    _ => panic!("unclassified window format at {origin}: {actual}"),
                };
                self.families.insert(family.into());
                for sample in samples {
                    self.value((*sample).into(), origin);
                }
            }
            Expr::Path(e) if e.path.is_ident("slot") => {
                // Its literal loop inventory is independently extracted below.
                self.forwarded += 1;
            }
            _ => panic!(
                "unclassified window expression at {origin}: {}",
                tokens(expr)
            ),
        }
    }

    fn refused(&self) -> Vec<&str> {
        self.values
            .iter()
            .filter(|v| super::Window::parse(v).is_err())
            .map(String::as_str)
            .collect()
    }
}

struct Origins<'a> {
    file: &'a str,
    function: String,
    vocabulary: &'a mut Vocabulary,
    cooling: usize,
    header_windows: usize,
    poll_calls: usize,
    forecast_loop_depth: usize,
    forecast_copies: usize,
}

impl Origins<'_> {
    fn origin(&self) -> String {
        format!(
            "{}::{}",
            self.file,
            if self.function.is_empty() {
                "<module>"
            } else {
                &self.function
            }
        )
    }
}

impl<'ast> Visit<'ast> for Origins<'_> {
    fn visit_use_rename(&mut self, alias: &'ast syn::UseRename) {
        assert!(
            alias.ident != "Window" && alias.ident != "QuotaCooling",
            "native producer alias requires classification at {}: {}",
            self.origin(),
            tokens(alias)
        );
        syn::visit::visit_use_rename(self, alias);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if test_only(&item.attrs) {
            return;
        }
        // A tuple such as outside_owner::Projected merely contains Window;
        // its alias cannot name a Window constructor. Inspect direct aliases,
        // not every nested type path in a composite container.
        let mut target = item.ty.as_ref();
        loop {
            target = match target {
                syn::Type::Paren(ty) => &ty.elem,
                syn::Type::Group(ty) => &ty.elem,
                _ => break,
            };
        }
        assert!(
            !matches!(target, syn::Type::Path(path) if path.path.segments.last().is_some_and(|s| s.ident == "Window" || s.ident == "QuotaCooling")),
            "native producer alias requires classification at {}: {} = {}",
            self.origin(),
            item.ident,
            tokens(&item.ty)
        );
        syn::visit::visit_item_type(self, item);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !test_only(&item.attrs) {
            syn::visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        let saved = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        syn::visit::visit_item_fn(self, item);
        self.function = saved;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        let saved = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, item);
        self.function = saved;
    }

    fn visit_expr_for_loop(&mut self, expr: &'ast syn::ExprForLoop) {
        let origin = self.origin();
        let forecast_source = self.file == "observability/forecast_log.rs"
            && self.function == "input"
            && tokens(&expr.pat) == "w"
            && tokens(&expr.expr) == "& a . windows";
        if self.function == "snapshot" && matches!(self.file, "observability.rs" | "quota/parse.rs")
        {
            if let Pat::Ident(pat) = expr.pat.as_ref() {
                if matches!(pat.ident.to_string().as_str(), "name" | "slot") {
                    let Expr::Array(array) = expr.expr.as_ref() else {
                        panic!("unclassified native loop: {}", self.origin());
                    };
                    for value in &array.elems {
                        self.vocabulary.expression(value, &origin);
                    }
                }
            } else if tokens(&expr.pat) == "(key , name)" {
                assert_eq!(
                    tokens(&expr.expr),
                    "CLAUDE_WINDOWS",
                    "poll tuple source changed"
                );
            }
        }
        if forecast_source {
            self.forecast_loop_depth += 1;
        }
        syn::visit::visit_expr_for_loop(self, expr);
        if forecast_source {
            self.forecast_loop_depth -= 1;
        }
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        let origin = self.origin();
        if self.function == "snapshot"
            && matches!(self.file, "observability.rs" | "quota/parse.rs")
            && let Pat::Ident(pat) = &local.pat
            && pat.ident == "name"
        {
            self.vocabulary.expression(
                &local.init.as_ref().expect("name initializer").expr,
                &origin,
            );
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        let origin = self.origin();
        let kind = expr.path.segments.last().unwrap().ident.to_string();
        let snapshot_window = kind == "Window"
            && expr.fields.iter().any(
                |f| matches!(&f.member, syn::Member::Named(n) if n == "name" || n == "used_pct"),
            );
        if snapshot_window
            && self.file == "observability/forecast_log.rs"
            && self.function == "input"
            && self.forecast_loop_depth > 0
        {
            // A different local Window type journals existing admin evidence.
            // Classify only this exact copy, inside the verified admin loop;
            // new literals, other constructors, and altered inputs still fail.
            let copy: syn::ExprStruct = syn::parse_quote!(Window {
                name: w.name.clone(),
                used_pct: valid,
                observed_at: observed,
                reset_at: reset,
            });
            assert_eq!(tokens(expr), tokens(&copy), "forecast Window copy changed");
            self.forecast_copies += 1;
            syn::visit::visit_expr_struct(self, expr);
            return;
        }
        if kind == "QuotaCooling" || snapshot_window {
            let field = if kind == "QuotaCooling" {
                "window"
            } else {
                "name"
            };
            let value = &expr
                .fields
                .iter()
                .find(|f| matches!(&f.member, syn::Member::Named(n) if n == field))
                .expect("native window field must remain explicit")
                .expr;
            let dynamic = tokens(value);
            if kind == "QuotaCooling" {
                self.cooling += 1;
                if self.file == "accounts/health.rs" && dynamic == "name . into ()" {
                    // The existing health path copies a snapshot name; it does
                    // not create a new identity. Freeze the supplying expression.
                    self.vocabulary.forwarded += 1;
                } else {
                    self.vocabulary.expression(value, &origin);
                }
            } else {
                self.header_windows += 1;
                if self.file != "observability.rs"
                    || !matches!(dynamic.as_str(), "name" | "name . into ()")
                {
                    self.vocabulary.expression(value, &origin);
                }
            }
        }
        syn::visit::visit_expr_struct(self, expr);
    }

    fn visit_expr_call(&mut self, expr: &'ast syn::ExprCall) {
        if self.file == "quota/parse.rs"
            && let Expr::Path(p) = expr.func.as_ref()
            && p.path.is_ident("window")
        {
            self.poll_calls += 1;
            assert!(
                matches!(
                    tokens(expr.args.first().unwrap()).as_str(),
                    "name" | "& name"
                ),
                "new poll window input"
            );
        }
        syn::visit::visit_expr_call(self, expr);
    }

    fn visit_expr_assign(&mut self, expr: &'ast syn::ExprAssign) {
        if self.file == "observability.rs"
            && self.function == "snapshot"
            && let Expr::Field(field) = expr.left.as_ref()
            && matches!(&field.member, syn::Member::Named(n) if n == "representative_claim")
        {
            // Native representative claims retain provider aliases. Extract
            // their actual matches! pattern, not a second copied name table.
            struct Claims<'a>(&'a mut Vocabulary);
            impl<'ast> Visit<'ast> for Claims<'_> {
                fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                    if mac.path.is_ident("matches") {
                        use syn::parse::Parser;
                        let pattern: TokenStream = mac
                            .tokens
                            .clone()
                            .into_iter()
                            .skip_while(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == ','))
                            .skip(1)
                            .collect();
                        let pattern = Pat::parse_multi
                            .parse2(pattern)
                            .expect("native claim pattern");
                        fn literals(pattern: &Pat, vocabulary: &mut Vocabulary) {
                            match pattern {
                                Pat::Or(p) => {
                                    for case in &p.cases {
                                        literals(case, vocabulary);
                                    }
                                }
                                Pat::Lit(lit) => vocabulary.expression(
                                    &Expr::Lit(lit.clone()),
                                    "observability.rs::representative_claim",
                                ),
                                _ => panic!("unclassified native claim pattern"),
                            }
                        }
                        literals(&pattern, self.0);
                    }
                }
            }
            Claims(&mut *self.vocabulary).visit_expr(&expr.right);
        }
        syn::visit::visit_expr_assign(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        assert!(
            !has_native_macro_syntax(mac.tokens.clone()),
            "macro-generated native producer requires classification at {}",
            self.origin()
        );
        let is_json = mac.path.segments.last().is_some_and(|s| s.ident == "json");
        if is_json {
            let origin = self.origin();
            for value in json_window_names(mac.tokens.clone()) {
                self.header_windows += 1;
                if self.file == "quota/parse.rs"
                    && self.function == "window"
                    && tokens(&value) == "name"
                {
                    continue;
                }
                self.vocabulary.expression(&value, &origin);
            }
        } else {
            assert!(
                json_window_names(mac.tokens.clone()).is_empty(),
                "macro-generated native producer requires classification at {}",
                self.origin()
            );
        }
        assert!(
            !mac.path.is_ident("include"),
            "source include requires inventory classification at {}",
            self.origin()
        );
        syn::visit::visit_macro(self, mac);
    }
}

// Macro definitions and invocations are both opaque to syn's expression walk.
// Also reject standalone native names in strings (including format! and raw strings):
// code-generating macros can turn those strings into constructors. No expansion
// is attempted. Identifier boundaries distinguish Window from Windows messages.
fn has_native_macro_syntax(stream: TokenStream) -> bool {
    stream.into_iter().any(|token| match token {
        TokenTree::Ident(ident) => matches!(
            ident.to_string().trim_start_matches("r#"),
            "Window" | "QuotaCooling" | "mod"
        ),
        TokenTree::Literal(literal) => {
            let spelling = literal.to_string();
            let text = match syn::parse_str::<syn::Lit>(&spelling) {
                Ok(syn::Lit::Str(text)) => text.value(),
                Ok(syn::Lit::ByteStr(text)) => String::from_utf8_lossy(&text.value()).into_owned(),
                _ => spelling,
            };
            text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| matches!(word, "Window" | "QuotaCooling"))
        }
        TokenTree::Group(group) => has_native_macro_syntax(group.stream()),
        _ => false,
    })
}

// syn leaves macro bodies opaque. Inspect balanced JSON object token groups,
// then parse the emitted name value as a real Rust expression.
fn json_window_names(stream: TokenStream) -> Vec<Expr> {
    let parts: Vec<_> = stream.into_iter().collect();
    let mut found = Vec::new();
    let field_at = |index: usize, field: &str| {
        matches!(parts.get(index), Some(TokenTree::Literal(lit)) if syn::parse_str::<syn::LitStr>(&lit.to_string()).is_ok_and(|s| s.value() == field))
            && matches!(parts.get(index + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
    };
    if (0..parts.len()).any(|i| field_at(i, "used_pct") || field_at(i, "reset_at")) {
        for i in 0..parts.len() {
            if field_at(i, "name") {
                let value: TokenStream = parts[i + 2..]
                    .iter()
                    .take_while(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == ','))
                    .cloned()
                    .collect();
                found.push(syn::parse2(value).expect("window JSON name must be a Rust expression"));
            }
        }
    }
    for part in parts {
        if let TokenTree::Group(group) = part {
            found.extend(json_window_names(group.stream()));
        }
    }
    found
}

fn scan(file: &str, source: &str, vocabulary: &mut Vocabulary) -> (usize, usize, usize) {
    let ast = syn::parse_file(source).expect("native source parses");
    if file == "observability/forecast_log.rs" {
        let local = ast
            .items
            .iter()
            .find_map(|item| match item {
                Item::Struct(item) if item.ident == "Window" => Some(item),
                _ => None,
            })
            .expect("forecast Window must remain a local struct");
        let expected: syn::ItemStruct = syn::parse_quote!(
            struct Window {
                name: String,
                used_pct: Option<f64>,
                observed_at: Option<i64>,
                reset_at: Option<i64>,
            }
        );
        assert_eq!(
            tokens(&local.fields),
            tokens(&expected.fields),
            "forecast Window shape changed"
        );
        let input = ast
            .items
            .iter()
            .find_map(|item| match item {
                Item::Fn(item) if item.sig.ident == "input" => Some(item),
                _ => None,
            })
            .expect("forecast input function");
        let signature: syn::Signature =
            syn::parse_quote!(fn input(a: &AdminAccount, now: i64) -> Result<Input>);
        assert_eq!(
            tokens(&input.sig),
            tokens(&signature),
            "forecast input type changed"
        );
    }
    let mut visitor = Origins {
        file,
        function: String::new(),
        vocabulary,
        cooling: 0,
        header_windows: 0,
        poll_calls: 0,
        forecast_loop_depth: 0,
        forecast_copies: 0,
    };
    visitor.visit_file(&ast);
    if file == "observability/forecast_log.rs" {
        assert_eq!(
            visitor.forecast_copies, 1,
            "forecast copy inventory changed"
        );
    }
    (visitor.cooling, visitor.header_windows, visitor.poll_calls)
}

// Walk actual production module declarations instead of a filename glob. A new
// module containing a cooling constructor must be classified, even on another
// platform or behind a disabled feature. Test-only external files are not loaded.
fn audit_modules(
    file: &Path,
    directory: &Path,
    vocabulary: &mut Vocabulary,
    seen: &mut BTreeSet<String>,
) {
    let source = std::fs::read_to_string(file).expect("production module is readable");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let relative = file
        .strip_prefix(&root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    seen.insert(relative.clone());
    if !matches!(
        relative.as_str(),
        "observability.rs"
            | "quota/parse.rs"
            | "accounts/health.rs"
            | "relay.rs"
            | "bindings/permit_tape.rs"
    ) {
        let (cooling, windows, _) = scan(&relative, &source, vocabulary);
        assert_eq!(
            cooling, 0,
            "new native cooling producer: {relative}; classify its inputs"
        );
        assert_eq!(
            windows, 0,
            "new native snapshot producer: {relative}; classify its inputs"
        );
    }
    fn modules(
        items: &[Item],
        file: &Path,
        directory: &Path,
        vocabulary: &mut Vocabulary,
        seen: &mut BTreeSet<String>,
    ) {
        for item in items {
            let Item::Mod(module) = item else {
                continue;
            };
            if test_only(&module.attrs) {
                continue;
            }
            if let Some((_, items)) = &module.content {
                modules(
                    items,
                    file,
                    &directory.join(module.ident.to_string()),
                    vocabulary,
                    seen,
                );
                continue;
            }
            let explicit = module
                .attrs
                .iter()
                .find(|a| a.path().is_ident("path"))
                .map(|a| {
                    let syn::Meta::NameValue(meta) = &a.meta else {
                        panic!("unsupported module path");
                    };
                    let Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(path),
                        ..
                    }) = &meta.value
                    else {
                        panic!("nonliteral module path");
                    };
                    file.parent().unwrap().join(path.value())
                });
            let child = explicit.unwrap_or_else(|| {
                let plain = directory.join(format!("{}.rs", module.ident));
                let nested = directory.join(module.ident.to_string()).join("mod.rs");
                assert_ne!(
                    plain.is_file(),
                    nested.is_file(),
                    "ambiguous/missing module {} in {}",
                    module.ident,
                    file.display()
                );
                if plain.is_file() { plain } else { nested }
            });
            let next = if child.file_name().unwrap() == "mod.rs" {
                child.parent().unwrap().to_owned()
            } else {
                child.with_extension("")
            };
            audit_modules(&child, &next, vocabulary, seen);
        }
    }
    modules(
        &syn::parse_file(&source).unwrap().items,
        file,
        directory,
        vocabulary,
        seen,
    );
}

#[test]
fn native_fixed_and_duration_window_origins_are_accepted() {
    let mut vocabulary = Vocabulary::default();
    assert_eq!(
        scan("observability.rs", HEADERS, &mut vocabulary),
        (0, 2, 0)
    );
    assert_eq!(scan("quota/parse.rs", POLL, &mut vocabulary), (0, 1, 2));
    assert_eq!(
        scan("accounts/health.rs", HEALTH, &mut vocabulary),
        (1, 0, 0)
    );
    assert_eq!(scan("relay.rs", RELAY, &mut vocabulary), (1, 0, 0));

    let poll = syn::parse_file(POLL).unwrap();
    let constant = poll
        .items
        .iter()
        .find_map(|i| match i {
            Item::Const(c) if c.ident == "CLAUDE_WINDOWS" => Some(c),
            _ => None,
        })
        .unwrap();
    let Expr::Reference(reference) = constant.expr.as_ref() else {
        panic!("Claude table must be a referenced array");
    };
    let Expr::Array(array) = reference.expr.as_ref() else {
        panic!("Claude table must be an array");
    };
    assert!(array.elems.len() >= 4);
    for row in &array.elems {
        let Expr::Tuple(tuple) = row else {
            panic!("Claude table row must be a tuple");
        };
        assert_eq!(tuple.elems.len(), 2);
        vocabulary.expression(&tuple.elems[1], "quota/parse.rs::CLAUDE_WINDOWS output");
    }
    // Verify pass-through and bounded dynamic inputs in parsed syntax; comments
    // or unrelated string contents cannot stand in for these expressions.
    let health_ast = tokens(&syn::parse_file(HEALTH).unwrap());
    let header_ast = tokens(&syn::parse_file(HEADERS).unwrap());
    let poll_ast = tokens(&poll);
    assert!(health_ast.contains("w [\"name\"] . as_str () . map (| name | QuotaCooling"));
    assert!(header_ast.contains("v . parse :: < u32 > () . ok ()"));
    assert!(poll_ast.contains("duration . as_u64 ()"));
    assert!(poll_ast.contains("Some (n) if n > 0 && n % 60 == 0 =>"));
    assert!(poll_ast.contains("Some (n) if n > 0 =>"));
    assert_eq!(vocabulary.families.len(), 3);
    assert_eq!(vocabulary.forwarded, 3);
    assert!(
        vocabulary.sites.len() >= 31,
        "origin inventory shrank: {:?}",
        vocabulary.sites
    );
    assert!(
        vocabulary.values.len() >= 18,
        "native value inventory shrank"
    );
    for required in [
        "5h",
        "7d",
        "7d_oi",
        "7d_sonnet",
        "primary",
        "secondary",
        "observed",
    ] {
        assert!(
            vocabulary.values.contains(required),
            "missing native origin: {required}"
        );
    }
    assert!(
        vocabulary.refused().is_empty(),
        "native windows refused at {HEAD}: {:?}; origins: {:?}",
        vocabulary.refused(),
        vocabulary.sites
    );
}

#[test]
fn mixed_cfg_scopes_skip_only_modules_that_require_test() {
    let source = r#"
        #[cfg(all(test, feature = "testkit"))]
        mod publication_tests {
            fn fixture() { let w = Window { name: "test_fixture_only".into(), ..base }; }
        }
        #[cfg(any(test, all(feature = "testkit", test)))]
        mod nested_test_only {
            fn fixture() { let w = Window { name: "nested_test_fixture_only".into(), ..base }; }
        }
        #[cfg(any(test, feature = "testkit"))]
        mod possibly_production {
            fn emit() { let w = Window { name: "new_native_bucket_not_yet_supported".into(), ..base }; }
        }
    "#;
    let mut vocabulary = Vocabulary::default();
    assert_eq!(scan("observability.rs", source, &mut vocabulary), (0, 1, 0));
    assert_eq!(
        vocabulary.sites,
        ["observability.rs::emit: new_native_bucket_not_yet_supported"]
    );
    assert_eq!(
        vocabulary.refused(),
        ["new_native_bucket_not_yet_supported"]
    );
    let negated: syn::ItemMod = syn::parse_quote!(
        #[cfg(not(test))]
        mod production {}
    );
    assert!(!test_only(&negated.attrs));
}

#[test]
fn production_module_inventory_has_no_unclassified_cooling_producer() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut seen = BTreeSet::new();
    audit_modules(
        &root.join("lib.rs"),
        &root,
        &mut Vocabulary::default(),
        &mut seen,
    );
    for required in [
        "relay.rs",
        "accounts/health.rs",
        "observability.rs",
        "quota/parse.rs",
        "bindings.rs",
    ] {
        assert!(
            seen.contains(required),
            "missing production module {required}"
        );
    }
}

#[test]
fn planted_new_native_literal_is_discovered_and_refused() {
    let planted = r#"fn candidates_with_evidence() {
        let q = crate::accounts::QuotaCooling {
            window: "new_native_bucket_not_yet_supported".into(), reset_at: now, started_at: now,
        };
    }"#;
    let mut vocabulary = Vocabulary::default();
    assert_eq!(scan("relay.rs", planted, &mut vocabulary), (1, 0, 0));
    assert_eq!(
        vocabulary.sites,
        ["relay.rs::candidates_with_evidence: new_native_bucket_not_yet_supported"]
    );
    assert_eq!(
        vocabulary.refused(),
        ["new_native_bucket_not_yet_supported"]
    );
}

#[test]
fn planted_native_window_struct_update_is_discovered_and_refused() {
    for constructor in ["Window", "crate::observability::Window"] {
        let source = format!(
            "fn emit() {{ let w = {constructor} {{ name: \"new_native_bucket_not_yet_supported\".into(), ..base }}; }}"
        );
        let mut vocabulary = Vocabulary::default();
        assert_eq!(
            scan("observability.rs", &source, &mut vocabulary),
            (0, 1, 0)
        );
        assert_eq!(
            vocabulary.sites,
            ["observability.rs::emit: new_native_bucket_not_yet_supported"]
        );
        assert_eq!(
            vocabulary.refused(),
            ["new_native_bucket_not_yet_supported"]
        );
    }
}

#[test]
fn harmless_windows_macro_message_is_not_native_constructor_syntax() {
    let source = r#"fn log() {
        tracing::info!("Windows listener initialized");
        let message = format!("Windows service ready: {}", version);
    }"#;
    let mut vocabulary = Vocabulary::default();
    assert_eq!(
        scan("platform/windows/service.rs", source, &mut vocabulary),
        (0, 0, 0)
    );
    assert!(vocabulary.sites.is_empty());
}

#[test]
fn planted_macro_native_windows_require_classification() {
    assert!(super::Window::parse("new_native_bucket_not_yet_supported").is_err());
    for source in [
        r#"macro_rules! hidden { () => { Window { name: "new_native_bucket_not_yet_supported".into(), used_pct: None } }; }"#,
        r#"fn snapshot() { windows.push(identity!(Window { name: "new_native_bucket_not_yet_supported".into(), used_pct: None })); }"#,
        r#"fn snapshot() { windows.push(identity!(Window { name: "new_native_bucket_not_yet_supported".into(), ..base })); }"#,
        r#"macro_rules! hidden { () => { QuotaCooling { window: "new_native_bucket_not_yet_supported".into() } }; }"#,
        r#"fn emit() { identity!(crate::accounts::QuotaCooling { window: "new_native_bucket_not_yet_supported".into() }); }"#,
        r##"fn emit() { format!(r#"Window {{ name: "new_native_bucket_not_yet_supported" }}"#); }"##,
        r##"fn emit() { generate!(r#"QuotaCooling { window: "new_native_bucket_not_yet_supported" }"#); }"##,
        r#"fn emit() { generate!("\u{57}indow { name: new_native_bucket_not_yet_supported }"); }"#,
        r#"fn emit() { generate!(b"QuotaCooling { window: new_native_bucket_not_yet_supported }"); }"#,
        r#"fn emit() { json!({"producer": "Window", "name": "new_native_bucket_not_yet_supported", "used_pct": null}); }"#,
    ] {
        let failure = std::panic::catch_unwind(|| {
            scan("observability.rs", source, &mut Vocabulary::default());
        })
        .expect_err("opaque native macro must not bypass the inventory");
        let message = failure
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| failure.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("macro-generated native producer requires classification"),
            "unexpected failure: {message}"
        );
    }
}

#[test]
fn planted_native_window_alias_requires_classification() {
    assert!(super::Window::parse("new_native_bucket_not_yet_supported").is_err());
    for alias in [
        "use crate::observability::Window as Renamed;",
        "use crate::observability::{Window as Renamed};",
        "type Renamed = crate::observability::Window;",
        "use crate::accounts::QuotaCooling as Renamed;",
        "type Renamed = crate::accounts::QuotaCooling;",
    ] {
        let source = format!(
            "{alias} fn emit() {{ let w = Renamed {{ name: \"new_native_bucket_not_yet_supported\".into(), used_pct: None }}; }}"
        );
        let failure = std::panic::catch_unwind(|| {
            scan("observability.rs", &source, &mut Vocabulary::default());
        })
        .expect_err("native aliases must not bypass the inventory");
        let message = failure
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| failure.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("native producer alias requires classification"),
            "unexpected failure: {message}"
        );
        assert!(
            message.contains("observability.rs::<module>"),
            "missing alias context: {message}"
        );
    }
}

#[test]
fn ordinary_window_imports_and_composite_types_are_not_constructor_aliases() {
    let source = r#"
        use super::outside_usage::{Observation, State as Detector, Window};
        type Projected = (String, Window, Option<f64>);
        type Windows = Vec<Window>;
        fn project() { let w = Window { start: None, reset: 0, duration_secs: 60 }; }
    "#;
    let mut vocabulary = Vocabulary::default();
    assert_eq!(
        scan("account_runtime/outside_owner.rs", source, &mut vocabulary),
        (0, 0, 0)
    );
    assert!(vocabulary.sites.is_empty());
}

#[test]
fn forecast_journal_window_copy_does_not_hide_new_literals() {
    let source = include_str!("../../observability/forecast_log.rs");
    assert_eq!(
        scan(
            "observability/forecast_log.rs",
            source,
            &mut Vocabulary::default()
        ),
        (0, 0, 0)
    );
    let changed = source.replacen(
        "name: w.name.clone(),",
        "name: \"new_native_bucket_not_yet_supported\".into(),",
        1,
    );
    assert_ne!(
        changed, source,
        "forecast copy fixture must alter a real emission"
    );
    let failure = std::panic::catch_unwind(|| {
        scan(
            "observability/forecast_log.rs",
            &changed,
            &mut Vocabulary::default(),
        );
    })
    .expect_err("new forecast literals must require classification");
    let message = failure
        .downcast_ref::<String>()
        .expect("assertion diagnostic");
    assert!(
        message.contains("forecast Window copy changed"),
        "unexpected failure: {message}"
    );
}

#[test]
#[should_panic(expected = "unclassified window expression")]
fn unsupported_dynamic_native_window_requires_classification() {
    scan(
        "relay.rs",
        "fn emit() { let q = QuotaCooling { window: provider_string(), reset_at: now, started_at: now }; }",
        &mut Vocabulary::default(),
    );
}

#[test]
fn placement_duration_families_are_accepted() {
    // Placement admits each positive u64 duration with one of these units.
    // Keep its AST predicate in view, including the upper bound, rather than
    // reducing that accepted language to the two familiar 5h/7d examples.
    let ast = syn::parse_file(PLACEMENT).unwrap();
    let predicate = ast
        .items
        .iter()
        .find_map(|i| match i {
            Item::Fn(f) if f.sig.ident == "placement_account_window" => Some(f),
            _ => None,
        })
        .unwrap();
    let body = tokens(&predicate.block);
    assert!(body.contains("name . len () > 21"));
    assert!(body.contains("digits . parse :: < u64 > ()"));
    assert!(body.contains("b's' | b'm' | b'h' | b'd'"));
    for unit in ['s', 'm', 'h', 'd'] {
        for duration in [1, 43_200, u64::MAX] {
            let value = format!("{duration}{unit}");
            assert!(
                super::Window::parse(&value).is_ok(),
                "placement duration {value}"
            );
        }
    }
}
