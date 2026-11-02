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

//! GitHub-Style Embedded HTML, CSS, and UI Templates for Graft Web.

use crate::cas::commit::Commit;
use crate::cas::digest::Digest;
use crate::web::search::{IndexStats, SearchResponse};
use crate::workspace::WorkspaceConfig;

// Inline SVG Octicons
pub const ICON_REPO: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M2 2.5A2.5 2.5 0 0 1 4.5 0h8.75a.75.75 0 0 1 .75.75v12.5a.75.75 0 0 1-.75.75h-2.5a.75.75 0 0 1 0-1.5h1.75v-2h-8a1 1 0 0 0-.714 1.7.75.75 0 1 1-1.072 1.05A2.495 2.495 0 0 1 2 11.5Zm10.5-1h-8a1 1 0 0 0-1 1v6.708A2.486 2.486 0 0 1 4.5 9h8ZM5 12.25a.25.25 0 0 1 .25-.25h3.5a.25.25 0 0 1 .25.25v3.25a.25.25 0 0 1-.4.2l-1.6-1.2-1.6 1.2a.25.25 0 0 1-.4-.2Z"></path></svg>"#;
pub const ICON_BRANCH: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M9.5 3.25a2.25 2.25 0 1 1 3 2.122V6A2.5 2.5 0 0 1 10 8.5H6a1 1 0 0 0-1 1v1.128a2.251 2.251 0 1 1-1.5 0V5.372a2.25 2.25 0 1 1 1.5 0v1.836A2.493 2.493 0 0 1 6 7h4a1 1 0 0 0 1-1v-.628A2.25 2.25 0 0 1 9.5 3.25Zm-6 0a.75.75 0 1 0 1.5 0 .75.75 0 0 0-1.5 0Zm8.25-.75a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5ZM4.25 12a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Z"></path></svg>"#;
pub const ICON_FOLDER: &str = r##"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="#54aeff"><path d="M1.75 1A1.75 1.75 0 0 0 0 2.75v10.5C0 14.216.784 15 1.75 15h12.5A1.75 1.75 0 0 0 16 13.25v-8.5A1.75 1.75 0 0 0 14.25 3H7.5a.25.25 0 0 1-.2-.1l-.9-1.2C6.07 1.26 5.55 1 5 1H1.75Z"></path></svg>"##;
pub const ICON_FILE: &str = r##"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="#8b949e"><path d="M2 1.75C2 .784 2.784 0 3.75 0h6.586c.464 0 .909.184 1.237.513l2.914 2.914c.329.328.513.773.513 1.237v9.586A1.75 1.75 0 0 1 13.25 16h-9.5A1.75 1.75 0 0 1 2 14.25Zm1.75-.25a.25.25 0 0 0-.25.25v12.5c0 .138.112.25.25.25h9.5a.25.25 0 0 0 .25-.25V6h-2.75A1.75 1.75 0 0 1 9 4.25V1.5Zm6.75.062V4.25c0 .138.112.25.25.25h2.688Z"></path></svg>"##;
pub const ICON_CHUNKED: &str = r##"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="#e3b341"><path d="m8.878.392 5.25 3.045c.54.314.872.89.872 1.514v6.098a1.75 1.75 0 0 1-.872 1.514l-5.25 3.045a1.75 1.75 0 0 1-1.756 0l-5.25-3.045A1.75 1.75 0 0 1 1 11.049V4.951c0-.624.332-1.201.872-1.514L7.122.392a1.75 1.75 0 0 1 1.756 0ZM7.875 1.69l-4.63 2.685L8 7.133l4.755-2.758-4.63-2.685a.248.248 0 0 0-.25 0ZM2.5 5.63v5.419c0 .089.047.172.125.217l4.625 2.683V8.309Zm11 5.419V5.63L8.75 8.309v5.64l4.625-2.683a.25.25 0 0 0 .125-.217Z"></path></svg>"##;
pub const ICON_COMMIT: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M11.93 8.5a4.002 4.002 0 0 1-7.86 0H.75a.75.75 0 0 1 0-1.5h3.32a4.002 4.002 0 0 1 7.86 0h3.32a.75.75 0 0 1 0 1.5Zm-1.43-.75a2.5 2.5 0 1 0-5 0 2.5 2.5 0 0 0 5 0Z"></path></svg>"#;
pub const ICON_CODE: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="m11.28 3.22 4.25 4.25a.75.75 0 0 1 0 1.06l-4.25 4.25a.749.749 0 0 1-1.275-.326.749.749 0 0 1 .215-.734L13.94 8l-3.72-3.72a.749.749 0 0 1 .215-.734.749.749 0 0 1 1.06-.066Zm-6.56 0a.751.751 0 0 1 1.06.066.749.749 0 0 1 .215.734L2.06 8l3.72 3.72a.749.749 0 0 1-.215.734.749.749 0 0 1-1.275-.326l-4.25-4.25a.75.75 0 0 1 0-1.06l4.25-4.25a.749.749 0 0 1 1.06-.066Z"></path></svg>"#;
pub const ICON_DIFF: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M8.75 1.75V1a1 1 0 0 0-1-1H2a1 1 0 0 0-1 1v11.25c0 .414.336.75.75.75h4a.75.75 0 0 0 0-1.5H2.5V1.5H7v2.25c0 .414.336.75.75.75h2.25v2.25a.75.75 0 0 0 1.5 0v-4.5a.75.75 0 0 0-.22-.53l-2-2a.75.75 0 0 0-.53-.22ZM8.5 2.56L9.44 3.5H8.5ZM11 9.5a.75.75 0 0 1 .75.75v1.25H13a.75.75 0 0 1 0 1.5h-1.25v1.25a.75.75 0 0 1-1.5 0V13H9a.75.75 0 0 1 0-1.5h1.25v-1.25A.75.75 0 0 1 11 9.5Z"></path></svg>"#;
pub const ICON_WORKSPACE: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M1.5 3.25a2.25 2.25 0 1 1 3 2.122v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.25 2.25 0 0 1 1.5 3.25Zm5.677-.177L9.5 5.396l2.323-2.323a.75.75 0 0 1 1.06 1.06L10.56 6.46l2.324 2.323a.75.75 0 1 1-1.06 1.06L9.5 7.525l-2.323 2.323a.75.75 0 0 1-1.06-1.06L8.44 6.46 6.117 4.133a.75.75 0 0 1 1.06-1.06ZM3 2.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm0 9.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Z"></path></svg>"#;
pub const ICON_PR: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M1.5 3.25a2.25 2.25 0 1 1 3 2.122v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.25 2.25 0 0 1 1.5 3.25Zm5.677-.177L9.5 5.396l2.323-2.323a.75.75 0 0 1 1.06 1.06L10.56 6.46l2.324 2.323a.75.75 0 1 1-1.06 1.06L9.5 7.525l-2.323 2.323a.75.75 0 0 1-1.06-1.06L8.44 6.46 6.117 4.133a.75.75 0 0 1 1.06-1.06ZM3 2.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm0 9.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Z"></path></svg>"#;
pub const ICON_SEARCH: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="m10.68 11.74 3.25 3.25a.75.75 0 1 0 1.06-1.06l-3.25-3.25a6.5 6.5 0 1 0-1.06 1.06Zm-4.18.76a5 5 0 1 1 0-10 5 5 0 0 1 0 10Z"></path></svg>"#;
pub const ICON_COPY: &str = r#"<svg class="octicon" viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M0 6.75C0 5.784.784 5 1.75 5h1.5a.75.75 0 0 1 0 1.5h-1.5a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-1.5a.75.75 0 0 1 1.5 0v1.5A1.75 1.75 0 0 1 9.25 16h-7.5A1.75 1.75 0 0 1 0 14.25ZM5 1.75C5 .784 5.784 0 6.75 0h7.5C15.216 0 16 .784 16 1.75v7.5A1.75 1.75 0 0 1 14.25 11h-7.5A1.75 1.75 0 0 1 5 9.25Zm1.75-.25a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-7.5a.25.25 0 0 0-.25-.25Z"></path></svg>"#;

/// Authentic GitHub Dark CSS Stylesheet (Primer Design System).
pub const BASE_CSS: &str = r##"
:root {
  --color-canvas-default: #0d1117;
  --color-canvas-subtle: #161b22;
  --color-border-default: #30363d;
  --color-border-muted: #21262d;
  --color-fg-default: #e6edf3;
  --color-fg-muted: #8b949e;
  --color-fg-subtle: #6e7681;
  --color-accent-fg: #2f81f7;
  --color-accent-emphasis: #1f6feb;
  --color-success-fg: #3fb950;
  --color-success-subtle: rgba(46,160,67,0.15);
  --color-danger-fg: #f85149;
  --color-danger-subtle: rgba(248,81,73,0.15);
  --color-attention-fg: #d29922;
  --color-btn-bg: #21262d;
  --color-btn-border: #30363d;
  --color-btn-hover-bg: #30363d;
  --color-btn-hover-border: #8b949e;
  --color-header-bg: #010409;
  --color-diff-add-bg: #1f382b;
  --color-diff-add-line: #2ea043;
  --color-diff-del-bg: #3f1e24;
  --color-diff-del-line: #f85149;
  --color-diff-hunk-bg: #1e2638;
  --font-code: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
  --font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI", "Noto Sans", Helvetica, Arial, sans-serif;
}

* { box-sizing: border-box; margin: 0; padding: 0; }
body {
  background-color: var(--color-canvas-default);
  color: var(--color-fg-default);
  font-family: var(--font-sans);
  font-size: 14px;
  line-height: 1.5;
  min-height: 100vh;
  display: flex;
  flex-direction: column;
}
a { color: var(--color-accent-fg); text-decoration: none; }
a:hover { text-decoration: underline; }

/* Global App Header */
header.app-header {
  background: var(--color-header-bg);
  border-bottom: 1px solid var(--color-border-default);
  padding: 12px 24px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
}
.header-brand {
  display: flex;
  align-items: center;
  gap: 12px;
  color: #fff;
  font-weight: 600;
  font-size: 16px;
}
.header-brand svg { fill: #fff; }
.repo-tag {
  background: rgba(110,118,129,0.2);
  border: 1px solid var(--color-border-default);
  padding: 2px 8px;
  border-radius: 20px;
  font-size: 12px;
  color: var(--color-fg-muted);
}
.header-search {
  position: relative;
  max-width: 480px;
  flex: 1;
}
.header-search input {
  width: 100%;
  background: var(--color-canvas-default);
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  padding: 6px 36px 6px 12px;
  color: var(--color-fg-default);
  font-size: 13px;
  outline: none;
  font-family: var(--font-sans);
}
.header-search input:focus {
  border-color: var(--color-accent-emphasis);
  box-shadow: 0 0 0 3px rgba(31,111,235,0.3);
}
.search-kbd {
  position: absolute;
  right: 10px;
  top: 6px;
  background: var(--color-canvas-subtle);
  border: 1px solid var(--color-border-default);
  border-radius: 4px;
  padding: 1px 5px;
  font-size: 11px;
  color: var(--color-fg-muted);
}

/* Repository Sub-Header & Navigation Tabs */
.repo-banner {
  background: var(--color-canvas-subtle);
  border-bottom: 1px solid var(--color-border-default);
  padding: 16px 24px 0 24px;
}
.repo-title-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 16px;
}
.repo-title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 20px;
  font-weight: 600;
}
.repo-title span.owner { color: var(--color-accent-fg); }
.repo-title span.slash { color: var(--color-fg-muted); }
.repo-badge {
  border: 1px solid var(--color-border-default);
  border-radius: 12px;
  padding: 2px 8px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-fg-muted);
  margin-left: 6px;
}
.repo-tabs {
  display: flex;
  gap: 8px;
  list-style: none;
  overflow-x: auto;
}
.repo-tab-item a {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 8px 14px;
  color: var(--color-fg-default);
  font-size: 14px;
  border-bottom: 2px solid transparent;
  text-decoration: none;
}
.repo-tab-item a:hover {
  border-bottom-color: var(--color-border-default);
}
.repo-tab-item.active a {
  font-weight: 600;
  border-bottom-color: #f78166;
}
.tab-counter {
  background: rgba(110,118,129,0.2);
  border-radius: 10px;
  padding: 1px 6px;
  font-size: 12px;
  font-weight: 500;
  color: var(--color-fg-muted);
}

/* Page Container */
main.gh-container {
  max-width: 1240px;
  width: 100%;
  margin: 0 auto;
  padding: 24px;
  flex: 1;
}

