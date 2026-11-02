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

//! # Virtual Filesystem (VFS) Tree & Metadata Model
//!
//! This module represents an in-memory directory tree for a specific repository snapshot.
//!
//! ### Key Concept: Inodes in a Virtual Filesystem
//! In POSIX/Linux, every file or folder is represented by a unique integer called an `Inode`.
//! In standard filesystems (ext4/xfs), inodes are stored on disk blocks.
//! In Graft, the `VfsTree` acts as a fast, virtual inode table:
//! - Inode `1` is always the root directory of the repository (`ROOT_INODE`).
//! - Each child file or subdirectory is assigned an incremental virtual inode number.
//! - The tree maps each inode to its file metadata (name, size, permissions, and CAS Digest).

pub mod fuse_server;
pub mod workspace_fs;

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::{DirectoryManifest, ManifestEntry};
use crate::cas::digest::Digest;
pub use fuse_server::{DirEntry, FileAttr, FileType, GraftFilesystem};
pub use workspace_fs::GraftWorkspaceFilesystem;
#[cfg(feature = "fuse")]
pub use fuse_server::{default_mount_options, mount_fuse, spawn_mount_fuse};
#[cfg(feature = "fuse")]
pub use workspace_fs::{
    default_workspace_mount_options, mount_workspace_fuse, spawn_mount_workspace_fuse,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;

/// Inode identifier in the virtual filesystem (64-bit unsigned integer).
pub type Inode = u64;

/// Root inode standard in Unix/Linux VFS.
pub const ROOT_INODE: Inode = 1;

/// File types recognized by the Graft VFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsEntryType {
    /// Directory containing child files and subdirectories.
    Directory,
    /// Regular contiguous file. `is_executable` controls the POSIX execute bit (0755 vs 0644).
    RegularFile { is_executable: bool },
    /// Large file backed by a FastCDC `ChunkManifest`.
    ChunkedFile {
        is_executable: bool,
        manifest_digest: Digest,
    },
    /// Symbolic link pointing to another path.
    Symlink,
}

/// A node within the virtual in-memory Merkle directory tree.
#[derive(Debug, Clone)]
pub struct VfsNode {
    /// Virtual inode number assigned by Graft.
    pub inode: Inode,
    /// Parent directory inode number (ROOT_INODE for ROOT_INODE).
    pub parent_inode: Inode,
    /// File or directory base name (e.g., "main.rs" or "src").
    pub name: String,
    /// Type of entry (Directory, RegularFile, Symlink).
    pub entry_type: VfsEntryType,
    /// Content-addressed Digest pointing to the blob or sub-directory manifest in CAS.
    pub digest: Digest,
    /// File size in bytes (reported to `ls` and `stat` without downloading the content).
    pub size_bytes: u64,
}

/// An in-memory metadata hierarchy representing a point-in-time repository snapshot.
///
/// Fast lookups:
/// - By Inode: `nodes.get(&inode)` in O(1) time.
/// - By Name in Directory: `dir_children.get(&parent_inode)` in O(log K) time.
#[derive(Debug)]
pub struct VfsTree {
    next_inode: Inode,
    nodes: HashMap<Inode, VfsNode>,
    dir_children: HashMap<Inode, BTreeMap<String, Inode>>,
    loaded_dirs: HashSet<Inode>,
}

impl VfsTree {
    /// Creates a new virtual tree initialized with a root directory at Inode 1.
    pub fn new(root_digest: Digest) -> Self {
        let mut nodes = HashMap::new();
        let dir_children = HashMap::new();

        let root_node = VfsNode {
            inode: ROOT_INODE,
            parent_inode: ROOT_INODE,
            name: String::new(),
            entry_type: VfsEntryType::Directory,
            digest: root_digest,
            size_bytes: 4096,
        };

        nodes.insert(ROOT_INODE, root_node);

        Self {
            next_inode: ROOT_INODE + 1,
            nodes,
            dir_children,
            loaded_dirs: HashSet::new(),
        }
    }

