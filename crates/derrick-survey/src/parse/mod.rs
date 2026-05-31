//! Tree-sitter symbol and reference extraction for the supported languages.
//!
//! Each language supplies two queries: a *symbols* query whose captures are
//! named after the [`SymbolKind`] they produce (with a `@name` capture on the
//! identifier), and a *refs* query whose captures (`@call` / `@reference`) mark
//! the textual target of an outgoing edge. Reference edges are attributed to
//! the innermost enclosing symbol at build time, so unattributed top-level
//! references are dropped.

use std::collections::HashMap;
use std::sync::LazyLock;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::model::{Lang, RefKind, SymbolKind};

mod queries;

/// Per-language tree-sitter language handle plus its pre-compiled symbol and
/// reference queries. Compiling a [`Query`] is expensive, so we do it once.
struct CompiledQueries {
    language: tree_sitter::Language,
    symbols: Query,
    refs: Query,
}

/// All supported languages' queries, compiled once on first parse. The
/// built-in query sources are constants, so a compile failure is a programmer
/// error.
static QUERIES: LazyLock<HashMap<Lang, CompiledQueries>> = LazyLock::new(|| {
    [
        Lang::Rust,
        Lang::Python,
        Lang::Go,
        Lang::JavaScript,
        Lang::TypeScript,
        Lang::CSharp,
        Lang::Java,
        Lang::Kotlin,
    ]
    .into_iter()
    .map(|lang| {
        let language = lang.ts_language();
        let symbols = Query::new(&language, lang.symbols_query_src())
            .expect("built-in symbols query must compile");
        let refs =
            Query::new(&language, lang.refs_query_src()).expect("built-in refs query must compile");
        (
            lang,
            CompiledQueries {
                language,
                symbols,
                refs,
            },
        )
    })
    .collect()
});

/// A symbol extracted from a single file.
#[derive(Clone, Debug)]
pub(crate) struct ParsedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
}

/// An outgoing reference, not yet attributed to its enclosing symbol.
#[derive(Clone, Debug)]
pub(crate) struct ParsedRef {
    pub dst_name: String,
    pub kind: RefKind,
    /// 1-based line the reference occurs on, used to find its enclosing symbol.
    pub line: u32,
}

/// Everything extracted from one file.
#[derive(Clone, Debug, Default)]
pub(crate) struct ParsedFile {
    pub symbols: Vec<ParsedSymbol>,
    pub refs: Vec<ParsedRef>,
}

impl Lang {
    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        }
    }

    fn symbols_query_src(self) -> &'static str {
        match self {
            Lang::Rust => queries::RUST_SYMBOLS,
            Lang::Python => queries::PYTHON_SYMBOLS,
            Lang::Go => queries::GO_SYMBOLS,
            Lang::JavaScript => queries::JAVASCRIPT_SYMBOLS,
            Lang::TypeScript => queries::TYPESCRIPT_SYMBOLS,
            Lang::CSharp => queries::CSHARP_SYMBOLS,
            Lang::Java => queries::JAVA_SYMBOLS,
            Lang::Kotlin => queries::KOTLIN_SYMBOLS,
        }
    }

    fn refs_query_src(self) -> &'static str {
        match self {
            Lang::Rust => queries::RUST_REFS,
            Lang::Python => queries::PYTHON_REFS,
            Lang::Go => queries::GO_REFS,
            Lang::JavaScript | Lang::TypeScript => queries::JS_TS_REFS,
            Lang::CSharp => queries::CSHARP_REFS,
            Lang::Java => queries::JAVA_REFS,
            Lang::Kotlin => queries::KOTLIN_REFS,
        }
    }
}

fn kind_from_capture(name: &str) -> Option<SymbolKind> {
    match name {
        "function" => Some(SymbolKind::Function),
        "type" => Some(SymbolKind::Type),
        "interface" => Some(SymbolKind::Interface),
        "enum" => Some(SymbolKind::Enum),
        "constant" => Some(SymbolKind::Constant),
        "module" => Some(SymbolKind::Module),
        _ => None,
    }
}

