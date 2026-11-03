// Copyright 2026 The Graft Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # AST & Semantic 3-Way Merge Engine for Autonomous AI Agents
//!
//! Provides trivia-preserving, Concrete Syntax Tree (CST) chunking and semantic 3-way
//! reconciliation for programming languages (Rust, Python, Go, JavaScript/TypeScript).
//!
//! ### Why Semantic Merge for AI Agents:
//! When autonomous coding agents work concurrently on a repository:
//! 1. **Zero-Conflict Import Union**: Agents frequently add different dependencies to the top of a file.
//!    Semantic merge uses 3-way set logic (`(ours \ base) ∪ (theirs \ base) ∪ (base ∩ ours ∩ theirs)`)
//!    to unify imports cleanly while honoring single-sided deletions.
//! 2. **Non-Colliding Adjacent Appends**: In traditional Git, two agents adding different functions
//!    at the bottom of a file triggers an adjacent-line conflict. Semantic merge identifies each function
//!    as an independent item and preserves both.
//! 3. **Intra-Container Merging**: Recurses 1 level into containers (`impl Foo`, `mod tests`, `class Service`),
//!    allowing concurrent method or `#[test]` additions to auto-merge with zero conflicts.
//! 4. **Surgical Conflict Scoping**: If two agents edit the same function, conflict markers are
//!    contained strictly within that item's span, preserving valid syntax across the rest of the file.
//! 5. **Lossless CST Round-Trip**: The parser partitions source files into byte-contiguous spans such
//!    that concatenating all spans reproduces the original input byte-for-byte (`reconstruct(parse(src)) == src`).

use crate::cas::merge::{merge3_text_with_strategy, ConflictStrategy};
use std::collections::{HashMap, HashSet};

/// Maximum file size (1 MB) for semantic parsing. Larger files fall back to line diff.
pub const MAX_SEMANTIC_FILE_SIZE: usize = 1_048_576;

/// Supported language grammar categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Python,
    Go,
    JavaScriptTypeScript,
}

impl SupportedLanguage {
    /// Detects language from file extension.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Some(Self::JavaScriptTypeScript),
            _ => None,
        }
    }
}

/// Unique semantic key for top-level or container-level code items.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemKey {
    /// Normalized signature / name (e.g. "fn calculate_tax", "impl Display for User", "class Account")
    pub name: String,
    /// Item kind (Function, Struct, Class, Impl, Mod, etc.)
    pub kind: ItemKind,
}

impl ItemKey {
    pub fn new(kind: ItemKind, name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// Category of semantic code declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Function,
    Struct,
    Enum,
    Class,
    Impl,
    Module,
    TypeAlias,
    Constant,
    Macro,
    GeneralItem,
}

/// An element in the trivia-preserving Concrete Syntax Tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CstElement {
    /// File header trivia (license preamble, shebang, file-level doc comments `//!`, Go `package main`).
    Header(String),
    /// Import statement (e.g. `use std::io;`, `import os`, `from typing import List`).
    Import {
        /// Normalized import key for 3-way set comparison.
        key: String,
        /// Full raw source including leading comments or attributes (`#[cfg(...)] use ...;`).
        text: String,
    },
    /// Top-level or container-level semantic declaration.
    Item {
        key: ItemKey,
        /// Header declaration line (e.g. `pub fn foo(x: i32) -> i32 {`).
        signature: String,
        /// True if this item can contain child declarations (e.g. `impl`, `mod`, `class`).
        is_container: bool,
        /// Inner body text excluding outer container header and closing brace.
        inner_body: Option<String>,
        /// Full raw source text including leading doc-comments (`///`, `#`) and decorators (`#[...]`).
        text: String,
    },
    /// Interstitial trivia (blank lines, comments not attached to declarations).
    Trivia(String),
}

impl CstElement {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Header(s) => s.as_str(),
            Self::Import { text, .. } => text.as_str(),
            Self::Item { text, .. } => text.as_str(),
            Self::Trivia(s) => s.as_str(),
        }
    }
}

/// Parsed file representation partitioned into contiguous CST elements.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub header: Option<String>,
    pub imports: Vec<(String, String)>, // (normalized_key, raw_text)
    pub items: Vec<CstElement>,
    pub trailing_trivia: Option<String>,
}

impl ParsedFile {
    /// Reconstructs the exact source string from all CST elements.
    pub fn reconstruct(&self) -> String {
        let mut out = String::new();
        if let Some(ref h) = self.header {
            out.push_str(h);
        }
        for (_, raw) in &self.imports {
            out.push_str(raw);
        }
        for item in &self.items {
            out.push_str(item.as_str());
        }
        if let Some(ref t) = self.trailing_trivia {
            out.push_str(t);
        }
        out
    }
}

// -----------------------------------------------------------------------------
// Lexical Tokenizer & Comment/String Scanner
// -----------------------------------------------------------------------------

/// Identifies code boundaries while skipping comments, raw strings, and char literals.
pub struct LexicalScanner<'a> {
    src: &'a str,
    bytes: &'a [u8],
    idx: usize,
    lang: SupportedLanguage,
}

