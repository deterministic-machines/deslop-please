//! Rust frontend using `Cargo.toml` and `tree-sitter-rust`.
//!
//! [`RustFrontend`] discovers a pure-TOML Cargo workspace, walks each crate's
//! `mod` tree, extracts a [`FileSummary`](crate::treesitter::summary::FileSummary)
//! per file with its inline modules, imports, declarations, and references,
//! and resolves `use` paths against the discovered tree.

mod cfg;
mod discovery;
mod extract;
mod resolve;

use std::path::Path;

use crate::Error;
use crate::configuration::Configuration;
use crate::model::NodeId;
use crate::treesitter::lang::{
    DiscoveredFile, Language, LanguageFrontend, ResolvedImport, WorkspaceLayout,
};
use crate::treesitter::summary::{FileSummary, RawImport};

/// The Rust language frontend.
pub struct RustFrontend;

impl LanguageFrontend for RustFrontend {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn detect(&self, root: &Path) -> bool {
        root.join("Cargo.toml").is_file()
    }

    fn discover(
        &self,
        root: &Path,
        configuration: &Configuration,
        layout: &mut WorkspaceLayout,
    ) -> Result<(), Error> {
        discovery::discover(root, configuration, layout)
    }

    fn extract_file(
        &self,
        file: &DiscoveredFile,
        source: &str,
        cargo_config: &Configuration,
    ) -> FileSummary {
        extract::extract(file, source, cargo_config)
    }

    fn resolve_import(
        &self,
        layout: &WorkspaceLayout,
        from_file: &DiscoveredFile,
        from_module: NodeId,
        import: &RawImport,
    ) -> Option<ResolvedImport> {
        resolve::resolve_import(layout, from_file, from_module, import)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::model::NodeKind;
    use crate::treesitter::lang::{Language, LanguageFrontend, WorkspaceLayout};

    fn temp_workspace(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "deslop-rust-frontend-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("temp workspace");
        directory
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directory");
        }
        std::fs::write(path, text).expect("fixture file");
    }

    fn discover_layout(root: &Path, cargo: &Configuration) -> WorkspaceLayout {
        let mut layout = WorkspaceLayout::new(root.to_path_buf());
        RustFrontend
            .discover(root, cargo, &mut layout)
            .expect("discovery");
        layout
    }

    fn modules(layout: &WorkspaceLayout) -> Vec<String> {
        layout
            .tree
            .ids()
            .filter(|&id| layout.tree.node(id).kind == NodeKind::Module)
            .map(|id| layout.tree.display(id))
            .collect()
    }

    fn extract(
        layout: &WorkspaceLayout,
        relative: &str,
    ) -> crate::treesitter::summary::FileSummary {
        let file = layout
            .files
            .iter()
            .find(|file| file.rel_path == relative)
            .unwrap_or_else(|| panic!("no file `{relative}`"));
        let source = std::fs::read_to_string(&file.abs_path).expect("source");
        RustFrontend.extract_file(file, &source, &Configuration::default())
    }

