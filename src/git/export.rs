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

//! # Git Export & Tree Materialization Engine
//!
//! This module bridges Graft with standard Git repositories, CI environments, and developer filesystems.
//!
//! ### Capabilities:
//! - **Patch Export**: Writes unified diffs into `.patch` files ready for `git apply`, `git am`, or Critique.
//! - **Tree Materialization**: Exports a complete, immutable Merkle snapshot from CAS into a target
//!   directory (e.g., standard Git worktree, release directory, or container volume).

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::{DirectoryManifest, ManifestEntry};
use crate::cas::digest::Digest;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Exports a unified diff string to a patch file on disk.
///
/// The resulting file can be applied directly to any Git repository using:
/// `git apply <patch_file>` or `patch -p1 < <patch_file>`.
///
/// # Arguments
/// * `diff_text` - The unified diff text to write.
/// * `output_path` - The file path where the patch will be saved.
///
/// # Errors
/// Returns an `io::Error` if the output file cannot be created or written.
///
/// # Examples
/// ```no_run
/// use graft::git::export::export_patch_file;
/// use std::path::Path;
///
/// let patch = "diff --git a/README.md b/README.md\n...";
/// export_patch_file(patch, Path::new("/tmp/agent.patch")).unwrap();
/// ```
pub fn export_patch_file<P: AsRef<Path>>(diff_text: &str, output_path: P) -> io::Result<()> {
    let path = output_path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, diff_text)?;
    Ok(())
}

/// Recursively materializes an entire repository Merkle tree from CAS into a destination directory.
///
/// All files and subdirectories defined by `root_digest` are written to `target_dir` with
/// their canonical POSIX permissions restored (`0755` for executables, `0644` for regular files).
///
/// # Arguments
/// * `root_digest` - The root Merkle Directory digest of the snapshot to export.
/// * `target_dir` - The destination directory on the local filesystem.
/// * `blob_store` - The backing CAS store holding directory manifests and file contents.
///
/// # Returns
/// The total number of files materialized.
///
/// # Errors
/// Returns an `io::Error` if any directory manifest cannot be parsed, if blobs are missing from CAS,
/// or if filesystem writes fail.
pub fn export_tree_to_dir<P: AsRef<Path>, B: BlobStore + ?Sized>(
    root_digest: &Digest,
    target_dir: P,
    blob_store: &B,
) -> io::Result<usize> {
    let dest = target_dir.as_ref();
    fs::create_dir_all(dest)?;

    let mut files_written = 0;
    materialize_directory_recursive(root_digest, dest, blob_store, &mut files_written)?;
    Ok(files_written)
}

fn materialize_directory_recursive<B: BlobStore + ?Sized>(
    dir_digest: &Digest,
    current_dest: &Path,
    blob_store: &B,
    files_written: &mut usize,
) -> io::Result<()> {
    let manifest_bytes = blob_store
        .get(dir_digest)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Directory manifest {} not found in CAS", dir_digest)))?;

    let manifest = DirectoryManifest::from_bytes(&manifest_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    for entry in manifest.entries.values() {
        match entry {
            ManifestEntry::File { mode, name, digest } => {
                let file_path = current_dest.join(name);
                let content = blob_store
                    .get(digest)?
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Blob {} for '{}' not found in CAS", digest, name)))?;

                fs::write(&file_path, &content)?;

                // Restore POSIX permissions
                let perm_bits = if mode == "0755" { 0o755 } else { 0o644 };
                fs::set_permissions(&file_path, fs::Permissions::from_mode(perm_bits))?;

                *files_written += 1;
            }
            ManifestEntry::ChunkedFile {
                mode,
                name,
                manifest_digest,
                ..
            } => {
                let file_path = current_dest.join(name);
                let manifest_bytes = blob_store.get(manifest_digest)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "Chunk manifest {} for '{}' not found in CAS",
                            manifest_digest, name
                        ),
                    )
                })?;
                let chunk_manifest =
                    crate::cas::chunker::ChunkManifest::from_bytes(&manifest_bytes)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

                let file = fs::File::create(&file_path)?;
                let mut writer = io::BufWriter::new(file);
                crate::cas::chunker::stream_chunked_file(
                    &chunk_manifest,
                    blob_store,
                    &mut writer,
                )?;

                let perm_bits = if mode == "0755" { 0o755 } else { 0o644 };
                fs::set_permissions(&file_path, fs::Permissions::from_mode(perm_bits))?;

                *files_written += 1;
            }
            ManifestEntry::Directory { name, digest } => {
                let sub_dir = current_dest.join(name);
                fs::create_dir_all(&sub_dir)?;
                materialize_directory_recursive(digest, &sub_dir, blob_store, files_written)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use tempfile::tempdir;

    #[test]
    fn test_export_patch_file() -> io::Result<()> {
        let tmp = tempdir()?;
        let patch_path = tmp.path().join("changes.patch");
        let content = "diff --git a/test.txt b/test.txt\n+new line\n";

        export_patch_file(content, &patch_path)?;
        let read_back = fs::read_to_string(&patch_path)?;
        assert_eq!(read_back, content);
        Ok(())
    }

    #[test]
    fn test_export_tree_to_dir() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path().join("cas"))?;

        // Construct Merkle tree in CAS:
        // root/
        //   README.md (0644)
        //   bin/
        //     tool.sh (0755)
        let readme_digest = store.put(b"# Hello Graft")?;
        let tool_digest = store.put(b"#!/bin/bash\necho ok")?;

        let mut bin_manifest = DirectoryManifest::default();
        bin_manifest.entries.insert(
            "tool.sh".to_string(),
            ManifestEntry::File {
                mode: "0755".to_string(),
                name: "tool.sh".to_string(),
                digest: tool_digest,
            },
        );
        let bin_digest = store.put(&bin_manifest.serialize())?;

        let mut root_manifest = DirectoryManifest::default();
        root_manifest.entries.insert(
            "README.md".to_string(),
            ManifestEntry::File {
                mode: "0644".to_string(),
                name: "README.md".to_string(),
                digest: readme_digest,
            },
        );
        root_manifest.entries.insert(
            "bin".to_string(),
            ManifestEntry::Directory {
                name: "bin".to_string(),
                digest: bin_digest,
            },
        );
        let root_digest = store.put(&root_manifest.serialize())?;

        // Export tree to target directory
        let export_dir = tmp.path().join("exported");
        let count = export_tree_to_dir(&root_digest, &export_dir, &store)?;

        assert_eq!(count, 2);
        assert_eq!(fs::read_to_string(export_dir.join("README.md"))?, "# Hello Graft");
        assert_eq!(fs::read_to_string(export_dir.join("bin").join("tool.sh"))?, "#!/bin/bash\necho ok");

        // Verify executable permission
        let tool_meta = fs::metadata(export_dir.join("bin").join("tool.sh"))?;
        assert_eq!(tool_meta.permissions().mode() & 0o111, 0o111);

        Ok(())
    }
}
