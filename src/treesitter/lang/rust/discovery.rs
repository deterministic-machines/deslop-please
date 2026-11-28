//! Cargo workspace and Rust module tree discovery, read with `toml` and
//! Tree-sitter only: no `cargo metadata` subprocess.
//!
//! A package's crate roots (`lib.rs`, bins, the build script, integration
//! tests) are the package's modules; each `mod foo;` they declare in turn
//! becomes a module of its own, resolved with Rust's file layout rules,
//! `#[path]`, and `#[cfg(..)]`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::Glob;
use toml::Value;
use tree_sitter::{Node, Parser};

use crate::Error;
use crate::configuration::Configuration;
use crate::model::NodeId;
use crate::model::NodeKind;
use crate::treesitter::lang::{DiscoveredFile, Language, WorkspaceLayout};

use super::cfg::{self, CfgContext};

/// Discover the Rust packages, modules, and files below `root`.
pub(crate) fn discover(
    root: &Path,
    configuration: &Configuration,
    layout: &mut WorkspaceLayout,
) -> Result<(), Error> {
    let root_manifest = read_manifest(root)?;
    let mut directories = Vec::new();
    let has_package = root_manifest.get("package").is_some();
    if has_package {
        directories.push(root.to_path_buf());
    }
    if let Some(workspace) = root_manifest.get("workspace") {
        let members: Vec<String> = workspace
            .get("members")
            .and_then(Value::as_array)
            .map(|members| {
                members
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let excludes: Vec<String> = workspace
            .get("exclude")
            .and_then(Value::as_array)
            .map(|members| {
                members
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|pattern| pattern.trim_end_matches('/').to_owned())
                    .collect()
            })
            .unwrap_or_default();
        for pattern in members {
            if matches_any(&pattern, &excludes) {
                continue;
            }
            for directory in member_directories(root, &pattern, &excludes)? {
                directories.push(directory);
            }
        }
    }
    if directories.is_empty() {
        if root_manifest.get("workspace").is_some() {
            // A virtual workspace whose members matched nothing holds no code.
            return Ok(());
        }
        return Err(Error::new(format!(
            "`{}` has neither [package] nor [workspace]",
            root.join("Cargo.toml").display()
        )));
    }
    let mut seen_directories = HashSet::new();
    directories.retain(|directory| seen_directories.insert(directory.clone()));
    for directory in directories {
        let manifest = if directory == root {
            root_manifest.clone()
        } else {
            read_manifest(&directory)?
        };
        let Some(spec) = package_spec(&directory, &manifest) else {
            continue;
        };
        package_modules(layout, root, &spec, configuration);
    }
    Ok(())
}

fn read_manifest(directory: &Path) -> Result<Value, Error> {
    let path = directory.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| Error::new(format!("cannot read `{}`: {error}", path.display())))?;
    toml::from_str::<Value>(&text)
        .map_err(|error| Error::new(format!("invalid `{}`: {error}", path.display())))
}

/// One Cargo package: its manifest, its roots, and where it lives.
struct PackageSpec {
    /// The name `[package].name` writes.
    name: String,
    /// The name code calls the package's library: `[lib].name` or the
    /// normalized package name.
    crate_name: String,
    manifest: Value,
    roots: Vec<Root>,
}

/// One file a target compiles as a crate root.
struct Root {
    path: PathBuf,
    is_test: bool,
}

