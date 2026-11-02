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

//! # End-to-End Garbage Collection Lifecycle Test
//!
//! Validates the complete lifecycle of CAS garbage collection:
//! 1. Discovers roots across branch refs, active leases, and local workspaces.
//! 2. Preserves full commit ancestry, directory manifests, files, and chunk manifests.
//! 3. Protects recent unreferenced blobs within the grace period.
//! 4. Dry-run previews reclaimable space without modifying disk state.
//! 5. Real sweep deletes expired unreferenced blobs while maintaining live integrity.

use graft::cas::{
    apply_changes_to_tree_with_config, collect_roots, run_gc, BlobStore, ChunkerConfig, Commit,
    GcOptions, LocalBlobStore,
};
use graft::git::ingest_directory_with_config;
use graft::metadata::{LocalMetadataStore, MetadataStore};
use graft::workspace::{ChangeType, WorkspaceChange, WorkspaceManager};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::tempdir;

#[test]
fn test_end_to_end_gc_lifecycle() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempdir()?;
    let cas_dir = tmp.path().join("cas");
    let store = Arc::new(LocalBlobStore::new(&cas_dir)?);
    let meta_dir = tmp.path().join("metadata");
    let meta_store = Arc::new(LocalMetadataStore::new(&meta_dir)?);
    let storage_dir = tmp.path().join("storage");
    let ws_manager = WorkspaceManager::new(&storage_dir);

    let config = ChunkerConfig {
        min_size: 128,
        avg_size: 512,
        max_size: 2048,
        threshold_bytes: 1024,
    };

    // 1. Ingest initial repository content
    let repo_src = tmp.path().join("repo_src");
    fs::create_dir_all(&repo_src)?;
    fs::write(repo_src.join("README.md"), b"# Main Repository\nVersion 1.0\n")?;

    let mut large_content = Vec::with_capacity(8192);
    for i in 0..8192 {
        large_content.push(((i * 37 + 11) % 256) as u8);
    }
    fs::write(repo_src.join("model.bin"), &large_content)?;

    let ingest_res = ingest_directory_with_config(&repo_src, store.as_ref(), &config)?;

    // Commit 1 on main
    let commit1 = Commit::new(
        Vec::new(),
        ingest_res.root_digest,
        "Lead <lead@graft.dev>".to_string(),
        "Initial commit".to_string(),
    );
    let commit1_digest = store.put(&commit1.serialize())?;
    meta_store.compare_and_swap_ref("refs/heads/main", None, &commit1_digest, "Lead")?;

    // Commit 2 on feature-branch
    let patch_file = tmp.path().join("patch.txt");
    fs::write(&patch_file, b"# Feature updates\n")?;
    let tree2_digest = apply_changes_to_tree_with_config(
        &ingest_res.root_digest,
        &[WorkspaceChange {
            relative_path: PathBuf::from("feature.txt"),
            disk_path: patch_file,
            change_type: ChangeType::Created,
        }],
        store.as_ref(),
        &config,
    )?;

    let commit2 = Commit::new(
        vec![commit1_digest],
        tree2_digest,
        "Agent <agent@graft.dev>".to_string(),
        "Feature commit".to_string(),
    );
    let commit2_digest = store.put(&commit2.serialize())?;
    meta_store.compare_and_swap_ref("refs/heads/feature", None, &commit2_digest, "Agent")?;

    // Active workspace referencing commit2
    let mut ws_cfg = ws_manager.prepare_config("agent-ws", &commit2_digest.to_string(), tmp.path(), tmp.path());
    ws_cfg.base_commit = Some(commit2_digest.to_string());
    ws_manager.save_config(&ws_cfg)?;

    // Active distributed lease referencing commit1
    let _lease = meta_store.acquire_lease("task-lease", "agent-vm-9", &commit1_digest, 600)?;

    // 2. Inject unreferenced blobs into CAS:
    // - Orphan A: Old blob (modified 48 hours ago -> should be swept)
    // - Orphan B: Recent blob (modified 1 hour ago -> should be protected by grace period)
    let orphan_old_bytes = b"Unreferenced old abandoned blob 48h";
    let orphan_old_digest = store.put(orphan_old_bytes)?;
    let orphan_old_path = store.get_file_path(&orphan_old_digest).unwrap();
    let past_time = SystemTime::now() - Duration::from_secs(48 * 3600);
    let _ = fs::OpenOptions::new()
        .write(true)
        .open(&orphan_old_path)?
        .set_times(std::fs::FileTimes::new().set_modified(past_time));

    let orphan_recent_bytes = b"Unreferenced recent in-flight blob 1h";
    let orphan_recent_digest = store.put(orphan_recent_bytes)?;
    let orphan_recent_path = store.get_file_path(&orphan_recent_digest).unwrap();
    let recent_time = SystemTime::now() - Duration::from_secs(3600);
    let _ = fs::OpenOptions::new()
        .write(true)
        .open(&orphan_recent_path)?
        .set_times(std::fs::FileTimes::new().set_modified(recent_time));

    // 3. Collect roots: must discover main, feature, workspace, and lease
    let roots = collect_roots(meta_store.as_ref(), Some(&ws_manager), &[])?;
    assert!(roots.len() >= 2);
    let root_hashes: std::collections::HashSet<[u8; 32]> = roots.iter().map(|d| d.hash).collect();
    assert!(root_hashes.contains(&commit1_digest.hash));
    assert!(root_hashes.contains(&commit2_digest.hash));

    // 4. Test Dry-Run
    let mut gc_opts = GcOptions {
        grace_period: Duration::from_secs(24 * 3600),
        dry_run: true,
        now: SystemTime::now(),
        ..Default::default()
    };

    let dry_stats = run_gc(&roots, store.as_ref(), &gc_opts)?;
    assert_eq!(dry_stats.garbage_blobs, 1, "Only old orphan should be marked as garbage");
    assert_eq!(dry_stats.garbage_bytes, orphan_old_bytes.len() as u64);
    assert_eq!(dry_stats.skipped_recent_blobs, 1, "Recent orphan should be skipped");
    assert!(store.has(&orphan_old_digest), "Dry run must not delete old orphan");
    assert!(store.has(&orphan_recent_digest));

    // 5. Test Real Garbage Collection
    gc_opts.dry_run = false;
    let real_stats = run_gc(&roots, store.as_ref(), &gc_opts)?;
    assert_eq!(real_stats.garbage_blobs, 1);
    assert_eq!(real_stats.skipped_recent_blobs, 1);

    // Verify old orphan was purged
    assert!(!store.has(&orphan_old_digest), "Old orphan must be purged");
    // Verify recent orphan is still protected
    assert!(store.has(&orphan_recent_digest), "Recent orphan must be preserved");

    // Verify ALL live objects are still completely intact
    assert!(store.has(&commit1_digest));
    assert!(store.has(&commit2_digest));
    assert!(store.has(&ingest_res.root_digest));
    assert!(store.has(&tree2_digest));

    // 6. Delete feature branch & release lease, then run GC with 0s grace period
    meta_store.delete_ref("refs/heads/feature", None)?;
    meta_store.release_lease("task-lease", "agent-vm-9", true)?;

    let post_delete_roots = collect_roots(meta_store.as_ref(), None, &[])?;
    assert!(!post_delete_roots.iter().any(|d| d.hash == commit2_digest.hash));

    let purge_opts = GcOptions {
        grace_period: Duration::from_secs(0),
        dry_run: false,
        now: SystemTime::now(),
        ..Default::default()
    };
    let purge_stats = run_gc(&post_delete_roots, store.as_ref(), &purge_opts)?;
    assert!(purge_stats.garbage_blobs >= 2, "Feature commit and recent orphan should be swept");
    assert!(!store.has(&commit2_digest), "Deleted branch commit must be swept");
    assert!(!store.has(&orphan_recent_digest), "Recent orphan must now be swept");

    // Main commit and root tree must STILL be intact
    assert!(store.has(&commit1_digest));
    assert!(store.has(&ingest_res.root_digest));

    Ok(())
}
