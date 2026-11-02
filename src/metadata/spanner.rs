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

//! # Spanner Distributed Relational Metadata Store
//!
//! Provides globally consistent ACID metadata storage for Graft across distributed
//! AI agent clusters.
//!
//! ### Why Spanner for Version Control Metadata:
//! While immutable file blobs and tree manifests live in Content-Addressed Storage
//! (GCS or NVMe SSDs), multi-agent coordination requires linearizable ACID guarantees:
//!
//! 1. **Atomic Branch Head CAS**: Updating a branch ref (e.g. `refs/heads/main`)
//!    must execute as an atomic Compare-And-Swap (`x-goog-if-generation-match` is not
//!    expressive enough for multi-table parent validations).
//! 2. **Distributed Workspace Leases**: Agents acquire time-bounded leases on task
//!    branches, preventing split-brain parallel work on identical issues.
//! 3. **Relational Commit Indexing**: Enables instant indexing of commit history
//!    by author, timestamp range, and ancestry paths.
//!
//! ### Spanner Table Schema (DDL):
//! ```sql
//! -- 1. Branch References
//! CREATE TABLE graft_refs (
//!     ref_name STRING(256) NOT NULL,
//!     target_commit STRING(128) NOT NULL,
//!     author STRING(256) NOT NULL,
//!     updated_at TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true),
//! ) PRIMARY KEY (ref_name);
//!
//! -- 2. Relational Commit Graph Index
//! CREATE TABLE graft_commits (
//!     commit_digest STRING(128) NOT NULL,
//!     root_tree STRING(128) NOT NULL,
//!     author STRING(256) NOT NULL,
//!     timestamp_seconds INT64 NOT NULL,
//!     message STRING(MAX) NOT NULL,
//!     created_at TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true),
//! ) PRIMARY KEY (commit_digest);
//!
//! -- 3. Parent Ancestry Links (Interleaved for O(1) locality)
//! CREATE TABLE graft_commit_parents (
//!     commit_digest STRING(128) NOT NULL,
//!     parent_index INT64 NOT NULL,
//!     parent_digest STRING(128) NOT NULL,
//! ) PRIMARY KEY (commit_digest, parent_index),
//!   INTERLEAVE IN PARENT graft_commits ON DELETE CASCADE;
//!
//! -- 4. Distributed Agent Workspace Leases
//! CREATE TABLE graft_workspace_leases (
//!     workspace_id STRING(128) NOT NULL,
//!     agent_id STRING(128) NOT NULL,
//!     base_commit STRING(128) NOT NULL,
//!     lease_expires_at TIMESTAMP NOT NULL,
//! ) PRIMARY KEY (workspace_id);
//! ```

use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use crate::cas::gcs::GcsTokenProvider;
use crate::metadata::{format_rfc3339_utc, parse_rfc3339_utc, MetadataStore, WorkspaceLease};
use std::collections::HashMap;
use std::env;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Configuration for connecting to Google Spanner or Cloud Spanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpannerConfig {
    /// Full database resource path:
    /// - Internal Spanner: `/span/global/<universe>/<db>` or `/span/nonprod/...`
    /// - Cloud Spanner: `projects/<project>/instances/<instance>/databases/<db>`
    pub database_path: String,
    /// Table name prefix (default: "graft_")
    pub table_prefix: String,
}

impl SpannerConfig {
    /// Validates and parses a Spanner database path.
    pub fn new(database_path: &str) -> io::Result<Self> {
        let trimmed = database_path.trim();
        // Validate valid Spanner naming conventions
        let is_internal_span = trimmed.starts_with("/span/global/")
            || trimmed.starts_with("/span/nonprod/")
            || trimmed.starts_with("/span/tmp/")
            || trimmed.starts_with("/span/test-universe/");
        let is_cloud_span = (trimmed.starts_with("projects/") || trimmed.starts_with("/projects/"))
            && trimmed.contains("/instances/");

        if !is_internal_span && !is_cloud_span {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Invalid Spanner database path '{}'. Expected internal path ('/span/global/...') or GCP Cloud Spanner path ('projects/.../instances/.../databases/...')",
                    database_path
                ),
            ));
        }

        Ok(Self {
            database_path: trimmed.trim_start_matches('/').to_string(),
            table_prefix: "graft_".to_string(),
        })
    }

    /// Emits the canonical DDL SQL statements to initialize the Spanner schema.
    pub fn ddl_schema(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE {}refs (ref_name STRING(256) NOT NULL, target_commit STRING(128) NOT NULL, author STRING(256) NOT NULL, updated_at TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true)) PRIMARY KEY (ref_name)",
                self.table_prefix
            ),
            format!(
                "CREATE TABLE {}commits (commit_digest STRING(128) NOT NULL, root_tree STRING(128) NOT NULL, author STRING(256) NOT NULL, timestamp_seconds INT64 NOT NULL, message STRING(MAX) NOT NULL, created_at TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp=true)) PRIMARY KEY (commit_digest)",
                self.table_prefix
            ),
            format!(
                "CREATE TABLE {}commit_parents (commit_digest STRING(128) NOT NULL, parent_index INT64 NOT NULL, parent_digest STRING(128) NOT NULL) PRIMARY KEY (commit_digest, parent_index), INTERLEAVE IN PARENT {}commits ON DELETE CASCADE",
                self.table_prefix, self.table_prefix
            ),
            format!(
                "CREATE TABLE {}workspace_leases (workspace_id STRING(128) NOT NULL, agent_id STRING(128) NOT NULL, base_commit STRING(128) NOT NULL, lease_expires_at TIMESTAMP NOT NULL) PRIMARY KEY (workspace_id)",
                self.table_prefix
            ),
        ]
    }
}