    #[test]
    fn detect_looks_for_a_cargo_manifest() {
        let root = temp_workspace("detect");
        assert!(!RustFrontend.detect(&root));
        write(
            &root,
            "Cargo.toml",
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\n",
        );
        assert!(RustFrontend.detect(&root));
        assert_eq!(RustFrontend.language(), Language::Rust);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_workspace_is_discovered_extracted_and_resolved_end_to_end() {
        let root = temp_workspace("end-to-end");
        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"crates/lib\"]\nresolver = \"3\"\n",
        );
        write(
            &root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nlib = { path = \"../crates/lib\" }\n",
        );
        write(
            &root,
            "app/src/main.rs",
            "mod cli;\nuse lib::queue::RunQueue;\nfn main() { let _ = RunQueue::new(1); }\n",
        );
        write(
            &root,
            "app/src/cli.rs",
            "pub struct Args { pub verbose: u32 }\n",
        );
        write(
            &root,
            "crates/lib/Cargo.toml",
            "[package]\nname = \"lib\"\nversion = \"0.1.0\"\n",
        );
        write(&root, "crates/lib/src/lib.rs", "pub mod queue;\n");
        write(
            &root,
            "crates/lib/src/queue.rs",
            "pub struct RunQueue { items: Vec<u32> }\nimpl RunQueue { pub fn new(limit: usize) -> Self { RunQueue { items: Vec::with_capacity(limit) } } }\n",
        );

        let layout = discover_layout(&root, &Configuration::default());
        assert_eq!(modules(&layout), ["app::cli", "lib::queue"]);
        assert_eq!(
            layout
                .files
                .iter()
                .map(|file| file.rel_path.as_str())
                .collect::<Vec<_>>(),
            [
                "app/src/main.rs",
                "app/src/cli.rs",
                "crates/lib/src/lib.rs",
                "crates/lib/src/queue.rs"
            ]
        );

        let main = extract(&layout, "app/src/main.rs");
        assert_eq!(main.imports.len(), 1);
        let import = &main.imports[0];
        assert_eq!(import.path_segments, ["lib", "queue", "RunQueue"]);
        let file = layout
            .files
            .iter()
            .find(|file| file.rel_path == "app/src/main.rs")
            .unwrap();
        let resolved = RustFrontend
            .resolve_import(&layout, file, file.module_id, import)
            .expect("resolved import");
        let crate::treesitter::lang::ResolvedImport::Module {
            module,
            item_name,
            is_glob,
            ..
        } = resolved;
        assert_eq!(layout.tree.display(module), "lib::queue");
        assert_eq!(item_name.as_deref(), Some("RunQueue"));
        assert!(!is_glob);

        let queue = extract(&layout, "crates/lib/src/queue.rs");
        assert!(queue.decls.iter().any(|decl| decl.name == "RunQueue"));
        assert!(queue.decls.iter().any(|decl| decl.name == "new"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::configuration::Configuration;
    use crate::model::NodeKind;
    use crate::treesitter::lang::{LanguageFrontend, WorkspaceLayout};
    use crate::treesitter::summary::{AccessMode, FileSummary, RawRefKind};

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn discover(root: &Path, cargo: &Configuration) -> WorkspaceLayout {
        let mut layout = WorkspaceLayout::new(root.to_path_buf());
        RustFrontend
            .discover(root, cargo, &mut layout)
            .expect("discovery");
        layout
    }

    fn modules(layout: &WorkspaceLayout) -> Vec<String> {
        layout
            .tree
            .ids()
            .filter(|&id| layout.tree.node(id).kind == NodeKind::Module)
            .map(|id| layout.tree.display(id))
            .collect()
    }

    fn files(layout: &WorkspaceLayout) -> Vec<&str> {
        layout
            .files
            .iter()
            .map(|file| file.rel_path.as_str())
            .collect()
    }

    fn extract(layout: &WorkspaceLayout, relative: &str) -> FileSummary {
        let file = layout
            .files
            .iter()
            .find(|file| file.rel_path == relative)
            .unwrap_or_else(|| panic!("no file `{relative}`"));
        let source = std::fs::read_to_string(&file.abs_path).expect("source");
        RustFrontend.extract_file(file, &source, &Configuration::default())
    }

    fn member_modes(summary: &FileSummary, member: &str) -> Vec<AccessMode> {
        summary
            .refs
            .iter()
            .filter_map(|reference| match &reference.kind {
                RawRefKind::MemberAccess { member: name, .. } if name == member => {
                    Some(reference.access)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_clean_fixture_has_the_module_tree_of_its_files() {
        let fixtures = fixtures();
        let layout = discover(&fixtures.join("clean"), &Configuration::default());
        assert_eq!(
            modules(&layout),
            ["clean::kinds", "clean::socket", "clean::socket::datagram"]
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
    }

    #[test]
    fn the_gated_fixture_honours_the_target_and_features() {
        let fixtures = fixtures();
        let default = discover(&fixtures.join("gated"), &Configuration::default());
        assert_eq!(
            modules(&default),
            [
                "gated::metrics",
                "gated::metrics::counter",
                "gated::metrics::sink",
            ]
        );
        let mut all = Configuration::default();
        all.all_features();
        let with_all = discover(&fixtures.join("gated"), &all);
        assert_eq!(
            modules(&with_all),
            [
                "gated::export",
                "gated::export::csv",
                "gated::export::rows",
                "gated::metrics",
                "gated::metrics::counter",
                "gated::metrics::sink",
            ]
        );
        let mut export = Configuration::default();
        export.features("export");
        export.no_default_features();
        let with_export = discover(&fixtures.join("gated"), &export);
        assert_eq!(
            modules(&with_export),
            ["gated::export", "gated::export::csv", "gated::export::rows"]
        );
    }

    #[test]
    fn the_two_crates_fixture_is_a_pure_toml_workspace_with_a_test_target() {
        let fixtures = fixtures();
        let layout = discover(&fixtures.join("two_crates"), &Configuration::default());
        assert_eq!(
            modules(&layout),
            ["lib::inner", "lib::parts", "lib::parts::detail"]
        );
        assert_eq!(
            files(&layout),
            [
                "app/src/main.rs",
                "app/tests/it.rs",
                "crates/lib/src/lib.rs",
                "crates/lib/src/inner.rs",
                "crates/lib/src/parts/mod.rs",
                "crates/lib/src/parts/detail.rs",
            ]
        );
        assert!(
            layout
                .files
                .iter()
                .find(|file| file.rel_path == "app/tests/it.rs")
                .unwrap()
                .is_test
        );
        assert!(layout.package_names.contains_key("app"));
        assert!(layout.package_names.contains_key("lib"));
    }

    #[test]
    fn the_included_module_fixture_finds_the_build_scripts_path_module() {
        let fixtures = fixtures();
        let layout = discover(&fixtures.join("included_module"), &Configuration::default());
        assert_eq!(modules(&layout), ["flags::flags"]);
        let flags = extract(&layout, "src/flags.rs");
        let names: Vec<&str> = flags.decls.iter().map(|decl| decl.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Flags",
                "names",
                "target",
                "Flags",
                "for_target",
                "describe"
            ]
        );
    }

    #[test]
    fn the_field_leak_fixture_extracts_its_fields_and_reaches() {
        let fixtures = fixtures();
        let layout = discover(&fixtures.join("field_leak"), &Configuration::default());
        let ring = extract(&layout, "src/queue/ring.rs");
        let names: Vec<&str> = ring.decls.iter().map(|decl| decl.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "RunQueue", "items", "head", "limit", "RunQueue", "new", "full"
            ]
        );
        let full = ring.decls.iter().find(|decl| decl.name == "full").unwrap();
        assert!(full.takes_self);
        assert!(full.self_is_shared_ref);
        assert!(full.body_summary.as_ref().unwrap().computes);
        assert!(
            full.body_summary
                .as_ref()
                .unwrap()
                .accessor_candidate
                .is_none()
        );

        let policy = extract(&layout, "src/sched/policy.rs");
        assert_eq!(
            member_modes(&policy, "items"),
            [AccessMode::Work, AccessMode::MethodCall]
        );
        assert_eq!(
            member_modes(&policy, "head"),
            [AccessMode::CopyOrRead, AccessMode::Work]
        );

        let report = extract(&layout, "src/sched/report.rs");
        assert_eq!(
            member_modes(&report, "items"),
            [AccessMode::MethodCall, AccessMode::MethodCall]
        );
        assert_eq!(
            member_modes(&report, "head"),
            [AccessMode::CopyOrRead, AccessMode::CopyOrRead]
        );
    }
}
