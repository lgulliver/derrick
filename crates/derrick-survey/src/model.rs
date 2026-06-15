//! Serializable types shared by the query API, the CLI, and the MCP server.

use serde::{Deserialize, Serialize};

/// Language of an indexed source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    /// Rust (`.rs`).
    Rust,
    /// Python (`.py`, `.pyi`).
    Python,
    /// Go (`.go`).
    Go,
    /// JavaScript / JSX (`.js`, `.jsx`, `.mjs`, `.cjs`).
    JavaScript,
    /// TypeScript / TSX (`.ts`, `.tsx`).
    TypeScript,
    /// C# (`.cs`).
    CSharp,
    /// Java (`.java`).
    Java,
    /// Kotlin (`.kt`, `.kts`).
    Kotlin,
}

impl Lang {
    /// Detect a language from a file path's extension, if supported.
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Some(Self::Rust),
            Some("py" | "pyi") => Some(Self::Python),
            Some("go") => Some(Self::Go),
            Some("js" | "jsx" | "mjs" | "cjs") => Some(Self::JavaScript),
            Some("ts" | "tsx") => Some(Self::TypeScript),
            Some("cs") => Some(Self::CSharp),
            Some("java") => Some(Self::Java),
            Some("kt" | "kts") => Some(Self::Kotlin),
            _ => None,
        }
    }

    /// Lowercase wire name used in the database and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Go => "go",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::CSharp => "csharp",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
        }
    }
}

/// Kind of an extracted symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    /// A free function or method.
    Function,
    /// A struct, class, or record type.
    Type,
    /// A trait, interface, or protocol.
    Interface,
    /// An enum.
    Enum,
    /// A constant or static.
    Constant,
    /// A module or namespace.
    Module,
}

impl SymbolKind {
    /// Lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Type => "type",
            Self::Interface => "interface",
            Self::Enum => "enum",
            Self::Constant => "constant",
            Self::Module => "module",
        }
    }

    /// Parse a wire name back into a kind, falling back to [`Self::Function`]
    /// for unknown values (forward-compatibility with future kinds).
    pub fn from_wire(s: &str) -> Self {
        match s {
            "type" => Self::Type,
            "interface" => Self::Interface,
            "enum" => Self::Enum,
            "constant" => Self::Constant,
            "module" => Self::Module,
            _ => Self::Function,
        }
    }
}

/// Kind of a reference edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    /// A call to a function/method.
    Call,
    /// A non-call reference (type usage, import).
    Reference,
}

impl RefKind {
    /// Lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Reference => "reference",
        }
    }
}

/// A single search/symbol hit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolHit {
    /// Symbol name.
    pub name: String,
    /// Symbol kind.
    pub kind: SymbolKind,
    /// File path relative to the repo root.
    pub path: String,
    /// 1-based first line of the symbol.
    pub start_line: u32,
    /// 1-based last line of the symbol.
    pub end_line: u32,
    /// Signature line, when one was extracted.
    pub signature: Option<String>,
}

/// Result of a `context` query: a focal area of the codebase.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolContext {
    /// Best-matching symbols for the query.
    pub entry_points: Vec<SymbolHit>,
    /// Symbols referenced by the entry points (callees / related).
    pub related: Vec<SymbolHit>,
}

/// Result of an `impact` query: who depends on a symbol.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImpactSet {
    /// The symbol the query resolved to.
    pub symbol: SymbolHit,
    /// Direct callers / referrers of the symbol.
    pub callers: Vec<SymbolHit>,
    /// Direct callees / referents of the symbol.
    pub callees: Vec<SymbolHit>,
}

/// A file the index knows is out of date relative to the working tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingFile {
    /// Path relative to the repo root.
    pub path: String,
    /// Why it is pending (`new`, `modified`, `deleted`).
    pub reason: String,
}

