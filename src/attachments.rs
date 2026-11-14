//! Filename/path helpers for attaching a dropped (or, in future, pasted)
//! image into the workspace next to the note that references it -
//! ported from `src/lib/attachments.ts`.

pub const ATTACHMENTS_DIR: &str = "attachments";

/// `true` if `name` matches the attachments directory name case-insensitively ("attachments", "Attachments", etc.).
pub fn is_attachments_dir_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(ATTACHMENTS_DIR)
}

/// `true` if `path` contains an attachments directory component (case-insensitive).
pub fn is_attachment_path(path: &std::path::Path) -> bool {
    path.components().any(|c| {
        if let std::path::Component::Normal(n) = c {
            n.to_string_lossy().eq_ignore_ascii_case(ATTACHMENTS_DIR)
        } else {
            false
        }
    })
}

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"];

/// `true` for a file extension the preview pane/attachment flow treats
/// as an image - matches the original's `isImagePath` allowlist exactly
/// (case-insensitive).
pub fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// `true` for PDF file extensions (case-insensitive).
pub fn is_pdf_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

/// `true` for video file extensions (case-insensitive).
pub fn is_video_path(path: &std::path::Path) -> bool {
    crate::video::is_video_path(path)
}

/// `true` for files that can be attached via drag-and-drop (images, PDFs, and videos).
pub fn is_supported_attachment_path(path: &std::path::Path) -> bool {
    is_image_path(path) || is_pdf_path(path) || is_video_path(path)
}

/// Second-resolution local-time stem with an atomic sequence suffix when multiple
/// files are attached within the same second:
/// `img-YYYYMMDD-HHmmss` (or `img-YYYYMMDD-HHmmss-2`, etc.).
fn timestamp_attachment_stem(is_pdf: bool, is_video: bool) -> String {
    use std::sync::Mutex;
    static LAST_STAMP: Mutex<(String, u32)> = Mutex::new((String::new(), 0));

    let prefix = if is_pdf {
        "pdf"
    } else if is_video {
        "vid"
    } else {
        "img"
    };
    let now_str = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let seq = if let Ok(mut guard) = LAST_STAMP.lock() {
        if guard.0 == now_str {
            guard.1 += 1;
            guard.1
        } else {
            guard.0 = now_str.clone();
            guard.1 = 1;
            1
        }
    } else {
        1
    };
    if seq == 1 {
        format!("{prefix}-{now_str}")
    } else {
        format!("{prefix}-{now_str}-{seq}")
    }
}

/// Collapses anything not `[A-Za-z0-9_.-]` to `-` - handles spaces/
/// unicode in OS-provided filenames, which would otherwise break bare
/// markdown image syntax (a raw space inside `![](attachments/my
/// photo.png)`) or shell quoting on the SSH upload side.
fn sanitize_attachment_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '-' }).collect()
}

/// Builds the final attachment filename for a dropped file named
/// `original_name`: `<timestamp>-<sanitized original name>`. No
/// collision handling beyond the timestamp prefix, matching the
/// original - two drops of the same basename within the same second
/// would still collide.
pub fn attachment_filename(original_name: &str) -> String {
    let is_pdf = original_name.to_lowercase().ends_with(".pdf");
    let is_video = crate::video::is_video_path(std::path::Path::new(original_name));
    format!("{}-{}", timestamp_attachment_stem(is_pdf, is_video), sanitize_attachment_name(original_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn recognizes_known_image_and_pdf_and_video_extensions_case_insensitively() {
        assert!(is_image_path(Path::new("a.png")));
        assert!(is_image_path(Path::new("a.PNG")));
        assert!(is_image_path(Path::new("a.jpeg")));
        assert!(is_image_path(Path::new("a.svg")));
        assert!(!is_image_path(Path::new("a.pdf")));
        assert!(!is_image_path(Path::new("a.mp4")));
        assert!(!is_image_path(Path::new("a.md")));
        assert!(!is_image_path(Path::new("a")));

        assert!(is_pdf_path(Path::new("a.pdf")));
        assert!(is_pdf_path(Path::new("a.PDF")));
        assert!(!is_pdf_path(Path::new("a.png")));
        assert!(!is_pdf_path(Path::new("a.mp4")));

        assert!(is_video_path(Path::new("a.mp4")));
        assert!(is_video_path(Path::new("a.MP4")));
        assert!(is_video_path(Path::new("a.mkv")));
        assert!(!is_video_path(Path::new("a.png")));

        assert!(is_supported_attachment_path(Path::new("a.png")));
        assert!(is_supported_attachment_path(Path::new("a.pdf")));
        assert!(is_supported_attachment_path(Path::new("a.mp4")));
        assert!(!is_supported_attachment_path(Path::new("a.md")));
    }

    #[test]
    fn sanitize_collapses_non_word_chars_but_keeps_the_extension_dot() {
        assert_eq!(sanitize_attachment_name("my photo (1).png"), "my-photo--1-.png");
        assert_eq!(sanitize_attachment_name("under_score-ok.png"), "under_score-ok.png");
        assert_eq!(sanitize_attachment_name("my document (1).pdf"), "my-document--1-.pdf");
        assert_eq!(sanitize_attachment_name("my movie (1).mp4"), "my-movie--1-.mp4");
    }

    #[test]
    fn attachment_filename_has_a_timestamp_prefix_and_the_sanitized_name() {
        let img_name = attachment_filename("Screen Shot.png");
        assert!(img_name.starts_with("img-"));
        assert!(img_name.ends_with("-Screen-Shot.png"));

        let vid_name = attachment_filename("My Movie.mp4");
        assert!(vid_name.starts_with("vid-"));
        assert!(vid_name.ends_with("-My-Movie.mp4"));

        let pdf_name = attachment_filename("My Document.pdf");
        assert!(pdf_name.starts_with("pdf-"));
        assert!(pdf_name.ends_with("-My-Document.pdf"));
    }

    #[test]
    fn recognizes_attachments_dir_name_case_insensitively() {
        assert!(is_attachments_dir_name("attachments"));
        assert!(is_attachments_dir_name("Attachments"));
        assert!(is_attachments_dir_name("ATTACHMENTS"));
        assert!(!is_attachments_dir_name("other"));

        assert!(is_attachment_path(Path::new("notes/Attachments/img.png")));
        assert!(is_attachment_path(Path::new("attachments/img.png")));
        assert!(is_attachment_path(Path::new("attachments/doc.pdf")));
    }
}