impl<'a> LexicalScanner<'a> {
    pub fn new(src: &'a str, lang: SupportedLanguage) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            idx: 0,
            lang,
        }
    }

    pub fn is_eof(&self) -> bool {
        self.idx >= self.bytes.len()
    }

    pub fn current_idx(&self) -> usize {
        self.idx
    }

    /// Advances past a line comment (`//` or `#`).
    pub fn skip_line_comment(&mut self) -> bool {
        if self.idx + 1 < self.bytes.len() && self.bytes[self.idx] == b'/' && self.bytes[self.idx + 1] == b'/' {
            self.idx += 2;
            while self.idx < self.bytes.len() && self.bytes[self.idx] != b'\n' {
                self.idx += 1;
            }
            if self.idx < self.bytes.len() && self.bytes[self.idx] == b'\n' {
                self.idx += 1;
            }
            return true;
        }
        if self.lang == SupportedLanguage::Python && self.idx < self.bytes.len() && self.bytes[self.idx] == b'#' {
            self.idx += 1;
            while self.idx < self.bytes.len() && self.bytes[self.idx] != b'\n' {
                self.idx += 1;
            }
            if self.idx < self.bytes.len() && self.bytes[self.idx] == b'\n' {
                self.idx += 1;
            }
            return true;
        }
        false
    }

    /// Advances past a block comment (`/* ... */`), supporting nested block comments in Rust.
    pub fn skip_block_comment(&mut self) -> Result<bool, ()> {
        if self.idx + 1 < self.bytes.len() && self.bytes[self.idx] == b'/' && self.bytes[self.idx + 1] == b'*' {
            self.idx += 2;
            let mut depth = 1;
            while self.idx + 1 < self.bytes.len() {
                if self.lang == SupportedLanguage::Rust
                    && self.bytes[self.idx] == b'/'
                    && self.bytes[self.idx + 1] == b'*'
                {
                    depth += 1;
                    self.idx += 2;
                } else if self.bytes[self.idx] == b'*' && self.bytes[self.idx + 1] == b'/' {
                    depth -= 1;
                    self.idx += 2;
                    if depth == 0 {
                        return Ok(true);
                    }
                } else {
                    self.idx += 1;
                }
            }
            return Err(()); // Unclosed block comment
        }
        Ok(false)
    }

    /// Advances past a string or character literal.
    /// Carefully avoids the Rust lifetime trap: `'a` and `'static` are NOT character literals!
    pub fn skip_literal(&mut self) -> Result<bool, ()> {
        if self.idx >= self.bytes.len() {
            return Ok(false);
        }

        // 1. Rust Raw String Literal: r#"..."# or r"..."
        if self.lang == SupportedLanguage::Rust && (self.bytes[self.idx] == b'r' || self.bytes[self.idx] == b'b') {
            let mut p = self.idx;
            if self.bytes[p] == b'b' {
                p += 1;
            }
            if p < self.bytes.len() && self.bytes[p] == b'r' {
                p += 1;
                let mut hashes = 0;
                while p < self.bytes.len() && self.bytes[p] == b'#' {
                    hashes += 1;
                    p += 1;
                }
                if p < self.bytes.len() && self.bytes[p] == b'"' {
                    p += 1;
                    // Scan until matching '"' followed by same number of '#'
                    while p < self.bytes.len() {
                        if self.bytes[p] == b'"' {
                            let mut end_hashes = 0;
                            while p + 1 + end_hashes < self.bytes.len() && self.bytes[p + 1 + end_hashes] == b'#' && end_hashes < hashes {
                                end_hashes += 1;
                            }
                            if end_hashes == hashes {
                                self.idx = p + 1 + hashes;
                                return Ok(true);
                            }
                        }
                        p += 1;
                    }
                    return Err(()); // Unclosed raw string
                }
            }
        }

        // 2. Python Triple-Quoted String: """...""" or '''...'''
        if self.lang == SupportedLanguage::Python && self.idx + 2 < self.bytes.len() {
            let quote = self.bytes[self.idx];
            if (quote == b'"' || quote == b'\'')
                && self.bytes[self.idx + 1] == quote
                && self.bytes[self.idx + 2] == quote
            {
                self.idx += 3;
                while self.idx + 2 < self.bytes.len() {
                    if self.bytes[self.idx] == quote
                        && self.bytes[self.idx + 1] == quote
                        && self.bytes[self.idx + 2] == quote
                    {
                        self.idx += 3;
                        return Ok(true);
                    }
                    if self.bytes[self.idx] == b'\\' {
                        self.idx += 2;
                    } else {
                        self.idx += 1;
                    }
                }
                return Err(()); // Unclosed triple quote
            }
        }

        // 3. Go Raw String: `...`
        if (self.lang == SupportedLanguage::Go || self.lang == SupportedLanguage::JavaScriptTypeScript)
            && self.bytes[self.idx] == b'`'
        {
            self.idx += 1;
            while self.idx < self.bytes.len() {
                if self.bytes[self.idx] == b'`' {
                    self.idx += 1;
                    return Ok(true);
                }
                if self.lang == SupportedLanguage::JavaScriptTypeScript && self.bytes[self.idx] == b'\\' {
                    self.idx += 2;
                } else {
                    self.idx += 1;
                }
            }
            return Err(()); // Unclosed template / raw string
        }

        // 4. Standard Double-Quoted String: "..."
        if self.bytes[self.idx] == b'"' {
            self.idx += 1;
            while self.idx < self.bytes.len() {
                if self.bytes[self.idx] == b'"' {
                    self.idx += 1;
                    return Ok(true);
                }
                if self.bytes[self.idx] == b'\\' {
                    self.idx += 2;
                } else if self.bytes[self.idx] == b'\n' && self.lang != SupportedLanguage::JavaScriptTypeScript {
                    return Err(()); // Newline in string without multiline support
                } else {
                    self.idx += 1;
                }
            }
            return Err(()); // Unclosed double quote
        }

        // 5. Single-Quoted Char Literal vs Rust Lifetime
        if self.bytes[self.idx] == b'\'' {
            if self.lang == SupportedLanguage::Rust {
                // RUST LIFETIME TRAP:
                // Check if this is a real char literal: 'x', '\'', '\n', '\x7f', etc.
                // A char literal has a closing single-quote after 1 char (or escape sequence).
                let mut is_char_lit = false;
                let mut close_offset = None;

                if self.idx + 2 < self.bytes.len() && self.bytes[self.idx + 1] != b'\\' && self.bytes[self.idx + 2] == b'\'' {
                    // Normal single char: 'c'
                    is_char_lit = true;
                    close_offset = Some(self.idx + 3);
                } else if self.idx + 3 < self.bytes.len() && self.bytes[self.idx + 1] == b'\\' && self.bytes[self.idx + 3] == b'\'' {
                    // Escaped char: '\n', '\'', '\\'
                    is_char_lit = true;
                    close_offset = Some(self.idx + 4);
                } else if self.idx + 1 < self.bytes.len() {
                    // Check unicode/hex escapes: '\x7f' or '\u{...}'
                    let rest = &self.src[self.idx + 1..];
                    if let Some(pos) = rest.find('\'') {
                        if pos <= 8 && rest.starts_with('\\') {
                            is_char_lit = true;
                            close_offset = Some(self.idx + 1 + pos + 1);
                        }
                    }
                }

                if is_char_lit {
                    if let Some(end) = close_offset {
                        self.idx = end;
                        return Ok(true);
                    }
                } else {
                    // This is a Rust lifetime ('a, 'static, 'de) or loop label ('outer: loop)
                    // Consume the apostrophe and identifier, but do NOT scan as an open string!
                    self.idx += 1;
                    while self.idx < self.bytes.len() && (self.bytes[self.idx].is_ascii_alphanumeric() || self.bytes[self.idx] == b'_') {
                        self.idx += 1;
                    }
                    return Ok(true);
                }
            } else {
                // Python / JS standard single-quoted string: '...'
                self.idx += 1;
                while self.idx < self.bytes.len() {
                    if self.bytes[self.idx] == b'\'' {
                        self.idx += 1;
                        return Ok(true);
                    }
                    if self.bytes[self.idx] == b'\\' {
                        self.idx += 2;
                    } else if self.bytes[self.idx] == b'\n' {
                        return Err(());
                    } else {
                        self.idx += 1;
                    }
                }
                return Err(());
            }
        }

        Ok(false)
    }

    /// Scans to find the closing brace `}` matching an opening brace `{` at top-level.
    /// Returns the byte index immediately after the closing `}`.
    pub fn find_matching_brace(&mut self) -> Result<usize, ()> {
        let mut depth = 0;
        let mut found_first = false;

        while self.idx < self.bytes.len() {
            if self.skip_line_comment() {
                continue;
            }
            if self.skip_block_comment()? {
                continue;
            }
            if self.skip_literal()? {
                continue;
            }

            let b = self.bytes[self.idx];
            if b == b'{' {
                depth += 1;
                found_first = true;
                self.idx += 1;
            } else if b == b'}' {
                if depth == 0 {
                    return Err(()); // Unexpected closing brace
                }
                depth -= 1;
                self.idx += 1;
                if depth == 0 && found_first {
                    return Ok(self.idx);
                }
            } else {
                self.idx += 1;
            }
        }

        if depth == 0 && found_first {
            Ok(self.idx)
        } else {
            Err(()) // Unbalanced braces
        }
    }
}

// -----------------------------------------------------------------------------
// Language Parsers (CST Chunkers)
// -----------------------------------------------------------------------------

/// Parses a source file into lossless CST elements for the given language.
pub fn parse_file(src: &str, lang: SupportedLanguage) -> Result<ParsedFile, ()> {
    if src.len() > MAX_SEMANTIC_FILE_SIZE {
        return Err(());
    }

    match lang {
        SupportedLanguage::Rust => parse_rust_file(src),
        SupportedLanguage::Python => parse_python_file(src),
        SupportedLanguage::Go => parse_go_file(src),
        SupportedLanguage::JavaScriptTypeScript => parse_js_ts_file(src),
    }
}