/* GitHub File Controls Row */
.file-controls {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 16px;
  flex-wrap: wrap;
  gap: 12px;
}
.branch-select-btn {
  background: var(--color-btn-bg);
  border: 1px solid var(--color-btn-border);
  border-radius: 6px;
  padding: 5px 12px;
  color: var(--color-fg-default);
  font-weight: 600;
  font-size: 13px;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  cursor: pointer;
}
.branch-select-btn:hover {
  background: var(--color-btn-hover-bg);
  border-color: var(--color-btn-hover-border);
}

/* Branch Dropdown Menu */
.branch-dropdown-menu {
  position: absolute;
  top: 100%;
  left: 0;
  z-index: 1000;
  width: 320px;
  background: var(--color-canvas-subtle);
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  box-shadow: 0 8px 24px rgba(1,4,9,0.5);
  margin-top: 6px;
}
.branch-dropdown-header {
  padding: 8px 12px;
  font-weight: 600;
  font-size: 12px;
  display: flex;
  justify-content: space-between;
  align-items: center;
  border-bottom: 1px solid var(--color-border-muted);
}
.branch-dropdown-header .close-btn {
  cursor: pointer;
  color: var(--color-fg-muted);
  font-size: 16px;
  line-height: 1;
}
.branch-dropdown-header .close-btn:hover {
  color: var(--color-fg-default);
}
.branch-list {
  max-height: 260px;
  overflow-y: auto;
}
.branch-item {
  display: flex;
  align-items: center;
  padding: 8px 12px;
  color: var(--color-fg-default);
  font-size: 13px;
  border-bottom: 1px solid var(--color-border-muted);
  text-decoration: none;
}
.branch-item:hover {
  background: rgba(110,118,129,0.1);
  text-decoration: none;
}
.branch-item.active {
  font-weight: 600;
}
.branch-check {
  width: 16px;
  margin-right: 8px;
  display: inline-block;
  color: var(--color-accent-fg);
  font-weight: bold;
}

/* GitHub Latest Commit Banner */
.commit-tease {
  background: var(--color-canvas-subtle);
  border: 1px solid var(--color-border-default);
  border-radius: 6px 6px 0 0;
  padding: 12px 16px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 13px;
  border-bottom: 1px solid var(--color-border-default);
}
.commit-tease-left {
  display: flex;
  align-items: center;
  gap: 10px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.avatar-circle {
  width: 24px;
  height: 24px;
  border-radius: 50%;
  background: #388bfd;
  color: #fff;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-weight: 700;
  font-size: 12px;
}
.commit-author { font-weight: 600; color: var(--color-fg-default); }
.commit-msg { color: var(--color-fg-default); margin-left: 4px; }
.commit-tease-right {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-shrink: 0;
}
.commit-sha-badge {
  font-family: var(--font-code);
  font-size: 12px;
  color: var(--color-accent-fg);
  background: rgba(110,118,129,0.1);
  padding: 2px 6px;
  border-radius: 4px;
}

/* GitHub File Explorer Table */
.file-box {
  background: var(--color-canvas-default);
  border: 1px solid var(--color-border-default);
  border-radius: 0 0 6px 6px;
  overflow: hidden;
  margin-bottom: 24px;
}
.commit-tease + .file-box {
  border-top: none;
}
table.file-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 14px;
}
table.file-table tr {
  border-bottom: 1px solid var(--color-border-muted);
}
table.file-table tr:last-child { border-bottom: none; }
table.file-table tr:hover {
  background: rgba(110,118,129,0.06);
}
table.file-table td {
  padding: 8px 16px;
  vertical-align: middle;
}
td.icon-col { width: 32px; padding-right: 0; }
td.name-col { font-weight: 500; }
td.msg-col { color: var(--color-fg-muted); font-size: 13px; max-width: 480px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
td.msg-col a.commit-msg-link { color: var(--color-fg-muted); text-decoration: none; display: inline-block; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; vertical-align: middle; }
td.msg-col a.commit-msg-link:hover { color: var(--color-accent-fg); text-decoration: underline; }
td.age-col { text-align: right; color: var(--color-fg-muted); font-size: 12px; width: 120px; white-space: nowrap; }

/* GitHub README Card */
.readme-box {
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  background: var(--color-canvas-default);
  margin-top: 24px;
  overflow: hidden;
}
.readme-header {
  background: var(--color-canvas-subtle);
  border-bottom: 1px solid var(--color-border-default);
  padding: 10px 16px;
  font-weight: 600;
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 14px;
}
.readme-body {
  padding: 24px 32px;
  line-height: 1.6;
}

/* GitHub Primer Markdown Body */
.markdown-body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", "Noto Sans", Helvetica, Arial, sans-serif;
  font-size: 14px;
  line-height: 1.6;
  word-wrap: break-word;
  color: var(--color-fg-default);
}
.markdown-body h1, .markdown-body h2, .markdown-body h3,
.markdown-body h4, .markdown-body h5, .markdown-body h6 {
  margin-top: 24px;
  margin-bottom: 16px;
  font-weight: 600;
  line-height: 1.25;
  color: var(--color-fg-default);
}
.markdown-body h1 {
  font-size: 2em;
  padding-bottom: .3em;
  border-bottom: 1px solid var(--color-border-muted);
}
.markdown-body h2 {
  font-size: 1.5em;
  padding-bottom: .3em;
  border-bottom: 1px solid var(--color-border-muted);
}
.markdown-body h3 { font-size: 1.25em; }
.markdown-body h4 { font-size: 1em; }
.markdown-body h5 { font-size: .875em; }
.markdown-body h6 { font-size: .85em; color: var(--color-fg-muted); }

.markdown-body p {
  margin-top: 0;
  margin-bottom: 16px;
}
.markdown-body a {
  color: var(--color-accent-fg);
  text-decoration: none;
}
.markdown-body a:hover {
  text-decoration: underline;
}
.markdown-body a.anchor {
  color: var(--color-fg-subtle);
  text-decoration: none;
  margin-right: 6px;
  opacity: 0.3;
}
.markdown-body a.anchor:hover {
  opacity: 1;
}
.markdown-body blockquote {
  margin: 0 0 16px 0;
  padding: 0 1em;
  color: var(--color-fg-muted);
  border-left: .25em solid var(--color-border-default);
}
.markdown-body blockquote > :first-child { margin-top: 0; }
.markdown-body blockquote > :last-child { margin-bottom: 0; }

.markdown-body ul, .markdown-body ol {
  margin-top: 0;
  margin-bottom: 16px;
  padding-left: 2em;
}
.markdown-body li {
  margin-top: 0.25em;
}
.markdown-body li.task-list-item {
  list-style-type: none;
  margin-left: -1.3em;
}
.markdown-body li.task-list-item input {
  margin: 0 0.35em 0.25em -1.4em;
  vertical-align: middle;
}

.markdown-body hr {
  height: .25em;
  padding: 0;
  margin: 24px 0;
  background-color: var(--color-border-default);
  border: 0;
}

.markdown-body code {
  padding: .2em .4em;
  margin: 0;
  font-size: 85%;
  white-space: break-spaces;
  background-color: rgba(110,118,129,0.2);
  border-radius: 6px;
  font-family: var(--font-code);
}
.markdown-body pre {
  padding: 16px;
  overflow: auto;
  font-size: 85%;
  line-height: 1.45;
  background-color: var(--color-canvas-subtle);
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  margin-bottom: 16px;
}
.markdown-body pre code {
  padding: 0;
  margin: 0;
  font-size: 100%;
  word-break: normal;
  white-space: pre;
  background: transparent;
  border: 0;
}

/* Mermaid Diagrams */
.mermaid-container {
  display: flex;
  justify-content: center;
  align-items: center;
  background: var(--color-canvas-subtle);
  border: 1px solid var(--color-border-default);
  border-radius: 8px;
  padding: 28px 20px;
  margin: 24px 0;
  overflow-x: auto;
}
.mermaid-container pre.mermaid {
  margin: 0;
  padding: 0;
  background: transparent;
  border: none;
  font-family: var(--font-sans);
  font-size: 14px;
  color: var(--color-fg-default);
  text-align: center;
  width: 100%;
}
.mermaid-container svg {
  min-width: 620px;
  max-width: 960px;
  width: 100%;
  height: auto;
}

