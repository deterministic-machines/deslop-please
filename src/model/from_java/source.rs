//! A Java file's text, read as tokens, as far as the model needs it: where
//! its `package` and `import` declarations are, what code does with a name it
//! writes, and the tokens of a method's body. Comments are left out; a
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

/// Types whose values code can only read: what is copied, and what cannot change.
const SCALARS: [&str; 20] = [
    "boolean",
    "byte",
    "char",
    "short",
    "int",
    "long",
    "float",
    "double",
    "Boolean",
    "Byte",
    "Character",
    "Short",
    "Integer",
    "Long",
    "Float",
    "Double",
    "String",
    "BigInteger",
    "BigDecimal",
    "Class",
];

const MODIFIERS: [&str; 12] = [
    "public",
    "protected",
    "private",
    "static",
    "final",
    "transient",
    "volatile",
    "abstract",
    "default",
    "synchronized",
    "native",
    "strictfp",
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

    /// Every token's text, in order.
    fn texts(&self) -> Vec<&str> {
        (0..self.tokens.len()).map(|at| self.text(at)).collect()
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

/// Whether a field declared by `signature`, `private final List<Integer> items`,
/// holds a value code can only read: a primitive, a boxed one, a string, a
/// big number, a class, or a constant of an enum whose name `is_enum` knows.
pub fn holds_scalar(signature: &str, is_enum: impl Fn(&str) -> bool) -> bool {
    let source = Source::new(signature.to_owned());
    let texts = source.texts();
    let mut at = 0;
    while at < texts.len() {
        if texts[at] == "@" {
            at += 2;
            while texts.get(at) == Some(&".") {
                at += 2;
            }
            if texts.get(at) == Some(&"(") {
                at = source.closing(at) + 1;
            }
        } else if MODIFIERS.contains(&texts[at]) {
            at += 1;
        } else {
            break;
        }
    }
    // The type, then the name.
    let ty = texts
        .get(at..texts.len().saturating_sub(1))
        .unwrap_or_default();
    match ty {
        [name] | [.., ".", name] => SCALARS.contains(name) || is_enum(name),
        ["Class", "<", ..] => true,
        _ => false,
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
        let texts = source.texts();
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
    fn fields_that_hold_scalars() {
        let holds_scalar = |signature| holds_scalar(signature, |name| name == "Strictness");
        assert!(holds_scalar("private Strictness strictness"));
        assert!(holds_scalar("public int head"));
        assert!(holds_scalar(
            "@Nullable private final java.lang.String name"
        ));
        assert!(holds_scalar("@A(x = 1) Integer count"));
        assert!(holds_scalar("@javax.annotation.Nullable long id"));
        assert!(!holds_scalar("public final List<Integer> items"));
        assert!(!holds_scalar("int[] slots"));
        assert!(holds_scalar("private final Class<? super T> rawType"));
        assert!(!holds_scalar("List<String> names"));
        assert!(!holds_scalar("private Inner inner"));
        assert!(!holds_scalar("private static"));
        assert!(!holds_scalar(""));
    }
}