fn package_spec(directory: &Path, manifest: &Value) -> Option<PackageSpec> {
    let name = manifest.get("package")?.get("name")?.as_str()?.to_owned();
    let lib = manifest.get("lib");
    let crate_name = lib
        .and_then(|lib| lib.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map_or_else(|| name.replace('-', "_"), str::to_owned);
    let mut roots = Vec::new();
    let mut add = |path: PathBuf, is_test: bool| add_root(&mut roots, path, is_test);
    // Library root.
    match lib.and_then(|lib| lib.get("path")).and_then(Value::as_str) {
        Some(path) => add(directory.join(path), false),
        None => {
            let default = directory.join("src/lib.rs");
            if lib.is_some() || default.is_file() {
                add(default, false);
            }
        }
    }
    // Bins: those the manifest names, then the ones cargo finds beside them.
    if let Some(bins) = manifest.get("bin").and_then(Value::as_array) {
        for bin in bins {
            let bin_name = bin.get("name").and_then(Value::as_str).unwrap_or(&name);
            match bin.get("path").and_then(Value::as_str) {
                Some(path) => add(directory.join(path), false),
                None => {
                    if bin_name == name && directory.join("src/main.rs").is_file() {
                        add(directory.join("src/main.rs"), false);
                    }
                    for candidate in [
                        directory.join(format!("src/bin/{bin_name}.rs")),
                        directory.join(format!("src/bin/{bin_name}/main.rs")),
                    ] {
                        if candidate.is_file() {
                            add(candidate, false);
                        }
                    }
                }
            }
        }
    }
    let main = directory.join("src/main.rs");
    if main.is_file() {
        add(main, false);
    }
    for candidate in bin_files(&directory.join("src/bin")) {
        add(candidate, false);
    }
    // The build script, whose `#[path]` modules are part of the package too.
    match manifest
        .get("package")
        .and_then(|package| package.get("build"))
    {
        Some(Value::Boolean(true)) => {
            let build = directory.join("build.rs");
            if build.is_file() {
                add(build, false);
            }
        }
        Some(Value::Boolean(false)) => {}
        Some(Value::String(path)) => add(directory.join(path), false),
        _ => {
            let build = directory.join("build.rs");
            if build.is_file() {
                add(build, false);
            }
        }
    }
    // Integration tests, named by the manifest or found in `tests/`.
    if let Some(tests) = manifest.get("test").and_then(Value::as_array) {
        for test in tests {
            if let Some(path) = test.get("path").and_then(Value::as_str) {
                add(directory.join(path), true);
            }
        }
    }
    for candidate in test_files(&directory.join("tests")) {
        add(candidate, true);
    }
    Some(PackageSpec {
        name,
        crate_name,
        manifest: manifest.clone(),
        roots,
    })
}

fn add_root(roots: &mut Vec<Root>, path: PathBuf, is_test: bool) {
    if !path.is_file() {
        return;
    }
    if let Some(existing) = roots.iter_mut().find(|root| root.path == path) {
        existing.is_test &= is_test;
        return;
    }
    roots.push(Root { path, is_test });
}

/// `src/bin/*.rs` and `src/bin/*/main.rs`, sorted.
fn bin_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return files;
    };
    let mut entries: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for entry in entries {
        if entry.is_file() && entry.extension().is_some_and(|extension| extension == "rs") {
            files.push(entry);
        } else if entry.is_dir() {
            let main = entry.join("main.rs");
            if main.is_file() {
                files.push(main);
            }
        }
    }
    files
}

/// `tests/*.rs`, sorted.
fn test_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().is_some_and(|extension| extension == "rs")
        })
        .collect();
    files.sort();
    files
}

/// Workspace members `pattern` names, resolved without cargo.
fn member_directories(
    root: &Path,
    pattern: &str,
    excludes: &[String],
) -> Result<Vec<PathBuf>, Error> {
    let pattern = pattern.trim_end_matches('/');
    if !has_wildcard(pattern) {
        let directory = root.join(pattern);
        let relative = slash_path(Path::new(pattern));
        if matches_any(&relative, excludes) || !directory.join("Cargo.toml").is_file() {
            return Ok(Vec::new());
        }
        return Ok(vec![directory]);
    }
    let mut directories = Vec::new();
    expand_glob(root, pattern, &mut directories);
    directories.retain(|directory| {
        category_matches(root, directory).is_some_and(|relative| !matches_any(&relative, excludes))
    });
    Ok(directories)
}

fn category_matches(root: &Path, directory: &Path) -> Option<String> {
    let relative = directory.strip_prefix(root).ok()?;
    Some(slash_path(relative))
}

/// Expand a workspace member glob such as `crates/*` or `tools/**`.
fn expand_glob(root: &Path, pattern: &str, out: &mut Vec<PathBuf>) {
    let components: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let mut current = vec![root.to_path_buf()];
    for component in components {
        let mut next = Vec::new();
        if component == "**" {
            for base in &current {
                collect_directories(base, &mut next);
            }
        } else if has_wildcard(component) {
            for base in &current {
                next.extend(matching_directories(base, component));
            }
        } else {
            for base in &current {
                let candidate = base.join(component);
                if candidate.is_dir() {
                    next.push(candidate);
                }
            }
        }
        next.sort();
        next.dedup();
        current = next;
        if current.is_empty() {
            return;
        }
    }
    out.extend(
        current
            .into_iter()
            .filter(|directory| directory.join("Cargo.toml").is_file()),
    );
}

