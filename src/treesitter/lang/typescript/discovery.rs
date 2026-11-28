//! Multi-package TypeScript/JavaScript workspace discovery.

use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde_json::Value;

use super::is_source_file;
use crate::Error;
use crate::model::{NodeId, NodeKind};
use crate::treesitter::lang::{DiscoveredFile, Language, WorkspaceLayout};

/// Directories never descended into, whatever `.gitignore` says.
const SKIP_DIRECTORIES: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".turbo",
    "coverage",
    ".git",
];

/// A discovered package: its display name, its directory, and its workspace-relative path.
struct PackageDir {
    name: String,
    dir: PathBuf,
}

/// Discover every package and source file under `root`, filling `layout`.
pub(super) fn discover(root: &Path, layout: &mut WorkspaceLayout) -> Result<(), Error> {
    for package in packages(root)? {
        add_package(root, &package, layout)?;
    }
    Ok(())
}

/// The workspace's packages, in a deterministic order.
fn packages(root: &Path) -> Result<Vec<PackageDir>, Error> {
    let root_package = read_package_json(root)?;
    let mut patterns = workspace_patterns(root_package.as_ref());
    patterns.extend(pnpm_patterns(root)?);
    patterns.sort();
    patterns.dedup();
    if patterns.is_empty() {
        return Ok(vec![PackageDir {
            name: package_name(root_package.as_ref(), root),
            dir: root.to_path_buf(),
        }]);
    }
    let mut packages = Vec::new();
    for relative in expand_patterns(root, &patterns)? {
        let dir = root.join(&relative);
        let package = read_package_json(&dir)?;
        packages.push(PackageDir {
            name: package_name(package.as_ref(), &dir),
            dir,
        });
    }
    if packages.is_empty() {
        packages.push(PackageDir {
            name: package_name(root_package.as_ref(), root),
            dir: root.to_path_buf(),
        });
    }
    Ok(packages)
}

fn package_name(package: Option<&Value>, dir: &Path) -> String {
    package
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            dir.file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "root".to_owned())
}

fn read_package_json(dir: &Path) -> Result<Option<Value>, Error> {
    let path = dir.join("package.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| Error::new(format!("cannot read `{}`: {e}", path.display())))?;
    let value = serde_json::from_str(&text)
        .map_err(|e| Error::new(format!("invalid `{}`: {e}", path.display())))?;
    Ok(Some(value))
}

/// npm, yarn, and pnpm workspace patterns from a root `package.json`.
fn workspace_patterns(root_package: Option<&Value>) -> Vec<String> {
    let Some(workspaces) = root_package.and_then(|value| value.get("workspaces")) else {
        return Vec::new();
    };
    let entries = match workspaces {
        Value::Array(entries) => entries.as_slice(),
        Value::Object(map) => match map.get("packages").and_then(Value::as_array) {
            Some(entries) => entries.as_slice(),
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    entries
        .iter()
        .filter_map(Value::as_str)
        .map(|pattern| pattern.trim_end_matches('/').to_owned())
        .filter(|pattern| !pattern.is_empty() && !pattern.starts_with('!'))
        .collect()
}

/// Patterns listed under `packages:` in `pnpm-workspace.yaml`.
fn pnpm_patterns(root: &Path) -> Result<Vec<String>, Error> {
    let path = root.join("pnpm-workspace.yaml");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| Error::new(format!("cannot read `{}`: {e}", path.display())))?;
    let mut patterns = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        if !indented && !trimmed.starts_with('-') {
            in_packages = trimmed.starts_with("packages:");
            continue;
        }
        if !in_packages {
            continue;
        }
        let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("-\t"))
        else {
            continue;
        };
        let item = unquote(item.trim());
        if item.is_empty() || item.starts_with('!') {
            continue;
        }
        patterns.push(item.trim_end_matches('/').to_owned());
    }
    Ok(patterns)
}

fn unquote(text: &str) -> &str {
    for quote in ['\'', '"'] {
        if let Some(rest) = text.strip_prefix(quote)
            && let Some(inner) = rest.strip_suffix(quote)
        {
            return inner;
        }
    }
    text
}

