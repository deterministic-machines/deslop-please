//! `#[cfg(..)]` evaluation and Cargo feature resolution for Rust sources.
//!
//! Every predicate is evaluated twice: once for a normal (non-test) build and
//! once for a test build. A module that only the test build compiles is test
//! code; a module no build compiles is skipped entirely. Unknown predicates are
//! assumed to hold so that code gated by a build script's `--cfg` is still
//! seen.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::configuration::Configuration;

/// Whether code behind an attribute is built normally and/or for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cfg {
    /// Built when the crate is built normally.
    pub normal: bool,
    /// Built when the crate is built for tests.
    pub test: bool,
}

impl Cfg {
    pub(crate) const ALWAYS: Cfg = Cfg {
        normal: true,
        test: true,
    };
    pub(crate) const NEVER: Cfg = Cfg {
        normal: false,
        test: false,
    };
    /// `cfg(test)`: built only when testing.
    pub(crate) const FOR_TESTS: Cfg = Cfg {
        normal: false,
        test: true,
    };

    pub(crate) fn and(self, other: Cfg) -> Cfg {
        Cfg {
            normal: self.normal && other.normal,
            test: self.test && other.test,
        }
    }

    pub(crate) fn or(self, other: Cfg) -> Cfg {
        Cfg {
            normal: self.normal || other.normal,
            test: self.test || other.test,
        }
    }

    pub(crate) fn negate(self) -> Cfg {
        Cfg {
            normal: !self.normal,
            test: !self.test,
        }
    }

    /// Built in at least one of the two configurations.
    pub(crate) fn built(self) -> bool {
        self.normal || self.test
    }

    /// Built only when testing.
    pub(crate) fn only_for_tests(self) -> bool {
        !self.normal && self.test
    }

    pub(crate) fn from_bool(value: bool) -> Cfg {
        if value { Cfg::ALWAYS } else { Cfg::NEVER }
    }
}

/// One attribute, decoded from `#[..]` without its delimiters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attr {
    /// Attribute path, e.g. `cfg`, `path`, `test`, `tokio::test`, `cfg_attr`.
    pub name: String,
    /// `name = "value"`.
    pub value: Option<String>,
    /// `name(..)` arguments, exactly as written.
    pub args: Option<String>,
}

impl Attr {
    /// Decode the text of one attribute: `cfg(test)`, `path = "x.rs"`, `test`.
    pub(crate) fn parse(text: &str) -> Option<Attr> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let name_end = text.find(['(', '=']).unwrap_or(text.len());
        let name = text[..name_end].trim();
        if name.is_empty() {
            return None;
        }
        let rest = &text[name_end..];
        if let Some(value) = rest.strip_prefix('=') {
            return Some(Attr {
                name: name.to_owned(),
                value: Some(unquote(value.trim()).to_owned()),
                args: None,
            });
        }
        if let Some(inner) = rest.strip_prefix('(') {
            let Some(inner) = inner.strip_suffix(')') else {
                return Some(Attr {
                    name: name.to_owned(),
                    value: None,
                    args: None,
                });
            };
            return Some(Attr {
                name: name.to_owned(),
                value: None,
                args: Some(inner.trim().to_owned()),
            });
        }
        Some(Attr {
            name: name.to_owned(),
            value: None,
            args: None,
        })
    }

    /// Attributes that mark test code: `#[test]`, `#[tokio::test]`, ...
    pub(crate) fn is_test(&self) -> bool {
        self.name == "test" || self.name.rsplit("::").next() == Some("test")
    }
}

/// What `#[cfg(..)]` and `#[cfg_attr(..)]` see: the target and enabled features.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CfgContext {
    pub target_os: String,
    pub target_family: String,
    pub target_arch: String,
    pub target_pointer_width: String,
    pub target_vendor: String,
    pub target_endian: String,
    pub features: BTreeSet<String>,
}

impl CfgContext {
    pub(crate) fn new(cargo: &Configuration, features: BTreeSet<String>) -> CfgContext {
        let (arch, vendor, width, endian) = target_details(cargo);
        CfgContext {
            target_os: cargo.target_os().to_owned(),
            target_family: cargo.target_family().to_owned(),
            target_arch: arch,
            target_pointer_width: width,
            target_vendor: vendor,
            target_endian: endian,
            features,
        }
    }

