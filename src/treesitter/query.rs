//! Shared Tree-sitter `.scm` query runner and byte-range lexical scope index.
//!
//! Language frontends supply declarative `.scm` query files (`include_str!("queries.scm")`)
//! to capture scopes, declarations, imports, references, and computation markers,
//! avoiding imperative whole-tree AST walkers per language.

use std::ops::Range;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language as TsLanguage, Node, Query, QueryCursor, QueryMatch};

use crate::treesitter::summary::{LocalDeclId, ModuleScope};

/// Compiled Tree-sitter `.scm` query with convenient capture lookup.
pub struct CompiledQuery {
    query: Query,
}

impl CompiledQuery {
    pub fn new(language: &TsLanguage, scm: &str) -> Self {
        let query = Query::new(language, scm)
            .unwrap_or_else(|e| panic!("invalid tree-sitter query (.scm): {e}"));
        Self { query }
    }

    /// Iterate every query match under `root`, passing a lightweight handle for capture lookup.
    pub fn for_each_match<'t, F>(&self, root: Node<'t>, source: &[u8], mut f: F)
    where
        F: FnMut(&MatchView<'_, 't>),
    {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&self.query, root, source);
        while let Some(m) = matches.next() {
            let view = MatchView {
                query: &self.query,
                m,
            };
            f(&view);
        }
    }
}

/// Borrowed view of a single `QueryMatch` with named-capture helpers.
pub struct MatchView<'q, 't> {
    query: &'q Query,
    m: &'q QueryMatch<'q, 't>,
}

impl<'q, 't> MatchView<'q, 't> {
    /// Iterate all `(capture_name, node)` pairs in this match.
    pub fn captures(&self) -> impl Iterator<Item = (&'q str, Node<'t>)> + '_ {
        let names = self.query.capture_names();
        self.m
            .captures
            .iter()
            .map(move |c| (names[c.index as usize], c.node))
    }
}

/// Span-based lexical scope index populated from `.scm` scope/declaration captures.
///
/// Allows flat `.scm` matches to resolve their enclosing `ModuleScope`, parent declaration,
/// enclosing `holder`, test context, and skipped regions without recursive AST walking.
#[derive(Debug, Default)]
pub struct ScopeIndex {
    modules: Vec<(Range<usize>, ModuleScope)>,
    holders: Vec<(Range<usize>, LocalDeclId)>,
    parents: Vec<(Range<usize>, LocalDeclId)>,
    test_spans: Vec<Range<usize>>,
    skipped_spans: Vec<Range<usize>>,
}

impl ScopeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_module(&mut self, range: Range<usize>, scope: ModuleScope) {
        insert_sorted_keyed(&mut self.modules, range, scope);
    }

    /// Register a declaration body span that holds code references (function, type, field, constant).
    pub fn add_holder(&mut self, range: Range<usize>, id: LocalDeclId) {
        insert_sorted_keyed(&mut self.holders, range, id);
    }

    /// Register a container declaration span (`struct`, `enum`, `class`, `impl`) for child items.
    pub fn add_parent(&mut self, range: Range<usize>, id: LocalDeclId) {
        insert_sorted_keyed(&mut self.parents, range, id);
    }

    pub fn add_test_span(&mut self, range: Range<usize>) {
        insert_sorted(&mut self.test_spans, range);
    }

    pub fn add_skipped_span(&mut self, range: Range<usize>) {
        insert_sorted(&mut self.skipped_spans, range);
    }

    /// Sort spans outermost-first (ascending start, descending end) for deterministic innermost queries.
    pub fn finalize(&mut self) {
        sort_spans_keyed(&mut self.modules);
        sort_spans_keyed(&mut self.holders);
        sort_spans_keyed(&mut self.parents);
        sort_spans(&mut self.test_spans);
        sort_spans(&mut self.skipped_spans);
    }

    /// Innermost module scope strictly or inclusively containing `byte`.
    pub fn module_at(&self, byte: usize) -> ModuleScope {
        innermost_containing(&self.modules, byte, None).unwrap_or(ModuleScope::FileRoot)
    }

    /// Innermost enclosing holder declaration containing `byte`.
    pub fn holder_at(&self, byte: usize) -> Option<LocalDeclId> {
        innermost_containing(&self.holders, byte, None)
    }

    /// Innermost enclosing parent container (`struct`/`enum`/`class`/`impl`) strictly containing `range`.
    pub fn parent_decl_of(&self, range: &Range<usize>) -> Option<LocalDeclId> {
        innermost_strictly_containing(&self.parents, range)
    }

    pub fn is_test_at(&self, byte: usize) -> bool {
        contains_any(&self.test_spans, byte)
    }

    pub fn is_skipped_at(&self, byte: usize) -> bool {
        contains_any(&self.skipped_spans, byte)
    }
}

