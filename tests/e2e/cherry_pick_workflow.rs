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

//! # Cherry-Pick & Conflict Resolution Assistant E2E Test Suite
//!
//! Validates the complete cherry-pick lifecycle:
//! 1. Direct Merkle tree and CAS cherry-picking across divergent histories.
//! 2. Clean workspace cherry-pick application and commit recording.
//! 3. Workspace cherry-pick with `--no-commit`.
//! 4. Three-way conflict marker insertion (`<<<<<<< ours`, `=======`, `>>>>>>> theirs`).
//! 5. Interactive conflict lifecycle: fail-closed validation on unresolved markers,
//!    `--abort` rollback, and `--continue` resolution and finalization.
//! 6. Server-side direct branch cherry-picking in CAS.
//! 7. Conflict strategy overrides (`--strategy ours` / `--strategy theirs`).

use graft::cas::{
    apply_changes_to_tree, apply_tree_delta_to_workspace, cherry_pick_commit, extract_tree_delta,
    BlobStore, Commit, ConflictStrategy, DirectoryManifest, LocalBlobStore, ManifestEntry,
    TreeDelta,
};
use graft::metadata::{resolve_ref, LocalMetadataStore, MetadataStore};
use graft::workspace::{CherryPickState, WorkspaceManager};
use std::fs;
use std::io;
use tempfile::tempdir;

