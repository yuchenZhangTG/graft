// Copyright 2026 The Graft Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Fast Code Search Engine across Merkle Trees in Content-Addressed Storage.
//!
//! Provides two complementary search levels:
//! - **Level 1 (Direct CAS Streaming)**: Direct linear Merkle tree scan without pre-indexing.
//! - **Level 2 (Trigram Inverted Index)**: Google Code Search-style N-gram inverted index
//!   with two-pointer postings intersection, Merkle directory caching, sub-millisecond
//!   candidate pruning, and regex literal acceleration.

use crate::cas::blob_store::BlobStore;
use crate::cas::chunker::{stream_chunked_file, ChunkManifest};
use crate::cas::commit::{DirectoryManifest, ManifestEntry};
use crate::cas::digest::Digest;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{OnceLock, RwLock};
use std::time::Instant;

/// Maximum file size to scan for text matches (2 MB).
pub const MAX_SEARCH_FILE_SIZE: usize = 2 * 1024 * 1024;

/// Packed 3-byte lowercase ASCII trigram representation.
pub type Trigram = u32;

/// Monotonic compact identifier for indexed file blobs.
pub type BlobId = u32;

/// Constructs a packed 24-bit lowercase ASCII trigram.
#[inline]
pub fn make_trigram(b0: u8, b1: u8, b2: u8) -> Trigram {
    ((b0.to_ascii_lowercase() as u32) << 16)
        | ((b1.to_ascii_lowercase() as u32) << 8)
        | (b2.to_ascii_lowercase() as u32)
}

/// Extracts unique, sorted lowercase ASCII trigrams from a byte slice.
pub fn extract_trigrams(bytes: &[u8]) -> Vec<Trigram> {
    if bytes.len() < 3 {
        return Vec::new();
    }
    let mut trigrams = Vec::with_capacity(bytes.len() - 2);
    for w in bytes.windows(3) {
        trigrams.push(make_trigram(w[0], w[1], w[2]));
    }
    trigrams.sort_unstable();
    trigrams.dedup();
    trigrams
}

/// Extracts guaranteed literal trigrams from a regular expression pattern.
/// Returns an empty list if no guaranteed literal segment of length >= 3 exists.
pub fn extract_literal_trigrams_from_regex(re_str: &str) -> Vec<Trigram> {
    if re_str.contains('|') {
        return Vec::new();
    }

    let mut trigrams = Vec::new();
    let bytes = re_str.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        let start = i;
        while i < len {
            let b = bytes[i];
            if matches!(
                b,
                b'\\' | b'.' | b'+' | b'*' | b'?' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'^' | b'$'
            ) {
                break;
            }
            if i + 1 < len && (bytes[i + 1] == b'*' || bytes[i + 1] == b'?' || bytes[i + 1] == b'{') {
                break;
            }
            i += 1;
        }

        let segment = &bytes[start..i];
        if segment.len() >= 3 {
            for w in segment.windows(3) {
                trigrams.push(make_trigram(w[0], w[1], w[2]));
            }
        }

        if i < len {
            if bytes[i] == b'\\' && i + 1 < len {
                i += 2;
            } else if bytes[i] == b'{' {
                while i < len && bytes[i] != b'}' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
    }

    trigrams.sort_unstable();
    trigrams.dedup();
    trigrams
}

/// Extracts search trigrams from either a literal or regex query.
pub fn extract_trigrams_from_query(query: &str, is_regex: bool) -> Vec<Trigram> {
    if is_regex {
        extract_literal_trigrams_from_regex(query)
    } else {
        extract_trigrams(query.as_bytes())
    }
}

/// Linear-time two-pointer intersection of two sorted slices of `BlobId`.
#[inline]
pub fn intersect_sorted(a: &[BlobId], b: &[BlobId]) -> Vec<BlobId> {
    let mut res = Vec::with_capacity(a.len().min(b.len()));
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            res.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    res
}

/// A matched snippet line with context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSnippet {
    /// 1-based line number.
    pub line_number: usize,
    /// Content of the line.
    pub content: String,
    /// True if this line matched the query.
    pub is_match: bool,
}

