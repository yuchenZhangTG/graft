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

//! # Agent Workspace Orchestration & O(K) Diff Scanner
//!
//! This module manages the lifecycle of ephemeral agent workspaces using the **Hybrid Mount**
//! architecture (inspired by Google CitC and Anthropic Scion).
//!
//! ### The Hybrid Mount Architecture:
//! - **`lowerdir`**: Read-only FUSE mount pointing to a repository commit tree.
//!   Files are hydrated lazily from CAS on demand.
//! - **`upperdir`**: A local NVMe directory dedicated to this workspace.
//!   All file modifications, newly created files, and compiler build outputs land here.
//! - **`workdir`**: Internal scratch space required by the Linux kernel OverlayFS driver.
//! - **`mergeddir`**: The unified directory exposed to the agent and compilers.
//!
//! ### Why this gives O(K) Instant Diff Detection:
//! In standard Git, checking `git status` or `git diff` requires calling `lstat()` on every
//! single file in the repository (O(N) operations, which can be 100,000+ files).
//!
//! In Graft:
//! - Untouched files NEVER enter `upperdir`.
//! - Any file edited or created by the agent physically exists inside `upperdir`.
//! - Any file deleted by the agent is represented by the kernel as a **whiteout character device**
//!   (`mknod c 0 0`) inside `upperdir`.
//!
//! Therefore, discovering the exact changeset only requires scanning `upperdir` (O(K) operations,
//! where K is the number of touched files). If an agent edits 2 files in a 200,000-file repository,
//! Graft detects the diff in under 1 millisecond.

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::{resolve_root_tree, DirectoryManifest, ManifestEntry};
use crate::cas::digest::Digest;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

/// The type of modification detected for a file in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeType {
    /// File was created by the agent and did not exist in the base commit.
    Created,
    /// File existed in the base commit and was modified by the agent.
    Modified,
    /// File existed in the base commit and was deleted by the agent (whiteout tombstone).
    Deleted,
}

/// A changed entry detected in the workspace's Copy-on-Write upper layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceChange {
    /// Path relative to the root of the repository (e.g. "src/main.rs").
    pub relative_path: PathBuf,
    /// Absolute path to the modified payload on the local filesystem.
    pub disk_path: PathBuf,
    /// Type of change: Created, Modified, or Deleted.
    pub change_type: ChangeType,
}

/// Directory configuration for an ephemeral agent workspace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceConfig {
    /// Unique task/workspace identifier (e.g. "agent-task-42").
    pub id: String,
    /// Base commit or Merkle tree hash this workspace was branched from.
    pub commit: String,
    /// Path to the read-only base layer (e.g. FUSE mount).
    pub lower_dir: PathBuf,
    /// Path to the mutable Copy-on-Write upper directory.
    pub upper_dir: PathBuf,
    /// Path to the internal OverlayFS work directory.
    pub work_dir: PathBuf,
    /// Path to the merged mount directory exposed to the agent.
    pub merged_dir: PathBuf,
    /// Optional agent identifier holding this workspace.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional branch reference being tracked by this workspace (e.g. "refs/heads/feature-1").
    #[serde(default)]
    pub branch: Option<String>,
    /// Base commit digest this workspace was branched from or last committed to.
    #[serde(default)]
    pub base_commit: Option<String>,
    /// PID of background mount daemon process if mounted.
    #[serde(default)]
    pub mount_pid: Option<u32>,
    /// Mount backend used: "fuse" or "overlay".
    #[serde(default)]
    pub mount_backend: Option<String>,
    /// Path where the workspace is currently mounted.
    #[serde(default)]
    pub mount_target: Option<PathBuf>,
}

/// In-progress state of an interactive workspace cherry-pick with merge conflicts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CherryPickState {
    /// Commit digest or revision spec that was cherry-picked.
    pub source_commit: String,
    /// Original or specified commit message.
    pub original_message: String,
    /// Author string for the resulting commit.
    pub original_author: String,
    /// Relative file paths that encountered 3-way merge conflicts.
    pub conflict_files: Vec<String>,
    /// All relative file paths modified, created, or deleted by the cherry-pick.
    pub applied_files: Vec<String>,
    /// Conflict resolution strategy applied: "markers", "ours", "theirs", or "fail".
    pub strategy: String,
    /// Tracked branch name if applicable.
    pub target_branch: Option<String>,
    /// Unix timestamp when the cherry-pick was initiated.
    pub timestamp: u64,
}

/// Manages the lifecycle and delta extraction of agent workspaces.
pub struct WorkspaceManager {
    base_storage_dir: PathBuf,
}

impl WorkspaceManager {
    /// Create a manager using `base_dir` to store ephemeral workspace upper/work directories.
    pub fn new<P: AsRef<Path>>(base_dir: P) -> Self {
        Self {
            base_storage_dir: base_dir.as_ref().to_path_buf(),
        }
    }

