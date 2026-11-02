# Git Interoperability (`src/git/`)

Handles repository ingestion, diff computation, and standard patch/snapshot export.

## Files

- [`git.rs`](../git.rs): Module declarations and public API re-exports for the Git interoperability layer.
- [`ingest.rs`](ingest.rs): Fast recursive directory crawler that hashes local file trees into CAS blobs and Merkle tree manifests without requiring a full `.git` history checkout.
- [`diff.rs`](diff.rs): Unified diff generator (`diff_trees`, `diff_workspace`, `generate_unified_diff`) computing line-level diffs directly from Merkle trees and upperdir modifications.
- [`export.rs`](export.rs): Exporter that writes diffs to standard `.patch` files (`export_patch_file`) and materializes Merkle trees into clean directories (`export_tree_to_dir`).
