//! Go workspaces are indexed by scip-go and checked by the normal lint engine.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

static WORKSPACE_ID: AtomicUsize = AtomicUsize::new(0);

fn copy_directory(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_directory(&path, &target);
        } else {
            std::fs::copy(path, target).unwrap();
        }
    }
}

fn workspace(fixture: &str) -> PathBuf {
    assert!(
        Command::new("go")
            .arg("version")
            .output()
            .is_ok_and(|output| output.status.success()),
        "these tests run Go, which is not on PATH"
    );
    // Like the Rust end-to-end tests, fail explicitly when the indexer is absent
    // rather than passing without exercising the frontend.
    assert!(
        Command::new("scip-go").arg("--version").output().is_ok(),
        "these tests run scip-go, which is not on PATH"
    );
    let copy = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "{fixture}-{}",
        WORKSPACE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    if copy.exists() {
        std::fs::remove_dir_all(&copy).unwrap();
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    copy_directory(&source, &copy);
    copy
}

fn lint(workspace: &Path, expected_code: i32) -> Value {
    lint_with_go_flags(workspace, expected_code, "")
}

fn lint_with_go_flags(workspace: &Path, expected_code: i32, flags: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_deslop-please"))
        .arg("--json")
        .current_dir(workspace)
        .env("CARGO_TERM_COLOR", "never")
        .env_remove("GOWORK")
        .env("GOFLAGS", flags)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        output.status.code(),
        Some(expected_code),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("invalid JSON: {error}\nstdout:\n{stdout}\nstderr:\n{stderr}")
    })
}

fn diagnostic<'a>(output: &'a Value, lint: &str) -> &'a Value {
    output["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| diagnostic["lint"] == lint)
        .unwrap_or_else(|| panic!("missing {lint} diagnostic: {output}"))
}

#[test]
fn go_module_reports_tangle_and_field_leak() {
    let output = lint(&workspace("go_module"), 1);
    assert_eq!(output["ok"], false);
    let tangle = diagnostic(&output, "tangle");
    assert!(
        tangle["message"].as_str().unwrap().contains("8 things"),
        "{tangle}"
    );
    let key = tangle["key"].as_str().unwrap();
    for member in ["A", "B", "C", "D", "E", "F", "G", "H"] {
        assert!(
            key.split(',')
                .any(|name| name.ends_with(&format!("::{member}"))),
            "{key}"
        );
    }
    let leak = diagnostic(&output, "field-leak");
    assert!(
        leak["key"].as_str().unwrap().ends_with("Queue::Items"),
        "{leak}"
    );
    assert!(
        leak["file"]
            .as_str()
            .unwrap()
            .ends_with("consumer/consumer.go"),
        "{leak}"
    );
    assert_eq!(leak["line"], 6);
}

#[test]
fn go_workspace_resolves_references_between_modules() {
    let output = lint(&workspace("go_workspace"), 1);
    let leak = diagnostic(&output, "field-leak");
    assert!(
        leak["key"].as_str().unwrap().ends_with("Queue::Items"),
        "{leak}"
    );
    assert!(
        leak["file"]
            .as_str()
            .unwrap()
            .ends_with("consumer/consumer.go"),
        "{leak}"
    );
    assert_eq!(leak["line"], 6);
}

#[test]
fn go_narrow_structure_has_no_findings() {
    let output = lint(&workspace("go_narrow"), 0);
    assert_eq!(output["ok"], true);
    assert_eq!(output["diagnostics"], serde_json::json!([]));
}

#[test]
fn go_workspace_is_discovered_from_a_nested_module() {
    let root = workspace("go_workspace");
    let output = lint(&root.join("consumer"), 1);
    let leak = diagnostic(&output, "field-leak");
    assert!(
        leak["key"].as_str().unwrap().ends_with("Queue::Items"),
        "{leak}"
    );
    assert!(
        leak["file"]
            .as_str()
            .unwrap()
            .ends_with("consumer/consumer.go"),
        "{leak}"
    );
    assert_eq!(leak["line"], 6);
}

#[test]
fn go_build_tags_select_the_active_configuration() {
    let root = workspace("go_build_tags");
    let default = lint_with_go_flags(&root, 0, "");
    assert_eq!(default["diagnostics"], serde_json::json!([]));

    let tagged = lint_with_go_flags(&root, 1, "-tags=expose");
    let leak = diagnostic(&tagged, "field-leak");
    assert!(
        leak["key"].as_str().unwrap().ends_with("Queue::Items"),
        "{leak}"
    );
    assert!(
        leak["file"]
            .as_str()
            .unwrap()
            .ends_with("consumer/expose.go"),
        "{leak}"
    );
    assert_eq!(leak["line"], 8);
}

#[test]
fn go_rejects_cargo_configuration_flags() {
    let root = workspace("go_narrow");
    let output = Command::new(env!("CARGO_BIN_EXE_deslop-please"))
        .args(["--json", "--all-features"])
        .current_dir(root)
        .env_remove("GOWORK")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Cargo configuration flags cannot be used for Go workspaces")
    );
}

#[test]
fn go_mutable_slice_getter_exposes_the_field_to_its_consumer() {
    let output = lint(&workspace("go_getter_slice"), 1);
    let leak = diagnostic(&output, "field-leak");
    assert!(
        leak["key"].as_str().unwrap().ends_with("Queue::items"),
        "{leak}"
    );
    assert!(
        leak["file"]
            .as_str()
            .unwrap()
            .ends_with("consumer/consumer.go"),
        "{leak}"
    );
    assert_eq!(leak["line"], 6);
}

#[test]
fn go_value_getter_does_not_expose_the_field() {
    let output = lint(&workspace("go_getter_value"), 0);
    assert_eq!(output["ok"], true);
    assert_eq!(output["diagnostics"], serde_json::json!([]));
}

#[test]
fn go_interface_method_dispatch_reports_unsupported_instead_of_omitting_edges() {
    let root = workspace("go_interface_dispatch");
    let output = Command::new(env!("CARGO_BIN_EXE_deslop-please"))
        .arg("--json")
        .current_dir(root)
        .env_remove("GOWORK")
        .env("GOFLAGS", "")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("interface method dispatch cannot be represented"),
        "{stderr}"
    );
    assert!(stderr.contains("consumer.go:10:"), "{stderr}");
}
