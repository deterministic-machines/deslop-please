//! A SCIP index of a Java project, as scip-java writes it, and its files'
//! text, turned into the plain model.
//!
//! A package of the model is a Maven module or Gradle project: the directory
//! that holds a `src/` of Java sources, named by its artifact. Its modules are
//! the Java packages below the package that all of its sources share, so
//! `com.acme.queue` of an artifact whose sources are all in `com.acme` is its
//! module `queue`. A class is a type, its nested classes, fields, methods and
//! constructors are written for it, a `static` field is a constant, and an
//! enum's constants are its variants. A method that is not `static` takes
//! `self`. A source set whose name says test, such as `src/test/java`, is
//! test code.

mod source;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::from_index::forwarders::{self, Name};
use crate::Error;
use crate::model::{
    File, FileId, Item, ItemId, ItemKind, Model, NodeId, NodeKind, Reference, Tree, Wrapper,
};
use crate::scip::{Document, Index, Kind, Place, Range, Symbol, SymbolInformation};
use source::{Access, Qualifier, Source};

/// The local symbol that stands for the implicit `this` of a method.
const THIS: &str = "this";

/// Directories that hold what a build writes, such as generated sources.
const OUTPUTS: [&str; 3] = ["target", "build", "out"];

pub fn build(index: &Index, root: &Path) -> Result<Model, Error> {
    let mut units = Vec::new();
    for document in &index.documents {
        let path: &str = &document.path;
        let generated = path.split('/').any(|part| OUTPUTS.contains(&part));
        if !path.ends_with(".java") || generated {
            continue;
        }
        let on_disk = root.join(path);
        let text = std::fs::read_to_string(&on_disk)
            .map_err(|e| Error::new(format!("{}: {e}", on_disk.display())))?;
        units.push(Unit::new(document, text));
    }
    let mut builder = Builder::new(&units, root);
    for at in 0..units.len() {
        builder.add_unit(at)?;
    }
    Ok(builder.finish())
}

struct Unit<'a> {
    document: &'a Document<'a>,
    source: Source,
    /// The directory of its Maven module or Gradle project, relative to the
    /// root, `""` for the root itself.
    directory: &'a str,
    /// Its Java package, by parts.
    package: Vec<String>,
    test: bool,
    lines: usize,
}

impl<'a> Unit<'a> {
    fn new(document: &'a Document<'a>, text: String) -> Unit<'a> {
        let path: &'a str = &document.path;
        let (directory, source_set) = match path.strip_prefix("src/") {
            Some(rest) => ("", rest),
            None => match path.split_once("/src/") {
                Some((directory, rest)) => (directory, rest),
                None => ("", ""),
            },
        };
        let set = source_set.split('/').next().unwrap_or_default();
        let lines = text.lines().count();
        let source = Source::new(text);
        Unit {
            document,
            package: source.package(),
            source,
            directory,
            test: set.to_ascii_lowercase().contains("test"),
            lines,
        }
    }
}

/// What a symbol of the index stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Named {
    /// A parameter or variable, which only its document names.
    Local,
    Item(ItemId),
    /// Something outside the project, or a Java package.
    Outside(Kind),
}

/// Where an item is defined: its unit, the whole item, and its name.
#[derive(Clone, Copy)]
struct Definition {
    unit: usize,
    block: Range,
    name: Range,
}

struct Builder<'a> {
    units: &'a [Unit<'a>],
    tree: Tree,
    files: Vec<File>,
    items: Vec<Item>,
    references: Vec<Reference>,
    /// By the place of each reference in `references`: what its code does
    /// with what it names.
    accesses: Vec<Access>,
    /// By directory: its package's node, and the Java package all of its
    /// sources are in.
    directories: HashMap<&'a str, (NodeId, Vec<String>)>,
    /// The directory of each SCIP package, by name and version.
    packages: HashMap<(&'a str, &'a str), usize>,
    directory_names: Vec<&'a str>,
    symbols: HashMap<&'a str, Named>,
    information: HashMap<&'a str, &'a SymbolInformation<'a>>,
    /// `None` for an item defined more than once.
    definitions: HashMap<ItemId, Option<Definition>>,
    /// By unit: the items whose definitions hold code, by their blocks.
    holders: Vec<Vec<(Range, ItemId)>>,
}

