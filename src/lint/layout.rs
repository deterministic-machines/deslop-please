//! `wide-directory` and `long-file`: how the code is laid out in files. A
//! reader takes in a directory by listing it and finds their way in a file by
//! scrolling through it, so each has a most it holds. A directory's members
//! are its source files and its directories that hold source files; a file's
//! lines are all of them, blank and comment lines too; test code counts like
//! any other. A directory concerns a crate or module when one of its files is
//! below it.

use std::collections::{BTreeMap, BTreeSet};

use super::{Diagnostic, Lint, Location, Picture, in_scope, listed};
use crate::model::{File, Model, NodeId};

/// The most files and directories one directory holds. Past fifteen names, a
/// listing is searched rather than read, and its files want directories of
/// their own, by what they are about.
const MOST_MEMBERS: usize = 15;

/// The most lines one file has. Past 2,000, a file holds parts that want
/// files of their own.
const MOST_LINES: usize = 2_000;

pub fn check(model: &Model, scope: Option<NodeId>) -> Vec<Diagnostic> {
    let concerned = |file: &File| in_scope(&model.tree, scope, file.module);
    let mut diagnostics: Vec<Diagnostic> = model
        .files
        .iter()
        .filter(|file| file.lines > MOST_LINES && concerned(file))
        .map(long_file)
        .collect();
    // By directory, its members, and whether a file below it is in scope.
    let mut directories: BTreeMap<&str, (BTreeSet<&str>, bool)> = BTreeMap::new();
    for file in &model.files {
        for (directory, member) in holders(&file.path) {
            let (members, below) = directories.entry(directory).or_default();
            members.insert(member);
            *below |= concerned(file);
        }
    }
    diagnostics.extend(
        directories
            .iter()
            .filter(|(_, (members, below))| *below && members.len() > MOST_MEMBERS)
            .map(|(directory, (members, _))| wide_directory(model, directory, members)),
    );
    diagnostics
}

/// Each directory that holds `path`, outermost first, `""` for the root, with
/// the member it holds `path` by: `src/lint/cycles.rs` is in the root by
/// `src/`, in `src` by `lint/`, and in `src/lint` by `cycles.rs`.
fn holders(path: &str) -> impl Iterator<Item = (&str, &str)> {
    let ends = path
        .match_indices('/')
        .map(|(at, _)| at + 1)
        .chain([path.len()]);
    let mut start = 0usize;
    ends.map(move |end| {
        let held = (&path[..start.saturating_sub(1)], &path[start..end]);
        start = end;
        held
    })
}

fn long_file(file: &File) -> Diagnostic {
    Diagnostic {
        lint: Lint::LongFile,
        key: file.path.clone(),
        message: format!(
            "`{}` is long: it has {} lines, more than {MOST_LINES}",
            file.path, file.lines
        ),
        location: Some(Location {
            file: file.path.clone(),
            line: 1,
        }),
        notes: Vec::new(),
        picture: Picture::Bar {
            label: "lines".to_owned(),
            length: file.lines,
            most: MOST_LINES,
        },
    }
}

