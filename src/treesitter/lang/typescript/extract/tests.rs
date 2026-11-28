use std::path::PathBuf;

use super::*;
use crate::model::{ItemKind, Tree};
use crate::treesitter::summary::{InitExpr, RawRefKind};

fn discovered(name: &str) -> DiscoveredFile {
    let module = Tree::new().root();
    DiscoveredFile {
        abs_path: PathBuf::from("/workspace").join(name),
        rel_path: name.to_owned(),
        module_id: module,
        package_id: module,
        language: Language::TypeScript,
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
                "no declaration `{name}` among {:?}",
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

fn member<'a>(summary: &'a FileSummary, name: &str) -> Vec<&'a RawRef> {
    summary
        .refs
        .iter()
        .filter(|reference| match &reference.kind {
            RawRefKind::MemberAccess { member, .. } => member == name,
            _ => false,
        })
        .collect()
}

fn paths(summary: &FileSummary) -> Vec<Vec<String>> {
    summary
        .refs
        .iter()
        .filter_map(|reference| match &reference.kind {
            RawRefKind::Path(path) => Some(path.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_import_and_reexport_form_is_extracted() {
    let summary = summary(
        "src/imports.ts",
        r#"
import { A, B as C } from './x';
import Foo from './foo';
import * as ns from './ns';
import './side';
export { A, B as C } from './y';
export * from './z';
export * as all from './all';
const { a, b: bee } = require('./cjs');
const r2 = require('./r2');
"#,
    );
    let specifiers: Vec<&str> = summary
        .imports
        .iter()
        .map(|import| import.raw_specifier.as_deref().unwrap())
        .collect();
    assert_eq!(
        specifiers,
        [
            "./x", "./x", "./foo", "./ns", "./side", "./y", "./y", "./z", "./all", "./cjs",
            "./cjs", "./r2",
        ]
    );

    let named: Vec<_> = summary
        .imports
        .iter()
        .filter(|import| matches!(import.kind, ImportKind::Named { .. }))
        .collect();
    assert_eq!(named.len(), 6);
    assert!(matches!(&named[0].kind, ImportKind::Named { name, alias: None } if name == "A"));
    assert!(
        matches!(&named[1].kind, ImportKind::Named { name, alias: Some(alias) } if name == "B" && alias == "C")
    );
    assert!(!named[0].is_reexport);
    assert!(named[2].is_reexport && named[3].is_reexport);
    assert!(
        !named[4].is_reexport && !named[5].is_reexport,
        "CommonJS requires are imports"
    );

    assert!(matches!(&summary.imports[2].kind, ImportKind::Default { alias } if alias == "Foo"));
    assert!(matches!(&summary.imports[3].kind, ImportKind::Namespace { alias } if alias == "ns"));
    assert!(matches!(&summary.imports[4].kind, ImportKind::SideEffect));
    assert!(matches!(&summary.imports[7].kind, ImportKind::Glob));
    assert!(summary.imports[7].is_reexport);
    assert!(matches!(&summary.imports[8].kind, ImportKind::Namespace { alias } if alias == "all"));
    assert!(matches!(&summary.imports[9].kind, ImportKind::Named { name, .. } if name == "a"));
    assert!(
        matches!(&summary.imports[10].kind, ImportKind::Named { name, alias: Some(alias) } if name == "b" && alias == "bee")
    );
    assert!(matches!(&summary.imports[11].kind, ImportKind::Default { alias } if alias == "r2"));
}

#[test]
fn classes_yield_fields_variants_methods_and_wrapper_candidates() {
    let summary = summary(
        "src/queue.ts",
        r#"
export class Queue<T> {
  private items: Item[] = [];
  public head: number = 0;
  static count = 0;
  constructor(private ring: Ring, public name: string) {}
  get size(): number { return this.items.length; }
  get inner(): Ring { return this.ring; }
  set size(v: number) { this.head = v; }
  push(item: Item): void { this.items.push(item); }
  peek(): Item | undefined { return this.items[0]; }
  add(item: Item): void { this.items.push(item); this.head++; }
  empty(): boolean { return this.head === 0; }
  static make(): Queue<Item> { return new Queue(); }
}
"#,
    );
    let queue = decl(&summary, "Queue");
    assert_eq!(queue.kind, ItemKind::Type);
    assert!(queue.is_exported);
    assert_eq!(queue.parent_decl, None);

    let field = |name: &str| {
        let found = decl(&summary, name);
        assert_eq!(found.kind, ItemKind::Field, "`{name}` is a field");
        assert_eq!(found.parent_decl, Some(queue.local_id));
        found
    };
    assert_eq!(
        field("items").type_annotation.as_ref().unwrap().path,
        ["Array"]
    );
    assert_eq!(
        field("head").type_annotation.as_ref().unwrap().path,
        ["number"]
    );
    assert!(field("count").type_annotation.is_none());
    assert!(field("ring").type_annotation.is_some());
    assert!(field("name").type_annotation.is_some());

    let push = decl(&summary, "push");
    assert_eq!(push.kind, ItemKind::Function);
    assert_eq!(push.parent_decl, Some(queue.local_id));
    assert!(push.takes_self);
    assert!(!push.self_is_shared_ref, "push mutates this.items");
    let body = push.body_summary.as_ref().expect("push has a body");
    assert!(!body.computes);
    let forwarding = body.forwarding_candidate.as_ref().expect("push forwards");
    assert_eq!(forwarding.self_fields, ["items"]);
    assert!(matches!(
        &forwarding.callee,
        CalleeExpr::Method { receiver: ReceiverExpr::FieldChain { base, fields }, method }
            if matches!(**base, ReceiverExpr::SelfValue) && fields == &["items"] && method == "push"
    ));

    let sizes = decls_named(&summary, "size");
    assert_eq!(sizes.len(), 2, "a getter and a setter");
    let getter = sizes[0];
    assert_eq!(getter.kind, ItemKind::Function);
    assert!(getter.self_is_shared_ref);
    assert!(
        getter
            .body_summary
            .as_ref()
            .unwrap()
            .accessor_candidate
            .is_none()
    );

    let inner = decl(&summary, "inner");
    assert!(inner.self_is_shared_ref);
    let accessor = inner
        .body_summary
        .as_ref()
        .unwrap()
        .accessor_candidate
        .as_ref()
        .expect("inner hands out a field");
    assert_eq!(accessor.ops.len(), 1);
    assert_eq!(accessor.ops[0].field_name, "ring");
    assert!(
        !accessor.ops[0].is_lent_or_written,
        "TypeScript has no borrow syntax: returning a field is a plain read"
    );
    assert!(!accessor.ops[0].is_write);

    let setter = sizes[1];
    assert!(matches!(setter.kind, ItemKind::Function));
    assert!(!setter.self_is_shared_ref, "a setter mutates this");
    let accessor = setter
        .body_summary
        .as_ref()
        .unwrap()
        .accessor_candidate
        .as_ref()
        .expect("the setter overwrites a field");
    assert_eq!(accessor.ops[0].field_name, "head");
    assert!(accessor.ops[0].is_write);
    assert!(accessor.ops[0].is_lent_or_written);

    let add = decl(&summary, "add");
    assert!(!add.self_is_shared_ref);
    assert!(
        add.body_summary
            .as_ref()
            .unwrap()
            .forwarding_candidate
            .is_none()
    );
    assert!(add.body_summary.as_ref().unwrap().computes);

    let empty = decl(&summary, "empty");
    assert!(
        empty.body_summary.as_ref().unwrap().computes,
        "comparison computes"
    );

    let make = decl(&summary, "make");
    assert!(!make.takes_self);
    assert!(make.body_summary.as_ref().unwrap().computes);

    let peek = decl(&summary, "peek");
    assert!(matches!(
        peek.signature
            .as_ref()
            .unwrap()
            .return_type
            .as_ref()
            .unwrap()
            .path[0]
            .as_str(),
        "Item"
    ));
}

#[test]
fn interfaces_type_aliases_and_enums_are_types_with_variants() {
    let summary = summary(
        "src/kinds.ts",
        r#"
export interface Shape extends Base {
  readonly kind: string;
  area(): number;
}
export type Alias<T> = { value: T } | null;
export enum Color { Red = 1, Green, Blue }
"#,
    );
    let shape = decl(&summary, "Shape");
    assert_eq!(shape.kind, ItemKind::Type);
    assert!(shape.is_exported);
    assert_eq!(decl(&summary, "Alias").kind, ItemKind::Type);
    let color = decl(&summary, "Color");
    assert_eq!(color.kind, ItemKind::Type);
    let variants: Vec<&str> = summary
        .decls
        .iter()
        .filter(|decl| decl.kind == ItemKind::Variant)
        .map(|decl| decl.name.as_str())
        .collect();
    assert_eq!(variants, ["Red", "Green", "Blue"]);
    assert!(
        summary
            .decls
            .iter()
            .filter(|decl| decl.kind == ItemKind::Variant)
            .all(|decl| decl.parent_decl == Some(color.local_id))
    );

    let paths = paths(&summary);
    assert!(
        paths.contains(&vec!["Base".to_owned()]),
        "extends is a type reference: {paths:?}"
    );
}

#[test]
fn functions_and_arrow_constants_carry_signatures_and_bodies() {
    let summary = summary(
        "src/functions.ts",
        r#"
export function forward(q: Queue<Item>, item: Item): void { q.push(item); }
export const arrow = (x: number): number => x * 2;
const plain = function (a: string) { return helper(a); };
const value = 3;
"#,
    );
    let forward = decl(&summary, "forward");
    assert_eq!(forward.kind, ItemKind::Function);
    assert!(forward.is_exported);
    assert!(!forward.takes_self);
    let signature = forward.signature.as_ref().unwrap();
    assert_eq!(
        signature
            .params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["q", "item"]
    );
    assert_eq!(signature.params[0].ty.as_ref().unwrap().path, ["Queue"]);
    assert_eq!(signature.params[1].ty.as_ref().unwrap().path, ["Item"]);
    let body = forward.body_summary.as_ref().unwrap();
    assert!(!body.computes, "a bare forwarding call computes nothing");
    assert_eq!(
        body.forwarding_candidate.as_ref().unwrap().callee,
        CalleeExpr::Method {
            receiver: ReceiverExpr::Local("q".to_owned()),
            method: "push".to_owned(),
        }
    );
    assert!(
        body.forwarding_candidate
            .as_ref()
            .unwrap()
            .self_fields
            .is_empty()
    );

    let arrow = decl(&summary, "arrow");
    assert_eq!(arrow.kind, ItemKind::Function);
    assert!(arrow.is_exported);
    assert_eq!(
        arrow
            .signature
            .as_ref()
            .unwrap()
            .return_type
            .as_ref()
            .unwrap()
            .path,
        ["number"]
    );
    assert!(arrow.body_summary.as_ref().unwrap().computes);

    let plain = decl(&summary, "plain");
    assert_eq!(plain.kind, ItemKind::Function);
    assert_eq!(
        plain
            .body_summary
            .as_ref()
            .unwrap()
            .forwarding_candidate
            .as_ref()
            .unwrap()
            .callee,
        CalleeExpr::Path(vec!["helper".to_owned()])
    );
    assert!(!plain.body_summary.as_ref().unwrap().computes);

    let value = decl(&summary, "value");
    assert_eq!(value.kind, ItemKind::Constant);
}

#[test]
fn local_bindings_record_types_constructed_values_calls_chains_and_copies() {
    let summary = summary(
        "src/locals.ts",
        r#"
function f(q: Queue, item: Item) {
  const a: number = 1;
  let b = new Queue<Item>();
  const c = make();
  const d = q.items;
  const e = b;
}
"#,
    );
    let body = decl(&summary, "f").body_summary.as_ref().unwrap();
    let names: Vec<&str> = body
        .locals
        .iter()
        .map(|local| local.name.as_str())
        .collect();
    assert_eq!(names, ["a", "b", "c", "d", "e"]);
    assert_eq!(
        body.locals[0].declared_type.as_ref().unwrap().path,
        ["number"]
    );
    assert!(body.locals[0].init.is_none());
    assert!(
        matches!(&body.locals[1].init, Some(InitExpr::StructLiteral(path)) if path == &["Queue"])
    );
    assert!(
        matches!(&body.locals[2].init, Some(InitExpr::Call(CalleeExpr::Path(path))) if path == &["make"])
    );
    assert!(matches!(
        &body.locals[3].init,
        Some(InitExpr::FieldChain { base: ReceiverExpr::Local(base), fields })
            if base == "q" && fields == &["items"]
    ));
    assert!(matches!(&body.locals[4].init, Some(InitExpr::Var(name)) if name == "b"));
}

#[test]
fn member_references_carry_accurate_access_modes() {
    let summary = summary(
        "src/refs.ts",
        r#"
import * as ns from './ns';
function f(q: Queue, item: Item) {
  q.items = [];
  q.items[0] = item;
  const n = q.count;
  q.ring.push(item);
  const m = q.items.length;
  q.count++;
  delete q.cache;
  q.other();
  ns.helper();
}
"#,
    );
    assert_eq!(member(&summary, "items")[0].access, AccessMode::Work);
    assert_eq!(member(&summary, "items")[1].access, AccessMode::Work);
    assert_eq!(member(&summary, "count")[0].access, AccessMode::CopyOrRead);
    assert_eq!(member(&summary, "count")[1].access, AccessMode::Work);
    assert_eq!(member(&summary, "cache")[0].access, AccessMode::Work);

    let push = member(&summary, "push")[0];
    assert_eq!(push.access, AccessMode::Work);
    assert!(matches!(
        &push.kind,
        RawRefKind::MemberAccess {
            receiver: ReceiverExpr::FieldChain { base, fields },
            member,
            is_call: true,
        } if matches!(**base, ReceiverExpr::Local(ref name) if name == "q")
            && fields == &["ring"] && member == "push"
    ));

    let length = member(&summary, "length")[0];
    assert_eq!(length.access, AccessMode::CopyOrRead);
    assert!(matches!(
        &length.kind,
        RawRefKind::MemberAccess { receiver: ReceiverExpr::FieldChain { fields, .. }, is_call: false, .. }
            if fields == &["items"]
    ));

    assert_eq!(member(&summary, "other")[0].access, AccessMode::MethodCall);
    assert!(paths(&summary).contains(&vec!["ns".to_owned(), "helper".to_owned()]));
    assert!(
        member(&summary, "helper").is_empty(),
        "namespace calls are path references"
    );
}

#[test]
fn mutating_method_calls_are_work_and_other_calls_are_method_calls() {
    let summary = summary(
        "src/fields.ts",
        r#"
class Counter {
  private hits: number = 0;
  private items: Item[] = [];
  private ring: Ring = new Ring();
  tick(): number { return this.hits.toFixed(0); }
  size(): number { return this.items.length; }
  clear(): void { this.items.length = 0; }
  advance(): void { this.ring.push(item); }
}
"#,
    );
    let tick = decl(&summary, "tick").body_summary.as_ref().unwrap();
    assert!(tick.computes, "a method call computes");
    let call = member(&summary, "toFixed")[0];
    assert_eq!(
        call.access,
        AccessMode::MethodCall,
        "a non-mutating method call"
    );
    let advance = decl(&summary, "advance").body_summary.as_ref().unwrap();
    assert!(advance.computes);
    assert_eq!(member(&summary, "push")[0].access, AccessMode::Work);
}

#[test]
fn tsx_components_parse_with_the_tsx_grammar() {
    let summary = summary(
        "src/App.tsx",
        r#"
import React from 'react';
import { Item } from './item';
export function App({ items }: Props): JSX.Element {
  return <ul>{items.map((entry) => <Item key={entry.id} />)}</ul>;
}
export const Header = () => <h1>Hi</h1>;
"#,
    );
    let app = decl(&summary, "App");
    assert_eq!(app.kind, ItemKind::Function);
    assert!(app.is_exported);
    assert!(!app.is_default_export);
    let body = app.body_summary.as_ref().unwrap();
    assert!(body.computes, "calling items.map computes");
    assert!(body.forwarding_candidate.is_none());
    let map = member(&summary, "map")[0];
    assert!(
        matches!(&map.kind, RawRefKind::MemberAccess { receiver: ReceiverExpr::Local(name), is_call: true, .. } if name == "items")
    );

    let header = decl(&summary, "Header");
    assert_eq!(header.kind, ItemKind::Function);
    assert!(header.is_exported);
}

#[test]
fn default_exports_mark_the_declaration() {
    let summary = summary(
        "src/default.ts",
        "export default class Service { run(): void {} }\nexport default function boot(): void {}",
    );
    let service = decl(&summary, "Service");
    assert!(service.is_exported);
    assert!(service.is_default_export);
    let boot = decl(&summary, "boot");
    assert!(boot.is_default_export);
}

#[test]
fn exporting_a_local_name_marks_its_declaration() {
    let summary = summary(
        "src/reexport.ts",
        r#"
const boot = () => 1;
class Hidden {}
export { boot as default, Hidden };
"#,
    );
    let boot = decl(&summary, "boot");
    assert!(boot.is_exported);
    assert!(boot.is_default_export);
    let hidden = decl(&summary, "Hidden");
    assert!(hidden.is_exported);
    assert!(!hidden.is_default_export);
}

#[test]
fn signature_types_keep_array_shorthand_and_primitives() {
    let summary = summary(
        "src/getters.ts",
        r#"
class Store {
  private items: number[] = [];
  private head: number = 0;
  get all(): number[] { return this.items; }
  set headValue(v: number) { this.head = v; }
}"#,
    );
    let all = decl(&summary, "all");
    let return_type = all
        .signature
        .as_ref()
        .unwrap()
        .return_type
        .as_ref()
        .unwrap();
    assert_eq!(return_type.path, ["Array"]);
    assert_eq!(return_type.args[0].path, ["number"]);
    assert!(
        all.body_summary
            .as_ref()
            .unwrap()
            .accessor_candidate
            .is_some()
    );
    let setter = decl(&summary, "headValue");
    let parameter = setter.signature.as_ref().unwrap().params[0]
        .ty
        .as_ref()
        .unwrap();
    assert_eq!(parameter.path, ["number"]);
    assert!(
        setter
            .body_summary
            .as_ref()
            .unwrap()
            .accessor_candidate
            .is_some()
    );
}

#[test]
fn type_annotations_become_path_references() {
    let summary = summary(
        "src/types.ts",
        r#"
import { Queue } from './queue';
export function drain(q: Queue<Item>): Ring { return q.items[0]; }
"#,
    );
    let paths = paths(&summary);
    assert!(paths.contains(&vec!["Queue".to_owned()]));
    assert!(paths.contains(&vec!["Item".to_owned()]));
    assert!(paths.contains(&vec!["Ring".to_owned()]));
    assert!(
        !paths
            .iter()
            .any(|path| path == &["Queue"] && path.len() > 1)
    );
}

#[test]
fn line_counts_and_file_identity_are_reported() {
    let summary = summary("src/lines.ts", "const a = 1;\nconst b = 2;\n");
    assert_eq!(summary.file_path, "src/lines.ts");
    assert_eq!(summary.language, Language::TypeScript);
    assert_eq!(summary.lines, 2);
    assert_eq!(summary.decls.len(), 2);
}