/// Parses Rust code into Header, Imports, and Items.
fn parse_rust_file(src: &str) -> Result<ParsedFile, ()> {
    let mut header = String::new();
    let mut imports = Vec::new();
    let mut items = Vec::new();

    let lines: Vec<&str> = src.split_inclusive('\n').collect();
    let mut line_idx = 0;

    // 1. Scan Header: License preamble, shebang, file-level doc-comments `//!`, `#![...]` crate attributes
    while line_idx < lines.len() {
        let trimmed = lines[line_idx].trim_start();
        if trimmed.starts_with("#!")
            || trimmed.starts_with("//!")
            || (trimmed.starts_with("//") && header.is_empty() && trimmed.to_lowercase().contains("copyright"))
            || (trimmed.starts_with("/*") && header.is_empty() && trimmed.to_lowercase().contains("copyright"))
            || (trimmed.starts_with("//") && !header.is_empty())
            || trimmed.is_empty()
        {
            if trimmed.is_empty() {
                // If it's a blank line, check if subsequent lines continue header directives (#!)
                let next_non_blank = lines[line_idx..].iter().find(|l| !l.trim().is_empty());
                if let Some(next) = next_non_blank {
                    let next_trimmed = next.trim_start();
                    if next_trimmed.starts_with("#!") || next_trimmed.starts_with("//!") || next_trimmed.starts_with("//") {
                        header.push_str(lines[line_idx]);
                        line_idx += 1;
                        continue;
                    }
                }
                break;
            }
            header.push_str(lines[line_idx]);
            line_idx += 1;
        } else {
            break;
        }
    }

    let header_opt = if header.is_empty() { None } else { Some(header) };

    // 2. Scan remaining lines for Imports and Items
    // Reconstruct remaining source offset
    let remaining_src: String = lines[line_idx..].concat();
    if remaining_src.is_empty() {
        return Ok(ParsedFile {
            header: header_opt,
            imports,
            items,
            trailing_trivia: None,
        });
    }

    let mut scanner = LexicalScanner::new(&remaining_src, SupportedLanguage::Rust);
    let mut current_pos = 0;

    while current_pos < remaining_src.len() {
        // Collect leading trivia (blank lines, whitespace)
        let trivia_start = current_pos;
        while current_pos < remaining_src.len() {
            let ch = remaining_src.as_bytes()[current_pos];
            if ch == b' ' || ch == b'\t' || ch == b'\r' || ch == b'\n' {
                current_pos += 1;
            } else {
                break;
            }
        }
        let leading_trivia = &remaining_src[trivia_start..current_pos];

        if current_pos >= remaining_src.len() {
            if !leading_trivia.is_empty() {
                items.push(CstElement::Trivia(leading_trivia.to_string()));
            }
            break;
        }

        // Check for doc comments or outer attributes `#[...]` attached to declaration
        let decl_start = current_pos;
        let mut scan_p = current_pos;
        while scan_p < remaining_src.len() {
            let slice = remaining_src[scan_p..].trim_start();
            if slice.starts_with("///") || slice.starts_with("/**") || slice.starts_with("#[") {
                // Find end of line or attribute
                if let Some(nl) = remaining_src[scan_p..].find('\n') {
                    scan_p += nl + 1;
                } else {
                    scan_p = remaining_src.len();
                }
            } else {
                break;
            }
        }

        let slice = remaining_src[scan_p..].trim_start();

        // A. Import statement: `use ...;` or `pub use ...;` or `extern crate ...;`
        if slice.starts_with("use ") || slice.starts_with("pub use ") || slice.starts_with("extern crate ") {
            // Scan until semicolon `;`
            let mut end_p = scan_p;
            scanner.idx = scan_p;
            let mut found_semi = false;
            while !scanner.is_eof() {
                if scanner.skip_line_comment() {
                    continue;
                }
                if scanner.skip_block_comment().map_err(|_| ())? {
                    continue;
                }
                if scanner.skip_literal().map_err(|_| ())? {
                    continue;
                }
                if scanner.bytes[scanner.idx] == b';' {
                    scanner.idx += 1;
                    end_p = scanner.idx;
                    // Consume any trailing newline
                    if end_p < scanner.bytes.len() && scanner.bytes[end_p] == b'\n' {
                        end_p += 1;
                    }
                    found_semi = true;
                    break;
                }
                scanner.idx += 1;
            }

            if !found_semi {
                return Err(()); // Malformed use statement
            }

            let raw_text = format!("{}{}", leading_trivia, &remaining_src[decl_start..end_p]);
            let norm_key = remaining_src[scan_p..end_p]
                .lines()
                .map(|l| l.trim())
                .collect::<Vec<_>>()
                .join(" ");

            imports.push((norm_key, raw_text));
            current_pos = end_p;
            continue;
        }

        // B. Item declaration: fn, struct, enum, impl, trait, mod, type, const, static, macro_rules!
        let (item_kind, item_name, is_container) = parse_rust_item_header(slice);

        if let Some((kind, name, container)) = item_kind.zip(item_name).map(|(k, n)| (k, n, is_container)) {
            // Find body bounds
            scanner.idx = scan_p;
            let body_end = if slice.contains('{') {
                // Scan to opening brace, then match closing brace
                while scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] != b'{' {
                    if scanner.bytes[scanner.idx] == b';' {
                        // Declaration without body (e.g. extern fn or trait method)
                        scanner.idx += 1;
                        break;
                    }
                    scanner.idx += 1;
                }
                if scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] == b'{' {
                    scanner.find_matching_brace().map_err(|_| ())?
                } else {
                    scanner.idx
                }
            } else {
                // Item terminated with semicolon `;` (type alias, const, static, extern)
                while scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] != b';' {
                    scanner.idx += 1;
                }
                if scanner.idx < scanner.bytes.len() {
                    scanner.idx += 1;
                }
                scanner.idx
            };

            // Consume trailing newline if present
            let mut end_p = body_end;
            if end_p < remaining_src.len() && remaining_src.as_bytes()[end_p] == b'\n' {
                end_p += 1;
            }

            let raw_item = format!("{}{}", leading_trivia, &remaining_src[decl_start..end_p]);
            let signature = remaining_src[scan_p..body_end]
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();

            // Extract inner body if container (for 1-level recursive merge)
            let inner_body = if container {
                extract_inner_braces(&remaining_src[decl_start..end_p])
            } else {
                None
            };

            items.push(CstElement::Item {
                key: ItemKey::new(kind, name),
                signature,
                is_container: container,
                inner_body,
                text: raw_item,
            });

            current_pos = end_p;
            continue;
        }

        // Fallback: If not recognized as a structured item, scan to end of line or next semicolon as Trivia
        let next_nl = remaining_src[current_pos..].find('\n').map(|p| current_pos + p + 1).unwrap_or(remaining_src.len());
        let trivia_chunk = &remaining_src[trivia_start..next_nl];
        items.push(CstElement::Trivia(trivia_chunk.to_string()));
        current_pos = next_nl;
    }

    Ok(ParsedFile {
        header: header_opt,
        imports,
        items,
        trailing_trivia: None,
    })
}

