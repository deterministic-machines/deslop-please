use super::*;
use crate::treesitter::summary::{
    AccessMode, CalleeExpr, FunctionBodySummary, InitExpr, RawRef, RawRefKind, ReceiverExpr,
};
use std::path::PathBuf;

use crate::model::{NodeKind, Tree};

fn file(module: crate::model::NodeId) -> DiscoveredFile {
    DiscoveredFile {
        abs_path: PathBuf::from("/nonexistent/deslop-rust-extract/src/lib.rs"),
        rel_path: "src/lib.rs".to_owned(),
        module_id: module,
        package_id: module,
        language: Language::Rust,
        is_test: false,
    }
}

fn extracted(source: &str) -> FileSummary {
    let mut tree = Tree::new();
    let package = tree.child(tree.root(), NodeKind::Package, "p");
    extract(&file(package), source, &Configuration::default())
}

fn decl<'a>(summary: &'a FileSummary, name: &str) -> &'a ItemDecl {
    summary
        .decls
        .iter()
        .find(|decl| decl.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no declaration {name}: {:?}",
                summary
                    .decls
                    .iter()
                    .map(|decl| (decl.name.as_str(), decl.kind))
                    .collect::<Vec<_>>()
            )
        })
}

fn named<'a>(summary: &'a FileSummary, name: &str) -> Vec<&'a ItemDecl> {
    summary
        .decls
        .iter()
        .filter(|decl| decl.name == name)
        .collect()
}

fn body<'a>(summary: &'a FileSummary, name: &str) -> &'a FunctionBodySummary {
    summary
        .decls
        .iter()
        .filter(|decl| decl.name == name && decl.kind == ItemKind::Function)
        .find_map(|decl| decl.body_summary.as_ref())
        .unwrap_or_else(|| panic!("{name} has no body summary"))
}

fn member_refs<'a>(
    summary: &'a FileSummary,
    member: &str,
) -> Vec<(&'a ReceiverExpr, bool, AccessMode)> {
    summary
        .refs
        .iter()
        .filter_map(|reference| match &reference.kind {
            RawRefKind::MemberAccess {
                receiver,
                member: name,
                is_call,
            } if name == member => Some((receiver, *is_call, reference.access)),
            _ => None,
        })
        .collect()
}

fn modes(summary: &FileSummary, member: &str, call: bool) -> Vec<AccessMode> {
    member_refs(summary, member)
        .into_iter()
        .filter(|(_, is_call, _)| *is_call == call)
        .map(|(_, _, mode)| mode)
        .collect()
}

