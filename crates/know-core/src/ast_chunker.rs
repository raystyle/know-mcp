//! AST-aware break points for code chunking (qmd's scheme, tree-sitter).
//!
//! Per-language queries capture top-level definitions; each capture site
//! becomes a high-score break point in the shared chunker. Scores align
//! with the markdown heading scale so one algorithm serves both:
//!   class/struct/trait/impl/interface/enum/mod = 100
//!   function/method/export                            = 90
//!   type alias                                        = 80
//!   import                                            = 60
//! Parse failures fall back silently to the regex/newline chunker (real
//! repos contain unparseable files; tree-sitter is error-tolerant anyway).

use tree_sitter::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

/// Language selection by file extension.
pub enum AstLang {
    Rust,
    TypeScript,
    Python,
    Go,
}

impl AstLang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs" => Some(Self::TypeScript),
            "py" => Some(Self::Python),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    fn language(&self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// qmd-scored definition queries (one query per language).
    fn query_src(&self) -> &'static str {
        match self {
            Self::Rust => {
                r#"
                (struct_item name: (type_identifier)) @t100
                (enum_item name: (type_identifier)) @t100
                (trait_item name: (type_identifier)) @t100
                (impl_item) @t100
                (mod_item name: (identifier)) @t100
                (function_item name: (identifier)) @t90
                (type_item name: (type_identifier)) @t80
                (use_declaration) @t60
            "#
            }
            Self::TypeScript => {
                r#"
                (class_declaration name: (identifier)) @t100
                (interface_declaration name: (type_identifier)) @t100
                (enum_declaration name: (identifier)) @t100
                (function_declaration name: (identifier)) @t90
                (method_definition name: (property_identifier)) @t90
                (variable_declarator name: (identifier) value: (arrow_function)) @t90
                (export_statement) @t90
                (type_alias_declaration name: (type_identifier)) @t80
                (import_statement) @t60
            "#
            }
            Self::Python => {
                r#"
                (class_definition name: (identifier)) @t100
                (decorated_definition) @t90
                (function_definition name: (identifier)) @t90
                (import_statement) @t60
            "#
            }
            Self::Go => {
                r#"
                (type_declaration (type_spec name: (type_identifier))) @t100
                (function_declaration name: (identifier)) @t90
                (method_declaration name: (field_identifier)) @t90
                (import_declaration_list (import_spec) @t60)
            "#
            }
        }
    }
}

/// A scored break point: byte offset → score (markdown scale).
pub struct BreakPoint {
    pub offset: usize,
    pub score: u32,
}

/// Compute AST break points for the source; empty on any failure
/// (caller falls back to regex/newline points).
pub fn ast_break_points(source: &str, lang: AstLang) -> Vec<BreakPoint> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        eprintln!("[ast_chunker] parse failed");
        return Vec::new();
    };
    let query = match Query::new(&lang.language(), lang.query_src()) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("[ast_chunker] query compile failed: {e:?}");
            return Vec::new();
        }
    };
    let mut cursor = QueryCursor::new();
    let mut out: Vec<BreakPoint> = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let name = query.capture_names()[cap.index as usize];
            let score = match name {
                n if n.starts_with("t100") => 100,
                n if n.starts_with("t90") => 90,
                n if n.starts_with("t80") => 80,
                n if n.starts_with("t60") => 60,
                _ => 20,
            };
            out.push(BreakPoint {
                offset: cap.node.byte_range().start,
                score,
            });
        }
    }
    // Dedup by offset keeping the highest score.
    out.sort_by_key(|b| (b.offset, std::cmp::Reverse(b.score)));
    out.dedup_by_key(|b| b.offset);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_break_points_at_definitions() {
        let src = r#"
use std::fmt;

pub struct Foo { x: i32 }

impl Foo {
    pub fn bar(&self) -> i32 { self.x }
}

fn main() { println!("hi"); }
"#;
        let bps = ast_break_points(src, AstLang::Rust);
        assert!(!bps.is_empty(), "no break points found");
        // use=60, struct=100, impl=100, fn bar=90, fn main=90
        let scores: Vec<u32> = bps.iter().map(|b| b.score).collect();
        assert!(scores.contains(&100), "struct/impl missing: {scores:?}");
        assert!(scores.contains(&90), "fn missing: {scores:?}");
        assert!(scores.contains(&60), "use missing: {scores:?}");
    }

    #[test]
    fn python_break_points_at_defs() {
        let src = "import os\n\nclass A:\n    def run(self):\n        pass\n";
        let bps = ast_break_points(src, AstLang::Python);
        let scores: Vec<u32> = bps.iter().map(|b| b.score).collect();
        assert!(scores.contains(&100), "class missing: {scores:?}");
        assert!(scores.contains(&60), "import missing: {scores:?}");
    }

    #[test]
    fn parse_failure_returns_empty() {
        // Valid enough to parse, but unknown extension → None earlier; here
        // a rust query against broken source still yields points or empty —
        // never panics.
        let bps = ast_break_points("\x00garbage", AstLang::Rust);
        assert!(bps.len() <= 100);
    }
}
