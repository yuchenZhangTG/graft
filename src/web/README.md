# Graft Web Subsystem (`src/web/`)

Embedded high-performance Web server, code search engine, repository tree browser, split/unified diff inspector, pull request review platform, and active AI agent workspace monitor.

## Files

- [`web.rs`](../web.rs): Module declarations, URL parsing, request routing (`route_request_full`), and embedded async Tokio TCP HTTP server (`start_web_server`).
- [`search.rs`](search.rs): Multi-tier code search engine featuring **Level-1 Direct CAS Merkle Streaming** and **Level-2 Trigram Inverted Index** (`TrigramIndex`) with packed 24-bit lowercase ASCII trigrams, monotonic `BlobId` interning, two-pointer sorted posting list intersection, Merkle directory caching (`indexed_dirs`), quantifier-aware literal regex extraction, and path filtering.
- [`handlers.rs`](handlers.rs): HTTP route handlers for tree directory browsing (`handle_tree`), blob file inspection (`handle_blob`), git blame tracking (`handle_blame`), commit history logs (`handle_commits`), split/unified commit diffs (`handle_diff`), workspace inspection (`handle_workspaces`), pull requests (`handle_pulls`, `handle_pull_detail`), and code search (`handle_search`).
- [`templates.rs`](templates.rs): GitHub Primer-inspired dark-mode HTML/CSS/JS components, interactive Search Console with engine selection and active index metrics, breadcrumb navigation, commit tease bars, blame tables, diff viewers, and client-side Mermaid diagram rendering.
- [`markdown.rs`](markdown.rs): GFM-compliant markdown parser with heading slug generation, relative URL resolution, GitHub callout alerts (`[!NOTE]`, `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]`, `[!CAUTION]`), task lists, tables, and Mermaid block extraction.
