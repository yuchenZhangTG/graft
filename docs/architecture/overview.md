# Graft Architecture Specification: System Overview

## 1. System Overview

Graft is a distributed, content-addressed source control and virtual workspace system designed for high-concurrency automated agent workflows and large monorepos.

```
                          ┌──────────────────────────┐
                          │   Upstream Git Remote    │
                          │   (GitHub / GitLab / etc)│
                          └─────────────┬────────────┘
                                        │ (Sync commits)
                                        ▼
                          ┌──────────────────────────┐
                          │       Graft Server       │
                          │  - Object CAS Store      │
                          │  - Tree & Commit Index   │
                          │  - Server-Side Merge/Diff│
                          └─────────────┬────────────┘
                                        │ gRPC Streaming / GCS
                                        ▼
                          ┌──────────────────────────┐
                          │    Local Graft Daemon    │
                          │  - In-Memory VfsTree     │
                          │  - NVMe Chunk Cache (L1) │
                          └─────────────┬────────────┘
                                        │
                 ┌──────────────────────┴──────────────────────┐
                 ▼                                             ▼
  ┌─────────────────────────────┐               ┌─────────────────────────────┐
  │   FUSE Mount (lowerdir)     │               │   FUSE Mount (lowerdir)     │
  │   /var/run/graft/trees/sha1 │               │   /var/run/graft/trees/sha2 │
  └──────────────┬──────────────┘               └──────────────┬──────────────┘
                 │                                             │
                 ▼ (OverlayFS)                                 ▼ (OverlayFS)
  ┌─────────────────────────────┐               ┌─────────────────────────────┐
  │ Agent Workspace #1 (Merged) │               │ Agent Workspace #2 (Merged) │
  │ Upper: /tmp/agent-1/upper   │               │ Upper: /tmp/agent-2/upper   │
  │ Work:  /tmp/agent-1/work    │               │ Work:  /tmp/agent-2/work    │
  └─────────────────────────────┘               └─────────────────────────────┘
```

---

## 2. Core Subsystems

Graft is partitioned into four decoupled subsystems:

1. **Content-Addressed Storage (`src/cas/`)**:
   - Cryptographic immutability via **BLAKE3-256**.
   - Two-tier storage hierarchy: Local NVMe SSD (L1) + Google Cloud Storage (L2).
   - Fast Content-Defined Chunking (**FastCDC**) for deduplicated large assets.
   - Deterministic Merkle directory manifests with preserved file sizes and POSIX execution bits.

2. **Virtual Filesystem Engine (`src/vfs/`)**:
   - In-memory virtual inode hierarchy (`VfsTree`) with $O(1)$ Inode lookups and $O(\log K)$ child resolution via `BTreeMap`.
   - Lazy per-directory manifest expansion (`ensure_dir_loaded`) ensuring $O(1)$ instant mounts regardless of monorepo size.
   - User-space FUSE driver (`GraftFilesystem`) using zero-copy `pread64` on locally cached files to eliminate $O(N^2)$ chunk amplification.
   - Signal-safe daemon orchestration (`spawn_mount_fuse`) and graceful unmounting.

3. **Workspace Orchestration (`src/workspace/`)**:
   - Ephemeral Copy-on-Write (CoW) workspaces combining FUSE `lowerdir` with native NVMe `upperdir` via Linux OverlayFS.
   - Sub-second workspace creation ($< 50\text{ ms}$).
   - $O(\text{modified})$ upperdir delta scanning and instant commit creation.

4. **Distributed Metadata & Concurrency (`src/metadata/`)**:
   - Linearizable atomic Compare-And-Swap (CAS) on branch references.
   - Direct Google Cloud Spanner KeySet CRUD integration (`/v1/{session}:read` and `/v1/{session}:commit`), bypassing SQL compilation.
   - Distributed workspace lease locking with TTL heartbeats to prevent agent contention.

---

## 3. Agent Lifecycle Sequence

```mermaid
sequenceDiagram
    participant Orchestrator as Agent Orchestrator
    participant Graft as Graft CLI / Daemon
    participant FUSE as FUSE Driver
    participant Kernel as Linux OverlayFS
    participant CAS as Blob Store (L1/L2)
    participant Spanner as Spanner Metadata

    Orchestrator->>Graft: workspace create --branch feature-1 --id task-1
    Graft->>Spanner: Acquire workspace lease (task-1, TTL 300s)
    Graft->>FUSE: Ensure tree C1 is mounted (lowerdir)
    Graft->>Kernel: Mount OverlayFS (lower=C1, upper=/tmp/task-1/upper)
    Graft-->>Orchestrator: Ready at /workspaces/task-1 (< 50ms)

    Orchestrator->>Kernel: Read src/main.rs
    Kernel->>FUSE: read(src/main.rs)
    FUSE->>CAS: Get blob (L1 cache hit or L2 GCS stream)
    CAS-->>Kernel: Return bytes
    Note over Kernel: Unopened files take 0 bytes disk & network

    Orchestrator->>Kernel: Edit src/main.rs & run compiler
    Note over Kernel: Kernel copies to upperdir (Copy-on-Write)
    Kernel-->>Orchestrator: Write succeeded at native NVMe speed

    Orchestrator->>Graft: commit -m "Implement feature" --workspace task-1
    Note over Graft: Scan only /tmp/task-1/upper (O(modified))
    Graft->>CAS: Store new blobs & compute new Merkle root
    Graft->>Spanner: Advance branch refs/heads/feature-1 via atomic CAS
    Graft-->>Orchestrator: Commit SHA created & branch advanced

    Orchestrator->>Graft: workspace destroy --id task-1
    Graft->>Kernel: Unmount OverlayFS
    Graft->>Spanner: Release workspace lease
    Graft->>Graft: rm -rf /tmp/task-1
    Graft-->>Orchestrator: Workspace removed
```

---

## 4. Architectural Deep-Dives

For detailed subsystem designs, refer to:
- [Storage Model & CAS Data Structures](storage_model.md)
- [Hybrid Virtual Filesystem (FUSE + OverlayFS)](vfs_hybrid.md)
- [Server-Side 3-Way Merkle Merge Engine](merge_engine.md)
