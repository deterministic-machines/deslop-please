//! Python import resolution: absolute dotted paths, relative imports and
//! package barrels (__init__.py re-exports).

use crate::model::NodeId;
use crate::treesitter::lang::{DiscoveredFile, ResolvedImport, WorkspaceLayout};
use crate::treesitter::summary::{ImportKind, RawImport};

/// Resolve an import written in from_file to the workspace module it names.
pub(super) fn resolve_import(
    layout: &WorkspaceLayout,
    from_file: &DiscoveredFile,
    from_module: NodeId,
    import: &RawImport,
) -> Option<ResolvedImport> {
    let specifier = import.raw_specifier.as_deref()?;
    let segments: Vec<&str> = import.path_segments.iter().map(String::as_str).collect();
    // import a.b.c binds a alone; import a.b.c as x binds the whole module.
    if let ImportKind::Namespace { alias } = &import.kind
        && segments.first() == Some(&alias.as_str())
    {
        let package = *layout.package_names.get(alias.as_str())?;
        return Some(module(package, None, false));
    }
    let (base, below) = if specifier.starts_with('.') {
        let level = specifier.chars().take_while(|&c| c == '.').count();
        (
            relative_base(layout, from_file, from_module, level)?,
            segments.as_slice(),
        )
    } else {
        // The first segment names the top-level package; the rest descend.
        (
            *layout.package_names.get(*segments.first()?)?,
            &segments[1..],
        )
    };
    let target = walk(layout, base, below)?;
    let (item_name, is_default, is_glob) = match &import.kind {
        ImportKind::Named { name, .. } => {
            // from pkg import sub may name a submodule rather than a symbol.
            if let Some(child) = layout.tree.find_child_by_name(target, name) {
                return Some(module(child, None, false));
            }
            (Some(name.clone()), false, false)
        }
        ImportKind::Default { .. } => (None, true, false),
        ImportKind::Namespace { .. } => (None, false, false),
        ImportKind::Glob => (None, false, true),
        ImportKind::SideEffect => (None, false, false),
    };
    Some(ResolvedImport::Module {
        module: target,
        item_name,
        is_default,
        is_glob,
    })
}

fn module(module: NodeId, item_name: Option<String>, is_glob: bool) -> ResolvedImport {
    ResolvedImport::Module {
        module,
        item_name,
        is_default: false,
        is_glob,
    }
}

/// The package a relative import counts dots from: level 1 is the package the
/// file is written in (the directory itself for an __init__.py, its parent for
/// a regular module), each further dot one package up.
fn relative_base(
    layout: &WorkspaceLayout,
    from_file: &DiscoveredFile,
    from_module: NodeId,
    level: usize,
) -> Option<NodeId> {
    let is_init = from_file
        .rel_path
        .rsplit('/')
        .next()
        .is_some_and(|name| name == "__init__.py");
    let mut base = if is_init {
        from_module
    } else {
        layout.tree.parent(from_module)?
    };
    for _ in 1..level {
        base = layout.tree.parent(base)?;
    }
    Some(base)
}

