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

//! # End-to-End 3-Way Merkle Merge Workflow Test
//!
//! Validates high-concurrency branch merging between parallel AI agent branches:
//! 1. Creating base repository snapshot (Commit B)
//! 2. Agent 1 branches off B, edits README and adds `feature_ours.rs` (Commit O)
//! 3. Agent 2 branches off B concurrently, adds `feature_theirs.rs` and edits conflicting file (Commit T)
//! 4. Auto-detecting Best Common Ancestor (BCA)
//! 5. Executing server-side 3-way Merkle merge in O(changed) time
//! 6. Verifying clean non-overlapping merges are integrated automatically
//! 7. Verifying conflicting edits produce standard `<<<<<<< ours` conflict markers
//! 8. Emitting a 2-parent merge commit and verifying commit ancestry history

use graft::cas::{
    apply_changes_to_tree, find_common_ancestor, get_commit_history, merge_trees, BlobStore,
    Commit, LocalBlobStore,
};
use graft::git::ingest_directory;
use graft::workspace::WorkspaceManager;
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn test_end_to_end_3way_merge_workflow() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = tempdir()?;
    let cas_dir = sandbox.path().join("cas");
    let base_repo = sandbox.path().join("base_repo");
    let ws_storage = sandbox.path().join("workspaces");

    fs::create_dir_all(base_repo.join("src"))?;

    // -------------------------------------------------------------------------
    // STEP 1: Create Base Commit (B)
    // -------------------------------------------------------------------------
    fs::write(base_repo.join("README.md"), b"# Original Monorepo Baseline\n")?;
    fs::write(
        base_repo.join("src").join("common.rs"),
        b"pub fn common() -> i32 { 42 }\n",
    )?;
    fs::write(
        base_repo.join("src").join("shared.rs"),
        b"pub fn version() -> &'static str { \"v1.0\" }\n",
    )?;

    let store: Arc<dyn BlobStore> = Arc::new(LocalBlobStore::new(&cas_dir)?);
    let base_ingest = ingest_directory(&base_repo, store.as_ref())?;

    let base_commit = Commit::new(
        vec![],
        base_ingest.root_digest,
        "Lead Architect <lead@company.com>".to_string(),
        "Base Monorepo v1.0".to_string(),
    );
    let base_commit_digest = store.put(&base_commit.serialize())?;

    // -------------------------------------------------------------------------
    // STEP 2: Agent 1 (Ours) Branches off B -> Commit O
    // Modifies README.md, updates shared.rs to v1.1-ours, adds feature_ours.rs
    // -------------------------------------------------------------------------
    let ws_mgr = WorkspaceManager::new(&ws_storage);
    let ws_ours_merged = sandbox.path().join("ws_ours_merged");
    let config_ours = ws_mgr.prepare_config(
        "agent-ours",
        &base_ingest.root_digest.to_hex(),
        &base_repo,
        &ws_ours_merged,
    );
    ws_mgr.init_workspace_dirs(&config_ours)?;

    fs::write(
        config_ours.upper_dir.join("README.md"),
        b"# Ours Monorepo: Performance Optimizations\n",
    )?;
    fs::create_dir_all(config_ours.upper_dir.join("src"))?;
    fs::write(
        config_ours.upper_dir.join("src").join("shared.rs"),
        b"pub fn version() -> &'static str { \"v1.1-ours\" }\n",
    )?;
    fs::write(
        config_ours.upper_dir.join("src").join("feature_ours.rs"),
        b"pub fn feature_ours() -> bool { true }\n",
    )?;

    let changes_ours = ws_mgr.scan_changes(&config_ours)?;
    assert_eq!(changes_ours.len(), 3);

    let ours_root_digest =
        apply_changes_to_tree(&base_ingest.root_digest, &changes_ours, store.as_ref())?;
    let ours_commit = Commit::new(
        vec![base_commit_digest],
        ours_root_digest,
        "Agent 1 <agent1@graft.dev>".to_string(),
        "Agent 1: feature ours and version update".to_string(),
    );
    let ours_commit_digest = store.put(&ours_commit.serialize())?;

    // -------------------------------------------------------------------------
    // STEP 3: Agent 2 (Theirs) Branches off B Concurrently -> Commit T
    // Adds feature_theirs.rs, updates shared.rs to v1.1-theirs
    // -------------------------------------------------------------------------
    let ws_theirs_merged = sandbox.path().join("ws_theirs_merged");
    let config_theirs = ws_mgr.prepare_config(
        "agent-theirs",
        &base_ingest.root_digest.to_hex(),
        &base_repo,
        &ws_theirs_merged,
    );
    ws_mgr.init_workspace_dirs(&config_theirs)?;

    fs::create_dir_all(config_theirs.upper_dir.join("src"))?;
    fs::write(
        config_theirs.upper_dir.join("src").join("shared.rs"),
        b"pub fn version() -> &'static str { \"v1.1-theirs\" }\n",
    )?;
    fs::write(
        config_theirs.upper_dir.join("src").join("feature_theirs.rs"),
        b"pub fn feature_theirs() -> &'static str { \"ready\" }\n",
    )?;

    let changes_theirs = ws_mgr.scan_changes(&config_theirs)?;
    assert_eq!(changes_theirs.len(), 2);

    let theirs_root_digest =
        apply_changes_to_tree(&base_ingest.root_digest, &changes_theirs, store.as_ref())?;
    let theirs_commit = Commit::new(
        vec![base_commit_digest],
        theirs_root_digest,
        "Agent 2 <agent2@graft.dev>".to_string(),
        "Agent 2: feature theirs and version update".to_string(),
    );
    let theirs_commit_digest = store.put(&theirs_commit.serialize())?;

    // -------------------------------------------------------------------------
    // STEP 4: Auto-Detect Best Common Ancestor (BCA)
    // -------------------------------------------------------------------------
    let ancestor_opt = find_common_ancestor(&ours_commit_digest, &theirs_commit_digest, store.as_ref())?;
    assert_eq!(ancestor_opt, Some(base_commit_digest));

    // -------------------------------------------------------------------------
    // STEP 5: Execute 3-Way Merkle Merge
    // -------------------------------------------------------------------------
    let merge_result = merge_trees(
        &base_ingest.root_digest,
        &ours_root_digest,
        &theirs_root_digest,
        store.as_ref(),
    )?;

    // Step 5a: Verify conflicts on shared.rs (both edited conflicting version)
    assert!(merge_result.has_conflicts);
    assert_eq!(merge_result.conflict_files, vec!["src/shared.rs"]);

    // Step 5b: Verify clean non-overlapping files were merged
    assert!(merge_result.merged_files.contains(&"README.md".to_string()));
    assert!(merge_result.merged_files.contains(&"src/feature_theirs.rs".to_string()));

    // -------------------------------------------------------------------------
    // STEP 6: Verify Content of Merged Manifest
    // -------------------------------------------------------------------------
    let merged_manifest_bytes = store
        .get(&merge_result.root_tree_digest)?
        .expect("merged manifest must exist in CAS");
    let merged_manifest_str = String::from_utf8_lossy(&merged_manifest_bytes);

    // Verify README.md is present (from Ours)
    assert!(merged_manifest_str.contains("README.md"));

    // Check src directory manifest
    let src_line = merged_manifest_str
        .lines()
        .find(|l| l.contains("DIR src"))
        .expect("must find DIR src");
    let src_hash = src_line.split_whitespace().last().unwrap();
    let src_digest = graft::cas::Digest::parse(src_hash)?;

    let src_manifest_bytes = store.get(&src_digest)?.expect("src manifest must exist");
    let src_manifest_str = String::from_utf8_lossy(&src_manifest_bytes);

    // Both features must exist in the merged tree!
    assert!(src_manifest_str.contains("feature_ours.rs"));
    assert!(src_manifest_str.contains("feature_theirs.rs"));
    assert!(src_manifest_str.contains("common.rs"));
    assert!(src_manifest_str.contains("shared.rs"));

    // Check shared.rs conflict markers
    let shared_line = src_manifest_str
        .lines()
        .find(|l| l.contains("shared.rs"))
        .expect("must find shared.rs");
    let shared_hash = shared_line.split_whitespace().last().unwrap();
    let shared_digest = graft::cas::Digest::parse(shared_hash)?;
    let shared_content = String::from_utf8_lossy(&store.get(&shared_digest)?.unwrap()).to_string();

    assert!(shared_content.contains("<<<<<<< ours"));
    assert!(shared_content.contains("v1.1-ours"));
    assert!(shared_content.contains("======="));
    assert!(shared_content.contains("v1.1-theirs"));
    assert!(shared_content.contains(">>>>>>> theirs"));

    // -------------------------------------------------------------------------
    // STEP 7: Create Merge Commit with 2 Parents
    // -------------------------------------------------------------------------
    let merge_commit = Commit::new(
        vec![ours_commit_digest, theirs_commit_digest],
        merge_result.root_tree_digest,
        "Orchestrator <bot@graft.dev>".to_string(),
        "Merge Agent 2 branch into Agent 1".to_string(),
    );
    let merge_commit_digest = store.put(&merge_commit.serialize())?;

    // -------------------------------------------------------------------------
    // STEP 8: Verify Commit History Graph
    // -------------------------------------------------------------------------
    let history = get_commit_history(&merge_commit_digest, None, store.as_ref())?;
    assert_eq!(history.len(), 3); // Merge commit -> Ours commit -> Base commit
    assert_eq!(history[0].0, merge_commit_digest);
    assert_eq!(history[0].1.parent_commits.len(), 2);
    assert_eq!(history[1].0, ours_commit_digest);
    assert_eq!(history[2].0, base_commit_digest);

    println!("✓ 3-Way Merkle Merge E2E test completed successfully!");
    Ok(())
}
