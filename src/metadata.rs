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

//! # Metadata Subsystem: References, Commit Graph Index, & Spanner Coordination
//!
//! While raw file payloads and tree manifests live in Content-Addressed Storage (GCS / NVMe),
//! high-concurrency multi-agent collaboration requires an ACID relational store for:
//! 1. **Branch References**: Atomic Compare-And-Swap (CAS) updates to branch heads (e.g. `refs/heads/main`).
//! 2. **Commit Indexing**: High-speed relational queries over author, timestamp, and ancestry DAGs.
//! 3. **Agent Workspace Leases**: Distributed lock leases preventing two agents from claiming
//!    the same branch or mutating the same task workspace concurrently.

pub mod local;
pub mod spanner;

pub use local::LocalMetadataStore;
pub use spanner::{SpannerConfig, SpannerMetadataStore};

use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use std::io;

/// Formats a UNIX timestamp (seconds) into a UTC RFC 3339 string ending in 'Z'.
pub fn format_rfc3339_utc(epoch_seconds: u64) -> String {
    let days = (epoch_seconds / 86400) as i64;
    let rem_secs = (epoch_seconds % 86400) as u32;
    let hours = rem_secs / 3600;
    let minutes = (rem_secs % 3600) / 60;
    let seconds = rem_secs % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 142401) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hours, minutes, seconds)
}

/// Parses an RFC 3339 string `YYYY-MM-DDTHH:MM:SS[.fraction]Z` into UNIX timestamp (seconds).
pub fn parse_rfc3339_utc(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.len() < 19 {
        return Err(format!("RFC 3339 string too short: '{}'", s));
    }
    let parts: Vec<&str> = trimmed.split('T').collect();
    if parts.len() != 2 {
        return Err(format!("Missing 'T' separator in '{}'", s));
    }
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    if date_parts.len() != 3 {
        return Err(format!("Invalid date part in '{}'", s));
    }
    let y: i64 = date_parts[0].parse().map_err(|e| format!("{}", e))?;
    let m: u32 = date_parts[1].parse().map_err(|e| format!("{}", e))?;
    let d: u32 = date_parts[2].parse().map_err(|e| format!("{}", e))?;

    let time_str = parts[1].trim_end_matches('Z');
    let time_core = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_core.split(':').collect();
    if time_parts.len() != 3 {
        return Err(format!("Invalid time part in '{}'", s));
    }
    let hour: u64 = time_parts[0].parse().map_err(|e| format!("{}", e))?;
    let min: u64 = time_parts[1].parse().map_err(|e| format!("{}", e))?;
    let sec: u64 = time_parts[2].parse().map_err(|e| format!("{}", e))?;

    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;

    if days < 0 {
        return Err("Timestamp before UNIX epoch".to_string());
    }
    Ok((days as u64) * 86400 + hour * 3600 + min * 60 + sec)
}

/// Normalizes a user branch or tag name into a canonical ref path.
///
/// If `name` already starts with `"refs/"`, it is returned unchanged.
/// Otherwise, it is prefixed with `"refs/heads/"`.
pub fn normalize_ref_name(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_string()
    } else {
        format!("refs/heads/{}", name)
    }
}

/// Validates a branch name according to Git and Graft naming rules.
///
/// Disallows empty names, whitespace, invalid characters (`..`, `~`, `^`, `:`, `?`, `*`, `[`),
/// leading/trailing slashes, and raw 64-character hex strings (to avoid digest ambiguity).
pub fn validate_branch_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Branch name cannot be empty".to_string());
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(format!("Branch name '{}' cannot contain whitespace", name));
    }
    if trimmed.starts_with('/') || trimmed.ends_with('/') {
        return Err(format!("Branch name '{}' cannot start or end with '/'", name));
    }
    if trimmed.contains("..") || trimmed.contains("//") {
        return Err(format!("Branch name '{}' cannot contain '..' or consecutive slashes", name));
    }
    for ch in ['~', '^', ':', '?', '*', '['] {
        if trimmed.contains(ch) {
            return Err(format!("Branch name '{}' contains invalid character '{}'", name, ch));
        }
    }
    // Reject 64-char hex strings to prevent collision with commit hashes
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Branch name '{}' is ambiguous with a 64-character commit digest",
            name
        ));
    }
    Ok(())
}