/// Abstract driver trait for Spanner transactional operations.
pub trait SpannerClient: Send + Sync {
    /// Executes a transactional Read-Modify-Write update.
    fn execute_cas_ref(
        &self,
        ref_name: &str,
        expected: Option<&str>,
        target: &str,
        author: &str,
    ) -> io::Result<()>;

    /// Deletes a reference atomically if expected matches.
    fn delete_ref(&self, ref_name: &str, expected: Option<&str>) -> io::Result<()>;

    /// Reads current reference target.
    fn read_ref(&self, ref_name: &str) -> io::Result<Option<String>>;

    /// Queries references matching prefix.
    fn query_refs_by_prefix(&self, prefix: &str) -> io::Result<Vec<(String, String)>>;

    /// Records commit and parent rows within a transaction.
    fn write_commit_metadata(
        &self,
        digest: &str,
        root_tree: &str,
        author: &str,
        timestamp: i64,
        message: &str,
        parents: &[String],
    ) -> io::Result<()>;

    /// Reads commit metadata.
    fn read_commit_metadata(
        &self,
        digest: &str,
    ) -> io::Result<Option<(String, String, i64, String, Vec<String>)>>;

    /// Acquires a workspace lease.
    fn acquire_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease>;

    /// Renews an active workspace lease.
    fn renew_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease>;

    /// Releases a workspace lease.
    fn release_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        force: bool,
    ) -> io::Result<()>;

    /// Reads current workspace lease.
    fn read_workspace_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>>;

    /// Lists all workspace leases.
    fn list_workspace_leases(&self) -> io::Result<Vec<WorkspaceLease>>;

    /// Executes DDL schema updates.
    fn update_ddl(&self, statements: &[String]) -> io::Result<()>;
}

/// Cloud Spanner REST client for production GCP Cloud Spanner (`https://spanner.googleapis.com`).
pub struct GcpCloudSpannerClient {
    database_path: String,
    endpoint: String,
    token_provider: Arc<GcsTokenProvider>,
    session: Mutex<Option<(String, Instant)>>,
}

impl GcpCloudSpannerClient {
    /// Creates a client connecting to the given database path.
    pub fn new(database_path: &str, token_provider: Arc<GcsTokenProvider>) -> Self {
        let endpoint = env::var("SPANNER_EMULATOR_HOST")
            .map(|h| format!("http://{}", h))
            .unwrap_or_else(|_| "https://spanner.googleapis.com".to_string());

        Self {
            database_path: database_path.trim_start_matches('/').to_string(),
            endpoint,
            token_provider,
            session: Mutex::new(None),
        }
    }

