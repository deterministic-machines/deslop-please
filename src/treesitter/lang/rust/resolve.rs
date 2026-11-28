//! Resolving Rust `use` paths against the discovered module tree.

use crate::model::NodeId;
use crate::model::NodeKind;
use crate::treesitter::lang::{DiscoveredFile, ResolvedImport, WorkspaceLayout};
use crate::treesitter::summary::{ImportKind, RawImport};

/// Resolve `import` written in `from_module` inside `from_file`.
pub(crate) fn resolve_import(
    layout: &WorkspaceLayout,
    from_file: &DiscoveredFile,
    from_module: NodeId,
    import: &RawImport,
) -> Option<ResolvedImport> {
    let segments = &import.path_segments;
    if segments.is_empty() {
        return None;
    }
    match &import.kind {
        ImportKind::Glob => {
            let module = resolve_path(layout, from_file, from_module, segments)?;
            Some(ResolvedImport::Module {
                module,
                item_name: None,
                is_default: false,
                is_glob: true,
            })
        }
        ImportKind::Named { name, .. } => {
            // A `use a::b as c` may name a module; a `use a::b::C` names an item.
            if let Some(module) = resolve_path(layout, from_file, from_module, segments) {
                return Some(ResolvedImport::Module {
                    module,
                    item_name: None,
                    is_default: false,
                    is_glob: false,
                });
            }
            let (module_segments, item) = segments.split_at(segments.len().checked_sub(1)?);
            let module = resolve_path(layout, from_file, from_module, module_segments)?;
            Some(ResolvedImport::Module {
                module,
                item_name: Some(item.first().cloned().unwrap_or_else(|| name.clone())),
                is_default: false,
                is_glob: false,
            })
        }
        // Only a TypeScript frontend emits these; no Rust `use` declares one.
        ImportKind::Default { .. } | ImportKind::SideEffect => None,
        ImportKind::Namespace { .. } => {
            if let Some(module) = resolve_path(layout, from_file, from_module, segments) {
                return Some(ResolvedImport::Module {
                    module,
                    item_name: None,
                    is_default: false,
                    is_glob: false,
                });
            }
            let (module_segments, item) = segments.split_at(segments.len().checked_sub(1)?);
            let module = resolve_path(layout, from_file, from_module, module_segments)?;
            Some(ResolvedImport::Module {
                module,
                item_name: Some(item.first()?.clone()),
                is_default: false,
                is_glob: false,
            })
        }
    }
}

