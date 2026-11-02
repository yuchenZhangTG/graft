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

//! # Virtual Filesystem (VFS) Server Engine
//!
//! This module implements the read-only virtual filesystem layer (the "CitC" engine).
//!
//! ### How Lazy Hydration Works:
//! 1. When an agent or tool lists a directory (`ls /mnt/repo`), the OS calls `readdir()`.
//!    Graft returns directory entries directly from the in-memory `VfsTree` (or lazily
//!    loads the corresponding `DirectoryManifest` from CAS on first visit).
//!    **Zero file blobs are downloaded.**
//! 2. When a tool checks file size (`stat /mnt/repo/main.rs`), the OS calls `getattr()`.
//!    Graft returns the size and permissions from `VfsNode`. **Zero bytes are downloaded.**
//! 3. Only when an application calls `open()` and `read()` to access file contents does
//!    Graft fetch the blob from `BlobStore`.
//! 4. Zero-Copy `pread64`: When blobs are cached locally, Graft uses kernel `pread64`
//!    directly into the FUSE buffer without re-hashing or full-file memory allocations.

use crate::cas::blob_store::BlobStore;
use crate::cas::chunker::{read_chunked_range, ChunkManifest};
use crate::cas::digest::Digest;
use crate::vfs::{Inode, VfsEntryType, VfsNode, VfsTree, ROOT_INODE};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Standard file attributes returned to the operating system kernel.
/// Matches the standard POSIX `stat` structure and FUSE `FileAttr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAttr {
    pub ino: Inode,
    pub size: u64,
    pub blocks: u64,
    pub atime: Duration,
    pub mtime: Duration,
    pub ctime: Duration,
    pub crtime: Duration,
    pub kind: FileType,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub flags: u32,
    pub blksize: u32,
}

/// POSIX file types supported by the virtual filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Directory,
    RegularFile,
    Symlink,
}

/// A Directory entry returned during `readdir()` calls.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: Inode,
    pub kind: FileType,
    pub name: String,
}

/// The core Graft Read-Only Virtual Filesystem Driver.
///
/// Dispatches kernel VFS operations to the in-memory Merkle tree (`VfsTree`)
/// and lazily hydrates directory manifests and file contents from the backing `BlobStore`.
pub struct GraftFilesystem {
    tree: Arc<RwLock<VfsTree>>,
    blob_store: Arc<dyn BlobStore>,
    uid: u32,
    gid: u32,
    commit_time: Duration,
    chunk_manifest_cache: RwLock<HashMap<Digest, Arc<ChunkManifest>>>,
}

