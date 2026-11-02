# Content-Addressed Storage (`src/cas/`)

Implements the immutable Merkle tree storage layer, cryptographic hashing, local and remote blob persistence, and server-side 3-way Merkle merging.

## Files

- [`cas.rs`](../cas.rs): Module declarations and re-exports for the CAS subsystem.
- [`digest.rs`](digest.rs): Cryptographic content addressing (`Digest`) with BLAKE3-256 calculation, hex formatting, and `<hash>:<size_bytes>` parsing.
- [`blob_store.rs`](blob_store.rs): Abstract `BlobStore` trait for immutable chunk storage, content deduplication, and local NVMe filesystem implementation (`LocalBlobStore`).
- [`gcs.rs`](gcs.rs): Remote Google Cloud Storage backend (`GcsBlobStore`) using HTTP REST with Bearer token authentication, 45-minute token caching, generation-match concurrency control, and hermetic offline mock transport (`MockGcsTransport`).
- [`cached_store.rs`](cached_store.rs): Two-tier storage engine (`CachedBlobStore`: L1 local NVMe + L2 remote GCS) providing transparent on-demand lazy hydration via `get_file_path`.
- [`commit.rs`](commit.rs): Version control commit object model (`Commit`), canonical plain-text directory manifests supporting both `FILE` and `CHUNKED` entries, commit ancestry traversal (`get_commit_history`), and tree-patch application (`apply_changes_to_tree`).
- [`chunker.rs`](chunker.rs): FastCDC v2020 content-defined chunker (`ChunkManifest`, `ChunkEntry`, `ChunkerConfig`), sub-range binary-search slicing (`read_chunked_range`), zero-copy streaming (`stream_chunked_file`), and shift-resistant deduplication across large file edits.
- [`gc.rs`](gc.rs): Mark-and-sweep garbage collection engine (`run_gc`, `mark_reachable`, `collect_roots`, `GcOptions`, `GcStats`) with fail-closed integrity checks, four-source live root discovery, typed Merkle DAG traversal, and configurable grace period protection.
- [`merge.rs`](merge.rs): Server-side 3-way Merkle tree merger (`merge_trees`) in $O(\text{changed})$ time, LCS diff3 hunk-based line-level conflict resolution (`merge3_text`, `merge3_text_with_strategy`), chunked file conflict tracking, common ancestor discovery (`find_common_ancestor`), commit cherry-picking (`cherry_pick_commit`), conflict resolution strategies (`ConflictStrategy`), recursive Merkle delta extraction (`extract_tree_delta`), and workspace upper-layer delta application (`apply_tree_delta_to_workspace`).