    /// Initializes a `VfsTree` from a root Merkle tree digest.
    ///
    /// The root directory manifest will be lazily loaded from CAS on first access.
    pub fn from_cas<B: BlobStore + ?Sized>(
        root_digest: &Digest,
        _blob_store: &B,
    ) -> io::Result<Self> {
        Ok(Self::new(*root_digest))
    }

    /// Allocates a new unique inode and inserts a child node under a parent directory inode.
    pub fn add_child(
        &mut self,
        parent_inode: Inode,
        name: &str,
        entry_type: VfsEntryType,
        digest: Digest,
        size_bytes: u64,
    ) -> Inode {
        let inode = self.next_inode;
        self.next_inode += 1;

        let node = VfsNode {
            inode,
            parent_inode,
            name: name.to_string(),
            entry_type,
            digest,
            size_bytes,
        };

        self.nodes.insert(inode, node);
        self.dir_children
            .entry(parent_inode)
            .or_default()
            .insert(name.to_string(), inode);

        inode
    }

    /// Looks up a child entry by filename within a parent directory in O(log K) time.
    pub fn lookup(&self, parent_inode: Inode, name: &str) -> Option<&VfsNode> {
        let child_inode = self.dir_children.get(&parent_inode)?.get(name)?;
        self.nodes.get(child_inode)
    }

    /// Retrieves node metadata by inode number in O(1) time.
    pub fn get_node(&self, inode: Inode) -> Option<&VfsNode> {
        self.nodes.get(&inode)
    }

    /// Returns the parent inode for a given inode.
    pub fn get_parent(&self, inode: Inode) -> Option<Inode> {
        self.nodes.get(&inode).map(|n| n.parent_inode)
    }

