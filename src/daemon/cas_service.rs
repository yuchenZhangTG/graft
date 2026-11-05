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

//! # Server-Side Content-Addressed Storage (CAS) Service
//!
//! Exposes HTTP endpoints for blob existence checks, single/batch downloads,
//! uploads with cryptographic integrity verification, and missing blob discovery.

use crate::cas::blob_store::BlobStore;
use crate::cas::digest::Digest;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct MissingBlobsRequest {
    pub digests: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MissingBlobsResponse {
    pub missing: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PutBlobResponse {
    pub digest: String,
    pub size_bytes: usize,
}

/// Checks whether a blob exists in the CAS.
pub fn handle_has_blob(blob_store: &Arc<dyn BlobStore>, digest_str: &str) -> (u16, &'static str, Vec<u8>) {
    let digest = match Digest::parse(digest_str) {
        Ok(d) => d,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid digest '{}': {}", digest_str, e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    if blob_store.has(&digest) {
        (200, "application/json", br#"{"exists":true}"#.to_vec())
    } else {
        (404, "application/json", br#"{"exists":false}"#.to_vec())
    }
}

/// Retrieves the raw bytes of a blob.
pub fn handle_get_blob(blob_store: &Arc<dyn BlobStore>, digest_str: &str) -> (u16, &'static str, Vec<u8>) {
    let digest = match Digest::parse(digest_str) {
        Ok(d) => d,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid digest '{}': {}", digest_str, e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    match blob_store.get(&digest) {
        Ok(Some(bytes)) => (200, "application/octet-stream", bytes),
        Ok(None) => (404, "application/json", br#"{"error":"Blob not found"}"#.to_vec()),
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Storage error: {}", e) });
            (500, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Stores a raw byte payload in the CAS, validating its hash if an expected digest is provided.
pub fn handle_put_blob(
    blob_store: &Arc<dyn BlobStore>,
    expected_digest_str: Option<&str>,
    data: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let computed = Digest::from_bytes(data);

    if let Some(expected_str) = expected_digest_str {
        if let Ok(expected) = Digest::parse(expected_str) {
            if computed.hash != expected.hash {
                let err = serde_json::json!({
                    "error": format!(
                        "Hash mismatch: expected {}, computed {}",
                        expected, computed
                    )
                });
                return (400, "application/json", serde_json::to_vec(&err).unwrap());
            }
        }
    }

    match blob_store.put(data) {
        Ok(saved_digest) => {
            let resp = PutBlobResponse {
                digest: saved_digest.to_string(),
                size_bytes: data.len(),
            };
            (201, "application/json", serde_json::to_vec(&resp).unwrap())
        }
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Failed to store blob: {}", e) });
            (500, "application/json", serde_json::to_vec(&err).unwrap())
        }
    }
}

/// Identifies which digests among the requested list are absent on the server.
pub fn handle_find_missing(blob_store: &Arc<dyn BlobStore>, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    let req: MissingBlobsRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = serde_json::json!({ "error": format!("Invalid JSON body: {}", e) });
            return (400, "application/json", serde_json::to_vec(&err).unwrap());
        }
    };

    let mut missing = Vec::new();
    for d_str in req.digests {
        if let Ok(digest) = Digest::parse(&d_str) {
            if !blob_store.has(&digest) {
                missing.push(d_str);
            }
        } else {
            // Unparseable digest is considered missing
            missing.push(d_str);
        }
    }

    let resp = MissingBlobsResponse { missing };
    (200, "application/json", serde_json::to_vec(&resp).unwrap())
}
