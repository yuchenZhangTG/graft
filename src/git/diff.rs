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

//! # Unified Diff & Patch Engine
//!
//! This module provides algorithms to compute unified diffs (like `git diff`)
//! between file versions, workspace modifications, and repository Merkle trees.
//!
//! ### Core Features:
//! - **Unified Diff Generation**: Implements Myers/LCS line diffing with standard
//!   unified diff headers (`--- a/...`, `+++ b/...`, `@@ -l,s +l,s @@`).
//! - **Workspace Diffing in $O(K)$**: Compares modified files in `upperdir` against
//!   their base versions in `lowerdir` or CAS without scanning untouched files.
//! - **Merkle Tree Diffing in $O(\text{changed})$**: Traverses two directory manifests,
//!   skipping identical subtrees in $O(1)$ pointer/digest comparisons.
//! - **Standard Patch Export**: Outputs formatted patches compatible with `git apply`
//!   and `patch -p1`.

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::{DirectoryManifest, ManifestEntry};
use crate::cas::digest::Digest;
use crate::workspace::{ChangeType, WorkspaceConfig, WorkspaceManager};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

/// Represents a single change line in a unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine<'a> {
    /// Context line common to both versions (` `).
    Context(&'a str),
    /// Line deleted from old version (`-`).
    Deletion(&'a str),
    /// Line added in new version (`+`).
    Addition(&'a str),
}

/// Summary statistics of a diff: number of files changed, lines added, and lines deleted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffStats {
    pub files_changed: usize,
    pub additions: usize,
    pub deletions: usize,
}

impl std::fmt::Display for DiffStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let file_plural = if self.files_changed == 1 { "file" } else { "files" };
        write!(
            f,
            "{} {} changed, +{} lines, -{} lines",
            self.files_changed, file_plural, self.additions, self.deletions
        )
    }
}

/// Computes file and line change statistics from a unified diff string.
pub fn compute_diff_stats(diff_text: &str) -> DiffStats {
    let mut files_changed = 0;
    let mut additions = 0;
    let mut deletions = 0;

    for line in diff_text.lines() {
        if line.starts_with("diff --git ") {
            files_changed += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }

    DiffStats {
        files_changed,
        additions,
        deletions,
    }
}

/// Status of a file within a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FileDiffStatus {
    Modified,
    Added,
    Deleted,
    Binary,
}

/// Type of cell within a split side-by-side diff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SplitCellType {
    Context,
    Addition,
    Deletion,
    Empty,
}

/// A single cell (left or right side) in a side-by-side split diff row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SplitCell {
    pub line_no: Option<usize>,
    pub content: String,
    pub cell_type: SplitCellType,
    /// Character offset (start, end) within content modified for intraline highlighting.
    pub highlight_range: Option<(usize, usize)>,
}

/// A paired row in a side-by-side split diff view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SplitRow {
    pub left: SplitCell,
    pub right: SplitCell,
}

/// A single row in a unified line-by-line diff view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnifiedRow {
    pub old_line_no: Option<usize>,
    pub new_line_no: Option<usize>,
    pub marker: char, // ' ', '+', '-'
    pub content: String,
}

/// A structured diff hunk containing both unified and split representations.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub unified_rows: Vec<UnifiedRow>,
    pub split_rows: Vec<SplitRow>,
}

/// A structured diff for an individual file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: FileDiffStatus,
    pub hunks: Vec<DiffHunk>,
    pub stats: DiffStats,
    pub is_binary: bool,
}

impl FileDiff {
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("unknown")
    }
}

/// A complete structured diff report spanning all changed files.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffReport {
    pub files: Vec<FileDiff>,
    pub total_stats: DiffStats,
}