/// Matches within a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchFileResult {
    /// Repository-relative path to the file.
    pub path: String,
    /// CAS digest of the file.
    pub digest: String,
    /// Total number of matches in this file.
    pub match_count: usize,
    /// Context snippets around matching lines.
    pub snippets: Vec<SearchSnippet>,
}

/// Aggregate search results for a repository snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    /// The search query term.
    pub query: String,
    /// Commit or tree digest searched.
    pub commit: String,
    /// Total number of matching lines found across all files.
    pub total_matches: usize,
    /// Total number of files scanned.
    pub files_searched: usize,
    /// Search results grouped by file.
    pub results: Vec<SearchFileResult>,
    /// Search engine used ("level2" or "level1").
    #[serde(default)]
    pub engine: String,
    /// Search execution duration in milliseconds.
    #[serde(default)]
    pub duration_ms: f64,
    /// Number of files pruned by index before scanning.
    #[serde(default)]
    pub files_pruned: usize,
    /// Whether query was treated as regular expression.
    #[serde(default)]
    pub is_regex: bool,
    /// Whether search was case-sensitive.
    #[serde(default)]
    pub is_case_sensitive: bool,
    /// Active path filter, if any.
    #[serde(default)]
    pub path_filter: String,
}

/// Metrics and metadata for the Trigram Inverted Index.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    pub total_indexed_files: usize,
    pub total_indexed_bytes: u64,
    pub total_unique_trigrams: usize,
    pub total_directories_indexed: usize,
}

/// Level-2 Trigram Inverted Index over Content-Addressed Storage blobs.
#[derive(Default)]
pub struct TrigramIndex {
    /// Maps 32-byte Blake3 raw hash to internal monotonic BlobId.
    hash_to_id: HashMap<[u8; 32], BlobId>,
    /// Inverse map from BlobId to raw hash.
    id_to_hash: Vec<[u8; 32]>,
    /// Trigram -> sorted list of BlobIds containing this trigram.
    postings: HashMap<Trigram, Vec<BlobId>>,
    /// Set of directory manifests already indexed (Merkle caching).
    indexed_dirs: HashSet<[u8; 32]>,
    /// Total bytes of indexed file blobs.
    total_bytes: u64,
}

