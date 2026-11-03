# Graft Architecture Specifications (`docs/architecture/`)

This directory contains in-depth architectural specifications and system designs for Graft.

## Documents

- [`overview.md`](overview.md): System architecture, end-to-end component topology, subsystem partition, and agent lifecycle sequence diagram.
- [`storage_model.md`](storage_model.md): Content-Addressed Storage data structures, BLAKE3-256 cryptographic digests, FastCDC chunking, two-tier NVMe L1 cache + Google Cloud Storage L2 replication.
- [`vfs_hybrid.md`](vfs_hybrid.md): The hybrid virtual filesystem (FUSE lower layer + native NVMe Copy-on-Write upper layer), in-memory `VfsTree` inode indexing, lazy directory loading, and zero-copy `pread64` reads.
- [`merge_engine.md`](merge_engine.md): Server-side 3-way Merkle merge engine, lowest common ancestor discovery, $O(1)$ subtree pruning, LCS diff3 hunk reconciliation, and atomic Compare-And-Swap branch coordination via Google Cloud Spanner.
- [`graftd_design.md`](graftd_design.md): Server-centric monorepo daemon architecture, gRPC/HTTP2 protocol specifications (`CasService`, `RefService`, `MergeService`), fleet-wide zero-copy deduplication, and zero-download server-side merges for autonomous AI agents.
