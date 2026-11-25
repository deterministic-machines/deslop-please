//! A Java file's text, read as tokens, as far as the model needs it: where
//! its `package` and `import` declarations are, what code does with a name it
//! writes, how a field's type is written, and the tokens of a method's body. Comments are left out; a
//! string, text block or character is one token.

use crate::scip::{Position, Range};

/// Punctuation of more than one character, longest first. `>>` is two
/// tokens, which close two generic argument lists.
const PUNCTUATION: [&str; 22] = [
    ">>>=", "<<=", ">>=", "...", "::", "->", "++", "--", "&&", "||", "==", "!=", "<=", ">=", "+=",
    "-=", "*=", "/=", "%=", "&=", "|=", "^=",
];

const ASSIGNMENTS: [&str; 12] = [
    "=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=", ">>>=",
];

const PRIMITIVES: [&str; 8] = [
    "boolean", "byte", "char", "short", "int", "long", "float", "double",
];

struct Token {
    start: Position,
    end: Position,
    /// Its bytes in the text.
    text: (usize, usize),
}

pub struct Source {
    text: String,
    tokens: Vec<Token>,
}

/// What code does with a name it writes, a field's or a method's it calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Assigns to it, or increments or decrements it.
    Write,
    /// Calls a method on it, reads one of its fields, or indexes it.
    Member,
    /// Takes its value.
    Value,
}

/// How a field's type is written, as far as what it holds can be told by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Primitive,
    /// A class, by the name it ends with, which the index resolves: `Status`,
    /// `java.util.UUID`, and `Class` in `Class<?>`, with `generic` set.
    Class {
        name: Range,
        generic: bool,
    },
    /// An array, a type variable's bound, or anything else.
    Other,
}

/// What a name is written on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qualifier {
    /// Nothing: a field or method of the class the code is in, or a local.
    None,
    /// `this.`
    This,
    /// Anything else: `queue.`, `Policy.`, `super.`.
    Other,
}

impl Source {
    pub fn new(text: String) -> Source {
        let tokens = read(&text);
        Source { text, tokens }
    }

    fn text(&self, at: usize) -> &str {
        let (start, end) = self.tokens[at].text;
        &self.text[start..end]
    }

    fn at(&self, range: Range) -> Option<usize> {
        let at = self.tokens.partition_point(|t| t.start < range.start);
        (at < self.tokens.len() && self.tokens[at].start == range.start).then_some(at)
    }

    /// The package the file declares, as its parts: `["com", "acme"]`.
    pub fn package(&self) -> Vec<String> {
        let Some(at) = (0..self.tokens.len()).find(|&at| self.text(at) == "package") else {
            return Vec::new();
        };
        (at + 1..self.tokens.len())
            .map(|at| self.text(at))
            .take_while(|&text| text != ";")
            .filter(|&text| text != ".")
            .map(str::to_owned)
            .collect()
    }

    /// Where each `package` and `import` declaration is: from its keyword to its `;`.
    pub fn declarations(&self) -> Vec<Range> {
        let mut ranges = Vec::new();
        for at in 0..self.tokens.len() {
            if !matches!(self.text(at), "package" | "import") {
                continue;
            }
            if let Some(end) = (at..self.tokens.len()).find(|&end| self.text(end) == ";") {
                ranges.push(Range {
                    start: self.tokens[at].start,
                    end: self.tokens[end].end,
                });
            }
        }
        ranges
    }

    /// What the code does with the name written at `range`: past the
    /// arguments of a call, what follows it, and what precedes the chain of
    /// names it ends.
    pub fn access(&self, range: Range) -> Access {
        let Some(at) = self.at(range) else {
            return Access::Value;
        };
        let mut next = at + 1;
        if next < self.tokens.len() && self.text(next) == "(" {
            next = self.closing(next) + 1;
        }
        let after = (next < self.tokens.len()).then(|| self.text(next));
        match after {
            Some(text) if ASSIGNMENTS.contains(&text) || text == "++" || text == "--" => {
                return Access::Write;
            }
            Some("." | "[") => return Access::Member,
            _ => {}
        }
        let mut start = at;
        while start >= 2 && self.text(start - 1) == "." && is_name(self.text(start - 2)) {
            start -= 2;
        }
        match start.checked_sub(1).map(|before| self.text(before)) {
            Some("++" | "--") => Access::Write,
            _ => Access::Value,
        }
    }

