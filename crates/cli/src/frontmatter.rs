//! Shared parsing for Markdown files with a leading YAML-ish frontmatter
//! block, e.g.:
//!
//! ```markdown
//! ---
//! name: foo
//! description: bar
//! ---
//!
//! Body text.
//! ```
//!
//! Used by [`crate::skills`] (`SKILL.md`) and [`crate::commands`]
//! (`.sigit/commands/*.md`) — both need the same "top-level `key: value`
//! scalars, then a Markdown body" shape, so the parsing lives here once
//! rather than being copied per format.

/// Extract the YAML frontmatter block from `contents`: the text between a
/// leading `---` line and the next `---` line. Returns `None` if absent.
pub fn extract_frontmatter(contents: &str) -> Option<&str> {
    // Strip an optional UTF-8 BOM and leading blank lines before the opener.
    let trimmed = contents.trim_start_matches('\u{feff}');
    let mut rest = trimmed;
    loop {
        let line_end = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        let (line, after) = rest.split_at(line_end);
        if line.trim().is_empty() {
            rest = after;
            continue;
        }
        if line.trim() != "---" {
            return None;
        }
        // `after` now begins just past the opening `---` line.
        let body = after;
        let mut search = body;
        let mut offset = 0;
        loop {
            let end = search.find('\n').map(|i| i + 1).unwrap_or(search.len());
            let (l, a) = search.split_at(end);
            if l.trim() == "---" {
                return Some(&body[..offset]);
            }
            if a.is_empty() {
                return None;
            }
            offset += end;
            search = a;
        }
    }
}

/// Parse top-level `key: value` scalar pairs from frontmatter, skipping
/// nested mappings (indented lines) and comments. Quoted values are
/// unquoted.
pub fn parse_frontmatter_fields(frontmatter: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    for line in frontmatter.lines() {
        // Indented lines belong to a nested mapping/sequence — skip them.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = unquote(value.trim());
        fields.push((key.to_string(), value));
    }
    fields
}

/// Strip a single layer of matching single or double quotes; otherwise
/// return the value unchanged.
pub fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

/// Return the content after the frontmatter block, or the whole input if
/// there is no frontmatter.
pub fn strip_frontmatter(contents: &str) -> &str {
    // Mirror `extract_frontmatter`'s leniency exactly: tolerate a UTF-8 BOM,
    // leading blank lines before the opener, and CRLF line endings (compare
    // trimmed lines). If the two disagree, a file whose frontmatter parses
    // still leaks that raw block into the body — see the round-trip test.
    let trimmed = contents.trim_start_matches('\u{feff}');
    let mut rest = trimmed;
    loop {
        let line_end = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        let (line, after) = rest.split_at(line_end);
        if line.trim().is_empty() {
            rest = after;
            continue;
        }
        if line.trim() != "---" {
            return contents;
        }
        // `after` begins just past the opening `---` line; find the closing one.
        let mut search = after;
        loop {
            let end = search.find('\n').map(|i| i + 1).unwrap_or(search.len());
            let (l, a) = search.split_at(end);
            if l.trim() == "---" {
                return a;
            }
            if a.is_empty() {
                return contents;
            }
            search = a;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_frontmatter_reads_block() {
        let md = "---\nname: foo\ndescription: bar\n---\n\nBody here.\n";
        let fm = extract_frontmatter(md).expect("frontmatter");
        assert!(fm.contains("name: foo"));
        assert!(fm.contains("description: bar"));
        assert!(!fm.contains("Body here"));
    }

    #[test]
    fn extract_frontmatter_none_without_delimiter() {
        assert!(extract_frontmatter("no frontmatter here").is_none());
        assert!(extract_frontmatter("---\nname: foo\n").is_none());
    }

    #[test]
    fn parse_fields_handles_quotes_and_nesting() {
        let fm = "name: pdf-processing\ndescription: \"Extract PDF text\"\nmetadata:\n  author: me\n  version: \"1.0\"\nlicense: Apache-2.0\n";
        let fields = parse_frontmatter_fields(fm);
        let get = |k: &str| {
            fields
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("name").as_deref(), Some("pdf-processing"));
        assert_eq!(get("description").as_deref(), Some("Extract PDF text"));
        assert_eq!(get("license").as_deref(), Some("Apache-2.0"));
        // Nested keys under `metadata:` are skipped.
        assert!(get("author").is_none());
        assert!(get("version").is_none());
    }

    #[test]
    fn strip_frontmatter_returns_body() {
        let md = "---\nname: foo\ndescription: bar\n---\n\n# Heading\n\nText.\n";
        assert_eq!(strip_frontmatter(md).trim(), "# Heading\n\nText.");
    }

    #[test]
    fn strip_frontmatter_passes_through_without_block() {
        assert_eq!(strip_frontmatter("just body"), "just body");
    }

    #[test]
    fn extract_and_strip_agree_on_crlf_and_leading_blanks() {
        // Whenever extract_frontmatter finds a block, strip_frontmatter must
        // remove exactly that block and never leak it into the body. These
        // inputs (CRLF endings, a leading blank line, a BOM) parse fine in
        // extract but used to defeat strip's byte-exact opener check.
        for md in [
            "---\r\nname: foo\r\ndescription: bar\r\n---\r\n\r\nBody.\r\n",
            "\n---\nname: foo\ndescription: bar\n---\n\nBody.\n",
            "\u{feff}---\nname: foo\ndescription: bar\n---\n\nBody.\n",
        ] {
            assert!(
                extract_frontmatter(md).is_some(),
                "extract should find frontmatter in {md:?}"
            );
            let body = strip_frontmatter(md);
            assert!(
                !body.contains("name: foo"),
                "frontmatter leaked into body for {md:?}: {body:?}"
            );
            assert_eq!(body.trim(), "Body.");
        }
    }
}
