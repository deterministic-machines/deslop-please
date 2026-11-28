//! End-to-end tests of the Tree-sitter backend, `--backend tree-sitter`.
//!
//! They cover the same fixture families as `end_to_end.rs` and expect the same
//! diagnostics: the backend must build the same model, reading the manifests
//! and sources alone. None of these tests needs rust-analyzer, and one proves
//! it by running the binary with an empty environment.
//!
//! `tests/fixtures/typescript` is the TypeScript counterpart of the Rust
//! families: cycles, a tangle and field leaks across a `tsconfig` path alias
//! and a barrel re-export. `tests/fixtures/go` and `tests/fixtures/python` do
//! the same for their grammars, and `tests/fixtures/mixed` holds all four
//! languages, which `--lang` chooses between.
//!
//! `tests/fixtures/build_script` is left out on purpose: its cycle runs through
//! constants its build script writes to `OUT_DIR`, which only the SCIP backend
//! sees, because indexing compiles and runs the script. The Tree-sitter backend
//! reads source alone, so it cannot see them, and pretending otherwise (by
//! copying constants into the fixture) would test a fiction.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TOOL: &str = "deslop-please";
const PROGRAM: &str = env!("CARGO_BIN_EXE_deslop-please");

/// A copy of `fixture`, with `variant`'s files over it, to run `test` in.
fn workspace(fixture: &str, variant: Option<&str>, test: &str) -> PathBuf {
    let copy = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    if copy.exists() {
        std::fs::remove_dir_all(&copy).unwrap();
    }
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    copy_directory(&tests.join("fixtures").join(fixture), &copy);
    if let Some(variant) = variant {
        copy_directory(&tests.join("variants").join(fixture).join(variant), &copy);
    }
    copy
}

