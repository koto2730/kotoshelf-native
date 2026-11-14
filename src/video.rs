//! Video thumbnail and playback helper module for video attachments (MP4, MKV, MOV, WebM, etc.).
//!
//! - Thumbnail generation:
//!   - If `ffmpeg` is available on the PATH, extracts frame at 1s into PNG bytes.
//!   - Fallback: Generates a stylish, modern cinema/video placeholder card with a play button (▶)
//!     and video metadata badge using the `image` crate in memory.
//! - Video playback:
//!   - Launches the video in the system's default media player.

use std::path::Path;

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "avi", "mov", "webm", "wmv", "flv", "m4v", "3gp", "ts"];

/// Returns `true` if `path` has a known video file extension (case-insensitive).
pub fn is_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Renders a thumbnail of a video file into PNG bytes.
/// If `file_path` is provided and exists on disk, tries `ffmpeg` first.
/// Otherwise falls back to a clean generated placeholder card.
pub fn render_video_thumbnail(file_path: Option<&Path>, original_name: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = file_path {
        if path.exists() {
            if let Ok(bytes) = extract_frame_with_ffmpeg(path) {
                return Ok(bytes);
            }
        }
    }

    generate_video_placeholder_png(original_name)
}

/// Tries to extract a single video frame at 00:00:01 using `ffmpeg` if available on PATH.
fn extract_frame_with_ffmpeg(video_path: &Path) -> Result<Vec<u8>, String> {
    let temp_dir = std::env::temp_dir();
    let id = format!("kotoshelf_vid_{}_{}", std::process::id(), chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
    let temp_out_png = temp_dir.join(format!("{id}.png"));

    let res = std::process::Command::new("ffmpeg")
        .args(["-ss", "00:00:01", "-i"])
        .arg(video_path)
        .args(["-vframes", "1", "-vf", "scale=480:-1", "-y"])
        .arg(&temp_out_png)
        .output();

    if let Ok(output) = res {
        if output.status.success() && temp_out_png.exists() {
            let bytes = std::fs::read(&temp_out_png).map_err(|e| e.to_string());
            let _ = std::fs::remove_file(&temp_out_png);
            return bytes;
        }
    }

    let _ = std::fs::remove_file(&temp_out_png);
    Err("ffmpeg not available or failed".to_string())
}

/// Generates a clean, stylized video player placeholder PNG image in memory.
pub fn generate_video_placeholder_png(_filename: &str) -> Result<Vec<u8>, String> {
    let width = 360;
    let height = 220;
    let mut img = image::RgbaImage::new(width, height);

    // Background: Dark cinema slate gradient (#181b22)
    for (_x, y, pixel) in img.enumerate_pixels_mut() {
        let grad = (y as f32 / height as f32 * 15.0) as u8;
        let r = 24u8.saturating_add(grad);
        let g = 27u8.saturating_add(grad);
        let b = 34u8.saturating_add(grad);
        *pixel = image::Rgba([r, g, b, 255]);
    }

    // Top and bottom film strip perforations
    let perf_w = 8;
    let perf_h = 6;
    let perf_step = 16;
    for x_start in (10..width - 10).step_by(perf_step) {
        // Top perforations
        for y in 6..(6 + perf_h) {
            for x in x_start..(x_start + perf_w) {
                img.put_pixel(x, y, image::Rgba([50, 56, 68, 255]));
            }
        }
        // Bottom perforations
        for y in (height - 12)..(height - 12 + perf_h) {
            for x in x_start..(x_start + perf_w) {
                img.put_pixel(x, y, image::Rgba([50, 56, 68, 255]));
            }
        }
    }

    // Centered Play Button Circle (Radius 32, center [width/2, height/2])
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let radius = 32.0;

    for y in 0..height {
        for x in 0..width {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist <= radius {
                // Circle interior: Vibrant Indigo/Purple gradient (#6366f1 to #4f46e5)
                let alpha = if (radius - dist) < 1.0 { (radius - dist) * 255.0 } else { 255.0 } as u8;
                let t = (dy + radius) / (radius * 2.0);
                let r = (99.0 * (1.0 - t) + 79.0 * t) as u8;
                let g = (102.0 * (1.0 - t) + 70.0 * t) as u8;
                let b = (241.0 * (1.0 - t) + 229.0 * t) as u8;
                img.put_pixel(x, y, image::Rgba([r, g, b, alpha]));
            }
        }
    }

    // Play triangle (pointing right) inside circle
    // Triangle vertices: (cx - 8, cy - 14), (cx - 8, cy + 14), (cx + 14, cy)
    let tri_x_left = cx - 8.0;
    let tri_x_right = cx + 14.0;
    let tri_y_top = cy - 14.0;
    let tri_y_bottom = cy + 14.0;

    for y in (tri_y_top as u32)..(tri_y_bottom as u32 + 1) {
        for x in (tri_x_left as u32)..(tri_x_right as u32 + 1) {
            let px = x as f32;
            let py = y as f32;
            let progress = (px - tri_x_left) / (tri_x_right - tri_x_left);
            let half_h = 14.0 * (1.0 - progress);
            if px >= tri_x_left && px <= tri_x_right && py >= (cy - half_h) && py <= (cy + half_h) {
                img.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
    }

    // Video extension badge in top-left: "VIDEO" badge (#4338ca)
    let badge_x = 24;
    let badge_y = 20;
    let badge_w = 54;
    let badge_h = 20;
    for y in badge_y..(badge_y + badge_h) {
        for x in badge_x..(badge_x + badge_w) {
            img.put_pixel(x, y, image::Rgba([67, 56, 202, 255]));
        }
    }

    let mut png_bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png_bytes);
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode video placeholder PNG: {e}"))?;

    Ok(png_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_video_path() {
        assert!(is_video_path(Path::new("movie.mp4")));
        assert!(is_video_path(Path::new("video.MP4")));
        assert!(is_video_path(Path::new("clip.mkv")));
        assert!(is_video_path(Path::new("sample.mov")));
        assert!(is_video_path(Path::new("test.webm")));
        assert!(!is_video_path(Path::new("picture.png")));
        assert!(!is_video_path(Path::new("doc.pdf")));
        assert!(!is_video_path(Path::new("note.md")));
    }

    #[test]
    fn test_generate_video_placeholder_png() {
        let bytes = generate_video_placeholder_png("test.mp4").expect("must generate video placeholder");
        assert!(!bytes.is_empty());
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
}
