//! Forwarder and accessor detection.
//!
//! A forwarder only calls one other workspace function, passing its parameters
//! on (`fn push(&mut self, x: T) { self.inner.push(x) }`). An accessor is an
//! inherent method that takes `self`, computes nothing, and only hands out or
//! overwrites fields of its own type (`fn items(&self) -> &[u32] { &self.items }`).
//!
//! Forwarder chains are kept acyclic: a function is only recorded as a
//! forwarder when following existing forwarders from its target cannot lead
//! back to it.

use crate::model::{ItemId, ItemKind, Wrapper};
use crate::treesitter::summary::{CalleeExpr, FileSummary, TypeExpr};

use super::Resolver;

/// What holds a value and is peeled off a field's declared type when checking
/// that an accessor's signature writes it (the reference implementation's
/// `HOLDERS`, but with one-argument holders only).
const HOLDERS: [&str; 8] = [
    "Option", "Cell", "RefCell", "Rc", "Arc", "Box", "Mutex", "RwLock",
];

impl Resolver {
    /// Pass 3c: classify wrapper functions, then relax `copies` on calls of
    /// accessors that only lend data to read.
    pub(super) fn detect_wrappers(&mut self, summaries: &[FileSummary]) {
        self.detect_forwarders(summaries);
        self.detect_accessors(summaries);
        self.propagate_accessor_reads();
    }

    fn detect_forwarders(&mut self, summaries: &[FileSummary]) {
        for (index, summary) in summaries.iter().enumerate() {
            for decl in &summary.decls {
                if decl.kind != ItemKind::Function {
                    continue;
                }
                let Some(item) = self.item_of(index, decl.local_id) else {
                    continue;
                };
                if self.items[item.index()].trait_impl {
                    continue;
                }
                let Some(body) = &decl.body_summary else {
                    continue;
                };
                let Some(candidate) = &body.forwarding_candidate else {
                    continue;
                };
                let module = self.scope_of(index, &decl.scope);
                let Some(target) =
                    self.forward_target(module, index, decl, item, &candidate.callee)
                else {
                    continue;
                };
                if target == item || self.items[target.index()].kind != ItemKind::Function {
                    continue;
                }
                let Some(fields) = self.forward_fields(item, &candidate.self_fields) else {
                    continue;
                };
                if self.forwarder_reaches(target, item) {
                    continue;
                }
                self.wrappers
                    .insert(item, Wrapper::Forwarder { target, fields });
            }
        }
    }

    fn forward_target(
        &self,
        module: crate::model::NodeId,
        index: usize,
        decl: &crate::treesitter::summary::ItemDecl,
        item: ItemId,
        callee: &CalleeExpr,
    ) -> Option<ItemId> {
        match callee {
            CalleeExpr::Path(segments) => self.resolve_path(module, segments),
            CalleeExpr::Method { receiver, method } => {
                let env = self.local_env(index, decl, item);
                let crate::treesitter::resolve::local_types::ReceiverTarget::Type(ty) =
                    self.eval_receiver(module, &env, receiver)?
                else {
                    return None;
                };
                self.type_methods
                    .get(&ty)
                    .and_then(|members| members.get(method))
                    .copied()
            }
        }
    }

    fn forward_fields(&self, item: ItemId, names: &[String]) -> Option<Vec<ItemId>> {
        if names.is_empty() {
            return Some(Vec::new());
        }
        let parent = self.items[item.index()].parent?;
        if self.items[parent.index()].kind != ItemKind::Type {
            return None;
        }
        let fields = self.type_fields.get(&parent)?;
        names.iter().map(|name| fields.get(name).copied()).collect()
    }

    /// Whether following forwarders from `from` reaches `to`.
    fn forwarder_reaches(&self, from: ItemId, to: ItemId) -> bool {
        let mut at = from;
        for _ in 0..1024 {
            if at == to {
                return true;
            }
            match self.wrappers.get(&at) {
                Some(Wrapper::Forwarder { target, .. }) => at = *target,
                _ => return false,
            }
        }
        false
    }

    fn detect_accessors(&mut self, summaries: &[FileSummary]) {
        for (index, summary) in summaries.iter().enumerate() {
            for decl in &summary.decls {
                if decl.kind != ItemKind::Function || !decl.takes_self {
                    continue;
                }
                let Some(item) = self.item_of(index, decl.local_id) else {
                    continue;
                };
                if self.items[item.index()].trait_impl || self.wrappers.contains_key(&item) {
                    continue;
                }
                let Some(body) = &decl.body_summary else {
                    continue;
                };
                if body.computes {
                    continue;
                }
                let Some(candidate) = &body.accessor_candidate else {
                    continue;
                };
                if candidate.ops.is_empty() {
                    continue;
                }
                let Some(parent) = self.items[item.index()].parent else {
                    continue;
                };
                if self.items[parent.index()].kind != ItemKind::Type {
                    continue;
                }
                let mut lent: Vec<ItemId> = Vec::new();
                let mut copied: Vec<ItemId> = Vec::new();
                let mut any_write = false;
                let mut valid = true;
                for op in &candidate.ops {
                    let Some(&field) = self
                        .type_fields
                        .get(&parent)
                        .and_then(|fields| fields.get(&op.field_name))
                    else {
                        valid = false;
                        break;
                    };
                    if !self.accessor_type_mentioned(decl, field) {
                        valid = false;
                        break;
                    }
                    any_write |= op.is_write;
                    let implicit_object_ref = summary.language.objects_passed_by_ref()
                        && self
                            .field_type_exprs
                            .get(&field)
                            .is_some_and(|ty| !super::local_types::is_plain_data(ty));
                    if op.is_lent_or_written || implicit_object_ref {
                        if !lent.contains(&field) {
                            lent.push(field);
                        }
                        if let Some(position) = copied.iter().position(|&other| other == field) {
                            copied.remove(position);
                        }
                    } else if !lent.contains(&field) && !copied.contains(&field) {
                        copied.push(field);
                    }
                }
                if !valid || (lent.is_empty() && copied.is_empty()) {
                    continue;
                }
                let to_read = decl.self_is_shared_ref
                    && !any_write
                    && (!summary.language.objects_passed_by_ref() || lent.is_empty());
                self.wrappers.insert(
                    item,
                    Wrapper::Accessor {
                        lent,
                        copied,
                        to_read,
                    },
                );
            }
        }
    }

