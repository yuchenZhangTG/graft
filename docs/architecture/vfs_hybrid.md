# Architecture Specification: The Hybrid Virtual Filesystem

This document specifies Graft's hybrid virtual filesystem combining a read-only FUSE lower layer with a native NVMe Copy-on-Write upper layer via Linux OverlayFS.

---

## 1. The Design Paradox: Why Pure FUSE or Pure OverlayFS Fails

| Filesystem Approach | Read Latency | Write Latency | Hydration Cost | Compilation Speed |
| :--- | :--- | :--- | :--- | :--- |
| **Traditional Git Clone** | Fast (Local NVMe) | Fast (Local NVMe) | Extreme ($O(N)$ full checkout) | Fast (Native) |
| **Pure FUSE (e.g. S3FS)** | Slow (Context switch) | Very Slow (Userspace RPC) | Low ($O(1)$ lazy streaming) | Broken (Compilers timeout) |
| **Pure OverlayFS** | Fast (Local NVMe) | Fast (Local NVMe) | High (Requires complete base checkout) | Fast (Native) |
| **Graft Hybrid (FUSE + OverlayFS)** | **Fast (Cached)** | **Fast (Native NVMe)** | **Zero ($O(1)$ instant mount)** | **Fast (100% Native)** |

### Why Pure FUSE Fails for AI Agents
Compilers (`rustc`, `gcc`, `tsc`, `clang`) and language servers (Rust-Analyzer, Pyright) generate massive volumes of tiny temporary files, locks, and pipes. Routing every `write()`, `close()`, and `unlink()` through user-space FUSE kernel transitions creates a 10x–50x performance penalty.

### Why Pure OverlayFS Fails
OverlayFS requires a physical, read-only directory tree on disk as its `lowerdir`. In a 500,000-file repository, checking out that base tree on disk takes minutes and consumes tens of gigabytes before the agent can even begin.

---

## 2. The Hybrid Solution: CitC Lower + Native NVMe Upper

Graft decouples read operations from write operations:

```
┌────────────────────────────────────────────────────────┐
│             Agent Workspace Mount View                 │
│              /tmp/workspaces/agent-1                   │
└───────────────────────────┬────────────────────────────┘
                            │ Linux OverlayFS
            ┌───────────────┴───────────────┐
            ▼                               ▼
 ┌─────────────────────┐         ┌─────────────────────┐
 │    Upper Layer      │         │     Lower Layer     │
 │    (Native NVMe)    │         │  (User-space FUSE)  │
 │  /tmp/agent-1/upper │         │  /tmp/mnt (read-only│
 └──────────┬──────────┘         └──────────┬──────────┘
            │                               │
            │ Modified files only           │ Streams blobs on demand
            ▼                               ▼
 ┌─────────────────────┐         ┌─────────────────────┐
 │ Native ext4 / xfs   │         │ Content-Addressed   │
 │   Kernel Speed      │         │   Storage (CAS)     │
 └─────────────────────┘         └─────────────────────┘
```

1. **Lower Layer (`lowerdir`)**: An in-memory, read-only FUSE mount representing the immutable base commit snapshot. Directory metadata is served from RAM; file contents are fetched lazily from CAS on demand.
2. **Upper Layer (`upperdir`)**: A temporary directory on the agent machine's fast local NVMe drive.
3. **OverlayFS Mount**: Unifies `lowerdir` and `upperdir` into a single POSIX working tree.

---

## 3. In-Memory Merkle Tree & Inode Architecture (`VfsTree`)

POSIX filesystems require every file and directory to possess a 64-bit integer identifier (`Inode`). Graft manages an in-memory virtual inode table:

- **Root Inode**: By standard Unix VFS convention, `ROOT_INODE` is fixed at `1`.
- **Node Allocation**: New child files and subdirectories receive monotonically increasing virtual inodes ($2, 3, 4, \dots$).
- **Child Indexing**: Rather than scanning lists of children ($O(K)$), `VfsTree` organizes children using a sorted map:
  $$\text{dir\_children}: \text{HashMap}\langle \text{Inode}, \text{BTreeMap}\langle\text{String}, \text{Inode}\rangle \rangle$$
  Path lookups within a directory complete in $O(\log K)$ time.
- **Parent Tracking**: Each `VfsNode` stores its `parent_inode`. When `readdir()` is called, Graft emits `.` (current inode) and `..` (actual parent inode), ensuring relative path navigation (`cd ..`) operates correctly throughout nested subdirectories.

### Lazy Directory Expansion (`ensure_dir_loaded`)
On a cold cache, traversing every directory in a monorepo during mount would force downloading thousands of manifests sequentially. Graft expands directories lazily:
- At mount time, only `ROOT_INODE` is created ($O(1)$ startup).
- When an agent or compiler accesses a directory via `lookup()` or `readdir()`, Graft checks `loaded_dirs`. If not yet loaded, Graft fetches the directory's manifest from CAS, populates its direct children in memory, and marks the directory as loaded.
- Unvisited directories are never downloaded.

---

## 4. Zero-Copy `pread64` & Eliminating $O(N^2)$ Read Amplification

When an application reads a file through FUSE, the Linux kernel requests chunks of up to $128\text{ KB}$. For a 10 MB file, the kernel issues ~80 separate `read()` calls.

### The Naive Mistake
Calling `BlobStore::get(&digest)` on every FUSE `read()` request loads and hashes the entire 10 MB file 80 times, reading 800 MB from disk and causing severe CPU throttling.

### Graft's Solution: Direct `FileExt::read_at`
When a blob is stored locally (or hydrated from GCS into the NVMe L1 cache):
1. `blob_store.get_file_path(&digest)` returns the direct on-disk path.
2. Graft opens the file and executes kernel `pread64` (`FileExt::read_at`):
   ```rust
   let file = std::fs::File::open(&path)?;
   let mut buf = vec![0u8; size as usize];
   let n = file.read_at(&mut buf, offset)?;
   buf.truncate(n);
   ```
3. Data is transferred directly into the FUSE kernel buffer with zero heap reallocation and zero re-hashing.