/// The report of `directory`, which holds `members`.
fn wide_directory(model: &Model, directory: &str, members: &BTreeSet<&str>) -> Diagnostic {
    let inside = if directory.is_empty() {
        String::new()
    } else {
        format!("{directory}/")
    };
    let shown = if directory.is_empty() {
        "./"
    } else {
        inside.as_str()
    };
    let (directories, files): (Vec<String>, Vec<String>) = members
        .iter()
        .map(|&member| member.to_owned())
        .partition(|member| member.ends_with('/'));
    let notes = [
        (files, "file", "files"),
        (directories, "directory", "directories"),
    ]
    .into_iter()
    .filter(|(names, ..)| !names.is_empty())
    .map(|(names, one, many)| {
        let noun = if names.len() == 1 { one } else { many };
        format!("{} {noun}: {}", names.len(), listed(&names))
    })
    .collect();
    // The file that declares its modules: in it, or beside it.
    const DECLARING: [&str; 7] = [
        "mod.rs",
        "lib.rs",
        "main.rs",
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
    ];
    let beside = format!("{directory}.rs");
    let declaring = DECLARING
        .into_iter()
        .filter(|name| members.contains(name))
        .map(|name| format!("{inside}{name}"))
        .chain(
            (!directory.is_empty() && model.files.iter().any(|file| file.path == beside))
                .then_some(beside),
        )
        .next();
    Diagnostic {
        lint: Lint::WideDirectory,
        key: shown.to_owned(),
        message: format!(
            "`{shown}` is wide: it holds {} files and directories, more than {MOST_MEMBERS}",
            members.len()
        ),
        location: declaring.map(|file| Location { file, line: 1 }),
        notes,
        picture: Picture::Bar {
            label: "holds".to_owned(),
            length: members.len(),
            most: MOST_MEMBERS,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeKind;
    use crate::model::tests::Sample;
    use crate::scip::with_slashes;

    /// Single-letter names, from `a`, as many as `count`.
    fn letters(count: usize) -> Vec<String> {
        ('a'..='z').take(count).map(String::from).collect()
    }

    /// A sample whose crate, `p`, has its root at `src/lib.rs`, and the
    /// crate's node.
    fn crate_p() -> (Sample, NodeId) {
        let mut sample = Sample::new();
        let tree = &mut sample.model.tree;
        let p = tree.child(tree.root(), NodeKind::Package, "p");
        sample.file(p, "src/lib.rs", 1);
        (sample, p)
    }

    fn keys(sample: &Sample, scope: Option<NodeId>) -> Vec<String> {
        check(&sample.model, scope)
            .into_iter()
            .map(|d| format!("{} {}", d.lint.name(), d.key))
            .collect()
    }

    #[test]
    fn a_directory_of_more_than_fifteen_files_and_directories_is_wide() {
        let (mut sample, _) = crate_p();
        for name in letters(14) {
            sample.module(&name);
        }
        assert_eq!(keys(&sample, None), Vec::<String>::new());
        sample.module("o::p");
        let diagnostics = check(&sample.model, None);
        assert_eq!(diagnostics.len(), 1);
        let wide = &diagnostics[0];
        assert_eq!(wide.key, "src/");
        assert_eq!(
            wide.message,
            "`src/` is wide: it holds 16 files and directories, more than 15"
        );
        assert_eq!(
            wide.location,
            Some(Location {
                file: "src/lib.rs".into(),
                line: 1
            })
        );
        assert_eq!(
            wide.notes,
            [
                "15 files: a.rs, b.rs, c.rs, d.rs, e.rs, f.rs, g.rs, +8 more",
                "1 directory: o/",
            ]
        );
        assert_eq!(
            wide.picture,
            Picture::Bar {
                label: "holds".into(),
                length: 16,
                most: 15
            }
        );
    }

    #[test]
    fn a_wide_directory_is_found_where_its_modules_are_declared() {
        let (mut sample, _) = crate_p();
        let store = sample.module("store");
        for name in letters(16) {
            sample.file(store, &format!("src/store/{name}.rs"), 1);
            sample.file(store, &format!("tests/{name}.rs"), 1);
        }
        let location = |sample: &Sample, key: &str| {
            check(&sample.model, None)
                .into_iter()
                .find(|d| d.key == key)
                .map(|d| d.location)
        };
        let beside = Location {
            file: "src/store.rs".into(),
            line: 1,
        };
        assert_eq!(location(&sample, "src/store/"), Some(Some(beside)));
        assert_eq!(location(&sample, "tests/"), Some(None));
        sample.file(store, "src/store/mod.rs", 1);
        let inside = Location {
            file: "src/store/mod.rs".into(),
            line: 1,
        };
        assert_eq!(location(&sample, "src/store/"), Some(Some(inside)));
    }

    #[test]
    fn a_directory_holds_its_files_whichever_separator_the_index_writes() {
        for separator in ['/', '\\'] {
            let (mut sample, _) = crate_p();
            let store = sample.module("store");
            for name in letters(16) {
                let written = format!("src{separator}store{separator}{name}.rs");
                sample.file(store, &with_slashes(&written, separator), 1);
            }
            assert_eq!(keys(&sample, None), ["wide-directory src/store/"]);
        }
    }

    #[test]
    fn a_file_of_more_than_2000_lines_is_long() {
        let (mut sample, p) = crate_p();
        sample.file(p, "src/table.rs", 2_000);
        assert_eq!(keys(&sample, None), Vec::<String>::new());
        sample.model.files.last_mut().expect("just added").lines = 2_001;
        let diagnostics = check(&sample.model, None);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].key, "src/table.rs");
        assert_eq!(
            diagnostics[0].message,
            "`src/table.rs` is long: it has 2001 lines, more than 2000"
        );
    }

    #[test]
    fn a_wide_directory_concerns_what_is_below_it_and_a_long_file_its_module() {
        let (mut sample, p) = crate_p();
        let modules: Vec<NodeId> = letters(15).iter().map(|name| sample.module(name)).collect();
        sample.model.files[1].lines = 2_001;
        let tree = &mut sample.model.tree;
        let q = tree.child(tree.root(), NodeKind::Package, "q");
        sample.file(q, "q/src/lib.rs", 1);
        let both = ["long-file src/a.rs", "wide-directory src/"];
        assert_eq!(keys(&sample, None), both);
        assert_eq!(keys(&sample, Some(p)), both);
        assert_eq!(keys(&sample, Some(modules[0])), both);
        assert_eq!(keys(&sample, Some(modules[1])), ["wide-directory src/"]);
        assert_eq!(keys(&sample, Some(q)), Vec::<String>::new());
    }

    #[test]
    fn a_path_is_held_by_each_directory_above_it() {
        let held: Vec<_> = holders("src/lint/cycles.rs").collect();
        assert_eq!(
            held,
            [("", "src/"), ("src", "lint/"), ("src/lint", "cycles.rs")]
        );
        assert_eq!(holders("build.rs").collect::<Vec<_>>(), [("", "build.rs")]);
    }
}