/// Resolves a reference or branch name against a metadata store, returning `(canonical_ref, commit_digest)`.
///
/// Supports automatic prefixing (`feature-1` -> `refs/heads/feature-1`) while falling back to
/// legacy bare names (e.g. `"main"`) for backward compatibility.
pub fn resolve_ref(store: &dyn MetadataStore, name: &str) -> io::Result<Option<(String, Digest)>> {
    if name.starts_with("refs/") {
        if let Some(d) = store.get_ref(name)? {
            return Ok(Some((name.to_string(), d)));
        }
    } else {
        let normalized = format!("refs/heads/{}", name);
        if let Some(d) = store.get_ref(&normalized)? {
            return Ok(Some((normalized, d)));
        }
        // Backward compatibility fallback (e.g. for bare "main" in Spanner):
        if let Some(d) = store.get_ref(name)? {
            return Ok(Some((name.to_string(), d)));
        }
    }
    Ok(None)
}

/// Distributed workspace lease held by an AI agent on a specific task branch or workspace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceLease {
    /// Workspace identifier (e.g. "default", "task-1234").
    pub workspace_id: String,
    /// Identifier of the agent holding the lease (e.g. "agent-007", "user@host").
    pub agent_id: String,
    /// Base commit digest this workspace is branched from.
    pub base_commit: Digest,
    /// Lease expiration timestamp in UTC RFC 3339 format (e.g. "2026-10-30T07:15:00Z").
    pub lease_expires_at: String,
}

impl WorkspaceLease {
    /// Checks whether the lease is currently active relative to current system time.
    pub fn is_active(&self) -> bool {
        self.remaining_seconds() > 0
    }

    /// Computes remaining lease time in seconds, or 0 if expired.
    pub fn remaining_seconds(&self) -> i64 {
        if let Ok(expires) = parse_rfc3339_utc(&self.lease_expires_at) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if expires > now {
                (expires - now) as i64
            } else {
                0
            }
        } else {
            0
        }
    }
}

/// Core interface for reference management and commit metadata indexing.
pub trait MetadataStore: Send + Sync {
    /// Retrieves the target commit digest for a reference name (e.g. "refs/heads/main").
    fn get_ref(&self, ref_name: &str) -> io::Result<Option<Digest>>;

    /// Atomically updates a reference using Compare-And-Swap (CAS).
    fn compare_and_swap_ref(
        &self,
        ref_name: &str,
        expected: Option<&Digest>,
        target: &Digest,
        author: &str,
    ) -> io::Result<()>;

    /// Lists all references matching a prefix (e.g. "refs/heads/").
    fn list_refs(&self, prefix: &str) -> io::Result<Vec<(String, Digest)>>;

    /// Deletes a reference if it matches expected digest.
    fn delete_ref(&self, ref_name: &str, expected: Option<&Digest>) -> io::Result<()>;

    /// Records commit metadata and parent relationships in the relational index.
    fn put_commit_metadata(&self, commit_digest: &Digest, commit: &Commit) -> io::Result<()>;

    /// Retrieves commit metadata by digest from the relational index.
    fn get_commit_metadata(&self, commit_digest: &Digest) -> io::Result<Option<Commit>>;

    /// Acquires an exclusive lease on a workspace ID.
    ///
    /// - If no lease exists, or if current lease is expired, or if held by the same `agent_id`,
    ///   the lease is granted/extended for `ttl_seconds`.
    /// - If held by a different agent and active, returns `io::ErrorKind::AlreadyExists`.
    fn acquire_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &Digest,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease>;

    /// Renews an existing lease held by `agent_id`.
    ///
    /// Fails with `io::ErrorKind::PermissionDenied` if held by another agent or not found.
    fn renew_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease>;