fn collect_directories(base: &Path, out: &mut Vec<PathBuf>) {
    out.push(base.to_path_buf());
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        let hidden = child
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        if hidden || child.file_name().is_some_and(|name| name == "target") {
            continue;
        }
        collect_directories(&child, out);
    }
}

fn matching_directories(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut matches: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| wildcard_match(pattern, name))
        })
        .collect();
    matches.sort();
    matches
}

fn has_wildcard(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

/// `*` and `?` matching, where neither crosses a path separator.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut at, mut in_text) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while in_text < text.len() {
        match pattern.get(at) {
            Some('*') => {
                star = Some(at);
                mark = in_text;
                at += 1;
            }
            Some('?') => {
                at += 1;
                in_text += 1;
            }
            Some(ch) if *ch == text[in_text] => {
                at += 1;
                in_text += 1;
            }
            _ => match star {
                Some(star) => {
                    at = star + 1;
                    mark += 1;
                    in_text = mark;
                }
                None => return false,
            },
        }
    }
    pattern[at..].iter().all(|ch| *ch == '*')
}

fn matches_any(relative: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        Glob::new(pattern).is_ok_and(|glob| glob.compile_matcher().is_match(relative))
    })
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Register one package's modules and files.
fn package_modules(
    layout: &mut WorkspaceLayout,
    root: &Path,
    spec: &PackageSpec,
    configuration: &Configuration,
) {
    let package = layout
        .tree
        .child(layout.tree.root(), NodeKind::Package, &spec.crate_name);
    layout.package_names.insert(spec.name.clone(), package);
    layout
        .package_names
        .insert(spec.crate_name.clone(), package);
    let features = cfg::enabled_features(&spec.manifest, configuration);
    let context = CfgContext::new(configuration, features);
    let mut walker = Walker {
        layout,
        root,
        package,
        context,
        flags: HashMap::new(),
        seen: HashSet::new(),
        parser: parser(),
    };
    for target in &spec.roots {
        walker.walk_root(target);
    }
    walker.finish();
}

fn parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("tree-sitter-rust is a linked language");
    parser
}

#[derive(Debug, Clone, Copy, Default)]
struct BuildFlags {
    normal: bool,
    test: bool,
}

struct Walker<'a> {
    layout: &'a mut WorkspaceLayout,
    root: &'a Path,
    package: NodeId,
    context: CfgContext,
    flags: HashMap<NodeId, BuildFlags>,
    seen: HashSet<PathBuf>,
    parser: Parser,
}

