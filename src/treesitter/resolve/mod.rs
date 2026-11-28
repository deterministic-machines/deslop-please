//! Language-agnostic Stage 3 resolver: turns discovery's `WorkspaceLayout` and
//! each file's `FileSummary` into the complete architectural `Model`.
//!
//! Three passes run in order:
//! 1. module tree, files and items, plus the global symbol table and `impl`
//!    binding (`symbols`),
//! 2. reference resolution with per-function local type propagation
//!    (`local_types`),
//! 3. forwarder and accessor detection (`wrappers`).

mod local_types;
mod symbols;
mod wrappers;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use crate::model::{File, Item, ItemId, Model, NodeId, Reference, Tree, Wrapper};
use crate::treesitter::lang::{LanguageFrontend, WorkspaceLayout};
use crate::treesitter::summary::{AccessMode, FileSummary, LocalDeclId, ModuleScope, TypeExpr};

/// Build the model from the workspace layout and every discovered file's summary.
///
/// Summaries are matched to discovered files by their workspace-relative path,
/// so their order does not have to match the layout's.
pub fn build_model(
    layout: WorkspaceLayout,
    summaries: Vec<FileSummary>,
    frontends: &[&dyn LanguageFrontend],
) -> Model {
    Resolver::run(layout, summaries, frontends)
}

struct Resolver {
    tree: Tree,
    /// By `FileId`.
    files: Vec<FileCtx>,
    /// By `ItemId`, in creation order.
    items: Vec<Item>,
    references: Vec<Reference>,
    /// By the place of each reference in `references`: how its code uses what
    /// it names. Not part of the model.
    accesses: Vec<AccessMode>,
    wrappers: HashMap<ItemId, Wrapper>,
    /// The module item of every package and module node.
    module_items: HashMap<NodeId, ItemId>,
    /// Every item declared directly in a module (exported or not).
    module_decls: HashMap<NodeId, HashMap<String, ItemId>>,
    /// Exported items of every module, before imports are resolved.
    local_exports: HashMap<NodeId, HashMap<String, ItemId>>,
    /// Exported items of every module once re-exports reach a fixpoint.
    module_exports: HashMap<NodeId, HashMap<String, ItemId>>,
    /// The default export of every module.
    module_default: HashMap<NodeId, ItemId>,
    /// Names a module imports or re-exports, by the name code uses for them.
    module_imports: HashMap<NodeId, HashMap<String, ItemId>>,
    /// Namespace imports: alias to the imported module.
    module_namespaces: HashMap<NodeId, HashMap<String, NodeId>>,
    /// Glob imports: modules whose exports are bound in a module's scope.
    module_globs: HashMap<NodeId, Vec<NodeId>>,
    /// Fields of a type, by name.
    type_fields: HashMap<ItemId, HashMap<String, ItemId>>,
    /// Variants of an enum, by name.
    type_variants: HashMap<ItemId, HashMap<String, ItemId>>,
    /// Methods and associated items of a type, by name.
    type_methods: HashMap<ItemId, HashMap<String, ItemId>>,
    /// A field's declared type, resolved to the workspace type it holds.
    field_types: HashMap<ItemId, ItemId>,
    /// A field's declared type as written, for accessor signature checks.
    field_type_exprs: HashMap<ItemId, TypeExpr>,
    /// A function's return type, resolved to the workspace type it holds.
    return_types: HashMap<ItemId, ItemId>,
    test_items: HashSet<ItemId>,
    /// Package name (as code writes it) to package node.
    package_names: HashMap<String, NodeId>,
}

struct FileCtx {
    entry: File,
    /// Index of the matching `DiscoveredFile` in the layout.
    discovered: Option<usize>,
    scopes: HashMap<ModuleScope, NodeId>,
    /// `LocalDeclId` to `ItemId` for this file.
    decls: HashMap<LocalDeclId, ItemId>,
}

impl Resolver {
    fn run(
        mut layout: WorkspaceLayout,
        summaries: Vec<FileSummary>,
        frontends: &[&dyn LanguageFrontend],
    ) -> Model {
        let mut resolver = Resolver::new();
        resolver.add_files(&mut layout, &summaries);
        resolver.tree = layout.tree.clone();
        resolver.finish_file_pass();
        resolver.add_declarations(&summaries);
        resolver.resolve_imports(&layout, &summaries, frontends);
        resolver.bind_types(&summaries);
        resolver.resolve_references(&summaries);
        resolver.detect_wrappers(&summaries);
        Model {
            tree: resolver.tree,
            files: resolver
                .files
                .into_iter()
                .map(|context| context.entry)
                .collect(),
            items: resolver.items,
            references: resolver.references,
            wrappers: resolver.wrappers,
        }
    }
    fn new() -> Resolver {
        Resolver {
            tree: Tree::new(),
            files: Vec::new(),
            items: Vec::new(),
            references: Vec::new(),
            accesses: Vec::new(),
            wrappers: HashMap::new(),
            module_items: HashMap::new(),
            module_decls: HashMap::new(),
            local_exports: HashMap::new(),
            module_exports: HashMap::new(),
            module_default: HashMap::new(),
            module_imports: HashMap::new(),
            module_namespaces: HashMap::new(),
            module_globs: HashMap::new(),
            type_fields: HashMap::new(),
            type_variants: HashMap::new(),
            type_methods: HashMap::new(),
            field_types: HashMap::new(),
            field_type_exprs: HashMap::new(),
            return_types: HashMap::new(),
            test_items: HashSet::new(),
            package_names: HashMap::new(),
        }
    }

    fn item_of(&self, file: usize, local: LocalDeclId) -> Option<ItemId> {
        self.files.get(file)?.decls.get(&local).copied()
    }

    fn scope_of(&self, file: usize, scope: &ModuleScope) -> NodeId {
        let context = &self.files[file];
        context
            .scopes
            .get(scope)
            .copied()
            .unwrap_or(context.entry.module)
    }
}
