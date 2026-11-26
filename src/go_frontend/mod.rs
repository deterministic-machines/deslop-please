//! Go modules and workspaces, indexed by scip-go and supplemented with facts
//! from the type-checked source (`facts.go`).
//!
//! Each Go package is a package of the model, named by its import path; an
//! external test package is one of its own, `path_test`. Types, functions,
//! methods, struct fields, interface methods, and package-level constants and
//! variables are its items. A method belongs to the type it is declared on,
//! through aliases. The index gives the references by name; the facts add
//! what code does with each field and with what each call returns, the
//! accessors and forwarders, and the references the source does not spell
//! out: the embedded fields a promoted field is reached through, the fields a
//! positional struct literal sets, and the methods a call of an interface's
//! method may run, by the concrete types the workspace converts to
//! interfaces.
mod index;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Error;
use crate::model::{File, FileId, Item, ItemId, ItemKind, Model, NodeKind, Reference, Wrapper};
use crate::scip::{self, Position, Range};
use serde_json::{Value, json};

/// A Go module, or a workspace of them, as the go command selects it.
pub struct Workspace {
    /// The workspace file's directory, or the module's; paths are relative
    /// to it.
    root: PathBuf,
    /// The workspace file, unless `GOWORK` is `off` or there is none.
    work: Option<PathBuf>,
}

