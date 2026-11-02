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

//! # Mark-and-Sweep Garbage Collection Subsystem
//!
//! Provides automated and manual storage reclamation for Graft's Content-Addressed Storage (CAS).
//!
//! ### Core Design & Safety Guarantees:
//! 1. **Complete Root Set**: Gathers live roots across four authoritative sources:
//!    - Active branch and tag references in `MetadataStore` (`refs/heads/*`, `refs/tags/*`, `main`).
//!    - Distributed workspace leases in `MetadataStore` (`list_leases()`), protecting remote VMs.
//!    - Active local workspaces in `WorkspaceManager` (`list_workspaces()`).
//!    - Explicit user-pinned roots via `--keep-root`.
//! 2. **Typed DAG Traversal**: Traverses Merkle containers (`Commit`, `DirectoryManifest`, `ChunkManifest`)
//!    without reading or downloading leaf `File` or data-chunk payloads. Leaf digests are marked directly,
//!    ensuring $O(\text{manifests})$ runtime rather than reading gigabytes of file bytes.
//! 3. **Raw `[u8; 32]` Hash Keying**: Reachable digests are tracked by their 32-byte cryptographic hash,
//!    preventing false deletions if a ref or manifest specifies `size_bytes == 0`.
//! 4. **Fail-Closed Protection**: If *any* reachable commit or manifest is missing or corrupt, GC aborts
//!    immediately with zero deletions.
//! 5. **Empty Root Guard**: Refuses to sweep if zero roots are found while CAS contains data (preventing
//!    accidental bucket wipes if `--spanner-database` was forgotten), unless `--allow-empty-roots` is set.
//! 6. **Grace Period Safety Window**: Blobs modified within the grace period (default: 24 hours) are
//!    preserved to prevent races with concurrent uncommitted workspaces.

use crate::cas::blob_store::{BlobInfo, BlobStore};
use crate::cas::chunker::ChunkManifest;
use crate::cas::commit::{Commit, DirectoryManifest, ManifestEntry};
use crate::cas::digest::Digest;
use crate::metadata::MetadataStore;
use crate::workspace::WorkspaceManager;
use std::collections::{HashSet, VecDeque};
use std::io;
use std::time::{Duration, Instant, SystemTime};

/// Configuration options for a garbage collection run.
#[derive(Debug, Clone)]
pub struct GcOptions {
    /// Grace period duration: unreferenced blobs modified within this window are preserved.
    /// Default is 24 hours.
    pub grace_period: Duration,
    /// If true, performs a preview analysis without deleting any blobs from disk or cloud.
    pub dry_run: bool,
    /// If true and a remote store is configured, also sweeps unreferenced blobs from remote storage.
    pub prune_remote: bool,
    /// Reference system time for calculating grace period expiration (defaults to SystemTime::now()).
    pub now: SystemTime,
    /// Safety override: allow garbage collection even if zero live roots were found.
    pub allow_empty_roots: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            grace_period: Duration::from_secs(24 * 3600),
            dry_run: false,
            prune_remote: false,
            now: SystemTime::now(),
            allow_empty_roots: false,
        }
    }
}

/// Statistics and accounting summary returned after a garbage collection run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcStats {
    /// Total number of distinct root references discovered.
    pub roots_count: usize,
    /// Total number of live, reachable blobs retained.
    pub live_blobs: usize,
    /// Total byte size of live, reachable blobs.
    pub live_bytes: u64,
    /// Number of unreferenced blobs deleted (or marked for deletion in dry-run).
    pub garbage_blobs: usize,
    /// Total byte size reclaimed (or reclaimable in dry-run).
    pub garbage_bytes: u64,
    /// Number of unreferenced blobs preserved because their modification time is within the grace period.
    pub skipped_recent_blobs: usize,
    /// Byte size of blobs protected by the grace period.
    pub skipped_recent_bytes: u64,
    /// Number of remote blobs deleted (when `--prune-remote` is active).
    pub remote_deleted_blobs: usize,
    /// Total wall-clock execution time.
    pub elapsed: Duration,
}

