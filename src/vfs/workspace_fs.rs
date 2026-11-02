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

//! # Writeable Copy-on-Write (CoW) Workspace Filesystem
//!
//! Implements a user-space writeable filesystem (`GraftWorkspaceFilesystem`) for unprivileged
//! agent workspaces.
//!
//! ### Architecture:
//! - **Lower Layer**: Read-only virtual Merkle tree (`VfsTree`) hydrated lazily from CAS (`BlobStore`).
//! - **Upper Layer**: Mutable directory on local disk (`upper_dir`).
//! - **Whiteouts**: When a file or directory that exists in the lower layer is unlinked or deleted,
//!   a tombstone file `.wh.<filename>` is created in `upper_dir`.
//! - **Opaque Directories**: When a directory is re-created over a whited-out lower directory,
//!   `.wh..wh..opq` is placed inside to hide lower contents.
//! - **Copy-Up**: When a lower file is opened for writing or truncated/modified via `setattr`,
//!   it is physically copied into `upper_dir` before modifying.
//! - **Inode Stability**: A bidirectional `rel_path <-> Inode` table guarantees that an Inode
//!   never changes even across copy-up operations.

use crate::cas::blob_store::BlobStore;
use crate::cas::chunker::{read_chunked_range, ChunkManifest};
use crate::cas::digest::Digest;
use crate::vfs::fuse_server::{DirEntry, FileAttr, FileType};
use crate::vfs::{Inode, VfsEntryType, VfsTree, ROOT_INODE};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// Short attribute TTL for writeable workspaces (500ms) to ensure file edits are immediately visible.
pub const WORKSPACE_TTL: Duration = Duration::from_millis(500);

/// Writeable Copy-on-Write (CoW) FUSE filesystem for Graft agent workspaces.
pub struct GraftWorkspaceFilesystem {
    lower_tree: Arc<RwLock<VfsTree>>,
    blob_store: Arc<dyn BlobStore>,
    upper_dir: PathBuf,
    path_to_inode: RwLock<HashMap<PathBuf, Inode>>,
    inode_to_path: RwLock<HashMap<Inode, PathBuf>>,
    next_inode: AtomicU64,
    open_files: RwLock<HashMap<u64, Arc<fs::File>>>,
    next_fh: AtomicU64,
    chunk_manifest_cache: RwLock<HashMap<Digest, Arc<ChunkManifest>>>,
    uid: u32,
    gid: u32,
    commit_time: Duration,
}

impl GraftWorkspaceFilesystem {
    /// Initializes a writeable workspace filesystem backed by a base CAS snapshot and a mutable upper directory.
    pub fn new<P: AsRef<Path>>(
        lower_tree: Arc<RwLock<VfsTree>>,
        blob_store: Arc<dyn BlobStore>,
        upper_dir: P,
    ) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let commit_time = Duration::from_secs(1_767_225_600);

        let mut path_to_inode = HashMap::new();
        let mut inode_to_path = HashMap::new();

        // Seed root inode
        path_to_inode.insert(PathBuf::from(""), ROOT_INODE);
        inode_to_path.insert(ROOT_INODE, PathBuf::from(""));

