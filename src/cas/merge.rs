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

//! # Server-Side 3-Way Merkle Merge Engine
//!
//! This module implements the 3-way merge and rebase algorithms operating directly on
//! Content-Addressed Storage (CAS) Merkle trees, inspired by Google Piper and Anthropic Scion.
//!
//! ### Why Merkle 3-Way Merge is Faster:
//! In standard Git, merging requires downloading packfiles and walking every file on disk ($O(N)$).
//! In Graft, merging operates directly on the Merkle DAG in $O(\text{changed})$:
//! 1. **$O(1)$ Subtree Resolution**:
//!    - If `ours_digest == base_digest` $\longrightarrow$ take `theirs` in $O(1)$ (no subtree traversal).
//!    - If `theirs_digest == base_digest` $\longrightarrow$ take `ours` in $O(1)$.
//!    - If `ours_digest == theirs_digest` $\longrightarrow$ identical modifications, take `ours` in $O(1)$.
//! 2. **Targeted Line Merging**:
//!    - Only files modified concurrently in both branches are downloaded from CAS.
//!    - Line-level 3-way merge is performed. Clean non-overlapping edits merge automatically.
//!    - Overlapping edits produce standard conflict markers (`<<<<<<< ours`, `=======`, `>>>>>>> theirs`).

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::{Commit, DirectoryManifest, ManifestEntry};
use crate::cas::digest::Digest;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Conflict resolution strategy for 3-way Merkle merges and cherry-picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictStrategy {
    /// Insert standard conflict markers: <<<<<<< ours ... ======= ... >>>>>>> theirs
    #[default]
    Markers,
    /// In case of conflict, unconditionally take ours (current branch / workspace version).
    Ours,
    /// In case of conflict, unconditionally take theirs (incoming cherry-picked version).
    Theirs,
    /// Fail immediately on any merge conflict.
    Fail,
}

impl std::str::FromStr for ConflictStrategy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "markers" | "marker" => Ok(Self::Markers),
            "ours" => Ok(Self::Ours),
            "theirs" => Ok(Self::Theirs),
            "fail" | "abort-on-conflict" => Ok(Self::Fail),
            other => Err(format!(
                "Invalid conflict strategy '{}'. Expected 'markers', 'ours', 'theirs', or 'fail'.",
                other
            )),
        }
    }
}

/// Summary of a 3-way Merkle merge operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    /// Root Merkle digest of the resulting merged tree.
    pub root_tree_digest: Digest,
    /// True if one or more files had merge conflicts.
    pub has_conflicts: bool,
    /// List of relative file paths that experienced merge conflicts.
    pub conflict_files: Vec<String>,
    /// List of relative file paths that were cleanly merged or taken from either side.
    pub merged_files: Vec<String>,
}

/// Finds the Best Common Ancestor (BCA) between two commits using Breadth-First Search (BFS).
///
/// # Arguments
/// * `commit_a` - Digest of the first commit.
/// * `commit_b` - Digest of the second commit.
/// * `blob_store` - The CAS backing store holding commit objects.
///
/// # Returns
/// `Ok(Some(ancestor_digest))` if a common ancestor is found, or `Ok(None)` if histories are disjoint.
///
/// # Errors
/// Returns an `io::Error` if reading commit objects from CAS fails.
pub fn find_common_ancestor<B: BlobStore + ?Sized>(
    commit_a: &Digest,
    commit_b: &Digest,
    blob_store: &B,
) -> io::Result<Option<Digest>> {
    if commit_a == commit_b {
        return Ok(Some(*commit_a));
    }

    // Traverse ancestors of commit_a and collect into a set
    let mut visited_a: HashSet<Digest> = HashSet::new();
    let mut queue_a: VecDeque<Digest> = VecDeque::new();
    queue_a.push_back(*commit_a);
    visited_a.insert(*commit_a);

    while let Some(current) = queue_a.pop_front() {
        if let Some(bytes) = blob_store.get(&current)? {
            if let Ok(c) = Commit::deserialize(&bytes) {
                for parent in c.parent_commits {
                    if visited_a.insert(parent) {
                        queue_a.push_back(parent);
                    }
                }
            }
        }
    }

    // Traverse ancestors of commit_b and return the first one present in visited_a
    let mut queue_b: VecDeque<Digest> = VecDeque::new();
    let mut visited_b: HashSet<Digest> = HashSet::new();
    queue_b.push_back(*commit_b);
    visited_b.insert(*commit_b);

    while let Some(current) = queue_b.pop_front() {
        if visited_a.contains(&current) {
            return Ok(Some(current));
        }

        if let Some(bytes) = blob_store.get(&current)? {
            if let Ok(c) = Commit::deserialize(&bytes) {
                for parent in c.parent_commits {
                    if visited_b.insert(parent) {
                        queue_b.push_back(parent);
                    }
                }
            }
        }
    }

    Ok(None)
}

/// Summary of a change hunk relative to base lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffHunk<'a> {
    base_start: usize,
    base_end: usize,
    lines: Vec<&'a str>,
}