/// Discovers live root digests from all four authoritative system sources.
pub fn collect_roots(
    metadata_store: &dyn MetadataStore,
    workspace_manager: Option<&WorkspaceManager>,
    extra_roots: &[Digest],
) -> io::Result<Vec<Digest>> {
    let mut root_set: HashSet<Digest> = HashSet::new();

    // 1. References from MetadataStore (branches, tags, bare refs)
    let all_refs = metadata_store.list_refs("")?;
    for (_ref_name, digest) in all_refs {
        root_set.insert(digest);
    }

    // 2. Active distributed workspace leases from MetadataStore
    let leases = metadata_store.list_leases()?;
    for lease in leases {
        root_set.insert(lease.base_commit);
    }

    // 3. Local workspace configurations
    if let Some(wm) = workspace_manager {
        let workspaces = wm.list_workspaces()?;
        for ws in workspaces {
            if let Some(base_commit_str) = &ws.base_commit {
                if let Ok(d) = Digest::parse(base_commit_str) {
                    root_set.insert(d);
                }
            }
            if let Ok(d) = Digest::parse(&ws.commit) {
                root_set.insert(d);
            } else if let Ok(Some((_, d))) = crate::metadata::resolve_ref(metadata_store, &ws.commit) {
                root_set.insert(d);
            }
        }
    }

    // 4. Explicit user-provided roots
    for &d in extra_roots {
        root_set.insert(d);
    }

    let mut roots: Vec<Digest> = root_set.into_iter().collect();
    roots.sort_by_key(|d| d.hash);
    Ok(roots)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerKind {
    UnknownRoot(Digest),
    Commit(Digest),
    Directory(Digest),
    ChunkManifest(Digest),
}

/// Performs a typed Merkle DAG traversal starting from roots, returning the set of all reachable 32-byte BLAKE3 hashes.
///
/// Fails closed with an error if any reachable container is missing or corrupt.
pub fn mark_reachable<B: BlobStore + ?Sized>(
    roots: &[Digest],
    store: &B,
) -> io::Result<HashSet<[u8; 32]>> {
    let mut reachable_hashes: HashSet<[u8; 32]> = HashSet::new();
    let mut visited_containers: HashSet<[u8; 32]> = HashSet::new();
    let mut queue: VecDeque<ContainerKind> = VecDeque::new();

    for &root in roots {
        reachable_hashes.insert(root.hash);
        queue.push_back(ContainerKind::UnknownRoot(root));
    }

    while let Some(item) = queue.pop_front() {
        match item {
            ContainerKind::UnknownRoot(digest) => {
                if visited_containers.contains(&digest.hash) {
                    continue;
                }
                visited_containers.insert(digest.hash);
                reachable_hashes.insert(digest.hash);

                let bytes = store.get(&digest)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Root object {} not found in CAS", digest),
                    )
                })?;

                // Try parsing as Commit first
                if let Ok(commit) = Commit::deserialize(&bytes) {
                    reachable_hashes.insert(commit.root_tree_digest.hash);
                    queue.push_back(ContainerKind::Directory(commit.root_tree_digest));
                    for parent in commit.parent_commits {
                        reachable_hashes.insert(parent.hash);
                        queue.push_back(ContainerKind::Commit(parent));
                    }
                } else if let Ok(manifest) = DirectoryManifest::from_bytes(&bytes) {
                    // Root is directly a DirectoryManifest
                    for entry in manifest.entries.values() {
                        match entry {
                            ManifestEntry::File { digest, .. } => {
                                reachable_hashes.insert(digest.hash);
                            }
                            ManifestEntry::ChunkedFile {
                                manifest_digest, ..
                            } => {
                                reachable_hashes.insert(manifest_digest.hash);
                                queue.push_back(ContainerKind::ChunkManifest(*manifest_digest));
                            }
                            ManifestEntry::Directory { digest, .. } => {
                                reachable_hashes.insert(digest.hash);
                                queue.push_back(ContainerKind::Directory(*digest));
                            }
                        }
                    }
                } else {
                    // Leaf data blob, nothing further to traverse
                }
            }

            ContainerKind::Commit(digest) => {
                if visited_containers.contains(&digest.hash) {
                    continue;
                }
                visited_containers.insert(digest.hash);
                reachable_hashes.insert(digest.hash);

                let bytes = store.get(&digest)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Commit object {} not found in CAS", digest),
                    )
                })?;

                let commit = Commit::deserialize(&bytes).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Corrupt commit {}: {}", digest, e),
                    )
                })?;

                reachable_hashes.insert(commit.root_tree_digest.hash);
                queue.push_back(ContainerKind::Directory(commit.root_tree_digest));
                for parent in commit.parent_commits {
                    reachable_hashes.insert(parent.hash);
                    queue.push_back(ContainerKind::Commit(parent));
                }
            }

            ContainerKind::Directory(digest) => {
                if visited_containers.contains(&digest.hash) {
                    continue;
                }
                visited_containers.insert(digest.hash);
                reachable_hashes.insert(digest.hash);

                let bytes = store.get(&digest)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Directory manifest {} not found in CAS", digest),
                    )
                })?;

                let manifest = DirectoryManifest::from_bytes(&bytes).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Corrupt directory manifest {}: {}", digest, e),
                    )
                })?;

                for entry in manifest.entries.values() {
                    match entry {
                        ManifestEntry::File { digest, .. } => {
                            // Leaf raw blob: Mark hash directly without calling get()!
                            reachable_hashes.insert(digest.hash);
                        }
                        ManifestEntry::ChunkedFile {
                            manifest_digest, ..
                        } => {
                            reachable_hashes.insert(manifest_digest.hash);
                            queue.push_back(ContainerKind::ChunkManifest(*manifest_digest));
                        }
                        ManifestEntry::Directory { digest, .. } => {
                            reachable_hashes.insert(digest.hash);
                            queue.push_back(ContainerKind::Directory(*digest));
                        }
                    }
                }
            }

            ContainerKind::ChunkManifest(digest) => {
                if visited_containers.contains(&digest.hash) {
                    continue;
                }
                visited_containers.insert(digest.hash);
                reachable_hashes.insert(digest.hash);

                let bytes = store.get(&digest)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Chunk manifest {} not found in CAS", digest),
                    )
                })?;

                let chunk_manifest = ChunkManifest::from_bytes(&bytes).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Corrupt chunk manifest {}: {}", digest, e),
                    )
                })?;

                for chunk in chunk_manifest.chunks {
                    // Leaf data chunk: Mark hash directly without calling get()!
                    reachable_hashes.insert(chunk.digest.hash);
                }
            }
        }
    }

    Ok(reachable_hashes)
}

