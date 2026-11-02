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

//! # End-to-End Writeable Workspace Mount Workflow
//!
//! Validates the writeable Copy-on-Write (CoW) FUSE filesystem for AI agents:
//! 1. Ingestion of multi-level base repository snapshot into CAS.
//! 2. Initialization of an ephemeral agent workspace.
//! 3. Mounting the workspace as a writeable FUSE filesystem.
//! 4. Live modifications:
//!    - Modifying existing base file (triggers transparent copy-up).
//!    - Creating a new file in an existing directory.
//!    - Creating a new directory tree and file.
//!    - Deleting an existing base file (creates whiteout tombstone `.wh.<name>`).
//!    - Renaming an existing base file (whiteout old + copy-up new).
//! 5. Verification of live filesystem view in the mounted merged directory.
//! 6. Instant O(K) delta detection (`scan_changes` on workspace upper directory).
//! 7. Merkle commit generation from the active workspace changes.
//! 8. RAII unmount guard ensuring clean unmounting on test completion or panic.

#[cfg(feature = "fuse")]
#[test]
fn test_end_to_end_writeable_workspace_mount_lifecycle() -> Result<(), Box<dyn std::error::Error>> {
    use graft::cas::blob_store::{BlobStore, LocalBlobStore};
    use graft::cas::commit::{apply_changes_to_tree, Commit};
    use graft::git::ingest_directory;
    use graft::vfs::{spawn_mount_workspace_fuse, GraftWorkspaceFilesystem, VfsTree};
    use graft::workspace::{ChangeType, WorkspaceManager};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::tempdir;

    struct UnmountGuard {
        mount_path: PathBuf,
        session: Option<fuser::BackgroundSession>,
    }

    impl Drop for UnmountGuard {
        fn drop(&mut self) {
            if let Some(session) = self.session.take() {
                drop(session);
            }
            if WorkspaceManager::is_mounted(&self.mount_path) {
                let _ = std::process::Command::new("fusermount3")
                    .arg("-u")
                    .arg("-z")
                    .arg(&self.mount_path)
                    .status();
            }
        }
    }

    let sandbox = tempdir()?;
    let repo_src = sandbox.path().join("repo_src");
    let cas_dir = sandbox.path().join("cas");
    let storage_dir = sandbox.path().join("storage");
    let mount_dir = sandbox.path().join("mnt_workspace");

    fs::create_dir_all(repo_src.join("src/utils"))?;
    fs::create_dir_all(repo_src.join("scripts"))?;
    fs::create_dir_all(&mount_dir)?;

    // 1. Initial repo contents
    let readme_content = b"# Graft Virtualized Monorepo\nWriteable CoW Agent Workspaces.\n";
    fs::write(repo_src.join("README.md"), readme_content)?;

    let main_content = b"fn main() { println!(\"Version 1\"); }\n";
    fs::write(repo_src.join("src/main.rs"), main_content)?;

    let math_content = b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n";
    fs::write(repo_src.join("src/utils/math.rs"), math_content)?;

    let script_content = b"#!/usr/bin/env bash\necho 'Agent runner'\n";
    let script_path = repo_src.join("scripts/run.sh");
    fs::write(&script_path, script_content)?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;

    // 2. Ingest into CAS
    let blob_store: Arc<dyn BlobStore> = Arc::new(LocalBlobStore::new(&cas_dir)?);
    let ingest_res = ingest_directory(&repo_src, blob_store.as_ref())?;
    assert_eq!(ingest_res.total_files, 4);

    let commit1 = Commit::new(
        vec![],
        ingest_res.root_digest,
        "Engineer <eng@graft.dev>".to_string(),
        "Initial repo snapshot".to_string(),
    );
    let commit1_digest = blob_store.put(&commit1.serialize())?;
    let _ = commit1_digest;

    // 3. Prepare workspace configuration
    let ws_mgr = WorkspaceManager::new(&storage_dir);
    let config = ws_mgr.prepare_config(
        "agent-task-writeable",
        &ingest_res.root_digest.to_hex(),
        &repo_src,
        &mount_dir,
    );
    ws_mgr.init_workspace_dirs(&config)?;

    // 4. Mount writeable workspace filesystem
    let vfs_tree = VfsTree::from_cas(&ingest_res.root_digest, blob_store.as_ref())?;
    let ws_fs = GraftWorkspaceFilesystem::from_tree(vfs_tree, Arc::clone(&blob_store), &config.upper_dir);
    let session = spawn_mount_workspace_fuse(ws_fs, &mount_dir, None)?;
    let _guard = UnmountGuard {
        mount_path: mount_dir.clone(),
        session: Some(session),
    };

    // 5. Verify initial read-only state visible in mount point
    assert_eq!(fs::read(mount_dir.join("README.md"))?, readme_content);
    assert_eq!(fs::read(mount_dir.join("src/main.rs"))?, main_content);
    let script_mode = fs::metadata(mount_dir.join("scripts/run.sh"))?
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(script_mode, 0o755);

    // 6. Perform write operations inside the mounted directory:
    // a) Edit existing file
    let updated_main = b"fn main() { println!(\"Version 2: Agent Edits\"); }\n";
    fs::write(mount_dir.join("src/main.rs"), updated_main)?;
    assert_eq!(fs::read(mount_dir.join("src/main.rs"))?, updated_main);

    // b) Create new file in existing folder
    let helper_content = b"pub fn helper() -> bool { true }\n";
    fs::write(mount_dir.join("src/helper.rs"), helper_content)?;
    assert_eq!(fs::read(mount_dir.join("src/helper.rs"))?, helper_content);

    // c) Create new directory tree and file
    fs::create_dir_all(mount_dir.join("docs/specs"))?;
    let doc_content = b"Agent task specification v1\n";
    fs::write(mount_dir.join("docs/specs/spec.txt"), doc_content)?;
    assert_eq!(fs::read(mount_dir.join("docs/specs/spec.txt"))?, doc_content);

    // d) Delete an existing file from the base snapshot
    fs::remove_file(mount_dir.join("src/utils/math.rs"))?;
    assert!(!mount_dir.join("src/utils/math.rs").exists());

    // e) Rename an existing base file
    fs::rename(mount_dir.join("README.md"), mount_dir.join("README_RENAMED.md"))?;
    assert!(!mount_dir.join("README.md").exists());
    assert_eq!(fs::read(mount_dir.join("README_RENAMED.md"))?, readme_content);

    // 7. Verify directory readdir inside mount point reflects edits and deletions
    let mut root_entries: Vec<String> = fs::read_dir(&mount_dir)?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .collect();
    root_entries.sort();
    assert_eq!(root_entries, vec!["README_RENAMED.md", "docs", "scripts", "src"]);

    let mut src_entries: Vec<String> = fs::read_dir(mount_dir.join("src"))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .collect();
    src_entries.sort();
    assert_eq!(src_entries, vec!["helper.rs", "main.rs", "utils"]);

    let utils_entries: Vec<String> = fs::read_dir(mount_dir.join("src/utils"))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .collect();
    // math.rs was deleted, so utils should be empty of visible files
    assert!(utils_entries.is_empty());

    // 8. O(K) Delta Detection: Verify scan_changes on workspace configuration
    let changes = ws_mgr.scan_changes(&config)?;
    assert_eq!(changes.len(), 6);

    let main_change = changes
        .iter()
        .find(|c| c.relative_path == Path::new("src/main.rs"))
        .expect("must find src/main.rs change");
    assert_eq!(main_change.change_type, ChangeType::Modified);

    let helper_change = changes
        .iter()
        .find(|c| c.relative_path == Path::new("src/helper.rs"))
        .expect("must find src/helper.rs change");
    assert_eq!(helper_change.change_type, ChangeType::Created);

    let doc_change = changes
        .iter()
        .find(|c| c.relative_path == Path::new("docs/specs/spec.txt"))
        .expect("must find docs/specs/spec.txt change");
    assert_eq!(doc_change.change_type, ChangeType::Created);

    let del_math = changes
        .iter()
        .find(|c| c.relative_path == Path::new("src/utils/math.rs"))
        .expect("must find math.rs deletion");
    assert_eq!(del_math.change_type, ChangeType::Deleted);

    let del_readme = changes
        .iter()
        .find(|c| c.relative_path == Path::new("README.md"))
        .expect("must find README.md deletion (rename old)");
    assert_eq!(del_readme.change_type, ChangeType::Deleted);

    let ren_readme = changes
        .iter()
        .find(|c| c.relative_path == Path::new("README_RENAMED.md"))
        .expect("must find README_RENAMED.md creation (rename new)");
    assert_eq!(ren_readme.change_type, ChangeType::Created);

    // 9. Commit changes to CAS and verify new Merkle tree
    let new_tree_root = apply_changes_to_tree(&ingest_res.root_digest, &changes, blob_store.as_ref())?;
    assert_ne!(new_tree_root, ingest_res.root_digest);

    let commit2 = Commit::new(
        vec![commit1_digest],
        new_tree_root,
        "AI Agent <agent@graft.dev>".to_string(),
        "Agent finished task modifications".to_string(),
    );
    let commit2_digest = blob_store.put(&commit2.serialize())?;
    assert_ne!(commit1_digest, commit2_digest);

    // 10. Drop guard and verify clean unmount
    drop(_guard);

    // Verify mount point is now empty
    let entries_after = fs::read_dir(&mount_dir)?.count();
    assert_eq!(entries_after, 0);

    // Cleanup workspace
    ws_mgr.cleanup(&config)?;
    assert!(!config.upper_dir.exists());

    Ok(())
}
