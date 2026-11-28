//! Go workspace discovery: the modules a `go.work` or `go.mod` declares and
//! the packages inside each of them.
//!
//! A Go package is a directory, so its tree shape is `Package` (the module's
//! import path) with one nested `Module` per subdirectory. Every `.go` file of
//! a directory shares that directory's node, whatever its name.

use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use super::is_source_file;
use crate::Error;
use crate::model::NodeKind;
use crate::treesitter::lang::{DiscoveredFile, Language, WorkspaceLayout};

/// Directories never descended into, whatever `.gitignore` says.
const SKIP_DIRECTORIES: &[&str] = &["vendor", "testdata", "node_modules", ".git"];

/// A module directory and the import path its `go.mod` declares.
struct GoModule {
    dir: PathBuf,
    import_path: String,
}

/// Discover every module and `.go` file under `root`, filling `layout`.
pub(super) fn discover(root: &Path, layout: &mut WorkspaceLayout) -> Result<(), Error> {
    for module in modules(root)? {
        add_module(root, &module, layout)?;
    }
    Ok(())
}

/// The modules of the workspace: what `go.work` uses, or the root module.
fn modules(root: &Path) -> Result<Vec<GoModule>, Error> {
    let dirs = work_module_dirs(root)?.unwrap_or_else(|| vec![root.to_path_buf()]);
    let mut modules = Vec::new();
    for dir in dirs {
        if let Some(import_path) = module_import_path(&dir)? {
            modules.push(GoModule { dir, import_path });
        }
    }
    if modules.is_empty()
        && let Some(import_path) = module_import_path(root)?
    {
        modules.push(GoModule {
            dir: root.to_path_buf(),
            import_path,
        });
    }
    Ok(modules)
}

/// Directories a `go.work` lists with `use`; `None` when there is no `go.work`.
fn work_module_dirs(root: &Path) -> Result<Option<Vec<PathBuf>>, Error> {
    let path = root.join("go.work");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| Error::new(format!("cannot read `{}`: {e}", path.display())))?;
    let mut dirs = Vec::new();
    let mut block = false;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if block {
            if line == ")" {
                block = false;
            } else if let Some(dir) = use_entry(line) {
                dirs.push(dir);
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("use") else {
            continue;
        };
        if !rest.starts_with([' ', '\t', '(']) {
            continue;
        }
        let rest = rest.trim();
        if rest.starts_with('(') {
            block = true;
        } else if let Some(dir) = use_entry(rest) {
            dirs.push(dir);
        }
    }
    Ok(Some(dirs.into_iter().map(|dir| root.join(dir)).collect()))
}

/// One `use` entry: a directory, optionally quoted.
fn use_entry(line: &str) -> Option<PathBuf> {
    let entry = unquote(line);
    (!entry.is_empty()).then(|| PathBuf::from(entry))
}

/// The `module <import_path>` of `dir`'s `go.mod`.
fn module_import_path(dir: &Path) -> Result<Option<String>, Error> {
    let path = dir.join("go.mod");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| Error::new(format!("cannot read `{}`: {e}", path.display())))?;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let Some(rest) = line.strip_prefix("module") else {
            continue;
        };
        if !rest.starts_with([' ', '\t', '"', '`']) {
            continue;
        }
        let name = unquote(rest.trim());
        if !name.is_empty() {
            return Ok(Some(name.to_owned()));
        }
    }
    Ok(None)
}

/// `line` without its `//` comment.
fn strip_comment(line: &str) -> &str {
    line.split_once("//").map_or(line, |(code, _)| code)
}

fn unquote(text: &str) -> &str {
    for quote in ['\'', '"', '`'] {
        if let Some(rest) = text.strip_prefix(quote)
            && let Some(inner) = rest.strip_suffix(quote)
        {
            return inner;
        }
    }
    text
}

/// Register `module`'s package and every `.go` file in it.
fn add_module(root: &Path, module: &GoModule, layout: &mut WorkspaceLayout) -> Result<(), Error> {
    let package = layout.tree.child_with_lang(
        layout.tree.root(),
        NodeKind::Package,
        &module.import_path,
        Some(Language::Go),
    );
    layout
        .package_names
        .entry(module.import_path.clone())
        .or_insert(package);
    let module_dir = module.dir.clone();
    let mut walk = WalkBuilder::new(&module_dir);
    walk.standard_filters(true);
    walk.follow_links(false);
    walk.sort_by_file_path(|a, b| a.cmp(b));
    walk.filter_entry(move |entry| {
        let path = entry.path();
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            if is_skipped_directory(path) {
                return false;
            }
            // A directory with its own `go.mod` is a module of its own.
            if path != module_dir && path.join("go.mod").is_file() {
                return false;
            }
        }
        true
    });
    let mut sources = Vec::new();
    for entry in walk.build() {
        let entry = entry
            .map_err(|e| Error::new(format!("cannot walk `{}`: {e}", module.dir.display())))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if !is_source_file(entry.file_name().to_string_lossy().as_ref()) {
            continue;
        }
        sources.push(SourceFile {
            absolute: entry.path().to_path_buf(),
            relative: relative_path(root, entry.path()),
            inside_module: relative_path(&module.dir, entry.path()),
        });
    }
    sources.sort_by(|a, b| a.relative.cmp(&b.relative));
    for source in sources {
        let components: Vec<&str> = source.inside_module.split('/').collect();
        let Some((file_name, directories)) = components.split_last() else {
            continue;
        };
        let mut module_id = package;
        for directory in directories {
            module_id = layout.tree.child(module_id, NodeKind::Module, directory);
        }
        let relative = source.relative;
        layout.files.push(DiscoveredFile {
            abs_path: source.absolute,
            rel_path: relative.clone(),
            module_id,
            package_id: package,
            language: Language::Go,
            is_test: file_name.ends_with("_test.go"),
        });
        layout.file_to_module.insert(relative, module_id);
    }
    Ok(())
}

