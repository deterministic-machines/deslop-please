//! A Java project, built with Maven or Gradle, and the index `scip-java`
//! writes of it. `scip-java index` builds the project with its own build
//! tool, with a compiler plugin that records what each name resolves to.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::indexing::{cached, hashes, io_error};
use crate::{Error, TOOL};

/// The files that make a directory part of a Maven or Gradle build.
const BUILD_FILES: [&str; 5] = [
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
];

/// Directories that hold what a build writes, not sources.
const OUTPUTS: [&str; 3] = ["target", "build", "out"];

pub struct Project {
    /// The outermost directory of the build, which holds its root `pom.xml`
    /// or `settings.gradle`, in an unbroken chain of build directories.
    root: PathBuf,
}

impl Project {
    /// The Java project around `dir`, if the closest directory up from it
    /// that holds a build file holds one of Maven's or Gradle's rather than
    /// a `Cargo.toml`.
    pub fn around(dir: &Path) -> Option<Project> {
        let is_cargo = |d: &Path| d.join("Cargo.toml").is_file();
        let nearest = dir.ancestors().find(|d| is_cargo(d) || is_build(d))?;
        if is_cargo(nearest) {
            return None;
        }
        let root = nearest.ancestors().take_while(|d| is_build(d)).last()?;
        Some(Project {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The index, built again when a source or build file has changed, or
    /// scip-java has.
    pub fn index(&self) -> Result<Vec<u8>, Error> {
        let version = Command::new("scip-java")
            .arg("--version")
            .output()
            .map_err(missing_scip_java)?;
        let mut stamp = String::from_utf8_lossy(&version.stdout).into_owned();
        stamp += &hashes(&self.root, &self.source_files()?)?;
        let output = if self.root.join("pom.xml").is_file() {
            "target"
        } else {
            "build"
        };
        let directory = self.root.join(output).join(TOOL);
        cached(&directory, &stamp, |index| {
            eprintln!("{TOOL}: indexing project with scip-java...");
            run_scip_java(&self.root, index)
        })
    }

    /// The Java sources and build files, relative to the root and sorted,
    /// past hidden directories and build outputs.
    fn source_files(&self) -> Result<Vec<String>, Error> {
        let mut files = Vec::new();
        collect_sources(&self.root, &self.root, &mut files)?;
        files.sort();
        Ok(files)
    }
}

fn is_build(directory: &Path) -> bool {
    BUILD_FILES
        .iter()
        .any(|file| directory.join(file).is_file())
}

fn collect_sources(directory: &Path, root: &Path, files: &mut Vec<String>) -> Result<(), Error> {
    let unreadable = |e| io_error(directory, e);
    for entry in std::fs::read_dir(directory).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // Not through symbolic links, which can loop.
        let file_type = entry.file_type().map_err(unreadable)?;
        if name.starts_with('.') {
            continue;
        }
        if file_type.is_dir() {
            if !OUTPUTS.contains(&name.as_str()) {
                collect_sources(&path, root, files)?;
            }
        } else if file_type.is_file()
            && (name.ends_with(".java") || BUILD_FILES.contains(&name.as_str()))
        {
            let inside = path.strip_prefix(root).unwrap_or(&path);
            let parts: Vec<_> = inside
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect();
            files.push(parts.join("/"));
        }
    }
    Ok(())
}

/// Runs `scip-java index` at `root` and puts the index at `index`. It builds
/// with `mvn clean verify` or the like, which removes the build directory the
/// index is kept in: the index is written apart, then moved in.
fn run_scip_java(root: &Path, index: &Path) -> Result<(), Error> {
    let written = std::env::temp_dir().join(format!("{TOOL}-{}.scip", std::process::id()));
    let output = Command::new("scip-java")
        .arg("index")
        .arg("--output")
        .arg(&written)
        .current_dir(root)
        .output()
        .map_err(missing_scip_java)?;
    if output.status.success() && written.exists() {
        let directory = index.parent().expect("the index is in a directory");
        std::fs::create_dir_all(directory).map_err(|e| io_error(directory, e))?;
        std::fs::copy(&written, index).map_err(|e| io_error(index, e))?;
        return std::fs::remove_file(&written).map_err(|e| io_error(&written, e));
    }
    // The build tool writes its errors to standard output.
    let mut written = String::from_utf8_lossy(&output.stdout).into_owned();
    written += &String::from_utf8_lossy(&output.stderr);
    let last_lines: Vec<&str> = written.lines().rev().take(20).collect();
    let last_lines: Vec<&str> = last_lines.into_iter().rev().collect();
    Err(Error::new(format!(
        "scip-java index failed ({}):\n{}",
        output.status,
        last_lines.join("\n")
    )))
}

fn missing_scip_java(error: std::io::Error) -> Error {
    if error.kind() == ErrorKind::NotFound {
        Error::new(format!(
            "scip-java is not on PATH: {TOOL} reads the index that `scip-java index` writes \
             of a Maven or Gradle project"
        ))
    } else {
        Error::new(format!("cannot run scip-java: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("{TOOL}-java-{name}"));
        if directory.exists() {
            std::fs::remove_dir_all(&directory).unwrap();
        }
        directory
    }

    fn touch(path: PathBuf) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    #[test]
    fn the_root_is_the_outermost_build_directory_and_cargo_closer_wins() {
        let w = directory("root");
        touch(w.join("pom.xml"));
        touch(w.join("core/pom.xml"));
        touch(w.join("core/src/main/java/A.java"));
        touch(w.join("core/native/Cargo.toml"));
        let found = Project::around(&w.join("core/src/main/java")).unwrap();
        assert_eq!(found.root, w);
        assert!(Project::around(&w.join("core/native")).is_none());
        touch(w.join("core/target/B.java"));
        touch(w.join("core/.hidden/C.java"));
        assert_eq!(
            found.source_files().unwrap(),
            ["core/pom.xml", "core/src/main/java/A.java", "pom.xml"]
        );
        std::fs::remove_dir_all(&w).unwrap();
    }
}