    fn get_session(&self) -> io::Result<String> {
        let mut lock = self.session.lock().unwrap();
        if let Some((s, created_at)) = &*lock {
            if created_at.elapsed() < Duration::from_secs(45 * 60) {
                return Ok(s.clone());
            }
        }

        let token = self.token_provider.get_token()?;
        let url = format!("{}/v1/{}/sessions", self.endpoint, self.database_path);
        let res = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .send(b"{}");

        match res {
            Ok(mut resp) => {
                let bytes = resp
                    .body_mut()
                    .with_config()
                    .read_to_vec()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let val: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let session_name = val["name"]
                    .as_str()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::Other, "Spanner session response missing 'name'")
                    })?
                    .to_string();
                *lock = Some((session_name.clone(), Instant::now()));
                Ok(session_name)
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to create Spanner session for '{}': {}", self.database_path, e),
            )),
        }
    }

    /// Low-latency KeySet Read API (bypasses SQL query engine entirely).
    fn read_keys(
        &self,
        table: &str,
        columns: &[&str],
        keys: &[&[&str]],
    ) -> io::Result<Vec<Vec<String>>> {
        let session = self.get_session()?;
        let token = self.token_provider.get_token()?;
        let url = format!("{}/v1/{}:read", self.endpoint, session);
        let payload = serde_json::json!({
            "table": table,
            "columns": columns,
            "keySet": {
                "keys": keys,
            },
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let res = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .send(&body);

        match res {
            Ok(mut resp) => {
                let bytes = resp
                    .body_mut()
                    .with_config()
                    .read_to_vec()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let val: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                let mut rows_out = Vec::new();
                if let Some(rows) = val["rows"].as_array() {
                    for r in rows {
                        if let Some(cols) = r.as_array() {
                            let str_cols = cols
                                .iter()
                                .map(|c| c.as_str().unwrap_or("").to_string())
                                .collect();
                            rows_out.push(str_cols);
                        }
                    }
                }
                Ok(rows_out)
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Spanner CRUD read_keys failed for table '{}': {}", table, e),
            )),
        }
    }

    /// Range KeySet Read for prefix matching (bypasses SQL engine).
    fn read_prefix(
        &self,
        table: &str,
        columns: &[&str],
        prefix: &str,
    ) -> io::Result<Vec<Vec<String>>> {
        let session = self.get_session()?;
        let token = self.token_provider.get_token()?;
        let url = format!("{}/v1/{}:read", self.endpoint, session);

        let key_set = if prefix.is_empty() {
            serde_json::json!({ "all": true })
        } else {
            serde_json::json!({
                "ranges": [{
                    "startClosed": [prefix],
                    "endOpen": [format!("{}~", prefix)]
                }]
            })
        };

        let payload = serde_json::json!({
            "table": table,
            "columns": columns,
            "keySet": key_set,
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let res = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .send(&body);

        match res {
            Ok(mut resp) => {
                let bytes = resp
                    .body_mut()
                    .with_config()
                    .read_to_vec()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let val: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                let mut rows_out = Vec::new();
                if let Some(rows) = val["rows"].as_array() {
                    for r in rows {
                        if let Some(cols) = r.as_array() {
                            let str_cols = cols
                                .iter()
                                .map(|c| c.as_str().unwrap_or("").to_string())
                                .collect();
                            rows_out.push(str_cols);
                        }
                    }
                }
                Ok(rows_out)
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Spanner CRUD read_prefix failed for table '{}': {}", table, e),
            )),
        }
    }

    fn commit_mutations(&self, mutations: serde_json::Value) -> io::Result<()> {
        let session = self.get_session()?;
        let token = self.token_provider.get_token()?;
        let url = format!("{}/v1/{}:commit", self.endpoint, session);
        let payload = serde_json::json!({
            "singleUseTransaction": {
                "readWrite": {}
            },
            "mutations": mutations,
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let res = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .send(&body);

        match res {
            Ok(_) => Ok(()),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Spanner commit failed: {}", e),
            )),
        }
    }

    fn begin_transaction(&self) -> io::Result<String> {
        let session = self.get_session()?;
        let token = self.token_provider.get_token()?;
        let url = format!("{}/v1/{}:beginTransaction", self.endpoint, session);
        let payload = serde_json::json!({
            "options": {
                "readWrite": {}
            }
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let res = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .send(&body);

        match res {
            Ok(mut resp) => {
                let bytes = resp
                    .body_mut()
                    .with_config()
                    .read_to_vec()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let val: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let id = val["id"]
                    .as_str()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::Other, "Spanner beginTransaction missing 'id'")
                    })?
                    .to_string();
                Ok(id)
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Spanner beginTransaction failed: {}", e),
            )),
        }
    }

    fn read_keys_tx(
        &self,
        tx_id: &str,
        table: &str,
        columns: &[&str],
        keys: &[&[&str]],
    ) -> io::Result<Vec<Vec<String>>> {
        let session = self.get_session()?;
        let token = self.token_provider.get_token()?;
        let url = format!("{}/v1/{}:read", self.endpoint, session);
        let payload = serde_json::json!({
            "transaction": {
                "id": tx_id
            },
            "table": table,
            "columns": columns,
            "keySet": {
                "keys": keys,
            },
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let res = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .send(&body);

        match res {
            Ok(mut resp) => {
                let bytes = resp
                    .body_mut()
                    .with_config()
                    .read_to_vec()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let val: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                let mut rows_out = Vec::new();
                if let Some(rows) = val["rows"].as_array() {
                    for r in rows {
                        if let Some(cols) = r.as_array() {
                            let str_cols = cols
                                .iter()
                                .map(|c| c.as_str().unwrap_or("").to_string())
                                .collect();
                            rows_out.push(str_cols);
                        }
                    }
                }
                Ok(rows_out)
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Spanner read_keys_tx failed for table '{}': {}", table, e),
            )),
        }
    }

    fn commit_tx(&self, tx_id: &str, mutations: serde_json::Value) -> Result<(), ureq::Error> {
        let session = self
            .get_session()
            .map_err(|e| ureq::Error::from(e))?;
        let token = self
            .token_provider
            .get_token()
            .map_err(|e| ureq::Error::from(e))?;
        let url = format!("{}/v1/{}:commit", self.endpoint, session);
        let payload = serde_json::json!({
            "transactionId": tx_id,
            "mutations": mutations,
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
            .map_err(|e| ureq::Error::from(e))?;

        ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .send(&body)?;
        Ok(())
    }
}

impl SpannerClient for GcpCloudSpannerClient {
    fn execute_cas_ref(
        &self,
        ref_name: &str,
        expected: Option<&str>,
        target: &str,
        author: &str,
    ) -> io::Result<()> {
        let current_ref = self.read_ref(ref_name)?;
        let current_digest = current_ref.as_deref().and_then(|c| Digest::parse(c).ok());
        let expected_digest = match expected {
            Some(e) => Some(Digest::parse(e).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?),
            None => None,
        };

        match (expected_digest, current_digest) {
            (Some(exp), Some(cur)) if exp.hash == cur.hash => {}
            (None, None) => {}
            (Some(exp), Some(cur)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Spanner CAS abort: ref '{}' moved from expected {} to {}", ref_name, exp, cur),
                ));
            }
            (Some(exp), None) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Spanner CAS abort: ref '{}' not found (expected {})", ref_name, exp),
                ));
            }
            (None, Some(cur)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Spanner CAS abort: ref '{}' already exists pointing to {}", ref_name, cur),
                ));
            }
        }

        let mutations = serde_json::json!([
            {
                "insertOrUpdate": {
                    "table": "graft_refs",
                    "columns": ["ref_name", "target_commit", "author", "updated_at"],
                    "values": [[ref_name, target, author, "spanner.commit_timestamp()"]]
                }
            }
        ]);
        self.commit_mutations(mutations)
    }

    fn delete_ref(&self, ref_name: &str, expected: Option<&str>) -> io::Result<()> {
        let current_ref = self.read_ref(ref_name)?;
        let current_digest = current_ref.as_deref().and_then(|c| Digest::parse(c).ok());
        let expected_digest = match expected {
            Some(e) => Some(Digest::parse(e).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?),
            None => None,
        };

        match (expected_digest, current_digest) {
            (Some(exp), Some(cur)) if exp.hash == cur.hash => {}
            (None, Some(_)) => {}
            (Some(exp), Some(cur)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Spanner CAS abort: ref '{}' moved from expected {} to {}", ref_name, exp, cur),
                ));
            }
            (Some(exp), None) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Spanner CAS abort: ref '{}' not found (expected {})", ref_name, exp),
                ));
            }
            (None, None) => return Ok(()),
        }

        let mutations = serde_json::json!([
            {
                "delete": {
                    "table": "graft_refs",
                    "keySet": {
                        "keys": [[ref_name]]
                    }
                }
            }
        ]);
        self.commit_mutations(mutations)
    }

    fn read_ref(&self, ref_name: &str) -> io::Result<Option<String>> {
        let rows = self.read_keys("graft_refs", &["target_commit"], &[&[ref_name]])?;
        if let Some(row) = rows.first() {
            if let Some(target) = row.first() {
                return Ok(Some(target.clone()));
            }
        }
        Ok(None)
    }

    fn query_refs_by_prefix(&self, prefix: &str) -> io::Result<Vec<(String, String)>> {
        let rows = self.read_prefix("graft_refs", &["ref_name", "target_commit"], prefix)?;
        let mut results = Vec::new();
        for r in rows {
            if r.len() >= 2 {
                results.push((r[0].clone(), r[1].clone()));
            }
        }
        Ok(results)
    }

    fn write_commit_metadata(
        &self,
        digest: &str,
        root_tree: &str,
        author: &str,
        timestamp: i64,
        message: &str,
        parents: &[String],
    ) -> io::Result<()> {
        let mut mutations = vec![serde_json::json!({
            "insertOrUpdate": {
                "table": "graft_commits",
                "columns": ["commit_digest", "root_tree", "author", "timestamp_seconds", "message", "created_at"],
                "values": [[digest, root_tree, author, timestamp.to_string(), message, "spanner.commit_timestamp()"]]
            }
        })];

        for (i, p) in parents.iter().enumerate() {
            mutations.push(serde_json::json!({
                "insertOrUpdate": {
                    "table": "graft_commit_parents",
                    "columns": ["commit_digest", "parent_index", "parent_digest"],
                    "values": [[digest, (i as i64).to_string(), p]]
                }
            }));
        }

        self.commit_mutations(serde_json::Value::Array(mutations))
    }

    fn read_commit_metadata(
        &self,
        digest: &str,
    ) -> io::Result<Option<(String, String, i64, String, Vec<String>)>> {
        let rows = self.read_keys(
            "graft_commits",
            &["root_tree", "author", "timestamp_seconds", "message"],
            &[&[digest]],
        )?;
        if let Some(row) = rows.first() {
            let root_tree = row.get(0).cloned().unwrap_or_default();
            let author = row.get(1).cloned().unwrap_or_default();
            let ts = row
                .get(2)
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let msg = row.get(3).cloned().unwrap_or_default();

            // Read parent digests using prefix range read on interleaved parent table
            let p_rows = self.read_prefix("graft_commit_parents", &["commit_digest", "parent_digest"], digest)?;
            let mut parents = Vec::new();
            for pr in p_rows {
                if pr.len() >= 2 && pr[0] == digest {
                    parents.push(pr[1].clone());
                }
            }
            return Ok(Some((root_tree, author, ts, msg, parents)));
        }
        Ok(None)
    }

    fn acquire_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let max_retries = 5;
        for attempt in 0..max_retries {
            let tx_id = self.begin_transaction()?;
            let rows = self.read_keys_tx(
                &tx_id,
                "graft_workspace_leases",
                &["workspace_id", "agent_id", "base_commit", "lease_expires_at"],
                &[&[workspace_id]],
            )?;

            let now_epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            if let Some(row) = rows.first() {
                if row.len() >= 4 {
                    let cur_owner = &row[1];
                    let cur_expires_at = &row[3];
                    let cur_expires_epoch = parse_rfc3339_utc(cur_expires_at).unwrap_or(0);
                    let is_active = cur_expires_epoch > now_epoch;

                    if is_active && cur_owner != agent_id {
                        let remaining = (cur_expires_epoch - now_epoch) as i64;
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "Workspace lease on '{}' is held by '{}' until {} ({}s remaining)",
                                workspace_id, cur_owner, cur_expires_at, remaining
                            ),
                        ));
                    }
                }
            }

            let new_expires_at = format_rfc3339_utc(now_epoch + ttl_seconds);
            let mutations = serde_json::json!([
                {
                    "insertOrUpdate": {
                        "table": "graft_workspace_leases",
                        "columns": ["workspace_id", "agent_id", "base_commit", "lease_expires_at"],
                        "values": [[workspace_id, agent_id, base_commit, new_expires_at]]
                    }
                }
            ]);

            match self.commit_tx(&tx_id, mutations) {
                Ok(_) => {
                    let base_digest = Digest::parse(base_commit)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    return Ok(WorkspaceLease {
                        workspace_id: workspace_id.to_string(),
                        agent_id: agent_id.to_string(),
                        base_commit: base_digest,
                        lease_expires_at: new_expires_at,
                    });
                }
                Err(ureq::Error::StatusCode(409)) => {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1) as u64));
                    continue;
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("Spanner lease acquire commit failed: {}", e),
                    ));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Spanner transaction aborted after maximum retries",
        ))
    }

    fn renew_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let max_retries = 5;
        for attempt in 0..max_retries {
            let tx_id = self.begin_transaction()?;
            let rows = self.read_keys_tx(
                &tx_id,
                "graft_workspace_leases",
                &["workspace_id", "agent_id", "base_commit", "lease_expires_at"],
                &[&[workspace_id]],
            )?;

            let row = rows.first().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Cannot renew lease: workspace '{}' has no active lease", workspace_id),
                )
            })?;

            if row.len() < 4 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid lease row in Spanner"));
            }

            let cur_owner = &row[1];
            let base_commit_str = &row[2];

            if cur_owner != agent_id {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Cannot renew lease for '{}': held by '{}', not '{}'", workspace_id, cur_owner, agent_id),
                ));
            }

            let now_epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let new_expires_at = format_rfc3339_utc(now_epoch + ttl_seconds);

            let mutations = serde_json::json!([
                {
                    "insertOrUpdate": {
                        "table": "graft_workspace_leases",
                        "columns": ["workspace_id", "agent_id", "base_commit", "lease_expires_at"],
                        "values": [[workspace_id, agent_id, base_commit_str, new_expires_at]]
                    }
                }
            ]);

            match self.commit_tx(&tx_id, mutations) {
                Ok(_) => {
                    let base_digest = Digest::parse(base_commit_str)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    return Ok(WorkspaceLease {
                        workspace_id: workspace_id.to_string(),
                        agent_id: agent_id.to_string(),
                        base_commit: base_digest,
                        lease_expires_at: new_expires_at,
                    });
                }
                Err(ureq::Error::StatusCode(409)) => {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1) as u64));
                    continue;
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("Spanner lease renew commit failed: {}", e),
                    ));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Spanner transaction aborted after maximum retries",
        ))
    }

    fn release_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        force: bool,
    ) -> io::Result<()> {
        let max_retries = 5;
        for attempt in 0..max_retries {
            let tx_id = self.begin_transaction()?;
            let rows = self.read_keys_tx(
                &tx_id,
                "graft_workspace_leases",
                &["workspace_id", "agent_id"],
                &[&[workspace_id]],
            )?;

            if let Some(row) = rows.first() {
                if let Some(cur_owner) = row.get(1) {
                    if !force && cur_owner != agent_id {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("Cannot release lease for '{}': held by '{}', not '{}'", workspace_id, cur_owner, agent_id),
                        ));
                    }
                }
            } else {
                return Ok(());
            }

            let mutations = serde_json::json!([
                {
                    "delete": {
                        "table": "graft_workspace_leases",
                        "keySet": {
                            "keys": [[workspace_id]]
                        }
                    }
                }
            ]);

            match self.commit_tx(&tx_id, mutations) {
                Ok(_) => return Ok(()),
                Err(ureq::Error::StatusCode(409)) => {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1) as u64));
                    continue;
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("Spanner lease release commit failed: {}", e),
                    ));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Spanner transaction aborted after maximum retries",
        ))
    }

    fn read_workspace_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>> {
        let rows = self.read_keys(
            "graft_workspace_leases",
            &["workspace_id", "agent_id", "base_commit", "lease_expires_at"],
            &[&[workspace_id]],
        )?;

        if let Some(row) = rows.first() {
            if row.len() >= 4 {
                let base_digest = Digest::parse(&row[2])
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                return Ok(Some(WorkspaceLease {
                    workspace_id: row[0].clone(),
                    agent_id: row[1].clone(),
                    base_commit: base_digest,
                    lease_expires_at: row[3].clone(),
                }));
            }
        }
        Ok(None)
    }

    fn list_workspace_leases(&self) -> io::Result<Vec<WorkspaceLease>> {
        let rows = self.read_prefix(
            "graft_workspace_leases",
            &["workspace_id", "agent_id", "base_commit", "lease_expires_at"],
            "",
        )?;

        let mut leases = Vec::new();
        for r in rows {
            if r.len() >= 4 {
                if let Ok(base_digest) = Digest::parse(&r[2]) {
                    leases.push(WorkspaceLease {
                        workspace_id: r[0].clone(),
                        agent_id: r[1].clone(),
                        base_commit: base_digest,
                        lease_expires_at: r[3].clone(),
                    });
                }
            }
        }
        Ok(leases)
    }

    fn update_ddl(&self, statements: &[String]) -> io::Result<()> {
        let token = self.token_provider.get_token()?;
        let url = format!("{}/v1/{}/ddl", self.endpoint, self.database_path);
        let payload = serde_json::json!({
            "statements": statements,
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let res = ureq::patch(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .send(&body);

        match res {
            Ok(mut resp) => {
                let bytes = resp
                    .body_mut()
                    .with_config()
                    .read_to_vec()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let val: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if let Some(err) = val.get("error") {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("Spanner DDL error: {}", err["message"].as_str().unwrap_or("unknown error")),
                    ));
                }
                Ok(())
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to execute Spanner DDL update for '{}': {}", self.database_path, e),
            )),
        }
    }
}

