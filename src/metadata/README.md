# Metadata Management (`src/metadata/`)

Provides linearizable ACID metadata storage for branch references, commit indexing, and distributed agent workspace leases.

## Key Capabilities

- **Linearizable Reference Management**: Compare-and-Swap (CAS) branch reference updates (`compare_and_swap_ref`, `get_ref`, `list_refs`, `delete_ref`).
- **Canonical Ref Normalization**: Maps shorthand branch names (e.g. `feat-auth` $\to$ `refs/heads/feat-auth`) with backward-compatible bare `main` fallback.
- **Distributed Workspace Leases**: Prevents concurrent agents from colliding on task branches with TTL heartbeats and fencing tokens.

## Files

- [`metadata.rs`](../metadata.rs): Abstract `MetadataStore` trait, reference normalization (`normalize_ref_name`), branch name validator (`validate_branch_name`), reference resolver (`resolve_ref`), distributed lease types (`WorkspaceLease`), and RFC 3339 timestamp helpers.
- [`local.rs`](local.rs): Local file-backed metadata store (`LocalMetadataStore`) utilizing atomic renames for lock-free branch updates and atomic lease file locks on single workstations.
- [`spanner.rs`](spanner.rs): Production Cloud Spanner distributed metadata store (`SpannerMetadataStore`, `GcpCloudSpannerClient`, `MockSpannerClient`) implementing sub-millisecond point reads (`/v1/{session}:read`), TrueTime serializable Read-Write transactions, and atomic CAS mutations (`/v1/{session}:commit`) bypassing SQL engine overhead.