impl TrigramIndex {
    /// Creates a new empty Trigram Index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns current indexing metrics.
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            total_indexed_files: self.id_to_hash.len(),
            total_indexed_bytes: self.total_bytes,
            total_unique_trigrams: self.postings.len(),
            total_directories_indexed: self.indexed_dirs.len(),
        }
    }

    /// Checks if a root or subtree directory is already cached in the index.
    pub fn is_tree_indexed(&self, root_digest: &Digest) -> bool {
        self.indexed_dirs.contains(&root_digest.hash)
    }

    /// Recursively indexes all file blobs in the Merkle tree.
    /// Employs Merkle caching: subdirectories already present in `indexed_dirs` are skipped.
    pub fn index_tree<B: BlobStore + ?Sized>(
        &mut self,
        root_digest: &Digest,
        store: &B,
    ) -> io::Result<usize> {
        let mut dir_queue = vec![*root_digest];
        let mut newly_indexed = 0;

        while let Some(dir_digest) = dir_queue.pop() {
            let dir_hash = dir_digest.hash;
            if self.indexed_dirs.contains(&dir_hash) {
                continue;
            }

            let dir_bytes = match store.get(&dir_digest)? {
                Some(b) => b,
                None => continue,
            };

            let manifest = match DirectoryManifest::from_bytes(&dir_bytes) {
                Ok(m) => m,
                Err(_) => continue,
            };

            for (_name, entry) in manifest.entries {
                match &entry {
                    ManifestEntry::Directory { digest, .. } => {
                        if !self.indexed_dirs.contains(&digest.hash) {
                            dir_queue.push(*digest);
                        }
                    }
                    ManifestEntry::File { digest, .. } => {
                        let file_hash = digest.hash;
                        if !self.hash_to_id.contains_key(&file_hash) {
                            if let Ok(bytes) = read_entry_bytes(&entry, store, MAX_SEARCH_FILE_SIZE) {
                                let is_bin = is_binary_content(&bytes);
                                let blob_id = self.id_to_hash.len() as BlobId;
                                self.hash_to_id.insert(file_hash, blob_id);
                                self.id_to_hash.push(file_hash);
                                self.total_bytes += bytes.len() as u64;
                                newly_indexed += 1;

                                if !is_bin {
                                    let trigrams = extract_trigrams(&bytes);
                                    for tri in trigrams {
                                        self.postings.entry(tri).or_default().push(blob_id);
                                    }
                                }
                            }
                        }
                    }
                    ManifestEntry::ChunkedFile { manifest_digest, .. } => {
                        let file_hash = manifest_digest.hash;
                        if !self.hash_to_id.contains_key(&file_hash) {
                            if let Ok(bytes) = read_entry_bytes(&entry, store, MAX_SEARCH_FILE_SIZE) {
                                let is_bin = is_binary_content(&bytes);
                                let blob_id = self.id_to_hash.len() as BlobId;
                                self.hash_to_id.insert(file_hash, blob_id);
                                self.id_to_hash.push(file_hash);
                                self.total_bytes += bytes.len() as u64;
                                newly_indexed += 1;

                                if !is_bin {
                                    let trigrams = extract_trigrams(&bytes);
                                    for tri in trigrams {
                                        self.postings.entry(tri).or_default().push(blob_id);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            self.indexed_dirs.insert(dir_hash);
        }

        Ok(newly_indexed)
    }

    /// Finds candidate BlobIds containing all trigrams extracted from the query.
    /// Returns None if no trigrams could be extracted (i.e. all files are candidates).
    /// Returns Some(vec) containing the intersection of sorted posting lists.
    pub fn find_candidate_blob_ids(&self, trigrams: &[Trigram]) -> Option<Vec<BlobId>> {
        if trigrams.is_empty() || self.id_to_hash.is_empty() {
            return None;
        }

        for tri in trigrams {
            if !self.postings.contains_key(tri) {
                return Some(Vec::new());
            }
        }

        let mut sorted_trigrams = trigrams.to_vec();
        sorted_trigrams
            .sort_unstable_by_key(|t| self.postings.get(t).map(|p| p.len()).unwrap_or(0));

        let first = &self.postings[&sorted_trigrams[0]];
        let mut candidates = first.clone();

        for tri in &sorted_trigrams[1..] {
            if candidates.is_empty() {
                break;
            }
            let list = &self.postings[tri];
            candidates = intersect_sorted(&candidates, list);
        }

        Some(candidates)
    }

    /// Searches files under `root_digest` using candidate pruning and line matching.
    pub fn search<B: BlobStore + ?Sized>(
        &self,
        root_digest: &Digest,
        commit_label: &str,
        query: &str,
        path_filter: Option<&str>,
        is_regex: bool,
        is_case_sensitive: bool,
        blob_store: &B,
        max_results: usize,
    ) -> io::Result<SearchResponse> {
        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return Ok(SearchResponse {
                query: String::new(),
                commit: commit_label.to_string(),
                total_matches: 0,
                files_searched: 0,
                results: Vec::new(),
                engine: "level2".to_string(),
                duration_ms: 0.0,
                files_pruned: 0,
                is_regex,
                is_case_sensitive,
                path_filter: path_filter.unwrap_or("").to_string(),
            });
        }

        let query_trigrams = extract_trigrams_from_query(trimmed_query, is_regex);
        let candidate_ids_opt = self.find_candidate_blob_ids(&query_trigrams);

        let candidate_hashes: Option<HashSet<[u8; 32]>> = candidate_ids_opt.map(|ids| {
            ids.iter()
                .filter_map(|&id| self.id_to_hash.get(id as usize).copied())
                .collect()
        });

        let re_opt = if is_regex {
            Some(
                RegexBuilder::new(trimmed_query)
                    .case_insensitive(!is_case_sensitive)
                    .build()
                    .map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidInput, format!("Invalid regex: {}", e))
                    })?,
            )
        } else {
            None
        };

        let lower_query = if is_case_sensitive {
            trimmed_query.to_string()
        } else {
            trimmed_query.to_lowercase()
        };

        let normalized_path_filter = path_filter.map(|p| p.trim()).filter(|p| !p.is_empty());

        let mut total_matches = 0;
        let mut files_searched = 0;
        let mut files_pruned = 0;
        let mut results = Vec::new();

        let mut dir_queue = vec![(*root_digest, String::new())];

        while let Some((dir_digest, prefix)) = dir_queue.pop() {
            if total_matches >= max_results {
                break;
            }

            let dir_bytes = match blob_store.get(&dir_digest)? {
                Some(b) => b,
                None => continue,
            };

            let manifest = match DirectoryManifest::from_bytes(&dir_bytes) {
                Ok(m) => m,
                Err(_) => continue,
            };

            for (name, entry) in manifest.entries {
                let rel_path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", prefix, name)
                };

                let (file_digest, is_file) = match &entry {
                    ManifestEntry::Directory { digest, .. } => {
                        dir_queue.push((*digest, rel_path));
                        continue;
                    }
                    ManifestEntry::File { digest, .. } => (*digest, true),
                    ManifestEntry::ChunkedFile {
                        manifest_digest, ..
                    } => (*manifest_digest, true),
                };

                if is_file {
                    if let Some(pf) = normalized_path_filter {
                        let path_match = if pf.starts_with("*.") {
                            rel_path.ends_with(&pf[1..])
                        } else {
                            rel_path.contains(pf)
                        };
                        if !path_match {
                            continue;
                        }
                    }

                    let path_matches = if let Some(ref re) = re_opt {
                        re.is_match(&rel_path)
                    } else if is_case_sensitive {
                        rel_path.contains(trimmed_query)
                    } else {
                        rel_path.to_lowercase().contains(&lower_query)
                    };

                    if let Some(ref hashes) = candidate_hashes {
                        let file_hash = file_digest.hash;
                        if !hashes.contains(&file_hash) && !path_matches {
                            files_pruned += 1;
                            continue;
                        }
                    }

                    files_searched += 1;

                    let bytes = match read_entry_bytes(&entry, blob_store, MAX_SEARCH_FILE_SIZE) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };

                    if is_binary_content(&bytes) {
                        if path_matches {
                            results.push(SearchFileResult {
                                path: rel_path,
                                digest: file_digest.to_string(),
                                match_count: 1,
                                snippets: vec![SearchSnippet {
                                    line_number: 1,
                                    content: "[Binary file matches path]".to_string(),
                                    is_match: true,
                                }],
                            });
                            total_matches += 1;
                        }
                        continue;
                    }

                    let text = String::from_utf8_lossy(&bytes);
                    let lines: Vec<&str> = text.lines().collect();
                    let mut matched_indices = Vec::new();

                    for (idx, line) in lines.iter().enumerate() {
                        let is_line_match = if let Some(ref re) = re_opt {
                            re.is_match(line)
                        } else if is_case_sensitive {
                            line.contains(trimmed_query)
                        } else {
                            line.to_lowercase().contains(&lower_query)
                        };

                        if is_line_match {
                            matched_indices.push(idx);
                        }
                    }

                    let file_match_count = matched_indices.len();
                    if file_match_count > 0 || path_matches {
                        let mut snippets = Vec::new();

                        if file_match_count == 0 && path_matches {
                            let first_line = lines.first().copied().unwrap_or("");
                            snippets.push(SearchSnippet {
                                line_number: 1,
                                content: first_line.chars().take(120).collect(),
                                is_match: false,
                            });
                            total_matches += 1;
                        } else {
                            let mut lines_to_show: HashMap<usize, bool> = HashMap::new();
                            for &idx in matched_indices.iter().take(10) {
                                let line_num = idx + 1;
                                if line_num > 1 {
                                    lines_to_show.entry(line_num - 1).or_insert(false);
                                }
                                lines_to_show.insert(line_num, true);
                                if idx + 1 < lines.len() {
                                    lines_to_show.entry(line_num + 1).or_insert(false);
                                }
                            }
                            let mut sorted_nums: Vec<usize> =
                                lines_to_show.keys().copied().collect();
                            sorted_nums.sort_unstable();

                            for num in sorted_nums {
                                let is_match = lines_to_show[&num];
                                let content = lines[num - 1].chars().take(240).collect();
                                snippets.push(SearchSnippet {
                                    line_number: num,
                                    content,
                                    is_match,
                                });
                            }
                            total_matches += file_match_count;
                        }

                        results.push(SearchFileResult {
                            path: rel_path,
                            digest: file_digest.to_string(),
                            match_count: file_match_count.max(1),
                            snippets,
                        });
                    }
                }

                if total_matches >= max_results {
                    break;
                }
            }
        }

        Ok(SearchResponse {
            query: trimmed_query.to_string(),
            commit: commit_label.to_string(),
            total_matches,
            files_searched,
            results,
            engine: "level2".to_string(),
            duration_ms: 0.0,
            files_pruned,
            is_regex,
            is_case_sensitive,
            path_filter: path_filter.unwrap_or("").to_string(),
        })
    }
}

