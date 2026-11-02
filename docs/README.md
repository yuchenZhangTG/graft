# Graft Documentation (`docs/`)

Welcome to the Graft documentation repository. Documentation is organized into two primary sections:

---

## 1. System Architecture Specifications (`docs/architecture/`)

In-depth technical whitepapers, data models, algorithm specifications, and internal design documents for developers and contributors:

- [`docs/architecture/overview.md`](architecture/overview.md): System overview, topology, subsystem partition, and agent lifecycle sequence diagram.
- [`docs/architecture/storage_model.md`](architecture/storage_model.md): Content-Addressed Storage, BLAKE3-256 digests, FastCDC chunking, two-tier NVMe L1 cache + GCS L2 replication.
- [`docs/architecture/vfs_hybrid.md`](architecture/vfs_hybrid.md): The hybrid virtual filesystem (FUSE lower layer + native OverlayFS upper layer), in-memory `VfsTree`, and zero-copy `pread64` reads.
- [`docs/architecture/merge_engine.md`](architecture/merge_engine.md): Server-side 3-way Merkle merge engine, lowest common ancestor discovery, $O(1)$ subtree pruning, LCS diff3 hunk reconciliation, and Cloud Spanner branch Compare-And-Swap.

For more details, see the [Architecture Index](architecture/README.md).

---

## 2. User Manual & Operational Guides (`docs/manual/`)

Practical operational guides, end-to-end tutorials, and complete command-line interface references for developers and agent operators:

- [`docs/manual/quickstart.md`](manual/quickstart.md): 5-minute hands-on walkthrough from installation to committing and mounting.
- [`docs/manual/cli_reference.md`](manual/cli_reference.md): Complete command-by-command reference of all subcommands, arguments, and flags.
- [`docs/manual/workflows.md`](manual/workflows.md): Step-by-step recipes for multi-agent branching, distributed GCS fleets, Spanner ACID leases, and FUSE mounting.

For more details, see the [User Manual Index](manual/README.md).
