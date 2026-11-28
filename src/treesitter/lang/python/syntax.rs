//! Python syntax helpers the `.scm` query layer cannot express cleanly:
//! `TypeExpr` parsing, receiver/callee chains, write/return positions, and the
//! module-or-class body a definition belongs to.

use std::collections::HashSet;

use tree_sitter::Node;

use crate::treesitter::query::node_text;
use crate::treesitter::summary::{CalleeExpr, InitExpr, ParamDecl, ReceiverExpr, TypeExpr};

pub(super) struct Syntax<'a> {
    pub source: &'a str,
    /// Aliases bound by `import ... as alias`.
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

    // ----- types ------------------------------------------------------------

    pub fn type_expression(&self, node: Node) -> Option<TypeExpr> {
        match node.kind() {
            // An annotation wraps its expression in a `type` node.
            "type" => self.type_expression(*named_children(node).first()?),
            "identifier" => Some(TypeExpr::simple(self.text(node))),
            "attribute" | "member_type" => Some(TypeExpr::from_path(self.dotted_segments(node))),
            "subscript" | "generic_type" => {
                let value = if node.kind() == "subscript" {
                    node.child_by_field_name("value")?
                } else {
                    *named_children(node).first()?
                };
                let mut ty = self.type_expression(value)?;
                ty.args = self.type_arguments(node);
                Some(ty)
            }
            // `Queue | None` hands out `Queue`; `A | B` is read as `A`.
            "union_type" => self.union_member(node),
            "binary_operator" if self.operator_is(node, "|") => self.union_member(node),
            "parenthesized_expression" | "constrained_type" | "splat_type" => {
                self.type_expression(*named_children(node).first()?)
            }
            // PEP 484 forward references: `"Queue"`.
            "string" => self.forward_reference(node),
            "none" => Some(TypeExpr::simple("None")),
            _ => None,
        }
    }

    /// Every dotted type path mentioned inside an annotation (`list[Queue]`
    /// yields `list` and `Queue`), for `RawRef`s to the types code names.
    pub fn type_paths(&self, node: Node) -> Vec<Vec<String>> {
        match node.kind() {
            "identifier" => vec![vec![self.text(node).to_owned()]],
            "attribute" | "member_type" => vec![self.dotted_segments(node)],
            "subscript" => {
                let mut paths = Vec::new();
                if let Some(value) = node.child_by_field_name("value") {
                    paths.extend(self.type_paths(value));
                }
                for argument in nodes_by_field(node, "subscript") {
                    paths.extend(self.type_paths(argument));
                }
                paths
            }
            "string" => self
                .forward_reference(node)
                .map(|ty| vec![ty.path])
                .unwrap_or_default(),
            "type"
            | "parenthesized_expression"
            | "constrained_type"
            | "splat_type"
            | "union_type"
            | "generic_type"
            | "type_parameter"
            | "binary_operator"
            | "block"
            | "expression_list" => named_children(node)
                .into_iter()
                .flat_map(|child| self.type_paths(child))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn operator_is(&self, node: Node, operator: &str) -> bool {
        node.child_by_field_name("operator")
            .is_some_and(|found| self.text(found) == operator)
    }

    fn type_arguments(&self, node: Node) -> Vec<TypeExpr> {
        if node.kind() == "subscript" {
            return nodes_by_field(node, "subscript")
                .into_iter()
                .filter_map(|argument| self.type_expression(argument))
                .collect();
        }
        let Some(parameters) = named_children(node)
            .into_iter()
            .find(|child| child.kind() == "type_parameter")
        else {
            return Vec::new();
        };
        named_children(parameters)
            .into_iter()
            .filter_map(|argument| self.type_expression(argument))
            .collect()
    }

    fn union_member(&self, node: Node) -> Option<TypeExpr> {
        let mut member: Option<TypeExpr> = None;
        for child in named_children(node) {
            let Some(ty) = self.type_expression(child) else {
                continue;
            };
            if matches!(
                ty.path.last().map(String::as_str),
                Some("None" | "NoneType")
            ) {
                continue;
            }
            member.get_or_insert(ty);
        }
        member
    }

    fn forward_reference(&self, node: Node) -> Option<TypeExpr> {
        let name = named_children(node)
            .into_iter()
            .filter(|child| child.kind() == "string_content")
            .map(|child| self.text(child))
            .collect::<String>();
        let name = name.trim();
        let plain = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '.' || c == '_');
        plain.then(|| TypeExpr::from_path(name.split('.').map(str::to_owned).collect()))
    }

    // ----- receivers and callees --------------------------------------------

    pub fn receiver_of(&self, node: Node) -> ReceiverExpr {
        match node.kind() {
            "identifier" => {
                let name = self.text(node);
                if name == "self" {
                    ReceiverExpr::SelfValue
                } else if self.is_namespace(name) || starts_uppercase(name) {
                    ReceiverExpr::Path(vec![name.to_owned()])
                } else {
                    ReceiverExpr::Local(name.to_owned())
                }
            }
            "attribute" => match self.flatten_attribute(node) {
                Some((base, fields)) => ReceiverExpr::FieldChain {
                    base: Box::new(self.receiver_of(base)),
                    fields,
                },
                None => ReceiverExpr::Unknown,
            },
            "call" => node
                .child_by_field_name("function")
                .and_then(|function| self.callee_of(function))
                .map_or(ReceiverExpr::Unknown, |callee| ReceiverExpr::CallResult {
                    callee: Box::new(callee),
                }),
            "parenthesized_expression" => node
                .named_child(0)
                .map_or(ReceiverExpr::Unknown, |inner| self.receiver_of(inner)),
            _ => ReceiverExpr::Unknown,
        }
    }

    pub fn callee_of(&self, node: Node) -> Option<CalleeExpr> {
        match node.kind() {
            "identifier" => Some(CalleeExpr::Path(vec![self.text(node).to_owned()])),
            "attribute" => Some(CalleeExpr::Method {
                receiver: self.receiver_of(node.child_by_field_name("object")?),
                method: self.text(node.child_by_field_name("attribute")?).to_owned(),
            }),
            _ => None,
        }
    }

    /// `q.ring.items` as its base (`q`) and the fields reached from it.
    pub fn flatten_attribute<'t>(&self, node: Node<'t>) -> Option<(Node<'t>, Vec<String>)> {
        let object = node.child_by_field_name("object")?;
        let member = self.text(node.child_by_field_name("attribute")?).to_owned();
        // Indexing a value and then reaching past it is not a plain field chain.
        if object.kind() == "subscript" {
            return None;
        }
        if object.kind() == "attribute" {
            let (base, mut fields) = self.flatten_attribute(object)?;
            fields.push(member);
            return Some((base, fields));
        }
        Some((object, vec![member]))
    }

    pub fn init_expr(&self, value: Node) -> Option<InitExpr> {
        match value.kind() {
            "call" => Some(InitExpr::Call(
                self.callee_of(value.child_by_field_name("function")?)?,
            )),
            "attribute" => {
                let (base, fields) = self.flatten_attribute(value)?;
                Some(InitExpr::FieldChain {
                    base: self.receiver_of(base),
                    fields,
                })
            }
            "identifier" => Some(InitExpr::Var(self.text(value).to_owned())),
            _ => None,
        }
    }

    pub fn dotted_segments(&self, node: Node) -> Vec<String> {
        self.text(node)
            .split('.')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect()
    }

    // ----- scopes -----------------------------------------------------------

    /// The `module` or `class_definition` body `node` is written directly in,
    /// looking through decorators: a `def` in a class body is a method, a `def`
    /// inside another `def` belongs to no declaration.
    pub fn scope_owner<'t>(&self, node: Node<'t>) -> Option<Node<'t>> {
        let mut current = node;
        loop {
            let parent = current.parent()?;
            match parent.kind() {
                "decorated_definition" => current = parent,
                "module" => return Some(parent),
                "block" => {
                    let owner = parent.parent()?;
                    return matches!(owner.kind(), "module" | "class_definition").then_some(owner);
                }
                _ => return None,
            }
        }
    }

    /// Decorator expressions written above `node`, innermost first.
    pub fn decorators(&self, node: Node) -> Vec<&'a str> {
        let Some(wrapper) = node
            .parent()
            .filter(|parent| parent.kind() == "decorated_definition")
        else {
            return Vec::new();
        };
        named_children(wrapper)
            .into_iter()
            .filter(|child| child.kind() == "decorator")
            .filter_map(|decorator| decorator.named_child(0))
            .map(|expression| self.text(expression))
            .collect()
    }

    // ----- positions --------------------------------------------------------

    /// `q.items = ...`, `q.items[i]`, `del q.items`, `q.items += ...`.
    pub fn in_write_position(&self, node: Node) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression"
                | "expression_list"
                | "pattern_list"
                | "list"
                | "tuple" => {
                    current = parent;
                }
                // Indexing a field works on the value behind it.
                "subscript" => {
                    return parent
                        .child_by_field_name("value")
                        .is_some_and(|value| value.id() == current.id());
                }
                "assignment" | "augmented_assignment" | "named_expression" => {
                    return parent
                        .child_by_field_name("left")
                        .is_some_and(|left| left.id() == current.id());
                }
                "for_statement" => {
                    return parent
                        .child_by_field_name("left")
                        .is_some_and(|left| left.id() == current.id());
                }
                "delete_statement" => return true,
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
                "parenthesized_expression" => current = parent,
                "return_statement" => return true,
                _ => return false,
            }
        }
    }

    /// The value an `attribute` is taken from being indexed: `q.items[0]`.
    pub fn is_indexed(&self, node: Node) -> bool {
        node.parent().is_some_and(|parent| {
            parent.kind() == "subscript"
                && parent
                    .child_by_field_name("value")
                    .is_some_and(|value| value.id() == node.id())
        })
    }

    /// `node` is the function of a call: `q.items.append(x)`.
    pub fn is_called(&self, node: Node) -> bool {
        node.parent().is_some_and(|parent| {
            parent.kind() == "call"
                && parent
                    .child_by_field_name("function")
                    .is_some_and(|function| function.id() == node.id())
        })
    }

    /// `self.x = param` in an assignment, with the parameter named `params`.
    pub fn assigned_param(&self, node: Node, params: &[ParamDecl]) -> bool {
        let mut current = node;
        loop {
            let Some(parent) = current.parent() else {
                return false;
            };
            match parent.kind() {
                "parenthesized_expression" => current = parent,
                "subscript" => {
                    if !parent
                        .child_by_field_name("value")
                        .is_some_and(|value| value.id() == current.id())
                    {
                        return false;
                    }
                    current = parent;
                }
                "assignment" => {
                    return parent.child_by_field_name("right").is_some_and(|right| {
                        right.kind() == "identifier"
                            && params.iter().any(|param| param.name == self.text(right))
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
        .filter(|child| child.kind() != "comment")
        .collect()
}

pub(super) fn nodes_by_field<'t>(node: Node<'t>, field: &str) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    let nodes: Vec<Node> = node.children_by_field_name(field, &mut cursor).collect();
    nodes
}

/// The single call a body makes, if that is all it does.
pub(super) fn single_call(body: Node) -> Option<Node> {
    match body.kind() {
        "block" => {
            let statements = named_children(body);
            let [statement] = statements.as_slice() else {
                return None;
            };
            match statement.kind() {
                "return_statement" | "expression_statement" => {
                    let inner = named_children(*statement);
                    let [only] = inner.as_slice() else {
                        return None;
                    };
                    (only.kind() == "call").then_some(*only)
                }
                _ => None,
            }
        }
        "call" => Some(body),
        _ => None,
    }
}

pub(super) fn starts_uppercase(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::treesitter::query::CompiledQuery;

    fn parsed(source: &str) -> (tree_sitter::Tree, Syntax<'_>) {
        let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language)
            .expect("python grammar loads");
        let tree = parser.parse(source, None).expect("source parses");
        (tree, Syntax::new(source))
    }

    fn find_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
        if node.kind() == kind {
            return Some(node);
        }
        named_children(node)
            .into_iter()
            .find_map(|child| find_kind(child, kind))
    }

    fn find_attribute<'t>(node: Node<'t>, syntax: &Syntax<'_>, member: &str) -> Option<Node<'t>> {
        if node.kind() == "attribute"
            && node
                .child_by_field_name("attribute")
                .is_some_and(|name| syntax.text(name) == member)
        {
            return Some(node);
        }
        named_children(node)
            .into_iter()
            .find_map(|child| find_attribute(child, syntax, member))
    }

    fn type_of(source: &str) -> Option<TypeExpr> {
        let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        let query = CompiledQuery::new(&language, "(typed_parameter type: (_) @ty)");
        let (tree, syntax) = parsed(source);
        let mut found = None;
        query.for_each_match(tree.root_node(), source.as_bytes(), |m| {
            found = m.node("ty").and_then(|node| syntax.type_expression(node));
        });
        found
    }

    #[test]
    fn annotations_become_type_expressions() {
        let ty = type_of("def f(q: Queue): pass").expect("Queue is a type");
        assert_eq!(ty.path, ["Queue"]);

        let generic = type_of("def f(items: list[int]): pass").expect("list[int]");
        assert_eq!(generic.path, ["list"]);
        assert_eq!(generic.args[0].path, ["int"]);

        let optional = type_of("def f(q: Optional[Queue]): pass").expect("Optional[Queue]");
        assert_eq!(optional.path, ["Optional"]);
        assert_eq!(optional.args[0].path, ["Queue"]);

        let union = type_of("def f(q: Queue | None): pass").expect("Queue | None peels");
        assert_eq!(union.path, ["Queue"]);

        let dotted = type_of("def f(q: app.queue.Queue): pass").expect("dotted type");
        assert_eq!(dotted.path, ["app", "queue", "Queue"]);

        let forward = type_of("def f(q: 'Queue'): pass").expect("forward reference");
        assert_eq!(forward.path, ["Queue"]);
    }

    #[test]
    fn type_paths_name_every_mentioned_type() {
        let (tree, syntax) = parsed("def f(items: list[Queue], limit: int): pass");
        let parameter = find_kind(tree.root_node(), "typed_parameter").expect("a parameter");
        let annotation = parameter
            .child_by_field_name("type")
            .expect("its annotation");
        assert_eq!(
            syntax.type_paths(annotation),
            vec![vec!["list".to_owned()], vec!["Queue".to_owned()]]
        );
    }

    #[test]
    fn receiver_chains_distinguish_self_locals_and_globals() {
        let source = "q.ring.items";
        let (tree, syntax) = parsed(source);
        let attribute = find_attribute(tree.root_node(), &syntax, "items").expect("an attribute");
        match syntax.receiver_of(attribute) {
            ReceiverExpr::FieldChain { base, fields } => {
                assert!(matches!(*base, ReceiverExpr::Local(ref name) if name == "q"));
                assert_eq!(fields, ["ring", "items"]);
            }
            other => panic!("q.ring.items is a field chain, not {other:?}"),
        }
    }

    #[test]
    fn a_self_receiver_is_the_instance_and_a_class_name_is_a_path() {
        let (tree, syntax) = parsed("self.limit");
        let attribute = find_attribute(tree.root_node(), &syntax, "limit").expect("self.limit");
        assert!(matches!(
            syntax.receiver_of(attribute),
            ReceiverExpr::FieldChain { ref base, ref fields }
                if matches!(**base, ReceiverExpr::SelfValue) && fields == &["limit"]
        ));

        let (tree, syntax) = parsed("Queue.make");
        let attribute = find_attribute(tree.root_node(), &syntax, "make").expect("Queue.make");
        assert!(matches!(
            syntax.receiver_of(attribute),
            ReceiverExpr::FieldChain { ref base, .. }
                if matches!(**base, ReceiverExpr::Path(ref path) if path == &["Queue"])
        ));

        // Only self, imported aliases and PascalCase names are not locals.
        let (tree, syntax) = parsed("queue.limit");
        let attribute = find_attribute(tree.root_node(), &syntax, "limit").expect("queue.limit");
        assert!(matches!(
            syntax.receiver_of(attribute),
            ReceiverExpr::FieldChain { ref base, .. }
                if matches!(**base, ReceiverExpr::Local(ref name) if name == "queue")
        ));
    }

    #[test]
    fn writes_reads_and_indexing_are_told_apart() {
        let cases = [
            ("q.items = []", true),
            ("q.items[0] = 1", true),
            ("q.items += []", true),
            ("del q.items", true),
            ("for q.items in rows: pass", true),
            ("q.items", false),
            ("len(q.items)", false),
            ("q.items[0]", true),
        ];
        for (source, expected) in cases {
            let (tree, syntax) = parsed(source);
            let attribute =
                find_attribute(tree.root_node(), &syntax, "items").expect("an attribute");
            assert_eq!(
                syntax.in_write_position(attribute),
                expected,
                "the write position of the attribute in {source}"
            );
        }
    }

    #[test]
    fn a_call_on_a_field_is_told_from_a_read() {
        let (tree, syntax) = parsed("q.items.append(x)");
        let attribute = find_attribute(tree.root_node(), &syntax, "append").expect("a call");
        assert!(syntax.is_called(attribute));

        let (tree, syntax) = parsed("len(q.items)");
        let attribute = find_attribute(tree.root_node(), &syntax, "items").expect("a read");
        assert!(!syntax.is_called(attribute));
        assert!(!syntax.in_write_position(attribute));
    }

    #[test]
    fn scope_owner_sees_through_decorators() {
        let source =
            "class Queue:\n    @property\n    def size(self):\n        return self.items\n";
        let (tree, syntax) = parsed(source);
        let function = find_kind(tree.root_node(), "function_definition").expect("a method");
        let owner = syntax.scope_owner(function).expect("a scope");
        assert_eq!(owner.kind(), "class_definition");
        assert_eq!(syntax.decorators(function), ["property"]);

        let closure = "def outer():\n    def inner():\n        pass\n";
        let (tree, syntax) = parsed(closure);
        let inner = find_kind(tree.root_node(), "function_definition")
            .and_then(|outer| {
                named_children(outer)
                    .into_iter()
                    .find_map(|child| find_kind(child, "function_definition"))
            })
            .expect("a nested def");
        assert!(syntax.scope_owner(inner).is_none());
    }

    #[test]
    fn initialisers_are_read_from_their_expression() {
        let (tree, syntax) = parsed("q = Queue()");
        let value = find_kind(tree.root_node(), "call").expect("a call");
        assert!(matches!(
            syntax.init_expr(value),
            Some(InitExpr::Call(CalleeExpr::Path(path))) if path == ["Queue"]
        ));

        let (tree, syntax) = parsed("r = q.ring");
        let value = find_kind(tree.root_node(), "attribute").expect("an attribute");
        assert!(matches!(
            syntax.init_expr(value),
            Some(InitExpr::FieldChain { base: ReceiverExpr::Local(name), fields })
                if name == "q" && fields == ["ring"]
        ));
    }
}
