//! Rust-specific syntax helpers the `.scm` query layer cannot express cleanly:
//! syntactic `AccessMode`, receiver/callee chains, `TypeExpr` generics, and paths.

use std::ops::Range;

use tree_sitter::Node;

use crate::treesitter::summary::{
    AccessMode, CalleeExpr, InitExpr, ModuleScope, ReceiverExpr, TypeExpr,
};

use super::Extractor;

impl Extractor<'_> {
    // --------------------------------------------------------- access modes

    /// What the code does with the value at "node": the only syntactic fact a
    /// reference carries; the resolver weighs it against the field's type.
    pub(super) fn access_mode(&self, node: Node<'_>) -> AccessMode {
        if self.is_assigned(node) {
            return AccessMode::Work;
        }
        match self.borrow_kind(node) {
            Some(true) => return AccessMode::Work,
            Some(false) => return AccessMode::SharedBorrow,
            None => {}
        }
        if self.is_indexed(node) {
            return AccessMode::Work;
        }
        if let Some((method, no_args)) = self.method_call_of(node) {
            return method_mode(&method, no_args);
        }
        AccessMode::CopyOrRead
    }

    /// What the code does with the result of a call expression, used for
    /// lending getters: `*queue.limit()` and `queue.limit().0` read a copy,
    /// while `let items = queue.items()` holds the borrow.
    pub(super) fn call_access_mode(&self, node: Node<'_>, is_method: bool) -> AccessMode {
        if !is_method {
            return AccessMode::Work;
        }
        if self.is_assigned(node) {
            return AccessMode::Work;
        }
        match self.borrow_kind(node) {
            Some(true) => return AccessMode::Work,
            Some(false) => return AccessMode::SharedBorrow,
            None => {}
        }
        if self.is_indexed(node) {
            return AccessMode::Work;
        }
        let mut tail = node;
        let mut stepped = false;
        loop {
            if let Some((method, no_args)) = self.method_call_of(tail) {
                return method_mode(&method, no_args);
            }
            match tail.parent() {
                Some(parent)
                    if parent.kind() == "field_expression"
                        && parent.child_by_field_name("value") == Some(tail) =>
                {
                    tail = parent;
                    stepped = true;
                }
                _ => break,
            }
        }
        if stepped || self.is_dereferenced(node) {
            return AccessMode::CopyOrRead;
        }
        AccessMode::SharedBorrow
    }

    fn is_dereferenced(&self, mut node: Node<'_>) -> bool {
        while let Some(parent) = node.parent() {
            match parent.kind() {
                "parenthesized_expression" => node = parent,
                "unary_expression" => {
                    return self.text(parent).trim_start().starts_with('*');
                }
                _ => return false,
            }
        }
        false
    }

    pub(super) fn is_assigned(&self, node: Node<'_>) -> bool {
        let place = place_end(node);
        place.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "assignment_expression" | "compound_assignment_expr"
            ) && parent.child_by_field_name("left") == Some(place)
        })
    }

    fn is_indexed(&self, node: Node<'_>) -> bool {
        let mut current = node;
        while let Some(parent) = place_step(current) {
            if parent.kind() == "index_expression" {
                return true;
            }
            current = parent;
        }
        false
    }

    /// Some(true) for a mutable borrow, Some(false) for a shared one.
    pub(super) fn borrow_kind(&self, node: Node<'_>) -> Option<bool> {
        let parent = place_end(node).parent()?;
        if parent.kind() != "reference_expression" {
            return None;
        }
        let mut cursor = parent.walk();
        let mutable = parent
            .children(&mut cursor)
            .any(|child| child.kind() == "mutable_specifier");
        Some(mutable)
    }

    /// The method call that takes "node" as its receiver: queue.items.len().
    fn method_call_of(&self, node: Node<'_>) -> Option<(String, bool)> {
        let parent = node.parent()?;
        if parent.kind() != "field_expression" || parent.child_by_field_name("value") != Some(node)
        {
            return None;
        }
        let call = parent.parent()?;
        if call.kind() != "call_expression" || call.child_by_field_name("function") != Some(parent)
        {
            return None;
        }
        let method = parent.child_by_field_name("field")?;
        let no_args = call
            .child_by_field_name("arguments")
            .is_none_or(|arguments| {
                let mut cursor = arguments.walk();
                arguments.named_children(&mut cursor).next().is_none()
            });
        Some((self.text(method).to_owned(), no_args))
    }

    // ------------------------------------------------------------ receivers

    pub(super) fn receiver_expr(&self, node: Node<'_>) -> ReceiverExpr {
        match node.kind() {
            "self" => ReceiverExpr::SelfValue,
            "identifier" => ReceiverExpr::Local(self.text(node).to_owned()),
            "scoped_identifier"
            | "scoped_type_identifier"
            | "scoped_type_identifier_in_expression_position"
            | "generic_function" => ReceiverExpr::Path(self.expr_path(node)),
            "field_expression" => match self.field_chain(node) {
                Some((base, fields)) => ReceiverExpr::FieldChain {
                    base: Box::new(base),
                    fields,
                },
                None => ReceiverExpr::Unknown,
            },
            "call_expression" => {
                let callee = node
                    .child_by_field_name("function")
                    .map(|function| self.callee_expr(function))
                    .unwrap_or(CalleeExpr::Path(Vec::new()));
                ReceiverExpr::CallResult {
                    callee: Box::new(callee),
                }
            }
            "parenthesized_expression"
            | "reference_expression"
            | "try_expression"
            | "await_expression"
            | "unary_expression" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .next()
                    .map(|inner| self.receiver_expr(inner))
                    .unwrap_or(ReceiverExpr::Unknown)
            }
            _ => ReceiverExpr::Unknown,
        }
    }

    pub(super) fn callee_expr(&self, node: Node<'_>) -> CalleeExpr {
        if node.kind() == "field_expression"
            && let (Some(receiver), Some(field)) = (
                node.child_by_field_name("value"),
                node.child_by_field_name("field"),
            )
        {
            return CalleeExpr::Method {
                receiver: self.receiver_expr(receiver),
                method: self.text(field).to_owned(),
            };
        }
        CalleeExpr::Path(self.expr_path(node))
    }

    pub(super) fn field_chain(&self, node: Node<'_>) -> Option<(ReceiverExpr, Vec<String>)> {
        let mut fields = Vec::new();
        let mut current = node;
        loop {
            match current.kind() {
                "field_expression" => {
                    let field = current.child_by_field_name("field")?;
                    fields.push(self.text(field).to_owned());
                    current = current.child_by_field_name("value")?;
                }
                _ => {
                    fields.reverse();
                    return Some((self.receiver_expr(current), fields));
                }
            }
        }
    }

    pub(super) fn init_expr(&self, node: Node<'_>) -> Option<InitExpr> {
        match node.kind() {
            "struct_expression" => {
                let path = node.child_by_field_name("name")?;
                Some(InitExpr::StructLiteral(self.expr_path(path)))
            }
            "call_expression" => {
                let function = node.child_by_field_name("function")?;
                Some(InitExpr::Call(self.callee_expr(function)))
            }
            "field_expression" => {
                let (base, fields) = self.field_chain(node)?;
                Some(InitExpr::FieldChain { base, fields })
            }
            "identifier" => Some(InitExpr::Var(self.text(node).to_owned())),
            "reference_expression" | "parenthesized_expression" | "try_expression" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .next()
                    .and_then(|inner| self.init_expr(inner))
            }
            _ => None,
        }
    }

    // ---------------------------------------------------------------- types

    pub(super) fn type_expr(&self, node: Node<'_>) -> Option<TypeExpr> {
        match node.kind() {
            "type_identifier" | "identifier" | "primitive_type" | "metavariable" => {
                Some(TypeExpr::simple(self.text(node)))
            }
            "scoped_type_identifier" | "scoped_identifier" => {
                Some(TypeExpr::from_path(self.expr_path(node)))
            }
            "generic_type" | "generic_type_with_turbofish" => {
                let mut ty = self.type_expr(
                    node.child_by_field_name("type")
                        .or_else(|| node.child_by_field_name("function"))?,
                )?;
                if let Some(arguments) = node.child_by_field_name("type_arguments") {
                    ty.args = self.type_arguments(arguments);
                }
                Some(ty)
            }
            "reference_type" => {
                let mut ty = self.type_expr(node.child_by_field_name("type")?)?;
                let mutable = has_child(node, "mutable_specifier");
                ty.is_shared_ref = !mutable;
                ty.is_mut_ref = mutable;
                Some(ty)
            }
            "pointer_type" => self.type_expr(node.child_by_field_name("type")?),
            "qualified_type" => self.type_expr(node.child_by_field_name("alias")?),
            "array_type" => Some(TypeExpr {
                path: Vec::new(),
                args: vec![self.type_expr(node.child_by_field_name("element")?)?],
                is_shared_ref: false,
                is_mut_ref: false,
            }),
            "tuple_type" | "function_type" => {
                let mut args = Vec::new();
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                for child in children {
                    let ty = child
                        .child_by_field_name("type")
                        .and_then(|ty| self.type_expr(ty))
                        .or_else(|| self.type_expr(child));
                    if let Some(ty) = ty {
                        args.push(ty);
                    }
                }
                Some(TypeExpr {
                    path: Vec::new(),
                    args,
                    is_shared_ref: false,
                    is_mut_ref: false,
                })
            }
            "unit_type" | "never_type" => Some(TypeExpr::from_path(Vec::new())),
            "abstract_type" | "dynamic_type" => self.type_expr(node.child_by_field_name("trait")?),
            "bounded_type" => {
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                children.iter().find_map(|child| self.type_expr(*child))
            }
            "bracketed_type" | "parenthesized_type" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .next()
                    .and_then(|child| self.type_expr(child))
            }
            _ => {
                let text = self.text(node).trim();
                (!text.is_empty() && !text.contains(['(', ')', '<', '>', ' ']))
                    .then(|| TypeExpr::simple(text))
            }
        }
    }

    fn type_arguments(&self, node: Node<'_>) -> Vec<TypeExpr> {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        let mut args = Vec::new();
        for child in children {
            match child.kind() {
                "lifetime" | "block" | "integer_literal" | "string_literal" => continue,
                "type_binding" => {
                    if let Some(ty) = child
                        .child_by_field_name("type")
                        .and_then(|ty| self.type_expr(ty))
                    {
                        args.push(ty);
                    }
                }
                _ => {
                    if let Some(ty) = self.type_expr(child) {
                        args.push(ty);
                    }
                }
            }
        }
        args
    }

    /// a::b::C as its segments.
    pub(super) fn expr_path(&self, node: Node<'_>) -> Vec<String> {
        match node.kind() {
            "identifier" | "type_identifier" | "primitive_type" | "self" | "super" | "crate"
            | "metavariable" => vec![self.text(node).to_owned()],
            "scoped_identifier"
            | "scoped_type_identifier"
            | "scoped_type_identifier_in_expression_position" => {
                let mut path = node
                    .child_by_field_name("path")
                    .map(|path| self.expr_path(path))
                    .unwrap_or_default();
                if let Some(name) = node.child_by_field_name("name") {
                    path.push(self.text(name).to_owned());
                }
                path
            }
            "generic_function" => node
                .child_by_field_name("function")
                .map(|function| self.expr_path(function))
                .unwrap_or_default(),
            "generic_type" | "generic_type_with_turbofish" => node
                .child_by_field_name("type")
                .map(|ty| self.expr_path(ty))
                .unwrap_or_default(),
            "qualified_type" => node
                .child_by_field_name("alias")
                .map(|alias| self.expr_path(alias))
                .unwrap_or_default(),
            "bracketed_type" | "parenthesized_expression" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .next()
                    .map(|inner| self.expr_path(inner))
                    .unwrap_or_default()
            }
            _ => {
                let text = self.text(node).trim();
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![text.to_owned()]
                }
            }
        }
    }
}

