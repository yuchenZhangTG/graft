//! GitHub Flavored Markdown (GFM) renderer for Graft Web UI.
//!
//! Converts Markdown documents (such as `README.md` or `.md` files) into
//! clean, semantic, GitHub Primer-styled HTML without external runtime dependencies.

use crate::web::templates::escape_html;

/// Renders a full Markdown document into GitHub Primer-styled HTML.
pub fn render_markdown(input: &str) -> String {
    render_markdown_with_ref(input, None)
}

/// Renders a Markdown document, resolving relative repository links against `current_ref`.
pub fn render_markdown_with_ref(input: &str, current_ref: Option<&str>) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    out.push_str("<div class=\"markdown-body\">\n");

    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // 1. Empty line
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // 2. Fenced Code Block: ``` or ~~~
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = if trimmed.starts_with("```") { "```" } else { "~~~" };
            let lang = trimmed.trim_start_matches(marker).trim();
            i += 1;
            let mut code_lines = Vec::new();
            while i < lines.len() {
                if lines[i].trim().starts_with(marker) {
                    i += 1;
                    break;
                }
                code_lines.push(lines[i]);
                i += 1;
            }
            if lang.eq_ignore_ascii_case("mermaid") {
                out.push_str("<div class=\"mermaid-container\"><pre class=\"mermaid\">\n");
                for (idx, cl) in code_lines.iter().enumerate() {
                    out.push_str(&escape_html(cl));
                    if idx + 1 < code_lines.len() {
                        out.push('\n');
                    }
                }
                out.push_str("\n</pre></div>\n");
                continue;
            }
            out.push_str("<div class=\"highlight\"><pre><code");
            if !lang.is_empty() {
                out.push_str(&format!(" class=\"language-{}\"", escape_html(lang)));
            }
            out.push('>');
            for (idx, cl) in code_lines.iter().enumerate() {
                out.push_str(&escape_html(cl));
                if idx + 1 < code_lines.len() {
                    out.push('\n');
                }
            }
            out.push_str("</code></pre></div>\n");
            continue;
        }

        // 3. Headings: # H1 ... ###### H6
        if trimmed.starts_with('#') {
            let mut level = 0;
            for ch in trimmed.chars() {
                if ch == '#' {
                    level += 1;
                } else {
                    break;
                }
            }
            if level <= 6 && trimmed[level..].starts_with(' ') {
                let heading_text = trimmed[level..].trim();
                let slug = generate_slug(heading_text);
                let rendered_content = render_inline_with_ref(heading_text, current_ref);
                out.push_str(&format!(
                    "<h{level} id=\"{slug}\"><a class=\"anchor\" href=\"#{slug}\">#</a> {rendered_content}</h{level}>\n",
                    level = level,
                    slug = slug,
                    rendered_content = rendered_content
                ));
                i += 1;
                continue;
            }
        }

        // 4. Horizontal Rule: ---, ***, ___
        if is_horizontal_rule(trimmed) {
            out.push_str("<hr />\n");
            i += 1;
            continue;
        }

        // 5. Blockquotes & GitHub Alerts (> [!NOTE], > [!TIP], etc.)
        if trimmed.starts_with('>') {
            let mut quote_lines = Vec::new();
            while i < lines.len() {
                let l_trimmed = lines[i].trim();
                if l_trimmed.starts_with('>') {
                    let content = l_trimmed.strip_prefix('>').unwrap_or("");
                    let content = content.strip_prefix(' ').unwrap_or(content);
                    quote_lines.push(content);
                    i += 1;
                } else if !l_trimmed.is_empty() && !quote_lines.is_empty() {
                    // Lazy continuation line
                    quote_lines.push(l_trimmed);
                    i += 1;
                } else {
                    break;
                }
            }

            if let Some(alert_type) = detect_alert(&quote_lines) {
                render_github_alert(&mut out, &alert_type, &quote_lines[1..], current_ref);
            } else {
                out.push_str("<blockquote>\n");
                let quote_body = quote_lines.join("\n");
                // Parse nested markdown inside quote block
                let rendered_quote = render_markdown_inner_with_ref(&quote_body, current_ref);
                out.push_str(&rendered_quote);
                out.push_str("</blockquote>\n");
            }
            continue;
        }

        // 6. Tables: GFM Table detection (| ... |)
        if trimmed.contains('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1].trim()) {
            let header_line = trimmed;
            let sep_line = lines[i + 1].trim();
            i += 2;

            let alignments = parse_table_alignments(sep_line);
            let headers = parse_table_row(header_line);

            out.push_str("<table class=\"markdown-table\">\n<thead>\n<tr>\n");
            for (col_idx, h) in headers.iter().enumerate() {
                let align_attr = get_align_attr(alignments.get(col_idx).copied());
                out.push_str(&format!(
                    "<th{}>{}</th>\n",
                    align_attr,
                    render_inline_with_ref(h, current_ref)
                ));
            }
            out.push_str("</tr>\n</thead>\n<tbody>\n");

            while i < lines.len() {
                let row_line = lines[i].trim();
                if !row_line.contains('|') || row_line.is_empty() {
                    break;
                }
                let cells = parse_table_row(row_line);
                out.push_str("<tr>\n");
                for (col_idx, cell) in cells.iter().enumerate() {
                    let align_attr = get_align_attr(alignments.get(col_idx).copied());
                    out.push_str(&format!(
                        "<td{}>{}</td>\n",
                        align_attr,
                        render_inline_with_ref(cell, current_ref)
                    ));
                }
                // Pad missing cells if row has fewer columns
                if cells.len() < headers.len() {
                    for col_idx in cells.len()..headers.len() {
                        let align_attr = get_align_attr(alignments.get(col_idx).copied());
                        out.push_str(&format!("<td{}></td>\n", align_attr));
                    }
                }
                out.push_str("</tr>\n");
                i += 1;
            }
            out.push_str("</tbody>\n</table>\n");
            continue;
        }

        // 7. Lists: Unordered (- / * / +) and Ordered (1. / 2.)
        if is_list_item(trimmed) {
            i = render_list_block(&lines, i, &mut out, current_ref);
            continue;
        }

        // 8. Paragraph: Collect consecutive non-empty lines
        let mut para_lines = Vec::new();
        while i < lines.len() {
            let cur = lines[i].trim();
            if cur.is_empty()
                || cur.starts_with('#')
                || cur.starts_with("```")
                || cur.starts_with("~~~")
                || cur.starts_with('>')
                || is_horizontal_rule(cur)
                || is_list_item(cur)
                || (cur.contains('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1].trim()))
            {
                break;
            }
            para_lines.push(cur);
            i += 1;
        }

        if !para_lines.is_empty() {
            let joined = para_lines.join(" ");
            out.push_str("<p>");
            out.push_str(&render_inline_with_ref(&joined, current_ref));
            out.push_str("</p>\n");
        }
    }

    out.push_str("</div>\n");
    out
}