/// Extracts normalized Rust item name, kind, and container status.
fn parse_rust_item_header(slice: &str) -> (Option<ItemKind>, Option<String>, bool) {
    let first_line = slice.lines().next().unwrap_or("").trim();
    let tokens: Vec<&str> = first_line.split_whitespace().collect();

    // Strip visibility (pub, pub(crate), etc.) and qualifiers (async, const, unsafe, extern)
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "pub" => {
                i += 1;
                if i < tokens.len() && tokens[i].starts_with('(') {
                    while i < tokens.len() && !tokens[i].ends_with(')') {
                        i += 1;
                    }
                    i += 1;
                }
            }
            "async" | "const" | "unsafe" | "extern" | "default" => {
                i += 1;
            }
            _ => break,
        }
    }

    if i >= tokens.len() {
        return (None, None, false);
    }

    match tokens[i] {
        "fn" => {
            let name = tokens.get(i + 1).and_then(|t| t.split('(').next()).unwrap_or("unknown");
            (Some(ItemKind::Function), Some(format!("fn {}", name)), false)
        }
        "struct" => {
            let name = tokens.get(i + 1).and_then(|t| t.split(&['<', '{', ';'][..]).next()).unwrap_or("unknown");
            (Some(ItemKind::Struct), Some(format!("struct {}", name)), false)
        }
        "enum" => {
            let name = tokens.get(i + 1).and_then(|t| t.split(&['<', '{'][..]).next()).unwrap_or("unknown");
            (Some(ItemKind::Enum), Some(format!("enum {}", name)), false)
        }
        "impl" => {
            // Full normalized signature: "impl Display for Foo" vs "impl Foo"
            let sig = first_line.split('{').next().unwrap_or("").trim();
            (Some(ItemKind::Impl), Some(sig.to_string()), true)
        }
        "trait" => {
            let name = tokens.get(i + 1).and_then(|t| t.split(&['<', ':', '{'][..]).next()).unwrap_or("unknown");
            (Some(ItemKind::Impl), Some(format!("trait {}", name)), true)
        }
        "mod" => {
            let name = tokens.get(i + 1).and_then(|t| t.split(&[';', '{'][..]).next()).unwrap_or("unknown");
            let is_inline_container = first_line.contains('{');
            (Some(ItemKind::Module), Some(format!("mod {}", name)), is_inline_container)
        }
        "type" => {
            let name = tokens.get(i + 1).and_then(|t| t.split(&['<', '=', ';'][..]).next()).unwrap_or("unknown");
            (Some(ItemKind::TypeAlias), Some(format!("type {}", name)), false)
        }
        "const" | "static" => {
            let name = tokens.get(i + 1).and_then(|t| t.split(':').next()).unwrap_or("unknown");
            (Some(ItemKind::Constant), Some(format!("const {}", name)), false)
        }
        s if s.starts_with("macro_rules!") => {
            let name = tokens.get(i + 1).and_then(|t| t.split(&['{', '('][..]).next()).unwrap_or("unknown");
            (Some(ItemKind::Macro), Some(format!("macro_rules! {}", name)), false)
        }
        _ => (None, None, false),
    }
}

/// Extracts inner text enclosed by outer braces `{ ... }`.
fn extract_inner_braces(text: &str) -> Option<String> {
    let open = text.find('{')?;
    let close = text.rfind('}')?;
    if close > open {
        Some(text[open + 1..close].to_string())
    } else {
        None
    }
}

// -----------------------------------------------------------------------------
// Python Parser (Indentation-Aware)
// -----------------------------------------------------------------------------

fn parse_python_file(src: &str) -> Result<ParsedFile, ()> {
    let mut header = String::new();
    let mut imports = Vec::new();
    let mut items = Vec::new();

    let lines: Vec<&str> = src.split_inclusive('\n').collect();
    let mut line_idx = 0;

    // 1. Scan Header: Shebang `#!`, encoding comments, module docstrings `"""..."""`
    while line_idx < lines.len() {
        let trimmed = lines[line_idx].trim_start();
        if trimmed.starts_with("#!")
            || (trimmed.starts_with('#') && header.is_empty())
            || (header.is_empty() && trimmed.is_empty())
        {
            header.push_str(lines[line_idx]);
            line_idx += 1;
        } else {
            break;
        }
    }

    // Check for module-level docstring
    if line_idx < lines.len() && (lines[line_idx].trim_start().starts_with("\"\"\"") || lines[line_idx].trim_start().starts_with("'''")) {
        let quote = if lines[line_idx].trim_start().starts_with("\"\"\"") { "\"\"\"" } else { "'''" };
        header.push_str(lines[line_idx]);
        let mut count = lines[line_idx].matches(quote).count();
        line_idx += 1;
        while line_idx < lines.len() && count < 2 {
            header.push_str(lines[line_idx]);
            count += lines[line_idx].matches(quote).count();
            line_idx += 1;
        }
    }

    let header_opt = if header.is_empty() { None } else { Some(header) };

    // 2. Scan Imports and Indented Items
    while line_idx < lines.len() {
        let line = lines[line_idx];
        let trimmed = line.trim_start();

        // Blank lines or comments at top level
        if trimmed.is_empty() || trimmed.starts_with('#') {
            items.push(CstElement::Trivia(line.to_string()));
            line_idx += 1;
            continue;
        }

        // Import statements
        if trimmed.starts_with("import ") || trimmed.starts_with("from ") {
            let mut import_raw = line.to_string();
            let mut norm_key = trimmed.to_string();
            // Handle multiline import with parentheses `from x import (\n a,\n b\n)`
            if trimmed.contains('(') && !trimmed.contains(')') {
                line_idx += 1;
                while line_idx < lines.len() {
                    import_raw.push_str(lines[line_idx]);
                    norm_key.push(' ');
                    norm_key.push_str(lines[line_idx].trim());
                    if lines[line_idx].contains(')') {
                        break;
                    }
                    line_idx += 1;
                }
            }
            imports.push((norm_key, import_raw));
            line_idx += 1;
            continue;
        }

        // Top-level declaration: def, async def, class, or decorator @
        if !line.starts_with(' ') && !line.starts_with('\t') {
            let item_start = line_idx;
            let mut decl_line = line_idx;

            // Collect preceding decorators `@decorator`
            while decl_line < lines.len() && lines[decl_line].trim_start().starts_with('@') {
                decl_line += 1;
            }

            if decl_line < lines.len() {
                let decl_trimmed = lines[decl_line].trim_start();
                let (kind, name, container) = if decl_trimmed.starts_with("def ") || decl_trimmed.starts_with("async def ") {
                    let fname = decl_trimmed
                        .split_whitespace()
                        .find(|w| *w != "def" && *w != "async")
                        .and_then(|w| w.split('(').next())
                        .unwrap_or("unknown");
                    (Some(ItemKind::Function), Some(format!("def {}", fname)), false)
                } else if decl_trimmed.starts_with("class ") {
                    let cname = decl_trimmed
                        .split_whitespace()
                        .nth(1)
                        .and_then(|w| w.split(&['(', ':'][..]).next())
                        .unwrap_or("unknown");
                    (Some(ItemKind::Class), Some(format!("class {}", cname)), true)
                } else {
                    (None, None, false)
                };

                if let Some((k, n)) = kind.zip(name) {
                    // Consume all lines belonging to this block (indented lines following the declaration)
                    let mut item_text = String::new();
                    line_idx = decl_line + 1;
                    while line_idx < lines.len() {
                        let next_line = lines[line_idx];
                        if next_line.trim().is_empty() {
                            // Blank lines inside function body
                            line_idx += 1;
                            continue;
                        }
                        if next_line.starts_with(' ') || next_line.starts_with('\t') {
                            line_idx += 1;
                        } else {
                            break; // Back to top-level indent
                        }
                    }

                    for i in item_start..line_idx {
                        item_text.push_str(lines[i]);
                    }

                    items.push(CstElement::Item {
                        key: ItemKey::new(k, n),
                        signature: lines[decl_line].trim().to_string(),
                        is_container: container,
                        inner_body: None,
                        text: item_text,
                    });
                    continue;
                }
            }
        }

        // Standalone statement
        items.push(CstElement::Trivia(line.to_string()));
        line_idx += 1;
    }

    Ok(ParsedFile {
        header: header_opt,
        imports,
        items,
        trailing_trivia: None,
    })
}

// -----------------------------------------------------------------------------
// Go Parser
// -----------------------------------------------------------------------------

