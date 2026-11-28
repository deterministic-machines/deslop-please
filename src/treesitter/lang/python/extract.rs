//! Tree-sitter .scm driven extraction into a lifetime-free FileSummary.
//!
//! queries.scm is compiled once; its captures are bucketed, the classes,
//! functions and assignments that sit directly in a module or class body become
//! ItemDecls, and ScopeIndex supplies the enclosing holder for every reference
//! and function-body fact.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use tree_sitter::{Node, Parser, Tree};

use super::syntax::{Syntax, named_children, nodes_by_field, single_call, starts_uppercase};
use crate::model::ItemKind;
use crate::treesitter::lang::{DiscoveredFile, Language};
use crate::treesitter::query::{CompiledQuery, ScopeIndex, line_count, node_line};
use crate::treesitter::summary::{
    AccessMode, AccessorCandidate, AccessorFieldOp, CalleeExpr, FileSummary, ForwardingCandidate,
    FunctionBodySummary, FunctionSig, ImportKind, ItemDecl, LocalBinding, LocalDeclId, ModuleScope,
    ParamDecl, RawImport, RawRef, RawRefKind, ReceiverExpr, TypeExpr,
};

const QUERIES: &str = include_str!("queries.scm");

/// Collection methods that mutate the receiver in place.
const MUTATING: &[&str] = &[
    "append",
    "appendleft",
    "add",
    "clear",
    "difference_update",
    "discard",
    "extend",
    "extendleft",
    "insert",
    "intersection_update",
    "pop",
    "popleft",
    "popitem",
    "remove",
    "reverse",
    "rotate",
    "setdefault",
    "sort",
    "symmetric_difference_update",
    "update",
    "__delitem__",
    "__setitem__",
];

static PYTHON_QUERY: LazyLock<CompiledQuery> =
    LazyLock::new(|| CompiledQuery::new(&tree_sitter_python::LANGUAGE.into(), QUERIES));

thread_local! {
    /// Parsers are not Sync; one per thread keeps parsing off the heap.
    static PARSER: RefCell<Parser> = RefCell::new(python_parser());
}

fn python_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .expect("the bundled Python grammar loads");
    parser
}

fn parse(source: &str) -> Option<Tree> {
    PARSER.with(|parser| parser.borrow_mut().parse(source, None))
}

fn empty_summary(file: &DiscoveredFile, lines: usize) -> FileSummary {
    FileSummary {
        file_path: file.rel_path.clone(),
        module_id: file.module_id,
        language: Language::Python,
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
}

impl<'t> Matches<'t> {
    fn nodes(&self, name: &str) -> &[Node<'t>] {
        self.buckets.get(name).map_or(&[], Vec::as_slice)
    }
}

fn capture_bucket(name: &str) -> Option<&'static str> {
    Some(match name {
        "import" => "import",
        "class" => "class",
        "function" => "function",
        "assignment" => "assignment",
        "member" => "member",
        "call" => "call",
        "computes" => "computes",
        "unary" => "unary",
        "skip" => "skip",
        _ => return None,
    })
}

fn collect_matches<'t>(root: Node<'t>, source: &str) -> Matches<'t> {
    let mut matches = Matches::default();
    PYTHON_QUERY.for_each_match(root, source.as_bytes(), |view| {
        for (name, node) in view.captures() {
            if let Some(bucket) = capture_bucket(name) {
                matches.buckets.entry(bucket).or_default().push(node);
            }
        }
    });
    for bucket in matches.buckets.values_mut() {
        bucket.sort_by_key(|node| node.start_byte());
    }
    matches
}

pub(super) fn summarize(file: &DiscoveredFile, source: &str) -> FileSummary {
    let lines = line_count(source);
    let Some(tree) = parse(source) else {
        return empty_summary(file, lines);
    };
    let matches = collect_matches(tree.root_node(), source);
    let mut extractor = Extractor {
        syntax: Syntax::new(source),
        file,
        summary: empty_summary(file, lines),
        index: ScopeIndex::new(),
        bodies: Vec::new(),
        body_ranges: HashMap::new(),
        methods: HashMap::new(),
        facts: HashMap::new(),
        classes: HashMap::new(),
        fields: HashMap::new(),
        self_fields: Vec::new(),
        annotations: Vec::new(),
        refs: Vec::new(),
    };
    extractor.declarations(&matches);
    extractor.references_and_facts(&matches);
    extractor.infer_property_returns();
    extractor.finish_bodies();
    let Extractor {
        mut summary,
        mut refs,
        ..
    } = extractor;
    refs.sort_by_key(|(offset, _)| *offset);
    summary.refs = refs.into_iter().map(|(_, reference)| reference).collect();
    summary
}

/// Decorators that change how a method binds self or what it exposes.
#[derive(Clone, Copy, Default)]
struct MethodFlags {
    is_static: bool,
    is_classmethod: bool,
    getter: bool,
    setter: bool,
}

impl MethodFlags {
    fn of(decorators: &[&str]) -> Self {
        let mut flags = Self::default();
        for decorator in decorators {
            let name = decorator.rsplit('.').next().unwrap_or(decorator).trim();
            match name {
                "property" | "cached_property" => flags.getter = true,
                "setter" => flags.setter = true,
                "staticmethod" => flags.is_static = true,
                "classmethod" => flags.is_classmethod = true,
                _ => {}
            }
        }
        flags
    }
}

/// One use of a field of self inside a function body.
struct FieldOp {
    field: String,
    write: bool,
    returned: bool,
}

