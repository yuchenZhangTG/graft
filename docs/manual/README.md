# Graft User Manual (`docs/manual/`)

Comprehensive user manuals, step-by-step tutorials, operational workflows, and complete command-line reference guides for Graft.

## Manual Contents

- [`quickstart.md`](quickstart.md): 5-minute hands-on walkthrough: building, ingesting, creating workspaces, committing in $O(\text{modified})$ time, FUSE mounting, and patch exporting.
- [`cli_reference.md`](cli_reference.md): Complete reference guide for every command, option, flag, environment variable, and exit status (`workspace`, `commit`, `branch`, `merge`, `mount`, `unmount`, `diff`, `log`, `export`, `lease`, `cas`, `ingest`).
- [`workflows.md`](workflows.md): Production operational patterns:
  - Multi-agent parallel branching and 3-way Merkle merging.
  - Multi-VM distributed storage with Google Cloud Storage (`gs://...`).
  - ACID serializable metadata and lease locking with Google Cloud Spanner.
  - Virtual repository inspection via interactive FUSE mounting.
