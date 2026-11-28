//! Reference resolution and local type propagation.
//!
//! Every function body gets a `LocalTypeEnv` from its receiver, parameters,
//! declared locals, struct literals, call results and field chains. That env
//! is what turns `q.push(x)` into a reference to `Queue::push` rather than
//! just `push`, and what lets chained field access reach the field of a
//! field's type.

use std::collections::HashMap;

use crate::model::{FileId, ItemId, ItemKind, NodeId, Reference};
use crate::treesitter::summary::{
    AccessMode, CalleeExpr, FileSummary, InitExpr, ItemDecl, RawRef, RawRefKind, ReceiverExpr,
    TypeExpr,
};

use super::Resolver;
use super::symbols::Target;

/// Local variable or parameter name to the workspace type it holds.
type TypeEnv = HashMap<String, ItemId>;

/// What an expression used as a receiver evaluates to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReceiverTarget {
    Type(ItemId),
    Module(NodeId),
}

impl Resolver {
    /// Pass 3b: resolve every `RawRef` to a workspace item, using the local
    /// types of the function that holds it.
    pub(super) fn resolve_references(&mut self, summaries: &[FileSummary]) {
        for (index, summary) in summaries.iter().enumerate() {
            let by_item: HashMap<ItemId, &ItemDecl> = summary
                .decls
                .iter()
                .filter_map(|decl| {
                    self.files[index]
                        .decls
                        .get(&decl.local_id)
                        .map(|&item| (item, decl))
                })
                .collect();
            let mut envs: HashMap<ItemId, TypeEnv> = HashMap::new();
            for raw in &summary.refs {
                let from = self.scope_of(index, &raw.scope);
                let holder = raw
                    .holder
                    .and_then(|local| self.files[index].decls.get(&local).copied());
                let env = match holder {
                    Some(holder) => envs
                        .entry(holder)
                        .or_insert_with(|| match by_item.get(&holder) {
                            Some(decl) => self.local_env(index, decl, holder),
                            None => TypeEnv::new(),
                        })
                        .clone(),
                    None => TypeEnv::new(),
                };
                let Some((item, copies)) = self.resolve_raw_ref(index, raw, &env) else {
                    continue;
                };
                let test = raw.is_test
                    || self.files[index].entry.test
                    || self.tree.is_test(from)
                    || holder.is_some_and(|holder| self.test_items.contains(&holder));
                self.references.push(Reference {
                    file: FileId::new(index),
                    line: raw.line,
                    from,
                    holder,
                    item,
                    copies,
                    test,
                });
                self.accesses.push(raw.access);
            }
        }
    }

    fn resolve_raw_ref(&self, index: usize, raw: &RawRef, env: &TypeEnv) -> Option<(ItemId, bool)> {
        let module = self.scope_of(index, &raw.scope);
        match &raw.kind {
            RawRefKind::Path(segments) => {
                let first = segments.first()?;
                if segments.len() == 1 {
                    if first == "self" || first == "this" || env.contains_key(first) {
                        return None;
                    }
                    if first == "Self" {
                        return env.get("self").copied().map(|item| (item, false));
                    }
                }
                let item = self.resolve_path(module, segments)?;
                Some((item, self.field_copies(item, raw.access)))
            }
            RawRefKind::MemberAccess {
                receiver,
                member,
                is_call,
            } => {
                let receiver = self.eval_receiver(module, env, receiver)?;
                let item = match receiver {
                    ReceiverTarget::Module(node) => self.module_member(node, member)?,
                    ReceiverTarget::Type(ty) => {
                        if *is_call {
                            self.member_of_type(ty, member, true)?
                        } else {
                            self.member_of_type(ty, member, false)?
                        }
                    }
                };
                Some((item, self.field_copies(item, raw.access)))
            }
        }
    }

    /// Whether this reference only copies what it names. The reference site's
    /// syntax says how the field is used; the field's declared type says
    /// whether that use can reach past its value.
    fn field_copies(&self, field: ItemId, access: AccessMode) -> bool {
        if self.items[field.index()].kind != ItemKind::Field {
            return false;
        }
        match access {
            AccessMode::CopyOrRead => true,
            AccessMode::Work => false,
            AccessMode::SharedBorrow => {
                self.field_type_exprs.get(&field).is_some_and(is_plain_data)
            }
            AccessMode::MethodCall => self.field_type_exprs.get(&field).is_some_and(is_scalar),
        }
    }