impl Walker<'_> {
    fn walk_root(&mut self, target: &Root) {
        let base = target.path.parent().unwrap_or(self.root).to_path_buf();
        let scope = module_directory(&target.path, true);
        self.walk_file(
            target.path.clone(),
            self.package,
            scope,
            base,
            target.is_test,
        );
    }

    fn walk_file(
        &mut self,
        path: PathBuf,
        module: NodeId,
        scope: PathBuf,
        base: PathBuf,
        test_context: bool,
    ) {
        let Some(relative) = self.relative(&path) else {
            return;
        };
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !self.seen.insert(canonical) {
            return;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            return;
        };
        self.layout.file_to_module.insert(relative.clone(), module);
        self.layout.files.push(DiscoveredFile {
            abs_path: path.clone(),
            rel_path: relative.clone(),
            module_id: module,
            package_id: self.package,
            language: Language::Rust,
            is_test: test_context,
        });
        let Some(tree) = self.parser.parse(&source, None) else {
            return;
        };
        self.walk_scope(tree.root_node(), module, scope, base, test_context, &source);
    }

    fn walk_scope(
        &mut self,
        node: Node<'_>,
        module: NodeId,
        directory: PathBuf,
        base: PathBuf,
        test_context: bool,
        source: &str,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() != "mod_item" {
                continue;
            }
            let attributes = cfg::attributes_of(child, source.as_bytes());
            let cfg = cfg::of_attrs(&attributes, &self.context);
            if !cfg.built() {
                continue;
            }
            let Some(name) = child
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
            else {
                continue;
            };
            let test = test_context || cfg.only_for_tests();
            let built_normal = cfg.normal && !test_context;
            let built_test = cfg.test;
            match child.child_by_field_name("body") {
                Some(body) => {
                    let inner = self.layout.tree.child(module, NodeKind::Module, name);
                    self.record(inner, built_normal, built_test);
                    self.walk_scope(
                        body,
                        inner,
                        directory.join(name),
                        base.join(name),
                        test,
                        source,
                    );
                }
                None => {
                    let candidate = cfg::path_of(&attributes, &self.context)
                        .map(|path| base.join(path))
                        .or_else(|| {
                            [
                                directory.join(format!("{name}.rs")),
                                directory.join(name).join("mod.rs"),
                            ]
                            .into_iter()
                            .find(|candidate| candidate.is_file())
                        });
                    let Some(candidate) = candidate.filter(|candidate| candidate.is_file()) else {
                        continue;
                    };
                    let inner = self.layout.tree.child(module, NodeKind::Module, name);
                    self.record(inner, built_normal, built_test);
                    let scope = module_directory(&candidate, false);
                    let file_base = candidate.parent().unwrap_or(&base).to_path_buf();
                    self.walk_file(candidate, inner, scope, file_base, test);
                }
            }
        }
    }

    fn record(&mut self, module: NodeId, normal: bool, test: bool) {
        let flags = self.flags.entry(module).or_default();
        flags.normal |= normal;
        flags.test |= test;
    }

    fn finish(&mut self) {
        let flags: Vec<(NodeId, BuildFlags)> =
            self.flags.iter().map(|(&id, &flags)| (id, flags)).collect();
        for (id, flags) in flags {
            let node = self.layout.tree.node(id);
            if node.kind != NodeKind::Module {
                continue;
            }
            let test = is_test_module(&node.name) || (flags.test && !flags.normal);
            self.layout.tree.node_mut(id).test = test;
        }
    }

    fn relative(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(self.root).ok()?;
        Some(slash_path(relative))
    }
}

/// The directory whose `foo.rs` and `foo/mod.rs` a `mod foo;` in `file` names.
fn module_directory(file: &Path, is_root: bool) -> PathBuf {
    let parent = file.parent().unwrap_or(Path::new("")).to_path_buf();
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if is_root || name == "mod.rs" || name == "lib.rs" || name == "main.rs" {
        parent
    } else {
        let stem = file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("");
        parent.join(stem)
    }
}

/// Names that mark test code even without `#[cfg(test)]`.
pub(crate) fn is_test_module(name: &str) -> bool {
    name == "tests" || name == "test" || name.ends_with("_tests") || name.starts_with("test_")
}

#[cfg(test)]
fn test_workspace(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "deslop-rust-discovery-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("temp workspace");
    directory
}

#[cfg(test)]
fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent directory");
    }
    std::fs::write(path, text).expect("fixture file");
}

#[cfg(test)]
fn discover_in(root: &Path) -> WorkspaceLayout {
    let mut layout = WorkspaceLayout::new(root.to_path_buf());
    discover(root, &Configuration::default(), &mut layout).expect("discovery");
    layout
}

#[cfg(test)]
fn modules(layout: &WorkspaceLayout) -> Vec<String> {
    layout
        .tree
        .ids()
        .filter(|&id| layout.tree.node(id).kind == NodeKind::Module)
        .map(|id| layout.tree.display(id))
        .collect()
}

#[cfg(test)]
fn files(layout: &WorkspaceLayout) -> Vec<String> {
    layout
        .files
        .iter()
        .map(|file| file.rel_path.clone())
        .collect()
}