/// First meaningful line of a definition node — the text up to the first `{` or
/// newline — trimmed and length-capped, used as a human-readable signature.
fn signature_of(node: Node, source: &str) -> Option<String> {
    let text = node.utf8_text(source.as_bytes()).ok()?;
    let end = text.find(['{', '\n']).unwrap_or(text.len());
    let sig = text[..end].trim();
    if sig.is_empty() {
        return None;
    }
    const MAX: usize = 200;
    let sig = if sig.len() > MAX {
        // Truncate on a char boundary.
        let mut cut = MAX;
        while !sig.is_char_boundary(cut) {
            cut -= 1;
        }
        &sig[..cut]
    } else {
        sig
    };
    Some(sig.to_owned())
}

/// Parse one file's source into symbols and unattributed references.
pub(crate) fn parse(lang: Lang, source: &str) -> Result<ParsedFile, crate::SurveyError> {
    let queries = &QUERIES[&lang];
    let mut parser = Parser::new();
    parser
        .set_language(&queries.language)
        .map_err(|e| crate::SurveyError::Internal(format!("set_language failed: {e}")))?;
    let tree = parser.parse(source, None).ok_or_else(|| {
        crate::SurveyError::Internal("tree-sitter parse returned None".to_owned())
    })?;
    let root = tree.root_node();

    let symbols = extract_symbols(&queries.symbols, root, source);
    let refs = extract_refs(&queries.refs, root, source);
    Ok(ParsedFile { symbols, refs })
}

fn extract_symbols(query: &Query, root: Node, source: &str) -> Vec<ParsedSymbol> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(query, root, source.as_bytes());
    while let Some(m) = matches.next() {
        let mut def_node: Option<(Node, SymbolKind)> = None;
        let mut name: Option<String> = None;
        for cap in m.captures {
            let cap_name = names[cap.index as usize];
            if cap_name == "name" {
                name = cap
                    .node
                    .utf8_text(source.as_bytes())
                    .ok()
                    .map(str::to_owned);
            } else if let Some(kind) = kind_from_capture(cap_name) {
                def_node = Some((cap.node, kind));
            }
        }
        if let (Some((node, kind)), Some(name)) = (def_node, name) {
            out.push(ParsedSymbol {
                name,
                kind,
                start_line: node.start_position().row as u32 + 1,
                end_line: node.end_position().row as u32 + 1,
                signature: signature_of(node, source),
            });
        }
    }
    out
}

fn extract_refs(query: &Query, root: Node, source: &str) -> Vec<ParsedRef> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(query, root, source.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let kind = match names[cap.index as usize] {
                "call" => RefKind::Call,
                "reference" => RefKind::Reference,
                _ => continue,
            };
            if let Ok(text) = cap.node.utf8_text(source.as_bytes()) {
                out.push(ParsedRef {
                    dst_name: text.to_owned(),
                    kind,
                    line: cap.node.start_position().row as u32 + 1,
                });
            }
        }
    }
    out
}