fn parse_go_file(src: &str) -> Result<ParsedFile, ()> {
    let mut header = String::new();
    let mut imports = Vec::new();
    let mut items = Vec::new();

    let lines: Vec<&str> = src.split_inclusive('\n').collect();
    let mut line_idx = 0;

    // 1. Scan Header: Package declaration and preceding comments
    while line_idx < lines.len() {
        let line = lines[line_idx];
        header.push_str(line);
        line_idx += 1;
        if line.trim_start().starts_with("package ") {
            break;
        }
    }

    let header_opt = if header.is_empty() { None } else { Some(header) };

    let remaining_src: String = lines[line_idx..].concat();
    let mut scanner = LexicalScanner::new(&remaining_src, SupportedLanguage::Go);
    let mut current_pos = 0;

    while current_pos < remaining_src.len() {
        let trivia_start = current_pos;
        while current_pos < remaining_src.len() {
            let ch = remaining_src.as_bytes()[current_pos];
            if ch == b' ' || ch == b'\t' || ch == b'\r' || ch == b'\n' {
                current_pos += 1;
            } else {
                break;
            }
        }
        let leading_trivia = &remaining_src[trivia_start..current_pos];

        if current_pos >= remaining_src.len() {
            if !leading_trivia.is_empty() {
                items.push(CstElement::Trivia(leading_trivia.to_string()));
            }
            break;
        }

        let slice = remaining_src[current_pos..].trim_start();

        // Import declaration: `import "..."` or `import ( ... )`
        if slice.starts_with("import ") {
            scanner.idx = current_pos;
            let end_p = if slice.starts_with("import (") {
                while scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] != b'(' {
                    scanner.idx += 1;
                }
                while scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] != b')' {
                    scanner.idx += 1;
                }
                if scanner.idx < scanner.bytes.len() {
                    scanner.idx += 1;
                }
                scanner.idx
            } else {
                let nl = remaining_src[current_pos..].find('\n').unwrap_or(remaining_src[current_pos..].len());
                current_pos + nl + 1
            };

            let raw = format!("{}{}", leading_trivia, &remaining_src[current_pos..end_p.min(remaining_src.len())]);
            let norm = remaining_src[current_pos..end_p.min(remaining_src.len())].replace('\n', " ");
            imports.push((norm, raw));
            current_pos = end_p.min(remaining_src.len());
            continue;
        }

        // Functions and Types: func, type, const, var
        let first_line = slice.lines().next().unwrap_or("");
        let tokens: Vec<&str> = first_line.split_whitespace().collect();

        if let Some(token) = tokens.first() {
            match *token {
                "func" => {
                    let sig = first_line.split('{').next().unwrap_or("").trim();
                    let name = tokens.get(1).and_then(|t| t.split('(').next()).unwrap_or("func");
                    scanner.idx = current_pos;
                    while scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] != b'{' {
                        scanner.idx += 1;
                    }
                    let end_p = scanner.find_matching_brace().map_err(|_| ())?;
                    let mut final_p = end_p;
                    if final_p < remaining_src.len() && remaining_src.as_bytes()[final_p] == b'\n' {
                        final_p += 1;
                    }

                    items.push(CstElement::Item {
                        key: ItemKey::new(ItemKind::Function, format!("func {}", name)),
                        signature: sig.to_string(),
                        is_container: false,
                        inner_body: None,
                        text: format!("{}{}", leading_trivia, &remaining_src[current_pos..final_p]),
                    });
                    current_pos = final_p;
                    continue;
                }
                "type" => {
                    let name = tokens.get(1).unwrap_or(&"unknown");
                    scanner.idx = current_pos;
                    let end_p = if first_line.contains('{') {
                        while scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] != b'{' {
                            scanner.idx += 1;
                        }
                        scanner.find_matching_brace().map_err(|_| ())?
                    } else {
                        let nl = remaining_src[current_pos..].find('\n').unwrap_or(remaining_src[current_pos..].len());
                        current_pos + nl + 1
                    };

                    items.push(CstElement::Item {
                        key: ItemKey::new(ItemKind::Struct, format!("type {}", name)),
                        signature: first_line.trim().to_string(),
                        is_container: false,
                        inner_body: None,
                        text: format!("{}{}", leading_trivia, &remaining_src[current_pos..end_p.min(remaining_src.len())]),
                    });
                    current_pos = end_p.min(remaining_src.len());
                    continue;
                }
                _ => {}
            }
        }

        let nl = remaining_src[current_pos..].find('\n').map(|p| current_pos + p + 1).unwrap_or(remaining_src.len());
        items.push(CstElement::Trivia(remaining_src[trivia_start..nl].to_string()));
        current_pos = nl;
    }

    Ok(ParsedFile {
        header: header_opt,
        imports,
        items,
        trailing_trivia: None,
    })
}

// -----------------------------------------------------------------------------
// JavaScript / TypeScript Parser
// -----------------------------------------------------------------------------

fn parse_js_ts_file(src: &str) -> Result<ParsedFile, ()> {
    let mut header = String::new();
    let mut imports = Vec::new();
    let mut items = Vec::new();

    let lines: Vec<&str> = src.split_inclusive('\n').collect();
    let mut line_idx = 0;

    // Scan Header
    while line_idx < lines.len() {
        let trimmed = lines[line_idx].trim_start();
        if trimmed.starts_with("#!") || (trimmed.starts_with("//") && header.is_empty()) {
            header.push_str(lines[line_idx]);
            line_idx += 1;
        } else {
            break;
        }
    }

    let header_opt = if header.is_empty() { None } else { Some(header) };
    let remaining_src: String = lines[line_idx..].concat();
    let mut scanner = LexicalScanner::new(&remaining_src, SupportedLanguage::JavaScriptTypeScript);
    let mut current_pos = 0;

    while current_pos < remaining_src.len() {
        let trivia_start = current_pos;
        while current_pos < remaining_src.len() {
            let ch = remaining_src.as_bytes()[current_pos];
            if ch == b' ' || ch == b'\t' || ch == b'\r' || ch == b'\n' {
                current_pos += 1;
            } else {
                break;
            }
        }
        let leading_trivia = &remaining_src[trivia_start..current_pos];

        if current_pos >= remaining_src.len() {
            if !leading_trivia.is_empty() {
                items.push(CstElement::Trivia(leading_trivia.to_string()));
            }
            break;
        }

        let slice = remaining_src[current_pos..].trim_start();

        // Imports: import ... from '...' or require('...')
        if slice.starts_with("import ") {
            let mut end_p = current_pos;
            if let Some(semi) = remaining_src[current_pos..].find(';') {
                end_p = current_pos + semi + 1;
            } else if let Some(nl) = remaining_src[current_pos..].find('\n') {
                end_p = current_pos + nl + 1;
            }
            if end_p < remaining_src.len() && remaining_src.as_bytes()[end_p] == b'\n' {
                end_p += 1;
            }
            let raw = format!("{}{}", leading_trivia, &remaining_src[current_pos..end_p.min(remaining_src.len())]);
            let norm = remaining_src[current_pos..end_p.min(remaining_src.len())].replace('\n', " ");
            imports.push((norm, raw));
            current_pos = end_p.min(remaining_src.len());
            continue;
        }

        // Functions and Classes (including export / export default)
        let first_line = slice.lines().next().unwrap_or("");
        let tokens: Vec<&str> = first_line.split_whitespace().collect();
        let mut i = 0;
        if i < tokens.len() && tokens[i] == "export" {
            i += 1;
            if i < tokens.len() && tokens[i] == "default" {
                i += 1;
            }
        }

        if i < tokens.len() {
            match tokens[i] {
                "function" | "async" => {
                    let fn_idx = if tokens[i] == "async" { i + 1 } else { i };
                    let name = tokens.get(fn_idx + 1).and_then(|t| t.split('(').next()).unwrap_or("anonymous");
                    scanner.idx = current_pos;
                    while scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] != b'{' {
                        scanner.idx += 1;
                    }
                    if scanner.idx < scanner.bytes.len() {
                        let end_p = scanner.find_matching_brace().map_err(|_| ())?;
                        let mut final_p = end_p;
                        if final_p < remaining_src.len() && remaining_src.as_bytes()[final_p] == b'\n' {
                            final_p += 1;
                        }
                        items.push(CstElement::Item {
                            key: ItemKey::new(ItemKind::Function, format!("function {}", name)),
                            signature: first_line.trim().to_string(),
                            is_container: false,
                            inner_body: None,
                            text: format!("{}{}", leading_trivia, &remaining_src[current_pos..final_p]),
                        });
                        current_pos = final_p;
                        continue;
                    }
                }
                "class" => {
                    let name = tokens.get(i + 1).and_then(|t| t.split(&['{', ' '][..]).next()).unwrap_or("anonymous");
                    scanner.idx = current_pos;
                    while scanner.idx < scanner.bytes.len() && scanner.bytes[scanner.idx] != b'{' {
                        scanner.idx += 1;
                    }
                    if scanner.idx < scanner.bytes.len() {
                        let end_p = scanner.find_matching_brace().map_err(|_| ())?;
                        let mut final_p = end_p;
                        if final_p < remaining_src.len() && remaining_src.as_bytes()[final_p] == b'\n' {
                            final_p += 1;
                        }
                        items.push(CstElement::Item {
                            key: ItemKey::new(ItemKind::Class, format!("class {}", name)),
                            signature: first_line.trim().to_string(),
                            is_container: true,
                            inner_body: extract_inner_braces(&remaining_src[current_pos..final_p]),
                            text: format!("{}{}", leading_trivia, &remaining_src[current_pos..final_p]),
                        });
                        current_pos = final_p;
                        continue;
                    }
                }
                _ => {}
            }
        }

        let nl = remaining_src[current_pos..].find('\n').map(|p| current_pos + p + 1).unwrap_or(remaining_src.len());
        items.push(CstElement::Trivia(remaining_src[trivia_start..nl].to_string()));
        current_pos = nl;
    }

    Ok(ParsedFile {
        header: header_opt,
        imports,
        items,
        trailing_trivia: None,
    })
}

