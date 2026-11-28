//! Go syntax helpers the `.scm` query layer cannot express cleanly:
//! `TypeExpr`s, receiver/callee chains, write/return positions, and receiver names.

use std::collections::HashSet;

use tree_sitter::Node;

use crate::treesitter::query::node_text;
use crate::treesitter::summary::{CalleeExpr, InitExpr, ParamDecl, ReceiverExpr, TypeExpr};

/// Builtins that take their first argument by mutable reference, so passing a
/// field to one works on the field itself.
const MUTATING_BUILTINS: &[&str] = &["append", "copy", "delete", "clear"];

/// A method's receiver: the name it binds and the type it is a method of.
pub(super) struct Receiver<'a> {
    /// The name the method binds its receiver to; absent for
    /// `func (*Queue) Foo()`, whose body cannot name it.
    pub name: Option<&'a str>,
    pub ty: TypeExpr,
    pub is_pointer: bool,
}

pub(super) struct Syntax<'a> {
    pub source: &'a str,
    /// Package names bound by imports, so `pkg.Name` reads as a path.
    namespaces: HashSet<String>,
}

impl<'a> Syntax<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            namespaces: HashSet::new(),
        }
    }

    pub fn add_namespace(&mut self, alias: String) {
        self.namespaces.insert(alias);
    }

    pub fn text(&self, node: Node) -> &'a str {
        node_text(node, self.source)
    }

    /// The `TypeExpr` a type is written as.
    pub fn type_expression(&self, node: Node) -> Option<TypeExpr> {
        match node.kind() {
            "type_identifier" | "identifier" => Some(TypeExpr::simple(self.text(node))),
            "qualified_type" => Some(TypeExpr::from_path(self.path_segments(node))),
            "pointer_type" => {
                let inner = self.type_expression(node.named_child(0)?)?;
                Some(TypeExpr {
                    is_mut_ref: true,
                    ..inner
                })
            }
            "slice_type" => Some(self.holding("slice", node.child_by_field_name("element")?)),
            "array_type" | "implicit_length_array_type" => {
                Some(self.holding("array", node.child_by_field_name("element")?))
            }
            "map_type" => {
                let key = self.type_expression(node.child_by_field_name("key")?)?;
                let value = self.type_expression(node.child_by_field_name("value")?)?;
                Some(TypeExpr {
                    path: vec!["Map".to_owned()],
                    args: vec![key, value],
                    is_shared_ref: false,
                    is_mut_ref: false,
                })
            }
            "channel_type" => self.type_expression(node.child_by_field_name("value")?),
            "generic_type" => {
                let mut ty = self.type_expression(node.child_by_field_name("type")?)?;
                if let Some(arguments) = node.child_by_field_name("type_arguments") {
                    ty.args = self.type_arguments(arguments);
                }
                Some(ty)
            }
            "type_instantiation_expression" => {
                self.type_expression(node.child_by_field_name("type")?)
            }
            "parenthesized_type" | "negated_type" => self.type_expression(node.named_child(0)?),
            _ => None,
        }
    }

    fn holding(&self, name: &str, element: Node) -> TypeExpr {
        let inner = self.type_expression(element);
        TypeExpr {
            path: vec![name.to_owned()],
            args: inner.into_iter().collect(),
            is_shared_ref: false,
            is_mut_ref: false,
        }
    }

    fn type_arguments(&self, node: Node) -> Vec<TypeExpr> {
        named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "type_elem")
            .filter_map(|element| self.type_expression(element.named_child(0)?))
            .collect()
    }

    /// The receiver of a `method_declaration`.
    pub fn receiver(&self, receiver: Node) -> Option<Receiver<'a>> {
        let parameter = named_children(receiver)
            .into_iter()
            .find(|child| child.kind() == "parameter_declaration")?;
        let ty = parameter.child_by_field_name("type")?;
        Some(Receiver {
            name: parameter
                .child_by_field_name("name")
                .map(|name| self.text(name)),
            ty: self.type_expression(ty)?,
            is_pointer: ty.kind() == "pointer_type",
        })
    }

    /// What an expression used as a receiver evaluates to. `self_name` is the
    /// receiver variable of the method the expression is written in, which Go
    /// names explicitly instead of `self`.
    pub fn receiver_of(&self, node: Node, self_name: Option<&str>) -> ReceiverExpr {
        match node.kind() {
            "identifier" => {
                let name = self.text(node);
                if Some(name) == self_name {
                    ReceiverExpr::SelfValue
                } else if self.namespaces.contains(name) || starts_uppercase(name) {
                    ReceiverExpr::Path(vec![name.to_owned()])
                } else {
                    ReceiverExpr::Local(name.to_owned())
                }
            }
            "selector_expression" => {
                let (Some(operand), Some(field)) = (
                    node.child_by_field_name("operand"),
                    node.child_by_field_name("field"),
                ) else {
                    return ReceiverExpr::Unknown;
                };
                let member = self.text(field).to_owned();
                match self.receiver_of(operand, self_name) {
                    ReceiverExpr::FieldChain { base, mut fields } => {
                        fields.push(member);
                        ReceiverExpr::FieldChain { base, fields }
                    }
                    ReceiverExpr::Unknown => ReceiverExpr::Unknown,
                    base => ReceiverExpr::FieldChain {
                        base: Box::new(base),
                        fields: vec![member],
                    },
                }
            }
            "parenthesized_expression" | "unary_expression" => {
                node.named_child(0).map_or(ReceiverExpr::Unknown, |inner| {
                    self.receiver_of(inner, self_name)
                })
            }
            "call_expression" => node
                .child_by_field_name("function")
                .and_then(|function| self.callee_of(function, self_name))
                .map_or(ReceiverExpr::Unknown, |callee| ReceiverExpr::CallResult {
                    callee: Box::new(callee),
                }),
            _ => ReceiverExpr::Unknown,
        }
    }

    /// What a called expression calls.
    pub fn callee_of(&self, node: Node, self_name: Option<&str>) -> Option<CalleeExpr> {
        match node.kind() {
            "identifier" | "type_identifier" => {
                Some(CalleeExpr::Path(vec![self.text(node).to_owned()]))
            }
            "qualified_type" => Some(CalleeExpr::Path(self.path_segments(node))),
            "parenthesized_expression" => self.callee_of(node.named_child(0)?, self_name),
            "index_expression" => self.callee_of(node.child_by_field_name("operand")?, self_name),
            "selector_expression" => {
                let operand = node.child_by_field_name("operand")?;
                let field = node.child_by_field_name("field")?;
                Some(CalleeExpr::Method {
                    receiver: self.receiver_of(operand, self_name),
                    method: self.text(field).to_owned(),
                })
            }
            _ => None,
        }
    }

    /// A selector chain split into its base expression and the field names on it.
    pub fn flatten_selector<'n>(&self, node: Node<'n>) -> Option<(Node<'n>, Vec<String>)> {
        let operand = node.child_by_field_name("operand")?;
        let member = self.text(node.child_by_field_name("field")?).to_owned();
        if operand.kind() == "selector_expression" {
            let (base, mut fields) = self.flatten_selector(operand)?;
            fields.push(member);
            return Some((base, fields));
        }
        Some((operand, vec![member]))
    }

    /// What an initializer expression makes: a literal, a call, a field, or a name.
    pub fn init_expr(&self, node: Node, self_name: Option<&str>) -> Option<InitExpr> {
        match node.kind() {
            "composite_literal" => Some(InitExpr::StructLiteral(
                self.type_path(node.child_by_field_name("type")?)?,
            )),
            "call_expression" => Some(InitExpr::Call(
                self.callee_of(node.child_by_field_name("function")?, self_name)?,
            )),
            "unary_expression" => {
                let operator = node.child_by_field_name("operator")?;
                if self.text(operator) != "&" {
                    return None;
                }
                self.init_expr(node.child_by_field_name("operand")?, self_name)
            }
            "selector_expression" => {
                let (base, fields) = self.flatten_selector(node)?;
                Some(InitExpr::FieldChain {
                    base: self.receiver_of(base, self_name),
                    fields,
                })
            }
            "identifier" => Some(InitExpr::Var(self.text(node).to_owned())),
            "parenthesized_expression" => self.init_expr(node.named_child(0)?, self_name),
            _ => None,
        }
    }

    /// The path a type or literal is named by.
    pub fn type_path(&self, node: Node) -> Option<Vec<String>> {
        match node.kind() {
            "type_identifier" | "identifier" => Some(vec![self.text(node).to_owned()]),
            "qualified_type" => Some(self.path_segments(node)),
            "generic_type" => self.type_path(node.child_by_field_name("type")?),
            "pointer_type" | "parenthesized_type" => self.type_path(node.named_child(0)?),
            "slice_type" => Some(vec!["slice".to_owned()]),
            "array_type" | "implicit_length_array_type" => Some(vec!["array".to_owned()]),
            "map_type" => Some(vec!["Map".to_owned()]),
            _ => None,
        }
    }

    /// The `/`-free segments of a qualified type: `pkg.Name` -> `[pkg, Name]`.
    pub fn path_segments(&self, node: Node) -> Vec<String> {
        match node.kind() {
            "qualified_type" => {
                let package = node.child_by_field_name("package").map(|p| self.text(p));
                let name = node.child_by_field_name("name").map(|n| self.text(n));
                match (package, name) {
                    (Some(package), Some(name)) => {
                        vec![package.to_owned(), name.to_owned()]
                    }
                    _ => Vec::new(),
                }
            }
            _ => vec![self.text(node).to_owned()],
        }
    }

    /// The text of a string literal, without its quotes.
    pub fn string_value(&self, node: Node) -> Option<String> {
        match node.kind() {
            "interpreted_string_literal" => Some(self.text(node).trim_matches('"').to_owned()),
            "raw_string_literal" => Some(self.text(node).trim_matches('`').to_owned()),
            _ => None,
        }
    }

    /// `q.items = ...`, `q.items[i] = ...`, `q.items[i]++`, or `*q.items = ...`.
    pub fn in_write_position(&self, node: Node) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression" | "expression_list" => current = parent,
                "index_expression" | "slice_expression" => {
                    if !node_field(parent, "operand", current) {
                        return false;
                    }
                    current = parent;
                }
                "unary_expression" => {
                    let operator = parent
                        .child_by_field_name("operator")
                        .map(|op| self.text(op));
                    if operator != Some("*") {
                        return false;
                    }
                    current = parent;
                }
                "assignment_statement" => {
                    return parent
                        .child_by_field_name("left")
                        .is_some_and(|left| contains(left, current));
                }
                "inc_statement" | "dec_statement" => return true,
                _ => return false,
            }
        }
    }

    /// Whether a field is handed out: returned, or worked on where it is read.
    pub fn in_return_position(&self, node: Node) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression" | "expression_list" | "unary_expression" => {
                    current = parent;
                }
                "return_statement" => return true,
                _ => return false,
            }
        }
    }

    /// `q.items = param` in an assignment, with the parameters named `params`.
    pub fn assigned_param(&self, node: Node, params: &[ParamDecl]) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression" | "expression_list" => current = parent,
                "index_expression" | "slice_expression" => {
                    if !node_field(parent, "operand", current) {
                        return false;
                    }
                    current = parent;
                }
                "assignment_statement" => {
                    let Some(right) = parent.child_by_field_name("right") else {
                        return false;
                    };
                    let values = named_children(right);
                    let [value] = values.as_slice() else {
                        return false;
                    };
                    return value.kind() == "identifier"
                        && params.iter().any(|param| param.name == self.text(*value));
                }
                _ => return false,
            }
        }
    }

    /// Whether reading this expression reaches into it: indexing, slicing, its
    /// address, a mutating builtin, or a write.
    pub fn reaches_into(&self, node: Node) -> bool {
        if self.in_write_position(node) {
            return true;
        }
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression" | "expression_list" => current = parent,
                "index_expression" | "slice_expression" => {
                    return node_field(parent, "operand", current);
                }
                "unary_expression" => {
                    let operator = parent
                        .child_by_field_name("operator")
                        .map(|op| self.text(op));
                    if operator == Some("&") {
                        return true;
                    }
                    if operator != Some("*") {
                        return false;
                    }
                    current = parent;
                }
                "argument_list" => {
                    let call = parent
                        .parent()
                        .filter(|call| call.kind() == "call_expression");
                    let Some(function) = call.and_then(|call| call.child_by_field_name("function"))
                    else {
                        return false;
                    };
                    return function.kind() == "identifier"
                        && MUTATING_BUILTINS.contains(&self.text(function));
                }
                _ => return false,
            }
        }
    }

    /// The single call a body makes, if that is all it does.
    pub fn single_call<'n>(&self, body: Node<'n>) -> Option<Node<'n>> {
        let statements = self.statements(body)?;
        let [statement] = statements.as_slice() else {
            return None;
        };
        match statement.kind() {
            "expression_statement" => statement
                .named_child(0)
                .filter(|expression| expression.kind() == "call_expression"),
            "return_statement" => {
                let values = named_children(statement.named_child(0)?);
                match values.as_slice() {
                    [value] if value.kind() == "call_expression" => Some(*value),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn statements<'n>(&self, body: Node<'n>) -> Option<Vec<Node<'n>>> {
        if body.kind() != "block" {
            return None;
        }
        let list = named_children(body)
            .into_iter()
            .find(|child| child.kind() == "statement_list")?;
        Some(named_children(list))
    }

    /// Whether `node` is the declared name of a type, not a use of one.
    pub fn is_declaration_name(&self, node: Node) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        if !matches!(parent.kind(), "type_spec" | "type_alias") {
            return false;
        }
        parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
    }
}

pub(super) fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

/// Whether `node` is the `field` of `parent`.
fn node_field(parent: Node, field: &str, node: Node) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|child| child.id() == node.id())
}

/// Whether `outer` spans `inner`.
fn contains(outer: Node, inner: Node) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}

pub(super) fn starts_uppercase(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}
