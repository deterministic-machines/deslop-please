//! Pure, per-file syntax extraction summary produced by Stage 2 (Tree-sitter).
//! Contains zero Tree-sitter lifetimes or cross-file pointers so Stage 2 runs
//! completely in parallel across files.

use crate::model::{ItemKind, NodeId};
use crate::treesitter::language::Language;

/// File-local identifier for a declaration inside a `FileSummary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalDeclId(pub u32);

/// File-local identifier for an inline module declared inside a file (e.g. Rust `mod inner { ... }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalModId(pub u32);

/// Which module scope inside a file an import/declaration/reference lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModuleScope {
    /// The file's own root module (`FileSummary::module_id`).
    FileRoot,
    /// An inline module declared within this file.
    Inline(LocalModId),
}

#[derive(Debug, Clone)]
pub struct InlineModuleDecl {
    pub local_id: LocalModId,
    pub parent: ModuleScope,
    pub name: String,
    pub is_test: bool,
}

#[derive(Debug, Clone)]
pub struct FileSummary {
    /// Workspace-relative `/`-separated path.
    pub file_path: String,
    /// Owning module in `WorkspaceLayout::tree`.
    pub module_id: NodeId,
    pub language: Language,
    pub is_test: bool,
    pub lines: usize,
    pub inline_modules: Vec<InlineModuleDecl>,
    pub imports: Vec<RawImport>,
    pub decls: Vec<ItemDecl>,
    pub refs: Vec<RawRef>,
}

#[derive(Debug, Clone)]
pub struct RawImport {
    pub scope: ModuleScope,
    /// Language-specific module path segments (e.g. Rust `["crate", "kinds", "Kind"]`).
    pub path_segments: Vec<String>,
    pub kind: ImportKind,
    /// True for `pub use ...`.
    pub is_reexport: bool,
}

#[derive(Debug, Clone)]
pub enum ImportKind {
    /// Named symbol import: `use a::b::Foo as Bar`.
    Named { name: String, alias: Option<String> },
    /// Namespace/module import: `use a::b as ns` or `use a::b` (module).
    Namespace { alias: String },
    /// Wildcard import: `use a::b::*`.
    Glob,
}

/// Simplified type expression extracted from signatures/annotations for local type propagation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeExpr {
    /// Primary path segments of the outermost type (e.g. `["Queue"]`, `["crate", "q", "Queue"]`, `["Option"]`).
    pub path: Vec<String>,
    /// Generic type arguments (e.g. for `Option<Arc<Queue>>`, args contain `Arc<Queue>`).
    pub args: Vec<TypeExpr>,
    /// True if written as a shared reference (`&T`) rather than `&mut T` or owned `T`.
    pub is_shared_ref: bool,
    /// True if written as a mutable reference (`&mut T`).
    pub is_mut_ref: bool,
}

impl TypeExpr {
    pub fn simple(name: impl Into<String>) -> Self {
        Self {
            path: vec![name.into()],
            args: Vec::new(),
            is_shared_ref: false,
            is_mut_ref: false,
        }
    }

