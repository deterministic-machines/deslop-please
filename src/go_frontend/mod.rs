//! Go modules, indexed by scip-go and supplemented with Go syntax facts.
mod index;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Error;
use crate::model::{
    File, FileId, Item, ItemId, ItemKind, Model, NodeId, NodeKind, Reference, Wrapper,
};
use crate::scip::{self, Position, Range};
use serde_json::Value;

static TEMP_ID: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn facts(root: &Path, paths: &[PathBuf]) -> Result<Vec<Value>, Error> {
    let dir = std::env::temp_dir().join(format!(
        "deslop-go-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).map_err(|e| Error::new(format!("{}: {e}", dir.display())))?;
    let dir = Temp(dir);
    let helper = dir.0.join("facts.go");
    std::fs::write(&helper, include_str!("facts.go"))
        .map_err(|e| Error::new(format!("{}: {e}", helper.display())))?;
    let mut command = Command::new("go");
    command
        .args(["run"])
        .arg(&helper)
        .env("GO111MODULE", "off")
        .env_remove("GOOS")
        .env_remove("GOARCH")
        .env_remove("GOFLAGS")
        .env(
            "GOWORK",
            if root.join("go.work").is_file() {
                root.join("go.work")
            } else {
                PathBuf::from("off")
            },
        );
    for key in ["GOOS", "GOARCH", "GOFLAGS", "GO111MODULE"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(format!("DESLOP_TARGET_{key}"), value);
        } else {
            command.env_remove(format!("DESLOP_TARGET_{key}"));
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::new(format!("cannot run Go syntax analyzer: {e}")))?;
    let input = serde_json::to_vec(paths).map_err(|e| Error::new(e.to_string()))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&input)
        .map_err(|e| Error::new(format!("cannot send paths to Go syntax analyzer: {e}")))?;
    let output = child
        .wait_with_output()
        .map_err(|e| Error::new(format!("Go syntax analyzer: {e}")))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "Go syntax analyzer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::new(format!("invalid Go syntax facts: {e}")))
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn pos(v: &Value) -> Position {
    Position {
        line: v["line"].as_u64().unwrap_or(0) as u32,
        column: v["col"].as_u64().unwrap_or(0) as u32,
    }
}
fn range(v: &Value) -> Range {
    Range {
        start: pos(&v["start"]),
        end: pos(&v["end"]),
    }
}

/// Analyze the active Go build configuration rooted at a go.mod or go.work.
pub fn build(root: &Path) -> Result<Model, Error> {
    let root = root
        .canonicalize()
        .map_err(|e| Error::new(format!("{}: {e}", root.display())))?;
    let indexes = index::index(&root)?;
    let parsed: Vec<_> = indexes
        .iter()
        .map(|i| {
            scip::read(&i.bytes).map_err(|e| {
                Error::new(format!(
                    "invalid scip-go index for {}: {e}",
                    i.root.display()
                ))
            })
        })
        .collect::<Result<_, _>>()?;
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for (module, idx) in indexes.iter().zip(&parsed) {
        for doc in &idx.documents {
            let p = module.root.join(doc.path.as_ref());
            if !p.starts_with(&module.root)
                || Path::new(doc.path.as_ref()).components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(Error::new(format!(
                    "invalid SCIP document path {}",
                    doc.path
                )));
            }
            if p.extension().is_some_and(|e| e == "go") && seen.insert(p.clone()) {
                paths.push(p);
            }
        }
    }
    if paths.is_empty() {
        return Err(Error::new("scip-go indexed no Go files"));
    }
    let syntax = facts(&root, &paths)?;
    let syntax: HashMap<PathBuf, Value> = syntax
        .into_iter()
        .map(|v| (PathBuf::from(text(&v, "path")), v))
        .collect();
    let mut model = Model::default();
    let mut files = HashMap::<PathBuf, FileId>::new();
    let mut package_nodes = HashMap::<PathBuf, NodeId>::new();
    let mut document_ids = HashMap::<PathBuf, usize>::new();
    for (module, idx) in indexes.iter().zip(&parsed) {
        for (doc_number, doc) in idx.documents.iter().enumerate() {
            let path = module.root.join(doc.path.as_ref());
            let Some(fact) = syntax.get(&path) else {
                continue;
            };
            if files.contains_key(&path) {
                continue;
            }
            let directory = path
                .parent()
                .ok_or_else(|| Error::new("Go document has no parent directory"))?;
            let package_key = directory.join(text(fact, "package"));
            let owner = *package_nodes.entry(package_key).or_insert_with(|| {
                let relative = directory.strip_prefix(&root).unwrap_or(directory);
                let name = if relative.as_os_str().is_empty() {
                    text(fact, "package").to_owned()
                } else {
                    relative
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/")
                };
                model
                    .tree
                    .child(model.tree.root(), NodeKind::Package, &name)
            });
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| Error::new(format!("{} outside workspace", path.display())))?;
            let file = FileId::new(model.files.len());
            model.files.push(File {
                path: relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
                module: owner,
                test: path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with("_test.go")),
                lines: fact["lines"].as_u64().unwrap_or(0) as usize,
            });
            files.insert(path.clone(), file);
            document_ids.insert(path, doc_number);
        }
    }
    let mut symbols = HashMap::<String, ItemId>::new();
    let mut parents = Vec::<(ItemId, NodeId, String)>::new();
    let mut declarations = HashMap::<PathBuf, Vec<(Range, ItemId)>>::new();
    let mut type_names = HashMap::<(NodeId, String), ItemId>::new();
    let mut forwarders = Vec::<(ItemId, String)>::new();
    let mut setters = Vec::<(ItemId, String)>::new();
    let mut getters = Vec::<(ItemId, String, bool)>::new();
    for (module, idx) in indexes.iter().zip(&parsed) {
        for (doc_number, doc) in idx.documents.iter().enumerate() {
            let path = module.root.join(doc.path.as_ref());
            if document_ids.get(&path) != Some(&doc_number) {
                continue;
            }
            let (Some(fact), Some(&file)) = (syntax.get(&path), files.get(&path)) else {
                continue;
            };
            let owner = model.files[file.index()].module;
            for decl in fact["decls"].as_array().into_iter().flatten() {
                let name_pos = pos(&decl["name_pos"]);
                let Some(occ) = doc.occurrences.iter().find(|o| {
                    o.definition && o.range.start == name_pos && !o.symbol.starts_with("local ")
                }) else {
                    continue;
                };
                if symbols.contains_key(occ.symbol) {
                    continue;
                }
                let kind = match text(decl, "kind") {
                    "type" => ItemKind::Type,
                    "function" => ItemKind::Function,
                    "field" => ItemKind::Field,
                    "constant" => ItemKind::Constant,
                    _ => continue,
                };
                let parent = text(decl, "parent");
                let name = if parent.is_empty() {
                    text(decl, "name").to_owned()
                } else {
                    format!("{}::{}", parent, text(decl, "name"))
                };
                let id = ItemId::new(model.items.len());
                model.items.push(Item {
                    name,
                    owner,
                    module: None,
                    file: Some(file),
                    kind,
                    parent: None,
                    trait_impl: false,
                    takes_self: decl["receiver"].as_bool().unwrap_or(false),
                    holds_nothing: false,
                });
                symbols.insert(occ.symbol.to_owned(), id);
                if decl.get("forward").is_some() {
                    let target_pos = pos(&decl["forward"]);
                    if let Some(target) = doc
                        .occurrences
                        .iter()
                        .find(|o| !o.definition && o.range.start == target_pos)
                    {
                        forwarders.push((id, target.symbol.to_owned()));
                    }
                }
                if decl.get("setter").is_some() {
                    let field_pos = pos(&decl["setter"]);
                    if let Some(field) = doc
                        .occurrences
                        .iter()
                        .find(|o| !o.definition && o.range.start == field_pos)
                    {
                        setters.push((id, field.symbol.to_owned()));
                    }
                }
                if decl.get("getter").is_some() {
                    let field_pos = pos(&decl["getter"]);
                    let field = doc
                        .occurrences
                        .iter()
                        .find(|o| !o.definition && o.range.start == field_pos)
                        .ok_or_else(|| {
                            Error::new(format!(
                                "cannot resolve Go getter field at {}:{}",
                                path.display(),
                                field_pos.line + 1
                            ))
                        })?;
                    getters.push((
                        id,
                        field.symbol.to_owned(),
                        decl["getter_lent"].as_bool().unwrap_or(true),
                    ));
                }
                declarations
                    .entry(path.clone())
                    .or_default()
                    .push((range(decl), id));
                if kind == ItemKind::Type {
                    type_names.insert((owner, text(decl, "name").to_owned()), id);
                }
                if !parent.is_empty() {
                    parents.push((id, owner, parent.to_owned()));
                }
            }
        }
    }
    for (item, owner, parent) in parents {
        let parent = type_names
            .get(&(owner, parent.clone()))
            .copied()
            .ok_or_else(|| {
                Error::new(format!(
                    "cannot resolve Go receiver or field owner {parent}"
                ))
            })?;
        model.items[item.index()].parent = Some(parent);
    }
    for (item, symbol) in forwarders {
        if let Some(&target) = symbols.get(&symbol)
            && item != target
        {
            model.wrappers.insert(
                item,
                Wrapper::Forwarder {
                    target,
                    fields: Vec::new(),
                },
            );
        }
    }
    for (item, symbol) in setters {
        if let Some(&field) = symbols.get(&symbol)
            && model.items[field.index()].kind == ItemKind::Field
        {
            model.wrappers.insert(
                item,
                Wrapper::Accessor {
                    lent: vec![field],
                    copied: Vec::new(),
                    to_read: false,
                },
            );
        }
    }
    let mut lent_getters = HashSet::new();
    for (item, symbol, lent) in getters {
        let &field = symbols
            .get(&symbol)
            .ok_or_else(|| Error::new(format!("cannot resolve Go getter field {symbol}")))?;
        if model.items[field.index()].kind != ItemKind::Field {
            return Err(Error::new(format!(
                "Go getter target is not a field: {symbol}"
            )));
        }
        let (lent, copied) = if lent {
            lent_getters.insert(item);
            (vec![field], Vec::new())
        } else {
            (Vec::new(), vec![field])
        };
        model.wrappers.insert(
            item,
            Wrapper::Accessor {
                lent,
                copied,
                to_read: false,
            },
        );
    }
    for &start in model.wrappers.keys() {
        let mut visited = HashSet::new();
        let mut current = start;
        while let Some(Wrapper::Forwarder { target, .. }) = model.wrappers.get(&current) {
            if !visited.insert(current) {
                return Err(Error::new("cyclic Go forwarders cannot be represented"));
            }
            current = *target;
        }
    }
    for &start in model.wrappers.keys() {
        let mut current = start;
        while let Some(Wrapper::Forwarder { target, .. }) = model.wrappers.get(&current) {
            current = *target;
        }
        if lent_getters.contains(&current) {
            lent_getters.insert(start);
        }
    }
    for (module, idx) in indexes.iter().zip(&parsed) {
        for (doc_number, doc) in idx.documents.iter().enumerate() {
            let path = module.root.join(doc.path.as_ref());
            if document_ids.get(&path) != Some(&doc_number) {
                continue;
            }
            let (Some(fact), Some(&file)) = (syntax.get(&path), files.get(&path)) else {
                continue;
            };
            let owner = model.files[file.index()].module;
            for occ in &doc.occurrences {
                if occ.definition {
                    continue;
                }
                let Some(&item) = symbols.get(occ.symbol) else {
                    continue;
                };
                let holder = declarations
                    .get(&path)
                    .into_iter()
                    .flatten()
                    .filter(|(r, _)| r.holds(occ.range))
                    .min_by_key(|(r, _)| {
                        (
                            r.end.line - r.start.line,
                            r.end.column.saturating_sub(r.start.column),
                        )
                    })
                    .map(|(_, id)| *id);
                let copies = if holder.is_some_and(|id| model.wrappers.contains_key(&id)) {
                    false
                } else if model.items[item.index()].kind == ItemKind::Field
                    || lent_getters.contains(&item)
                {
                    match fact["uses"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|v| pos(&v["pos"]) == occ.range.start)
                        .map(|v| text(v, "mode"))
                    {
                        Some("read") => true,
                        Some("mutate") => false,
                        _ => {
                            return Err(Error::new(format!(
                                "cannot determine field access semantics at {}:{}",
                                path.display(),
                                occ.range.start.line + 1
                            )));
                        }
                    }
                } else {
                    false
                };
                model.references.push(Reference {
                    file,
                    line: occ.range.start.line + 1,
                    from: owner,
                    holder,
                    item,
                    copies,
                    test: model.files[file.index()].test,
                });
            }
        }
    }
    Ok(model)
}
