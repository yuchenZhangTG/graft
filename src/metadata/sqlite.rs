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

//! # Embedded SQLite Metadata & Reference Store
//!
//! Provides a zero-dependency, local-first, serverless implementation of [`MetadataStore`]
//! backed by embedded SQLite.
//!
//! ### Key Capabilities:
//! 1. **Zero-Setup ACID Transactions**: Runs directly in-process with WAL (Write-Ahead Logging)
//!    for high-concurrency multi-threaded access without requiring external daemons or Spanner instances.
//! 2. **Single-File Portability**: All branch refs, commit indices, and distributed agent workspace
//!    leases are stored in a single `.db` file (e.g. `~/.graft/metadata.db`).
//! 3. **Full Compatibility**: Implements identical semantics to `SpannerMetadataStore` and
//!    `LocalMetadataStore`, including atomic Compare-And-Swap (CAS) ref updates and TTL lease enforcement.

use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use crate::metadata::{format_rfc3339_utc, MetadataStore, WorkspaceLease};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Embedded SQLite metadata and reference store.
pub struct SqliteMetadataStore {
    conn: Mutex<Connection>,
    db_path: String,
}

impl SqliteMetadataStore {
    /// Opens or creates an embedded SQLite metadata database.
    ///
    /// Accepts a file path or URL string:
    /// - `":memory:"` or `"sqlite::memory:"` for an in-memory database.
    /// - `"sqlite:///path/to/db"` or `"/path/to/db"` for a file-backed database.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path_str = path.as_ref().to_string_lossy();
        let trimmed = path_str.trim();