/// Global shared in-memory Trigram Index instance.
pub static GLOBAL_TRIGRAM_INDEX: OnceLock<RwLock<TrigramIndex>> = OnceLock::new();

/// Returns the global Trigram Index.
pub fn get_trigram_index() -> &'static RwLock<TrigramIndex> {
    GLOBAL_TRIGRAM_INDEX.get_or_init(|| RwLock::new(TrigramIndex::new()))
}

/// Returns current global index statistics.
pub fn get_index_stats() -> IndexStats {
    get_trigram_index()
        .read()
        .map(|idx| idx.stats())
        .unwrap_or_default()
}

/// Level-1 Direct CAS Search: streams Merkle tree without trigram pruning.
pub fn search_level1<B: BlobStore + ?Sized>(
    root_digest: &Digest,
    commit_label: &str,
    query: &str,
    path_filter: Option<&str>,
    is_regex: bool,
    is_case_sensitive: bool,
    blob_store: &B,
    max_results: usize,
) -> io::Result<SearchResponse> {
    let start = Instant::now();
    let unindexed = TrigramIndex::new();
    let mut resp = unindexed.search(
        root_digest,
        commit_label,
        query,
        path_filter,
        is_regex,
        is_case_sensitive,
        blob_store,
        max_results,
    )?;
    resp.engine = "level1".to_string();
    resp.duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok(resp)
}