        Self {
            lower_tree,
            blob_store,
            upper_dir: upper_dir.as_ref().to_path_buf(),
            path_to_inode: RwLock::new(path_to_inode),
            inode_to_path: RwLock::new(inode_to_path),
            next_inode: AtomicU64::new(10_000_000),
            open_files: RwLock::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            chunk_manifest_cache: RwLock::new(HashMap::new()),
            uid,
            gid,
            commit_time,
        }
    }

    /// Initializes from an owned `VfsTree`.
    pub fn from_tree<P: AsRef<Path>>(
        tree: VfsTree,
        blob_store: Arc<dyn BlobStore>,
        upper_dir: P,
    ) -> Self {
        Self::new(Arc::new(RwLock::new(tree)), blob_store, upper_dir)
    }

    /// Sets the reported commit timestamp for unmodified lower-layer files.
    pub fn with_commit_time(mut self, time: Duration) -> Self {
        self.commit_time = time;
        self
    }

    /// Looks up or assigns a stable Inode for the given repository-relative path.
    pub fn get_or_assign_inode(&self, rel_path: &Path) -> Inode {
        {
            let map = self.path_to_inode.read().unwrap();
            if let Some(ino) = map.get(rel_path) {
                return *ino;
            }
        }

        let mut path_map = self.path_to_inode.write().unwrap();
        let mut inode_map = self.inode_to_path.write().unwrap();

        if let Some(ino) = path_map.get(rel_path) {
            return *ino;
        }

        let ino = self.next_inode.fetch_add(1, Ordering::SeqCst);
        let path_buf = rel_path.to_path_buf();
        path_map.insert(path_buf.clone(), ino);
        inode_map.insert(ino, path_buf);
        ino
    }

    /// Returns the repository-relative path for an Inode.
    pub fn get_path(&self, ino: Inode) -> Option<PathBuf> {
        self.inode_to_path.read().unwrap().get(&ino).cloned()
    }

    /// Remaps cached paths in the Inode table following a directory rename.
    pub fn remap_descendants(&self, old_rel: &Path, new_rel: &Path) {
        let mut path_map = self.path_to_inode.write().unwrap();
        let mut inode_map = self.inode_to_path.write().unwrap();

        let mut remapped = Vec::new();
        for (path, ino) in path_map.iter() {
            if path == old_rel {
                remapped.push((*ino, new_rel.to_path_buf()));
            } else if let Ok(suffix) = path.strip_prefix(old_rel) {
                remapped.push((*ino, new_rel.join(suffix)));
            }
        }

        for (ino, new_p) in remapped {
            if let Some(old_p) = inode_map.insert(ino, new_p.clone()) {
                path_map.remove(&old_p);
            }
            path_map.insert(new_p, ino);
        }
    }

    /// Checks if a path is hidden by a whiteout file (`.wh.<name>`) or an opaque directory (`.wh..wh..opq`).
    pub fn is_whiteouted(&self, rel_path: &Path) -> bool {
        let mut current = PathBuf::new();
        let components: Vec<_> = rel_path.components().collect();

        for (i, comp) in components.iter().enumerate() {
            let name = comp.as_os_str().to_string_lossy();
            let wh_path = self.upper_dir.join(&current).join(format!(".wh.{}", name));
            if wh_path.exists() {
                // If the item physically exists in upper_dir, it was recreated; not whiteouted
                let physical = self.upper_dir.join(&current).join(comp.as_os_str());
                if !physical.exists() {
                    return true;
                }
            }

            current.push(comp.as_os_str());

            // If current is an opaque directory and we are looking at a descendant:
            if i + 1 < components.len() {
                let opq_path = self.upper_dir.join(&current).join(".wh..wh..opq");
                if opq_path.exists() {
                    let rest: PathBuf = components[i + 1..].iter().collect();
                    let full_upper = self.upper_dir.join(&current).join(rest);
                    if !full_upper.exists() {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Resolves metadata for an entry in the lower `VfsTree`.
    pub fn get_lower_node_info(&self, rel_path: &Path) -> Option<(VfsEntryType, Digest, u64)> {
        let mut current_ino = ROOT_INODE;
        let components: Vec<_> = rel_path.components().collect();

        if components.is_empty() {
            let tree = self.lower_tree.read().ok()?;
            let node = tree.get_node(ROOT_INODE)?;
            return Some((node.entry_type, node.digest, node.size_bytes));
        }

        for (i, comp) in components.iter().enumerate() {
            let name = comp.as_os_str().to_string_lossy();

            let needs_load = {
                let tree = self.lower_tree.read().ok()?;
                !tree.is_dir_loaded(current_ino)
            };
            if needs_load {
                let mut tree = self.lower_tree.write().ok()?;
                let _ = tree.ensure_dir_loaded(current_ino, self.blob_store.as_ref());
            }

            let tree = self.lower_tree.read().ok()?;
            let child = tree.lookup(current_ino, &name)?;

            if i + 1 == components.len() {
                return Some((child.entry_type, child.digest, child.size_bytes));
            } else {
                current_ino = child.inode;
            }
        }

        None
    }

    /// Checks if an entry exists in the lower Merkle tree and is not whiteouted.
    pub fn exists_in_lower(&self, rel_path: &Path) -> bool {
        if self.is_whiteouted(rel_path) {
            return false;
        }
        self.get_lower_node_info(rel_path).is_some()
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

    /// Ensures that a file from the lower layer is copied into `upper_dir` before modification.
    pub fn ensure_copied_up(&self, rel_path: &Path) -> io::Result<PathBuf> {
        let upper_path = self.upper_dir.join(rel_path);
        if upper_path.exists() {
            return Ok(upper_path);
        }

        if let Some(parent) = upper_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Remove any whiteout marker in upper_dir
        if let Some(file_name) = rel_path.file_name() {
            if let Some(parent_rel) = rel_path.parent() {
                let wh = self
                    .upper_dir
                    .join(parent_rel)
                    .join(format!(".wh.{}", file_name.to_string_lossy()));
                let _ = fs::remove_file(wh);
            }
        }

        let (entry_type, digest, _) = self.get_lower_node_info(rel_path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Entry does not exist in lower tree")
        })?;

        let temp_path = upper_path.with_extension(format!("tmp_copy_{}", std::process::id()));

        match entry_type {
            VfsEntryType::RegularFile { is_executable } => {
                let content = if let Some(path) = self.blob_store.get_file_path(&digest) {
                    fs::read(&path)?
                } else {
                    self.blob_store
                        .get(&digest)?
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Blob missing"))?
                };
                fs::write(&temp_path, &content)?;
                let mode = if is_executable { 0o755 } else { 0o644 };
                fs::set_permissions(&temp_path, fs::Permissions::from_mode(mode))?;
                fs::rename(&temp_path, &upper_path)?;
            }
            VfsEntryType::ChunkedFile {
                is_executable,
                manifest_digest,
            } => {
                let manifest = self
                    .get_or_load_chunk_manifest(&manifest_digest)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                let mut outfile = fs::File::create(&temp_path)?;
                use std::io::Write;
                for chunk in &manifest.chunks {
                    let chunk_bytes = self.blob_store.get(&chunk.digest)?.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "Missing chunk in CAS")
                    })?;
                    outfile.write_all(&chunk_bytes)?;
                }
                outfile.flush()?;
                drop(outfile);
                let mode = if is_executable { 0o755 } else { 0o644 };
                fs::set_permissions(&temp_path, fs::Permissions::from_mode(mode))?;
                fs::rename(&temp_path, &upper_path)?;
            }
            VfsEntryType::Directory => {
                fs::create_dir_all(&upper_path)?;
            }
            VfsEntryType::Symlink => {
                let target = self
                    .blob_store
                    .get(&digest)?
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Symlink blob missing"))?;
                let target_str = String::from_utf8_lossy(&target);
                std::os::unix::fs::symlink(target_str.as_ref(), &upper_path)?;
            }
        }

        Ok(upper_path)
    }

    /// Converts a path to standard `FileAttr`, taking `upper_dir` mutations over lower defaults.
    pub fn get_attr_for_path(&self, rel_path: &Path) -> Option<FileAttr> {
        if self.is_whiteouted(rel_path) {
            return None;
        }

        let upper_path = self.upper_dir.join(rel_path);
        let ino = self.get_or_assign_inode(rel_path);

        if upper_path.exists() {
            let meta = fs::symlink_metadata(&upper_path).ok()?;
            let file_type = meta.file_type();
            let kind = if file_type.is_dir() {
                FileType::Directory
            } else if file_type.is_symlink() {
                FileType::Symlink
            } else {
                FileType::RegularFile
            };

            let perm = (meta.permissions().mode() & 0o7777) as u16;
            let size = meta.len();
            let blocks = (size + 511) / 512;

            let epoch = SystemTime::UNIX_EPOCH;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(epoch).ok())
                .unwrap_or(self.commit_time);
            let atime = meta
                .accessed()
                .ok()
                .and_then(|t| t.duration_since(epoch).ok())
                .unwrap_or(self.commit_time);

            return Some(FileAttr {
                ino,
                size,
                blocks,
                atime,
                mtime,
                ctime: mtime,
                crtime: mtime,
                kind,
                perm,
                nlink: if kind == FileType::Directory { 2 } else { 1 },
                uid: self.uid,
                gid: self.gid,
                rdev: 0,
                flags: 0,
                blksize: 4096,
            });
        }

        // Fall back to lower tree
        let (entry_type, _, size) = self.get_lower_node_info(rel_path)?;
        let (kind, perm) = match entry_type {
            VfsEntryType::Directory => (FileType::Directory, 0o755),
            VfsEntryType::RegularFile { is_executable }
            | VfsEntryType::ChunkedFile { is_executable, .. } => {
                let p = if is_executable { 0o755 } else { 0o644 };
                (FileType::RegularFile, p)
            }
            VfsEntryType::Symlink => (FileType::Symlink, 0o777),
        };

        let blocks = (size + 511) / 512;

        Some(FileAttr {
            ino,
            size,
            blocks,
            atime: self.commit_time,
            mtime: self.commit_time,
            ctime: self.commit_time,
            crtime: self.commit_time,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        })
    }

    /// Reads up to `size` bytes starting at `offset` for a file.
    pub fn read_file_content(
        &self,
        rel_path: &Path,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, String> {
        if self.is_whiteouted(rel_path) {
            return Err("File is deleted".to_string());
        }

        let upper_path = self.upper_dir.join(rel_path);
        if upper_path.exists() {
            let file = fs::File::open(&upper_path).map_err(|e| format!("Open error: {}", e))?;
            let mut buf = vec![0u8; size as usize];
            let n = file
                .read_at(&mut buf, offset)
                .map_err(|e| format!("pread64 error: {}", e))?;
            buf.truncate(n);
            return Ok(buf);
        }

        // Read from lower
        let (entry_type, digest, _) = self
            .get_lower_node_info(rel_path)
            .ok_or_else(|| "Not found in lower tree".to_string())?;

        match entry_type {
            VfsEntryType::RegularFile { .. } => {
                if let Some(path) = self.blob_store.get_file_path(&digest) {
                    let file =
                        fs::File::open(&path).map_err(|e| format!("Local cache open: {}", e))?;
                    let mut buf = vec![0u8; size as usize];
                    let n = file
                        .read_at(&mut buf, offset)
                        .map_err(|e| format!("pread64: {}", e))?;
                    buf.truncate(n);
                    Ok(buf)
                } else {
                    let blob = self
                        .blob_store
                        .get(&digest)
                        .map_err(|e| format!("CAS fetch error: {}", e))?
                        .ok_or_else(|| format!("Blob {} missing", digest))?;
                    let offset = offset as usize;
                    let size = size as usize;
                    if offset >= blob.len() {
                        return Ok(Vec::new());
                    }
                    let end = (offset + size).min(blob.len());
                    Ok(blob[offset..end].to_vec())
                }
            }
            VfsEntryType::ChunkedFile { manifest_digest, .. } => {
                let manifest = self.get_or_load_chunk_manifest(&manifest_digest)?;
                read_chunked_range(&manifest, offset, size, self.blob_store.as_ref())
                    .map_err(|e| format!("Chunked read error: {}", e))
            }
            _ => Err("Not a regular or chunked file".to_string()),
        }
    }

    /// Lists merged directory contents for a directory path.
    pub fn readdir_for_path(&self, rel_path: &Path) -> Option<Vec<DirEntry>> {
        if self.is_whiteouted(rel_path) {
            return None;
        }

        let current_ino = self.get_or_assign_inode(rel_path);
        let parent_ino = if rel_path.as_os_str().is_empty() {
            ROOT_INODE
        } else {
            let parent_p = rel_path.parent().unwrap_or(Path::new(""));
            self.get_or_assign_inode(parent_p)
        };

        let mut entries = Vec::new();
        // Entry 1: "."
        entries.push(DirEntry {
            ino: current_ino,
            kind: FileType::Directory,
            name: ".".to_string(),
        });
        // Entry 2: ".."
        entries.push(DirEntry {
            ino: parent_ino,
            kind: FileType::Directory,
            name: "..".to_string(),
        });

        let upper_path = self.upper_dir.join(rel_path);
        let mut whiteouts = HashSet::new();
        let mut upper_names = HashSet::new();
        let mut is_opaque = false;

        if upper_path.is_dir() {
            if let Ok(read_dir) = fs::read_dir(&upper_path) {
                for entry in read_dir.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name == ".wh..wh..opq" {
                        is_opaque = true;
                    } else if let Some(wh_name) = name.strip_prefix(".wh.") {
                        whiteouts.insert(wh_name.to_string());
                    } else {
                        upper_names.insert(name);
                    }
                }
            }
        }

        // 1. Lower entries (if not opaque)
        if !is_opaque {
            if let Some((VfsEntryType::Directory, _, _)) = self.get_lower_node_info(rel_path) {
                // Find lower node inode
                let mut current_lower_ino = ROOT_INODE;
                let components: Vec<_> = rel_path.components().collect();
                let mut lower_found = true;

                for comp in &components {
                    let name = comp.as_os_str().to_string_lossy();
                    let tree = self.lower_tree.read().ok()?;
                    if let Some(child) = tree.lookup(current_lower_ino, &name) {
                        current_lower_ino = child.inode;
                    } else {
                        lower_found = false;
                        break;
                    }
                }

                if lower_found {
                    let needs_load = {
                        let tree = self.lower_tree.read().ok()?;
                        !tree.is_dir_loaded(current_lower_ino)
                    };
                    if needs_load {
                        let mut tree = self.lower_tree.write().ok()?;
                        let _ =
                            tree.ensure_dir_loaded(current_lower_ino, self.blob_store.as_ref());
                    }

                    let tree = self.lower_tree.read().ok()?;
                    for child in tree.list_children(current_lower_ino) {
                        if whiteouts.contains(&child.name) || upper_names.contains(&child.name) {
                            continue;
                        }
                        let child_rel = if rel_path.as_os_str().is_empty() {
                            PathBuf::from(&child.name)
                        } else {
                            rel_path.join(&child.name)
                        };
                        let child_ino = self.get_or_assign_inode(&child_rel);
                        let kind = match child.entry_type {
                            VfsEntryType::Directory => FileType::Directory,
                            VfsEntryType::RegularFile { .. }
                            | VfsEntryType::ChunkedFile { .. } => FileType::RegularFile,
                            VfsEntryType::Symlink => FileType::Symlink,
                        };
                        entries.push(DirEntry {
                            ino: child_ino,
                            kind,
                            name: child.name.clone(),
                        });
                    }
                }
            }
        }

        // 2. Upper entries
        if upper_path.is_dir() {
            if let Ok(read_dir) = fs::read_dir(&upper_path) {
                for entry in read_dir.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with(".wh.") {
                        continue;
                    }
                    let child_rel = if rel_path.as_os_str().is_empty() {
                        PathBuf::from(&name)
                    } else {
                        rel_path.join(&name)
                    };
                    let child_ino = self.get_or_assign_inode(&child_rel);
                    let ft = entry.file_type().ok()?;
                    let kind = if ft.is_dir() {
                        FileType::Directory
                    } else if ft.is_symlink() {
                        FileType::Symlink
                    } else {
                        FileType::RegularFile
                    };
                    entries.push(DirEntry {
                        ino: child_ino,
                        kind,
                        name,
                    });
                }
            }
        }

        // Deterministic sort by name (keeping . and .. at the front)
        entries[2..].sort_by(|a, b| a.name.cmp(&b.name));
        Some(entries)
    }
}

