//! `oxc_resolver` integration: module specifiers to workspace modules.

use std::path::{Component, Path};

use oxc_resolver::Resolver;

use crate::treesitter::lang::{DiscoveredFile, ResolvedImport, WorkspaceLayout};
use crate::treesitter::summary::{ImportKind, RawImport};

/// Resolve `import` to the workspace module its specifier points at.
pub(super) fn resolve_import(
    resolver: &Resolver,
    layout: &WorkspaceLayout,
    from_file: &DiscoveredFile,
    import: &RawImport,
) -> Option<ResolvedImport> {
    let specifier = import.raw_specifier.as_deref()?;
    if specifier.is_empty() {
        return None;
    }
    // `resolve_file` is the entry point that applies `TsconfigDiscovery::Auto`;
    // it resolves relative to the importing file's directory.
    let resolution = resolver.resolve_file(&from_file.abs_path, specifier).ok()?;
    let relative = workspace_relative(layout, resolution.path())?;
    let &module = layout.file_to_module.get(&relative)?;
    let (item_name, is_default, is_glob) = match &import.kind {
        ImportKind::Named { name, .. } => (Some(name.clone()), false, false),
        ImportKind::Default { .. } => (None, true, false),
        ImportKind::Namespace { .. } => (None, false, false),
        ImportKind::Glob => (None, false, true),
        ImportKind::SideEffect => (None, false, false),
    };
    Some(ResolvedImport::Module {
        module,
        item_name,
        is_default,
        is_glob,
    })
}

/// The workspace-relative, `/`-separated form of `path`, if it is inside the workspace.
fn workspace_relative(layout: &WorkspaceLayout, path: &Path) -> Option<String> {
    if let Ok(relative) = path.strip_prefix(&layout.root) {
        return relative_string(relative);
    }
    // The resolver follows symlinks, so its answer may be canonical while the
    // configured root (for example a temporary directory) is not.
    let root = std::fs::canonicalize(&layout.root).ok()?;
    let relative = path.strip_prefix(root).ok()?;
    relative_string(relative)
}

