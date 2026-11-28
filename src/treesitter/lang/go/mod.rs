//! Go frontend: `go.work`/`go.mod` module discovery, package-based import
//! resolution, and Tree-sitter extraction into the language-agnostic IR.
//!
//! A Go package is a directory: every `.go` file directly in it shares one
//! `Module` node, and each subdirectory is a nested module under it, so
//! methods declared in one file of a package belong to the type declared in
//! another.

mod discovery;
mod extract;
mod resolve;
mod syntax;

use std::path::Path;

use crate::Error;
use crate::configuration::Configuration;
use crate::model::NodeId;
use crate::treesitter::lang::{
    DiscoveredFile, Language, LanguageFrontend, ResolvedImport, WorkspaceLayout,
};
use crate::treesitter::summary::{FileSummary, RawImport};

/// Go frontend.
pub struct GoFrontend;

impl LanguageFrontend for GoFrontend {
    fn language(&self) -> Language {
        Language::Go
    }

    fn detect(&self, root: &Path) -> bool {
        root.join("go.mod").is_file() || root.join("go.work").is_file()
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
        _from_file: &DiscoveredFile,
        _from_module: NodeId,
        import: &RawImport,
    ) -> Option<ResolvedImport> {
        resolve::resolve_import(layout, import)
    }
}