#[cfg(feature = "fuse")]
pub mod fuse_workspace_impl {
    use super::*;
    use fuser::{
        FileAttr as FuserFileAttr, FileType as FuserFileType, Filesystem, MountOption,
        ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
        ReplyWrite, Request, TimeOrNow,
    };
    use std::ffi::OsStr;
    use std::time::SystemTime;

    impl GraftWorkspaceFilesystem {
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

    impl Filesystem for GraftWorkspaceFilesystem {
        fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            let parent_path = match self.get_path(parent) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            let rel_path = if parent_path.as_os_str().is_empty() {
                PathBuf::from(name_str)
            } else {
                parent_path.join(name_str)
            };

            match self.get_attr_for_path(&rel_path) {
                Some(attr) => {
                    let fuser_attr = self.to_fuser_attr(&attr);
                    reply.entry(&WORKSPACE_TTL, &fuser_attr, 0);
                }
                None => reply.error(libc::ENOENT),
            }
        }

        fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
            let rel_path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            match self.get_attr_for_path(&rel_path) {
                Some(attr) => {
                    let fuser_attr = self.to_fuser_attr(&attr);
                    reply.attr(&WORKSPACE_TTL, &fuser_attr);
                }
                None => reply.error(libc::ENOENT),
            }
        }

        fn setattr(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            mode: Option<u32>,
            uid: Option<u32>,
            gid: Option<u32>,
            size: Option<u64>,
            atime: Option<TimeOrNow>,
            mtime: Option<TimeOrNow>,
            _ctime: Option<SystemTime>,
            fh: Option<u64>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<u32>,
            reply: ReplyAttr,
        ) {
            let rel_path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            if self.is_whiteouted(&rel_path) {
                reply.error(libc::ENOENT);
                return;
            }

            // Check if directory
            let is_dir = {
                let upper_p = self.upper_dir.join(&rel_path);
                if upper_p.exists() {
                    upper_p.is_dir()
                } else {
                    matches!(
                        self.get_lower_node_info(&rel_path),
                        Some((VfsEntryType::Directory, _, _))
                    )
                }
            };

            if is_dir {
                // Directories: modify permissions or create in upper without OpenOptions::write(true)
                let upper_p = self.upper_dir.join(&rel_path);
                if !upper_p.exists() {
                    let _ = fs::create_dir_all(&upper_p);
                }
                if let Some(m) = mode {
                    let _ = fs::set_permissions(
                        &upper_p,
                        fs::Permissions::from_mode(m & 0o7777),
                    );
                }
            } else {
                // File: copy-up first if only in lower
                let upper_p = match self.ensure_copied_up(&rel_path) {
                    Ok(p) => p,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };

                // Truncate size
                if let Some(new_size) = size {
                    let truncated = if let Some(handle) = fh {
                        let open_files = self.open_files.read().unwrap();
                        if let Some(file) = open_files.get(&handle) {
                            file.set_len(new_size).is_ok()
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !truncated {
                        if let Ok(file) = fs::OpenOptions::new().write(true).open(&upper_p) {
                            let _ = file.set_len(new_size);
                        } else {
                            reply.error(libc::EIO);
                            return;
                        }
                    }
                }

                // Change mode (mask off file-type bits)
                if let Some(m) = mode {
                    let _ = fs::set_permissions(
                        &upper_p,
                        fs::Permissions::from_mode(m & 0o7777),
                    );
                }

                // Timestamps
                if mtime.is_some() || atime.is_some() {
                    let mut times = fs::FileTimes::new();
                    if let Some(at) = atime {
                        let t = match at {
                            TimeOrNow::Now => SystemTime::now(),
                            TimeOrNow::SpecificTime(st) => st,
                        };
                        times = times.set_accessed(t);
                    }
                    if let Some(mt) = mtime {
                        let t = match mt {
                            TimeOrNow::Now => SystemTime::now(),
                            TimeOrNow::SpecificTime(st) => st,
                        };
                        times = times.set_modified(t);
                    }
                    if let Ok(f) = fs::File::open(&upper_p) {
                        let _ = f.set_times(times);
                    }
                }
            }

            // Cross-user chown check: treat own UID/GID as no-op; error EPERM only on mismatched foreign UID
            if let Some(u) = uid {
                if u != self.uid && u != 0 {
                    reply.error(libc::EPERM);
                    return;
                }
            }
            if let Some(g) = gid {
                if g != self.gid && g != 0 {
                    reply.error(libc::EPERM);
                    return;
                }
            }

            match self.get_attr_for_path(&rel_path) {
                Some(attr) => {
                    let fuser_attr = self.to_fuser_attr(&attr);
                    reply.attr(&WORKSPACE_TTL, &fuser_attr);
                }
                None => reply.error(libc::ENOENT),
            }
        }

        fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
            let rel_path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            if self.is_whiteouted(&rel_path) {
                reply.error(libc::ENOENT);
                return;
            }

            let is_write = (flags & libc::O_ACCMODE) != libc::O_RDONLY
                || (flags & libc::O_TRUNC) != 0
                || (flags & libc::O_APPEND) != 0;

            if is_write {
                let upper_p = match self.ensure_copied_up(&rel_path) {
                    Ok(p) => p,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };

                let file = match fs::OpenOptions::new().read(true).write(true).open(&upper_p) {
                    Ok(f) => f,
                    Err(e) => {
                        reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                        return;
                    }
                };

                let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
                self.open_files.write().unwrap().insert(fh, Arc::new(file));
                reply.opened(fh, 0);
            } else {
                let upper_p = self.upper_dir.join(&rel_path);
                if upper_p.exists() {
                    let file = match fs::File::open(&upper_p) {
                        Ok(f) => f,
                        Err(e) => {
                            reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                            return;
                        }
                    };
                    let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
                    self.open_files.write().unwrap().insert(fh, Arc::new(file));
                    reply.opened(fh, 0);
                } else if self.exists_in_lower(&rel_path) {
                    // Stateless read from lower CAS
                    reply.opened(0, 0);
                } else {
                    reply.error(libc::ENOENT);
                }
            }
        }

        fn read(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            fh: u64,
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

            let rel_path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            // Fast path: Positional pread64 on opened file handle
            if fh != 0 {
                let file_arc = {
                    let map = self.open_files.read().unwrap();
                    map.get(&fh).cloned()
                };

                if let Some(file) = file_arc {
                    let mut buf = vec![0u8; size as usize];
                    match file.read_at(&mut buf, offset as u64) {
                        Ok(n) => {
                            buf.truncate(n);
                            reply.data(&buf);
                            return;
                        }
                        Err(e) => {
                            reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                            return;
                        }
                    }
                }
            }

            // Fallback: Read via path (checking upper first, then lower)
            match self.read_file_content(&rel_path, offset as u64, size) {
                Ok(data) => reply.data(&data),
                Err(_) => reply.error(libc::EIO),
            }
        }

        fn write(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            fh: u64,
            offset: i64,
            data: &[u8],
            _write_flags: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyWrite,
        ) {
            if offset < 0 {
                reply.error(libc::EINVAL);
                return;
            }

            let rel_path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            // Fast path: Positional pwrite64 on opened file handle
            if fh != 0 {
                let file_arc = {
                    let map = self.open_files.read().unwrap();
                    map.get(&fh).cloned()
                };

                if let Some(file) = file_arc {
                    match file.write_at(data, offset as u64) {
                        Ok(n) => {
                            reply.written(n as u32);
                            return;
                        }
                        Err(e) => {
                            reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                            return;
                        }
                    }
                }
            }

            // Fallback: Open and write directly
            let upper_p = match self.ensure_copied_up(&rel_path) {
                Ok(p) => p,
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
            };

            match fs::OpenOptions::new().write(true).open(&upper_p) {
                Ok(file) => match file.write_at(data, offset as u64) {
                    Ok(n) => reply.written(n as u32),
                    Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
                },
                Err(e) => reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        }

        fn create(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            mode: u32,
            _umask: u32,
            _flags: i32,
            reply: ReplyCreate,
        ) {
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            let parent_path = match self.get_path(parent) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            let rel_path = if parent_path.as_os_str().is_empty() {
                PathBuf::from(name_str)
            } else {
                parent_path.join(name_str)
            };

            let upper_p = self.upper_dir.join(&rel_path);

            if let Some(parent_p) = upper_p.parent() {
                let _ = fs::create_dir_all(parent_p);
            }

            // Remove any whiteout file if previously deleted
            if let Some(parent_rel) = rel_path.parent() {
                let wh = self
                    .upper_dir
                    .join(parent_rel)
                    .join(format!(".wh.{}", name_str));
                let _ = fs::remove_file(wh);
            }

            let file = match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .mode(mode & 0o7777)
                .open(&upper_p)
            {
                Ok(f) => f,
                Err(e) => {
                    reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                    return;
                }
            };

            let _ino = self.get_or_assign_inode(&rel_path);
            let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
            self.open_files.write().unwrap().insert(fh, Arc::new(file));

            match self.get_attr_for_path(&rel_path) {
                Some(attr) => {
                    let fuser_attr = self.to_fuser_attr(&attr);
                    reply.created(&WORKSPACE_TTL, &fuser_attr, 0, fh, 0);
                }
                None => reply.error(libc::EIO),
            }
        }

        fn mkdir(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            mode: u32,
            _umask: u32,
            reply: ReplyEntry,
        ) {
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            let parent_path = match self.get_path(parent) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            let rel_path = if parent_path.as_os_str().is_empty() {
                PathBuf::from(name_str)
            } else {
                parent_path.join(name_str)
            };

            let upper_p = self.upper_dir.join(&rel_path);

            // If a whiteout marker existed for this directory in parent, remove it and mark new dir opaque
            let had_whiteout = if let Some(parent_rel) = rel_path.parent() {
                let wh = self
                    .upper_dir
                    .join(parent_rel)
                    .join(format!(".wh.{}", name_str));
                fs::remove_file(wh).is_ok()
            } else {
                false
            };

            if let Err(e) = fs::create_dir_all(&upper_p) {
                reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                return;
            }

            let _ = fs::set_permissions(&upper_p, fs::Permissions::from_mode(mode & 0o7777));

            if had_whiteout {
                let _ = fs::File::create(upper_p.join(".wh..wh..opq"));
            }

            let _ino = self.get_or_assign_inode(&rel_path);
            match self.get_attr_for_path(&rel_path) {
                Some(attr) => {
                    let fuser_attr = self.to_fuser_attr(&attr);
                    reply.entry(&WORKSPACE_TTL, &fuser_attr, 0);
                }
                None => reply.error(libc::EIO),
            }
        }

        fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            let parent_path = match self.get_path(parent) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            let rel_path = if parent_path.as_os_str().is_empty() {
                PathBuf::from(name_str)
            } else {
                parent_path.join(name_str)
            };

            let upper_p = self.upper_dir.join(&rel_path);
            let existed_in_upper = upper_p.exists();
            if existed_in_upper {
                let _ = fs::remove_file(&upper_p);
            }

            // If existed in lower Merkle tree, write .wh.<filename> tombstone
            if self.exists_in_lower(&rel_path) {
                let upper_parent = self.upper_dir.join(&parent_path);
                let _ = fs::create_dir_all(&upper_parent);
                let wh_path = upper_parent.join(format!(".wh.{}", name_str));
                let _ = fs::File::create(wh_path);
            } else if !existed_in_upper {
                reply.error(libc::ENOENT);
                return;
            }

            reply.ok();
        }

        fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            let parent_path = match self.get_path(parent) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            let rel_path = if parent_path.as_os_str().is_empty() {
                PathBuf::from(name_str)
            } else {
                parent_path.join(name_str)
            };

            // Check logical emptiness (excluding . and ..)
            if let Some(entries) = self.readdir_for_path(&rel_path) {
                let non_dots = entries.iter().filter(|e| e.name != "." && e.name != "..").count();
                if non_dots > 0 {
                    reply.error(libc::ENOTEMPTY);
                    return;
                }
            } else {
                reply.error(libc::ENOENT);
                return;
            }

            let upper_p = self.upper_dir.join(&rel_path);
            if upper_p.exists() {
                // Clear out any internal whiteouts or files
                let _ = fs::remove_dir_all(&upper_p);
            }

            if self.exists_in_lower(&rel_path) {
                let upper_parent = self.upper_dir.join(&parent_path);
                let _ = fs::create_dir_all(&upper_parent);
                let wh_path = upper_parent.join(format!(".wh.{}", name_str));
                let _ = fs::File::create(wh_path);
            }

            reply.ok();
        }