/// Follow the segments down the module tree from base.
fn walk(layout: &WorkspaceLayout, base: NodeId, segments: &[&str]) -> Option<NodeId> {
    let mut node = base;
    for segment in segments {
        node = layout.tree.find_child_by_name(node, segment)?;
    }
    Some(node)
}
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::super::PythonFrontend;
    use crate::configuration::Configuration;
    use crate::treesitter::lang::LanguageFrontend;
    use crate::treesitter::lang::{ResolvedImport, WorkspaceLayout};
    use crate::treesitter::summary::{FileSummary, ImportKind};

    struct Fixture {
        _temp: TempDir,
        frontend: PythonFrontend,
        layout: WorkspaceLayout,
        summaries: Vec<FileSummary>,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Fixture {
            let temp = TempDir::new().expect("temp dir");
            let root = temp.path();
            write(root, "pyproject.toml", "[project]");
            for (path, contents) in files {
                write(root, path, contents);
            }
            let frontend = PythonFrontend;
            let mut layout = WorkspaceLayout::new(root.to_path_buf());
            frontend
                .discover(root, &Configuration::default(), &mut layout)
                .expect("discovery succeeds");
            let summaries = layout
                .files
                .iter()
                .map(|file| {
                    let source = fs::read_to_string(&file.abs_path).expect("read source");
                    frontend.extract_file(file, &source, &Configuration::default())
                })
                .collect();
            Fixture {
                _temp: temp,
                frontend,
                layout,
                summaries,
            }
        }

        fn resolve(&self, from: &str, specifier: &str) -> Option<ResolvedImport> {
            self.resolve_named(from, specifier, None)
        }

        fn resolve_named(
            &self,
            from: &str,
            specifier: &str,
            name: Option<&str>,
        ) -> Option<ResolvedImport> {
            let index = self
                .layout
                .files
                .iter()
                .position(|file| file.rel_path == from)
                .unwrap_or_else(|| panic!("no discovered file {from}"));
            let file = &self.layout.files[index];
            let import = self.summaries[index]
                .imports
                .iter()
                .find(|import| {
                    import.raw_specifier.as_deref() == Some(specifier)
                        && match name {
                            None => true,
                            Some(name) => matches!(
                                &import.kind,
                                ImportKind::Named { name: found, .. } if found == name
                            ),
                        }
                })
                .unwrap_or_else(|| panic!("no import of {specifier} in {from}"));
            self.frontend
                .resolve_import(&self.layout, file, file.module_id, import)
        }

        fn display(&self, resolved: &ResolvedImport) -> String {
            match resolved {
                ResolvedImport::Module { module, .. } => self.layout.tree.display(*module),
            }
        }
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
    }

    #[test]
    fn absolute_imports_reach_the_module_and_the_name_it_names() {
        let fixture = Fixture::new(&[
            ("src/app/__init__.py", ""),
            ("src/app/queue/__init__.py", ""),
            ("src/app/queue/ring.py", "class RunQueue: pass\n"),
            (
                "src/app/main.py",
                "from app.queue.ring import RunQueue\nfrom app.queue import ring\n",
            ),
        ]);
        let resolved = fixture
            .resolve_named("src/app/main.py", "app.queue.ring", Some("RunQueue"))
            .expect("the module resolves");
        assert_eq!(fixture.display(&resolved), "app.queue.ring");
        assert!(matches!(
            resolved,
            ResolvedImport::Module { item_name: Some(name), is_default: false, is_glob: false, .. }
                if name == "RunQueue"
        ));

        let submodule = fixture
            .resolve_named("src/app/main.py", "app.queue", Some("ring"))
            .expect("a submodule import resolves");
        assert_eq!(
            fixture.display(&submodule),
            "app.queue.ring",
            "from pkg import sub names the submodule itself"
        );
        assert!(matches!(
            submodule,
            ResolvedImport::Module {
                item_name: None,
                ..
            }
        ));
    }

    #[test]
    fn a_bare_import_binds_the_top_level_package() {
        let fixture = Fixture::new(&[
            ("src/app/__init__.py", ""),
            ("src/app/queue/__init__.py", ""),
            ("src/app/queue/ring.py", ""),
            ("src/app/main.py", "import app.queue.ring\n"),
        ]);
        let resolved = fixture
            .resolve("src/app/main.py", "app.queue.ring")
            .expect("the package binds");
        assert_eq!(
            fixture.display(&resolved),
            "app",
            "import a.b.c binds a alone"
        );
    }

    #[test]
    fn an_aliased_import_binds_the_whole_module() {
        let fixture = Fixture::new(&[
            ("src/app/__init__.py", ""),
            ("src/app/queue/__init__.py", ""),
            ("src/app/queue/ring.py", ""),
            ("src/app/main.py", "import app.queue.ring as rq\n"),
        ]);
        let resolved = fixture
            .resolve("src/app/main.py", "app.queue.ring")
            .expect("the alias resolves");
        assert_eq!(fixture.display(&resolved), "app.queue.ring");
    }

    #[test]
    fn relative_imports_count_from_the_package_a_file_is_written_in() {
        let fixture = Fixture::new(&[
            (
                "src/app/__init__.py",
                "from .queue import Queue\nfrom . import queue\n",
            ),
            ("src/app/queue/__init__.py", ""),
        ]);
        let queue = fixture
            .resolve_named("src/app/__init__.py", ".", Some("queue"))
            .expect("from . import queue names the child module");
        assert_eq!(fixture.display(&queue), "app.queue");
        assert!(matches!(
            queue,
            ResolvedImport::Module {
                item_name: None,
                ..
            }
        ));

        let name = fixture
            .resolve_named("src/app/__init__.py", ".queue", Some("Queue"))
            .expect("from .queue import Queue");
        assert_eq!(fixture.display(&name), "app.queue");
        assert!(matches!(
            name,
            ResolvedImport::Module { item_name: Some(ref name), .. } if name == "Queue"
        ));
    }

    #[test]
    fn a_regular_module_counts_from_its_parent_package() {
        let fixture = Fixture::new(&[
            ("src/app/__init__.py", ""),
            ("src/app/sub/__init__.py", ""),
            ("src/app/sub/helpers.py", "def helper(): pass\n"),
            (
                "src/app/deep/user.py",
                "from ..sub.helpers import helper\nfrom .. import sub\n",
            ),
        ]);
        let helper = fixture
            .resolve_named("src/app/deep/user.py", "..sub.helpers", Some("helper"))
            .expect("two dots climb one package");
        assert_eq!(fixture.display(&helper), "app.sub.helpers");

        let parent = fixture
            .resolve_named("src/app/deep/user.py", "..", Some("sub"))
            .expect("from .. import sub");
        assert_eq!(fixture.display(&parent), "app.sub");
        assert!(matches!(
            parent,
            ResolvedImport::Module {
                item_name: None,
                ..
            }
        ));
    }

    #[test]
    fn wildcard_imports_resolve_to_their_module() {
        let fixture = Fixture::new(&[
            ("src/app/__init__.py", ""),
            ("src/app/queue/__init__.py", ""),
            ("src/app/main.py", "from app.queue import *\n"),
        ]);
        let resolved = fixture
            .resolve("src/app/main.py", "app.queue")
            .expect("the wildcard resolves");
        assert!(matches!(
            resolved,
            ResolvedImport::Module {
                is_glob: true,
                item_name: None,
                ..
            }
        ));
        assert_eq!(fixture.display(&resolved), "app.queue");
    }

    #[test]
    fn external_stdlib_and_missing_modules_do_not_resolve() {
        let fixture = Fixture::new(&[
            ("src/app/__init__.py", ""),
            (
                "src/app/main.py",
                "import os\nimport json\nfrom typing import Optional\n",
            ),
            (
                "src/app/other.py",
                "from app.missing import nope\nfrom nowhere import thing\n",
            ),
        ]);
        assert!(fixture.resolve("src/app/main.py", "os").is_none());
        assert!(fixture.resolve("src/app/main.py", "json").is_none());
        assert!(fixture.resolve("src/app/main.py", "typing").is_none());
        assert!(fixture.resolve("src/app/other.py", "app.missing").is_none());
        assert!(fixture.resolve("src/app/other.py", "nowhere").is_none());
    }

    #[test]
    fn a_package_barrel_resolves_through_its_reexports() {
        let fixture = Fixture::new(&[
            ("src/app/__init__.py", "from .queue import RunQueue\n"),
            ("src/app/queue/__init__.py", "from .ring import RunQueue\n"),
            ("src/app/queue/ring.py", "class RunQueue: pass\n"),
            ("src/app/main.py", "from app import RunQueue\n"),
        ]);
        let resolved = fixture
            .resolve_named("src/app/main.py", "app", Some("RunQueue"))
            .expect("the package resolves");
        assert_eq!(
            fixture.display(&resolved),
            "app",
            "the name lives in the package __init__.py"
        );
        assert!(matches!(
            resolved,
            ResolvedImport::Module { item_name: Some(ref name), .. } if name == "RunQueue"
        ));
    }
}
