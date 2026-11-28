//! Python frontend: project and package discovery, dotted import resolution,
//! and Tree-sitter extraction into the language-agnostic IR.

mod discovery;
mod extract;
mod resolve;
mod syntax;

use std::fs;
use std::path::Path;

use crate::Error;
use crate::configuration::Configuration;
use crate::model::NodeId;
use crate::treesitter::lang::{
    DiscoveredFile, Language, LanguageFrontend, ResolvedImport, WorkspaceLayout,
};
use crate::treesitter::summary::{FileSummary, RawImport};

/// Python projects: pyproject/setup metadata, packages, modules and tests.
pub struct PythonFrontend;

impl LanguageFrontend for PythonFrontend {
    fn language(&self) -> Language {
        Language::Python
    }

    fn detect(&self, root: &Path) -> bool {
        ["pyproject.toml", "setup.py", "setup.cfg"]
            .iter()
            .any(|name| root.join(name).is_file())
            || holds_python(root)
            || holds_python(&root.join("src"))
    }

    fn discover(
        &self,
        root: &Path,
        _configuration: &Configuration,
        layout: &mut WorkspaceLayout,
    ) -> Result<(), Error> {
        discovery::discover(root, layout)
    }

    fn extract_file(
        &self,
        file: &DiscoveredFile,
        source: &str,
        _configuration: &Configuration,
    ) -> FileSummary {
        extract::summarize(file, source)
    }

    fn resolve_import(
        &self,
        layout: &WorkspaceLayout,
        from_file: &DiscoveredFile,
        from_module: NodeId,
        import: &RawImport,
    ) -> Option<ResolvedImport> {
        resolve::resolve_import(layout, from_file, from_module, import)
    }
}

