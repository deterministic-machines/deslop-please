//! Discovery and SCIP indexing for Go modules and workspaces.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

pub struct ModuleIndex {
    /// The module's directory, canonical.
    pub root: PathBuf,
    /// Its module path, the prefix of its packages' import paths.
    pub path: String,
    pub bytes: Vec<u8>,
}

fn failure(program: &str, root: &Path, output: &std::process::Output) -> Error {
    let detail = String::from_utf8_lossy(&output.stderr);
    Error::new(format!(
        "{program} failed in {}: {}",
        root.display(),
        detail.trim()
    ))
}

/// The value of `GOWORK` for the go commands: the workspace file, or `off`.
pub fn gowork(work: Option<&Path>) -> std::ffi::OsString {
    work.map_or_else(|| "off".into(), |work| work.as_os_str().to_owned())
}

/// Index each main module separately: scip-go expects its root to contain
/// go.mod. A workspace's modules may be anywhere, inside its directory or
/// not.
pub fn index(root: &Path, work: Option<&Path>) -> Result<Vec<ModuleIndex>, Error> {
    let mut go = Command::new("go");
    go.current_dir(root)
        .args(["list", "-m", "-json"])
        .env("GOWORK", gowork(work));
    let output = go
        .output()
        .map_err(|e| Error::new(format!("cannot run go list: {e}")))?;
    if !output.status.success() {
        return Err(failure("go list -m", root, &output));
    }
    let mut modules = Vec::new();
    for value in
        serde_json::Deserializer::from_slice(&output.stdout).into_iter::<serde_json::Value>()
    {
        let value = value.map_err(|e| Error::new(format!("invalid go list -m output: {e}")))?;
        if value.get("Main").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let text = |key: &str| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::new(format!("go list -m omitted a main module's {key}")))
        };
        let dir = text("Dir")?;
        let dir = PathBuf::from(dir)
            .canonicalize()
            .map_err(|e| Error::new(format!("{dir}: {e}")))?;
        modules.push((dir, text("Path")?.to_owned()));
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
    for (module, path) in modules {
        let mut command = Command::new("scip-go");
        command
            .current_dir(&module)
            .arg("index")
            .arg("--module-root")
            .arg(&module)
            .args(["--module-version", ".", "--output", "-", "--quiet"])
            .env("GOWORK", gowork(work));
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
            path,
            bytes: output.stdout,
        });
    }
    Ok(indexes)
}