    /// Whether code at an `access` site only reads every one of `fields`:
    /// what an accessor hands out when the caller only reads the call's value.
    pub(super) fn fields_copy(&self, fields: &[ItemId], access: AccessMode) -> bool {
        match access {
            AccessMode::CopyOrRead => true,
            AccessMode::Work => false,
            AccessMode::SharedBorrow => fields
                .iter()
                .all(|&field| self.field_type_exprs.get(&field).is_some_and(is_plain_data)),
            AccessMode::MethodCall => fields
                .iter()
                .all(|&field| self.field_type_exprs.get(&field).is_some_and(is_scalar)),
        }
    }

    /// A member named through a namespace import or a module path.
    fn module_member(&self, module: NodeId, member: &str) -> Option<ItemId> {
        if let Some(&item) = self
            .module_exports
            .get(&module)
            .and_then(|exports| exports.get(member))
        {
            return Some(item);
        }
        if member == "default"
            && let Some(&item) = self.module_default.get(&module)
        {
            return Some(item);
        }
        match self.lookup_direct(module, member)? {
            Target::Item(item) => Some(item),
            Target::Module(node) => self.module_items.get(&node).copied(),
        }
    }

    fn member_of_type(&self, ty: ItemId, member: &str, prefer_method: bool) -> Option<ItemId> {
        let methods = || {
            self.type_methods
                .get(&ty)
                .and_then(|members| members.get(member))
                .copied()
        };
        let fields = || {
            self.type_fields
                .get(&ty)
                .and_then(|members| members.get(member))
                .copied()
        };
        let variants = || {
            self.type_variants
                .get(&ty)
                .and_then(|members| members.get(member))
                .copied()
        };
        if prefer_method {
            methods().or_else(fields).or_else(variants)
        } else {
            fields().or_else(variants).or_else(methods)
        }
    }

    /// The types a function body can name: `self`, its parameters, and each
    /// local in source order.
    pub(super) fn local_env(&self, index: usize, decl: &ItemDecl, item: ItemId) -> TypeEnv {
        let module = self.scope_of(index, &decl.scope);
        let mut env = TypeEnv::new();
        if let Some(parent) = self.items[item.index()]
            .parent
            .filter(|parent| self.items[parent.index()].kind == ItemKind::Type)
        {
            for name in ["self", "this", "Self"] {
                env.insert(name.to_owned(), parent);
            }
        }
        if let Some(signature) = &decl.signature {
            for param in &signature.params {
                let Some(ty) = &param.ty else {
                    continue;
                };
                if let Some(resolved) = self.resolve_type_expr(module, ty) {
                    env.insert(param.name.clone(), resolved);
                }
            }
        }
        if let Some(body) = &decl.body_summary {
            for local in &body.locals {
                let resolved = match (&local.declared_type, &local.init) {
                    (Some(ty), _) => self.resolve_type_expr(module, ty),
                    (None, Some(init)) => self.init_type(module, &env, init),
                    (None, None) => None,
                };
                if let Some(resolved) = resolved {
                    env.insert(local.name.clone(), resolved);
                }
            }
        }
        env
    }

    fn init_type(&self, module: NodeId, env: &TypeEnv, init: &InitExpr) -> Option<ItemId> {
        match init {
            InitExpr::StructLiteral(path) => self.type_from_path(module, env, path),
            InitExpr::Call(callee) => self.callee_return(module, env, callee),
            InitExpr::FieldChain { base, fields } => {
                let ReceiverTarget::Type(mut ty) = self.eval_receiver(module, env, base)? else {
                    return None;
                };
                for field in fields {
                    let field = self.type_fields.get(&ty)?.get(field)?;
                    ty = *self.field_types.get(field)?;
                }
                Some(ty)
            }
            InitExpr::Var(name) => env.get(name).copied(),
        }
    }

    fn type_from_path(&self, module: NodeId, env: &TypeEnv, path: &[String]) -> Option<ItemId> {
        if let [name] = path {
            if let Some(&ty) = env.get(name) {
                return Some(ty);
            }
            if name == "Self" {
                return env.get("self").copied();
            }
        }
        self.resolve_type_expr(module, &TypeExpr::from_path(path.to_vec()))
    }