fn compute_lcs_matches(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let m = a.len();
    let n = b.len();
    if m == 0 || n == 0 {
        return Vec::new();
    }
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..m {
        for j in 0..n {
            if a[i] == b[j] {
                dp[i + 1][j + 1] = dp[i][j] + 1;
            } else {
                dp[i + 1][j + 1] = dp[i][j + 1].max(dp[i + 1][j]);
            }
        }
    }
    let mut matches = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 && j > 0 {
        if a[i - 1] == b[j - 1] {
            matches.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    matches.reverse();
    matches
}

fn extract_diff_hunks<'a>(base: &[&'a str], target: &[&'a str]) -> Vec<DiffHunk<'a>> {
    let matches = compute_lcs_matches(base, target);
    let mut hunks = Vec::new();
    let mut b_prev = 0;
    let mut t_prev = 0;

    for (b_idx, t_idx) in matches {
        if b_prev < b_idx || t_prev < t_idx {
            hunks.push(DiffHunk {
                base_start: b_prev,
                base_end: b_idx,
                lines: target[t_prev..t_idx].to_vec(),
            });
        }
        b_prev = b_idx + 1;
        t_prev = t_idx + 1;
    }

    if b_prev < base.len() || t_prev < target.len() {
        hunks.push(DiffHunk {
            base_start: b_prev,
            base_end: base.len(),
            lines: target[t_prev..target.len()].to_vec(),
        });
    }

    hunks
}

/// Reconstructs the lines produced by a branch over a base range `[span_start..span_end]`.
fn materialise_span_lines<'a>(
    base: &[&'a str],
    hunks: &[DiffHunk<'a>],
    span_start: usize,
    span_end: usize,
) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut curr = span_start;

    for h in hunks {
        if h.base_end < span_start || h.base_start > span_end {
            continue;
        }
        if curr < h.base_start {
            let end_idx = h.base_start.min(span_end);
            out.extend_from_slice(&base[curr..end_idx]);
            curr = end_idx;
        }
        out.extend_from_slice(&h.lines);
        curr = h.base_end.max(curr);
    }

    if curr < span_end {
        out.extend_from_slice(&base[curr..span_end]);
    }

    out
}

/// Performs a 3-way line-level merge between base, ours, and theirs text payloads.
///
/// # Arguments
/// * `base` - Text of the common ancestor version.
/// * `ours` - Text of the current branch version.
/// * `theirs` - Text of the incoming branch version.
///
/// # Returns
/// A tuple of `(merged_text, has_conflict)`. If `has_conflict` is true, the text
/// contains standard `<<<<<<< ours`, `=======`, `>>>>>>> theirs` markers.
///
/// # Examples
/// ```
/// use graft::cas::merge::merge3_text;
///
/// let base = "line 1\n";
/// let ours = "line 1\nline 2 (ours)\n";
/// let theirs = "line 1\nline 3 (theirs)\n";
/// let (merged, conflict) = merge3_text(base, ours, theirs);
/// assert!(!conflict);
/// assert!(merged.contains("line 2 (ours)"));
/// assert!(merged.contains("line 3 (theirs)"));
/// ```
/// Performs a 3-way line-level merge between base, ours, and theirs text payloads with a custom conflict strategy.
pub fn merge3_text_with_strategy(
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

    let base_lines: Vec<&str> = base.lines().collect();
    let ours_lines: Vec<&str> = ours.lines().collect();
    let theirs_lines: Vec<&str> = theirs.lines().collect();

    let ours_hunks = extract_diff_hunks(&base_lines, &ours_lines);
    let theirs_hunks = extract_diff_hunks(&base_lines, &theirs_lines);

    let mut merged_lines: Vec<String> = Vec::new();
    let mut has_conflict = false;

    let mut b_curr = 0;
    let mut o_idx = 0;
    let mut t_idx = 0;

    while b_curr <= base_lines.len() {
        let has_o = o_idx < ours_hunks.len();
        let has_t = t_idx < theirs_hunks.len();

        if !has_o && !has_t {
            if b_curr < base_lines.len() {
                for line in &base_lines[b_curr..] {
                    merged_lines.push(line.to_string());
                }
            }
            break;
        }

        let next_o_start = if has_o { ours_hunks[o_idx].base_start } else { usize::MAX };
        let next_t_start = if has_t { theirs_hunks[t_idx].base_start } else { usize::MAX };

        // If neither hunk starts at b_curr, emit unchanged base lines up to the next hunk
        if b_curr < next_o_start && b_curr < next_t_start {
            let next_event = next_o_start.min(next_t_start).min(base_lines.len());
            for line in &base_lines[b_curr..next_event] {
                merged_lines.push(line.to_string());
            }
            b_curr = next_event;
            continue;
        }

        // Check if only ours touches this region
        if has_o && ours_hunks[o_idx].base_start == b_curr && (!has_t || ours_hunks[o_idx].base_end < next_t_start) {
            for line in &ours_hunks[o_idx].lines {
                merged_lines.push(line.to_string());
            }
            b_curr = ours_hunks[o_idx].base_end.max(b_curr);
            o_idx += 1;
            continue;
        }

        // Check if only theirs touches this region
        if has_t && theirs_hunks[t_idx].base_start == b_curr && (!has_o || theirs_hunks[t_idx].base_end < next_o_start) {
            for line in &theirs_hunks[t_idx].lines {
                merged_lines.push(line.to_string());
            }
            b_curr = theirs_hunks[t_idx].base_end.max(b_curr);
            t_idx += 1;
            continue;
        }

        // Both touch or overlap: find the full overlapping span
        let span_start = b_curr;
        let mut span_end = b_curr;

        if has_o && ours_hunks[o_idx].base_start <= span_end {
            span_end = span_end.max(ours_hunks[o_idx].base_end);
        }
        if has_t && theirs_hunks[t_idx].base_start <= span_end {
            span_end = span_end.max(theirs_hunks[t_idx].base_end);
        }

        let o_start_idx = o_idx;
        let t_start_idx = t_idx;

        loop {
            let mut expanded = false;
            while o_idx < ours_hunks.len() && ours_hunks[o_idx].base_start <= span_end {
                if ours_hunks[o_idx].base_end > span_end {
                    span_end = ours_hunks[o_idx].base_end;
                    expanded = true;
                }
                o_idx += 1;
            }
            while t_idx < theirs_hunks.len() && theirs_hunks[t_idx].base_start <= span_end {
                if theirs_hunks[t_idx].base_end > span_end {
                    span_end = theirs_hunks[t_idx].base_end;
                    expanded = true;
                }
                t_idx += 1;
            }
            if !expanded {
                break;
            }
        }

        let o_slice = &ours_hunks[o_start_idx..o_idx];
        let t_slice = &theirs_hunks[t_start_idx..t_idx];

        let o_res = materialise_span_lines(&base_lines, o_slice, span_start, span_end);
        let t_res = materialise_span_lines(&base_lines, t_slice, span_start, span_end);

        if o_res == t_res {
            for line in o_res {
                merged_lines.push(line.to_string());
            }
        } else if span_start == base_lines.len() {
            // Clean EOF append: both appended new content to the end of the file
            for line in o_res {
                merged_lines.push(line.to_string());
            }
            for line in t_res {
                merged_lines.push(line.to_string());
            }
        } else {
            // Overlapping conflicting modification: apply conflict strategy
            has_conflict = true;
            match strategy {
                ConflictStrategy::Ours => {
                    for line in o_res {
                        merged_lines.push(line.to_string());
                    }
                }
                ConflictStrategy::Theirs => {
                    for line in t_res {
                        merged_lines.push(line.to_string());
                    }
                }
                ConflictStrategy::Markers | ConflictStrategy::Fail => {
                    merged_lines.push("<<<<<<< ours".to_string());
                    for line in o_res {
                        merged_lines.push(line.to_string());
                    }
                    merged_lines.push("=======".to_string());
                    for line in t_res {
                        merged_lines.push(line.to_string());
                    }
                    merged_lines.push(">>>>>>> theirs".to_string());
                }
            }
        }

        b_curr = span_end.max(b_curr);
        if span_start == span_end && (has_o || has_t) {
            // Insertion at point without consuming base lines: do not advance b_curr automatically
        }
    }

    let mut result = merged_lines.join("\n");
    if !result.is_empty() && (ours.ends_with('\n') || theirs.ends_with('\n') || base.ends_with('\n')) {
        result.push('\n');
    }

    (result, has_conflict)
}

/// Performs a 3-way line-level merge between base, ours, and theirs text payloads.
///
/// # Arguments
/// * `base` - Text of the common ancestor version.
/// * `ours` - Text of the current branch version.
/// * `theirs` - Text of the incoming branch version.
///
/// # Returns
/// A tuple of `(merged_text, has_conflict)`. If `has_conflict` is true, the text
/// contains standard `<<<<<<< ours`, `=======`, `>>>>>>> theirs` markers.
///
/// # Examples
/// ```
/// use graft::cas::merge::merge3_text;
///
/// let base = "line 1\n";
/// let ours = "line 1\nline 2 (ours)\n";
/// let theirs = "line 1\nline 3 (theirs)\n";
/// let (merged, conflict) = merge3_text(base, ours, theirs);
/// assert!(!conflict);
/// assert!(merged.contains("line 2 (ours)"));
/// assert!(merged.contains("line 3 (theirs)"));
/// ```
pub fn merge3_text(base: &str, ours: &str, theirs: &str) -> (String, bool) {
    merge3_text_with_strategy(base, ours, theirs, ConflictStrategy::Markers)
}

/// Performs a 3-way Merkle tree merge with an explicit conflict resolution strategy.
pub fn merge_trees_with_strategy<B: BlobStore + ?Sized>(
    base_root: &Digest,
    ours_root: &Digest,
    theirs_root: &Digest,
    blob_store: &B,
    strategy: ConflictStrategy,
) -> io::Result<MergeResult> {
    let mut conflict_files = Vec::new();
    let mut merged_files = Vec::new();

    let merged_root = merge_directories_recursive(
        base_root,
        ours_root,
        theirs_root,
        "",
        blob_store,
        strategy,
        &mut conflict_files,
        &mut merged_files,
    )?;

    let has_conflicts = !conflict_files.is_empty();

    if has_conflicts && strategy == ConflictStrategy::Fail {
        return Err(io::Error::other(format!(
            "Merge failed due to conflicts in: {:?}",
            conflict_files
        )));
    }

    Ok(MergeResult {
        root_tree_digest: merged_root,
        has_conflicts,
        conflict_files,
        merged_files,
    })
}

/// Performs a 3-way Merkle tree merge between Base, Ours, and Theirs root directory digests.
///
/// Evaluates directory manifests recursively, resolving subtrees in $O(1)$ where digests match,
/// and performing line-level 3-way merges on files modified in both branches.
///
/// # Arguments
/// * `base_root` - Root Merkle digest of the common ancestor commit.
/// * `ours_root` - Root Merkle digest of our current branch.
/// * `theirs_root` - Root Merkle digest of their incoming branch.
/// * `blob_store` - The backing CAS store holding directory manifests and file blobs.
///
/// # Returns
/// A `MergeResult` containing the root digest of the merged tree and conflict statuses.
///
/// # Errors
/// Returns an `io::Error` if manifest deserialization or CAS writes fail.
pub fn merge_trees<B: BlobStore + ?Sized>(
    base_root: &Digest,
    ours_root: &Digest,
    theirs_root: &Digest,
    blob_store: &B,
) -> io::Result<MergeResult> {
    merge_trees_with_strategy(
        base_root,
        ours_root,
        theirs_root,
        blob_store,
        ConflictStrategy::Markers,
    )
}

fn load_manifest<B: BlobStore + ?Sized>(
    digest: &Digest,
    blob_store: &B,
) -> io::Result<DirectoryManifest> {
    if digest.size_bytes == 0 && !blob_store.has(digest) {
        return Ok(DirectoryManifest::default());
    }
    if let Some(bytes) = blob_store.get(digest)? {
        DirectoryManifest::from_bytes(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    } else {
        Ok(DirectoryManifest::default())
    }
}

fn merge_directories_recursive<B: BlobStore + ?Sized>(
    base_dir: &Digest,
    ours_dir: &Digest,
    theirs_dir: &Digest,
    prefix: &str,
    blob_store: &B,
    strategy: ConflictStrategy,
    conflict_files: &mut Vec<String>,
    merged_files: &mut Vec<String>,
) -> io::Result<Digest> {
    // -------------------------------------------------------------------------
    // O(1) FAST PATHS: Digest Equality
    // -------------------------------------------------------------------------
    // Case 1: Both sides made identical changes (or neither changed)
    if ours_dir == theirs_dir {
        return Ok(*ours_dir);
    }
    // Case 2: Ours unchanged relative to Base -> accept Theirs in O(1)
    if ours_dir == base_dir {
        return Ok(*theirs_dir);
    }
    // Case 3: Theirs unchanged relative to Base -> keep Ours in O(1)
    if theirs_dir == base_dir {
        return Ok(*ours_dir);
    }

    // Load manifests from CAS
    let base_m = load_manifest(base_dir, blob_store)?;
    let ours_m = load_manifest(ours_dir, blob_store)?;
    let theirs_m = load_manifest(theirs_dir, blob_store)?;

    let mut merged_manifest = DirectoryManifest::default();

    // Union of all filenames
    let mut all_keys: BTreeMap<String, ()> = BTreeMap::new();
    for k in base_m.entries.keys() {
        all_keys.insert(k.clone(), ());
    }
    for k in ours_m.entries.keys() {
        all_keys.insert(k.clone(), ());
    }
    for k in theirs_m.entries.keys() {
        all_keys.insert(k.clone(), ());
    }

    for name in all_keys.keys() {
        let b = base_m.entries.get(name);
        let o = ours_m.entries.get(name);
        let t = theirs_m.entries.get(name);

        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", prefix, name)
        };

        match (b, o, t) {
            // Case 1: Identical on both sides -> take Ours
            (_, Some(o_entry), Some(t_entry)) if o_entry == t_entry => {
                merged_manifest.entries.insert(name.clone(), o_entry.clone());
            }
            // Case 2: Ours matches Base -> take Theirs
            (Some(b_entry), Some(o_entry), Some(t_entry)) if o_entry == b_entry => {
                merged_manifest.entries.insert(name.clone(), t_entry.clone());
                merged_files.push(path);
            }
            // Case 3: Theirs matches Base -> take Ours
            (Some(b_entry), Some(o_entry), Some(t_entry)) if t_entry == b_entry => {
                merged_manifest.entries.insert(name.clone(), o_entry.clone());
                merged_files.push(path);
            }
            // Case 4: Both deleted relative to Base
            (Some(_), None, None) => {
                // Kept deleted
            }
            // Case 5: Deleted in Ours, untouched in Theirs -> deleted
            (Some(b_entry), None, Some(t_entry)) if t_entry == b_entry => {
                // Kept deleted
            }
            // Case 6: Deleted in Theirs, untouched in Ours -> deleted
            (Some(b_entry), Some(o_entry), None) if o_entry == b_entry => {
                // Kept deleted
            }
            // Case 7: Added only in Ours
            (None, Some(o_entry), None) => {
                merged_manifest.entries.insert(name.clone(), o_entry.clone());
                merged_files.push(path);
            }
            // Case 8: Added only in Theirs
            (None, None, Some(t_entry)) => {
                merged_manifest.entries.insert(name.clone(), t_entry.clone());
                merged_files.push(path);
            }
            // Case 9: Both added or modified as Directories -> recurse
            (
                b_opt,
                Some(ManifestEntry::Directory { digest: d_o, .. }),
                Some(ManifestEntry::Directory { digest: d_t, .. }),
            ) => {
                let empty_digest = Digest::from_bytes(b"empty");
                let d_b = match b_opt {
                    Some(ManifestEntry::Directory { digest, .. }) => digest,
                    _ => &empty_digest,
                };
                let sub_merged = merge_directories_recursive(
                    d_b,
                    d_o,
                    d_t,
                    &path,
                    blob_store,
                    strategy,
                    conflict_files,
                    merged_files,
                )?;
                merged_manifest.entries.insert(
                    name.clone(),
                    ManifestEntry::Directory {
                        name: name.clone(),
                        digest: sub_merged,
                    },
                );
            }
            // Case 10: Both modified or added as Files
            (
                b_opt,
                Some(ManifestEntry::File { mode: m_o, digest: d_o, .. }),
                Some(ManifestEntry::File { mode: m_t, digest: d_t, .. }),
            ) => {
                if d_o == d_t {
                    merged_manifest.entries.insert(
                        name.clone(),
                        ManifestEntry::File {
                            mode: m_o.clone(),
                            name: name.clone(),
                            digest: *d_o,
                        },
                    );
                } else {
                    let base_bytes = match b_opt {
                        Some(ManifestEntry::File { digest, .. }) => {
                            blob_store.get(digest)?.unwrap_or_default()
                        }
                        _ => Vec::new(),
                    };
                    let ours_bytes = blob_store.get(d_o)?.unwrap_or_default();
                    let theirs_bytes = blob_store.get(d_t)?.unwrap_or_default();

                    let is_binary = base_bytes.contains(&0)
                        || ours_bytes.contains(&0)
                        || theirs_bytes.contains(&0);

                    if is_binary {
                        conflict_files.push(format!("{} (binary file)", path));
                        let (chosen_digest, chosen_mode) = match strategy {
                            ConflictStrategy::Theirs => (*d_t, m_t),
                            _ => (*d_o, m_o),
                        };
                        merged_manifest.entries.insert(
                            name.clone(),
                            ManifestEntry::File {
                                mode: chosen_mode.clone(),
                                name: name.clone(),
                                digest: chosen_digest,
                            },
                        );
                    } else {
                        let base_str = String::from_utf8_lossy(&base_bytes);
                        let ours_str = String::from_utf8_lossy(&ours_bytes);
                        let theirs_str = String::from_utf8_lossy(&theirs_bytes);

                        let (merged_text, conflict) =
                            merge3_text_with_strategy(&base_str, &ours_str, &theirs_str, strategy);

                        let merged_digest = blob_store.put(merged_text.as_bytes())?;
                        let chosen_mode = if m_o == "0755" || m_t == "0755" {
                            "0755"
                        } else {
                            "0644"
                        };

                        merged_manifest.entries.insert(
                            name.clone(),
                            ManifestEntry::File {
                                mode: chosen_mode.to_string(),
                                name: name.clone(),
                                digest: merged_digest,
                            },
                        );

                        if conflict {
                            conflict_files.push(path);
                        } else {
                            merged_files.push(path);
                        }
                    }
                }
            }
            // Case 10b: Both modified or added as ChunkedFiles
            (
                _b_opt,
                Some(ManifestEntry::ChunkedFile {
                    mode: m_o,
                    manifest_digest: d_o,
                    total_size: s_o,
                    ..
                }),
                Some(ManifestEntry::ChunkedFile {
                    mode: m_t,
                    manifest_digest: d_t,
                    total_size: _s_t,
                    ..
                }),
            ) => {
                if d_o == d_t {
                    merged_manifest.entries.insert(
                        name.clone(),
                        ManifestEntry::ChunkedFile {
                            mode: m_o.clone(),
                            name: name.clone(),
                            manifest_digest: *d_o,
                            total_size: *s_o,
                        },
                    );
                } else {
                    let (chosen_digest, chosen_size, chosen_mode) = match strategy {
                        ConflictStrategy::Theirs => (*d_t, *_s_t, m_t),
                        _ => (*d_o, *s_o, m_o),
                    };
                    merged_manifest.entries.insert(
                        name.clone(),
                        ManifestEntry::ChunkedFile {
                            mode: chosen_mode.clone(),
                            name: name.clone(),
                            manifest_digest: chosen_digest,
                            total_size: chosen_size,
                        },
                    );
                    conflict_files.push(path);
                }
            }
            // Default conflict: modify/delete or type conflict
            _ => {
                conflict_files.push(path);
                let chosen_entry = match strategy {
                    ConflictStrategy::Theirs => t.or(o),
                    _ => o.or(t),
                };
                if let Some(entry) = chosen_entry {
                    merged_manifest.entries.insert(name.clone(), entry.clone());
                }
            }
        }
    }

    let manifest_bytes = merged_manifest.serialize();
    blob_store.put(&manifest_bytes)
}

/// Result of a cherry-pick operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CherryPickResult {
    /// Root Merkle digest of the resulting tree after cherry-picking.
    pub root_tree_digest: Digest,
    /// Target tree digest before the cherry-pick was applied.
    pub target_tree_digest: Digest,
    /// Base commit tree digest used for diffing the cherry-picked commit.
    pub base_tree_digest: Digest,
    /// Incoming commit tree digest.
    pub incoming_tree_digest: Digest,
    /// Whether any merge conflicts occurred during cherry-picking.
    pub has_conflicts: bool,
    /// List of relative file paths with conflicts.
    pub conflict_files: Vec<String>,
    /// List of cleanly applied / merged files.
    pub merged_files: Vec<String>,
    /// Original commit message from the cherry-picked commit.
    pub original_message: String,
    /// Original author string.
    pub original_author: String,
    /// Source commit digest.
    pub source_commit: Digest,
}

/// Applies a single commit's changeset onto a target Merkle tree via 3-way merge.
///
/// # Arguments
/// * `source_commit_digest` - Digest of the commit to cherry-pick.
/// * `target_root_digest` - Root Merkle digest of the target branch or workspace.
/// * `mainline` - For merge commits, 1-based parent index (e.g. 1 for first parent).
/// * `strategy` - Conflict resolution strategy (Markers, Ours, Theirs, Fail).
/// * `blob_store` - CAS backing store.
pub fn cherry_pick_commit<B: BlobStore + ?Sized>(
    source_commit_digest: &Digest,
    target_root_digest: &Digest,
    mainline: Option<usize>,
    strategy: ConflictStrategy,
    blob_store: &B,
) -> io::Result<CherryPickResult> {
    let commit_bytes = blob_store
        .get(source_commit_digest)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Commit {} not found in CAS", source_commit_digest)))?;
    let commit = Commit::deserialize(&commit_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Failed to parse commit {}: {}", source_commit_digest, e)))?;

    // Determine base tree digest (parent commit of source commit)
    let base_tree_digest = if commit.parent_commits.is_empty() {
        // Root commit with no parents: base is empty directory
        Digest::from_bytes(b"empty")
    } else if commit.parent_commits.len() == 1 {
        let parent_digest = commit.parent_commits[0];
        let parent_bytes = blob_store
            .get(&parent_digest)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Parent commit {} not found", parent_digest)))?;
        let parent_commit = Commit::deserialize(&parent_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        parent_commit.root_tree_digest
    } else {
        // Merge commit: require mainline
        let parent_idx = match mainline {
            Some(n) if n >= 1 && n <= commit.parent_commits.len() => n - 1,
            Some(n) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Invalid --mainline {}; commit {} has {} parents.", n, source_commit_digest, commit.parent_commits.len()),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Commit {} is a merge commit with {} parents. Specify --mainline 1 (or 2) to pick the parent.",
                        source_commit_digest,
                        commit.parent_commits.len()
                    ),
                ));
            }
        };
        let parent_digest = commit.parent_commits[parent_idx];
        let parent_bytes = blob_store
            .get(&parent_digest)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Parent commit {} not found", parent_digest)))?;
        let parent_commit = Commit::deserialize(&parent_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        parent_commit.root_tree_digest
    };

    // 3-way Merkle merge: Base = parent tree, Ours = target tree, Theirs = commit tree
    let merge_res = merge_trees_with_strategy(
        &base_tree_digest,
        target_root_digest,
        &commit.root_tree_digest,
        blob_store,
        strategy,
    )?;

    Ok(CherryPickResult {
        root_tree_digest: merge_res.root_tree_digest,
        target_tree_digest: *target_root_digest,
        base_tree_digest,
        incoming_tree_digest: commit.root_tree_digest,
        has_conflicts: merge_res.has_conflicts,
        conflict_files: merge_res.conflict_files,
        merged_files: merge_res.merged_files,
        original_message: commit.message,
        original_author: commit.author,
        source_commit: *source_commit_digest,
    })
}

