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

//! # Pull Request & Code Review Subsystem
//!
//! Provides GitHub/Critique-style Pull Requests, inline code review comments,
//! and server-side 3-way Merkle merge execution.

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use crate::cas::merge::merge_trees;
use crate::metadata::MetadataStore;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lifecycle state of a Pull Request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestStatus {
    Open,
    Merged,
    Closed,
}

impl std::fmt::Display for PullRequestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PullRequestStatus::Open => write!(f, "Open"),
            PullRequestStatus::Merged => write!(f, "Merged"),
            PullRequestStatus::Closed => write!(f, "Closed"),
        }
    }
}

/// A review or general comment on a Pull Request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// 1-based comment identifier.
    pub id: usize,
    /// Pull request identifier.
    pub pr_id: usize,
    /// Author identity.
    pub author: String,
    /// Comment markdown content.
    pub body: String,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    /// Whether inline review thread is marked resolved.
    pub resolved: bool,
    /// Optional file path for inline line comments.
    pub file_path: Option<String>,
    /// Optional 1-based line number for inline line comments.
    pub line_no: Option<usize>,
}

/// A structured Pull Request record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// 1-based numerical ID (e.g. 1, 2).
    pub id: usize,
    /// Title summary.
    pub title: String,
    /// Description markdown body.
    pub description: String,
    /// Author of the pull request.
    pub author: String,
    /// Source feature branch or workspace being proposed.
    pub source_branch: String,
    /// Target base branch (e.g. "main").
    pub target_branch: String,
    /// Current status.
    pub status: PullRequestStatus,
    /// Commit digest of the source branch head when submitted/updated.
    pub head_commit: Digest,
    /// Base commit digest (LCA) when created.
    pub base_commit: Digest,
    /// Resulting merge commit digest if merged.
    pub merge_commit: Option<Digest>,
    /// Creation timestamp (Unix seconds).
    pub created_at: i64,
    /// Last update timestamp (Unix seconds).
    pub updated_at: i64,
    /// Review discussion thread and inline comments.
    pub comments: Vec<ReviewComment>,
}

/// On-disk database of Pull Requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PrDatabase {
    pub next_pr_id: usize,
    pub next_comment_id: usize,
    pub prs: Vec<PullRequest>,
}

/// Manager responsible for persisting and querying Pull Requests and review comments.
pub struct ReviewManager {
    db_path: PathBuf,
}

impl ReviewManager {
    /// Creates a new `ReviewManager` storing data under `storage_dir/reviews/prs.json`.
    pub fn new<P: AsRef<Path>>(storage_dir: P) -> Self {
        let db_path = storage_dir.as_ref().join("reviews").join("prs.json");
        Self { db_path }
    }