fn relative_string(relative: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::super::TypeScriptFrontend;
    use super::*;
    use crate::configuration::Configuration;
    use crate::treesitter::lang::LanguageFrontend;
    use crate::treesitter::summary::{ImportKind, ModuleScope, RawImport};

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
    }

    struct Fixture {
        _temp: TempDir,
        frontend: TypeScriptFrontend,
        layout: WorkspaceLayout,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Fixture {
            let temp = TempDir::new().expect("temp dir");
            let root = temp.path();
            write(root, "package.json", r#"{"name":"app"}"#);
            for (path, contents) in files {
                write(root, path, contents);
            }
            let frontend = TypeScriptFrontend::new();
            let mut layout = WorkspaceLayout::new(root.to_path_buf());
            frontend
                .discover(root, &Configuration::default(), &mut layout)
                .expect("discovery succeeds");
            Fixture {
                _temp: temp,
                frontend,
                layout,
            }
        }

        fn resolve(&self, from: &str, specifier: &str, kind: ImportKind) -> Option<ResolvedImport> {
            let file = self
                .layout
                .files
                .iter()
                .find(|file| file.rel_path == from)
                .unwrap_or_else(|| panic!("no discovered file `{from}`"));
            let import = RawImport {
                scope: ModuleScope::FileRoot,
                path_segments: specifier.split('/').map(str::to_owned).collect(),
                raw_specifier: Some(specifier.to_owned()),
                kind,
                is_reexport: false,
            };
            self.frontend
                .resolve_import(&self.layout, file, file.module_id, &import)
        }

        fn display(&self, resolved: &ResolvedImport) -> String {
            match resolved {
                ResolvedImport::Module { module, .. } => self.layout.tree.display(*module),
            }
        }
    }

    fn named(name: &str) -> ImportKind {
        ImportKind::Named {
            name: name.to_owned(),
            alias: None,
        }
    }

    #[test]
    fn relative_specifiers_resolve_to_their_files() {
        let fixture = Fixture::new(&[
            ("src/app.ts", "import { Ring } from './queue/ring';"),
            ("src/queue/ring.ts", "export class Ring {}"),
            ("src/queue/index.ts", "export class Queue {}"),
        ]);
        let resolved = fixture.resolve("src/app.ts", "./queue/ring", named("Ring"));
        let resolved = resolved.expect("ring resolves");
        assert_eq!(fixture.display(&resolved), "app/src/queue/ring");
        assert!(matches!(
            resolved,
            ResolvedImport::Module { item_name: Some(name), is_default: false, is_glob: false, .. }
                if name == "Ring"
        ));
    }

    #[test]
    fn a_directory_specifier_resolves_through_its_index_barrel() {
        let fixture = Fixture::new(&[
            ("src/app.ts", "import { Queue } from './queue';"),
            ("src/queue/index.ts", "export * from './ring';"),
            ("src/queue/ring.ts", "export class Ring {}"),
        ]);
        let resolved = fixture
            .resolve("src/app.ts", "./queue", named("Queue"))
            .expect("the barrel resolves");
        assert_eq!(fixture.display(&resolved), "app/src/queue");
    }

    #[test]
    fn tsconfig_paths_aliases_resolve_through_oxc() {
        let fixture = Fixture::new(&[
            (
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
            ),
            ("src/app.ts", "import { util } from '@/utils';"),
            ("src/utils.ts", "export const util = 1;"),
            ("src/queue/ring.ts", "export class Ring {}"),
        ]);
        let resolved = fixture
            .resolve("src/app.ts", "@/utils", named("util"))
            .expect("the alias resolves");
        assert_eq!(fixture.display(&resolved), "app/src/utils");
        let nested = fixture
            .resolve("src/app.ts", "@/queue/ring", named("Ring"))
            .expect("nested aliases resolve");
        assert_eq!(fixture.display(&nested), "app/src/queue/ring");
    }

    #[test]
    fn extension_aliases_let_javascript_specifiers_reach_typescript_files() {
        let fixture = Fixture::new(&[
            ("src/app.mts", "import { ring } from './ring.js';"),
            ("src/ring.ts", "export const ring = 1;"),
        ]);
        let resolved = fixture
            .resolve("src/app.mts", "./ring.js", named("ring"))
            .expect("'.js' resolves to '.ts'");
        assert_eq!(fixture.display(&resolved), "app/src/ring");
    }

    #[test]
    fn every_import_kind_maps_onto_the_resolved_shape() {
        let fixture = Fixture::new(&[
            ("src/app.ts", "import './setup';"),
            ("src/setup.ts", "export const ready = 1;"),
        ]);
        let side_effect = fixture
            .resolve("src/app.ts", "./setup", ImportKind::SideEffect)
            .expect("side effects resolve");
        assert!(matches!(
            side_effect,
            ResolvedImport::Module {
                item_name: None,
                is_default: false,
                is_glob: false,
                ..
            }
        ));
        let default = fixture
            .resolve(
                "src/app.ts",
                "./setup",
                ImportKind::Default {
                    alias: "setup".to_owned(),
                },
            )
            .expect("default imports resolve");
        assert!(matches!(
            default,
            ResolvedImport::Module {
                item_name: None,
                is_default: true,
                is_glob: false,
                ..
            }
        ));
        let namespace = fixture
            .resolve(
                "src/app.ts",
                "./setup",
                ImportKind::Namespace {
                    alias: "setup".to_owned(),
                },
            )
            .expect("namespace imports resolve");
        assert!(matches!(
            namespace,
            ResolvedImport::Module {
                item_name: None,
                is_default: false,
                is_glob: false,
                ..
            }
        ));
        let glob = fixture
            .resolve("src/app.ts", "./setup", ImportKind::Glob)
            .expect("glob reexports resolve");
        assert!(matches!(
            glob,
            ResolvedImport::Module {
                item_name: None,
                is_default: false,
                is_glob: true,
                ..
            }
        ));
    }

    #[test]
    fn external_builtin_and_missing_specifiers_do_not_resolve() {
        let fixture = Fixture::new(&[
            ("src/app.ts", "import 'react';"),
            ("node_modules/dep/index.js", "module.exports = 1;"),
            (
                "node_modules/dep/package.json",
                r#"{"name":"dep","main":"index.js"}"#,
            ),
        ]);
        assert!(
            fixture
                .resolve("src/app.ts", "react", ImportKind::SideEffect)
                .is_none()
        );
        assert!(
            fixture
                .resolve("src/app.ts", "dep", ImportKind::SideEffect)
                .is_none()
        );
        assert!(
            fixture
                .resolve("src/app.ts", "node:fs", ImportKind::SideEffect)
                .is_none()
        );
        assert!(
            fixture
                .resolve("src/app.ts", "./missing", ImportKind::SideEffect)
                .is_none()
        );
    }

    #[test]
    fn imports_without_a_specifier_are_not_resolved() {
        let fixture = Fixture::new(&[("src/app.ts", "export const a = 1;")]);
        let file = &fixture.layout.files[0];
        let import = RawImport {
            scope: ModuleScope::FileRoot,
            path_segments: Vec::new(),
            raw_specifier: None,
            kind: ImportKind::Glob,
            is_reexport: false,
        };
        assert!(
            fixture
                .frontend
                .resolve_import(&fixture.layout, file, file.module_id, &import)
                .is_none()
        );
    }
}