/// An item in a Merkle tree delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeDelta {
    /// File added or modified.
    Upsert {
        path: PathBuf,
        digest: Digest,
        is_executable: bool,
    },
    /// File deleted.
    Delete {
        path: PathBuf,
    },
}

/// Recursively computes the file-level delta between old and new Merkle trees.
pub fn extract_tree_delta<B: BlobStore + ?Sized>(
    old_root: &Digest,
    new_root: &Digest,
    blob_store: &B,
) -> io::Result<Vec<TreeDelta>> {
    let mut deltas = Vec::new();
    extract_tree_delta_recursive(old_root, new_root, Path::new(""), blob_store, &mut deltas)?;
    Ok(deltas)
}

fn extract_tree_delta_recursive<B: BlobStore + ?Sized>(
    old_dir: &Digest,
    new_dir: &Digest,
    current_path: &Path,
    blob_store: &B,
    deltas: &mut Vec<TreeDelta>,
) -> io::Result<()> {
    if old_dir == new_dir {
        return Ok(());
    }

    let old_manifest = load_manifest(old_dir, blob_store)?;
    let new_manifest = load_manifest(new_dir, blob_store)?;

    let mut all_keys: BTreeMap<String, ()> = BTreeMap::new();
    for k in old_manifest.entries.keys() {
        all_keys.insert(k.clone(), ());
    }
    for k in new_manifest.entries.keys() {
        all_keys.insert(k.clone(), ());
    }

    let empty_dir = Digest::from_bytes(b"empty");

    for name in all_keys.keys() {
        let old_e = old_manifest.entries.get(name);
        let new_e = new_manifest.entries.get(name);
        let child_path = if current_path.as_os_str().is_empty() {
            PathBuf::from(name)
        } else {
            current_path.join(name)
        };

        match (old_e, new_e) {
            (Some(o), Some(n)) if o == n => {}
            (
                Some(ManifestEntry::Directory { digest: d_o, .. }),
                Some(ManifestEntry::Directory { digest: d_n, .. }),
            ) => {
                extract_tree_delta_recursive(d_o, d_n, &child_path, blob_store, deltas)?;
            }
            (None, Some(ManifestEntry::Directory { digest: d_n, .. })) => {
                extract_tree_delta_recursive(&empty_dir, d_n, &child_path, blob_store, deltas)?;
            }
            (Some(ManifestEntry::Directory { digest: d_o, .. }), None) => {
                extract_tree_delta_recursive(d_o, &empty_dir, &child_path, blob_store, deltas)?;
            }
            (_, Some(ManifestEntry::File { digest, mode, .. })) => {
                deltas.push(TreeDelta::Upsert {
                    path: child_path,
                    digest: *digest,
                    is_executable: mode == "0755",
                });
            }
            (_, Some(ManifestEntry::ChunkedFile { manifest_digest, mode, .. })) => {
                deltas.push(TreeDelta::Upsert {
                    path: child_path,
                    digest: *manifest_digest,
                    is_executable: mode == "0755",
                });
            }
            (Some(ManifestEntry::File { .. }), None)
            | (Some(ManifestEntry::ChunkedFile { .. }), None) => {
                deltas.push(TreeDelta::Delete { path: child_path });
            }
            _ => {}
        }
    }

    Ok(())
}

