//! Python project and package discovery: import roots, packages, modules and
//! test files, in both `src/` and flat layouts and across `uv` workspace
//! members.

use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use toml::Value;

use crate::Error;
use crate::model::NodeKind;
use crate::treesitter::lang::{DiscoveredFile, Language, WorkspaceLayout};

/// Directories never descended into, whatever `.gitignore` says.
const SKIP_DIRECTORIES: &[&str] = &[
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    "build",
    "dist",
    ".git",
    "node_modules",
];

/// Directory suffixes that hold installed package metadata.
const SKIP_DIRECTORY_SUFFIXES: &[&str] = &[".egg-info"];

/// Discover every Python package and module under `root`, filling `layout`.
pub(super) fn discover(root: &Path, layout: &mut WorkspaceLayout) -> Result<(), Error> {
    let import_roots = import_roots(root)?;
    for path in python_files(root)? {
        add_file(root, &import_roots, &path, layout);
    }
    Ok(())
}

/// The directories absolute imports are resolved against: each project root,
/// plus its `src/` directory when that holds Python sources.
fn import_roots(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut roots = Vec::new();
    for project in projects(root)? {
        let src = project.join("src");
        if has_python_contents(&src) {
            roots.push(src);
        }
        roots.push(project);
    }
    Ok(roots)
}

/// The workspace root, or the members of its `[tool.uv.workspace]` when it
/// declares a multi-project workspace.
fn projects(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let pyproject = root.join("pyproject.toml");
    if pyproject.is_file() {
        let members = uv_workspace_members(root, &pyproject)?;
        if !members.is_empty() {
            return Ok(members);
        }
    }
    Ok(vec![root.to_path_buf()])
}

/// Glob-expanded `[tool.uv.workspace].members` directories that hold a
/// `pyproject.toml`.
fn uv_workspace_members(root: &Path, pyproject: &Path) -> Result<Vec<PathBuf>, Error> {
    let text = fs::read_to_string(pyproject)
        .map_err(|e| Error::new(format!("cannot read `{}`: {e}", pyproject.display())))?;
    let manifest: Value = toml::from_str(&text)
        .map_err(|e| Error::new(format!("invalid `{}`: {e}", pyproject.display())))?;
    let patterns = manifest
        .get("tool")
        .and_then(|tool| tool.get("uv"))
        .and_then(|uv| uv.get("workspace"))
        .and_then(|workspace| workspace.get("members"))
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(Value::as_str)
                .map(|member| member.trim_end_matches('/').to_owned())
                .filter(|member| !member.is_empty() && !member.starts_with('!'))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    let (globs, depth) = compile_globs(&patterns)?;
    let mut members: Vec<PathBuf> = Vec::new();
    for candidate in candidate_directories(root, &globs, depth)? {
        let directory = root.join(candidate);
        if directory.join("pyproject.toml").is_file() {
            members.push(directory);
        }
    }
    Ok(members)
}

/// The compiled member globs, and the depth to walk when none of them can
/// reach arbitrarily far.
fn compile_globs(patterns: &[String]) -> Result<(GlobSet, Option<usize>), Error> {
    let mut builder = GlobSetBuilder::new();
    let mut depth = 0;
    let mut bounded = true;
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
    Ok((globs, bounded.then_some(depth)))
}

