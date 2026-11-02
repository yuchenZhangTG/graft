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

//! # Local Filesystem Metadata & Reference Store
//!
//! Provides a local implementation of [`MetadataStore`] for development and offline testing.
//! Uses atomic filesystem operations for atomic branch updates.

use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use crate::metadata::{format_rfc3339_utc, MetadataStore, WorkspaceLease};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Filesystem-backed reference and metadata store.
pub struct LocalMetadataStore {
    root_dir: PathBuf,
    lock: Mutex<()>,
}

impl LocalMetadataStore {
    /// Creates or opens a local metadata store.
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let root_dir = path.as_ref().to_path_buf();
        fs::create_dir_all(root_dir.join("refs"))?;
        fs::create_dir_all(root_dir.join("commits"))?;
        fs::create_dir_all(root_dir.join("leases"))?;
        Ok(Self {
            root_dir,
            lock: Mutex::new(()),
        })
    }

    fn ref_path(&self, ref_name: &str) -> PathBuf {
        let clean = ref_name.trim_start_matches('/');
        self.root_dir.join("refs").join(clean)
    }

    fn lease_path(&self, workspace_id: &str) -> PathBuf {
        self.root_dir.join("leases").join(format!("{}.json", workspace_id))
    }

    fn commit_meta_path(&self, digest: &Digest) -> PathBuf {
        self.root_dir
            .join("commits")
            .join(digest.shard_prefix())
            .join(format!("{}.json", digest.shard_suffix()))
    }
}