    /// The value of one cfg key, for the normal and test configurations.
    fn key(&self, name: &str) -> Cfg {
        match name {
            "test" => Cfg::FOR_TESTS,
            "unix" => Cfg::from_bool(self.target_family == "unix"),
            "windows" => Cfg::from_bool(self.target_family == "windows"),
            "debug_assertions" | "target_thread_local" | "target_has_atomic" => Cfg::ALWAYS,
            "miri" | "clippy" | "docsrs" | "doc" | "doctest" | "rust_analyzer" => Cfg::NEVER,
            // A cfg a build script sets, or an unknown one: assume it holds.
            _ => Cfg::ALWAYS,
        }
    }

    /// The value of `name = "value"`.
    fn value(&self, name: &str, value: &str) -> Cfg {
        match name {
            "feature" => Cfg::from_bool(self.features.contains(value)),
            "target_os" => Cfg::from_bool(self.target_os == value),
            "target_family" => Cfg::from_bool(self.target_family == value),
            "target_arch" => Cfg::from_bool(self.target_arch == value),
            "target_pointer_width" => Cfg::from_bool(self.target_pointer_width == value),
            "target_vendor" => Cfg::from_bool(self.target_vendor == value),
            "target_endian" => Cfg::from_bool(self.target_endian == value),
            "panic" => Cfg::from_bool(value == "unwind"),
            "target_feature" => Cfg::ALWAYS,
            _ => Cfg::ALWAYS,
        }
    }

    /// Evaluate the argument text of one `cfg(..)` attribute.
    pub(crate) fn eval(&self, args: &str) -> Cfg {
        let mut parser = Predicates {
            bytes: args.as_bytes(),
            at: 0,
        };
        match parser.expression(self) {
            Some(cfg) => cfg,
            // Malformed or empty: include rather than lose code.
            None => Cfg::ALWAYS,
        }
    }
}

/// The text after the last element of a comma-separated list at depth zero.
fn split_first(args: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (at, ch) in args.char_indices() {
        match ch {
            '"' if quote.is_none() => quote = Some('"'),
            '"' if quote == Some('"') => quote = None,
            '(' | '[' | '{' if quote.is_none() => depth += 1,
            ')' | ']' | '}' if quote.is_none() => depth -= 1,
            ',' if quote.is_none() && depth == 0 => {
                return Some((args[..at].trim(), args[at + 1..].trim()));
            }
            _ => {}
        }
    }
    None
}

/// The `#[cfg(..)]` attributes of `attrs`, ANDed together.
pub(crate) fn of_attrs(attrs: &[Attr], ctx: &CfgContext) -> Cfg {
    let mut cfg = Cfg::ALWAYS;
    for attr in attrs {
        if attr.name != "cfg" {
            continue;
        }
        let Some(args) = &attr.args else {
            continue;
        };
        cfg = cfg.and(ctx.eval(args));
    }
    cfg
}

/// `#[test]`, `#[cfg(test)]`, ... among `attrs`.
pub(crate) fn marks_test(attrs: &[Attr], ctx: &CfgContext) -> bool {
    attrs.iter().any(Attr::is_test) || of_attrs(attrs, ctx).only_for_tests()
}

/// The active `#[path = ".."]` attribute of `attrs`, whether written directly
/// or through an active `#[cfg_attr(.., path = "..")]`.
pub(crate) fn path_of(attrs: &[Attr], ctx: &CfgContext) -> Option<String> {
    for attr in attrs {
        if attr.name == "path"
            && let Some(value) = &attr.value
        {
            return Some(value.clone());
        }
        if attr.name == "cfg_attr" {
            let Some(args) = &attr.args else {
                continue;
            };
            let Some((condition, rest)) = split_first(args) else {
                continue;
            };
            if !ctx.eval(condition).normal {
                continue;
            }
            let Some(inner) = Attr::parse(rest) else {
                continue;
            };
            if inner.name == "path" {
                return inner.value;
            }
        }
    }
    None
}

