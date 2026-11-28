//! Unit tests for the resolver: module tree and inline modules, re-export
//! chains, `impl` binding, local type propagation, forwarders and accessors.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::Error;
use crate::configuration::Configuration;
use crate::model::{ItemId, ItemKind, Model, NodeId, NodeKind, Wrapper};
use crate::treesitter::lang::{
    DiscoveredFile, Language, LanguageFrontend, ResolvedImport, WorkspaceLayout,
};
use crate::treesitter::summary::{
    AccessMode, AccessorCandidate, AccessorFieldOp, CalleeExpr, FileSummary, ForwardingCandidate,
    FunctionBodySummary, FunctionSig, ImportKind, InitExpr, InlineModuleDecl, ItemDecl,
    LocalBinding, LocalDeclId, LocalModId, ModuleScope, ParamDecl, RawImport, RawRef, RawRefKind,
    ReceiverExpr, TypeExpr,
};

struct Fixture {
    layout: WorkspaceLayout,
    package: NodeId,
}

impl Fixture {
    fn new() -> Fixture {
        let mut layout = WorkspaceLayout::new();
        let package = layout
            .tree
            .child(layout.tree.root(), NodeKind::Package, "p");
        Fixture { layout, package }
    }

    fn module(&mut self, name: &str) -> NodeId {
        self.layout.tree.child(self.package, NodeKind::Module, name)
    }

    fn file(&mut self, rel_path: &str, module: NodeId, language: Language) -> DiscoveredFile {
        self.file_with_test(rel_path, module, language, false)
    }

    fn file_with_test(
        &mut self,
        rel_path: &str,
        module: NodeId,
        language: Language,
        is_test: bool,
    ) -> DiscoveredFile {
        let file = DiscoveredFile {
            abs_path: PathBuf::from(format!("/workspace/{rel_path}")),
            rel_path: rel_path.to_owned(),
            module_id: module,
            package_id: self.package,
            language,
            is_test,
        };
        self.layout.files.push(file.clone());
        file
    }
}

fn summary(file: &DiscoveredFile, lines: usize) -> FileSummary {
    FileSummary {
        file_path: file.rel_path.clone(),
        module_id: file.module_id,
        language: file.language,
        is_test: file.is_test,
        lines,
        inline_modules: Vec::new(),
        imports: Vec::new(),
        decls: Vec::new(),
        refs: Vec::new(),
    }
}

fn decl(local: u32, name: &str, kind: ItemKind) -> ItemDecl {
    ItemDecl {
        local_id: LocalDeclId(local),
        scope: ModuleScope::FileRoot,
        name: name.to_owned(),
        kind,
        parent_decl: None,
        impl_target: None,
        impl_trait: None,
        is_exported: true,
        is_default_export: false,
        trait_impl: false,
        takes_self: false,
        self_is_shared_ref: false,
        holds_nothing: false,
        is_test: false,
        type_annotation: None,
        signature: None,
        body_summary: None,
    }
}

fn type_decl(local: u32, name: &str) -> ItemDecl {
    decl(local, name, ItemKind::Type)
}

fn field_decl(local: u32, name: &str, ty: TypeExpr) -> ItemDecl {
    let mut field = decl(local, name, ItemKind::Field);
    field.type_annotation = Some(ty);
    field
}

fn function(local: u32, name: &str) -> ItemDecl {
    let mut function = decl(local, name, ItemKind::Function);
    function.signature = Some(signature(&[], None));
    function.body_summary = Some(FunctionBodySummary::default());
    function
}

fn plain(name: &str) -> TypeExpr {
    TypeExpr::simple(name)
}

fn wrapping(name: &str, inner: TypeExpr) -> TypeExpr {
    TypeExpr {
        path: vec![name.to_owned()],
        args: vec![inner],
        is_shared_ref: false,
        is_mut_ref: false,
    }
}

fn signature(params: &[(&str, Option<TypeExpr>)], ret: Option<TypeExpr>) -> FunctionSig {
    FunctionSig {
        params: params
            .iter()
            .map(|(name, ty)| ParamDecl {
                name: (*name).to_owned(),
                ty: ty.clone(),
            })
            .collect(),
        return_type: ret,
    }
}

fn reference(line: u32, holder: Option<u32>, kind: RawRefKind, access: AccessMode) -> RawRef {
    RawRef {
        line,
        scope: ModuleScope::FileRoot,
        holder: holder.map(LocalDeclId),
        kind,
        access,
        is_test: false,
    }
}

fn path_ref(segments: &[&str]) -> RawRefKind {
    RawRefKind::Path(
        segments
            .iter()
            .map(|segment| (*segment).to_owned())
            .collect(),
    )
}