/// Attribute each reference to the innermost symbol whose line span contains
/// it, returning `(src_symbol_index, ref)` pairs. References that fall outside
/// every symbol are dropped.
pub(crate) fn attribute_refs<'a>(
    symbols: &[ParsedSymbol],
    refs: &'a [ParsedRef],
) -> Vec<(usize, &'a ParsedRef)> {
    refs.iter()
        .filter_map(|r| {
            symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| s.start_line <= r.line && r.line <= s.end_line)
                .min_by_key(|(_, s)| s.end_line - s.start_line)
                .map(|(idx, _)| (idx, r))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_extracts_functions_and_calls() {
        let src = "fn helper() {}\nfn caller() {\n    helper();\n}\n";
        let parsed = parse(Lang::Rust, src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"caller"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "helper"));

        let attributed = attribute_refs(&parsed.symbols, &parsed.refs);
        let caller_idx = parsed
            .symbols
            .iter()
            .position(|s| s.name == "caller")
            .unwrap();
        assert!(
            attributed
                .iter()
                .any(|(idx, r)| *idx == caller_idx && r.dst_name == "helper")
        );
    }

    #[test]
    fn python_extracts_classes_and_functions() {
        let src = "class Foo:\n    def method(self):\n        bar()\n\ndef bar():\n    pass\n";
        let parsed = parse(Lang::Python, src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"method"));
        assert!(names.contains(&"bar"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "bar"));
    }

    #[test]
    fn go_extracts_funcs_and_types() {
        let src = "package main\ntype T struct{}\nfunc f() {\n\tg()\n}\nfunc g() {}\n";
        let parsed = parse(Lang::Go, src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"T"));
        assert!(names.contains(&"f"));
        assert!(names.contains(&"g"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "g"));
    }

    #[test]
    fn typescript_extracts_interface_and_class() {
        let src = "interface I { x: number }\nclass C {\n  m() { fn(); }\n}\nfunction fn() {}\n";
        let parsed = parse(Lang::TypeScript, src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"I"));
        assert!(names.contains(&"C"));
        assert!(names.contains(&"fn"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "fn"));
    }

    #[test]
    fn csharp_extracts_types_methods_and_calls() {
        let src = "namespace App;\ninterface IGreeter { string Greet(); }\nclass Greeter : IGreeter {\n    public string Greet() { return Build(); }\n    string Build() { return \"hi\"; }\n}\nenum Color { Red, Green }\n";
        let parsed = parse(Lang::CSharp, src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"App"));
        assert!(names.contains(&"IGreeter"));
        assert!(names.contains(&"Greeter"));
        assert!(names.contains(&"Greet"));
        assert!(names.contains(&"Build"));
        assert!(names.contains(&"Color"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "Build"));

        // The call to `Build` lives in the class's `Greet` method (line 4),
        // not the interface's `Greet` declaration (line 2), so attribute it to
        // the enclosing symbol by line span rather than by name.
        let attributed = attribute_refs(&parsed.symbols, &parsed.refs);
        let greet_idx = parsed
            .symbols
            .iter()
            .position(|s| s.name == "Greet" && s.start_line == 4)
            .unwrap();
        assert!(
            attributed
                .iter()
                .any(|(idx, r)| *idx == greet_idx && r.dst_name == "Build")
        );
    }

    #[test]
    fn java_extracts_types_methods_and_calls() {
        let src = "package com.acme.billing;\ninterface Invoice { double total(); }\nclass InvoiceService implements Invoice {\n    public double total() { return compute(); }\n    private double compute() { return new TaxCalculator().apply(100.0); }\n}\nenum Color { RED, GREEN }\n";
        let parsed = parse(Lang::Java, src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"com.acme.billing"));
        assert!(names.contains(&"Invoice"));
        assert!(names.contains(&"InvoiceService"));
        assert!(names.contains(&"total"));
        assert!(names.contains(&"compute"));
        assert!(names.contains(&"Color"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "compute"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "TaxCalculator"));

        let attributed = attribute_refs(&parsed.symbols, &parsed.refs);
        let total_idx = parsed
            .symbols
            .iter()
            .position(|s| s.name == "total" && s.start_line == 4)
            .unwrap();
        assert!(
            attributed
                .iter()
                .any(|(idx, r)| *idx == total_idx && r.dst_name == "compute")
        );
    }

    #[test]
    fn kotlin_extracts_types_functions_and_calls() {
        let src = "package com.acme.app\ninterface Greeter { fun greet(): String }\nclass RealGreeter : Greeter {\n    override fun greet(): String { return build() }\n    private fun build(): String { return helper() }\n}\nfun helper(): String { return \"hi\" }\n";
        let parsed = parse(Lang::Kotlin, src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"com.acme.app"));
        assert!(names.contains(&"Greeter"));
        assert!(names.contains(&"RealGreeter"));
        assert!(names.contains(&"greet"));
        assert!(names.contains(&"build"));
        assert!(names.contains(&"helper"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "build"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "helper"));

        let attributed = attribute_refs(&parsed.symbols, &parsed.refs);
        let build_idx = parsed
            .symbols
            .iter()
            .position(|s| s.name == "build")
            .unwrap();
        assert!(
            attributed
                .iter()
                .any(|(idx, r)| *idx == build_idx && r.dst_name == "helper")
        );
    }

    #[test]
    fn javascript_extracts_arrow_and_calls() {
        let src = "const add = (a, b) => a + b;\nfunction run() {\n  add(1, 2);\n}\n";
        let parsed = parse(Lang::JavaScript, src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"add"));
        assert!(names.contains(&"run"));
        assert!(parsed.refs.iter().any(|r| r.dst_name == "add"));
    }
}
