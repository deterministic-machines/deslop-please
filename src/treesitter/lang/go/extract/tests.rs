//! Extraction tests over small Go sources: declarations, written types,
//! the receiver-as-`self` rule, access modes, locals, and wrappers.

use std::path::PathBuf;

use super::*;
use crate::model::Tree;
use crate::treesitter::summary::{AccessMode, ImportKind, InitExpr, ReceiverExpr};

fn discovered(rel_path: &str, is_test: bool) -> DiscoveredFile {
    let module = Tree::new().root();
    DiscoveredFile {
        abs_path: PathBuf::from(rel_path),
        rel_path: rel_path.to_owned(),
        module_id: module,
        package_id: module,
        language: Language::Go,
        is_test,
    }
}

fn summary(source: &str) -> FileSummary {
    summarize(&discovered("queue/queue.go", false), source)
}

fn decl<'a>(summary: &'a FileSummary, name: &str, kind: ItemKind) -> &'a ItemDecl {
    summary
        .decls
        .iter()
        .find(|decl| decl.name == name && decl.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} `{name}`"))
}

fn body<'a>(summary: &'a FileSummary, name: &str) -> &'a FunctionBodySummary {
    decl(summary, name, ItemKind::Function)
        .body_summary
        .as_ref()
        .unwrap_or_else(|| panic!("`{name}` has no body summary"))
}

fn field_names(summary: &FileSummary, parent: LocalDeclId) -> Vec<&str> {
    summary
        .decls
        .iter()
        .filter(|decl| decl.kind == ItemKind::Field && decl.parent_decl == Some(parent))
        .map(|decl| decl.name.as_str())
        .collect()
}

/// Every field/method reference, as `(member, receiver reads self, access)`.
fn member_refs(summary: &FileSummary) -> Vec<(&str, bool, AccessMode)> {
    summary
        .refs
        .iter()
        .filter_map(|reference| match &reference.kind {
            RawRefKind::MemberAccess {
                receiver, member, ..
            } => Some((
                member.as_str(),
                matches!(receiver, ReceiverExpr::SelfValue)
                    || matches!(receiver, ReceiverExpr::FieldChain { base, .. }
                        if matches!(**base, ReceiverExpr::SelfValue)),
                reference.access,
            )),
            RawRefKind::Path(_) => None,
        })
        .collect()
}

#[test]
fn imports_bind_the_package_name_an_unaliased_import_uses() {
    let summary = summary(
        r#"
package queue

import (
	"fmt"
	"example.com/shop/queue/ring"
	. "example.com/shop/queue/items"
	_ "example.com/shop/queue/side"
	alias "example.com/shop/queue/inner"
	"example.com/shop/queue/v2"
)

func NewQueue() *Queue { return nil }
"#,
    );
    let kinds: Vec<String> = summary
        .imports
        .iter()
        .map(|import| match &import.kind {
            ImportKind::Namespace { alias } => alias.clone(),
            ImportKind::Glob => ".".to_owned(),
            ImportKind::SideEffect => "_".to_owned(),
            other => panic!("unexpected import kind: {other:?}"),
        })
        .collect();
    assert_eq!(kinds, ["fmt", "ring", ".", "_", "alias", "queue"]);
    assert_eq!(
        summary.imports[1].raw_specifier.as_deref(),
        Some("example.com/shop/queue/ring")
    );
    assert_eq!(
        summary.imports[1].path_segments,
        ["example.com", "shop", "queue", "ring"]
    );
}

#[test]
fn a_struct_declares_its_fields_with_their_written_types() {
    let summary = summary(
        r#"
package queue

type Item struct {
	ID   int
	Tags []string
}

type Queue struct {
	items []Item
	meta  map[string][]Item
	next  *Item
	tags  [3]string
}

type Store interface {
	Put(item Item) error
	Len() int
}

type Alias = Queue

type Wrapper struct {
	Item
	name string
}
"#,
    );
    let queue = decl(&summary, "Queue", ItemKind::Type).local_id;
    assert_eq!(
        field_names(&summary, queue),
        ["items", "meta", "next", "tags"]
    );
    let item_type = |name: &str| {
        decl(&summary, name, ItemKind::Field)
            .type_annotation
            .clone()
            .expect("a written type")
    };
    assert_eq!(item_type("items").path, ["slice"]);
    assert_eq!(item_type("items").args[0].path, ["Item"]);
    assert_eq!(item_type("meta").path, ["Map"]);
    assert_eq!(item_type("meta").args[0].path, ["string"]);
    assert_eq!(item_type("meta").args[1].path, ["slice"]);
    assert_eq!(item_type("meta").args[1].args[0].path, ["Item"]);
    assert_eq!(item_type("next").path, ["Item"]);
    assert!(item_type("next").is_mut_ref, "`*Item` is a pointer");
    assert_eq!(item_type("tags").path, ["array"]);
    assert_eq!(
        field_names(&summary, decl(&summary, "Wrapper", ItemKind::Type).local_id),
        ["name"],
        "an embedded field declares no name"
    );
    let store = decl(&summary, "Store", ItemKind::Type).local_id;
    let len = decl(&summary, "Len", ItemKind::Function);
    assert_eq!(len.parent_decl, Some(store));
    assert!(len.takes_self);
    assert!(len.is_exported);
    let put = decl(&summary, "Put", ItemKind::Function);
    assert_eq!(put.signature.as_ref().unwrap().params[0].name, "item");
    assert_eq!(
        put.signature.as_ref().unwrap().params[0]
            .ty
            .as_ref()
            .unwrap()
            .path,
        ["Item"]
    );
    assert!(decl(&summary, "Alias", ItemKind::Type).is_exported);
    assert!(!decl(&summary, "items", ItemKind::Field).is_exported);
}