    fn read_db(&self) -> io::Result<PrDatabase> {
        if !self.db_path.exists() {
            return Ok(PrDatabase {
                next_pr_id: 1,
                next_comment_id: 1,
                prs: Vec::new(),
            });
        }
        let data = fs::read(&self.db_path)?;
        serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn write_db(&self, db: &PrDatabase) -> io::Result<()> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(db)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&self.db_path, json)
    }

    /// Lists all Pull Requests.
    pub fn list_prs(&self) -> io::Result<Vec<PullRequest>> {
        let db = self.read_db()?;
        Ok(db.prs)
    }

    /// Gets a specific Pull Request by ID.
    pub fn get_pr(&self, id: usize) -> io::Result<Option<PullRequest>> {
        let db = self.read_db()?;
        Ok(db.prs.into_iter().find(|p| p.id == id))
    }

    /// Creates a new Pull Request.
    pub fn create_pr(
        &self,
        title: &str,
        description: &str,
        author: &str,
        source_branch: &str,
        target_branch: &str,
        head_commit: Digest,
        base_commit: Digest,
    ) -> io::Result<PullRequest> {
        let mut db = self.read_db()?;
        let id = db.next_pr_id;
        db.next_pr_id += 1;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let pr = PullRequest {
            id,
            title: title.to_string(),
            description: description.to_string(),
            author: author.to_string(),
            source_branch: source_branch.to_string(),
            target_branch: target_branch.to_string(),
            status: PullRequestStatus::Open,
            head_commit,
            base_commit,
            merge_commit: None,
            created_at: now,
            updated_at: now,
            comments: Vec::new(),
        };

        db.prs.push(pr.clone());
        self.write_db(&db)?;
        Ok(pr)
    }

    /// Adds a review comment to a Pull Request.
    pub fn add_comment(
        &self,
        pr_id: usize,
        author: &str,
        body: &str,
        file_path: Option<&str>,
        line_no: Option<usize>,
    ) -> io::Result<ReviewComment> {
        let mut db = self.read_db()?;
        let pr = db
            .prs
            .iter_mut()
            .find(|p| p.id == pr_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("PR #{} not found", pr_id)))?;

        let comment_id = db.next_comment_id;
        db.next_comment_id += 1;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let comment = ReviewComment {
            id: comment_id,
            pr_id,
            author: author.to_string(),
            body: body.to_string(),
            created_at: now,
            resolved: false,
            file_path: file_path.map(|s| s.to_string()),
            line_no,
        };

        pr.comments.push(comment.clone());
        pr.updated_at = now;

        self.write_db(&db)?;
        Ok(comment)
    }

    /// Closes a Pull Request without merging.
    pub fn close_pr(&self, pr_id: usize) -> io::Result<()> {
        let mut db = self.read_db()?;
        let pr = db
            .prs
            .iter_mut()
            .find(|p| p.id == pr_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("PR #{} not found", pr_id)))?;

        pr.status = PullRequestStatus::Closed;
        pr.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.write_db(&db)
    }

    /// Merges an open Pull Request into its target branch using 3-way Merkle merge.
    pub fn merge_pr<B: BlobStore + ?Sized, M: MetadataStore + ?Sized>(
        &self,
        pr_id: usize,
        store: &B,
        meta: &M,
    ) -> io::Result<Digest> {
        let mut db = self.read_db()?;
        let pr = db
            .prs
            .iter_mut()
            .find(|p| p.id == pr_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("PR #{} not found", pr_id)))?;

        if pr.status != PullRequestStatus::Open {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("PR #{} is already {:?}", pr_id, pr.status),
            ));
        }

        // 1. Resolve current target branch commit
        let target_ref = crate::metadata::normalize_ref_name(&pr.target_branch);
        let target_commit_digest = meta.get_ref(&target_ref)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Target branch '{}' not found", pr.target_branch),
            )
        })?;

        // 2. Load commits
        let base_commit_bytes = store.get(&pr.base_commit)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Base commit not found in CAS")
        })?;
        let base_commit = Commit::deserialize(&base_commit_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let head_commit_bytes = store.get(&pr.head_commit)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Head commit not found in CAS")
        })?;
        let head_commit = Commit::deserialize(&head_commit_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let target_commit_bytes = store.get(&target_commit_digest)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Target commit not found in CAS")
        })?;
        let target_commit = Commit::deserialize(&target_commit_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // 3. Execute 3-way Merkle merge: base -> target, base -> head
        let merge_result = merge_trees(
            &base_commit.root_tree_digest,
            &target_commit.root_tree_digest,
            &head_commit.root_tree_digest,
            store,
        )?;

        if merge_result.has_conflicts {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Merge conflicts detected in {} files. Manual resolution required.",
                    merge_result.conflict_files.len()
                ),
            ));
        }

        // 4. Create merge commit
        let merge_msg = format!(
            "Merge pull request #{} from {}\n\n{}\n",
            pr.id, pr.source_branch, pr.title
        );
        let merge_commit = Commit::new(
            vec![target_commit_digest, pr.head_commit],
            merge_result.root_tree_digest,
            format!("Graft Merge <merge@graft.dev>"),
            merge_msg,
        );

        let merge_bytes = merge_commit.serialize();
        let merge_digest = store.put(&merge_bytes)?;

        // 5. Atomic CAS branch advance
        meta.compare_and_swap_ref(
            &target_ref,
            Some(&target_commit_digest),
            &merge_digest,
            "Graft Merge",
        )?;

        // 6. Update PR state
        pr.status = PullRequestStatus::Merged;
        pr.merge_commit = Some(merge_digest);
        pr.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.write_db(&db)?;
        Ok(merge_digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_pr_lifecycle_and_comments() -> io::Result<()> {
        let tmp = tempdir()?;
        let mgr = ReviewManager::new(tmp.path());

        let pr = mgr.create_pr(
            "Add high performance VFS cache",
            "This implements L1 NVMe chunk cache.",
            "Agent-007",
            "feature-vfs",
            "main",
            Digest::from_bytes(b"head"),
            Digest::from_bytes(b"base"),
        )?;

        assert_eq!(pr.id, 1);
        assert_eq!(pr.status, PullRequestStatus::Open);
        assert_eq!(pr.title, "Add high performance VFS cache");

        let c1 = mgr.add_comment(1, "Reviewer-1", "Looks great! LGTM.", None, None)?;
        assert_eq!(c1.id, 1);
        assert_eq!(c1.author, "Reviewer-1");

        let c2 = mgr.add_comment(
            1,
            "Reviewer-2",
            "Please check memory bounds here.",
            Some("src/vfs.rs"),
            Some(42),
        )?;
        assert_eq!(c2.line_no, Some(42));
        assert_eq!(c2.file_path, Some("src/vfs.rs".to_string()));

        let pr_loaded = mgr.get_pr(1)?.unwrap();
        assert_eq!(pr_loaded.comments.len(), 2);

        mgr.close_pr(1)?;
        let pr_closed = mgr.get_pr(1)?.unwrap();
        assert_eq!(pr_closed.status, PullRequestStatus::Closed);

        Ok(())
    }
}
