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

//! # End-to-End Agent Lifecycle Workflow Test
//!
//! Validates the full developer & autonomous agent development cycle:
//! 1. Ingesting repository into CAS Merkle DAG
//! 2. Virtual filesystem mounting and lazy hydration
//! 3. O(1) Ephemeral workspace creation
//! 4. Autonomous agent file modifications and new file creations
//! 5. O(K) Workspace diffing (`graft diff`) generating standard unified diffs
//! 6. Committing changes and creating new Merkle snapshot (`graft commit`)
//! 7. Inspecting commit ancestry history (`graft log`)
//! 8. Merkle tree-level diffing (`graft diff --base --target`)
//! 9. Materializing repository trees to disk (`graft export tree`)
//! 10. Exporting standard patch files (`graft export patch`)
//! 11. Immutability verification of historical CAS blobs
//! 12. Instant workspace teardown

use graft::cas::{
    apply_changes_to_tree, get_commit_history, BlobStore, Commit, LocalBlobStore,
};
use graft::git::{
    diff_trees, diff_workspace, export_patch_file, export_tree_to_dir, ingest_directory,
};
use graft::vfs::{FileType, GraftFilesystem, ROOT_INODE};
use graft::workspace::{ChangeType, WorkspaceManager};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn test_end_to_end_agent_workflow() -> Result<(), Box<dyn std::error::Error>> {
    // -------------------------------------------------------------------------
    // STEP 1: Set up temporary sandbox environment
    // -------------------------------------------------------------------------
    let root_sandbox = tempdir()?;
    let repo_src = root_sandbox.path().join("upstream_repo");
    let cas_dir = root_sandbox.path().join("cas_storage");
    let ws_storage = root_sandbox.path().join("agent_workspaces");
    let ws_merged = root_sandbox.path().join("workspace_merged");

    fs::create_dir_all(repo_src.join("src"))?;

    // -------------------------------------------------------------------------
    // STEP 2: Populate simulated repository base
    // -------------------------------------------------------------------------
    let initial_readme = b"# Monorepo v1.0\nWelcome to Graft.\n";
    let initial_main = b"fn main() {\n    println!(\"Hello World\");\n}\n";

    fs::write(repo_src.join("README.md"), initial_readme)?;
    fs::write(repo_src.join("src").join("main.rs"), initial_main)?;

    // -------------------------------------------------------------------------
    // STEP 3: Ingest into Content-Addressed Storage (CAS)
    // -------------------------------------------------------------------------
    let blob_store: Arc<dyn BlobStore> = Arc::new(LocalBlobStore::new(&cas_dir)?);
    let ingest_result = ingest_directory(&repo_src, blob_store.as_ref())?;

    assert_eq!(ingest_result.total_files, 2);
    let vfs_tree = Arc::new(std::sync::RwLock::new(ingest_result.tree));

    // Initial root commit (parent 0)
    let initial_commit = Commit::new(
        vec![],
        ingest_result.root_digest,
        "Engineer <engineer@company.com>".to_string(),
        "Initial monorepo baseline".to_string(),
    );
    let initial_commit_digest = blob_store.put(&initial_commit.serialize())?;

    // -------------------------------------------------------------------------
    // STEP 4: Virtual Filesystem (CitC Lower Layer) & Lazy Hydration
    // -------------------------------------------------------------------------
    let vfs = GraftFilesystem::new(Arc::clone(&vfs_tree), Arc::clone(&blob_store));

    // Verify VFS can read base files lazily
    let readme_attr = vfs
        .lookup(ROOT_INODE, "README.md")
        .expect("README must exist");
    assert_eq!(readme_attr.kind, FileType::RegularFile);

    let readme_bytes = vfs.read(readme_attr.ino, 0, 100)?;
    assert_eq!(readme_bytes, initial_readme);

    // -------------------------------------------------------------------------
    // STEP 5: Spawn Ephemeral Agent Workspace in O(1) time
    // -------------------------------------------------------------------------
    let ws_manager = WorkspaceManager::new(&ws_storage);
    let ws_config = ws_manager.prepare_config(
        "agent-task-42",
        &ingest_result.root_digest.to_hex(),
        &repo_src,
        &ws_merged,
    );
    ws_manager.init_workspace_dirs(&ws_config)?;

    // -------------------------------------------------------------------------
    // STEP 6: Simulate Autonomous AI Agent Execution
    // Agent modifies README.md and adds src/patch.rs in upperdir
    // -------------------------------------------------------------------------
    let modified_readme = b"# Monorepo v1.1\nModified by Claude Code Agent.\n";
    fs::write(ws_config.upper_dir.join("README.md"), modified_readme)?;

    fs::create_dir_all(ws_config.upper_dir.join("src"))?;
    let agent_patch = b"pub fn agent_optimization() -> bool {\n    true\n}\n";
    fs::write(
        ws_config.upper_dir.join("src").join("patch.rs"),
        agent_patch,
    )?;

    // -------------------------------------------------------------------------
    // STEP 7: O(K) Delta Detection (graft workspace status)
    // -------------------------------------------------------------------------
    let changes = ws_manager.scan_changes(&ws_config)?;
    assert_eq!(changes.len(), 2);

    let modified = changes
        .iter()
        .find(|c| c.relative_path == Path::new("README.md"))
        .expect("Must find modified README");
    assert_eq!(modified.change_type, ChangeType::Modified);

    let created = changes
        .iter()
        .find(|c| c.relative_path == Path::new("src/patch.rs"))
        .expect("Must find created patch.rs");
    assert_eq!(created.change_type, ChangeType::Created);

    // -------------------------------------------------------------------------
    // STEP 8: Workspace Diff Generation (graft diff --workspace)
    // -------------------------------------------------------------------------
    let ws_diff = diff_workspace(&ws_config, blob_store.as_ref())?;
    assert!(ws_diff.contains("diff --git a/README.md b/README.md"));
    assert!(ws_diff.contains("-Welcome to Graft."));
    assert!(ws_diff.contains("+Modified by Claude Code Agent."));
    assert!(ws_diff.contains("diff --git /dev/null b/src/patch.rs"));
    assert!(ws_diff.contains("+pub fn agent_optimization() -> bool {"));

    // -------------------------------------------------------------------------
    // STEP 9: Commit Snapshot Creation (graft commit)
    // -------------------------------------------------------------------------
    let new_root_digest =
        apply_changes_to_tree(&ingest_result.root_digest, &changes, blob_store.as_ref())?;
    assert_ne!(new_root_digest, ingest_result.root_digest);

    let agent_commit = Commit::new(
        vec![initial_commit_digest],
        new_root_digest,
        "Claude Code <agent@anthropic.com>".to_string(),
        "Agent patch: optimized parser and documentation update".to_string(),
    );
    let agent_commit_digest = blob_store.put(&agent_commit.serialize())?;
    assert!(blob_store.has(&agent_commit_digest));

    // -------------------------------------------------------------------------
    // STEP 10: Commit History Graph Verification (graft log)
    // -------------------------------------------------------------------------
    let history = get_commit_history(&agent_commit_digest, None, blob_store.as_ref())?;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].0, agent_commit_digest);
    assert_eq!(
        history[0].1.message,
        "Agent patch: optimized parser and documentation update"
    );
    assert_eq!(history[1].0, initial_commit_digest);
    assert_eq!(history[1].1.message, "Initial monorepo baseline");

    // -------------------------------------------------------------------------
    // STEP 11: Merkle Tree Diffing (graft diff --base --target)
    // -------------------------------------------------------------------------
    let tree_diff =
        diff_trees(&ingest_result.root_digest, &new_root_digest, blob_store.as_ref())?;
    assert!(tree_diff.contains("diff --git a/README.md b/README.md"));
    assert!(tree_diff.contains("diff --git /dev/null b/src/patch.rs"));

    // -------------------------------------------------------------------------
    // STEP 12: Materialize Tree to Disk (graft export tree)
    // -------------------------------------------------------------------------
    let export_dir = root_sandbox.path().join("materialized_export");
    let files_exported =
        export_tree_to_dir(&new_root_digest, &export_dir, blob_store.as_ref())?;
    assert_eq!(files_exported, 3); // README.md, src/main.rs, src/patch.rs

    let exported_readme = fs::read_to_string(export_dir.join("README.md"))?;
    assert_eq!(
        exported_readme,
        "# Monorepo v1.1\nModified by Claude Code Agent.\n"
    );

    let exported_patch = fs::read_to_string(export_dir.join("src").join("patch.rs"))?;
    assert_eq!(
        exported_patch,
        "pub fn agent_optimization() -> bool {\n    true\n}\n"
    );

    // -------------------------------------------------------------------------
    // STEP 13: Export Standard Patch File (graft export patch)
    // -------------------------------------------------------------------------
    let patch_file = root_sandbox.path().join("claude_agent.patch");
    export_patch_file(&tree_diff, &patch_file)?;
    assert!(patch_file.exists());
    let saved_patch = fs::read_to_string(&patch_file)?;
    assert_eq!(saved_patch, tree_diff);

    // -------------------------------------------------------------------------
    // STEP 14: Verify Base CAS Immutability
    // -------------------------------------------------------------------------
    let original_bytes = vfs.read(readme_attr.ino, 0, 100)?;
    assert_eq!(original_bytes, initial_readme);

    // -------------------------------------------------------------------------
    // STEP 15: Teardown Workspace (< 5ms)
    // -------------------------------------------------------------------------
    ws_manager.cleanup(&ws_config)?;
    assert!(!ws_config.upper_dir.exists());

    println!("✓ All 15 End-to-End integration steps verified successfully!");
    Ok(())
}