/// True when the directory directly holds a Python module or package.
fn holds_python(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            path.join("__init__.py").is_file()
        } else {
            path.extension().is_some_and(|extension| extension == "py")
        }
    })
}
#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::PythonFrontend;
    use crate::configuration::Configuration;
    use crate::lint::{self, Diagnostic, Lint};
    use crate::model::{ItemId, ItemKind, Model, Wrapper};
    use crate::treesitter::lang::{LanguageFrontend, WorkspaceLayout};
    use crate::treesitter::resolve::build_model;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
    }

    fn analyze(root: &Path) -> (Model, Vec<Diagnostic>) {
        let frontend = PythonFrontend;
        let mut layout = WorkspaceLayout::new(root.to_path_buf());
        frontend
            .discover(root, &Configuration::default(), &mut layout)
            .expect("discovery succeeds");
        let summaries = layout
            .files
            .iter()
            .map(|file| {
                let source = fs::read_to_string(&file.abs_path).expect("read source");
                frontend.extract_file(file, &source, &Configuration::default())
            })
            .collect();
        let model = build_model(layout, summaries, &[&frontend]);
        let diagnostics = lint::check(&model, None);
        (model, diagnostics)
    }

    fn item(model: &Model, name: &str, kind: ItemKind) -> ItemId {
        model
            .items
            .iter()
            .position(|item| item.name == name && item.kind == kind)
            .map(ItemId::new)
            .unwrap_or_else(|| panic!("no {kind:?} named {name}"))
    }

    fn member(model: &Model, type_name: &str, member: &str) -> ItemId {
        let owner = item(model, type_name, ItemKind::Type);
        let field = format!("{type_name}::{member}");
        // A field is written inside its type's symbol, as SCIP names it;
        // a method keeps its bare name.
        model
            .items
            .iter()
            .position(|item| {
                item.parent == Some(owner) && (item.name == member || item.name == field)
            })
            .map(ItemId::new)
            .unwrap_or_else(|| panic!("no {type_name}.{member}"))
    }

    fn keys(diagnostics: &[Diagnostic], lint: Lint) -> Vec<&str> {
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.lint == lint)
            .map(|diagnostic| diagnostic.key.as_str())
            .collect()
    }

    const PROJECT: &str = "pyproject.toml";
    const PYPROJECT: &str = "[project]\nname = \"app\"\n";

    fn workspace() -> Vec<(&'static str, &'static str)> {
        vec![
            (PROJECT, PYPROJECT),
            ("src/app/__init__.py", "from .queue import Queue\n"),
            (
                "src/app/queue.py",
                "from .billing import Invoice\n\
                 \n\
                 class Ring:\n\
                 \x20   def __init__(self):\n\
                 \x20       self.count = 0\n\
                 \n\
                 \x20   def push(self, item: int) -> None:\n\
                 \x20       self.count += item\n\
                 \n\
                 class Queue:\n\
                 \x20   def __init__(self, limit: int):\n\
                 \x20       self.items: list[int] = []\n\
                 \x20       self.limit = limit\n\
                 \x20       self.ring = Ring()\n\
                 \x20       self.invoice: Invoice | None = None\n\
                 \n\
                 \x20   def push(self, item: int) -> None:\n\
                 \x20       self.ring.push(item)\n\
                 \n\
                 \x20   @property\n\
                 \x20   def contents(self):\n\
                 \x20       return self.items\n\
                 \n\
                 \x20   def full(self) -> bool:\n\
                 \x20       return self.limit <= len(self.items)\n",
            ),
            (
                "src/app/billing.py",
                "from .queue import Queue\n\
                 \n\
                 class Invoice:\n\
                 \x20   def __init__(self, queue: Queue):\n\
                 \x20       self.queue = queue\n\
                 \n\
                 \x20   def add(self, item: int) -> None:\n\
                 \x20       self.queue.push(item)\n\
                 \n\
                 \x20   def ready(self) -> bool:\n\
                 \x20       full = self.queue.full()\n\
                 \x20       return not full\n",
            ),
            (
                "src/app/report.py",
                "from .queue import Queue\n\
                 \n\
                 def drain(queue: Queue) -> None:\n\
                 \x20   queue.items.append(1)\n\
                 \n\
                 def grow(queue: Queue) -> None:\n\
                 \x20   queue.contents.append(2)\n",
            ),
            (
                "src/app/a.py",
                "from .b import B\n\
                 \n\
                 class A:\n\
                 \x20   def ping(self) -> int:\n\
                 \x20       return 1\n\
                 \n\
                 def use_b(b: B) -> int:\n\
                 \x20   return b.ping()\n",
            ),
            (
                "src/app/b.py",
                "from .a import A\n\
                 \n\
                 class B:\n\
                 \x20   def ping(self) -> int:\n\
                 \x20       return 2\n\
                 \n\
                 def use_a(a: A) -> int:\n\
                 \x20   return a.ping()\n",
            ),
            (
                "src/app/store/__init__.py",
                "from .orders import Order, cancel_order, place_order\n",
            ),
            (
                "src/app/store/orders.py",
                "class Order:\n\
                 \x20   def __init__(self, sku: str):\n\
                 \x20       self.sku = sku\n\
                 \n\
                 \x20   def total(self) -> int:\n\
                 \x20       return 0\n\
                 \n\
                 def place_order(order: Order) -> int:\n\
                 \x20   return order.total() + 1\n\
                 \n\
                 def cancel_order(order: Order) -> int:\n\
                 \x20   return order.total() * 2\n",
            ),
            (
                "src/app/main.py",
                "from .store import Order, cancel_order, place_order\n\
                 \n\
                 def run(order: Order) -> int:\n\
                 \x20   place_order(order)\n\
                 \x20   cancel_order(order)\n\
                 \x20   return order.total()\n",
            ),
            (
                "tests/test_store.py",
                "from app.store import Order, place_order\n\
                 \n\
                 def test_places() -> None:\n\
                 \x20   order = Order(\"sku\")\n\
                 \x20   order.sku = \"other\"\n\
                 \x20   place_order(order)\n",
            ),
        ]
    }

    #[test]
    fn a_python_workspace_resolves_into_a_model_and_reports_its_lints() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        for (path, contents) in workspace() {
            write(root, path, contents);
        }
        let (model, diagnostics) = analyze(root);

        // The frontend found the packages, modules, and the test file.
        let queue = item(&model, "Queue", ItemKind::Type);
        let push = member(&model, "Queue", "push");
        assert_eq!(model.items[push.index()].parent, Some(queue));
        let items = member(&model, "Queue", "items");
        assert_eq!(
            model.items[items.index()].kind,
            ItemKind::Field,
            "Queue.items is a field"
        );

        // self.ring.push(item) forwards to Ring.push on the ring field.
        let ring = member(&model, "Queue", "ring");
        match model.wrappers.get(&push) {
            Some(Wrapper::Forwarder { fields, .. }) => assert_eq!(fields, &[ring]),
            other => panic!("Queue.push is no forwarder: {other:?}"),
        }

        // The property getter hands out the list itself, so it lends the field.
        let contents = member(&model, "Queue", "contents");
        match model.wrappers.get(&contents) {
            Some(Wrapper::Accessor { lent, .. }) => assert_eq!(lent, &[items]),
            other => panic!("Queue.contents is no accessor: {other:?}"),
        }

        let cycle = keys(&diagnostics, Lint::ModuleCycle);
        assert!(
            cycle.contains(&"app.a,app.b"),
            "a and b import each other: {cycle:?}"
        );

        let tangles = keys(&diagnostics, Lint::Tangle);
        assert!(
            tangles
                .iter()
                .any(|key| key.contains("app.billing.Invoice") && key.contains("app.queue.Queue")),
            "Invoice and Queue know each other: {tangles:?}"
        );

        let leaks = keys(&diagnostics, Lint::FieldLeak);
        assert!(
            leaks.contains(&"app.queue.Queue::items"),
            "items leaks directly and through the getter: {leaks:?}"
        );
        let leak = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.lint == Lint::FieldLeak && diagnostic.key == "app.queue.Queue::items"
            })
            .expect("the leak of Queue.items");
        assert!(
            leak.notes.iter().any(|note| note.contains(", through ")),
            "the getter hands the field out: {:?}",
            leak.notes
        );

        // Test code is analysed but never counted.
        let files: BTreeSet<&str> = diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.location.as_ref())
            .map(|location| location.file.as_str())
            .collect();
        assert!(
            files.iter().all(|file| !file.starts_with("tests/")),
            "test files raise no diagnostics: {files:?}"
        );
        let test_file = model
            .files
            .iter()
            .find(|file| file.path == "tests/test_store.py")
            .expect("the test file is discovered");
        assert!(test_file.test);
    }

    #[test]
    fn local_types_come_from_annotations_calls_and_factories() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, PROJECT, PYPROJECT);
        write(
            root,
            "src/app/__init__.py",
            "from .queue import Queue, make_queue\n",
        );
        write(
            root,
            "src/app/queue.py",
            "class Queue:\n\
             \x20   def __init__(self):\n\
             \x20       self.items: list[int] = []\n\
             \n\
             \x20   def push(self, item: int) -> None:\n\
             \x20       self.items.append(item)\n\
             \n\
             def make_queue() -> Queue:\n\
             \x20   return Queue()\n",
        );
        write(
            root,
            "src/app/use.py",
            "from .queue import Queue, make_queue\n\
             \n\
             def from_annotation(queue: Queue) -> None:\n\
             \x20   queue.push(1)\n\
             \n\
             def from_constructor() -> None:\n\
             \x20   queue = Queue()\n\
             \x20   queue.push(2)\n\
             \n\
             def from_factory() -> None:\n\
             \x20   queue = make_queue()\n\
             \x20   queue.push(3)\n",
        );
        let (model, _) = analyze(root);

        let push = member(&model, "Queue", "push");
        let mut holders: Vec<&str> = model
            .references
            .iter()
            .filter(|reference| reference.item == push)
            .filter_map(|reference| reference.holder)
            .map(|holder| model.items[holder.index()].name.as_str())
            .collect();
        holders.sort();
        holders.dedup();
        assert_eq!(
            holders,
            ["from_annotation", "from_constructor", "from_factory"],
            "every receiver type reaches Queue.push"
        );
        assert!(
            model
                .references
                .iter()
                .any(|reference| reference.item == push && !reference.copies),
            "a method call is not a copy"
        );
    }
}
