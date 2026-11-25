//! `rustc`/`clippy`-style diagnostic formatting, in color when stdout is a
//! terminal, with a picture under each report.

mod picture;

use std::fmt::Write;
use std::io::IsTerminal;

use serde_json::{Value, json};

use crate::TOOL;
use crate::lint::Diagnostic;

pub fn render(
    diagnostics: &[Diagnostic],
    fixed_allowed: usize,
    allow_path: Option<&str>,
) -> String {
    let paint = Paint(stdout_has_color());
    let mut out = String::new();
    for diag in diagnostics {
        let _ = writeln!(
            out,
            "{}{}",
            paint.error(&format!("error[{}]", diag.lint.name())),
            paint.bold(&format!(": {}", diag.message))
        );
        if let Some(loc) = &diag.location {
            // rustc shape (`file:line:column`) so IDE terminals link-navigate.
            let _ = writeln!(out, "  {} {}:{}:1", paint.gutter("-->"), loc.file, loc.line);
        }
        for note in &diag.notes {
            let _ = writeln!(
                out,
                "  {} {} {note}",
                paint.gutter("="),
                paint.bold("note:")
            );
        }
        for line in picture::draw(&diag.picture, &paint) {
            let _ = writeln!(out, "{line}");
        }
        out.push('\n');
    }

    if fixed_allowed > 0
        && let Some(path) = allow_path
    {
        let noun = if fixed_allowed == 1 {
            "entry"
        } else {
            "entries"
        };
        let _ = writeln!(
            out,
            "{} {fixed_allowed} allowed {noun} in {path} are now fixed (run `{TOOL} --update-allow {path}` to tighten it)",
            paint.bold("note:")
        );
    }

    if diagnostics.is_empty() {
        let _ = writeln!(out, "{TOOL}: {}", paint.ok("ok"));
    } else {
        let noun = if diagnostics.len() == 1 {
            "error"
        } else {
            "errors"
        };
        let count = format!("{} {noun}", diagnostics.len());
        let _ = writeln!(out, "{TOOL}: {}, well done!", paint.error(&count));
    }
    out
}

/// Like cargo: `CARGO_TERM_COLOR=always|never` decides, then `NO_COLOR`, then
/// whether stdout is a terminal.
fn stdout_has_color() -> bool {
    match std::env::var("CARGO_TERM_COLOR").as_deref() {
        Ok("always") => true,
        Ok("never") => false,
        _ => {
            std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
                && std::io::stdout().is_terminal()
        }
    }
}

pub fn render_json(
    diagnostics: &[Diagnostic],
    fixed_allowed: usize,
    allow_path: Option<&str>,
) -> String {
    let diagnostics: Vec<Value> = diagnostics.iter().map(diagnostic_json).collect();
    let mut out = json!({
        "ok": diagnostics.is_empty(),
        "count": diagnostics.len(),
        "fixed_allowed": fixed_allowed,
        "diagnostics": diagnostics,
    });
    if let Some(path) = allow_path {
        out["allow_path"] = json!(path);
    }
    serde_json::to_string_pretty(&out).expect("diagnostics serialize to JSON") + "\n"
}

fn diagnostic_json(diagnostic: &Diagnostic) -> Value {
    let mut out = json!({
        "lint": diagnostic.lint.name(),
        "key": diagnostic.key,
        "message": diagnostic.message,
        "notes": diagnostic.notes,
    });
    if let Some(location) = &diagnostic.location {
        out["file"] = json!(location.file);
        out["line"] = json!(location.line);
    } else {
        out["file"] = Value::Null;
        out["line"] = Value::Null;
    }
    out
}

/// rustc's colors: bold red errors, bold blue gutter, bold green success.
struct Paint(bool);

impl Paint {
    fn error(&self, text: &str) -> String {
        self.sgr("1;31", text)
    }

    fn gutter(&self, text: &str) -> String {
        self.sgr("1;34", text)
    }

    fn ok(&self, text: &str) -> String {
        self.sgr("1;32", text)
    }

    fn bold(&self, text: &str) -> String {
        self.sgr("1", text)
    }

    fn sgr(&self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}