        fn rename(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            newparent: u64,
            newname: &OsStr,
            flags: u32,
            reply: ReplyEmpty,
        ) {
            // Check flags
            if (flags & libc::RENAME_EXCHANGE) != 0 {
                reply.error(libc::EINVAL);
                return;
            }

            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };
            let newname_str = match newname.to_str() {
                Some(s) => s,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            let parent_path = match self.get_path(parent) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };
            let newparent_path = match self.get_path(newparent) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            let old_rel = if parent_path.as_os_str().is_empty() {
                PathBuf::from(name_str)
            } else {
                parent_path.join(name_str)
            };
            let new_rel = if newparent_path.as_os_str().is_empty() {
                PathBuf::from(newname_str)
            } else {
                newparent_path.join(newname_str)
            };

            if (flags & libc::RENAME_NOREPLACE) != 0 && self.get_attr_for_path(&new_rel).is_some() {
                reply.error(libc::EEXIST);
                return;
            }

            let upper_old = self.upper_dir.join(&old_rel);
            let upper_new = self.upper_dir.join(&new_rel);

            if let Some(p) = upper_new.parent() {
                let _ = fs::create_dir_all(p);
            }

            // Remove destination whiteout if present
            let new_wh = self
                .upper_dir
                .join(&newparent_path)
                .join(format!(".wh.{}", newname_str));
            let _ = fs::remove_file(new_wh);

