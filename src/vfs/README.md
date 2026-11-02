# Virtual Filesystem (`src/vfs/`)

Implements the user-space virtual filesystem (VFS) and FUSE daemon that projects immutable Content-Addressed Merkle trees into standard POSIX directory trees with CitC-style lazy hydration.

## Files

- [`vfs.rs`](../vfs.rs): In-memory metadata model (`VfsTree`, `VfsNode`) providing $O(1)$ Inode lookup, $O(\log K)$ directory child resolution via `BTreeMap`, parent Inode tracking for `..`, and lazy on-demand `DirectoryManifest` loading (`ensure_dir_loaded`).
- [`fuse_server.rs`](fuse_server.rs): User-space FUSE server (`GraftFilesystem`) implementing `fuser::Filesystem` with zero-copy `pread64` local cache reads to eliminate $O(N^2)$ chunk-read amplification, 1-hour kernel attribute caching (`TTL`), deterministic commit timestamps, and background mount daemon orchestration (`spawn_mount_fuse`).
- [`workspace_fs.rs`](workspace_fs.rs): Writeable Copy-on-Write (CoW) FUSE filesystem (`GraftWorkspaceFilesystem`) projecting a hybrid view of lower read-only CAS snapshots and a mutable `upper_dir`. Features atomic copy-up on write/truncate, whiteout tombstone management (`.wh.<name>`), opaque directory markers (`.wh..wh..opq`), stable bidirectional `path <-> Inode` mappings, positional thread-safe I/O via `read_at`/`write_at`, and unprivileged background mounting (`spawn_mount_workspace_fuse`).