/// Renders markdown without wrapping `<div class="markdown-body">` (used for blockquotes).
fn render_markdown_inner_with_ref(input: &str, current_ref: Option<&str>) -> String {
    let full = render_markdown_with_ref(input, current_ref);
    let stripped = full
        .strip_prefix("<div class=\"markdown-body\">\n")
        .unwrap_or(&full);
    let stripped = stripped.strip_suffix("</div>\n").unwrap_or(stripped);
    stripped.to_string()
}

/// Renders inline markdown formatting: code, bold, italic, strikethrough, links, images.
pub fn render_inline(input: &str) -> String {
    render_inline_with_ref(input, None)
}

/// Renders inline markdown formatting with optional relative URL resolution against `current_ref`.
pub fn render_inline_with_ref(input: &str, current_ref: Option<&str>) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut idx = 0;
    let n = chars.len();

    while idx < n {
        let ch = chars[idx];

        // 1. Inline code: `code`
        if ch == '`' {
            let mut end = idx + 1;
            while end < n && chars[end] != '`' {
                end += 1;
            }
            if end < n {
                let code_content: String = chars[idx + 1..end].iter().collect();
                out.push_str("<code>");
                out.push_str(&escape_html(&code_content));
                out.push_str("</code>");
                idx = end + 1;
                continue;
            }
        }

        // 2. Images: ![alt](url)
        if ch == '!' && idx + 1 < n && chars[idx + 1] == '[' {
            if let Some((alt, url, next_idx)) = parse_link_or_image(&chars, idx + 1) {
                let resolved_url = resolve_markdown_url(&url, current_ref);
                out.push_str(&format!(
                    "<img src=\"{}\" alt=\"{}\" style=\"max-width:100%;\" />",
                    escape_html(&resolved_url),
                    escape_html(&alt)
                ));
                idx = next_idx;
                continue;
            }
        }

        // 3. Links: [text](url)
        if ch == '[' {
            if let Some((text, url, next_idx)) = parse_link_or_image(&chars, idx) {
                let resolved_url = resolve_markdown_url(&url, current_ref);
                out.push_str(&format!(
                    "<a href=\"{}\">{}</a>",
                    escape_html(&resolved_url),
                    render_inline_with_ref(&text, current_ref)
                ));
                idx = next_idx;
                continue;
            }
        }

        // 4. Strikethrough: ~~del~~
        if ch == '~' && idx + 1 < n && chars[idx + 1] == '~' {
            if let Some((inner, next_idx)) = find_closing_delim(&chars, idx + 2, "~~") {
                out.push_str("<del>");
                out.push_str(&render_inline_with_ref(&inner, current_ref));
                out.push_str("</del>");
                idx = next_idx;
                continue;
            }
        }

        // 5. Bold & Italic: ***text*** or ___text___
        if (ch == '*' || ch == '_')
            && idx + 2 < n
            && chars[idx + 1] == ch
            && chars[idx + 2] == ch
        {
            let delim = format!("{}{}{}", ch, ch, ch);
            if let Some((inner, next_idx)) = find_closing_delim(&chars, idx + 3, &delim) {
                out.push_str("<strong><em>");
                out.push_str(&render_inline_with_ref(&inner, current_ref));
                out.push_str("</em></strong>");
                idx = next_idx;
                continue;
            }
        }

        // 6. Bold: **text** or __text__
        if (ch == '*' || ch == '_') && idx + 1 < n && chars[idx + 1] == ch {
            let delim = format!("{}{}", ch, ch);
            if let Some((inner, next_idx)) = find_closing_delim(&chars, idx + 2, &delim) {
                out.push_str("<strong>");
                out.push_str(&render_inline_with_ref(&inner, current_ref));
                out.push_str("</strong>");
                idx = next_idx;
                continue;
            }
        }

        // 7. Italic: *text* or _text_ (ensure not mid-identifier like snake_case)
        if (ch == '*' || ch == '_') && (idx == 0 || chars[idx - 1] == ' ' || chars[idx - 1] == '(' || ch == '*') {
            let delim = format!("{}", ch);
            if let Some((inner, next_idx)) = find_closing_delim(&chars, idx + 1, &delim) {
                out.push_str("<em>");
                out.push_str(&render_inline_with_ref(&inner, current_ref));
                out.push_str("</em>");
                idx = next_idx;
                continue;
            }
        }

        // 8. Escaped characters: \<, \>, \*, \_
        if ch == '\\' && idx + 1 < n {
            let next_ch = chars[idx + 1];
            match next_ch {
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '&' => out.push_str("&amp;"),
                '"' => out.push_str("&quot;"),
                '*' | '_' | '`' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '!' | '~' | '\\' => {
                    out.push(next_ch);
                }
                _ => {
                    out.push('\\');
                    out.push(next_ch);
                }
            }
            idx += 2;
            continue;
        }

        // Regular character escaping
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
        idx += 1;
    }

    out
}

