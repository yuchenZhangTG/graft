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

//! # Commit Snapshot & Tree Mutation Engine
//!
//! This module represents immutable Commit objects in Graft and implements the
//! algorithms to apply workspace deltas to an existing Merkle tree.
//!
//! ### The Merkle Tree Mutation Algorithm:
//! When an agent modifies 2 files in a repository with 200,000 files:
//! 1. The agent writes to `upperdir`.
//! 2. Only the modified files are hashed into new CAS blobs.
//! 3. Graft updates only the directories along the path from the modified files
//!    up to the root (O(depth) updates, typically 3-5 directory manifests).
//! 4. Unchanged subtrees reuse their existing Merkle digests with zero re-hashing.
//! 5. A new immutable `Commit` object is generated and saved in CAS.

use crate::cas::blob_store::BlobStore;
use crate::cas::digest::Digest;
use crate::workspace::{ChangeType, WorkspaceChange};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// An immutable Commit object snapshot in Graft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Digests of parent commits (empty for root commit, 1 for linear, 2+ for merges).
    pub parent_commits: Vec<Digest>,
    /// Digest pointing to the root Directory manifest.
    pub root_tree_digest: Digest,
    /// Author identity string (e.g. "Name <email@example.com>").
    pub author: String,
    /// UTC Unix timestamp in seconds.
    pub timestamp_seconds: i64,
    /// Human-readable commit message.
    pub message: String,
}

impl Commit {
    /// Create a new Commit snapshot.
    pub fn new(
        parent_commits: Vec<Digest>,
        root_tree_digest: Digest,
        author: String,
        message: String,
    ) -> Self {
        let timestamp_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        Self {
            parent_commits,
            root_tree_digest,
            author,
            timestamp_seconds,
            message,
        }
    }

    /// Serializes the commit into a canonical, deterministic format for CAS storage.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(&format!("tree {}\n", self.root_tree_digest));
        for parent in &self.parent_commits {
            out.push_str(&format!("parent {}\n", parent));
        }
        out.push_str(&format!(
            "author {} {}\n",
            self.author, self.timestamp_seconds
        ));
        out.push('\n');
        out.push_str(&self.message);
        if !self.message.ends_with('\n') {
            out.push('\n');
        }
        out.into_bytes()
    }

    /// Deserializes a canonical commit string back into a `Commit` struct.
    pub fn deserialize(data: &[u8]) -> Result<Self, String> {
        let text = String::from_utf8_lossy(data);
        let mut lines = text.lines();

        let mut root_tree_digest = None;
        let mut parent_commits = Vec::new();
        let mut author = String::new();
        let mut timestamp_seconds = 0;

        while let Some(line) = lines.next() {
            if line.is_empty() {
                break; // Header ends, rest is commit message
            }

            if let Some(rest) = line.strip_prefix("tree ") {
                let parts: Vec<&str> = rest.split(':').collect();
                let size = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                root_tree_digest = Some(
                    Digest::from_hex(parts[0], size)
                        .map_err(|e| format!("Invalid tree digest: {}", e))?,
                );
            } else if let Some(rest) = line.strip_prefix("parent ") {
                let parts: Vec<&str> = rest.split(':').collect();
                let size = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                parent_commits.push(
                    Digest::from_hex(parts[0], size)
                        .map_err(|e| format!("Invalid parent digest: {}", e))?,
                );
            } else if let Some(rest) = line.strip_prefix("author ") {
                let parts: Vec<&str> = rest.rsplitn(2, ' ').collect();
                if parts.len() == 2 {
                    timestamp_seconds = parts[0].parse().unwrap_or(0);
                    author = parts[1].to_string();
                } else {
                    author = rest.to_string();
                }
            }
        }

        let message = lines.collect::<Vec<&str>>().join("\n");
        let root_tree_digest =
            root_tree_digest.ok_or_else(|| "Missing 'tree' header in commit".to_string())?;

        Ok(Self {
            parent_commits,
            root_tree_digest,
            author,
            timestamp_seconds,
            message,
        })
    }
}

