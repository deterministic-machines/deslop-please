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

/// Directories that hold what a build writes, outside a `src` directory.
const OUTPUTS: [&str; 3] = ["target", "build", "out"];

/// Endings of the files a build reads besides its sources: Gradle scripts
/// and version catalogs, and properties.
const BUILD_INPUTS: [&str; 4] = [".gradle", ".kts", ".versions.toml", ".properties"];

/// Whether `path`, relative to the root and `/`-separated, is in a directory
/// a build writes: a `target`, `build` or `out` that is not inside a `src`
/// directory. `src/main/java/com/acme/build/` holds sources.
pub fn is_generated(path: &str) -> bool {
    path.split('/')
        .take_while(|&part| part != "src")
        .any(|part| OUTPUTS.contains(&part))
}

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
    /// scip-java or the JDK has.
    pub fn index(&self) -> Result<Vec<u8>, Error> {
        let version = Command::new("scip-java")
            .arg("--version")
            .output()
            .map_err(missing_scip_java)?;
        let mut stamp = String::from_utf8_lossy(&version.stdout).into_owned();
        // `java -version` writes to standard error.
        if let Ok(java) = Command::new("java").arg("-version").output() {
            stamp += &String::from_utf8_lossy(&java.stderr);
        }
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

    /// The Java sources and what the build reads, relative to the root and
    /// sorted, past build outputs and hidden directories but `.mvn`.
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
        let inside = path.strip_prefix(root).unwrap_or(&path);
        let parts: Vec<_> = inside
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect();
        let relative = parts.join("/");
        // Not through symbolic links, which can loop.
        let file_type = entry.file_type().map_err(unreadable)?;
        if name.starts_with('.') && name != ".mvn" {
            continue;
        }
        if file_type.is_dir() {
            if !is_generated(&relative) {
                collect_sources(&path, root, files)?;
            }
        } else if file_type.is_file()
            && (name.ends_with(".java")
                || name == "pom.xml"
                || BUILD_INPUTS.iter().any(|ending| name.ends_with(ending))
                || relative.starts_with(".mvn/"))
        {
            files.push(relative);
        }
    }
    Ok(())
}

/// Runs `scip-java index` at `root` and puts the index at `index`. It builds
/// with `mvn clean verify` or the like, which removes the build directory the
/// index is kept in: the index is written apart, in a directory of this run's
/// own that no one else can write to, then moved in.
fn run_scip_java(root: &Path, index: &Path) -> Result<(), Error> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let apart = std::env::temp_dir().join(format!("{TOOL}-{}-{nanos}", std::process::id()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    // Fails if the path is there already, a symbolic link too.
    builder.create(&apart).map_err(|e| io_error(&apart, e))?;
    let result = index_apart(root, &apart.join("index.scip"), index);
    let removed = std::fs::remove_dir_all(&apart).map_err(|e| io_error(&apart, e));
    result.and(removed)
}

fn index_apart(root: &Path, written: &Path, index: &Path) -> Result<(), Error> {
    let output = Command::new("scip-java")
        .arg("index")
        .arg("--output")
        .arg(written)
        .current_dir(root)
        .output()
        .map_err(missing_scip_java)?;
    if output.status.success() && written.exists() {
        let directory = index.parent().expect("the index is in a directory");
        std::fs::create_dir_all(directory).map_err(|e| io_error(directory, e))?;
        std::fs::copy(written, index).map_err(|e| io_error(index, e))?;
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(Error::new(format!(
        "scip-java index failed ({}):\n{}",
        output.status,
        failure(&stdout, &stderr)
    )))
}

/// What to show of a failed build: Maven's errors, which it writes to
/// standard output, or else the last lines of both outputs.
fn failure(stdout: &str, stderr: &str) -> String {
    let errors: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("[ERROR]"))
        .take(20)
        .collect();
    if !errors.is_empty() {
        return errors.join("\n");
    }
    let lines: Vec<&str> = stdout.lines().chain(stderr.lines()).collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
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
    fn build_outputs_are_outside_src() {
        assert!(is_generated("target/generated-sources/A.java"));
        assert!(is_generated("core/build/classes"));
        assert!(!is_generated("src/main/java/com/acme/build/A.java"));
        assert!(!is_generated("core/src/main/java/out/A.java"));
    }

    #[test]
    fn a_failed_build_shows_mavens_errors_or_the_last_lines() {
        let stdout = "[INFO] a\n[ERROR] compile failed\n[INFO] b\n";
        assert_eq!(
            failure(stdout, "WARNING: Unsafe\n"),
            "[ERROR] compile failed"
        );
        let many: String = (0..30).map(|n| format!("{n}\n")).collect();
        let shown = failure(&many, "last");
        assert!(
            shown.starts_with("11\n") && shown.ends_with("29\nlast"),
            "{shown}"
        );
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
        touch(w.join("core/src/main/java/build/D.java"));
        touch(w.join(".mvn/maven.config"));
        touch(w.join("gradle/libs.versions.toml"));
        assert_eq!(
            found.source_files().unwrap(),
            [
                ".mvn/maven.config",
                "core/pom.xml",
                "core/src/main/java/A.java",
                "core/src/main/java/build/D.java",
                "gradle/libs.versions.toml",
                "pom.xml"
            ]
        );
        std::fs::remove_dir_all(&w).unwrap();
    }
}
