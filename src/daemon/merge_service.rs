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

//! # Server-Side 3-Way AST Semantic Merge Service
//!
//! Reconciles concurrent agent branches entirely in-memory on `graftd`.
//! Clients send a lightweight JSON RPC containing branch names; the server
//! executes the Merkle DAG walk, runs the Concrete Syntax Tree (CST) semantic merger,
//! stores the resulting manifests, and advances trunk atomically via Compare-And-Swap.
//!
//! **Network Transfer**: 0 bytes of source code downloaded by the client.

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use crate::cas::merge::{merge_trees_with_strategy, ConflictStrategy};
use crate::metadata::{resolve_ref, MetadataStore};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSideMergeRequest {
    /// Common ancestor base branch or commit digest
    pub base: String,
    /// Destination/target branch or commit digest (e.g. "main")
    pub ours: String,
    /// Feature branch or commit digest to merge in (e.g. "feature-agent-42")
    pub theirs: String,
    /// Conflict resolution strategy: "error" (default), "ours", "theirs", "union"
    pub strategy: Option<String>,
    /// Author identity for the merge commit
    pub author: Option<String>,
    /// Commit message
    pub message: Option<String>,
    /// Optional target branch to advance via atomic Compare-And-Swap (e.g. "refs/heads/main")
    pub advance_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSideMergeResponse {
    pub status: String,
    pub clean: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_tree: Option<String>,
    pub conflict_files: Vec<String>,
}

fn resolve_commit(
    meta_store: &Arc<dyn MetadataStore>,
    blob_store: &Arc<dyn BlobStore>,
    target: &str,
) -> Result<(Digest, Commit), String> {
    // 1. Try parsing directly as commit digest
    if let Ok(d) = Digest::parse(target) {
        if let Ok(Some(bytes)) = blob_store.get(&d) {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((d, commit));
            }
        }
    }

    // 2. Resolve via branch/ref lookup
    if let Ok(Some((_, d))) = resolve_ref(meta_store.as_ref(), target) {
        if let Ok(Some(bytes)) = blob_store.get(&d) {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((d, commit));
            }
        }
        return Err(format!("Found ref '{}' pointing to {}, but commit blob is missing", target, d));
    }

    Err(format!("Could not resolve '{}' to a valid commit", target))
}

/// Executes a 3-way AST semantic merge on the server.
pub fn handle_server_side_merge(
    blob_store: &Arc<dyn BlobStore>,
    meta_store: &Arc<dyn MetadataStore>,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let req: ServerSideMergeRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid JSON request: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let (_base_digest, base_commit) = match resolve_commit(meta_store, blob_store, &req.base) {
        Ok(res) => res,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to resolve base: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let (ours_digest, ours_commit) = match resolve_commit(meta_store, blob_store, &req.ours) {
        Ok(res) => res,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to resolve ours: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let (theirs_digest, theirs_commit) = match resolve_commit(meta_store, blob_store, &req.theirs) {
        Ok(res) => res,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to resolve theirs: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let strategy = match req.strategy.as_deref().unwrap_or("markers").to_lowercase().as_str() {
        "ours" => ConflictStrategy::Ours,
        "theirs" => ConflictStrategy::Theirs,
        "fail" | "error" => ConflictStrategy::Fail,
        _ => ConflictStrategy::Markers,
    };

    // Execute server-side 3-way Merkle + AST semantic merge
    let merge_result = match merge_trees_with_strategy(
        &base_commit.root_tree_digest,
        &ours_commit.root_tree_digest,
        &theirs_commit.root_tree_digest,
        blob_store.as_ref(),
        strategy,
    ) {
        Ok(res) => res,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Merge algorithm failed: {}", e) });
            return (500, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    if merge_result.has_conflicts {
        let resp = ServerSideMergeResponse {
            status: "conflict".to_string(),
            clean: false,
            merged_commit: None,
            merged_tree: None,
            conflict_files: merge_result.conflict_files,
        };
        return (409, "application/json", serde_json::to_vec(&resp).unwrap());
    }

    // Synthesize new merge commit
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let author = req.author.unwrap_or_else(|| "Graft Daemon <graftd@graft.dev>".to_string());
    let message = req.message.unwrap_or_else(|| format!("Merge branch '{}' into '{}'", req.theirs, req.ours));

    let merge_commit = Commit {
        parent_commits: vec![ours_digest, theirs_digest],
        root_tree_digest: merge_result.root_tree_digest,
        author: author.clone(),
        timestamp_seconds: now_epoch,
        message,
    };

    let serialized = merge_commit.serialize();
    let merge_commit_digest = match blob_store.put(&serialized) {
        Ok(d) => d,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to store merge commit blob: {}", e) });
            return (500, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    if let Err(e) = meta_store.put_commit_metadata(&merge_commit_digest, &merge_commit) {
        let err = serde_json::json!({ "error": format!("Failed to index merge commit: {}", e) });
        return (500, "application/json", serde_json::to_vec(&err).unwrap());
    }

    // Advance branch atomically if requested
    if let Some(ref branch) = req.advance_branch {
        if let Err(e) = meta_store.compare_and_swap_ref(
            branch,
            Some(&ours_digest),
            &merge_commit_digest,
            &author,
        ) {
            let status = if e.kind() == std::io::ErrorKind::AlreadyExists {
                409 // Optimistic CAS conflict
            } else {
                500
            };
            let err = serde_json::json!({
                "error": format!("Merge succeeded but advancing branch '{}' failed: {}", branch, e),
                "merged_commit": merge_commit_digest.to_string(),
                "merged_tree": merge_result.root_tree_digest.to_string(),
            });
            return (status, "application/json", serde_json::to_vec(&err).unwrap());
        }
    }

    let resp = ServerSideMergeResponse {
        status: "success".to_string(),
        clean: true,
        merged_commit: Some(merge_commit_digest.to_string()),
        merged_tree: Some(merge_result.root_tree_digest.to_string()),
        conflict_files: vec![],
    };

    (200, "application/json", serde_json::to_vec(&resp).unwrap())
}
