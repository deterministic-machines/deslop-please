//! Tree-sitter `.scm` driven extraction into a lifetime-free [`FileSummary`].
//!
//! `queries.scm` is compiled once per grammar; its captures are bucketed, the
//! declarations become `ItemDecl`s, and `ScopeIndex` supplies the enclosing
//! holder and parent for every reference and function-body fact.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::LazyLock;

use tree_sitter::{Node, Parser, Tree};

use super::is_tsx_file;
use super::syntax::{
    Syntax, has_token, is_call, is_declaration_name, is_new_constructor, named_children,
    single_call,
};
use crate::model::ItemKind;
use crate::treesitter::lang::{DiscoveredFile, Language};
use crate::treesitter::query::{CompiledQuery, ScopeIndex, line_count, node_line};
use crate::treesitter::summary::{
    AccessMode, AccessorCandidate, AccessorFieldOp, CalleeExpr, FileSummary, ForwardingCandidate,
    FunctionBodySummary, FunctionSig, ImportKind, ItemDecl, LocalBinding, LocalDeclId, ModuleScope,
    ParamDecl, RawImport, RawRef, RawRefKind, ReceiverExpr,
};

const QUERIES: &str = include_str!("queries.scm");
const MUTATING: &[&str] = &[
    "push", "pop", "shift", "unshift", "splice", "sort", "reverse", "fill", "set", "delete",
    "clear", "add",
];

static TYPESCRIPT_QUERY: LazyLock<CompiledQuery> = LazyLock::new(|| {
    CompiledQuery::new(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), QUERIES)
});
static TSX_QUERY: LazyLock<CompiledQuery> =
    LazyLock::new(|| CompiledQuery::new(&tree_sitter_typescript::LANGUAGE_TSX.into(), QUERIES));

fn parse(file: &DiscoveredFile, source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    let language = if is_tsx_file(file.rel_path.rsplit('/').next().unwrap_or("")) {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    parser
        .set_language(&language)
        .expect("the bundled TypeScript grammar loads");
    parser.parse(source, None)
}

fn empty_summary(file: &DiscoveredFile, lines: usize) -> FileSummary {
    FileSummary {
        file_path: file.rel_path.clone(),
        module_id: file.module_id,
        language: Language::TypeScript,
        is_test: file.is_test,
        lines,
        inline_modules: Vec::new(),
        imports: Vec::new(),
        decls: Vec::new(),
        refs: Vec::new(),
    }
}

/// Capture buckets, filled by one pass of the compiled query.
#[derive(Default)]
struct Matches<'t> {
    buckets: HashMap<&'static str, Vec<Node<'t>>>,
    /// `(declarator, function, source)` of a CommonJS `require` binding.
    requires: Vec<(Node<'t>, Node<'t>, Node<'t>)>,
}

impl<'t> Matches<'t> {
    fn nodes(&self, name: &str) -> &[Node<'t>] {
        self.buckets.get(name).map_or(&[], Vec::as_slice)
    }

    fn requires(&self) -> &[(Node<'t>, Node<'t>, Node<'t>)] {
        &self.requires
    }
}

fn capture_bucket(name: &str) -> Option<&'static str> {
    Some(match name {
        "import" => "import",
        "export" => "export",
        "decl" => "decl",
        "default_function" => "default_function",
        "default_class" => "default_class",
        "field" => "field",
        "method" => "method",
        "param_property" => "param_property",
        "enum_member" => "enum_member",
        "function_body" => "function_body",
        "type_ref" => "type_ref",
        "type_path_ref" => "type_path_ref",
        "new" => "new",
        "call" => "call",
        "member" => "member",
        "value_ref" => "value_ref",
        "local" => "local",
        "computes" => "computes",
        "unary" => "unary",
        _ => return None,
    })
}

fn collect_matches<'t>(query: &CompiledQuery, root: Node<'t>, source: &str) -> Matches<'t> {
    let mut m = Matches::default();
    query.for_each_match(root, source.as_bytes(), |view| {
        for (name, node) in view.captures() {
            if name == "cjs.declarator" {
                if let (Some(function), Some(source)) =
                    (view.node("cjs.function"), view.node("cjs.source"))
                {
                    m.requires.push((node, function, source));
                }
            } else if let Some(key) = capture_bucket(name) {
                m.buckets.entry(key).or_default().push(node);
            }
        }
    });
    for bucket in m.buckets.values_mut() {
        bucket.sort_by_key(|node| node.start_byte());
    }
    m
}