/// Directories matching the workspace globs that actually hold a `package.json`.
fn expand_patterns(root: &Path, patterns: &[String]) -> Result<Vec<String>, Error> {
    let mut builder = GlobSetBuilder::new();
    let mut bounded = true;
    let mut depth = 0;
    for pattern in patterns {
        let pattern = pattern.trim_start_matches("./");
        let glob = Glob::new(pattern)
            .map_err(|e| Error::new(format!("invalid workspace glob `{pattern}`: {e}")))?;
        builder.add(glob);
        bounded &= !pattern.contains("**");
        depth = depth.max(pattern.split('/').count());
    }
    let globs = builder
        .build()
        .map_err(|e| Error::new(format!("failed to compile workspace globs: {e}")))?;
    let mut walk = WalkBuilder::new(root);
    walk.standard_filters(true);
    walk.follow_links(false);
    walk.max_depth(bounded.then_some(depth));
    walk.sort_by_file_path(|a, b| a.cmp(b));
    walk.filter_entry(|entry| {
        !(entry.file_type().is_some_and(|kind| kind.is_dir()) && is_skipped_directory(entry.path()))
    });
    let mut found = Vec::new();
    for entry in walk.build() {
        let entry =
            entry.map_err(|e| Error::new(format!("cannot walk `{}`: {e}", root.display())))?;
        if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        let relative = relative_path(root, entry.path());
        if relative.is_empty() || !globs.is_match(&relative) {
            continue;
        }
        if !entry.path().join("package.json").is_file() {
            continue;
        }
        found.push(relative);
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn add_package(
    root: &Path,
    package: &PackageDir,
    layout: &mut WorkspaceLayout,
) -> Result<(), Error> {
    let package_id = layout.tree.child_with_lang(
        layout.tree.root(),
        NodeKind::Package,
        &package.name,
        Some(Language::TypeScript),
    );
    layout
        .package_names
        .entry(package.name.clone())
        .or_insert(package_id);
    add_files(root, package, package_id, layout)
}

struct SourceFile {
    absolute: PathBuf,
    relative: String,
    inside_package: String,
}

fn add_files(
    root: &Path,
    package: &PackageDir,
    package_id: NodeId,
    layout: &mut WorkspaceLayout,
) -> Result<(), Error> {
    let mut walk = WalkBuilder::new(&package.dir);
    walk.standard_filters(true);
    walk.follow_links(false);
    walk.sort_by_file_path(|a, b| a.cmp(b));
    walk.filter_entry(|entry| {
        !(entry.file_type().is_some_and(|kind| kind.is_dir()) && is_skipped_directory(entry.path()))
    });
    let mut sources = Vec::new();
    for entry in walk.build() {
        let entry = entry
            .map_err(|e| Error::new(format!("cannot walk `{}`: {e}", package.dir.display())))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !is_source_file(&name) {
            continue;
        }
        sources.push(SourceFile {
            absolute: entry.path().to_path_buf(),
            relative: relative_path(root, entry.path()),
            inside_package: relative_path(&package.dir, entry.path()),
        });
    }
    sources.sort_by(|a, b| a.relative.cmp(&b.relative));
    for source in sources {
        let components: Vec<&str> = source.inside_package.split('/').collect();
        let Some((file_name, directories)) = components.split_last() else {
            continue;
        };
        let mut parent = package_id;
        for (depth, directory) in directories.iter().enumerate() {
            parent = layout.tree.child(parent, NodeKind::Module, directory);
            if is_test_directory(directory, depth) {
                layout.tree.node_mut(parent).test = true;
            }
        }
        let is_test = is_test_path(directories, file_name);
        let module_id = if is_barrel(file_name) {
            parent
        } else {
            let stem = file_stem(file_name);
            layout.tree.child(parent, NodeKind::Module, stem)
        };
        if is_test {
            layout.tree.node_mut(module_id).test = true;
        }
        let relative = source.relative;
        layout.files.push(DiscoveredFile {
            abs_path: source.absolute,
            rel_path: relative.clone(),
            module_id,
            package_id,
            language: Language::TypeScript,
            is_test,
        });
        layout.file_to_module.insert(relative, module_id);
    }
    Ok(())
}

/// `index.{ts,tsx,js,jsx,mts,cts,mjs,cjs}` belongs to its directory, like Rust's `mod.rs`.
fn is_barrel(file_name: &str) -> bool {
    file_stem(file_name) == "index"
}

fn file_stem(file_name: &str) -> &str {
    file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem)
}

fn is_test_directory(name: &str, depth: usize) -> bool {
    name == "__tests__"
        || name == "__mocks__"
        || (depth == 0 && (name == "test" || name == "tests"))
}

fn is_test_path(directories: &[&str], file_name: &str) -> bool {
    file_name.contains(".test.")
        || file_name.contains(".spec.")
        || directories
            .iter()
            .enumerate()
            .any(|(depth, name)| is_test_directory(name, depth))
}

fn is_skipped_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIP_DIRECTORIES.contains(&name))
}

