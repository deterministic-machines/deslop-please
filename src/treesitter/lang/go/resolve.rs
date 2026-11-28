//! Go import resolution: an import path names a workspace package, or a
//! directory inside one.

use crate::model::{Language, NodeId, NodeKind, Tree};
use crate::treesitter::lang::{ResolvedImport, WorkspaceLayout};
use crate::treesitter::summary::{ImportKind, RawImport};

/// Resolve `import` to the package or package directory it names.
pub(super) fn resolve_import(
    layout: &WorkspaceLayout,
    import: &RawImport,
) -> Option<ResolvedImport> {
    if matches!(import.kind, ImportKind::SideEffect) {
        return None;
    }
    let specifier = import.raw_specifier.as_deref()?;
    let (package, rest) = package_prefix(layout, specifier)?;
    let module = descend(&layout.tree, package, rest)?;
    Some(ResolvedImport::Module {
        module,
        item_name: None,
        is_default: false,
        is_glob: matches!(import.kind, ImportKind::Glob),
    })
}

/// The longest Go package whose import path prefixes `specifier`, and the
/// directory segments below it. Standard-library and third-party imports
/// prefix no workspace package and so resolve to nothing.
fn package_prefix<'a>(
    layout: &'a WorkspaceLayout,
    specifier: &'a str,
) -> Option<(NodeId, &'a str)> {
    let tree = &layout.tree;
    let mut best: Option<(usize, NodeId)> = None;
    for (name, &node) in &layout.package_names {
        if tree.node(node).kind != NodeKind::Package
            || tree.node(node).language != Some(Language::Go)
        {
            continue;
        }
        let Some(rest) = specifier.strip_prefix(name.as_str()) else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with('/') {
            continue;
        }
        if best.is_none_or(|(longest, _)| name.len() > longest) {
            best = Some((name.len(), node));
        }
    }
    let (longest, package) = best?;
    Some((package, &specifier[longest..]))
}

/// Walk the module tree from `package` down `rest`'s `/`-separated directories.
fn descend(tree: &Tree, package: NodeId, rest: &str) -> Option<NodeId> {
    let mut current = package;
    for segment in rest.split('/').filter(|segment| !segment.is_empty()) {
        current = tree.find_child_by_name(current, segment)?;
    }
    Some(current)
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
    use crate::treesitter::summary::ModuleScope;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
    }

    struct Fixture {
        _temp: TempDir,
        layout: WorkspaceLayout,
    }

    impl Fixture {
        /// A workspace rooted at one `example.com/shop` module, plus `files`.
        fn new(files: &[(&str, &str)]) -> Fixture {
            let temp = TempDir::new().expect("temp dir");
            let root = temp.path();
            write(root, "go.mod", "module example.com/shop\n");
            for (path, contents) in files {
                write(root, path, contents);
            }
            let mut layout = WorkspaceLayout::new(root.to_path_buf());
            GoFrontend
                .discover(root, &Configuration::default(), &mut layout)
                .expect("discovery succeeds");
            Fixture {
                _temp: temp,
                layout,
            }
        }

        fn resolve(&self, specifier: &str, kind: ImportKind) -> Option<ResolvedImport> {
            let import = RawImport {
                scope: ModuleScope::FileRoot,
                path_segments: specifier
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_owned)
                    .collect(),
                raw_specifier: Some(specifier.to_owned()),
                kind,
                is_reexport: false,
            };
            resolve_import(&self.layout, &import)
        }

        fn display(&self, resolved: &ResolvedImport) -> String {
            match resolved {
                ResolvedImport::Module { module, .. } => self.layout.tree.display(*module),
            }
        }
    }

    fn namespace(alias: &str) -> ImportKind {
        ImportKind::Namespace {
            alias: alias.to_owned(),
        }
    }

    #[test]
    fn an_import_names_its_package_or_a_directory_inside_it() {
        let fixture = Fixture::new(&[
            (
                "queue/queue.go",
                "package queue\n\nimport \"example.com/shop/queue/internal/ring\"\n",
            ),
            ("queue/internal/ring/ring.go", "package ring\n"),
        ]);
        let resolved = fixture
            .resolve("example.com/shop/queue", namespace("queue"))
            .expect("the package resolves");
        assert_eq!(fixture.display(&resolved), "example.com/shop/queue");
        assert!(matches!(
            resolved,
            ResolvedImport::Module {
                item_name: None,
                is_default: false,
                is_glob: false,
                ..
            }
        ));
        let resolved = fixture
            .resolve("example.com/shop/queue/internal", namespace("internal"))
            .expect("a directory in the package resolves");
        assert_eq!(
            fixture.display(&resolved),
            "example.com/shop/queue/internal"
        );
        let resolved = fixture
            .resolve("example.com/shop/queue/internal/ring", namespace("ring"))
            .expect("a nested directory resolves");
        assert_eq!(
            fixture.display(&resolved),
            "example.com/shop/queue/internal/ring"
        );
        assert!(
            fixture
                .resolve("example.com/shop/queue/missing", namespace("missing"))
                .is_none(),
            "a directory that does not exist resolves to nothing"
        );
        assert!(
            fixture.resolve("fmt", namespace("fmt")).is_none(),
            "the standard library is no workspace package"
        );
        assert!(
            fixture
                .resolve("example.com/other", namespace("other"))
                .is_none()
        );
    }

    #[test]
    fn the_longest_package_prefix_wins() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "go.work", "go 1.22\n\nuse (\n\t.\n\t./tools\n)\n");
        write(root, "go.mod", "module example.com/shop\n");
        write(root, "queue/queue.go", "package queue\n");
        write(root, "tools/go.mod", "module example.com/shop/tools\n");
        write(root, "tools/tool.go", "package tools\n");
        let mut layout = WorkspaceLayout::new(root.to_path_buf());
        GoFrontend
            .discover(root, &Configuration::default(), &mut layout)
            .expect("discovery succeeds");
        let fixture = Fixture {
            _temp: temp,
            layout,
        };
        let resolved = fixture
            .resolve("example.com/shop/tools", namespace("tools"))
            .expect("the inner module resolves");
        assert_eq!(fixture.display(&resolved), "example.com/shop/tools");
        let resolved = fixture
            .resolve("example.com/shop/queue", namespace("queue"))
            .expect("the outer module still resolves");
        assert_eq!(fixture.display(&resolved), "example.com/shop/queue");
    }

    #[test]
    fn dot_imports_glob_and_blank_imports_bind_nothing() {
        let fixture = Fixture::new(&[
            ("queue/queue.go", "package queue\n"),
            ("app/app.go", "package app\n"),
        ]);
        let glob = fixture
            .resolve("example.com/shop/queue", ImportKind::Glob)
            .expect("a dot import resolves");
        assert!(matches!(
            glob,
            ResolvedImport::Module {
                item_name: None,
                is_default: false,
                is_glob: true,
                ..
            }
        ));
        assert!(
            fixture
                .resolve("example.com/shop/queue", ImportKind::SideEffect)
                .is_none(),
            "a blank import names no code"
        );
    }
}