// -----------------------------------------------------------------------------
// 3-Way Semantic Merge Algorithm
// -----------------------------------------------------------------------------

/// Performs 3-way semantic merge for code files, falling back to line merge when parsing fails or is ambiguous.
pub fn semantic_merge_with_strategy(
    ext: &str,
    base: &str,
    ours: &str,
    theirs: &str,
    strategy: ConflictStrategy,
) -> (String, bool) {
    if ours == theirs {
        return (ours.to_string(), false);
    }
    if ours == base {
        return (theirs.to_string(), false);
    }
    if theirs == base {
        return (ours.to_string(), false);
    }

    let lang = match SupportedLanguage::from_extension(ext) {
        Some(l) => l,
        None => return merge3_text_with_strategy(base, ours, theirs, strategy),
    };

    let base_cst = match parse_file(base, lang) {
        Ok(c) => c,
        Err(_) => return merge3_text_with_strategy(base, ours, theirs, strategy),
    };
    let ours_cst = match parse_file(ours, lang) {
        Ok(c) => c,
        Err(_) => return merge3_text_with_strategy(base, ours, theirs, strategy),
    };
    let theirs_cst = match parse_file(theirs, lang) {
        Ok(c) => c,
        Err(_) => return merge3_text_with_strategy(base, ours, theirs, strategy),
    };

    // Lossless CST Round-Trip Invariant Check
    if base_cst.reconstruct() != base || ours_cst.reconstruct() != ours || theirs_cst.reconstruct() != theirs {
        return merge3_text_with_strategy(base, ours, theirs, strategy);
    }

    // If none of the files contain any semantic items or imports, fall back to standard line diff
    let has_any_items = base_cst.items.iter().any(|e| matches!(e, CstElement::Item { .. }))
        || ours_cst.items.iter().any(|e| matches!(e, CstElement::Item { .. }))
        || theirs_cst.items.iter().any(|e| matches!(e, CstElement::Item { .. }))
        || !base_cst.imports.is_empty()
        || !ours_cst.imports.is_empty()
        || !theirs_cst.imports.is_empty();

    if !has_any_items {
        return merge3_text_with_strategy(base, ours, theirs, strategy);
    }

    // If any version contains unstructured top-level code (statements that are not declarations/items/comments),
    // fall back to standard line-diff3 merge.
    if has_unstructured_code(&base_cst) || has_unstructured_code(&ours_cst) || has_unstructured_code(&theirs_cst) {
        return merge3_text_with_strategy(base, ours, theirs, strategy);
    }

    // Check for duplicate ItemKeys within any single version (e.g. overloaded `func init()`, `#[cfg] fn foo`)
    if has_duplicate_keys(&base_cst) || has_duplicate_keys(&ours_cst) || has_duplicate_keys(&theirs_cst) {
        return merge3_text_with_strategy(base, ours, theirs, strategy);
    }

    // 1. Merge Headers
    let (merged_header, header_conflict) = merge_headers(
        base_cst.header.as_deref().unwrap_or(""),
        ours_cst.header.as_deref().unwrap_or(""),
        theirs_cst.header.as_deref().unwrap_or(""),
        strategy,
    );

    // 2. 3-Way Import Merge: Kept from base + added in ours + added in theirs (honoring single-sided deletions)
    let merged_imports = merge_imports(&base_cst.imports, &ours_cst.imports, &theirs_cst.imports);

    // 3. Merge Declarations / Items
    let (merged_items, items_conflict) = merge_items(&base_cst, &ours_cst, &theirs_cst, lang, strategy);

    let mut result = String::new();
    if !merged_header.is_empty() {
        result.push_str(&merged_header);
    }
    for import_raw in merged_imports {
        result.push_str(&import_raw);
    }
    for item_str in merged_items {
        result.push_str(&item_str);
    }

    let has_conflict = header_conflict || items_conflict;
    if has_conflict {
        match strategy {
            ConflictStrategy::Ours => return (ours.to_string(), false),
            ConflictStrategy::Theirs => return (theirs.to_string(), false),
            ConflictStrategy::Markers | ConflictStrategy::Fail => {}
        }
    }
    (result, has_conflict)
}

fn has_unstructured_code(cst: &ParsedFile) -> bool {
    for elem in &cst.items {
        if let CstElement::Trivia(text) = elem {
            for line in text.lines() {
                let t = line.trim();
                if !t.is_empty()
                    && !t.starts_with("//")
                    && !t.starts_with('#')
                    && !t.starts_with("/*")
                    && !t.ends_with("*/")
                    && !t.starts_with('*')
                {
                    return true;
                }
            }
        }
    }
    false
}

fn has_duplicate_keys(cst: &ParsedFile) -> bool {
    let mut seen = HashSet::new();
    for elem in &cst.items {
        if let CstElement::Item { key, .. } = elem {
            if !seen.insert(key) {
                return true; // Duplicate key collision in same file scope!
            }
        }
    }
    false
}

fn merge_headers(base: &str, ours: &str, theirs: &str, strategy: ConflictStrategy) -> (String, bool) {
    if ours == theirs || ours == base {
        (theirs.to_string(), false)
    } else if theirs == base {
        (ours.to_string(), false)
    } else {
        merge3_text_with_strategy(base, ours, theirs, strategy)
    }
}

