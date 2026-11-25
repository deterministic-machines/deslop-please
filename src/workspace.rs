//! The Cargo workspace around the current directory, as `cargo metadata` describes it.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::Error;

pub struct Workspace {
    pub root: PathBuf,
    pub target_directory: PathBuf,
    pub packages: Vec<Package>,
}

pub struct Package {
    pub name: String,
    pub version: String,
    /// How code names the package's library: `my_crate` for `my-crate`.
    pub crate_name: String,
    /// Relative to the workspace's root, `/`-separated; empty for the root itself.
    pub directory: String,
    pub targets: Vec<Target>,
}

pub struct Target {
    pub kind: TargetKind,
    /// The crate's root file, relative to the workspace's root.
    pub root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKind {
    Library,
    Binary,
    Example,
    Test,
    Bench,
    BuildScript,
}

impl TargetKind {
    pub fn is_test(self) -> bool {
        matches!(self, TargetKind::Test | TargetKind::Bench)
    }
}

impl Workspace {
    pub fn discover() -> Result<Workspace, Error> {
        let cwd = std::env::current_dir()
            .map_err(|e| Error::new(format!("cannot read current directory: {e}")))?;
        Workspace::discover_in(&cwd)
    }

    pub fn discover_in(dir: &Path) -> Result<Workspace, Error> {
        let output = Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(dir)
            .output()
            .map_err(|e| Error::new(format!("cannot run cargo: {e}")))?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr);
            return Err(Error::new(format!(
                "not inside a Cargo workspace: cargo metadata says: {}",
                message.trim()
            )));
        }
        let metadata: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::new(format!("cargo metadata's output is not JSON: {e}")))?;
        Workspace::from_metadata(&metadata)
    }

    fn from_metadata(metadata: &Value) -> Result<Workspace, Error> {
        let root = PathBuf::from(text(metadata, "workspace_root")?);
        let target_directory = PathBuf::from(text(metadata, "target_directory")?);
        let packages = list(metadata, "packages")?
            .iter()
            .map(|package| Package::from_metadata(package, &root))
            .collect::<Result<_, _>>()?;
        Ok(Workspace {
            root,
            target_directory,
            packages,
        })
    }

    /// The files whose change can change the index: every package's Rust
    /// sources and manifest, and the lock file.
    pub fn source_files(&self) -> Result<Vec<String>, Error> {
        let mut files = vec!["Cargo.toml".to_owned(), "Cargo.lock".to_owned()];
        for package in &self.packages {
            let directory = self.root.join(&package.directory);
            collect_sources(&directory, &self.root, &self.target_directory, &mut files)?;
        }
        files.sort();
        files.dedup();
        Ok(files)
    }
}

impl Package {
    fn from_metadata(package: &Value, root: &Path) -> Result<Package, Error> {
        let name = text(package, "name")?.to_owned();
        let manifest = relative(Path::new(text(package, "manifest_path")?), root)?;
        let directory = manifest
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory)
            .to_owned();
        let mut targets = Vec::new();
        let mut library_name = None;
        for target in list(package, "targets")? {
            let kind = target_kind(target)?;
            if kind == TargetKind::Library {
                library_name = Some(text(target, "name")?.to_owned());
            }
            let root = relative(Path::new(text(target, "src_path")?), root)?;
            targets.push(Target { kind, root });
        }
        Ok(Package {
            crate_name: library_name.unwrap_or_else(|| name.replace('-', "_")),
            version: text(package, "version")?.to_owned(),
            name,
            directory,
            targets,
        })
    }
}

fn target_kind(target: &Value) -> Result<TargetKind, Error> {
    let kinds = list(target, "kind")?;
    let first = kinds.first().and_then(Value::as_str).unwrap_or_default();
    Ok(match first {
        "bin" => TargetKind::Binary,
        "example" => TargetKind::Example,
        "test" => TargetKind::Test,
        "bench" => TargetKind::Bench,
        "custom-build" => TargetKind::BuildScript,
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => TargetKind::Library,
        other => {
            return Err(Error::new(format!(
                "cargo metadata has a target of an unknown kind: {other:?}"
            )));
        }
    })
}