/// Executes a complete Mark-and-Sweep garbage collection cycle.
///
/// # Arguments
/// * `roots` - The list of live root digests.
/// * `store` - The backing CAS store to sweep.
/// * `options` - Garbage collection parameters (grace period, dry-run, etc.).
///
/// # Returns
/// A populated [`GcStats`] structure detailing live, preserved, and reclaimed storage.
///
/// # Errors
/// Returns an error if any reachable container is unreadable or if safety checks fail.
pub fn run_gc<B: BlobStore + ?Sized>(
    roots: &[Digest],
    store: &B,
    options: &GcOptions,
) -> io::Result<GcStats> {
    let start_instant = Instant::now();

    // 1. Snapshot candidates BEFORE marking to avoid race conditions with concurrent writers
    let candidates: Vec<BlobInfo> = store.list_blobs()?;

    // 2. Empty roots safety guard: refuse to delete if zero roots are found but store is non-empty
    if roots.is_empty() && !candidates.is_empty() && !options.allow_empty_roots {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Refusing to garbage collect: zero live roots found, but CAS contains stored blobs. \
             Pass --allow-empty-roots if you explicitly intended to purge all repository data.",
        ));
    }

    // 3. Mark phase: Typed Merkle DAG traversal
    let reachable_hashes = mark_reachable(roots, store)?;

    let mut stats = GcStats {
        roots_count: roots.len(),
        ..Default::default()
    };

    // 4. Sweep phase: evaluate candidates
    for blob in candidates {
        if reachable_hashes.contains(&blob.digest.hash) {
            stats.live_blobs += 1;
            stats.live_bytes += blob.size_bytes;
        } else {
            // Check grace period
            let is_recent = if let Some(mtime) = blob.modified_at {
                if let Ok(age) = options.now.duration_since(mtime) {
                    age < options.grace_period
                } else {
                    // Timestamp is in the future relative to `now`, treat as recent
                    true
                }
            } else {
                false
            };

            if is_recent {
                stats.skipped_recent_blobs += 1;
                stats.skipped_recent_bytes += blob.size_bytes;
            } else {
                stats.garbage_blobs += 1;
                stats.garbage_bytes += blob.size_bytes;

                if !options.dry_run {
                    store.delete(&blob.digest)?;
                }
            }
        }
    }

    stats.elapsed = start_instant.elapsed();
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use crate::cas::chunker::{chunk_slice, ChunkerConfig};
    use crate::metadata::LocalMetadataStore;
    use std::fs;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn test_gc_preserves_live_objects_and_sweeps_unreferenced() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let cas_dir = tmp.path().join("cas");
        let store = Arc::new(LocalBlobStore::new(&cas_dir)?);

        // 1. Create a live file and commit
        let file1_bytes = b"Live file 1 content";
        let file1_digest = store.put(file1_bytes)?;

        let mut manifest = DirectoryManifest::default();
        manifest.entries.insert(
            "file1.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "file1.txt".to_string(),
                digest: file1_digest,
            },
        );
        let tree_bytes = manifest.serialize();
        let tree_digest = store.put(&tree_bytes)?;

        let commit = Commit::new(
            Vec::new(),
            tree_digest,
            "Author <author@example.com>".to_string(),
            "Initial commit".to_string(),
        );
        let commit_bytes = commit.serialize();
        let commit_digest = store.put(&commit_bytes)?;

        // 2. Create an orphaned unreferenced blob (set mtime in the past)
        let orphan_bytes = b"Orphaned garbage blob";
        let orphan_digest = store.put(orphan_bytes)?;

        let orphan_path = store.get_file_path(&orphan_digest).unwrap();
        let past_time = SystemTime::now() - Duration::from_secs(48 * 3600); // 48h ago
        let _ = fs::OpenOptions::new()
            .write(true)
            .open(&orphan_path)?
            .set_times(std::fs::FileTimes::new().set_modified(past_time));

        // 3. Test dry-run: should report 1 garbage blob without deleting it
        let mut options = GcOptions {
            grace_period: Duration::from_secs(24 * 3600),
            dry_run: true,
            now: SystemTime::now(),
            ..Default::default()
        };

        let dry_stats = run_gc(&[commit_digest], store.as_ref(), &options)?;
        assert_eq!(dry_stats.garbage_blobs, 1);
        assert_eq!(dry_stats.garbage_bytes, orphan_bytes.len() as u64);
        assert_eq!(dry_stats.live_blobs, 3); // commit, tree manifest, file1
        assert!(store.has(&orphan_digest), "Dry run must not delete files");

        // 4. Test real sweep: should delete orphan blob
        options.dry_run = false;
        let real_stats = run_gc(&[commit_digest], store.as_ref(), &options)?;
        assert_eq!(real_stats.garbage_blobs, 1);
        assert!(!store.has(&orphan_digest), "Real GC must delete orphan blob");
        assert!(store.has(&file1_digest));
        assert!(store.has(&tree_digest));
        assert!(store.has(&commit_digest));

        Ok(())
    }

    #[test]
    fn test_gc_preserves_recent_unreferenced_blobs_within_grace_period() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let cas_dir = tmp.path().join("cas");
        let store = Arc::new(LocalBlobStore::new(&cas_dir)?);

        let live_digest = store.put(b"Live content")?;
        let recent_unref_digest = store.put(b"Recent unreferenced content")?;

        // Recent blob has mtime = now, within the 24h grace window
        let options = GcOptions {
            grace_period: Duration::from_secs(24 * 3600),
            dry_run: false,
            now: SystemTime::now(),
            ..Default::default()
        };

        let stats = run_gc(&[live_digest], store.as_ref(), &options)?;
        assert_eq!(stats.live_blobs, 1);
        assert_eq!(stats.skipped_recent_blobs, 1);
        assert_eq!(stats.garbage_blobs, 0);

        assert!(store.has(&recent_unref_digest));
        assert!(store.has(&live_digest));

        Ok(())
    }

    #[test]
    fn test_gc_preserves_chunked_file_manifests_and_chunks() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let cas_dir = tmp.path().join("cas");
        let store = Arc::new(LocalBlobStore::new(&cas_dir)?);

        // Create a chunked file with variable content so chunks have distinct digests
        let chunk_config = ChunkerConfig::test_small();
        let mut large_bytes = Vec::with_capacity(4096);
        for i in 0..4096 {
            large_bytes.push(((i * 43 + 17) % 256) as u8);
        }
        let (manifest_digest, chunk_manifest) =
            chunk_slice(&large_bytes, store.as_ref(), &chunk_config)?;
        assert!(chunk_manifest.chunks.len() >= 3);

        let mut dir_manifest = DirectoryManifest::default();
        dir_manifest.entries.insert(
            "large.bin".to_string(),
            ManifestEntry::ChunkedFile {
                mode: "0644".to_string(),
                name: "large.bin".to_string(),
                manifest_digest,
                total_size: large_bytes.len() as u64,
            },
        );
        let tree_digest = store.put(&dir_manifest.serialize())?;

        let options = GcOptions {
            grace_period: Duration::from_secs(0), // No grace period
            dry_run: false,
            now: SystemTime::now(),
            ..Default::default()
        };

        let stats = run_gc(&[tree_digest], store.as_ref(), &options)?;
        assert_eq!(stats.garbage_blobs, 0);

        let unique_chunks: HashSet<[u8; 32]> =
            chunk_manifest.chunks.iter().map(|c| c.digest.hash).collect();
        assert_eq!(stats.live_blobs, 2 + unique_chunks.len());

        for chunk in &chunk_manifest.chunks {
            assert!(store.has(&chunk.digest));
        }
        assert!(store.has(&manifest_digest));
        assert!(store.has(&tree_digest));

        Ok(())
    }

    #[test]
    fn test_gc_fails_closed_on_missing_container() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let cas_dir = tmp.path().join("cas");
        let store = Arc::new(LocalBlobStore::new(&cas_dir)?);

        let missing_digest = Digest::from_bytes(b"missing-root-object");
        let options = GcOptions::default();

        let res = run_gc(&[missing_digest], store.as_ref(), &options);
        assert!(res.is_err(), "Must fail closed if a root or container is missing");

        Ok(())
    }

    #[test]
    fn test_gc_empty_roots_safety_guard() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let cas_dir = tmp.path().join("cas");
        let store = Arc::new(LocalBlobStore::new(&cas_dir)?);

        store.put(b"Some data in CAS")?;

        let options = GcOptions {
            allow_empty_roots: false,
            ..Default::default()
        };

        let res = run_gc(&[], store.as_ref(), &options);
        assert!(res.is_err(), "Must refuse to sweep when roots are empty and CAS is non-empty");

        Ok(())
    }

    #[test]
    fn test_collect_roots_from_metadata_and_workspaces() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let meta_dir = tmp.path().join("metadata");
        let meta_store = LocalMetadataStore::new(&meta_dir)?;

        let branch_commit = Digest::from_bytes(b"branch-commit-1");
        meta_store.compare_and_swap_ref("refs/heads/main", None, &branch_commit, "Test")?;

        let ws_commit = Digest::from_bytes(b"ws-base-commit-2");
        let ws_mgr = WorkspaceManager::new(tmp.path().join("workspaces"));
        let mut ws_cfg = ws_mgr.prepare_config("ws-1", &ws_commit.to_string(), tmp.path(), tmp.path());
        ws_cfg.base_commit = Some(ws_commit.to_string());
        ws_mgr.save_config(&ws_cfg)?;

        let extra_root = Digest::from_bytes(b"extra-pinned-root");
        let roots = collect_roots(&meta_store, Some(&ws_mgr), &[extra_root])?;

        let root_hashes: HashSet<[u8; 32]> = roots.iter().map(|d| d.hash).collect();
        assert!(root_hashes.contains(&branch_commit.hash));
        assert!(root_hashes.contains(&ws_commit.hash));
        assert!(root_hashes.contains(&extra_root.hash));

        Ok(())
    }
}