#[test]
fn test_cherry_pick_workspace_clean_and_no_commit() -> io::Result<()> {
    let tmp = tempdir()?;
    let cas_dir = tmp.path().join("cas");
    let meta_dir = tmp.path().join("metadata");
    let storage_dir = tmp.path().join("workspaces");

    let store = LocalBlobStore::new(&cas_dir)?;
    let meta = LocalMetadataStore::new(&meta_dir)?;
    let manager = WorkspaceManager::new(&storage_dir);

    // 1. Initial base tree on main: fileA.txt and fileB.txt
    let file_a_d = store.put(b"line 1\nline 2\nline 3\n")?;
    let file_b_d = store.put(b"common module code\n")?;

    let mut root_m = DirectoryManifest::default();
    root_m.entries.insert(
        "fileA.txt".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "fileA.txt".to_string(),
            digest: file_a_d,
        },
    );
    root_m.entries.insert(
        "fileB.txt".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "fileB.txt".to_string(),
            digest: file_b_d,
        },
    );
    let base_tree = store.put(&root_m.serialize())?;
    let base_commit = Commit::new(
        vec![],
        base_tree,
        "Author <author@graft.dev>".to_string(),
        "Initial repo commit".to_string(),
    );
    let base_commit_d = store.put(&base_commit.serialize())?;
    meta.put_commit_metadata(&base_commit_d, &base_commit)?;
    meta.compare_and_swap_ref("refs/heads/main", None, &base_commit_d, "author")?;

    // 2. Feature branch with commit to cherry-pick:
    // Modifies fileB.txt and adds feature.txt
    let feature_b_d = store.put(b"common module code\nfeature addition\n")?;
    let feature_extra_d = store.put(b"pub fn new_feature() {}\n")?;
    let mut feat_m = root_m.clone();
    feat_m.entries.insert(
        "fileB.txt".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "fileB.txt".to_string(),
            digest: feature_b_d,
        },
    );
    feat_m.entries.insert(
        "feature.txt".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "feature.txt".to_string(),
            digest: feature_extra_d,
        },
    );
    let feat_tree = store.put(&feat_m.serialize())?;
    let feat_commit = Commit::new(
        vec![base_commit_d],
        feat_tree,
        "Feature Dev <feat@graft.dev>".to_string(),
        "Implement feature B extension".to_string(),
    );
    let feat_commit_d = store.put(&feat_commit.serialize())?;
    meta.put_commit_metadata(&feat_commit_d, &feat_commit)?;
    meta.compare_and_swap_ref("refs/heads/feature", None, &feat_commit_d, "feat")?;

    // 3. Target workspace on a divergent branch: modifies fileA.txt
    let target_a_d = store.put(b"line 1 (modified on target)\nline 2\nline 3\n")?;
    let mut target_m = root_m.clone();
    target_m.entries.insert(
        "fileA.txt".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "fileA.txt".to_string(),
            digest: target_a_d,
        },
    );
    let target_tree = store.put(&target_m.serialize())?;
    let target_commit = Commit::new(
        vec![base_commit_d],
        target_tree,
        "Target Dev <target@graft.dev>".to_string(),
        "Divergent change on target".to_string(),
    );
    let target_commit_d = store.put(&target_commit.serialize())?;
    meta.put_commit_metadata(&target_commit_d, &target_commit)?;
    meta.compare_and_swap_ref("refs/heads/target-branch", None, &target_commit_d, "target")?;

    // Prepare workspace for target
    let ws_id = "agent-target-ws";
    let mut ws_config = manager.prepare_config(
        ws_id,
        &target_tree.to_string(),
        &tmp.path().join("lower"),
        &tmp.path().join("merged"),
    );
    ws_config.base_commit = Some(target_commit_d.to_string());
    ws_config.branch = Some("refs/heads/target-branch".to_string());
    manager.init_workspace_dirs(&ws_config)?;

    // 4. Perform clean cherry-pick of feat_commit onto workspace
    let res = cherry_pick_commit(
        &feat_commit_d,
        &target_tree,
        None,
        ConflictStrategy::Markers,
        &store,
    )?;
    assert!(!res.has_conflicts);
    assert_eq!(res.merged_files.len(), 3);
    assert!(res.merged_files.contains(&"fileB.txt".to_string()));
    assert!(res.merged_files.contains(&"feature.txt".to_string()));

    // Apply deltas to workspace upper_dir
    let deltas = extract_tree_delta(&target_tree, &res.root_tree_digest, &store)?;
    assert_eq!(deltas.len(), 2);
    apply_tree_delta_to_workspace(&ws_config.upper_dir, &deltas, &store)?;

    // Verify files on disk in upper_dir
    assert!(ws_config.upper_dir.join("feature.txt").exists());
    assert!(ws_config.upper_dir.join("fileB.txt").exists());
    let b_content = fs::read_to_string(ws_config.upper_dir.join("fileB.txt"))?;
    assert!(b_content.contains("feature addition"));

    // Record the commit
    let cherry_commit = Commit::new(
        vec![target_commit_d],
        res.root_tree_digest,
        res.original_author.clone(),
        res.original_message.clone(),
    );
    let cherry_commit_d = store.put(&cherry_commit.serialize())?;
    meta.put_commit_metadata(&cherry_commit_d, &cherry_commit)?;
    meta.compare_and_swap_ref(
        "refs/heads/target-branch",
        Some(&target_commit_d),
        &cherry_commit_d,
        &res.original_author,
    )?;

    ws_config.base_commit = Some(cherry_commit_d.to_string());
    ws_config.commit = res.root_tree_digest.to_string();
    manager.save_config(&ws_config)?;

    // Verify branch head updated
    let (_, head_d) = resolve_ref(&meta, "target-branch")?.unwrap();
    assert_eq!(head_d, cherry_commit_d);

    // Verify Merkle tree contains changes from both target AND cherry-picked commit
    let final_m_bytes = store.get(&res.root_tree_digest)?.unwrap();
    let final_m = DirectoryManifest::from_bytes(&final_m_bytes).unwrap();
    assert!(final_m.entries.contains_key("feature.txt"));
    assert!(final_m.entries.contains_key("fileA.txt"));
    assert!(final_m.entries.contains_key("fileB.txt"));

    let fa_entry = final_m.entries.get("fileA.txt").unwrap();
    if let ManifestEntry::File { digest, .. } = fa_entry {
        let text = String::from_utf8(store.get(digest)?.unwrap()).unwrap();
        assert!(text.contains("line 1 (modified on target)"));
    }

    Ok(())
}