fn member(receiver: ReceiverExpr, name: &str, is_call: bool) -> RawRefKind {
    RawRefKind::MemberAccess {
        receiver,
        member: name.to_owned(),
        is_call,
    }
}

fn import(segments: &[&str], kind: ImportKind, reexport: bool) -> RawImport {
    RawImport {
        scope: ModuleScope::FileRoot,
        path_segments: segments
            .iter()
            .map(|segment| (*segment).to_owned())
            .collect(),
        kind,
        is_reexport: reexport,
    }
}

fn accessor(ops: Vec<AccessorFieldOp>) -> FunctionBodySummary {
    FunctionBodySummary {
        computes: false,
        accessor_candidate: Some(AccessorCandidate { ops }),
        ..Default::default()
    }
}

fn field_op(name: &str, lent: bool, write: bool) -> AccessorFieldOp {
    AccessorFieldOp {
        field_name: name.to_owned(),
        is_lent_or_written: lent,
        is_write: write,
    }
}

fn item_id(model: &Model, name: &str) -> ItemId {
    model
        .items
        .iter()
        .position(|item| item.name == name)
        .map(ItemId::new)
        .unwrap_or_else(|| panic!("no item named `{name}`"))
}

fn child_id(model: &Model, parent_name: &str, name: &str) -> ItemId {
    let parent = model
        .items
        .iter()
        .position(|item| item.name == parent_name && item.kind == ItemKind::Type)
        .map(ItemId::new)
        .unwrap_or_else(|| panic!("no type named `{parent_name}`"));
    model
        .items
        .iter()
        .position(|item| {
            item.parent == Some(parent)
                && item
                    .name
                    .rsplit("::")
                    .next()
                    .is_some_and(|bare| bare == name)
        })
        .map(ItemId::new)
        .unwrap_or_else(|| panic!("no member `{name}` of `{parent_name}`"))
}

/// A frontend that resolves Rust paths through the tree, recording how often
/// it is asked.
struct MockFrontend {
    language: Language,
    calls: AtomicUsize,
}

