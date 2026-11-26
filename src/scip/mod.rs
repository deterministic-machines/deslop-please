//! A SCIP index (github.com/sourcegraph/scip, `scip.proto`): the documents, and
//! in each every name written there with the symbol it resolves to. Only the
//! fields this tool reads are decoded; the rest are skipped.

mod protobuf;
mod symbol;

pub use protobuf::Malformed;
pub use symbol::{Impl, Kind, Place, Symbol};

use std::borrow::Cow;

use protobuf::{Fields, Value};

pub struct Index<'a> {
    pub documents: Vec<Document<'a>>,
}

pub struct Document<'a> {
    /// Relative to the indexed workspace's root, `/`-separated on every
    /// platform: see [`with_slashes`].
    pub path: Cow<'a, str>,
    pub occurrences: Vec<Occurrence<'a>>,
    /// What the document says about each symbol it defines.
    pub symbols: Vec<SymbolInformation<'a>>,
}

pub struct SymbolInformation<'a> {
    pub symbol: &'a str,
    /// `scip.proto`'s `SymbolInformation.Kind`: what the symbol is (struct, field, method...).
    pub kind: u64,
    /// The declaration as written, body left out: `pub(crate) fn serve(&self)`, `pub mod log`.
    pub signature: &'a str,
}

pub struct Occurrence<'a> {
    pub range: Range,
    pub symbol: &'a str,
    pub definition: bool,
    /// For a definition: the whole item, body included.
    pub enclosing: Option<Range>,
}

/// Zero-based lines and columns; `end` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn holds(self, other: Range) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

// scip.proto's field numbers.
const INDEX_DOCUMENTS: u32 = 2;
const DOCUMENT_RELATIVE_PATH: u32 = 1;
const DOCUMENT_OCCURRENCES: u32 = 2;
const DOCUMENT_SYMBOLS: u32 = 3;
const DOCUMENT_TEXT: u32 = 5;
const SYMBOL_INFORMATION_SYMBOL: u32 = 1;
const SYMBOL_INFORMATION_KIND: u32 = 5;
const SYMBOL_INFORMATION_SIGNATURE: u32 = 7;
const OCCURRENCE_RANGE: u32 = 1;
const OCCURRENCE_SYMBOL: u32 = 2;
const OCCURRENCE_SYMBOL_ROLES: u32 = 3;
const OCCURRENCE_ENCLOSING_RANGE: u32 = 7;
const OCCURRENCE_SINGLE_LINE_RANGE: u32 = 8;
const OCCURRENCE_MULTI_LINE_RANGE: u32 = 9;
const OCCURRENCE_SINGLE_LINE_ENCLOSING_RANGE: u32 = 10;
const OCCURRENCE_MULTI_LINE_ENCLOSING_RANGE: u32 = 11;
const ROLE_DEFINITION: u64 = 1;

pub fn read(bytes: &[u8]) -> Result<Index<'_>, Malformed> {
    let mut documents = Vec::new();
    for field in Fields::new(bytes) {
        if let (INDEX_DOCUMENTS, value) = field? {
            documents.push(document(protobuf::bytes(value)?)?);
        }
    }
    Ok(Index { documents })
}

fn document(bytes: &[u8]) -> Result<Document<'_>, Malformed> {
    let mut path = "";
    let mut occurrences = Vec::new();
    let mut symbols = Vec::new();
    for field in Fields::new(bytes) {
        match field? {
            (DOCUMENT_RELATIVE_PATH, value) => path = protobuf::string(value)?,
            (DOCUMENT_OCCURRENCES, value) => occurrences.push(occurrence(protobuf::bytes(value)?)?),
            (DOCUMENT_SYMBOLS, value) => {
                symbols.push(symbol_information(protobuf::bytes(value)?)?);
            }
            _ => {}
        }
    }
    Ok(Document {
        path: with_slashes(path, std::path::MAIN_SEPARATOR),
        occurrences,
        symbols,
    })
}

/// `path`, written with the platform's `separator`, with `/` in its place.
/// `scip.proto` asks for `/` "including on Windows", but rust-analyzer writes
/// `\` there. Only the platform's separator is replaced: elsewhere `\` is a
/// character a file's name can hold.
pub fn with_slashes(path: &str, separator: char) -> Cow<'_, str> {
    if separator != '/' && path.contains(separator) {
        Cow::Owned(path.replace(separator, "/"))
    } else {
        Cow::Borrowed(path)
    }
}