#[test]
fn a_single_crate_has_a_package_and_its_modules() {
    let root = test_workspace("single");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"my-lib\"\nversion = \"0.1.0\"\n",
    );
    write(&root, "src/lib.rs", "mod kinds;\nmod socket;\n");
    write(&root, "src/kinds.rs", "pub struct Kind;\n");
    write(&root, "src/socket/mod.rs", "mod datagram;\n");
    write(&root, "src/socket/datagram.rs", "pub struct Datagram;\n");
    let layout = discover_in(&root);
    assert_eq!(
        modules(&layout),
        [
            "my_lib::kinds",
            "my_lib::socket",
            "my_lib::socket::datagram"
        ]
    );
    assert_eq!(
        files(&layout),
        [
            "src/lib.rs",
            "src/kinds.rs",
            "src/socket/mod.rs",
            "src/socket/datagram.rs"
        ]
    );
    let package = layout.package_names["my-lib"];
    assert_eq!(layout.tree.display(package), "my_lib");
    assert_eq!(layout.package_names["my_lib"], package);
    let root_file = &layout.files[0];
    assert_eq!(root_file.module_id, package);
    assert_eq!(
        layout
            .files
            .iter()
            .find(|file| file.rel_path == "src/socket/datagram.rs")
            .unwrap()
            .module_id,
        layout
            .tree
            .find_child_by_name(
                layout.tree.find_child_by_name(package, "socket").unwrap(),
                "datagram"
            )
            .unwrap()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_virtual_workspace_expands_members_and_their_excludes() {
    let root = test_workspace("workspace");
    write(
        &root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/old\"]\n",
    );
    write(
        &root,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core-lib\"\nversion = \"0.1.0\"\n",
    );
    write(&root, "crates/core/src/lib.rs", "mod inner;\n");
    write(&root, "crates/core/src/inner.rs", "pub fn f() {}\n");
    write(
        &root,
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(&root, "crates/app/src/main.rs", "mod cli;\n");
    write(&root, "crates/app/src/cli.rs", "pub struct Args;\n");
    write(
        &root,
        "crates/old/Cargo.toml",
        "[package]\nname = \"old\"\nversion = \"0.1.0\"\n",
    );
    write(&root, "crates/old/src/lib.rs", "");
    let layout = discover_in(&root);
    assert_eq!(modules(&layout), ["app::cli", "core_lib::inner"]);
    assert_eq!(
        files(&layout),
        [
            "crates/app/src/main.rs",
            "crates/app/src/cli.rs",
            "crates/core/src/lib.rs",
            "crates/core/src/inner.rs"
        ]
    );
    assert!(layout.package_names.contains_key("core-lib"));
    assert!(layout.package_names.contains_key("app"));
    assert!(!layout.package_names.contains_key("old"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_root_package_beside_its_workspace_members_is_a_package_too() {
    let root = test_workspace("root-package");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"tool\"\nversion = \"0.1.0\"\n\n[workspace]\nmembers = [\"helper\"]\n",
    );
    write(&root, "src/main.rs", "mod report;\n");
    write(&root, "src/report.rs", "pub fn f() {}\n");
    write(
        &root,
        "helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\n",
    );
    write(&root, "helper/src/lib.rs", "pub fn g() {}\n");
    let layout = discover_in(&root);
    assert_eq!(
        files(&layout),
        ["src/main.rs", "src/report.rs", "helper/src/lib.rs"]
    );
    assert_eq!(modules(&layout), ["tool::report"]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bins_explicit_targets_and_integration_tests_are_roots() {
    let root = test_workspace("targets");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"wt\"\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"git-wt\"\npath = \"src/git_wt.rs\"\n\n[[test]]\nname = \"it\"\npath = \"tests/it.rs\"\n",
    );
    write(&root, "src/main.rs", "mod cli;\nfn main() {}\n");
    write(&root, "src/cli.rs", "pub struct Args;\n");
    write(&root, "src/git_wt.rs", "include!(\"main.rs\");\n");
    write(&root, "src/bin/extra.rs", "fn main() {}\n");
    write(&root, "tests/it.rs", "mod support;\n");
    write(&root, "tests/support.rs", "pub fn helper() {}\n");
    let layout = discover_in(&root);
    assert_eq!(
        files(&layout),
        [
            "src/git_wt.rs",
            "src/main.rs",
            "src/cli.rs",
            "src/bin/extra.rs",
            "tests/it.rs",
            "tests/support.rs"
        ]
    );
    let test_files: Vec<&str> = layout
        .files
        .iter()
        .filter(|file| file.is_test)
        .map(|file| file.rel_path.as_str())
        .collect();
    assert_eq!(test_files, ["tests/it.rs", "tests/support.rs"]);
    // `src/git_wt.rs` is a bin root, so its `include!` target keeps `src/`.
    assert_eq!(
        layout
            .files
            .iter()
            .find(|file| file.rel_path == "src/main.rs")
            .unwrap()
            .module_id,
        layout.package_names["wt"]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn path_attributes_and_cfg_gates_choose_files_and_mark_test_modules() {
    let root = test_workspace("gates");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"gated\"\nversion = \"0.1.0\"\n\n[features]\ndefault = [\"metrics\"]\nmetrics = []\nexport = []\n",
    );
    write(
        &root,
        "src/lib.rs",
        "#[cfg(windows)]\npub mod console;\n#[cfg(feature = \"export\")]\npub mod export;\n#[cfg(feature = \"metrics\")]\npub mod metrics;\n#[cfg(test)]\nmod support;\n#[path = \"moved/real.rs\"]\nmod moved;\n",
    );
    write(&root, "src/console.rs", "");
    write(&root, "src/export.rs", "");
    write(&root, "src/metrics.rs", "");
    write(&root, "src/support.rs", "");
    write(&root, "src/moved/real.rs", "");
    let layout = discover_in(&root);
    let names = modules(&layout);
    assert_eq!(names, ["gated::metrics", "gated::support", "gated::moved"]);
    assert!(
        layout.tree.is_test(
            layout
                .tree
                .find_child_by_name(layout.package_names["gated"], "support")
                .unwrap()
        )
    );
    assert!(
        !layout.tree.is_test(
            layout
                .tree
                .find_child_by_name(layout.package_names["gated"], "moved")
                .unwrap()
        )
    );
    let support = layout
        .files
        .iter()
        .find(|file| file.rel_path == "src/support.rs")
        .unwrap();
    assert!(support.is_test);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn features_from_the_manifest_and_the_command_line_select_cfg_gated_modules() {
    let root = test_workspace("features");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"gated\"\nversion = \"0.1.0\"\n\n[features]\ndefault = [\"metrics\"]\nmetrics = []\nexport = []\n",
    );
    write(
        &root,
        "src/lib.rs",
        "#[cfg(feature = \"export\")]\npub mod export;\n#[cfg(feature = \"metrics\")]\npub mod metrics;\n",
    );
    write(&root, "src/export.rs", "");
    write(&root, "src/metrics.rs", "");
    let base = discover_in(&root);
    assert_eq!(modules(&base), ["gated::metrics"]);

    let mut features = Configuration::default();
    features.features("export");
    let mut layout = WorkspaceLayout::new(root.to_path_buf());
    discover(&root, &features, &mut layout).unwrap();
    assert_eq!(modules(&layout), ["gated::export", "gated::metrics"]);

    let mut all = Configuration::default();
    all.all_features();
    let mut layout = WorkspaceLayout::new(root.to_path_buf());
    discover(&root, &all, &mut layout).unwrap();
    assert_eq!(modules(&layout), ["gated::export", "gated::metrics"]);

    let mut none = Configuration::default();
    none.no_default_features();
    let mut layout = WorkspaceLayout::new(root.to_path_buf());
    discover(&root, &none, &mut layout).unwrap();
    assert_eq!(modules(&layout), Vec::<String>::new());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_cfg_test_module_shares_a_name_with_the_library_module_it_also_is() {
    let root = test_workspace("shared");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"clean\"\nversion = \"0.1.0\"\n",
    );
    write(&root, "src/lib.rs", "pub mod queue;\n");
    write(&root, "src/queue.rs", "pub fn f() {}\n");
    write(&root, "tests/main.rs", "mod queue;\n");
    write(&root, "tests/queue/mod.rs", "mod part;\n");
    write(&root, "tests/queue/part.rs", "#[test]\nfn runs() {}\n");
    let layout = discover_in(&root);
    let package = layout.package_names["clean"];
    let queue = layout.tree.find_child_by_name(package, "queue").unwrap();
    assert!(
        !layout.tree.is_test(queue),
        "the library builds `queue` too"
    );
    let part = layout.tree.find_child_by_name(queue, "part").unwrap();
    assert!(layout.tree.is_test(part));
    assert_eq!(modules(&layout), ["clean::queue", "clean::queue::part"]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_empty_virtual_workspace_discovers_nothing() {
    let root = test_workspace("empty-workspace");
    write(&root, "Cargo.toml", "[workspace]\nmembers = []\n");
    let layout = discover_in(&root);
    assert!(layout.files.is_empty());
    assert_eq!(layout.tree.ids().count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_manifest_with_no_package_and_no_workspace_is_an_error() {
    let root = test_workspace("no-package");
    write(&root, "Cargo.toml", "[profile.release]\nlto = true\n");
    let mut layout = WorkspaceLayout::new(root.to_path_buf());
    let error = discover(&root, &Configuration::default(), &mut layout).expect_err("no package");
    assert!(
        error
            .to_string()
            .contains("neither [package] nor [workspace]"),
        "{error}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn wildcard_matching_is_path_free() {
    assert!(!wildcard_match("crates/*", "crates"));
    assert!(wildcard_match("*", "crates"));
    assert!(!wildcard_match("crates?", "crates"));
    assert!(wildcard_match("crate?", "crates"));
    assert!(wildcard_match("c*", "crates"));
    assert!(!wildcard_match("d*", "crates"));
    assert!(wildcard_match("a*b*c", "axbyc"));
    assert!(wildcard_match("a*b*c", "abc"));
    assert!(!wildcard_match("a*b*c", "abd"));
}

#[test]
fn module_directories_follow_rust_file_layout() {
    let path = |path: &str| PathBuf::from(path);
    assert_eq!(module_directory(&path("src/lib.rs"), true), path("src"));
    assert_eq!(
        module_directory(&path("src/bin/tool.rs"), true),
        path("src/bin")
    );
    assert_eq!(
        module_directory(&path("src/socket/mod.rs"), false),
        path("src/socket")
    );
    assert_eq!(
        module_directory(&path("src/socket.rs"), false),
        path("src/socket")
    );
    assert_eq!(module_directory(&path("tests/it.rs"), true), path("tests"));
}

#[test]
fn build_scripts_are_indexed_so_their_path_modules_exist() {
    let root = test_workspace("build-script");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"flags\"\nversion = \"0.1.0\"\n",
    );
    write(&root, "src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    write(
        &root,
        "build.rs",
        "#[path = \"src/flags.rs\"]\nmod flags;\nfn main() {}\n",
    );
    write(&root, "src/flags.rs", "pub struct Flags;\n");
    let layout = discover_in(&root);
    assert_eq!(files(&layout), ["src/lib.rs", "build.rs", "src/flags.rs"]);
    assert_eq!(modules(&layout), ["flags::flags"]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn external_modules_of_inline_and_non_root_files_follow_the_file_layout() {
    let root = test_workspace("layout");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\n",
    );
    write(
        &root,
        "src/lib.rs",
        "mod outer {\n    mod inner;\n}\nmod leaf;\n",
    );
    write(&root, "src/outer/inner.rs", "pub fn f() {}\n");
    write(&root, "src/leaf.rs", "mod child;\n");
    write(&root, "src/leaf/child.rs", "pub fn g() {}\n");
    let layout = discover_in(&root);
    assert_eq!(
        modules(&layout),
        ["p::outer", "p::outer::inner", "p::leaf", "p::leaf::child"]
    );
    assert_eq!(
        files(&layout),
        [
            "src/lib.rs",
            "src/outer/inner.rs",
            "src/leaf.rs",
            "src/leaf/child.rs"
        ]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_path_attribute_inside_a_non_root_file_is_relative_to_its_source_directory() {
    let root = test_workspace("path-attribute");
    write(
        &root,
        "Cargo.toml",
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\n",
    );
    write(&root, "src/lib.rs", "mod branch;\n");
    write(
        &root,
        "src/branch.rs",
        "#[path = \"elsewhere/moved.rs\"]\nmod moved;\n",
    );
    write(&root, "src/elsewhere/moved.rs", "pub fn h() {}\n");
    let layout = discover_in(&root);
    assert_eq!(modules(&layout), ["p::branch", "p::branch::moved"]);
    std::fs::remove_dir_all(root).unwrap();
}