        let (conn, db_path) = if trimmed == ":memory:"
            || trimmed == "sqlite::memory:"
            || trimmed.is_empty()
        {
            let conn = Connection::open_in_memory()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("SQLite open in-memory failed: {}", e)))?;
            (conn, ":memory:".to_string())
        } else {
            let clean_path = if let Some(stripped) = trimmed.strip_prefix("sqlite://") {
                PathBuf::from(stripped)
            } else if let Some(stripped) = trimmed.strip_prefix("sqlite:") {
                PathBuf::from(stripped)
            } else {
                path.as_ref().to_path_buf()
            };

            if let Some(parent) = clean_path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }

            let conn = Connection::open(&clean_path)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("SQLite open '{}' failed: {}", clean_path.display(), e)))?;

            // Configure SQLite for high concurrency and resilience
            let _ = conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA busy_timeout = 5000;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;",
            );

            (conn, clean_path.to_string_lossy().to_string())
        };

        let store = Self {
            conn: Mutex::new(conn),
            db_path,
        };

        store.init_tables()?;
        Ok(store)
    }

    /// Creates an in-memory SQLite store (ideal for hermetic unit testing).
    pub fn in_memory() -> io::Result<Self> {
        Self::open(":memory:")
    }

    /// Returns the database path (or `":memory:"`).
    pub fn db_path(&self) -> &str {
        &self.db_path
    }

    fn init_tables(&self) -> io::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS refs (
                ref_name TEXT PRIMARY KEY,
                digest TEXT NOT NULL,
                author TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS commits (
                digest TEXT PRIMARY KEY,
                author TEXT NOT NULL,
                timestamp_seconds INTEGER NOT NULL,
                message TEXT NOT NULL,
                root_tree_digest TEXT NOT NULL,
                parent_commits TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS leases (
                workspace_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                base_commit TEXT NOT NULL,
                lease_expires_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_refs_name ON refs(ref_name);
            CREATE INDEX IF NOT EXISTS idx_commits_author ON commits(author);
            CREATE INDEX IF NOT EXISTS idx_leases_expires ON leases(lease_expires_at);",
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to initialize SQLite schema: {}", e)))?;
        Ok(())
    }
}

impl MetadataStore for SqliteMetadataStore {
    fn get_ref(&self, ref_name: &str) -> io::Result<Option<Digest>> {
        let conn = self.conn.lock().unwrap();

        // 1. Direct match
        let mut stmt = conn
            .prepare("SELECT digest FROM refs WHERE ref_name = ?1;")
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let res: Option<String> = stmt
            .query_row(params![ref_name], |row| row.get(0))
            .optional()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        if let Some(hex) = res {
            let digest = Digest::parse(&hex)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            return Ok(Some(digest));
        }

        // 2. Bidirectional fallback for bare / canonical branch names
        let alternative = if let Some(bare) = ref_name
            .trim_start_matches('/')
            .strip_prefix("refs/heads/")
            .or_else(|| ref_name.trim_start_matches('/').strip_prefix("refs/"))
        {
            Some(bare.to_string())
        } else if !ref_name.starts_with("refs/") {
            Some(format!("refs/heads/{}", ref_name))
        } else {
            None
        };

        if let Some(alt) = alternative {
            let alt_res: Option<String> = stmt
                .query_row(params![alt], |row| row.get(0))
                .optional()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            if let Some(hex) = alt_res {
                let digest = Digest::parse(&hex)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                return Ok(Some(digest));
            }
        }

        Ok(None)
    }

    fn compare_and_swap_ref(
        &self,
        ref_name: &str,
        expected: Option<&Digest>,
        target: &Digest,
        author: &str,
    ) -> io::Result<()> {
        let conn = self.conn.lock().unwrap();

        // Execute CAS atomically inside a transaction
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Begin transaction failed: {}", e)))?;

        let mut legacy_bare_to_cleanup: Option<String> = None;

        // Fetch current ref value
        let current_digest_hex: Option<String> = {
            let mut stmt = tx
                .prepare("SELECT digest FROM refs WHERE ref_name = ?1;")
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            let res = stmt
                .query_row(params![ref_name], |row| row.get(0))
                .optional()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            if res.is_none() {
                // Check legacy bare ref
                let stripped = ref_name
                    .trim_start_matches('/')
                    .strip_prefix("refs/heads/")
                    .or_else(|| ref_name.trim_start_matches('/').strip_prefix("refs/"));

                if let Some(bare) = stripped {
                    let bare_res: Option<String> = stmt
                        .query_row(params![bare], |row| row.get(0))
                        .optional()
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                    if bare_res.is_some() {
                        legacy_bare_to_cleanup = Some(bare.to_string());
                    }
                    bare_res
                } else {
                    None
                }
            } else {
                res
            }
        };

        let current = match current_digest_hex {
            Some(hex) => Some(
                Digest::parse(&hex)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            ),
            None => None,
        };

        // Validate CAS condition
        match (expected, current) {
            (Some(exp), Some(cur)) if exp.hash == cur.hash => {}
            (None, None) => {}
            (Some(exp), Some(cur)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "CAS conflict on '{}': expected {}, found {}",
                        ref_name, exp, cur
                    ),
                ));
            }
            (Some(exp), None) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Ref '{}' does not exist; expected {}", ref_name, exp),
                ));
            }
            (None, Some(cur)) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Ref '{}' already exists pointing to {}", ref_name, cur),
                ));
            }
        }

        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let updated_at = format_rfc3339_utc(now_epoch);

        // Update target ref
        tx.execute(
            "INSERT INTO refs (ref_name, digest, author, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(ref_name) DO UPDATE SET
                digest = excluded.digest,
                author = excluded.author,
                updated_at = excluded.updated_at;",
            params![ref_name, target.to_string(), author, updated_at],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to update ref: {}", e)))?;

        // Cleanup obsolete legacy bare ref if migrating
        if let Some(bare) = legacy_bare_to_cleanup {
            let _ = tx.execute("DELETE FROM refs WHERE ref_name = ?1;", params![bare]);
        }

        tx.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Commit failed: {}", e)))?;

        Ok(())
    }

    fn list_refs(&self, prefix: &str) -> io::Result<Vec<(String, Digest)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT ref_name, digest FROM refs ORDER BY ref_name;")
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let clean_prefix = prefix.trim_start_matches('/');
        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                let hex: String = row.get(1)?;
                Ok((name, hex))
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut results = Vec::new();
        for item in rows {
            let (name, hex) = item.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            if clean_prefix.is_empty()
                || name.starts_with(clean_prefix)
                || name.starts_with(prefix)
            {
                if let Ok(d) = Digest::parse(&hex) {
                    results.push((name, d));
                }
            }
        }
        Ok(results)
    }

    fn delete_ref(&self, ref_name: &str, expected: Option<&Digest>) -> io::Result<()> {
        let conn = self.conn.lock().unwrap();

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Begin transaction failed: {}", e)))?;

        let current_digest_hex: Option<String> = {
            let mut stmt = tx
                .prepare("SELECT digest FROM refs WHERE ref_name = ?1;")
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            stmt.query_row(params![ref_name], |row| row.get(0))
                .optional()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
        };

        if let Some(exp) = expected {
            match current_digest_hex.as_deref() {
                Some(hex) => {
                    let cur = Digest::parse(hex)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    if cur.hash != exp.hash {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("Cannot delete '{}': ref has moved to {}", ref_name, cur),
                        ));
                    }
                }
                None => return Ok(()),
            }
        }

        tx.execute("DELETE FROM refs WHERE ref_name = ?1;", params![ref_name])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let stripped = ref_name
            .trim_start_matches('/')
            .strip_prefix("refs/heads/")
            .or_else(|| ref_name.trim_start_matches('/').strip_prefix("refs/"));

        if let Some(bare) = stripped {
            let _ = tx.execute("DELETE FROM refs WHERE ref_name = ?1;", params![bare]);
        }

        tx.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Commit failed: {}", e)))?;

        Ok(())
    }

    fn put_commit_metadata(&self, commit_digest: &Digest, commit: &Commit) -> io::Result<()> {
        let conn = self.conn.lock().unwrap();

        let parents_json = serde_json::to_string(&commit.parent_commits)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        conn.execute(
            "INSERT INTO commits (digest, author, timestamp_seconds, message, root_tree_digest, parent_commits)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(digest) DO NOTHING;",
            params![
                commit_digest.to_string(),
                commit.author,
                commit.timestamp_seconds,
                commit.message,
                commit.root_tree_digest.to_string(),
                parents_json,
            ],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to put commit metadata: {}", e)))?;

        Ok(())
    }

    fn get_commit_metadata(&self, commit_digest: &Digest) -> io::Result<Option<Commit>> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT author, timestamp_seconds, message, root_tree_digest, parent_commits
                 FROM commits WHERE digest = ?1 OR digest = ?2;",
            )
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let row_opt = stmt
            .query_row(params![commit_digest.to_string(), commit_digest.to_hex()], |row| {
                let author: String = row.get(0)?;
                let ts: i64 = row.get(1)?;
                let message: String = row.get(2)?;
                let root_tree_str: String = row.get(3)?;
                let parents_json: String = row.get(4)?;
                Ok((author, ts, message, root_tree_str, parents_json))
            })
            .optional()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        match row_opt {
            Some((author, ts, message, root_tree_str, parents_json)) => {
                let root_tree_digest = Digest::parse(&root_tree_str)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let parent_commits: Vec<Digest> = serde_json::from_str(&parents_json)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                Ok(Some(Commit {
                    author,
                    timestamp_seconds: ts,
                    message,
                    root_tree_digest,
                    parent_commits,
                }))
            }
            None => Ok(None),
        }
    }

    fn acquire_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &Digest,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let conn = self.conn.lock().unwrap();

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Begin transaction failed: {}", e)))?;

        let existing: Option<(String, String, String)> = {
            let mut stmt = tx
                .prepare("SELECT agent_id, base_commit, lease_expires_at FROM leases WHERE workspace_id = ?1;")
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            stmt.query_row(params![workspace_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
        };

        if let Some((cur_agent, cur_base, cur_expires)) = existing {
            if let Ok(cur_base_digest) = Digest::parse(&cur_base) {
                let current_lease = WorkspaceLease {
                    workspace_id: workspace_id.to_string(),
                    agent_id: cur_agent.clone(),
                    base_commit: cur_base_digest,
                    lease_expires_at: cur_expires.clone(),
                };

                if current_lease.is_active() && current_lease.agent_id != agent_id {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "Workspace lease on '{}' is held by '{}' until {} ({}s remaining)",
                            workspace_id,
                            current_lease.agent_id,
                            current_lease.lease_expires_at,
                            current_lease.remaining_seconds()
                        ),
                    ));
                }
            }
        }

        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expires_at = format_rfc3339_utc(now_epoch + ttl_seconds);

        tx.execute(
            "INSERT INTO leases (workspace_id, agent_id, base_commit, lease_expires_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(workspace_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                base_commit = excluded.base_commit,
                lease_expires_at = excluded.lease_expires_at;",
            params![workspace_id, agent_id, base_commit.to_string(), expires_at],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to acquire lease: {}", e)))?;

        tx.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Commit failed: {}", e)))?;

        Ok(WorkspaceLease {
            workspace_id: workspace_id.to_string(),
            agent_id: agent_id.to_string(),
            base_commit: *base_commit,
            lease_expires_at: expires_at,
        })
    }

    fn renew_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let conn = self.conn.lock().unwrap();

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Begin transaction failed: {}", e)))?;

        let existing: Option<(String, String, String)> = {
            let mut stmt = tx
                .prepare("SELECT agent_id, base_commit, lease_expires_at FROM leases WHERE workspace_id = ?1;")
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            stmt.query_row(params![workspace_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
        };

        let (cur_agent, cur_base, _) = match existing {
            Some(vals) => vals,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Workspace lease on '{}' not found", workspace_id),
                ));
            }
        };

        if cur_agent != agent_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Cannot renew lease on '{}': held by '{}', not '{}'",
                    workspace_id, cur_agent, agent_id
                ),
            ));
        }

        let base_commit = Digest::parse(&cur_base)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expires_at = format_rfc3339_utc(now_epoch + ttl_seconds);

        tx.execute(
            "UPDATE leases SET lease_expires_at = ?1 WHERE workspace_id = ?2;",
            params![expires_at, workspace_id],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to renew lease: {}", e)))?;

        tx.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Commit failed: {}", e)))?;

        Ok(WorkspaceLease {
            workspace_id: workspace_id.to_string(),
            agent_id: agent_id.to_string(),
            base_commit,
            lease_expires_at: expires_at,
        })
    }

    fn release_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        force: bool,
    ) -> io::Result<()> {
        let conn = self.conn.lock().unwrap();

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Begin transaction failed: {}", e)))?;

        let cur_agent: Option<String> = {
            let mut stmt = tx
                .prepare("SELECT agent_id FROM leases WHERE workspace_id = ?1;")
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            stmt.query_row(params![workspace_id], |row| row.get(0))
                .optional()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
        };

        if let Some(holder) = cur_agent {
            if holder != agent_id && !force {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "Cannot release lease on '{}': held by '{}', not '{}'",
                        workspace_id, holder, agent_id
                    ),
                ));
            }
            tx.execute("DELETE FROM leases WHERE workspace_id = ?1;", params![workspace_id])
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        }

        tx.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Commit failed: {}", e)))?;

        Ok(())
    }

    fn get_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn
            .prepare("SELECT workspace_id, agent_id, base_commit, lease_expires_at FROM leases WHERE workspace_id = ?1;")
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let row_opt = stmt
            .query_row(params![workspace_id], |row| {
                let ws_id: String = row.get(0)?;
                let ag_id: String = row.get(1)?;
                let base_hex: String = row.get(2)?;
                let expires: String = row.get(3)?;
                Ok((ws_id, ag_id, base_hex, expires))
            })
            .optional()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        match row_opt {
            Some((ws_id, ag_id, base_hex, expires)) => {
                let base_commit = Digest::parse(&base_hex)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Some(WorkspaceLease {
                    workspace_id: ws_id,
                    agent_id: ag_id,
                    base_commit,
                    lease_expires_at: expires,
                }))
            }
            None => Ok(None),
        }
    }

    fn list_leases(&self) -> io::Result<Vec<WorkspaceLease>> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn
            .prepare("SELECT workspace_id, agent_id, base_commit, lease_expires_at FROM leases ORDER BY workspace_id;")
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let rows = stmt
            .query_map([], |row| {
                let ws_id: String = row.get(0)?;
                let ag_id: String = row.get(1)?;
                let base_hex: String = row.get(2)?;
                let expires: String = row.get(3)?;
                Ok((ws_id, ag_id, base_hex, expires))
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut results = Vec::new();
        for item in rows {
            let (ws_id, ag_id, base_hex, expires) =
                item.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            if let Ok(base_commit) = Digest::parse(&base_hex) {
                results.push(WorkspaceLease {
                    workspace_id: ws_id,
                    agent_id: ag_id,
                    base_commit,
                    lease_expires_at: expires,
                });
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_in_memory_refs_cas() {
        let store = SqliteMetadataStore::in_memory().unwrap();
        let d1 = Digest::from_bytes(b"commit 1");
        let d2 = Digest::from_bytes(b"commit 2");
        let d3 = Digest::from_bytes(b"commit 3");

        // Initial creation requires expected: None
        store
            .compare_and_swap_ref("refs/heads/main", None, &d1, "agent-1")
            .unwrap();
        assert_eq!(store.get_ref("refs/heads/main").unwrap(), Some(d1));
        // Fallback bare ref lookup
        assert_eq!(store.get_ref("main").unwrap(), Some(d1));

        // Attempting to create again with None should fail
        let err = store
            .compare_and_swap_ref("refs/heads/main", None, &d2, "agent-2")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        // Updating with wrong expected should fail
        let err = store
            .compare_and_swap_ref("refs/heads/main", Some(&d3), &d2, "agent-2")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        // Updating with correct expected should succeed
        store
            .compare_and_swap_ref("refs/heads/main", Some(&d1), &d2, "agent-2")
            .unwrap();
        assert_eq!(store.get_ref("refs/heads/main").unwrap(), Some(d2));

        // Listing refs
        store
            .compare_and_swap_ref("refs/heads/feature-a", None, &d3, "agent-1")
            .unwrap();
        let list = store.list_refs("refs/heads/").unwrap();
        assert_eq!(list.len(), 2);

        // Deleting ref with wrong expected fails
        let err = store
            .delete_ref("refs/heads/main", Some(&d1))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        // Deleting ref with correct expected succeeds
        store.delete_ref("refs/heads/main", Some(&d2)).unwrap();
        assert_eq!(store.get_ref("refs/heads/main").unwrap(), None);

        // Deleting non-existent ref returns Ok(())
        assert!(store.delete_ref("refs/heads/nonexistent", None).is_ok());
    }

    #[test]
    fn test_sqlite_commit_metadata_roundtrip() {
        let store = SqliteMetadataStore::in_memory().unwrap();
        let tree_digest = Digest::from_bytes(b"root-tree");
        let p1 = Digest::from_bytes(b"parent 1");
        let p2 = Digest::from_bytes(b"parent 2");

        let commit = Commit {
            author: "Graft Agent <agent@graft.dev>".to_string(),
            timestamp_seconds: 1730000000,
            message: "Initial commit".to_string(),
            root_tree_digest: tree_digest,
            parent_commits: vec![p1, p2],
        };

        let commit_digest = Digest::from_bytes(&commit.serialize());
        store.put_commit_metadata(&commit_digest, &commit).unwrap();

        let retrieved = store.get_commit_metadata(&commit_digest).unwrap().unwrap();
        assert_eq!(retrieved.author, commit.author);
        assert_eq!(retrieved.timestamp_seconds, commit.timestamp_seconds);
        assert_eq!(retrieved.message, commit.message);
        assert_eq!(retrieved.root_tree_digest, commit.root_tree_digest);
        assert_eq!(retrieved.parent_commits, commit.parent_commits);
    }

    #[test]
    fn test_sqlite_leases_lifecycle() {
        let store = SqliteMetadataStore::in_memory().unwrap();
        let base_commit = Digest::from_bytes(b"base");

        // 1. Acquire lease
        let lease = store
            .acquire_lease("ws-01", "agent-alpha", &base_commit, 300)
            .unwrap();
        assert_eq!(lease.workspace_id, "ws-01");
        assert_eq!(lease.agent_id, "agent-alpha");
        assert!(lease.is_active());

        // 2. Another agent cannot acquire active lease
        let err = store
            .acquire_lease("ws-01", "agent-beta", &base_commit, 300)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        // 3. Same agent can re-acquire / extend
        let reacquired = store
            .acquire_lease("ws-01", "agent-alpha", &base_commit, 600)
            .unwrap();
        assert_eq!(reacquired.agent_id, "agent-alpha");

        // 4. Renew lease
        let renewed = store.renew_lease("ws-01", "agent-alpha", 1200).unwrap();
        assert!(renewed.remaining_seconds() > 500);

        // 5. Wrong agent cannot renew
        let err = store.renew_lease("ws-01", "agent-beta", 1200).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        // 6. Wrong agent cannot release without force
        let err = store.release_lease("ws-01", "agent-beta", false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        // 7. Force release succeeds
        store.release_lease("ws-01", "agent-beta", true).unwrap();
        assert_eq!(store.get_lease("ws-01").unwrap(), None);

        // 8. Expired lease can be acquired by another agent
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO leases (workspace_id, agent_id, base_commit, lease_expires_at)
                 VALUES ('ws-expired', 'agent-old', '0000000000000000000000000000000000000000000000000000000000000000:0', '2020-01-01T00:00:00Z');",
                [],
            ).unwrap();
        }
        let stolen = store.acquire_lease("ws-expired", "agent-new", &base_commit, 300).unwrap();
        assert_eq!(stolen.agent_id, "agent-new");
        assert_eq!(store.list_leases().unwrap().len(), 1);
    }

    #[test]
    fn test_sqlite_file_backed_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let db_file = tmp.path().join("sub").join("meta.db");
        let d = Digest::from_bytes(b"persisted commit");

        {
            let store = SqliteMetadataStore::open(&db_file).unwrap();
            store
                .compare_and_swap_ref("refs/heads/main", None, &d, "author")
                .unwrap();
        }

        // Reopen database from same file path
        {
            let store = SqliteMetadataStore::open(&db_file).unwrap();
            let retrieved = store.get_ref("refs/heads/main").unwrap();
            assert_eq!(retrieved, Some(d));
        }
    }
}