fn collect_sources(
    directory: &Path,
    root: &Path,
    target: &Path,
    files: &mut Vec<String>,
) -> Result<(), Error> {
    let unreadable = |e| Error::new(format!("cannot read {}: {e}", directory.display()));
    for entry in std::fs::read_dir(directory).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        let hidden = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        let build_target = path.file_name().is_some_and(|name| name == "target");
        if hidden || path == target || build_target {
            continue;
        }
        // Not through symbolic links, which can loop.
        let file_type = entry.file_type().map_err(unreadable)?;
        if file_type.is_dir() {
            collect_sources(&path, root, target, files)?;
        } else if file_type.is_file()
            && (path.extension().is_some_and(|extension| extension == "rs")
                || path.file_name().is_some_and(|name| name == "Cargo.toml"))
        {
            files.push(relative(&path, root)?);
        }
    }
    Ok(())
}

fn relative(path: &Path, root: &Path) -> Result<String, Error> {
    let inside = path.strip_prefix(root).map_err(|_| {
        Error::new(format!(
            "{} is outside the workspace's root {}",
            path.display(),
            root.display()
        ))
    })?;
    let parts: Vec<_> = inside
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect();
    Ok(parts.join("/"))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value[key].as_str().ok_or_else(|| missing(key))
}

fn list<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, Error> {
    value[key].as_array().ok_or_else(|| missing(key))
}

fn missing(key: &str) -> Error {
    Error::new(format!("cargo metadata's output has no {key:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_files_skip_target_directories() {
        let root =
            std::env::temp_dir().join(format!("deslop-please-target-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = root.join("pkg");
        let target = package.join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::write(
            package.join("Cargo.toml"),
            "[package]\nname='p'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(package.join("lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(target.join("generated.rs"), "pub fn g() {}\n").unwrap();
        let workspace = Workspace {
            root: root.clone(),
            target_directory: root.join("target"),
            packages: vec![Package {
                name: "p".into(),
                version: "0.1.0".into(),
                crate_name: "p".into(),
                directory: "pkg".into(),
                targets: vec![],
            }],
        };
        let files = workspace.source_files().unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(files.iter().any(|f| f == "pkg/lib.rs"), "{files:?}");
        assert!(files.iter().any(|f| f == "pkg/Cargo.toml"), "{files:?}");
        assert!(
            !files.iter().any(|f| f.contains("generated.rs")),
            "{files:?}"
        );
    }

    #[test]
    fn reads_packages_their_directories_and_targets() {
        let metadata = serde_json::json!({
            "workspace_root": "/w",
            "target_directory": "/w/target",
            "packages": [{
                "name": "my-tool",
                "version": "0.2.0",
                "manifest_path": "/w/tools/my-tool/Cargo.toml",
                "targets": [
                    {"kind": ["lib"], "name": "my_tool", "src_path": "/w/tools/my-tool/src/lib.rs"},
                    {"kind": ["test"], "name": "it", "src_path": "/w/tools/my-tool/tests/it.rs"}
                ]
            }, {
                "name": "root-bin",
                "version": "1.0.0",
                "manifest_path": "/w/Cargo.toml",
                "targets": [{"kind": ["bin"], "name": "root-bin", "src_path": "/w/src/main.rs"}]
            }]
        });
        let workspace = Workspace::from_metadata(&metadata).unwrap();
        let tool = &workspace.packages[0];
        assert_eq!(
            (tool.directory.as_str(), tool.crate_name.as_str()),
            ("tools/my-tool", "my_tool")
        );
        assert_eq!(tool.targets[1].kind, TargetKind::Test);
        assert_eq!(tool.targets[1].root, "tools/my-tool/tests/it.rs");
        let root_bin = &workspace.packages[1];
        assert_eq!(
            (root_bin.directory.as_str(), root_bin.crate_name.as_str()),
            ("", "root_bin")
        );
    }
}
