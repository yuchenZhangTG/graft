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

//! # Server-Side Reference & Commit Metadata Service
//!
//! Provides HTTP handlers for atomic Compare-And-Swap (CAS) branch pointer updates,
//! branch reference lookups, commit DAG queries, and ref deletion.

use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use crate::metadata::MetadataStore;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct CasRefRequest {
    pub ref_name: String,
    pub expected: Option<String>,
    pub target: String,
    pub author: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteRefRequest {
    pub expected: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RefEntry {
    pub name: String,
    pub digest: String,
}

#[derive(Debug, Serialize)]
pub struct CommitMetadataResponse {
    pub author: String,
    pub timestamp_seconds: i64,
    pub message: String,
    pub root_tree_digest: String,
    pub parent_commits: Vec<String>,
}

/// Retrieves the target commit digest for a reference name.
pub fn handle_get_ref(meta_store: &Arc<dyn MetadataStore>, ref_name: &str) -> (u16, &'static str, Vec<u8>) {
    match meta_store.get_ref(ref_name) {
        Ok(Some(digest)) => {
            let resp = serde_json::json!({
                "ref_name": ref_name,
                "digest": digest.to_string(),
            });
            (200, "application/json", serde_json::to_vec(&resp).unwrap())
        }
        Ok(None) => (404, "application/json", br#"{"error":"Ref not found"}"#.to_vec()),
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Metadata error: {}", e) });
            (500, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Atomically advances a reference pointer using Compare-And-Swap (CAS).
pub fn handle_cas_ref(meta_store: &Arc<dyn MetadataStore>, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let req: CasRefRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid JSON request: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let target_digest = match Digest::parse(&req.target) {
        Ok(d) => d,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid target digest '{}': {}", req.target, e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let expected_digest = match req.expected.as_deref() {
        Some(exp_str) => match Digest::parse(exp_str) {
            Ok(d) => Some(d),
            Err(e) => {
                let err = serde_json::json!({ "error": format!("Invalid expected digest '{}': {}", exp_str, e) });
                return (400, "application/json", serde_json::to_vec(&err).unwrap());
            }
        },
        None => None,
    };

    match meta_store.compare_and_swap_ref(
        &req.ref_name,
        expected_digest.as_ref(),
        &target_digest,
        &req.author,
    ) {
        Ok(()) => (200, "application/json", br#"{"status":"ok"}"#.to_vec()),
        Err(e) => {
            let status = match e.kind() {
                std::io::ErrorKind::AlreadyExists => 409, // CAS conflict
                std::io::ErrorKind::NotFound => 404,
                _ => 500,
            };
            let err = serde_json::json!({ "error": e.to_string() });
            (status, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Lists all references matching a prefix.
pub fn handle_list_refs(meta_store: &Arc<dyn MetadataStore>, prefix: &str) -> (u16, &'static str, Vec<u8>) {
    match meta_store.list_refs(prefix) {
        Ok(refs) => {
            let entries: Vec<RefEntry> = refs
                .into_iter()
                .map(|(name, digest)| RefEntry {
                    name,
                    digest: digest.to_string(),
                })
                .collect();
            let resp = serde_json::json!({ "refs": entries });
            (200, "application/json", serde_json::to_vec(&resp).unwrap())
        }
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to list refs: {}", e) });
            (500, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Deletes a reference if expected digest matches.
pub fn handle_delete_ref(
    meta_store: &Arc<dyn MetadataStore>,
    ref_name: &str,
    expected_param: Option<&str>,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let expected = if let Some(param) = expected_param {
        Digest::parse(param).ok()
    } else if !body.is_empty() {
        if let Ok(req) = serde_json::from_slice::<DeleteRefRequest>(body) {
            req.expected.and_then(|s| Digest::parse(&s).ok())
        } else {
            None
        }
    } else {
        None
    };

    match meta_store.delete_ref(ref_name, expected.as_ref()) {
        Ok(()) => (200, "application/json", br#"{"status":"deleted"}"#.to_vec()),
        Err(e) => {
            let status = if e.kind() == std::io::ErrorKind::AlreadyExists {
                409
            } else {
                500
            };
            let err = serde_json::json!({ "error": e.to_string() });
            (status, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Retrieves commit metadata by digest.
pub fn handle_get_commit(meta_store: &Arc<dyn MetadataStore>, digest_str: &str) -> (u16, &'static str, Vec<u8>) {
    let digest = match Digest::parse(digest_str) {
        Ok(d) => d,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid digest '{}': {}", digest_str, e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    match meta_store.get_commit_metadata(&digest) {
        Ok(Some(c)) => {
            let resp = CommitMetadataResponse {
                author: c.author,
                timestamp_seconds: c.timestamp_seconds,
                message: c.message,
                root_tree_digest: c.root_tree_digest.to_string(),
                parent_commits: c.parent_commits.iter().map(|d| d.to_string()).collect(),
            };
            (200, "application/json", serde_json::to_vec(&resp).unwrap())
        }
        Ok(None) => (404, "application/json", br#"{"error":"Commit not found"}"#.to_vec()),
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to get commit: {}", e) });
            (500, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Records commit metadata into the relational index.
pub fn handle_put_commit(meta_store: &Arc<dyn MetadataStore>, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    #[derive(Deserialize)]
    struct PutCommitRequest {
        pub digest: String,
        pub author: String,
        pub timestamp_seconds: i64,
        pub message: String,
        pub root_tree_digest: String,
        pub parent_commits: Vec<String>,
    }

    let req: PutCommitRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid JSON request: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let commit_digest = match Digest::parse(&req.digest) {
        Ok(d) => d,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid commit digest: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let root_tree_digest = match Digest::parse(&req.root_tree_digest) {
        Ok(d) => d,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid root tree digest: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let mut parent_commits = Vec::new();
    for p in req.parent_commits {
        if let Ok(d) = Digest::parse(&p) {
            parent_commits.push(d);
        }
    }

    let commit = Commit {
        author: req.author,
        timestamp_seconds: req.timestamp_seconds,
        message: req.message,
        root_tree_digest,
        parent_commits,
    };

    match meta_store.put_commit_metadata(&commit_digest, &commit) {
        Ok(()) => (200, "application/json", br#"{"status":"ok"}"#.to_vec()),
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to record commit metadata: {}", e) });
            (500, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}
