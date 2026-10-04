#![allow(dead_code)]

use proc_macro2::Span;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use syn::{
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
    visit::{self, Visit},
    Attribute, Expr, ExprCall, ExprMethodCall, File, ImplItemFn, Item, ItemFn, ItemMacro, Lit,
    Meta, Path as SynPath, Result as SynResult, Token, UseTree,
};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SiteUse {
    pub crate_name: String,
    pub file: PathBuf,
    pub line: usize,
    pub site: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct UntaggedOrDynamic {
    pub file: PathBuf,
    pub line: usize,
    pub fn_name: String,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct HelperCall {
    pub crate_name: String,
    pub helper: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct MacroUse {
    pub crate_name: String,
    pub file: PathBuf,
    pub kind: String,
    pub fn_name: String,
    pub site: Option<String>,
    pub skip: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    pub sites: Vec<SiteUse>,
    pub violations: Vec<UntaggedOrDynamic>,
    pub helper_calls: Vec<HelperCall>,
    pub macros: Vec<MacroUse>,
    pub test_fns: BTreeMap<PathBuf, BTreeSet<String>>,
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn scan() -> ScanResult {
    static RESULT: OnceLock<ScanResult> = OnceLock::new();
    RESULT.get_or_init(scan_uncached).clone()
}

fn scan_uncached() -> ScanResult {
    let root = workspace_root();
    let crates = root.join("crates");
    let mut result = ScanResult::default();

    for crate_dir in child_dirs(&crates) {
        let crate_name = crate_dir
            .file_name()
            .expect("crate directory has no name")
            .to_string_lossy()
            .into_owned();

        for file in rust_files(&crate_dir.join("src")) {
            if is_test_module_file(&crate_dir.join("src"), &file) {
                continue;
            }
            scan_production_file(&root, &crate_name, &file, &mut result);
        }

        for file in rust_files(&crate_dir.join("tests")) {
            scan_test_file(&root, &crate_name, &file, &mut result);
        }
    }

    result.sites.sort();
    result.violations.sort();
    result.helper_calls.sort();
    result.macros.sort();
    result
}

fn scan_production_file(root: &Path, crate_name: &str, file: &Path, result: &mut ScanResult) {
    let parsed = parse_file(file);
    let relative = relative_path(root, file);
    let imports = collect_imports(&parsed);
    let local_fns = collect_local_fns(&parsed);

    let mut scanner = ProductionScanner {
        crate_name,
        file: relative,
        in_common_fs: crate_name == "htap-common"
            && path_has_prefix(file, &root.join("crates/htap-common/src/fs")),
        imports,
        local_fns,
        current_fn: Vec::new(),
        sites: &mut result.sites,
        violations: &mut result.violations,
        helper_calls: &mut result.helper_calls,
    };
    scanner.visit_file(&parsed);
}

fn scan_test_file(root: &Path, crate_name: &str, file: &Path, result: &mut ScanResult) {
    let parsed = parse_file(file);
    let relative = relative_path(root, file);
    let tests = result.test_fns.entry(relative.clone()).or_default();

    for item in &parsed.items {
        match item {
            Item::Fn(function) if has_test_attr(&function.attrs) => {
                tests.insert(function.sig.ident.to_string());
            }
            Item::Macro(item_macro) => {
                if let Some(use_) = parse_crashsim_macro(crate_name, &relative, item_macro) {
                    result.macros.push(use_);
                }
            }
            _ => {}
        }
    }
}

fn parse_file(path: &Path) -> File {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    syn::parse_file(&source)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn child_dirs(path: &Path) -> Vec<PathBuf> {
    let mut dirs = match fs::read_dir(path) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    };
    dirs.sort();
    dirs
}

fn rust_files(path: &Path) -> Vec<PathBuf> {
    fn walk(path: &Path, files: &mut Vec<PathBuf>) {
        let mut entries = match fs::read_dir(path) {
            Ok(entries) => entries.filter_map(|entry| entry.ok()).collect::<Vec<_>>(),
            Err(_) => return,
        };
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    walk(path, &mut files);
    files
}

fn relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .unwrap_or_else(|_| panic!("{} is outside {}", path.display(), root.display()))
        .to_path_buf()
}

fn path_has_prefix(path: &Path, prefix: &Path) -> bool {
    path.strip_prefix(prefix).is_ok()
}

fn is_test_module_file(src: &Path, file: &Path) -> bool {
    let relative = file.strip_prefix(src).unwrap_or(file);
    file.file_name().and_then(|name| name.to_str()) == Some("tests.rs")
        || relative
            .components()
            .any(|part| part.as_os_str() == "tests")
}

fn excluded(attrs: &[Attribute]) -> bool {
    has_test_attr(attrs) || attrs.iter().any(is_cfg_test)
}

fn has_test_attr(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("test"))
}

fn is_cfg_test(attr: &Attribute) -> bool {
    if !attr.path().is_ident("cfg") {
        return false;
    }

    match &attr.meta {
        Meta::List(list) => syn::parse2::<CfgArgs>(list.tokens.clone())
            .map(|args| args.0.iter().any(meta_mentions_test))
            .unwrap_or(false),
        _ => false,
    }
}

fn meta_mentions_test(meta: &Meta) -> bool {
    if meta.path().is_ident("test") {
        return true;
    }

    match meta {
        Meta::List(list) if list.path.is_ident("all") => {
            syn::parse2::<CfgArgs>(list.tokens.clone())
                .map(|args| args.0.iter().any(meta_mentions_test))
                .unwrap_or(false)
        }
        _ => false,
    }
}

struct CfgArgs(Punctuated<Meta, Token![,]>);

impl Parse for CfgArgs {
    fn parse(input: ParseStream<'_>) -> SynResult<Self> {
        Ok(Self(Punctuated::parse_terminated(input)?))
    }
}

#[derive(Clone, Debug)]
struct Import {
    local: String,
    target: Vec<String>,
}

fn collect_imports(file: &File) -> Vec<Import> {
    struct Collector {
        imports: Vec<Import>,
    }

    impl<'ast> Visit<'ast> for Collector {
        fn visit_item(&mut self, item: &'ast Item) {
            if item_attrs(item).is_some_and(excluded) {
                return;
            }
            if let Item::Use(item_use) = item {
                flatten_use(&item_use.tree, Vec::new(), &mut self.imports);
            }
            visit::visit_item(self, item);
        }
    }

    let mut collector = Collector {
        imports: Vec::new(),
    };
    collector.visit_file(file);
    collector.imports
}

fn flatten_use(tree: &UseTree, prefix: Vec<String>, imports: &mut Vec<Import>) {
    match tree {
        UseTree::Path(path) => {
            let mut next = prefix;
            next.push(path.ident.to_string());
            flatten_use(&path.tree, next, imports);
        }
        UseTree::Name(name) => {
            let mut target = prefix;
            target.push(name.ident.to_string());
            imports.push(Import {
                local: name.ident.to_string(),
                target,
            });
        }
        UseTree::Rename(rename) => {
            let mut target = prefix;
            target.push(rename.ident.to_string());
            imports.push(Import {
                local: rename.rename.to_string(),
                target,
            });
        }
        UseTree::Group(group) => {
            for item in &group.items {
                flatten_use(item, prefix.clone(), imports);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn collect_local_fns(file: &File) -> BTreeSet<String> {
    struct Collector(BTreeSet<String>);

    impl<'ast> Visit<'ast> for Collector {
        fn visit_item_fn(&mut self, function: &'ast ItemFn) {
            if excluded(&function.attrs) {
                return;
            }
            self.0.insert(function.sig.ident.to_string());
            visit::visit_item_fn(self, function);
        }

        fn visit_item(&mut self, item: &'ast Item) {
            if item_attrs(item).is_some_and(excluded) {
                return;
            }
            visit::visit_item(self, item);
        }
    }

    let mut collector = Collector(BTreeSet::new());
    collector.visit_file(file);
    collector.0
}

fn item_attrs(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Const(item) => Some(&item.attrs),
        Item::Enum(item) => Some(&item.attrs),
        Item::ExternCrate(item) => Some(&item.attrs),
        Item::Fn(item) => Some(&item.attrs),
        Item::ForeignMod(item) => Some(&item.attrs),
        Item::Impl(item) => Some(&item.attrs),
        Item::Macro(item) => Some(&item.attrs),
        Item::Mod(item) => Some(&item.attrs),
        Item::Static(item) => Some(&item.attrs),
        Item::Struct(item) => Some(&item.attrs),
        Item::Trait(item) => Some(&item.attrs),
        Item::TraitAlias(item) => Some(&item.attrs),
        Item::Type(item) => Some(&item.attrs),
        Item::Union(item) => Some(&item.attrs),
        Item::Use(item) => Some(&item.attrs),
        _ => None,
    }
}

struct ProductionScanner<'a> {
    crate_name: &'a str,
    file: PathBuf,
    in_common_fs: bool,
    imports: Vec<Import>,
    local_fns: BTreeSet<String>,
    current_fn: Vec<String>,
    sites: &'a mut Vec<SiteUse>,
    violations: &'a mut Vec<UntaggedOrDynamic>,
    helper_calls: &'a mut Vec<HelperCall>,
}

impl ProductionScanner<'_> {
    fn function_name(&self) -> String {
        self.current_fn
            .last()
            .cloned()
            .unwrap_or_else(|| "<module>".to_owned())
    }

    fn pass_through_allowed(&self) -> bool {
        let file = self.file.to_string_lossy().replace('\\', "/");
        (file == "crates/htap-common/src/fs/dur.rs" && !self.current_fn.is_empty())
            || (file == "crates/htap-txn/src/journal.rs"
                && self
                    .current_fn
                    .last()
                    .is_some_and(|name| name == "sync_all_checked"))
    }

    fn record_site(&mut self, span: Span, name: &str, argument: Option<&Expr>) {
        match argument.and_then(string_literal) {
            Some(site) => self.sites.push(SiteUse {
                crate_name: self.crate_name.to_owned(),
                file: self.file.clone(),
                line: span.start().line,
                site,
            }),
            None if !self.pass_through_allowed() => {
                self.violations.push(UntaggedOrDynamic {
                    file: self.file.clone(),
                    line: span.start().line,
                    fn_name: name.to_owned(),
                    reason: "dynamic site id",
                });
            }
            None => {}
        }
    }

    fn record_untagged(&mut self, span: Span, name: &str) {
        if !self.in_common_fs {
            self.violations.push(UntaggedOrDynamic {
                file: self.file.clone(),
                line: span.start().line,
                fn_name: name.to_owned(),
                reason: "untagged sync",
            });
        }
    }

    fn imported_target(&self, local: &str) -> Option<&[String]> {
        self.imports
            .iter()
            .find(|import| import.local == local)
            .map(|import| import.target.as_slice())
    }

    fn call_target(&self, path: &SynPath) -> Vec<String> {
        let segments = path_segments(path);
        if segments.len() == 1 {
            if let Some(target) = self.imported_target(&segments[0]) {
                return target.to_vec();
            }
        }
        segments
    }

    fn record_helper(&mut self, path: &SynPath) {
        if self.crate_name == "htap-common" {
            return;
        }

        const HELPERS: &[&str] = &[
            "atomic_publish",
            "write_new_tmp_file",
            "sync_dir",
            "sync_ancestors_best_effort",
            "create_dir_all_durable",
            "fsync_file",
        ];

        let raw = path_segments(path);
        let resolved = self.call_target(path);
        let helper = resolved.last().map(String::as_str);

        let explicit = resolved.len() >= 3
            && resolved[resolved.len() - 3..resolved.len() - 1] == ["htap_common", "fs"];
        let imported = raw.len() == 1
            && self.imported_target(&raw[0]).is_some_and(|target| {
                target.len() >= 3
                    && target[target.len() - 3..target.len() - 1] == ["htap_common", "fs"]
            });

        if (explicit || imported)
            && helper.is_some_and(|name| HELPERS.contains(&name))
            && !(raw.len() == 1 && self.local_fns.contains(&raw[0]) && !imported)
        {
            self.helper_calls.push(HelperCall {
                crate_name: self.crate_name.to_owned(),
                helper: helper.unwrap().to_owned(),
            });
        }
    }
}

impl<'ast> Visit<'ast> for ProductionScanner<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if item_attrs(item).is_some_and(excluded) {
            return;
        }
        visit::visit_item(self, item);
    }

    fn visit_item_fn(&mut self, function: &'ast ItemFn) {
        if excluded(&function.attrs) {
            return;
        }
        self.current_fn.push(function.sig.ident.to_string());
        visit::visit_item_fn(self, function);
        self.current_fn.pop();
    }

    fn visit_impl_item_fn(&mut self, function: &'ast ImplItemFn) {
        if excluded(&function.attrs) {
            return;
        }
        self.current_fn.push(function.sig.ident.to_string());
        visit::visit_impl_item_fn(self, function);
        self.current_fn.pop();
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let name = call.method.to_string();
        match name.as_str() {
            "sync_all_site" | "sync_data_site" | "sync_all_checked" => {
                self.record_site(call.method.span(), &name, call.args.last());
            }
            "sync_all" | "sync_data" if call.args.is_empty() => {
                self.record_untagged(call.method.span(), &name);
            }
            _ => {}
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Expr::Path(callee) = call.func.as_ref() {
            let path = &callee.path;
            let raw = path_segments(path);
            let resolved = self.call_target(path);
            let name = raw.last().cloned().unwrap_or_default();

            if matches!(name.as_str(), "sync_dir_site" | "fsync_path_site") {
                self.record_site(path.span(), &name, call.args.iter().nth(1));
            }

            let is_direct_dur_sync = raw.len() >= 2
                && raw[raw.len() - 2] == "dur"
                && matches!(
                    raw.last().map(String::as_str),
                    Some("sync_dir" | "fsync_path")
                );
            let is_imported_dur_sync = raw.len() == 1
                && resolved.len() >= 4
                && resolved[resolved.len() - 4..resolved.len() - 1] == ["htap_common", "fs", "dur"]
                && matches!(
                    resolved.last().map(String::as_str),
                    Some("sync_dir" | "fsync_path")
                );

            if is_direct_dur_sync || is_imported_dur_sync {
                self.record_untagged(path.span(), &name);
            }

            self.record_helper(path);
        }

        visit::visit_expr_call(self, call);
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(literal) => match &literal.lit {
            Lit::Str(value) => Some(value.value()),
            _ => None,
        },
        _ => None,
    }
}

fn path_segments(path: &SynPath) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

struct MacroArgs {
    fn_name: syn::Ident,
    fields: Vec<(syn::Ident, MacroValue)>,
}

enum MacroValue {
    String(syn::LitStr),
    Path(SynPath),
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream<'_>) -> SynResult<Self> {
        let fn_name = input.parse()?;
        input.parse::<Token![,]>()?;

        let mut fields = Vec::new();
        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let value = if key == "site" {
                MacroValue::String(input.parse()?)
            } else {
                MacroValue::Path(input.parse()?)
            };
            fields.push((key, value));

            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(Self { fn_name, fields })
    }
}

fn parse_crashsim_macro(crate_name: &str, file: &Path, item: &ItemMacro) -> Option<MacroUse> {
    let kind = item.mac.path.segments.last()?.ident.to_string();
    if !matches!(
        kind.as_str(),
        "crashsim_witness" | "crashsim_control" | "crashsim_survivor"
    ) {
        return None;
    }

    let args = syn::parse2::<MacroArgs>(item.mac.tokens.clone()).unwrap_or_else(|error| {
        panic!(
            "failed to parse {kind}! in {} at line {}: {error}",
            file.display(),
            item.span().start().line
        )
    });

    let mut site = None;
    let mut skip = None;
    let mut body = None;

    for (key, value) in args.fields {
        match (key.to_string().as_str(), value) {
            ("site", MacroValue::String(value)) => site = Some(value.value()),
            ("skip", MacroValue::Path(path)) => {
                skip = path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string());
            }
            ("body", MacroValue::Path(path)) => {
                body = path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string());
            }
            _ => {}
        }
    }

    Some(MacroUse {
        crate_name: crate_name.to_owned(),
        file: file.to_path_buf(),
        kind,
        fn_name: args.fn_name.to_string(),
        site,
        skip,
        body: body.unwrap_or_else(|| {
            panic!(
                "crashsim macro in {} at line {} has no body",
                file.display(),
                item.span().start().line
            )
        }),
    })
}
