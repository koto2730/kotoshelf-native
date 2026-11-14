//! PDF rendering module for generating image previews (page 1 thumbnail/cover)
//! of PDF documents across Windows, Linux, and macOS.
//!
//! - Windows: Uses the built-in `Windows.Data.Pdf.PdfDocument` WinRT API.
//! - Linux: Uses `pdftoppm` (poppler-utils), `mutool`, or `gs` (Ghostscript) if present.
//! - macOS: Uses `qlmanage` (built-in QuickLook thumbnailer) or `pdftoppm`.
//! - Fallback: Generates a clean PDF document placeholder card via the `image` crate,
//!   ensuring a broken-image red triangle is never shown regardless of platform.

use std::path::Path;

/// Renders the first page (page 0) of a PDF document given its raw bytes into PNG bytes.
pub fn render_first_page_to_png(pdf_bytes: &[u8]) -> Result<Vec<u8>, String> {
    if pdf_bytes.is_empty() {
        return Err("PDF byte slice is empty".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        match render_windows_pdf(pdf_bytes) {
            Ok(bytes) => return Ok(bytes),
            Err(e) => eprintln!("Windows PDF render failed, falling back to placeholder: {e}"),
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(bytes) = render_linux_pdf(pdf_bytes) {
            return Ok(bytes);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(bytes) = render_macos_pdf(pdf_bytes) {
            return Ok(bytes);
        }
    }

    // Fallback: Generate a clean placeholder card
    generate_pdf_placeholder_png()
}

/// `true` if `path` or `name` has a `.pdf` extension (case-insensitive).
pub fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn render_windows_pdf(pdf_bytes: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Data::Pdf::PdfDocument;
    use windows::Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream};

    let stream = InMemoryRandomAccessStream::new().map_err(|e| format!("Failed to create in-memory stream: {e}"))?;
    let writer = DataWriter::CreateDataWriter(&stream).map_err(|e| format!("Failed to create DataWriter: {e}"))?;

    writer.WriteBytes(pdf_bytes).map_err(|e| format!("Failed to write PDF bytes: {e}"))?;
    writer.StoreAsync().map_err(|e| format!("Failed to store writer: {e}"))?.get().map_err(|e| format!("Failed to complete store: {e}"))?;
    writer.DetachStream().map_err(|e| format!("Failed to detach stream: {e}"))?;

    stream.Seek(0).map_err(|e| format!("Failed to rewind input stream: {e}"))?;

    let doc = PdfDocument::LoadFromStreamAsync(&stream)
        .map_err(|e| format!("Failed to load PDF document: {e}"))?
        .get()
        .map_err(|e| format!("Failed to complete PDF load: {e}"))?;

    let count = doc.PageCount().map_err(|e| format!("Failed to get page count: {e}"))?;
    if count == 0 {
        return Err("PDF document contains 0 pages".to_string());
    }

    let page = doc.GetPage(0).map_err(|e| format!("Failed to get page 0: {e}"))?;

    let output_stream = InMemoryRandomAccessStream::new().map_err(|e| format!("Failed to create output stream: {e}"))?;

    page.RenderToStreamAsync(&output_stream)
        .map_err(|e| format!("Failed to start page render: {e}"))?
        .get()
        .map_err(|e| format!("Failed to complete page render: {e}"))?;

    let size = output_stream.Size().map_err(|e| format!("Failed to get output stream size: {e}"))? as usize;
    if size == 0 {
        return Err("Rendered PDF page size is 0 bytes".to_string());
    }

    output_stream.Seek(0).map_err(|e| format!("Failed to rewind output stream: {e}"))?;

    let reader = DataReader::CreateDataReader(&output_stream).map_err(|e| format!("Failed to create DataReader: {e}"))?;
    reader.LoadAsync(size as u32)
        .map_err(|e| format!("Failed to load reader: {e}"))?
        .get()
        .map_err(|e| format!("Failed to complete reader load: {e}"))?;

    let mut result = vec![0u8; size];
    reader.ReadBytes(&mut result).map_err(|e| format!("Failed to read rendered bytes: {e}"))?;

    Ok(result)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[allow(dead_code)]
fn render_linux_pdf(pdf_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let temp_dir = std::env::temp_dir();
    let id = format!("kotoshelf_pdf_{}_{}", std::process::id(), chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
    let temp_pdf = temp_dir.join(format!("{id}.pdf"));
    let temp_out_prefix = temp_dir.join(&id);
    let temp_out_png = temp_dir.join(format!("{id}.png"));

    std::fs::write(&temp_pdf, pdf_bytes).map_err(|e| e.to_string())?;

    // 1. Try pdftoppm (poppler-utils)
    let pdftoppm_res = std::process::Command::new("pdftoppm")
        .args(["-png", "-f", "1", "-l", "1", "-singlefile", "-r", "150"])
        .arg(&temp_pdf)
        .arg(&temp_out_prefix)
        .output();

    if let Ok(output) = pdftoppm_res {
        if output.status.success() && temp_out_png.exists() {
            let png_bytes = std::fs::read(&temp_out_png).map_err(|e| e.to_string());
            let _ = std::fs::remove_file(&temp_pdf);
            let _ = std::fs::remove_file(&temp_out_png);
            return png_bytes;
        }
    }

    // 2. Try mutool
    let mutool_res = std::process::Command::new("mutool")
        .args(["draw", "-o"])
        .arg(&temp_out_png)
        .args(["-F", "png"])
        .arg(&temp_pdf)
        .arg("1")
        .output();

    if let Ok(output) = mutool_res {
        if output.status.success() && temp_out_png.exists() {
            let png_bytes = std::fs::read(&temp_out_png).map_err(|e| e.to_string());
            let _ = std::fs::remove_file(&temp_pdf);
            let _ = std::fs::remove_file(&temp_out_png);
            return png_bytes;
        }
    }

    // Cleanup temp pdf on failure
    let _ = std::fs::remove_file(&temp_pdf);
    let _ = std::fs::remove_file(&temp_out_png);

    Err("No PDF rendering tool (pdftoppm, mutool) found on Linux".to_string())
}

#[cfg(any(target_os = "macos", test))]
#[allow(dead_code)]
fn render_macos_pdf(pdf_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let temp_dir = std::env::temp_dir();
    let id = format!("kotoshelf_pdf_{}_{}", std::process::id(), chrono::Local::now().timestamp_nanos_opt().unwrap_or(0));
    let temp_pdf = temp_dir.join(format!("{id}.pdf"));
    let temp_out_png = temp_dir.join(format!("{id}.pdf.png"));

    std::fs::write(&temp_pdf, pdf_bytes).map_err(|e| e.to_string())?;

    // 1. Try qlmanage (macOS standard QuickLook thumbnailer)
    let ql_res = std::process::Command::new("qlmanage")
        .args(["-t", "-s", "1024", "-o"])
        .arg(&temp_dir)
        .arg(&temp_pdf)
        .output();

    if let Ok(output) = ql_res {
        if output.status.success() && temp_out_png.exists() {
            let png_bytes = std::fs::read(&temp_out_png).map_err(|e| e.to_string());
            let _ = std::fs::remove_file(&temp_pdf);
            let _ = std::fs::remove_file(&temp_out_png);
            return png_bytes;
        }
    }

    // 2. Fallback to pdftoppm if available
    let _ = std::fs::remove_file(&temp_pdf);
    let _ = std::fs::remove_file(&temp_out_png);

    render_linux_pdf(pdf_bytes)
}

/// Generates a clean, stylized PDF document placeholder PNG image in memory.
pub fn generate_pdf_placeholder_png() -> Result<Vec<u8>, String> {
    let width = 360;
    let height = 240;
    let mut img = image::RgbaImage::new(width, height);

    // Background: Soft slate dark (#1e212b)
    for pixel in img.pixels_mut() {
        *pixel = image::Rgba([30, 33, 43, 255]);
    }

    // Card rectangle: #282c37 with border #3c4252
    let pad_x = 40;
    let pad_y = 25;
    for y in pad_y..(height - pad_y) {
        for x in pad_x..(width - pad_x) {
            let is_border = x == pad_x || x == width - pad_x - 1 || y == pad_y || y == height - pad_y - 1;
            if is_border {
                img.put_pixel(x, y, image::Rgba([60, 66, 82, 255]));
            } else {
                img.put_pixel(x, y, image::Rgba([40, 44, 55, 255]));
            }
        }
    }

    // PDF Red Badge in upper left: #e53935
    let badge_x = pad_x + 20;
    let badge_y = pad_y + 20;
    let badge_w = 48;
    let badge_h = 24;
    for y in badge_y..(badge_y + badge_h) {
        for x in badge_x..(badge_x + badge_w) {
            img.put_pixel(x, y, image::Rgba([229, 57, 53, 255]));
        }
    }

    // Horizontal document line bars representing text
    let lines = [(pad_x + 80, badge_y + 8, 120), (pad_x + 20, badge_y + 45, 240), (pad_x + 20, badge_y + 65, 210), (pad_x + 20, badge_y + 85, 180), (pad_x + 20, badge_y + 105, 230)];
    for (lx, ly, lw) in lines {
        for y in ly..(ly + 6) {
            for x in lx..(lx + lw) {
                if x < width - pad_x - 15 && y < height - pad_y - 10 {
                    img.put_pixel(x, y, image::Rgba([110, 118, 138, 255]));
                }
            }
        }
    }

    let mut png_bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png_bytes);
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode placeholder PNG: {e}"))?;

    Ok(png_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn recognizes_pdf_paths() {
        assert!(is_pdf_path(Path::new("document.pdf")));
        assert!(is_pdf_path(Path::new("document.PDF")));
        assert!(is_pdf_path(Path::new("path/to/my.file.pdf")));
        assert!(!is_pdf_path(Path::new("image.png")));
        assert!(!is_pdf_path(Path::new("document")));
    }

    #[test]
    fn generates_valid_placeholder_png() {
        let bytes = generate_pdf_placeholder_png().expect("must generate placeholder");
        assert!(!bytes.is_empty());
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
}
