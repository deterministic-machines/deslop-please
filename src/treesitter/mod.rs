//! The Tree-sitter backend: a Cargo workspace is read with `toml` and
//! `tree-sitter` alone, without `cargo metadata` or `rust-analyzer`, and
//! resolved into the same [`Model`] the SCIP backend builds.
//!
//! The pipeline is the SCIP one in miniature: discovery walks the manifests
//! and `mod` declarations into a module tree, extraction reads every file
//! into a [`summary::FileSummary`] in parallel, and resolution turns the
//! summaries into items, references and wrappers.

mod lang;
mod language;
mod query;
mod resolve;
mod summary;

use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::Error;
use crate::configuration::Configuration;
use crate::model::Model;
use crate::treesitter::lang::rust::RustFrontend;
use crate::treesitter::lang::{LanguageFrontend, WorkspaceLayout};

/// Build the model of the workspace whose root is above `start`.
///
/// The Rust frontend reads the manifests and sources itself: nothing here
/// runs a subprocess, so the backend works without rust-analyzer or a
/// toolchain on `PATH`.
pub fn build(start: &Path, configuration: &Configuration) -> Result<Model, Error> {
    let root = workspace_root(start);
    let frontend = RustFrontend;
    if !frontend.detect(&root) {
        return Err(Error::new(format!(
            "no Cargo workspace found in `{}`",
            root.display()
        )));
    }
    let mut layout = WorkspaceLayout::new();
    frontend.discover(&root, configuration, &mut layout)?;
    // Stage 2: every file is parsed on its own, so the files run in parallel.
    let summaries = layout
        .files
        .par_iter()
        .map(|file| {
            let source = std::fs::read_to_string(&file.abs_path).unwrap_or_default();
            frontend.extract_file(file, &source, configuration)
        })
        .collect();
    Ok(resolve::build_model(layout, summaries, &[&frontend]))
}

/// The workspace root above `start`: the nearest `Cargo.toml` that declares
/// a `[workspace]`, or else the nearest one at all.
fn workspace_root(start: &Path) -> PathBuf {
    let mut nearest = None;
    for directory in start.ancestors() {
        let manifest = directory.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        if nearest.is_none() {
            nearest = Some(directory.to_path_buf());
        }
        if std::fs::read_to_string(&manifest).is_ok_and(|text| text.contains("[workspace]")) {
            return directory.to_path_buf();
        }
    }
    nearest.unwrap_or_else(|| start.to_path_buf())
}
