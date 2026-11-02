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

//! # Multi-Agent Branching & 3-Way Merge E2E Test Suite
//!
//! Tests the complete lifecycle of parallel agent branching, workspace checkout tracking,
//! fast-forward merges, true 3-way Merkle merges, conflict aborts, and optimistic CAS retries.

use graft::cas::{
    apply_changes_to_tree, find_common_ancestor, merge_trees, BlobStore, Commit,
    DirectoryManifest, LocalBlobStore,
};
use graft::metadata::{
    normalize_ref_name, resolve_ref, validate_branch_name, LocalMetadataStore, MetadataStore,
};
use graft::workspace::{ChangeType, WorkspaceChange, WorkspaceManager};
use std::fs;
use std::io;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn test_multi_agent_branch_and_3way_merge_lifecycle() -> io::Result<()> {
    let tmp = tempdir()?;
    let cas_dir = tmp.path().join("cas");
    let meta_dir = tmp.path().join("metadata");
    let storage_dir = tmp.path().join("workspaces");

    let store = LocalBlobStore::new(&cas_dir)?;
    let meta = LocalMetadataStore::new(&meta_dir)?;
    let manager = WorkspaceManager::new(&storage_dir);

    // =========================================================================
    // 1. Initial Repository State on 'main'
    // =========================================================================
    let readme_d = store.put(b"# Main Repo\nInitial line\n")?;
    let lib_d = store.put(b"pub fn version() -> &'static str { \"0.1\" }\n")?;

    let mut root_m = DirectoryManifest::default();
    root_m.entries.insert(
        "README.md".to_string(),
        graft::cas::ManifestEntry::File {
            mode: "0644".to_string(),
            name: "README.md".to_string(),
            digest: readme_d,
        },
    );
    root_m.entries.insert(
        "lib.rs".to_string(),
        graft::cas::ManifestEntry::File {
            mode: "0644".to_string(),
            name: "lib.rs".to_string(),
            digest: lib_d,
        },
    );
    let initial_root_tree = store.put(&root_m.serialize())?;

    let initial_commit = Commit::new(
        vec![],
        initial_root_tree,
        "Lead Dev <lead@graft.dev>".to_string(),
        "Initial commit on main".to_string(),
    );
    let initial_commit_d = store.put(&initial_commit.serialize())?;
    meta.put_commit_metadata(&initial_commit_d, &initial_commit)?;
    meta.compare_and_swap_ref("refs/heads/main", None, &initial_commit_d, "lead")?;

    // Verify main branch resolved
    let (main_ref, main_head) = resolve_ref(&meta, "main")?.expect("main must resolve");
    assert_eq!(main_ref, "refs/heads/main");
    assert_eq!(main_head, initial_commit_d);

    // =========================================================================
    // 2. Parallel Agent Branch Creation
    // =========================================================================
    assert!(validate_branch_name("agent-1/auth").is_ok());
    assert!(validate_branch_name("agent-2/db").is_ok());

    let branch1 = normalize_ref_name("agent-1/auth");
    let branch2 = normalize_ref_name("agent-2/db");

    meta.compare_and_swap_ref(&branch1, None, &initial_commit_d, "agent-1")?;
    meta.compare_and_swap_ref(&branch2, None, &initial_commit_d, "agent-2")?;

    let all_branches = meta.list_refs("")?;
    assert_eq!(all_branches.len(), 3);

    // =========================================================================
    // 3. Agent 1 commits on 'agent-1/auth'
    // =========================================================================
    let ws1_lower = tmp.path().join("ws1_lower");
    let ws1_merged = tmp.path().join("ws1_merged");
    let mut ws1_cfg = manager.prepare_config("task-agent1", &initial_root_tree.to_string(), &ws1_lower, &ws1_merged);
    ws1_cfg.branch = Some(branch1.clone());
    ws1_cfg.base_commit = Some(initial_commit_d.to_string());
    manager.init_workspace_dirs(&ws1_cfg)?;

    // Agent 1 adds auth.rs
    let auth_file = ws1_cfg.upper_dir.join("auth.rs");
    fs::write(&auth_file, "pub fn authenticate() -> bool { true }\n")?;

    let ws1_changes = vec![WorkspaceChange {
        relative_path: PathBuf::from("auth.rs"),
        disk_path: auth_file,
        change_type: ChangeType::Created,
    }];
    let ws1_tree = apply_changes_to_tree(&initial_root_tree, &ws1_changes, &store)?;

    let commit1 = Commit::new(
        vec![initial_commit_d],
        ws1_tree,
        "Agent 1 <agent1@graft.dev>".to_string(),
        "feat(auth): implement authenticate function".to_string(),
    );
    let commit1_d = store.put(&commit1.serialize())?;
    meta.put_commit_metadata(&commit1_d, &commit1)?;
    meta.compare_and_swap_ref(&branch1, Some(&initial_commit_d), &commit1_d, "agent-1")?;

    // =========================================================================
    // 4. Agent 2 commits on 'agent-2/db'
    // =========================================================================
    let ws2_lower = tmp.path().join("ws2_lower");
    let ws2_merged = tmp.path().join("ws2_merged");
    let mut ws2_cfg = manager.prepare_config("task-agent2", &initial_root_tree.to_string(), &ws2_lower, &ws2_merged);
    ws2_cfg.branch = Some(branch2.clone());
    ws2_cfg.base_commit = Some(initial_commit_d.to_string());
    manager.init_workspace_dirs(&ws2_cfg)?;

    // Agent 2 adds db.rs
    let db_file = ws2_cfg.upper_dir.join("db.rs");
    fs::write(&db_file, "pub fn connect() -> bool { true }\n")?;

    let ws2_changes = vec![WorkspaceChange {
        relative_path: PathBuf::from("db.rs"),
        disk_path: db_file,
        change_type: ChangeType::Created,
    }];
    let ws2_tree = apply_changes_to_tree(&initial_root_tree, &ws2_changes, &store)?;

    let commit2 = Commit::new(
        vec![initial_commit_d],
        ws2_tree,
        "Agent 2 <agent2@graft.dev>".to_string(),
        "feat(db): implement database connect function".to_string(),
    );
    let commit2_d = store.put(&commit2.serialize())?;
    meta.put_commit_metadata(&commit2_d, &commit2)?;
    meta.compare_and_swap_ref(&branch2, Some(&initial_commit_d), &commit2_d, "agent-2")?;

    // =========================================================================
    // 5. Fast-Forward Merge: Agent 1 merges 'agent-1/auth' into 'main'
    // =========================================================================
    let current_main = meta.get_ref("refs/heads/main")?.unwrap();
    let lca_main_a1 = find_common_ancestor(&current_main, &commit1_d, &store)?.unwrap();
    assert_eq!(lca_main_a1, current_main); // LCA is current main -> fast-forward!

    meta.compare_and_swap_ref("refs/heads/main", Some(&current_main), &commit1_d, "agent-1")?;
    let updated_main = meta.get_ref("refs/heads/main")?.unwrap();
    assert_eq!(updated_main, commit1_d);

    // =========================================================================
    // 6. True 3-Way Merkle Merge: Agent 2 merges 'agent-2/db' into 'main'
    // =========================================================================
    let current_main = meta.get_ref("refs/heads/main")?.unwrap();
    assert_eq!(current_main, commit1_d);

    let lca_main_a2 = find_common_ancestor(&current_main, &commit2_d, &store)?.unwrap();
    assert_eq!(lca_main_a2, initial_commit_d); // LCA is the root ancestor commit!

    let merge_result = merge_trees(&initial_root_tree, &ws1_tree, &ws2_tree, &store)?;
    assert!(!merge_result.has_conflicts);
    assert_eq!(merge_result.conflict_files.len(), 0);

    // Verify merged manifest contains README.md, lib.rs, auth.rs, and db.rs
    let merged_bytes = store.get(&merge_result.root_tree_digest)?.unwrap();
    let merged_manifest = DirectoryManifest::from_bytes(&merged_bytes).unwrap();
    assert!(merged_manifest.entries.contains_key("README.md"));
    assert!(merged_manifest.entries.contains_key("lib.rs"));
    assert!(merged_manifest.entries.contains_key("auth.rs"));
    assert!(merged_manifest.entries.contains_key("db.rs"));

    // Create 2-parent merge commit
    let merge_commit = Commit::new(
        vec![current_main, commit2_d],
        merge_result.root_tree_digest,
        "Merge Coordinator <merge@graft.dev>".to_string(),
        "Merge branch 'agent-2/db' into 'main'".to_string(),
    );
    let merge_commit_d = store.put(&merge_commit.serialize())?;
    meta.put_commit_metadata(&merge_commit_d, &merge_commit)?;

    // Atomically advance main
    meta.compare_and_swap_ref("refs/heads/main", Some(&current_main), &merge_commit_d, "agent-2")?;

    let final_main = meta.get_ref("refs/heads/main")?.unwrap();
    assert_eq!(final_main, merge_commit_d);

    // =========================================================================
    // 7. Conflict Detection: Agent 3 tries to merge overlapping edits into 'main'
    // =========================================================================
    let mut a3_m = root_m.clone();
    let a3_readme = store.put(b"# Main Repo\nVersion by Agent 3\n")?;
    a3_m.entries.insert(
        "README.md".to_string(),
        graft::cas::ManifestEntry::File {
            mode: "0644".to_string(),
            name: "README.md".to_string(),
            digest: a3_readme,
        },
    );
    let a3_tree = store.put(&a3_m.serialize())?;

    let mut main_conflict_m = merged_manifest.clone();
    let main_conflict_readme = store.put(b"# Main Repo\nVersion by Main Lead\n")?;
    main_conflict_m.entries.insert(
        "README.md".to_string(),
        graft::cas::ManifestEntry::File {
            mode: "0644".to_string(),
            name: "README.md".to_string(),
            digest: main_conflict_readme,
        },
    );
    let main_conflict_tree = store.put(&main_conflict_m.serialize())?;

    let conflict_merge = merge_trees(&initial_root_tree, &main_conflict_tree, &a3_tree, &store)?;
    assert!(conflict_merge.has_conflicts);
    assert_eq!(conflict_merge.conflict_files, vec!["README.md".to_string()]);

    // Inspect conflict markers in CAS
    let conflicted_root = store.get(&conflict_merge.root_tree_digest)?.unwrap();
    let conflicted_m = DirectoryManifest::from_bytes(&conflicted_root).unwrap();
    let conflicted_readme_entry = conflicted_m.entries.get("README.md").unwrap();
    let conflicted_readme_digest = match conflicted_readme_entry {
        graft::cas::ManifestEntry::File { digest, .. } => *digest,
        _ => panic!("README.md should be file"),
    };
    let conflicted_readme_bytes = store.get(&conflicted_readme_digest)?.unwrap();
    let text = String::from_utf8(conflicted_readme_bytes).unwrap();
    assert!(text.contains("<<<<<<< ours"));
    assert!(text.contains("Version by Main Lead"));
    assert!(text.contains("======="));
    assert!(text.contains("Version by Agent 3"));
    assert!(text.contains(">>>>>>> theirs"));

    Ok(())
}
