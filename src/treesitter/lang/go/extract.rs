//! Tree-sitter `.scm` driven extraction of a Go file into a lifetime-free
//! [`FileSummary`].
//!
//! `queries.scm` is compiled once; its captures are bucketed, the declarations
//! become `ItemDecl`s, and `ScopeIndex` supplies the enclosing holder and
//! parent for every reference and function-body fact. Go names its method
//! receiver (`q`), so the extractor maps that name to `self`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::LazyLock;

use tree_sitter::{Node, Parser, Tree};

use super::syntax::{Syntax, named_children, starts_uppercase};
use crate::model::ItemKind;
use crate::treesitter::lang::{DiscoveredFile, Language};
use crate::treesitter::query::{CompiledQuery, ScopeIndex, line_count, node_line};
use crate::treesitter::summary::{
    AccessMode, AccessorCandidate, AccessorFieldOp, CalleeExpr, FileSummary, ForwardingCandidate,
    FunctionBodySummary, FunctionSig, ImportKind, ItemDecl, LocalBinding, LocalDeclId, ModuleScope,
    ParamDecl, RawImport, RawRef, RawRefKind, ReceiverExpr, TypeExpr,
};

const QUERIES: &str = include_str!("queries.scm");

/// Go predeclared functions: names no workspace item can have.
const BUILTINS: &[&str] = &[
    "append", "cap", "clear", "close", "complex", "copy", "delete", "imag", "len", "make", "max",
    "min", "new", "panic", "print", "println", "real", "recover",
];

static GO_QUERY: LazyLock<CompiledQuery> =
    LazyLock::new(|| CompiledQuery::new(&tree_sitter_go::LANGUAGE.into(), QUERIES));

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(go_parser());
}

fn go_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .expect("the bundled Go grammar loads");
    parser
}

fn parse(source: &str) -> Option<Tree> {
    PARSER.with(|parser| parser.borrow_mut().parse(source, None))
}

fn empty_summary(file: &DiscoveredFile, lines: usize) -> FileSummary {
    FileSummary {
        file_path: file.rel_path.clone(),
        module_id: file.module_id,
        language: Language::Go,
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
        "decl" => "decl",
        "field" => "field",
        "interface_method" => "interface_method",
        "function_body" => "function_body",
        "local" => "local",
        "local_spec" => "local_spec",
        "computes" => "computes",
        "assignment" => "assignment",
        "unary" => "unary",
        "call" => "call",
        "composite" => "composite",
        "selector" => "selector",
        "type_ref" => "type_ref",
        "type_path_ref" => "type_path_ref",
        _ => return None,
    })
}