/// An instance attribute a method assigns on self.
struct SelfField {
    class: LocalDeclId,
    name: String,
    ty: Option<TypeExpr>,
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
    methods: HashMap<LocalDeclId, MethodFlags>,
    facts: HashMap<LocalDeclId, BodyFacts>,
    /// Class declaration by the byte its definition starts at.
    classes: HashMap<usize, LocalDeclId>,
    /// Field declaration by owning class and field name, for deduplication.
    fields: HashMap<(LocalDeclId, String), LocalDeclId>,
    self_fields: Vec<SelfField>,
    annotations: Vec<(Node<'t>, Option<LocalDeclId>)>,
    refs: Vec<(usize, RawRef)>,
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
    }

    fn push_import(
        &mut self,
        specifier: String,
        path_segments: Vec<String>,
        kind: ImportKind,
        reexport: bool,
    ) {
        self.summary.imports.push(RawImport {
            scope: ModuleScope::FileRoot,
            path_segments,
            raw_specifier: Some(specifier),
            kind,
            is_reexport: reexport,
        });
    }

    fn push_ref(&mut self, node: Node, kind: RawRefKind, access: AccessMode) {
        let holder = self.index.holder_at(node.start_byte());
        self.push_ref_at(node, kind, access, holder);
    }

    fn push_ref_at(
        &mut self,
        node: Node,
        kind: RawRefKind,
        access: AccessMode,
        holder: Option<LocalDeclId>,
    ) {
        self.refs.push((
            node.start_byte(),
            RawRef {
                line: node_line(node),
                scope: ModuleScope::FileRoot,
                holder,
                kind,
                access,
                is_test: self.file.is_test,
            },
        ));
    }

    // ----- declarations ------------------------------------------------------

    fn declarations(&mut self, m: &Matches<'t>) {
        let classes: Vec<Node<'t>> = m
            .nodes("class")
            .iter()
            .copied()
            .filter(|node| self.syntax.scope_owner(*node).is_some())
            .collect();
        for node in &classes {
            self.class(*node);
        }
        self.index.finalize();
        for node in &classes {
            self.class_parent(*node);
        }
        for &node in m.nodes("function") {
            self.function(node);
        }
        for &node in m.nodes("skip") {
            self.index.add_skipped_span(node.byte_range());
        }
        self.index.finalize();
        for &node in m.nodes("assignment") {
            self.assignment(node);
        }
        for field in std::mem::take(&mut self.self_fields) {
            self.field(field.class, field.name, field.ty, None);
        }
        self.index.finalize();
        for &node in m.nodes("import") {
            self.import(node);
        }
    }

    fn class(&mut self, node: Node<'t>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name).to_owned();
        let id = self.push_decl(name, ItemKind::Type, None);
        // Python has no private declarations: from mod import _foo is legal.
        self.decl_mut(id).is_exported = true;
        self.index.add_holder(node.byte_range(), id);
        self.index.add_parent(node.byte_range(), id);
        self.classes.insert(node.start_byte(), id);
    }

    fn class_parent(&mut self, node: Node<'t>) {
        let Some(&id) = self.classes.get(&node.start_byte()) else {
            return;
        };
        let parent = self.index.parent_decl_of(&node.byte_range());
        self.summary.decls[id.0 as usize].parent_decl = parent;
    }

