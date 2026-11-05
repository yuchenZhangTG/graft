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

//! # Server-Side Workspace Lease Service
//!
//! Provides distributed concurrency coordination preventing multiple AI agents
//! from mutating the same branch or task workspace simultaneously.

use crate::cas::digest::Digest;
use crate::metadata::MetadataStore;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct AcquireLeaseRequest {
    pub workspace_id: String,
    pub agent_id: String,
    pub base_commit: String,
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct RenewLeaseRequest {
    pub workspace_id: String,
    pub agent_id: String,
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseLeaseRequest {
    pub workspace_id: String,
    pub agent_id: String,
    pub force: Option<bool>,
}

/// Acquires an exclusive workspace lease for an agent.
pub fn handle_acquire_lease(meta_store: &Arc<dyn MetadataStore>, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let req: AcquireLeaseRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid JSON request: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let base_commit = match Digest::parse(&req.base_commit) {
        Ok(d) => d,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid base commit digest: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let ttl = req.ttl_seconds.unwrap_or(300);

    match meta_store.acquire_lease(&req.workspace_id, &req.agent_id, &base_commit, ttl) {
        Ok(lease) => {
            let resp = serde_json::json!({
                "status": "acquired",
                "lease": lease,
            });
            (200, "application/json", serde_json::to_vec(&resp).unwrap())
        }
        Err(e) => {
            let status = if e.kind() == std::io::ErrorKind::AlreadyExists {
                409 // Lease contention
            } else {
                500
            };
            let err = serde_json::json!({ "error": e.to_string() });
            (status, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Renews an active workspace lease.
pub fn handle_renew_lease(meta_store: &Arc<dyn MetadataStore>, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let req: RenewLeaseRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid JSON request: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let ttl = req.ttl_seconds.unwrap_or(300);

    match meta_store.renew_lease(&req.workspace_id, &req.agent_id, ttl) {
        Ok(lease) => {
            let resp = serde_json::json!({
                "status": "renewed",
                "lease": lease,
            });
            (200, "application/json", serde_json::to_vec(&resp).unwrap())
        }
        Err(e) => {
            let status = match e.kind() {
                std::io::ErrorKind::NotFound => 404,
                std::io::ErrorKind::PermissionDenied => 403,
                _ => 500,
            };
            let err = serde_json::json!({ "error": e.to_string() });
            (status, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Releases a workspace lease.
pub fn handle_release_lease(meta_store: &Arc<dyn MetadataStore>, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let req: ReleaseLeaseRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid JSON request: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let force = req.force.unwrap_or(false);

    match meta_store.release_lease(&req.workspace_id, &req.agent_id, force) {
        Ok(()) => (200, "application/json", br#"{"status":"released"}"#.to_vec()),
        Err(e) => {
            let status = if e.kind() == std::io::ErrorKind::PermissionDenied {
                403
            } else {
                500
            };
            let err = serde_json::json!({ "error": e.to_string() });
            (status, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Retrieves the current lease on a workspace ID.
pub fn handle_get_lease(meta_store: &Arc<dyn MetadataStore>, workspace_id: &str) -> (u16, &'static str, Vec<u8>) {
    match meta_store.get_lease(workspace_id) {
        Ok(Some(lease)) => {
            let resp = serde_json::json!({ "lease": lease });
            (200, "application/json", serde_json::to_vec(&resp).unwrap())
        }
        Ok(None) => (404, "application/json", br#"{"error":"Lease not found"}"#.to_vec()),
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to get lease: {}", e) });
            (500, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Lists all workspace leases.
pub fn handle_list_leases(meta_store: &Arc<dyn MetadataStore>) -> (u16, &'static str, Vec<u8>) {
    match meta_store.list_leases() {
        Ok(leases) => {
            let resp = serde_json::json!({ "leases": leases });
            (200, "application/json", serde_json::to_vec(&resp).unwrap())
        }
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to list leases: {}", e) });
            (500, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}