impl<'a> Builder<'a> {
    fn new(units: &'a [Unit<'a>], root: &Path) -> Builder<'a> {
        let mut builder = Builder {
            units,
            tree: Tree::new(),
            files: Vec::new(),
            items: Vec::new(),
            references: Vec::new(),
            accesses: Vec::new(),
            directories: HashMap::new(),
            packages: HashMap::new(),
            directory_names: Vec::new(),
            symbols: HashMap::new(),
            information: HashMap::new(),
            definitions: HashMap::new(),
            holders: vec![Vec::new(); units.len()],
        };
        for information in units.iter().flat_map(|unit| &unit.document.symbols) {
            builder.information.insert(information.symbol, information);
        }
        let root_name = root
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        builder.add_packages(&root_name);
        builder
    }

    /// A package of the tree for each directory, inside the one whose
    /// directory holds it, named by its artifact.
    fn add_packages(&mut self, root_name: &str) {
        let mut directories: Vec<&'a str> = self.units.iter().map(|unit| unit.directory).collect();
        directories.sort_by_key(|directory| (directory.split('/').count(), *directory));
        directories.dedup();
        let mut artifacts: HashMap<&'a str, String> = HashMap::new();
        for unit in self.units {
            for occurrence in unit.document.occurrences.iter().filter(|o| o.definition) {
                let Ok(Some(symbol)) = Symbol::parse(occurrence.symbol) else {
                    continue;
                };
                let Some((package, version)) = symbol.package_version() else {
                    continue;
                };
                if package == "." {
                    continue;
                }
                let at = directories
                    .iter()
                    .position(|d| *d == unit.directory)
                    .expect("every unit's directory is listed");
                self.packages.entry((package, version)).or_insert(at);
                let artifact = package.rsplit('/').next().unwrap_or(package);
                artifacts
                    .entry(unit.directory)
                    .or_insert_with(|| artifact.to_owned());
            }
        }
        let root = self.tree.root();
        let mut placed: Vec<(&str, NodeId)> = Vec::new();
        for &directory in &directories {
            let name = artifacts.get(directory).cloned().unwrap_or_else(|| {
                let last = directory.rsplit('/').next().unwrap_or(directory);
                if last.is_empty() {
                    root_name.to_owned()
                } else {
                    last.to_owned()
                }
            });
            let node = if directory.is_empty() {
                let node = self.tree.node_mut(root);
                node.kind = NodeKind::Package;
                node.name = name;
                root
            } else {
                let parent = placed
                    .iter()
                    .filter(|(outer, _)| is_under(directory, outer))
                    .max_by_key(|(outer, _)| outer.len())
                    .map_or(root, |&(_, node)| node);
                self.tree.child(parent, NodeKind::Package, &name)
            };
            placed.push((directory, node));
            let packages = self
                .units
                .iter()
                .filter(|unit| unit.directory == directory)
                .map(|unit| unit.package.as_slice());
            self.directories
                .insert(directory, (node, common_prefix(packages)));
        }
        self.directory_names = directories;
    }

    /// The module of the Java package `parts` in the package of `directory`.
    fn module(&mut self, directory: &str, parts: &[impl AsRef<str>]) -> NodeId {
        let (mut node, prefix) = self.directories[directory].clone();
        let shares =
            parts.len() >= prefix.len() && parts.iter().zip(&prefix).all(|(a, b)| a.as_ref() == b);
        let below = if shares {
            &parts[prefix.len()..]
        } else {
            parts
        };
        for part in below {
            node = self.tree.child(node, NodeKind::Module, part.as_ref());
        }
        node
    }

    fn add_unit(&mut self, at: usize) -> Result<(), Error> {
        let unit = &self.units[at];
        let module = self.module(unit.directory, &unit.package);
        let file = FileId::new(self.files.len());
        self.files.push(File {
            path: unit.document.path.to_string(),
            module,
            test: unit.test,
            lines: unit.lines,
        });
        for occurrence in unit.document.occurrences.iter().filter(|o| o.definition) {
            let Some(item) = self.item(occurrence.symbol)? else {
                continue;
            };
            self.items[item.index()].file.get_or_insert(file);
            if let Some(block) = occurrence.enclosing {
                self.holders[at].push((block, item));
                let definition = Definition {
                    unit: at,
                    block,
                    name: occurrence.range,
                };
                self.definitions
                    .entry(item)
                    .and_modify(|known| *known = None)
                    .or_insert(Some(definition));
            }
        }
        let declarations = unit.source.declarations();
        for occurrence in unit.document.occurrences.iter().filter(|o| !o.definition) {
            let Some(item) = self.item(occurrence.symbol)? else {
                continue;
            };
            if declarations.iter().any(|d| d.holds(occurrence.range)) {
                continue;
            }
            self.references.push(Reference {
                file,
                line: occurrence.range.start.line + 1,
                from: module,
                holder: innermost(&self.holders[at], occurrence.range),
                item,
                copies: false,
                test: false,
            });
            self.accesses.push(unit.source.access(occurrence.range));
        }
        Ok(())
    }

    fn item(&mut self, text: &'a str) -> Result<Option<ItemId>, Error> {
        match self.named(text)? {
            Named::Item(item) => Ok(Some(item)),
            Named::Local | Named::Outside(_) => Ok(None),
        }
    }

    fn named(&mut self, text: &'a str) -> Result<Named, Error> {
        if let Some(&known) = self.symbols.get(text) {
            return Ok(known);
        }
        let symbol = Symbol::parse(text).map_err(|e| Error::new(e.to_string()))?;
        let named = match symbol {
            None => Named::Local,
            Some(symbol) => match (symbol.package_in(&self.packages), symbol.place()) {
                // `Queue.class` names the class.
                (Some(_), Place::In(container)) if symbol.item_name().ends_with("::class") => {
                    self.named(container)?
                }
                (Some(directory), _)
                    if symbol.module().is_none()
                        && !matches!(symbol.kind(), Kind::TypeParameter | Kind::Parameter) =>
                {
                    Named::Item(self.new_item(&symbol, directory)?)
                }
                _ => Named::Outside(symbol.kind()),
            },
        };
        self.symbols.insert(text, named);
        Ok(named)
    }

    fn new_item(&mut self, symbol: &Symbol<'a>, directory: usize) -> Result<ItemId, Error> {
        let parent = match symbol.place() {
            Place::In(container) => self.item(container)?,
            Place::Module | Place::Impl(_) => None,
        };
        let directory = self.directory_names[directory];
        let owner = self.module(directory, &symbol.namespaces());
        let information = self.information.get(symbol.text()).copied();
        let signature = information.map_or("", |information| information.signature);
        let is_static = information.is_some_and(|information| {
            matches!(information.kind, STATIC_FIELD | STATIC_METHOD)
                || information
                    .signature
                    .split_whitespace()
                    .any(|w| w == "static")
        });
        let kind = match symbol.kind() {
            Kind::Type => ItemKind::Type,
            Kind::Method => ItemKind::Function,
            Kind::Term if signature.contains("/* ordinal") => ItemKind::Variant,
            Kind::Term if is_static => ItemKind::Constant,
            Kind::Term => ItemKind::Field,
            _ => ItemKind::Other,
        };
        let constructor = symbol.text().contains("`<init>`(");
        let id = ItemId::new(self.items.len());
        self.items.push(Item {
            name: symbol.item_name(),
            owner,
            module: None,
            file: None,
            kind,
            parent,
            trait_impl: information.is_some_and(|information| information.implements)
                && !signature.split_whitespace().any(|w| w == "abstract"),
            takes_self: kind == ItemKind::Function && !is_static && !constructor,
            holds_nothing: false,
        });
        Ok(id)
    }

    fn finish(mut self) -> Model {
        self.mark_test_code();
        let scalars = self.scalar_fields();
        for at in 0..self.references.len() {
            let field = self.references[at].item;
            if self.items[field.index()].kind == ItemKind::Field {
                self.references[at].copies = only_reads(self.accesses[at], &[field], &scalars);
            }
        }
        let wrappers = self.wrappers(&scalars);
        self.mark_lent_reads(&wrappers, &scalars);
        Model {
            tree: self.tree,
            files: self.files,
            items: self.items,
            references: self.references,
            wrappers,
        }
    }

    /// A module is test code when only test sources are in it.
    fn mark_test_code(&mut self) {
        let mut test_modules = HashSet::new();
        let mut non_test_modules = HashSet::new();
        for file in &self.files {
            if file.test {
                test_modules.insert(file.module);
            } else {
                non_test_modules.extend(self.tree.ancestors(file.module));
            }
        }
        for id in self.tree.ids() {
            let node = self.tree.node_mut(id);
            node.test = node.kind == NodeKind::Module
                && test_modules.contains(&id)
                && !non_test_modules.contains(&id);
        }
        for reference in &mut self.references {
            reference.test =
                self.files[reference.file.index()].test || self.tree.is_test(reference.from);
        }
    }

    /// The fields that hold values code can only read, as their declarations say.
    fn scalar_fields(&self) -> HashSet<ItemId> {
        const ENUM: u64 = 11;
        let enums: HashSet<&str> = self
            .symbols
            .iter()
            .filter(|(text, named)| {
                matches!(named, Named::Item(_))
                    && self.information.get(*text).is_some_and(|i| i.kind == ENUM)
            })
            .filter_map(|(_, named)| match named {
                Named::Item(item) => Some(simple(&self.items[item.index()].name)),
                _ => None,
            })
            .collect();
        self.symbols
            .iter()
            .filter_map(|(&text, &named)| match named {
                Named::Item(item) if self.items[item.index()].kind == ItemKind::Field => {
                    let information = self.information.get(text)?;
                    let is_enum = |name: &str| enums.contains(name);
                    source::holds_scalar(information.signature, is_enum).then_some(item)
                }
                _ => None,
            })
            .collect()
    }

    /// Every forwarder and accessor.
    fn wrappers(&self, scalars: &HashSet<ItemId>) -> HashMap<ItemId, Wrapper> {
        let mut definitions = forwarders::Definitions::default();
        for (&function, definition) in &self.definitions {
            let item = &self.items[function.index()];
            if let (ItemKind::Function, Some(definition)) = (item.kind, definition) {
                let names = self.names(function, definition);
                definitions.add(function, !item.trait_impl, &names);
            }
        }
        let mut wrappers = HashMap::new();
        for forwarding in definitions.forwardings() {
            if !reaches(&wrappers, forwarding.target, forwarding.forwarder) {
                let wrapper = Wrapper::Forwarder {
                    target: forwarding.target,
                    fields: forwarding.fields,
                };
                wrappers.insert(forwarding.forwarder, wrapper);
            }
        }
        let mut named: HashMap<ItemId, HashSet<ItemId>> = HashMap::new();
        for reference in &self.references {
            if let Some(holder) = reference.holder {
                named.entry(holder).or_default().insert(reference.item);
            }
        }
        let fields_by_name: HashMap<(ItemId, &str), ItemId> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.kind == ItemKind::Field)
            .filter_map(|(at, item)| Some(((item.parent?, simple(&item.name)), ItemId::new(at))))
            .collect();
        for (at, item) in self.items.iter().enumerate() {
            let function = ItemId::new(at);
            if item.kind != ItemKind::Function || !item.takes_self || item.trait_impl {
                continue;
            }
            let Some(own) = item.parent else {
                continue;
            };
            let accessor = match self.definitions.get(&function) {
                Some(Some(definition)) => {
                    let names = named.get(&function).cloned().unwrap_or_default();
                    self.accessor(own, *definition, &names, scalars)
                }
                Some(None) => None,
                // A record's accessor, which the compiler writes.
                None => fields_by_name
                    .get(&(own, simple(&item.name)))
                    .map(|&field| getter(field, scalars)),
            };
            if let Some(accessor) = accessor {
                wrappers.entry(function).or_insert(accessor);
            }
        }
        wrappers
    }

    /// What a method of `own`'s is as an accessor, if it is one: its body only
    /// returns a field of `own`'s, `return items;`, or only sets one to its
    /// parameter, `this.head = head;`. `names` are the items its code names.
    fn accessor(
        &self,
        own: ItemId,
        definition: Definition,
        names: &HashSet<ItemId>,
        scalars: &HashSet<ItemId>,
    ) -> Option<Wrapper> {
        let mut field = None;
        for &name in names {
            let named = &self.items[name.index()];
            match named.kind {
                ItemKind::Field if named.parent == Some(own) && field.is_none() => {
                    field = Some(name);
                }
                ItemKind::Type => {}
                _ => return None,
            }
        }
        let field = field?;
        let unit = &self.units[definition.unit];
        let body = unit.source.body(definition.block, definition.name)?;
        let name = simple(&self.items[field.index()].name);
        let body = match body.as_slice() {
            ["this", ".", rest @ ..] | ["return", "this", ".", rest @ ..] => {
                let returns = body[0] == "return";
                (returns, rest)
            }
            ["return", rest @ ..] => (true, rest),
            rest => (false, rest),
        };
        match body {
            (true, [written, ";"]) if *written == name => Some(getter(field, scalars)),
            (false, [written, "=", parameter, ";"])
                if *written == name && is_identifier(parameter) =>
            {
                Some(Wrapper::Accessor {
                    lent: vec![field],
                    copied: Vec::new(),
                    to_read: false,
                })
            }
            _ => None,
        }
    }

    /// The names written in `function`'s item after its own, as forwarders
    /// are told by them. A method that takes `self` has `this` for its first
    /// parameter, and names a field or such a method on it unless the code
    /// writes another receiver.
    fn names(&self, function: ItemId, definition: &Definition) -> Vec<Name<'a>> {
        let unit = &self.units[definition.unit];
        let takes_self = self.items[function.index()].takes_self;
        let kinds: HashMap<&str, u64> = unit
            .document
            .symbols
            .iter()
            .map(|information| (information.symbol, information.kind))
            .collect();
        let mut occurrences: Vec<_> = unit
            .document
            .occurrences
            .iter()
            .filter(|o| definition.block.holds(o.range) && o.range.start > definition.name.start)
            .collect();
        occurrences.sort_by_key(|o| o.range.start);
        let mut names = Vec::new();
        if takes_self {
            names.push(Name::Parameter {
                local: THIS,
                is_self: true,
            });
        }
        for occurrence in occurrences {
            let symbol = occurrence.symbol;
            let on_this = takes_self
                && matches!(
                    unit.source.qualifier(occurrence.range),
                    Qualifier::None | Qualifier::This
                );
            let name = match self.symbols.get(symbol).copied() {
                None => Name::Other,
                Some(Named::Local) if occurrence.definition => {
                    if kinds.get(symbol).copied().unwrap_or_default() == 0 {
                        Name::Parameter {
                            local: symbol,
                            is_self: false,
                        }
                    } else {
                        Name::Other
                    }
                }
                Some(Named::Local) => Name::Value(symbol),
                Some(_) if occurrence.definition => Name::Other,
                Some(Named::Item(item)) => {
                    let named = &self.items[item.index()];
                    match named.kind {
                        ItemKind::Type | ItemKind::Module | ItemKind::Impl => Name::Type,
                        ItemKind::Field | ItemKind::Function => {
                            let implicit = named.kind == ItemKind::Field || named.takes_self;
                            if on_this && implicit {
                                names.push(Name::Value(THIS));
                            }
                            if named.kind == ItemKind::Field {
                                Name::Field(item)
                            } else {
                                Name::Function(Some(item))
                            }
                        }
                        ItemKind::Variant | ItemKind::Constant | ItemKind::Other => Name::Other,
                    }
                }
                Some(Named::Outside(kind)) => match kind {
                    Kind::Namespace | Kind::Type | Kind::TypeParameter => Name::Type,
                    Kind::Method => Name::Function(None),
                    Kind::Term | Kind::Parameter | Kind::Meta | Kind::Macro => Name::Other,
                },
            };
            names.push(name);
        }
        names
    }

    /// Marks the calls of getters, directly or through forwarders, that
    /// only read what they hand out, as `finish` marks code that names fields.
    fn mark_lent_reads(&mut self, wrappers: &HashMap<ItemId, Wrapper>, scalars: &HashSet<ItemId>) {
        for at in 0..self.references.len() {
            let mut called = self.references[at].item;
            while let Some(Wrapper::Forwarder { target, .. }) = wrappers.get(&called) {
                called = *target;
            }
            if let Some(Wrapper::Accessor {
                lent,
                to_read: true,
                ..
            }) = wrappers.get(&called)
                && !lent.is_empty()
            {
                self.references[at].copies = only_reads(self.accesses[at], lent, scalars);
            }
        }
    }
}