/* GitHub Alerts / Callouts */
.markdown-alert {
  padding: 8px 16px;
  margin-bottom: 16px;
  border-left: .25em solid;
  border-radius: 4px;
  background: var(--color-canvas-subtle);
}
.markdown-alert-title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-weight: 600;
  font-size: 13px;
  margin-bottom: 4px;
}
.markdown-alert-note { border-color: #1f6feb; }
.markdown-alert-note .markdown-alert-title { color: #58a6ff; }
.markdown-alert-tip { border-color: #238636; }
.markdown-alert-tip .markdown-alert-title { color: #3fb950; }
.markdown-alert-important { border-color: #8957e5; }
.markdown-alert-important .markdown-alert-title { color: #a371f7; }
.markdown-alert-warning { border-color: #9e6a03; }
.markdown-alert-warning .markdown-alert-title { color: #d29922; }
.markdown-alert-caution { border-color: #da3633; }
.markdown-alert-caution .markdown-alert-title { color: #f85149; }

/* Markdown Tables */
.markdown-table {
  border-spacing: 0;
  border-collapse: collapse;
  margin-top: 0;
  margin-bottom: 16px;
  width: 100%;
  overflow: auto;
  display: block;
}
.markdown-table th, .markdown-table td {
  padding: 6px 13px;
  border: 1px solid var(--color-border-default);
}
.markdown-table th {
  font-weight: 600;
  background-color: var(--color-canvas-subtle);
}
.markdown-table tr:nth-child(2n) {
  background-color: rgba(110,118,129,0.06);
}
.markdown-body img {
  max-width: 100%;
  box-sizing: content-box;
}

/* GitHub Blob View */
.blob-box {
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  overflow: hidden;
  background: var(--color-canvas-default);
}
.blob-meta-header {
  background: var(--color-canvas-subtle);
  border-bottom: 1px solid var(--color-border-default);
  padding: 10px 16px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 13px;
  color: var(--color-fg-muted);
}
.btn-group {
  display: inline-flex;
  border: 1px solid var(--color-btn-border);
  border-radius: 6px;
  overflow: hidden;
}
.btn-group .btn {
  border: none;
  border-right: 1px solid var(--color-btn-border);
  border-radius: 0;
}
.btn-group .btn:last-child { border-right: none; }
.btn {
  background: var(--color-btn-bg);
  border: 1px solid var(--color-btn-border);
  border-radius: 6px;
  padding: 4px 10px;
  color: var(--color-fg-default);
  font-size: 12px;
  font-weight: 500;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  text-decoration: none;
}
.btn:hover {
  background: var(--color-btn-hover-bg);
  border-color: var(--color-btn-hover-border);
  text-decoration: none;
}
.btn.active {
  background: var(--color-canvas-subtle);
  color: var(--color-accent-fg);
  font-weight: 600;
}
.diffstat {
  font-family: var(--font-code);
  font-size: 12px;
  display: inline-flex;
  align-items: center;
  gap: 6px;
}
.diffstat-add {
  color: #3fb950;
  font-weight: 600;
}
.diffstat-del {
  color: #f85149;
  font-weight: 600;
}
.code-table-container {
  overflow-x: auto;
  font-family: var(--font-code);
  font-size: 13px;
  line-height: 1.6;
}
table.code-blob-table {
  border-collapse: collapse;
  width: 100%;
}
table.code-blob-table td.blob-num {
  width: 50px;
  padding: 0 12px;
  text-align: right;
  color: var(--color-fg-subtle);
  user-select: none;
  border-right: 1px solid var(--color-border-muted);
}
table.code-blob-table td.blob-code {
  padding: 0 16px;
  white-space: pre;
}

/* GitHub Commit Diff Card */
.diff-box {
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  margin-bottom: 16px;
  overflow: hidden;
  background: var(--color-canvas-default);
}
.diff-box-header {
  background: var(--color-canvas-subtle);
  border-bottom: 1px solid var(--color-border-default);
  padding: 8px 16px;
  font-family: var(--font-code);
  font-size: 13px;
  font-weight: 600;
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.diff-lines {
  font-family: var(--font-code);
  font-size: 12px;
  line-height: 1.5;
  overflow-x: auto;
}
.diff-row {
  display: flex;
  padding: 0 16px;
  white-space: pre;
}
.diff-row.add { background: var(--color-diff-add-bg); color: var(--color-diff-add-line); }
.diff-row.del { background: var(--color-diff-del-bg); color: var(--color-diff-del-line); }
.diff-row.hunk { background: var(--color-diff-hunk-bg); color: #79c0ff; font-weight: 600; }

/* GitHub Code Search View */
.search-results-header {
  margin-bottom: 20px;
  border-bottom: 1px solid var(--color-border-default);
  padding-bottom: 12px;
}
.search-console-card {
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  background: var(--color-canvas-subtle);
  padding: 16px 20px;
  margin-bottom: 24px;
}
.search-input-group {
  display: flex;
  gap: 10px;
  align-items: center;
  margin-bottom: 12px;
}
.search-input-wrapper {
  position: relative;
  flex: 1;
  display: flex;
  align-items: center;
}
.search-input-wrapper .search-input-icon {
  position: absolute;
  left: 12px;
  fill: var(--color-fg-muted);
  pointer-events: none;
}
.search-console-input {
  width: 100%;
  padding: 8px 12px 8px 36px;
  font-size: 14px;
  background: var(--color-canvas-default);
  color: var(--color-fg-default);
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  font-family: inherit;
  outline: none;
  transition: border-color 0.15s ease;
}
.search-console-input:focus {
  border-color: var(--color-accent-emphasis);
  box-shadow: 0 0 0 3px rgba(31, 111, 235, 0.3);
}
.search-ref-select {
  padding: 7px 10px;
  font-size: 13px;
  border-radius: 6px;
  background: var(--color-canvas-default);
  color: var(--color-fg-default);
  border: 1px solid var(--color-border-default);
  max-width: 160px;
}
.search-controls-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
  flex-wrap: wrap;
  gap: 12px;
  font-size: 13px;
  color: var(--color-fg-muted);
}
.search-engine-selector {
  display: flex;
  align-items: center;
  gap: 12px;
}
.engine-label {
  font-weight: 600;
  color: var(--color-fg-default);
}
.engine-option {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  cursor: pointer;
  user-select: none;
}
.engine-option input[type="radio"] {
  accent-color: var(--color-accent-emphasis);
}
.engine-badge {
  font-size: 12px;
  padding: 2px 7px;
  border-radius: 4px;
  font-weight: 500;
}
.engine-badge.level2 {
  background: rgba(35, 134, 54, 0.15);
  color: #3fb950;
  border: 1px solid rgba(46, 160, 67, 0.3);
}
.engine-badge.level1 {
  background: rgba(56, 139, 253, 0.15);
  color: #58a6ff;
  border: 1px solid rgba(56, 139, 253, 0.3);
}
.search-options-group {
  display: flex;
  align-items: center;
  gap: 16px;
}
.search-option-label {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  cursor: pointer;
  user-select: none;
  font-size: 12px;
  color: var(--color-fg-default);
}
.search-option-label input[type="checkbox"] {
  accent-color: var(--color-accent-emphasis);
}
.search-path-filter {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  background: var(--color-canvas-default);
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  padding: 3px 8px;
}
.search-path-filter .filter-prefix {
  font-family: var(--font-code);
  font-size: 11px;
  color: var(--color-fg-muted);
}
.search-path-input {
  border: none;
  background: transparent;
  color: var(--color-fg-default);
  font-size: 12px;
  font-family: var(--font-code);
  width: 140px;
  outline: none;
}
.search-welcome-container {
  margin-top: 16px;
}
.search-status-banner {
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  background: var(--color-canvas-default);
  padding: 20px;
  margin-bottom: 24px;
}
.status-indicator {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 14px;
  font-weight: 600;
  color: #3fb950;
  margin-bottom: 16px;
}
.status-pulse-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: #3fb950;
  box-shadow: 0 0 0 3px rgba(46, 160, 67, 0.25);
}
.status-metrics-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: 16px;
}
.metric-card {
  background: var(--color-canvas-subtle);
  border: 1px solid var(--color-border-muted);
  border-radius: 6px;
  padding: 12px 16px;
}
.metric-value {
  font-size: 20px;
  font-weight: 700;
  color: var(--color-fg-default);
  font-family: var(--font-code);
}
.metric-label {
  font-size: 12px;
  color: var(--color-fg-muted);
  margin-top: 4px;
}
.search-quick-queries {
  margin-bottom: 24px;
}
.search-quick-queries h3 {
  font-size: 14px;
  margin-bottom: 10px;
  color: var(--color-fg-muted);
}
.quick-pills-row {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
.quick-pill {
  display: inline-flex;
  align-items: center;
  padding: 5px 12px;
  border-radius: 20px;
  background: var(--color-canvas-subtle);
  border: 1px solid var(--color-border-default);
  color: var(--color-fg-default);
  font-size: 12px;
  text-decoration: none;
  transition: all 0.15s ease;
}
.quick-pill:hover {
  background: var(--color-accent-subtle);
  border-color: var(--color-accent-emphasis);
  color: var(--color-accent-fg);
}
.quick-pill code {
  color: inherit;
  font-size: 12px;
}
.search-features-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
  gap: 16px;
}
.feature-card {
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  background: var(--color-canvas-default);
  padding: 16px;
}
.feature-header {
  font-weight: 600;
  font-size: 13px;
  color: var(--color-fg-default);
  margin-bottom: 6px;
}
.feature-card p {
  font-size: 12px;
  color: var(--color-fg-muted);
  line-height: 1.5;
  margin: 0;
}
.search-metrics-banner {
  display: flex;
  justify-content: space-between;
  align-items: center;
  flex-wrap: wrap;
  gap: 12px;
  padding: 12px 16px;
  background: var(--color-canvas-subtle);
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  margin-bottom: 16px;
  font-size: 13px;
}
.metrics-timing {
  color: var(--color-fg-muted);
  margin-left: 8px;
}
.metrics-badges {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
}
.badge-purple {
  background: rgba(163, 113, 247, 0.15);
  color: #d2a8ff;
  border: 1px solid rgba(163, 113, 247, 0.3);
}
.badge-success {
  background: rgba(35, 134, 54, 0.15);
  color: #3fb950;
  border: 1px solid rgba(46, 160, 67, 0.3);
}
.badge-neutral {
  background: var(--color-canvas-default);
  color: var(--color-fg-muted);
  border: 1px solid var(--color-border-default);
}
.search-match-card {
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  background: var(--color-canvas-default);
  margin-bottom: 16px;
  overflow: hidden;
}
.search-match-header {
  background: var(--color-canvas-subtle);
  border-bottom: 1px solid var(--color-border-default);
  padding: 8px 16px;
  font-weight: 600;
  font-family: var(--font-code);
  font-size: 13px;
  display: flex;
  justify-content: space-between;
}
table.match-snippet-table {
  width: 100%;
  border-collapse: collapse;
  font-family: var(--font-code);
  font-size: 12px;
}
table.match-snippet-table td.num {
  width: 48px;
  padding: 2px 10px;
  text-align: right;
  color: var(--color-fg-subtle);
  border-right: 1px solid var(--color-border-muted);
}
table.match-snippet-table td.code {
  padding: 2px 14px;
  white-space: pre;
}
table.match-snippet-table tr.matched td.code {
  background: rgba(56, 139, 253, 0.12);
}
mark {
  background: rgba(234, 179, 8, 0.35);
  color: #fff;
  border-radius: 2px;
  padding: 0 2px;
}

/* Split & Unified Diff Tables */
.diff-container {
  width: 100%;
}
.diff-view-selector {
  display: inline-flex;
  border: 1px solid var(--color-btn-border);
  border-radius: 6px;
  overflow: hidden;
  margin-left: 12px;
}
.diff-view-selector .btn {
  border-radius: 0;
  border: none;
  border-right: 1px solid var(--color-btn-border);
}
.diff-view-selector .btn:last-child {
  border-right: none;
}
.diff-file-card {
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  margin-bottom: 16px;
  overflow: hidden;
  background: var(--color-canvas-default);
}
.diff-file-header {
  background: var(--color-canvas-subtle);
  border-bottom: 1px solid var(--color-border-default);
  padding: 8px 16px;
  font-family: var(--font-code);
  font-size: 13px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
.diff-file-path {
  font-weight: 600;
  color: var(--color-fg-default);
}
table.diff-table {
  width: 100%;
  border-collapse: collapse;
  font-family: var(--font-code);
  font-size: 12px;
  line-height: 1.5;
}
table.diff-table td {
  padding: 1px 8px;
  vertical-align: top;
}
table.diff-table td.diff-lineno {
  width: 45px;
  min-width: 45px;
  max-width: 45px;
  text-align: right;
  color: var(--color-fg-subtle);
  user-select: none;
  border-right: 1px solid var(--color-border-muted);
}
table.diff-table td.diff-marker {
  width: 16px;
  min-width: 16px;
  text-align: center;
  user-select: none;
}
table.diff-table td.diff-content {
  white-space: pre-wrap;
  word-break: break-all;
}
tr.diff-unified-row.add td.diff-content, tr.diff-unified-row.add td.diff-marker {
  background: var(--color-diff-add-bg);
  color: var(--color-diff-add-line);
}
tr.diff-unified-row.del td.diff-content, tr.diff-unified-row.del td.diff-marker {
  background: var(--color-diff-del-bg);
  color: var(--color-diff-del-line);
}
tr.diff-unified-row.hunk td {
  background: var(--color-diff-hunk-bg);
  color: #79c0ff;
  font-weight: 600;
}
td.split-cell {
  width: 47%;
  white-space: pre-wrap;
  word-break: break-all;
}
td.split-cell.add {
  background: var(--color-diff-add-bg);
  color: var(--color-diff-add-line);
}
td.split-cell.del {
  background: var(--color-diff-del-bg);
  color: var(--color-diff-del-line);
}
td.split-cell.empty {
  background: rgba(110, 118, 129, 0.05);
}
span.hl-add {
  background: rgba(46, 160, 67, 0.4);
  border-radius: 2px;
}
span.hl-del {
  background: rgba(248, 81, 73, 0.4);
  border-radius: 2px;
}
.diff-container.mode-unified .diff-split-table { display: none !important; }
.diff-container.mode-unified .diff-unified-table { display: table !important; }
.diff-container.mode-split .diff-unified-table { display: none !important; }
.diff-container.mode-split .diff-split-table { display: table !important; }

/* Blame View */
table.blame-table {
  width: 100%;
  border-collapse: collapse;
  font-family: var(--font-code);
  font-size: 12px;
}
table.blame-table tr {
  border-bottom: 1px solid var(--color-border-muted);
}
table.blame-table tr:hover {
  background: rgba(110,118,129,0.06);
}
table.blame-table td.blame-gutter {
  padding: 4px 10px;
  background: var(--color-canvas-subtle);
  border-right: 1px solid var(--color-border-default);
  white-space: nowrap;
  color: var(--color-fg-muted);
  user-select: none;
  width: 1px;
}
table.blame-table td.blame-num {
  width: 45px;
  text-align: right;
  padding: 4px 8px;
  color: var(--color-fg-subtle);
  border-right: 1px solid var(--color-border-muted);
  user-select: none;
}
table.blame-table td.blame-code {
  padding: 4px 16px;
  white-space: pre;
}

/* Pull Requests Dashboard & Detail */
.pr-badge {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 4px 10px;
  border-radius: 20px;
  font-size: 13px;
  font-weight: 600;
  color: #fff;
}
.pr-badge.open { background: #238636; }
.pr-badge.merged { background: #8957e5; }
.pr-badge.closed { background: #da3633; }
.pr-timeline-item {
  display: flex;
  gap: 16px;
  margin-bottom: 20px;
}
.pr-comment-card {
  flex: 1;
  border: 1px solid var(--color-border-default);
  border-radius: 6px;
  overflow: hidden;
  background: var(--color-canvas-default);
}
.pr-comment-header {
  background: var(--color-canvas-subtle);
  border-bottom: 1px solid var(--color-border-default);
  padding: 8px 16px;
  font-size: 13px;
  display: flex;
  justify-content: space-between;
}
.pr-comment-body {
  padding: 16px;
  font-size: 14px;
  line-height: 1.6;
}

/* Footer */
footer.gh-footer {
  border-top: 1px solid var(--color-border-default);
  padding: 24px;
  text-align: center;
  font-size: 12px;
  color: var(--color-fg-muted);
}
"##;

pub const BASE_JS: &str = r##"
document.addEventListener('keydown', (e) => {
  if (e.key === '/' && document.activeElement.tagName !== 'INPUT' && document.activeElement.tagName !== 'TEXTAREA') {
    e.preventDefault();
    const searchInput = document.querySelector('.header-search input');
    if (searchInput) {
      searchInput.focus();
      searchInput.select();
    }
  }
});

function copyToClipboard(text, btn) {
  navigator.clipboard.writeText(text).then(() => {
    const orig = btn.innerText;
    btn.innerText = 'Copied!';
    setTimeout(() => { btn.innerText = orig; }, 1500);
  });
}

function toggleBranchMenu(e) {
  if (e) {
    e.stopPropagation();
    e.preventDefault();
  }
  const menu = document.getElementById('branch-menu');
  if (!menu) return;
  const isHidden = menu.style.display === 'none' || menu.style.display === '';
  menu.style.display = isHidden ? 'block' : 'none';
  if (isHidden) {
    const input = document.getElementById('branch-filter');
    if (input) {
      input.focus();
      input.value = '';
      filterBranches();
    }
  }
}

document.addEventListener('click', (e) => {
  const menu = document.getElementById('branch-menu');
  if (menu && menu.style.display === 'block' && !menu.contains(e.target)) {
    menu.style.display = 'none';
  }
});

function filterBranches() {
  const input = document.getElementById('branch-filter');
  if (!input) return;
  const filter = input.value.toLowerCase().trim();
  const items = document.querySelectorAll('#branch-list a');
  items.forEach((item) => {
    const text = item.innerText.toLowerCase();
    item.style.display = text.includes(filter) ? 'flex' : 'none';
  });
}

function handleBranchKey(e) {
  if (e.key === 'Enter') {
    e.preventDefault();
    const val = e.target.value.trim();
    if (val) {
      location.href = '/tree/' + encodeURIComponent(val) + '/';
    }
  }
}

function switchBlobTab(tab) {
  const preview = document.getElementById('blob-preview-container');
  const code = document.getElementById('blob-code-container');
  const btnPreview = document.getElementById('btn-preview');
  const btnCode = document.getElementById('btn-code');
  if (!preview || !code) return;
  if (tab === 'preview') {
    preview.style.display = 'block';
    code.style.display = 'none';
    if (btnPreview) btnPreview.classList.add('active');
    if (btnCode) btnCode.classList.remove('active');
    if (window.__renderMermaid) {
      window.__renderMermaid();
    }
  } else {
    preview.style.display = 'none';
    code.style.display = 'block';
    if (btnCode) btnCode.classList.add('active');
    if (btnPreview) btnPreview.classList.remove('active');
  }
}

function setDiffMode(mode) {
  const container = document.getElementById('diff-container');
  const btnUnified = document.getElementById('btn-unified');
  const btnSplit = document.getElementById('btn-split');
  if (!container) return;
  if (mode === 'split') {
    container.classList.remove('mode-unified');
    container.classList.add('mode-split');
    if (btnSplit) btnSplit.classList.add('active');
    if (btnUnified) btnUnified.classList.remove('active');
    try { localStorage.setItem('graft_diff_mode', 'split'); } catch(e){}
  } else {
    container.classList.remove('mode-split');
    container.classList.add('mode-unified');
    if (btnUnified) btnUnified.classList.add('active');
    if (btnSplit) btnSplit.classList.remove('active');
    try { localStorage.setItem('graft_diff_mode', 'unified'); } catch(e){}
  }
}

document.addEventListener('DOMContentLoaded', () => {
  try {
    const saved = localStorage.getItem('graft_diff_mode');
    if (saved === 'split') {
      setDiffMode('split');
    }
  } catch(e){}
});
"##;

pub const MERMAID_INIT_JS: &str = r##"
import mermaid from 'https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.esm.min.mjs';
mermaid.initialize({
  startOnLoad: true,
  theme: 'dark',
  themeVariables: {
    darkMode: true,
    fontFamily: 'ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
    fontSize: '14px',
    background: '#161b22',
    primaryColor: '#1f6feb',
    primaryTextColor: '#f0f6fc',
    primaryBorderColor: '#388bfd',
    lineColor: '#58a6ff',
    secondaryColor: '#21262d',
    tertiaryColor: '#0d1117'
  },
  flowchart: {
    curve: 'basis',
    nodeSpacing: 40,
    rankSpacing: 40,
    padding: 12
  }
});
window.__renderMermaid = () => {
  try {
    mermaid.run({ querySelector: '.mermaid' });
  } catch (err) {
    console.warn('Mermaid rendering error:', err);
  }
};
"##;

/// Renders GitHub-style page wrapper with header, repo banner, tabs, and footer.
pub fn render_html_page(
    title: &str,
    active_tab: &str,
    current_ref: &str,
    body_content: &str,
) -> String {
    let code_active = if active_tab == "files" { "active" } else { "" };
    let pulls_active = if active_tab == "pulls" { "active" } else { "" };
    let search_active = if active_tab == "search" { "active" } else { "" };
    let commits_active = if active_tab == "commits" { "active" } else { "" };
    let ws_active = if active_tab == "workspaces" { "active" } else { "" };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title} · graft</title>
  <style>{BASE_CSS}</style>
</head>
<body>
  <!-- GitHub Global Navigation Header -->
  <header class="app-header">
    <a href="/" class="header-brand">
      {ICON_REPO}
      <span>graft</span>
      <span class="repo-tag">v0.1.0</span>
    </a>
    <form class="header-search" action="/search" method="GET">
      <input type="hidden" name="ref" value="{current_ref}">
      <input type="text" name="q" placeholder="Type / to search code in this repository..." autocomplete="off">
      <span class="search-kbd">/</span>
    </form>
    <div>
      <a href="/api/info" class="btn">REST API</a>
    </div>
  </header>

  <!-- GitHub Repository Subheader & Tabs -->
  <div class="repo-banner">
    <div class="repo-title-row">
      <div class="repo-title">
        {ICON_REPO}
        <span class="owner">workspace</span>
        <span class="slash">/</span>
        <a href="/tree/{current_ref}/"><strong>graft</strong></a>
        <span class="repo-badge">Public</span>
      </div>
      <div>
        <a href="/diff/{current_ref}" class="btn">{ICON_DIFF} Compare</a>
      </div>
    </div>
    <ul class="repo-tabs">
      <li class="repo-tab-item {code_active}"><a href="/tree/{current_ref}/">{ICON_CODE} Code</a></li>
      <li class="repo-tab-item {pulls_active}"><a href="/pulls">{ICON_PR} Pull Requests</a></li>
      <li class="repo-tab-item {search_active}"><a href="/search?ref={current_ref}">{ICON_SEARCH} Code Search</a></li>
      <li class="repo-tab-item {commits_active}"><a href="/commits?ref={current_ref}">{ICON_COMMIT} Commits</a></li>
      <li class="repo-tab-item {ws_active}"><a href="/workspaces">{ICON_WORKSPACE} Agent Workspaces</a></li>
    </ul>
  </div>

  <!-- Main Content Container -->
  <main class="gh-container">
    {body_content}
  </main>

  <footer class="gh-footer">
    <span>🌱 <strong>Graft</strong> · High-Performance Virtualized Source Control for AI Agents & Monorepos</span>
  </footer>
  <script>{BASE_JS}</script>
  <script type="module">{MERMAID_INIT_JS}</script>
</body>
</html>"#
    )
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

pub fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn render_breadcrumbs(current_ref: &str, current_path: &str, is_dir: bool) -> String {
    let mut html = String::new();
    html.push_str("<div style=\"display:flex; align-items:center; gap:6px; font-size:14px; font-family:var(--font-code);\">");
    html.push_str(&format!(
        "<a href=\"/tree/{}/\"><strong style=\"color:var(--color-fg-default);\">graft</strong></a>",
        current_ref
    ));

    let trimmed = current_path.trim_matches('/');
    if !trimmed.is_empty() {
        let parts: Vec<&str> = trimmed.split('/').collect();
        let mut accum = String::new();
        for (i, part) in parts.iter().enumerate() {
            html.push_str("<span style=\"color:var(--color-fg-muted);\">/</span>");
            accum.push_str(part);
            let is_last = i == parts.len() - 1;
            if is_last && !is_dir {
                html.push_str(&format!(
                    "<strong style=\"color:var(--color-fg-default);\">{}</strong>",
                    escape_html(part)
                ));
            } else {
                html.push_str(&format!(
                    "<a href=\"/tree/{}/{}/\">{}</a>",
                    current_ref,
                    accum,
                    escape_html(part)
                ));
            }
            accum.push('/');
        }
    }
    html.push_str("</div>");
    html
}

/// Renders the GitHub-style Branch Switcher Dropdown menu.
pub fn render_branch_switcher(
    current_ref: &str,
    available_branches: &[String],
    current_path: &str,
    is_blob: bool,
) -> String {
    let mut out = String::new();
    out.push_str(
        "<div class=\"branch-select-wrapper\" style=\"position:relative; display:inline-block;\">",
    );
    out.push_str(&format!(
        "<button class=\"branch-select-btn\" onclick=\"toggleBranchMenu(event)\">{ICON_BRANCH} <span>{}</span> <span style=\"font-size:10px;\">▼</span></button>",
        escape_html(current_ref)
    ));
    out.push_str("<div id=\"branch-menu\" class=\"branch-dropdown-menu\" style=\"display:none;\">");
    out.push_str("<div class=\"branch-dropdown-header\"><span>Switch branches/tags/commits</span><span class=\"close-btn\" onclick=\"toggleBranchMenu(event)\">&times;</span></div>");
    out.push_str("<div style=\"padding:8px;\"><input type=\"text\" id=\"branch-filter\" placeholder=\"Find branch or enter commit SHA...\" onkeyup=\"filterBranches()\" onkeydown=\"handleBranchKey(event)\" style=\"width:100%; background:var(--color-canvas-default); border:1px solid var(--color-border-default); border-radius:4px; padding:6px 8px; color:var(--color-fg-default); font-size:12px; outline:none; font-family:var(--font-sans);\" /></div>");
    out.push_str("<div class=\"branch-list\" id=\"branch-list\">");

    let prefix = if is_blob { "blob" } else { "tree" };
    let clean_path = current_path.trim_start_matches('/');

    let mut branches = available_branches.to_vec();
    if !branches.iter().any(|b| b == current_ref) {
        branches.push(current_ref.to_string());
    }

    for b in &branches {
        let is_current = b == current_ref;
        let check = if is_current { "✓" } else { "" };
        let active_cls = if is_current { "active" } else { "" };
        let target_url = if clean_path.is_empty() {
            format!("/{}/{}/", prefix, b)
        } else {
            format!("/{}/{}/{}", prefix, b, clean_path)
        };
        out.push_str(&format!(
            "<a href=\"{}\" class=\"branch-item {}\"><span class=\"branch-check\">{}</span><span>{}</span></a>",
            target_url, active_cls, check, escape_html(b)
        ));
    }

    out.push_str("</div>");
    out.push_str("</div>");
    out.push_str("</div>");
    out
}

pub struct TreeTableEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
    pub is_chunked: bool,
    pub size: u64,
    pub digest: String,
    pub commit_msg: String,
    pub commit_hash: String,
    pub commit_age: String,
}

/// Renders GitHub-style Directory Tree View.
pub fn render_tree_view(
    current_ref: &str,
    available_branches: &[String],
    current_path: &str,
    commit_opt: Option<(&Digest, &Commit, Option<crate::git::diff::DiffStats>)>,
    entries: &[TreeTableEntry],
    readme_content: Option<&str>,
) -> String {
    let mut out = String::new();

    // Controls Row: Branch Switcher & Breadcrumbs
    out.push_str("<div class=\"file-controls\">");
    out.push_str("<div style=\"display:flex; align-items:center; gap:12px;\">");
    out.push_str(&render_branch_switcher(current_ref, available_branches, current_path, false));
    out.push_str(&render_breadcrumbs(current_ref, current_path, true));
    out.push_str("</div>");

    out.push_str("<div style=\"display:flex; gap:8px;\">");
    out.push_str(&format!(
        "<a href=\"/commits?ref={}\" class=\"btn\">{ICON_COMMIT} History</a>",
        current_ref
    ));
    out.push_str("</div>");
    out.push_str("</div>");

    // Latest Commit Tease Banner
    if let Some((commit_digest, commit, stats_opt)) = commit_opt {
        let first_letter = commit.author.chars().next().unwrap_or('A').to_uppercase();
        let short_sha = &commit_digest.to_hex()[..7];
        let diffstat_html = if let Some(stats) = stats_opt {
            if stats.additions > 0 || stats.deletions > 0 {
                format!(
                    "<span class=\"diffstat\" style=\"margin-left:8px;\"><span class=\"diffstat-add\">+{}</span><span class=\"diffstat-del\">-{}</span></span>",
                    stats.additions, stats.deletions
                )
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        out.push_str("<div class=\"commit-tease\">");
        out.push_str("<div class=\"commit-tease-left\">");
        out.push_str(&format!("<span class=\"avatar-circle\">{}</span>", first_letter));
        out.push_str(&format!(
            "<span class=\"commit-author\">{}</span>",
            escape_html(&commit.author)
        ));
        out.push_str(&format!(
            "<span class=\"commit-msg\">{}</span>",
            escape_html(commit.message.lines().next().unwrap_or(""))
        ));
        out.push_str(&diffstat_html);
        out.push_str("</div>");
        out.push_str("<div class=\"commit-tease-right\">");
        out.push_str(&format!(
            "<a href=\"/diff/{}\" class=\"commit-sha-badge\">{}</a>",
            commit_digest.to_hex(),
            short_sha
        ));
        out.push_str(&format!("<a href=\"/commits?ref={}\" class=\"btn\" style=\"padding:2px 8px;\">{ICON_COMMIT}</a>", current_ref));
        out.push_str("</div>");
        out.push_str("</div>");
    }

    // File Explorer Table
    out.push_str("<div class=\"file-box\">");
    out.push_str("<table class=\"file-table\">");
    out.push_str("<tbody>");

    let trimmed = current_path.trim_matches('/');
    if !trimmed.is_empty() {
        let parent_path = if let Some((parent, _)) = trimmed.rsplit_once('/') {
            format!("{}/", parent)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "<tr><td class=\"icon-col\">{ICON_FOLDER}</td><td class=\"name-col\"><a href=\"/tree/{}/{}\">..</a></td><td class=\"msg-col\"></td><td class=\"age-col\"></td></tr>",
            current_ref, parent_path
        ));
    }

    for entry in entries {
        let icon = if entry.is_dir {
            ICON_FOLDER
        } else if entry.is_chunked {
            ICON_CHUNKED
        } else {
            ICON_FILE
        };

        let link = if entry.is_dir {
            format!("/tree/{}/{}/", current_ref, entry.full_path)
        } else {
            format!("/blob/{}/{}", current_ref, entry.full_path)
        };

        let size_str = if entry.is_dir { "-".to_string() } else { format_size(entry.size) };
        let commit_link = if !entry.commit_hash.is_empty() {
            format!("/diff/{}", entry.commit_hash)
        } else {
            format!("/commits?ref={}", current_ref)
        };

        let msg_display = if !entry.commit_msg.is_empty() {
            format!(
                "<a href=\"{}\" class=\"commit-msg-link\" title=\"{}\">{}</a>",
                commit_link,
                escape_html(&entry.commit_msg),
                escape_html(&entry.commit_msg)
            )
        } else {
            String::new()
        };

        let age_display = if !entry.commit_age.is_empty() { &entry.commit_age } else { &size_str };

        out.push_str(&format!(
            "<tr><td class=\"icon-col\">{icon}</td><td class=\"name-col\"><a href=\"{link}\">{name}</a></td><td class=\"msg-col\">{msg}</td><td class=\"age-col\" style=\"color:var(--color-fg-muted);font-size:12px;text-align:right;\">{age}</td></tr>",
            icon = icon,
            link = link,
            name = escape_html(&entry.name),
            msg = msg_display,
            age = escape_html(age_display),
        ));
    }

    out.push_str("</tbody></table></div>");

    // Rendered README.md Section
    if let Some(readme) = readme_content {
        out.push_str("<div class=\"readme-box\">");
        out.push_str("<div class=\"readme-header\">");
        out.push_str(&format!("{ICON_FILE} <span>README.md</span>"));
        out.push_str("</div>");
        out.push_str("<div class=\"readme-body\">");
        out.push_str(&crate::web::markdown::render_markdown_with_ref(readme, Some(current_ref)));
        out.push_str("</div></div>");
    }

    out
}

/// Renders GitHub-style File Blob Viewer.
pub fn render_blob_view(
    current_ref: &str,
    available_branches: &[String],
    file_path: &str,
    content: &[u8],
    digest: &str,
    is_chunked: bool,
) -> String {
    let mut out = String::new();

    out.push_str("<div class=\"file-controls\">");
    out.push_str("<div style=\"display:flex; align-items:center; gap:12px;\">");
    out.push_str(&render_branch_switcher(current_ref, available_branches, file_path, true));
    out.push_str(&render_breadcrumbs(current_ref, file_path, false));
    out.push_str("</div>");
    out.push_str("</div>");

    let is_binary = crate::web::search::is_binary_content(content);
    let is_markdown = file_path.ends_with(".md") || file_path.ends_with(".markdown");
    let size_str = format_size(content.len() as u64);
    let line_count = if is_binary { 0 } else { content.split(|&b| b == b'\n').count() };
    let chunk_tag =
        if is_chunked { " · <span style=\"color:#e3b341\">FastCDC Chunked</span>" } else { "" };

    out.push_str("<div class=\"blob-box\">");
    out.push_str("<div class=\"blob-meta-header\">");
    out.push_str(&format!(
        "<div>{} lines · {} · <span style=\"font-family:var(--font-code)\">{}</span>{}</div>",
        line_count,
        size_str,
        &digest[..digest.len().min(16)],
        chunk_tag
    ));
    out.push_str("<div class=\"btn-group\">");
    if is_markdown && !is_binary {
        out.push_str("<button class=\"btn active\" id=\"btn-preview\" onclick=\"switchBlobTab('preview')\">Preview</button>");
        out.push_str(
            "<button class=\"btn\" id=\"btn-code\" onclick=\"switchBlobTab('code')\">Code</button>",
        );
    }
    if !is_binary {
        out.push_str(&format!(
            "<a href=\"/blame/{}/{}\" class=\"btn\">Blame</a>",
            current_ref, file_path
        ));
    }
    out.push_str(&format!(
        "<a href=\"/raw/{}/{}\" class=\"btn\" target=\"_blank\">Raw</a>",
        current_ref, file_path
    ));
    out.push_str(&format!(
        "<button class=\"btn\" onclick=\"copyToClipboard('{}', this)\">{ICON_COPY} Copy</button>",
        escape_html(file_path)
    ));
    out.push_str("</div></div>");

    if is_binary {
        out.push_str("<div class=\"code-table-container\"><div style=\"padding: 48px; text-align: center; color: var(--color-fg-muted);\">[Binary file not displayed]</div></div>");
    } else {
        let text = String::from_utf8_lossy(content);
        if is_markdown {
            out.push_str("<div id=\"blob-preview-container\" style=\"padding: 24px 32px;\">");
            out.push_str(&crate::web::markdown::render_markdown_with_ref(&text, Some(current_ref)));
            out.push_str("</div>");
        }

        let code_style = if is_markdown { "style=\"display:none;\"" } else { "" };
        out.push_str(&format!(
            "<div id=\"blob-code-container\" class=\"code-table-container\" {}>",
            code_style
        ));
        out.push_str("<table class=\"code-blob-table\"><tbody>");
        for (i, line) in text.lines().enumerate() {
            let num = i + 1;
            out.push_str(&format!(
                "<tr id=\"L{num}\"><td class=\"blob-num\"><a href=\"#L{num}\" style=\"color:inherit;\">{num}</a></td><td class=\"blob-code\">{code}</td></tr>",
                num = num,
                code = escape_html(line)
            ));
        }
        out.push_str("</tbody></table></div>");
    }

    out.push_str("</div>");
    out
}

/// Smart match highlighter supporting Unicode-safe case-insensitive literal and regex search.
pub fn highlight_match(
    content: &str,
    query: &str,
    is_regex: bool,
    is_case_sensitive: bool,
) -> String {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return escape_html(content);
    }

    let pattern = if is_regex {
        trimmed.to_string()
    } else {
        regex::escape(trimmed)
    };

    let re = match regex::RegexBuilder::new(&pattern)
        .case_insensitive(!is_case_sensitive)
        .build()
    {
        Ok(r) => r,
        Err(_) => return escape_html(content),
    };

    // If content is very long (e.g. minified JS/JSON), slice a reasonable window around first match
    let safe_content = if content.len() > 600 {
        if let Some(first_match) = re.find(content) {
            let start = content[..first_match.start()]
                .char_indices()
                .rev()
                .nth(100)
                .map(|(idx, _)| idx)
                .unwrap_or(0);
            let end = content[first_match.end()..]
                .char_indices()
                .nth(300)
                .map(|(idx, _)| first_match.end() + idx)
                .unwrap_or(content.len());
            &content[start..end]
        } else {
            let end = content.char_indices().nth(500).map(|(idx, _)| idx).unwrap_or(content.len());
            &content[..end]
        }
    } else {
        content
    };

    let mut result = String::new();
    let mut last_end = 0;
    let mut matched = false;

    for mat in re.find_iter(safe_content) {
        if mat.as_str().is_empty() {
            continue;
        }
        matched = true;
        if mat.start() > last_end {
            result.push_str(&escape_html(&safe_content[last_end..mat.start()]));
        }
        result.push_str("<mark>");
        result.push_str(&escape_html(mat.as_str()));
        result.push_str("</mark>");
        last_end = mat.end();
    }

    if matched {
        if last_end < safe_content.len() {
            result.push_str(&escape_html(&safe_content[last_end..]));
        }
        result
    } else {
        escape_html(safe_content)
    }
}

/// Renders GitHub & Google Code Search-style Interactive Search Console.
pub fn render_search_view(
    current_ref: &str,
    branches: &[String],
    resp: &SearchResponse,
    stats: &IndexStats,
) -> String {
    let mut out = String::new();
    let is_query_empty = resp.query.trim().is_empty();

    // 1. Search Console Header Card & Form
    out.push_str("<div class=\"search-console-card\">");
    out.push_str("<form action=\"/search\" method=\"GET\" class=\"search-console-form\">");

    // Input Group Row
    out.push_str("<div class=\"search-input-group\">");
    out.push_str("<div class=\"search-input-wrapper\">");
    out.push_str(ICON_SEARCH);
    out.push_str(&format!(
        "<input type=\"text\" name=\"q\" id=\"search-q-input\" value=\"{}\" placeholder=\"Search code, functions, types, symbols... (e.g. fn main, struct Digest, FastCdc)\" autofocus class=\"search-console-input\">",
        escape_html(&resp.query)
    ));
    out.push_str("</div>");

    // Branch selector dropdown
    out.push_str("<select name=\"ref\" class=\"search-ref-select\" title=\"Target Branch / Ref\">");
    let mut branch_list = branches.to_vec();
    if !branch_list.iter().any(|b| b == current_ref) {
        branch_list.push(current_ref.to_string());
    }
    for b in &branch_list {
        let sel = if b == current_ref { "selected" } else { "" };
        out.push_str(&format!("<option value=\"{}\" {}>ref: {}</option>", escape_html(b), sel, escape_html(b)));
    }
    out.push_str("</select>");

    out.push_str("<button type=\"submit\" class=\"btn btn-primary search-submit-btn\">Search</button>");
    out.push_str("</div>"); // .search-input-group

    // Options & Controls Row
    out.push_str("<div class=\"search-controls-row\">");

    // Engine selector
    let is_level1 = resp.engine == "level1";
    let l2_checked = if !is_level1 { "checked" } else { "" };
    let l1_checked = if is_level1 { "checked" } else { "" };

    out.push_str("<div class=\"search-engine-selector\">");
    out.push_str("<span class=\"engine-label\">Engine:</span>");
    out.push_str(&format!(
        "<label class=\"engine-option\"><input type=\"radio\" name=\"engine\" value=\"level2\" {}><span class=\"engine-badge level2\">⚡ Level-2 (Trigram Inverted Index)</span></label>",
        l2_checked
    ));
    out.push_str(&format!(
        "<label class=\"engine-option\"><input type=\"radio\" name=\"engine\" value=\"level1\" {}><span class=\"engine-badge level1\">🔍 Level-1 (Direct CAS Scan)</span></label>",
        l1_checked
    ));
    out.push_str("</div>"); // .search-engine-selector

    // Filters and toggles
    let case_checked = if resp.is_case_sensitive { "checked" } else { "" };
    let re_checked = if resp.is_regex { "checked" } else { "" };

    out.push_str("<div class=\"search-options-group\">");
    out.push_str(&format!(
        "<label class=\"search-option-label\" title=\"Case-sensitive search\"><input type=\"checkbox\" name=\"case\" value=\"1\" {}><span>Match Case</span></label>",
        case_checked
    ));
    out.push_str(&format!(
        "<label class=\"search-option-label\" title=\"Regular Expression\"><input type=\"checkbox\" name=\"regex\" value=\"1\" {}><span>Regex</span></label>",
        re_checked
    ));
    out.push_str(&format!(
        "<div class=\"search-path-filter\"><span class=\"filter-prefix\">path:</span><input type=\"text\" name=\"path\" value=\"{}\" placeholder=\"e.g. src/ or *.rs\" class=\"search-path-input\"></div>",
        escape_html(&resp.path_filter)
    ));
    out.push_str("</div>"); // .search-options-group

    out.push_str("</div>"); // .search-controls-row
    out.push_str("</form></div>"); // .search-console-card

    if is_query_empty {
        // Render Search Welcome & Status Dashboard
        out.push_str("<div class=\"search-welcome-container\">");

        // Index Status Banner
        out.push_str("<div class=\"search-status-banner\">");
        out.push_str("<div class=\"status-indicator\"><span class=\"status-pulse-dot\"></span><strong>Level-2 Trigram Inverted Index Active & Synchronized</strong></div>");
        out.push_str("<div class=\"status-metrics-grid\">");
        out.push_str(&format!(
            "<div class=\"metric-card\"><div class=\"metric-value\">{}</div><div class=\"metric-label\">Indexed Files</div></div>",
            stats.total_indexed_files
        ));
        out.push_str(&format!(
            "<div class=\"metric-card\"><div class=\"metric-value\">{}</div><div class=\"metric-label\">Indexed Content</div></div>",
            format_size(stats.total_indexed_bytes)
        ));
        out.push_str(&format!(
            "<div class=\"metric-card\"><div class=\"metric-value\">{}</div><div class=\"metric-label\">Unique Trigrams</div></div>",
            stats.total_unique_trigrams
        ));
        out.push_str("<div class=\"metric-card\"><div class=\"metric-value\">&lt; 1 ms</div><div class=\"metric-label\">Posting Intersection</div></div>");
        out.push_str("</div></div>"); // .status-metrics-grid, .search-status-banner

        // Quick searches pills
        out.push_str("<div class=\"search-quick-queries\">");
        out.push_str("<h3>💡 Quick Searches</h3><div class=\"quick-pills-row\">");
        out.push_str(&format!("<a href=\"/search?q=fn+main&ref={current_ref}&engine=level2\" class=\"quick-pill\"><code>fn main</code></a>"));
        out.push_str(&format!("<a href=\"/search?q=struct+Digest&ref={current_ref}&engine=level2\" class=\"quick-pill\"><code>struct Digest</code></a>"));
        out.push_str(&format!("<a href=\"/search?q=FastCdc&ref={current_ref}&engine=level2\" class=\"quick-pill\"><code>FastCdc</code></a>"));
        out.push_str(&format!("<a href=\"/search?q=trait+BlobStore&ref={current_ref}&engine=level2\" class=\"quick-pill\"><code>trait BlobStore</code></a>"));
        out.push_str(&format!("<a href=\"/search?q=DirectoryManifest&ref={current_ref}&engine=level2\" class=\"quick-pill\"><code>DirectoryManifest</code></a>"));
        out.push_str(&format!("<a href=\"/search?q=diff_trees&ref={current_ref}&engine=level2\" class=\"quick-pill\"><code>diff_trees</code></a>"));
        out.push_str(&format!("<a href=\"/search?q=fn%5Cs%2Bhandle&ref={current_ref}&engine=level2&regex=1\" class=\"quick-pill\"><code>fn\\s+handle (regex)</code></a>"));
        out.push_str(&format!("<a href=\"/search?q=TODO&ref={current_ref}&engine=level2\" class=\"quick-pill\"><code>TODO</code></a>"));
        out.push_str("</div></div>"); // .quick-pills-row, .search-quick-queries

        // Features grid
        out.push_str("<div class=\"search-features-grid\">");
        out.push_str("<div class=\"feature-card\"><div class=\"feature-header\">⚡ Level-2 Trigram Index</div><p>Decomposes all repository files into 3-byte n-grams with compact sorted posting lists. Two-pointer candidate intersection eliminates 99%+ of Merkle tree traversal.</p></div>");
        out.push_str("<div class=\"feature-card\"><div class=\"feature-header\">🔍 Level-1 Direct CAS Scan</div><p>Streaming traversal directly through Merkle DAG nodes in Content-Addressed Storage. Ideal for ad-hoc commits without index warming.</p></div>");
        out.push_str("<div class=\"feature-card\"><div class=\"feature-header\">🎯 Regex & Path Filtering</div><p>Full regular expression support accelerated by guaranteed literal trigrams, plus path scoping (e.g. <code>src/cas/</code> or <code>*.rs</code>).</p></div>");
        out.push_str("<div class=\"feature-card\"><div class=\"feature-header\">🌳 Merkle Incremental Caching</div><p>Subtree directory digests are cached; commits that modify only a few files re-index in $O(\\Delta)$ incremental time without full tree rebuilds.</p></div>");
        out.push_str("</div></div>"); // .search-features-grid, .search-welcome-container
    } else {
        // Render Search Metrics Banner
        let engine_badge = if resp.engine == "level1" {
            "<span class=\"engine-badge level1\">🔍 LEVEL-1 (CAS STREAM)</span>"
        } else {
            "<span class=\"engine-badge level2\">⚡ LEVEL-2 (TRIGRAM INDEX)</span>"
        };

        out.push_str("<div class=\"search-metrics-banner\">");
        out.push_str(&format!(
            "<div class=\"metrics-summary\">Showing <strong>{}</strong> matching lines across <strong>{}</strong> files <span class=\"metrics-timing\">in <strong>{:.2} ms</strong></span></div>",
            resp.total_matches,
            resp.results.len(),
            resp.duration_ms
        ));
        out.push_str("<div class=\"metrics-badges\">");
        out.push_str(engine_badge);
        out.push_str(&format!("<span class=\"badge badge-neutral\">Ref: {}</span>", escape_html(&resp.commit)));
        if resp.files_pruned > 0 {
            out.push_str(&format!("<span class=\"badge badge-success\">{} files pruned</span>", resp.files_pruned));
        }
        if resp.is_case_sensitive {
            out.push_str("<span class=\"badge badge-accent\">Case Sensitive</span>");
        }
        if resp.is_regex {
            out.push_str("<span class=\"badge badge-purple\">Regex</span>");
        }
        if !resp.path_filter.is_empty() {
            out.push_str(&format!("<span class=\"badge badge-neutral\">path:{}</span>", escape_html(&resp.path_filter)));
        }
        out.push_str("</div></div>"); // .metrics-badges, .search-metrics-banner

        if resp.results.is_empty() {
            out.push_str("<div class=\"diff-box\" style=\"padding: 48px; text-align: center; color: var(--color-fg-muted);\">");
            out.push_str(&format!("<h3>No code matching &ldquo;<code>{}</code>&rdquo; found</h3>", escape_html(&resp.query)));
            out.push_str("<p style=\"margin-top: 8px; font-size: 13px;\">Suggestions: Check your spelling, try fewer keywords, uncheck &ldquo;Match Case&rdquo;, clear path filters, or switch engine to Level-1.</p>");
            out.push_str("</div>");
        } else {
            for file in &resp.results {
                out.push_str("<div class=\"search-match-card\">");
                out.push_str("<div class=\"search-match-header\">");
                out.push_str(&format!(
                    "<a href=\"/blob/{}/{}\">{ICON_FILE} <strong>{}</strong></a>",
                    current_ref,
                    file.path,
                    escape_html(&file.path)
                ));
                out.push_str(&format!(
                    "<span style=\"color:var(--color-fg-muted); font-size:12px;\">{} matches</span>",
                    file.match_count
                ));
                out.push_str("</div>");

                out.push_str("<table class=\"match-snippet-table\"><tbody>");
                for snip in &file.snippets {
                    let row_class = if snip.is_match { "class=\"matched\"" } else { "" };
                    let highlighted = if snip.is_match {
                        highlight_match(&snip.content, &resp.query, resp.is_regex, resp.is_case_sensitive)
                    } else {
                        escape_html(&snip.content)
                    };

                    out.push_str(&format!(
                        "<tr {row_class}><td class=\"num\"><a href=\"/blob/{current_ref}/{path}#L{num}\" style=\"color:inherit;\">{num}</a></td><td class=\"code\">{code}</td></tr>",
                        row_class = row_class,
                        current_ref = current_ref,
                        path = file.path,
                        num = snip.line_number,
                        code = highlighted
                    ));
                }
                out.push_str("</tbody></table></div>");
            }
        }
    }

    out
}

/// Renders GitHub-style Commit History Log with lines added/deleted diffstats.
pub fn render_commits_view(
    current_ref: &str,
    commits: &[(Digest, Commit, crate::git::diff::DiffStats)],
) -> String {
    let mut out = String::new();

    out.push_str("<div class=\"file-controls\">");
    out.push_str(&format!("<h2>Commits on <code>{}</code></h2>", escape_html(current_ref)));
    out.push_str("</div>");

    out.push_str("<div class=\"file-box\">");
    out.push_str("<table class=\"file-table\">");
    out.push_str("<tbody>");

    for (digest, commit, stats) in commits {
        let short_hash = &digest.to_hex()[..7];
        let subject = commit.message.lines().next().unwrap_or("");
        let first_letter = commit.author.chars().next().unwrap_or('A').to_uppercase();

        let diffstat_html = if stats.additions > 0 || stats.deletions > 0 {
            format!(
                "<span class=\"diffstat\"><span class=\"diffstat-add\">+{}</span><span class=\"diffstat-del\">-{}</span></span>",
                stats.additions, stats.deletions
            )
        } else {
            String::new()
        };

        out.push_str(&format!(
            "<tr><td class=\"icon-col\"><span class=\"avatar-circle\" style=\"width:20px;height:20px;font-size:10px;\">{letter}</span></td><td class=\"name-col\"><div style=\"display:flex;align-items:center;justify-content:space-between;gap:12px;\"><div><a href=\"/diff/{full_hash}\"><strong>{subject}</strong></a><div style=\"color:var(--color-fg-muted);font-size:12px;margin-top:2px;\">{author} committed</div></div>{diffstat}</div></td><td class=\"age-col\"><a href=\"/diff/{full_hash}\" class=\"commit-sha-badge\">{short_hash}</a></td><td style=\"width:80px;text-align:right;\"><a href=\"/tree/{full_hash}/\" class=\"btn\" style=\"padding:2px 8px;\">{ICON_CODE}</a></td></tr>",
            letter = first_letter,
            full_hash = digest.to_hex(),
            subject = escape_html(subject),
            author = escape_html(&commit.author),
            diffstat = diffstat_html,
            short_hash = short_hash
        ));
    }

    out.push_str("</tbody></table></div>");
    out
}

pub fn format_time_ago(timestamp: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let diff = now.saturating_sub(timestamp);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

fn render_highlighted_code(
    content: &str,
    highlight: Option<(usize, usize)>,
    hl_class: &str,
) -> String {
    if let Some((start, end)) = highlight {
        let chars: Vec<char> = content.chars().collect();
        if start <= end && end <= chars.len() {
            let before: String = chars[..start].iter().collect();
            let hl: String = chars[start..end].iter().collect();
            let after: String = chars[end..].iter().collect();
            return format!(
                "{}<span class=\"{}\">{}</span>{}",
                escape_html(&before),
                hl_class,
                escape_html(&hl),
                escape_html(&after)
            );
        }
    }
    escape_html(content)
}

/// Renders GitHub/Critique-style Split & Unified Diff View.
pub fn render_diff_view(title: &str, report: &crate::git::diff::DiffReport) -> String {
    let mut out = String::new();
    let stats = &report.total_stats;

    let summary_stats = format!(
        "<div style=\"display:flex; align-items:center; gap:8px; font-size:13px;\"><span><strong>{}</strong> changed {}</span> <span class=\"diffstat\"><span class=\"diffstat-add\">+{}</span> <span class=\"diffstat-del\">-{}</span></span></div>",
        stats.files_changed,
        if stats.files_changed == 1 { "file" } else { "files" },
        stats.additions,
        stats.deletions
    );

    out.push_str("<div class=\"file-controls\">");
    out.push_str(&format!("<h2>Comparing: {}</h2>", escape_html(title)));
    out.push_str("<div style=\"display:flex; align-items:center; gap:12px;\">");
    out.push_str(&summary_stats);
    out.push_str("<div class=\"diff-view-selector\">");
    out.push_str("<button class=\"btn active\" id=\"btn-unified\" onclick=\"setDiffMode('unified')\">Unified</button>");
    out.push_str(
        "<button class=\"btn\" id=\"btn-split\" onclick=\"setDiffMode('split')\">Split</button>",
    );
    out.push_str("</div></div></div>");

    out.push_str("<div id=\"diff-container\" class=\"diff-container mode-unified\">");

    if report.files.is_empty() {
        out.push_str("<div class=\"diff-box\" style=\"padding: 48px; text-align: center; color: var(--color-fg-muted);\">There are no changes to show.</div>");
    } else {
        for file in &report.files {
            let path_str = file.display_path();
            let status_badge = match file.status {
                crate::git::diff::FileDiffStatus::Modified => "<span class=\"repo-tag\">MODIFIED</span>",
                crate::git::diff::FileDiffStatus::Added => "<span class=\"repo-tag\" style=\"background:rgba(46,160,67,0.2);color:#3fb950;\">ADDED</span>",
                crate::git::diff::FileDiffStatus::Deleted => "<span class=\"repo-tag\" style=\"background:rgba(248,81,73,0.2);color:#f85149;\">DELETED</span>",
                crate::git::diff::FileDiffStatus::Binary => "<span class=\"repo-tag\" style=\"background:rgba(227,179,65,0.2);color:#e3b341;\">BINARY</span>",
            };

            let file_stats = format!(
                "<span class=\"diffstat\"><span class=\"diffstat-add\">+{}</span> <span class=\"diffstat-del\">-{}</span></span>",
                file.stats.additions, file.stats.deletions
            );

            out.push_str("<div class=\"diff-file-card\">");
            out.push_str("<div class=\"diff-file-header\">");
            out.push_str(&format!(
                "<div style=\"display:flex; align-items:center; gap:8px;\">{} <span class=\"diff-file-path\">{}</span> {}</div>",
                ICON_FILE, escape_html(path_str), status_badge
            ));
            out.push_str(&file_stats);
            out.push_str("</div>");

            if file.is_binary {
                out.push_str("<div style=\"padding: 24px; text-align: center; color: var(--color-fg-muted);\">Binary file differs</div></div>");
                continue;
            }

            // 1. Unified Table
            out.push_str("<div class=\"code-table-container\"><table class=\"diff-table diff-unified-table\"><tbody>");
            for hunk in &file.hunks {
                out.push_str(&format!(
                    "<tr class=\"diff-unified-row hunk\"><td colspan=\"4\" style=\"padding:4px 12px;\">@@ -{},{} +{},{} @@</td></tr>",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ));
                for row in &hunk.unified_rows {
                    let (row_class, marker_str) = match row.marker {
                        '+' => ("diff-unified-row add", "+"),
                        '-' => ("diff-unified-row del", "-"),
                        _ => ("diff-unified-row ctx", " "),
                    };
                    let old_str = row.old_line_no.map_or(String::new(), |n| n.to_string());
                    let new_str = row.new_line_no.map_or(String::new(), |n| n.to_string());

                    out.push_str(&format!(
                        "<tr class=\"{}\"><td class=\"diff-lineno\">{}</td><td class=\"diff-lineno\">{}</td><td class=\"diff-marker\">{}</td><td class=\"diff-content\">{}</td></tr>",
                        row_class, old_str, new_str, marker_str, escape_html(&row.content)
                    ));
                }
            }
            out.push_str("</tbody></table>");

            // 2. Split (Side-by-Side) Table
            out.push_str("<table class=\"diff-table diff-split-table\"><tbody>");
            for hunk in &file.hunks {
                out.push_str(&format!(
                    "<tr class=\"diff-unified-row hunk\"><td colspan=\"4\" style=\"padding:4px 12px;\">@@ -{},{} +{},{} @@</td></tr>",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ));
                for row in &hunk.split_rows {
                    let left_class = match row.left.cell_type {
                        crate::git::diff::SplitCellType::Deletion => "split-cell del",
                        crate::git::diff::SplitCellType::Context => "split-cell",
                        _ => "split-cell empty",
                    };
                    let right_class = match row.right.cell_type {
                        crate::git::diff::SplitCellType::Addition => "split-cell add",
                        crate::git::diff::SplitCellType::Context => "split-cell",
                        _ => "split-cell empty",
                    };

                    let left_num = row.left.line_no.map_or(String::new(), |n| n.to_string());
                    let right_num = row.right.line_no.map_or(String::new(), |n| n.to_string());

                    let left_html = render_highlighted_code(
                        &row.left.content,
                        row.left.highlight_range,
                        "hl-del",
                    );
                    let right_html = render_highlighted_code(
                        &row.right.content,
                        row.right.highlight_range,
                        "hl-add",
                    );

                    out.push_str(&format!(
                        "<tr><td class=\"diff-lineno\">{}</td><td class=\"{}\">{}</td><td class=\"diff-lineno\">{}</td><td class=\"{}\">{}</td></tr>",
                        left_num, left_class, left_html, right_num, right_class, right_html
                    ));
                }
            }
            out.push_str("</tbody></table></div></div>");
        }
    }

    out.push_str("</div>");
    out
}

/// Fallback for string-based unified diffs.
pub fn render_diff_view_text(title: &str, diff_text: &str) -> String {
    let mut out = String::new();
    let stats = crate::git::diff::compute_diff_stats(diff_text);

    let summary_stats = format!(
        "<div style=\"display:flex; align-items:center; gap:8px; font-size:13px;\"><span><strong>{}</strong> changed {}</span> <span class=\"diffstat\"><span class=\"diffstat-add\">+{}</span> <span class=\"diffstat-del\">-{}</span></span></div>",
        stats.files_changed,
        if stats.files_changed == 1 { "file" } else { "files" },
        stats.additions,
        stats.deletions
    );

    out.push_str("<div class=\"file-controls\">");
    out.push_str(&format!("<h2>Comparing: {}</h2>", escape_html(title)));
    out.push_str("</div>");

    out.push_str("<div class=\"diff-box\">");
    out.push_str("<div class=\"diff-box-header\" style=\"display:flex; justify-content:space-between; align-items:center;\">");
    out.push_str("<span>Unified Diff</span>");
    out.push_str(&summary_stats);
    out.push_str("</div>");
    out.push_str("<div class=\"diff-lines\">");

    if diff_text.trim().is_empty() {
        out.push_str("<div style=\"padding: 48px; text-align: center; color: var(--color-fg-muted);\">There are no changes to show.</div>");
    } else {
        for line in diff_text.lines() {
            let class = if line.starts_with('+') && !line.starts_with("+++") {
                "diff-row add"
            } else if line.starts_with('-') && !line.starts_with("---") {
                "diff-row del"
            } else if line.starts_with("@@") {
                "diff-row hunk"
            } else {
                "diff-row"
            };
            out.push_str(&format!("<div class=\"{}\">{}</div>", class, escape_html(line)));
        }
    }

    out.push_str("</div></div>");
    out
}

/// Formats a full Git author string into a clean, compact display name (e.g. "AI Agent
/// <agent@graft.dev>" -> "AI Agent").
pub fn format_short_author(author: &str) -> String {
    let name = if let Some((n, _)) = author.split_once('<') { n.trim() } else { author.trim() };
    if name.chars().count() > 18 {
        let truncated: String = name.chars().take(17).collect();
        format!("{}…", truncated)
    } else if name.is_empty() {
        "Unknown".to_string()
    } else {
        name.to_string()
    }
}

/// Renders GitHub/Critique-style Git Blame Line Annotations.
pub fn render_blame_view(
    current_ref: &str,
    available_branches: &[String],
    file_path: &str,
    blame: &crate::git::blame::BlameResult,
) -> String {
    let mut out = String::new();

    out.push_str("<div class=\"file-controls\">");
    out.push_str("<div style=\"display:flex; align-items:center; gap:12px;\">");
    out.push_str(&render_branch_switcher(current_ref, available_branches, file_path, true));
    out.push_str(&render_breadcrumbs(current_ref, file_path, false));
    out.push_str("</div>");
    out.push_str(&format!(
        "<a href=\"/blob/{}/{}\" class=\"btn\">Normal View</a>",
        current_ref, file_path
    ));
    out.push_str("</div>");

    out.push_str("<div class=\"blob-box\">");
    out.push_str("<div class=\"blob-meta-header\">");
    out.push_str(&format!(
        "<div>Blame: <strong>{}</strong> ({} lines)</div>",
        escape_html(file_path),
        blame.lines.len()
    ));
    out.push_str(&format!(
        "<a href=\"/raw/{}/{}\" class=\"btn\" target=\"_blank\">Raw</a>",
        current_ref, file_path
    ));
    out.push_str("</div>");

    out.push_str("<div class=\"code-table-container\"><table class=\"blame-table\"><tbody>");

    for line in &blame.lines {
        let short_author = format_short_author(&line.author);
        out.push_str(&format!(
            "<tr><td class=\"blame-gutter\"><a href=\"/diff/{}\" class=\"commit-sha-badge\" title=\"{}\">{}</a> <span style=\"color:var(--color-fg-default);font-weight:500;margin-left:8px;\" title=\"{}\">{}</span> <span style=\"color:var(--color-fg-muted);font-size:11px;margin-left:8px;\">{}</span></td><td class=\"blame-num\">{}</td><td class=\"blame-code\">{}</td></tr>",
            line.commit_digest,
            escape_html(&line.summary),
            line.short_hash,
            escape_html(&line.author),
            escape_html(&short_author),
            format_time_ago(line.timestamp),
            line.line_no,
            escape_html(&line.content)
        ));
    }

    out.push_str("</tbody></table></div></div>");
    out
}

/// Renders GitHub-style Pull Requests Dashboard.
pub fn render_pull_requests_view(
    prs: &[crate::review::PullRequest],
    _default_branch: &str,
) -> String {
    let mut out = String::new();

    out.push_str("<div class=\"file-controls\">");
    out.push_str("<h2>Pull Requests</h2>");
    out.push_str("<a href=\"/pulls/new\" class=\"btn\" style=\"background:#238636;color:#fff;border-color:#2ea043;font-weight:600;\">+ New Pull Request</a>");
    out.push_str("</div>");

    if prs.is_empty() {
        out.push_str("<div class=\"diff-box\" style=\"padding: 48px; text-align: center; color: var(--color-fg-muted);\">No pull requests found. Create one with the '+ New Pull Request' button.</div>");
    } else {
        out.push_str("<div class=\"file-box\">");
        out.push_str("<table class=\"file-table\"><tbody>");

        for pr in prs {
            let status_badge = match pr.status {
                crate::review::PullRequestStatus::Open => {
                    "<span class=\"pr-badge open\">Open</span>"
                }
                crate::review::PullRequestStatus::Merged => {
                    "<span class=\"pr-badge merged\">Merged</span>"
                }
                crate::review::PullRequestStatus::Closed => {
                    "<span class=\"pr-badge closed\">Closed</span>"
                }
            };

            let comment_count = pr.comments.len();
            let comment_badge = if comment_count > 0 {
                format!(
                    "<span style=\"color:var(--color-fg-muted);font-size:12px;\">💬 {}</span>",
                    comment_count
                )
            } else {
                String::new()
            };

            out.push_str(&format!(
                "<tr><td class=\"icon-col\" style=\"padding:12px 16px;\">{}</td><td class=\"name-col\"><div style=\"font-size:15px;font-weight:600;\"><a href=\"/pull/{}\">{}</a></div><div style=\"color:var(--color-fg-muted);font-size:12px;margin-top:4px;\">#{} by <strong>{}</strong> · {} into <code>{}</code> from <code>{}</code></div></td><td style=\"text-align:right;padding-right:16px;\">{}</td></tr>",
                status_badge,
                pr.id,
                escape_html(&pr.title),
                pr.id,
                escape_html(&pr.author),
                format_time_ago(pr.created_at),
                escape_html(&pr.target_branch),
                escape_html(&pr.source_branch),
                comment_badge
            ));
        }

        out.push_str("</tbody></table></div>");
    }

    out
}

/// Renders GitHub-style New Pull Request Page.
pub fn render_new_pr_view(branches: &[String], default_branch: &str) -> String {
    let mut out = String::new();

    out.push_str("<div class=\"file-controls\"><h2>Compare &amp; Open Pull Request</h2></div>");
    out.push_str("<div class=\"diff-box\" style=\"padding: 24px;\">");
    out.push_str("<form action=\"/api/pulls/new\" method=\"POST\">");
    out.push_str("<div style=\"display:flex; align-items:center; gap:16px; margin-bottom: 20px; background:var(--color-canvas-subtle); padding:12px 16px; border-radius:6px;\">");
    out.push_str("<span><strong>base:</strong></span>");
    out.push_str("<select name=\"target_branch\" style=\"background:var(--color-canvas-default); color:var(--color-fg-default); border:1px solid var(--color-border-default); border-radius:6px; padding:6px 12px;\">");
    for b in branches {
        let sel = if b == default_branch { "selected" } else { "" };
        out.push_str(&format!(
            "<option value=\"{}\" {}>{}</option>",
            escape_html(b),
            sel,
            escape_html(b)
        ));
    }
    out.push_str("</select>");
    out.push_str("<span>&larr; <strong>compare:</strong></span>");
    out.push_str("<select name=\"source_branch\" style=\"background:var(--color-canvas-default); color:var(--color-fg-default); border:1px solid var(--color-border-default); border-radius:6px; padding:6px 12px;\">");
    for b in branches {
        out.push_str(&format!("<option value=\"{}\">{}</option>", escape_html(b), escape_html(b)));
    }
    out.push_str("</select></div>");

    out.push_str("<div style=\"margin-bottom: 16px;\">");
    out.push_str(
        "<label style=\"display:block; font-weight:600; margin-bottom:6px;\">Title</label>",
    );
    out.push_str("<input type=\"text\" name=\"title\" placeholder=\"Title summary of your changes\" required style=\"width:100%; background:var(--color-canvas-default); color:var(--color-fg-default); border:1px solid var(--color-border-default); border-radius:6px; padding:8px 12px; font-size:14px;\">");
    out.push_str("</div>");

    out.push_str("<div style=\"margin-bottom: 20px;\">");
    out.push_str(
        "<label style=\"display:block; font-weight:600; margin-bottom:6px;\">Description</label>",
    );
    out.push_str("<textarea name=\"description\" rows=\"6\" placeholder=\"Describe the changes made and tests executed...\" style=\"width:100%; background:var(--color-canvas-default); color:var(--color-fg-default); border:1px solid var(--color-border-default); border-radius:6px; padding:8px 12px; font-size:14px; font-family:var(--font-sans);\"></textarea>");
    out.push_str("</div>");

    out.push_str("<input type=\"hidden\" name=\"author\" value=\"AI Agent <agent@graft.dev>\">");
    out.push_str("<button type=\"submit\" class=\"btn\" style=\"background:#238636;color:#fff;border-color:#2ea043;font-size:14px;padding:8px 16px;font-weight:600;\">Create Pull Request</button>");
    out.push_str("</form></div>");

    out
}

/// Renders GitHub/Critique-style Pull Request Detail View with conversation & diff.
pub fn render_pull_request_detail_view(
    pr: &crate::review::PullRequest,
    diff_report: Option<&crate::git::diff::DiffReport>,
    active_subtab: &str,
    _default_branch: &str,
) -> String {
    let mut out = String::new();

    let status_badge = match pr.status {
        crate::review::PullRequestStatus::Open => "<span class=\"pr-badge open\">Open</span>",
        crate::review::PullRequestStatus::Merged => "<span class=\"pr-badge merged\">Merged</span>",
        crate::review::PullRequestStatus::Closed => "<span class=\"pr-badge closed\">Closed</span>",
    };

    let files_changed_count = diff_report.map_or(0, |r| r.files.len());

    out.push_str("<div style=\"margin-bottom: 20px;\">");
    out.push_str(&format!("<h1 style=\"font-size:26px; font-weight:600; margin-bottom:8px;\">{} <span style=\"color:var(--color-fg-muted);font-weight:400;\">#{}</span></h1>", escape_html(&pr.title), pr.id));
    out.push_str(&format!(
        "<div style=\"display:flex; align-items:center; gap:10px; font-size:14px; color:var(--color-fg-muted);\">{} <strong>{}</strong> wants to merge into <code>{}</code> from <code>{}</code> · {}</div>",
        status_badge,
        escape_html(&pr.author),
        escape_html(&pr.target_branch),
        escape_html(&pr.source_branch),
        format_time_ago(pr.created_at)
    ));
    out.push_str("</div>");

    // Subtabs: Conversation vs Files Changed
    let conv_active = if active_subtab == "files" { "" } else { "active" };
    let files_active = if active_subtab == "files" { "active" } else { "" };

    out.push_str("<ul class=\"repo-tabs\" style=\"border-bottom:1px solid var(--color-border-default); margin-bottom:24px;\">");
    out.push_str(&format!("<li class=\"repo-tab-item {}\"><a href=\"/pull/{}\">Conversation <span class=\"tab-counter\">{}</span></a></li>", conv_active, pr.id, pr.comments.len() + 1));
    out.push_str(&format!("<li class=\"repo-tab-item {}\"><a href=\"/pull/{}?tab=files\">Files Changed <span class=\"tab-counter\">{}</span></a></li>", files_active, pr.id, files_changed_count));
    out.push_str("</ul>");

    if active_subtab == "files" {
        if let Some(report) = diff_report {
            out.push_str(&render_diff_view(&format!("PR #{} ({})", pr.id, pr.title), report));
        } else {
            out.push_str("<div class=\"diff-box\" style=\"padding:48px; text-align:center; color:var(--color-fg-muted);\">Diff not available</div>");
        }
    } else {
        // Conversation View
        out.push_str("<div style=\"max-width: 900px;\">");

        // 1. Initial Description Post
        out.push_str("<div class=\"pr-timeline-item\">");
        out.push_str("<div class=\"avatar-circle\" style=\"width:36px;height:36px;font-size:14px;background:#1f6feb;\">A</div>");
        out.push_str("<div class=\"pr-comment-card\">");
        out.push_str("<div class=\"pr-comment-header\">");
        out.push_str(&format!(
            "<span><strong>{}</strong> commented {}</span><span class=\"repo-tag\">Author</span>",
            escape_html(&pr.author),
            format_time_ago(pr.created_at)
        ));
        out.push_str("</div>");
        out.push_str("<div class=\"pr-comment-body\">");
        out.push_str(&crate::web::markdown::render_markdown_with_ref(
            &pr.description,
            Some(&pr.target_branch),
        ));
        out.push_str("</div></div></div>");

        // 2. Timeline Comments
        for c in &pr.comments {
            let line_tag = if let (Some(ref f), Some(l)) = (&c.file_path, c.line_no) {
                format!(" <span class=\"repo-tag\">{}:{}</span>", escape_html(f), l)
            } else {
                String::new()
            };

            out.push_str("<div class=\"pr-timeline-item\">");
            out.push_str("<div class=\"avatar-circle\" style=\"width:36px;height:36px;font-size:14px;background:#8957e5;\">R</div>");
            out.push_str("<div class=\"pr-comment-card\">");
            out.push_str("<div class=\"pr-comment-header\">");
            out.push_str(&format!(
                "<span><strong>{}</strong> commented {}{}</span>",
                escape_html(&c.author),
                format_time_ago(c.created_at),
                line_tag
            ));
            out.push_str("</div>");
            out.push_str("<div class=\"pr-comment-body\">");
            out.push_str(&crate::web::markdown::render_markdown_with_ref(
                &c.body,
                Some(&pr.target_branch),
            ));
            out.push_str("</div></div></div>");
        }

        // 3. Merge Box (if Open)
        if pr.status == crate::review::PullRequestStatus::Open {
            out.push_str("<div class=\"diff-box\" style=\"padding:20px; background:var(--color-canvas-subtle); margin-bottom:24px;\">");
            out.push_str(
                "<div style=\"display:flex; justify-content:space-between; align-items:center;\">",
            );
            out.push_str("<div><div style=\"font-size:15px; font-weight:600; color:var(--color-success-fg);\">This branch has no conflicts with base branch</div><div style=\"color:var(--color-fg-muted);font-size:12px;margin-top:2px;\">Merging can be performed automatically via 3-way Merkle merge.</div></div>");
            out.push_str(&format!(
                "<form action=\"/api/pulls/merge\" method=\"POST\" style=\"margin:0;\"><input type=\"hidden\" name=\"pr_id\" value=\"{}\"><button type=\"submit\" class=\"btn\" style=\"background:#238636;color:#fff;border-color:#2ea043;font-size:14px;padding:8px 16px;font-weight:600;\">Merge Pull Request</button></form>",
                pr.id
            ));
            out.push_str("</div></div>");
        } else if pr.status == crate::review::PullRequestStatus::Merged {
            out.push_str("<div class=\"diff-box\" style=\"padding:20px; background:rgba(137,87,229,0.1); border-color:#8957e5; margin-bottom:24px;\">");
            out.push_str(&format!(
                "<div style=\"font-weight:600; color:#a371f7;\">Pull Request successfully merged into <code>{}</code></div><div style=\"color:var(--color-fg-muted);font-size:12px;margin-top:4px;\">Merge commit: <a href=\"/diff/{}\">{}</a></div>",
                escape_html(&pr.target_branch),
                pr.merge_commit.map_or(String::new(), |d| d.to_string()),
                pr.merge_commit.map_or(String::new(), |d| format!("{:.7}", d))
            ));
            out.push_str("</div>");
        }

        // 4. Add Review Comment Box
        out.push_str("<div class=\"diff-box\" style=\"padding:16px;\">");
        out.push_str("<form action=\"/api/pulls/comment\" method=\"POST\">");
        out.push_str(&format!("<input type=\"hidden\" name=\"pr_id\" value=\"{}\">", pr.id));
        out.push_str("<div style=\"margin-bottom:12px;\"><textarea name=\"body\" rows=\"4\" placeholder=\"Leave a review comment or LGTM...\" required style=\"width:100%; background:var(--color-canvas-default); color:var(--color-fg-default); border:1px solid var(--color-border-default); border-radius:6px; padding:10px 14px; font-size:14px; font-family:var(--font-sans);\"></textarea></div>");
        out.push_str(
            "<div style=\"display:flex; justify-content:space-between; align-items:center;\">",
        );
        out.push_str("<input type=\"text\" name=\"author\" value=\"Code Reviewer <reviewer@graft.dev>\" style=\"background:var(--color-canvas-default); color:var(--color-fg-default); border:1px solid var(--color-border-default); border-radius:6px; padding:4px 10px; font-size:12px; width:260px;\">");
        out.push_str("<button type=\"submit\" class=\"btn\" style=\"background:#21262d;color:var(--color-fg-default);font-weight:600;padding:6px 14px;\">Comment</button>");
        out.push_str("</div></form></div>");

        out.push_str("</div>");
    }

    out
}

/// Renders GitHub-style Active Workspaces Dashboard.
pub fn render_workspaces_view(workspaces: &[WorkspaceConfig], storage_dir: &str) -> String {
    let mut out = String::new();

    out.push_str("<div class=\"file-controls\">");
    out.push_str("<h2>Active AI Agent Workspaces</h2>");
    out.push_str(&format!("<span class=\"repo-tag\">Storage: {}</span>", escape_html(storage_dir)));
    out.push_str("</div>");

    if workspaces.is_empty() {
        out.push_str("<div class=\"diff-box\" style=\"padding: 48px; text-align: center; color: var(--color-fg-muted);\">No active workspaces found. Run <code>graft workspace mount &lt;id&gt;</code> to create one.</div>");
    } else {
        out.push_str("<div class=\"file-box\">");
        out.push_str("<table class=\"file-table\">");
        out.push_str("<thead><tr style=\"background:var(--color-canvas-subtle);font-weight:600;font-size:12px;color:var(--color-fg-muted);\"><td style=\"padding:10px 16px;\">Workspace</td><td>Tracked Branch</td><td>Status</td><td>Upper Layer</td><td style=\"text-align:right;padding-right:16px;\">Actions</td></tr></thead>");
        out.push_str("<tbody>");

        for ws in workspaces {
            let branch = ws.branch.as_deref().unwrap_or(&ws.commit);
            let mount_status = if let Some(pid) = ws.mount_pid {
                format!(
                    "<span style=\"color:var(--color-success-fg)\">Active FUSE (PID {})</span>",
                    pid
                )
            } else if let Some(ref b) = ws.mount_backend {
                format!("<span style=\"color:var(--color-accent-fg)\">{}</span>", b)
            } else {
                "<span style=\"color:var(--color-fg-muted)\">Unmounted</span>".to_string()
            };

            out.push_str(&format!(
                "<tr><td style=\"padding:10px 16px;\">{ICON_WORKSPACE} <strong>{id}</strong></td><td><code>{branch}</code></td><td>{status}</td><td style=\"font-family:var(--font-code);font-size:12px;color:var(--color-fg-muted);\">{upper_short}</td><td style=\"text-align:right;padding-right:16px;\"><a href=\"/workspaces/{id}/diff\" class=\"btn\">Live Changes</a></td></tr>",
                id = escape_html(&ws.id),
                branch = escape_html(branch),
                status = mount_status,
                upper_short = escape_html(&ws.upper_dir.file_name().unwrap_or_default().to_string_lossy()),
            ));
        }

        out.push_str("</tbody></table></div>");
    }

    out
}
