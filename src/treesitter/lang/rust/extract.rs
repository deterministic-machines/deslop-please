//! Tree-sitter extraction of one Rust file into a FileSummary.
//!
//! One declarative query (queries.scm) reads the file: its captures are filed
//! into buckets (modules, declarations, imports, locals, references, and the
//! nodes that make a body compute) and the summary is assembled from byte
//! ranges, so nothing recursively walks the tree. Only the shapes a query
//! cannot express keep a small helper: nested "use" trees, nested type
//! arguments, receiver chains, the syntactic access modes, and the single-body
//! checks that recognise forwarders and accessors.

mod body;
mod syntax;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::LazyLock;

use tree_sitter::Node;

use crate::configuration::Configuration;
use crate::model::ItemKind;
use crate::treesitter::lang::DiscoveredFile;
use crate::treesitter::lang::Language;
use crate::treesitter::query::{CompiledQuery, MatchView, ScopeIndex, line_count, node_line};
use crate::treesitter::summary::{
    AccessorFieldOp, FileSummary, FunctionSig, ImportKind, InlineModuleDecl, ItemDecl,
    LocalBinding, LocalDeclId, LocalModId, ModuleScope, ParamDecl, RawImport, TypeExpr,
};

use self::syntax::{holds_nothing, place};

use super::cfg::{self, CfgContext};
use super::discovery::is_test_module;

/// The one query every Rust file is read by.
static QUERY: LazyLock<CompiledQuery> = LazyLock::new(|| {
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    CompiledQuery::new(&language, include_str!("queries.scm"))
});

thread_local! {
    static PARSER: RefCell<tree_sitter::Parser> = RefCell::new({
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("tree-sitter-rust is a linked language");
        parser
    });
}

fn parse_rust(source: &str) -> Option<tree_sitter::Tree> {
    PARSER.with(|parser| parser.borrow_mut().parse(source, None))
}

/// Extract the FileSummary of "file" from "source".
pub(crate) fn extract(
    file: &DiscoveredFile,
    source: &str,
    cargo_config: &Configuration,
) -> FileSummary {
    let features = cfg::features_for_file(&file.abs_path, cargo_config);
    let summary = FileSummary {
        file_path: file.rel_path.clone(),
        module_id: file.module_id,
        language: Language::Rust,
        is_test: file.is_test,
        lines: line_count(source),
        inline_modules: Vec::new(),
        imports: Vec::new(),
        decls: Vec::new(),
        refs: Vec::new(),
    };
    let Some(tree) = parse_rust(source) else {
        return summary;
    };
    let mut extractor = Extractor {
        source,
        context: CfgContext::new(cargo_config, features),
        summary,
        scope: ScopeIndex::new(),
        takes_self: HashMap::new(),
        self_by_value: HashMap::new(),
        params: HashMap::new(),
        ops: HashMap::new(),
        computes: HashSet::new(),
        lends_to_write: HashSet::new(),
        locals: HashMap::new(),
        local_names: HashMap::new(),
        offset: 0,
        line_offset: 0,
    };
    extractor.run(tree.root_node());
    extractor.finish()
}

/// A declaration kind the query captures, before it becomes an ItemDecl.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclKind {
    Struct,
    Union,
    Enum,
    Trait,
    Type,
    Impl,
    Function,
    Constant,
    Variant,
    Field,
    TupleFields,
}

impl DeclKind {
    fn item_kind(self) -> ItemKind {
        match self {
            Self::Struct | Self::Union | Self::Enum | Self::Trait | Self::Type => ItemKind::Type,
            Self::Impl => ItemKind::Impl,
            Self::Function => ItemKind::Function,
            Self::Constant => ItemKind::Constant,
            Self::Variant => ItemKind::Variant,
            Self::Field | Self::TupleFields => ItemKind::Field,
        }
    }

    /// Whether items declared inside this declaration call it their parent.
    fn parental(self) -> bool {
        matches!(
            self,
            Self::Struct | Self::Union | Self::Enum | Self::Trait | Self::Impl | Self::Variant
        )
    }
}