/// The innermost module scope of "placed" strictly containing "inner".
pub(super) fn place(
    placed: &[(Range<usize>, ModuleScope)],
    inner: Range<usize>,
) -> Option<ModuleScope> {
    placed
        .iter()
        .rev()
        .find(|(range, _)| range.start <= inner.start && range.end >= inner.end)
        .map(|(_, scope)| *scope)
}

/// The outermost node of the place "node" names: q.items[0].name reaches its
/// assignment through the index and the trailing field.
pub(super) fn place_end(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    while let Some(parent) = place_step(current) {
        current = parent;
    }
    current
}

/// One step out of a place expression, when "node" is part of it.
fn place_step(node: Node<'_>) -> Option<Node<'_>> {
    let parent = node.parent()?;
    match parent.kind() {
        "field_expression" if parent.child_by_field_name("value") == Some(node) => Some(parent),
        "index_expression" if parent.named_child(0) == Some(node) => Some(parent),
        "parenthesized_expression" | "try_expression" | "unary_expression" => Some(parent),
        _ => None,
    }
}

/// Methods that only copy their receiver out.
const COPIES_OUT: [&str; 5] = ["clone", "cloned", "copied", "to_owned", "load"];

/// Methods that change what they are called on.
const MUTATING: [&str; 24] = [
    "append",
    "clear",
    "dedup",
    "drain",
    "extend",
    "extend_from_slice",
    "fill",
    "insert",
    "pop",
    "push",
    "remove",
    "resize",
    "retain",
    "reverse",
    "rotate_left",
    "rotate_right",
    "set",
    "sort",
    "sort_by",
    "sort_by_key",
    "splice",
    "split_off",
    "swap",
    "swap_remove",
];

/// Calls that overwrite what they are called on through a shared reference.
pub(super) const OVERWRITES: [&str; 5] = ["replace", "set", "store", "swap", "take"];

/// Calls that only pass a value on, from deslop-please's PASSES.
pub(super) const PASSES: [&str; 32] = [
    "Ok",
    "Some",
    "as_deref",
    "as_deref_mut",
    "as_mut",
    "as_mut_slice",
    "as_path",
    "as_ref",
    "as_slice",
    "as_str",
    "borrow",
    "borrow_mut",
    "clone",
    "cloned",
    "copied",
    "deref",
    "deref_mut",
    "expect",
    "get",
    "get_mut",
    "into_inner",
    "load",
    "lock",
    "read",
    "replace",
    "set",
    "store",
    "swap",
    "take",
    "to_owned",
    "unwrap",
    "write",
];

/// What a method call on a value says about the value.
pub(super) fn method_mode(method: &str, no_args: bool) -> AccessMode {
    if COPIES_OUT.contains(&method) || (method == "get" && no_args) {
        AccessMode::CopyOrRead
    } else if MUTATING.contains(&method) {
        AccessMode::Work
    } else {
        AccessMode::MethodCall
    }
}

/// Whether a declared type holds nothing: a marker field plays no part.
pub(super) fn holds_nothing(ty: &TypeExpr) -> bool {
    ty.path
        .last()
        .is_some_and(|name| name == "PhantomData" || name == "PhantomPinned")
}

pub(super) fn has_child(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

/// "expr" with &, &mut, *, (...), and ? peeled off.
pub(super) fn unwrap_argument(mut node: Node<'_>) -> Node<'_> {
    loop {
        match node.kind() {
            "reference_expression"
            | "unary_expression"
            | "parenthesized_expression"
            | "try_expression" => {
                let mut cursor = node.walk();
                match node.named_children(&mut cursor).next() {
                    Some(inner) => node = inner,
                    None => return node,
                }
            }
            _ => return node,
        }
    }
}

pub(super) fn is_expression(node: Node<'_>) -> bool {
    node.kind().ends_with("_expression")
        || matches!(node.kind(), "identifier" | "scoped_identifier" | "block")
}

/// Whether a function's return type lends something its caller can write:
/// `&mut`, `*mut`/`*const`, `impl`, `*Mut`, or a lock guard other than a read guard.
pub(super) fn return_lends_to_write(node: Node<'_>, source: &str, offset: usize) -> bool {
    node.child_by_field_name("return_type")
        .is_some_and(|ret| return_type_matches(ret, source, offset, true))
}

/// Whether a function's return type points into `self`:
/// `&`, `*`, `impl`, `Ref`, `RefMut`, or `*Guard`.
pub(super) fn return_points(node: Node<'_>, source: &str, offset: usize) -> bool {
    node.child_by_field_name("return_type")
        .is_some_and(|ret| return_type_matches(ret, source, offset, false))
}

fn return_type_matches(root: Node<'_>, source: &str, offset: usize, write_only: bool) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let hit = if write_only {
            match node.kind() {
                "mutable_specifier" | "pointer_type" | "abstract_type" => true,
                "type_identifier" => {
                    let range = node.byte_range();
                    let name = source
                        .get(offset + range.start..offset + range.end)
                        .unwrap_or("");
                    name.ends_with("Mut")
                        || (name.ends_with("Guard") && !name.ends_with("ReadGuard"))
                }
                _ => false,
            }
        } else {
            match node.kind() {
                "reference_type" | "pointer_type" | "abstract_type" | "lifetime" => true,
                "type_identifier" => {
                    let range = node.byte_range();
                    let name = source
                        .get(offset + range.start..offset + range.end)
                        .unwrap_or("");
                    name == "Ref" || name == "RefMut" || name.ends_with("Guard")
                }
                _ => false,
            }
        };
        if hit {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    false
}