    fn function(&mut self, node: Node<'t>) {
        let Some(owner) = self.syntax.scope_owner(node) else {
            // A closure inside another body is that body's business.
            self.index.add_skipped_span(node.byte_range());
            return;
        };
        let parent = match owner.kind() {
            "module" => None,
            "class_definition" => match self.classes.get(&owner.start_byte()).copied() {
                Some(class) => Some(class),
                None => return,
            },
            _ => return,
        };
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name).to_owned();
        let flags = MethodFlags::of(&self.syntax.decorators(node));
        let (signature, annotations) = self.signature_of(node);
        let takes_self = parent.is_some()
            && !flags.is_static
            && !flags.is_classmethod
            && signature
                .params
                .first()
                .is_some_and(|param| param.name == "self");
        let id = self.push_decl(name, ItemKind::Function, parent);
        let decl = self.decl_mut(id);
        decl.is_exported = parent.is_none();
        decl.takes_self = takes_self;
        decl.signature = Some(signature);
        self.methods.insert(id, flags);
        self.index.add_holder(node.byte_range(), id);
        self.annotations.extend(
            annotations
                .into_iter()
                .map(|annotation| (annotation, Some(id))),
        );
        if let Some(body) = node.child_by_field_name("body") {
            self.add_body(id, body);
        }
    }

    fn assignment(&mut self, node: Node<'t>) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let annotation = node.child_by_field_name("type");
        let ty = annotation.and_then(|annotation| self.syntax.type_expression(annotation));
        let value = node.child_by_field_name("right");
        let holder = self.index.holder_at(node.start_byte());
        match left.kind() {
            "attribute" => self.self_attribute(left, annotation, ty, value, holder),
            "identifier" => {
                let name = self.text(left).to_owned();
                match holder.map(|id| (id, self.summary.decls[id.0 as usize].kind)) {
                    None => {
                        let id = self.push_decl(name, ItemKind::Constant, None);
                        let decl = self.decl_mut(id);
                        decl.is_exported = true;
                        decl.type_annotation = ty;
                        self.index.add_holder(node.byte_range(), id);
                        if let Some(annotation) = annotation {
                            self.annotations.push((annotation, Some(id)));
                        }
                    }
                    Some((class, ItemKind::Type)) => {
                        let field = self.field(class, name, ty, value);
                        if let Some(annotation) = annotation {
                            self.annotations.push((annotation, Some(field)));
                        }
                    }
                    Some((owner, _)) => {
                        self.local(owner, name, ty, value);
                        if let Some(annotation) = annotation {
                            self.annotations.push((annotation, Some(owner)));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// self.items = ... inside a method declares a field of its class.
    fn self_attribute(
        &mut self,
        left: Node<'t>,
        annotation: Option<Node<'t>>,
        ty: Option<TypeExpr>,
        value: Option<Node<'t>>,
        holder: Option<LocalDeclId>,
    ) {
        let Some(object) = left.child_by_field_name("object") else {
            return;
        };
        if self.text(object) != "self" {
            return;
        }
        let Some(attribute) = left.child_by_field_name("attribute") else {
            return;
        };
        let Some(class) = self.owning_class(holder) else {
            return;
        };
        let name = self.text(attribute).to_owned();
        let ty = ty
            .or_else(|| self.parameter_type(holder, value))
            .or_else(|| self.inferred_value_type(value));
        self.self_fields.push(SelfField { class, name, ty });
        if let Some(annotation) = annotation {
            self.annotations.push((annotation, holder));
        }
    }

    /// The class a declaration written in holder belongs to.
    fn owning_class(&self, holder: Option<LocalDeclId>) -> Option<LocalDeclId> {
        let holder = holder?;
        let decl = &self.summary.decls[holder.0 as usize];
        if decl.kind == ItemKind::Type {
            return Some(holder);
        }
        decl.parent_decl
    }

    /// The declared type of the parameter self.x = x copies.
    fn parameter_type(
        &self,
        holder: Option<LocalDeclId>,
        value: Option<Node<'t>>,
    ) -> Option<TypeExpr> {
        let holder = holder?;
        let value = value?;
        if value.kind() != "identifier" {
            return None;
        }
        let name = self.text(value);
        let signature = self.summary.decls[holder.0 as usize].signature.as_ref()?;
        signature
            .params
            .iter()
            .find(|param| param.name == name)?
            .ty
            .clone()
    }

    /// A field built by a call to a class: self.ring = Ring().
    fn inferred_value_type(&self, value: Option<Node<'t>>) -> Option<TypeExpr> {
        let value = value?;
        if value.kind() != "call" {
            return None;
        }
        let function = value.child_by_field_name("function")?;
        if !matches!(function.kind(), "identifier" | "attribute") {
            return None;
        }
        let path = self.syntax.dotted_segments(function);
        let last = path.last()?;
        starts_uppercase(last).then(|| TypeExpr::from_path(path))
    }

    fn field(
        &mut self,
        class: LocalDeclId,
        name: String,
        ty: Option<TypeExpr>,
        value: Option<Node<'t>>,
    ) -> LocalDeclId {
        if let Some(&existing) = self.fields.get(&(class, name.clone())) {
            let decl = &mut self.summary.decls[existing.0 as usize];
            if decl.type_annotation.is_none() {
                decl.type_annotation = ty;
            }
            return existing;
        }
        let id = self.push_decl(name.clone(), ItemKind::Field, Some(class));
        self.decl_mut(id).type_annotation = ty;
        self.fields.insert((class, name), id);
        if let Some(value) = value {
            self.index.add_holder(value.byte_range(), id);
        }
        id
    }

    fn local(
        &mut self,
        owner: LocalDeclId,
        name: String,
        declared_type: Option<TypeExpr>,
        value: Option<Node<'t>>,
    ) {
        let init = value.and_then(|value| self.syntax.init_expr(value));
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

    fn signature_of(&self, node: Node<'t>) -> (FunctionSig, Vec<Node<'t>>) {
        let mut params = Vec::new();
        let mut annotations = Vec::new();
        if let Some(parameters) = node.child_by_field_name("parameters") {
            for parameter in named_children(parameters) {
                let (name_node, type_node) = match parameter.kind() {
                    "identifier" => (Some(parameter), None),
                    "typed_parameter" => (
                        named_children(parameter).into_iter().next(),
                        parameter.child_by_field_name("type"),
                    ),
                    "default_parameter" => (parameter.child_by_field_name("name"), None),
                    "typed_default_parameter" => (
                        parameter.child_by_field_name("name"),
                        parameter.child_by_field_name("type"),
                    ),
                    "list_splat_pattern" | "dictionary_splat_pattern" => {
                        (named_children(parameter).into_iter().next(), None)
                    }
                    _ => continue,
                };
                let Some(name_node) = name_node else {
                    continue;
                };
                let name = self.text(name_node).to_owned();
                if name.is_empty() {
                    continue;
                }
                let ty = type_node.and_then(|node| self.syntax.type_expression(node));
                if let Some(type_node) = type_node {
                    annotations.push(type_node);
                }
                params.push(ParamDecl { name, ty });
            }
        }
        let return_node = node.child_by_field_name("return_type");
        let return_type = return_node.and_then(|node| self.syntax.type_expression(node));
        if let Some(return_node) = return_node {
            annotations.push(return_node);
        }
        (
            FunctionSig {
                params,
                return_type,
            },
            annotations,
        )
    }

    // ----- references and function-body facts --------------------------------

    fn references_and_facts(&mut self, m: &Matches<'t>) {
        for &node in m.nodes("computes") {
            self.mark_op(node);
        }
        for &node in m.nodes("unary") {
            if node
                .child_by_field_name("operator")
                .is_some_and(|operator| matches!(self.text(operator), "-" | "+" | "~" | "not"))
            {
                self.mark_op(node);
            }
        }
        for &node in m.nodes("call") {
            self.call(node);
        }
        for &node in m.nodes("member") {
            self.member(node);
        }
        self.annotation_references();
    }

    fn mark_op(&mut self, node: Node) {
        if let Some(owner) = self.body_owner(node) {
            self.facts.entry(owner).or_default().ops = true;
        }
    }

    /// The function whose body contains node, unless node sits in a nested body.
    fn body_owner(&self, node: Node) -> Option<LocalDeclId> {
        let id = self.index.holder_at(node.start_byte())?;
        let range = self.body_ranges.get(&id)?;
        (!self.index.is_skipped_at(node.start_byte()) && range.contains(&node.start_byte()))
            .then_some(id)
    }

    fn call(&mut self, node: Node<'t>) {
        if self.in_annotation(node) {
            return;
        }
        if let Some(owner) = self.body_owner(node) {
            self.facts.entry(owner).or_default().calls += 1;
        }
        let Some(function) = node.child_by_field_name("function") else {
            return;
        };
        // A call on an attribute is a member access; a call on a name refers to
        // a function or a class.
        if function.kind() != "identifier" {
            return;
        }
        self.push_ref(
            function,
            RawRefKind::Path(vec![self.text(function).to_owned()]),
            AccessMode::CopyOrRead,
        );
    }

    fn member(&mut self, node: Node<'t>) {
        if self.in_annotation(node) {
            return;
        }
        // The outermost attribute of a chain speaks for the whole chain.
        if node.parent().is_some_and(|parent| {
            parent.kind() == "attribute"
                && parent
                    .child_by_field_name("object")
                    .is_some_and(|object| object.id() == node.id())
        }) {
            return;
        }
        let Some((base, fields)) = self.syntax.flatten_attribute(node) else {
            return;
        };
        let is_call = self.syntax.is_called(node);
        let write = self.syntax.in_write_position(node);
        let indexed = self.syntax.is_indexed(node);
        let outer = fields
            .last()
            .cloned()
            .expect("a flattened chain names a member");
        let mutating_call = is_call && MUTATING.contains(&outer.as_str());
        let outer_access = if write || indexed || mutating_call {
            AccessMode::Work
        } else if is_call {
            AccessMode::MethodCall
        } else {
            AccessMode::CopyOrRead
        };
        // Reaching past an earlier link to work on the last one works on both.
        let intermediate_access = if write || indexed || mutating_call {
            AccessMode::Work
        } else if is_call {
            AccessMode::MethodCall
        } else {
            AccessMode::CopyOrRead
        };
        let mut receiver = self.syntax.receiver_of(base);
        let count = fields.len();
        for (index, field) in fields.iter().enumerate() {
            let last = index + 1 == count;
            let access = if last {
                outer_access
            } else {
                intermediate_access
            };
            self.push_ref(
                node,
                RawRefKind::MemberAccess {
                    receiver: receiver.clone(),
                    member: field.clone(),
                    is_call: last && is_call,
                },
                access,
            );
            receiver = extend_receiver(receiver, field.clone());
        }
        self.member_facts(node, base, &fields, is_call, write, indexed);
    }

    /// What a use of self.field says about the enclosing method.
    fn member_facts(
        &mut self,
        node: Node<'t>,
        base: Node<'t>,
        fields: &[String],
        is_call: bool,
        write: bool,
        indexed: bool,
    ) {
        if base.kind() != "identifier" || self.text(base) != "self" {
            return;
        }
        let Some(owner) = self.body_owner(node) else {
            return;
        };
        let outer = fields.last().cloned().expect("a chain names a field");
        let param = fields.len() == 1 && self.assigned_param(node, owner);
        let facts = self.facts.entry(owner).or_default();
        if is_call {
            facts.mutated |= MUTATING.contains(&outer.as_str());
            facts.reaches = true;
        } else if write {
            facts.mutated = true;
            if param {
                facts.field_ops.push(FieldOp {
                    field: outer,
                    write: true,
                    returned: false,
                });
            } else if fields.len() > 1 {
                facts.reaches = true;
            }
        } else if indexed {
            facts.reaches = true;
        } else if fields.len() == 1 {
            let returned = self.syntax.in_return_position(node);
            facts.field_ops.push(FieldOp {
                field: outer,
                write: false,
                returned,
            });
        } else {
            facts.reaches = true;
        }
    }

    fn assigned_param(&self, node: Node, owner: LocalDeclId) -> bool {
        let params = self.summary.decls[owner.0 as usize]
            .signature
            .as_ref()
            .map(|signature| signature.params.as_slice())
            .unwrap_or_default();
        self.syntax.assigned_param(node, params)
    }

    fn annotation_references(&mut self) {
        for (node, holder) in std::mem::take(&mut self.annotations) {
            for path in self.syntax.type_paths(node) {
                if path.is_empty() {
                    continue;
                }
                self.push_ref_at(node, RawRefKind::Path(path), AccessMode::CopyOrRead, holder);
            }
        }
    }

    /// True when node sits in a type annotation rather than in code.
    fn in_annotation(&self, node: Node) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "typed_parameter" | "typed_default_parameter" => {
                    return parent
                        .child_by_field_name("type")
                        .is_some_and(|ty| contains(ty, node));
                }
                "function_definition" => {
                    return parent
                        .child_by_field_name("return_type")
                        .is_some_and(|ty| contains(ty, node));
                }
                "assignment" => {
                    return parent
                        .child_by_field_name("type")
                        .is_some_and(|ty| contains(ty, node));
                }
                "module" | "block" | "class_definition" | "decorated_definition" => return false,
                _ => current = parent,
            }
        }
    }

    // ----- wrappers ----------------------------------------------------------

    /// A getter hands out the type of the field it returns, so accessor checks
    /// that compare a field's type with the signature have something to read.
    fn infer_property_returns(&mut self) {
        let mut inferred = Vec::new();
        for (&id, facts) in &self.facts {
            if facts.field_ops.iter().any(|op| op.write) {
                continue;
            }
            let returned: Vec<&FieldOp> = facts.field_ops.iter().filter(|op| op.returned).collect();
            let [only] = returned.as_slice() else {
                continue;
            };
            let decl = &self.summary.decls[id.0 as usize];
            let Some(parent) = decl.parent_decl else {
                continue;
            };
            if decl
                .signature
                .as_ref()
                .is_none_or(|signature| signature.return_type.is_some())
            {
                continue;
            }
            let Some(&field) = self.fields.get(&(parent, only.field.clone())) else {
                continue;
            };
            let Some(ty) = self.summary.decls[field.0 as usize].type_annotation.clone() else {
                continue;
            };
            inferred.push((id, ty));
        }
        for (id, ty) in inferred {
            let Some(signature) = &mut self.summary.decls[id.0 as usize].signature else {
                continue;
            };
            signature.return_type = Some(ty);
        }
    }

    fn finish_bodies(&mut self) {
        for (id, body) in std::mem::take(&mut self.bodies) {
            let facts = self.facts.remove(&id).unwrap_or_default();
            let params: Vec<String> = self.summary.decls[id.0 as usize]
                .signature
                .as_ref()
                .map(|signature| {
                    signature
                        .params
                        .iter()
                        .map(|param| param.name.clone())
                        .collect()
                })
                .unwrap_or_default();
            let forwarding = self.forwarding_candidate(body, &params);
            let computes = if forwarding.is_some() {
                facts.ops
            } else {
                facts.ops || facts.reaches || facts.calls > 0
            };
            let takes_self = self.summary.decls[id.0 as usize].takes_self;
            let flags = self.methods.get(&id).copied().unwrap_or_default();
            let self_is_shared_ref = if !takes_self {
                false
            } else if flags.getter {
                true
            } else if flags.setter {
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
            // Python objects are references, but resolve decides plainness from
            // the field's declared type; only an overwrite is written outright.
            let lent = op.write;
            if let Some(existing) = ops.iter_mut().find(|entry| entry.field_name == op.field) {
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
                CalleeExpr::Path(vec![self.text(function).to_owned()]),
                Vec::new(),
            ),
            "attribute" => {
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
                            .text(function.child_by_field_name("attribute")?)
                            .to_owned(),
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

    // ----- imports -----------------------------------------------------------

    fn import(&mut self, node: Node<'t>) {
        // Everything an __init__.py imports is part of its package.
        let reexport = self.file.rel_path.rsplit('/').next() == Some("__init__.py");
        match node.kind() {
            "import_statement" => self.import_statement(node, reexport),
            "import_from_statement" => self.import_from_statement(node, reexport),
            _ => {}
        }
    }

    fn import_statement(&mut self, node: Node<'t>, reexport: bool) {
        for name in nodes_by_field(node, "name") {
            let (module, alias) = match name.kind() {
                "dotted_name" => {
                    let segments = self.syntax.dotted_segments(name);
                    let Some(first) = segments.first().cloned() else {
                        continue;
                    };
                    // import a.b binds a alone.
                    (segments, first)
                }
                "aliased_import" => {
                    let (Some(module), Some(alias)) = (
                        name.child_by_field_name("name"),
                        name.child_by_field_name("alias"),
                    ) else {
                        continue;
                    };
                    (
                        self.syntax.dotted_segments(module),
                        self.text(alias).to_owned(),
                    )
                }
                _ => continue,
            };
            if module.is_empty() {
                continue;
            }
            self.syntax.add_namespace(alias.clone());
            let specifier = module.join(".");
            self.push_import(specifier, module, ImportKind::Namespace { alias }, reexport);
        }
    }

    fn import_from_statement(&mut self, node: Node<'t>, reexport: bool) {
        let Some(module) = node.child_by_field_name("module_name") else {
            return;
        };
        let (specifier, segments) = self.module_specifier(module);
        if specifier.is_empty() {
            return;
        }
        if named_children(node)
            .iter()
            .any(|child| child.kind() == "wildcard_import")
        {
            self.push_import(specifier, segments, ImportKind::Glob, reexport);
            return;
        }
        for name in nodes_by_field(node, "name") {
            let kind = match name.kind() {
                "dotted_name" => ImportKind::Named {
                    name: self.text(name).to_owned(),
                    alias: None,
                },
                "aliased_import" => {
                    let (Some(original), Some(alias)) = (
                        name.child_by_field_name("name"),
                        name.child_by_field_name("alias"),
                    ) else {
                        continue;
                    };
                    ImportKind::Named {
                        name: self.text(original).to_owned(),
                        alias: Some(self.text(alias).to_owned()),
                    }
                }
                _ => continue,
            };
            self.push_import(specifier.clone(), segments.clone(), kind, reexport);
        }
    }

    /// A from-import module as written (".sub") and its dotted segments.
    fn module_specifier(&self, node: Node) -> (String, Vec<String>) {
        match node.kind() {
            "dotted_name" => {
                let segments = self.syntax.dotted_segments(node);
                (segments.join("."), segments)
            }
            "relative_import" => {
                let mut dots = String::new();
                let mut segments = Vec::new();
                for child in named_children(node) {
                    match child.kind() {
                        "import_prefix" => dots = self.text(child).to_owned(),
                        "dotted_name" => segments = self.syntax.dotted_segments(child),
                        _ => {}
                    }
                }
                (format!("{dots}{}", segments.join(".")), segments)
            }
            _ => (String::new(), Vec::new()),
        }
    }
}

/// The receiver that reads field off receiver.
fn extend_receiver(receiver: ReceiverExpr, field: String) -> ReceiverExpr {
    match receiver {
        ReceiverExpr::FieldChain { base, mut fields } => {
            fields.push(field);
            ReceiverExpr::FieldChain { base, fields }
        }
        ReceiverExpr::Unknown => ReceiverExpr::Unknown,
        base => ReceiverExpr::FieldChain {
            base: Box::new(base),
            fields: vec![field],
        },
    }
}

fn contains(outer: Node, inner: Node) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::Tree;
    use crate::treesitter::summary::{AccessMode, ImportKind, InitExpr, RawRefKind};

    fn discovered(name: &str) -> DiscoveredFile {
        let module = Tree::new().root();
        DiscoveredFile {
            abs_path: PathBuf::from("/workspace").join(name),
            rel_path: name.to_owned(),
            module_id: module,
            package_id: module,
            language: Language::Python,
            is_test: false,
        }
    }

    fn summary(name: &str, source: &str) -> FileSummary {
        summarize(&discovered(name), source)
    }

    fn decl<'a>(summary: &'a FileSummary, name: &str) -> &'a ItemDecl {
        summary
            .decls
            .iter()
            .find(|decl| decl.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "no declaration {name} among {:?}",
                    summary
                        .decls
                        .iter()
                        .map(|decl| &decl.name)
                        .collect::<Vec<_>>()
                )
            })
    }

    fn decls_named<'a>(summary: &'a FileSummary, name: &str) -> Vec<&'a ItemDecl> {
        summary
            .decls
            .iter()
            .filter(|decl| decl.name == name)
            .collect()
    }

    fn members(summary: &FileSummary, member: &str) -> Vec<AccessMode> {
        summary
            .refs
            .iter()
            .filter(|reference| {
                matches!(
                    &reference.kind,
                    RawRefKind::MemberAccess { member: found, .. } if found == member
                )
            })
            .map(|reference| reference.access)
            .collect()
    }

    fn local<'a>(summary: &'a FileSummary, function: &str, name: &str) -> &'a LocalBinding {
        decl(summary, function)
            .body_summary
            .as_ref()
            .expect("the function has a body")
            .locals
            .iter()
            .find(|local| local.name == name)
            .unwrap_or_else(|| panic!("no local {name} in {function}"))
    }

    #[test]
    fn every_import_form_is_extracted() {
        let summary = summary(
            "app/main.py",
            "import os\n\
             import pkg.sub.mod\n\
             import pkg.sub.mod as alias\n\
             from pkg.sub import Foo, Bar as B\n\
             from . import sibling\n\
             from .sub import Thing\n\
             from ..utils import helper\n\
             from pkg.sub import *\n",
        );
        let specifiers: Vec<&str> = summary
            .imports
            .iter()
            .map(|import| import.raw_specifier.as_deref().unwrap())
            .collect();
        assert_eq!(
            specifiers,
            [
                "os",
                "pkg.sub.mod",
                "pkg.sub.mod",
                "pkg.sub",
                "pkg.sub",
                ".",
                ".sub",
                "..utils",
                "pkg.sub",
            ]
        );
        assert!(summary.imports.iter().all(|import| !import.is_reexport));

        assert!(matches!(
            &summary.imports[0].kind,
            ImportKind::Namespace { alias } if alias == "os"
        ));
        assert!(
            matches!(&summary.imports[1].kind, ImportKind::Namespace { alias } if alias == "pkg"),
            "import a.b binds a alone"
        );
        assert!(matches!(
            &summary.imports[2].kind,
            ImportKind::Namespace { alias } if alias == "alias"
        ));
        assert!(matches!(
            &summary.imports[3].kind,
            ImportKind::Named { name, alias: None } if name == "Foo"
        ));
        assert!(matches!(
            &summary.imports[4].kind,
            ImportKind::Named { name, alias: Some(alias) } if name == "Bar" && alias == "B"
        ));
        assert!(matches!(
            &summary.imports[5].kind,
            ImportKind::Named { name, alias: None } if name == "sibling"
        ));
        assert!(
            matches!(&summary.imports[8].kind, ImportKind::Glob),
            "a wildcard import is global"
        );
    }

    #[test]
    fn a_package_init_reexports_what_it_imports() {
        let summary = summary(
            "app/queue/__init__.py",
            "from .ring import RunQueue\nfrom . import helpers\n",
        );
        assert!(summary.imports.iter().all(|import| import.is_reexport));
        assert_eq!(summary.imports[0].raw_specifier.as_deref(), Some(".ring"));
        assert_eq!(summary.imports[1].raw_specifier.as_deref(), Some("."));
    }

    #[test]
    fn classes_yield_fields_methods_and_wrapper_candidates() {
        let summary = summary(
            "app/queue.py",
            "class Queue:\n\
             \x20   limit: int = 10\n\
             \x20   def __init__(self, ring: Ring, limit: int):\n\
             \x20       self.ring = ring\n\
             \x20       self.items: list[Item] = []\n\
             \x20       self.limit = limit\n\
             \x20   @property\n\
             \x20   def size(self) -> int:\n\
             \x20       return self.limit\n\
             \x20   @size.setter\n\
             \x20   def size(self, value: int) -> None:\n\
             \x20       self.limit = value\n\
             \x20   def push(self, item: Item) -> None:\n\
             \x20       self.items.append(item)\n\
             \x20   def full(self) -> bool:\n\
             \x20       return self.limit <= len(self.items)\n\
             \x20   @staticmethod\n\
             \x20   def make() -> Queue:\n\
             \x20       return Queue(Ring(), 1)\n",
        );
        let queue = decl(&summary, "Queue");
        assert_eq!(queue.kind, ItemKind::Type);
        assert!(queue.is_exported);
        assert_eq!(queue.parent_decl, None);

        let field = |name: &str| {
            let found = decl(&summary, name);
            assert_eq!(found.kind, ItemKind::Field, "the field {name}");
            assert_eq!(found.parent_decl, Some(queue.local_id));
            found
        };
        assert_eq!(
            field("limit").type_annotation.as_ref().unwrap().path,
            ["int"]
        );
        assert_eq!(
            field("ring").type_annotation.as_ref().unwrap().path,
            ["Ring"]
        );
        assert_eq!(
            field("items").type_annotation.as_ref().unwrap().path,
            ["list"]
        );
        assert_eq!(
            decls_named(&summary, "limit").len(),
            1,
            "a field declared twice is one field"
        );

        let init = decl(&summary, "__init__");
        assert!(init.takes_self);
        assert!(!init.self_is_shared_ref, "__init__ writes self");

        let push = decl(&summary, "push");
        assert_eq!(push.parent_decl, Some(queue.local_id));
        assert!(push.takes_self);
        assert!(!push.self_is_shared_ref, "push mutates self.items");
        let body = push.body_summary.as_ref().expect("push has a body");
        assert!(!body.computes);
        let forwarding = body.forwarding_candidate.as_ref().expect("push forwards");
        assert_eq!(forwarding.self_fields, ["items"]);
        assert!(matches!(
            &forwarding.callee,
            CalleeExpr::Method { receiver: ReceiverExpr::FieldChain { base, fields }, method }
                if matches!(**base, ReceiverExpr::SelfValue) && fields == &["items"] && method == "append"
        ));

        let sizes = decls_named(&summary, "size");
        assert_eq!(sizes.len(), 2, "a getter and a setter");
        assert!(sizes[0].self_is_shared_ref, "a getter only reads");
        assert!(!sizes[1].self_is_shared_ref, "a setter writes");
        let getter = sizes[0].body_summary.as_ref().unwrap();
        assert!(!getter.computes);
        let accessor = getter.accessor_candidate.as_ref().expect("a getter");
        assert_eq!(accessor.ops.len(), 1);
        assert_eq!(accessor.ops[0].field_name, "limit");
        assert!(!accessor.ops[0].is_write);

        let full = decl(&summary, "full");
        assert!(full.body_summary.as_ref().unwrap().computes);

        let make = decl(&summary, "make");
        assert!(!make.takes_self, "a static method is not a method");
        assert_eq!(
            make.signature
                .as_ref()
                .unwrap()
                .return_type
                .as_ref()
                .unwrap()
                .path,
            ["Queue"]
        );
    }

    #[test]
    fn a_property_infers_the_type_of_the_field_it_returns() {
        let summary = summary(
            "app/ring.py",
            "class Ring:\n\
             \x20   def __init__(self):\n\
             \x20       self.items: list[int] = []\n\
             \x20   @property\n\
             \x20   def contents(self):\n\
             \x20       return self.items\n",
        );
        let contents = decl(&summary, "contents");
        let signature = contents.signature.as_ref().expect("a signature");
        assert_eq!(
            signature.return_type.as_ref().unwrap().path,
            ["list"],
            "a getter hands out its field's type"
        );
        assert!(contents.self_is_shared_ref);
        let accessor = contents
            .body_summary
            .as_ref()
            .unwrap()
            .accessor_candidate
            .as_ref()
            .expect("a getter");
        assert_eq!(accessor.ops[0].field_name, "items");
    }

    #[test]
    fn self_attributes_become_typed_fields() {
        let summary = summary(
            "app/queue.py",
            "class Queue:\n\
             \x20   items: list[int] = []\n\
             \x20   def __init__(self, limit: int):\n\
             \x20       self.limit = limit\n\
             \x20       self.items = []\n\
             \x20       self.ring = Ring()\n",
        );
        let queue = decl(&summary, "Queue");
        let fields: Vec<(&str, Option<&TypeExpr>)> = summary
            .decls
            .iter()
            .filter(|decl| decl.kind == ItemKind::Field)
            .map(|decl| (decl.name.as_str(), decl.type_annotation.as_ref()))
            .collect();
        assert_eq!(
            fields
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .len(),
            3,
            "items, limit and ring, each once: {fields:?}"
        );
        assert_eq!(
            decl(&summary, "limit")
                .type_annotation
                .as_ref()
                .unwrap()
                .path,
            ["int"],
            "a parameter annotation types the field it fills"
        );
        assert_eq!(
            decl(&summary, "items")
                .type_annotation
                .as_ref()
                .unwrap()
                .path,
            ["list"],
            "the class body declaration keeps its type"
        );
        assert_eq!(
            decl(&summary, "ring")
                .type_annotation
                .as_ref()
                .unwrap()
                .path,
            ["Ring"],
            "a call to a class types the field it builds"
        );
        assert!(
            summary
                .decls
                .iter()
                .any(|decl| decl.parent_decl == Some(queue.local_id) && decl.name == "ring")
        );
    }

    #[test]
    fn module_functions_have_signatures_and_locals() {
        let summary = summary(
            "app/run.py",
            "LIMIT = 10\n\
             \n\
             def run(q: Queue, items: list[Item] | None = None) -> bool:\n\
             \x20   local: Queue = q\n\
             \x20   created = Queue()\n\
             \x20   made = create_queue()\n\
             \x20   ring = q.ring\n\
             \x20   alias = q\n\
             \x20   return created.full()\n",
        );
        let limit = decl(&summary, "LIMIT");
        assert_eq!(limit.kind, ItemKind::Constant);
        assert!(limit.is_exported);
        assert!(limit.type_annotation.is_none());

        let run = decl(&summary, "run");
        assert_eq!(run.kind, ItemKind::Function);
        assert_eq!(run.parent_decl, None);
        assert!(run.is_exported);
        let signature = run.signature.as_ref().expect("a signature");
        assert_eq!(signature.params[0].name, "q");
        assert_eq!(signature.params[0].ty.as_ref().unwrap().path, ["Queue"]);
        assert_eq!(signature.params[1].name, "items");
        assert_eq!(
            signature.params[1].ty.as_ref().unwrap().path,
            ["list"],
            "X | None peels to X"
        );
        assert_eq!(signature.return_type.as_ref().unwrap().path, ["bool"]);

        assert_eq!(
            local(&summary, "run", "local")
                .declared_type
                .as_ref()
                .unwrap()
                .path,
            ["Queue"]
        );
        assert!(matches!(
            local(&summary, "run", "created").init,
            Some(InitExpr::Call(CalleeExpr::Path(ref path))) if path == &["Queue"]
        ));
        assert!(matches!(
            local(&summary, "run", "made").init,
            Some(InitExpr::Call(CalleeExpr::Path(ref path))) if path == &["create_queue"]
        ));
        assert!(matches!(
            local(&summary, "run", "ring").init,
            Some(InitExpr::FieldChain { base: ReceiverExpr::Local(ref base), ref fields })
                if base == "q" && fields == &["ring"]
        ));
        assert!(matches!(
            local(&summary, "run", "alias").init,
            Some(InitExpr::Var(ref name)) if name == "q"
        ));
    }

    #[test]
    fn access_modes_mark_writes_method_calls_and_reads() {
        let summary = summary(
            "app/use.py",
            "def use(q: Queue, ring: Ring):\n\
             \x20   total = q.count\n\
             \x20   q.items = []\n\
             \x20   q.items[0] = 1\n\
             \x20   q.items.append(2)\n\
             \x20   q.ring.push(1)\n\
             \x20   sink = len(q.items)\n",
        );
        assert_eq!(members(&summary, "count"), [AccessMode::CopyOrRead]);
        assert_eq!(
            members(&summary, "items"),
            [
                AccessMode::Work,
                AccessMode::Work,
                AccessMode::Work,
                AccessMode::CopyOrRead,
            ],
            "an assignment, an index, a mutating call and a plain read"
        );
        assert_eq!(
            members(&summary, "ring"),
            [AccessMode::MethodCall],
            "push is not a collection mutation"
        );
        assert!(summary.refs.iter().any(|reference| {
            matches!(
                &reference.kind,
                RawRefKind::Path(path) if path == &["len"] || path == &["use"]
            ) && reference.access == AccessMode::CopyOrRead
        }));
    }

    #[test]
    fn annotations_are_references_to_the_types_they_name() {
        let summary = summary(
            "app/typed.py",
            "def make(q: Queue, items: list[Item]) -> Ring:\n\
             \x20   return q\n",
        );
        let paths: Vec<&Vec<String>> = summary
            .refs
            .iter()
            .filter_map(|reference| match &reference.kind {
                RawRefKind::Path(path) => Some(path),
                _ => None,
            })
            .collect();
        assert!(paths.iter().any(|path| path.as_slice() == ["Queue"]));
        assert!(paths.iter().any(|path| path.as_slice() == ["list"]));
        assert!(paths.iter().any(|path| path.as_slice() == ["Item"]));
        assert!(paths.iter().any(|path| path.as_slice() == ["Ring"]));
    }

    #[test]
    fn a_closures_body_is_its_own() {
        let summary = summary(
            "app/forward.py",
            "def push(self, item, key=lambda x: x + 1):\n\
             \x20   self.items.append(item)\n",
        );
        let push = decl(&summary, "push");
        let body = push.body_summary.as_ref().expect("a body");
        assert!(
            body.forwarding_candidate.is_some(),
            "the closure in the default argument computes nothing for the method"
        );
        assert!(!body.computes);
    }

    #[test]
    fn comprehensions_loops_and_raises_count_as_work() {
        for source in [
            "def f(rows):\n    return [row for row in rows]\n",
            "def f(rows):\n    for row in rows:\n        pass\n",
            "def f(flag):\n    raise ValueError(flag)\n",
            "def f(flag):\n    with open(flag) as handle:\n        pass\n",
            "def f(x):\n    return -x\n",
        ] {
            let summary = summary("app/work.py", source);
            let function = decl(&summary, "f");
            assert!(
                function.body_summary.as_ref().unwrap().computes,
                "{source} computes"
            );
        }
    }

    #[test]
    fn test_files_mark_their_declarations() {
        let module = Tree::new().root();
        let file = DiscoveredFile {
            abs_path: PathBuf::from("/workspace/tests/test_queue.py"),
            rel_path: "tests/test_queue.py".to_owned(),
            module_id: module,
            package_id: module,
            language: Language::Python,
            is_test: true,
        };
        let summary = summarize(&file, "class Queue:\n    pass\n");
        assert!(summary.is_test);
        assert!(summary.decls.iter().all(|decl| decl.is_test));
        assert!(summary.refs.iter().all(|reference| reference.is_test));
    }
}