/// Resolve `crate::a::b`, `super::a`, `self::a`, a sibling package, or a
/// relative child module to a node of the workspace tree.
fn resolve_path(
    layout: &WorkspaceLayout,
    from_file: &DiscoveredFile,
    from_module: NodeId,
    segments: &[String],
) -> Option<NodeId> {
    let mut segments = segments.iter();
    let first = segments.next()?;
    let mut current = match first.as_str() {
        "crate" => from_file.package_id,
        "self" => from_module,
        "super" => layout
            .tree
            .parent(from_module)
            .filter(|&parent| layout.tree.node(parent).kind != NodeKind::Workspace)?,
        name => {
            if let Some(&package) = layout.package_names.get(name) {
                package
            } else {
                layout
                    .tree
                    .find_child_by_name(from_module, name)
                    .or_else(|| layout.tree.find_child_by_name(from_file.module_id, name))
                    .or_else(|| layout.tree.find_child_by_name(from_file.package_id, name))?
            }
        }
    };
    for segment in segments {
        match segment.as_str() {
            "self" => {}
            "crate" => current = from_file.package_id,
            "super" => {
                current = layout
                    .tree
                    .parent(current)
                    .filter(|&parent| layout.tree.node(parent).kind != NodeKind::Workspace)?;
            }
            name => current = layout.tree.find_child_by_name(current, name)?,
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::treesitter::lang::{DiscoveredFile, Language, WorkspaceLayout};
    use crate::treesitter::summary::ImportKind;
    use std::path::PathBuf;

    struct Fixture {
        layout: WorkspaceLayout,
        file: DiscoveredFile,
        app: NodeId,
        cli: NodeId,
        part: NodeId,
        lib: NodeId,
        queue: NodeId,
    }

    fn fixture() -> Fixture {
        let mut layout = WorkspaceLayout::new(PathBuf::new());
        let app = layout
            .tree
            .child(layout.tree.root(), NodeKind::Package, "app");
        let cli = layout.tree.child(app, NodeKind::Module, "cli");
        let part = layout.tree.child(cli, NodeKind::Module, "part");
        let lib = layout
            .tree
            .child(layout.tree.root(), NodeKind::Package, "lib");
        let queue = layout.tree.child(lib, NodeKind::Module, "queue");
        layout.package_names.insert("app".to_owned(), app);
        layout.package_names.insert("lib".to_owned(), lib);
        let file = DiscoveredFile {
            abs_path: PathBuf::from("/w/app/src/main.rs"),
            rel_path: "app/src/main.rs".to_owned(),
            module_id: app,
            package_id: app,
            language: Language::Rust,
            is_test: false,
        };
        Fixture {
            layout,
            file,
            app,
            cli,
            part,
            lib,
            queue,
        }
    }

    fn import(kind: ImportKind, segments: &[&str]) -> RawImport {
        RawImport {
            scope: crate::treesitter::summary::ModuleScope::FileRoot,
            path_segments: segments
                .iter()
                .map(|segment| (*segment).to_owned())
                .collect(),
            kind,
            raw_specifier: None,
            is_reexport: false,
        }
    }

    fn resolve(
        fixture: &Fixture,
        from_module: NodeId,
        import: &RawImport,
    ) -> Option<ResolvedImport> {
        resolve_import(&fixture.layout, &fixture.file, from_module, import)
    }

    #[test]
    fn crate_paths_resolve_through_modules_to_their_items() {
        let fixture = fixture();
        let item = import(
            ImportKind::Named {
                name: "Args".to_owned(),
                alias: None,
            },
            &["crate", "cli", "Args"],
        );
        assert_eq!(
            resolve(&fixture, fixture.app, &item),
            Some(ResolvedImport::Module {
                module: fixture.cli,
                item_name: Some("Args".to_owned()),
                is_default: false,
                is_glob: false,
            })
        );
        let module = import(
            ImportKind::Namespace {
                alias: "cli".to_owned(),
            },
            &["crate", "cli"],
        );
        assert_eq!(
            resolve(&fixture, fixture.app, &module),
            Some(ResolvedImport::Module {
                module: fixture.cli,
                item_name: None,
                is_default: false,
                is_glob: false,
            })
        );
        let glob = import(ImportKind::Glob, &["crate", "cli"]);
        assert_eq!(
            resolve(&fixture, fixture.app, &glob),
            Some(ResolvedImport::Module {
                module: fixture.cli,
                item_name: None,
                is_default: false,
                is_glob: true,
            })
        );
    }

    #[test]
    fn self_and_super_resolve_from_the_module_that_writes_them() {
        let fixture = fixture();
        let own = import(
            ImportKind::Named {
                name: "Thing".to_owned(),
                alias: None,
            },
            &["self", "part", "Thing"],
        );
        assert_eq!(
            resolve(&fixture, fixture.cli, &own),
            Some(ResolvedImport::Module {
                module: fixture.part,
                item_name: Some("Thing".to_owned()),
                is_default: false,
                is_glob: false,
            })
        );
        let parent = import(
            ImportKind::Named {
                name: "Args".to_owned(),
                alias: None,
            },
            &["super", "Args"],
        );
        assert_eq!(
            resolve(&fixture, fixture.part, &parent),
            Some(ResolvedImport::Module {
                module: fixture.cli,
                item_name: Some("Args".to_owned()),
                is_default: false,
                is_glob: false,
            })
        );
        let grandparent = import(
            ImportKind::Namespace {
                alias: "cli".to_owned(),
            },
            &["super", "super", "cli"],
        );
        assert_eq!(
            resolve(&fixture, fixture.part, &grandparent),
            Some(ResolvedImport::Module {
                module: fixture.cli,
                item_name: None,
                is_default: false,
                is_glob: false,
            })
        );
    }

    #[test]
    fn a_sibling_crate_is_found_by_its_package_name() {
        let fixture = fixture();
        let item = import(
            ImportKind::Named {
                name: "Vault".to_owned(),
                alias: None,
            },
            &["lib", "Vault"],
        );
        assert_eq!(
            resolve(&fixture, fixture.app, &item),
            Some(ResolvedImport::Module {
                module: fixture.lib,
                item_name: Some("Vault".to_owned()),
                is_default: false,
                is_glob: false,
            })
        );
        let nested = import(
            ImportKind::Namespace {
                alias: "queue".to_owned(),
            },
            &["lib", "queue"],
        );
        assert_eq!(
            resolve(&fixture, fixture.app, &nested),
            Some(ResolvedImport::Module {
                module: fixture.queue,
                item_name: None,
                is_default: false,
                is_glob: false,
            })
        );
    }

    #[test]
    fn a_relative_child_module_resolves_from_the_module_that_writes_it() {
        let fixture = fixture();
        let child = import(
            ImportKind::Named {
                name: "Thing".to_owned(),
                alias: None,
            },
            &["part", "Thing"],
        );
        assert_eq!(
            resolve(&fixture, fixture.cli, &child),
            Some(ResolvedImport::Module {
                module: fixture.part,
                item_name: Some("Thing".to_owned()),
                is_default: false,
                is_glob: false,
            })
        );
    }

    #[test]
    fn a_namespace_import_of_an_item_names_it_when_the_path_is_not_a_module() {
        let fixture = fixture();
        let item = import(
            ImportKind::Namespace {
                alias: "Kind".to_owned(),
            },
            &["crate", "cli", "Kind"],
        );
        assert_eq!(
            resolve(&fixture, fixture.app, &item),
            Some(ResolvedImport::Module {
                module: fixture.cli,
                item_name: Some("Kind".to_owned()),
                is_default: false,
                is_glob: false,
            })
        );
    }

    #[test]
    fn an_aliased_module_import_resolves_to_the_module_itself() {
        let fixture = fixture();
        let module = import(
            ImportKind::Named {
                name: "cli".to_owned(),
                alias: Some("command".to_owned()),
            },
            &["crate", "cli"],
        );
        assert_eq!(
            resolve(&fixture, fixture.app, &module),
            Some(ResolvedImport::Module {
                module: fixture.cli,
                item_name: None,
                is_default: false,
                is_glob: false,
            })
        );
    }

    #[test]
    fn unknown_and_non_rust_imports_resolve_to_nothing() {
        let fixture = fixture();
        let missing = import(
            ImportKind::Named {
                name: "X".to_owned(),
                alias: None,
            },
            &["crate", "missing", "X"],
        );
        assert_eq!(resolve(&fixture, fixture.app, &missing), None);
        let bare = import(
            ImportKind::Named {
                name: "Serde".to_owned(),
                alias: None,
            },
            &["serde", "Serialize"],
        );
        assert_eq!(resolve(&fixture, fixture.app, &bare), None);
    }

    #[test]
    fn crate_alone_is_the_package_root() {
        let fixture = fixture();
        let module = import(
            ImportKind::Namespace {
                alias: "crate".to_owned(),
            },
            &["crate"],
        );
        assert_eq!(
            resolve(&fixture, fixture.cli, &module),
            Some(ResolvedImport::Module {
                module: fixture.app,
                item_name: None,
                is_default: false,
                is_glob: false,
            })
        );
    }
}