impl MockFrontend {
    fn new(language: Language) -> MockFrontend {
        MockFrontend {
            language,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

fn target_module(
    layout: &WorkspaceLayout,
    from_module: NodeId,
    segments: &[String],
) -> Option<NodeId> {
    let first = segments.first()?;
    let (mut node, start) = if first == "crate" {
        (layout.tree.package_of(from_module)?, 1)
    } else if let Some(&package) = layout.package_names.get(first) {
        (package, 1)
    } else {
        (from_module, 0)
    };
    for segment in &segments[start..] {
        node = layout.tree.find_child_by_name(node, segment)?;
    }
    Some(node)
}

impl LanguageFrontend for MockFrontend {
    fn language(&self) -> Language {
        self.language
    }

    fn detect(&self, _root: &std::path::Path) -> bool {
        false
    }

    fn discover(
        &self,
        _root: &std::path::Path,
        _configuration: &Configuration,
        _layout: &mut WorkspaceLayout,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn extract_file(
        &self,
        _file: &DiscoveredFile,
        _source: &str,
        _cargo_config: &Configuration,
    ) -> FileSummary {
        panic!("the mock frontend does not parse files")
    }

    fn resolve_import(
        &self,
        layout: &WorkspaceLayout,
        _from_file: &DiscoveredFile,
        from_module: NodeId,
        import: &RawImport,
    ) -> Option<ResolvedImport> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let item_name = match &import.kind {
            ImportKind::Named { name, .. } => Some(name.clone()),
            _ => None,
        };
        let mut segments = import.path_segments.clone();
        if item_name
            .as_ref()
            .is_some_and(|name| segments.last() == Some(name))
        {
            segments.pop();
        }
        let module = target_module(layout, from_module, &segments)?;
        Some(ResolvedImport::Module {
            module,
            item_name,
            is_default: false,
            is_glob: matches!(import.kind, ImportKind::Glob),
        })
    }
}

#[test]
fn inline_modules_are_registered_and_test_modules_marked() {
    let mut fixture = Fixture::new();
    let outer = fixture.module("outer");
    let file = fixture.file("src/outer.rs", outer, Language::Rust);
    let mut outer_summary = summary(&file, 20);
    outer_summary.inline_modules = vec![
        InlineModuleDecl {
            local_id: LocalModId(0),
            parent: ModuleScope::FileRoot,
            name: "tests".to_owned(),
            is_test: false,
        },
        InlineModuleDecl {
            local_id: LocalModId(1),
            parent: ModuleScope::Inline(LocalModId(0)),
            name: "helpers".to_owned(),
            is_test: false,
        },
    ];
    outer_summary.decls.push(type_decl(0, "Queue"));
    let mut helper = function(1, "helper");
    helper.scope = ModuleScope::Inline(LocalModId(1));
    outer_summary.decls.push(helper);

    let model = build_model(fixture.layout, vec![outer_summary], &[]);

    let tests = model
        .tree
        .find_child_by_name(outer, "tests")
        .expect("the tests module is registered");
    assert!(model.tree.node(tests).test, "a `tests` module is test code");
    let helpers = model
        .tree
        .find_child_by_name(tests, "helpers")
        .expect("nested inline modules are registered");
    assert!(model.tree.is_test(helpers), "test-ness is inherited");
    assert_eq!(model.files.len(), 1);
    assert_eq!(model.files[0].module, outer);
    assert_eq!(model.files[0].lines, 20);
    let module_items: Vec<&Item> = model
        .items
        .iter()
        .filter(|item| item.kind == ItemKind::Module)
        .collect();
    assert!(module_items.iter().any(|item| item.module == Some(outer)));
    assert!(module_items.iter().any(|item| item.module == Some(tests)));
    let helper = model
        .items
        .iter()
        .find(|item| item.name == "helper")
        .expect("the inline module's function exists");
    assert_eq!(helper.owner, helpers);
}

#[test]
fn reexport_chains_reach_the_original_definition() {
    let mut fixture = Fixture::new();
    let a = fixture.module("a");
    let b = fixture.module("b");
    let c = fixture.module("c");
    let a_file = fixture.file("src/a.rs", a, Language::Rust);
    let b_file = fixture.file("src/b.rs", b, Language::Rust);
    let c_file = fixture.file("src/c.rs", c, Language::Rust);
    let frontend = MockFrontend::new(Language::Rust);

    let mut a_summary = summary(&a_file, 5);
    a_summary.decls.push(type_decl(0, "Queue"));

    let mut b_summary = summary(&b_file, 5);
    b_summary.imports.push(import(
        &["crate", "a", "Queue"],
        ImportKind::Named {
            name: "Queue".to_owned(),
            alias: Some("Q".to_owned()),
        },
        true,
    ));

    let mut c_summary = summary(&c_file, 5);
    c_summary.imports.push(import(
        &["crate", "b", "Q"],
        ImportKind::Named {
            name: "Q".to_owned(),
            alias: None,
        },
        false,
    ));
    c_summary
        .refs
        .push(reference(3, None, path_ref(&["Q"]), AccessMode::CopyOrRead));

    let model = build_model(
        fixture.layout,
        vec![a_summary, b_summary, c_summary],
        &[&frontend as &dyn LanguageFrontend],
    );
    let queue = model
        .items
        .iter()
        .position(|item| item.name == "Queue" && item.kind == ItemKind::Type)
        .map(ItemId::new)
        .expect("the original type exists");
    assert_eq!(model.references.len(), 1);
    assert_eq!(
        model.references[0].item, queue,
        "the re-exported name points at the original definition"
    );
    assert_eq!(frontend.calls(), 2, "every import was resolved once");
}

#[test]
fn glob_reexports_are_transparent() {
    let mut fixture = Fixture::new();
    let a = fixture.module("a");
    let b = fixture.module("b");
    let c = fixture.module("c");
    let a_file = fixture.file("src/a.rs", a, Language::Rust);
    let b_file = fixture.file("src/b.rs", b, Language::Rust);
    let c_file = fixture.file("src/c.rs", c, Language::Rust);
    let frontend = MockFrontend::new(Language::Rust);

    let mut a_summary = summary(&a_file, 5);
    a_summary.decls.push(type_decl(0, "Queue"));

    let mut b_summary = summary(&b_file, 5);
    b_summary
        .imports
        .push(import(&["crate", "a"], ImportKind::Glob, true));

    let mut c_summary = summary(&c_file, 5);
    c_summary
        .imports
        .push(import(&["crate", "b"], ImportKind::Glob, false));
    c_summary
        .refs
        .push(reference(3, None, path_ref(&["Queue"]), AccessMode::Work));

    let model = build_model(
        fixture.layout,
        vec![a_summary, b_summary, c_summary],
        &[&frontend as &dyn LanguageFrontend],
    );
    let queue = model
        .items
        .iter()
        .position(|item| item.name == "Queue" && item.kind == ItemKind::Type)
        .map(ItemId::new)
        .expect("the original type exists");
    assert_eq!(model.references[0].item, queue);
}

#[test]
fn impl_blocks_bind_methods_to_their_target_type() {
    let mut fixture = Fixture::new();
    let queue_module = fixture.module("queue");
    let file = fixture.file("src/queue.rs", queue_module, Language::Rust);
    let mut queue_summary = summary(&file, 40);
    queue_summary.decls.push(type_decl(0, "Queue"));

    let mut inherent = decl(1, "Queue", ItemKind::Impl);
    inherent.impl_target = Some(plain("Queue"));
    queue_summary.decls.push(inherent);

    let mut push = function(2, "push");
    push.parent_decl = Some(LocalDeclId(1));
    push.takes_self = true;
    queue_summary.decls.push(push);

    let mut display = decl(3, "Display", ItemKind::Impl);
    display.impl_target = Some(plain("Queue"));
    display.impl_trait = Some(plain("Display"));
    display.trait_impl = true;
    queue_summary.decls.push(display);

    let mut fmt = function(4, "fmt");
    fmt.parent_decl = Some(LocalDeclId(3));
    fmt.trait_impl = true;
    queue_summary.decls.push(fmt);

    let mut handle = function(5, "handle");
    handle.signature = Some(signature(
        &[(
            "q",
            Some(wrapping("Option", wrapping("Arc", plain("Queue")))),
        )],
        None,
    ));
    queue_summary.decls.push(handle);
    queue_summary.refs.push(reference(
        30,
        Some(5),
        member(ReceiverExpr::Local("q".to_owned()), "push", true),
        AccessMode::Work,
    ));

    let model = build_model(fixture.layout, vec![queue_summary], &[]);
    let queue = item_id(&model, "Queue");
    let block = model
        .items
        .iter()
        .position(|item| item.kind == ItemKind::Impl)
        .map(ItemId::new)
        .expect("the inherent impl block exists");
    assert_eq!(model.items[block.index()].parent, Some(queue));
    let push = item_id(&model, "Queue::push");
    assert_eq!(model.items[push.index()].parent, Some(queue));
    assert!(model.items[push.index()].takes_self);
    let fmt = item_id(&model, "<Queue as Display>::fmt");
    assert_eq!(model.items[fmt.index()].parent, Some(queue));
    assert!(model.items[fmt.index()].trait_impl);
    assert_eq!(
        model.references.len(),
        1,
        "the call on a parameter of a wrapped type resolves"
    );
    assert_eq!(model.references[0].item, push);
}

#[test]
fn local_types_follow_params_returns_calls_and_field_chains() {
    let mut fixture = Fixture::new();
    let queue_module = fixture.module("queue");
    let file = fixture.file("src/queue.rs", queue_module, Language::Rust);
    let mut queue_summary = summary(&file, 80);
    queue_summary.decls.push(type_decl(0, "Queue"));
    queue_summary.decls.push(type_decl(1, "Inner"));
    let mut inner_field = field_decl(2, "inner", plain("Inner"));
    inner_field.parent_decl = Some(LocalDeclId(0));
    queue_summary.decls.push(inner_field);
    let mut inner_items = field_decl(3, "items", wrapping("Vec", plain("u32")));
    inner_items.parent_decl = Some(LocalDeclId(1));
    queue_summary.decls.push(inner_items);
    let mut queue_items = field_decl(4, "q_items", wrapping("Vec", plain("u32")));
    queue_items.parent_decl = Some(LocalDeclId(0));
    queue_summary.decls.push(queue_items);

    let mut make = function(5, "make");
    make.signature = Some(signature(
        &[],
        Some(wrapping("Option", wrapping("Arc", plain("Queue")))),
    ));
    queue_summary.decls.push(make);

    let mut handle = function(6, "handle");
    handle.signature = Some(signature(
        &[(
            "q",
            Some(wrapping("Option", wrapping("Arc", plain("Queue")))),
        )],
        None,
    ));
    handle.body_summary = Some(FunctionBodySummary {
        locals: vec![
            LocalBinding {
                name: "made".to_owned(),
                declared_type: None,
                init: Some(InitExpr::Call(CalleeExpr::Path(vec!["make".to_owned()]))),
            },
            LocalBinding {
                name: "inner".to_owned(),
                declared_type: None,
                init: Some(InitExpr::FieldChain {
                    base: ReceiverExpr::Local("q".to_owned()),
                    fields: vec!["inner".to_owned()],
                }),
            },
        ],
        ..Default::default()
    });
    queue_summary.decls.push(handle);

    // `q.inner.items`
    queue_summary.refs.push(reference(
        40,
        Some(6),
        member(
            ReceiverExpr::FieldChain {
                base: Box::new(ReceiverExpr::Local("q".to_owned())),
                fields: vec!["inner".to_owned()],
            },
            "items",
            false,
        ),
        AccessMode::Work,
    ));
    // `q.q_items`, read only
    queue_summary.refs.push(reference(
        41,
        Some(6),
        member(ReceiverExpr::Local("q".to_owned()), "q_items", false),
        AccessMode::CopyOrRead,
    ));
    // `made.q_items`, where `made = make()` and `make` peels to `Queue`
    queue_summary.refs.push(reference(
        42,
        Some(6),
        member(ReceiverExpr::Local("made".to_owned()), "q_items", false),
        AccessMode::Work,
    ));
    // `inner.items`, where `inner` is a field-chain local of type `Inner`
    queue_summary.refs.push(reference(
        43,
        Some(6),
        member(ReceiverExpr::Local("inner".to_owned()), "items", false),
        AccessMode::Work,
    ));

    let model = build_model(fixture.layout, vec![queue_summary], &[]);
    let inner_items = child_id(&model, "Inner", "items");
    let queue_items = child_id(&model, "Queue", "q_items");
    assert_eq!(model.references.len(), 4);
    assert_eq!(model.references[0].item, inner_items);
    assert_eq!(model.references[1].item, queue_items);
    assert!(model.references[1].copies);
    assert_eq!(model.references[2].item, queue_items);
    assert_eq!(model.references[3].item, inner_items);
}

#[test]
fn namespace_imports_reach_the_exported_item() {
    let mut fixture = Fixture::new();
    let a = fixture.module("a");
    let c = fixture.module("c");
    let a_file = fixture.file("src/a.rs", a, Language::Rust);
    let c_file = fixture.file("src/c.rs", c, Language::Rust);
    let frontend = MockFrontend::new(Language::Rust);

    let mut a_summary = summary(&a_file, 5);
    a_summary.decls.push(type_decl(0, "Queue"));

    let mut c_summary = summary(&c_file, 5);
    c_summary.imports.push(import(
        &["crate", "a"],
        ImportKind::Namespace {
            alias: "ns".to_owned(),
        },
        false,
    ));
    c_summary.refs.push(reference(
        3,
        None,
        member(ReceiverExpr::Path(vec!["ns".to_owned()]), "Queue", false),
        AccessMode::Work,
    ));
    // An external name resolves nowhere and is skipped.
    c_summary
        .refs
        .push(reference(4, None, path_ref(&["String"]), AccessMode::Work));

    let model = build_model(
        fixture.layout,
        vec![a_summary, c_summary],
        &[&frontend as &dyn LanguageFrontend],
    );
    let queue = model
        .items
        .iter()
        .position(|item| item.name == "Queue" && item.kind == ItemKind::Type)
        .map(ItemId::new)
        .expect("the exported type exists");
    assert_eq!(model.references.len(), 1);
    assert_eq!(model.references[0].item, queue);
}

#[test]
fn forwarders_resolve_targets_fields_and_guard_cycles() {
    let mut fixture = Fixture::new();
    let module = fixture.module("flow");
    let file = fixture.file("src/flow.rs", module, Language::Rust);
    let mut flow_summary = summary(&file, 30);
    let mut alpha = function(0, "alpha");
    alpha.body_summary = Some(FunctionBodySummary {
        forwarding_candidate: Some(ForwardingCandidate {
            callee: CalleeExpr::Path(vec!["beta".to_owned()]),
            self_fields: Vec::new(),
        }),
        ..Default::default()
    });
    let mut beta = function(1, "beta");
    beta.body_summary = Some(FunctionBodySummary {
        forwarding_candidate: Some(ForwardingCandidate {
            callee: CalleeExpr::Path(vec!["alpha".to_owned()]),
            self_fields: Vec::new(),
        }),
        ..Default::default()
    });
    flow_summary.decls.extend([alpha, beta]);

    let model = build_model(fixture.layout, vec![flow_summary], &[]);
    let alpha = item_id(&model, "alpha");
    let beta = item_id(&model, "beta");
    assert_eq!(model.wrappers.len(), 1, "a cycle closes exactly once");
    assert_eq!(
        model.wrappers.get(&alpha),
        Some(&Wrapper::Forwarder {
            target: beta,
            fields: Vec::new(),
        })
    );
    assert!(!model.wrappers.contains_key(&beta));
}

#[test]
fn forwarders_record_the_fields_of_self_they_call_on() {
    let mut fixture = Fixture::new();
    let module = fixture.module("nested");
    let file = fixture.file("src/nested.rs", module, Language::Rust);
    let mut nested_summary = summary(&file, 40);
    nested_summary.decls.push(type_decl(0, "Inner"));
    let mut inner_impl = decl(1, "Inner", ItemKind::Impl);
    inner_impl.impl_target = Some(plain("Inner"));
    nested_summary.decls.push(inner_impl);
    let mut inner_push = function(2, "push");
    inner_push.parent_decl = Some(LocalDeclId(1));
    inner_push.takes_self = true;
    nested_summary.decls.push(inner_push);

    nested_summary.decls.push(type_decl(3, "Queue"));
    let mut inner_field = field_decl(4, "inner", plain("Inner"));
    inner_field.parent_decl = Some(LocalDeclId(3));
    nested_summary.decls.push(inner_field);
    let mut queue_impl = decl(5, "Queue", ItemKind::Impl);
    queue_impl.impl_target = Some(plain("Queue"));
    nested_summary.decls.push(queue_impl);
    let mut queue_push = function(6, "push");
    queue_push.parent_decl = Some(LocalDeclId(5));
    queue_push.takes_self = true;
    queue_push.body_summary = Some(FunctionBodySummary {
        forwarding_candidate: Some(ForwardingCandidate {
            callee: CalleeExpr::Method {
                receiver: ReceiverExpr::FieldChain {
                    base: Box::new(ReceiverExpr::SelfValue),
                    fields: vec!["inner".to_owned()],
                },
                method: "push".to_owned(),
            },
            self_fields: vec!["inner".to_owned()],
        }),
        ..Default::default()
    });
    nested_summary.decls.push(queue_push);

    let model = build_model(fixture.layout, vec![nested_summary], &[]);
    let queue_push = item_id(&model, "Queue::push");
    let inner_push = item_id(&model, "Inner::push");
    let inner_field = child_id(&model, "Queue", "inner");
    assert_eq!(
        model.wrappers.get(&queue_push),
        Some(&Wrapper::Forwarder {
            target: inner_push,
            fields: vec![inner_field],
        })
    );
}

#[test]
fn accessors_classify_lent_copied_and_read_only_fields() {
    let mut fixture = Fixture::new();
    let module = fixture.module("queue");
    let file = fixture.file("src/queue.rs", module, Language::Rust);
    let mut queue_summary = summary(&file, 80);
    queue_summary.decls.push(type_decl(0, "Queue"));

    let mut items_field = field_decl(1, "items", wrapping("Vec", plain("u32")));
    items_field.parent_decl = Some(LocalDeclId(0));
    queue_summary.decls.push(items_field);
    let mut count_field = field_decl(2, "count", plain("u32"));
    count_field.parent_decl = Some(LocalDeclId(0));
    queue_summary.decls.push(count_field);
    let mut label_field = field_decl(3, "label", plain("String"));
    label_field.parent_decl = Some(LocalDeclId(0));
    queue_summary.decls.push(label_field);
    let mut untyped_field = decl(4, "untyped", ItemKind::Field);
    untyped_field.parent_decl = Some(LocalDeclId(0));
    queue_summary.decls.push(untyped_field);

    let mut getter = function(5, "get_items");
    getter.parent_decl = Some(LocalDeclId(0));
    getter.takes_self = true;
    getter.self_is_shared_ref = true;
    getter.signature = Some(signature(&[], Some(wrapping("Vec", plain("u32")))));
    getter.body_summary = Some(accessor(vec![field_op("items", true, false)]));
    queue_summary.decls.push(getter);

    let mut setter = function(6, "set_count");
    setter.parent_decl = Some(LocalDeclId(0));
    setter.takes_self = true;
    setter.signature = Some(signature(&[("value", Some(plain("u32")))], None));
    setter.body_summary = Some(accessor(vec![field_op("count", true, true)]));
    queue_summary.decls.push(setter);

    let mut label = function(7, "label_copy");
    label.parent_decl = Some(LocalDeclId(0));
    label.takes_self = true;
    label.self_is_shared_ref = true;
    label.signature = Some(signature(&[], Some(plain("str"))));
    label.body_summary = Some(accessor(vec![field_op("label", false, false)]));
    queue_summary.decls.push(label);

    let mut untyped = function(8, "untyped_value");
    untyped.parent_decl = Some(LocalDeclId(0));
    untyped.takes_self = true;
    untyped.self_is_shared_ref = true;
    untyped.signature = Some(signature(&[], Some(plain("u32"))));
    untyped.body_summary = Some(accessor(vec![field_op("untyped", true, false)]));
    queue_summary.decls.push(untyped);

    let mut computing = function(9, "computing");
    computing.parent_decl = Some(LocalDeclId(0));
    computing.takes_self = true;
    computing.self_is_shared_ref = true;
    computing.signature = Some(signature(&[], Some(plain("u32"))));
    computing.body_summary = Some(FunctionBodySummary {
        computes: true,
        accessor_candidate: Some(AccessorCandidate {
            ops: vec![field_op("items", true, false)],
        }),
        ..Default::default()
    });
    queue_summary.decls.push(computing);

    let mut trait_method = function(10, "trait_method");
    trait_method.parent_decl = Some(LocalDeclId(0));
    trait_method.takes_self = true;
    trait_method.self_is_shared_ref = true;
    trait_method.trait_impl = true;
    trait_method.signature = Some(signature(&[], Some(plain("u32"))));
    trait_method.body_summary = Some(accessor(vec![field_op("count", true, false)]));
    queue_summary.decls.push(trait_method);

    let mut mismatched = function(11, "mismatched");
    mismatched.parent_decl = Some(LocalDeclId(0));
    mismatched.takes_self = true;
    mismatched.self_is_shared_ref = true;
    mismatched.signature = Some(signature(&[], Some(plain("bool"))));
    mismatched.body_summary = Some(accessor(vec![field_op("items", true, false)]));
    queue_summary.decls.push(mismatched);

    let mut outsider = function(12, "outsider");
    outsider.signature = Some(signature(&[("q", Some(plain("Queue")))], None));
    queue_summary.decls.push(outsider);
    queue_summary.refs.push(reference(
        60,
        Some(12),
        member(ReceiverExpr::Local("q".to_owned()), "get_items", true),
        AccessMode::CopyOrRead,
    ));
    queue_summary.refs.push(reference(
        61,
        Some(12),
        member(ReceiverExpr::Local("q".to_owned()), "get_items", true),
        AccessMode::Work,
    ));
    queue_summary.refs.push(reference(
        62,
        Some(12),
        member(ReceiverExpr::Local("q".to_owned()), "label", false),
        AccessMode::CopyOrRead,
    ));

    let model = build_model(fixture.layout, vec![queue_summary], &[]);
    let items = child_id(&model, "Queue", "items");
    let count = child_id(&model, "Queue", "count");
    let label = child_id(&model, "Queue", "label");
    let untyped_field = child_id(&model, "Queue", "untyped");
    let getter = item_id(&model, "get_items");
    let setter = item_id(&model, "set_count");
    let label_getter = item_id(&model, "label_copy");
    let untyped_getter = item_id(&model, "untyped_value");
    assert_eq!(
        model.wrappers.get(&getter),
        Some(&Wrapper::Accessor {
            lent: vec![items],
            copied: Vec::new(),
            to_read: true,
        })
    );
    assert_eq!(
        model.wrappers.get(&setter),
        Some(&Wrapper::Accessor {
            lent: vec![count],
            copied: Vec::new(),
            to_read: false,
        })
    );
    assert_eq!(
        model.wrappers.get(&label_getter),
        Some(&Wrapper::Accessor {
            lent: Vec::new(),
            copied: vec![label],
            to_read: true,
        })
    );
    assert_eq!(
        model.wrappers.get(&untyped_getter),
        Some(&Wrapper::Accessor {
            lent: vec![untyped_field],
            copied: Vec::new(),
            to_read: true,
        })
    );
    for rejected in ["computing", "trait_method", "mismatched"] {
        assert!(
            !model.wrappers.contains_key(&item_id(&model, rejected)),
            "`{rejected}` is no accessor"
        );
    }
    assert_eq!(model.references.len(), 3);
    assert!(
        model.references[0].copies,
        "a read call of a lending getter"
    );
    assert!(
        !model.references[1].copies,
        "a call that works on the field"
    );
    assert!(model.references[2].copies, "reading a plain field");
}

#[test]
fn shared_borrows_and_method_calls_copy_only_plain_fields() {
    let mut fixture = Fixture::new();
    let module = fixture.module("queue");
    let file = fixture.file("src/queue.rs", module, Language::Rust);
    let mut queue_summary = summary(&file, 60);
    queue_summary.decls.push(type_decl(0, "Queue"));
    queue_summary.decls.push(type_decl(1, "Inner"));
    for (local, name, ty) in [
        (2, "count", plain("u32")),
        (3, "flag", plain("bool")),
        (4, "name", plain("String")),
        (5, "items", wrapping("Vec", plain("u32"))),
        (6, "inner", plain("Inner")),
        (7, "maybe", wrapping("Option", plain("u32"))),
    ] {
        let mut field = field_decl(local, name, ty);
        field.parent_decl = Some(LocalDeclId(0));
        queue_summary.decls.push(field);
    }
    let mut untyped = decl(8, "untyped", ItemKind::Field);
    untyped.parent_decl = Some(LocalDeclId(0));
    queue_summary.decls.push(untyped);

    let mut outsider = function(9, "outsider");
    outsider.signature = Some(signature(&[("q", Some(plain("Queue")))], None));
    queue_summary.decls.push(outsider);

    let sites = [
        ("count", AccessMode::SharedBorrow, false),
        ("items", AccessMode::SharedBorrow, false),
        ("inner", AccessMode::SharedBorrow, false),
        ("name", AccessMode::SharedBorrow, false),
        ("maybe", AccessMode::SharedBorrow, false),
        ("untyped", AccessMode::SharedBorrow, false),
        ("count", AccessMode::MethodCall, true),
        ("name", AccessMode::MethodCall, true),
        ("items", AccessMode::MethodCall, true),
        ("flag", AccessMode::MethodCall, true),
        ("maybe", AccessMode::MethodCall, true),
    ];
    for (index, (name, access, is_call)) in sites.iter().enumerate() {
        queue_summary.refs.push(reference(
            30 + index as u32,
            Some(9),
            member(ReceiverExpr::Local("q".to_owned()), name, *is_call),
            *access,
        ));
    }

    let model = build_model(fixture.layout, vec![queue_summary], &[]);
    let copies: Vec<bool> = model
        .references
        .iter()
        .map(|reference| reference.copies)
        .collect();
    assert_eq!(
        copies,
        [
            true, false, false, true, true, false, true, false, false, true, true
        ]
    );
}

#[test]
fn accessor_lent_plain_fields_decide_shared_borrow_and_method_call_copies() {
    let mut fixture = Fixture::new();
    let module = fixture.module("queue");
    let file = fixture.file("src/queue.rs", module, Language::Rust);
    let mut queue_summary = summary(&file, 80);
    queue_summary.decls.push(type_decl(0, "Queue"));

    for (local, name, ty) in [
        (1, "count", plain("u32")),
        (2, "name", plain("String")),
        (3, "items", wrapping("Vec", plain("u32"))),
    ] {
        let mut field = field_decl(local, name, ty);
        field.parent_decl = Some(LocalDeclId(0));
        queue_summary.decls.push(field);
    }

    for (local, name, return_type, field) in [
        (4, "count_ref", plain("u32"), "count"),
        (5, "name_ref", plain("str"), "name"),
        (6, "items_ref", wrapping("Vec", plain("u32")), "items"),
    ] {
        let mut getter = function(local, name);
        getter.parent_decl = Some(LocalDeclId(0));
        getter.takes_self = true;
        getter.self_is_shared_ref = true;
        getter.signature = Some(signature(&[], Some(return_type)));
        getter.body_summary = Some(accessor(vec![field_op(field, true, false)]));
        queue_summary.decls.push(getter);
    }

    let mut outsider = function(7, "outsider");
    outsider.signature = Some(signature(&[("q", Some(plain("Queue")))], None));
    queue_summary.decls.push(outsider);

    let calls = [
        ("count_ref", AccessMode::SharedBorrow, true),
        ("count_ref", AccessMode::MethodCall, true),
        ("count_ref", AccessMode::Work, false),
        ("name_ref", AccessMode::SharedBorrow, true),
        ("name_ref", AccessMode::MethodCall, false),
        ("items_ref", AccessMode::SharedBorrow, false),
        ("items_ref", AccessMode::MethodCall, false),
        ("items_ref", AccessMode::CopyOrRead, true),
    ];
    for (index, (name, access, is_call)) in calls.iter().enumerate() {
        queue_summary.refs.push(reference(
            50 + index as u32,
            Some(7),
            member(ReceiverExpr::Local("q".to_owned()), name, *is_call),
            *access,
        ));
    }

    let model = build_model(fixture.layout, vec![queue_summary], &[]);
    for name in ["count_ref", "name_ref", "items_ref"] {
        assert!(
            matches!(
                model.wrappers.get(&item_id(&model, name)),
                Some(Wrapper::Accessor { to_read: true, .. })
            ),
            "`{name}` is a read-only accessor"
        );
    }
    let copies: Vec<bool> = model
        .references
        .iter()
        .map(|reference| reference.copies)
        .collect();
    assert_eq!(copies, [true, true, false, true, false, false, false, true]);
}