    /// What the name written at `range` is written on.
    pub fn qualifier(&self, range: Range) -> Qualifier {
        match self.at(range) {
            Some(at) if at >= 1 && self.text(at - 1) == "." => {
                if at >= 2 && self.text(at - 2) == "this" {
                    Qualifier::This
                } else {
                    Qualifier::Other
                }
            }
            _ => Qualifier::None,
        }
    }

    /// The tokens of the body of the method written at `item` and named at
    /// `name`, braces left out, if it has one.
    pub fn body(&self, item: Range, name: Range) -> Option<Vec<&str>> {
        let first = self.at(name)?;
        let last = self.tokens.partition_point(|t| t.end <= item.end);
        let parameters = (first..last).find(|&at| self.text(at) == "(")?;
        let after = self.closing(parameters) + 1;
        let open = (after..last).find(|&at| matches!(self.text(at), "{" | ";"))?;
        if self.text(open) != "{" {
            return None;
        }
        let close = self.closing(open);
        Some((open + 1..close).map(|at| self.text(at)).collect())
    }

    /// How the type of the field named at `name` is written: by the tokens
    /// before the name.
    pub fn type_before(&self, name: Range) -> Written {
        let Some(at) = self.at(name).and_then(|at| at.checked_sub(1)) else {
            return Written::Other;
        };
        let range = |at: usize| Range {
            start: self.tokens[at].start,
            end: self.tokens[at].end,
        };
        match self.text(at) {
            text if PRIMITIVES.contains(&text) => Written::Primitive,
            text if is_name(text) => Written::Class {
                name: range(at),
                generic: false,
            },
            ">" => {
                let mut depth = 0usize;
                for open in (0..=at).rev() {
                    match self.text(open) {
                        ">" => depth += 1,
                        "<" => depth -= 1,
                        _ => {}
                    }
                    if depth == 0 {
                        return match open.checked_sub(1) {
                            Some(head) if is_name(self.text(head)) => Written::Class {
                                name: range(head),
                                generic: true,
                            },
                            _ => Written::Other,
                        };
                    }
                }
                Written::Other
            }
            _ => Written::Other,
        }
    }

    /// The text of the token before the one at `range`: `enum` before the
    /// name of an enum.
    pub fn before(&self, range: Range) -> Option<&str> {
        let at = self.at(range)?.checked_sub(1)?;
        Some(self.text(at))
    }

    /// The bracket that closes the one at `open`, or the last token.
    fn closing(&self, open: usize) -> usize {
        let mut depth = 0usize;
        for at in open..self.tokens.len() {
            match self.text(at) {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => {
                    depth -= 1;
                    if depth == 0 {
                        return at;
                    }
                }
                _ => {}
            }
        }
        self.tokens.len() - 1
    }
}

fn is_name(text: &str) -> bool {
    text.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '$')
}

fn read(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut line = 0u32;
    let mut column = 0u32;
    // Where each char is: its byte, and its line and UTF-16 column.
    let mut places: Vec<(usize, Position)> = Vec::with_capacity(text.len() + 1);
    for (byte, c) in text.char_indices() {
        places.push((byte, Position { line, column }));
        if c == '\n' {
            line += 1;
            column = 0;
        } else {
            column += c.len_utf16() as u32;
        }
    }
    places.push((text.len(), Position { line, column }));
    let position = |byte: usize| places[places.partition_point(|&(at, _)| at < byte)].1;
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &text[at..];
        let c = rest.chars().next().expect("not past the end");
        let length = if c.is_whitespace() {
            at += c.len_utf8();
            continue;
        } else if rest.starts_with("//") {
            at += rest.find('\n').unwrap_or(rest.len());
            continue;
        } else if let Some(comment) = rest.strip_prefix("/*") {
            at += comment.find("*/").map_or(rest.len(), |end| end + 4);
            continue;
        } else if let Some(block) = rest.strip_prefix("\"\"\"") {
            block.find("\"\"\"").map_or(rest.len(), |end| end + 6)
        } else if c == '"' || c == '\'' {
            quoted(rest, c)
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len())
        } else {
            PUNCTUATION
                .iter()
                .find(|p| rest.starts_with(*p))
                .map_or(c.len_utf8(), |p| p.len())
        };
        tokens.push(Token {
            start: position(at),
            end: position(at + length),
            text: (at, at + length),
        });
        at += length;
    }
    tokens
}

