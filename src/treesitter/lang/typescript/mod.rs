//! TypeScript/JavaScript frontend: workspace discovery, `oxc_resolver` import
//! resolution, and Tree-sitter extraction into the language-agnostic IR.

mod discovery;
mod extract;
mod resolve;
mod syntax;

use std::path::Path;

use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};

use crate::Error;
use crate::configuration::Configuration;
use crate::model::NodeId;
use crate::treesitter::lang::{
    DiscoveredFile, Language, LanguageFrontend, ResolvedImport, WorkspaceLayout,
};
use crate::treesitter::summary::{FileSummary, RawImport};

/// TypeScript and JavaScript frontend.
pub struct TypeScriptFrontend {
    resolver: Resolver,
}

impl TypeScriptFrontend {
    pub fn new() -> Self {
        Self {
            resolver: Resolver::new(resolve_options()),
        }
    }
}

impl Default for TypeScriptFrontend {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageFrontend for TypeScriptFrontend {
    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn detect(&self, root: &Path) -> bool {
        ["package.json", "tsconfig.json", "jsconfig.json"]
            .iter()
            .any(|name| root.join(name).is_file())
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
        _from_module: NodeId,
        import: &RawImport,
    ) -> Option<ResolvedImport> {
        resolve::resolve_import(&self.resolver, layout, from_file, import)
    }
}

/// Source extensions the frontend walks and parses.
const SOURCE_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];

/// True for a file name this frontend parses, excluding declaration files.
fn is_source_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".d.ts") || lower.ends_with(".d.mts") || lower.ends_with(".d.cts") {
        return false;
    }
    SOURCE_EXTENSIONS
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// True when the file is TSX, and so must be parsed with the TSX grammar.
fn is_tsx_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tsx") || lower.ends_with(".jsx")
}

fn resolve_options() -> ResolveOptions {
    ResolveOptions {
        tsconfig: Some(TsconfigDiscovery::Auto),
        extensions: [
            ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        extension_alias: vec![
            (
                ".js".to_owned(),
                vec![".ts".to_owned(), ".tsx".to_owned(), ".js".to_owned()],
            ),
            (
                ".mjs".to_owned(),
                vec![".mts".to_owned(), ".mjs".to_owned()],
            ),
            (
                ".cjs".to_owned(),
                vec![".cts".to_owned(), ".cjs".to_owned()],
            ),
        ],
        condition_names: ["types", "import", "require", "node", "default"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        main_fields: ["types", "module", "main"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ..ResolveOptions::default()
    }
}
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::TypeScriptFrontend;
    use crate::configuration::Configuration;
    use crate::model::{ItemKind, Wrapper};
    use crate::treesitter::lang::{LanguageFrontend, WorkspaceLayout};
    use crate::treesitter::resolve::build_model;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
    }

    #[test]
    fn a_typescript_workspace_resolves_into_a_model() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path();
        write(root, "package.json", r#"{"name":"app"}"#);
        write(
            root,
            "src/ring.ts",
            r#"
export interface Item {
  id: number;
}

export class Ring {
  private count: number = 0;
  push(item: Item): void {
    this.count = this.count + item.id;
  }
}
"#,
        );
        write(
            root,
            "src/queue.ts",
            r#"
import { Item, Ring } from './ring';

export class Queue {
  private ring: Ring = new Ring();
  private head: number = 0;

  get headValue(): number {
    return this.head;
  }

  push(item: Item): void {
    this.ring.push(item);
  }
}
"#,
        );
        write(
            root,
            "src/app.ts",
            r#"
import { Queue } from './queue';
import { Item } from './ring';

export function run(queue: Queue, item: Item): void {
  queue.push(item);
}
"#,
        );

        let frontend = TypeScriptFrontend::new();
        let mut layout = WorkspaceLayout::new(root.to_path_buf());
        frontend
            .discover(root, &Configuration::default(), &mut layout)
            .expect("discovery succeeds");
        assert_eq!(layout.files.len(), 3);
        let summaries: Vec<_> = layout
            .files
            .iter()
            .map(|file| {
                let source = fs::read_to_string(&file.abs_path).expect("read source");
                frontend.extract_file(file, &source, &Configuration::default())
            })
            .collect();
        let model = build_model(layout, summaries, &[&frontend]);

        let item = |name: &str, kind: ItemKind| {
            model
                .items
                .iter()
                .position(|item| item.name == name && item.kind == kind)
                .unwrap_or_else(|| panic!("no `{name}` item"))
        };
        let queue_id = crate::model::ItemId::new(item("Queue", ItemKind::Type));
        let ring_id = crate::model::ItemId::new(item("Ring", ItemKind::Type));
        let push_id = crate::model::ItemId::new(item("push", ItemKind::Function));
        let ring_push = model
            .items
            .iter()
            .enumerate()
            .find(|(_, item)| {
                item.name == "push"
                    && item.kind == ItemKind::Function
                    && item.parent == Some(ring_id)
            })
            .map(|(index, _)| crate::model::ItemId::new(index))
            .expect("Ring::push");
        assert_eq!(model.items[push_id.index()].parent, Some(queue_id));

        // `this.ring.push(item)` forwards to `Ring::push` on the `ring` field.
        match model.wrappers.get(&push_id) {
            Some(Wrapper::Forwarder { target, fields }) => {
                assert_eq!(*target, ring_push);
                assert_eq!(fields.len(), 1);
                let ring_field = model
                    .items
                    .iter()
                    .position(|item| item.name == "Queue::ring" && item.parent == Some(queue_id))
                    .expect("Queue.ring");
                assert_eq!(fields[0], crate::model::ItemId::new(ring_field));
            }
            other => panic!("Queue::push is no forwarder: {other:?}"),
        }

        // The getter hands out a primitive field, so it is an accessor that copies.
        let getter = model
            .items
            .iter()
            .position(|item| item.name == "headValue" && item.parent == Some(queue_id))
            .expect("Queue.headValue");
        let head_field = model
            .items
            .iter()
            .position(|item| item.name == "Queue::head" && item.parent == Some(queue_id))
            .expect("Queue.head");
        match model.wrappers.get(&crate::model::ItemId::new(getter)) {
            Some(Wrapper::Accessor {
                copied,
                lent,
                to_read,
            }) => {
                assert_eq!(copied, &[crate::model::ItemId::new(head_field)]);
                assert!(lent.is_empty());
                assert!(to_read);
            }
            other => panic!("Queue.headValue is no accessor: {other:?}"),
        }

        // `queue.push(item)` in app.ts reaches the class through its import.
        assert!(
            model
                .references
                .iter()
                .any(|reference| reference.item == push_id && !reference.copies),
            "the call resolves to Queue::push"
        );
    }
}
