//! The Tree-sitter backend: a Cargo or TypeScript workspace is read with
//! `toml`, `serde_json`, and `tree-sitter` alone, without `cargo metadata`,
//! `rust-analyzer`, or a TypeScript compiler, and resolved into the same
//! [`Model`] the SCIP backend builds.
//!
//! The pipeline is the SCIP one in miniature: discovery walks the manifests
//! and `mod` declarations into a module tree, extraction reads every file
//! into a [`summary::FileSummary`] in parallel, and resolution turns the
//! summaries into items, references and wrappers.

mod lang;
mod query;
mod resolve;
mod summary;

use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::Error;
use crate::configuration::Configuration;
use crate::model::Model;
use crate::treesitter::lang::Language;
use crate::treesitter::lang::rust::RustFrontend;
use crate::treesitter::lang::typescript::TypeScriptFrontend;
use crate::treesitter::lang::{LanguageFrontend, WorkspaceLayout};

/// Build the model of the workspace whose root is above `start`, out of the
/// languages in `languages`: every language it has, if that is empty.
///
/// The frontends read the manifests and sources themselves: nothing here
/// runs a subprocess, so the backend works without `rust-analyzer`, `node`,
/// or a toolchain on `PATH`.
pub fn build(
    start: &Path,
    configuration: &Configuration,
    languages: &[Language],
) -> Result<Model, Error> {
    let root = workspace_root(start);
    let rust = RustFrontend;
    let typescript = TypeScriptFrontend::new();
    let all: [&dyn LanguageFrontend; 2] = [&rust, &typescript];
    let frontends: Vec<&dyn LanguageFrontend> = all
        .into_iter()
        .filter(|frontend| {
            (languages.is_empty() || languages.contains(&frontend.language()))
                && frontend.detect(&root)
        })
        .collect();
    if frontends.is_empty() {
        let asked = if languages.is_empty() {
            "Rust or TypeScript".to_owned()
        } else {
            languages
                .iter()
                .map(|language| language.name())
                .collect::<Vec<_>>()
                .join(" or ")
        };
        return Err(Error::new(format!(
            "no {asked} workspace found in `{}`",
            root.display()
        )));
    }
    let mut layout = WorkspaceLayout::new(root.clone());
    for frontend in &frontends {
        frontend.discover(&root, configuration, &mut layout)?;
    }
    // Stage 2: every file is parsed on its own, so the files run in parallel.
    let summaries = layout
        .files
        .par_iter()
        .map(|file| {
            let source = std::fs::read_to_string(&file.abs_path).unwrap_or_default();
            frontend_of(&frontends, file.language).extract_file(file, &source, configuration)
        })
        .collect();
    Ok(resolve::build_model(layout, summaries, &frontends))
}

/// The frontend that reads `language`, which every discovered file is in.
fn frontend_of<'a>(
    frontends: &[&'a dyn LanguageFrontend],
    language: Language,
) -> &'a dyn LanguageFrontend {
    *frontends
        .iter()
        .find(|frontend| frontend.language() == language)
        .expect("every discovered file is in a language a frontend reads")
}

/// The workspace root above `start`: the nearest directory that holds a
/// manifest of a language this backend reads.
///
/// A Cargo package is read from the workspace that holds it, the nearest
/// `Cargo.toml` above it that declares `[workspace]`; a TypeScript package,
/// from its own directory, since a `tsconfig` and its `paths` are per package.
fn workspace_root(start: &Path) -> PathBuf {
    for directory in start.ancestors() {
        if directory.join("Cargo.toml").is_file() {
            return directory
                .ancestors()
                .find(|above| declares_workspace(&above.join("Cargo.toml")))
                .map_or_else(|| directory.to_path_buf(), Path::to_path_buf);
        }
        if [
            "package.json",
            "tsconfig.json",
            "jsconfig.json",
            "pnpm-workspace.yaml",
        ]
        .iter()
        .any(|manifest| directory.join(manifest).is_file())
        {
            return directory.to_path_buf();
        }
    }
    start.to_path_buf()
}

/// Whether `manifest` declares a Cargo workspace.
fn declares_workspace(manifest: &Path) -> bool {
    std::fs::read_to_string(manifest).is_ok_and(|text| text.contains("[workspace]"))
}
