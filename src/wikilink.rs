//! `[[target]]` wiki-link detection - shared by the editor's syntax
//! highlighter (which colors the span) and the click-to-navigate
//! handler (which needs the same spans' byte ranges to hit-test
//! against). Ported from the original React app's CodeMirror 6
//! extension (`src/editor/wikilink.ts`), which only ever supported bare
//! `[[target]]` - no `[[target|Alias]]`, no `[[target#Heading]]` - so
//! this doesn't either; a `[[note|Alias]]` link's "target" is the raw
//! `note|Alias` string.

use std::collections::HashSet;
use std::ops::Range;

/// Builds an in-memory O(1) lookup set of all resolvable WikiLink target keys
/// (lowercased, `/`-normalized basenames and path suffixes, both with and without `.md`)
/// from the workspace `TreeNode` hierarchy. Called only when the workspace tree is loaded or refreshed,
/// avoiding any disk or network I/O during 60fps editor/preview rendering.
pub fn build_wiki_target_set(tree: &[agent_protocol::TreeNode]) -> HashSet<String> {
    let mut set = HashSet::new();
    collect_wiki_targets_recursive(tree, &mut set);
    set
}

fn collect_wiki_targets_recursive(nodes: &[agent_protocol::TreeNode], set: &mut HashSet<String>) {
    for node in nodes {
        if node.is_dir {
            if let Some(children) = &node.children {
                collect_wiki_targets_recursive(children, set);
            }
        } else {
            let name_lower = node.name.to_lowercase();
            if let Some(stem) = name_lower.strip_suffix(".md") {
                set.insert(stem.to_string());
            }
            set.insert(name_lower);

            let full_norm = node.path.replace('\\', "/").to_lowercase();
            insert_path_and_md_stem(set, &full_norm);
            for (idx, ch) in full_norm.char_indices() {
                if ch == '/' && idx + 1 < full_norm.len() {
                    insert_path_and_md_stem(set, &full_norm[idx + 1..]);
                }
            }
        }
    }
}

fn insert_path_and_md_stem(set: &mut HashSet<String>, path_slice: &str) {
    if path_slice.is_empty() {
        return;
    }
    if let Some(stem) = path_slice.strip_suffix(".md") {
        if !stem.is_empty() {
            set.insert(stem.to_string());
        }
    }
    set.insert(path_slice.to_string());
}

/// Checks in O(1) time whether `raw_target` (the text inside `[[...]]`, which may include
/// `#heading` or `|alias`) resolves to an existing file in `known`.
pub fn wiki_target_exists(raw_target: &str, known: &HashSet<String>) -> bool {
    let mut clean = raw_target.trim();
    if let Some((base, _alias)) = clean.split_once('|') {
        clean = base.trim();
    }
    if let Some((base, _heading)) = clean.split_once('#') {
        clean = base.trim();
    }
    if clean.is_empty() {
        return false;
    }
    let norm = clean.to_lowercase().replace('\\', "/");
    let norm = norm.trim_start_matches('/');
    known.contains(norm)
}