    /// Whether the method's signature writes the field's declared type, after
    /// peeling holders and allowing what a getter may hand out instead: the
    /// type itself, `[T]` for `Vec<T>`, or what a `String`-like type hands out.
    /// A field with no type annotation always passes, as in an untyped file.
    fn accessor_type_mentioned(
        &self,
        decl: &crate::treesitter::summary::ItemDecl,
        field: ItemId,
    ) -> bool {
        let Some(ty) = self.field_type_exprs.get(&field) else {
            return true;
        };
        let Some(signature) = &decl.signature else {
            return false;
        };
        let written: Vec<&TypeExpr> = signature
            .params
            .iter()
            .filter_map(|param| param.ty.as_ref())
            .chain(signature.return_type.as_ref())
            .collect();
        let peeled = peel_accessor_holder(ty);
        written.iter().any(|candidate| mentions(candidate, peeled)) || hands_out(&written, peeled)
    }

    fn propagate_accessor_reads(&mut self) {
        for index in 0..self.references.len() {
            let mut item = self.references[index].item;
            for _ in 0..256 {
                match self.wrappers.get(&item) {
                    Some(Wrapper::Forwarder { target, .. }) => item = *target,
                    _ => break,
                }
            }
            let copies = match self.wrappers.get(&item) {
                Some(Wrapper::Accessor {
                    lent,
                    to_read: true,
                    ..
                }) if !lent.is_empty() => Some(self.fields_copy(lent, self.accesses[index])),
                _ => None,
            };
            if let Some(copies) = copies {
                self.references[index].copies = copies;
            }
        }
    }
}

/// Whether a written type is `target`, or holds it.
fn mentions(written: &TypeExpr, target: &TypeExpr) -> bool {
    if written.path == target.path
        && written.args.len() == target.args.len()
        && written
            .args
            .iter()
            .zip(&target.args)
            .all(|(written, target)| mentions(written, target))
    {
        return true;
    }
    written
        .args
        .iter()
        .any(|argument| mentions(argument, target))
}

/// Whether a getter may hand out the peeled field type in another form:
/// `[T]` for `Vec<T>`, and what a `String`-like type or an atomic hands out.
fn hands_out(written: &[&TypeExpr], peeled: &TypeExpr) -> bool {
    let handled = match peeled.path.last().map(String::as_str) {
        Some("Vec") if peeled.args.len() == 1 => Some(Handed::Array(&peeled.args[0])),
        Some(name) => HANDED_OUT
            .iter()
            .find(|(from, _)| *from == name)
            .map(|(_, to)| Handed::Name(to)),
        None => None,
    };
    let Some(handled) = handled else {
        return false;
    };
    written.iter().any(|candidate| match handled {
        Handed::Array(element) => mentions_array(candidate, element),
        Handed::Name(name) => mentions_name(candidate, name),
    })
}

enum Handed<'a> {
    Array(&'a TypeExpr),
    Name(&'a str),
}

/// Whether a written type is the array `element` writes, or holds it.
fn mentions_array(written: &TypeExpr, element: &TypeExpr) -> bool {
    (written.path.is_empty() && written.args.len() == 1 && mentions(&written.args[0], element))
        || written
            .args
            .iter()
            .any(|argument| mentions_array(argument, element))
}

/// Whether a written type names `name` somewhere.
fn mentions_name(written: &TypeExpr, name: &str) -> bool {
    written.path.last().is_some_and(|segment| segment == name)
        || written
            .args
            .iter()
            .any(|argument| mentions_name(argument, name))
}

/// Types and what a getter of one may hand out instead: what it dereferences
/// to, or the value an atomic holds.
const HANDED_OUT: [(&str, &str); 15] = [
    ("String", "str"),
    ("PathBuf", "Path"),
    ("OsString", "OsStr"),
    ("CString", "CStr"),
    ("AtomicBool", "bool"),
    ("AtomicI8", "i8"),
    ("AtomicI16", "i16"),
    ("AtomicI32", "i32"),
    ("AtomicI64", "i64"),
    ("AtomicIsize", "isize"),
    ("AtomicU8", "u8"),
    ("AtomicU16", "u16"),
    ("AtomicU32", "u32"),
    ("AtomicU64", "u64"),
    ("AtomicUsize", "usize"),
];

fn peel_accessor_holder(ty: &TypeExpr) -> &TypeExpr {
    let mut current = ty;
    loop {
        let Some(name) = current.path.last().map(String::as_str) else {
            return current;
        };
        if HOLDERS.contains(&name) && current.args.len() == 1 {
            current = &current.args[0];
        } else {
            return current;
        }
    }
}