/// Applies a Merkle tree delta into an active workspace's `upper_dir`.
pub fn apply_tree_delta_to_workspace<B: BlobStore + ?Sized>(
    upper_dir: &Path,
    deltas: &[TreeDelta],
    blob_store: &B,
) -> io::Result<()> {
    for delta in deltas {
        match delta {
            TreeDelta::Upsert {
                path,
                digest,
                is_executable,
            } => {
                let target_file = upper_dir.join(path);
                if let Some(parent) = target_file.parent() {
                    fs::create_dir_all(parent)?;
                }
                // If a whiteout marker existed for this file, remove it
                if let Some(file_name) = path.file_name() {
                    let parent_rel = path.parent().unwrap_or(Path::new(""));
                    let wh = upper_dir.join(parent_rel).join(format!(".wh.{}", file_name.to_string_lossy()));
                    let _ = fs::remove_file(wh);
                }

                // Check if blob is a ChunkManifest
                if let Some(bytes) = blob_store.get(digest)? {
                    if let Ok(manifest) = crate::cas::chunker::ChunkManifest::from_bytes(&bytes) {
                        let mut outfile = fs::File::create(&target_file)?;
                        use std::io::Write;
                        for chunk in &manifest.chunks {
                            let chunk_bytes = blob_store.get(&chunk.digest)?.ok_or_else(|| {
                                io::Error::new(io::ErrorKind::NotFound, "Missing chunk blob")
                            })?;
                            outfile.write_all(&chunk_bytes)?;
                        }
                    } else {
                        fs::write(&target_file, &bytes)?;
                    }
                } else {
                    return Err(io::Error::new(io::ErrorKind::NotFound, format!("Blob {} missing from CAS", digest)));
                }

                use std::os::unix::fs::PermissionsExt;
                let mode = if *is_executable { 0o755 } else { 0o644 };
                let _ = fs::set_permissions(&target_file, fs::Permissions::from_mode(mode));
            }
            TreeDelta::Delete { path } => {
                let target_file = upper_dir.join(path);
                if target_file.exists() {
                    let _ = fs::remove_file(&target_file);
                }
                if let Some(file_name) = path.file_name() {
                    let parent_rel = path.parent().unwrap_or(Path::new(""));
                    let upper_parent = upper_dir.join(parent_rel);
                    fs::create_dir_all(&upper_parent)?;
                    let wh_path = upper_parent.join(format!(".wh.{}", file_name.to_string_lossy()));
                    let _ = fs::File::create(wh_path);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use tempfile::tempdir;

    #[test]
    fn test_merge3_text_clean_append() {
        let base = "line 1\n";
        let ours = "line 1\nours added\n";
        let theirs = "line 1\ntheirs added\n";

        let (merged, conflict) = merge3_text(base, ours, theirs);
        assert!(!conflict);
        assert!(merged.contains("ours added"));
        assert!(merged.contains("theirs added"));
    }

    #[test]
    fn test_merge3_text_non_overlapping_edits() {
        let base = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        let ours = "line 1 (ours)\nline 2\nline 3\nline 4\nline 5\n";
        let theirs = "line 1\nline 2\nline 3\nline 4 (theirs)\nline 5\n";

        let (merged, conflict) = merge3_text(base, ours, theirs);
        assert!(!conflict);
        assert!(merged.contains("line 1 (ours)"));
        assert!(merged.contains("line 2"));
        assert!(merged.contains("line 3"));
        assert!(merged.contains("line 4 (theirs)"));
        assert!(merged.contains("line 5"));
    }

    #[test]
    fn test_merge3_text_append_preserves_syntax_and_braces() {
        let base = "fn main() {}\n";
        let ours = "fn main() {}\nfn ours() {\n    return;\n}\n";
        let theirs = "fn main() {}\nfn theirs() {\n    return;\n}\n";

        let (merged, conflict) = merge3_text(base, ours, theirs);
        assert!(!conflict);
        // Verify no duplicate braces were dropped
        assert!(merged.contains("fn ours() {\n    return;\n}"));
        assert!(merged.contains("fn theirs() {\n    return;\n}"));
    }

    #[test]
    fn test_merge3_text_conflict() {
        let base = "original value\n";
        let ours = "ours modified\n";
        let theirs = "theirs modified\n";

        let (merged, conflict) = merge3_text(base, ours, theirs);
        assert!(conflict);
        assert!(merged.contains("<<<<<<< ours"));
        assert!(merged.contains("ours modified"));
        assert!(merged.contains("======="));
        assert!(merged.contains("theirs modified"));
        assert!(merged.contains(">>>>>>> theirs"));
    }

    #[test]
    fn test_merge_trees_non_overlapping_files() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        // Base has common.txt
        let common_digest = store.put(b"common content")?;
        let mut base_m = DirectoryManifest::default();
        base_m.entries.insert(
            "common.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "common.txt".to_string(),
                digest: common_digest,
            },
        );
        let base_root = store.put(&base_m.serialize())?;

        // Ours adds ours.txt
        let ours_file = store.put(b"ours content")?;
        let mut ours_m = base_m.clone();
        ours_m.entries.insert(
            "ours.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "ours.txt".to_string(),
                digest: ours_file,
            },
        );
        let ours_root = store.put(&ours_m.serialize())?;

        // Theirs adds theirs.txt
        let theirs_file = store.put(b"theirs content")?;
        let mut theirs_m = base_m.clone();
        theirs_m.entries.insert(
            "theirs.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "theirs.txt".to_string(),
                digest: theirs_file,
            },
        );
        let theirs_root = store.put(&theirs_m.serialize())?;

        // Execute 3-way tree merge
        let result = merge_trees(&base_root, &ours_root, &theirs_root, &store)?;
        assert!(!result.has_conflicts);

        // Verify merged tree has all 3 files
        let merged_bytes = store.get(&result.root_tree_digest)?.unwrap();
        let merged_m = DirectoryManifest::from_bytes(&merged_bytes).unwrap();
        assert!(merged_m.entries.contains_key("common.txt"));
        assert!(merged_m.entries.contains_key("ours.txt"));
        assert!(merged_m.entries.contains_key("theirs.txt"));

        Ok(())
    }

    #[test]
    fn test_cherry_pick_clean_and_conflict() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        // 1. Initial base commit
        let base_content = b"line 1\nline 2\nline 3\n";
        let base_file_digest = store.put(base_content)?;
        let mut base_m = DirectoryManifest::default();
        base_m.entries.insert(
            "file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "file.txt".to_string(),
                digest: base_file_digest,
            },
        );
        let base_tree = store.put(&base_m.serialize())?;
        let base_commit = Commit::new(
            vec![],
            base_tree,
            "Dev <dev@graft.dev>".to_string(),
            "Initial base commit".to_string(),
        );
        let base_commit_digest = store.put(&base_commit.serialize())?;

        // 2. Feature commit to cherry-pick: modifies line 3 and adds new_file.txt
        let feature_content = b"line 1\nline 2\nline 3 (feature)\n";
        let feature_file_digest = store.put(feature_content)?;
        let new_file_digest = store.put(b"new feature code")?;
        let mut feat_m = base_m.clone();
        feat_m.entries.insert(
            "file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "file.txt".to_string(),
                digest: feature_file_digest,
            },
        );
        feat_m.entries.insert(
            "new_file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "new_file.txt".to_string(),
                digest: new_file_digest,
            },
        );
        let feat_tree = store.put(&feat_m.serialize())?;
        let feat_commit = Commit::new(
            vec![base_commit_digest],
            feat_tree,
            "Feature Dev <feat@graft.dev>".to_string(),
            "Modify line 3 and add new_file.txt".to_string(),
        );
        let feat_commit_digest = store.put(&feat_commit.serialize())?;

        // 3. Target tree: non-conflicting change (modifies line 1)
        let target_clean_content = b"line 1 (target modified)\nline 2\nline 3\n";
        let target_clean_digest = store.put(target_clean_content)?;
        let mut target_clean_m = base_m.clone();
        target_clean_m.entries.insert(
            "file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "file.txt".to_string(),
                digest: target_clean_digest,
            },
        );
        let target_clean_tree = store.put(&target_clean_m.serialize())?;

        // Cherry-pick feat_commit onto target_clean_tree
        let res = cherry_pick_commit(
            &feat_commit_digest,
            &target_clean_tree,
            None,
            ConflictStrategy::Markers,
            &store,
        )?;
        assert!(!res.has_conflicts);
        assert_eq!(res.original_message, "Modify line 3 and add new_file.txt");

        // Verify merged tree has both line 1 modification AND line 3 modification
        let merged_bytes = store.get(&res.root_tree_digest)?.unwrap();
        let merged_m = DirectoryManifest::from_bytes(&merged_bytes).unwrap();
        assert!(merged_m.entries.contains_key("new_file.txt"));
        let merged_file_entry = merged_m.entries.get("file.txt").unwrap();
        if let ManifestEntry::File { digest, .. } = merged_file_entry {
            let content = store.get(digest)?.unwrap();
            let s = String::from_utf8(content).unwrap();
            assert!(s.contains("line 1 (target modified)"));
            assert!(s.contains("line 2"));
            assert!(s.contains("line 3 (feature)"));
        }

        // 4. Target with overlapping conflict on line 3
        let target_conflict_content = b"line 1\nline 2\nline 3 (conflicting target)\n";
        let target_conflict_digest = store.put(target_conflict_content)?;
        let mut target_conf_m = base_m.clone();
        target_conf_m.entries.insert(
            "file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "file.txt".to_string(),
                digest: target_conflict_digest,
            },
        );
        let target_conf_tree = store.put(&target_conf_m.serialize())?;

        // Cherry-pick with Markers strategy
        let conf_res = cherry_pick_commit(
            &feat_commit_digest,
            &target_conf_tree,
            None,
            ConflictStrategy::Markers,
            &store,
        )?;
        assert!(conf_res.has_conflicts);
        assert!(conf_res.conflict_files.contains(&"file.txt".to_string()));

        let conf_bytes = store.get(&conf_res.root_tree_digest)?.unwrap();
        let conf_m = DirectoryManifest::from_bytes(&conf_bytes).unwrap();
        if let ManifestEntry::File { digest, .. } = conf_m.entries.get("file.txt").unwrap() {
            let content = store.get(digest)?.unwrap();
            let s = String::from_utf8(content).unwrap();
            assert!(s.contains("<<<<<<< ours"));
            assert!(s.contains("line 3 (conflicting target)"));
            assert!(s.contains("======="));
            assert!(s.contains("line 3 (feature)"));
            assert!(s.contains(">>>>>>> theirs"));
        }

        // 5. Test apply_tree_delta_to_workspace
        let deltas = extract_tree_delta(&target_clean_tree, &res.root_tree_digest, &store)?;
        let upper_dir = tmp.path().join("upper");
        fs::create_dir_all(&upper_dir)?;
        apply_tree_delta_to_workspace(&upper_dir, &deltas, &store)?;

        assert!(upper_dir.join("new_file.txt").exists());
        assert!(upper_dir.join("file.txt").exists());

        Ok(())
    }
}