/// The attributes written before `node`, comments aside, in source order.
pub(crate) fn attributes_of(node: tree_sitter::Node<'_>, source: &[u8]) -> Vec<Attr> {
    let mut attributes = Vec::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(current) = sibling {
        match current.kind() {
            "attribute_item" => {
                if let Some(inner) = current.named_child(0)
                    && let Ok(text) = inner.utf8_text(source)
                    && let Some(attr) = Attr::parse(text)
                {
                    attributes.push(attr);
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sibling = current.prev_named_sibling();
    }
    attributes.reverse();
    attributes
}

/// A tiny recursive-descent parser for cfg predicates.
struct Predicates<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Predicates<'a> {
    fn expression(&mut self, ctx: &CfgContext) -> Option<Cfg> {
        self.space();
        let name = self.ident()?;
        self.space();
        match self.bytes.get(self.at).copied() {
            Some(b'=') => {
                self.at += 1;
                self.space();
                let value = self.string()?;
                Some(ctx.value(&name, &value))
            }
            Some(b'(') => {
                self.at += 1;
                let mut parts = Vec::new();
                loop {
                    self.space();
                    if self.bytes.get(self.at).copied() == Some(b')') {
                        self.at += 1;
                        break;
                    }
                    parts.push(self.expression(ctx)?);
                    self.space();
                    match self.bytes.get(self.at).copied() {
                        Some(b',') => self.at += 1,
                        Some(b')') => {
                            self.at += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
                Some(match name.as_str() {
                    "all" => parts.into_iter().fold(Cfg::ALWAYS, Cfg::and),
                    "any" => parts.into_iter().fold(Cfg::NEVER, Cfg::or),
                    "not" => match parts.as_slice() {
                        [one] => one.negate(),
                        _ => Cfg::ALWAYS,
                    },
                    _ => Cfg::ALWAYS,
                })
            }
            _ => Some(ctx.key(&name)),
        }
    }

    fn space(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.space();
        let start = self.at;
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            self.at += 1;
        }
        (self.at > start).then(|| String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned())
    }

    fn string(&mut self) -> Option<String> {
        if self.bytes.get(self.at).copied() != Some(b'"') {
            return None;
        }
        self.at += 1;
        let start = self.at;
        while self.bytes.get(self.at).copied().is_some_and(|b| b != b'"') {
            self.at += 1;
        }
        let value = String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned();
        self.at += 1;
        Some(value)
    }
}

/// The best guesses for a target's architecture, vendor, pointer width and endianness.
fn target_details(cargo: &Configuration) -> (String, String, String, String) {
    let triple = cargo.target_triple().unwrap_or("").to_ascii_lowercase();
    let parts: Vec<&str> = triple.split('-').collect();
    let arch = parts
        .first()
        .copied()
        .filter(|arch| !arch.is_empty())
        .map_or_else(|| std::env::consts::ARCH.to_owned(), str::to_owned);
    let vendor = parts.get(1).copied().unwrap_or("unknown").to_owned();
    let width = if arch.contains("64") || matches!(arch.as_str(), "x86_64" | "aarch64") {
        "64"
    } else if arch.contains("32")
        || matches!(arch.as_str(), "x86" | "i386" | "i586" | "i686" | "arm")
    {
        "32"
    } else if std::env::consts::ARCH.contains("64") {
        "64"
    } else {
        "32"
    };
    let endian = if arch.starts_with("mips") && !arch.starts_with("mipsel")
        || arch.starts_with("powerpc") && !arch.starts_with("powerpc64le")
        || arch.starts_with("s390")
        || arch.starts_with("sparc")
    {
        "big"
    } else {
        "little"
    };
    (arch, vendor, width.to_owned(), endian.to_owned())
}

/// Strip the quotes of a string literal, including raw strings.
fn unquote(value: &str) -> &str {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('"') {
        return rest.strip_suffix('"').unwrap_or(rest);
    }
    if let Some(rest) = value.strip_prefix('r') {
        let hashes = rest.len() - rest.trim_start_matches('#').len();
        let rest = &rest[hashes..];
        if let Some(rest) = rest.strip_prefix('"') {
            let closing = format!("\"{}", "#".repeat(hashes));
            if let Some(inner) = rest.strip_suffix(closing.as_str()) {
                return inner;
            }
        }
    }
    value
}

/// The features enabled for a package whose manifest is `manifest`.
pub(crate) fn enabled_features(manifest: &toml::Value, cargo: &Configuration) -> BTreeSet<String> {
    let table = manifest
        .get("features")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let entries: BTreeMap<String, Vec<String>> = table
        .iter()
        .map(|(name, list)| {
            let list = list
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            (name.clone(), list)
        })
        .collect();

    let mut enabled = BTreeSet::new();
    let mut queue: Vec<String> = Vec::new();
    if !cargo.no_default_features_selected() && entries.contains_key("default") {
        queue.push("default".to_owned());
    }
    queue.extend(cargo.feature_names().iter().cloned());
    if cargo.all_features_selected() {
        queue.extend(entries.keys().cloned());
    }
    while let Some(feature) = queue.pop() {
        if !enabled.insert(feature.clone()) {
            continue;
        }
        let Some(list) = entries.get(&feature) else {
            continue;
        };
        for entry in list {
            if entry.contains('/') || entry.starts_with("dep:") {
                continue;
            }
            if !enabled.contains(entry) {
                queue.push(entry.clone());
            }
        }
    }
    enabled
}

thread_local! {
    static FEATURES: RefCell<BTreeMap<(PathBuf, String), BTreeSet<String>>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// The features enabled for the package that owns `file`.
pub(crate) fn features_for_file(file: &Path, cargo: &Configuration) -> BTreeSet<String> {
    let Some(manifest) = nearest_manifest(file) else {
        return cargo.feature_names().clone();
    };
    let key = format!("{cargo}");
    FEATURES.with(|cache| {
        if let Some(features) = cache.borrow().get(&(manifest.clone(), key.clone())) {
            return features.clone();
        }
        let features = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
            .map_or_else(
                || cargo.feature_names().clone(),
                |value| enabled_features(&value, cargo),
            );
        cache.borrow_mut().insert((manifest, key), features.clone());
        features
    })
}

/// The closest `Cargo.toml` above `file`.
pub(crate) fn nearest_manifest(file: &Path) -> Option<PathBuf> {
    let mut directory = file.parent();
    while let Some(current) = directory {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        directory = current.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(
        target: Option<&str>,
        features: &[&str],
        all: bool,
        no_default: bool,
    ) -> Configuration {
        let mut configuration = Configuration::default();
        if let Some(target) = target {
            configuration.target(target).unwrap();
        }
        configuration.features(&features.join(","));
        if all {
            configuration.all_features();
        }
        if no_default {
            configuration.no_default_features();
        }
        configuration
    }

    fn linux(features: &[&str]) -> CfgContext {
        let cargo = config(Some("x86_64-unknown-linux-gnu"), features, false, false);
        let features = cargo.feature_names().clone();
        CfgContext::new(&cargo, features)
    }

    fn cfg_of(text: &str) -> Cfg {
        let context = linux(&["x"]);
        let attribute = Attr::parse(text).expect("attribute");
        of_attrs(&[attribute], &context)
    }

    #[test]
    fn attributes_are_decoded_without_their_delimiters() {
        assert_eq!(
            Attr::parse("cfg(test)"),
            Some(Attr {
                name: "cfg".into(),
                value: None,
                args: Some("test".into())
            })
        );
        assert_eq!(
            Attr::parse("path = \"src/moved.rs\""),
            Some(Attr {
                name: "path".into(),
                value: Some("src/moved.rs".into()),
                args: None
            })
        );
        assert_eq!(
            Attr::parse("derive(Debug, Clone)"),
            Some(Attr {
                name: "derive".into(),
                value: None,
                args: Some("Debug, Clone".into())
            })
        );
        assert_eq!(
            Attr::parse("test"),
            Some(Attr {
                name: "test".into(),
                value: None,
                args: None
            })
        );
        assert_eq!(
            Attr::parse("doc = \"a(b)\""),
            Some(Attr {
                name: "doc".into(),
                value: Some("a(b)".into()),
                args: None
            })
        );
        assert_eq!(
            Attr::parse("path = r\"raw/path.rs\""),
            Some(Attr {
                name: "path".into(),
                value: Some("raw/path.rs".into()),
                args: None
            })
        );
        assert_eq!(Attr::parse(""), None);
        assert_eq!(
            Attr::parse("cfg(test"),
            Some(Attr {
                name: "cfg".into(),
                value: None,
                args: None
            })
        );
    }

    #[test]
    fn test_attributes_are_recognized_however_they_are_named() {
        assert!(Attr::parse("test").unwrap().is_test());
        assert!(Attr::parse("tokio::test").unwrap().is_test());
        assert!(!Attr::parse("cfg(test)").unwrap().is_test());
        assert!(!Attr::parse("derive(Test)").unwrap().is_test());
    }

    #[test]
    fn target_predicates_hold_for_the_selected_target_only() {
        assert_eq!(cfg_of("cfg(unix)"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(windows)"), Cfg::NEVER);
        assert_eq!(cfg_of("cfg(target_os = \"linux\")"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(target_os = \"macos\")"), Cfg::NEVER);
        assert_eq!(cfg_of("cfg(target_family = \"unix\")"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(target_family = \"windows\")"), Cfg::NEVER);
        assert_eq!(cfg_of("cfg(target_arch = \"x86_64\")"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(target_pointer_width = \"64\")"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(target_endian = \"little\")"), Cfg::ALWAYS);
    }

    #[test]
    fn combinators_are_evaluated_for_the_normal_and_the_test_build() {
        assert_eq!(
            cfg_of("cfg(all(unix, target_arch = \"x86_64\"))"),
            Cfg::ALWAYS
        );
        assert_eq!(cfg_of("cfg(any(windows, unix))"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(not(windows))"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(any())"), Cfg::NEVER);
        assert_eq!(cfg_of("cfg(all())"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(test)"), Cfg::FOR_TESTS);
        assert_eq!(cfg_of("cfg(all(test, unix))"), Cfg::FOR_TESTS);
        assert_eq!(cfg_of("cfg(any(test, windows))"), Cfg::FOR_TESTS);
        assert_eq!(
            cfg_of("cfg(not(test))"),
            Cfg {
                normal: true,
                test: false
            }
        );
        assert_eq!(cfg_of("cfg(unknown_cfg)"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(miri)"), Cfg::NEVER);
        assert_eq!(cfg_of("cfg(not(miri))"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(test"), Cfg::ALWAYS);
    }

    #[test]
    fn feature_predicates_follow_the_enabled_features() {
        assert_eq!(cfg_of("cfg(feature = \"x\")"), Cfg::ALWAYS);
        assert_eq!(cfg_of("cfg(feature = \"export\")"), Cfg::NEVER);
        assert_eq!(
            cfg_of("cfg(any(feature = \"export\", feature = \"x\"))"),
            Cfg::ALWAYS
        );
    }

    #[test]
    fn a_cfg_test_predicate_marks_test_code_and_never_skips_it() {
        let context = linux(&[]);
        let attributes = [Attr::parse("cfg(test)").unwrap()];
        assert!(marks_test(&attributes, &context));
        let attributes = [Attr::parse("test").unwrap()];
        assert!(marks_test(&attributes, &context));
        let attributes = [Attr::parse("cfg(unix)").unwrap()];
        assert!(!marks_test(&attributes, &context));
        let mut windows = Configuration::default();
        windows.target("x86_64-pc-windows-msvc").unwrap();
        let context = CfgContext::new(&windows, BTreeSet::new());
        assert_eq!(context.eval("windows"), Cfg::ALWAYS);
        assert_eq!(context.eval("unix"), Cfg::NEVER);
        assert_eq!(context.eval("test"), Cfg::FOR_TESTS);
    }

    #[test]
    fn a_path_attribute_is_read_directly_or_through_an_active_cfg_attr() {
        let context = linux(&[]);
        let path = |text: &str| {
            let attribute = Attr::parse(text).expect("attribute");
            path_of(&[attribute], &context)
        };
        assert_eq!(path("path = \"x.rs\"").as_deref(), Some("x.rs"));
        assert_eq!(
            path("cfg_attr(unix, path = \"x.rs\")").as_deref(),
            Some("x.rs")
        );
        assert_eq!(path("cfg_attr(windows, path = \"x.rs\")"), None);
        assert_eq!(path("cfg_attr(test, path = \"x.rs\")"), None);
        assert_eq!(path("inline"), None);
    }

    #[test]
    fn enabled_features_expand_defaults_requested_features_and_all_features() {
        let manifest: toml::Value = toml::from_str(
            "[features]\ndefault = [\"metrics\"]\nmetrics = []\nexport = []\nalias = [\"dep:serde\", \"pkg/feat\", \"extra\"]\nextra = []\n",
        )
        .unwrap();
        let features = |cargo: &Configuration| enabled_features(&manifest, cargo);
        let names = |cargo: &Configuration| features(cargo).into_iter().collect::<Vec<_>>();
        assert_eq!(names(&Configuration::default()), ["default", "metrics"]);
        let mut no_default = Configuration::default();
        no_default.no_default_features();
        assert_eq!(names(&no_default), Vec::<String>::new());
        let mut requested = Configuration::default();
        requested.features("export");
        assert_eq!(names(&requested), ["default", "export", "metrics"]);
        let mut all = Configuration::default();
        all.all_features();
        assert_eq!(
            names(&all),
            ["alias", "default", "export", "extra", "metrics"]
        );
        // `dep:` and `pkg/feat` entries enable no feature of this crate.
        assert!(features(&all).contains("extra"));
    }

    #[test]
    fn features_for_a_file_come_from_its_package_manifest() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\n\n[features]\ndefault = [\"on\"]\non = []\noff = []\n",
        )
        .unwrap();
        let file = directory.path().join("src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();
        let cargo = Configuration::default();
        assert_eq!(
            features_for_file(&file, &cargo)
                .into_iter()
                .collect::<Vec<_>>(),
            ["default", "on"]
        );
        // The cache answers the same way on the second call.
        assert!(features_for_file(&file, &cargo).contains("on"));
    }
}