/// 3-Way set merge for imports:
/// - Surviving base: present in Base, kept in both Ours and Theirs
/// - Single-sided deletion: present in Base, deleted in one branch and untouched in the other -> DELETED
/// - Additions: present in Ours and not in Base, or present in Theirs and not in Base -> ADDED
fn merge_imports(
    base: &[(String, String)],
    ours: &[(String, String)],
    theirs: &[(String, String)],
) -> Vec<String> {
    let base_map: HashMap<&str, &str> = base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let ours_map: HashMap<&str, &str> = ours.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let theirs_map: HashMap<&str, &str> = theirs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let mut result = Vec::new();
    let mut emitted = HashSet::new();

    // Preserve base import order for surviving imports
    for (k, v) in base {
        let in_ours = ours_map.contains_key(k.as_str());
        let in_theirs = theirs_map.contains_key(k.as_str());

        if in_ours && in_theirs {
            // Kept by both
            result.push(v.clone());
            emitted.insert(k.clone());
        }
        // If deleted in ours or deleted in theirs, it is cleanly removed!
    }

    // Append additions from Ours
    for (k, v) in ours {
        if !base_map.contains_key(k.as_str()) && emitted.insert(k.clone()) {
            result.push(v.clone());
        }
    }

    // Append additions from Theirs
    for (k, v) in theirs {
        if !base_map.contains_key(k.as_str()) && emitted.insert(k.clone()) {
            result.push(v.clone());
        }
    }

    result
}

/// Merges CST items using 3-way alignment against Base anchors.
fn merge_items(
    base_cst: &ParsedFile,
    ours_cst: &ParsedFile,
    theirs_cst: &ParsedFile,
    lang: SupportedLanguage,
    strategy: ConflictStrategy,
) -> (Vec<String>, bool) {
    let mut base_items: Vec<&CstElement> = Vec::new();
    let mut base_key_to_idx = HashMap::new();

    for elem in &base_cst.items {
        if let CstElement::Item { key, .. } = elem {
            base_key_to_idx.insert(key.clone(), base_items.len());
            base_items.push(elem);
        }
    }

    let mut ours_key_to_elem: HashMap<ItemKey, &CstElement> = HashMap::new();
    for elem in &ours_cst.items {
        if let CstElement::Item { key, .. } = elem {
            ours_key_to_elem.insert(key.clone(), elem);
        }
    }

    let mut theirs_key_to_elem: HashMap<ItemKey, &CstElement> = HashMap::new();
    for elem in &theirs_cst.items {
        if let CstElement::Item { key, .. } = elem {
            theirs_key_to_elem.insert(key.clone(), elem);
        }
    }

    // Group new additions by their preceding Base anchor
    // Anchor::None = before first base item, Anchor::Some(idx) = after base item at idx
    let mut ours_additions_by_anchor: HashMap<Option<usize>, Vec<&CstElement>> = HashMap::new();
    let mut curr_anchor = None;
    for elem in &ours_cst.items {
        match elem {
            CstElement::Item { key, .. } => {
                if let Some(&idx) = base_key_to_idx.get(key) {
                    curr_anchor = Some(idx);
                } else {
                    ours_additions_by_anchor.entry(curr_anchor).or_default().push(elem);
                }
            }
            CstElement::Trivia(_) => {
                // Trivia attached to previous item or unattached
            }
            _ => {}
        }
    }

    let mut theirs_additions_by_anchor: HashMap<Option<usize>, Vec<&CstElement>> = HashMap::new();
    curr_anchor = None;
    for elem in &theirs_cst.items {
        match elem {
            CstElement::Item { key, .. } => {
                if let Some(&idx) = base_key_to_idx.get(key) {
                    curr_anchor = Some(idx);
                } else {
                    theirs_additions_by_anchor.entry(curr_anchor).or_default().push(elem);
                }
            }
            CstElement::Trivia(_) => {}
            _ => {}
        }
    }

    let mut merged = Vec::new();
    let mut has_conflicts = false;

    // Helper closure to emit additions at a given anchor point
    let emit_additions = |anchor: Option<usize>,
                              merged: &mut Vec<String>,
                              has_conflicts: &mut bool,
                              ours_adds: &HashMap<Option<usize>, Vec<&CstElement>>,
                              theirs_adds: &HashMap<Option<usize>, Vec<&CstElement>>| {
        let o_list = ours_adds.get(&anchor).cloned().unwrap_or_default();
        let t_list = theirs_adds.get(&anchor).cloned().unwrap_or_default();

        let mut o_emitted = HashSet::new();

        // Check for collisions between newly added items
        for o_elem in &o_list {
            if let CstElement::Item { key: o_key, text: o_text, .. } = o_elem {
                let matching_t = t_list.iter().find(|t| match t {
                    CstElement::Item { key: t_key, .. } => t_key == o_key,
                    _ => false,
                });

                if let Some(CstElement::Item { text: t_text, .. }) = matching_t {
                    if o_text == t_text {
                        // Identical addition: emit once!
                        merged.push(o_text.clone());
                    } else {
                        // Same name added independently with different bodies: CONFLICT!
                        *has_conflicts = true;
                        merged.push(format!(
                            "<<<<<<< ours\n{}=======\n{}>>>>>>> theirs\n",
                            o_text, t_text
                        ));
                    }
                    o_emitted.insert(o_key.clone());
                } else {
                    merged.push(o_text.clone());
                    o_emitted.insert(o_key.clone());
                }
            }
        }

        // Emit remaining additions from Theirs that did not collide
        for t_elem in &t_list {
            if let CstElement::Item { key: t_key, text: t_text, .. } = t_elem {
                if !o_emitted.contains(t_key) {
                    merged.push(t_text.clone());
                }
            }
        }
    };

    // Emit additions before first base item
    emit_additions(None, &mut merged, &mut has_conflicts, &ours_additions_by_anchor, &theirs_additions_by_anchor);

    // Reconcile each base item
    for (idx, base_elem) in base_items.iter().enumerate() {
        if let CstElement::Item { key, text: b_text, is_container, .. } = base_elem {
            let o_opt = ours_key_to_elem.get(key);
            let t_opt = theirs_key_to_elem.get(key);

            match (o_opt, t_opt) {
                // Case 1: Untouched on both sides
                (Some(o_elem), Some(t_elem)) if o_elem.as_str() == b_text.as_str() && t_elem.as_str() == b_text.as_str() => {
                    merged.push(b_text.clone());
                }
                // Case 2: Untouched in Ours, modified in Theirs -> take Theirs
                (Some(o_elem), Some(t_elem)) if o_elem.as_str() == b_text.as_str() => {
                    merged.push(t_elem.as_str().to_string());
                }
                // Case 3: Untouched in Theirs, modified in Ours -> take Ours
                (Some(o_elem), Some(t_elem)) if t_elem.as_str() == b_text.as_str() => {
                    merged.push(o_elem.as_str().to_string());
                }
                // Case 4: Identical modifications on both sides -> take Ours
                (Some(o_elem), Some(t_elem)) if o_elem.as_str() == t_elem.as_str() => {
                    merged.push(o_elem.as_str().to_string());
                }
                // Case 5: Deleted on both sides
                (None, None) => {
                    // Item deleted cleanly
                }
                // Case 6: Deleted in Ours, untouched in Theirs -> deleted
                (None, Some(t_elem)) if t_elem.as_str() == b_text.as_str() => {
                    // Item deleted cleanly
                }
                // Case 7: Deleted in Theirs, untouched in Ours -> deleted
                (Some(o_elem), None) if o_elem.as_str() == b_text.as_str() => {
                    // Item deleted cleanly
                }
                // Case 8: Modify vs Delete Conflict (Ours deleted, Theirs modified)
                (None, Some(t_elem)) => {
                    has_conflicts = true;
                    merged.push(format!("<<<<<<< ours\n=======\n{}>>>>>>> theirs\n", t_elem.as_str()));
                }
                // Case 9: Modify vs Delete Conflict (Ours modified, Theirs deleted)
                (Some(o_elem), None) => {
                    has_conflicts = true;
                    merged.push(format!("<<<<<<< ours\n{}=======\n>>>>>>> theirs\n", o_elem.as_str()));
                }
                // Case 10: Concurrent modifications of the SAME Item!
                (Some(o_elem), Some(t_elem)) => {
                    if *is_container {
                        // Recurse 1 level into container body (e.g. `mod tests { ... }` or `impl Foo { ... }`)
                        let b_inner = extract_inner_braces(b_text);
                        let o_inner = extract_inner_braces(o_elem.as_str());
                        let t_inner = extract_inner_braces(t_elem.as_str());

                        if let (Some(bi), Some(oi), Some(ti)) = (b_inner, o_inner, t_inner) {
                            let (merged_inner, inner_conflict) = semantic_merge_with_strategy(
                                match lang {
                                    SupportedLanguage::Rust => "rs",
                                    SupportedLanguage::Python => "py",
                                    SupportedLanguage::Go => "go",
                                    SupportedLanguage::JavaScriptTypeScript => "ts",
                                },
                                &bi,
                                &oi,
                                &ti,
                                strategy,
                            );

                            // Reassemble container: signature + inner + closing brace
                            let sig = o_elem.as_str().lines().next().unwrap_or("").trim_end();
                            let reassembled = format!("{} {{\n{}\n}}\n", sig, merged_inner.trim_end());
                            merged.push(reassembled);
                            if inner_conflict {
                                has_conflicts = true;
                            }
                        } else {
                            // Container extraction failed: fall back to item-level line diff3
                            let (m_text, conflict) = merge3_text_with_strategy(
                                b_text,
                                o_elem.as_str(),
                                t_elem.as_str(),
                                strategy,
                            );
                            merged.push(m_text);
                            if conflict {
                                has_conflicts = true;
                            }
                        }
                    } else {
                        // Leaf item: run surgical 3-way line merge strictly within this item's span!
                        let (m_text, conflict) = merge3_text_with_strategy(
                            b_text,
                            o_elem.as_str(),
                            t_elem.as_str(),
                            strategy,
                        );
                        merged.push(m_text);
                        if conflict {
                            has_conflicts = true;
                        }
                    }
                }
            }
        }

        // Emit additions anchored to this base item
        emit_additions(Some(idx), &mut merged, &mut has_conflicts, &ours_additions_by_anchor, &theirs_additions_by_anchor);
    }

    (merged, has_conflicts)
}