/// Level-2 Trigram Inverted Index Search: utilizes global index with postings intersection.
pub fn search_level2<B: BlobStore + ?Sized>(
    root_digest: &Digest,
    commit_label: &str,
    query: &str,
    path_filter: Option<&str>,
    is_regex: bool,
    is_case_sensitive: bool,
    blob_store: &B,
    max_results: usize,
) -> io::Result<SearchResponse> {
    let start = Instant::now();

    // 1. Ensure root tree is indexed in the global cache
    {
        let needs_indexing = {
            let idx = get_trigram_index().read().unwrap();
            !idx.is_tree_indexed(root_digest)
        };
        if needs_indexing {
            let mut idx = get_trigram_index().write().unwrap();
            idx.index_tree(root_digest, blob_store)?;
        }
    }

    let idx = get_trigram_index().read().unwrap();
    let mut resp = idx.search(
        root_digest,
        commit_label,
        query,
        path_filter,
        is_regex,
        is_case_sensitive,
        blob_store,
        max_results,
    )?;
    resp.engine = "level2".to_string();
    resp.duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok(resp)
}

/// Reads the byte contents of a `ManifestEntry` (File or ChunkedFile) up to `max_bytes`.
pub fn read_entry_bytes<B: BlobStore + ?Sized>(
    entry: &ManifestEntry,
    store: &B,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    match entry {
        ManifestEntry::File { digest, .. } => {
            let bytes = store.get(digest)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("Blob {} not found", digest))
            })?;
            if bytes.len() > max_bytes {
                Ok(bytes[..max_bytes].to_vec())
            } else {
                Ok(bytes)
            }
        }
        ManifestEntry::ChunkedFile {
            manifest_digest, ..
        } => {
            let manifest_bytes = store.get(manifest_digest)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Chunk manifest {} not found", manifest_digest),
                )
            })?;
            let manifest = ChunkManifest::from_bytes(&manifest_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut out = Vec::new();
            stream_chunked_file(&manifest, store, &mut out)?;
            if out.len() > max_bytes {
                out.truncate(max_bytes);
            }
            Ok(out)
        }
        ManifestEntry::Directory { .. } => {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "Entry is a directory"))
        }
    }
}