fn copy_directory(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_directory(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(workspace: &Path, arguments: &[&str]) -> Run {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(PROGRAM)
        .args(["--backend", "tree-sitter"])
        .args(arguments)
        .current_dir(workspace)
        // CI sets CARGO_TERM_COLOR=always; the tests read plain text.
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .unwrap();
    let run = Run {
        code: status.code(),
        stdout: String::from_utf8(stdout).unwrap(),
        stderr: String::from_utf8(stderr).unwrap(),
    };
    eprintln!(
        "$ {TOOL} --backend tree-sitter {}\n{}{}",
        arguments.join(" "),
        run.stdout,
        run.stderr
    );
    run
}

/// A run with nothing on `PATH`: the backend must not spawn a subprocess.
fn run_without_path(workspace: &Path, arguments: &[&str]) -> Run {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(PROGRAM)
        .args(["--backend", "tree-sitter"])
        .args(arguments)
        .current_dir(workspace)
        .env_clear()
        .env("PATH", "")
        .output()
        .unwrap();
    Run {
        code: status.code(),
        stdout: String::from_utf8(stdout).unwrap(),
        stderr: String::from_utf8(stderr).unwrap(),
    }
}

/// `lint key` for every diagnostic in `workspace`, as the allow file lists them.
fn keys(workspace: &Path) -> Vec<String> {
    configured_keys(workspace, &[])
}

/// The keys of the configuration that `flags` choose.
fn configured_keys(workspace: &Path, flags: &[&str]) -> Vec<String> {
    let allow = workspace.with_extension("keys");
    let mut arguments = vec!["--update-allow", allow.to_str().unwrap()];
    arguments.extend(flags);
    let update = run(workspace, &arguments);
    assert_eq!(update.code, Some(0));
    let mut keys: Vec<String> = std::fs::read_to_string(&allow)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect();
    keys.sort();
    keys
}

/// The keys of `family`'s base, or of one of its variants.
fn variant_keys(family: &str, variant: Option<&str>) -> Vec<String> {
    let test = format!("treesitter-{family}-{}", variant.unwrap_or("base"));
    keys(&workspace(family, variant, &test))
}

#[test]
fn the_backend_needs_no_rust_analyzer_or_cargo() {
    let workspace = workspace("field_leak", None, "treesitter-no-tools");
    let result = run_without_path(&workspace, &[]);
    assert_eq!(result.code, Some(1));
    assert!(
        result
            .stdout
            .contains("error[field-leak]: `RunQueue::items` is known outside `RunQueue`")
    );
    assert!(result.stderr.is_empty(), "{}", result.stderr);
}

#[test]
fn a_clean_workspace_exits_zero() {
    let workspace = workspace("clean", None, "treesitter-clean");
    let result = run(&workspace, &[]);
    assert_eq!(result.code, Some(0));
    assert!(result.stdout.contains(&format!("{TOOL}: ok")));
}

#[test]
fn a_leaked_field_leaks_however_it_is_reached_and_not_once_the_type_does_the_work() {
    let run_queue = run(&workspace("field_leak", None, "treesitter-field_leak"), &[]);
    assert_eq!(run_queue.code, Some(1));
    assert!(run_queue.stdout.contains(
        "error[field-leak]: `RunQueue::items` is known outside `RunQueue`: 4 places in 2 modules"
    ));
    assert!(run_queue.stdout.contains(
        "    runqueue::queue::ring::RunQueue\n\
         \x20     items ◀━┳━ 2  runqueue::sched::policy\n\
         \x20             ┗━ 2  runqueue::sched::report\n\
         \x20     also known outside: head\n\
         \x20     kept: limit\n"
    ));
    let leaked = [
        "field-leak runqueue::queue::ring::RunQueue::head",
        "field-leak runqueue::queue::ring::RunQueue::items",
    ];
    for variant in [
        "getters",
        "let_getters",
        "all_pub",
        "pasted",
        "tuple_getter",
    ] {
        assert_eq!(
            variant_keys("field_leak", Some(variant)),
            leaked,
            "{variant}"
        );
    }
    for variant in ["real", "passed_on", "read", "lent_read"] {
        assert_eq!(
            variant_keys("field_leak", Some(variant)),
            Vec::<String>::new(),
            "{variant}"
        );
    }
}

#[test]
fn a_module_cycle_stays_one_through_a_re_export_or_a_relay_until_code_moves_where_it_is_used() {
    let net = run(
        &workspace("module_cycle", None, "treesitter-module_cycle"),
        &[],
    );
    assert_eq!(net.code, Some(1));
    assert!(
        net.stdout
            .contains("error[module-cycle]: 2 modules in `net` use each other in a circle")
    );
    assert!(!net.stdout.contains("error[tangle]"));
    assert_eq!(
        variant_keys("module_cycle", Some("reexported")),
        ["module-cycle net::kinds,net::socket"]
    );
    assert_eq!(
        variant_keys("module_cycle", Some("relayed")),
        ["module-cycle net::kinds,net::limits,net::socket"]
    );
    assert_eq!(
        variant_keys("module_cycle", Some("moved")),
        Vec::<String>::new()
    );
    assert_eq!(
        variant_keys("module_cycle", Some("for_tests")),
        Vec::<String>::new()
    );
}

#[test]
fn a_tangle_stays_one_however_it_is_laid_out_until_it_is_one_module_or_untied() {
    let tasks_and_timers = "tangle boot::kernel::tasks::Tasks,boot::kernel::timers::Timers";
    let boot = run(&workspace("tangle", None, "treesitter-tangle"), &[]);
    assert!(
        boot.stdout
            .contains("error[tangle]: 2 things know each other in a circle, across 2 modules")
    );
    assert_eq!(
        variant_keys("tangle", Some("impl_moved")),
        [tasks_and_timers]
    );
    assert_eq!(
        variant_keys("tangle", Some("lifted")),
        ["tangle boot::kernel::Tasks,boot::kernel::timers::Timers"]
    );
    for variant in ["merged", "real", "lifted_untied"] {
        assert_eq!(
            variant_keys("tangle", Some(variant)),
            Vec::<String>::new(),
            "{variant}"
        );
    }
    let ring = workspace("tangle", Some("ring"), "treesitter-tangle-ring");
    assert!(
        run(&ring, &[])
            .stdout
            .contains("error[tangle]: 8 things know each other in a circle, all in `boot::ring`")
    );
    let ring_key = "tangle boot::ring::A,boot::ring::B,boot::ring::C,boot::ring::D,\
                    boot::ring::E,boot::ring::F,boot::ring::G,boot::ring::H";
    assert_eq!(
        keys(&ring),
        [
            "module-cycle boot::kernel::tasks,boot::kernel::timers",
            tasks_and_timers,
            ring_key
        ]
    );
}

#[test]
fn a_shallow_module_stays_shallow_however_it_is_dressed_until_it_does_more_with_less() {
    let store = run(&workspace("shallow", None, "treesitter-shallow"), &[]);
    assert!(store.stdout.contains(
        "error[shallow-module]: `inventory::store` is shallow: it shows 9 of its 15 functions"
    ));
    assert!(store.stdout.contains(
        "= note: `Store` shows 9 functions: Store::contains, Store::get, Store::insert, \
         Store::is_full, Store::largest, Store::new, Store::remove, +2 more"
    ));
    let shallow = ["shallow-module inventory::store"];
    for variant in [
        "padded",
        "impls_moved",
        "pub_field",
        "private_modules",
        "some_free_functions",
        "free_functions",
    ] {
        assert_eq!(variant_keys("shallow", Some(variant)), shallow, "{variant}");
    }
    assert_eq!(variant_keys("shallow", Some("deep")), Vec::<String>::new());
}

#[test]
fn a_few_small_types_and_two_crates_are_clean() {
    for fixture in ["small_types", "two_crates"] {
        let workspace = workspace(fixture, None, &format!("treesitter-{fixture}"));
        assert_eq!(run(&workspace, &[]).code, Some(0), "{fixture}");
    }
}

#[test]
fn a_file_included_inside_a_module_or_a_binarys_root_is_that_module() {
    let flags = run(
        &workspace("included_module", None, "treesitter-included_module"),
        &[],
    );
    assert_eq!(flags.code, Some(1));
    assert!(flags.stdout.contains(
        "error[field-leak]: `Flags::names` is known outside `Flags`: 1 place in 1 module\n\
         \x20 --> build.rs:6:1\n"
    ));
    assert!(flags.stdout.contains("kept: target"));
    assert_eq!(
        variant_keys("included_module", None),
        ["field-leak flags::flags::Flags::names"]
    );
    let leaked = ["field-leak wt::cli::Args::verbose"];
    let wt = run(
        &workspace("included_root", None, "treesitter-included_root"),
        &[],
    );
    assert_eq!(wt.code, Some(1));
    assert!(wt.stdout.contains(
        "error[field-leak]: `Args::verbose` is known outside `Args`: 1 place in 1 module\n\
         \x20 --> src/main.rs:5:1\n"
    ));
    assert_eq!(variant_keys("included_root", None), leaked);
    assert_eq!(variant_keys("included_root", Some("renamed")), leaked);
}

#[test]
fn a_target_or_features_bring_in_the_code_behind_their_cfg() {
    let workspace = workspace("gated", None, "treesitter-gated");
    let console = "module-cycle gated::console::handle,gated::console::modes";
    let export = "module-cycle gated::export::csv,gated::export::rows";
    let metrics = "module-cycle gated::metrics::counter,gated::metrics::sink";
    assert_eq!(keys(&workspace), [metrics]);
    assert_eq!(
        configured_keys(&workspace, &["--target", "x86_64-pc-windows-msvc"]),
        [console, metrics]
    );
    assert_eq!(
        configured_keys(&workspace, &["--features", "export"]),
        [export, metrics]
    );
    assert_eq!(
        configured_keys(&workspace, &["--all-features"]),
        [export, metrics]
    );
    assert_eq!(
        configured_keys(&workspace, &["--no-default-features"]),
        Vec::<String>::new()
    );
    assert_eq!(
        configured_keys(&workspace, &["-F", "export", "--no-default-features"]),
        [export]
    );
}

#[test]
fn json_and_sarif_output_report_the_same_diagnostics() {
    let clean = workspace("clean", None, "treesitter-json-clean");
    let result = run(&clean, &["--json"]);
    assert_eq!(result.code, Some(0));
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["count"], 0);

    let workspace = workspace("field_leak", None, "treesitter-json-field-leak");
    let result = run(&workspace, &["--json"]);
    assert_eq!(result.code, Some(1));
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["count"], 2);
    let diagnostics = value["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics
            .iter()
            .all(|d| d["lint"] == "field-leak" && d["file"].is_string() && d["line"].is_number())
    );

    let result = run(&workspace, &["--sarif"]);
    assert_eq!(result.code, Some(1));
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let results = value["runs"][0]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| {
        result["ruleId"] == "field-leak"
            && result["partialFingerprints"]["deslop-please/v1"].is_string()
            && result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"].is_string()
    }));
}