    /// Returns all direct child nodes belonging to a parent directory inode, sorted by name.
    pub fn list_children(&self, parent_inode: Inode) -> Vec<&VfsNode> {
        self.dir_children
            .get(&parent_inode)
            .map(|children| {
                children
                    .values()
                    .filter_map(|inode| self.nodes.get(inode))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Checks if a directory's manifest has been loaded from CAS.
    pub fn is_dir_loaded(&self, dir_inode: Inode) -> bool {
        self.loaded_dirs.contains(&dir_inode)
    }

    /// Ensures that the children of `dir_inode` are loaded into the VfsTree from CAS.
    ///
    /// If `dir_inode` has already been loaded, returns immediately in O(1).
    /// Otherwise, fetches the `DirectoryManifest` from `blob_store`, adds all child
    /// entries (with Inodes and sizes), and marks `dir_inode` as loaded.
    pub fn ensure_dir_loaded<B: BlobStore + ?Sized>(
        &mut self,
        dir_inode: Inode,
        blob_store: &B,
    ) -> io::Result<()> {
        if self.loaded_dirs.contains(&dir_inode) {
            return Ok(());
        }

        let (dir_digest, is_dir) = match self.nodes.get(&dir_inode) {
            Some(n) => (n.digest, n.entry_type == VfsEntryType::Directory),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Inode {} not found", dir_inode),
                ))
            }
        };

        if !is_dir {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Inode {} is not a directory", dir_inode),
            ));
        }

        // Fetch directory manifest from CAS
        if let Some(manifest_bytes) = blob_store.get(&dir_digest)? {
            let manifest = DirectoryManifest::from_bytes(&manifest_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            for entry in manifest.entries.values() {
                match entry {
                    ManifestEntry::File { mode, name, digest } => {
                        let is_executable = mode == "0755";
                        let size_bytes = if digest.size_bytes > 0 {
                            digest.size_bytes as u64
                        } else if let Some(path) = blob_store.get_file_path(digest) {
                            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
                        } else {
                            0
                        };

                        self.add_child(
                            dir_inode,
                            name,
                            VfsEntryType::RegularFile { is_executable },
                            *digest,
                            size_bytes,
                        );
                    }
                    ManifestEntry::ChunkedFile {
                        mode,
                        name,
                        manifest_digest,
                        total_size,
                    } => {
                        let is_executable = mode == "0755";
                        self.add_child(
                            dir_inode,
                            name,
                            VfsEntryType::ChunkedFile {
                                is_executable,
                                manifest_digest: *manifest_digest,
                            },
                            *manifest_digest,
                            *total_size,
                        );
                    }
                    ManifestEntry::Directory { name, digest } => {
                        self.add_child(
                            dir_inode,
                            name,
                            VfsEntryType::Directory,
                            *digest,
                            4096,
                        );
                    }
                }
            }
        }

        self.loaded_dirs.insert(dir_inode);
        Ok(())
    }

    /// Recursively preloads the entire directory tree from CAS into memory.
    pub fn preload_all<B: BlobStore + ?Sized>(&mut self, blob_store: &B) -> io::Result<()> {
        self.preload_recursive(ROOT_INODE, blob_store)
    }

    fn preload_recursive<B: BlobStore + ?Sized>(
        &mut self,
        dir_inode: Inode,
        blob_store: &B,
    ) -> io::Result<()> {
        self.ensure_dir_loaded(dir_inode, blob_store)?;
        let subdirs: Vec<Inode> = self
            .dir_children
            .get(&dir_inode)
            .map(|m| {
                m.values()
                    .copied()
                    .filter(|&ino| {
                        self.nodes
                            .get(&ino)
                            .map(|n| n.entry_type == VfsEntryType::Directory)
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();

        for subdir in subdirs {
            self.preload_recursive(subdir, blob_store)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use tempfile::tempdir;

    #[test]
    fn test_vfs_tree_lazy_loading() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        // 1. Create file blobs
        let f1_data = b"fn main() {}";
        let f1_digest = store.put(f1_data)?;

        let f2_data = b"# Readme content";
        let f2_digest = store.put(f2_data)?;

        // 2. Subdirectory manifest for "src"
        let mut src_m = DirectoryManifest::default();
        src_m.entries.insert(
            "main.rs".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "main.rs".to_string(),
                digest: f1_digest,
            },
        );
        let src_bytes = src_m.serialize();
        let src_digest = store.put(&src_bytes)?;

        // 3. Root directory manifest
        let mut root_m = DirectoryManifest::default();
        root_m.entries.insert(
            "README.md".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "README.md".to_string(),
                digest: f2_digest,
            },
        );
        root_m.entries.insert(
            "src".to_string(),
            ManifestEntry::Directory {
                name: "src".to_string(),
                digest: src_digest,
            },
        );
        let root_bytes = root_m.serialize();
        let root_digest = store.put(&root_bytes)?;

        // 4. Create VfsTree with root_digest
        let mut tree = VfsTree::from_cas(&root_digest, &store)?;
        assert!(!tree.is_dir_loaded(ROOT_INODE));

        // 5. Ensure root is loaded
        tree.ensure_dir_loaded(ROOT_INODE, &store)?;
        assert!(tree.is_dir_loaded(ROOT_INODE));

        let readme = tree.lookup(ROOT_INODE, "README.md").expect("README.md must exist");
        assert_eq!(readme.size_bytes, f2_data.len() as u64);
        assert_eq!(readme.parent_inode, ROOT_INODE);

        let src_node = tree.lookup(ROOT_INODE, "src").expect("src dir must exist");
        let src_ino = src_node.inode;
        assert_eq!(src_node.parent_inode, ROOT_INODE);
        assert!(!tree.is_dir_loaded(src_ino));

        // 6. Ensure src is loaded lazily
        tree.ensure_dir_loaded(src_ino, &store)?;
        assert!(tree.is_dir_loaded(src_ino));

        let main_rs = tree.lookup(src_ino, "main.rs").expect("main.rs must exist");
        assert_eq!(main_rs.size_bytes, f1_data.len() as u64);
        assert_eq!(main_rs.parent_inode, src_ino);

        Ok(())
    }
}