// -----------------------------------------------------------------------------
// Unit Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_lossless_cst_round_trip() {
        let code = r##"// Copyright 2026 The Graft Authors.
//! Module doc comments with 'lifetimes like 'a and 'static.

#![allow(unused)]

use std::collections::HashMap;
use std::sync::Arc;

/// A test struct with lifetimes and raw strings
pub struct Worker<'a> {
    pub name: &'a str,
    pub payload: String,
}

impl<'a> Worker<'a> {
    pub fn new(name: &'a str) -> Self {
        let pattern = r#"hello "world" \n not escaped"#;
        Self {
            name,
            payload: pattern.to_string(),
        }
    }
}
"##;

        let parsed = parse_file(code, SupportedLanguage::Rust).expect("Rust parsing must succeed");
        assert_eq!(parsed.reconstruct(), code, "CST reconstruction must match byte-for-byte!");
    }

    #[test]
    fn test_3way_import_union_and_deletion() {
        let base = "use std::io;\nuse std::fs;\n";
        let ours = "use std::io;\nuse std::fs;\nuse std::sync::Arc;\n";
        let theirs = "use std::io;\nuse std::collections::HashMap;\n"; // Deleted `fs`, added `HashMap`

        let (merged, conflict) = semantic_merge_with_strategy("rs", base, ours, theirs, ConflictStrategy::Markers);
        assert!(!conflict, "Import 3-way merge should not conflict!");
        assert!(merged.contains("use std::io;"));
        assert!(merged.contains("use std::sync::Arc;"));
        assert!(merged.contains("use std::collections::HashMap;"));
        assert!(!merged.contains("use std::fs;"), "One-sided deletion of fs should be honored!");
    }

    #[test]
    fn test_concurrent_eof_function_additions() {
        let base = r#"pub fn existing() -> i32 {
    42
}
"#;

        let ours = r#"pub fn existing() -> i32 {
    42
}

pub fn calculate_tax() -> f64 {
    0.15
}
"#;

        let theirs = r#"pub fn existing() -> i32 {
    42
}

pub fn send_invoice() -> bool {
    true
}
"#;

        let (merged, conflict) = semantic_merge_with_strategy("rs", base, ours, theirs, ConflictStrategy::Markers);
        assert!(!conflict, "Non-colliding function additions must merge with 0 conflicts!");
        assert!(merged.contains("pub fn existing() -> i32"));
        assert!(merged.contains("pub fn calculate_tax() -> f64"));
        assert!(merged.contains("pub fn send_invoice() -> bool"));
    }

    #[test]
    fn test_same_name_new_function_collision() {
        let base = "pub fn base_func() {}\n";

        let ours = r#"pub fn base_func() {}

pub fn helper() -> i32 {
    1
}
"#;

        let theirs = r#"pub fn base_func() {}

pub fn helper() -> i32 {
    2
}
"#;

        let (merged, conflict) = semantic_merge_with_strategy("rs", base, ours, theirs, ConflictStrategy::Markers);
        assert!(conflict, "Same name added with different bodies must trigger a conflict!");
        assert!(merged.contains("<<<<<<< ours"));
        assert!(merged.contains(">>>>>>> theirs"));
    }

    #[test]
    fn test_modify_vs_delete_conflict() {
        let base = r#"pub fn victim() {
    println!("original");
}
"#;

        let ours = ""; // Ours deleted the function

        let theirs = r#"pub fn victim() {
    println!("modified by agent 2");
}
"#;

        let (merged, conflict) = semantic_merge_with_strategy("rs", base, ours, theirs, ConflictStrategy::Markers);
        assert!(conflict, "Modify vs delete must generate conflict markers!");
        assert!(merged.contains("<<<<<<< ours"));
        assert!(merged.contains("modified by agent 2"));
    }

    #[test]
    fn test_recursive_mod_tests_merging() {
        let base = r#"mod tests {
    #[test]
    fn test_base() {
        assert!(true);
    }
}
"#;

        let ours = r#"mod tests {
    #[test]
    fn test_base() {
        assert!(true);
    }

    #[test]
    fn test_agent_a() {
        assert_eq!(1, 1);
    }
}
"#;

        let theirs = r#"mod tests {
    #[test]
    fn test_base() {
        assert!(true);
    }

    #[test]
    fn test_agent_b() {
        assert_eq!(2, 2);
    }
}
"#;

        let (merged, conflict) = semantic_merge_with_strategy("rs", base, ours, theirs, ConflictStrategy::Markers);
        assert!(!conflict, "Concurrent test additions inside mod tests must merge with 0 conflicts!");
        assert!(merged.contains("fn test_base()"));
        assert!(merged.contains("fn test_agent_a()"));
        assert!(merged.contains("fn test_agent_b()"));
    }

    #[test]
    fn test_python_semantic_merge() {
        let base = r#"def calculate_sum(a, b):
    return a + b
"#;

        let ours = r#"def calculate_sum(a, b):
    return a + b

def multiply(a, b):
    return a * b
"#;

        let theirs = r#"def calculate_sum(a, b):
    return a + b

def divide(a, b):
    return a / b
"#;

        let (merged, conflict) = semantic_merge_with_strategy("py", base, ours, theirs, ConflictStrategy::Markers);
        assert!(!conflict, "Python function additions must merge cleanly!");
        assert!(merged.contains("def calculate_sum(a, b):"));
        assert!(merged.contains("def multiply(a, b):"));
        assert!(merged.contains("def divide(a, b):"));
    }
}