            let old_existed_in_lower = self.exists_in_lower(&old_rel);

            if upper_old.exists() {
                if let Err(e) = fs::rename(&upper_old, &upper_new) {
                    reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                    return;
                }
            } else if old_existed_in_lower {
                // Copy-up old to new path in upper
                if self.ensure_copied_up(&old_rel).is_err() {
                    reply.error(libc::EIO);
                    return;
                }
                if let Err(e) = fs::rename(&upper_old, &upper_new) {
                    reply.error(e.raw_os_error().unwrap_or(libc::EIO));
                    return;
                }
            } else {
                reply.error(libc::ENOENT);
                return;
            }

            if old_existed_in_lower {
                let upper_old_parent = self.upper_dir.join(&parent_path);
                let _ = fs::create_dir_all(&upper_old_parent);
                let _ = fs::File::create(upper_old_parent.join(format!(".wh.{}", name_str)));
            }

            // Remap cached Inode table paths
            self.remap_descendants(&old_rel, &new_rel);

            reply.ok();
        }

        fn flush(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
            if fh != 0 {
                let map = self.open_files.read().unwrap();
                if let Some(file) = map.get(&fh) {
                    let _ = file.sync_data();
                }
            }
            reply.ok();
        }

        fn fsync(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
            if fh != 0 {
                let map = self.open_files.read().unwrap();
                if let Some(file) = map.get(&fh) {
                    let _ = file.sync_data();
                }
            }
            reply.ok();
        }

        fn release(
            &mut self,
            _req: &Request<'_>,
            _ino: u64,
            fh: u64,
            _flags: i32,
            _lock_owner: Option<u64>,
            _flush: bool,
            reply: ReplyEmpty,
        ) {
            if fh != 0 {
                self.open_files.write().unwrap().remove(&fh);
            }
            reply.ok();
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

            let rel_path = match self.get_path(ino) {
                Some(p) => p,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            let entries = match self.readdir_for_path(&rel_path) {
                Some(e) => e,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
                let kind = match entry.kind {
                    FileType::Directory => FuserFileType::Directory,
                    FileType::RegularFile => FuserFileType::RegularFile,
                    FileType::Symlink => FuserFileType::Symlink,
                };

                if reply.add(entry.ino, (i + 1) as i64, kind, &entry.name) {
                    break;
                }
            }

            reply.ok();
        }
    }

    /// Mount options for writeable Graft FUSE workspaces (read-write, default permissions).
    pub fn default_workspace_mount_options() -> Vec<MountOption> {
        vec![
            MountOption::FSName("graft-workspace".to_string()),
            MountOption::DefaultPermissions,
        ]
    }

    /// Mounts the writeable workspace filesystem in a blocking loop on the calling thread.
    pub fn mount_workspace_fuse<P: AsRef<Path>>(
        fs: GraftWorkspaceFilesystem,
        mountpoint: P,
        custom_options: Option<&[MountOption]>,
    ) -> io::Result<()> {
        let default_opts = default_workspace_mount_options();
        let opts = custom_options.unwrap_or(&default_opts);
        fuser::mount2(fs, mountpoint, opts)
    }

    /// Spawns a background FUSE session for the writeable workspace filesystem.
    pub fn spawn_mount_workspace_fuse<P: AsRef<Path>>(
        fs: GraftWorkspaceFilesystem,
        mountpoint: P,
        custom_options: Option<&[MountOption]>,
    ) -> io::Result<fuser::BackgroundSession> {
        let default_opts = default_workspace_mount_options();
        let opts = custom_options.unwrap_or(&default_opts);
        fuser::spawn_mount2(fs, mountpoint, opts)
    }
}