#[test]
fn the_allow_file_suppresses_only_existing_entries_and_notes_what_is_fixed() {
    let workspace = workspace("field_leak", None, "treesitter-allow");
    let allow = workspace.with_extension("allow");
    let allow = allow.to_str().unwrap();
    assert_eq!(run(&workspace, &["--update-allow", allow]).code, Some(0));
    let allowed = run(&workspace, &["--allow", allow]);
    assert_eq!(allowed.code, Some(0));
    assert!(allowed.stdout.contains(&format!("{TOOL}: ok")));

    let real = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/variants/field_leak/real");
    copy_directory(&real, &workspace);
    let fixed = run(&workspace, &["--allow", allow]);
    assert_eq!(fixed.code, Some(0));
    assert!(fixed.stdout.contains("2 allowed entries"));
    assert!(fixed.stdout.contains("now fixed"));
}

/// A run with exactly `arguments`: no backend flag, so a test can pass its own.
fn run_raw(workspace: &Path, arguments: &[&str]) -> Run {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(PROGRAM)
        .args(arguments)
        .current_dir(workspace)
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .unwrap();
    Run {
        code: status.code(),
        stdout: String::from_utf8(stdout).unwrap(),
        stderr: String::from_utf8(stderr).unwrap(),
    }
}

#[test]
fn a_typescript_workspace_reports_its_cycles_tangles_and_leaks() {
    let workspace = workspace("typescript", None, "treesitter-typescript");
    let result = run(&workspace, &[]);
    assert_eq!(result.code, Some(1));
    // The `clock` and `queue` cycle exists only through both a `tsconfig`
    // path (`@/clock`) and a barrel: `queue/index.ts` re-exports `ring`,
    // `ring` imports `@/clock`, and `clock` imports `./queue`.
    assert!(
        result
            .stdout
            .contains("error[module-cycle]: 2 modules in `web/src` use each other in a circle")
    );
    assert!(
        result
            .stdout
            .contains("error[tangle]: 2 things know each other in a circle, across 2 modules")
    );
    // The two leaks are read through `@/queue/ring`, so they need the alias too.
    assert!(
        result.stdout.contains(
            "error[field-leak]: `Ring::items` is known outside `Ring`: 1 place in 1 module"
        )
    );
    assert!(result.stdout.contains(
        "    web/src/queue/ring/Ring\n\
         \x20     items ◀━━━ 1  web/src/sched/policy\n\
         \x20     also known outside: head\n\
         \x20     kept: limit\n"
    ));
    assert_eq!(
        keys(&workspace),
        [
            "field-leak web/src/queue/ring/Ring::head",
            "field-leak web/src/queue/ring/Ring::items",
            "module-cycle web/src/clock,web/src/queue",
            "module-cycle web/src/kernel/tasks,web/src/kernel/timers",
            "tangle web/src/clock/tick,web/src/queue/ring/Ring",
        ]
    );
}