struct SourceFile {
    absolute: PathBuf,
    relative: String,
    inside_module: String,
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
    use std::path::Path;

    use tempfile::TempDir;

    use super::super::GoFrontend;
    use super::*;
    use crate::configuration::Configuration;
    use crate::treesitter::lang::LanguageFrontend;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
    }

    fn discover(root: &Path) -> WorkspaceLayout {
        let mut layout = WorkspaceLayout::new(root.to_path_buf());
        GoFrontend
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
            let node = layout.tree.node(id);
            if node.kind == NodeKind::Package {
                names.push(node.name.as_str());
            }
        }
        names
    }

    #[test]
    fn a_go_module_becomes_a_package_with_one_module_per_directory() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(
            root,
            "go.mod",
            "// the module line carries the import path\nmodule \"example.com/shop\" // trailing\n\ngo 1.22\n",
        );
        write(root, "main.go", "package main\n");
        write(root, "queue/queue.go", "package queue\n");
        write(root, "queue/methods.go", "package queue\n");
        write(root, "queue/queue_test.go", "package queue\n");
        write(root, "queue/internal/ring/ring.go", "package ring\n");
        write(root, "queue/internal/ring/ring_test.go", "package ring\n");
        write(root, "queue/vendor/dep/dep.go", "package dep\n");
        write(root, "queue/testdata/sample.go", "package sample\n");
        write(root, "queue/nested/go.mod", "module example.com/nested\n");
        write(root, "queue/nested/nested.go", "package nested\n");
        write(root, "queue/.hidden/hidden.go", "package hidden\n");
        write(root, "queue/README.md", "# queue\n");

        let layout = discover(root);
        assert_eq!(
            paths(&layout),
            [
                "main.go",
                "queue/internal/ring/ring.go",
                "queue/internal/ring/ring_test.go",
                "queue/methods.go",
                "queue/queue.go",
                "queue/queue_test.go",
            ]
        );
        assert_eq!(package_names(&layout), ["example.com/shop"]);
        let package = layout.package_names["example.com/shop"];
        assert_eq!(layout.tree.node(package).kind, NodeKind::Package);
        assert_eq!(layout.tree.node(package).language, Some(Language::Go));
        assert_eq!(layout.tree.parent(package), Some(layout.tree.root()));

        assert_eq!(display(&layout, "main.go"), "example.com/shop");
        assert_eq!(display(&layout, "queue/queue.go"), "example.com/shop/queue");
        assert_eq!(
            layout.file_to_module["queue/queue.go"], layout.file_to_module["queue/methods.go"],
            "a package is a directory, whatever its files are called"
        );
        assert_eq!(
            display(&layout, "queue/internal/ring/ring.go"),
            "example.com/shop/queue/internal/ring"
        );

        let test_of = |path: &str| {
            layout
                .files
                .iter()
                .find(|file| file.rel_path == path)
                .unwrap()
                .is_test
        };
        assert!(test_of("queue/queue_test.go"));
        assert!(test_of("queue/internal/ring/ring_test.go"));
        assert!(!test_of("queue/queue.go"));
        assert!(
            !layout.tree.node(package).test,
            "the package has non-test files too"
        );
    }

    #[test]
    fn a_go_work_lists_the_modules_it_uses() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(
            root,
            "go.work",
            "go 1.22\n\nuse (\n\t./services/api // the API\n\t\"./libs/core\"\n)\n\nuse ./tools/cli\n",
        );
        write(root, "services/api/go.mod", "module example.com/api\n");
        write(root, "services/api/handler.go", "package api\n");
        write(
            root,
            "services/api/internal/auth/token.go",
            "package auth\n",
        );
        write(root, "libs/core/go.mod", "module example.com/core\n");
        write(root, "libs/core/core.go", "package core\n");
        write(root, "tools/cli/go.mod", "module example.com/cli\n");
        write(root, "tools/cli/main.go", "package main\n");
        write(root, "loose/loose.go", "package loose\n");

        let layout = discover(root);
        assert_eq!(
            package_names(&layout),
            ["example.com/api", "example.com/core", "example.com/cli"]
        );
        assert_eq!(
            paths(&layout),
            [
                "services/api/handler.go",
                "services/api/internal/auth/token.go",
                "libs/core/core.go",
                "tools/cli/main.go",
            ],
            "a `go.work` walks its modules only"
        );
        assert_eq!(
            display(&layout, "services/api/internal/auth/token.go"),
            "example.com/api/internal/auth"
        );
        assert_eq!(display(&layout, "libs/core/core.go"), "example.com/core");
    }

    #[test]
    fn detect_recognizes_a_go_module_or_go_work() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        assert!(!GoFrontend.detect(root));
        write(root, "go.mod", "module example.com/shop\n");
        assert!(GoFrontend.detect(root));

        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "go.work", "go 1.22\n");
        assert!(GoFrontend.detect(root));
    }
}