#[cfg(feature = "fuse")]
pub use fuse_workspace_impl::{
    default_workspace_mount_options, mount_workspace_fuse, spawn_mount_workspace_fuse,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use tempfile::tempdir;

    #[test]
    fn test_workspace_fs_lookup_and_copy_up() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let store = Arc::new(LocalBlobStore::new(tmp.path().join("cas"))?);
        let upper_dir = tmp.path().join("upper");
        fs::create_dir_all(&upper_dir)?;

        let content = b"original lower content";
        let digest = store.put(content)?;

        let mut tree = VfsTree::new(Digest::from_bytes(b"root"));
        tree.add_child(
            ROOT_INODE,
            "test.txt",
            VfsEntryType::RegularFile {
                is_executable: false,
            },
            digest,
            content.len() as u64,
        );

        let ws_fs = GraftWorkspaceFilesystem::from_tree(tree, store, &upper_dir);

        // 1. Initial getattr reflects lower content
        let attr = ws_fs
            .get_attr_for_path(Path::new("test.txt"))
            .expect("must find test.txt");
        assert_eq!(attr.size, content.len() as u64);

        // 2. Read content reflects lower
        let bytes = ws_fs
            .read_file_content(Path::new("test.txt"), 0, 100)
            .expect("read lower");
        assert_eq!(bytes, content);

        // 3. Ensure copied up
        let copied = ws_fs
            .ensure_copied_up(Path::new("test.txt"))
            .expect("copy up");
        assert!(copied.exists());

        // 4. Modify upper file
        fs::write(&copied, b"mutated upper content")?;

        // 5. Read reflects mutated upper content
        let new_bytes = ws_fs
            .read_file_content(Path::new("test.txt"), 0, 100)
            .expect("read upper");
        assert_eq!(new_bytes, b"mutated upper content");

        // 6. Delete (whiteout) test.txt
        fs::remove_file(&copied)?;
        fs::File::create(upper_dir.join(".wh.test.txt"))?;

        // Now get_attr_for_path and read must return None / Err
        assert!(ws_fs.get_attr_for_path(Path::new("test.txt")).is_none());
        assert!(ws_fs.read_file_content(Path::new("test.txt"), 0, 100).is_err());

        // Readdir should not contain test.txt
        let readdir = ws_fs.readdir_for_path(Path::new("")).expect("readdir");
        assert!(!readdir.iter().any(|e| e.name == "test.txt"));

        Ok(())
    }
}