/// Scans `text` for `[[target]]` spans, returning each one's byte range
/// (brackets included) and its raw target text (whatever's between the
/// brackets, unmodified). Mirrors the original's bracket-scan exactly:
/// triggered by `[[`, ended by the first `]]`, never crossing a
/// newline, target must be non-empty (`[[]]` doesn't count).
pub fn find_wiki_links(text: &str) -> Vec<(Range<usize>, &str)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            let mut close = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'\n' {
                    break;
                }
                if bytes[j] == b']' && bytes[j + 1] == b']' {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = close {
                if end > i + 2 {
                    // Safe to slice at these exact byte offsets even for
                    // multi-byte UTF-8 targets (Japanese note titles are
                    // common here): every boundary is immediately next to
                    // an ASCII '['/']' byte, which can never fall inside
                    // a multi-byte sequence.
                    let target = text[i + 2..end].trim();
                    if !target.is_empty() {
                        out.push((i..end + 2, target));
                    }
                    i = end + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Scans `text` for Markdown links `[anchor](url)`, returning each one's byte range, anchor text, and destination url.
pub fn find_markdown_links(text: &str) -> Vec<(Range<usize>, &str, &str)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' && (i == 0 || bytes[i - 1] != b'!') {
            let start = i;
            if let Some(close_bracket) = text[start + 1..].find("](") {
                let split_idx = start + 1 + close_bracket;
                if !text[start + 1..split_idx].contains('\n') {
                    if let Some(close_paren) = text[split_idx + 2..].find(')') {
                        let end = split_idx + 2 + close_paren + 1;
                        let url = text[split_idx + 2..end - 1].trim();
                        let anchor = text[start + 1..split_idx].trim();
                        if !url.is_empty() && !url.contains('\n') {
                            out.push((start..end, anchor, url));
                            i = end;
                            continue;
                        }
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// Rewrites `![[target]]` to `![target](target)` and `[[target]]` or `[[target|alias]]` in `text`
/// to CommonMark `[label](<wikilink:target>)` format so that `egui_commonmark` renders them
/// as embedded images or clickable links even with spaces or headings (#).
pub fn rewrite_wiki_links_to_markdown(text: &str) -> String {
    rewrite_wiki_links_to_markdown_with_exists(text, |_| true)
}

/// Like [`rewrite_wiki_links_to_markdown`], but checks `exists(target)` for each `[[target]]` link:
/// - Existing targets render as `[label](<wikilink:target>)`.
/// - Missing targets append ` ➕` to the visible label (`[label ➕](<wikilink:target>)`) while keeping
///   the `<wikilink:target>` URL intact so clicking it still creates and opens the target file.
pub fn rewrite_wiki_links_to_markdown_with_exists(text: &str, exists: impl Fn(&str) -> bool) -> String {
    // First, convert ![[target]] or ![[target|alt]] embed syntax to ![alt](target)
    let embed_re = regex::Regex::new(r#"!\[\[([^\]\n]+)\]\]"#).unwrap();
    let with_embeds = embed_re.replace_all(text, |caps: &regex::Captures| {
        let raw = caps[1].trim();
        if let Some((target, alt)) = raw.split_once('|') {
            let t = target.trim();
            let a = alt.trim();
            format!("![{a}]({t})")
        } else {
            format!("![{raw}]({raw})")
        }
    });

    let link_re = regex::Regex::new(r#"\[\[([^\]\n]+)\]\]"#).unwrap();
    link_re
        .replace_all(&with_embeds, |caps: &regex::Captures| {
            let raw = caps[1].trim();
            let (t, label) = if let Some((target, alias)) = raw.split_once('|') {
                (target.trim(), alias.trim())
            } else {
                (raw, raw)
            };
            if exists(raw) {
                format!("[{label}](<wikilink:{t}>)")
            } else {
                format!("[{label} ➕](<wikilink:{t}>)")
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_simple_link() {
        let links = find_wiki_links("see [[my note]] for details");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "my note");
        assert_eq!(&"see [[my note]] for details"[links[0].0.clone()], "[[my note]]");
    }

    #[test]
    fn finds_multiple_links_on_one_line() {
        let links = find_wiki_links("[[a]] and [[b]]");
        assert_eq!(links.iter().map(|(_, t)| *t).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn ignores_empty_or_whitespace_only_brackets() {
        assert!(find_wiki_links("[[]]").is_empty());
        assert!(find_wiki_links("[[   ]]").is_empty());
    }

    #[test]
    fn trims_whitespace_around_target_keyword() {
        let links = find_wiki_links("see [[ keyword ]] for details");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "keyword");
        assert_eq!(&"see [[ keyword ]] for details"[links[0].0.clone()], "[[ keyword ]]");
    }

    #[test]
    fn does_not_cross_a_newline() {
        assert!(find_wiki_links("[[unterminated\nstill not a link]]").is_empty());
    }

    #[test]
    fn ignores_a_single_unmatched_open() {
        assert!(find_wiki_links("[[dangling").is_empty());
    }

    #[test]
    fn handles_multibyte_targets() {
        let links = find_wiki_links("日本語の[[会議メモ]]です");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "会議メモ");
    }

    #[test]
    fn pipe_and_hash_are_part_of_the_raw_target_not_stripped() {
        // Matches the original app's behavior exactly - no alias/heading
        // support, so this isn't a bug to "fix" here.
        let links = find_wiki_links("[[note|Alias]] and [[note#Heading]]");
        assert_eq!(links[0].1, "note|Alias");
        assert_eq!(links[1].1, "note#Heading");
    }

    #[test]
    fn rewrite_wiki_links_to_markdown_converts_links() {
        let input = "Check [[MyNote]] and [[Target with Space#Heading 1|Custom Label]] and ![[image.png]] and ![[doc.pdf|My PDF]] here";
        let output = rewrite_wiki_links_to_markdown(input);
        assert_eq!(
            output,
            "Check [MyNote](<wikilink:MyNote>) and [Custom Label](<wikilink:Target with Space#Heading 1>) and ![image.png](image.png) and ![My PDF](doc.pdf) here"
        );
    }

    #[test]
    fn test_find_markdown_links_extracts_links() {
        let text = "Check [Google](https://google.com) and [Other Note](note.md) here";
        let links = find_markdown_links(text);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].1, "Google");
        assert_eq!(links[0].2, "https://google.com");
        assert_eq!(links[1].1, "Other Note");
        assert_eq!(links[1].2, "note.md");
    }

    #[test]
    fn build_wiki_target_set_and_exists_distinguish_existing_and_missing_targets() {
        let tree = vec![agent_protocol::TreeNode {
            name: "notes".to_string(),
            path: "D:\\workspace\\notes".to_string(),
            is_dir: true,
            size: 0,
            children: Some(vec![agent_protocol::TreeNode {
                name: "ExistingNote.md".to_string(),
                path: "D:\\workspace\\notes\\ExistingNote.md".to_string(),
                is_dir: false,
                size: 42,
                children: None,
            }]),
        }];
        let known = build_wiki_target_set(&tree);
        assert!(wiki_target_exists("ExistingNote", &known));
        assert!(wiki_target_exists("existingnote.md", &known));
        assert!(wiki_target_exists("notes/ExistingNote#Section 1|Alias", &known));
        assert!(!wiki_target_exists("MissingNote", &known));
        assert!(!wiki_target_exists("other/ExistingNote", &known));

        let md = "See [[ExistingNote]] and [[MissingNote]] and [[MissingNote|New Page]]";
        let rewritten = rewrite_wiki_links_to_markdown_with_exists(md, |t| wiki_target_exists(t, &known));
        assert_eq!(
            rewritten,
            "See [ExistingNote](<wikilink:ExistingNote>) and [MissingNote ➕](<wikilink:MissingNote>) and [New Page ➕](<wikilink:MissingNote>)"
        );
    }
}