const STATIC_FIELD: u64 = 79;
const STATIC_METHOD: u64 = 80;

/// An accessor that returns `field`: it hands out a copy of a scalar, and
/// lends anything else.
fn getter(field: ItemId, scalars: &HashSet<ItemId>) -> Wrapper {
    let (copied, lent) = if scalars.contains(&field) {
        (vec![field], Vec::new())
    } else {
        (Vec::new(), vec![field])
    };
    Wrapper::Accessor {
        lent,
        copied,
        to_read: true,
    }
}

/// Whether code that does `access` with `fields` only reads their values:
/// it writes none, and each holds a value that cannot change.
fn only_reads(access: Access, fields: &[ItemId], scalars: &HashSet<ItemId>) -> bool {
    access != Access::Write && fields.iter().all(|field| scalars.contains(field))
}

/// Whether the chain of forwarders from `from` passes `to`.
fn reaches(wrappers: &HashMap<ItemId, Wrapper>, from: ItemId, to: ItemId) -> bool {
    let mut at = from;
    loop {
        if at == to {
            return true;
        }
        match wrappers.get(&at) {
            Some(Wrapper::Forwarder { target, .. }) => at = *target,
            _ => return false,
        }
    }
}

/// The smallest of `blocks` that holds `range`.
fn innermost(blocks: &[(Range, ItemId)], range: Range) -> Option<ItemId> {
    blocks
        .iter()
        .filter(|(block, _)| *block != range && block.holds(range))
        .min_by_key(|(block, _)| {
            (
                block.end.line - block.start.line,
                std::cmp::Reverse(block.start),
            )
        })
        .map(|&(_, item)| item)
}