fn collect_matches<'t>(query: &CompiledQuery, root: Node<'t>, source: &str) -> Matches<'t> {
    let mut matches = Matches::default();
    query.for_each_match(root, source.as_bytes(), |view| {
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
    let Some(tree) = parse(source) else {
        return empty_summary(file, line_count(source));
    };
    let matches = collect_matches(&GO_QUERY, tree.root_node(), source);
    let mut extractor = Extractor {
        syntax: Syntax::new(source),
        file,
        summary: empty_summary(file, line_count(source)),
        index: ScopeIndex::new(),
        bodies: Vec::new(),
        body_ranges: HashMap::new(),
        declared_bodies: HashSet::new(),
        receivers: HashMap::new(),
        facts: HashMap::new(),
        refs: Vec::new(),
    };
    extractor.declarations(&matches);
    extractor.skip_nested_bodies(&matches);
    extractor.index.finalize();
    extractor.references_and_facts(&matches);
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
    /// Receiver variable of each method, which Go names instead of `self`.
    receivers: HashMap<LocalDeclId, String>,
    facts: HashMap<LocalDeclId, BodyFacts>,
    refs: Vec<(usize, RawRef)>,
}

impl<'a, 't> Extractor<'a, 't> {
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

    fn push_ref(&mut self, node: Node, kind: RawRefKind, access: AccessMode) {
        let holder = self.index.holder_at(node.start_byte());
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

    /// The receiver name of the method whose body holds `byte`.
    fn receiver_name(&self, byte: usize) -> Option<String> {
        let holder = self.index.holder_at(byte)?;
        self.receivers.get(&holder).cloned()
    }

    // ----- declarations ----------------------------------------------------

    fn declarations(&mut self, m: &Matches<'t>) {
        for &node in m.nodes("decl") {
            match node.kind() {
                "import_declaration" => self.imports(node),
                "type_declaration" => self.type_declaration(node),
                "function_declaration" => self.function(node),
                "method_declaration" => self.method(node),
                "const_declaration" | "var_declaration" => self.constants(node),
                _ => {}
            }
        }
        self.index.finalize();
        for &node in m.nodes("field") {
            self.field(node);
        }
        for &node in m.nodes("interface_method") {
            self.interface_method(node);
        }
    }

    fn type_declaration(&mut self, node: Node<'t>) {
        for spec in named_children(node) {
            if matches!(spec.kind(), "type_spec" | "type_alias") {
                self.type_spec(spec);
            }
        }
    }

    fn type_spec(&mut self, node: Node<'t>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.syntax.text(name).to_owned();
        let id = self.push_decl(name.clone(), ItemKind::Type, None);
        let exported = starts_uppercase(&name);
        self.decl_mut(id).is_exported = exported;
        self.index.add_holder(node.byte_range(), id);
        if let Some(ty) = node.child_by_field_name("type")
            && matches!(ty.kind(), "struct_type" | "interface_type")
        {
            self.index.add_parent(node.byte_range(), id);
        }
    }

    fn field(&mut self, node: Node<'t>) {
        let Some(parent) = self.index.parent_decl_of(&node.byte_range()) else {
            return;
        };
        let ty = node
            .child_by_field_name("type")
            .and_then(|ty| self.syntax.type_expression(ty));
        let names: Vec<String> = named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "field_identifier")
            .map(|child| self.syntax.text(child).to_owned())
            .collect();
        // An embedded field (`Item`, `io.Reader`) declares no name of its own.
        for name in names {
            let id = self.push_decl(name.clone(), ItemKind::Field, Some(parent));
            let exported = starts_uppercase(&name);
            let decl = self.decl_mut(id);
            decl.is_exported = exported;
            decl.type_annotation = ty.clone();
            self.index.add_holder(node.byte_range(), id);
        }
    }

    fn interface_method(&mut self, node: Node<'t>) {
        let Some(parent) = self.index.parent_decl_of(&node.byte_range()) else {
            return;
        };
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.syntax.text(name).to_owned();
        let signature = self.signature_of(node, None);
        let id = self.push_decl(name.clone(), ItemKind::Function, Some(parent));
        let exported = starts_uppercase(&name);
        let decl = self.decl_mut(id);
        decl.is_exported = exported;
        decl.takes_self = true;
        decl.self_is_shared_ref = true;
        decl.signature = Some(signature);
    }

    fn function(&mut self, node: Node<'t>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.syntax.text(name_node).to_owned();
        let signature = self.signature_of(node, None);
        let id = self.push_decl(name.clone(), ItemKind::Function, None);
        let exported = starts_uppercase(&name);
        let decl = self.decl_mut(id);
        decl.is_exported = exported;
        decl.signature = Some(signature);
        self.index.add_holder(node.byte_range(), id);
        if let Some(body) = node.child_by_field_name("body") {
            self.add_body(id, body);
        }
    }

    /// A method is wrapped in an `impl` block for its receiver type, so the
    /// resolver parents it to that type wherever the method is written.
    fn method(&mut self, node: Node<'t>) {
        let Some(receiver_node) = node.child_by_field_name("receiver") else {
            return;
        };
        let Some(receiver) = self.syntax.receiver(receiver_node) else {
            return;
        };
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let receiver_name = receiver.name.map(str::to_owned);
        let is_pointer = receiver.is_pointer;
        let ty = receiver.ty;
        let name = self.syntax.text(name_node).to_owned();
        let impl_name = ty.path.last().cloned().unwrap_or_default();
        let impl_id = self.push_decl(impl_name, ItemKind::Impl, None);
        self.decl_mut(impl_id).impl_target = Some(ty.clone());
        let signature = self.signature_of(
            node,
            Some(ParamDecl {
                name: "self".to_owned(),
                ty: Some(ty),
            }),
        );
        let id = self.push_decl(name.clone(), ItemKind::Function, Some(impl_id));
        let exported = starts_uppercase(&name);
        let decl = self.decl_mut(id);
        decl.is_exported = exported;
        decl.takes_self = true;
        decl.self_is_shared_ref = !is_pointer;
        decl.signature = Some(signature);
        if let Some(receiver_name) = receiver_name {
            self.receivers.insert(id, receiver_name);
        }
        self.index.add_holder(node.byte_range(), id);
        if let Some(body) = node.child_by_field_name("body") {
            self.add_body(id, body);
        }
    }

    fn constants(&mut self, node: Node<'t>) {
        for spec in named_children(node) {
            match spec.kind() {
                "const_spec" | "var_spec" => self.constant_spec(spec),
                "var_spec_list" => {
                    for inner in named_children(spec) {
                        if inner.kind() == "var_spec" {
                            self.constant_spec(inner);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn constant_spec(&mut self, node: Node<'t>) {
        let ty = node
            .child_by_field_name("type")
            .and_then(|ty| self.syntax.type_expression(ty));
        let names: Vec<String> = named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "identifier")
            .map(|child| self.syntax.text(child).to_owned())
            .collect();
        for name in names {
            let exported = starts_uppercase(&name);
            let id = self.push_decl(name, ItemKind::Constant, None);
            let decl = self.decl_mut(id);
            decl.is_exported = exported;
            decl.type_annotation = ty.clone();
        }
    }

    fn imports(&mut self, node: Node<'t>) {
        for child in named_children(node) {
            match child.kind() {
                "import_spec" => self.import_spec(child),
                "import_spec_list" => {
                    for spec in named_children(child) {
                        if spec.kind() == "import_spec" {
                            self.import_spec(spec);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn import_spec(&mut self, node: Node<'t>) {
        let Some(path) = node.child_by_field_name("path") else {
            return;
        };
        let Some(specifier) = self.syntax.string_value(path) else {
            return;
        };
        if specifier.is_empty() {
            return;
        }
        let kind = match node.child_by_field_name("name") {
            None => {
                let alias = default_alias(&specifier);
                self.syntax.add_namespace(alias.clone());
                ImportKind::Namespace { alias }
            }
            Some(name) => match name.kind() {
                "dot" => ImportKind::Glob,
                "blank_identifier" => ImportKind::SideEffect,
                "package_identifier" => {
                    let alias = self.syntax.text(name).to_owned();
                    self.syntax.add_namespace(alias.clone());
                    ImportKind::Namespace { alias }
                }
                _ => return,
            },
        };
        let path_segments = specifier
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect();
        self.summary.imports.push(RawImport {
            scope: ModuleScope::FileRoot,
            path_segments,
            raw_specifier: Some(specifier),
            kind,
            is_reexport: false,
        });
    }

    fn skip_nested_bodies(&mut self, m: &Matches<'t>) {
        for &body in m.nodes("function_body") {
            if !self.declared_bodies.contains(&body.start_byte()) {
                self.index.add_skipped_span(body.byte_range());
            }
        }
    }

    // ----- references and function-body facts -------------------------------

    fn references_and_facts(&mut self, m: &Matches<'t>) {
        for &node in m.nodes("local") {
            self.short_var(node);
        }
        for &node in m.nodes("local_spec") {
            self.var_spec(node);
        }
        for &node in m.nodes("computes") {
            self.mark_op(node);
        }
        for &node in m.nodes("assignment") {
            let operator = node
                .child_by_field_name("operator")
                .map(|operator| self.syntax.text(operator));
            if operator != Some("=") {
                self.mark_op(node);
            }
        }
        for &node in m.nodes("unary") {
            let operator = node
                .child_by_field_name("operator")
                .map(|operator| self.syntax.text(operator));
            if matches!(operator, Some("!" | "-" | "+" | "^" | "<-")) {
                self.mark_op(node);
            }
        }
        for &node in m.nodes("call") {
            self.count_call(node);
            self.call_ref(node);
        }
        for &node in m.nodes("composite") {
            self.composite_ref(node);
        }
        for &node in m.nodes("selector") {
            self.selector(node);
        }
        for &node in m.nodes("type_ref") {
            self.type_ref(node);
        }
        for &node in m.nodes("type_path_ref") {
            self.path_ref(node);
        }
    }

    fn short_var(&mut self, node: Node<'t>) {
        let Some(owner) = self.body_owner(node) else {
            return;
        };
        let (Some(left), Some(right)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) else {
            return;
        };
        let self_name = self.receivers.get(&owner).cloned();
        let names = named_children(left);
        let values = named_children(right);
        let mut locals = Vec::new();
        for (index, name) in names.iter().enumerate() {
            if name.kind() != "identifier" {
                continue;
            }
            let value = if values.len() == names.len() {
                values.get(index)
            } else if index == 0 && values.len() == 1 {
                values.first()
            } else {
                None
            };
            let init = value
                .copied()
                .and_then(|value| self.syntax.init_expr(value, self_name.as_deref()));
            locals.push(LocalBinding {
                name: self.syntax.text(*name).to_owned(),
                declared_type: None,
                init,
            });
        }
        self.facts.entry(owner).or_default().locals.extend(locals);
    }

    fn var_spec(&mut self, node: Node<'t>) {
        let Some(owner) = self.body_owner(node) else {
            return;
        };
        let ty = node
            .child_by_field_name("type")
            .and_then(|ty| self.syntax.type_expression(ty));
        let values = node
            .child_by_field_name("value")
            .map(named_children)
            .unwrap_or_default();
        let self_name = self.receivers.get(&owner).cloned();
        let names: Vec<Node> = named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "identifier")
            .collect();
        let mut locals = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let init = values
                .get(index)
                .copied()
                .and_then(|value| self.syntax.init_expr(value, self_name.as_deref()));
            locals.push(LocalBinding {
                name: self.syntax.text(*name).to_owned(),
                declared_type: ty.clone(),
                init,
            });
        }
        self.facts.entry(owner).or_default().locals.extend(locals);
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

    fn selector(&mut self, node: Node<'t>) {
        let Some(operand) = node.child_by_field_name("operand") else {
            return;
        };
        let Some(field) = node.child_by_field_name("field") else {
            return;
        };
        // The inner selectors of a chain are covered by the outermost one.
        let inner = node.parent().is_some_and(|parent| {
            parent.kind() == "selector_expression"
                && parent
                    .child_by_field_name("operand")
                    .is_some_and(|operand| operand.id() == node.id())
        });
        if inner {
            return;
        }
        let member = self.syntax.text(field).to_owned();
        let is_call = node.parent().is_some_and(|parent| {
            parent.kind() == "call_expression"
                && parent
                    .child_by_field_name("function")
                    .is_some_and(|function| function.id() == node.id())
        });
        let self_name = self.receiver_name(node.start_byte());
        let receiver = self.syntax.receiver_of(operand, self_name.as_deref());
        let write = self.syntax.in_write_position(node);
        let access = if write || self.syntax.reaches_into(node) {
            AccessMode::Work
        } else if is_call {
            AccessMode::MethodCall
        } else {
            AccessMode::CopyOrRead
        };
        self.selector_facts(node, &receiver, &member, is_call, write);
        self.push_ref(
            node,
            RawRefKind::MemberAccess {
                receiver,
                member,
                is_call,
            },
            access,
        );
    }

    fn selector_facts(
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
            facts.reaches = true;
        } else if write {
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

    fn call_ref(&mut self, node: Node<'t>) {
        let Some(function) = node.child_by_field_name("function") else {
            return;
        };
        // Selectors are member accesses; conversions and generic
        // instantiations are type references.
        if function.kind() != "identifier" {
            return;
        }
        let name = self.syntax.text(function).to_owned();
        if !BUILTINS.contains(&name.as_str()) {
            self.push_ref(
                function,
                RawRefKind::Path(vec![name]),
                AccessMode::CopyOrRead,
            );
        }
    }

    fn composite_ref(&mut self, node: Node<'t>) {
        let Some(ty) = node.child_by_field_name("type") else {
            return;
        };
        let Some(path) = self.syntax.type_path(ty) else {
            return;
        };
        self.push_ref(node, RawRefKind::Path(path), AccessMode::CopyOrRead);
    }

    fn type_ref(&mut self, node: Node<'t>) {
        if self.syntax.is_declaration_name(node) {
            return;
        }
        let Some(parent) = node.parent() else {
            return;
        };
        match parent.kind() {
            // A qualified path is emitted in one piece by `path_ref`.
            "qualified_type" => return,
            // A literal's type is emitted by `composite_ref`.
            "composite_literal" => return,
            // Type parameters name themselves and constrain their own types.
            "type_parameter_declaration" | "type_parameter_list" => return,
            _ => {}
        }
        self.push_ref(
            node,
            RawRefKind::Path(vec![self.syntax.text(node).to_owned()]),
            AccessMode::CopyOrRead,
        );
    }

    fn path_ref(&mut self, node: Node<'t>) {
        if node
            .parent()
            .is_some_and(|parent| parent.kind() == "composite_literal")
        {
            return;
        }
        let path = self.syntax.path_segments(node);
        if !path.is_empty() {
            self.push_ref(node, RawRefKind::Path(path), AccessMode::CopyOrRead);
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

    fn signature_of(&self, node: Node, receiver: Option<ParamDecl>) -> FunctionSig {
        let mut params: Vec<ParamDecl> = receiver.into_iter().collect();
        if let Some(parameters) = node.child_by_field_name("parameters") {
            params.extend(self.parameters_of(parameters));
        }
        let return_type = node
            .child_by_field_name("result")
            .and_then(|result| self.result_type(result));
        FunctionSig {
            params,
            return_type,
        }
    }

    fn parameters_of(&self, node: Node) -> Vec<ParamDecl> {
        let mut params = Vec::new();
        for parameter in named_children(node) {
            let ty = parameter
                .child_by_field_name("type")
                .and_then(|ty| self.syntax.type_expression(ty));
            let names: Vec<String> = named_children(parameter)
                .into_iter()
                .filter(|child| child.kind() == "identifier")
                .map(|child| self.syntax.text(child).to_owned())
                .collect();
            match names.as_slice() {
                [] => params.push(ParamDecl {
                    name: "_".to_owned(),
                    ty,
                }),
                names => {
                    for name in names {
                        params.push(ParamDecl {
                            name: name.clone(),
                            ty: ty.clone(),
                        });
                    }
                }
            }
        }
        params
    }

    /// A function's result: the first type it returns that is not `error`.
    fn result_type(&self, node: Node) -> Option<TypeExpr> {
        if node.kind() != "parameter_list" {
            return self.syntax.type_expression(node);
        }
        named_children(node).into_iter().find_map(|parameter| {
            let ty = parameter
                .child_by_field_name("type")
                .and_then(|ty| self.syntax.type_expression(ty))?;
            (ty.path.last().map(String::as_str) != Some("error")).then_some(ty)
        })
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
            let self_name = self.receivers.get(&id).cloned();
            let forwarding = self.forwarding_candidate(body, &params, self_name.as_deref());
            let computes = if forwarding.is_some() {
                facts.ops
            } else {
                facts.ops || facts.reaches || facts.calls > 0
            };
            let takes_self = self.summary.decls[id.0 as usize].takes_self;
            let accessor = if takes_self && !computes {
                Self::accessor_candidate(&facts)
            } else {
                None
            };
            self.summary.decls[id.0 as usize].body_summary = Some(FunctionBodySummary {
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
            // Go has no borrow syntax: only a write is lent or written here,
            // and returning an object is handled by the resolver.
            let lent = op.write;
            if let Some(existing) = ops
                .iter_mut()
                .find(|existing| existing.field_name == op.field)
            {
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

    fn forwarding_candidate(
        &self,
        body: Node,
        params: &[String],
        self_name: Option<&str>,
    ) -> Option<ForwardingCandidate> {
        let call = self.syntax.single_call(body)?;
        let function = call.child_by_field_name("function")?;
        let arguments = call.child_by_field_name("arguments")?;
        for argument in named_children(arguments) {
            let value = if argument.kind() == "variadic_argument" {
                argument.named_child(0)?
            } else {
                argument
            };
            if value.kind() != "identifier"
                || !params.iter().any(|param| param == self.syntax.text(value))
            {
                return None;
            }
        }
        let (callee, self_fields) = match function.kind() {
            "identifier" => (
                CalleeExpr::Path(vec![self.syntax.text(function).to_owned()]),
                Vec::new(),
            ),
            "selector_expression" => {
                let operand = function.child_by_field_name("operand")?;
                let field = function.child_by_field_name("field")?;
                let receiver = self.syntax.receiver_of(operand, self_name);
                let self_fields = match &receiver {
                    ReceiverExpr::SelfValue => Vec::new(),
                    ReceiverExpr::FieldChain { base, fields }
                        if matches!(**base, ReceiverExpr::SelfValue) =>
                    {
                        fields.clone()
                    }
                    ReceiverExpr::Local(name) if params.iter().any(|param| param == name) => {
                        Vec::new()
                    }
                    _ => return None,
                };
                (
                    CalleeExpr::Method {
                        receiver,
                        method: self.syntax.text(field).to_owned(),
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
}

/// The package name an unaliased import binds: its last path segment, past a
/// `/v2`..`/v9` major-version suffix.
fn default_alias(specifier: &str) -> String {
    let segments: Vec<&str> = specifier
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let mut index = segments.len().saturating_sub(1);
    if index > 0 && is_major_version(segments[index]) {
        index -= 1;
    }
    segments.get(index).copied().unwrap_or(specifier).to_owned()
}

fn is_major_version(segment: &str) -> bool {
    matches!(
        segment.strip_prefix('v'),
        Some("2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
    )
}
#[cfg(test)]
mod tests;