/// Source file names this frontend parses.
fn is_source_file(name: &str) -> bool {
    name.len() > 3 && name.ends_with(".go")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::GoFrontend;
    use crate::configuration::Configuration;
    use crate::lint::{self, Lint};
    use crate::model::{ItemId, ItemKind, Model, NodeKind, Wrapper};
    use crate::treesitter::lang::{LanguageFrontend, WorkspaceLayout};
    use crate::treesitter::resolve::build_model;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
    }

    fn build(root: &Path) -> Model {
        let frontend = GoFrontend;
        let mut layout = WorkspaceLayout::new(root.to_path_buf());
        frontend
            .discover(root, &Configuration::default(), &mut layout)
            .expect("discovery succeeds");
        let summaries: Vec<_> = layout
            .files
            .iter()
            .map(|file| {
                let source = fs::read_to_string(&file.abs_path).expect("read source");
                frontend.extract_file(file, &source, &Configuration::default())
            })
            .collect();
        build_model(layout, summaries, &[&frontend])
    }

    fn item(model: &Model, name: &str, kind: ItemKind) -> ItemId {
        model
            .items
            .iter()
            .position(|item| item.name == name && item.kind == kind)
            .map(ItemId::new)
            .unwrap_or_else(|| panic!("no {kind:?} item `{name}`"))
    }

    fn member(model: &Model, parent: ItemId, name: &str) -> ItemId {
        model
            .items
            .iter()
            .position(|item| item.parent == Some(parent) && item.name.ends_with(name))
            .map(ItemId::new)
            .unwrap_or_else(|| panic!("no member `{name}` of the item at {}", parent.index()))
    }

    /// A package `queue` split over two files, a `queue_test.go`, and a
    /// package `app` that wraps it.
    fn shop() -> TempDir {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "go.mod", "module example.com/shop\n\ngo 1.22\n");
        write(
            root,
            "queue/queue.go",
            r#"
package queue

type Item struct {
	ID int
}

type Queue struct {
	Items []Item
	Head  int
	Limit int
}

func NewQueue(limit int) *Queue {
	return &Queue{Limit: limit}
}
"#,
        );
        write(
            root,
            "queue/methods.go",
            r#"
package queue

func (q *Queue) Full() bool {
	return q.Head >= q.Limit
}

func (q *Queue) Push(item Item) {
	q.Items = append(q.Items, item)
}

func (q *Queue) Drain() []Item {
	return q.Items
}
"#,
        );
        write(
            root,
            "queue/queue_test.go",
            r#"
package queue

import "testing"

func TestPush(t *testing.T) {
	q := NewQueue(1)
	q.Push(Item{ID: 1})
}
"#,
        );
        write(
            root,
            "app/app.go",
            r#"
package app

import "example.com/shop/queue"

type Scheduler struct {
	queue *queue.Queue
}

func NewScheduler() *Scheduler {
	return &Scheduler{queue: queue.NewQueue(7)}
}

func (s *Scheduler) Push(item queue.Item) {
	s.queue.Push(item)
}

func (s *Scheduler) Drain() []queue.Item {
	return s.queue.Drain()
}

func (s *Scheduler) Reset() {
	s.queue.Items = nil
}

func Run(s *Scheduler) {
	s.Push(queue.Item{ID: 1})
	s.Reset()
}

func Build() {
	q := queue.NewQueue(1)
	q.Push(queue.Item{ID: 2})
	q2, err := queue.NewQueue(2)
	if err != nil {
		return
	}
	q2.Push(queue.Item{ID: 3})
}
"#,
        );
        temp
    }

    #[test]
    fn a_go_workspace_resolves_into_a_model() {
        let temp = shop();
        let model = build(temp.path());

        // One module per directory, shared by the package's files.
        let module_of = |path: &str| {
            model
                .files
                .iter()
                .find(|file| file.path == path)
                .unwrap_or_else(|| panic!("no file `{path}`"))
                .module
        };
        assert_eq!(module_of("queue/queue.go"), module_of("queue/methods.go"));
        assert_eq!(
            model.tree.display(module_of("queue/queue.go")),
            "example.com/shop/queue"
        );
        assert_eq!(
            model.tree.display(module_of("app/app.go")),
            "example.com/shop/app"
        );
        assert_eq!(
            model.tree.node(module_of("queue/queue.go")).kind,
            NodeKind::Module
        );
        assert_eq!(
            model.items[item(&model, "Queue", ItemKind::Type).index()].kind,
            ItemKind::Type
        );

        // Methods declared in another file of the package belong to the type.
        let queue = item(&model, "Queue", ItemKind::Type);
        let queue_push = item(&model, "Queue::Push", ItemKind::Function);
        assert_eq!(model.items[queue_push.index()].parent, Some(queue));
        assert!(model.items[queue_push.index()].takes_self);
        assert!(model.items[item(&model, "Queue::Full", ItemKind::Function).index()].takes_self);
        let drain = item(&model, "Queue::Drain", ItemKind::Function);
        let items = member(&model, queue, "Items");

        // The getter hands out the slice itself, so it lends the field.
        match model.wrappers.get(&drain) {
            Some(Wrapper::Accessor { lent, copied, .. }) => {
                assert_eq!(lent, &[items]);
                assert!(copied.is_empty());
            }
            other => panic!("`Queue::Drain` is no accessor: {other:?}"),
        }

        // A method that only calls another one on its own field forwards to it.
        let scheduler = item(&model, "Scheduler", ItemKind::Type);
        let scheduler_queue = member(&model, scheduler, "queue");
        for (name, target) in [("Scheduler::Push", queue_push), ("Scheduler::Drain", drain)] {
            match model.wrappers.get(&item(&model, name, ItemKind::Function)) {
                Some(Wrapper::Forwarder { target: at, fields }) => {
                    assert_eq!(*at, target, "`{name}` forwards");
                    assert_eq!(fields, &[scheduler_queue]);
                }
                other => panic!("`{name}` is no forwarder: {other:?}"),
            }
        }

        // `q := queue.NewQueue(1); q.Push(...)` uses the local type env, and
        // `q2, err := ...` keeps `q2` because the error result is dropped.
        let build = item(&model, "Build", ItemKind::Function);
        let pushes: Vec<&crate::model::Reference> = model
            .references
            .iter()
            .filter(|reference| {
                reference.holder == Some(build) && reference.item == queue_push && !reference.copies
            })
            .collect();
        assert_eq!(pushes.len(), 2, "both locals call `Queue::Push`");

        // A write to a field reaches into it.
        let write = model
            .references
            .iter()
            .find(|reference| {
                reference.item == items
                    && reference.holder
                        == Some(item(&model, "Scheduler::Reset", ItemKind::Function))
            })
            .expect("`Scheduler::Reset` writes `Queue::Items`");
        assert!(!write.copies);

        // Composite literals name their type, wherever they are written.
        let composite = model.references.iter().any(|reference| {
            reference.item == item(&model, "Item", ItemKind::Type)
                && reference.holder == Some(build)
        });
        assert!(composite, "a composite literal names its type");

        // `_test.go` files are test code, and so are their references.
        let test = model
            .files
            .iter()
            .find(|file| file.path == "queue/queue_test.go")
            .expect("the test file is discovered");
        assert!(test.test);
        assert!(
            model.references.iter().any(|reference| {
                model.files[reference.file.index()].path == "queue/queue_test.go"
                    && reference.item == queue_push
                    && reference.test
            }),
            "the test's `Push` call is test code"
        );
        assert!(!model.tree.node(module_of("queue/queue.go")).test);
    }

    #[test]
    fn a_go_work_workspace_links_its_modules() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "go.work", "go 1.22\n\nuse (\n\t./core\n\t./api\n)\n");
        write(root, "core/go.mod", "module example.com/core\n");
        write(
            root,
            "core/store/store.go",
            r#"
package store

type Entry struct {
	Key string
}
"#,
        );
        write(root, "api/go.mod", "module example.com/api\n");
        write(
            root,
            "api/save.go",
            r#"
package api

import "example.com/core/store"

func Save(entry store.Entry) {
	_ = entry.Key
}
"#,
        );

        let model = build(root);
        let packages: Vec<&str> = model
            .tree
            .ids()
            .filter(|&id| model.tree.node(id).kind == NodeKind::Package)
            .map(|id| model.tree.node(id).name.as_str())
            .collect();
        assert_eq!(packages, ["example.com/core", "example.com/api"]);
        let entry = item(&model, "Entry", ItemKind::Type);
        let key = member(&model, entry, "Key");
        let save = item(&model, "Save", ItemKind::Function);
        let read = model
            .references
            .iter()
            .find(|reference| reference.item == key && reference.holder == Some(save))
            .expect("`entry.Key` resolves to the field of the other module");
        assert!(read.copies, "reading a string copies it");
        let entry_ref = model
            .references
            .iter()
            .find(|reference| reference.item == entry && reference.holder == Some(save))
            .expect("`store.Entry` resolves through the import");
        assert!(!entry_ref.copies, "a type is no field to copy");
    }

    #[test]
    fn a_directory_import_reaches_the_module_of_that_directory() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "go.mod", "module example.com/shop\n");
        write(
            root,
            "queue/internal/ring/ring.go",
            r#"
package ring

type Buffer struct {
	values []int
}
"#,
        );
        write(
            root,
            "queue/queue.go",
            r#"
package queue

import "example.com/shop/queue/internal/ring"

type Queue struct {
	buffer *ring.Buffer
}
"#,
        );
        let model = build(root);
        let buffer = item(&model, "Buffer", ItemKind::Type);
        assert_eq!(
            model.tree.display(model.items[buffer.index()].owner),
            "example.com/shop/queue/internal/ring"
        );
        let queue = item(&model, "Queue", ItemKind::Type);
        let field = member(&model, queue, "buffer");
        assert!(
            model
                .references
                .iter()
                .any(|reference| reference.item == buffer && reference.holder == Some(field)),
            "`*ring.Buffer` resolves through the directory import"
        );
    }
    #[test]
    fn go_lints_report_a_leak_and_leave_a_small_tangle_to_its_module() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(
            root,
            "go.work",
            "go 1.22\n\nuse (\n\t./shop\n\t./ring\n\t./consumer\n)\n",
        );
        write(root, "shop/go.mod", "module example.com/shop\n");
        write(
            root,
            "shop/queue/queue.go",
            r#"
package queue

type Item struct {
	ID int
}

type Queue struct {
	Items []Item
	Head  int
	Limit int
}

func NewQueue(limit int) *Queue {
	return &Queue{Limit: limit}
}

func (q *Queue) Full() bool {
	return q.Head >= q.Limit
}

func (q *Queue) Push(item Item) {
	q.Items = append(q.Items, item)
}

func (q *Queue) Drain() []Item {
	return q.Items
}

func Reset(q *Queue) {
	q.Items = nil
}

func Handout(q *Queue) {
	_ = q.Drain()
}
"#,
        );
        write(
            root,
            "shop/link/link.go",
            r#"
package link

type Alpha struct {
	beta *Beta
}

type Beta struct {
	gamma *Gamma
}

type Gamma struct {
	alpha *Alpha
}
"#,
        );
        write(root, "ring/go.mod", "module example.com/ring\n");
        write(
            root,
            "ring/ring.go",
            r#"
package ring

type Ring struct {
	values []int
}

func (r *Ring) Push(value int) {
	r.values = append(r.values, value)
}

func (r *Ring) Pop() int {
	last := r.values[len(r.values)-1]
	r.values = r.values[:len(r.values)-1]
	return last
}

func (r *Ring) Len() int {
	return len(r.values)
}
"#,
        );
        write(root, "consumer/go.mod", "module example.com/consumer\n");
        write(
            root,
            "consumer/main.go",
            r#"
package main

import "example.com/ring"

func Run(r *ring.Ring) {
	r.Push(1)
	_ = r.Pop()
	_ = r.Len()
}
"#,
        );
        let model = build(root);
        let diagnostics = lint::check(&model, None);
        let keys = |lint: Lint| -> Vec<String> {
            let mut keys: Vec<String> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.lint == lint)
                .map(|diagnostic| diagnostic.key.clone())
                .collect();
            keys.sort();
            keys
        };

        // A field another type writes or is lent by a getter is known outside.
        assert_eq!(
            keys(Lint::FieldLeak),
            ["example.com/shop/queue/Queue::Items"]
        );
        let leak = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.lint == Lint::FieldLeak)
            .expect("a field leak");
        assert!(
            leak.notes.iter().any(|note| note.contains("Reset")),
            "a write by another function leaks: {:?}",
            leak.notes
        );
        assert!(
            leak.notes
                .iter()
                .any(|note| note.contains("through `Queue::Drain`")),
            "the getter hands the field out: {:?}",
            leak.notes
        );

        // Alpha, Beta and Gamma know each other in a circle, but all three are
        // written in one module: a reader learns them together, so no tangle.
        assert!(
            keys(Lint::Tangle).is_empty(),
            "a small triangle of one module is held in mind: {:?}",
            keys(Lint::Tangle)
        );

        assert!(
            keys(Lint::ModuleCycle).is_empty(),
            "the imports form no cycle"
        );
    }
}