    pub fn from_path(path: Vec<String>) -> Self {
        Self {
            path,
            args: Vec::new(),
            is_shared_ref: false,
            is_mut_ref: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParamDecl {
    pub name: String,
    pub ty: Option<TypeExpr>,
}

#[derive(Debug, Clone)]
pub struct FunctionSig {
    pub params: Vec<ParamDecl>,
    pub return_type: Option<TypeExpr>,
}

#[derive(Debug, Clone)]
pub struct ItemDecl {
    pub local_id: LocalDeclId,
    pub scope: ModuleScope,
    /// Bare name (e.g. `Queue`, `push`, `items`). For `ItemKind::Impl`, may be empty or target name.
    pub name: String,
    pub kind: ItemKind,
    /// Enclosing type/impl/variant declaration in the same file, if any.
    pub parent_decl: Option<LocalDeclId>,
    /// For `ItemKind::Impl`: the type being implemented (e.g. `["Queue"]` or `["crate", "q", "Queue"]`).
    pub impl_target: Option<TypeExpr>,
    /// For `ItemKind::Impl`: `Some(...)` if this is a trait implementation (`impl Trait for Type`).
    pub impl_trait: Option<TypeExpr>,
    pub is_exported: bool,
    pub is_default_export: bool,
    pub trait_impl: bool,
    pub takes_self: bool,
    /// True when the method's receiver is immutable (`&self`).
    pub self_is_shared_ref: bool,
    pub holds_nothing: bool,
    pub is_test: bool,
    /// Declared type for `Field` or `Constant`.
    pub type_annotation: Option<TypeExpr>,
    /// Signature for `Function`.
    pub signature: Option<FunctionSig>,
    /// Body analysis summary for `Function`.
    pub body_summary: Option<FunctionBodySummary>,
}

/// Receiver of a member access or method call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverExpr {
    /// `self`.
    SelfValue,
    /// Local variable or parameter name (`q`).
    Local(String),
    /// Path/static receiver (`Queue`, `crate::q::Queue`, `ns`).
    Path(Vec<String>),
    /// Chained field access (`self.ring`, `q.inner.buf`).
    FieldChain {
        base: Box<ReceiverExpr>,
        fields: Vec<String>,
    },
    /// Result of a call (`create_queue()`, `q.ring()`).
    CallResult { callee: Box<CalleeExpr> },
    /// Unanalyzable complex expression.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalleeExpr {
    Path(Vec<String>),
    Method {
        receiver: ReceiverExpr,
        method: String,
    },
}

#[derive(Debug, Clone)]
pub enum InitExpr {
    StructLiteral(Vec<String>),
    Call(CalleeExpr),
    FieldChain {
        base: ReceiverExpr,
        fields: Vec<String>,
    },
    Var(String),
}

#[derive(Debug, Clone)]
pub struct LocalBinding {
    pub name: String,
    pub declared_type: Option<TypeExpr>,
    pub init: Option<InitExpr>,
}

#[derive(Debug, Clone)]
pub struct ForwardingCandidate {
    pub callee: CalleeExpr,
    /// Field names traversed on `self`/`this` as receiver (e.g. `["inner"]` for `self.inner.push(x)`).
    pub self_fields: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AccessorFieldOp {
    pub field_name: String,
    /// True if written/mutably-lent/returned-by-reference; false if copied value.
    pub is_lent_or_written: bool,
    /// True if overwritten (`self.x = val`).
    pub is_write: bool,
}

#[derive(Debug, Clone)]
pub struct AccessorCandidate {
    pub ops: Vec<AccessorFieldOp>,
    /// True if the method takes `self` by value (giving or taking fields whole).
    pub takes_self_by_value: bool,
    /// True if the body overwrites through interior mutability, calls a
    /// parameter, or returns a mutable borrow, guard, or raw pointer.
    pub lends_to_write: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FunctionBodySummary {
    pub locals: Vec<LocalBinding>,
    /// True if the body performs computation (operators, control flow, macros, non-pass calls).
    pub computes: bool,
    pub forwarding_candidate: Option<ForwardingCandidate>,
    pub accessor_candidate: Option<AccessorCandidate>,
}

/// Pure syntactic access context at a member/field reference site.
/// Frontends emit only syntax; `resolve` combines this with field types to compute `Reference::copies`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Plain value read / copy (`let x = q.count`, `q.x + 1`).
    CopyOrRead,
    /// Shared read-only borrow (`&q.name`): `copies = true` for plain data types, `false` for structs/collections.
    SharedBorrow,
    /// Non-mutating method call on the field (`q.x.foo()`): `copies = true` only for primitive/plain values.
    MethodCall,
    /// Write, `&mut` borrow, indexing, mutating call, or reaching into the field (`copies = false`).
    Work,
}

#[derive(Debug, Clone)]
pub enum RawRefKind {
    /// Path or identifier reference: `Queue`, `Queue::new`, `crate::a::foo`, `helper`.
    Path(Vec<String>),
    /// Member/field/method access: `q.items`, `q.push(...)`, `self.inner`.
    MemberAccess {
        receiver: ReceiverExpr,
        member: String,
        is_call: bool,
    },
}

#[derive(Debug, Clone)]
pub struct RawRef {
    pub line: u32,
    pub scope: ModuleScope,
    pub holder: Option<LocalDeclId>,
    pub kind: RawRefKind,
    pub access: AccessMode,
    pub is_test: bool,
}