pub(super) fn summarize(file: &DiscoveredFile, source: &str) -> FileSummary {
    let Some(tree) = parse(file, source) else {
        return empty_summary(file, line_count(source));
    };
    let tsx = is_tsx_file(file.rel_path.rsplit('/').next().unwrap_or(""));
    let query: &CompiledQuery = if tsx { &TSX_QUERY } else { &TYPESCRIPT_QUERY };
    let matches = collect_matches(query, tree.root_node(), source);
    let mut ex = Extractor {
        syntax: Syntax::new(source),
        file,
        summary: empty_summary(file, line_count(source)),
        index: ScopeIndex::new(),
        bodies: Vec::new(),
        body_ranges: HashMap::new(),
        declared_bodies: HashSet::new(),
        methods: HashMap::new(),
        facts: HashMap::new(),
        refs: Vec::new(),
        imports: Vec::new(),
    };
    ex.declarations(&matches);
    let mut imports = std::mem::take(&mut ex.imports);
    imports.sort_by_key(|(offset, _)| *offset);
    ex.summary.imports = imports.into_iter().map(|(_, import)| import).collect();
    ex.skip_nested_bodies(&matches);
    ex.index.finalize();
    ex.references_and_facts(&matches);
    ex.finish_bodies();
    let Extractor {
        mut summary,
        mut refs,
        ..
    } = ex;
    refs.sort_by_key(|(offset, _)| *offset);
    summary.refs = refs.into_iter().map(|(_, reference)| reference).collect();
    summary
}

#[derive(Clone, Copy)]
struct MethodFlags {
    is_static: bool,
    getter: bool,
    setter: bool,
}
struct FieldOp {
    field: String,
    write: bool,
    returned: bool,
}

#[derive(Default)]
struct BodyFacts {
    locals: Vec<LocalBinding>,
    ops: bool,
    calls: usize,
    reaches: bool,
    mutated: bool,
    field_ops: Vec<FieldOp>,
}

struct Extractor<'a, 't> {
    syntax: Syntax<'a>,
    file: &'a DiscoveredFile,
    summary: FileSummary,
    index: ScopeIndex,
    bodies: Vec<(LocalDeclId, Node<'t>)>,
    body_ranges: HashMap<LocalDeclId, Range<usize>>,
    declared_bodies: HashSet<usize>,
    methods: HashMap<LocalDeclId, MethodFlags>,
    facts: HashMap<LocalDeclId, BodyFacts>,
    refs: Vec<(usize, RawRef)>,
    /// Imports with the offset they are written at, so they come out in
    /// source order even when a `require` is found apart from a `use`.
    imports: Vec<(usize, RawImport)>,
}

impl<'a, 't> Extractor<'a, 't> {
    fn text(&self, node: Node) -> &'a str {
        self.syntax.text(node)
    }

    fn push_decl(
        &mut self,
        name: String,
        kind: ItemKind,
        parent: Option<LocalDeclId>,
    ) -> LocalDeclId {
        let id = LocalDeclId(
            u32::try_from(self.summary.decls.len()).expect("fewer than 2^32 declarations"),
        );
        self.summary.decls.push(ItemDecl {
            local_id: id,
            scope: ModuleScope::FileRoot,
            name,
            kind,
            parent_decl: parent,
            impl_target: None,
            impl_trait: None,
            is_exported: false,
            is_default_export: false,
            trait_impl: false,
            takes_self: false,
            self_is_shared_ref: false,
            holds_nothing: false,
            is_test: self.file.is_test,
            type_annotation: None,
            signature: None,
            body_summary: None,
        });
        id
    }

    fn decl_mut(&mut self, id: LocalDeclId) -> &mut ItemDecl {
        &mut self.summary.decls[id.0 as usize]
    }

    fn add_body(&mut self, id: LocalDeclId, body: Node<'t>) {
        self.bodies.push((id, body));
        self.body_ranges.insert(id, body.byte_range());
        self.declared_bodies.insert(body.start_byte());
    }

    fn push_import(&mut self, node: Node, specifier: String, kind: ImportKind, reexport: bool) {
        let path_segments = specifier
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        self.imports.push((
            node.start_byte(),
            RawImport {
                scope: ModuleScope::FileRoot,
                path_segments,
                raw_specifier: Some(specifier),
                kind,
                is_reexport: reexport,
            },
        ));
    }

