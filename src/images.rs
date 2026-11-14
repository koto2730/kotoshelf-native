//! Markdown image-reference rewriting for the preview pane - resolves a
//! `![alt](target)` reference's relative `target` against the directory
//! of the note that contains it, turning it into a URI the preview's
//! `egui_commonmark` viewer (and the `egui`/`egui_extras` image-loader
//! machinery it installs on first use) can actually load: an absolute
//! `file://` URI for a local workspace, or a synthetic
//! `kotoshelf-ssh://<relpath>` URI for an SSH one (whose bytes are
//! fetched/cached and registered via `Context::include_bytes` separately
//! in `app.rs` - this module only computes the rewritten markdown
//! string, no I/O of its own).

use regex::Regex;

/// Matches CommonMark image syntax `![alt](target)` - deliberately not
/// title-aware (`![alt](target "title")`); none of this app's own
/// content uses image titles, and a wrong split there would just leave
/// that one target unresolved rather than corrupt anything.
fn image_ref_regex() -> Regex {
    Regex::new(r#"!\[([^\]]*)\]\(([^)\s]+)\)"#).unwrap()
}

/// `true` if `target` already carries its own scheme (`scheme://...`) or
/// is a `data:` URI - these are left untouched, matching the original's
/// `resolveLocalImageSrc`'s early return for absolute http(s)/data/asset
/// URIs.
pub fn has_scheme(target: &str) -> bool {
    target.contains("://") || target.starts_with("data:")
}

/// Rewrites every schemeless image target in `content` via `resolve`
/// (which turns a relative target into an absolute URI string) - run
/// once per preview render, never persisted back into the tab's own
/// content.
pub fn rewrite_image_refs(content: &str, mut resolve: impl FnMut(&str) -> String) -> String {
    image_ref_regex()
        .replace_all(content, |caps: &regex::Captures| {
            let alt = &caps[1];
            let target = &caps[2];
            if has_scheme(target) {
                caps[0].to_string()
            } else {
                format!("![{alt}]({})", resolve(target))
            }
        })
        .into_owned()
}

/// Turns an absolute local filesystem path into a `file://` URI -
/// Windows paths (`C:\Users\...`) need their backslashes flipped and an
/// extra leading slash before the drive letter (`file:///C:/Users/...`);
/// a POSIX absolute path already starts with `/` and only needs the
/// `file://` prefix.
pub fn local_file_uri(path: &std::path::Path) -> String {
    // `Path::canonicalize` on Windows returns a "verbatim" path prefixed
    // with `\\?\` (e.g. `\\?\C:\Users\...`) - a real, working Windows
    // path for ordinary filesystem calls, but not a form any `file://`
    // URI consumer understands. Left in place, backslash-flipping turns
    // it into `//?/C:/...`, which *also* happens to satisfy this
    // function's own "already starts with /" POSIX-absolute check,
    // silently producing a malformed URI (`file:////?/C:/...`) - caught
    // live: the preview showed a broken-image icon for every local
    // image until this strip was added.
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_schemeless_targets() {
        let out = rewrite_image_refs("![a](img.png) and ![b](sub/other.jpg)", |t| format!("resolved://{t}"));
        assert_eq!(out, "![a](resolved://img.png) and ![b](resolved://sub/other.jpg)");
    }

    #[test]
    fn leaves_scheme_and_data_targets_alone() {
        let out = rewrite_image_refs(
            "![a](https://example.com/x.png) ![b](data:image/png;base64,xx)",
            |_| "SHOULD_NOT_BE_CALLED".to_string(),
        );
        assert_eq!(out, "![a](https://example.com/x.png) ![b](data:image/png;base64,xx)");
    }

    #[test]
    fn ignores_non_image_links() {
        let out = rewrite_image_refs("[a link](note.md)", |_| "SHOULD_NOT_BE_CALLED".to_string());
        assert_eq!(out, "[a link](note.md)");
    }

    #[test]
    fn local_file_uri_handles_windows_and_posix_paths() {
        assert_eq!(local_file_uri(std::path::Path::new("C:/Users/me/img.png")), "file:///C:/Users/me/img.png");
        assert_eq!(local_file_uri(std::path::Path::new("/home/me/img.png")), "file:///home/me/img.png");
    }

    #[test]
    fn local_file_uri_strips_the_windows_verbatim_prefix() {
        assert_eq!(
            local_file_uri(std::path::Path::new(r"\\?\C:\Users\me\img.png")),
            "file:///C:/Users/me/img.png"
        );
    }
}

/// Decodes raw image bytes (PNG, JPEG, etc.) and copies the decoded RGBA image to the system clipboard.
pub fn copy_image_bytes_to_clipboard(bytes: &[u8]) -> Result<(), String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("Failed to decode image: {e}"))?.to_rgba8();
    let (width, height) = img.dimensions();
    let img_data = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: std::borrow::Cow::Owned(img.into_raw()),
    };
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("Clipboard error: {e}"))?;
    clipboard.set_image(img_data).map_err(|e| format!("Failed to copy to clipboard: {e}"))?;
    Ok(())
}