/// In-memory mock Spanner client providing TrueTime/ACID semantics for tests.
#[derive(Debug, Default)]
pub struct MockSpannerClient {
    refs: Mutex<HashMap<String, (String, String)>>, // name -> (commit, author)
    commits: Mutex<HashMap<String, (String, String, i64, String, Vec<String>)>>,
    leases: Mutex<HashMap<String, WorkspaceLease>>,
}

impl MockSpannerClient {
    /// Creates an empty in-memory Spanner mock.
    pub fn new() -> Self {
        Self {
            refs: Mutex::new(HashMap::new()),
            commits: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
        }
    }
}

impl SpannerClient for MockSpannerClient {
    fn execute_cas_ref(
        &self,
        ref_name: &str,
        expected: Option<&str>,
        target: &str,
        author: &str,
    ) -> io::Result<()> {
        let mut map = self.refs.lock().unwrap();
        let current_digest = map.get(ref_name).and_then(|(c, _)| Digest::parse(c).ok());
        let expected_digest = match expected {
            Some(e) => Some(Digest::parse(e).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?),
            None => None,
        };

        match (expected_digest, current_digest) {
            (Some(exp), Some(cur)) if exp.hash == cur.hash => {}
            (None, None) => {}
            (Some(exp), Some(cur)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Spanner CAS abort: ref '{}' moved from expected {} to {}",
                        ref_name, exp, cur
                    ),
                ));
            }
            (Some(exp), None) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Spanner CAS abort: ref '{}' not found (expected {})", ref_name, exp),
                ));
            }
            (None, Some(cur)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Spanner CAS abort: ref '{}' already exists at {}", ref_name, cur),
                ));
            }
        }

        map.insert(ref_name.to_string(), (target.to_string(), author.to_string()));
        Ok(())
    }

    fn delete_ref(&self, ref_name: &str, expected: Option<&str>) -> io::Result<()> {
        let mut map = self.refs.lock().unwrap();
        let current_digest = map.get(ref_name).and_then(|(c, _)| Digest::parse(c).ok());
        let expected_digest = match expected {
            Some(e) => Some(Digest::parse(e).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?),
            None => None,
        };

        match (expected_digest, current_digest) {
            (Some(exp), Some(cur)) if exp.hash == cur.hash => {}
            (None, Some(_)) => {}
            (Some(exp), Some(cur)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Spanner CAS abort: ref '{}' moved from expected {} to {}", ref_name, exp, cur),
                ));
            }
            (Some(exp), None) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Spanner CAS abort: ref '{}' not found (expected {})", ref_name, exp),
                ));
            }
            (None, None) => return Ok(()),
        }

        map.remove(ref_name);
        Ok(())
    }

    fn read_ref(&self, ref_name: &str) -> io::Result<Option<String>> {
        let map = self.refs.lock().unwrap();
        Ok(map.get(ref_name).map(|(c, _)| c.clone()))
    }

    fn query_refs_by_prefix(&self, prefix: &str) -> io::Result<Vec<(String, String)>> {
        let map = self.refs.lock().unwrap();
        let mut matches = Vec::new();
        for (k, (c, _)) in map.iter() {
            if k.starts_with(prefix) {
                matches.push((k.clone(), c.clone()));
            }
        }
        Ok(matches)
    }

    fn write_commit_metadata(
        &self,
        digest: &str,
        root_tree: &str,
        author: &str,
        timestamp: i64,
        message: &str,
        parents: &[String],
    ) -> io::Result<()> {
        let mut map = self.commits.lock().unwrap();
        map.insert(
            digest.to_string(),
            (
                root_tree.to_string(),
                author.to_string(),
                timestamp,
                message.to_string(),
                parents.to_vec(),
            ),
        );
        Ok(())
    }

    fn read_commit_metadata(
        &self,
        digest: &str,
    ) -> io::Result<Option<(String, String, i64, String, Vec<String>)>> {
        let map = self.commits.lock().unwrap();
        Ok(map.get(digest).cloned())
    }

    fn acquire_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let mut map = self.leases.lock().unwrap();
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if let Some(cur) = map.get(workspace_id) {
            let cur_expires_epoch = parse_rfc3339_utc(&cur.lease_expires_at).unwrap_or(0);
            let is_active = cur_expires_epoch > now_epoch;
            if is_active && cur.agent_id != agent_id {
                let remaining = (cur_expires_epoch - now_epoch) as i64;
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Workspace lease on '{}' is held by '{}' until {} ({}s remaining)",
                        workspace_id, cur.agent_id, cur.lease_expires_at, remaining
                    ),
                ));
            }
        }

        let new_expires_at = format_rfc3339_utc(now_epoch + ttl_seconds);
        let base_digest = Digest::parse(base_commit)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let lease = WorkspaceLease {
            workspace_id: workspace_id.to_string(),
            agent_id: agent_id.to_string(),
            base_commit: base_digest,
            lease_expires_at: new_expires_at,
        };
        map.insert(workspace_id.to_string(), lease.clone());
        Ok(lease)
    }

    fn renew_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let mut map = self.leases.lock().unwrap();
        let lease = map.get_mut(workspace_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Cannot renew lease: workspace '{}' has no active lease", workspace_id),
            )
        })?;

        if lease.agent_id != agent_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Cannot renew lease for '{}': held by '{}', not '{}'",
                    workspace_id, lease.agent_id, agent_id
                ),
            ));
        }

        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        lease.lease_expires_at = format_rfc3339_utc(now_epoch + ttl_seconds);
        Ok(lease.clone())
    }

    fn release_workspace_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        force: bool,
    ) -> io::Result<()> {
        let mut map = self.leases.lock().unwrap();
        if let Some(lease) = map.get(workspace_id) {
            if !force && lease.agent_id != agent_id {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "Cannot release lease for '{}': held by '{}', not '{}'",
                        workspace_id, lease.agent_id, agent_id
                    ),
                ));
            }
            map.remove(workspace_id);
        }
        Ok(())
    }

    fn read_workspace_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>> {
        let map = self.leases.lock().unwrap();
        Ok(map.get(workspace_id).cloned())
    }

    fn list_workspace_leases(&self) -> io::Result<Vec<WorkspaceLease>> {
        let map = self.leases.lock().unwrap();
        Ok(map.values().cloned().collect())
    }

    fn update_ddl(&self, _statements: &[String]) -> io::Result<()> {
        Ok(())
    }
}