    fn push_ref(&mut self, node: Node, kind: RawRefKind, access: AccessMode) {
        let holder = self.index.holder_at(node.start_byte());
        let reference = RawRef {
            line: node_line(node),
            scope: ModuleScope::FileRoot,
            holder,
            kind,
            access,
            is_test: self.file.is_test,
        };
        self.refs.push((node.start_byte(), reference));
    }

    // ----- declarations ----------------------------------------------------

    fn declarations(&mut self, m: &Matches<'t>) {
        for &node in m.nodes("decl") {
            match node.kind() {
                "class_declaration" | "abstract_class_declaration" => {
                    self.container(node, true, true)
                }
                "enum_declaration" => self.container(node, true, false),
                "interface_declaration"
                | "type_alias_declaration"
                | "internal_module"
                | "module" => self.container(node, false, false),
                "function_declaration" | "generator_function_declaration" => {
                    self.function(node, false)
                }
                "lexical_declaration" | "variable_declaration" => self.variables(node),
                _ => {}
            }
        }
        self.index.finalize();
        for &node in m.nodes("enum_member") {
            self.enum_member(node);
        }
        for &node in m.nodes("field") {
            self.field(node);
        }
        for &node in m.nodes("param_property") {
            self.param_property(node);
        }
        for &node in m.nodes("method") {
            self.method(node);
        }
        for &node in m.nodes("default_function") {
            self.function(node, true);
        }
        for &node in m.nodes("default_class") {
            self.default_class(node);
        }
        for &(declarator, function, source) in m.requires() {
            self.require(declarator, function, source);
        }
        for &node in m.nodes("import") {
            self.import(node);
        }
        for &node in m.nodes("export") {
            self.export(node);
        }
    }