/// A path relative to `base`, `/`-separated, with `.` components removed.
fn relative_path(base: &Path, path: &Path) -> String {
    let Ok(relative) = path.strip_prefix(base) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::CurDir => {}
            _ => return String::new(),
        }
    }
    parts.join("/")
}
#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::super::TypeScriptFrontend;
    use super::*;
    use crate::configuration::Configuration;
    use crate::treesitter::lang::{Language, LanguageFrontend, WorkspaceLayout};

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
    }

    fn discover(root: &Path) -> WorkspaceLayout {
        let mut layout = WorkspaceLayout::new(root.to_path_buf());
        TypeScriptFrontend::new()
            .discover(root, &Configuration::default(), &mut layout)
            .expect("discovery succeeds");
        layout
    }

    fn paths(layout: &WorkspaceLayout) -> Vec<&str> {
        layout
            .files
            .iter()
            .map(|file| file.rel_path.as_str())
            .collect()
    }

    fn display(layout: &WorkspaceLayout, path: &str) -> String {
        let module = *layout
            .file_to_module
            .get(path)
            .unwrap_or_else(|| panic!("no module for `{path}`"));
        layout.tree.display(module)
    }

    fn package_names(layout: &WorkspaceLayout) -> Vec<&str> {
        let mut names = Vec::new();
        for id in layout.tree.ids() {
            if layout.tree.node(id).kind == NodeKind::Package {
                names.push(layout.tree.node(id).name.as_str());
            }
        }
        names
    }

    #[test]
    fn a_single_package_yields_barrels_tests_and_nested_modules() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "package.json", r#"{"name":"app"}"#);
        write(root, "src/index.ts", "export const a = 1;");
        write(root, "src/queue/index.ts", "export const b = 2;");
        write(root, "src/queue/ring.ts", "export const c = 3;");
        write(root, "src/queue.test.ts", "export const d = 4;");
        write(root, "src/__tests__/helper.ts", "export const e = 5;");
        write(root, "src/main.spec.tsx", "export const f = 6;");
        write(root, "tests/e2e.ts", "export const g = 7;");
        write(root, "src/types.d.ts", "declare const h: number;");
        write(root, "node_modules/dep/index.js", "module.exports = 1;");
        write(root, "dist/bundle.js", "export default 1;");
        write(root, "README.md", "# app");

        let layout = discover(root);
        assert_eq!(
            paths(&layout),
            [
                "src/__tests__/helper.ts",
                "src/index.ts",
                "src/main.spec.tsx",
                "src/queue.test.ts",
                "src/queue/index.ts",
                "src/queue/ring.ts",
                "tests/e2e.ts",
            ]
        );
        assert_eq!(
            display(&layout, "src/index.ts"),
            "app/src",
            "a barrel belongs to its directory"
        );
        assert_eq!(display(&layout, "src/queue/index.ts"), "app/src/queue");
        assert_eq!(display(&layout, "src/queue/ring.ts"), "app/src/queue/ring");
        assert_eq!(display(&layout, "src/queue.test.ts"), "app/src/queue.test");
        assert_eq!(
            display(&layout, "src/__tests__/helper.ts"),
            "app/src/__tests__/helper"
        );
        assert_eq!(display(&layout, "tests/e2e.ts"), "app/tests/e2e");

        let test_of = |path: &str| {
            layout
                .files
                .iter()
                .find(|file| file.rel_path == path)
                .unwrap()
                .is_test
        };
        assert!(test_of("src/queue.test.ts"));
        assert!(test_of("src/main.spec.tsx"));
        assert!(test_of("src/__tests__/helper.ts"));
        assert!(test_of("tests/e2e.ts"));
        assert!(!test_of("src/index.ts"));
        assert!(!test_of("src/queue/ring.ts"));

        assert_eq!(package_names(&layout), ["app"]);
        let package = layout.package_names["app"];
        assert_eq!(layout.tree.node(package).kind, NodeKind::Package);
        assert_eq!(
            layout.tree.node(package).language,
            Some(Language::TypeScript)
        );
        assert_eq!(layout.tree.parent(package), Some(layout.tree.root()));
        assert!(
            layout
                .files
                .iter()
                .all(|file| file.language == Language::TypeScript)
        );
    }

    #[test]
    fn npm_workspaces_create_one_package_per_member() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(
            root,
            "package.json",
            r#"{"name":"monorepo","private":true,"workspaces":["packages/*"]}"#,
        );
        write(root, "packages/a/package.json", r#"{"name":"@app/a"}"#);
        write(root, "packages/a/src/index.ts", "export const a = 1;");
        write(root, "packages/a/src/deep/leaf.ts", "export const b = 2;");
        write(root, "packages/a/test/unit.ts", "export const c = 3;");
        write(root, "packages/b/package.json", r#"{"name":"@app/b"}"#);
        write(root, "packages/b/src/main.ts", "export const d = 4;");
        write(root, "packages/not-a-package/README.md", "ignored");
        write(root, "src/root.ts", "export const e = 5;");

        let layout = discover(root);
        assert_eq!(package_names(&layout), ["@app/a", "@app/b"]);
        assert_eq!(
            paths(&layout),
            [
                "packages/a/src/deep/leaf.ts",
                "packages/a/src/index.ts",
                "packages/a/test/unit.ts",
                "packages/b/src/main.ts",
            ]
        );
        assert_eq!(display(&layout, "packages/a/src/index.ts"), "@app/a/src");
        assert_eq!(
            display(&layout, "packages/a/src/deep/leaf.ts"),
            "@app/a/src/deep/leaf"
        );
        assert_eq!(
            display(&layout, "packages/b/src/main.ts"),
            "@app/b/src/main"
        );
        assert_eq!(
            layout.tree.parent(layout.package_names["@app/a"]),
            Some(layout.tree.root())
        );
        assert!(
            !layout.package_names.contains_key("monorepo"),
            "the workspace root is no package"
        );
        assert!(
            layout
                .files
                .iter()
                .find(|file| file.rel_path == "packages/a/test/unit.ts")
                .unwrap()
                .is_test
        );
    }

    #[test]
    fn workspace_objects_and_pnpm_yaml_are_supported() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(
            root,
            "package.json",
            r#"{"name":"monorepo","workspaces":{"packages":["libs/*"]}}"#,
        );
        write(root, "libs/core/package.json", r#"{"name":"core"}"#);
        write(root, "libs/core/src/index.ts", "export const a = 1;");

        let layout = discover(root);
        assert_eq!(package_names(&layout), ["core"]);
        assert_eq!(paths(&layout), ["libs/core/src/index.ts"]);

        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "package.json", r#"{"name":"monorepo"}"#);
        write(
            root,
            "pnpm-workspace.yaml",
            "packages:\n  - 'apps/*'\n  - \"tools/*\"\n",
        );
        write(root, "apps/web/package.json", r#"{"name":"web"}"#);
        write(root, "apps/web/src/app.tsx", "export const a = 1;");
        write(root, "tools/cli/package.json", r#"{"name":"cli"}"#);
        write(root, "tools/cli/src/main.ts", "export const b = 2;");

        let layout = discover(root);
        assert_eq!(
            package_names(&layout),
            ["web", "cli"],
            "packages follow directory order"
        );
        assert_eq!(
            paths(&layout),
            ["apps/web/src/app.tsx", "tools/cli/src/main.ts"]
        );
    }

    #[test]
    fn extension_and_ignore_rules_select_only_source_files() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "package.json", r#"{"name":"app"}"#);
        for name in [
            "a.ts", "b.tsx", "c.mts", "d.cts", "e.js", "f.jsx", "g.mjs", "h.cjs", "i.d.ts",
            "j.d.mts", "k.d.cts", "l.json", "m.md",
        ] {
            write(root, name, "export const x = 1;");
        }
        write(root, "coverage/report.ts", "export const x = 1;");
        write(root, "out/generated.ts", "export const x = 1;");
        write(root, ".next/page.tsx", "export const x = 1;");

        let layout = discover(root);
        assert_eq!(
            paths(&layout),
            [
                "a.ts", "b.tsx", "c.mts", "d.cts", "e.js", "f.jsx", "g.mjs", "h.cjs"
            ]
        );
    }

    #[test]
    fn package_name_falls_back_to_the_directory_name() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "package.json", r#"{"private":true}"#);
        write(root, "main.ts", "export const a = 1;");

        let layout = discover(root);
        let expected = root.file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(package_names(&layout), [expected.as_str()]);
        assert_eq!(display(&layout, "main.ts"), format!("{expected}/main"));
    }

    #[test]
    fn detect_recognizes_javascript_and_typescript_projects() {
        let frontend = TypeScriptFrontend::new();
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        assert!(!frontend.detect(root));
        write(root, "tsconfig.json", "{}");
        assert!(frontend.detect(root));

        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "jsconfig.json", "{}");
        assert!(frontend.detect(root));

        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "package.json", "{}");
        assert!(frontend.detect(root));
    }
}
