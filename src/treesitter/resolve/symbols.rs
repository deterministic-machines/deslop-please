//! Module tree, item creation, the global symbol table, imports and `impl`
//! binding.
//!
//! A module's symbol table has three layers: the items it declares itself, the
//! names it imports (alias bindings, namespace aliases and glob imports), and
//! the names visible through its ancestors. Imports and re-exports are resolved
//! to a fixpoint so a chain of `pub use ...` always points at the original
//! defining item.

use std::collections::HashMap;

use crate::model::{File, FileId, Item, ItemId, ItemKind, NodeId, NodeKind};
use crate::treesitter::lang::{LanguageFrontend, ResolvedImport, WorkspaceLayout};
use crate::treesitter::summary::{FileSummary, ImportKind, ModuleScope, TypeExpr};

use super::{FileCtx, Resolver};

/// What a path segment names before it is turned into an item reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Module(NodeId),
    Item(ItemId),
}

/// One language-level import, kept between passes so re-exports can reach a
/// fixpoint.
struct ImportEdge {
    from: NodeId,
    target: NodeId,
    binding: Binding,
    reexport: bool,
    /// The import names a default-exported item; none in Rust.
    default: bool,
}

enum Binding {
    Named { name: String, alias: Option<String> },
    Default { alias: String },
    Namespace { alias: String },
    Glob,
}

