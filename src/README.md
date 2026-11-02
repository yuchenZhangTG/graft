# Graft Source Code (`src/`)

Root source directory implementing the Graft virtual version control engine and CLI.

## Subdirectories

- [`cas/`](cas/README.md): Content-Addressed Storage engine, hashing, blob stores (Local NVMe + GCS), caching, Merkle commits, and 3-way Merkle merging.
- [`git/`](git/README.md): Git interoperability, repository ingestion, unified diff computation, and patch export.
- [`metadata/`](metadata/README.md): ACID metadata stores for branch references and commit indices (Local atomic files and GCP Cloud Spanner).
- [`vfs/`](vfs/README.md): User-space FUSE virtual filesystem presenting CAS Merkle trees as read-only POSIX directory trees.
- [`web/`](web/README.md): Embedded high-performance Web server, Level-1/Level-2 code search engine, repository tree browser, and pull request review platform.
- [`workspace/`](workspace/README.md): Copy-on-Write workspace coordinator managing OverlayFS mounts and upperdir modification scanning.

## Files

- [`lib.rs`](lib.rs): Core library root exposing public API modules (`cas`, `git`, `metadata`, `vfs`, `workspace`, `review`, `web`).
- [`main.rs`](main.rs): Complete CLI application providing subcommands: `ingest`, `workspace`, `commit`, `diff`, `log`, `export`, `merge`, `branch`, `lease`, `cas`, and `web`.
- [`cas.rs`](cas.rs): Entry point and module re-exports for Content-Addressed Storage.
- [`git.rs`](git.rs): Entry point and module re-exports for Git interoperability.
- [`metadata.rs`](metadata.rs): Entry point, lease types, and `MetadataStore` trait for metadata management.
- [`review.rs`](review.rs): Pull request and code review data model, inline line comment tracking, and automated merge checks.
- [`vfs.rs`](vfs.rs): Entry point and types for the virtual filesystem layer.
- [`web.rs`](web.rs): Entry point, URL routing, and async server runtime for the Web UI and REST API.
- [`workspace.rs`](workspace.rs): Entry point and implementation for Copy-on-Write workspace management.