    /// Releases a workspace lease.
    ///
    /// Unless `force` is true, verifies that `agent_id` is the current lease holder.
    fn release_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        force: bool,
    ) -> io::Result<()>;

    /// Retrieves current lease metadata for a workspace ID, if any.
    fn get_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>>;

    /// Lists all workspace leases.
    fn list_leases(&self) -> io::Result<Vec<WorkspaceLease>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_ref_name() {
        assert_eq!(normalize_ref_name("main"), "refs/heads/main");
        assert_eq!(normalize_ref_name("feature-auth"), "refs/heads/feature-auth");
        assert_eq!(normalize_ref_name("refs/heads/feature-auth"), "refs/heads/feature-auth");
        assert_eq!(normalize_ref_name("refs/tags/v1.0.0"), "refs/tags/v1.0.0");
    }

    #[test]
    fn test_validate_branch_name() {
        assert!(validate_branch_name("main").is_ok());
        assert!(validate_branch_name("feature-123").is_ok());
        assert!(validate_branch_name("feat/auth_system").is_ok());

        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("feat name").is_err());
        assert!(validate_branch_name("/leading").is_err());
        assert!(validate_branch_name("trailing/").is_err());
        assert!(validate_branch_name("feat..name").is_err());
        assert!(validate_branch_name("feat~1").is_err());
        assert!(validate_branch_name("feat^2").is_err());
        assert!(validate_branch_name("feat:name").is_err());

        // 64-char hex strings are rejected to avoid ambiguity with commit digests
        let hex_64 = "a".repeat(64);
        assert!(validate_branch_name(&hex_64).is_err());
    }

    #[test]
    fn test_resolve_ref_with_fallback() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = LocalMetadataStore::new(tmp.path())?;
        let dummy_digest = Digest::from_bytes(b"dummy");

        // Set canonical ref
        store.compare_and_swap_ref("refs/heads/feature", None, &dummy_digest, "dev")?;

        // Resolve by short name
        let (r, d) = resolve_ref(&store, "feature")?.expect("should resolve feature");
        assert_eq!(r, "refs/heads/feature");
        assert_eq!(d, dummy_digest);

        // Resolve by full ref
        let (r, d) = resolve_ref(&store, "refs/heads/feature")?.expect("should resolve refs/heads/feature");
        assert_eq!(r, "refs/heads/feature");
        assert_eq!(d, dummy_digest);

        // Test fallback for bare "main"
        let main_digest = Digest::from_bytes(b"main_commit");
        store.compare_and_swap_ref("main", None, &main_digest, "dev")?;
        let (r, d) = resolve_ref(&store, "main")?.expect("should resolve bare main fallback");
        assert_eq!(r, "main");
        assert_eq!(d, main_digest);

        Ok(())
    }
}

impl<M: MetadataStore + ?Sized> MetadataStore for std::sync::Arc<M> {
    fn get_ref(&self, ref_name: &str) -> io::Result<Option<Digest>> {
        (**self).get_ref(ref_name)
    }

    fn compare_and_swap_ref(
        &self,
        ref_name: &str,
        expected: Option<&Digest>,
        target: &Digest,
        author: &str,
    ) -> io::Result<()> {
        (**self).compare_and_swap_ref(ref_name, expected, target, author)
    }

    fn list_refs(&self, prefix: &str) -> io::Result<Vec<(String, Digest)>> {
        (**self).list_refs(prefix)
    }

    fn delete_ref(&self, ref_name: &str, expected: Option<&Digest>) -> io::Result<()> {
        (**self).delete_ref(ref_name, expected)
    }

    fn put_commit_metadata(&self, commit_digest: &Digest, commit: &Commit) -> io::Result<()> {
        (**self).put_commit_metadata(commit_digest, commit)
    }

    fn get_commit_metadata(&self, commit_digest: &Digest) -> io::Result<Option<Commit>> {
        (**self).get_commit_metadata(commit_digest)
    }

    fn acquire_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &Digest,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        (**self).acquire_lease(workspace_id, agent_id, base_commit, ttl_seconds)
    }

    fn renew_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        (**self).renew_lease(workspace_id, agent_id, ttl_seconds)
    }

    fn release_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        force: bool,
    ) -> io::Result<()> {
        (**self).release_lease(workspace_id, agent_id, force)
    }

    fn get_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>> {
        (**self).get_lease(workspace_id)
    }

    fn list_leases(&self) -> io::Result<Vec<WorkspaceLease>> {
        (**self).list_leases()
    }
}