impl MetadataStore for LocalMetadataStore {
    fn get_ref(&self, ref_name: &str) -> io::Result<Option<Digest>> {
        let path = self.ref_path(ref_name);
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(path)?;
        let digest = Digest::parse(&content)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(digest))
    }

    fn compare_and_swap_ref(
        &self,
        ref_name: &str,
        expected: Option<&Digest>,
        target: &Digest,
        _author: &str,
    ) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap();
        let path = self.ref_path(ref_name);

        let mut current = self.get_ref(ref_name)?;
        let mut legacy_bare_to_cleanup: Option<PathBuf> = None;

        // If not found under full ref, check fallback bare ref (e.g. "refs/heads/main" -> "main")
        if current.is_none() {
            let stripped = ref_name
                .trim_start_matches('/')
                .strip_prefix("refs/heads/")
                .or_else(|| ref_name.trim_start_matches('/').strip_prefix("refs/"));
            if let Some(bare) = stripped {
                let bare_path = self.ref_path(bare);
                if bare_path.exists() && bare_path.is_file() {
                    let content = fs::read_to_string(&bare_path)?;
                    if let Ok(d) = Digest::parse(&content) {
                        current = Some(d);
                        legacy_bare_to_cleanup = Some(bare_path);
                    }
                }
            }
        }

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

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp = path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            writeln!(f, "{}", target)?;
            f.flush()?;
        }
        fs::rename(tmp, &path)?;

        // If migrating from a legacy bare ref, clean up the old bare file
        if let Some(bare_path) = legacy_bare_to_cleanup {
            if bare_path.exists() && bare_path != path {
                let _ = fs::remove_file(bare_path);
            }
        }

        Ok(())
    }

    fn list_refs(&self, prefix: &str) -> io::Result<Vec<(String, Digest)>> {
        let refs_root = self.root_dir.join("refs");
        let mut results = Vec::new();
        if !refs_root.exists() {
            return Ok(results);
        }

        fn walk(
            dir: &Path,
            base: &Path,
            prefix: &str,
            results: &mut Vec<(String, Digest)>,
        ) -> io::Result<()> {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, prefix, results)?;
                } else if path.is_file() {
                    let rel = path
                        .strip_prefix(base)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                    let name = rel.to_string_lossy().to_string();
                    if name.starts_with(prefix) {
                        let content = fs::read_to_string(&path)?;
                        if let Ok(d) = Digest::parse(&content) {
                            results.push((name, d));
                        }
                    }
                }
            }
            Ok(())
        }

        walk(&refs_root, &refs_root, prefix.trim_start_matches('/'), &mut results)?;
        Ok(results)
    }

    fn delete_ref(&self, ref_name: &str, expected: Option<&Digest>) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap();
        let path = self.ref_path(ref_name);
        let current = self.get_ref(ref_name)?;

        if let Some(exp) = expected {
            match current {
                Some(cur) if cur.hash == exp.hash => {}
                Some(cur) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("Cannot delete '{}': ref has moved to {}", ref_name, cur),
                    ));
                }
                None => return Ok(()),
            }
        }

        if path.exists() {
            fs::remove_file(&path)?;
        }

        let stripped = ref_name
            .trim_start_matches('/')
            .strip_prefix("refs/heads/")
            .or_else(|| ref_name.trim_start_matches('/').strip_prefix("refs/"));
        if let Some(bare) = stripped {
            let bare_path = self.ref_path(bare);
            if bare_path.exists() && bare_path.is_file() {
                let _ = fs::remove_file(bare_path);
            }
        }

        Ok(())
    }

    fn put_commit_metadata(&self, commit_digest: &Digest, commit: &Commit) -> io::Result<()> {
        let path = self.commit_meta_path(commit_digest);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized = commit.serialize();
        let tmp = path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&serialized)?;
            f.flush()?;
        }
        fs::rename(tmp, path)?;
        Ok(())
    }

    fn get_commit_metadata(&self, commit_digest: &Digest) -> io::Result<Option<Commit>> {
        let path = self.commit_meta_path(commit_digest);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        let commit = Commit::deserialize(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(commit))
    }

    fn acquire_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        base_commit: &Digest,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let _guard = self.lock.lock().unwrap();
        let path = self.lease_path(workspace_id);

        if path.exists() {
            let content = fs::read_to_string(&path)?;
            if let Ok(current) = serde_json::from_str::<WorkspaceLease>(&content) {
                if current.is_active() && current.agent_id != agent_id {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "Workspace lease on '{}' is held by '{}' until {} ({}s remaining)",
                            workspace_id,
                            current.agent_id,
                            current.lease_expires_at,
                            current.remaining_seconds()
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

        let lease = WorkspaceLease {
            workspace_id: workspace_id.to_string(),
            agent_id: agent_id.to_string(),
            base_commit: *base_commit,
            lease_expires_at: expires_at,
        };

        let json = serde_json::to_string_pretty(&lease)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.flush()?;
        }
        fs::rename(tmp, path)?;
        Ok(lease)
    }

    fn renew_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        ttl_seconds: u64,
    ) -> io::Result<WorkspaceLease> {
        let _guard = self.lock.lock().unwrap();
        let path = self.lease_path(workspace_id);

        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Cannot renew lease: workspace '{}' has no active lease", workspace_id),
            ));
        }

        let content = fs::read_to_string(&path)?;
        let mut lease: WorkspaceLease = serde_json::from_str(&content)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if lease.agent_id != agent_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Cannot renew lease for '{}': lease is held by '{}', not '{}'",
                    workspace_id, lease.agent_id, agent_id
                ),
            ));
        }

        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        lease.lease_expires_at = format_rfc3339_utc(now_epoch + ttl_seconds);

        let json = serde_json::to_string_pretty(&lease)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.flush()?;
        }
        fs::rename(tmp, path)?;
        Ok(lease)
    }

    fn release_lease(
        &self,
        workspace_id: &str,
        agent_id: &str,
        force: bool,
    ) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap();
        let path = self.lease_path(workspace_id);

        if !path.exists() {
            return Ok(());
        }

        if !force {
            let content = fs::read_to_string(&path)?;
            if let Ok(lease) = serde_json::from_str::<WorkspaceLease>(&content) {
                if lease.agent_id != agent_id {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "Cannot release lease for '{}': held by '{}', not '{}'",
                            workspace_id, lease.agent_id, agent_id
                        ),
                    ));
                }
            }
        }

        let _ = fs::remove_file(path);
        Ok(())
    }

    fn get_lease(&self, workspace_id: &str) -> io::Result<Option<WorkspaceLease>> {
        let path = self.lease_path(workspace_id);
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(path)?;
        let lease = serde_json::from_str::<WorkspaceLease>(&content)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(lease))
    }

    fn list_leases(&self) -> io::Result<Vec<WorkspaceLease>> {
        let leases_dir = self.root_dir.join("leases");
        let mut list = Vec::new();
        if !leases_dir.exists() {
            return Ok(list);
        }
        for entry in fs::read_dir(leases_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(lease) = serde_json::from_str::<WorkspaceLease>(&content) {
                        list.push(lease);
                    }
                }
            }
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_compare_and_swap_refs() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalMetadataStore::new(tmp.path())?;

        let c1 = Digest::from_bytes(b"commit 1");
        let c2 = Digest::from_bytes(b"commit 2");
        let c3 = Digest::from_bytes(b"commit 3");

        // 1. Initial creation (expected = None)
        store.compare_and_swap_ref("refs/heads/main", None, &c1, "lead@company.com")?;
        assert_eq!(store.get_ref("refs/heads/main")?, Some(c1));

        // 2. Conflicting create should fail (already exists)
        assert!(store
            .compare_and_swap_ref("refs/heads/main", None, &c2, "agent@graft.dev")
            .is_err());

        // 3. Stale expected update should fail
        assert!(store
            .compare_and_swap_ref("refs/heads/main", Some(&c3), &c2, "agent@graft.dev")
            .is_err());

        // 4. Valid CAS update: c1 -> c2
        store.compare_and_swap_ref("refs/heads/main", Some(&c1), &c2, "agent@graft.dev")?;
        assert_eq!(store.get_ref("refs/heads/main")?, Some(c2));

        // 5. List refs
        let refs = store.list_refs("refs/heads")?;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "refs/heads/main");

        // 6. Delete ref
        store.delete_ref("refs/heads/main", Some(&c2))?;
        assert_eq!(store.get_ref("refs/heads/main")?, None);

        Ok(())
    }

    #[test]
    fn test_local_workspace_leases() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalMetadataStore::new(tmp.path())?;

        let base = Digest::from_bytes(b"base-commit");

        // 1. Agent 1 acquires lease for 60s
        let l1 = store.acquire_lease("ws-1", "agent-1", &base, 60)?;
        assert_eq!(l1.workspace_id, "ws-1");
        assert_eq!(l1.agent_id, "agent-1");
        assert!(l1.is_active());

        // 2. Agent 2 tries to acquire same lease -> conflict error
        let conflict = store.acquire_lease("ws-1", "agent-2", &base, 60);
        assert!(conflict.is_err());
        assert_eq!(conflict.unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        // 3. Agent 1 re-acquires / heartbeats -> succeeds
        let renewed = store.renew_lease("ws-1", "agent-1", 120)?;
        assert!(renewed.is_active());

        // 4. Agent 2 cannot release Agent 1's lease
        let release_err = store.release_lease("ws-1", "agent-2", false);
        assert!(release_err.is_err());

        // 5. Agent 1 releases lease
        store.release_lease("ws-1", "agent-1", false)?;
        assert!(store.get_lease("ws-1")?.is_none());

        // 6. Now Agent 2 can acquire
        let l2 = store.acquire_lease("ws-1", "agent-2", &base, 60)?;
        assert_eq!(l2.agent_id, "agent-2");

        Ok(())
    }
}
