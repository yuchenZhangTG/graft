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

//! # Git Blame Engine with Merkle-Pruned Line Tracking
//!
//! Traces the origin of each line of a file back through the commit DAG.
//! Leverages Graft's Content-Addressed Merkle tree to skip untouched commits
//! in $O(1)$ digest comparisons with zero blob hydration.

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::{get_tree_entry, Commit, ManifestEntry};
use crate::cas::digest::Digest;
use std::collections::HashSet;
use std::io;
use std::path::Path;

/// Information about the commit and author responsible for a specific line of code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlameEntry {
    /// 1-based line number in the current version of the file.
    pub line_no: usize,
    /// Full commit digest where this line was introduced or last modified.
    pub commit_digest: Digest,
    /// 7-character short commit hash for display.
    pub short_hash: String,
    /// Author who committed the line.
    pub author: String,
    /// Unix timestamp of the commit.
    pub timestamp: i64,
    /// First line of the commit message.
    pub summary: String,
    /// Content of the line.
    pub content: String,
}

/// The complete blame result for an entire file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlameResult {
    /// Repository-relative path to the file.
    pub file_path: String,
    /// Commit digest at which the blame was evaluated.
    pub commit: Digest,
    /// Line-by-line blame records.
    pub lines: Vec<BlameEntry>,
}