/// The length of the string or character that opens `rest` with `quote`.
fn quoted(rest: &str, quote: char) -> usize {
    let mut escaped = false;
    for (at, c) in rest.char_indices().skip(1) {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '\n' => return at,
            _ if c == quote => return at + 1,
            _ => {}
        }
    }
    rest.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The range of the `nth` token whose text is `name`.
    fn find(source: &Source, name: &str, nth: usize) -> Range {
        let at = (0..source.tokens.len())
            .filter(|&at| source.text(at) == name)
            .nth(nth)
            .unwrap();
        Range {
            start: source.tokens[at].start,
            end: source.tokens[at].end,
        }
    }

    #[test]
    fn comments_strings_and_characters_hide_names() {
        let source = Source::new("a /* b */ c // d\n\"e \\\" f\" 'g' \"\"\"\nh\"\"\" i".into());
        let texts: Vec<&str> = (0..source.tokens.len()).map(|at| source.text(at)).collect();
        assert_eq!(
            texts,
            ["a", "c", "\"e \\\" f\"", "'g'", "\"\"\"\nh\"\"\"", "i"]
        );
        assert_eq!(source.tokens[5].start, Position { line: 2, column: 5 });
    }

    #[test]
    fn columns_count_utf16_units() {
        let source = Source::new("\"😀\" x".into());
        assert_eq!(source.tokens[1].start.column, 5);
    }

    #[test]
    fn the_package_and_where_declarations_are() {
        let source = Source::new(
            "package com.acme;\n\nimport java.util.List;\nimport static a.B.c;\nclass X {}".into(),
        );
        assert_eq!(source.package(), ["com", "acme"]);
        let declarations = source.declarations();
        assert_eq!(declarations.len(), 3);
        assert_eq!(declarations[1].start.line, 2);
        assert!(declarations[2].holds(find(&source, "c", 0)));
    }

    #[test]
    fn what_code_does_with_a_name() {
        let source = Source::new(
            "q.head = 1; q.head += 1; q.head++; ++q.head; ++head; x = q.items.size(); \
             y = q.items[0]; z = q.head; f(q.head); g = q.items(); h = q.items().size();"
                .into(),
        );
        let head = |nth| source.access(find(&source, "head", nth));
        assert_eq!(
            [
                head(0),
                head(1),
                head(2),
                head(3),
                head(4),
                head(5),
                head(6)
            ],
            [
                Access::Write,
                Access::Write,
                Access::Write,
                Access::Write,
                Access::Write,
                Access::Value,
                Access::Value
            ]
        );
        let items = |nth| source.access(find(&source, "items", nth));
        assert_eq!(
            [items(0), items(1), items(2), items(3)],
            [
                Access::Member,
                Access::Member,
                Access::Value,
                Access::Member
            ]
        );
    }

    #[test]
    fn what_a_name_is_written_on() {
        let source = Source::new("a(); this.b(); q.c(); super.d();".into());
        let qualifier = |name| source.qualifier(find(&source, name, 0));
        assert_eq!(qualifier("a"), Qualifier::None);
        assert_eq!(qualifier("b"), Qualifier::This);
        assert_eq!(qualifier("c"), Qualifier::Other);
        assert_eq!(qualifier("d"), Qualifier::Other);
    }

    #[test]
    fn a_methods_body() {
        let text = "@A(\"y\") public int head(@A(\"x\") int a) throws E { return this.head; }";
        let source = Source::new(text.into());
        let whole = Range {
            start: Position { line: 0, column: 0 },
            end: Position {
                line: 0,
                column: text.len() as u32,
            },
        };
        let name = find(&source, "head", 0);
        assert_eq!(
            source.body(whole, name).unwrap(),
            ["return", "this", ".", "head", ";"]
        );
        let abstract_method = Source::new("abstract int head();".into());
        let name = find(&abstract_method, "head", 0);
        assert_eq!(abstract_method.body(whole, name), None);
    }

    #[test]
    fn how_a_fields_type_is_written() {
        let written = |text: &str| {
            let source = Source::new(text.into());
            let name = find(&source, "f", 0);
            match source.type_before(name) {
                Written::Class { name, generic } => {
                    let at = source.at(name).unwrap();
                    format!("{}{}", source.text(at), if generic { "<>" } else { "" })
                }
                other => format!("{other:?}"),
            }
        };
        assert_eq!(written("@A(x = 1) private final int f;"), "Primitive");
        assert_eq!(written("@Nullable java.util.UUID f;"), "UUID");
        assert_eq!(written("Class<? super List<T>> f;"), "Class<>");
        assert_eq!(written("int[] f;"), "Other");
        assert_eq!(written("String... f"), "Other");
        let source = Source::new("public enum Mode {}".into());
        assert_eq!(source.before(find(&source, "Mode", 0)), Some("enum"));
    }
}