impl DiffReport {
    /// Formats the structured diff report into standard unified diff patch text.
    pub fn to_unified_string(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            let old_header = file
                .old_path
                .as_deref()
                .map_or("/dev/null".to_string(), |p| format!("a/{}", p));
            let new_header = file
                .new_path
                .as_deref()
                .map_or("/dev/null".to_string(), |p| format!("b/{}", p));

            if file.is_binary {
                out.push_str(&format!(
                    "diff --git {} {}\nBinary files {} and {} differ\n",
                    old_header, new_header, old_header, new_header
                ));
                continue;
            }

            out.push_str(&format!("diff --git {} {}\n", old_header, new_header));
            out.push_str(&format!("--- {}\n", old_header));
            out.push_str(&format!("+++ {}\n", new_header));

            for hunk in &file.hunks {
                out.push_str(&format!(
                    "@@ -{},{} +{},{} @@\n",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ));
                for r in &hunk.unified_rows {
                    out.push(r.marker);
                    out.push_str(&r.content);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Computes character ranges that differ between two modified strings (intraline highlighting).
pub fn compute_intraline_diff(
    old_str: &str,
    new_str: &str,
) -> (Option<(usize, usize)>, Option<(usize, usize)>) {
    if old_str.is_empty() || new_str.is_empty() {
        return (None, None);
    }
    let old_chars: Vec<char> = old_str.chars().collect();
    let new_chars: Vec<char> = new_str.chars().collect();

    // Common prefix
    let mut prefix_len = 0;
    while prefix_len < old_chars.len()
        && prefix_len < new_chars.len()
        && old_chars[prefix_len] == new_chars[prefix_len]
    {
        prefix_len += 1;
    }

    // Common suffix
    let mut suffix_len = 0;
    while suffix_len < (old_chars.len() - prefix_len)
        && suffix_len < (new_chars.len() - prefix_len)
    {
        if old_chars[old_chars.len() - 1 - suffix_len]
            == new_chars[new_chars.len() - 1 - suffix_len]
        {
            suffix_len += 1;
        } else {
            break;
        }
    }

    let left_start = prefix_len;
    let left_end = old_chars.len().saturating_sub(suffix_len);
    let right_start = prefix_len;
    let right_end = new_chars.len().saturating_sub(suffix_len);

    let left_range = if left_start < left_end {
        Some((left_start, left_end))
    } else {
        None
    };
    let right_range = if right_start < right_end {
        Some((right_start, right_end))
    } else {
        None
    };

    (left_range, right_range)
}

/// Computes the Longest Common Subsequence (LCS) matrix between two slices of line strings.
fn compute_lcs_matrix<'a>(old_lines: &[&'a str], new_lines: &[&'a str]) -> Vec<Vec<usize>> {
    let m = old_lines.len();
    let n = new_lines.len();
    let mut matrix = vec![vec![0; n + 1]; m + 1];

    for i in 0..m {
        for j in 0..n {
            if old_lines[i] == new_lines[j] {
                matrix[i + 1][j + 1] = matrix[i][j] + 1;
            } else {
                matrix[i + 1][j + 1] = matrix[i + 1][j].max(matrix[i][j + 1]);
            }
        }
    }
    matrix
}

/// Backtracks the LCS matrix to produce a sequence of diff lines.
fn backtrack_diff<'a>(
    matrix: &[Vec<usize>],
    old_lines: &[&'a str],
    new_lines: &[&'a str],
) -> Vec<DiffLine<'a>> {
    let mut diff = Vec::new();
    let mut i = old_lines.len();
    let mut j = new_lines.len();

    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_lines[i - 1] == new_lines[j - 1] {
            diff.push(DiffLine::Context(old_lines[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || matrix[i][j - 1] >= matrix[i - 1][j]) {
            diff.push(DiffLine::Addition(new_lines[j - 1]));
            j -= 1;
        } else if i > 0 && (j == 0 || matrix[i][j - 1] < matrix[i - 1][j]) {
            diff.push(DiffLine::Deletion(old_lines[i - 1]));
            i -= 1;
        }
    }

    diff.reverse();
    diff
}

/// Generates a standard unified diff string between two text payloads.
///
/// # Arguments
/// * `old_path` - Relative file path of the old version (e.g. `Some("src/main.rs")`, or `None` if newly created).
/// * `new_path` - Relative file path of the new version (e.g. `Some("src/main.rs")`, or `None` if deleted).
/// * `old_bytes` - Byte slice of the old file (or `None` if newly created).
/// * `new_bytes` - Byte slice of the new file (or `None` if deleted).
///
/// # Returns
/// A formatted unified diff string compatible with `git apply`.
///
/// # Examples
/// ```
/// use graft::git::diff::generate_unified_diff;
///
/// let old_text = b"fn main() {\n    println!(\"old\");\n}\n";
/// let new_text = b"fn main() {\n    println!(\"new\");\n}\n";
/// let diff = generate_unified_diff(
///     Some("src/main.rs"),
///     Some("src/main.rs"),
///     Some(old_text),
///     Some(new_text),
/// );
/// assert!(diff.contains("-    println!(\"old\");"));
/// assert!(diff.contains("+    println!(\"new\");"));
/// ```
/// Computes a structured AST `FileDiff` between two file byte payloads.
pub fn compute_file_diff_ast(
    old_path: Option<&str>,
    new_path: Option<&str>,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
) -> Option<FileDiff> {
    let is_old_empty = old_bytes.is_none() || old_bytes == Some(b"");
    let is_new_empty = new_bytes.is_none() || new_bytes == Some(b"");

    if is_old_empty && is_new_empty {
        return None;
    }

    let is_old_binary = old_bytes.map_or(false, |b| b.contains(&0));
    let is_new_binary = new_bytes.map_or(false, |b| b.contains(&0));

    let old_path_str = old_path.map(|p| p.to_string());
    let new_path_str = new_path.map(|p| p.to_string());

    if is_old_binary || is_new_binary {
        return Some(FileDiff {
            old_path: old_path_str,
            new_path: new_path_str,
            status: FileDiffStatus::Binary,
            hunks: Vec::new(),
            stats: DiffStats {
                files_changed: 1,
                additions: 0,
                deletions: 0,
            },
            is_binary: true,
        });
    }

    let status = match (old_path, new_path) {
        (None, Some(_)) => FileDiffStatus::Added,
        (Some(_), None) => FileDiffStatus::Deleted,
        _ => FileDiffStatus::Modified,
    };

    let old_str = old_bytes.map_or("", |b| std::str::from_utf8(b).unwrap_or(""));
    let new_str = new_bytes.map_or("", |b| std::str::from_utf8(b).unwrap_or(""));

    let old_lines: Vec<&str> = if old_str.is_empty() {
        Vec::new()
    } else {
        old_str.lines().collect()
    };
    let new_lines: Vec<&str> = if new_str.is_empty() {
        Vec::new()
    } else {
        new_str.lines().collect()
    };

    let matrix = compute_lcs_matrix(&old_lines, &new_lines);
    let diff_lines = backtrack_diff(&matrix, &old_lines, &new_lines);

    let has_changes = diff_lines
        .iter()
        .any(|line| matches!(line, DiffLine::Addition(_) | DiffLine::Deletion(_)));
    if !has_changes {
        return None;
    }

    let mut unified_rows = Vec::new();
    let mut split_rows = Vec::new();

    let mut cur_old_lineno = 1;
    let mut cur_new_lineno = 1;

    let mut idx = 0;
    while idx < diff_lines.len() {
        match diff_lines[idx] {
            DiffLine::Context(c) => {
                unified_rows.push(UnifiedRow {
                    old_line_no: Some(cur_old_lineno),
                    new_line_no: Some(cur_new_lineno),
                    marker: ' ',
                    content: c.to_string(),
                });
                split_rows.push(SplitRow {
                    left: SplitCell {
                        line_no: Some(cur_old_lineno),
                        content: c.to_string(),
                        cell_type: SplitCellType::Context,
                        highlight_range: None,
                    },
                    right: SplitCell {
                        line_no: Some(cur_new_lineno),
                        content: c.to_string(),
                        cell_type: SplitCellType::Context,
                        highlight_range: None,
                    },
                });
                cur_old_lineno += 1;
                cur_new_lineno += 1;
                idx += 1;
            }
            _ => {
                let mut dels: Vec<(usize, &str)> = Vec::new();
                let mut adds: Vec<(usize, &str)> = Vec::new();

                while idx < diff_lines.len() {
                    match diff_lines[idx] {
                        DiffLine::Deletion(d) => {
                            dels.push((cur_old_lineno, d));
                            cur_old_lineno += 1;
                            idx += 1;
                        }
                        DiffLine::Addition(a) => {
                            adds.push((cur_new_lineno, a));
                            cur_new_lineno += 1;
                            idx += 1;
                        }
                        DiffLine::Context(_) => break,
                    }
                }

                for (old_num, d) in &dels {
                    unified_rows.push(UnifiedRow {
                        old_line_no: Some(*old_num),
                        new_line_no: None,
                        marker: '-',
                        content: d.to_string(),
                    });
                }
                for (new_num, a) in &adds {
                    unified_rows.push(UnifiedRow {
                        old_line_no: None,
                        new_line_no: Some(*new_num),
                        marker: '+',
                        content: a.to_string(),
                    });
                }

                let max_len = dels.len().max(adds.len());
                for k in 0..max_len {
                    let (left_hl, right_hl) = if k < dels.len() && k < adds.len() {
                        compute_intraline_diff(dels[k].1, adds[k].1)
                    } else {
                        (None, None)
                    };

                    let left = if let Some((old_num, d)) = dels.get(k) {
                        SplitCell {
                            line_no: Some(*old_num),
                            content: d.to_string(),
                            cell_type: SplitCellType::Deletion,
                            highlight_range: left_hl,
                        }
                    } else {
                        SplitCell {
                            line_no: None,
                            content: String::new(),
                            cell_type: SplitCellType::Empty,
                            highlight_range: None,
                        }
                    };

                    let right = if let Some((new_num, a)) = adds.get(k) {
                        SplitCell {
                            line_no: Some(*new_num),
                            content: a.to_string(),
                            cell_type: SplitCellType::Addition,
                            highlight_range: right_hl,
                        }
                    } else {
                        SplitCell {
                            line_no: None,
                            content: String::new(),
                            cell_type: SplitCellType::Empty,
                            highlight_range: None,
                        }
                    };

                    split_rows.push(SplitRow { left, right });
                }
            }
        }
    }

    let additions = unified_rows.iter().filter(|r| r.marker == '+').count();
    let deletions = unified_rows.iter().filter(|r| r.marker == '-').count();

    let hunk = DiffHunk {
        old_start: if old_lines.is_empty() { 0 } else { 1 },
        old_len: old_lines.len(),
        new_start: if new_lines.is_empty() { 0 } else { 1 },
        new_len: new_lines.len(),
        unified_rows,
        split_rows,
    };

    Some(FileDiff {
        old_path: old_path_str,
        new_path: new_path_str,
        status,
        hunks: vec![hunk],
        stats: DiffStats {
            files_changed: 1,
            additions,
            deletions,
        },
        is_binary: false,
    })
}

/// Generates a standard unified diff string between two text payloads.
pub fn generate_unified_diff(
    old_path: Option<&str>,
    new_path: Option<&str>,
    old_bytes: Option<&[u8]>,
    new_bytes: Option<&[u8]>,
) -> String {
    match compute_file_diff_ast(old_path, new_path, old_bytes, new_bytes) {
        Some(file_diff) => {
            let report = DiffReport {
                total_stats: file_diff.stats,
                files: vec![file_diff],
            };
            report.to_unified_string()
        }
        None => String::new(),
    }
}

/// Helper to retrieve the original base bytes for a file from the CAS Merkle tree.
fn get_base_file_bytes<B: BlobStore + ?Sized>(
    base_root: &Option<Digest>,
    rel_path: &Path,
    blob_store: &B,
) -> Option<Vec<u8>> {
    let root = base_root.as_ref()?;
    let entry = crate::cas::commit::get_tree_entry(root, rel_path, blob_store).ok()??;
    match entry {
        ManifestEntry::File { digest, .. } => blob_store.get(&digest).ok()?,
        ManifestEntry::ChunkedFile {
            manifest_digest, ..
        } => {
            let m_bytes = blob_store.get(&manifest_digest).ok()??;
            let manifest = crate::cas::chunker::ChunkManifest::from_bytes(&m_bytes).ok()?;
            let mut out = Vec::new();
            crate::cas::chunker::stream_chunked_file(&manifest, blob_store, &mut out).ok()?;
            Some(out)
        }
        ManifestEntry::Directory { .. } => None,
    }
}

/// Generates a complete structured diff report for all modifications in an active agent workspace.
pub fn diff_workspace_report<B: BlobStore + ?Sized>(
    config: &WorkspaceConfig,
    blob_store: &B,
) -> io::Result<DiffReport> {
    let manager = WorkspaceManager::new(
        config
            .upper_dir
            .parent()
            .unwrap_or_else(|| Path::new("/tmp")),
    );
    let changes = manager.scan_changes_with_store(config, Some(blob_store))?;

    let base_root = if !config.commit.is_empty() && config.commit != "unknown" {
        crate::cas::commit::resolve_root_tree(&config.commit, blob_store)
            .ok()
            .flatten()
    } else {
        None
    };

    let mut files = Vec::new();
    let mut total_additions = 0;
    let mut total_deletions = 0;

    for change in changes {
        let rel_str = change.relative_path.to_string_lossy().to_string();

        let file_diff_opt = match change.change_type {
            ChangeType::Created => {
                let new_bytes = fs::read(&change.disk_path)?;
                compute_file_diff_ast(None, Some(&rel_str), None, Some(&new_bytes))
            }
            ChangeType::Modified => {
                let old_bytes =
                    get_base_file_bytes(&base_root, &change.relative_path, blob_store)
                        .unwrap_or_else(|| {
                            let lower_file = config.lower_dir.join(&change.relative_path);
                            if lower_file.exists() {
                                fs::read(&lower_file).unwrap_or_default()
                            } else {
                                Vec::new()
                            }
                        });
                let new_bytes = fs::read(&change.disk_path)?;
                compute_file_diff_ast(
                    Some(&rel_str),
                    Some(&rel_str),
                    Some(&old_bytes),
                    Some(&new_bytes),
                )
            }
            ChangeType::Deleted => {
                let old_bytes =
                    get_base_file_bytes(&base_root, &change.relative_path, blob_store)
                        .unwrap_or_else(|| {
                            let lower_file = config.lower_dir.join(&change.relative_path);
                            if lower_file.is_file() {
                                fs::read(&lower_file).unwrap_or_default()
                            } else {
                                Vec::new()
                            }
                        });
                compute_file_diff_ast(Some(&rel_str), None, Some(&old_bytes), None)
            }
        };

        if let Some(fd) = file_diff_opt {
            total_additions += fd.stats.additions;
            total_deletions += fd.stats.deletions;
            files.push(fd);
        }
    }

    let total_stats = DiffStats {
        files_changed: files.len(),
        additions: total_additions,
        deletions: total_deletions,
    };

    Ok(DiffReport { files, total_stats })
}

/// Generates a complete unified diff for all modifications in an active agent workspace.
pub fn diff_workspace<B: BlobStore + ?Sized>(
    config: &WorkspaceConfig,
    blob_store: &B,
) -> io::Result<String> {
    diff_workspace_report(config, blob_store).map(|r| r.to_unified_string())
}

/// Recursively computes a structured diff report between two repository Merkle trees in CAS.
pub fn diff_trees_report<B: BlobStore + ?Sized>(
    base_root: &Digest,
    target_root: &Digest,
    blob_store: &B,
) -> io::Result<DiffReport> {
    let mut files = Vec::new();
    diff_trees_report_recursive(base_root, target_root, "", blob_store, &mut files)?;

    let total_additions = files.iter().map(|f| f.stats.additions).sum();
    let total_deletions = files.iter().map(|f| f.stats.deletions).sum();

    let total_stats = DiffStats {
        files_changed: files.len(),
        additions: total_additions,
        deletions: total_deletions,
    };

    Ok(DiffReport { files, total_stats })
}

fn diff_trees_report_recursive<B: BlobStore + ?Sized>(
    base_dir_digest: &Digest,
    target_dir_digest: &Digest,
    prefix: &str,
    blob_store: &B,
    files: &mut Vec<FileDiff>,
) -> io::Result<()> {
    if base_dir_digest == target_dir_digest {
        return Ok(());
    }

    let base_manifest = if blob_store.has(base_dir_digest) {
        let bytes = blob_store.get(base_dir_digest)?.unwrap_or_default();
        DirectoryManifest::from_bytes(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    } else {
        DirectoryManifest::default()
    };

    let target_manifest = if blob_store.has(target_dir_digest) {
        let bytes = blob_store.get(target_dir_digest)?.unwrap_or_default();
        DirectoryManifest::from_bytes(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    } else {
        DirectoryManifest::default()
    };

    let mut all_keys: BTreeMap<String, ()> = BTreeMap::new();
    for k in base_manifest.entries.keys() {
        all_keys.insert(k.clone(), ());
    }
    for k in target_manifest.entries.keys() {
        all_keys.insert(k.clone(), ());
    }

    for name in all_keys.keys() {
        let base_entry = base_manifest.entries.get(name);
        let target_entry = target_manifest.entries.get(name);
        let current_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", prefix, name)
        };

        match (base_entry, target_entry) {
            (Some(ManifestEntry::File { digest: d1, .. }), Some(ManifestEntry::File { digest: d2, .. })) => {
                if d1 != d2 {
                    let old_bytes = blob_store.get(d1)?.unwrap_or_default();
                    let new_bytes = blob_store.get(d2)?.unwrap_or_default();
                    if let Some(fd) = compute_file_diff_ast(
                        Some(&current_path),
                        Some(&current_path),
                        Some(&old_bytes),
                        Some(&new_bytes),
                    ) {
                        files.push(fd);
                    }
                }
            }
            (
                Some(ManifestEntry::ChunkedFile { manifest_digest: d1, .. }),
                Some(ManifestEntry::ChunkedFile { manifest_digest: d2, .. }),
            ) => {
                if d1 != d2 {
                    files.push(FileDiff {
                        old_path: Some(current_path.clone()),
                        new_path: Some(current_path.clone()),
                        status: FileDiffStatus::Binary,
                        hunks: Vec::new(),
                        stats: DiffStats {
                            files_changed: 1,
                            additions: 0,
                            deletions: 0,
                        },
                        is_binary: true,
                    });
                }
            }
            (Some(ManifestEntry::ChunkedFile { .. }), Some(ManifestEntry::File { .. }))
            | (Some(ManifestEntry::File { .. }), Some(ManifestEntry::ChunkedFile { .. })) => {
                files.push(FileDiff {
                    old_path: Some(current_path.clone()),
                    new_path: Some(current_path.clone()),
                    status: FileDiffStatus::Binary,
                    hunks: Vec::new(),
                    stats: DiffStats {
                        files_changed: 1,
                        additions: 0,
                        deletions: 0,
                    },
                    is_binary: true,
                });
            }
            (None, Some(ManifestEntry::File { digest: d2, .. })) => {
                let new_bytes = blob_store.get(d2)?.unwrap_or_default();
                if let Some(fd) = compute_file_diff_ast(None, Some(&current_path), None, Some(&new_bytes)) {
                    files.push(fd);
                }
            }
            (None, Some(ManifestEntry::ChunkedFile { total_size, .. })) => {
                files.push(FileDiff {
                    old_path: None,
                    new_path: Some(current_path.clone()),
                    status: FileDiffStatus::Binary,
                    hunks: Vec::new(),
                    stats: DiffStats {
                        files_changed: 1,
                        additions: *total_size as usize,
                        deletions: 0,
                    },
                    is_binary: true,
                });
            }
            (Some(ManifestEntry::File { digest: d1, .. }), None) => {
                let old_bytes = blob_store.get(d1)?.unwrap_or_default();
                if let Some(fd) = compute_file_diff_ast(Some(&current_path), None, Some(&old_bytes), None) {
                    files.push(fd);
                }
            }
            (Some(ManifestEntry::ChunkedFile { total_size, .. }), None) => {
                files.push(FileDiff {
                    old_path: Some(current_path.clone()),
                    new_path: None,
                    status: FileDiffStatus::Binary,
                    hunks: Vec::new(),
                    stats: DiffStats {
                        files_changed: 1,
                        additions: 0,
                        deletions: *total_size as usize,
                    },
                    is_binary: true,
                });
            }
            (Some(ManifestEntry::Directory { digest: d1, .. }), Some(ManifestEntry::Directory { digest: d2, .. })) => {
                diff_trees_report_recursive(d1, d2, &current_path, blob_store, files)?;
            }
            (None, Some(ManifestEntry::Directory { digest: d2, .. })) => {
                let empty = Digest::from_bytes(b"empty");
                diff_trees_report_recursive(&empty, d2, &current_path, blob_store, files)?;
            }
            (Some(ManifestEntry::Directory { digest: d1, .. }), None) => {
                let empty = Digest::from_bytes(b"empty");
                diff_trees_report_recursive(d1, &empty, &current_path, blob_store, files)?;
            }
            _ => {}
        }
    }

    Ok(())
}

/// Recursively computes the diff between two repository Merkle trees in CAS.
pub fn diff_trees<B: BlobStore + ?Sized>(
    base_root: &Digest,
    target_root: &Digest,
    blob_store: &B,
) -> io::Result<String> {
    diff_trees_report(base_root, target_root, blob_store).map(|r| r.to_unified_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use tempfile::tempdir;

    #[test]
    fn test_split_diff_ast_and_intraline() {
        let old = b"fn calculate(a: i32) -> i32 {\n    a * 2\n}\n";
        let new = b"fn calculate(a: i32, b: i32) -> i32 {\n    a * b\n}\n";

        let ast = compute_file_diff_ast(Some("math.rs"), Some("math.rs"), Some(old), Some(new)).unwrap();
        assert_eq!(ast.status, FileDiffStatus::Modified);
        assert_eq!(ast.hunks.len(), 1);

        let hunk = &ast.hunks[0];
        assert_eq!(hunk.split_rows.len(), 3);

        // Row 0 is modified line (intraline insertion on right)
        let r0 = &hunk.split_rows[0];
        assert_eq!(r0.left.cell_type, SplitCellType::Deletion);
        assert_eq!(r0.right.cell_type, SplitCellType::Addition);
        assert!(r0.right.highlight_range.is_some());

        // Row 1 is modified line (intraline replacement on both left and right)
        let r1 = &hunk.split_rows[1];
        assert_eq!(r1.left.cell_type, SplitCellType::Deletion);
        assert_eq!(r1.right.cell_type, SplitCellType::Addition);
        assert!(r1.left.highlight_range.is_some());
        assert!(r1.right.highlight_range.is_some());

        // Row 2 is context "}"
        let r2 = &hunk.split_rows[2];
        assert_eq!(r2.left.cell_type, SplitCellType::Context);
        assert_eq!(r2.right.cell_type, SplitCellType::Context);
    }

    #[test]
    fn test_unified_diff_basic() {
        let old = b"line 1\nline 2\nline 3\n";
        let new = b"line 1\nline 2 modified\nline 3\nline 4\n";

        let diff = generate_unified_diff(Some("test.txt"), Some("test.txt"), Some(old), Some(new));

        assert!(diff.contains("--- a/test.txt"));
        assert!(diff.contains("+++ b/test.txt"));
        assert!(diff.contains("-line 2"));
        assert!(diff.contains("+line 2 modified"));
        assert!(diff.contains("+line 4"));
    }

    #[test]
    fn test_unified_diff_new_file() {
        let new = b"brand new file\nsecond line\n";
        let diff = generate_unified_diff(None, Some("new.txt"), None, Some(new));

        assert!(diff.contains("--- /dev/null"));
        assert!(diff.contains("+++ b/new.txt"));
        assert!(diff.contains("+brand new file"));
    }

    #[test]
    fn test_diff_trees() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        let mut base_m = DirectoryManifest::default();
        let f1_digest = store.put(b"hello world\n")?;
        base_m.entries.insert(
            "file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "file.txt".to_string(),
                digest: f1_digest,
            },
        );
        let base_root = store.put(&base_m.serialize())?;

        let mut target_m = DirectoryManifest::default();
        let f2_digest = store.put(b"hello graft\n")?;
        target_m.entries.insert(
            "file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "file.txt".to_string(),
                digest: f2_digest,
            },
        );
        let target_root = store.put(&target_m.serialize())?;

        let diff = diff_trees(&base_root, &target_root, &store)?;
        assert!(diff.contains("-hello world"));
        assert!(diff.contains("+hello graft"));

        Ok(())
    }

    #[test]
    fn test_diff_workspace_with_cas_store() -> io::Result<()> {
        use std::io::Write;

        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path().join("cas"))?;
        let mgr = WorkspaceManager::new(tmp.path().join("storage"));

        let orig_d = store.put(b"original line\n")?;
        let mut manifest = DirectoryManifest::default();
        manifest.entries.insert(
            "file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "file.txt".to_string(),
                digest: orig_d,
            },
        );
        let root_d = store.put(&manifest.serialize())?;

        let config = mgr.prepare_config(
            "diff-cas-task",
            &root_d.to_string(),
            &tmp.path().join("lower"),
            &tmp.path().join("merged"),
        );
        mgr.init_workspace_dirs(&config)?;

        // 1. Edit file.txt in upper
        let mut f1 = fs::File::create(config.upper_dir.join("file.txt"))?;
        f1.write_all(b"modified line\n")?;

        // 2. Add new.txt in upper
        let mut f2 = fs::File::create(config.upper_dir.join("new.txt"))?;
        f2.write_all(b"brand new\n")?;

        let diff = diff_workspace(&config, &store)?;
        assert!(diff.contains("diff --git a/file.txt b/file.txt"));
        assert!(diff.contains("-original line"));
        assert!(diff.contains("+modified line"));
        assert!(diff.contains("diff --git /dev/null b/new.txt"));
        assert!(diff.contains("+brand new"));

        Ok(())
    }
}