/// Freshness and size summary of the index.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexStatus {
    /// Number of files indexed.
    pub files: u64,
    /// Number of symbols indexed.
    pub symbols: u64,
    /// Number of reference edges indexed.
    pub refs: u64,
    /// On-disk schema version.
    pub schema_version: u32,
    /// Files that differ from the working tree (stale or untracked).
    pub pending: Vec<PendingFile>,
    /// Unix timestamp (seconds since epoch) of the last completed build.
    /// `None` when the index has never been built.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_build_ts: Option<i64>,
    /// Human-readable freshness label for agents.
    ///
    /// One of:
    /// - `"fresh"` — index matches the working tree
    /// - `"rebuilding"` — a rebuild is in progress (watcher fired)
    /// - `"stale since <ISO-8601>"` — pending files exist; includes the
    ///   timestamp of the last completed build when available
    pub freshness: String,
}

/// Summary of a `build` run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildReport {
    /// Files (re)parsed this run.
    pub files_indexed: u64,
    /// Files removed from the index (deleted on disk).
    pub files_removed: u64,
    /// Files skipped because their content hash was unchanged.
    pub files_unchanged: u64,
    /// Total symbols in the index after the build.
    pub symbols: u64,
    /// Total reference edges in the index after the build.
    pub refs: u64,
}

/// Options controlling a `build` run.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildOptions {
    /// When true, reparse every file regardless of content hash.
    pub full: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn from_path_detects_every_supported_extension() {
        let cases = [
            ("lib.rs", Some(Lang::Rust)),
            ("mod.py", Some(Lang::Python)),
            ("types.pyi", Some(Lang::Python)),
            ("main.go", Some(Lang::Go)),
            ("app.js", Some(Lang::JavaScript)),
            ("ui.jsx", Some(Lang::JavaScript)),
            ("esm.mjs", Some(Lang::JavaScript)),
            ("cjs.cjs", Some(Lang::JavaScript)),
            ("app.ts", Some(Lang::TypeScript)),
            ("view.tsx", Some(Lang::TypeScript)),
            ("Service.cs", Some(Lang::CSharp)),
            ("Service.java", Some(Lang::Java)),
            ("Main.kt", Some(Lang::Kotlin)),
            ("build.gradle.kts", Some(Lang::Kotlin)),
        ];
        for (name, expected) in cases {
            assert_eq!(
                Lang::from_path(Path::new(name)),
                expected,
                "unexpected language for {name}"
            );
        }
    }

    #[test]
    fn from_path_returns_none_for_unsupported_or_missing_extension() {
        assert_eq!(Lang::from_path(Path::new("README.md")), None);
        assert_eq!(Lang::from_path(Path::new("Makefile")), None);
        assert_eq!(Lang::from_path(Path::new("notes.txt")), None);
    }

    #[test]
    fn lang_wire_names_are_stable() {
        assert_eq!(Lang::Rust.as_str(), "rust");
        assert_eq!(Lang::Python.as_str(), "python");
        assert_eq!(Lang::Go.as_str(), "go");
        assert_eq!(Lang::JavaScript.as_str(), "javascript");
        assert_eq!(Lang::TypeScript.as_str(), "typescript");
        assert_eq!(Lang::CSharp.as_str(), "csharp");
        assert_eq!(Lang::Java.as_str(), "java");
        assert_eq!(Lang::Kotlin.as_str(), "kotlin");
    }

    #[test]
    fn symbol_kind_wire_round_trips() {
        for kind in [
            SymbolKind::Function,
            SymbolKind::Type,
            SymbolKind::Interface,
            SymbolKind::Enum,
            SymbolKind::Constant,
            SymbolKind::Module,
        ] {
            assert_eq!(SymbolKind::from_wire(kind.as_str()), kind);
        }
        // Unknown wire values fall back to Function (forward-compatibility).
        assert_eq!(SymbolKind::from_wire("something_new"), SymbolKind::Function);
    }

    #[test]
    fn ref_kind_wire_names_are_stable() {
        assert_eq!(RefKind::Call.as_str(), "call");
        assert_eq!(RefKind::Reference.as_str(), "reference");
    }
}