impl Resolver {
    /// Register every file, its inline modules and one module item per tree
    /// package/module. Must run before the other passes.
    pub(super) fn add_files(&mut self, layout: &mut WorkspaceLayout, summaries: &[FileSummary]) {
        self.package_names = layout.package_names.clone();
        let discovered: HashMap<String, usize> = layout
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.rel_path.clone(), index))
            .collect();
        for summary in summaries {
            let found = discovered.get(&summary.file_path).copied();
            let (module, is_test) = match found {
                Some(index) => {
                    let file = &layout.files[index];
                    (file.module_id, file.is_test || summary.is_test)
                }
                None => (summary.module_id, summary.is_test),
            };
            let mut scopes = HashMap::new();
            scopes.insert(ModuleScope::FileRoot, module);
            register_inline_modules(layout, summary, module, &mut scopes);
            self.files.push(FileCtx {
                entry: File {
                    path: summary.file_path.clone(),
                    module,
                    test: is_test,
                    lines: summary.lines,
                },
                discovered: found,
                scopes,
                decls: HashMap::new(),
            });
        }
    }

    /// Finish the file pass once the resolver owns the final tree: mark
    /// modules that exist only for tests and create one module item each.
    pub(super) fn finish_file_pass(&mut self) {
        self.mark_test_only_modules();
        self.create_module_items();
    }

    /// One `Item` per declaration of every file, preserving the file-local
    /// mapping, plus the module-level scope and export tables.
    pub(super) fn add_declarations(&mut self, summaries: &[FileSummary]) {
        for (index, summary) in summaries.iter().enumerate() {
            let default_owner = self.files[index].entry.module;
            for decl in &summary.decls {
                let owner = self.files[index]
                    .scopes
                    .get(&decl.scope)
                    .copied()
                    .unwrap_or(default_owner);
                let item = ItemId::new(self.items.len());
                self.items.push(Item {
                    name: decl.name.clone(),
                    owner,
                    module: None,
                    file: Some(FileId::new(index)),
                    kind: decl.kind,
                    parent: None,
                    trait_impl: decl.trait_impl,
                    takes_self: decl.takes_self,
                    holds_nothing: decl.holds_nothing,
                });
                self.files[index].decls.insert(decl.local_id, item);
                if decl.is_test {
                    self.test_items.insert(item);
                }
                if matches!(decl.kind, ItemKind::Module | ItemKind::Impl) {
                    continue;
                }
                if decl.parent_decl.is_some() {
                    continue;
                }
                self.module_decls
                    .entry(owner)
                    .or_default()
                    .insert(decl.name.clone(), item);
                if decl.is_exported {
                    self.local_exports
                        .entry(owner)
                        .or_default()
                        .insert(decl.name.clone(), item);
                }
                if decl.is_default_export {
                    self.module_default.insert(owner, item);
                }
            }
        }
        self.module_exports = self.local_exports.clone();
    }

    /// Resolve every import through its file's frontend, then iterate named,
    /// default and glob re-exports to a fixpoint.
    pub(super) fn resolve_imports(
        &mut self,
        layout: &WorkspaceLayout,
        summaries: &[FileSummary],
        frontends: &[&dyn LanguageFrontend],
    ) {
        let mut edges = Vec::new();
        for (index, summary) in summaries.iter().enumerate() {
            let Some(discovered) = self.files[index].discovered else {
                continue;
            };
            let file = &layout.files[discovered];
            let Some(frontend) = frontends
                .iter()
                .find(|frontend| frontend.language() == file.language)
            else {
                continue;
            };
            for import in &summary.imports {
                let from = self.files[index]
                    .scopes
                    .get(&import.scope)
                    .copied()
                    .unwrap_or(self.files[index].entry.module);
                let Some(ResolvedImport::Module {
                    module,
                    item_name,
                    is_default,
                    is_glob,
                }) = frontend.resolve_import(layout, file, from, import)
                else {
                    continue;
                };
                let binding = if is_glob {
                    Binding::Glob
                } else if is_default {
                    let alias = match &import.kind {
                        ImportKind::Named { name, alias } => {
                            alias.clone().unwrap_or_else(|| name.clone())
                        }
                        ImportKind::Namespace { alias } => alias.clone(),
                        ImportKind::Glob => continue,
                    };
                    Binding::Default { alias }
                } else {
                    match (&import.kind, &item_name) {
                        (ImportKind::Named { name, alias }, Some(resolved_name)) => {
                            let effective_alias = alias
                                .clone()
                                .or_else(|| (name != resolved_name).then(|| name.clone()));
                            Binding::Named {
                                name: resolved_name.clone(),
                                alias: effective_alias,
                            }
                        }
                        (ImportKind::Named { name, alias }, None) => Binding::Namespace {
                            alias: alias.clone().unwrap_or_else(|| name.clone()),
                        },
                        (ImportKind::Namespace { alias }, Some(resolved_name)) => Binding::Named {
                            name: resolved_name.clone(),
                            alias: (alias != resolved_name).then(|| alias.clone()),
                        },
                        (ImportKind::Namespace { alias }, None) => Binding::Namespace {
                            alias: alias.clone(),
                        },
                        (ImportKind::Glob, _) => Binding::Glob,
                    }
                };
                edges.push(ImportEdge {
                    from,
                    target: module,
                    binding,
                    reexport: import.is_reexport,
                    default: is_default,
                });
            }
        }
        self.resolve_reexports(&edges);
        self.bind_imports(&edges);
    }

    /// Re-export names transparently to the original defining item, bounded so
    /// a pathological chain cannot loop forever.
    fn resolve_reexports(&mut self, edges: &[ImportEdge]) {
        let base_exports = self.local_exports.clone();
        let base_defaults = self.module_default.clone();
        for _ in 0..16 {
            let mut exports = base_exports.clone();
            let mut defaults = base_defaults.clone();
            for edge in edges.iter().filter(|edge| edge.reexport) {
                match &edge.binding {
                    Binding::Named { name, alias } => {
                        let item = self.exported_item(edge.target, name, edge.default);
                        if let Some(item) = item {
                            let exported = alias.clone().unwrap_or_else(|| name.clone());
                            exports.entry(edge.from).or_default().insert(exported, item);
                        }
                    }
                    Binding::Default { alias } => {
                        if let Some(&item) = self.module_default.get(&edge.target) {
                            defaults.insert(edge.from, item);
                            exports
                                .entry(edge.from)
                                .or_default()
                                .entry(alias.clone())
                                .or_insert(item);
                        }
                    }
                    Binding::Glob => {
                        if let Some(source) = self.module_exports.get(&edge.target) {
                            let destination = exports.entry(edge.from).or_default();
                            for (name, item) in source {
                                destination.entry(name.clone()).or_insert(*item);
                            }
                        }
                    }
                    Binding::Namespace { .. } => {}
                }
            }
            let converged = exports == self.module_exports && defaults == self.module_default;
            self.module_exports = exports;
            self.module_default = defaults;
            if converged {
                break;
            }
        }
    }

    /// Bind the names code in each module can write: named imports and default
    /// imports as items, namespace imports as modules, glob imports as scopes.
    fn bind_imports(&mut self, edges: &[ImportEdge]) {
        for edge in edges {
            match &edge.binding {
                Binding::Named { name, alias } => {
                    let Some(item) = self.exported_item(edge.target, name, edge.default) else {
                        continue;
                    };
                    self.module_imports
                        .entry(edge.from)
                        .or_default()
                        .insert(alias.clone().unwrap_or_else(|| name.clone()), item);
                }
                Binding::Default { alias } => {
                    let Some(&item) = self.module_default.get(&edge.target) else {
                        continue;
                    };
                    self.module_imports
                        .entry(edge.from)
                        .or_default()
                        .insert(alias.clone(), item);
                }
                Binding::Namespace { alias } => {
                    self.module_namespaces
                        .entry(edge.from)
                        .or_default()
                        .insert(alias.clone(), edge.target);
                }
                Binding::Glob => {
                    let globs = self.module_globs.entry(edge.from).or_default();
                    if !globs.contains(&edge.target) {
                        globs.push(edge.target);
                    }
                }
            }
        }
    }

    fn exported_item(&self, module: NodeId, name: &str, default: bool) -> Option<ItemId> {
        if default || name == "default" {
            return self.module_default.get(&module).copied();
        }
        self.module_exports
            .get(&module)
            .and_then(|exports| exports.get(name))
            .copied()
    }

    /// Bind each `impl` block to its target type, place methods and fields
    /// under their parent, and resolve declared field and return types.
    pub(super) fn bind_types(&mut self, summaries: &[FileSummary]) {
        let mut impl_targets: HashMap<ItemId, Option<ItemId>> = HashMap::new();
        let mut impl_traits: HashMap<ItemId, Option<String>> = HashMap::new();
        let mut impl_is_trait: HashMap<ItemId, bool> = HashMap::new();
        for (index, summary) in summaries.iter().enumerate() {
            for decl in &summary.decls {
                if decl.kind != ItemKind::Impl {
                    continue;
                }
                let Some(item) = self.item_of(index, decl.local_id) else {
                    continue;
                };
                let module = self.scope_of(index, &decl.scope);
                let target = decl
                    .impl_target
                    .as_ref()
                    .and_then(|ty| self.resolve_type_expr(module, ty));
                if let Some(target) = target {
                    // An `impl` block belongs to the type it implements, so the
                    // graph folds its items into that type's thing.
                    self.items[item.index()].parent = Some(target);
                }
                impl_targets.insert(item, target);
                impl_traits.insert(
                    item,
                    decl.impl_trait
                        .as_ref()
                        .and_then(|ty| ty.path.last().cloned()),
                );
                impl_is_trait.insert(item, decl.trait_impl || decl.impl_trait.is_some());
            }
        }
        for (index, summary) in summaries.iter().enumerate() {
            for decl in &summary.decls {
                if decl.kind == ItemKind::Impl {
                    continue;
                }
                let Some(parent_local) = decl.parent_decl else {
                    continue;
                };
                let Some(item) = self.item_of(index, decl.local_id) else {
                    continue;
                };
                let Some(parent) = self.item_of(index, parent_local) else {
                    continue;
                };
                match self.items[parent.index()].kind {
                    ItemKind::Impl => {
                        let is_trait = impl_is_trait.get(&parent).copied().unwrap_or(false);
                        if is_trait {
                            self.items[item.index()].trait_impl = true;
                        }
                        let Some(target) = impl_targets.get(&parent).copied().flatten() else {
                            continue;
                        };
                        self.items[item.index()].parent = Some(target);
                        let type_name = self.items[target.index()].name.clone();
                        let name = if is_trait {
                            let trait_name = impl_traits
                                .get(&parent)
                                .cloned()
                                .flatten()
                                .unwrap_or_else(|| type_name.clone());
                            format!("<{type_name} as {trait_name}>::{}", decl.name)
                        } else {
                            format!("{type_name}::{}", decl.name)
                        };
                        self.items[item.index()].name = name;
                        self.register_member(target, &decl.name, item, decl.kind);
                    }
                    ItemKind::Type => {
                        self.items[item.index()].parent = Some(parent);
                        // A field or a variant is written inside its type's
                        // symbol, as SCIP names it: `RunQueue::items`.
                        if matches!(decl.kind, ItemKind::Field | ItemKind::Variant) {
                            let type_name = self.items[parent.index()].name.clone();
                            self.items[item.index()].name = format!("{type_name}::{}", decl.name);
                        }
                        self.register_member(parent, &decl.name, item, decl.kind);
                    }
                    ItemKind::Variant => {
                        self.items[item.index()].parent = Some(parent);
                    }
                    _ => {}
                }
            }
        }
        for (index, summary) in summaries.iter().enumerate() {
            for decl in &summary.decls {
                let Some(item) = self.item_of(index, decl.local_id) else {
                    continue;
                };
                let module = self.scope_of(index, &decl.scope);
                match decl.kind {
                    ItemKind::Field => {
                        let Some(ty) = &decl.type_annotation else {
                            continue;
                        };
                        self.field_type_exprs.insert(item, ty.clone());
                        if let Some(target) = self.resolve_type_expr(module, ty) {
                            self.field_types.insert(item, target);
                        }
                    }
                    ItemKind::Function => {
                        let Some(ty) = decl
                            .signature
                            .as_ref()
                            .and_then(|signature| signature.return_type.as_ref())
                        else {
                            continue;
                        };
                        if let Some(target) = self.resolve_type_expr(module, ty) {
                            self.return_types.insert(item, target);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn register_member(&mut self, parent: ItemId, name: &str, item: ItemId, kind: ItemKind) {
        let members = match kind {
            ItemKind::Field => &mut self.type_fields,
            ItemKind::Variant => &mut self.type_variants,
            _ => &mut self.type_methods,
        };
        members
            .entry(parent)
            .or_default()
            .insert(name.to_owned(), item);
    }

    /// Resolve a declared type to the workspace type it holds, peeling the
    /// containers that only wrap a value.
    pub(super) fn resolve_type_expr(&self, module: NodeId, ty: &TypeExpr) -> Option<ItemId> {
        let peeled = peel_holder(ty);
        if peeled.path.is_empty() || peeled.path.last().map(String::as_str) == Some("Self") {
            return None;
        }
        let item = self.resolve_path(module, &peeled.path)?;
        (self.items[item.index()].kind == ItemKind::Type).then_some(item)
    }

    /// Resolve a (possibly qualified) path written in `module` to the item it
    /// names. A path that ends at a module resolves to that module's item.
    pub(super) fn resolve_path(&self, module: NodeId, segments: &[String]) -> Option<ItemId> {
        let (first, rest) = segments.split_first()?;
        let mut target = self.lookup_name(module, first)?;
        let mut item = match target {
            Target::Module(_) => None,
            Target::Item(item) => Some(item),
        };
        for segment in rest {
            match item {
                None => {
                    let Target::Module(node) = target else {
                        return None;
                    };
                    let relative = match segment.as_str() {
                        "super" => self.tree.parent(node).map(Target::Module),
                        "self" => Some(Target::Module(node)),
                        "crate" => Some(Target::Module(self.package_root(node))),
                        _ => None,
                    };
                    if let Some(next) = relative {
                        target = next;
                        continue;
                    }
                    match self.lookup_direct(node, segment)? {
                        Target::Module(next) => {
                            target = Target::Module(next);
                            item = None;
                        }
                        Target::Item(next) => item = Some(next),
                    }
                }
                Some(current) => item = Some(self.member_any(current, segment)?),
            }
        }
        match item {
            Some(item) => Some(item),
            None => match target {
                Target::Module(node) => self.module_items.get(&node).copied(),
                Target::Item(_) => None,
            },
        }
    }

    /// Look up a single name as code written in `module` sees it: its own
    /// declarations, imports, child modules, and globs, or a workspace package.
    pub(super) fn lookup_name(&self, module: NodeId, name: &str) -> Option<Target> {
        match name {
            "crate" => return Some(Target::Module(self.package_root(module))),
            "self" => return Some(Target::Module(module)),
            "super" => return self.tree.parent(module).map(Target::Module),
            _ => {}
        }
        if let Some(target) = self.lookup_direct(module, name) {
            return Some(target);
        }
        if let Some(&node) = self.package_names.get(name) {
            return Some(Target::Module(node));
        }
        None
    }

    /// Look up a name in one module only, without walking its ancestors.
    pub(super) fn lookup_direct(&self, module: NodeId, name: &str) -> Option<Target> {
        if let Some(&node) = self
            .module_namespaces
            .get(&module)
            .and_then(|names| names.get(name))
        {
            return Some(Target::Module(node));
        }
        if let Some(&item) = self
            .module_decls
            .get(&module)
            .and_then(|decls| decls.get(name))
        {
            return Some(Target::Item(item));
        }
        if let Some(&item) = self
            .module_imports
            .get(&module)
            .and_then(|imports| imports.get(name))
        {
            return Some(Target::Item(item));
        }
        if let Some(node) = self.tree.find_child_by_name(module, name) {
            return Some(Target::Module(node));
        }
        for glob in self
            .module_globs
            .get(&module)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if let Some(&item) = self
                .module_exports
                .get(glob)
                .and_then(|exports| exports.get(name))
            {
                return Some(Target::Item(item));
            }
        }
        None
    }

    pub(super) fn package_root(&self, module: NodeId) -> NodeId {
        self.tree
            .package_of(module)
            .unwrap_or_else(|| self.tree.root())
    }

    fn member_any(&self, item: ItemId, name: &str) -> Option<ItemId> {
        if self.items[item.index()].kind == ItemKind::Module {
            let module = self.items[item.index()].module?;
            return match self.lookup_direct(module, name)? {
                Target::Item(id) => Some(id),
                Target::Module(child) => self.module_items.get(&child).copied(),
            };
        }
        if self.items[item.index()].kind != ItemKind::Type {
            return None;
        }
        self.type_variants
            .get(&item)
            .and_then(|members| members.get(name))
            .copied()
            .or_else(|| {
                self.type_methods
                    .get(&item)
                    .and_then(|members| members.get(name))
                    .copied()
            })
            .or_else(|| {
                self.type_fields
                    .get(&item)
                    .and_then(|members| members.get(name))
                    .copied()
            })
    }

    fn mark_test_only_modules(&mut self) {
        let mut modules: HashMap<NodeId, bool> = HashMap::new();
        for context in &self.files {
            let all_test = modules.entry(context.entry.module).or_insert(true);
            *all_test &= context.entry.test;
        }
        for (module, all_test) in modules {
            if all_test && self.tree.node(module).kind == NodeKind::Module {
                self.tree.node_mut(module).test = true;
            }
        }
    }

    fn create_module_items(&mut self) {
        let root = self.tree.root();
        let ids: Vec<NodeId> = self.tree.ids().collect();
        for id in ids {
            if id == root {
                continue;
            }
            let node = self.tree.node(id);
            if !matches!(node.kind, NodeKind::Module | NodeKind::Package) {
                continue;
            }
            let item = ItemId::new(self.items.len());
            self.items.push(Item {
                name: node.name.clone(),
                owner: node.parent.unwrap_or(root),
                module: Some(id),
                file: None,
                kind: ItemKind::Module,
                parent: None,
                trait_impl: false,
                takes_self: false,
                holds_nothing: false,
            });
            self.module_items.insert(id, item);
        }
        let modules: Vec<(NodeId, ItemId, NodeId, String)> = self
            .module_items
            .iter()
            .filter_map(|(&module, &item)| {
                let parent = self.tree.parent(module)?;
                Some((module, item, parent, self.tree.node(module).name.clone()))
            })
            .collect();
        for (_, item, parent, name) in modules {
            self.module_decls
                .entry(parent)
                .or_default()
                .entry(name.clone())
                .or_insert(item);
            self.local_exports
                .entry(parent)
                .or_default()
                .entry(name)
                .or_insert(item);
        }
    }
}

/// Register a file's inline modules under their parents. Declarations may be
/// listed in any order, so parents are resolved until no progress is made; a
/// declaration whose parent is missing is attached to the file's module.
fn register_inline_modules(
    layout: &mut WorkspaceLayout,
    summary: &FileSummary,
    module: NodeId,
    scopes: &mut HashMap<ModuleScope, NodeId>,
) {
    let mut pending: Vec<&crate::treesitter::summary::InlineModuleDecl> =
        summary.inline_modules.iter().collect();
    let mut unresolved = Vec::new();
    loop {
        if pending.is_empty() {
            break;
        }
        let mut remaining = Vec::new();
        let mut progressed = false;
        for decl in pending.drain(..) {
            let Some(&parent) = scopes.get(&decl.parent) else {
                remaining.push(decl);
                continue;
            };
            progressed = true;
            let existed = layout.tree.find_child_by_name(parent, &decl.name).is_some();
            let node = layout.tree.child(parent, NodeKind::Module, &decl.name);
            if !existed && (decl.is_test || is_test_module(&decl.name) || summary.is_test) {
                layout.tree.node_mut(node).test = true;
            }
            scopes.insert(ModuleScope::Inline(decl.local_id), node);
        }
        if !progressed {
            unresolved = remaining;
            break;
        }
        pending = remaining;
    }
    for decl in unresolved {
        let existed = layout.tree.find_child_by_name(module, &decl.name).is_some();
        let node = layout.tree.child(module, NodeKind::Module, &decl.name);
        if !existed && (decl.is_test || is_test_module(&decl.name) || summary.is_test) {
            layout.tree.node_mut(node).test = true;
        }
        scopes.insert(ModuleScope::Inline(decl.local_id), node);
    }
}

fn is_test_module(name: &str) -> bool {
    name == "tests" || name == "test" || name.ends_with("_tests") || name.starts_with("test_")
}

/// What holds a value and is peeled off a declared type to reach it.
const HOLDERS: [&str; 27] = [
    "Optional",
    "List",
    "Dict",
    "Sequence",
    "Iterable",
    "Awaitable",
    "list",
    "dict",
    "set",
    "slice",
    "Option",
    "Result",
    "Box",
    "Arc",
    "Rc",
    "RefCell",
    "Mutex",
    "RwLock",
    "Cell",
    "Vec",
    "Promise",
    "Array",
    "Readonly",
    "ReadonlyArray",
    "Set",
    "Map",
    "Weak",
];

/// Peel the containers that only wrap a value: `Option<Arc<Queue>>` -> `Queue`.
fn peel_holder(ty: &TypeExpr) -> &TypeExpr {
    let mut current = ty;
    loop {
        let Some(name) = current.path.last().map(String::as_str) else {
            return current;
        };
        if !HOLDERS.contains(&name) {
            return current;
        }
        let inner = match name {
            "Result" => current.args.first(),
            "Map" => current.args.last(),
            _ if current.args.len() == 1 => current.args.first(),
            _ => None,
        };
        let Some(inner) = inner else {
            return current;
        };
        current = inner;
    }
}