/// Spanner-backed implementation of [`MetadataStore`].
pub struct SpannerMetadataStore {
    config: SpannerConfig,
    client: Arc<dyn SpannerClient>,
}

impl SpannerMetadataStore {
    /// Creates a new Spanner metadata store backed by an abstract client.
    pub fn new(config: SpannerConfig, client: Arc<dyn SpannerClient>) -> Self {
        Self { config, client }
    }

    /// Connects to a GCP Cloud Spanner database using Application Default Credentials / OAuth token.
    pub fn connect_gcp(database_path: &str) -> io::Result<Self> {
        let config = SpannerConfig::new(database_path)?;
        let token_provider = Arc::new(GcsTokenProvider::new());
        let client = Arc::new(GcpCloudSpannerClient::new(database_path, token_provider));
        Ok(Self::new(config, client))
    }

    /// Access configuration.
    pub fn config(&self) -> &SpannerConfig {
        &self.config
    }

    /// Executes DDL schema updates against the Spanner database.
    pub fn update_ddl(&self, statements: &[String]) -> io::Result<()> {
        self.client.update_ddl(statements)
    }

    /// Initializes all required Graft tables in the Spanner database.
    pub fn init_schema(&self) -> io::Result<()> {
        let stmts = self.config.ddl_schema();
        self.client.update_ddl(&stmts)
    }
}

