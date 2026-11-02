# Protocol Buffers (`proto/`)

Contains protocol definitions for Content-Addressed Storage (CAS) and remote execution compatibility.

## Files

- [`cas.proto`](cas.proto): Defines Protobuf schemas for content-addressed primitives:
  - `Digest`: SHA-256 hash and size payload identifier.
  - `DirectoryEntry` / `Directory`: Merkle tree hierarchy and directory manifests.
  - `FileBlob`: File payload metadata and execution bits.
  - `CommitNode`: Version control commit graph nodes with parent digests and timestamps.
  - `CASService`: gRPC service interface for Remote Execution (RE) and distributed chunk retrieval.