/// The Go workspace `cwd` is in, if the nearest directory above it with a
/// `go.mod`, `go.work` or `Cargo.toml` has one of Go's. The go command
/// chooses the workspace: a `go.work` above a nested module, or `GOWORK`.
pub fn discover(cwd: &Path) -> Result<Option<Workspace>, Error> {
    let go = |dir: &Path| dir.join("go.mod").is_file() || dir.join("go.work").is_file();
    let manifest = cwd
        .ancestors()
        .find(|dir| go(dir) || dir.join("Cargo.toml").is_file());
    if !manifest.is_some_and(go) {
        return Ok(None);
    }
    let output = Command::new("go")
        .args(["env", "GOWORK"])
        .current_dir(cwd)
        .output()
        .map_err(|e| Error::new(format!("cannot run go env: {e}")))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "go env GOWORK failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let work = String::from_utf8(output.stdout)
        .map_err(|e| Error::new(format!("go env GOWORK is not UTF-8: {e}")))?;
    let canonical = |path: &Path| {
        path.canonicalize()
            .map_err(|e| Error::new(format!("{}: {e}", path.display())))
    };
    let work = work.trim();
    let workspace = if work.is_empty() || work == "off" {
        let module = cwd
            .ancestors()
            .find(|dir| dir.join("go.mod").is_file())
            .ok_or_else(|| Error::new("no go.mod here or above, and no Go workspace in use"))?;
        Workspace {
            root: canonical(module)?,
            work: None,
        }
    } else {
        let work = canonical(Path::new(work))?;
        let root = work
            .parent()
            .ok_or_else(|| Error::new(format!("invalid go.work path: {}", work.display())))?
            .to_owned();
        Workspace {
            root,
            work: Some(work),
        }
    };
    Ok(Some(workspace))
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `facts.go` on the packages, by directory: their import paths and
/// files. Dependencies resolve from `dir`, in the target configuration.
fn facts(
    workspace: &Workspace,
    dir: &Path,
    packages: &BTreeMap<PathBuf, (String, Vec<PathBuf>)>,
) -> Result<Vec<Value>, Error> {
    let utf8 = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::new(format!("{} is not UTF-8", path.display())))
    };
    let mut list = Vec::with_capacity(packages.len());
    for (directory, (import_path, files)) in packages {
        list.push(json!({
            "dir": utf8(directory)?,
            "import_path": import_path,
            "files": files.iter().map(|file| utf8(file)).collect::<Result<Vec<_>, _>>()?,
        }));
    }
    let input = json!({ "dir": utf8(dir)?, "packages": list });
    let temp = std::env::temp_dir().join(format!(
        "deslop-go-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&temp).map_err(|e| Error::new(format!("{}: {e}", temp.display())))?;
    let temp = Temp(temp);
    let helper = temp.0.join("facts.go");
    std::fs::write(&helper, include_str!("facts.go"))
        .map_err(|e| Error::new(format!("{}: {e}", helper.display())))?;
    // The analyzer itself is built for this machine, outside any module; the
    // go commands it runs get the target configuration back.
    let mut command = Command::new("go");
    command
        .arg("run")
        .arg(&helper)
        .current_dir(&temp.0)
        .env("GO111MODULE", "off")
        .env_remove("GOOS")
        .env_remove("GOARCH")
        .env_remove("GOFLAGS")
        .env("GOWORK", index::gowork(workspace.work.as_deref()));
    for key in ["GOOS", "GOARCH", "GOFLAGS", "GO111MODULE"] {
        match std::env::var_os(key) {
            Some(value) => command.env(format!("DESLOP_TARGET_{key}"), value),
            None => command.env_remove(format!("DESLOP_TARGET_{key}")),
        };
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::new(format!("cannot run the Go analyzer: {e}")))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let written = stdin.write_all(input.to_string().as_bytes());
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|e| Error::new(format!("Go analyzer: {e}")))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "Go analyzer failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    written.map_err(|e| Error::new(format!("cannot send packages to the Go analyzer: {e}")))?;
    serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::new(format!("invalid Go analyzer output: {e}")))
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn number(v: &Value, key: &str) -> u32 {
    v[key]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

fn pos(v: &Value) -> Position {
    Position {
        line: number(v, "line"),
        column: number(v, "col"),
    }
}

fn span(v: &Value) -> Range {
    Range {
        start: pos(&v["start"]),
        end: pos(&v["end"]),
    }
}

/// Where a workspace object is declared: its file and its name's position.
type Loc = (PathBuf, Position);

fn loc(v: &Value) -> Loc {
    (PathBuf::from(text(v, "path")), pos(v))
}

fn array<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

/// `path` relative to `root`, `/`-separated, through `..` for a module
/// outside the workspace's directory.
fn relative(root: &Path, path: &Path) -> String {
    let root: Vec<Component> = root.components().collect();
    let path: Vec<Component> = path.components().collect();
    let common = root.iter().zip(&path).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_owned(); root.len() - common];
    parts.extend(
        path[common..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

fn import_path(module: &index::ModuleIndex, directory: &Path) -> String {
    match directory.strip_prefix(&module.root) {
        Ok(inside) if inside.as_os_str().is_empty() => module.path.clone(),
        Ok(inside) => format!("{}/{}", module.path, relative(Path::new(""), inside)),
        Err(_) => module.path.clone(),
    }
}

/// Analyze the active Go build configuration of the workspace.
pub fn build(workspace: &Workspace) -> Result<Model, Error> {
    let root = &workspace.root;
    let indexes = index::index(root, workspace.work.as_deref())?;
    let parsed = indexes
        .iter()
        .map(|i| {
            scip::read(&i.bytes).map_err(|e| {
                Error::new(format!(
                    "invalid scip-go index for {}: {e}",
                    i.root.display()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Each Go file once, with its module.
    let mut documents = Vec::new();
    let mut seen = HashSet::new();
    for (module, idx) in indexes.iter().zip(&parsed) {
        for doc in &idx.documents {
            let inside = Path::new(doc.path.as_ref());
            if !inside
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            {
                // scip-go indexes a cgo package's generated files, in the
                // build cache, in place of its sources.
                return Err(Error::new(format!(
                    "scip-go indexed {} outside module {}, as it does for a package that uses \
                     cgo; cgo packages cannot be analyzed (CGO_ENABLED=0 analyzes the build \
                     without cgo)",
                    doc.path,
                    module.root.display()
                )));
            }
            let path = module.root.join(inside);
            if path.extension().is_some_and(|e| e == "go") && seen.insert(path.clone()) {
                documents.push((path, doc, module));
            }
        }
    }
    if documents.is_empty() {
        return Err(Error::new("scip-go indexed no Go files"));
    }
    let mut packages = BTreeMap::<PathBuf, (String, Vec<PathBuf>)>::new();
    for (path, _, module) in &documents {
        let directory = path.parent().expect("a file in a module's directory");
        packages
            .entry(directory.to_owned())
            .or_insert_with(|| (import_path(module, directory), Vec::new()))
            .1
            .push(path.clone());
    }
    let facts: HashMap<PathBuf, Value> = facts(workspace, &indexes[0].root, &packages)?
        .into_iter()
        .map(|v| (PathBuf::from(text(&v, "path")), v))
        .collect();
    let fact_of = |path: &Path| {
        facts
            .get(path)
            .ok_or_else(|| Error::new(format!("no Go facts for {}", path.display())))
    };

    let mut model = Model::default();
    for (path, _, _) in &documents {
        let fact = fact_of(path)?;
        let node = model.tree.child(
            model.tree.root(),
            NodeKind::Package,
            text(fact, "import_path"),
        );
        model.files.push(File {
            path: relative(root, path),
            module: node,
            test: path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("_test.go")),
            lines: fact["lines"].as_u64().unwrap_or(0) as usize,
        });
    }

    // The items, by where they are declared and by symbol.
    let mut declared = HashMap::<Loc, ItemId>::new();
    let mut symbols = HashMap::<&str, ItemId>::new();
    let mut holders: Vec<Vec<(Range, ItemId)>> = vec![Vec::new(); documents.len()];
    let mut parents = Vec::<(ItemId, Loc)>::new();
    let mut wrappers = Vec::<(ItemId, &Value)>::new();
    for (index, (path, doc, _)) in documents.iter().enumerate() {
        let fact = fact_of(path)?;
        let definitions: HashMap<Position, &str> = doc
            .occurrences
            .iter()
            .filter(|o| o.definition && !o.symbol.starts_with("local "))
            .map(|o| (o.range.start, o.symbol))
            .collect();
        for decl in array(fact, "decls") {
            let kind = match text(decl, "kind") {
                "type" => ItemKind::Type,
                "function" => ItemKind::Function,
                "field" => ItemKind::Field,
                "constant" => ItemKind::Constant,
                "variant" => ItemKind::Variant,
                other => return Err(Error::new(format!("unknown Go declaration kind {other}"))),
            };
            let at = pos(&decl["name_pos"]);
            let symbol = definitions.get(&at).ok_or_else(|| {
                Error::new(format!(
                    "scip-go defines no symbol for `{}` at {}:{}:{}",
                    text(decl, "name"),
                    path.display(),
                    at.line + 1,
                    at.column + 1
                ))
            })?;
            let id = ItemId::new(model.items.len());
            model.items.push(Item {
                name: text(decl, "name").to_owned(),
                owner: model.files[index].module,
                module: None,
                file: Some(FileId::new(index)),
                kind,
                parent: None,
                trait_impl: false,
                takes_self: decl["receiver"].as_bool().unwrap_or(false),
                holds_nothing: false,
            });
            declared.insert((path.clone(), at), id);
            // Several `init` functions of a package share a symbol, which
            // code cannot name.
            symbols.entry(symbol).or_insert(id);
            holders[index].push((span(decl), id));
            if let Some(parent) = decl.get("parent") {
                parents.push((id, loc(parent)));
            }
            if ["getter", "setter", "forward"]
                .iter()
                .any(|key| decl.get(key).is_some())
            {
                wrappers.push((id, decl));
            }
        }
    }
    let item_at = |loc: &Loc| {
        declared.get(loc).copied().ok_or_else(|| {
            Error::new(format!(
                "no Go declaration at {}:{}:{}",
                loc.0.display(),
                loc.1.line + 1,
                loc.1.column + 1
            ))
        })
    };
    for (item, parent) in parents {
        let parent = item_at(&parent)?;
        let name = format!(
            "{}::{}",
            model.items[parent.index()].name,
            model.items[item.index()].name
        );
        let item = &mut model.items[item.index()];
        item.parent = Some(parent);
        item.name = name;
    }

    // Getters and setters are accessors; a getter whose fields it lends is
    // read as the code that calls it reads, a setter always writes.
    let mut reads_lent = HashSet::new();
    let mut forwarders = Vec::new();
    for (item, decl) in wrappers {
        let field_at = |v: &Value| {
            let field = item_at(&loc(v))?;
            if model.items[field.index()].kind != ItemKind::Field {
                return Err(Error::new(format!(
                    "Go accessor `{}` hands out something other than a field",
                    model.items[item.index()].name
                )));
            }
            Ok(field)
        };
        if let Some(forward) = decl.get("forward") {
            let target = item_at(&loc(&forward["target"]))?;
            let fields = array(forward, "fields")
                .map(field_at)
                .collect::<Result<Vec<_>, _>>()?;
            forwarders.push((item, target, fields));
        } else if decl.get("getter").is_some() {
            let (mut lent, mut copied) = (Vec::new(), Vec::new());
            for result in array(decl, "getter") {
                let field = field_at(&result["field"])?;
                if result["lent"].as_bool().unwrap_or(true) {
                    lent.push(field);
                } else {
                    copied.push(field);
                }
            }
            if !lent.is_empty() {
                reads_lent.insert(item);
            }
            model.wrappers.insert(
                item,
                Wrapper::Accessor {
                    lent,
                    copied,
                    to_read: false,
                },
            );
        } else {
            let lent = array(decl, "setter")
                .map(field_at)
                .collect::<Result<Vec<_>, _>>()?;
            model.wrappers.insert(
                item,
                Wrapper::Accessor {
                    lent,
                    copied: Vec::new(),
                    to_read: false,
                },
            );
        }
    }
    // No chain of forwarders may loop: one that would close a loop is a
    // function of its own.
    for (item, target, fields) in forwarders {
        let mut at = target;
        let mut loops = at == item;
        while let Some(Wrapper::Forwarder { target, .. }) = model.wrappers.get(&at) {
            at = *target;
            loops |= at == item;
        }
        if !loops {
            model
                .wrappers
                .insert(item, Wrapper::Forwarder { target, fields });
        }
    }
    let behind_forwarders = |mut at: ItemId| {
        while let Some(Wrapper::Forwarder { target, .. }) = model.wrappers.get(&at) {
            at = *target;
        }
        at
    };
    let reads_lent: HashSet<ItemId> = model
        .wrappers
        .keys()
        .copied()
        .filter(|&item| reads_lent.contains(&behind_forwarders(item)))
        .collect();

    for (index, (path, doc, _)) in documents.iter().enumerate() {
        let fact = fact_of(path)?;
        let file = FileId::new(index);
        let from = model.files[index].module;
        let test = model.files[index].test;
        let modes: HashMap<Position, &str> = array(fact, "uses")
            .map(|u| (pos(&u["pos"]), text(u, "mode")))
            .collect();
        let holder_of = |at: Range| {
            holders[index]
                .iter()
                .filter(|(range, _)| range.holds(at))
                .max_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.end)))
                .map(|&(_, id)| id)
        };
        let mut refer = |at: Range, item: ItemId, mode: Option<&str>, dispatched: bool| {
            let holder = holder_of(at);
            let copies = if holder.is_some_and(|h| model.wrappers.contains_key(&h)) {
                false
            } else if model.items[item.index()].kind == ItemKind::Field
                || reads_lent.contains(&item)
            {
                match mode {
                    Some("read") => true,
                    Some("mutate") => false,
                    _ => {
                        return Err(Error::new(format!(
                            "cannot tell what the Go code at {}:{}:{} does with `{}`",
                            path.display(),
                            at.start.line + 1,
                            at.start.column + 1,
                            model.items[item.index()].name
                        )));
                    }
                }
            } else {
                false
            };
            model.references.push(Reference {
                file,
                line: at.start.line + 1,
                from,
                holder,
                item,
                copies,
                test,
                dispatched,
            });
            Ok(())
        };
        for occ in doc.occurrences.iter().filter(|o| !o.definition) {
            if let Some(&item) = symbols.get(occ.symbol) {
                refer(occ.range, item, modes.get(&occ.range.start).copied(), false)?;
            }
        }
        for implicit in array(fact, "implicit") {
            let at = pos(&implicit["pos"]);
            let item = item_at(&loc(&implicit["target"]))?;
            let dispatched = implicit["dispatched"].as_bool().unwrap_or(false);
            let range = Range { start: at, end: at };
            refer(range, item, Some(text(implicit, "mode")), dispatched)?;
        }
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_module_outside_the_workspace_directory_is_reached_through_parent_directories() {
        assert_eq!(
            relative(Path::new("/r/work"), Path::new("/r/shared/queue/queue.go")),
            "../shared/queue/queue.go"
        );
        assert_eq!(
            relative(Path::new("/r/work"), Path::new("/r/work/app/main.go")),
            "app/main.go"
        );
    }
}