/// Checks if a byte slice appears to be binary data (presence of null bytes in first 1024 bytes).
pub fn is_binary_content(data: &[u8]) -> bool {
    let check_len = data.len().min(1024);
    data[..check_len].contains(&0)
}

/// Backward-compatible CAS Merkle tree search wrapper (defaults to Level 1 scan).
pub fn search_cas_tree<B: BlobStore + ?Sized>(
    root_digest: &Digest,
    commit_label: &str,
    query: &str,
    blob_store: &B,
    max_results: usize,
) -> io::Result<SearchResponse> {
    search_level1(
        root_digest,
        commit_label,
        query,
        None,
        false,
        false,
        blob_store,
        max_results,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use tempfile::tempdir;

    #[test]
    fn test_make_trigram_and_extract() {
        let tri = make_trigram(b'F', b'o', b'O');
        assert_eq!(tri, make_trigram(b'f', b'O', b'o'));

        let trigrams = extract_trigrams(b"Hello");
        assert_eq!(trigrams.len(), 3); // "hel", "ell", "llo"
    }

    #[test]
    fn test_extract_literal_trigrams_from_regex() {
        let regex1 = r"struct\s+Digest\b";
        let tris1 = extract_literal_trigrams_from_regex(regex1);
        assert!(!tris1.is_empty());
        assert!(tris1.contains(&make_trigram(b's', b't', b'r')));
        assert!(tris1.contains(&make_trigram(b'd', b'i', b'g')));

        let regex2 = r"(foo|bar)";
        let tris2 = extract_literal_trigrams_from_regex(regex2);
        assert!(tris2.is_empty()); // Alternation produces safe empty trigram list

        // Quantifier test: 'b' is optional, only "cdef" trigrams should be extracted
        let regex3 = r"ab?cdef";
        let tris3 = extract_literal_trigrams_from_regex(regex3);
        assert_eq!(tris3.len(), 2); // "cde", "def"
        assert!(tris3.contains(&make_trigram(b'c', b'd', b'e')));
        assert!(tris3.contains(&make_trigram(b'd', b'e', b'f')));
    }

    #[test]
    fn test_intersect_sorted() {
        let a = vec![1, 3, 5, 7, 9];
        let b = vec![2, 3, 6, 7, 10];
        let c = intersect_sorted(&a, &b);
        assert_eq!(c, vec![3, 7]);
    }

    #[test]
    fn test_level1_and_level2_search_parity() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path()).unwrap();

        let f1_content = b"fn main() {\n    println!(\"Hello Graft\");\n}\n";
        let f1_digest = store.put(f1_content).unwrap();

        let f2_content = b"// Config parser\npub struct FastCdcChunker;\n";
        let f2_digest = store.put(f2_content).unwrap();

        let f3_content = b"// Utilities\npub fn helper() -> bool { true }\n";
        let f3_digest = store.put(f3_content).unwrap();

        let mut sub_manifest = DirectoryManifest::default();
        sub_manifest.entries.insert(
            "chunker.rs".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "chunker.rs".to_string(),
                digest: f2_digest,
            },
        );
        sub_manifest.entries.insert(
            "utils.rs".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "utils.rs".to_string(),
                digest: f3_digest,
            },
        );
        let sub_bytes = sub_manifest.serialize();
        let sub_digest = store.put(&sub_bytes).unwrap();

        let mut root_manifest = DirectoryManifest::default();
        root_manifest.entries.insert(
            "main.rs".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "main.rs".to_string(),
                digest: f1_digest,
            },
        );
        root_manifest.entries.insert(
            "cas".to_string(),
            ManifestEntry::Directory {
                name: "cas".to_string(),
                digest: sub_digest,
            },
        );
        let root_bytes = root_manifest.serialize();
        let root_digest = store.put(&root_bytes).unwrap();

        let mut index = TrigramIndex::new();
        index.index_tree(&root_digest, &store).unwrap();
        assert_eq!(index.stats().total_indexed_files, 3);

        // Test A: Literal query parity
        let r1 = search_level1(&root_digest, "main", "Graft", None, false, false, &store, 50).unwrap();
        let r2 = index.search(&root_digest, "main", "Graft", None, false, false, &store, 50).unwrap();
        assert_eq!(r1.total_matches, r2.total_matches);
        assert_eq!(r1.results[0].path, r2.results[0].path);
        assert_eq!(r2.files_pruned, 2); // 2 files pruned!

        // Test B: Case sensitivity
        let r_case_fail = index.search(&root_digest, "main", "graft", None, false, true, &store, 50).unwrap();
        assert_eq!(r_case_fail.total_matches, 0);
        let r_case_pass = index.search(&root_digest, "main", "Graft", None, false, true, &store, 50).unwrap();
        assert_eq!(r_case_pass.total_matches, 1);

        // Test C: Regex search
        let r_re = index.search(&root_digest, "main", r"struct\s+FastCdc", None, true, false, &store, 50).unwrap();
        assert_eq!(r_re.total_matches, 1);
        assert_eq!(r_re.results[0].path, "cas/chunker.rs");

        // Test D: Path filter (prefix and extension)
        let r_path = index.search(&root_digest, "main", "pub", Some("cas/"), false, false, &store, 50).unwrap();
        assert_eq!(r_path.results.len(), 2);
        let r_ext = index.search(&root_digest, "main", "pub", Some("*.rs"), false, false, &store, 50).unwrap();
        assert_eq!(r_ext.results.len(), 2);

        // Test E: Short query (< 3 chars) parity
        let r_short1 = search_level1(&root_digest, "main", "fn", None, false, false, &store, 50).unwrap();
        let r_short2 = index.search(&root_digest, "main", "fn", None, false, false, &store, 50).unwrap();
        assert_eq!(r_short1.total_matches, r_short2.total_matches);
        assert_eq!(r_short2.files_pruned, 0); // Short query falls back to scanning candidate files

        // Test F: Incremental Merkle indexing
        // Adding a new file in a new directory manifest
        let f4_content = b"pub fn incremental() {}\n";
        let f4_digest = store.put(f4_content).unwrap();
        let mut new_root = DirectoryManifest::default();
        new_root.entries.insert("cas".to_string(), ManifestEntry::Directory { name: "cas".to_string(), digest: sub_digest });
        new_root.entries.insert("new_file.rs".to_string(), ManifestEntry::File { mode: "100644".to_string(), name: "new_file.rs".to_string(), digest: f4_digest });
        let new_root_digest = store.put(&new_root.serialize()).unwrap();

        let newly_indexed = index.index_tree(&new_root_digest, &store).unwrap();
        // Since `cas` sub_digest was already in indexed_dirs, only new_file.rs was indexed!
        assert_eq!(newly_indexed, 1);
        let r_inc = index.search(&new_root_digest, "main", "incremental", None, false, false, &store, 50).unwrap();
        assert_eq!(r_inc.total_matches, 1);
        assert_eq!(r_inc.results[0].path, "new_file.rs");
    }
}