    /// Prepares directory path configurations for a new workspace.
    pub fn prepare_config(
        &self,
        workspace_id: &str,
        commit: &str,
        lower_dir: &Path,
        merged_dir: &Path,
    ) -> WorkspaceConfig {
        let ws_root = self.base_storage_dir.join(workspace_id);
        WorkspaceConfig {
            id: workspace_id.to_string(),
            commit: commit.to_string(),
            lower_dir: lower_dir.to_path_buf(),
            upper_dir: ws_root.join("upper"),
            work_dir: ws_root.join("work"),
            merged_dir: merged_dir.to_path_buf(),
            agent_id: None,
            branch: None,
            base_commit: None,
            mount_pid: None,
            mount_backend: None,
            mount_target: None,
        }
    }

    /// Saves workspace state configuration to `<storage_dir>/<id>/workspace.json`.
    pub fn save_config(&self, config: &WorkspaceConfig) -> io::Result<()> {
        let ws_root = self.base_storage_dir.join(&config.id);
        fs::create_dir_all(&ws_root)?;
        let json_path = ws_root.join("workspace.json");
        let json_str = serde_json::to_string_pretty(config)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        fs::write(json_path, json_str)?;
        Ok(())
    }

    /// Loads workspace state configuration from `<storage_dir>/<id>/workspace.json`.
    pub fn load_config(&self, id: &str) -> io::Result<WorkspaceConfig> {
        let json_path = self.base_storage_dir.join(id).join("workspace.json");
        if !json_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Workspace '{}' not found at '{}'. Run 'graft workspace create' first.",
                    id,
                    json_path.display()
                ),
            ));
        }
        let json_str = fs::read_to_string(json_path)?;
        let config: WorkspaceConfig = serde_json::from_str(&json_str)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(config)
    }

    /// Lists all existing workspace configurations in `<storage_dir>/*/workspace.json`.
    pub fn list_workspaces(&self) -> io::Result<Vec<WorkspaceConfig>> {
        let mut configs = Vec::new();
        if !self.base_storage_dir.exists() {
            return Ok(configs);
        }

        for entry in fs::read_dir(&self.base_storage_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let json_path = path.join("workspace.json");
                if json_path.exists() {
                    if let Ok(json_str) = fs::read_to_string(&json_path) {
                        if let Ok(config) = serde_json::from_str::<WorkspaceConfig>(&json_str) {
                            configs.push(config);
                        }
                    }
                }
            }
        }

        configs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(configs)
    }

    /// Finds a workspace configuration by workspace ID, mount path, or directory name.
    pub fn find_workspace(&self, id_or_path: &str) -> io::Result<WorkspaceConfig> {
        // 1. Direct ID match
        if let Ok(config) = self.load_config(id_or_path) {
            return Ok(config);
        }

        // 2. Search through all existing workspaces
        let workspaces = self.list_workspaces()?;
        let query_path = Path::new(id_or_path);
        let query_canon = query_path.canonicalize().ok();

        for ws in workspaces {
            if ws.id == id_or_path {
                return Ok(ws);
            }
            if let Some(ref target) = ws.mount_target {
                if target == query_path
                    || target.to_string_lossy() == id_or_path
                    || target.file_name().and_then(|n| n.to_str()) == Some(id_or_path)
                {
                    return Ok(ws);
                }
                if let (Some(ref q), Ok(t_canon)) = (&query_canon, target.canonicalize()) {
                    if q == &t_canon {
                        return Ok(ws);
                    }
                }
            }
            if ws.merged_dir == query_path
                || ws.merged_dir.to_string_lossy() == id_or_path
                || ws.merged_dir.file_name().and_then(|n| n.to_str()) == Some(id_or_path)
            {
                return Ok(ws);
            }
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Workspace '{}' not found in '{}'. Run 'graft workspace list' to view active workspaces.",
                id_or_path,
                self.base_storage_dir.display()
            ),
        ))
    }

    /// Saves in-progress cherry-pick conflict state to `<storage_dir>/<id>/cherry_pick.json`.
    pub fn save_cherry_pick_state(&self, workspace_id: &str, state: &CherryPickState) -> io::Result<()> {
        let ws_root = self.base_storage_dir.join(workspace_id);
        fs::create_dir_all(&ws_root)?;
        let json_path = ws_root.join("cherry_pick.json");
        let json_str = serde_json::to_string_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        fs::write(json_path, json_str)?;
        Ok(())
    }

    /// Loads in-progress cherry-pick conflict state from `<storage_dir>/<id>/cherry_pick.json`.
    pub fn load_cherry_pick_state(&self, workspace_id: &str) -> io::Result<Option<CherryPickState>> {
        let json_path = self.base_storage_dir.join(workspace_id).join("cherry_pick.json");
        if !json_path.exists() {
            return Ok(None);
        }
        let json_str = fs::read_to_string(json_path)?;
        let state: CherryPickState = serde_json::from_str(&json_str)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(state))
    }

    /// Deletes `<storage_dir>/<id>/cherry_pick.json` once conflicts are resolved or aborted.
    pub fn clear_cherry_pick_state(&self, workspace_id: &str) -> io::Result<()> {
        let json_path = self.base_storage_dir.join(workspace_id).join("cherry_pick.json");
        if json_path.exists() {
            fs::remove_file(json_path)?;
        }
        Ok(())
    }

    /// Creates directory trees for `upper` and `work` directories on local disk and persists `workspace.json`.
    pub fn init_workspace_dirs(&self, config: &WorkspaceConfig) -> io::Result<()> {
        fs::create_dir_all(&config.upper_dir)?;
        fs::create_dir_all(&config.work_dir)?;
        fs::create_dir_all(&config.merged_dir)?;
        self.save_config(config)?;
        Ok(())
    }

    /// Scans the upper directory to extract the workspace changeset in O(K) time.
    ///
    /// - Checks `upper_dir` recursively.
    /// - If a file is a character device (OverlayFS whiteout), it marks it as `Deleted`.
    /// - If a file exists in `lower_dir`, it marks it as `Modified`.
    /// - Otherwise, it marks it as `Created`.
    /// Scans the upper directory to extract the workspace changeset in O(K) time.
    ///
    /// - Checks `upper_dir` recursively.
    /// - If a file is a character device (OverlayFS whiteout), it marks it as `Deleted`.
    /// - If a file exists in `lower_dir`, it marks it as `Modified`.
    /// - Otherwise, it marks it as `Created`.
    pub fn scan_changes(&self, config: &WorkspaceConfig) -> io::Result<Vec<WorkspaceChange>> {
        self.scan_changes_with_store::<crate::cas::blob_store::LocalBlobStore>(config, None)
    }

    /// Scans the workspace for modified, created, and deleted files.
    ///
    /// When `blob_store` is provided and `config.commit` is a valid tree or commit digest,
    /// this compares the files in `upper_dir` directly against the Merkle tree in CAS.
    ///
    /// This provides critical guarantees for live virtualized FUSE workspaces:
    /// 1. Untouched copy-ups and files committed in previous commits whose content
    ///    matches the base Merkle tree are recognized as clean (skipped).
    /// 2. Whiteouts (`.wh.<file>` or char dev 0,0) only emit `ChangeType::Deleted` if the
    ///    file actually existed in the base Merkle tree (preventing post-commit deleted files
    ///    from staying permanently dirty).
    /// 3. Mode changes (e.g. `chmod +x`) are accurately detected.
    /// 4. Opaque directory markers (`.wh..wh..opq`) emit deletions for base tree children
    ///    missing from `upper_dir`.
    pub fn scan_changes_with_store<B: BlobStore + ?Sized>(
        &self,
        config: &WorkspaceConfig,
        blob_store: Option<&B>,
    ) -> io::Result<Vec<WorkspaceChange>> {
        let mut changes = Vec::new();
        if !config.upper_dir.exists() {
            return Ok(changes);
        }

        // Try resolving base root tree digest from CAS if blob_store is available
        let base_root = if let Some(store) = blob_store {
            if !config.commit.is_empty() && config.commit != "unknown" {
                resolve_root_tree(&config.commit, store).ok().flatten()
            } else {
                None
            }
        } else {
            None
        };

        if let (Some(store), Some(root_digest)) = (blob_store, base_root) {
            let mut manifest_cache = HashMap::new();
            self.walk_upper_dir_cas(
                &config.upper_dir,
                &config.upper_dir,
                &root_digest,
                store,
                &mut manifest_cache,
                &mut changes,
            )?;
        } else {
            self.walk_upper_dir(
                &config.upper_dir,
                &config.upper_dir,
                &config.lower_dir,
                &mut changes,
            )?;
        }

        Ok(changes)
    }

    /// Traverses `upper_dir` comparing against the CAS Merkle tree.
    fn walk_upper_dir_cas<B: BlobStore + ?Sized>(
        &self,
        current_dir: &Path,
        upper_root: &Path,
        root_digest: &Digest,
        blob_store: &B,
        manifest_cache: &mut HashMap<PathBuf, Option<DirectoryManifest>>,
        changes: &mut Vec<WorkspaceChange>,
    ) -> io::Result<()> {
        let rel_dir = current_dir
            .strip_prefix(upper_root)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // 1. Check for opaque directory marker: .wh..wh..opq
        let has_opaque = current_dir.join(".wh..wh..opq").exists();
        if has_opaque {
            if let Some(base_manifest) =
                self.get_directory_manifest(root_digest, rel_dir, blob_store, manifest_cache)?
            {
                for (name, _) in &base_manifest.entries {
                    if !current_dir.join(name).exists()
                        && !current_dir.join(format!(".wh.{}", name)).exists()
                    {
                        let rel_entry_path = if rel_dir.as_os_str().is_empty() {
                            PathBuf::from(name)
                        } else {
                            rel_dir.join(name)
                        };
                        changes.push(WorkspaceChange {
                            relative_path: rel_entry_path,
                            disk_path: current_dir.join(name),
                            change_type: ChangeType::Deleted,
                        });
                    }
                }
            }
        }

        // 2. Iterate through entries in current_dir
        for entry in fs::read_dir(current_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            if name_str == ".wh..wh..opq" {
                continue;
            }

            // User-space whiteout marker: .wh.<filename>
            if let Some(deleted_name) = name_str.strip_prefix(".wh.") {
                if current_dir.join(deleted_name).exists() {
                    continue;
                }
                let deleted_rel = if rel_dir.as_os_str().is_empty() {
                    PathBuf::from(deleted_name)
                } else {
                    rel_dir.join(deleted_name)
                };

                // Critical: Only emit Deleted if the file actually existed in base tree!
                let entry_in_base =
                    self.lookup_in_base(root_digest, &deleted_rel, blob_store, manifest_cache)?;
                if entry_in_base.is_some() {
                    changes.push(WorkspaceChange {
                        relative_path: deleted_rel,
                        disk_path: path,
                        change_type: ChangeType::Deleted,
                    });
                }
                continue;
            }

            let relative_path = path
                .strip_prefix(upper_root)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
                .to_path_buf();

            if file_type.is_dir() {
                self.walk_upper_dir_cas(
                    &path,
                    upper_root,
                    root_digest,
                    blob_store,
                    manifest_cache,
                    changes,
                )?;
            } else if file_type.is_char_device() {
                // Kernel whiteout character device (rdev = 0)
                let entry_in_base =
                    self.lookup_in_base(root_digest, &relative_path, blob_store, manifest_cache)?;
                if entry_in_base.is_some() {
                    changes.push(WorkspaceChange {
                        relative_path,
                        disk_path: path,
                        change_type: ChangeType::Deleted,
                    });
                }
            } else if file_type.is_file() || file_type.is_symlink() {
                let metadata = fs::metadata(&path)?;
                use std::os::unix::fs::PermissionsExt;
                let is_exec = (metadata.permissions().mode() & 0o111) != 0;
                let upper_mode = if is_exec { "0755" } else { "0644" };

                let entry_in_base =
                    self.lookup_in_base(root_digest, &relative_path, blob_store, manifest_cache)?;
                match entry_in_base {
                    Some(ManifestEntry::File { mode, digest, .. }) => {
                        let is_clean =
                            if upper_mode == mode && metadata.len() == digest.size_bytes as u64 {
                                match Digest::from_file(&path) {
                                    Ok(disk_digest) => disk_digest.hash == digest.hash,
                                    Err(_) => false,
                                }
                            } else {
                                false
                            };

                        if !is_clean {
                            changes.push(WorkspaceChange {
                                relative_path,
                                disk_path: path,
                                change_type: ChangeType::Modified,
                            });
                        }
                    }
                    Some(ManifestEntry::ChunkedFile {
                        mode,
                        manifest_digest,
                        total_size,
                        ..
                    }) => {
                        let is_clean = if upper_mode == mode && metadata.len() == total_size {
                            if let Ok(Some(m_bytes)) = blob_store.get(&manifest_digest) {
                                if let Ok(chunk_manifest) =
                                    crate::cas::chunker::ChunkManifest::from_bytes(&m_bytes)
                                {
                                    match Digest::from_file(&path) {
                                        Ok(disk_digest) => {
                                            disk_digest.to_hex()
                                                == chunk_manifest.whole_file_blake3_hex
                                        }
                                        Err(_) => false,
                                    }
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if !is_clean {
                            changes.push(WorkspaceChange {
                                relative_path,
                                disk_path: path,
                                change_type: ChangeType::Modified,
                            });
                        }
                    }
                    Some(ManifestEntry::Directory { .. }) => {
                        changes.push(WorkspaceChange {
                            relative_path,
                            disk_path: path,
                            change_type: ChangeType::Modified,
                        });
                    }
                    None => {
                        changes.push(WorkspaceChange {
                            relative_path,
                            disk_path: path,
                            change_type: ChangeType::Created,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Fetches and memoizes a DirectoryManifest at `rel_dir`.
    fn get_directory_manifest<B: BlobStore + ?Sized>(
        &self,
        root_digest: &Digest,
        rel_dir: &Path,
        blob_store: &B,
        cache: &mut HashMap<PathBuf, Option<DirectoryManifest>>,
    ) -> io::Result<Option<DirectoryManifest>> {
        if let Some(cached) = cache.get(rel_dir) {
            return Ok(cached.clone());
        }

        let manifest = if rel_dir.as_os_str().is_empty() {
            if let Some(bytes) = blob_store.get(root_digest)? {
                DirectoryManifest::from_bytes(&bytes).ok()
            } else {
                None
            }
        } else {
            let parent_dir = rel_dir.parent().unwrap_or_else(|| Path::new(""));
            let dir_name = rel_dir.file_name().unwrap().to_string_lossy().to_string();
            let parent_manifest =
                self.get_directory_manifest(root_digest, parent_dir, blob_store, cache)?;
            if let Some(pm) = parent_manifest {
                if let Some(ManifestEntry::Directory { digest, .. }) = pm.entries.get(&dir_name) {
                    if let Some(bytes) = blob_store.get(digest)? {
                        DirectoryManifest::from_bytes(&bytes).ok()
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };

        cache.insert(rel_dir.to_path_buf(), manifest.clone());
        Ok(manifest)
    }

    /// Looks up a file/dir ManifestEntry in the base Merkle tree.
    fn lookup_in_base<B: BlobStore + ?Sized>(
        &self,
        root_digest: &Digest,
        rel_path: &Path,
        blob_store: &B,
        cache: &mut HashMap<PathBuf, Option<DirectoryManifest>>,
    ) -> io::Result<Option<ManifestEntry>> {
        let parent_dir = rel_path.parent().unwrap_or_else(|| Path::new(""));
        let file_name = match rel_path.file_name() {
            Some(f) => f.to_string_lossy().to_string(),
            None => return Ok(None),
        };
        let manifest = self.get_directory_manifest(root_digest, parent_dir, blob_store, cache)?;
        if let Some(m) = manifest {
            Ok(m.entries.get(&file_name).cloned())
        } else {
            Ok(None)
        }
    }

    /// Prunes files and whiteout markers from `upper_dir` that are identical to `config.commit`.
    ///
    /// SAFETY: This must ONLY be invoked when the workspace is unmounted!
    pub fn prune_clean_upper_files<B: BlobStore + ?Sized>(
        &self,
        config: &WorkspaceConfig,
        blob_store: &B,
    ) -> io::Result<usize> {
        if !config.upper_dir.exists() {
            return Ok(0);
        }
        let root_digest = match resolve_root_tree(&config.commit, blob_store)? {
            Some(d) => d,
            None => return Ok(0),
        };

        let mut manifest_cache = HashMap::new();
        let mut pruned = 0;
        self.prune_dir_recursive(
            &config.upper_dir,
            &config.upper_dir,
            &root_digest,
            blob_store,
            &mut manifest_cache,
            &mut pruned,
        )?;
        Ok(pruned)
    }

    fn prune_dir_recursive<B: BlobStore + ?Sized>(
        &self,
        current_dir: &Path,
        upper_root: &Path,
        root_digest: &Digest,
        blob_store: &B,
        cache: &mut HashMap<PathBuf, Option<DirectoryManifest>>,
        pruned: &mut usize,
    ) -> io::Result<bool> {
        let rel_dir = current_dir
            .strip_prefix(upper_root)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut has_remaining_entries = false;

        for entry in fs::read_dir(current_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            if name_str == ".wh..wh..opq" {
                has_remaining_entries = true;
                continue;
            }

            if let Some(deleted_name) = name_str.strip_prefix(".wh.") {
                let deleted_rel = if rel_dir.as_os_str().is_empty() {
                    PathBuf::from(deleted_name)
                } else {
                    rel_dir.join(deleted_name)
                };
                let entry_in_base =
                    self.lookup_in_base(root_digest, &deleted_rel, blob_store, cache)?;
                if entry_in_base.is_none() {
                    let _ = fs::remove_file(&path);
                    *pruned += 1;
                } else {
                    has_remaining_entries = true;
                }
                continue;
            }

            let rel_path = if rel_dir.as_os_str().is_empty() {
                PathBuf::from(&*name_str)
            } else {
                rel_dir.join(&*name_str)
            };

            if file_type.is_dir() {
                let sub_has_entries = self.prune_dir_recursive(
                    &path,
                    upper_root,
                    root_digest,
                    blob_store,
                    cache,
                    pruned,
                )?;
                if !sub_has_entries {
                    let _ = fs::remove_dir(&path);
                } else {
                    has_remaining_entries = true;
                }
            } else if file_type.is_file() {
                let metadata = fs::metadata(&path)?;
                use std::os::unix::fs::PermissionsExt;
                let is_exec = (metadata.permissions().mode() & 0o111) != 0;
                let upper_mode = if is_exec { "0755" } else { "0644" };

                let entry_in_base =
                    self.lookup_in_base(root_digest, &rel_path, blob_store, cache)?;
                let can_prune = match entry_in_base {
                    Some(ManifestEntry::File { mode, digest, .. }) => {
                        upper_mode == mode
                            && metadata.len() == digest.size_bytes as u64
                            && Digest::from_file(&path)
                                .map(|d| d.hash == digest.hash)
                                .unwrap_or(false)
                    }
                    Some(ManifestEntry::ChunkedFile {
                        mode,
                        manifest_digest,
                        total_size,
                        ..
                    }) => {
                        if upper_mode == mode && metadata.len() == total_size {
                            if let Ok(Some(m_bytes)) = blob_store.get(&manifest_digest) {
                                if let Ok(cm) =
                                    crate::cas::chunker::ChunkManifest::from_bytes(&m_bytes)
                                {
                                    Digest::from_file(&path)
                                        .map(|d| d.to_hex() == cm.whole_file_blake3_hex)
                                        .unwrap_or(false)
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    }
                    _ => false,
                };

                if can_prune {
                    let _ = fs::remove_file(&path);
                    *pruned += 1;
                } else {
                    has_remaining_entries = true;
                }
            } else {
                has_remaining_entries = true;
            }
        }

        Ok(has_remaining_entries)
    }

    /// Recursively traverses `upper_dir` to collect modified files.
    fn walk_upper_dir(
        &self,
        current_dir: &Path,
        upper_root: &Path,
        lower_root: &Path,
        changes: &mut Vec<WorkspaceChange>,
    ) -> io::Result<()> {
        for entry in fs::read_dir(current_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            // Opaque directory marker: .wh..wh..opq
            if name_str == ".wh..wh..opq" {
                continue;
            }

            // User-space whiteout marker: .wh.<filename>
            if let Some(deleted_name) = name_str.strip_prefix(".wh.") {
                // If a non-whiteout file with the same name also exists in current_dir,
                // the file was deleted and subsequently re-created. Skip the whiteout marker.
                if current_dir.join(deleted_name).exists() {
                    continue;
                }
                let relative_dir = current_dir
                    .strip_prefix(upper_root)
                    .map_err(|e| io::Error::other(e))?;
                let deleted_rel = relative_dir.join(deleted_name);
                changes.push(WorkspaceChange {
                    relative_path: deleted_rel,
                    disk_path: path,
                    change_type: ChangeType::Deleted,
                });
                continue;
            }

            // Compute relative path within the repo (e.g. "src/main.rs")
            let relative_path = path
                .strip_prefix(upper_root)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
                .to_path_buf();

            if file_type.is_dir() {
                self.walk_upper_dir(&path, upper_root, lower_root, changes)?;
            } else if file_type.is_char_device() {
                // In Linux OverlayFS, when a file from lowerdir is deleted in the merged view,
                // the kernel creates a special whiteout character device (rdev = 0) in upperdir.
                changes.push(WorkspaceChange {
                    relative_path,
                    disk_path: path,
                    change_type: ChangeType::Deleted,
                });
            } else if file_type.is_file() || file_type.is_symlink() {
                // If the file already existed in lowerdir, it's a modification; otherwise it's newly created.
                let corresponding_lower = lower_root.join(&relative_path);
                let change_type = if corresponding_lower.exists() {
                    ChangeType::Modified
                } else {
                    ChangeType::Created
                };

                changes.push(WorkspaceChange {
                    relative_path,
                    disk_path: path,
                    change_type,
                });
            }
        }
        Ok(())
    }

    /// Checks whether the given path is currently mounted by querying `/proc/mounts`.
    pub fn is_mounted<P: AsRef<Path>>(mountpoint: P) -> bool {
        let target = mountpoint.as_ref();
        let target_str = target.to_string_lossy();
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if parts[1] == target_str.as_ref() {
                        return true;
                    }
                    if let Ok(canon) = fs::canonicalize(parts[1]) {
                        if canon == target {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Unmounts an active workspace if mounted, terminating background daemons cleanly.
    pub fn unmount_workspace(&self, config: &mut WorkspaceConfig, lazy: bool) -> io::Result<()> {
        let mount_target = config
            .mount_target
            .clone()
            .unwrap_or_else(|| config.merged_dir.clone());

        // 1. If a background daemon PID was recorded, verify command line and terminate safely
        if let Some(pid) = config.mount_pid {
            let cmdline_path = format!("/proc/{}/cmdline", pid);
            if let Ok(cmdline) = fs::read_to_string(&cmdline_path) {
                if cmdline.contains("graft") {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                    // Wait up to 1 second for daemon process exit
                    let start = std::time::Instant::now();
                    while start.elapsed() < std::time::Duration::from_millis(1000) {
                        if !Path::new(&cmdline_path).exists() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
        }

        // 2. If target directory is still mounted, invoke fusermount3 or umount
        if Self::is_mounted(&mount_target) {
            let mut cmd = std::process::Command::new("fusermount3");
            if lazy {
                cmd.arg("-u").arg("-z").arg(&mount_target);
            } else {
                cmd.arg("-u").arg(&mount_target);
            }

            let mut unmounted = match cmd.status() {
                Ok(s) => s.success(),
                Err(_) => false,
            };

            if !unmounted {
                // Fallback to standard umount (e.g. for kernel overlay mounts or root)
                let mut ucmd = std::process::Command::new("umount");
                if lazy {
                    ucmd.arg("-l").arg(&mount_target);
                } else {
                    ucmd.arg(&mount_target);
                }
                if let Ok(s) = ucmd.status() {
                    unmounted = s.success();
                }
            }

            let _ = unmounted;
        }

        config.mount_pid = None;
        config.mount_backend = None;
        // Keep mount_target so workspace retains its target directory path for future mounts, list, and destroy
        self.save_config(config)?;
        Ok(())
    }

    /// Cleans up workspace upper and work directories when the task finishes.
    pub fn cleanup(&self, config: &WorkspaceConfig) -> io::Result<()> {
        let mount_target = config
            .mount_target
            .clone()
            .unwrap_or_else(|| config.merged_dir.clone());
        if Self::is_mounted(&mount_target) {
            let mut conf_clone = config.clone();
            let _ = self.unmount_workspace(&mut conf_clone, true);
        }

        if let Some(ref target) = config.mount_target {
            if target.exists() && target.is_dir() {
                // Protect against accidental root or home directory deletion
                if target != Path::new("/") && target.parent().is_some() {
                    let _ = fs::remove_dir_all(target);
                }
            }
        }

        let ws_root = self.base_storage_dir.join(&config.id);
        if ws_root.exists() {
            fs::remove_dir_all(&ws_root)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_scan_changes_created_and_modified() -> io::Result<()> {
        let tmp = tempdir()?;
        let mgr = WorkspaceManager::new(tmp.path().join("storage"));

        let lower = tmp.path().join("lower");
        let merged = tmp.path().join("merged");
        fs::create_dir_all(&lower)?;

        // Existing file in base
        let base_file = lower.join("existing.txt");
        let mut f = fs::File::create(&base_file)?;
        write!(f, "initial version")?;

        let config = mgr.prepare_config("test-task", "commit-1", &lower, &merged);
        mgr.init_workspace_dirs(&config)?;

        // Simulate agent editing existing file in upper
        let modified_file = config.upper_dir.join("existing.txt");
        let mut f = fs::File::create(&modified_file)?;
        write!(f, "modified version")?;

        // Simulate agent creating a new file in upper
        let new_file = config.upper_dir.join("new_feature.rs");
        let mut f = fs::File::create(&new_file)?;
        write!(f, "fn new_feature() {{}}")?;

        let changes = mgr.scan_changes(&config)?;
        assert_eq!(changes.len(), 2);

        let modified_change = changes
            .iter()
            .find(|c| c.relative_path == Path::new("existing.txt"))
            .expect("must find existing.txt");
        assert_eq!(modified_change.change_type, ChangeType::Modified);

        let created_change = changes
            .iter()
            .find(|c| c.relative_path == Path::new("new_feature.rs"))
            .expect("must find new_feature.rs");
        assert_eq!(created_change.change_type, ChangeType::Created);

        mgr.cleanup(&config)?;
        assert!(!config.upper_dir.exists());

        Ok(())
    }

    #[test]
    fn test_workspace_config_persistence() -> io::Result<()> {
        let tmp = tempdir()?;
        let mgr = WorkspaceManager::new(tmp.path().join("storage"));
        let lower = tmp.path().join("lower");
        let merged = tmp.path().join("merged");

        let config = mgr.prepare_config("task-saved", "commit-abc123", &lower, &merged);
        mgr.init_workspace_dirs(&config)?;

        let loaded = mgr.load_config("task-saved")?;
        assert_eq!(loaded.id, "task-saved");
        assert_eq!(loaded.commit, "commit-abc123");
        assert_eq!(loaded.lower_dir, lower);
        assert_eq!(loaded.merged_dir, merged);

        assert!(mgr.load_config("non-existent-task").is_err());
        Ok(())
    }

    #[test]
    fn test_scan_changes_whiteout_deletion_and_recreation() -> io::Result<()> {
        let tmp = tempdir()?;
        let mgr = WorkspaceManager::new(tmp.path().join("storage"));
        let lower = tmp.path().join("lower");
        let merged = tmp.path().join("merged");
        fs::create_dir_all(&lower)?;

        // Base file
        fs::write(lower.join("to_delete.txt"), "hello world")?;
        fs::write(lower.join("to_recreate.txt"), "initial")?;

        let config = mgr.prepare_config("task-whiteout", "commit-w", &lower, &merged);
        mgr.init_workspace_dirs(&config)?;

        // 1. Create whiteout marker for to_delete.txt
        fs::File::create(config.upper_dir.join(".wh.to_delete.txt"))?;

        // 2. Create whiteout marker for to_recreate.txt, but ALSO write to_recreate.txt (re-created)
        fs::File::create(config.upper_dir.join(".wh.to_recreate.txt"))?;
        fs::write(config.upper_dir.join("to_recreate.txt"), "recreated content")?;

        // 3. Create opaque directory marker .wh..wh..opq (should be ignored)
        fs::create_dir_all(config.upper_dir.join("sub"))?;
        fs::File::create(config.upper_dir.join("sub/.wh..wh..opq"))?;

        let changes = mgr.scan_changes(&config)?;

        // Should have exactly 2 changes:
        // - to_delete.txt -> Deleted
        // - to_recreate.txt -> Modified (since it existed in lower)
        // .wh..wh..opq must NOT appear
        assert_eq!(changes.len(), 2);

        let del_change = changes
            .iter()
            .find(|c| c.relative_path == Path::new("to_delete.txt"))
            .expect("must find to_delete.txt");
        assert_eq!(del_change.change_type, ChangeType::Deleted);

        let rec_change = changes
            .iter()
            .find(|c| c.relative_path == Path::new("to_recreate.txt"))
            .expect("must find to_recreate.txt");
        assert_eq!(rec_change.change_type, ChangeType::Modified);

        Ok(())
    }

    #[test]
    fn test_find_workspace_and_cleanup_mount_target() -> io::Result<()> {
        let tmp = tempdir()?;
        let mgr = WorkspaceManager::new(tmp.path().join("storage"));
        let lower = tmp.path().join("lower");
        let merged = tmp.path().join("merged");
        let target = tmp.path().join("my-task-mount");
        fs::create_dir_all(&target)?;

        let mut config = mgr.prepare_config("task-find", "commit-123", &lower, &merged);
        config.mount_target = Some(target.clone());
        mgr.save_config(&config)?;

        // Find by exact workspace ID
        let ws_by_id = mgr.find_workspace("task-find")?;
        assert_eq!(ws_by_id.id, "task-find");

        // Find by target directory name
        let ws_by_name = mgr.find_workspace("my-task-mount")?;
        assert_eq!(ws_by_name.id, "task-find");

        // Find by full path
        let ws_by_path = mgr.find_workspace(&target.to_string_lossy())?;
        assert_eq!(ws_by_path.id, "task-find");

        // Cleanup removes target directory as well
        assert!(target.exists());
        mgr.cleanup(&config)?;
        assert!(!target.exists());

        // Now find should return NotFound error
        assert!(mgr.find_workspace("task-find").is_err());
        assert!(mgr.find_workspace("my-task-mount").is_err());

        Ok(())
    }

    #[test]
    fn test_scan_changes_with_cas_store_and_unmounted_prune() -> io::Result<()> {
        use crate::cas::blob_store::LocalBlobStore;
        use std::io::Write;

        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path().join("cas"))?;
        let mgr = WorkspaceManager::new(tmp.path().join("storage"));

        // 1. Create base files in CAS
        let base_d = store.put(b"version 1\n")?;
        let del_d = store.put(b"delete me\n")?;

        let mut manifest = DirectoryManifest::default();
        manifest.entries.insert(
            "base_file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "base_file.txt".to_string(),
                digest: base_d,
            },
        );
        manifest.entries.insert(
            "del_file.txt".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "del_file.txt".to_string(),
                digest: del_d,
            },
        );
        let root_d = store.put(&manifest.serialize())?;

        let config = mgr.prepare_config(
            "task-cas-status",
            &root_d.to_string(),
            &tmp.path().join("lower"),
            &tmp.path().join("merged"),
        );
        mgr.init_workspace_dirs(&config)?;

        // 2. In upper_dir:
        // (a) Identical copy-up of base_file.txt
        let mut f1 = fs::File::create(config.upper_dir.join("base_file.txt"))?;
        f1.write_all(b"version 1\n")?;

        // (b) Whiteout for del_file.txt (existed in base)
        fs::File::create(config.upper_dir.join(".wh.del_file.txt"))?;

        // (c) Stale whiteout for file that never existed in base
        fs::File::create(config.upper_dir.join(".wh.stale.txt"))?;

        // (d) Newly created file
        let mut f2 = fs::File::create(config.upper_dir.join("new_file.rs"))?;
        f2.write_all(b"fn new() {}\n")?;

        // 3. Scan with CAS store:
        let changes = mgr.scan_changes_with_store(&config, Some(&store))?;
        assert_eq!(changes.len(), 2, "Only new_file and del_file should be dirty");

        let del_c = changes.iter().find(|c| c.relative_path == Path::new("del_file.txt")).unwrap();
        assert_eq!(del_c.change_type, ChangeType::Deleted);

        let new_c = changes.iter().find(|c| c.relative_path == Path::new("new_file.rs")).unwrap();
        assert_eq!(new_c.change_type, ChangeType::Created);

        // base_file.txt and stale whiteout must NOT be in changes!
        assert!(changes.iter().all(|c| c.relative_path != Path::new("base_file.txt")));
        assert!(changes.iter().all(|c| c.relative_path != Path::new("stale.txt")));

        // 4. Test unmounted compaction prune
        let pruned = mgr.prune_clean_upper_files(&config, &store)?;
        assert_eq!(pruned, 2, "base_file.txt and .wh.stale.txt should be pruned");
        assert!(!config.upper_dir.join("base_file.txt").exists());
        assert!(!config.upper_dir.join(".wh.stale.txt").exists());
        assert!(config.upper_dir.join(".wh.del_file.txt").exists());
        assert!(config.upper_dir.join("new_file.rs").exists());

        Ok(())
    }
}
