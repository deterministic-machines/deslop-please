//! Discovery and SCIP indexing for Go modules and workspaces.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

pub struct ModuleIndex {
    pub root: PathBuf,
    pub bytes: Vec<u8>,
}

fn failure(program: &str, root: &Path, output: &std::process::Output) -> Error {
    let detail = String::from_utf8_lossy(&output.stderr);
    Error::new(format!(
        "{program} failed in {} ({}): {}",
        root.display(),
        output.status,
        detail.trim()
    ))
}

/// Index each main module separately: scip-go expects its root to contain go.mod.
pub fn index(root: &Path) -> Result<Vec<ModuleIndex>, Error> {
    let root = root
        .canonicalize()
        .map_err(|e| Error::new(format!("{}: {e}", root.display())))?;
    let workspace = root.join("go.work").is_file();
    if !workspace && !root.join("go.mod").is_file() {
        return Err(Error::new(format!(
            "{} has neither go.work nor go.mod",
            root.display()
        )));
    }
    let mut go = Command::new("go");
    go.current_dir(&root).args(["list", "-m", "-json"]);
    if workspace {
        go.env("GOWORK", root.join("go.work"));
    } else {
        go.env("GOWORK", "off");
    }
    let output = go
        .output()
        .map_err(|e| Error::new(format!("cannot run go list: {e}")))?;
    if !output.status.success() {
        return Err(failure("go list -m", &root, &output));
    }
    let mut modules = Vec::new();
    for value in
        serde_json::Deserializer::from_slice(&output.stdout).into_iter::<serde_json::Value>()
    {
        let value = value.map_err(|e| Error::new(format!("invalid go list -m output: {e}")))?;
        if value.get("Main").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let dir = value
            .get("Dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::new("go list -m omitted the main module directory"))?;
        let dir = PathBuf::from(dir)
            .canonicalize()
            .map_err(|e| Error::new(format!("{dir}: {e}")))?;
        if !dir.starts_with(&root) {
            return Err(Error::new(format!(
                "Go workspace module {} is outside workspace root {}; this layout cannot be represented",
                dir.display(),
                root.display()
            )));
        }
        modules.push(dir);
    }
    modules.sort();
    modules.dedup();
    if modules.is_empty() {
        return Err(Error::new(format!(
            "no main Go modules found in {}",
            root.display()
        )));
    }
    let mut indexes = Vec::with_capacity(modules.len());
    for module in modules {
        let mut command = Command::new("scip-go");
        command
            .current_dir(&module)
            .arg("index")
            .arg("--module-root")
            .arg(&module)
            .args(["--module-version", ".", "--output", "-", "--quiet"]);
        if workspace {
            command.env("GOWORK", root.join("go.work"));
        } else {
            command.env("GOWORK", "off");
        }
        let output = command.output().map_err(|e| {
            Error::new(format!(
                "cannot run scip-go (install it and add it to PATH): {e}"
            ))
        })?;
        if !output.status.success() {
            return Err(failure("scip-go", &module, &output));
        }
        if output.stdout.is_empty() {
            return Err(Error::new(format!(
                "scip-go produced an empty index for {}",
                module.display()
            )));
        }
        indexes.push(ModuleIndex {
            root: module,
            bytes: output.stdout,
        });
    }
    Ok(indexes)
}