/// Traverses commit ancestry in CAS starting from `head_commit` to build linear commit history.
///
/// # Arguments
/// * `head_commit` - Digest of the commit to begin walking from.
/// * `limit` - Optional maximum number of ancestor commits to return.
/// * `blob_store` - The backing CAS store holding commit objects.
///
/// # Returns
/// An ordered vector of `(Digest, Commit)` pairs from newest to oldest.
///
/// # Errors
/// Returns an `io::Error` if a commit object cannot be found in CAS or is malformed.
pub fn get_commit_history<B: BlobStore + ?Sized>(
    head_commit: &Digest,
    limit: Option<usize>,
    blob_store: &B,
) -> io::Result<Vec<(Digest, Commit)>> {
    let mut history = Vec::new();
    let mut current_digest = Some(*head_commit);
    let max = limit.unwrap_or(usize::MAX);

    while let Some(digest) = current_digest {
        if history.len() >= max {
            break;
        }

        let bytes = blob_store
            .get(&digest)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Commit {} not found in CAS", digest)))?;

        let commit = Commit::deserialize(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let parent = commit.parent_commits.first().copied();
        history.push((digest, commit));
        current_digest = parent;
    }

    Ok(history)
}

/// A parsed entry from a Directory manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestEntry {
    File {
        mode: String,
        name: String,
        digest: Digest,
    },
    ChunkedFile {
        mode: String,
        name: String,
        manifest_digest: Digest,
        total_size: u64,
    },
    Directory {
        name: String,
        digest: Digest,
    },
}

impl ManifestEntry {
    pub fn name(&self) -> &str {
        match self {
            ManifestEntry::File { name, .. } => name,
            ManifestEntry::ChunkedFile { name, .. } => name,
            ManifestEntry::Directory { name, .. } => name,
        }
    }
}

/// In-memory representation of a directory manifest (sorted by name).
#[derive(Default, Debug, Clone)]
pub struct DirectoryManifest {
    pub entries: BTreeMap<String, ManifestEntry>,
}

impl DirectoryManifest {
    /// Parse a directory manifest payload from CAS bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let text = String::from_utf8_lossy(data);
        let mut entries = BTreeMap::new();

        for line in text.lines() {
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }

            match parts[0] {
                "FILE" => {
                    if parts.len() < 4 {
                        return Err(format!("Malformed FILE line: {}", line));
                    }
                    let mode = parts[1].to_string();
                    let name = parts[2].to_string();
                    let digest = Digest::parse(parts[3])
                        .map_err(|e| format!("Invalid file digest: {}", e))?;
                    entries.insert(
                        name.clone(),
                        ManifestEntry::File { mode, name, digest },
                    );
                }
                "CHUNKED" => {
                    if parts.len() < 5 {
                        return Err(format!("Malformed CHUNKED line: {}", line));
                    }
                    let mode = parts[1].to_string();
                    let name = parts[2].to_string();
                    let manifest_digest = Digest::parse(parts[3])
                        .map_err(|e| format!("Invalid chunked manifest digest: {}", e))?;
                    let total_size = parts[4]
                        .parse::<u64>()
                        .map_err(|e| format!("Invalid total_size in CHUNKED line: {}", e))?;
                    entries.insert(
                        name.clone(),
                        ManifestEntry::ChunkedFile {
                            mode,
                            name,
                            manifest_digest,
                            total_size,
                        },
                    );
                }
                "DIR" => {
                    if parts.len() < 3 {
                        return Err(format!("Malformed DIR line: {}", line));
                    }
                    let name = parts[1].to_string();
                    let digest = Digest::parse(parts[2])
                        .map_err(|e| format!("Invalid dir digest: {}", e))?;
                    entries.insert(
                        name.clone(),
                        ManifestEntry::Directory { name, digest },
                    );
                }
                _ => {}
            }
        }

        Ok(Self { entries })
    }

    /// Serializes the manifest deterministically into canonical bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // BTreeMap guarantees alphabetical sorting by key (name)
        for entry in self.entries.values() {
            match entry {
                ManifestEntry::File { mode, name, digest } => {
                    out.extend_from_slice(
                        format!("FILE {} {} {}\n", mode, name, digest).as_bytes(),
                    );
                }
                ManifestEntry::ChunkedFile {
                    mode,
                    name,
                    manifest_digest,
                    total_size,
                } => {
                    out.extend_from_slice(
                        format!(
                            "CHUNKED {} {} {} {}\n",
                            mode, name, manifest_digest, total_size
                        )
                        .as_bytes(),
                    );
                }
                ManifestEntry::Directory { name, digest } => {
                    out.extend_from_slice(
                        format!("DIR {} {}\n", name, digest).as_bytes(),
                    );
                }
            }
        }
        out
    }
}