impl GraftFilesystem {
    /// Initialize a new virtual filesystem backed by a shared in-memory tree and blob store.
    pub fn new(tree: Arc<RwLock<VfsTree>>, blob_store: Arc<dyn BlobStore>) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        // Default to a deterministic non-zero timestamp (e.g. Jan 1 2026) so compilers and tools don't complain
        let commit_time = Duration::from_secs(1_767_225_600);
        Self {
            tree,
            blob_store,
            uid,
            gid,
            commit_time,
            chunk_manifest_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Initializes a virtual filesystem from an owned `VfsTree`.
    pub fn from_tree(tree: VfsTree, blob_store: Arc<dyn BlobStore>) -> Self {
        Self::new(Arc::new(RwLock::new(tree)), blob_store)
    }

    /// Configures the reported commit timestamp for file attributes.
    pub fn with_commit_time(mut self, time: Duration) -> Self {
        self.commit_time = time;
        self
    }

    /// Converts an internal `VfsNode` into standard POSIX `FileAttr`.
    pub fn node_to_attr(&self, node: &VfsNode) -> FileAttr {
        let (kind, perm, nlink) = match node.entry_type {
            VfsEntryType::Directory => (FileType::Directory, 0o755, 2),
            VfsEntryType::RegularFile { is_executable }
            | VfsEntryType::ChunkedFile { is_executable, .. } => {
                let perm = if is_executable { 0o755 } else { 0o644 };
                (FileType::RegularFile, perm, 1)
            }
            VfsEntryType::Symlink => (FileType::Symlink, 0o777, 1),
        };

        // Calculate 512-byte blocks allocated
        let blocks = (node.size_bytes + 511) / 512;

        FileAttr {
            ino: node.inode,
            size: node.size_bytes,
            blocks,
            atime: self.commit_time,
            mtime: self.commit_time,
            ctime: self.commit_time,
            crtime: self.commit_time,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    /// Resolves a file or directory name within a parent directory.
    ///
    /// Called by the OS when resolving path components (e.g. resolving "src" in "/mnt/repo/src").
    pub fn lookup(&self, parent: Inode, name: &str) -> Option<FileAttr> {
        // Ensure parent directory manifest is loaded from CAS
        let needs_load = {
            let tree = self.tree.read().ok()?;
            !tree.is_dir_loaded(parent)
        };
        if needs_load {
            let mut tree = self.tree.write().ok()?;
            let _ = tree.ensure_dir_loaded(parent, self.blob_store.as_ref());
        }

        let tree = self.tree.read().ok()?;
        let node = tree.lookup(parent, name)?;
        Some(self.node_to_attr(node))
    }

    /// Returns attributes (size, permissions, timestamps) for a given inode.
    ///
    /// Called by `stat`, `ls -l`, and before `open`.
    pub fn getattr(&self, ino: Inode) -> Option<FileAttr> {
        let tree = self.tree.read().ok()?;
        let node = tree.get_node(ino)?;
        Some(self.node_to_attr(node))
    }

    /// Lists directory contents for a given directory inode.
    ///
    /// Required by POSIX: always returns `.` (current dir) and `..` (parent dir) first,
    /// followed by all child entries in the directory.
    pub fn readdir(&self, ino: Inode) -> Option<Vec<DirEntry>> {
        // Ensure directory manifest is loaded from CAS
        let needs_load = {
            let tree = self.tree.read().ok()?;
            !tree.is_dir_loaded(ino)
        };
        if needs_load {
            let mut tree = self.tree.write().ok()?;
            let _ = tree.ensure_dir_loaded(ino, self.blob_store.as_ref());
        }

        let tree = self.tree.read().ok()?;
        let parent_node = tree.get_node(ino)?;
        if parent_node.entry_type != VfsEntryType::Directory {
            return None;
        }

        let mut entries = Vec::new();
        // Entry 1: Current dir "."
        entries.push(DirEntry {
            ino,
            kind: FileType::Directory,
            name: ".".to_string(),
        });

        // Entry 2: Parent dir ".." (actual parent inode from tree)
        let parent_ino = tree.get_parent(ino).unwrap_or(ROOT_INODE);
        entries.push(DirEntry {
            ino: parent_ino,
            kind: FileType::Directory,
            name: "..".to_string(),
        });

        // Child files and subdirectories from in-memory tree (already sorted by name)
        for child in tree.list_children(ino) {
            let kind = match child.entry_type {
                VfsEntryType::Directory => FileType::Directory,
                VfsEntryType::RegularFile { .. }
                | VfsEntryType::ChunkedFile { .. } => FileType::RegularFile,
                VfsEntryType::Symlink => FileType::Symlink,
            };

            entries.push(DirEntry {
                ino: child.inode,
                kind,
                name: child.name.clone(),
            });
        }

        Some(entries)
    }

    /// Fetches and caches a parsed `ChunkManifest` in memory.
    fn get_or_load_chunk_manifest(&self, digest: &Digest) -> Result<Arc<ChunkManifest>, String> {
        {
            let cache = self.chunk_manifest_cache.read().map_err(|e| e.to_string())?;
            if let Some(m) = cache.get(digest) {
                return Ok(Arc::clone(m));
            }
        }
        let bytes = self
            .blob_store
            .get(digest)
            .map_err(|e| format!("CAS fetch error for chunk manifest: {}", e))?
            .ok_or_else(|| format!("Chunk manifest blob {} missing from CAS", digest))?;
        let manifest = ChunkManifest::from_bytes(&bytes)
            .map_err(|e| format!("Invalid chunk manifest {}: {}", digest, e))?;
        let arc = Arc::new(manifest);
        let mut cache = self.chunk_manifest_cache.write().map_err(|e| e.to_string())?;
        cache.insert(*digest, Arc::clone(&arc));
        Ok(arc)
    }

    /// Reads up to `size` bytes starting at `offset` for a given file inode.
    ///
    /// This is where lazy hydration occurs:
    /// - Checks if the file is cached locally on NVMe SSD, using zero-copy `pread64`.
    /// - Otherwise fetches from upstream CAS (`BlobStore::get`), caching locally.
    pub fn read(&self, ino: Inode, offset: u64, size: u32) -> Result<Vec<u8>, String> {
        let (digest, entry_type) = {
            let tree = self.tree.read().map_err(|e| e.to_string())?;
            let node = tree
                .get_node(ino)
                .ok_or_else(|| format!("Inode {} not found", ino))?;
            (node.digest, node.entry_type)
        };

        if let VfsEntryType::Directory = entry_type {
            return Err("Cannot read a directory as a file".to_string());
        }

        // FastCDC Chunked File: fetch/cache manifest, read only overlapping chunks
        if let VfsEntryType::ChunkedFile { manifest_digest, .. } = entry_type {
            let manifest = self.get_or_load_chunk_manifest(&manifest_digest)?;
            return read_chunked_range(&manifest, offset, size, self.blob_store.as_ref())
                .map_err(|e| format!("Chunked read error: {}", e));
        }

        // Fast path: Zero-copy read directly from local NVMe file via pread64.
        // Prevents O(N^2) read amplification when reading large files in chunks!
        if let Some(path) = self.blob_store.get_file_path(&digest) {
            use std::os::unix::fs::FileExt;
            let file = std::fs::File::open(&path).map_err(|e| format!("File open error: {}", e))?;
            let mut buf = vec![0u8; size as usize];
            let n = file
                .read_at(&mut buf, offset)
                .map_err(|e| format!("pread64 error: {}", e))?;
            buf.truncate(n);
            return Ok(buf);
        }

        // Fallback: Lazy fetch from CAS
        let blob = self
            .blob_store
            .get(&digest)
            .map_err(|e| format!("CAS fetch error: {}", e))?
            .ok_or_else(|| format!("Blob {} missing from CAS", digest))?;

        let offset = offset as usize;
        let size = size as usize;

        if offset >= blob.len() {
            return Ok(Vec::new());
        }

        let end = (offset + size).min(blob.len());
        Ok(blob[offset..end].to_vec())
    }
}

#[cfg(feature = "fuse")]
pub mod fuse_impl {
    use super::*;
    use fuser::{
        FileAttr as FuserFileAttr, FileType as FuserFileType, Filesystem, MountOption, ReplyAttr,
        ReplyData, ReplyDirectory, ReplyEntry, Request,
    };
    use std::ffi::OsStr;
    use std::io;
    use std::path::Path;
    use std::time::SystemTime;

    /// Immutable snapshots can be cached aggressively by the kernel VFS.
    const TTL: Duration = Duration::from_secs(3600);

    impl GraftFilesystem {
        fn to_fuser_attr(&self, attr: &FileAttr) -> FuserFileAttr {
            let kind = match attr.kind {
                FileType::Directory => FuserFileType::Directory,
                FileType::RegularFile => FuserFileType::RegularFile,
                FileType::Symlink => FuserFileType::Symlink,
            };

            let epoch = SystemTime::UNIX_EPOCH;
            FuserFileAttr {
                ino: attr.ino,
                size: attr.size,
                blocks: attr.blocks,
                atime: epoch + attr.atime,
                mtime: epoch + attr.mtime,
                ctime: epoch + attr.ctime,
                crtime: epoch + attr.crtime,
                kind,
                perm: attr.perm,
                nlink: attr.nlink,
                uid: attr.uid,
                gid: attr.gid,
                rdev: attr.rdev,
                blksize: attr.blksize,
                flags: attr.flags,
            }
        }
    }

    impl Filesystem for GraftFilesystem {
        fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            match GraftFilesystem::lookup(self, parent, name_str) {
                Some(attr) => {
                    let fuser_attr = self.to_fuser_attr(&attr);
                    reply.entry(&TTL, &fuser_attr, 0);
                }
                None => reply.error(libc::ENOENT),
            }
        }

        fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
            match GraftFilesystem::getattr(self, ino) {
                Some(attr) => {
                    let fuser_attr = self.to_fuser_attr(&attr);
                    reply.attr(&TTL, &fuser_attr);
                }
                None => reply.error(libc::ENOENT),
            }
        }

        fn read(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyData,
        ) {
            if offset < 0 {
                reply.error(libc::EINVAL);
                return;
            }

            match GraftFilesystem::read(self, ino, offset as u64, size) {
                Ok(data) => reply.data(&data),
                Err(_) => reply.error(libc::EIO),
            }
        }

        fn readdir(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            if offset < 0 {
                reply.error(libc::EINVAL);
                return;
            }

            let entries = match GraftFilesystem::readdir(self, ino) {
                Some(entries) => entries,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            // offset in readdir represents the next entry index (1-based index)
            for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
                let kind = match entry.kind {
                    FileType::Directory => FuserFileType::Directory,
                    FileType::RegularFile => FuserFileType::RegularFile,
                    FileType::Symlink => FuserFileType::Symlink,
                };

                // i + 1 is the next offset
                if reply.add(entry.ino, (i + 1) as i64, kind, &entry.name) {
                    break;
                }
            }
            reply.ok();
        }
    }

    /// Standard mount options for read-only Graft FUSE filesystems.
    pub fn default_mount_options() -> Vec<MountOption> {
        vec![
            MountOption::RO,
            MountOption::FSName("graft".to_string()),
            MountOption::DefaultPermissions,
        ]
    }

    /// Mounts the virtual filesystem in a blocking loop on the calling thread.
    pub fn mount_fuse<P: AsRef<Path>>(
        fs: GraftFilesystem,
        mountpoint: P,
        custom_options: Option<&[MountOption]>,
    ) -> io::Result<()> {
        let default_opts = default_mount_options();
        let opts = custom_options.unwrap_or(&default_opts);
        fuser::mount2(fs, mountpoint, opts)
    }

    /// Spawns a background FUSE session, returning a handle that unmounts cleanly on drop.
    pub fn spawn_mount_fuse<P: AsRef<Path>>(
        fs: GraftFilesystem,
        mountpoint: P,
        custom_options: Option<&[MountOption]>,
    ) -> io::Result<fuser::BackgroundSession> {
        let default_opts = default_mount_options();
        let opts = custom_options.unwrap_or(&default_opts);
        fuser::spawn_mount2(fs, mountpoint, opts)
    }
}

#[cfg(feature = "fuse")]
pub use fuse_impl::{default_mount_options, mount_fuse, spawn_mount_fuse};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use crate::cas::digest::Digest;
    use tempfile::tempdir;

