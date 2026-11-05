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

//! # Client Adapters for Remote `graftd` Server
//!
//! Provides [`RemoteBlobStore`] (implementing [`BlobStore`]) and
//! [`RemoteMetadataStore`] (implementing [`MetadataStore`]) allowing clients
//! to communicate transparently with `graftd` over HTTP/REST with Keep-Alive connection reuse.

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use crate::cas::gcs::percent_encode;
use crate::daemon::merge_service::{ServerSideMergeRequest, ServerSideMergeResponse};
use crate::metadata::{MetadataStore, WorkspaceLease};
use serde::Deserialize;
use std::io;
use std::path::PathBuf;

/// Remote Content-Addressed Storage client backed by `graftd`.
pub struct RemoteBlobStore {
    base_url: String,
    auth_token: Option<String>,
}

impl RemoteBlobStore {
    /// Connects to a `graftd` daemon instance at `base_url` (e.g. `"http://127.0.0.1:9090"`).
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_token: std::env::var("GRAFT_AUTH_TOKEN").ok(),
        }
    }

    /// Connects with an explicit bearer authentication token.
    pub fn with_auth(base_url: &str, auth_token: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_token,
        }
    }

    fn add_auth<B>(&self, mut req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        if let Some(ref token) = self.auth_token {
            req = req.header("Authorization", &format!("Bearer {}", token));
        }
        req
    }

    /// Discovers which of the given digests are absent on the server in a single batch RPC.
    pub fn find_missing(&self, digests: &[Digest]) -> io::Result<Vec<Digest>> {
        let url = format!("{}/api/v1/cas/missing", self.base_url);
        let req_body = serde_json::json!({
            "digests": digests.iter().map(|d| d.to_string()).collect::<Vec<_>>()
        });

        let mut req = ureq::post(&url).header("Content-Type", "application/json");
        req = self.add_auth(req);

        let mut resp = req.send(serde_json::to_vec(&req_body).unwrap().as_slice()).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Find missing RPC failed: {}", e))
        })?;

        #[derive(Deserialize)]
        struct Resp {
            pub missing: Vec<String>,
        }

        let body_bytes = read_all_bytes(&mut resp)?;
        let parsed: Resp = serde_json::from_slice(&body_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let mut results = Vec::new();
        for m in parsed.missing {
            if let Ok(d) = Digest::parse(&m) {
                results.push(d);
            }
        }
        Ok(results)
    }
}

impl BlobStore for RemoteBlobStore {
    fn put(&self, data: &[u8]) -> io::Result<Digest> {
        let computed = Digest::from_bytes(data);
        let url = format!("{}/api/v1/cas/blobs/{}", self.base_url, computed);

        let mut req = ureq::put(&url).header("Content-Type", "application/octet-stream");
        req = self.add_auth(req);

        let resp = req.send(data);
        match resp {
            Ok(_) => Ok(computed),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote PUT blob failed: {}", e),
            )),
        }
    }

    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>> {
        let url = format!("{}/api/v1/cas/blobs/{}", self.base_url, digest);
        let mut req = ureq::get(&url);
        req = self.add_auth(req);

        match req.call() {
            Ok(mut resp) => {
                let bytes = read_all_bytes(&mut resp)?;
                // Cryptographic bit-rot validation
                let computed = Digest::from_bytes(&bytes);
                if computed.hash != digest.hash {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "graftd bit-rot corruption detected for {}: expected {}, computed {}",
                            digest, digest, computed
                        ),
                    ));
                }
                Ok(Some(bytes))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote GET blob failed: {}", e),
            )),
        }
    }

    fn has(&self, digest: &Digest) -> bool {
        let url = format!("{}/api/v1/cas/blobs/{}", self.base_url, digest);
        let mut req = ureq::head(&url);
        req = self.add_auth(req);

        match req.call() {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    fn get_file_path(&self, _digest: &Digest) -> Option<PathBuf> {
        // Remote store has no local POSIX path.
        // CachedBlobStore transparently hydrates blobs to local NVMe L1 cache on demand.
        None
    }
}

/// Remote metadata, branch reference, and lease coordinator backed by `graftd`.
pub struct RemoteMetadataStore {
    base_url: String,
    auth_token: Option<String>,
}

impl RemoteMetadataStore {
    /// Connects to a `graftd` metadata service at `base_url`.
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_token: std::env::var("GRAFT_AUTH_TOKEN").ok(),
        }
    }

    /// Connects with an explicit bearer authentication token.
    pub fn with_auth(base_url: &str, auth_token: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_token,
        }
    }

    fn add_auth<B>(&self, mut req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        if let Some(ref token) = self.auth_token {
            req = req.header("Authorization", &format!("Bearer {}", token));
        }
        req
    }

    /// Requests `graftd` to execute an in-memory 3-way AST semantic merge on the server.
    pub fn server_side_merge(&self, req: &ServerSideMergeRequest) -> io::Result<ServerSideMergeResponse> {
        let url = format!("{}/api/v1/merge", self.base_url);
        let mut http_req = ureq::post(&url).header("Content-Type", "application/json");
        http_req = self.add_auth(http_req);

        let json_body = serde_json::to_vec(req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        match http_req.send(json_body.as_slice()) {
            Ok(mut resp) => {
                let bytes = read_all_bytes(&mut resp)?;
                let parsed: ServerSideMergeResponse = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(parsed)
            }
            Err(ureq::Error::StatusCode(409)) => {
                Err(io::Error::new(io::ErrorKind::AlreadyExists, "Merge conflict detected"))
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Server-side merge failed: {}", e),
            )),
        }
    }
}