/// Computes the Longest Common Subsequence line mapping between parent lines and child lines.
///
/// Returns a vector where index `j` corresponds to child line `j`.
/// If matched, `Some(i)` contains the 0-based index of the matching parent line.
fn match_lines_lcs(parent_lines: &[&str], child_lines: &[&str]) -> Vec<Option<usize>> {
    let m = parent_lines.len();
    let n = child_lines.len();
    if m == 0 || n == 0 {
        return vec![None; n];
    }

    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..m {
        for j in 0..n {
            if parent_lines[i] == child_lines[j] {
                dp[i + 1][j + 1] = dp[i][j] + 1;
            } else {
                dp[i + 1][j + 1] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    let mut matches = vec![None; n];
    let mut i = m;
    let mut j = n;

    while i > 0 && j > 0 {
        if parent_lines[i - 1] == child_lines[j - 1] {
            matches[j - 1] = Some(i - 1);
            i -= 1;
            j -= 1;
        } else if dp[i][j - 1] >= dp[i - 1][j] {
            j -= 1;
        } else {
            i -= 1;
        }
    }

    matches
}

/// Computes line-by-line blame for a file at a specific commit.
///
/// Traces lines backwards through the commit DAG using Merkle digests to skip unchanged
/// revisions in $O(1)$ time.
pub fn compute_blame<B: BlobStore + ?Sized>(
    file_path: &str,
    head_commit_digest: &Digest,
    store: &B,
) -> io::Result<BlameResult> {
    let path = Path::new(file_path);

    // 1. Load starting commit
    let head_commit_bytes = store.get(head_commit_digest)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Commit {} not found", head_commit_digest),
        )
    })?;
    let head_commit = Commit::deserialize(&head_commit_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // 2. Load head file entry
    let head_entry = get_tree_entry(&head_commit.root_tree_digest, path, store)?.ok_or_else(
        || {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("File '{}' not found at commit {}", file_path, head_commit_digest),
            )
        },
    )?;

    let head_bytes = match head_entry {
        ManifestEntry::File { digest, .. } => store.get(&digest)?.unwrap_or_default(),
        ManifestEntry::ChunkedFile {
            manifest_digest, ..
        } => {
            let m_bytes = store.get(&manifest_digest)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Chunk manifest {} not found", manifest_digest),
                )
            })?;
            let manifest = crate::cas::chunker::ChunkManifest::from_bytes(&m_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut out = Vec::new();
            crate::cas::chunker::stream_chunked_file(&manifest, store, &mut out)?;
            out
        }
        ManifestEntry::Directory { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{}' is a directory, not a file", file_path),
            ));
        }
    };

    let head_text = String::from_utf8_lossy(&head_bytes);
    let head_lines: Vec<String> = head_text.lines().map(|s| s.to_string()).collect();

    if head_lines.is_empty() {
        return Ok(BlameResult {
            file_path: file_path.to_string(),
            commit: *head_commit_digest,
            lines: Vec::new(),
        });
    }

    let mut blamed_entries: Vec<Option<BlameEntry>> = vec![None; head_lines.len()];

    // Pending mappings: list of (head_line_idx, current_commit_line_idx)
    let initial_pending: Vec<(usize, usize)> = (0..head_lines.len()).map(|i| (i, i)).collect();

    // Stack item: (commit_digest, current_file_bytes, pending)
    let mut stack = vec![(*head_commit_digest, head_bytes, initial_pending)];
    let mut visited_commits = HashSet::new();

    while let Some((commit_digest, cur_bytes, pending)) = stack.pop() {
        if pending.is_empty() {
            continue;
        }

        if !visited_commits.insert(commit_digest) {
            continue;
        }

        let commit_bytes = match store.get(&commit_digest)? {
            Some(b) => b,
            None => continue,
        };
        let commit = match Commit::deserialize(&commit_bytes) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let summary = commit
            .message
            .lines()
            .next()
            .unwrap_or("No commit message")
            .to_string();
        let short_hash = commit_digest.short_hex();

        // If commit has no parents, blame all remaining pending lines on this commit
        if commit.parent_commits.is_empty() {
            let cur_text = String::from_utf8_lossy(&cur_bytes);
            let cur_lines: Vec<&str> = cur_text.lines().collect();

            for (head_idx, cur_idx) in pending {
                if blamed_entries[head_idx].is_none() {
                    let content = cur_lines.get(cur_idx).copied().unwrap_or("").to_string();
                    blamed_entries[head_idx] = Some(BlameEntry {
                        line_no: head_idx + 1,
                        commit_digest,
                        short_hash: short_hash.clone(),
                        author: commit.author.clone(),
                        timestamp: commit.timestamp_seconds,
                        summary: summary.clone(),
                        content,
                    });
                }
            }
            continue;
        }

        // Check parents
        let cur_text = String::from_utf8_lossy(&cur_bytes);
        let cur_lines: Vec<&str> = cur_text.lines().collect();
        let mut unblamed_pending = pending;

        for parent_digest in &commit.parent_commits {
            if unblamed_pending.is_empty() {
                break;
            }

            let parent_commit_bytes = match store.get(parent_digest)? {
                Some(b) => b,
                None => continue,
            };
            let parent_commit = match Commit::deserialize(&parent_commit_bytes) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let parent_entry = match get_tree_entry(&parent_commit.root_tree_digest, path, store)? {
                Some(e) => e,
                None => continue, // File did not exist in parent
            };

            // $O(1)$ Merkle fast-skip: If file digest is identical to current file digest,
            // lines did not change between parent and current!
            let cur_entry = get_tree_entry(&commit.root_tree_digest, path, store)?;
            if let (Some(ref c_entry), ref p_entry) = (&cur_entry, &parent_entry) {
                if c_entry == *p_entry {
                    // Pass unblamed directly to parent with zero LCS diffing!
                    stack.push((*parent_digest, cur_bytes.clone(), unblamed_pending.clone()));
                    unblamed_pending.clear();
                    break;
                }
            }

            // File exists in parent but differs: read parent bytes and run LCS
            let parent_bytes = match parent_entry {
                ManifestEntry::File { digest, .. } => store.get(&digest)?.unwrap_or_default(),
                ManifestEntry::ChunkedFile {
                    manifest_digest, ..
                } => {
                    let m_bytes = store.get(&manifest_digest)?.unwrap_or_default();
                    if let Ok(manifest) = crate::cas::chunker::ChunkManifest::from_bytes(&m_bytes) {
                        let mut out = Vec::new();
                        let _ = crate::cas::chunker::stream_chunked_file(&manifest, store, &mut out);
                        out
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            };

            let parent_text = String::from_utf8_lossy(&parent_bytes);
            let parent_lines: Vec<&str> = parent_text.lines().collect();

            let matches = match_lines_lcs(&parent_lines, &cur_lines);

            let mut next_parent_pending = Vec::new();
            let mut remaining_pending = Vec::new();

            for (head_idx, cur_idx) in unblamed_pending {
                if let Some(parent_idx) = matches.get(cur_idx).copied().flatten() {
                    next_parent_pending.push((head_idx, parent_idx));
                } else {
                    remaining_pending.push((head_idx, cur_idx));
                }
            }

            if !next_parent_pending.is_empty() {
                stack.push((*parent_digest, parent_bytes, next_parent_pending));
            }

            unblamed_pending = remaining_pending;
        }

        // Any remaining lines in unblamed_pending were not matched to any parent.
        // Therefore, they were introduced by THIS commit!
        for (head_idx, cur_idx) in unblamed_pending {
            if blamed_entries[head_idx].is_none() {
                let content = cur_lines.get(cur_idx).copied().unwrap_or("").to_string();
                blamed_entries[head_idx] = Some(BlameEntry {
                    line_no: head_idx + 1,
                    commit_digest,
                    short_hash: short_hash.clone(),
                    author: commit.author.clone(),
                    timestamp: commit.timestamp_seconds,
                    summary: summary.clone(),
                    content,
                });
            }
        }
    }

    // Fallback: any lines still unblamed get attributed to head commit
    let head_short_hash = head_commit_digest.short_hex();
    let head_summary = head_commit
        .message
        .lines()
        .next()
        .unwrap_or("No commit message")
        .to_string();

    let mut final_lines = Vec::new();
    for (i, entry_opt) in blamed_entries.into_iter().enumerate() {
        let entry = entry_opt.unwrap_or_else(|| BlameEntry {
            line_no: i + 1,
            commit_digest: *head_commit_digest,
            short_hash: head_short_hash.clone(),
            author: head_commit.author.clone(),
            timestamp: head_commit.timestamp_seconds,
            summary: head_summary.clone(),
            content: head_lines.get(i).cloned().unwrap_or_default(),
        });
        final_lines.push(entry);
    }

    Ok(BlameResult {
        file_path: file_path.to_string(),
        commit: *head_commit_digest,
        lines: final_lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use crate::cas::commit::DirectoryManifest;
    use tempfile::tempdir;

    #[test]
    fn test_blame_single_commit() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        let f_digest = store.put(b"line 1\nline 2\nline 3\n")?;
        let mut manifest = DirectoryManifest::default();
        manifest.entries.insert(
            "file.txt".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "file.txt".to_string(),
                digest: f_digest,
            },
        );
        let m_digest = store.put(&manifest.serialize())?;

        let commit = Commit {
            root_tree_digest: m_digest,
            parent_commits: Vec::new(),
            author: "Alice <alice@graft.dev>".to_string(),
            timestamp_seconds: 1700000000,
            message: "Initial commit".to_string(),
        };
        let c_digest = store.put(&commit.serialize())?;

        let blame = compute_blame("file.txt", &c_digest, &store)?;
        assert_eq!(blame.lines.len(), 3);
        assert_eq!(blame.lines[0].author, "Alice <alice@graft.dev>");
        assert_eq!(blame.lines[0].content, "line 1");
        assert_eq!(blame.lines[1].content, "line 2");
        assert_eq!(blame.lines[2].content, "line 3");

        Ok(())
    }

    #[test]
    fn test_blame_multi_commit_tracking() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        // Commit 1: Alice creates file with line 1 and line 2
        let f1 = store.put(b"line 1\nline 2\n")?;
        let mut m1 = DirectoryManifest::default();
        m1.entries.insert(
            "code.rs".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "code.rs".to_string(),
                digest: f1,
            },
        );
        let m1_digest = store.put(&m1.serialize())?;
        let c1 = Commit {
            root_tree_digest: m1_digest,
            parent_commits: Vec::new(),
            author: "Alice".to_string(),
            timestamp_seconds: 1700000001,
            message: "Commit 1".to_string(),
        };
        let c1_digest = store.put(&c1.serialize())?;

        // Commit 2: Bob modifies line 2 and adds line 3
        let f2 = store.put(b"line 1\nline 2 edited\nline 3\n")?;
        let mut m2 = DirectoryManifest::default();
        m2.entries.insert(
            "code.rs".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "code.rs".to_string(),
                digest: f2,
            },
        );
        let m2_digest = store.put(&m2.serialize())?;
        let c2 = Commit {
            root_tree_digest: m2_digest,
            parent_commits: vec![c1_digest],
            author: "Bob".to_string(),
            timestamp_seconds: 1700000002,
            message: "Commit 2".to_string(),
        };
        let c2_digest = store.put(&c2.serialize())?;

        let blame = compute_blame("code.rs", &c2_digest, &store)?;
        assert_eq!(blame.lines.len(), 3);

        // Line 1 should be blamed on Alice (Commit 1)
        assert_eq!(blame.lines[0].author, "Alice");
        assert_eq!(blame.lines[0].commit_digest, c1_digest);
        assert_eq!(blame.lines[0].short_hash.len(), 7);
        assert_eq!(blame.lines[0].short_hash, c1_digest.short_hex());
        assert_eq!(blame.lines[0].content, "line 1");

        // Line 2 and 3 should be blamed on Bob (Commit 2)
        assert_eq!(blame.lines[1].author, "Bob");
        assert_eq!(blame.lines[1].commit_digest, c2_digest);
        assert_eq!(blame.lines[1].short_hash.len(), 7);
        assert_eq!(blame.lines[1].content, "line 2 edited");

        assert_eq!(blame.lines[2].author, "Bob");
        assert_eq!(blame.lines[2].commit_digest, c2_digest);
        assert_eq!(blame.lines[2].short_hash.len(), 7);
        assert_eq!(blame.lines[2].content, "line 3");

        Ok(())
    }
}