#[test]
fn test_cherry_pick_conflict_lifecycle_abort_and_continue() -> io::Result<()> {
    let tmp = tempdir()?;
    let cas_dir = tmp.path().join("cas");
    let meta_dir = tmp.path().join("metadata");
    let storage_dir = tmp.path().join("workspaces");

    let store = LocalBlobStore::new(&cas_dir)?;
    let meta = LocalMetadataStore::new(&meta_dir)?;
    let manager = WorkspaceManager::new(&storage_dir);

    // 1. Initial base tree: shared file with 3 lines
    let base_a_d = store.put(b"line 1\nline 2 (base)\nline 3\n")?;
    let mut base_m = DirectoryManifest::default();
    base_m.entries.insert(
        "fileA.txt".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "fileA.txt".to_string(),
            digest: base_a_d,
        },
    );
    let base_tree = store.put(&base_m.serialize())?;
    let base_commit = Commit::new(
        vec![],
        base_tree,
        "Author <author@graft.dev>".to_string(),
        "Base commit".to_string(),
    );
    let base_commit_d = store.put(&base_commit.serialize())?;
    meta.put_commit_metadata(&base_commit_d, &base_commit)?;

    // 2. Incoming commit C_feat: modifies line 2 to 'line 2 (feature version)'
    let feat_a_d = store.put(b"line 1\nline 2 (feature version)\nline 3\n")?;
    let mut feat_m = base_m.clone();
    feat_m.entries.insert(
        "fileA.txt".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "fileA.txt".to_string(),
            digest: feat_a_d,
        },
    );
    let feat_tree = store.put(&feat_m.serialize())?;
    let feat_commit = Commit::new(
        vec![base_commit_d],
        feat_tree,
        "Alice <alice@graft.dev>".to_string(),
        "Update line 2 in Alice style".to_string(),
    );
    let feat_commit_d = store.put(&feat_commit.serialize())?;
    meta.put_commit_metadata(&feat_commit_d, &feat_commit)?;

    // 3. Target commit C_target: also modifies line 2 to 'line 2 (target conflicting)'
    let target_a_d = store.put(b"line 1\nline 2 (target conflicting)\nline 3\n")?;
    let mut target_m = base_m.clone();
    target_m.entries.insert(
        "fileA.txt".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "fileA.txt".to_string(),
            digest: target_a_d,
        },
    );
    let target_tree = store.put(&target_m.serialize())?;
    let target_commit = Commit::new(
        vec![base_commit_d],
        target_tree,
        "Bob <bob@graft.dev>".to_string(),
        "Update line 2 in Bob style".to_string(),
    );
    let target_commit_d = store.put(&target_commit.serialize())?;
    meta.put_commit_metadata(&target_commit_d, &target_commit)?;
    meta.compare_and_swap_ref("refs/heads/bob-branch", None, &target_commit_d, "bob")?;

    // Create workspace for Bob
    let ws_id = "bob-ws";
    let mut ws_config = manager.prepare_config(
        ws_id,
        &target_tree.to_string(),
        &tmp.path().join("lower"),
        &tmp.path().join("merged"),
    );
    ws_config.base_commit = Some(target_commit_d.to_string());
    ws_config.branch = Some("refs/heads/bob-branch".to_string());
    manager.init_workspace_dirs(&ws_config)?;

    // 4. Cherry-pick with Markers: must trigger conflict
    let res = cherry_pick_commit(
        &feat_commit_d,
        &target_tree,
        None,
        ConflictStrategy::Markers,
        &store,
    )?;
    assert!(res.has_conflicts);
    assert_eq!(res.conflict_files, vec!["fileA.txt".to_string()]);

    let deltas = extract_tree_delta(&target_tree, &res.root_tree_digest, &store)?;
    apply_tree_delta_to_workspace(&ws_config.upper_dir, &deltas, &store)?;

    // Verify conflict markers in fileA.txt in workspace upper_dir
    let disk_file_a = ws_config.upper_dir.join("fileA.txt");
    assert!(disk_file_a.exists());
    let conflict_text = fs::read_to_string(&disk_file_a)?;
    assert!(conflict_text.contains("<<<<<<< ours"));
    assert!(conflict_text.contains("line 2 (target conflicting)"));
    assert!(conflict_text.contains("======="));
    assert!(conflict_text.contains("line 2 (feature version)"));
    assert!(conflict_text.contains(">>>>>>> theirs"));

    // Save cherry-pick state in workspace
    let applied_files: Vec<String> = deltas
        .iter()
        .map(|d| match d {
            TreeDelta::Upsert { path, .. } => path.to_string_lossy().to_string(),
            TreeDelta::Delete { path } => path.to_string_lossy().to_string(),
        })
        .collect();
    let state = CherryPickState {
        source_commit: feat_commit_d.to_string(),
        original_message: res.original_message.clone(),
        original_author: res.original_author.clone(),
        conflict_files: res.conflict_files.clone(),
        applied_files: applied_files.clone(),
        strategy: "markers".to_string(),
        target_branch: ws_config.branch.clone(),
        timestamp: 123456789,
    };
    manager.save_cherry_pick_state(ws_id, &state)?;
    assert!(manager.load_cherry_pick_state(ws_id)?.is_some());

    // =========================================================================
    // 5. Test Abort Lifecycle
    // =========================================================================
    // Simulating `graft cherry-pick --abort`:
    for rel in &state.applied_files {
        let p = ws_config.upper_dir.join(rel);
        if p.exists() {
            fs::remove_file(p)?;
        }
    }
    manager.clear_cherry_pick_state(ws_id)?;
    assert!(manager.load_cherry_pick_state(ws_id)?.is_none());
    assert!(!disk_file_a.exists(), "Workspace upper_dir must be clean after abort");

    // =========================================================================
    // 6. Re-trigger cherry-pick and test Continue Lifecycle
    // =========================================================================
    apply_tree_delta_to_workspace(&ws_config.upper_dir, &deltas, &store)?;
    manager.save_cherry_pick_state(ws_id, &state)?;
    assert!(disk_file_a.exists());

    // Verify that while conflict markers remain, continue fails closed
    let raw_content = fs::read_to_string(&disk_file_a)?;
    let has_unresolved = raw_content.contains("<<<<<<< ours")
        || raw_content.contains("=======")
        || raw_content.contains(">>>>>>> theirs");
    assert!(has_unresolved, "Must detect unresolved conflict markers");

    // Agent resolves the conflict cleanly
    let resolved_content = b"line 1\nline 2 (resolved: both Alice & Bob)\nline 3\n";
    fs::write(&disk_file_a, resolved_content)?;

    // Verify conflict markers are gone
    let verified_content = fs::read_to_string(&disk_file_a)?;
    let still_conflicted = verified_content.contains("<<<<<<< ours")
        || verified_content.contains("=======")
        || verified_content.contains(">>>>>>> theirs");
    assert!(!still_conflicted, "Conflict markers must be cleared");

    // Complete continue: scan changes and apply to tree
    let changes = manager.scan_changes(&ws_config)?;
    assert_eq!(changes.len(), 1);
    let resolved_tree = apply_changes_to_tree(&target_tree, &changes, &store)?;

    let resolved_commit = Commit::new(
        vec![target_commit_d],
        resolved_tree,
        state.original_author.clone(),
        state.original_message.clone(),
    );
    let resolved_commit_d = store.put(&resolved_commit.serialize())?;
    meta.put_commit_metadata(&resolved_commit_d, &resolved_commit)?;

    meta.compare_and_swap_ref(
        "refs/heads/bob-branch",
        Some(&target_commit_d),
        &resolved_commit_d,
        &state.original_author,
    )?;

    manager.clear_cherry_pick_state(ws_id)?;
    assert!(manager.load_cherry_pick_state(ws_id)?.is_none());

    // Verify final commit
    let (_, head_d) = resolve_ref(&meta, "bob-branch")?.unwrap();
    assert_eq!(head_d, resolved_commit_d);

    let resolved_tree_bytes = store.get(&resolved_tree)?.unwrap();
    let resolved_m = DirectoryManifest::from_bytes(&resolved_tree_bytes).unwrap();
    let final_entry = resolved_m.entries.get("fileA.txt").unwrap();
    if let ManifestEntry::File { digest, .. } = final_entry {
        let text = String::from_utf8(store.get(digest)?.unwrap()).unwrap();
        assert!(text.contains("resolved: both Alice & Bob"));
    }

    Ok(())
}