fn symbol_information(bytes: &[u8]) -> Result<SymbolInformation<'_>, Malformed> {
    let mut information = SymbolInformation {
        symbol: "",
        kind: 0,
        signature: "",
    };
    for field in Fields::new(bytes) {
        match field? {
            (SYMBOL_INFORMATION_SYMBOL, value) => information.symbol = protobuf::string(value)?,
            (SYMBOL_INFORMATION_KIND, Value::Varint(kind)) => information.kind = kind,
            (SYMBOL_INFORMATION_SIGNATURE, value) => {
                for field in Fields::new(protobuf::bytes(value)?) {
                    if let (DOCUMENT_TEXT, text) = field? {
                        information.signature = protobuf::string(text)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(information)
}

fn occurrence(bytes: &[u8]) -> Result<Occurrence<'_>, Malformed> {
    let mut range = Vec::new();
    let mut enclosing = Vec::new();
    let mut typed_range = None;
    let mut typed_enclosing = None;
    let mut symbol = "";
    let mut roles = 0;
    for field in Fields::new(bytes) {
        match field? {
            (OCCURRENCE_RANGE, value) => protobuf::integers(value, &mut range)?,
            (OCCURRENCE_SYMBOL, value) => symbol = protobuf::string(value)?,
            (OCCURRENCE_SYMBOL_ROLES, Value::Varint(value)) => roles = value,
            (OCCURRENCE_ENCLOSING_RANGE, value) => protobuf::integers(value, &mut enclosing)?,
            (OCCURRENCE_SINGLE_LINE_RANGE, value) => {
                typed_range = Some(typed_range_from(protobuf::bytes(value)?, false)?);
            }
            (OCCURRENCE_MULTI_LINE_RANGE, value) => {
                typed_range = Some(typed_range_from(protobuf::bytes(value)?, true)?);
            }
            (OCCURRENCE_SINGLE_LINE_ENCLOSING_RANGE, value) => {
                typed_enclosing = Some(typed_range_from(protobuf::bytes(value)?, false)?);
            }
            (OCCURRENCE_MULTI_LINE_ENCLOSING_RANGE, value) => {
                typed_enclosing = Some(typed_range_from(protobuf::bytes(value)?, true)?);
            }
            _ => {}
        }
    }
    Ok(Occurrence {
        range: typed_range
            .or_else(|| to_range(&range))
            .ok_or(Malformed("an occurrence's range"))?,
        symbol,
        definition: roles & ROLE_DEFINITION != 0,
        enclosing: typed_enclosing.or_else(|| to_range(&enclosing)),
    })
}

/// Proto3 omits fields whose value is zero; typed ranges must default them.
fn typed_range_from(bytes: &[u8], multiline: bool) -> Result<Range, Malformed> {
    let mut parts = [0_u64; 4];
    for field in Fields::new(bytes) {
        if let (number @ 1..=4, Value::Varint(value)) = field? {
            parts[number as usize - 1] = value;
        }
    }
    let numbers = if multiline { &parts[..4] } else { &parts[..3] };
    to_range(numbers).ok_or(Malformed("a typed occurrence range"))
}

/// SCIP writes `[line, start, end]` for a range on one line, `[start line, start, end line, end]` otherwise.
fn to_range(numbers: &[u64]) -> Option<Range> {
    let numbers: Vec<u32> = numbers
        .iter()
        .map(|&n| u32::try_from(n).ok())
        .collect::<Option<_>>()?;
    let at = |line, column| Position { line, column };
    match numbers[..] {
        [line, start, end] => Some(Range {
            start: at(line, start),
            end: at(line, end),
        }),
        [start_line, start, end_line, end] => Some(Range {
            start: at(start_line, start),
            end: at(end_line, end),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ranges_override_legacy_and_default_zero_fields() {
        // Legacy range [7, 1, 2], then a single-line typed range [0, 0, 4]
        // and a multiline enclosing range [0, 0, 2, 3].
        let bytes = [
            0x0a, 3, 7, 1, 2, 0x42, 2, 0x18, 4, 0x5a, 4, 0x18, 2, 0x20, 3,
        ];
        let occurrence = occurrence(&bytes).unwrap();
        assert_eq!(occurrence.range, to_range(&[0, 0, 4]).unwrap());
        assert_eq!(occurrence.enclosing, to_range(&[0, 0, 2, 3]));
    }

    #[test]
    fn a_path_has_slashes_whichever_separator_its_platform_writes() {
        assert_eq!(with_slashes(r"src\lint\a.rs", '\\'), "src/lint/a.rs");
        assert_eq!(with_slashes("src/lint/a.rs", '\\'), "src/lint/a.rs");
        assert_eq!(with_slashes("src/lint/a.rs", '/'), "src/lint/a.rs");
        // A Unix file's name can hold a backslash.
        assert_eq!(with_slashes(r"src/a\b.rs", '/'), r"src/a\b.rs");
        assert!(matches!(
            with_slashes("src/lib.rs", '\\'),
            Cow::Borrowed("src/lib.rs")
        ));
    }
}