/// Directories under `root`, as `/`-separated relative paths, that the member
/// globs match.
fn candidate_directories(
    root: &Path,
    globs: &GlobSet,
    depth: Option<usize>,
) -> Result<Vec<String>, Error> {
    let mut walk = WalkBuilder::new(root);
    walk.standard_filters(true);
    walk.follow_links(false);
    walk.max_depth(depth);
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
        if !relative.is_empty() && globs.is_match(&relative) {
            found.push(relative);
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// Every `.py` file in the workspace, sorted by path.
fn python_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut walk = WalkBuilder::new(root);
    walk.standard_filters(true);
    walk.follow_links(false);
    walk.sort_by_file_path(|a, b| a.cmp(b));
    walk.filter_entry(|entry| {
        let path = entry.path();
        !(entry.file_type().is_some_and(|kind| kind.is_dir()) && is_skipped_directory(path))
    });
    let mut files = Vec::new();
    for entry in walk.build() {
        let entry =
            entry.map_err(|e| Error::new(format!("cannot walk `{}`: {e}", root.display())))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry.file_name().to_string_lossy().ends_with(".py") {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

/// Register one file: its package, the module nodes on the way down, and its
/// `DiscoveredFile`.
fn add_file(root: &Path, import_roots: &[PathBuf], path: &Path, layout: &mut WorkspaceLayout) {
    let relative = relative_path(root, path);
    let (inside_root, base) = match import_root_for(import_roots, path) {
        Some(base) => (relative_path(base, path), base),
        None => (relative.clone(), root),
    };
    let components: Vec<&str> = inside_root
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let Some((file_name, directories)) = components.split_last() else {
        return;
    };
    let stem = file_stem(file_name);
    let is_init = stem == "__init__";
    let is_test = is_test_path(directories, file_name);
    let (package_id, module_id) = if directories.is_empty() {
        // A standalone module at an import root is a package of its own: `main.py`.
        let name = if is_init {
            base.file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("root")
                .to_owned()
        } else {
            stem.to_owned()
        };
        let package = layout.tree.child_with_lang(
            layout.tree.root(),
            NodeKind::Package,
            &name,
            Some(Language::Python),
        );
        layout.package_names.entry(name).or_insert(package);
        (package, package)
    } else {
        let package = layout.tree.child_with_lang(
            layout.tree.root(),
            NodeKind::Package,
            directories[0],
            Some(Language::Python),
        );
        layout
            .package_names
            .entry(directories[0].to_owned())
            .or_insert(package);
        if is_test_directory(directories[0]) {
            layout.tree.node_mut(package).test = true;
        }
        let mut parent = package;
        for directory in &directories[1..] {
            parent = layout.tree.child(parent, NodeKind::Module, directory);
            if is_test_directory(directory) {
                layout.tree.node_mut(parent).test = true;
            }
        }
        // `__init__.py` is its package or subpackage, like Rust's `mod.rs`.
        let module = if is_init {
            parent
        } else {
            layout.tree.child(parent, NodeKind::Module, stem)
        };
        (package, module)
    };
    if is_test {
        layout.tree.node_mut(module_id).test = true;
    }
    layout.files.push(DiscoveredFile {
        abs_path: path.to_path_buf(),
        rel_path: relative.clone(),
        module_id,
        package_id,
        language: Language::Python,
        is_test,
    });
    layout.file_to_module.insert(relative, module_id);
}

/// The deepest import root containing `path`.
fn import_root_for<'a>(roots: &'a [PathBuf], path: &Path) -> Option<&'a Path> {
    roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .map(PathBuf::as_path)
}

fn file_stem(file_name: &str) -> &str {
    file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem)
}

/// True when `dir` directly holds Python: a `.py` file or a package directory.
fn has_python_contents(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            path.join("__init__.py").is_file() || holds_python_file(&path)
        } else {
            path.extension().is_some_and(|extension| extension == "py")
        }
    })
}

fn holds_python_file(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "py")
    })
}

fn is_test_directory(name: &str) -> bool {
    matches!(name, "test" | "tests" | "__tests__")
}

fn is_test_path(directories: &[&str], file_name: &str) -> bool {
    file_name == "conftest.py"
        || (file_name.starts_with("test_") && file_name.ends_with(".py"))
        || file_name.ends_with("_test.py")
        || directories.iter().any(|name| is_test_directory(name))
}

