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

//! # Git & Repository Ingestion Engine
//!
//! This module ingests existing codebases or Git tree structures into Graft's Content-Addressed
//! Storage (CAS) and constructs the in-memory `VfsTree` Merkle hierarchy.
//!
//! ### Deterministic Merkle Tree Hashing:
//! To guarantee that identical repository states produce the exact same root `Digest` across
//! different machines, Graft enforces canonical ordering:
//! 1. Directory entries are strictly sorted alphabetically by filename before generating the manifest.
//! 2. Directory manifests are formatted as:
//!    - `FILE <mode> <name> <digest>\n` (e.g. `FILE 0644 main.rs c54b58c2...`)
//!    - `DIR <name> <digest>\n`        (e.g. `DIR src a7bc7e80...`)
//! 3. The manifest bytes are hashed with BLAKE3 and stored in CAS as an immutable directory blob.

use crate::cas::blob_store::BlobStore;
use crate::cas::chunker::{store_file_or_chunked, ChunkerConfig};
use crate::cas::commit::ManifestEntry;
use crate::cas::digest::Digest;
use crate::vfs::{Inode, VfsEntryType, VfsTree, ROOT_INODE};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Result summary returned after ingesting a repository directory.
#[derive(Debug)]
pub struct IngestResult {
    /// The top-level Merkle root digest of the entire repository snapshot.
    pub root_digest: Digest,
    /// The in-memory virtual filesystem tree constructed during ingestion.
    pub tree: VfsTree,
    /// Total number of individual files ingested into CAS.
    pub total_files: usize,
    /// Total aggregate uncompressed size in bytes across all files.
    pub total_bytes: u64,
}

/// Ingests a local repository or directory tree into Graft CAS and constructs a VfsTree.
///
/// Skips `.git`, `.citc`, and other internal VCS metadata directories.
/// Enforces deterministic alphabetical sorting of entries so that identical directory
/// contents always produce the exact same Merkle root digest.
///
/// # Arguments
/// * `dir_path` - Path to the directory or repository on the local filesystem.
/// * `blob_store` - The backing CAS store where file blobs and manifests will be written.
///
/// # Returns
/// An `IngestResult` containing the root Merkle digest, total file count, total byte count,
/// and the populated in-memory `VfsTree`.
///
/// # Errors
/// Returns an `io::Error` if filesystem reads or CAS writes fail.
pub fn ingest_directory<P: AsRef<Path>, B: BlobStore + ?Sized>(
    dir_path: P,
    blob_store: &B,
) -> io::Result<IngestResult> {
    ingest_directory_with_config(dir_path, blob_store, &ChunkerConfig::default())
}

/// Ingests a directory into CAS using a custom `ChunkerConfig`.
pub fn ingest_directory_with_config<P: AsRef<Path>, B: BlobStore + ?Sized>(
    dir_path: P,
    blob_store: &B,
    config: &ChunkerConfig,
) -> io::Result<IngestResult> {
    let path = dir_path.as_ref();
    let mut tree = VfsTree::new(Digest::from_bytes(b"root-placeholder"));
    let mut total_files = 0;
    let mut total_bytes = 0;

    // Start recursive traversal from the root inode (Inode 1)
    let root_digest = ingest_recursive(
        path,
        ROOT_INODE,
        blob_store,
        &mut tree,
        &mut total_files,
        &mut total_bytes,
        config,
    )?;

    Ok(IngestResult {
        root_digest,
        tree,
        total_files,
        total_bytes,
    })
}

/// Recursively processes directories, computes file digests, and builds directory manifests.
fn ingest_recursive<B: BlobStore + ?Sized>(
    current_dir: &Path,
    parent_inode: Inode,
    blob_store: &B,
    tree: &mut VfsTree,
    total_files: &mut usize,
    total_bytes: &mut u64,
    config: &ChunkerConfig,
) -> io::Result<Digest> {
    let mut entries = fs::read_dir(current_dir)?
        .filter_map(|res| res.ok())
        .collect::<Vec<_>>();

    // CRITICAL: Sort alphabetically so directory manifests are 100% deterministic.
    entries.sort_by_key(|e| e.file_name());

    let mut dir_manifest_bytes = Vec::new();

    for entry in entries {
        let file_name_os = entry.file_name();
        let name = file_name_os.to_string_lossy();

        // Skip VCS metadata directories
        if name == ".git" || name == ".citc" || name == ".svn" {
            continue;
        }

        let entry_path = entry.path();
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            // Allocate a virtual inode for the child directory
            let child_inode = tree.add_child(
                parent_inode,
                &name,
                VfsEntryType::Directory,
                Digest::from_bytes(b"dir-placeholder"),
                4096,
            );

            // Recursively ingest child directory to obtain its Merkle digest
            let sub_digest = ingest_recursive(
                &entry_path,
                child_inode,
                blob_store,
                tree,
                total_files,
                total_bytes,
                config,
            )?;

            // Record subdirectory in the parent directory manifest:
            // "DIR <name> <digest>\n"
            dir_manifest_bytes.extend_from_slice(b"DIR ");
            dir_manifest_bytes.extend_from_slice(name.as_bytes());
            dir_manifest_bytes.extend_from_slice(b" ");
            dir_manifest_bytes.extend_from_slice(sub_digest.to_string().as_bytes());
            dir_manifest_bytes.extend_from_slice(b"\n");
        } else if metadata.is_file() {
            let is_executable = (metadata.permissions().mode() & 0o111) != 0;
            let file_size = metadata.len();
            let entry = store_file_or_chunked(
                &entry_path,
                &name,
                is_executable,
                blob_store,
                config,
            )?;

            *total_files += 1;
            *total_bytes += file_size;

            match entry {
                ManifestEntry::File { mode, digest, .. } => {
                    tree.add_child(
                        parent_inode,
                        &name,
                        VfsEntryType::RegularFile { is_executable },
                        digest,
                        file_size,
                    );
                    dir_manifest_bytes.extend_from_slice(
                        format!("FILE {} {} {}\n", mode, name, digest).as_bytes(),
                    );
                }
                ManifestEntry::ChunkedFile {
                    mode,
                    manifest_digest,
                    total_size,
                    ..
                } => {
                    tree.add_child(
                        parent_inode,
                        &name,
                        VfsEntryType::ChunkedFile {
                            is_executable,
                            manifest_digest,
                        },
                        manifest_digest,
                        total_size,
                    );
                    dir_manifest_bytes.extend_from_slice(
                        format!(
                            "CHUNKED {} {} {} {}\n",
                            mode, name, manifest_digest, total_size
                        )
                        .as_bytes(),
                    );
                }
                _ => {}
            }
        }
    }

    // Store the directory manifest itself into CAS
    let dir_digest = blob_store.put(&dir_manifest_bytes)?;
    Ok(dir_digest)
}