/// Resolves a revision, commit digest, or tree digest string into a root directory tree digest.
pub fn resolve_root_tree<B: BlobStore + ?Sized>(
    digest_str: &str,
    blob_store: &B,
) -> io::Result<Option<Digest>> {
    let digest = match Digest::parse(digest_str) {
        Ok(d) => d,
        Err(_) => return Ok(None),
    };
    if let Some(bytes) = blob_store.get(&digest)? {
        if let Ok(commit) = Commit::deserialize(&bytes) {
            return Ok(Some(commit.root_tree_digest));
        }
    }
    // If it's not a commit or missing, treat as direct tree digest
    Ok(Some(digest))
}

/// Resolves a commit digest to its root directory tree digest.
/// If it is already a tree digest or cannot be parsed as a commit, returns the digest as-is.
pub fn get_commit_tree_digest<B: BlobStore + ?Sized>(
    commit_or_tree: &Digest,
    blob_store: &B,
) -> Digest {
    if let Ok(Some(bytes)) = blob_store.get(commit_or_tree) {
        if let Ok(commit) = Commit::deserialize(&bytes) {
            return commit.root_tree_digest;
        }
    }
    *commit_or_tree
}

/// Looks up a file or directory entry in a Merkle tree given its relative path.
pub fn get_tree_entry<B: BlobStore + ?Sized>(
    root_digest: &Digest,
    rel_path: &Path,
    blob_store: &B,
) -> io::Result<Option<ManifestEntry>> {
    let components: Vec<String> = rel_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();

    if components.is_empty() {
        return Ok(None);
    }

    let mut current_digest = *root_digest;
    for (i, comp) in components.iter().enumerate() {
        let manifest_bytes = match blob_store.get(&current_digest)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let manifest = DirectoryManifest::from_bytes(&manifest_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        match manifest.entries.get(comp) {
            Some(entry) => {
                if i == components.len() - 1 {
                    return Ok(Some(entry.clone()));
                }
                match entry {
                    ManifestEntry::Directory { digest, .. } => {
                        current_digest = *digest;
                    }
                    _ => return Ok(None),
                }
            }
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Applies a set of workspace changes to an existing Merkle tree and generates a new root Digest.
///
/// Modifies only directories along the path of modified files ($O(\text{depth})$ updates),
/// reusing identical subtrees in $O(1)$ without re-hashing untouched directories.
///
/// # Arguments
/// * `base_root_digest` - The root Merkle directory digest of the base tree.
/// * `changes` - The list of modified, created, or deleted files extracted from `upperdir`.
/// * `blob_store` - The backing CAS store where new file blobs and directory manifests will be stored.
///
/// # Returns
/// The newly computed root Merkle `Digest` representing the updated repository snapshot.
///
/// # Errors
/// Returns an `io::Error` if reading modified payloads or writing manifests to CAS fails.
pub fn apply_changes_to_tree<B: BlobStore + ?Sized>(
    base_root_digest: &Digest,
    changes: &[WorkspaceChange],
    blob_store: &B,
) -> io::Result<Digest> {
    apply_changes_to_tree_with_config(
        base_root_digest,
        changes,
        blob_store,
        &crate::cas::chunker::ChunkerConfig::default(),
    )
}

/// Applies workspace changes with a custom chunker configuration (e.g. for testing).
pub fn apply_changes_to_tree_with_config<B: BlobStore + ?Sized>(
    base_root_digest: &Digest,
    changes: &[WorkspaceChange],
    blob_store: &B,
    config: &crate::cas::chunker::ChunkerConfig,
) -> io::Result<Digest> {
    // 1. Ingest all modified and created files into CAS, getting their ManifestEntry
    let mut file_updates = Vec::new(); // (relative_path_components, Option<ManifestEntry>)

    for change in changes {
        let components: Vec<String> = change
            .relative_path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();

        match change.change_type {
            ChangeType::Created | ChangeType::Modified => {
                let metadata = std::fs::metadata(&change.disk_path)?;
                use std::os::unix::fs::PermissionsExt;
                let is_executable = (metadata.permissions().mode() & 0o111) != 0;
                let file_name = components.last().cloned().unwrap_or_default();

                let entry = crate::cas::chunker::store_file_or_chunked(
                    &change.disk_path,
                    &file_name,
                    is_executable,
                    blob_store,
                    config,
                )?;
                file_updates.push((components, Some(entry)));
            }
            ChangeType::Deleted => {
                file_updates.push((components, None));
            }
        }
    }

    // 2. Recursively apply updates starting from the root manifest
    update_directory_recursive(base_root_digest, &file_updates, 0, blob_store)
}

fn update_directory_recursive<B: BlobStore + ?Sized>(
    current_dir_digest: &Digest,
    updates: &[(Vec<String>, Option<ManifestEntry>)],
    depth: usize,
    blob_store: &B,
) -> io::Result<Digest> {
    // Load current directory manifest from CAS (or create empty if new)
    let mut manifest = if current_dir_digest.size_bytes > 0 || blob_store.has(current_dir_digest) {
        if let Some(bytes) = blob_store.get(current_dir_digest)? {
            DirectoryManifest::from_bytes(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else {
            DirectoryManifest::default()
        }
    } else {
        DirectoryManifest::default()
    };

    // Group updates for this directory level:
    // - Direct file updates (depth == components.len() - 1)
    // - Subdirectory updates grouped by subdirectory name
    let mut direct_file_updates = Vec::new();
    let mut subdir_updates: BTreeMap<String, Vec<(Vec<String>, Option<ManifestEntry>)>> =
        BTreeMap::new();

    for update in updates {
        let (components, payload) = update;
        if depth == components.len() - 1 {
            // Target file is directly in this directory
            direct_file_updates.push((&components[depth], payload));
        } else if depth < components.len() - 1 {
            // Target is inside a subdirectory
            let subdir_name = &components[depth];
            subdir_updates
                .entry(subdir_name.clone())
                .or_default()
                .push(update.clone());
        }
    }

    // Apply direct file updates
    for (name, payload) in direct_file_updates {
        match payload {
            Some(entry) => {
                manifest.entries.insert(name.to_string(), entry.clone());
            }
            None => {
                // File deleted
                manifest.entries.remove(name);
            }
        }
    }

    // Recursively apply subdirectory updates
    for (subdir_name, sub_updates) in subdir_updates {
        let existing_sub_digest = manifest.entries.get(&subdir_name).and_then(|entry| {
            if let ManifestEntry::Directory { digest, .. } = entry {
                Some(*digest)
            } else {
                None
            }
        });

        let placeholder = Digest::from_bytes(b"empty");
        let base_sub_digest = existing_sub_digest.unwrap_or(placeholder);

        let new_sub_digest =
            update_directory_recursive(&base_sub_digest, &sub_updates, depth + 1, blob_store)?;

        manifest.entries.insert(
            subdir_name.clone(),
            ManifestEntry::Directory {
                name: subdir_name,
                digest: new_sub_digest,
            },
        );
    }

    // Serialize and write new directory manifest to CAS
    let manifest_bytes = manifest.serialize();
    blob_store.put(&manifest_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use tempfile::tempdir;

    #[test]
    fn test_commit_serialization_roundtrip() {
        let root = Digest::from_bytes(b"root-tree-data");
        let parent = Digest::from_bytes(b"parent-commit-data");

        let commit = Commit::new(
            vec![parent],
            root,
            "Graft Agent <agent@graft.dev>".to_string(),
            "Add agent-based code velocity benchmarks".to_string(),
        );

        let bytes = commit.serialize();
        let parsed = Commit::deserialize(&bytes).expect("deserialization must succeed");

        assert_eq!(parsed.root_tree_digest, root);
        assert_eq!(parsed.parent_commits, vec![parent]);
        assert_eq!(parsed.author, commit.author);
        assert_eq!(parsed.message.trim(), commit.message.trim());
    }

    #[test]
    fn test_apply_changes_to_tree() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        // 1. Initial base tree: README.md
        let mut initial_manifest = DirectoryManifest::default();
        let readme_digest = store.put(b"# Initial README")?;
        initial_manifest.entries.insert(
            "README.md".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "README.md".to_string(),
                digest: readme_digest,
            },
        );
        let base_root = store.put(&initial_manifest.serialize())?;

        // 2. Prepare modifications in upperdir:
        // - Modified: README.md
        // - Created: src/main.rs
        let upper_dir = tmp.path().join("upper");
        std::fs::create_dir_all(upper_dir.join("src"))?;

        let modified_readme = upper_dir.join("README.md");
        std::fs::write(&modified_readme, b"# Updated README by Agent")?;

        let new_main = upper_dir.join("src").join("main.rs");
        std::fs::write(&new_main, b"fn main() {}")?;

        let changes = vec![
            WorkspaceChange {
                relative_path: std::path::PathBuf::from("README.md"),
                disk_path: modified_readme,
                change_type: ChangeType::Modified,
            },
            WorkspaceChange {
                relative_path: std::path::PathBuf::from("src/main.rs"),
                disk_path: new_main,
                change_type: ChangeType::Created,
            },
        ];

        // 3. Apply changes
        let new_root = apply_changes_to_tree(&base_root, &changes, &store)?;
        assert_ne!(new_root, base_root);

        // 4. Verify new root manifest
        let new_root_bytes = store.get(&new_root)?.expect("new root must exist");
        let new_root_manifest = DirectoryManifest::from_bytes(&new_root_bytes).unwrap();

        // Check README was updated
        let readme_entry = new_root_manifest.entries.get("README.md").unwrap();
        if let ManifestEntry::File { digest, .. } = readme_entry {
            assert_ne!(*digest, readme_digest);
            let updated_bytes = store.get(digest)?.unwrap();
            assert_eq!(updated_bytes, b"# Updated README by Agent");
        } else {
            panic!("Expected file");
        }

        // Check src directory was added
        assert!(new_root_manifest.entries.contains_key("src"));

        Ok(())
    }

    #[test]
    fn test_commit_history_traversal() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        let tree1 = store.put(b"tree1")?;
        let tree2 = store.put(b"tree2")?;

        let c1 = Commit::new(vec![], tree1, "Alice <alice@test.com>".to_string(), "Initial commit".to_string());
        let c1_digest = store.put(&c1.serialize())?;

        let c2 = Commit::new(vec![c1_digest], tree2, "Bob <bob@test.com>".to_string(), "Second commit".to_string());
        let c2_digest = store.put(&c2.serialize())?;

        let history = get_commit_history(&c2_digest, None, &store)?;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, c2_digest);
        assert_eq!(history[0].1.message, "Second commit");
        assert_eq!(history[1].0, c1_digest);
        assert_eq!(history[1].1.message, "Initial commit");

        // Test with limit
        let limited = get_commit_history(&c2_digest, Some(1), &store)?;
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].0, c2_digest);

        Ok(())
    }

    #[test]
    fn test_resolve_root_tree_and_get_tree_entry() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        let f_digest = store.put(b"fn hello() {}\n")?;
        let mut sub_manifest = DirectoryManifest::default();
        sub_manifest.entries.insert(
            "mod.rs".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "mod.rs".to_string(),
                digest: f_digest,
            },
        );
        let sub_digest = store.put(&sub_manifest.serialize())?;

        let mut root_manifest = DirectoryManifest::default();
        root_manifest.entries.insert(
            "src".to_string(),
            ManifestEntry::Directory {
                name: "src".to_string(),
                digest: sub_digest,
            },
        );
        let root_digest = store.put(&root_manifest.serialize())?;

        let commit = Commit::new(
            vec![],
            root_digest,
            "Dev <dev@graft.dev>".to_string(),
            "Init".to_string(),
        );
        let commit_digest = store.put(&commit.serialize())?;

        // Test resolve_root_tree
        let resolved = resolve_root_tree(&commit_digest.to_string(), &store)?;
        assert_eq!(resolved, Some(root_digest));

        let resolved_direct = resolve_root_tree(&root_digest.to_string(), &store)?;
        assert_eq!(resolved_direct, Some(root_digest));

        // Test get_tree_entry
        let entry = get_tree_entry(&root_digest, Path::new("src/mod.rs"), &store)?;
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().name(), "mod.rs");

        let missing = get_tree_entry(&root_digest, Path::new("src/nonexistent.rs"), &store)?;
        assert!(missing.is_none());

        Ok(())
    }
}