impl MetadataStore for RemoteMetadataStore {
    fn get_ref(&self, ref_name: &str) -> io::Result<Option<Digest>> {
        let url = format!("{}/api/v1/refs/{}", self.base_url, ref_name);
        let mut req = ureq::get(&url);
        req = self.add_auth(req);

        match req.call() {
            Ok(mut resp) => {
                #[derive(Deserialize)]
                struct Resp {
                    pub digest: String,
                }
                let bytes = read_all_bytes(&mut resp)?;
                let parsed: Resp = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let digest = Digest::parse(&parsed.digest)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(digest))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote get_ref failed: {}", e),
            )),
        }
    }

    fn compare_and_swap_ref(
        &self,
        ref_name: &str,
        expected: Option<&Digest>,
        target: &Digest,
        author: &str,
    ) -> io::Result<()> {
        let url = format!("{}/api/v1/refs/cas", self.base_url);
        let req_body = serde_json::json!({
            "ref_name": ref_name,
            "expected": expected.map(|d| d.to_string()),
            "target": target.to_string(),
            "author": author,
        });

        let mut req = ureq::post(&url).header("Content-Type", "application/json");
        req = self.add_auth(req);

        match req.send(serde_json::to_vec(&req_body).unwrap().as_slice()) {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(409)) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("CAS conflict on '{}'", ref_name),
            )),
            Err(ureq::Error::StatusCode(404)) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Ref '{}' not found", ref_name),
            )),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote compare_and_swap_ref failed: {}", e),
            )),
        }
    }

    fn list_refs(&self, prefix: &str) -> io::Result<Vec<(String, Digest)>> {
        let url = format!(
            "{}/api/v1/refs?prefix={}",
            self.base_url,
            percent_encode(prefix)
        );
        let mut req = ureq::get(&url);
        req = self.add_auth(req);

        let mut resp = req.call().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Remote list_refs failed: {}", e))
        })?;

        #[derive(Deserialize)]
        struct RefEntry {
            pub name: String,
            pub digest: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            pub refs: Vec<RefEntry>,
        }

        let bytes = read_all_bytes(&mut resp)?;
        let parsed: Resp = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let mut out = Vec::new();
        for r in parsed.refs {
            if let Ok(d) = Digest::parse(&r.digest) {
                out.push((r.name, d));
            }
        }
        Ok(out)
    }

    fn delete_ref(&self, ref_name: &str, expected: Option<&Digest>) -> io::Result<()> {
        let url = if let Some(exp) = expected {
            format!(
                "{}/api/v1/refs/{}?expected={}",
                self.base_url,
                percent_encode(ref_name),
                percent_encode(&exp.to_string())
            )
        } else {
            format!("{}/api/v1/refs/{}", self.base_url, percent_encode(ref_name))
        };

        let mut req = ureq::delete(&url);
        req = self.add_auth(req);

        match req.call() {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(409)) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Cannot delete '{}': ref has moved", ref_name),
            )),
            Err(ureq::Error::StatusCode(404)) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Ref '{}' not found", ref_name),
            )),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote delete_ref failed: {}", e),
            )),
        }
    }

    fn put_commit_metadata(&self, commit_digest: &Digest, commit: &Commit) -> io::Result<()> {
        let url = format!("{}/api/v1/commits", self.base_url);
        let req_body = serde_json::json!({
            "digest": commit_digest.to_string(),
            "author": commit.author,
            "timestamp_seconds": commit.timestamp_seconds,
            "message": commit.message,
            "root_tree_digest": commit.root_tree_digest.to_string(),
            "parent_commits": commit.parent_commits.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
        });

        let mut req = ureq::post(&url).header("Content-Type", "application/json");
        req = self.add_auth(req);

        req.send(serde_json::to_vec(&req_body).unwrap().as_slice())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("put_commit_metadata failed: {}", e)))?;

        Ok(())
    }

    fn get_commit_metadata(&self, commit_digest: &Digest) -> io::Result<Option<Commit>> {
        let url = format!("{}/api/v1/commits/{}", self.base_url, commit_digest);
        let mut req = ureq::get(&url);
        req = self.add_auth(req);

        match req.call() {
            Ok(mut resp) => {
                #[derive(Deserialize)]
                struct Resp {
                    pub author: String,
                    pub timestamp_seconds: i64,
                    pub message: String,
                    pub root_tree_digest: String,
                    pub parent_commits: Vec<String>,
                }
                let bytes = read_all_bytes(&mut resp)?;
                let parsed: Resp = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                let root_tree_digest = Digest::parse(&parsed.root_tree_digest)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let mut parent_commits = Vec::new();
                for p in parsed.parent_commits {
                    if let Ok(d) = Digest::parse(&p) {
                        parent_commits.push(d);
                    }
                }

                Ok(Some(Commit {
                    author: parsed.author,
                    timestamp_seconds: parsed.timestamp_seconds,
                    message: parsed.message,
                    root_tree_digest,
                    parent_commits,
                }))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote get_commit_metadata failed: {}", e),
            )),
        }
    }

    fn acquire_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &Digest,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let url = format!("{}/api/v1/leases/acquire", self.base_url);
        let req_body = serde_json::json!({
            "workspace_id": workspace_id,
            "agent_id": agent_id,
            "base_commit": base_commit.to_string(),
            "ttl_seconds": ttl_seconds,
        });

        let mut req = ureq::post(&url).header("Content-Type", "application/json");
        req = self.add_auth(req);

        match req.send(serde_json::to_vec(&req_body).unwrap().as_slice()) {
            Ok(mut resp) => {
                #[derive(Deserialize)]
                struct Resp {
                    pub lease: WorkspaceLease,
                }
                let bytes = read_all_bytes(&mut resp)?;
                let parsed: Resp = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(parsed.lease)
            }
            Err(ureq::Error::StatusCode(409)) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Workspace lease on '{}' is held by another agent", workspace_id),
            )),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote acquire_lease failed: {}", e),
            )),
        }
    }

    fn renew_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let url = format!("{}/api/v1/leases/renew", self.base_url);
        let req_body = serde_json::json!({
            "workspace_id": workspace_id,
            "agent_id": agent_id,
            "ttl_seconds": ttl_seconds,
        });

        let mut req = ureq::post(&url).header("Content-Type", "application/json");
        req = self.add_auth(req);

        match req.send(serde_json::to_vec(&req_body).unwrap().as_slice()) {
            Ok(mut resp) => {
                #[derive(Deserialize)]
                struct Resp {
                    pub lease: WorkspaceLease,
                }
                let bytes = read_all_bytes(&mut resp)?;
                let parsed: Resp = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(parsed.lease)
            }
            Err(ureq::Error::StatusCode(403)) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("Cannot renew lease on '{}': permission denied", workspace_id),
            )),
            Err(ureq::Error::StatusCode(404)) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Lease on '{}' not found", workspace_id),
            )),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote renew_lease failed: {}", e),
            )),
        }
    }

    fn release_lease(&self, workspace_id: &str, agent_id: &str, force: bool) -> io::Result<()> {
        let url = format!("{}/api/v1/leases/release", self.base_url);
        let req_body = serde_json::json!({
            "workspace_id": workspace_id,
            "agent_id": agent_id,
            "force": force,
        });

        let mut req = ureq::post(&url).header("Content-Type", "application/json");
        req = self.add_auth(req);

        match req.send(serde_json::to_vec(&req_body).unwrap().as_slice()) {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(403)) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("Cannot release lease on '{}': permission denied", workspace_id),
            )),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote release_lease failed: {}", e),
            )),
        }
    }

    fn get_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>> {
        let url = format!("{}/api/v1/leases/{}", self.base_url, workspace_id);
        let mut req = ureq::get(&url);
        req = self.add_auth(req);

        match req.call() {
            Ok(mut resp) => {
                #[derive(Deserialize)]
                struct Resp {
                    pub lease: WorkspaceLease,
                }
                let bytes = read_all_bytes(&mut resp)?;
                let parsed: Resp = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(parsed.lease))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Remote get_lease failed: {}", e),
            )),
        }
    }

    fn list_leases(&self) -> io::Result<Vec<WorkspaceLease>> {
        let url = format!("{}/api/v1/leases", self.base_url);
        let mut req = ureq::get(&url);
        req = self.add_auth(req);

        let mut resp = req.call().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Remote list_leases failed: {}", e))
        })?;

        #[derive(Deserialize)]
        struct Resp {
            pub leases: Vec<WorkspaceLease>,
        }

        let bytes = read_all_bytes(&mut resp)?;
        let parsed: Resp = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(parsed.leases)
    }
}

fn read_all_bytes(resp: &mut ureq::http::Response<ureq::Body>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut resp.body_mut().as_reader(), &mut bytes)?;
    Ok(bytes)
}