/// Resolves a markdown URL to a valid web route in Graft Web.
pub fn resolve_markdown_url(url: &str, current_ref: Option<&str>) -> String {
    let trimmed = url.trim();

    // 1. Anchors (#heading): keep as is
    if trimmed.starts_with('#') {
        return trimmed.to_string();
    }

    // 2. Absolute web links: keep as is
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("mailto:")
    {
        return trimmed.to_string();
    }

    // 3. Clean file:/// URLs if user or tool passed file:///google/src/cloud/.../graft/path
    let clean_path = if let Some(rest) = trimmed.strip_prefix("file://") {
        if let Some(idx) = rest.find("/graft/") {
            &rest[idx + "/graft/".len()..]
        } else {
            rest.trim_start_matches('/')
        }
    } else {
        trimmed
    };

    let clean_path = clean_path.trim_start_matches('/');

    // 4. If current_ref is specified, route to /tree/{ref}/ or /blob/{ref}/
    if let Some(r) = current_ref {
        if clean_path.is_empty() {
            return format!("/tree/{}/", r);
        }
        // If it's a directory link (ends with / or has no file extension)
        if clean_path.ends_with('/') || (!clean_path.contains('.') && !clean_path.is_empty()) {
            let path_with_slash = if clean_path.ends_with('/') {
                clean_path.to_string()
            } else {
                format!("{}/", clean_path)
            };
            format!("/tree/{}/{}", r, path_with_slash)
        } else {
            format!("/blob/{}/{}", r, clean_path)
        }
    } else {
        clean_path.to_string()
    }
}