/// The parts all of `packages` start with.
fn common_prefix<'p>(mut packages: impl Iterator<Item = &'p [String]>) -> Vec<String> {
    let Some(first) = packages.next() else {
        return Vec::new();
    };
    let mut length = first.len();
    for package in packages {
        length = first
            .iter()
            .zip(package)
            .take(length)
            .take_while(|(a, b)| a == b)
            .count();
    }
    first[..length].to_vec()
}

/// `items` for `RunQueue::items`.
fn simple(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

fn is_identifier(text: &str) -> bool {
    text.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '$')
        && !matches!(text, "null" | "true" | "false" | "this" | "new")
}

fn is_under(path: &str, directory: &str) -> bool {
    directory.is_empty()
        || path
            .strip_prefix(directory)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_all_sources_share() {
        let parts = |text: &str| -> Vec<String> { text.split('.').map(str::to_owned).collect() };
        let packages = [
            parts("com.acme.queue"),
            parts("com.acme.sched.policy"),
            parts("com.acme"),
        ];
        assert_eq!(
            common_prefix(packages.iter().map(Vec::as_slice)),
            ["com", "acme"]
        );
        let one = [parts("com.acme.queue")];
        assert_eq!(
            common_prefix(one.iter().map(Vec::as_slice)),
            ["com", "acme", "queue"]
        );
        let none = [parts("a.b"), parts("c")];
        assert!(common_prefix(none.iter().map(Vec::as_slice)).is_empty());
    }

    #[test]
    fn test_source_sets_by_name() {
        let unit = |path: &str| {
            let document = Box::leak(Box::new(Document {
                path: path.to_owned().into(),
                occurrences: Vec::new(),
                symbols: Vec::new(),
            }));
            let unit = Unit::new(document, "package a;".into());
            (unit.directory, unit.test)
        };
        assert_eq!(unit("src/main/java/a/A.java"), ("", false));
        assert_eq!(unit("src/test/java/a/ATest.java"), ("", true));
        assert_eq!(
            unit("core/src/integrationTest/java/a/B.java"),
            ("core", true)
        );
        assert_eq!(unit("apps/web/src/main/java/a/C.java"), ("apps/web", false));
    }
}