#[test]
fn a_mixed_workspace_reports_every_language_and_lang_chooses_one() {
    let go = "field-leak example.com/mixed/gosvc/Queue::Items";
    let rust = "field-leak core::ring::Ring::items";
    let python = "module-cycle pysvc.a,pysvc.b";
    let typescript = "module-cycle web/src/a,web/src/b";
    let workspace = workspace("mixed", None, "treesitter-mixed");
    let result = run(&workspace, &[]);
    assert_eq!(result.code, Some(1));
    assert!(
        result
            .stdout
            .contains("error[field-leak]: `Ring::items` is known outside `Ring`")
    );
    assert!(
        result
            .stdout
            .contains("error[field-leak]: `Queue::Items` is known outside `Queue`")
    );
    assert!(
        result
            .stdout
            .contains("error[module-cycle]: 2 modules in `web/src` use each other in a circle")
    );
    assert!(
        result
            .stdout
            .contains("error[module-cycle]: 2 modules in `pysvc` use each other in a circle")
    );
    assert_eq!(keys(&workspace), [rust, go, python, typescript]);
    assert_eq!(configured_keys(&workspace, &["--lang", "rust"]), [rust]);
    assert_eq!(configured_keys(&workspace, &["--lang", "go"]), [go]);
    assert_eq!(configured_keys(&workspace, &["--lang", "python"]), [python]);
    assert_eq!(configured_keys(&workspace, &["--lang", "ts"]), [typescript]);
    assert_eq!(
        configured_keys(&workspace, &["--lang", "javascript"]),
        [typescript]
    );
    assert_eq!(
        configured_keys(&workspace, &["--lang=rs,ts"]),
        [rust, typescript]
    );
    assert_eq!(configured_keys(&workspace, &["--lang=go,py"]), [go, python]);
}