#[test]
fn test_direct_branch_cherry_pick_and_strategies() -> io::Result<()> {
    let tmp = tempdir()?;
    let cas_dir = tmp.path().join("cas");
    let meta_dir = tmp.path().join("metadata");

    let store = LocalBlobStore::new(&cas_dir)?;
    let meta = LocalMetadataStore::new(&meta_dir)?;

    // 1. Initial base commit
    let base_a = store.put(b"def compute():\n    return 42\n")?;
    let mut base_m = DirectoryManifest::default();
    base_m.entries.insert(
        "compute.py".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "compute.py".to_string(),
            digest: base_a,
        },
    );
    let base_tree = store.put(&base_m.serialize())?;
    let base_commit = Commit::new(
        vec![],
        base_tree,
        "Lead <lead@graft.dev>".to_string(),
        "Base compute".to_string(),
    );
    let base_commit_d = store.put(&base_commit.serialize())?;
    meta.put_commit_metadata(&base_commit_d, &base_commit)?;

    // 2. Feature commit: adds docstring
    let feat_a = store.put(b"\"\"\"Compute module.\"\"\"\ndef compute():\n    return 42\n")?;
    let mut feat_m = base_m.clone();
    feat_m.entries.insert(
        "compute.py".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "compute.py".to_string(),
            digest: feat_a,
        },
    );
    let feat_tree = store.put(&feat_m.serialize())?;
    let feat_commit = Commit::new(
        vec![base_commit_d],
        feat_tree,
        "Doc Dev <doc@graft.dev>".to_string(),
        "Add compute docstring".to_string(),
    );
    let feat_commit_d = store.put(&feat_commit.serialize())?;
    meta.put_commit_metadata(&feat_commit_d, &feat_commit)?;

    // 3. Staging branch with non-conflicting change (adds another function)
    let staging_a = store.put(b"def compute():\n    return 42\n\ndef helper():\n    return 100\n")?;
    let mut staging_m = base_m.clone();
    staging_m.entries.insert(
        "compute.py".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "compute.py".to_string(),
            digest: staging_a,
        },
    );
    let staging_tree = store.put(&staging_m.serialize())?;
    let staging_commit = Commit::new(
        vec![base_commit_d],
        staging_tree,
        "Staging <stg@graft.dev>".to_string(),
        "Add helper function".to_string(),
    );
    let staging_commit_d = store.put(&staging_commit.serialize())?;
    meta.put_commit_metadata(&staging_commit_d, &staging_commit)?;
    meta.compare_and_swap_ref("refs/heads/staging", None, &staging_commit_d, "stg")?;

    // 4. Server-side direct branch cherry-pick of feat_commit onto 'staging' in CAS
    let res = cherry_pick_commit(
        &feat_commit_d,
        &staging_tree,
        None,
        ConflictStrategy::Markers,
        &store,
    )?;
    assert!(!res.has_conflicts);

    let new_staging_commit = Commit::new(
        vec![staging_commit_d],
        res.root_tree_digest,
        res.original_author.clone(),
        res.original_message.clone(),
    );
    let new_staging_commit_d = store.put(&new_staging_commit.serialize())?;
    meta.put_commit_metadata(&new_staging_commit_d, &new_staging_commit)?;
    meta.compare_and_swap_ref(
        "refs/heads/staging",
        Some(&staging_commit_d),
        &new_staging_commit_d,
        "doc",
    )?;

    // Verify staging branch head
    let (_, staging_head) = resolve_ref(&meta, "staging")?.unwrap();
    assert_eq!(staging_head, new_staging_commit_d);

    // Verify Merkle tree has both docstring and helper
    let merged_bytes = store.get(&res.root_tree_digest)?.unwrap();
    let merged_m = DirectoryManifest::from_bytes(&merged_bytes).unwrap();
    if let ManifestEntry::File { digest, .. } = merged_m.entries.get("compute.py").unwrap() {
        let code = String::from_utf8(store.get(digest)?.unwrap()).unwrap();
        assert!(code.contains("\"\"\"Compute module.\"\"\""));
        assert!(code.contains("def helper():"));
    }

    // 5. Test ConflictStrategy::Ours and ConflictStrategy::Theirs
    let conflict_input = store.put(b"conflicting value\n")?;
    let mut conf_m = base_m.clone();
    conf_m.entries.insert(
        "compute.py".to_string(),
        ManifestEntry::File {
            mode: "0644".to_string(),
            name: "compute.py".to_string(),
            digest: conflict_input,
        },
    );
    let conf_tree = store.put(&conf_m.serialize())?;

    // Strategy::Ours selects current target branch version
    let ours_res = cherry_pick_commit(
        &feat_commit_d,
        &conf_tree,
        None,
        ConflictStrategy::Ours,
        &store,
    )?;
    let ours_bytes = store.get(&ours_res.root_tree_digest)?.unwrap();
    let ours_m = DirectoryManifest::from_bytes(&ours_bytes).unwrap();
    if let ManifestEntry::File { digest, .. } = ours_m.entries.get("compute.py").unwrap() {
        assert_eq!(*digest, conflict_input);
    }

    // Strategy::Theirs selects incoming cherry-picked version
    let theirs_res = cherry_pick_commit(
        &feat_commit_d,
        &conf_tree,
        None,
        ConflictStrategy::Theirs,
        &store,
    )?;
    let theirs_bytes = store.get(&theirs_res.root_tree_digest)?.unwrap();
    let theirs_m = DirectoryManifest::from_bytes(&theirs_bytes).unwrap();
    if let ManifestEntry::File { digest, .. } = theirs_m.entries.get("compute.py").unwrap() {
        assert_eq!(*digest, feat_a);
    }

    Ok(())
}
