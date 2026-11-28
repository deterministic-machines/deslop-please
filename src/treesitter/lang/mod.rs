//! Language frontend abstraction and workspace layout discovery types.

pub mod go;
pub mod python;
pub mod rust;
pub mod typescript;

use std::collections::HashMap;
use std::path::PathBuf;

use crate::Error;
use crate::configuration::Configuration;
use crate::model::NodeId;
use crate::model::Tree;
use crate::treesitter::summary::{FileSummary, RawImport};

pub use crate::model::Language;

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    /// Absolute path on disk.
    pub abs_path: PathBuf,
    /// Workspace-relative `/`-separated path.
    pub rel_path: String,
    /// Owning module in `WorkspaceLayout::tree`.
    pub module_id: NodeId,
    /// Owning package in `WorkspaceLayout::tree`.
    pub package_id: NodeId,
    pub language: Language,
    pub is_test: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceLayout {
    /// The workspace root every `rel_path` is relative to.
    pub root: PathBuf,
    pub tree: Tree,
    pub files: Vec<DiscoveredFile>,
    /// By workspace-relative path, the module a file belongs to.
    pub file_to_module: HashMap<String, NodeId>,
    /// Map from package name (and crate-normalized snake_case name for Rust) to package `NodeId`.
    pub package_names: HashMap<String, NodeId>,
}

impl WorkspaceLayout {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            tree: Tree::new(),
            files: Vec::new(),
            file_to_module: HashMap::new(),
            package_names: HashMap::new(),
        }
    }
}

/// Result of resolving a language-specific import statement to a workspace target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedImport {
    /// Resolves to a target module; optionally names a specific exported item inside it.
    Module {
        module: NodeId,
        /// If `Some`, imports this named symbol from `module` (e.g. `Foo`).
        /// If `None` and `ImportKind::Namespace`, binds `module` itself.
        item_name: Option<String>,
        /// True when the import names a default-exported item.
        is_default: bool,
        /// True when wildcard glob import (`use a::*`, `export * from './a'`).
        is_glob: bool,
    },
}

/// Language frontend trait implemented by the Rust, TypeScript, Go and Python
/// frontends.
pub trait LanguageFrontend: Sync {
    fn language(&self) -> Language;

    /// Check if `root` contains a project of this language.
    fn detect(&self, root: &std::path::Path) -> bool;

    /// Populate packages, module hierarchy, and discovered source files into `layout`.
    fn discover(
        &self,
        root: &std::path::Path,
        configuration: &Configuration,
        layout: &mut WorkspaceLayout,
    ) -> Result<(), Error>;

    /// Parse a single source file with Tree-sitter and extract its `FileSummary`.
    fn extract_file(
        &self,
        file: &DiscoveredFile,
        source: &str,
        configuration: &Configuration,
    ) -> FileSummary;

    /// Resolve a `RawImport` written in `from_module` (inside `from_file`) to a workspace target.
    fn resolve_import(
        &self,
        layout: &WorkspaceLayout,
        from_file: &DiscoveredFile,
        from_module: NodeId,
        import: &RawImport,
    ) -> Option<ResolvedImport>;
}