    fn callee_return(&self, module: NodeId, env: &TypeEnv, callee: &CalleeExpr) -> Option<ItemId> {
        match callee {
            CalleeExpr::Path(segments) => {
                let item = self.resolve_path(module, segments)?;
                match self.items[item.index()].kind {
                    ItemKind::Function => self.return_types.get(&item).copied(),
                    // A Rust tuple-struct call, `Wrap(x)`.
                    ItemKind::Type => Some(item),
                    _ => None,
                }
            }
            CalleeExpr::Method { receiver, method } => {
                match self.eval_receiver(module, env, receiver)? {
                    ReceiverTarget::Type(ty) => {
                        let function = self.type_methods.get(&ty)?.get(method)?;
                        self.return_types.get(function).copied()
                    }
                    ReceiverTarget::Module(target) => {
                        let item = self.module_member(target, method)?;
                        match self.items[item.index()].kind {
                            ItemKind::Function => self.return_types.get(&item).copied(),
                            ItemKind::Type => Some(item),
                            _ => None,
                        }
                    }
                }
            }
        }
    }

    /// What `expression` evaluates to, if it names a workspace type or module.
    pub(super) fn eval_receiver(
        &self,
        module: NodeId,
        env: &TypeEnv,
        expression: &ReceiverExpr,
    ) -> Option<ReceiverTarget> {
        match expression {
            ReceiverExpr::SelfValue => env.get("self").copied().map(ReceiverTarget::Type),
            ReceiverExpr::Local(name) => env.get(name).copied().map(ReceiverTarget::Type),
            ReceiverExpr::Path(segments) => {
                if let [name] = segments.as_slice() {
                    if name == "self" || name == "this" || name == "Self" {
                        return env.get("self").copied().map(ReceiverTarget::Type);
                    }
                    if let Some(&ty) = env.get(name) {
                        return Some(ReceiverTarget::Type(ty));
                    }
                }
                let item = self.resolve_path(module, segments)?;
                match self.items[item.index()].kind {
                    ItemKind::Type => Some(ReceiverTarget::Type(item)),
                    ItemKind::Module => self.items[item.index()].module.map(ReceiverTarget::Module),
                    _ => None,
                }
            }
            ReceiverExpr::FieldChain { base, fields } => {
                let ReceiverTarget::Type(mut ty) = self.eval_receiver(module, env, base)? else {
                    return None;
                };
                for field in fields {
                    let field = self.type_fields.get(&ty)?.get(field)?;
                    ty = *self.field_types.get(field)?;
                }
                Some(ReceiverTarget::Type(ty))
            }
            ReceiverExpr::CallResult { callee } => self
                .callee_return(module, env, callee)
                .map(ReceiverTarget::Type),
            ReceiverExpr::Unknown => None,
        }
    }
}

/// Types whose values are plain data: a borrow of one reads a value, no matter
/// how the owning struct keeps it.
const PLAIN_DATA: [&str; 59] = [
    "bool",
    "char",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "isize",
    "usize",
    "f32",
    "f64",
    "str",
    "String",
    "Path",
    "PathBuf",
    "OsStr",
    "OsString",
    "CStr",
    "CString",
    "Ipv4Addr",
    "Ipv6Addr",
    "IpAddr",
    "SocketAddr",
    "Duration",
    "SystemTime",
    "Instant",
    "number",
    "string",
    "boolean",
    "bigint",
    "symbol",
    "Date",
    "RegExp",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "float",
    "float32",
    "float64",
    "complex64",
    "complex128",
    "byte",
    "rune",
    "bytes",
    "None",
    "Decimal",
];

/// Scalar primitives: a non-mutating method call on one cannot reach into more
/// than its own value.
const SCALARS: [&str; 41] = [
    "bool",
    "char",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "isize",
    "usize",
    "f32",
    "f64",
    "number",
    "string",
    "boolean",
    "bigint",
    "symbol",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "float",
    "float32",
    "float64",
    "complex64",
    "complex128",
    "byte",
    "rune",
    "str",
    "bytes",
];

/// The type a value is, past references (which `TypeExpr` already records) and
/// the `Option`/`Box` wrappers that only hold it.
fn value_type(ty: &TypeExpr) -> &TypeExpr {
    let mut current = ty;
    loop {
        let Some(name) = current.path.last().map(String::as_str) else {
            return current;
        };
        if !matches!(name, "Option" | "Optional" | "Box") || current.args.len() != 1 {
            return current;
        }
        current = &current.args[0];
    }
}

pub(super) fn is_plain_data(ty: &TypeExpr) -> bool {
    value_type(ty)
        .path
        .last()
        .is_some_and(|name| PLAIN_DATA.contains(&name.as_str()))
}

pub(super) fn is_scalar(ty: &TypeExpr) -> bool {
    value_type(ty)
        .path
        .last()
        .is_some_and(|name| SCALARS.contains(&name.as_str()))
}