    #[test]
    fn test_filesystem_read_and_readdir() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let store = Arc::new(LocalBlobStore::new(tmp.path())?);

        // Put test file in CAS
        let content = b"fn main() { println!(\"Graft VFS\"); }";
        let digest = store.put(content)?;

        // Build in-memory tree
        let mut tree = VfsTree::new(Digest::from_bytes(b"root"));
        let file_ino = tree.add_child(
            ROOT_INODE,
            "main.rs",
            VfsEntryType::RegularFile { is_executable: false },
            digest,
            content.len() as u64,
        );

        let fs = GraftFilesystem::from_tree(tree, store);

        // Test getattr
        let attr = fs.getattr(file_ino).expect("getattr must succeed");
        assert_eq!(attr.size, content.len() as u64);
        assert_eq!(attr.kind, FileType::RegularFile);

        // Test lookup
        let lookup_attr = fs.lookup(ROOT_INODE, "main.rs").expect("lookup must succeed");
        assert_eq!(lookup_attr.ino, file_ino);

        // Test readdir
        let dir_entries = fs.readdir(ROOT_INODE).expect("readdir must succeed");
        assert!(dir_entries.iter().any(|e| e.name == "main.rs"));
        assert!(dir_entries.iter().any(|e| e.name == "."));
        assert!(dir_entries.iter().any(|e| e.name == ".."));

        // Test lazy read
        let bytes = fs.read(file_ino, 0, 100).expect("read must succeed");
        assert_eq!(bytes, content);

        // Test offset read
        let slice = fs.read(file_ino, 3, 4).expect("offset read must succeed");
        assert_eq!(slice, b"main");

        Ok(())
    }
}