    fn container(&mut self, node: Node, class: bool, variant_owner: bool) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let (exported, default) = self.export_flags(node);
        let id = self.push_decl(self.syntax.property_name(name), ItemKind::Type, None);
        let decl = self.decl_mut(id);
        decl.is_exported = exported;
        decl.is_default_export = default;
        self.index.add_holder(node.byte_range(), id);
        if class || variant_owner {
            self.index.add_parent(node.byte_range(), id);
        }
    }

    fn default_class(&mut self, node: Node) {
        let name = node
            .child_by_field_name("name")
            .map(|n| self.syntax.property_name(n))
            .filter(|n| !n.is_empty());
        let id = self.push_decl(
            name.unwrap_or_else(|| "default".to_owned()),
            ItemKind::Type,
            None,
        );
        let decl = self.decl_mut(id);
        decl.is_exported = true;
        decl.is_default_export = true;
        self.index.add_holder(node.byte_range(), id);
    }

    fn enum_member(&mut self, node: Node) {
        let Some(parent) = self.index.parent_decl_of(&node.byte_range()) else {
            return;
        };
        let name = if node.kind() == "enum_assignment" {
            node.child_by_field_name("name")
                .map(|n| self.syntax.property_name(n))
        } else {
            Some(self.syntax.property_name(node))
        };
        if let Some(name) = name {
            self.push_decl(name, ItemKind::Variant, Some(parent));
        }
    }

    fn field(&mut self, node: Node) {
        let Some(parent) = self.index.parent_decl_of(&node.byte_range()) else {
            return;
        };
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let ty = node
            .child_by_field_name("type")
            .and_then(|t| self.syntax.type_expression(t));
        let id = self.push_decl(
            self.syntax.property_name(name),
            ItemKind::Field,
            Some(parent),
        );
        self.decl_mut(id).type_annotation = ty;
        self.index.add_holder(node.byte_range(), id);
    }

    fn param_property(&mut self, node: Node) {
        let Some(parent) = self.index.parent_decl_of(&node.byte_range()) else {
            return;
        };
        let Some(pattern) = node.child_by_field_name("pattern") else {
            return;
        };
        if pattern.kind() != "identifier" {
            return;
        }
        let ty = node
            .child_by_field_name("type")
            .and_then(|t| self.syntax.type_expression(t));
        let id = self.push_decl(self.text(pattern).to_owned(), ItemKind::Field, Some(parent));
        self.decl_mut(id).type_annotation = ty;
        self.index.add_holder(node.byte_range(), id);
    }

    fn method(&mut self, node: Node<'t>) {
        let Some(parent) = self.index.parent_decl_of(&node.byte_range()) else {
            return;
        };
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let flags = MethodFlags {
            is_static: has_token(node, "static"),
            getter: has_token(node, "get"),
            setter: has_token(node, "set"),
        };
        let signature = self.signature_of(node);
        let id = self.push_decl(
            self.syntax.property_name(name),
            ItemKind::Function,
            Some(parent),
        );
        let decl = self.decl_mut(id);
        decl.takes_self = !flags.is_static;
        decl.signature = Some(signature);
        self.methods.insert(id, flags);
        self.index.add_holder(node.byte_range(), id);
        if node.kind() == "method_definition"
            && let Some(body) = node.child_by_field_name("body")
        {
            self.add_body(id, body);
        }
    }

    fn function(&mut self, node: Node<'t>, default: bool) {
        let name = if default {
            "default".to_owned()
        } else {
            node.child_by_field_name("name")
                .map(|n| self.syntax.property_name(n))
                .unwrap_or_default()
        };
        if name.is_empty() {
            return;
        }
        let (exported, exported_default) = self.export_flags(node);
        let signature = self.signature_of(node);
        let id = self.push_decl(name, ItemKind::Function, None);
        let decl = self.decl_mut(id);
        decl.is_exported = exported || default;
        decl.is_default_export = exported_default || default;
        decl.signature = Some(signature);
        self.index.add_holder(node.byte_range(), id);
        if let Some(body) = node.child_by_field_name("body") {
            self.add_body(id, body);
        }
    }

    fn variables(&mut self, node: Node<'t>) {
        let (exported, default) = self.export_flags(node);
        for declarator in named_children(node) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            if let Some(value) = declarator.child_by_field_name("value") {
                if self.syntax.is_require_call(value) {
                    continue;
                }
                if matches!(
                    value.kind(),
                    "arrow_function" | "function_expression" | "generator_function"
                ) {
                    if name_node.kind() == "identifier" {
                        let signature = self.signature_of(value);
                        let id = self.push_decl(
                            self.text(name_node).to_owned(),
                            ItemKind::Function,
                            None,
                        );
                        let decl = self.decl_mut(id);
                        decl.is_exported = exported;
                        decl.is_default_export = default;
                        decl.signature = Some(signature);
                        self.index.add_holder(value.byte_range(), id);
                        if let Some(body) = value.child_by_field_name("body") {
                            self.add_body(id, body);
                        }
                    }
                    continue;
                }
            }
            let ty = declarator
                .child_by_field_name("type")
                .and_then(|t| self.syntax.type_expression(t));
            for name in self.syntax.binding_names(name_node) {
                let id = self.push_decl(name, ItemKind::Constant, None);
                let decl = self.decl_mut(id);
                decl.is_exported = exported;
                decl.is_default_export = default;
                decl.type_annotation = ty.clone();
            }
        }
    }

    fn skip_nested_bodies(&mut self, m: &Matches<'t>) {
        for &body in m.nodes("function_body") {
            if !self.declared_bodies.contains(&body.start_byte()) {
                self.index.add_skipped_span(body.byte_range());
            }
        }
    }

    // ----- references and function-body facts ------------------------------

    fn references_and_facts(&mut self, m: &Matches<'t>) {
        for &node in m.nodes("local") {
            self.local(node);
        }
        for &node in m.nodes("computes") {
            self.mark_op(node);
        }
        for &node in m.nodes("unary") {
            if node
                .child_by_field_name("operator")
                .is_some_and(|op| matches!(self.text(op), "-" | "+" | "~" | "!" | "delete"))
            {
                self.mark_op(node);
            }
        }
        for &node in m.nodes("call") {
            self.count_call(node);
            self.call_ref(node);
        }
        for &node in m.nodes("new") {
            self.count_call(node);
            self.new_ref(node);
        }
        for &node in m.nodes("member") {
            self.member(node);
        }
        for &node in m.nodes("type_ref") {
            self.type_ref(node);
        }
        for &node in m.nodes("type_path_ref") {
            self.path_ref(node);
        }
        for &node in m.nodes("value_ref") {
            self.path_ref(node);
        }
    }

    fn local(&mut self, declarator: Node) {
        let Some(owner) = self.body_owner(declarator) else {
            return;
        };
        let Some(name) = declarator.child_by_field_name("name") else {
            return;
        };
        if name.kind() != "identifier" {
            return;
        }
        let name = self.text(name).to_owned();
        let declared_type = declarator
            .child_by_field_name("type")
            .and_then(|t| self.syntax.type_expression(t));
        let init = declarator
            .child_by_field_name("value")
            .and_then(|v| self.syntax.init_expr(v));
        self.facts
            .entry(owner)
            .or_default()
            .locals
            .push(LocalBinding {
                name,
                declared_type,
                init,
            });
    }

    fn mark_op(&mut self, node: Node) {
        if let Some(owner) = self.body_owner(node) {
            self.facts.entry(owner).or_default().ops = true;
        }
    }

    fn count_call(&mut self, node: Node) {
        if let Some(owner) = self.body_owner(node) {
            self.facts.entry(owner).or_default().calls += 1;
        }
    }

    /// The function whose body contains `node`, unless `node` is in a nested body.
    fn body_owner(&self, node: Node) -> Option<LocalDeclId> {
        let id = self.index.holder_at(node.start_byte())?;
        let range = self.body_ranges.get(&id)?;
        (!self.index.is_skipped_at(node.start_byte()) && range.contains(&node.start_byte()))
            .then_some(id)
    }

    fn member(&mut self, node: Node) {
        let inner = node.parent().is_some_and(|parent| {
            parent.kind() == "member_expression"
                && parent
                    .child_by_field_name("object")
                    .is_some_and(|o| o.id() == node.id())
        });
        if inner || is_new_constructor(node) {
            return;
        }
        let (Some(property), Some(object)) = (
            node.child_by_field_name("property"),
            node.child_by_field_name("object"),
        ) else {
            return;
        };
        let receiver = self.syntax.receiver_of(object);
        let member_name = self.syntax.property_name(property);
        let is_call = is_call(node);
        let write = self.syntax.in_write_position(node);
        self.member_facts(node, &receiver, &member_name, is_call, write);
        if is_call
            && let ReceiverExpr::Path(path) = &receiver
            && path.len() == 1
            && self.syntax.is_namespace(&path[0])
        {
            let mut path = path.clone();
            path.push(member_name);
            self.push_ref(node, RawRefKind::Path(path), AccessMode::CopyOrRead);
            return;
        }
        let access = if write {
            AccessMode::Work
        } else if is_call && !MUTATING.contains(&member_name.as_str()) {
            AccessMode::MethodCall
        } else if is_call {
            AccessMode::Work
        } else {
            AccessMode::CopyOrRead
        };
        self.push_ref(
            node,
            RawRefKind::MemberAccess {
                receiver,
                member: member_name,
                is_call,
            },
            access,
        );
    }

    fn member_facts(
        &mut self,
        node: Node,
        receiver: &ReceiverExpr,
        member: &str,
        is_call: bool,
        write: bool,
    ) {
        let chain = match receiver {
            ReceiverExpr::SelfValue => vec![member.to_owned()],
            ReceiverExpr::FieldChain { base, fields }
                if matches!(**base, ReceiverExpr::SelfValue) =>
            {
                let mut chain = fields.clone();
                chain.push(member.to_owned());
                chain
            }
            _ => return,
        };
        let Some(owner) = self.body_owner(node) else {
            return;
        };
        let returned = !is_call && !write && self.syntax.in_return_position(node);
        let param = !is_call && write && chain.len() == 1 && self.assigned_param(node, owner);
        let facts = self.facts.entry(owner).or_default();
        if is_call {
            facts.mutated |= MUTATING.contains(&member);
            facts.reaches = true;
        } else if write {
            facts.mutated = true;
            if param {
                facts.field_ops.push(FieldOp {
                    field: chain[0].clone(),
                    write: true,
                    returned: false,
                });
            } else if chain.len() > 1 {
                facts.reaches = true;
            }
        } else if chain.len() == 1 {
            facts.field_ops.push(FieldOp {
                field: chain[0].clone(),
                write: false,
                returned,
            });
        } else {
            facts.reaches = true;
        }
    }

    fn type_ref(&mut self, node: Node) {
        if is_declaration_name(node)
            || node
                .parent()
                .is_some_and(|p| matches!(p.kind(), "nested_type_identifier" | "type_parameter"))
        {
            return;
        }
        self.push_ref(
            node,
            RawRefKind::Path(vec![self.text(node).to_owned()]),
            AccessMode::CopyOrRead,
        );
    }

    fn path_ref(&mut self, node: Node) {
        let path = if node.kind() == "identifier" {
            vec![self.text(node).to_owned()]
        } else {
            self.syntax.path_segments(node)
        };
        self.push_ref(node, RawRefKind::Path(path), AccessMode::CopyOrRead);
    }

    fn call_ref(&mut self, node: Node) {
        let Some(function) = node.child_by_field_name("function") else {
            return;
        };
        if function.kind() == "identifier" {
            let name = self.text(function);
            if !matches!(name, "require" | "import") {
                self.push_ref(
                    function,
                    RawRefKind::Path(vec![name.to_owned()]),
                    AccessMode::CopyOrRead,
                );
            }
        } else if function.kind() == "nested_identifier" {
            self.push_ref(
                function,
                RawRefKind::Path(self.syntax.path_segments(function)),
                AccessMode::CopyOrRead,
            );
        }
    }

    fn new_ref(&mut self, node: Node) {
        let Some(constructor) = node.child_by_field_name("constructor") else {
            return;
        };
        let path = self.syntax.path_segments(constructor);
        if !path.is_empty() {
            self.push_ref(node, RawRefKind::Path(path), AccessMode::CopyOrRead);
        }
    }

    fn finish_bodies(&mut self) {
        for (id, body) in std::mem::take(&mut self.bodies) {
            let facts = self.facts.remove(&id).unwrap_or_default();
            let params: Vec<String> = self.summary.decls[id.0 as usize]
                .signature
                .as_ref()
                .map(|s| s.params.iter().map(|p| p.name.clone()).collect())
                .unwrap_or_default();
            let forwarding = self.forwarding_candidate(body, &params);
            let computes = if forwarding.is_some() {
                facts.ops
            } else {
                facts.ops || facts.reaches || facts.calls > 0
            };
            let takes_self = self.summary.decls[id.0 as usize].takes_self;
            let flags = self.methods.get(&id).copied();
            let self_is_shared_ref = if !takes_self {
                false
            } else if flags.is_some_and(|f| f.getter) {
                true
            } else if flags.is_some_and(|f| f.setter) {
                false
            } else {
                !facts.mutated
            };
            let accessor = if takes_self && !computes {
                Self::accessor_candidate(&facts)
            } else {
                None
            };
            let decl = &mut self.summary.decls[id.0 as usize];
            decl.self_is_shared_ref = self_is_shared_ref;
            decl.body_summary = Some(FunctionBodySummary {
                locals: facts.locals,
                computes,
                forwarding_candidate: forwarding,
                accessor_candidate: accessor,
            });
        }
    }

    fn accessor_candidate(facts: &BodyFacts) -> Option<AccessorCandidate> {
        if !facts.field_ops.iter().any(|op| op.write || op.returned) {
            return None;
        }
        let mut ops: Vec<AccessorFieldOp> = Vec::new();
        for op in &facts.field_ops {
            // TypeScript has no borrow syntax: only a write is lent or written.
            let lent = op.write;
            if let Some(existing) = ops.iter_mut().find(|e| e.field_name == op.field) {
                existing.is_lent_or_written |= lent;
                existing.is_write |= op.write;
            } else {
                ops.push(AccessorFieldOp {
                    field_name: op.field.clone(),
                    is_lent_or_written: lent,
                    is_write: op.write,
                });
            }
        }
        Some(AccessorCandidate { ops })
    }

    fn forwarding_candidate(&self, body: Node, params: &[String]) -> Option<ForwardingCandidate> {
        let call = single_call(body)?;
        let function = call.child_by_field_name("function")?;
        let arguments = call.child_by_field_name("arguments")?;
        for argument in named_children(arguments) {
            if argument.kind() != "identifier" || !params.iter().any(|p| p == self.text(argument)) {
                return None;
            }
        }
        let (callee, self_fields) = match function.kind() {
            "identifier" => (
                CalleeExpr::Path(vec![self.syntax.property_name(function)]),
                Vec::new(),
            ),
            "nested_identifier" => (
                CalleeExpr::Path(self.syntax.path_segments(function)),
                Vec::new(),
            ),
            "member_expression" => {
                let receiver = self
                    .syntax
                    .receiver_of(function.child_by_field_name("object")?);
                let self_fields = match &receiver {
                    ReceiverExpr::SelfValue => Vec::new(),
                    ReceiverExpr::FieldChain { base, fields }
                        if matches!(**base, ReceiverExpr::SelfValue) =>
                    {
                        fields.clone()
                    }
                    ReceiverExpr::Local(name) if params.iter().any(|p| p == name) => Vec::new(),
                    _ => return None,
                };
                (
                    CalleeExpr::Method {
                        receiver,
                        method: self
                            .syntax
                            .property_name(function.child_by_field_name("property")?),
                    },
                    self_fields,
                )
            }
            _ => return None,
        };
        Some(ForwardingCandidate {
            callee,
            self_fields,
        })
    }

    // ----- imports and exports --------------------------------------------

    fn import(&mut self, node: Node) {
        let Some(specifier) = node
            .child_by_field_name("source")
            .and_then(|s| self.syntax.string_value(s))
        else {
            return;
        };
        let mut found = false;
        for child in named_children(node) {
            match child.kind() {
                "import_clause" => found |= self.import_clause(node, &specifier, child),
                "import_require_clause" => {
                    if let Some(alias) = child.named_child(0) {
                        found = true;
                        let kind = ImportKind::Default {
                            alias: self.text(alias).to_owned(),
                        };
                        self.push_import(node, specifier.clone(), kind, false);
                    }
                }
                _ => {}
            }
        }
        if !found {
            self.push_import(node, specifier, ImportKind::SideEffect, false);
        }
    }

    fn import_clause(&mut self, node: Node, specifier: &str, clause: Node) -> bool {
        let mut found = false;
        for child in named_children(clause) {
            match child.kind() {
                "identifier" => {
                    found = true;
                    let kind = ImportKind::Default {
                        alias: self.text(child).to_owned(),
                    };
                    self.push_import(node, specifier.to_owned(), kind, false);
                }
                "namespace_import" => {
                    if let Some(alias) = child.named_child(0) {
                        found = true;
                        let alias = self.text(alias).to_owned();
                        self.syntax.add_namespace(alias.clone());
                        self.push_import(
                            node,
                            specifier.to_owned(),
                            ImportKind::Namespace { alias },
                            false,
                        );
                    }
                }
                "named_imports" => {
                    for spec in named_children(child)
                        .into_iter()
                        .filter(|s| s.kind() == "import_specifier")
                    {
                        let Some(name) = spec.child_by_field_name("name") else {
                            continue;
                        };
                        found = true;
                        let name = self.syntax.property_name(name);
                        let alias = spec
                            .child_by_field_name("alias")
                            .map(|a| self.syntax.property_name(a));
                        self.push_import(
                            node,
                            specifier.to_owned(),
                            ImportKind::Named { name, alias },
                            false,
                        );
                    }
                }
                _ => {}
            }
        }
        found
    }

    fn require(&mut self, declarator: Node, function: Node, source: Node) {
        if self.text(function) != "require" {
            return;
        }
        let Some(specifier) = self.syntax.string_value(source) else {
            return;
        };
        let Some(name_node) = declarator.child_by_field_name("name") else {
            return;
        };
        if name_node.kind() == "identifier" {
            let kind = ImportKind::Default {
                alias: self.text(name_node).to_owned(),
            };
            self.push_import(declarator, specifier, kind, false);
            return;
        }
        if name_node.kind() != "object_pattern" {
            return;
        }
        for child in named_children(name_node) {
            let kind = match child.kind() {
                "shorthand_property_identifier_pattern" => ImportKind::Named {
                    name: self.text(child).to_owned(),
                    alias: None,
                },
                "pair_pattern" => {
                    let Some(key) = child.child_by_field_name("key") else {
                        continue;
                    };
                    ImportKind::Named {
                        name: self.syntax.property_name(key),
                        alias: child
                            .child_by_field_name("value")
                            .map(|v| self.text(v).to_owned()),
                    }
                }
                _ => continue,
            };
            self.push_import(declarator, specifier.clone(), kind, false);
        }
    }

    fn export(&mut self, node: Node) {
        if node.child_by_field_name("source").is_some() {
            return self.reexport(node);
        }
        if let Some(value) = node.child_by_field_name("value") {
            if value.kind() == "identifier" {
                let name = self.text(value).to_owned();
                self.export_local(&name, true);
            }
            return;
        }
        for clause in named_children(node)
            .into_iter()
            .filter(|c| c.kind() == "export_clause")
        {
            for spec in named_children(clause)
                .into_iter()
                .filter(|s| s.kind() == "export_specifier")
            {
                let Some(name) = spec.child_by_field_name("name") else {
                    continue;
                };
                let name = self.syntax.property_name(name);
                let default = spec
                    .child_by_field_name("alias")
                    .is_some_and(|a| self.syntax.property_name(a) == "default");
                self.export_local(&name, default);
            }
        }
    }

    fn reexport(&mut self, node: Node) {
        let Some(specifier) = node
            .child_by_field_name("source")
            .and_then(|s| self.syntax.string_value(s))
        else {
            return;
        };
        let mut glob = true;
        for child in named_children(node) {
            match child.kind() {
                "export_clause" => {
                    glob = false;
                    for spec in named_children(child)
                        .into_iter()
                        .filter(|s| s.kind() == "export_specifier")
                    {
                        let Some(name) = spec.child_by_field_name("name") else {
                            continue;
                        };
                        let name = self.syntax.property_name(name);
                        let alias = spec
                            .child_by_field_name("alias")
                            .map(|a| self.syntax.property_name(a));
                        self.push_import(
                            node,
                            specifier.clone(),
                            ImportKind::Named { name, alias },
                            true,
                        );
                    }
                }
                "namespace_export" => {
                    glob = false;
                    if let Some(alias) = child.named_child(0) {
                        let alias = self.syntax.property_name(alias);
                        self.push_import(
                            node,
                            specifier.clone(),
                            ImportKind::Namespace { alias },
                            true,
                        );
                    }
                }
                _ => {}
            }
        }
        if glob {
            self.push_import(node, specifier, ImportKind::Glob, true);
        }
    }

    fn export_local(&mut self, name: &str, default: bool) {
        if let Some(decl) = self
            .summary
            .decls
            .iter_mut()
            .rev()
            .find(|d| d.parent_decl.is_none() && d.name == name)
        {
            decl.is_exported = true;
            decl.is_default_export |= default;
        }
    }

    fn export_flags(&self, node: Node) -> (bool, bool) {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return (false, false);
            };
            match parent.kind() {
                "ambient_declaration" => current = parent,
                "export_statement" => return (true, has_token(parent, "default")),
                _ => return (false, false),
            }
        }
    }

    fn assigned_param(&self, node: Node, owner: LocalDeclId) -> bool {
        let params = self.summary.decls[owner.0 as usize]
            .signature
            .as_ref()
            .map(|s| s.params.as_slice())
            .unwrap_or_default();
        self.syntax.assigned_param(node, params)
    }

    fn signature_of(&self, node: Node) -> FunctionSig {
        let mut params = Vec::new();
        if let Some(parameters) = node.child_by_field_name("parameters") {
            for parameter in named_children(parameters) {
                if !matches!(
                    parameter.kind(),
                    "required_parameter" | "optional_parameter"
                ) {
                    continue;
                }
                let name_node = parameter
                    .child_by_field_name("pattern")
                    .or_else(|| parameter.child_by_field_name("name"));
                let Some(name_node) = name_node else { continue };
                let name = match name_node.kind() {
                    "identifier" => self.text(name_node).to_owned(),
                    "rest_pattern" => name_node
                        .named_child(0)
                        .map(|n| self.text(n).to_owned())
                        .unwrap_or_default(),
                    _ => continue,
                };
                if name.is_empty() {
                    continue;
                }
                let ty = parameter
                    .child_by_field_name("type")
                    .and_then(|t| self.syntax.type_expression(t));
                params.push(ParamDecl { name, ty });
            }
        } else if let Some(parameter) = node.child_by_field_name("parameter")
            && parameter.kind() == "identifier"
        {
            params.push(ParamDecl {
                name: self.text(parameter).to_owned(),
                ty: None,
            });
        }
        let return_type = node
            .child_by_field_name("return_type")
            .and_then(|t| self.syntax.type_expression(t));
        FunctionSig {
            params,
            return_type,
        }
    }
}

#[cfg(test)]
mod tests;