#[test]
fn a_go_workspace_reports_a_field_leak_and_a_tangle_and_skips_test_files() {
    let workspace = workspace("go", None, "treesitter-go");
    let result = run(&workspace, &[]);
    assert_eq!(result.code, Some(1));
    assert!(result.stdout.contains(
        "error[field-leak]: `Queue::Items` is known outside `Queue`: 1 place in 1 module"
    ));
    assert!(result.stdout.contains(
        "    example.com/demo/queue/Queue\n\
         \x20     Items ◀━━━ 1  example.com/demo/app\n\
         \x20     kept: limit\n"
    ));
    assert!(result.stdout.contains(
        "error[tangle]: 8 things know each other in a circle, all in `example.com/demo/ring`"
    ));
    // `app_test.go` writes the field too, but test code never counts.
    assert!(!result.stdout.contains("app_test.go"), "{}", result.stdout);
    assert_eq!(
        keys(&workspace),
        [
            "field-leak example.com/demo/queue/Queue::Items",
            "tangle example.com/demo/ring/A,example.com/demo/ring/B,\
             example.com/demo/ring/C,example.com/demo/ring/D,example.com/demo/ring/E,\
             example.com/demo/ring/F,example.com/demo/ring/G,example.com/demo/ring/H",
        ]
    );
}

#[test]
fn a_python_workspace_reports_its_cycle_tangle_property_leak_and_skips_test_files() {
    let workspace = workspace("python", None, "treesitter-python");
    let result = run(&workspace, &[]);
    assert_eq!(result.code, Some(1));
    assert!(
        result
            .stdout
            .contains("error[module-cycle]: 2 modules in `app` use each other in a circle")
    );
    assert!(
        result
            .stdout
            .contains("error[tangle]: 2 things know each other in a circle, across 2 modules")
    );
    assert!(result.stdout.contains(
        "error[field-leak]: `RunQueue::items` is known outside `RunQueue`: 1 place in 1 module"
    ));
    // The property getter hands the list out: `app/queue/__init__.py` and
    // `app/__init__.py` re-export the class the consumer imports it from.
    assert!(result.stdout.contains(
        "    app.queue.ring.RunQueue\n\
         \x20     items ◀━━━ 1  app.consumer\n\
         \x20     kept: _limit\n"
    ));
    assert!(
        result.stdout.contains("through `contents`"),
        "{}",
        result.stdout
    );
    // `tests/test_queue.py` writes the field too, but test code never counts.
    assert!(
        !result.stdout.contains("test_queue.py"),
        "{}",
        result.stdout
    );
    assert_eq!(
        keys(&workspace),
        [
            "field-leak app.queue.ring.RunQueue::items",
            "module-cycle app.a,app.b",
            "tangle app.a.Alpha,app.b.Beta",
        ]
    );
}

#[test]
fn a_language_or_backend_that_cannot_serve_the_workspace_is_an_error() {
    let typescript = workspace("typescript", None, "treesitter-typescript-errors");
    let missing = run(&typescript, &["--lang", "rust"]);
    assert_eq!(missing.code, Some(2));
    assert!(
        missing.stderr.contains("no rust workspace found"),
        "{}",
        missing.stderr
    );

    let scip = run_raw(&typescript, &["--lang", "ts"]);
    assert_eq!(scip.code, Some(2));
    assert!(
        scip.stderr.contains("needs '--backend tree-sitter'"),
        "{}",
        scip.stderr
    );

    let unknown = run_raw(&typescript, &["--lang", "cobol"]);
    assert_eq!(unknown.code, Some(2));
    assert!(
        unknown.stderr.contains("'cobol' is not a language"),
        "{}",
        unknown.stderr
    );
}