#[test]
fn a_method_wraps_its_receiver_type_and_keeps_pointer_or_value_semantics() {
    let summary = summary(
        r#"
package queue

type Queue struct {
	items []Item
}

func (q *Queue) Push(item Item) {
	q.items = append(q.items, item)
}

func (q Queue) Len() int {
	return len(q.items)
}

func (Queue) Zero() int {
	return 0
}
"#,
    );
    let push = decl(&summary, "Push", ItemKind::Function);
    assert!(push.takes_self);
    assert!(!push.self_is_shared_ref, "a pointer receiver can write");
    let impl_id = push.parent_decl.expect("a synthetic impl block");
    let block = summary
        .decls
        .iter()
        .find(|decl| decl.local_id == impl_id)
        .expect("the impl decl");
    assert_eq!(block.kind, ItemKind::Impl);
    assert_eq!(
        block.impl_target.as_ref().expect("an impl target").path,
        ["Queue"]
    );
    let len = decl(&summary, "Len", ItemKind::Function);
    assert!(len.self_is_shared_ref, "a value receiver reads a copy");
    let zero = decl(&summary, "Zero", ItemKind::Function);
    assert!(zero.takes_self);
    assert!(
        zero.body_summary.is_some(),
        "an unnamed receiver still declares the method"
    );
    assert_eq!(
        len.signature
            .as_ref()
            .unwrap()
            .return_type
            .as_ref()
            .unwrap()
            .path,
        ["int"]
    );
}

#[test]
fn the_receiver_name_reads_as_self_and_access_modes_stay_syntactic() {
    let summary = summary(
        r#"
package queue

type Queue struct {
	items []Item
	head  int
	meta  map[string]int
}

func (q *Queue) Assign() { q.items = nil }

func (q *Queue) Element() { q.items[0] = nil }

func (q *Queue) Increment() { q.head++ }

func (q *Queue) Address() { _ = &q.head }

func (q *Queue) Read() { _ = len(q.items) }

func (q *Queue) Append() { _ = append(q.items, nil) }

func (q *Queue) Remove() { delete(q.meta, "a") }

func (q *Queue) Index() { _ = q.items[0] }

func (q *Queue) Method() { q.items.Len() }

func (q *Queue) Plain() { _ = q.head }
"#,
    );
    let first_ref = |name: &str| {
        let holder = decl(&summary, name, ItemKind::Function).local_id;
        summary
            .refs
            .iter()
            .find(|reference| {
                reference.holder == Some(holder)
                    && matches!(reference.kind, RawRefKind::MemberAccess { .. })
            })
            .unwrap_or_else(|| panic!("no member reference in `{name}`"))
    };
    assert_eq!(first_ref("Assign").access, AccessMode::Work);
    assert_eq!(first_ref("Element").access, AccessMode::Work);
    assert_eq!(first_ref("Increment").access, AccessMode::Work);
    assert_eq!(first_ref("Address").access, AccessMode::Work);
    assert_eq!(first_ref("Read").access, AccessMode::CopyOrRead);
    assert_eq!(first_ref("Append").access, AccessMode::Work);
    assert_eq!(first_ref("Remove").access, AccessMode::Work);
    assert_eq!(first_ref("Index").access, AccessMode::Work);
    assert_eq!(first_ref("Plain").access, AccessMode::CopyOrRead);
    let method: Vec<(&str, bool, AccessMode)> = member_refs(&summary)
        .into_iter()
        .filter(|(member, ..)| *member == "Len")
        .collect();
    assert_eq!(method, [("Len", true, AccessMode::MethodCall)]);
    assert!(
        summary.refs.iter().all(|reference| match &reference.kind {
            RawRefKind::MemberAccess { receiver, .. } => match receiver {
                ReceiverExpr::SelfValue => true,
                ReceiverExpr::FieldChain { base, .. } => matches!(**base, ReceiverExpr::SelfValue),
                other => panic!("a receiver that is no self: {other:?}"),
            },
            RawRefKind::Path(_) => true,
        }),
        "`q` reads as the receiver everywhere in a method"
    );
}

