//! Reference, local-binding, computation, macro, and wrapper body analysis
//! for the Rust Tree-sitter extractor.

use std::collections::HashSet;

use tree_sitter::Node;

use crate::treesitter::summary::{
    AccessMode, AccessorCandidate, AccessorFieldOp, CalleeExpr, ForwardingCandidate,
    FunctionBodySummary, LocalBinding, LocalDeclId, RawRef, RawRefKind, ReceiverExpr,
};

use super::syntax::{PASSES, is_expression, unwrap_argument};
use super::{Extractor, Nodes, RefSite, parse_rust};

impl Extractor<'_> {
    // ----------------------------------------------------------- references

    pub(super) fn emit_refs(&mut self, sites: &[RefSite<'_>]) {
        let mut sites = sites.to_vec();
        sites.sort_by_key(|site| site.node().start_byte());
        for site in sites {
            match site {
                RefSite::Type(node) if !self.type_name_is_definition(node) => {
                    self.path_ref(node, vec![self.text(node).to_owned()]);
                }
                RefSite::TypePath(node) if !self.in_use_declaration(node) => {
                    self.path_ref(node, self.expr_path(node));
                }
                RefSite::Path(node) if !self.path_handled_elsewhere(node) => {
                    self.path_ref(node, self.expr_path(node));
                }
                RefSite::Struct(node) => self.struct_expression(node),
                RefSite::Call(node) => self.call_expression(node),
                RefSite::Field(node) => self.field_expression(node),
                RefSite::Identifier(node)
                    if self.identifier_is_use(node) && !self.is_local(node) =>
                {
                    self.path_ref(node, vec![self.text(node).to_owned()]);
                }
                _ => {}
            }
        }
    }

    /// A type name declared here rather than written here.
    fn type_name_is_definition(&self, node: Node<'_>) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        match parent.kind() {
            "struct_item"
            | "union_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "associated_type"
            | "use_declaration"
            | "scoped_type_identifier"
            | "struct_expression" => true,
            "type_binding" => parent.child_by_field_name("name") == Some(node),
            _ => false,
        }
    }

    fn in_use_declaration(&self, node: Node<'_>) -> bool {
        let mut current = node.parent();
        while let Some(parent) = current {
            if parent.kind() == "use_declaration" {
                return true;
            }
            current = parent.parent();
        }
        false
    }

    fn path_handled_elsewhere(&self, node: Node<'_>) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        match parent.kind() {
            "call_expression" => parent.child_by_field_name("function") == Some(node),
            "macro_invocation"
            | "generic_function"
            | "scoped_identifier"
            | "scoped_type_identifier" => true,
            "struct_expression" => parent.child_by_field_name("name") == Some(node),
            _ => self.in_use_declaration(node),
        }
    }

    /// An identifier in expression position, not a name some syntax declares.
    fn identifier_is_use(&self, node: Node<'_>) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        match parent.kind() {
            "let_declaration" | "for_expression" | "let_condition" => {
                parent.child_by_field_name("pattern") != Some(node)
            }
            "field_expression"
            | "arguments"
            | "block"
            | "binary_expression"
            | "unary_expression"
            | "reference_expression"
            | "assignment_expression"
            | "compound_assignment_expr"
            | "index_expression"
            | "array_expression"
            | "tuple_expression"
            | "if_expression"
            | "match_expression"
            | "while_expression"
            | "loop_expression"
            | "return_expression"
            | "break_expression"
            | "expression_statement"
            | "try_expression"
            | "await_expression"
            | "type_cast_expression"
            | "range_expression"
            | "field_initializer"
            | "unsafe_block"
            | "struct_expression"
            | "parenthesized_expression" => true,
            _ => false,
        }
    }

    fn is_local(&self, node: Node<'_>) -> bool {
        if node.kind() != "identifier" {
            return false;
        }
        let Some(holder) = self.scope.holder_at(node.start_byte() + self.offset) else {
            return false;
        };
        let name = self.text(node);
        self.params
            .get(&holder)
            .is_some_and(|params| params.contains(name))
            || self
                .local_names
                .get(&holder)
                .is_some_and(|names| names.contains(name))
    }

    fn struct_expression(&mut self, node: Node<'_>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let path = self.expr_path(name);
        if !path.is_empty() {
            self.path_ref(name, path.clone());
        }
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        let initializers: Vec<Node<'_>> = body.named_children(&mut cursor).collect();
        for initializer in initializers {
            let field = match initializer.kind() {
                "field_initializer" => initializer.child_by_field_name("field"),
                "shorthand_field_initializer" => initializer.named_child(0),
                _ => None,
            };
            if let Some(field) = field {
                let member = self.text(field).to_owned();
                self.member_ref(
                    field,
                    ReceiverExpr::Path(path.clone()),
                    member,
                    false,
                    AccessMode::CopyOrRead,
                );
            }
        }
    }

    fn field_expression(&mut self, node: Node<'_>) {
        let Some(parent) = node.parent() else {
            return;
        };
        // A method call names its member through its call.
        if parent.kind() == "call_expression"
            && parent.child_by_field_name("function") == Some(node)
        {
            return;
        }
        let (Some(receiver), Some(field)) = (
            node.child_by_field_name("value"),
            node.child_by_field_name("field"),
        ) else {
            return;
        };
        let member = self.text(field).to_owned();
        let receiver = self.receiver_expr(receiver);
        let mode = self.access_mode(node);
        self.member_ref(node, receiver.clone(), member.clone(), false, mode);
        if receiver == ReceiverExpr::SelfValue {
            self.field_op(node, &member);
        }
    }

    fn call_expression(&mut self, node: Node<'_>) {
        let Some(function) = node.child_by_field_name("function") else {
            return;
        };
        let mode = self.access_mode(node);
        match function.kind() {
            "field_expression" => {
                let (Some(receiver), Some(field)) = (
                    function.child_by_field_name("value"),
                    function.child_by_field_name("field"),
                ) else {
                    return;
                };
                let member = self.text(field).to_owned();
                self.member_ref(
                    function,
                    self.receiver_expr(receiver),
                    member.clone(),
                    true,
                    mode,
                );
                if !self.passes(&member, node) {
                    self.mark_compute(node);
                }
            }
            "identifier"
            | "scoped_identifier"
            | "generic_function"
            | "scoped_type_identifier_in_expression_position" => {
                let path = self.expr_path(function);
                if path.is_empty() {
                    return;
                }
                let name = path.last().cloned().unwrap_or_default();
                self.path_ref(function, path);
                if !PASSES.contains(&name.as_str()) && !self.is_param(node, &name) {
                    self.mark_compute(node);
                }
            }
            _ => self.mark_compute(node),
        }
    }

    fn path_ref(&mut self, node: Node<'_>, path: Vec<String>) {
        if path.is_empty() {
            return;
        }
        let at = node.start_byte() + self.offset;
        if self.scope.is_skipped_at(at) {
            return;
        }
        self.summary.refs.push(RawRef {
            line: self.line(node),
            scope: self.scope.module_at(at),
            holder: self.scope.holder_at(at),
            kind: RawRefKind::Path(path),
            access: AccessMode::CopyOrRead,
            is_test: self.scope.is_test_at(at),
        });
    }

    fn member_ref(
        &mut self,
        node: Node<'_>,
        receiver: ReceiverExpr,
        member: String,
        is_call: bool,
        access: AccessMode,
    ) {
        let at = node.start_byte() + self.offset;
        if self.scope.is_skipped_at(at) {
            return;
        }
        self.summary.refs.push(RawRef {
            line: self.line(node),
            scope: self.scope.module_at(at),
            holder: self.scope.holder_at(at),
            kind: RawRefKind::MemberAccess {
                receiver,
                member,
                is_call,
            },
            access,
            is_test: self.scope.is_test_at(at),
        });
    }

    /// Record how a method's body reaches one of its own fields.
    fn field_op(&mut self, node: Node<'_>, member: &str) {
        let Some(holder) = self.scope.holder_at(node.start_byte() + self.offset) else {
            return;
        };
        if !self.takes_self.get(&holder).copied().unwrap_or(false) {
            return;
        }
        let assigned = self.is_assigned(node);
        let lent = assigned || self.borrow_kind(node).is_some();
        let op = self
            .ops
            .entry(holder)
            .or_default()
            .entry(member.to_owned())
            .or_insert(AccessorFieldOp {
                field_name: member.to_owned(),
                is_lent_or_written: false,
                is_write: false,
            });
        op.is_write |= assigned;
        op.is_lent_or_written |= lent;
    }

    // --------------------------------------------------------------- bodies

    pub(super) fn collect_locals(&mut self, locals: &[Node<'_>]) {
        let mut locals = locals.to_vec();
        locals.sort_by_key(|node| node.start_byte());
        for node in locals {
            let at = node.start_byte() + self.offset;
            if self.scope.is_skipped_at(at) {
                continue;
            }
            let Some(holder) = self.scope.holder_at(at) else {
                continue;
            };
            let Some(pattern) = node.child_by_field_name("pattern") else {
                continue;
            };
            let names = self.pattern_names(pattern);
            self.local_names
                .entry(holder)
                .or_default()
                .extend(names.iter().cloned());
            if names.len() != 1
                || !matches!(pattern.kind(), "identifier" | "mut_pattern" | "ref_pattern")
            {
                continue;
            }
            let declared_type = node
                .child_by_field_name("type")
                .and_then(|ty| self.type_expr(ty));
            let init = node
                .child_by_field_name("value")
                .and_then(|value| self.init_expr(value));
            self.locals.entry(holder).or_default().push(LocalBinding {
                name: names[0].clone(),
                declared_type,
                init,
            });
        }
    }

    fn pattern_names(&self, pattern: Node<'_>) -> Vec<String> {
        let mut names = Vec::new();
        let mut stack = vec![pattern];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "identifier" | "metavariable" => names.push(self.text(node).to_owned()),
                "mut_pattern" | "ref_pattern" | "captured_pattern" | "reference_pattern" => {
                    let mut cursor = node.walk();
                    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                    stack.extend(children);
                }
                _ => {}
            }
        }
        names
    }

    pub(super) fn emit_computes(&mut self, markers: &[Node<'_>]) {
        let mut markers = markers.to_vec();
        markers.sort_by_key(|node| node.start_byte());
        for node in markers {
            match node.kind() {
                "binary_expression"
                | "compound_assignment_expr"
                | "if_expression"
                | "match_expression"
                | "while_expression"
                | "for_expression"
                | "loop_expression" => {
                    self.mark_compute(node);
                }
                "unary_expression" => {
                    if matches!(self.text(node).chars().next(), Some('!' | '-')) {
                        self.mark_compute(node);
                    }
                }
                "macro_invocation" => {
                    if self.macro_name(node) != "vec" {
                        self.mark_compute(node);
                    }
                    self.macro_arguments(node);
                }
                _ => {}
            }
        }
    }

    fn macro_name(&self, node: Node<'_>) -> String {
        node.child_by_field_name("macro")
            .map(|name| {
                self.text(name)
                    .rsplit("::")
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_default()
    }

    /// Read a macro's arguments with the same query: a token tree is a list of
    /// expressions once parsed on its own, at its own byte offsets.
    fn macro_arguments(&mut self, node: Node<'_>) {
        let source = self.source;
        let mut cursor = node.walk();
        let Some(arguments) = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "token_tree")
        else {
            return;
        };
        let range = arguments.byte_range();
        let Some(text) = source.get(range.clone()) else {
            return;
        };
        let Some(tree) = parse_rust(text) else {
            return;
        };
        let saved = (self.offset, self.line_offset);
        self.offset = range.start;
        self.line_offset = arguments.start_position().row as u32;
        let nodes = Nodes::collect(tree.root_node(), text.as_bytes());
        self.emit_refs(&nodes.refs);
        self.emit_computes(&nodes.computes);
        self.offset = saved.0;
        self.line_offset = saved.1;
    }

    fn mark_compute(&mut self, node: Node<'_>) {
        if let Some(holder) = self.scope.holder_at(node.start_byte() + self.offset) {
            self.computes.insert(holder);
        }
    }

    fn is_param(&self, node: Node<'_>, name: &str) -> bool {
        self.scope
            .holder_at(node.start_byte() + self.offset)
            .and_then(|holder| self.params.get(&holder))
            .is_some_and(|params| params.contains(name))
    }

    /// Whether a method call only passes its values on.
    fn passes(&self, name: &str, node: Node<'_>) -> bool {
        PASSES.contains(&name) || self.is_param(node, name)
    }

    pub(super) fn emit_bodies(&mut self, functions: &[(LocalDeclId, Node<'_>)]) {
        for &(id, node) in functions {
            let takes_self = self.takes_self.get(&id).copied().unwrap_or(false);
            let computes = self.computes.contains(&id);
            let ops: Vec<AccessorFieldOp> = self
                .ops
                .get(&id)
                .map(|ops| ops.values().cloned().collect())
                .unwrap_or_default();
            let accessor =
                (takes_self && !computes && !ops.is_empty()).then_some(AccessorCandidate { ops });
            let forwarding = node
                .child_by_field_name("body")
                .and_then(|body| self.forwarding(body, id, takes_self));
            let locals = self.locals.remove(&id).unwrap_or_default();
            self.summary.decls[id.0 as usize].body_summary = Some(FunctionBodySummary {
                locals,
                computes,
                forwarding_candidate: forwarding,
                accessor_candidate: accessor,
            });
        }
    }

    /// A body that is one call passing only its own parameters on.
    fn forwarding(
        &self,
        block: Node<'_>,
        id: LocalDeclId,
        takes_self: bool,
    ) -> Option<ForwardingCandidate> {
        let mut cursor = block.walk();
        let statements: Vec<Node<'_>> = block
            .named_children(&mut cursor)
            .filter(|node| {
                !matches!(
                    node.kind(),
                    "line_comment" | "block_comment" | "attribute_item"
                )
            })
            .collect();
        let expression = match statements.as_slice() {
            [only] if only.kind() == "expression_statement" => only.named_child(0)?,
            [only] if is_expression(*only) => *only,
            _ => return None,
        };
        if expression.kind() != "call_expression" {
            return None;
        }
        let callee = expression.child_by_field_name("function")?;
        let arguments = expression.child_by_field_name("arguments")?;
        let params = self.params.get(&id);
        let mut cursor = arguments.walk();
        let arguments: Vec<Node<'_>> = arguments.named_children(&mut cursor).collect();
        for argument in arguments {
            let argument = unwrap_argument(argument);
            match argument.kind() {
                "identifier"
                    if params.is_some_and(|params| params.contains(self.text(argument))) => {}
                "self" if takes_self => {}
                _ => return None,
            }
        }
        if callee.kind() == "field_expression" {
            let method = callee.child_by_field_name("field")?;
            let receiver = callee.child_by_field_name("value")?;
            let (receiver, self_fields) = self.forwarding_receiver(receiver, params)?;
            return Some(ForwardingCandidate {
                callee: CalleeExpr::Method {
                    receiver,
                    method: self.text(method).to_owned(),
                },
                self_fields,
            });
        }
        let path = self.expr_path(callee);
        (!path.is_empty()).then(|| ForwardingCandidate {
            callee: CalleeExpr::Path(path),
            self_fields: Vec::new(),
        })
    }

    fn forwarding_receiver(
        &self,
        node: Node<'_>,
        params: Option<&HashSet<String>>,
    ) -> Option<(ReceiverExpr, Vec<String>)> {
        let node = unwrap_argument(node);
        match node.kind() {
            "self" => Some((ReceiverExpr::SelfValue, Vec::new())),
            "identifier" => {
                let name = self.text(node);
                let passed = params.is_some_and(|params| params.contains(name));
                passed.then(|| (ReceiverExpr::Local(name.to_owned()), Vec::new()))
            }
            "field_expression" => {
                let mut fields = Vec::new();
                let mut current = node;
                loop {
                    let field = current.child_by_field_name("field")?;
                    fields.push(self.text(field).to_owned());
                    let value = current.child_by_field_name("value")?;
                    match value.kind() {
                        "self" => {
                            fields.reverse();
                            return Some((ReceiverExpr::SelfValue, fields));
                        }
                        "field_expression" => current = value,
                        _ => return None,
                    }
                }
            }
            _ => None,
        }
    }
}