fn insert_sorted_keyed<T>(spans: &mut Vec<(Range<usize>, T)>, range: Range<usize>, val: T) {
    let pos = spans.partition_point(|(existing, _)| {
        existing.start < range.start || (existing.start == range.start && existing.end > range.end)
    });
    spans.insert(pos, (range, val));
}

fn insert_sorted(spans: &mut Vec<Range<usize>>, range: Range<usize>) {
    let pos = spans.partition_point(|existing| {
        existing.start < range.start || (existing.start == range.start && existing.end > range.end)
    });
    spans.insert(pos, range);
}

fn sort_spans_keyed<T>(spans: &mut [(Range<usize>, T)]) {
    spans.sort_by(|a, b| {
        a.0.start
            .cmp(&b.0.start)
            .then_with(|| b.0.end.cmp(&a.0.end))
    });
}

fn sort_spans(spans: &mut [Range<usize>]) {
    spans.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));
}

fn innermost_containing<T: Copy>(
    spans: &[(Range<usize>, T)],
    byte: usize,
    exclude_exact: Option<&Range<usize>>,
) -> Option<T> {
    let mut best: Option<(usize, T)> = None;
    for (range, val) in spans {
        if range.start > byte {
            break;
        }
        if byte < range.end {
            if exclude_exact.is_some_and(|ex| ex == range) {
                continue;
            }
            let width = range.end - range.start;
            if best.is_none_or(|(bw, _)| width <= bw) {
                best = Some((width, *val));
            }
        }
    }
    best.map(|(_, v)| v)
}

fn innermost_strictly_containing<T: Copy>(
    spans: &[(Range<usize>, T)],
    inner: &Range<usize>,
) -> Option<T> {
    let mut best: Option<(usize, T)> = None;
    for (range, val) in spans {
        if range.start > inner.start {
            break;
        }
        if range.start <= inner.start && range.end >= inner.end && range != inner {
            let width = range.end - range.start;
            if best.is_none_or(|(bw, _)| width <= bw) {
                best = Some((width, *val));
            }
        }
    }
    best.map(|(_, v)| v)
}

fn contains_any(spans: &[Range<usize>], byte: usize) -> bool {
    for range in spans {
        if range.start > byte {
            break;
        }
        if byte < range.end {
            return true;
        }
    }
    false
}

/// 1-based start line of a Tree-sitter node.
pub fn node_line(node: Node<'_>) -> u32 {
    (node.start_position().row as u32).saturating_add(1)
}

/// Count lines in `source` (matches `deslop-please`: every newline-delimited line).
pub fn line_count(source: &str) -> usize {
    if source.is_empty() {
        0
    } else {
        source.lines().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::treesitter::summary::LocalModId;

    #[test]
    fn scope_index_picks_innermost_module_holder_and_parent() {
        let mut idx = ScopeIndex::new();
        idx.add_module(0..100, ModuleScope::Inline(LocalModId(0)));
        idx.add_module(20..80, ModuleScope::Inline(LocalModId(1)));
        idx.add_parent(10..90, LocalDeclId(0));
        idx.add_parent(30..60, LocalDeclId(1));
        idx.add_holder(35..55, LocalDeclId(2));
        idx.add_test_span(20..80);
        idx.add_skipped_span(70..75);
        idx.finalize();

        assert_eq!(idx.module_at(5), ModuleScope::Inline(LocalModId(0)));
        assert_eq!(idx.module_at(40), ModuleScope::Inline(LocalModId(1)));
        assert_eq!(idx.parent_decl_of(&(35..55)), Some(LocalDeclId(1)));
        assert_eq!(idx.parent_decl_of(&(30..60)), Some(LocalDeclId(0)));
        assert_eq!(idx.holder_at(40), Some(LocalDeclId(2)));
        assert_eq!(idx.holder_at(15), None);
        assert!(idx.is_test_at(40));
        assert!(!idx.is_test_at(5));
        assert!(idx.is_skipped_at(72));
        assert!(!idx.is_skipped_at(76));
    }

    #[test]
    fn compiled_query_runs_scm_against_rust_grammar() {
        let lang: TsLanguage = tree_sitter_rust::LANGUAGE.into();
        let q = CompiledQuery::new(&lang, "(function_item name: (identifier) @fn.name)");
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let src = "fn alpha() {} fn beta() {}";
        let tree = parser.parse(src, None).unwrap();
        let mut names = Vec::new();
        q.for_each_match(tree.root_node(), src.as_bytes(), |m| {
            for (name, node) in m.captures() {
                if name == "fn.name" {
                    names.push(src[node.byte_range()].to_owned());
                }
            }
        });
        assert_eq!(names, vec!["alpha", "beta"]);
    }
}