#[test]
fn locals_carry_the_type_their_initializer_gives_them() {
    let summary = summary(
        r#"
package queue

func Build() {
	q := NewQueue()
	r := &Queue{}
	var x Queue
	var y, z Queue
	w := q.items
	q2, err := NewQueue()
	_, _, _, _, _, _, _ = q, r, x, y, z, w, q2
	_ = err
}
"#,
    );
    let locals = &body(&summary, "Build").locals;
    let local = |name: &str| {
        locals
            .iter()
            .find(|local| local.name == name)
            .unwrap_or_else(|| panic!("no local `{name}`"))
    };
    match &local("q").init {
        Some(InitExpr::Call(CalleeExpr::Path(path))) => {
            assert_eq!(path, &["NewQueue".to_owned()]);
        }
        other => panic!("`q` is no call: {other:?}"),
    }
    match &local("r").init {
        Some(InitExpr::StructLiteral(path)) => {
            assert_eq!(path, &["Queue".to_owned()]);
        }
        other => panic!("`r` is no struct literal: {other:?}"),
    }
    assert_eq!(local("r").declared_type, None);
    assert_eq!(local("x").declared_type.as_ref().unwrap().path, ["Queue"]);
    assert!(local("y").init.is_none());
    assert_eq!(local("z").declared_type.as_ref().unwrap().path, ["Queue"]);
    match &local("w").init {
        Some(InitExpr::FieldChain { base, fields }) => {
            assert_eq!(base, &ReceiverExpr::Local("q".to_owned()));
            assert_eq!(fields, &["items".to_owned()]);
        }
        other => panic!("`w` is no field chain: {other:?}"),
    }
    match &local("q2").init {
        Some(InitExpr::Call(_)) => {}
        other => panic!("`q2` is no call: {other:?}"),
    }
    assert!(local("err").init.is_none(), "a second name gets no init");
}

#[test]
fn forwarding_and_accessor_candidates_read_the_receiver_as_self() {
    let summary = summary(
        r#"
package queue

type Queue struct {
	items []Item
	size  int
	ring  *Ring
}

func (q *Queue) Push(item Item) {
	q.ring.Push(item)
}

func (q *Queue) Items() []Item {
	return q.items
}

func (q *Queue) SetItems(items []Item) {
	q.items = items
}

func (q *Queue) Length() int {
	return q.size + 1
}
"#,
    );
    let push = body(&summary, "Push");
    assert!(!push.computes, "a forwarder computes nothing");
    let forwarding = push.forwarding_candidate.as_ref().expect("`Push` forwards");
    match &forwarding.callee {
        CalleeExpr::Method { receiver, method } => {
            assert_eq!(method, "Push");
            match receiver {
                ReceiverExpr::FieldChain { base, fields } => {
                    assert!(matches!(**base, ReceiverExpr::SelfValue));
                    assert_eq!(fields, &["ring".to_owned()]);
                }
                other => panic!("`q.ring` is no field chain: {other:?}"),
            }
        }
        other => panic!("`q.ring.Push` is no method: {other:?}"),
    }
    assert_eq!(forwarding.self_fields, ["ring"]);

    let items = body(&summary, "Items");
    assert!(!items.computes);
    let accessor = items
        .accessor_candidate
        .as_ref()
        .expect("`Items` is an accessor");
    assert_eq!(accessor.ops.len(), 1);
    assert_eq!(accessor.ops[0].field_name, "items");
    assert!(!accessor.ops[0].is_write);

    let set = body(&summary, "SetItems");
    let accessor = set.accessor_candidate.as_ref().expect("`SetItems` writes");
    assert!(accessor.ops[0].is_write);
    assert!(accessor.ops[0].is_lent_or_written);

    let length = body(&summary, "Length");
    assert!(length.computes);
    assert!(length.accessor_candidate.is_none());
}

#[test]
fn a_result_picks_its_first_non_error_type_and_function_literals_are_skipped() {
    let summary = summary(
        r#"
package queue

func NewQueue() (*Queue, error) {
	return &Queue{}, nil
}

func Count() (int, error) {
	return 0, nil
}

func (q *Queue) Run() {
	work := func() {
		q.items = nil
	}
	work()
}
"#,
    );
    let new_queue = decl(&summary, "NewQueue", ItemKind::Function);
    let result = new_queue
        .signature
        .as_ref()
        .unwrap()
        .return_type
        .as_ref()
        .unwrap();
    assert_eq!(result.path, ["Queue"]);
    assert!(result.is_mut_ref);
    assert_eq!(
        decl(&summary, "Count", ItemKind::Function)
            .signature
            .as_ref()
            .unwrap()
            .return_type
            .as_ref()
            .unwrap()
            .path,
        ["int"]
    );
    let run = body(&summary, "Run");
    assert!(
        run.accessor_candidate.is_none(),
        "a write inside a function literal is not the method's own work"
    );
}

#[test]
fn test_files_mark_their_declarations_and_references() {
    let summary = summarize(
        &discovered("queue/queue_test.go", true),
        r#"
package queue

import "testing"

func TestPush(t *testing.T) {
	q := NewQueue()
	q.Push(Item{})
}
"#,
    );
    assert!(summary.is_test);
    assert!(summary.decls.iter().all(|decl| decl.is_test));
    assert!(summary.refs.iter().all(|reference| reference.is_test));
}