/// A reference site the query captures, before it becomes a RawRef.
#[derive(Debug, Clone, Copy)]
enum RefSite<'t> {
    Type(Node<'t>),
    TypePath(Node<'t>),
    Struct(Node<'t>),
    Call(Node<'t>),
    Field(Node<'t>),
    Path(Node<'t>),
    Identifier(Node<'t>),
}

impl<'t> RefSite<'t> {
    fn node(self) -> Node<'t> {
        match self {
            Self::Type(node)
            | Self::TypePath(node)
            | Self::Struct(node)
            | Self::Call(node)
            | Self::Field(node)
            | Self::Path(node)
            | Self::Identifier(node) => node,
        }
    }
}

/// Every node one query run captured, for a file or a macro's arguments.
#[derive(Default)]
struct Nodes<'t> {
    modules: Vec<Node<'t>>,
    decls: Vec<(DeclKind, Node<'t>)>,
    imports: Vec<Node<'t>>,
    locals: Vec<Node<'t>>,
    refs: Vec<RefSite<'t>>,
    computes: Vec<Node<'t>>,
}

impl<'t> Nodes<'t> {
    fn collect(root: Node<'t>, source: &[u8]) -> Self {
        let mut nodes = Nodes::default();
        QUERY.for_each_match(root, source, |view| nodes.record(view));
        nodes
    }

    fn record(&mut self, view: &MatchView<'_, 't>) {
        for (name, node) in view.captures() {
            match name {
                "module" => self.modules.push(node),
                "decl.struct" => self.decls.push((DeclKind::Struct, node)),
                "decl.union" => self.decls.push((DeclKind::Union, node)),
                "decl.enum" => self.decls.push((DeclKind::Enum, node)),
                "decl.trait" => self.decls.push((DeclKind::Trait, node)),
                "decl.type" => self.decls.push((DeclKind::Type, node)),
                "decl.impl" => self.decls.push((DeclKind::Impl, node)),
                "decl.function" => self.decls.push((DeclKind::Function, node)),
                "decl.constant" => self.decls.push((DeclKind::Constant, node)),
                "decl.variant" => self.decls.push((DeclKind::Variant, node)),
                "decl.field" => self.decls.push((DeclKind::Field, node)),
                "decl.tuple_fields" => self.decls.push((DeclKind::TupleFields, node)),
                "import" => self.imports.push(node),
                "local" => self.locals.push(node),
                "ref.type" => self.refs.push(RefSite::Type(node)),
                "ref.type_path" => self.refs.push(RefSite::TypePath(node)),
                "ref.struct" => self.refs.push(RefSite::Struct(node)),
                "ref.call" => self.refs.push(RefSite::Call(node)),
                "ref.field" => self.refs.push(RefSite::Field(node)),
                "ref.path" => self.refs.push(RefSite::Path(node)),
                "ref.identifier" => self.refs.push(RefSite::Identifier(node)),
                "computes" => self.computes.push(node),
                _ => {}
            }
        }
    }
}

struct Extractor<'a> {
    source: &'a str,
    context: CfgContext,
    summary: FileSummary,
    scope: ScopeIndex,
    takes_self: HashMap<LocalDeclId, bool>,
    self_by_value: HashMap<LocalDeclId, bool>,
    params: HashMap<LocalDeclId, HashSet<String>>,
    /// Field operations of each method's body, for accessor detection.
    ops: HashMap<LocalDeclId, BTreeMap<String, AccessorFieldOp>>,
    computes: HashSet<LocalDeclId>,
    lends_to_write: HashSet<LocalDeclId>,
    locals: HashMap<LocalDeclId, Vec<LocalBinding>>,
    local_names: HashMap<LocalDeclId, HashSet<String>>,
    /// Byte offset and rows of the snippet being read: a macro's arguments.
    offset: usize,
    line_offset: u32,
}

impl Extractor<'_> {
    fn text(&self, node: Node<'_>) -> &str {
        let range = node.byte_range();
        self.source
            .get(self.offset + range.start..self.offset + range.end)
            .unwrap_or("")
    }

    fn line(&self, node: Node<'_>) -> u32 {
        node_line(node) + self.line_offset
    }

    fn run(&mut self, root: Node<'_>) {
        let nodes = Nodes::collect(root, self.source.as_bytes());
        self.register_modules(&nodes.modules);
        self.scope.finalize();
        let functions = self.register_decls(&nodes.decls);
        self.scope.finalize();
        self.collect_locals(&nodes.locals);
        self.emit_imports(&nodes.imports);
        self.emit_refs(&nodes.refs);
        self.emit_computes(&nodes.computes);
        self.emit_bodies(&functions);
    }

    fn finish(mut self) -> FileSummary {
        self.summary.refs.sort_by_key(|reference| reference.line);
        self.summary
    }

    /// Whether a declaration's cfg attributes leave it in this build.
    fn built(&self, node: Node<'_>) -> bool {
        let attributes = cfg::attributes_of(node, self.source.as_bytes());
        cfg::of_attrs(&attributes, &self.context).built()
    }

    /// Whether a declaration is test code: cfg(test), #[test], or a test name.
    fn test_context(&self, node: Node<'_>, name: Option<&str>) -> bool {
        let attributes = cfg::attributes_of(node, self.source.as_bytes());
        name.is_some_and(is_test_module) || cfg::marks_test(&attributes, &self.context)
    }

    // --------------------------------------------------------------- scopes

    fn register_modules(&mut self, modules: &[Node<'_>]) {
        let mut modules = modules.to_vec();
        modules.sort_by_key(|node| node.start_byte());
        let mut placed: Vec<(Range<usize>, ModuleScope)> = Vec::new();
        for node in modules {
            if !self.built(node) {
                self.scope.add_skipped_span(node.byte_range());
                continue;
            }
            let (Some(body), Some(name)) = (
                node.child_by_field_name("body"),
                node.child_by_field_name("name"),
            ) else {
                // An external "mod foo;" is discovered from the filesystem.
                continue;
            };
            let name = self.text(name).to_owned();
            let is_test =
                self.scope.is_test_at(node.start_byte()) || self.test_context(node, Some(&name));
            let parent = place(&placed, node.byte_range()).unwrap_or(ModuleScope::FileRoot);
            let local =
                LocalModId(u32::try_from(self.summary.inline_modules.len()).expect("module ids"));
            self.summary.inline_modules.push(InlineModuleDecl {
                local_id: local,
                parent,
                name,
                is_test,
            });
            if is_test {
                self.scope.add_test_span(node.byte_range());
            }
            let scope = ModuleScope::Inline(local);
            self.scope.add_module(body.byte_range(), scope);
            placed.push((node.byte_range(), scope));
        }
    }

    // --------------------------------------------------------- declarations

    fn register_decls<'t>(
        &mut self,
        decls: &[(DeclKind, Node<'t>)],
    ) -> Vec<(LocalDeclId, Node<'t>)> {
        let mut decls = decls.to_vec();
        decls.sort_by_key(|(_, node)| node.start_byte());
        let mut functions = Vec::new();
        for (kind, node) in decls {
            if self.scope.is_skipped_at(node.start_byte()) {
                continue;
            }
            if !self.built(node) {
                self.scope.add_skipped_span(node.byte_range());
                continue;
            }
            let is_test = self.scope.is_test_at(node.start_byte()) || self.test_context(node, None);
            if is_test {
                self.scope.add_test_span(node.byte_range());
            }
            if kind == DeclKind::TupleFields {
                self.tuple_fields(node);
                continue;
            }
            let Some(name) = self.decl_name(kind, node) else {
                continue;
            };
            let mut item = self.item(kind, node, name);
            item.is_test = is_test;
            let mut function = None;
            match kind {
                DeclKind::Function => {
                    let (params, takes_self, shared, by_value) = self.signature_parts(node);
                    item.takes_self = takes_self;
                    item.self_is_shared_ref = shared;
                    item.signature = Some(self.signature(node, &params));
                    item.trait_impl = self.in_trait_impl(node);
                    function = Some((params, takes_self, by_value));
                }
                DeclKind::Impl => {
                    let target = self.type_child(node, "type");
                    item.name = target
                        .as_ref()
                        .and_then(|target| target.path.last().cloned())
                        .unwrap_or_default();
                    item.trait_impl = node.child_by_field_name("trait").is_some();
                    item.impl_target = target;
                    item.impl_trait = self.type_child(node, "trait");
                }
                DeclKind::Field | DeclKind::Constant | DeclKind::Type => {
                    item.type_annotation = self.type_child(node, "type");
                    item.holds_nothing = item.type_annotation.as_ref().is_some_and(holds_nothing);
                }
                DeclKind::Variant => item.is_exported = true,
                _ => {}
            }
            let id = self.push_decl(item);
            let range = node.byte_range();
            // An impl header holds no item: its references belong to the enclosing one.
            if kind != DeclKind::Impl {
                self.scope.add_holder(range.clone(), id);
            }
            if kind.parental() {
                self.scope.add_parent(range, id);
            }
            if let Some((params, takes_self, by_value)) = function {
                self.takes_self.insert(id, takes_self);
                self.self_by_value.insert(id, by_value);
                self.params
                    .insert(id, params.into_iter().map(|param| param.name).collect());
                functions.push((id, node));
            }
        }
        functions
    }

    fn push_decl(&mut self, mut item: ItemDecl) -> LocalDeclId {
        item.local_id =
            LocalDeclId(u32::try_from(self.summary.decls.len()).expect("declaration ids"));
        let id = item.local_id;
        self.summary.decls.push(item);
        id
    }

    /// An item with everything its syntax alone says; some kinds fill in more.
    fn item(&self, kind: DeclKind, node: Node<'_>, name: String) -> ItemDecl {
        let range = node.byte_range();
        ItemDecl {
            local_id: LocalDeclId(0),
            scope: self.scope.module_at(range.start),
            name,
            kind: kind.item_kind(),
            parent_decl: self.scope.parent_decl_of(&range),
            impl_target: None,
            impl_trait: None,
            is_exported: self.is_exported(node),
            is_default_export: false,
            trait_impl: false,
            takes_self: false,
            self_is_shared_ref: false,
            holds_nothing: false,
            is_test: self.scope.is_test_at(range.start),
            type_annotation: None,
            signature: None,
            body_summary: None,
        }
    }

    fn decl_name(&self, kind: DeclKind, node: Node<'_>) -> Option<String> {
        node.child_by_field_name("name")
            .map(|name| self.text(name).to_owned())
            .or_else(|| (kind == DeclKind::Impl).then(String::new))
    }

    /// Tuple fields and tuple variants hold their types as direct children of
    /// an ordered list, with visibility modifiers between them.
    fn tuple_fields(&mut self, node: Node<'_>) {
        let at = node.start_byte();
        if self.scope.is_skipped_at(at) {
            return;
        }
        let Some(owner) = self.scope.parent_decl_of(&node.byte_range()) else {
            return;
        };
        let mut index = 0u32;
        let mut exported = false;
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "attribute_item" | "line_comment" | "block_comment" => continue,
                "visibility_modifier" => {
                    exported = true;
                    continue;
                }
                _ => {}
            }
            let ty = self.type_expr(child);
            let mut item = self.item(DeclKind::Field, child, index.to_string());
            item.parent_decl = Some(owner);
            item.is_exported = exported;
            item.holds_nothing = ty.as_ref().is_some_and(holds_nothing);
            item.type_annotation = ty;
            let id = self.push_decl(item);
            self.scope.add_holder(child.byte_range(), id);
            index += 1;
            exported = false;
        }
    }

    fn in_trait_impl(&self, node: Node<'_>) -> bool {
        self.scope
            .parent_decl_of(&node.byte_range())
            .is_some_and(|parent| self.summary.decls[parent.0 as usize].trait_impl)
    }

    fn type_child(&self, node: Node<'_>, field: &str) -> Option<TypeExpr> {
        node.child_by_field_name(field)
            .and_then(|ty| self.type_expr(ty))
    }

    fn is_exported(&self, node: Node<'_>) -> bool {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .any(|child| child.kind() == "visibility_modifier")
    }

    fn signature_parts(&self, node: Node<'_>) -> (Vec<ParamDecl>, bool, bool, bool) {
        let mut params = Vec::new();
        let mut takes_self = false;
        let mut shared = false;
        let mut by_value = false;
        if let Some(parameters) = node.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            let children: Vec<Node<'_>> = parameters.named_children(&mut cursor).collect();
            for child in children {
                match child.kind() {
                    "self_parameter" => {
                        takes_self = true;
                        let (is_shared, is_ref) = self.self_reference_kind(child);
                        shared = is_shared;
                        by_value = !is_ref;
                    }
                    "parameter" => params.push(ParamDecl {
                        name: child
                            .child_by_field_name("pattern")
                            .map(|pattern| self.pattern_name(pattern))
                            .unwrap_or_default(),
                        ty: child
                            .child_by_field_name("type")
                            .and_then(|ty| self.type_expr(ty)),
                    }),
                    _ => {}
                }
            }
        }
        (params, takes_self, shared, by_value)
    }

    fn signature(&self, node: Node<'_>, params: &[ParamDecl]) -> FunctionSig {
        let return_type = node
            .child_by_field_name("return_type")
            .and_then(|ty| self.type_expr(ty));
        FunctionSig {
            params: params.to_vec(),
            return_type,
        }
    }

    fn self_reference_kind(&self, node: Node<'_>) -> (bool, bool) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        let is_ref = children.iter().any(|child| child.kind() == "&");
        let is_mut = children
            .iter()
            .any(|child| child.kind() == "mutable_specifier");
        (is_ref && !is_mut, is_ref)
    }

    fn pattern_name(&self, node: Node<'_>) -> String {
        match node.kind() {
            "identifier" | "self" | "metavariable" => self.text(node).to_owned(),
            "mut_pattern" | "ref_pattern" | "captured_pattern" | "reference_pattern" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .next()
                    .map(|inner| self.pattern_name(inner))
                    .unwrap_or_else(|| self.text(node).trim().to_owned())
            }
            _ => self.text(node).trim().to_owned(),
        }
    }

    // -------------------------------------------------------------- imports

    fn emit_imports(&mut self, imports: &[Node<'_>]) {
        let mut imports = imports.to_vec();
        imports.sort_by_key(|node| node.start_byte());
        for node in imports {
            let at = node.start_byte();
            if self.scope.is_skipped_at(at) {
                continue;
            }
            let Some(argument) = node.child_by_field_name("argument") else {
                continue;
            };
            let scope = self.scope.module_at(at);
            let reexport = self.is_exported(node);
            self.flatten_use(argument, Vec::new(), scope, reexport);
        }
    }

    fn flatten_use(
        &mut self,
        clause: Node<'_>,
        prefix: Vec<String>,
        scope: ModuleScope,
        reexport: bool,
    ) {
        match clause.kind() {
            "use_list" => {
                let mut cursor = clause.walk();
                let children: Vec<Node<'_>> = clause.named_children(&mut cursor).collect();
                for child in children {
                    self.flatten_use(child, prefix.clone(), scope, reexport);
                }
            }
            "scoped_use_list" => {
                let mut path = prefix;
                if let Some(node) = clause.child_by_field_name("path") {
                    path.extend(self.expr_path(node));
                }
                if let Some(list) = clause.child_by_field_name("list") {
                    self.flatten_use(list, path, scope, reexport);
                }
            }
            "use_as_clause" => {
                let (Some(node), Some(alias)) = (
                    clause.child_by_field_name("path"),
                    clause.child_by_field_name("alias"),
                ) else {
                    return;
                };
                let mut path = prefix;
                path.extend(self.expr_path(node));
                let alias = self.text(alias).to_owned();
                let leaf = path.last().cloned().unwrap_or_default();
                let kind = if leaf == "self" {
                    path.pop();
                    ImportKind::Namespace { alias }
                } else {
                    ImportKind::Named {
                        name: leaf,
                        alias: Some(alias),
                    }
                };
                self.push_import(scope, path, kind, reexport);
            }
            "use_wildcard" => {
                let mut path = prefix;
                if let Some(node) = clause.named_child(0) {
                    path.extend(self.expr_path(node));
                }
                self.push_import(scope, path, ImportKind::Glob, reexport);
            }
            "identifier"
            | "self"
            | "super"
            | "crate"
            | "scoped_identifier"
            | "scoped_type_identifier" => {
                let mut path = prefix;
                path.extend(self.expr_path(clause));
                let leaf = path.last().cloned().unwrap_or_default();
                let kind = if leaf == "self" {
                    path.pop();
                    ImportKind::Namespace {
                        alias: path.last().cloned().unwrap_or_default(),
                    }
                } else {
                    ImportKind::Namespace { alias: leaf }
                };
                self.push_import(scope, path, kind, reexport);
            }
            _ => {}
        }
    }

    fn push_import(
        &mut self,
        scope: ModuleScope,
        path_segments: Vec<String>,
        kind: ImportKind,
        is_reexport: bool,
    ) {
        if path_segments.is_empty() {
            return;
        }
        self.summary.imports.push(RawImport {
            scope,
            path_segments,
            kind,
            is_reexport,
        });
    }
}

#[cfg(test)]
mod tests;