fn is_skipped_directory(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    SKIP_DIRECTORIES.contains(&name)
        || SKIP_DIRECTORY_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
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

/// The module node a workspace-relative path resolved to, if any.
#[cfg(test)]
fn module_of(layout: &WorkspaceLayout, path: &str) -> crate::model::NodeId {
    *layout
        .file_to_module
        .get(path)
        .unwrap_or_else(|| panic!("no module for `{path}`"))
}
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::super::PythonFrontend;
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
        PythonFrontend
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
        layout.tree.display(module_of(layout, path))
    }

    fn is_test(layout: &WorkspaceLayout, path: &str) -> bool {
        layout
            .files
            .iter()
            .find(|file| file.rel_path == path)
            .unwrap_or_else(|| panic!("no discovered file {path}"))
            .is_test
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
    fn a_src_layout_resolves_packages_from_the_import_root() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(
            root,
            "pyproject.toml",
            "[project]\nname = \"app\"\n[tool.setuptools.packages.find]\nwhere = [\"src\"]\n",
        );
        write(root, "src/app/__init__.py", "from .queue import Queue\n");
        write(
            root,
            "src/app/queue/__init__.py",
            "from .ring import RunQueue\n",
        );
        write(root, "src/app/queue/ring.py", "class RunQueue: pass\n");
        write(root, "src/app/main.py", "def run(): pass\n");

        let layout = discover(root);
        assert_eq!(
            paths(&layout),
            [
                "src/app/__init__.py",
                "src/app/main.py",
                "src/app/queue/__init__.py",
                "src/app/queue/ring.py",
            ]
        );
        assert_eq!(
            display(&layout, "src/app/__init__.py"),
            "app",
            "a package owns its __init__.py"
        );
        assert_eq!(display(&layout, "src/app/queue/__init__.py"), "app.queue");
        assert_eq!(display(&layout, "src/app/queue/ring.py"), "app.queue.ring");
        assert_eq!(
            display(&layout, "src/app/main.py"),
            "app.main",
            "src is an import root, not a module"
        );
        assert_eq!(package_names(&layout), ["app"]);
        assert!(layout.package_names.contains_key("app"));

        let package = layout.package_names["app"];
        assert_eq!(layout.tree.node(package).kind, NodeKind::Package);
        assert_eq!(layout.tree.node(package).language, Some(Language::Python));
        assert_eq!(layout.tree.parent(package), Some(layout.tree.root()));
    }

    #[test]
    fn a_flat_layout_puts_packages_and_standalone_modules_beside_the_project() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "pyproject.toml", "[project]\nname = \"app\"\n");
        write(root, "app/__init__.py", "");
        write(root, "app/queue.py", "class Queue: pass\n");
        write(root, "app/nested/deep.py", "CONST = 1\n");
        write(root, "main.py", "from app import Queue\n");

        let layout = discover(root);
        assert_eq!(display(&layout, "app/queue.py"), "app.queue");
        assert_eq!(display(&layout, "app/nested/deep.py"), "app.nested.deep");
        assert_eq!(
            display(&layout, "main.py"),
            "main",
            "a standalone module is its own package"
        );
        let mut names = package_names(&layout);
        names.sort();
        assert_eq!(names, ["app", "main"]);
    }

    #[test]
    fn a_uv_workspace_member_is_its_own_import_root() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(
            root,
            "pyproject.toml",
            "[tool.uv.workspace]\nmembers = [\"packages/*\"]\n",
        );
        write(
            root,
            "packages/core/pyproject.toml",
            "[project]\nname = \"core\"\n",
        );
        write(root, "packages/core/src/core/__init__.py", "");
        write(root, "packages/core/src/core/ring.py", "class Ring: pass\n");
        write(
            root,
            "packages/api/pyproject.toml",
            "[project]\nname = \"api\"\n",
        );
        write(root, "packages/api/src/api/__init__.py", "");
        write(
            root,
            "packages/api/src/api/app.py",
            "from core import Ring\n",
        );
        write(root, "packages/notes.md", "ignored");

        let layout = discover(root);
        assert_eq!(
            paths(&layout),
            [
                "packages/api/src/api/__init__.py",
                "packages/api/src/api/app.py",
                "packages/core/src/core/__init__.py",
                "packages/core/src/core/ring.py",
            ]
        );
        assert_eq!(
            display(&layout, "packages/core/src/core/ring.py"),
            "core.ring"
        );
        assert_eq!(display(&layout, "packages/api/src/api/app.py"), "api.app");
        let mut names = package_names(&layout);
        names.sort();
        assert_eq!(names, ["api", "core"]);
    }

    #[test]
    fn test_files_and_directories_are_marked() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "pyproject.toml", "[project]\nname = \"app\"\n");
        write(root, "app/__init__.py", "");
        write(root, "app/service.py", "");
        write(root, "app/test_service.py", "");
        write(root, "app/service_test.py", "");
        write(root, "app/conftest.py", "");
        write(root, "tests/test_flow.py", "");
        write(root, "tests/helpers.py", "");

        let layout = discover(root);
        assert!(is_test(&layout, "app/test_service.py"));
        assert!(is_test(&layout, "app/service_test.py"));
        assert!(is_test(&layout, "app/conftest.py"));
        assert!(is_test(&layout, "tests/test_flow.py"));
        assert!(
            is_test(&layout, "tests/helpers.py"),
            "everything under tests/ is test code"
        );
        assert!(!is_test(&layout, "app/service.py"));
        assert!(!is_test(&layout, "app/__init__.py"));

        let tests_package = layout.package_names["tests"];
        assert!(layout.tree.is_test(tests_package));
    }

    #[test]
    fn detect_recognizes_python_projects() {
        let frontend = PythonFrontend;
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        assert!(!frontend.detect(root));
        write(root, "pyproject.toml", "");
        assert!(frontend.detect(root));

        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "setup.py", "");
        assert!(frontend.detect(root));

        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "src/app/__init__.py", "");
        assert!(
            frontend.detect(root),
            "a src layout with a package is a Python project"
        );

        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "tool.py", "");
        assert!(frontend.detect(root));
    }
}