impl MetadataStore for SpannerMetadataStore {
    fn get_ref(&self, ref_name: &str) -> io::Result<Option<Digest>> {
        if let Some(target_str) = self.client.read_ref(ref_name)? {
            let digest = Digest::parse(&target_str)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some(digest))
        } else {
            Ok(None)
        }
    }

    fn compare_and_swap_ref(
        &self,
        ref_name: &str,
        expected: Option<&Digest>,
        target: &Digest,
        author: &str,
    ) -> io::Result<()> {
        let exp_str = expected.map(|d| d.to_string());
        self.client.execute_cas_ref(
            ref_name,
            exp_str.as_deref(),
            &target.to_string(),
            author,
        )
    }

    fn list_refs(&self, prefix: &str) -> io::Result<Vec<(String, Digest)>> {
        let raw = self.client.query_refs_by_prefix(prefix)?;
        let mut results = Vec::new();
        for (name, val) in raw {
            if let Ok(d) = Digest::parse(&val) {
                results.push((name, d));
            }
        }
        Ok(results)
    }

    fn delete_ref(&self, ref_name: &str, expected: Option<&Digest>) -> io::Result<()> {
        let exp_str = expected.map(|d| d.to_string());
        self.client.delete_ref(ref_name, exp_str.as_deref())
    }

    fn put_commit_metadata(&self, commit_digest: &Digest, commit: &Commit) -> io::Result<()> {
        let parents = commit
            .parent_commits
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>();
        self.client.write_commit_metadata(
            &commit_digest.to_string(),
            &commit.root_tree_digest.to_string(),
            &commit.author,
            commit.timestamp_seconds,
            &commit.message,
            &parents,
        )
    }

    fn get_commit_metadata(&self, commit_digest: &Digest) -> io::Result<Option<Commit>> {
        if let Some((root_tree_str, author, timestamp, message, parents_str)) =
            self.client.read_commit_metadata(&commit_digest.to_string())?
        {
            let root_tree_digest = Digest::parse(&root_tree_str)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut parent_commits = Vec::new();
            for p in parents_str {
                if let Ok(d) = Digest::parse(&p) {
                    parent_commits.push(d);
                }
            }
            Ok(Some(Commit {
                parent_commits,
                root_tree_digest,
                author,
                message,
                timestamp_seconds: timestamp,
            }))
        } else {
            Ok(None)
        }
    }

    fn acquire_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &Digest,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        self.client.acquire_workspace_lease(
            workspace_id,
            agent_id,
            &base_commit.to_string(),
            ttl_seconds,
        )
    }

    fn renew_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        self.client.renew_workspace_lease(workspace_id, agent_id, ttl_seconds)
    }

    fn release_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        force: bool,
    ) -> io::Result<()> {
        self.client.release_workspace_lease(workspace_id, agent_id, force)
    }

    fn get_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>> {
        self.client.read_workspace_lease(workspace_id)
    }

    fn list_leases(&self) -> io::Result<Vec<WorkspaceLease>> {
        self.client.list_workspace_leases()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spanner_config_and_ddl() {
        let conf = SpannerConfig::new("/span/global/graft-prod/metadata").unwrap();
        assert_eq!(conf.database_path, "span/global/graft-prod/metadata");
        let ddl = conf.ddl_schema();
        assert_eq!(ddl.len(), 4);
        assert!(ddl[0].contains("CREATE TABLE graft_refs"));

        let cloud_conf = SpannerConfig::new("projects/vmm-net-prod/instances/graft-inst/databases/graft-db").unwrap();
        assert!(cloud_conf.database_path.starts_with("projects/"));

        // Invalid path should fail
        assert!(SpannerConfig::new("/invalid/path").is_err());
    }

    #[test]
    fn test_spanner_metadata_store_cas() -> io::Result<()> {
        let conf = SpannerConfig::new("/span/global/test/db").unwrap();
        let client = Arc::new(MockSpannerClient::new());
        let store = SpannerMetadataStore::new(conf, client);

        let c1 = Digest::from_bytes(b"spanner commit 1");
        let c2 = Digest::from_bytes(b"spanner commit 2");

        // 1. Initial CAS insertion
        store.compare_and_swap_ref("refs/heads/main", None, &c1, "lead@graft.dev")?;
        assert_eq!(store.get_ref("refs/heads/main")?, Some(c1));

        // 2. Conflicting CAS update must fail
        let fake = Digest::from_bytes(b"wrong ancestor");
        assert!(store
            .compare_and_swap_ref("refs/heads/main", Some(&fake), &c2, "agent@graft.dev")
            .is_err());

        // 3. Valid CAS advance
        store.compare_and_swap_ref("refs/heads/main", Some(&c1), &c2, "agent@graft.dev")?;
        assert_eq!(store.get_ref("refs/heads/main")?, Some(c2));

        // 4. Commit metadata roundtrip
        let commit = Commit::new(vec![c1], c2, "Author <a@b.com>".to_string(), "feat: Spanner metadata".to_string());
        store.put_commit_metadata(&c2, &commit)?;

        let retrieved = store.get_commit_metadata(&c2)?.expect("commit must exist");
        assert_eq!(retrieved.author, "Author <a@b.com>");
        assert_eq!(retrieved.parent_commits, vec![c1]);

        Ok(())
    }

    #[test]
    fn test_mock_spanner_leases() -> io::Result<()> {
        let conf = SpannerConfig::new("projects/test/instances/test/databases/test").unwrap();
        let client = Arc::new(MockSpannerClient::new());
        let store = SpannerMetadataStore::new(conf, client);

        let base = Digest::from_bytes(b"base-tree");

        // 1. Agent 1 acquires lease for 60s
        let l1 = store.acquire_lease("task-100", "agent-alpha", &base, 60)?;
        assert_eq!(l1.agent_id, "agent-alpha");
        assert!(l1.is_active());

        // 2. Agent 2 attempts acquisition -> conflict
        let conflict = store.acquire_lease("task-100", "agent-beta", &base, 60);
        assert!(conflict.is_err());
        assert_eq!(conflict.unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        // 3. Agent 1 heartbeats / renews
        let renewed = store.renew_lease("task-100", "agent-alpha", 120)?;
        assert!(renewed.is_active());

        // 4. Agent 2 cannot release Agent 1's lease
        assert!(store.release_lease("task-100", "agent-beta", false).is_err());

        // 5. Agent 1 releases lease
        store.release_lease("task-100", "agent-alpha", false)?;
        assert!(store.get_lease("task-100")?.is_none());

        // 6. Agent 2 can now acquire
        let l2 = store.acquire_lease("task-100", "agent-beta", &base, 60)?;
        assert_eq!(l2.agent_id, "agent-beta");

        Ok(())
    }

    #[test]
    fn test_spanner_init_schema() -> io::Result<()> {
        let conf = SpannerConfig::new("projects/test/instances/test/databases/test").unwrap();
        let client = Arc::new(MockSpannerClient::new());
        let store = SpannerMetadataStore::new(conf, client);

        assert!(store.init_schema().is_ok());
        Ok(())
    }
}
