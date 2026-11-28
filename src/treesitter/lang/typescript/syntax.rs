//! TypeScript syntax helpers the `.scm` query layer cannot express cleanly:
//! `TypeExpr` parsing, receiver/callee chains, write/return positions, and names.

use std::collections::HashSet;

use tree_sitter::Node;

use crate::treesitter::query::node_text;
use crate::treesitter::summary::{CalleeExpr, InitExpr, ParamDecl, ReceiverExpr, TypeExpr};

pub(super) struct Syntax<'a> {
    pub source: &'a str,
    /// Aliases bound by `import * as alias from '...'`.
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

    pub fn is_namespace(&self, name: &str) -> bool {
        self.namespaces.contains(name)
    }

    pub fn text(&self, node: Node) -> &'a str {
        node_text(node, self.source)
    }

    pub fn type_expression(&self, node: Node) -> Option<TypeExpr> {
        if node.kind() == "type_annotation" {
            return node.named_child(0).and_then(|n| self.type_from(n));
        }
        self.type_from(node)
    }

    fn type_from(&self, node: Node) -> Option<TypeExpr> {
        match node.kind() {
            "type_identifier" | "identifier" | "predefined_type" => {
                Some(TypeExpr::simple(self.text(node)))
            }
            "nested_type_identifier" => Some(TypeExpr::from_path(self.path_segments(node))),
            "generic_type" => {
                let mut ty = self.type_from(node.child_by_field_name("name")?)?;
                if let Some(arguments) = node.child_by_field_name("type_arguments") {
                    ty.args = named_children(arguments)
                        .into_iter()
                        .filter_map(|a| self.type_from(a))
                        .collect();
                }
                Some(ty)
            }
            "array_type" => {
                let inner = self.type_from(node.named_child(0)?)?;
                Some(TypeExpr {
                    path: vec!["Array".to_owned()],
                    args: vec![inner],
                    is_shared_ref: false,
                    is_mut_ref: false,
                })
            }
            "parenthesized_type" | "readonly_type" | "optional_type" | "rest_type"
            | "lookup_type" | "index_type_query" | "union_type" | "intersection_type"
            | "type_query" => self.type_from(node.named_child(0)?),
            "this_type" => Some(TypeExpr::simple("this")),
            "literal_type" => Some(TypeExpr::simple(self.text(node))),
            _ => None,
        }
    }

    pub fn receiver_of(&self, node: Node) -> ReceiverExpr {
        match node.kind() {
            "this" => ReceiverExpr::SelfValue,
            "identifier" => {
                let name = self.text(node);
                if self.namespaces.contains(name) || starts_uppercase(name) {
                    ReceiverExpr::Path(vec![name.to_owned()])
                } else {
                    ReceiverExpr::Local(name.to_owned())
                }
            }
            "nested_identifier" => ReceiverExpr::Path(self.path_segments(node)),
            "member_expression" => {
                let Some(object) = node.child_by_field_name("object") else {
                    return ReceiverExpr::Unknown;
                };
                let Some(property) = node.child_by_field_name("property") else {
                    return ReceiverExpr::Unknown;
                };
                let member = self.property_name(property);
                match self.receiver_of(object) {
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
            "call_expression" => node
                .child_by_field_name("function")
                .and_then(|f| self.callee_of(f))
                .map_or(ReceiverExpr::Unknown, |callee| ReceiverExpr::CallResult {
                    callee: Box::new(callee),
                }),
            "new_expression" => node.child_by_field_name("constructor").map_or(
                ReceiverExpr::Unknown,
                |constructor| ReceiverExpr::CallResult {
                    callee: Box::new(CalleeExpr::Path(self.path_segments(constructor))),
                },
            ),
            "parenthesized_expression" | "await_expression" => node
                .named_child(0)
                .map_or(ReceiverExpr::Unknown, |inner| self.receiver_of(inner)),
            _ => ReceiverExpr::Unknown,
        }
    }

    pub fn callee_of(&self, node: Node) -> Option<CalleeExpr> {
        match node.kind() {
            "identifier" | "nested_identifier" => Some(CalleeExpr::Path(self.path_segments(node))),
            "member_expression" => Some(CalleeExpr::Method {
                receiver: self.receiver_of(node.child_by_field_name("object")?),
                method: self.property_name(node.child_by_field_name("property")?),
            }),
            _ => None,
        }
    }

    pub fn flatten_member<'n>(&self, node: Node<'n>) -> Option<(Node<'n>, Vec<String>)> {
        let object = node.child_by_field_name("object")?;
        let member = self.property_name(node.child_by_field_name("property")?);
        if matches!(object.kind(), "subscript_expression" | "optional_chain") {
            return None;
        }
        if object.kind() == "member_expression" {
            let (base, mut fields) = self.flatten_member(object)?;
            fields.push(member);
            return Some((base, fields));
        }
        Some((object, vec![member]))
    }

    pub fn init_expr(&self, value: Node) -> Option<InitExpr> {
        match value.kind() {
            "new_expression" => Some(InitExpr::StructLiteral(
                self.path_segments(value.child_by_field_name("constructor")?),
            )),
            "call_expression" => Some(InitExpr::Call(
                self.callee_of(value.child_by_field_name("function")?)?,
            )),
            "member_expression" => {
                let (base, fields) = self.flatten_member(value)?;
                Some(InitExpr::FieldChain {
                    base: self.receiver_of(base),
                    fields,
                })
            }
            "identifier" => Some(InitExpr::Var(self.text(value).to_owned())),
            _ => None,
        }
    }

    pub fn path_segments(&self, node: Node) -> Vec<String> {
        self.text(node)
            .split('.')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }

    pub fn string_value(&self, node: Node) -> Option<String> {
        if node.kind() != "string" {
            return None;
        }
        let mut value = String::new();
        for child in named_children(node) {
            if child.kind() == "string_fragment" {
                value.push_str(self.text(child));
            }
        }
        if value.is_empty() {
            value = self
                .text(node)
                .trim_matches('"')
                .trim_matches('\'')
                .to_owned();
        }
        Some(value)
    }

    pub fn property_name(&self, node: Node) -> String {
        match node.kind() {
            "string" => self.string_value(node).unwrap_or_default(),
            "computed_property_name" => self.text(node).trim_matches(['[', ']']).to_owned(),
            _ => self.text(node).to_owned(),
        }
    }

    pub fn binding_names(&self, node: Node) -> Vec<String> {
        match node.kind() {
            "identifier" => vec![self.text(node).to_owned()],
            "object_pattern" => named_children(node)
                .into_iter()
                .filter_map(|child| match child.kind() {
                    "shorthand_property_identifier_pattern" => Some(self.text(child).to_owned()),
                    "pair_pattern" => child
                        .child_by_field_name("key")
                        .map(|key| self.property_name(key)),
                    "object_assignment_pattern" => child
                        .child_by_field_name("left")
                        .map(|left| self.text(left).to_owned()),
                    _ => None,
                })
                .collect(),
            "array_pattern" => named_children(node)
                .into_iter()
                .filter(|c| c.kind() == "identifier")
                .map(|c| self.text(c).to_owned())
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn is_require_call(&self, value: Node) -> bool {
        if value.kind() != "call_expression" {
            return false;
        }
        let Some(function) = value.child_by_field_name("function") else {
            return false;
        };
        function.kind() == "identifier"
            && self.text(function) == "require"
            && value
                .child_by_field_name("arguments")
                .and_then(|args| named_children(args).first().copied())
                .is_some_and(|argument| argument.kind() == "string")
    }

    /// `q.items = ...`, `q.items[i] = ...`, `++q.count`, `delete q.cache`, or indexing.
    pub fn in_write_position(&self, node: Node) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression" | "non_null_expression" | "as_expression" => {
                    current = parent
                }
                "subscript_expression" => return true,
                "assignment_expression" | "augmented_assignment_expression" => {
                    return parent
                        .child_by_field_name("left")
                        .is_some_and(|l| l.id() == current.id());
                }
                "update_expression" => return true,
                "unary_expression" => {
                    return parent
                        .child_by_field_name("operator")
                        .is_some_and(|op| self.text(op) == "delete");
                }
                _ => return false,
            }
        }
    }

    pub fn in_return_position(&self, node: Node) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression" | "sequence_expression" => current = parent,
                "return_statement" => return true,
                _ => return false,
            }
        }
    }

    /// `this.x = param` in an assignment, with the parameter named `params`.
    pub fn assigned_param(&self, node: Node, params: &[ParamDecl]) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression" | "as_expression" => current = parent,
                "subscript_expression" => {
                    if !parent
                        .child_by_field_name("object")
                        .is_some_and(|o| o.id() == current.id())
                    {
                        return false;
                    }
                    current = parent;
                }
                "assignment_expression" => {
                    return parent.child_by_field_name("right").is_some_and(|right| {
                        right.kind() == "identifier"
                            && params.iter().any(|p| p.name == self.text(right))
                    });
                }
                _ => return false,
            }
        }
    }
}

pub(super) fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "comment" | "hash_bang_line"))
        .collect()
}

pub(super) fn has_token(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

pub(super) fn is_call(node: Node) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "call_expression"
            && parent
                .child_by_field_name("function")
                .is_some_and(|f| f.id() == node.id())
    })
}

pub(super) fn is_new_constructor(node: Node) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "new_expression"
            && parent
                .child_by_field_name("constructor")
                .is_some_and(|c| c.id() == node.id())
    })
}

pub(super) fn single_call(body: Node) -> Option<Node> {
    match body.kind() {
        "statement_block" => {
            let statements = named_children(body);
            let [statement] = statements.as_slice() else {
                return None;
            };
            match statement.kind() {
                "return_statement" | "expression_statement" => statement
                    .named_child(0)
                    .filter(|e| e.kind() == "call_expression"),
                _ => None,
            }
        }
        "call_expression" => Some(body),
        _ => None,
    }
}

pub(super) fn is_declaration_name(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
        && matches!(
            parent.kind(),
            "class_declaration"
                | "abstract_class_declaration"
                | "class"
                | "interface_declaration"
                | "type_alias_declaration"
                | "type_parameter"
                | "internal_module"
                | "module"
        )
}

fn starts_uppercase(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}