/// Helper to parse `[text](url)`.
fn parse_link_or_image(chars: &[char], start_bracket: usize) -> Option<(String, String, usize)> {
    let mut i = start_bracket + 1;
    let n = chars.len();

    // Find closing ']'
    while i < n && chars[i] != ']' {
        i += 1;
    }
    if i >= n || i + 1 >= n || chars[i + 1] != '(' {
        return None;
    }

    let text: String = chars[start_bracket + 1..i].iter().collect();
    let url_start = i + 2;
    let mut url_end = url_start;

    // Find closing ')'
    while url_end < n && chars[url_end] != ')' {
        url_end += 1;
    }
    if url_end >= n {
        return None;
    }

    let url: String = chars[url_start..url_end].iter().collect();
    Some((text, url.trim().to_string(), url_end + 1))
}

/// Helper to find closing delimiter string like `**`, `*`, `~~`.
fn find_closing_delim(chars: &[char], start: usize, delim: &str) -> Option<(String, usize)> {
    let delim_chars: Vec<char> = delim.chars().collect();
    let dlen = delim_chars.len();
    let n = chars.len();

    let mut i = start;
    while i + dlen <= n {
        if &chars[i..i + dlen] == delim_chars.as_slice() {
            let inner: String = chars[start..i].iter().collect();
            return Some((inner, i + dlen));
        }
        i += 1;
    }
    None
}