/// A parsed entry from a raw Git tree object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTreeEntry {
    /// File mode string (e.g., "100644" for normal file, "100755" for executable, "040000" for tree).
    pub mode: String,
    /// Base name of the file or directory.
    pub name: String,
    /// 40-character hex string of the 20-byte Git SHA-1.
    pub sha_hex: String,
}

/// Parses raw Git tree binary structures into structured `GitTreeEntry` items.
///
/// In Git, tree objects are encoded as sequences of:
/// `<mode> <name>\0<20-byte binary sha>`
///
/// # Arguments
/// * `data` - Raw byte payload of a decompressed Git tree object.
///
/// # Returns
/// A vector of parsed `GitTreeEntry` records on success, or an error string if malformed.
pub fn parse_git_tree_entries(data: &[u8]) -> Result<Vec<GitTreeEntry>, String> {
    let mut entries = Vec::new();
    let mut idx = 0;

    while idx < data.len() {
        // Find space separating mode and name
        let space_pos = data[idx..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or("Malformed tree: missing space")?
            + idx;
        let mode = String::from_utf8_lossy(&data[idx..space_pos]).to_string();

        // Find null byte separating name and sha
        let null_pos = data[space_pos + 1..]
            .iter()
            .position(|&b| b == 0)
            .ok_or("Malformed tree: missing null terminator")?
            + space_pos
            + 1;
        let name = String::from_utf8_lossy(&data[space_pos + 1..null_pos]).to_string();

        // Next 20 bytes is standard Git SHA-1
        let sha_end = null_pos + 1 + 20;
        if sha_end > data.len() {
            return Err("Unexpected EOF reading Git SHA".to_string());
        }

        let sha_bytes = &data[null_pos + 1..sha_end];
        let sha_hex = hex::encode(sha_bytes);

        entries.push(GitTreeEntry {
            mode,
            name,
            sha_hex,
        });

        idx = sha_end;
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_ingest_directory() -> io::Result<()> {
        let repo_dir = tempdir()?;
        let cas_dir = tempdir()?;

        // Create sample repo structure
        let src_dir = repo_dir.path().join("src");
        fs::create_dir_all(&src_dir)?;

        let mut f1 = fs::File::create(repo_dir.path().join("README.md"))?;
        write!(f1, "# Test Project")?;

        let mut f2 = fs::File::create(src_dir.join("main.rs"))?;
        write!(f2, "fn main() {{}}")?;

        // Ingest into CAS
        let store = LocalBlobStore::new(cas_dir.path())?;
        let result = ingest_directory(repo_dir.path(), &store)?;

        assert_eq!(result.total_files, 2);
        assert!(store.has(&result.root_digest));

        // Verify lookup in generated VfsTree
        let readme = result
            .tree
            .lookup(ROOT_INODE, "README.md")
            .expect("must find README.md");
        assert_eq!(readme.size_bytes, 14);

        let src = result
            .tree
            .lookup(ROOT_INODE, "src")
            .expect("must find src dir");
        assert_eq!(src.entry_type, VfsEntryType::Directory);

        let main = result
            .tree
            .lookup(src.inode, "main.rs")
            .expect("must find src/main.rs");
        assert_eq!(main.size_bytes, 12);

        Ok(())
    }

    #[test]
    fn test_parse_git_tree_entries() {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"100644 file.txt\0");
        raw.extend_from_slice(&[0x12; 20]); // dummy sha

        let entries = parse_git_tree_entries(&raw).expect("parsing must succeed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file.txt");
        assert_eq!(entries[0].mode, "100644");
        assert_eq!(entries[0].sha_hex, "12".repeat(20));
    }
}