fn path_refs(summary: &FileSummary, last: &str) -> Vec<Vec<String>> {
    summary
        .refs
        .iter()
        .filter_map(|reference| match &reference.kind {
            RawRefKind::Path(path) if path.last().is_some_and(|name| name == last) => {
                Some(path.clone())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn types_and_their_fields_are_declared_with_parents() {
    let summary = extracted(
        r#"
pub struct Queue { pub items: Vec<u32>, head: usize, marker: PhantomData<fn()> }
pub struct Pair(u32, pub i32);
pub enum State { Idle, Busy { at: u32 }, Done(u8) }
"#,
    );
    let queue = decl(&summary, "Queue");
    assert_eq!(queue.kind, ItemKind::Type);
    assert!(queue.is_exported);
    let fields: Vec<(&ItemDecl, Option<LocalDeclId>)> = named(&summary, "items")
        .into_iter()
        .chain(named(&summary, "head"))
        .chain(named(&summary, "marker"))
        .map(|field| (field, field.parent_decl))
        .collect();
    assert_eq!(fields.len(), 3);
    assert!(fields.iter().all(
        |(field, parent)| *parent == Some(queue.local_id) && field.kind == ItemKind::Field
    ));
    assert!(decl(&summary, "marker").holds_nothing);
    assert!(decl(&summary, "items").is_exported);
    assert!(!decl(&summary, "head").is_exported);

    let pair = decl(&summary, "Pair");
    let tuple: Vec<&ItemDecl> = summary
        .decls
        .iter()
        .filter(|field| field.parent_decl == Some(pair.local_id))
        .collect();
    assert_eq!(tuple.len(), 2, "tuple fields are numbered");
    assert_eq!((tuple[0].name.as_str(), tuple[1].name.as_str()), ("0", "1"));
    assert!(!tuple[0].is_exported);
    assert!(tuple[1].is_exported);

    let state = decl(&summary, "State");
    let busy = decl(&summary, "Busy");
    assert_eq!(busy.kind, ItemKind::Variant);
    assert_eq!(busy.parent_decl, Some(state.local_id));
    assert_eq!(decl(&summary, "at").parent_decl, Some(busy.local_id));
}

#[test]
fn traits_and_impls_carry_their_targets_and_receivers() {
    let summary = extracted(
        r#"
trait Show { fn show(&self) -> String; const LIMIT: usize; }
struct Queue;
impl Show for Queue {
fn show(&self) -> String { String::new() }
const LIMIT: usize = 1;
}
impl Queue {
fn reset(&mut self) {}
fn head(&self) -> usize { 0 }
}
"#,
    );
    let implementations: Vec<&ItemDecl> = summary
        .decls
        .iter()
        .filter(|decl| decl.kind == ItemKind::Impl)
        .collect();
    assert_eq!(implementations.len(), 2);
    assert!(implementations[0].trait_impl);
    assert_eq!(
        implementations[0].impl_trait.as_ref().unwrap().path,
        ["Show"]
    );
    assert_eq!(
        implementations[0].impl_target.as_ref().unwrap().path,
        ["Queue"]
    );
    assert!(!implementations[1].trait_impl);

    let show = named(&summary, "show")[1];
    assert!(show.trait_impl && show.takes_self && show.self_is_shared_ref);
    let limit = named(&summary, "LIMIT")[1];
    assert_eq!(limit.kind, ItemKind::Constant);
    assert_eq!(limit.parent_decl, Some(implementations[0].local_id));
    let reset = named(&summary, "reset")[0];
    assert!(reset.takes_self && !reset.self_is_shared_ref && !reset.trait_impl);
}

#[test]
fn use_declarations_are_flattened_with_their_kinds() {
    let summary = extracted(
        r#"
use crate::a::{self, b::C as D, e::*};
use super::x;
pub use crate::y::Z;
use crate::w as namespace;
"#,
    );
    let imports: Vec<(Vec<&str>, String)> = summary
        .imports
        .iter()
        .map(|import| {
            let path: Vec<&str> = import.path_segments.iter().map(String::as_str).collect();
            let kind = match &import.kind {
                ImportKind::Named { name, alias } => {
                    format!(
                        "named {name}{}",
                        alias
                            .as_ref()
                            .map(|a| format!(" as {a}"))
                            .unwrap_or_default()
                    )
                }
                ImportKind::Namespace { alias } => format!("namespace {alias}"),
                ImportKind::Glob => "glob".to_owned(),
                ImportKind::Default { .. } | ImportKind::SideEffect => {
                    unreachable!("no Rust import is a default or side-effect one")
                }
            };
            (path, kind)
        })
        .collect();
    assert!(imports.contains(&(vec!["crate", "a"], "namespace a".to_owned())));
    assert!(imports.contains(&(vec!["crate", "a", "b", "C"], "named C as D".to_owned())));
    assert!(imports.contains(&(vec!["crate", "a", "e"], "glob".to_owned())));
    assert!(imports.contains(&(vec!["super", "x"], "namespace x".to_owned())));
    assert!(imports.contains(&(vec!["crate", "y", "Z"], "namespace Z".to_owned())));
    assert!(imports.contains(&(vec!["crate", "w"], "named w as namespace".to_owned())));
    assert!(
        summary
            .imports
            .iter()
            .any(|import| import.is_reexport && import.path_segments == ["crate", "y", "Z"])
    );
}

#[test]
fn inline_modules_carry_scopes_and_test_names() {
    let summary = extracted(
        r#"
mod outer {
pub fn f() {}
mod inner { pub fn g() {} }
}
mod tests { fn t() {} }
#[cfg(test)]
mod gated { fn g() {} }
"#,
    );
    let modules: Vec<(&str, ModuleScope, bool)> = summary
        .inline_modules
        .iter()
        .map(|module| (module.name.as_str(), module.parent, module.is_test))
        .collect();
    assert_eq!(modules[0], ("outer", ModuleScope::FileRoot, false));
    assert_eq!(
        modules[1],
        (
            "inner",
            ModuleScope::Inline(summary.inline_modules[0].local_id),
            false
        )
    );
    assert_eq!(modules[2].0, "tests");
    assert!(modules[2].2);
    assert_eq!(modules[3].0, "gated");
    assert!(modules[3].2);
    let inner_g = named(&summary, "g")[0];
    assert!(inner_g.scope != ModuleScope::FileRoot);
    assert!(named(&summary, "t")[0].is_test);
    assert!(named(&summary, "g")[1].is_test);
}

#[test]
fn cfg_gates_items_and_marks_test_items_without_skipping_them() {
    let summary = extracted(
        r#"
#[cfg(feature = "extra")]
pub fn extra() {}
#[cfg(test)]
pub fn only_tests() {}
#[test]
fn checks() { assert!(true); }
pub fn plain() {}
"#,
    );
    assert!(named(&summary, "extra").is_empty());
    assert!(named(&summary, "only_tests")[0].is_test);
    assert!(named(&summary, "checks")[0].is_test);
    assert!(!named(&summary, "plain")[0].is_test);
}

#[test]
fn function_bodies_record_locals_and_computation() {
    let summary = extracted(
        r#"
struct Queue { items: Vec<u32>, head: usize }
impl Queue {
fn len(&self) -> usize { self.items.len() }
fn make() -> Queue { Queue { items: Vec::new(), head: 0 } }
}
fn plain(q: &Queue) -> usize { let head = q.head; head }
fn adds(a: usize, b: usize) -> usize { a + b }
fn branches(q: &Queue) -> usize { if q.head > 0 { 0 } else { 1 } }
fn borrow(q: &Queue) -> &[u32] { &q.items }
fn derefs(q: &Queue) -> usize { *&q.head }
fn clones(q: &Queue) -> Queue { q.clone() }
fn makes() -> Queue { Queue::new() }
fn checks(q: &Queue) -> bool { q.items.len() > 1 }
"#,
    );
    let plain = body(&summary, "plain");
    assert!(!plain.computes);
    assert_eq!(plain.locals.len(), 1);
    assert_eq!(plain.locals[0].name, "head");
    assert_eq!(plain.locals[0].declared_type, None);
    match plain.locals[0].init.as_ref().unwrap() {
        InitExpr::FieldChain { base, fields } => {
            assert_eq!(base, &ReceiverExpr::Local("q".to_owned()));
            assert_eq!(fields, &["head"]);
        }
        other => panic!("unexpected init: {other:?}"),
    }
    assert!(body(&summary, "adds").computes);
    assert!(body(&summary, "branches").computes);
    assert!(!body(&summary, "borrow").computes);
    assert!(!body(&summary, "derefs").computes);
    assert!(!body(&summary, "clones").computes);
    assert!(body(&summary, "makes").computes);
    assert!(body(&summary, "checks").computes);
    assert!(body(&summary, "make").locals.is_empty());
    assert_eq!(
        decl(&summary, "make")
            .signature
            .as_ref()
            .unwrap()
            .params
            .len(),
        0
    );
}

#[test]
fn forwarding_candidates_record_callee_and_self_fields() {
    let summary = extracted(
        r#"
struct Wrapper { inner: Vec<u32> }
impl Wrapper {
fn push(&self, task: u32) -> bool { self.inner.push(task) }
fn get(&self, key: &u32) -> Option<u32> { self.inner.get(key) }
fn span(&self, raw: u32) -> u32 { helper(self, raw) }
fn bound(&self) -> u32 { self.target(self.inner) }
fn added(&self, x: u32) -> u32 { self.inner.get(x + 1) }
fn twice(&self, x: u32) -> u32 { let y = self.inner.get(x); y }
fn plain(x: u32) -> u32 { helper(x) }
fn tail(&self, x: u32) -> u32 { return helper(x); }
fn commented(&self, x: u32) -> u32 {
    // a note
    helper(x)
}
}
"#,
    );
    let push = body(&summary, "push")
        .forwarding_candidate
        .as_ref()
        .expect("push forwards");
    assert_eq!(
        push.callee,
        CalleeExpr::Method {
            receiver: ReceiverExpr::SelfValue,
            method: "push".to_owned()
        }
    );
    assert_eq!(push.self_fields, ["inner"]);
    assert_eq!(
        body(&summary, "get")
            .forwarding_candidate
            .as_ref()
            .expect("get forwards")
            .self_fields,
        ["inner"]
    );
    let span = body(&summary, "span")
        .forwarding_candidate
        .as_ref()
        .expect("span forwards");
    assert_eq!(span.callee, CalleeExpr::Path(vec!["helper".to_owned()]));
    assert!(span.self_fields.is_empty());
    for not_forwarding in ["bound", "added", "twice", "tail"] {
        assert!(
            body(&summary, not_forwarding)
                .forwarding_candidate
                .is_none(),
            "{not_forwarding}"
        );
    }
    assert!(body(&summary, "plain").forwarding_candidate.is_some());
    assert_eq!(
        body(&summary, "commented")
            .forwarding_candidate
            .as_ref()
            .unwrap()
            .callee,
        CalleeExpr::Path(vec!["helper".to_owned()])
    );
}

#[test]
fn accessor_candidates_track_lent_written_and_read_fields() {
    let summary = extracted(
        r#"
struct Wrapper { items: Vec<u32>, head: usize }
impl Wrapper {
fn items(&self) -> &Vec<u32> { &self.items }
fn head(&self) -> usize { self.head }
fn set_head(&mut self, head: usize) { self.head = head; }
fn take(self) -> Vec<u32> { self.items }
fn len(&self) -> usize { self.items.len() }
}
struct Table { values: Vec<u32> }
impl Table {
fn value(&self, at: usize) -> u32 { self.values[at] }
fn put(&mut self, at: usize, value: u32) { self.values[at] = value; }
}
"#,
    );
    let items = body(&summary, "items")
        .accessor_candidate
        .as_ref()
        .expect("items is an accessor");
    assert_eq!(items.ops.len(), 1);
    assert_eq!(items.ops[0].field_name, "items");
    assert!(items.ops[0].is_lent_or_written);
    assert!(!items.ops[0].is_write);

    let head = body(&summary, "head")
        .accessor_candidate
        .as_ref()
        .expect("head is an accessor");
    assert!(!head.ops[0].is_lent_or_written);
    assert!(!head.ops[0].is_write);

    let set = body(&summary, "set_head")
        .accessor_candidate
        .as_ref()
        .expect("set_head");
    assert!(set.ops[0].is_lent_or_written && set.ops[0].is_write);

    let take = body(&summary, "take")
        .accessor_candidate
        .as_ref()
        .expect("take");
    assert!(!take.ops[0].is_lent_or_written);

    assert!(
        body(&summary, "len").accessor_candidate.is_none(),
        "len computes"
    );

    let value = body(&summary, "value")
        .accessor_candidate
        .as_ref()
        .expect("value reads");
    assert!(
        !value.ops[0].is_lent_or_written,
        "reading through an index copies"
    );
    assert!(!value.ops[0].is_write);

    let put = body(&summary, "put")
        .accessor_candidate
        .as_ref()
        .expect("put writes");
    assert!(put.ops[0].is_lent_or_written && put.ops[0].is_write);
}

#[test]
fn member_accesses_carry_their_syntactic_access_mode() {
    let summary = extracted(
        r#"
struct Queue { items: Vec<u32>, head: usize }
fn work(q: &mut Queue) {
let _ = q.head;
q.head = 1;
q.head += 1;
let _ = &q.items;
let _ = &mut q.items;
let _ = q.items[0];
let _ = q.items.len();
q.items.push(1);
let _ = q.items.clone();
let _ = q.items.get(0);
let _ = q.items.get();
}
"#,
    );
    assert_eq!(
        modes(&summary, "head", false),
        [AccessMode::CopyOrRead, AccessMode::Work, AccessMode::Work]
    );
    assert_eq!(
        modes(&summary, "items", false),
        [
            AccessMode::SharedBorrow,
            AccessMode::Work,
            AccessMode::Work,
            AccessMode::MethodCall,
            AccessMode::Work,
            AccessMode::CopyOrRead,
            AccessMode::MethodCall,
            AccessMode::CopyOrRead,
        ]
    );
    assert!(
        member_refs(&summary, "len")
            .iter()
            .all(|(_, call, _)| *call)
    );
    assert_eq!(
        member_refs(&summary, "items")[3].0,
        &ReceiverExpr::Local("q".to_owned())
    );
}

#[test]
fn method_call_access_follows_what_the_caller_does_with_the_result() {
    let summary = extracted(
        r#"
struct Queue { items: Vec<u32>, head: usize }
impl Queue {
fn items(&self) -> &Vec<u32> { &self.items }
fn head(&self) -> usize { self.head }
}
fn uses(q: &Queue) {
let _ = q.items().get(0);
let _ = q.items().clone();
let _ = q.items().len();
let _ = q.head() + 1;
let _ = q.items();
}
"#,
    );
    assert_eq!(
        modes(&summary, "items", true),
        [
            AccessMode::MethodCall,
            AccessMode::CopyOrRead,
            AccessMode::MethodCall,
            AccessMode::CopyOrRead,
        ]
    );
    assert_eq!(modes(&summary, "head", true), [AccessMode::CopyOrRead]);
}

#[test]
fn receivers_are_structured_through_fields_calls_and_self() {
    let summary = extracted(
        r#"
struct Inner { buf: Vec<u32> }
struct Wrapper { inner: Inner }
impl Wrapper {
fn first(&self) -> u32 { self.inner.buf[0] + self.inner.buf.len() as u32 }
fn wrap(&self) -> u32 { create().inner.buf.len() as u32 }
}
"#,
    );
    let buf = member_refs(&summary, "buf");
    assert!(buf.iter().any(|(receiver, _, _)| matches!(
        receiver,
        ReceiverExpr::FieldChain { base, fields }
        if **base == ReceiverExpr::SelfValue && fields == &["inner"]
    )));
    let len = member_refs(&summary, "len");
    assert!(len.iter().any(|(receiver, is_call, _)| {
        *is_call
            && matches!(receiver, ReceiverExpr::FieldChain { base, fields }
            if **base == ReceiverExpr::SelfValue && fields == &["inner", "buf"])
    }));
    let call = len.iter().find_map(|(receiver, _, _)| match receiver {
        ReceiverExpr::FieldChain { base, fields }
            if matches!(**base, ReceiverExpr::CallResult { .. }) =>
        {
            Some(fields.clone())
        }
        _ => None,
    });
    assert_eq!(call, Some(vec!["inner".to_owned(), "buf".to_owned()]));
}

#[test]
fn type_references_are_attributed_to_their_holders() {
    let summary = extracted(
        r#"
struct Holder { ring: Ring }
type Alias = crate::kinds::Kind;
const LIMIT: usize = 0;
impl Holder {
fn new(ring: &Ring) -> Option<Holder> {
    let holder = Holder { ring: Ring::new() };
    Some(holder)
}
}
"#,
    );
    let field = decl(&summary, "ring");
    let new = decl(&summary, "new");
    let ring_refs: Vec<&RawRef> = summary
        .refs
        .iter()
        .filter(|reference| matches!(&reference.kind, RawRefKind::Path(path) if path == &["Ring"]))
        .collect();
    assert!(
        ring_refs
            .iter()
            .any(|reference| reference.holder == Some(field.local_id))
    );
    assert!(
        ring_refs
            .iter()
            .any(|reference| reference.holder == Some(new.local_id))
    );
    assert_eq!(
        path_refs(&summary, "Kind"),
        [vec![
            "crate".to_owned(),
            "kinds".to_owned(),
            "Kind".to_owned()
        ]]
    );
    let kind_holder = summary
        .refs
        .iter()
        .find(|reference| {
            matches!(&reference.kind, RawRefKind::Path(path) if path.last().is_some_and(|name| name == "Kind"))
        })
        .and_then(|reference| reference.holder);
    assert_eq!(kind_holder, Some(decl(&summary, "Alias").local_id));
    assert!(
        path_refs(&summary, "Holder")
            .iter()
            .any(|path| path == &["Holder"])
    );
    assert!(
        path_refs(&summary, "new")
            .iter()
            .any(|path| path == &["Ring", "new"])
    );
}

#[test]
fn a_struct_literal_names_the_fields_it_builds() {
    let summary = extracted(
        r#"
struct Queue { items: Vec<u32>, head: usize }
fn build(head: usize) -> Queue { Queue { items: Vec::new(), head } }
"#,
    );
    for member in ["items", "head"] {
        let refs = member_refs(&summary, member);
        assert_eq!(refs.len(), 1, "{member}");
        assert_eq!(refs[0].0, &ReceiverExpr::Path(vec!["Queue".to_owned()]));
        assert!(!refs[0].1);
        assert_eq!(refs[0].2, AccessMode::CopyOrRead);
    }
}

#[test]
fn macro_arguments_hold_refs_as_if_they_were_written_plainly() {
    let summary = extracted(
        "struct Flags { names: Vec<String>, target: String }\nimpl Flags {\n    fn describe(&self) -> String {\n        format!(\"{}: {}\", self.target, self.names.join(\",\"))\n    }\n    fn empty(&self) -> Vec<String> {\n        vec![]\n    }\n}\nfn report(flags: &Flags) {\n    println!(\"{}\", flags.names.len());\n}\n",
    );
    let lines: Vec<u32> = summary
        .refs
        .iter()
        .filter(|reference| {
            matches!(&reference.kind, RawRefKind::MemberAccess { member, .. } if member == "names")
        })
        .map(|reference| reference.line)
        .collect();
    assert_eq!(lines, [4, 11]);
    assert_eq!(modes(&summary, "target", false), [AccessMode::CopyOrRead]);
    assert!(!body(&summary, "empty").computes);
}

#[test]
fn a_file_reports_its_path_metadata_and_line_count() {
    let summary = extracted("//! docs\n\npub fn f() {}\n\n");
    assert_eq!(summary.file_path, "src/lib.rs");
    assert_eq!(summary.language, Language::Rust);
    assert!(!summary.is_test);
    assert_eq!(summary.lines, 4);
}