/// Helper to strip markdown formatting (links, code, bold, images) before computing slug.
fn strip_markdown_formatting(input: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let n = chars.len();

    while i < n {
        // [text](url) -> keep only text
        if chars[i] == '[' {
            let mut j = i + 1;
            while j < n && chars[j] != ']' {
                j += 1;
            }
            if j < n && j + 1 < n && chars[j + 1] == '(' {
                let mut k = j + 2;
                while k < n && chars[k] != ')' {
                    k += 1;
                }
                if k < n {
                    let link_text: String = chars[i + 1..j].iter().collect();
                    out.push_str(&strip_markdown_formatting(&link_text));
                    i = k + 1;
                    continue;
                }
            }
        }

        // Ignore delimiters ` * _ ~
        if chars[i] == '`' || chars[i] == '*' || chars[i] == '_' || chars[i] == '~' {
            i += 1;
            continue;
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Generates a GitHub-style URL slug from heading text.
fn generate_slug(text: &str) -> String {
    let clean = strip_markdown_formatting(text);
    let mut slug = String::new();
    let mut last_dash = false;

    for ch in clean.chars() {
        if ch.is_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if ch == ' ' || ch == '-' || ch == '_' {
            if !last_dash && !slug.is_empty() {
                slug.push('-');
                last_dash = true;
            }
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// Checks if line is a horizontal rule (`---`, `***`, `___`).
fn is_horizontal_rule(line: &str) -> bool {
    let s = line.replace(' ', "");
    if s.len() >= 3 {
        let first = s.chars().next().unwrap();
        if (first == '-' || first == '*' || first == '_') && s.chars().all(|c| c == first) {
            return true;
        }
    }
    false
}

/// Checks if line is a list item (`- `, `* `, `+ `, or `1. `).
fn is_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return true;
    }
    if let Some((prefix, _)) = trimmed.split_once(' ') {
        if prefix.ends_with('.') && prefix[..prefix.len() - 1].chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    false
}

/// Renders a contiguous list block (supporting unordered, ordered, and GFM task checkboxes).
fn render_list_block(
    lines: &[&str],
    mut start_idx: usize,
    out: &mut String,
    current_ref: Option<&str>,
) -> usize {
    let first_line = lines[start_idx].trim_start();
    let is_ordered = first_line.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);

    let tag = if is_ordered { "ol" } else { "ul" };
    out.push_str(&format!("<{}>\n", tag));

    while start_idx < lines.len() {
        let line = lines[start_idx];
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            break;
        }

        let is_cur_ordered = trimmed.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
        if !is_list_item(trimmed) || (is_ordered != is_cur_ordered) {
            break;
        }

        // Extract item text
        let item_content = if let Some(stripped) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            stripped
        } else if let Some((_, rest)) = trimmed.split_once(' ') {
            rest
        } else {
            ""
        };

        // GFM Task list checkbox: [ ] or [x]
        if let Some(task_rest) = item_content.strip_prefix("[ ] ") {
            out.push_str(&format!(
                "<li class=\"task-list-item\"><input type=\"checkbox\" disabled /> {}</li>\n",
                render_inline_with_ref(task_rest, current_ref)
            ));
        } else if let Some(task_rest) = item_content
            .strip_prefix("[x] ")
            .or_else(|| item_content.strip_prefix("[X] "))
        {
            out.push_str(&format!(
                "<li class=\"task-list-item\"><input type=\"checkbox\" checked disabled /> {}</li>\n",
                render_inline_with_ref(task_rest, current_ref)
            ));
        } else {
            out.push_str(&format!(
                "<li>{}</li>\n",
                render_inline_with_ref(item_content, current_ref)
            ));
        }

        start_idx += 1;
    }

    out.push_str(&format!("</{}>\n", tag));
    start_idx
}

/// Detects GitHub Alert syntax: `[!NOTE]`, `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]`, `[!CAUTION]`.
fn detect_alert(lines: &[&str]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let first = lines[0].trim();
    if first.starts_with("[!") && first.ends_with(']') {
        let alert_name = &first[2..first.len() - 1];
        return Some(alert_name.to_uppercase());
    }
    None
}

/// Renders a GitHub-style alert callout box.
fn render_github_alert(
    out: &mut String,
    alert_type: &str,
    content_lines: &[&str],
    current_ref: Option<&str>,
) {
    let (css_class, icon_svg, title) = match alert_type {
        "NOTE" => (
            "markdown-alert-note",
            "<svg class=\"octicon\" viewBox=\"0 0 16 16\" width=\"16\" height=\"16\" fill=\"currentColor\"><path d=\"M0 8a8 8 0 1 1 16 0A8 8 0 0 1 0 8Zm8-6.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13ZM6.5 7.75A.75.75 0 0 1 7.25 7h1a.75.75 0 0 1 .75.75v2.75h.25a.75.75 0 0 1 0 1.5h-2a.75.75 0 0 1 0-1.5h.25v-2h-.25a.75.75 0 0 1-.75-.75ZM8 6a1 1 0 1 1 0-2 1 1 0 0 1 0 2Z\"></path></svg>",
            "Note",
        ),
        "TIP" => (
            "markdown-alert-tip",
            "<svg class=\"octicon\" viewBox=\"0 0 16 16\" width=\"16\" height=\"16\" fill=\"currentColor\"><path d=\"M8 1.5c-2.363 0-4 1.69-4 3.75 0 .984.424 1.625.984 2.304l.214.253c.223.264.47.556.673.848.284.411.537.896.621 1.49a.75.75 0 0 1-1.484.211c-.04-.282-.163-.547-.37-.847a8.456 8.456 0 0 0-.542-.68c-.084-.1-.173-.205-.268-.32C3.201 7.75 2.5 6.766 2.5 5.25 2.5 2.31 4.863 0 8 0s5.5 2.31 5.5 5.25c0 1.516-.701 2.5-1.328 3.259-.095.115-.184.22-.268.319-.18.213-.362.43-.542.681-.207.3-.33.565-.37.847a.751.751 0 0 1-1.485-.212c.084-.593.337-1.078.621-1.489.203-.292.45-.584.673-.848.075-.088.147-.173.213-.253.561-.679.985-1.32.985-2.304 0-2.06-1.637-3.75-4-3.75ZM5.75 12h4.5a.75.75 0 0 1 0 1.5h-4.5a.75.75 0 0 1 0-1.5Zm1 3h2.5a.75.75 0 0 1 0 1.5h-2.5a.75.75 0 0 1 0-1.5Z\"></path></svg>",
            "Tip",
        ),
        "IMPORTANT" => (
            "markdown-alert-important",
            "<svg class=\"octicon\" viewBox=\"0 0 16 16\" width=\"16\" height=\"16\" fill=\"currentColor\"><path d=\"M0 1.75C0 .784.784 0 1.75 0h12.5C15.216 0 16 .784 16 1.75v9.5A1.75 1.75 0 0 1 14.25 13H9.06l-2.573 2.573A1.458 1.458 0 0 1 4 14.543V13H1.75A1.75 1.75 0 0 1 0 11.25Zm1.75-.25a.25.25 0 0 0-.25.25v9.5c0 .138.112.25.25.25h2.5a.75.75 0 0 1 .75.75v2.19l2.72-2.72a.749.749 0 0 1 .53-.22h6a.25.25 0 0 0 .25-.25v-9.5a.25.25 0 0 0-.25-.25Zm7 2.25v4.5a.75.75 0 0 1-1.5 0v-4.5a.75.75 0 0 1 1.5 0Zm-1.5 7.5a1 1 0 1 1 1.5 0 1 1 0 0 1-1.5 0Z\"></path></svg>",
            "Important",
        ),
        "WARNING" => (
            "markdown-alert-warning",
            "<svg class=\"octicon\" viewBox=\"0 0 16 16\" width=\"16\" height=\"16\" fill=\"currentColor\"><path d=\"M6.457 1.047c.659-1.234 2.427-1.234 3.086 0l6.082 11.378A1.75 1.75 0 0 1 14.082 15H1.918a1.75 1.75 0 0 1-1.543-2.575Zm1.763.707a.25.25 0 0 0-.44 0L1.698 13.132a.25.25 0 0 0 .22.368h12.164a.25.25 0 0 0 .22-.368Zm.53 3.996v2.5a.75.75 0 0 1-1.5 0v-2.5a.75.75 0 0 1 1.5 0ZM9 11a1 1 0 1 1-2 0 1 1 0 0 1 2 0Z\"></path></svg>",
            "Warning",
        ),
        _ => (
            "markdown-alert-caution",
            "<svg class=\"octicon\" viewBox=\"0 0 16 16\" width=\"16\" height=\"16\" fill=\"currentColor\"><path d=\"M4.47.047A1.75 1.75 0 0 1 5.71 0h4.58c.464 0 .909.184 1.237.513l4.96 4.96c.329.328.513.773.513 1.237v4.58c0 .464-.184.91-.513 1.238l-4.96 4.96a1.75 1.75 0 0 1-1.237.512H5.71a1.75 1.75 0 0 1-1.238-.512l-4.96-4.96A1.75 1.75 0 0 1 0 11.29V6.71c0-.464.184-.909.513-1.237l4.96-4.96c.328-.329.774-.513 1.237-.513ZM5.71 1.5a.25.25 0 0 0-.177.073L.573 6.533a.25.25 0 0 0-.073.177v4.58c0 .066.026.13.073.177l4.96 4.96c.047.047.111.073.177.073h4.58a.25.25 0 0 0 .177-.073l4.96-4.96a.25.25 0 0 0 .073-.177V6.71a.25.25 0 0 0-.073-.177l-4.96-4.96a.25.25 0 0 0-.177-.073H5.71Zm3.04 3.75a.75.75 0 0 0-1.5 0v3.5a.75.75 0 0 0 1.5 0v-3.5ZM8 12a1 1 0 1 0 0-2 1 1 0 0 0 0 2Z\"></path></svg>",
            "Caution",
        ),
    };

    out.push_str(&format!(
        "<div class=\"markdown-alert {}\">\n<p class=\"markdown-alert-title\">{} <span>{}</span></p>\n",
        css_class, icon_svg, title
    ));

    let body = content_lines.join("\n");
    let rendered_body = render_markdown_inner_with_ref(&body, current_ref);
    out.push_str(&rendered_body);
    out.push_str("</div>\n");
}

/// Checks if line is a Markdown table separator: e.g. `| --- | :---: | ---: |`.
fn is_table_separator(line: &str) -> bool {
    let clean = line.trim_matches('|').trim();
    if clean.is_empty() {
        return false;
    }
    clean.split('|').all(|col| {
        let col = col.trim();
        if col.is_empty() {
            return false;
        }
        let dashes = col.trim_matches(':');
        !dashes.is_empty() && dashes.chars().all(|c| c == '-')
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ColAlign {
    Left,
    Center,
    Right,
}

fn parse_table_alignments(sep_line: &str) -> Vec<ColAlign> {
    let clean = sep_line.trim_matches('|');
    clean
        .split('|')
        .map(|col| {
            let col = col.trim();
            let left_colon = col.starts_with(':');
            let right_colon = col.ends_with(':');
            match (left_colon, right_colon) {
                (true, true) => ColAlign::Center,
                (false, true) => ColAlign::Right,
                _ => ColAlign::Left,
            }
        })
        .collect()
}

fn parse_table_row(line: &str) -> Vec<String> {
    let clean = line.trim_matches('|');
    clean.split('|').map(|s| s.trim().to_string()).collect()
}

fn get_align_attr(align: Option<ColAlign>) -> &'static str {
    match align {
        Some(ColAlign::Center) => " align=\"center\"",
        Some(ColAlign::Right) => " align=\"right\"",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_headings_and_slugs() {
        let md = "# Graft Overview\n## Quick Start: Getting Started!\n### Deep Dive\n### [User Manual](docs/manual/README.md)";
        let html = render_markdown(md);
        assert!(html.contains("<h1 id=\"graft-overview\">"));
        assert!(html.contains("<h2 id=\"quick-start-getting-started\">"));
        assert!(html.contains("<h3 id=\"deep-dive\">"));
        // Heading with link produces clean slug "user-manual", not "user-manual-docs-manual-readme-md"
        assert!(html.contains("<h3 id=\"user-manual\">"));
    }

    #[test]
    fn test_fenced_code_block() {
        let md = "```rust\nfn main() {\n    println!(\"Hello Graft!\");\n}\n```";
        let html = render_markdown(md);
        assert!(html.contains("<pre><code class=\"language-rust\">fn main() {\n    println!(&quot;Hello Graft!&quot;);\n}</code></pre>"));
    }

    #[test]
    fn test_inline_formatting() {
        let md = "This has `code`, **bold**, *italic*, ~~deleted~~, and [Graft](https://graft.dev).";
        let html = render_markdown(md);
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>italic</em>"));
        assert!(html.contains("<del>deleted</del>"));
        assert!(html.contains("<a href=\"https://graft.dev\">Graft</a>"));
    }

    #[test]
    fn test_relative_url_resolution() {
        let md = "See [Quickstart](docs/manual/quickstart.md) or [Manual](docs/manual/).";
        let html = render_markdown_with_ref(md, Some("main"));
        assert!(html.contains("<a href=\"/blob/main/docs/manual/quickstart.md\">Quickstart</a>"));
        assert!(html.contains("<a href=\"/tree/main/docs/manual/\">Manual</a>"));
    }

    #[test]
    fn test_file_url_cleaning() {
        let md = "Check [Config](file:///google/src/cloud/user/workspace/graft/docs/manual/quickstart.md)";
        let html = render_markdown_with_ref(md, Some("main"));
        assert!(html.contains("<a href=\"/blob/main/docs/manual/quickstart.md\">Config</a>"));
    }

    #[test]
    fn test_github_alerts() {
        let md = "> [!NOTE]\n> Here is an important note about CAS storage.";
        let html = render_markdown(md);
        assert!(html.contains("markdown-alert markdown-alert-note"));
        assert!(html.contains("Note"));
        assert!(html.contains("Here is an important note about CAS storage."));
    }

    #[test]
    fn test_gfm_tables() {
        let md = "| Feature | Status | Speed |\n| :--- | :---: | ---: |\n| Merkle Tree | Complete | 100x |\n| FUSE | Done | Realtime |";
        let html = render_markdown(md);
        assert!(html.contains("<table class=\"markdown-table\">"));
        assert!(html.contains("<th align=\"center\">Status</th>"));
        assert!(html.contains("<th align=\"right\">Speed</th>"));
        assert!(html.contains("<td>Merkle Tree</td>"));
    }

    #[test]
    fn test_mermaid_rendering() {
        let md = "```mermaid\ngraph TD\n    A[Client] --> B[Server]\n```";
        let html = render_markdown(md);
        assert!(html.contains("<div class=\"mermaid-container\"><pre class=\"mermaid\">"));
        assert!(html.contains("A[Client] --&gt; B[Server]"));
    }

    #[test]
    fn test_task_lists() {
        let md = "- [x] Initial commit\n- [ ] Release v1.0";
        let html = render_markdown(md);
        assert!(html.contains("<li class=\"task-list-item\"><input type=\"checkbox\" checked disabled /> Initial commit</li>"));
        assert!(html.contains("<li class=\"task-list-item\"><input type=\"checkbox\" disabled /> Release v1.0</li>"));
    }
}
