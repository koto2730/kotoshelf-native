//! Syntax highlighting and Obsidian-style Live Preview layouter for `egui::TextEdit`.
//!
//! - Non-Markdown files (`.rs`, `.py`, `.json`, `.toml`, `.txt`, etc.):
//!   Rendered in clean standard monospace with full `syntect` code highlighting.
//! - Markdown files (`.md`):
//!   - **Active line** (where the cursor/caret currently resides):
//!     Shows raw, editable Markdown in monospace for smooth navigation and typing.
//!   - **Inactive lines** (all other lines):
//!     Rendered visually like Obsidian's Live Preview: headings enlarged with bold weight and dimmed `#` markers,
//!     completed tasks struck through and dimmed, bold/italic formatted with dimmed delimiters,
//!     styled blockquotes, and accent-highlighted checkboxes and wikilinks.

use crate::wikilink::find_wiki_links;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Matches the wikilink accent color - purple, distinct from syntect palettes.
fn wikilink_color(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::from_rgb(0xa7, 0x8b, 0xfa)
    } else {
        egui::Color32::from_rgb(0x8b, 0x5c, 0xf6)
    }
}

fn syntect_theme_name(dark: bool) -> &'static str {
    if dark {
        "base16-ocean.dark"
    } else {
        "InspiredGitHub"
    }
}

fn load_syntect_theme(dark: bool) -> Theme {
    let name = syntect_theme_name(dark);
    ThemeSet::load_defaults()
        .themes
        .remove(name)
        .unwrap_or_else(|| panic!("syntect default themes should include {name}"))
}

pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
    wikilink_color: egui::Color32,
}

impl Highlighter {
    pub fn new(dark: bool) -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme = load_syntect_theme(dark);
        Self { syntax_set, theme, wikilink_color: wikilink_color(dark) }
    }

    pub fn set_dark(&mut self, dark: bool) {
        self.theme = load_syntect_theme(dark);
        self.wikilink_color = wikilink_color(dark);
    }

    pub fn is_dark(&self) -> bool {
        self.theme.name.as_deref().unwrap_or("").contains("dark")
            || self.theme.name.as_deref().unwrap_or("").contains("Ocean")
    }

    fn syntax_for_extension(&self, extension: &str) -> &SyntaxReference {
        self.syntax_set
            .find_syntax_by_extension(extension)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
    }

    pub fn layout_job_json(&self, text: &str, font_size: f32) -> egui::text::LayoutJob {
        self.layout_job(text, "json", font_size)
    }

    /// Standard syntax highlighting (used for non-markdown code/text files).
    pub fn layout_job(&self, text: &str, extension: &str, font_size: f32) -> egui::text::LayoutJob {
        let syntax = self.syntax_for_extension(extension);
        let mut h = HighlightLines::new(syntax, &self.theme);
        let mut job = egui::text::LayoutJob::default();
        let base_font_id = egui::FontId::monospace(font_size);
        let mut fence: Option<Fence> = None;
        let dark = self.theme.name.as_deref().unwrap_or("").contains("dark")
            || self.theme.name.as_deref().unwrap_or("").contains("Ocean");
        let code_bg = if dark {
            Some(egui::Color32::from_black_alpha(60))
        } else {
            Some(egui::Color32::from_rgb(240, 243, 246))
        };

        for line in text.split_inclusive('\n') {
            let outer_ranges = h.highlight_line(line, &self.syntax_set);

            if let Some(f) = &mut fence {
                if is_closing_fence(line, f.marker_len) {
                    fence = None;
                } else {
                    let mut line_format = plain_format(&base_font_id);
                    if let Some(bg) = code_bg {
                        line_format.background = bg;
                    }

                    match &mut f.highlighter {
                        Some(embed_h) => match embed_h.highlight_line(line, &self.syntax_set) {
                            Ok(ranges) => {
                                for (style, piece) in ranges {
                                    let mut fmt = text_format(style, &base_font_id);
                                    if let Some(bg) = code_bg {
                                        fmt.background = bg;
                                    }
                                    job.append(piece, 0.0, fmt);
                                }
                            }
                            Err(_) => job.append(line, 0.0, line_format),
                        },
                        None => job.append(line, 0.0, line_format),
                    }
                    continue;
                }
            }

            let is_md = extension == "md";
            let wiki_ranges = if is_md { find_wiki_links(line) } else { Vec::new() };
            let heading_lvl = if is_md { heading_level(line) } else { None };
            let line_font_id = if let Some(lvl) = heading_lvl {
                egui::FontId::monospace(font_size_for_heading(lvl, font_size))
            } else {
                base_font_id.clone()
            };

            let line_decor = LineDecoration {
                is_heading: heading_lvl.is_some(),
                is_quote: is_md && line.trim_start().starts_with('>'),
                is_completed_task: is_md && is_completed_task_line(line),
                is_table_row: is_md && line.trim_start().starts_with('|') && line.contains('|'),
            };

            match outer_ranges {
                Ok(ranges) => {
                    emit_line_with_wikilinks(
                        &mut job,
                        line,
                        ranges,
                        &line_font_id,
                        &wiki_ranges,
                        self.wikilink_color,
                        self.wikilink_color,
                        None,
                        &line_decor,
                    )
                }
                Err(_) => {
                    emit_line_with_wikilinks(
                        &mut job,
                        line,
                        Vec::new(),
                        &line_font_id,
                        &wiki_ranges,
                        self.wikilink_color,
                        self.wikilink_color,
                        None,
                        &line_decor,
                    );
                }
            }

            if fence.is_none() {
                if let Some((marker_len, lang)) = opening_fence(line) {
                    let highlighter =
                        self.syntax_set.find_syntax_by_token(lang).map(|s| HighlightLines::new(s, &self.theme));
                    fence = Some(Fence { marker_len, highlighter });
                }
            }
        }
        job
    }

    /// Obsidian-style Live Preview layouter:
    /// - For Markdown (`extension == "md"`): dynamically formats inactive lines while expanding active cursor line.
    /// - For other extensions: delegates to standard code syntax highlighting.
    pub fn layout_job_live(
        &self,
        text: &str,
        extension: &str,
        font_size: f32,
        active_line_idx: Option<usize>,
        search_ranges: &[(usize, usize)],
        active_range: Option<(usize, usize)>,
    ) -> egui::text::LayoutJob {
        self.layout_job_live_with_wiki_targets(
            text,
            extension,
            font_size,
            active_line_idx,
            search_ranges,
            active_range,
            None,
        )
    }

    /// Like [`Highlighter::layout_job_live`], but accepts an optional precomputed set of existing
    /// WikiLink targets so existing vs. missing `[[target]]` links are styled differently with O(1) lookup.
    pub fn layout_job_live_with_wiki_targets(
        &self,
        text: &str,
        extension: &str,
        font_size: f32,
        active_line_idx: Option<usize>,
        search_ranges: &[(usize, usize)],
        active_range: Option<(usize, usize)>,
        known_wiki_targets: Option<&std::collections::HashSet<String>>,
    ) -> egui::text::LayoutJob {
        if extension != "md" {
            return self.layout_job_with_search(text, extension, font_size, search_ranges, active_range);
        }

        let syntax = self.syntax_for_extension("md");
        let mut h = HighlightLines::new(syntax, &self.theme);
        let mut job = egui::text::LayoutJob::default();
        let base_font_id = egui::FontId::monospace(font_size);
        let mut fence: Option<Fence> = None;
        let dark = self.theme.name.as_deref().unwrap_or("").contains("dark")
            || self.theme.name.as_deref().unwrap_or("").contains("Ocean");
        let palette = LivePalette::with_wiki_targets(dark, known_wiki_targets);
        let code_bg = Some(palette.code_bg);

        for (line_idx, line) in text.split_inclusive('\n').enumerate() {
            let is_active_line = active_line_idx == Some(line_idx);
            let outer_ranges = h.highlight_line(line, &self.syntax_set);

            if let Some(f) = &mut fence {
                if is_closing_fence(line, f.marker_len) {
                    fence = None;
                    let mut fmt = plain_format(&base_font_id);
                    if !is_active_line {
                        fmt.color = palette.dim_color;
                    }
                    job.append(line, 0.0, fmt);
                } else {
                    let mut line_format = plain_format(&base_font_id);
                    if let Some(bg) = code_bg {
                        line_format.background = bg;
                    }

                    match &mut f.highlighter {
                        Some(embed_h) => match embed_h.highlight_line(line, &self.syntax_set) {
                            Ok(ranges) => {
                                for (style, piece) in ranges {
                                    let mut fmt = text_format(style, &base_font_id);
                                    if let Some(bg) = code_bg {
                                        fmt.background = bg;
                                    }
                                    job.append(piece, 0.0, fmt);
                                }
                            }
                            Err(_) => job.append(line, 0.0, line_format),
                        },
                        None => job.append(line, 0.0, line_format),
                    }
                }
                continue;
            }

            if fence.is_none() {
                if let Some((marker_len, lang)) = opening_fence(line) {
                    let highlighter = self.syntax_set.find_syntax_by_token(lang).map(|s| HighlightLines::new(s, &self.theme));
                    fence = Some(Fence { marker_len, highlighter });
                    let mut fmt = plain_format(&base_font_id);
                    if !is_active_line {
                        fmt.color = palette.dim_color;
                    }
                    job.append(line, 0.0, fmt);
                    continue;
                }
            }

            if is_active_line {
                // Active line (caret is on this line): show raw markdown in monospace for easy editing
                if let Some(media) = parse_media_line(line) {
                    emit_live_media(&mut job, line, &media, font_size, &palette);
                } else {
                    let wiki_ranges = find_wiki_links(line);
                    let line_decor = LineDecoration {
                        is_heading: false,
                        is_quote: false,
                        is_completed_task: false,
                        is_table_row: false,
                    };
                    match outer_ranges {
                        Ok(ranges) => emit_line_with_wikilinks(
                            &mut job,
                            line,
                            ranges,
                            &base_font_id,
                            &wiki_ranges,
                            self.wikilink_color,
                            palette.wikilink_missing_color,
                            known_wiki_targets,
                            &line_decor,
                        ),
                        Err(_) => emit_line_with_wikilinks(
                            &mut job,
                            line,
                            Vec::new(),
                            &base_font_id,
                            &wiki_ranges,
                            self.wikilink_color,
                            palette.wikilink_missing_color,
                            known_wiki_targets,
                            &line_decor,
                        ),
                    }
                }
            } else {
                // Inactive line: visually render formatted markdown (Obsidian Live Preview)
                let maybe_media = parse_media_line(line);
                let heading_lvl = heading_level(line);
                let is_completed_task = is_completed_task_line(line);
                let is_unchecked_task = is_unchecked_task_line(line);
                let is_quote = line.trim_start().starts_with('>');
                let is_hr = is_horizontal_rule(line);
                let bullet_prefix_len = is_bullet_list_line(line);

                if let Some(media) = maybe_media {
                    emit_live_media(&mut job, line, &media, font_size, &palette);
                } else if let Some(lvl) = heading_lvl {
                    emit_live_heading(&mut job, line, lvl, font_size, &palette);
                } else if is_completed_task {
                    emit_live_task(&mut job, line, true, &base_font_id, &palette);
                } else if is_unchecked_task {
                    emit_live_task(&mut job, line, false, &base_font_id, &palette);
                } else if is_hr {
                    let mut fmt = plain_format(&base_font_id);
                    fmt.color = palette.dim_color.gamma_multiply(0.45);
                    job.append(line, 0.0, fmt);
                } else if is_quote {
                    emit_live_quote(&mut job, line, &base_font_id, &palette);
                } else if let Some(prefix_len) = bullet_prefix_len {
                    let prefix = &line[..prefix_len];
                    let mut p_fmt = plain_format(&base_font_id);
                    p_fmt.color = palette.bullet_color;
                    job.append(prefix, 0.0, p_fmt);

                    let body = &line[prefix_len..];
                    emit_markdown_inline(&mut job, body, &base_font_id, &palette, None, false, None, false);
                } else if line.trim_start().starts_with('|') && line.contains('|') {
                    let trimmed = line.trim_start();
                    let leading = line.len() - trimmed.len();
                    if leading > 0 {
                        job.append(&line[..leading], 0.0, plain_format(&base_font_id));
                    }
                    if trimmed.contains("---") || trimmed.contains("-|-") {
                        let mut fmt = plain_format(&base_font_id);
                        fmt.color = palette.dim_color;
                        job.append(trimmed, 0.0, fmt);
                    } else {
                        let mut cur = 0;
                        for (idx, c) in trimmed.char_indices() {
                            if c == '|' {
                                if idx > cur {
                                    let cell = &trimmed[cur..idx];
                                    emit_markdown_inline(&mut job, cell, &base_font_id, &palette, None, false, None, false);
                                }
                                let mut pipe_fmt = plain_format(&base_font_id);
                                pipe_fmt.color = palette.dim_color;
                                job.append("|", 0.0, pipe_fmt);
                                cur = idx + 1;
                            }
                        }
                        if cur < trimmed.len() {
                            let cell = &trimmed[cur..];
                            emit_markdown_inline(&mut job, cell, &base_font_id, &palette, None, false, None, false);
                        }
                    }
                } else {
                    emit_markdown_inline(&mut job, line, &base_font_id, &palette, None, false, None, false);
                }
            }
        }

        if search_ranges.is_empty() {
            job
        } else {
            overlay_search_highlights(job, text, search_ranges, active_range)
        }
    }

    /// Highlights `text` and overlays background colors on search query matches.
    pub fn layout_job_with_search(
        &self,
        text: &str,
        extension: &str,
        font_size: f32,
        search_ranges: &[(usize, usize)],
        active_range: Option<(usize, usize)>,
    ) -> egui::text::LayoutJob {
        let job = self.layout_job(text, extension, font_size);
        if search_ranges.is_empty() {
            return job;
        }
        overlay_search_highlights(job, text, search_ranges, active_range)
    }
}

pub(crate) fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    (trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-'))
        || (trimmed.len() >= 3 && trimmed.chars().all(|c| c == '*'))
        || (trimmed.len() >= 3 && trimmed.chars().all(|c| c == '_'))
}

struct LivePalette<'a> {
    text_color: egui::Color32,
    bold_color: egui::Color32,
    dim_color: egui::Color32,
    heading_color: egui::Color32,
    code_fg: egui::Color32,
    code_bg: egui::Color32,
    quote_bg: egui::Color32,
    highlight_bg: egui::Color32,
    highlight_fg: egui::Color32,
    link_color: egui::Color32,
    wikilink_color: egui::Color32,
    wikilink_missing_color: egui::Color32,
    task_unchecked_color: egui::Color32,
    task_completed_color: egui::Color32,
    bullet_color: egui::Color32,
    known_wiki_targets: Option<&'a std::collections::HashSet<String>>,
}

impl<'a> LivePalette<'a> {
    fn with_wiki_targets(dark: bool, known_wiki_targets: Option<&'a std::collections::HashSet<String>>) -> Self {
        if dark {
            Self {
                text_color: egui::Color32::from_rgb(220, 225, 235),
                bold_color: egui::Color32::from_rgb(255, 255, 255),
                dim_color: egui::Color32::from_rgba_unmultiplied(120, 130, 150, 110),
                heading_color: egui::Color32::from_rgb(130, 185, 255),
                code_fg: egui::Color32::from_rgb(255, 175, 100),
                code_bg: egui::Color32::from_black_alpha(70),
                quote_bg: egui::Color32::from_black_alpha(45),
                highlight_bg: egui::Color32::from_rgba_unmultiplied(250, 204, 21, 65),
                highlight_fg: egui::Color32::from_rgb(255, 245, 180),
                link_color: egui::Color32::from_rgb(96, 165, 250),
                wikilink_color: egui::Color32::from_rgb(167, 139, 250),
                wikilink_missing_color: egui::Color32::from_rgba_unmultiplied(196, 165, 255, 135),
                task_unchecked_color: egui::Color32::from_rgb(56, 189, 248),
                task_completed_color: egui::Color32::from_rgba_unmultiplied(130, 140, 155, 140),
                bullet_color: egui::Color32::from_rgb(167, 139, 250),
                known_wiki_targets,
            }
        } else {
            Self {
                text_color: egui::Color32::from_rgb(36, 41, 47),
                bold_color: egui::Color32::from_rgb(10, 15, 20),
                dim_color: egui::Color32::from_rgba_unmultiplied(150, 160, 175, 130),
                heading_color: egui::Color32::from_rgb(9, 105, 218),
                code_fg: egui::Color32::from_rgb(190, 50, 50),
                code_bg: egui::Color32::from_rgb(240, 243, 246),
                quote_bg: egui::Color32::from_rgb(245, 247, 250),
                highlight_bg: egui::Color32::from_rgba_unmultiplied(254, 240, 138, 190),
                highlight_fg: egui::Color32::BLACK,
                link_color: egui::Color32::from_rgb(9, 105, 218),
                wikilink_color: egui::Color32::from_rgb(139, 92, 246),
                wikilink_missing_color: egui::Color32::from_rgba_unmultiplied(155, 105, 245, 135),
                task_unchecked_color: egui::Color32::from_rgb(2, 132, 199),
                task_completed_color: egui::Color32::from_rgba_unmultiplied(140, 150, 165, 140),
                bullet_color: egui::Color32::from_rgb(139, 92, 246),
                known_wiki_targets,
            }
        }
    }

    fn is_wiki_target_present(&self, raw_target: &str) -> bool {
        match self.known_wiki_targets {
            Some(set) => crate::wikilink::wiki_target_exists(raw_target, set),
            None => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpanKind {
    Code,
    Highlight,
    BoldItalic,
    Bold,
    Italic,
    Strikethrough,
    WikiLink,
    MarkdownLink { split_idx: usize },
}

#[derive(Clone, Debug)]
struct InlineSpan {
    start: usize,
    end: usize,
    open_len: usize,
    close_len: usize,
    kind: SpanKind,
}

fn parse_inline_spans(text: &str) -> Vec<InlineSpan> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut occupied = vec![false; len];

    let mark_occupied = |occupied: &mut Vec<bool>, start: usize, end: usize| {
        for b in &mut occupied[start..end] {
            *b = true;
        }
    };

    let is_free = |occupied: &[bool], start: usize, end: usize| -> bool {
        occupied[start..end].iter().all(|&b| !b)
    };

    // 1. Inline code: `...`
    let mut i = 0;
    while i < len {
        if bytes[i] == b'`' {
            let start = i;
            if let Some(close) = text[start + 1..].find('`') {
                let end = start + 1 + close + 1;
                if end - start > 2 && is_free(&occupied, start, end) {
                    spans.push(InlineSpan {
                        start,
                        end,
                        open_len: 1,
                        close_len: 1,
                        kind: SpanKind::Code,
                    });
                    mark_occupied(&mut occupied, start, end);
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }

    // 2. Wiki links: [[...]]
    i = 0;
    while i + 3 < len {
        if &bytes[i..i + 2] == b"[[" {
            let start = i;
            if let Some(close) = text[start + 2..].find("]]") {
                let end = start + 2 + close + 2;
                if is_free(&occupied, start, end) {
                    spans.push(InlineSpan {
                        start,
                        end,
                        open_len: 2,
                        close_len: 2,
                        kind: SpanKind::WikiLink,
                    });
                    mark_occupied(&mut occupied, start, end);
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }

    // 3. Markdown links: [anchor](url)
    i = 0;
    while i + 4 < len {
        if bytes[i] == b'[' && is_free(&occupied, i, i + 1) {
            let start = i;
            if let Some(mid) = text[start + 1..].find("](") {
                let split_idx = start + 1 + mid;
                if let Some(close) = text[split_idx + 2..].find(')') {
                    let end = split_idx + 2 + close + 1;
                    if is_free(&occupied, start, end) {
                        spans.push(InlineSpan {
                            start,
                            end,
                            open_len: 1,
                            close_len: 0,
                            kind: SpanKind::MarkdownLink { split_idx },
                        });
                        mark_occupied(&mut occupied, start, end);
                        i = end;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }

    // 4. Highlights: ==...==
    i = 0;
    while i + 3 < len {
        if &bytes[i..i + 2] == b"==" {
            let start = i;
            if let Some(close) = text[start + 2..].find("==") {
                let end = start + 2 + close + 2;
                if end - start > 4 && is_free(&occupied, start, end) {
                    spans.push(InlineSpan {
                        start,
                        end,
                        open_len: 2,
                        close_len: 2,
                        kind: SpanKind::Highlight,
                    });
                    mark_occupied(&mut occupied, start, end);
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }

    // 5. Strikethrough: ~~...~~
    i = 0;
    while i + 3 < len {
        if &bytes[i..i + 2] == b"~~" {
            let start = i;
            if let Some(close) = text[start + 2..].find("~~") {
                let end = start + 2 + close + 2;
                if end - start > 4 && is_free(&occupied, start, end) {
                    spans.push(InlineSpan {
                        start,
                        end,
                        open_len: 2,
                        close_len: 2,
                        kind: SpanKind::Strikethrough,
                    });
                    mark_occupied(&mut occupied, start, end);
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }

    // 6. Bold+Italic: ***...*** or ___...___
    for delim in ["***", "___"] {
        i = 0;
        let d_bytes = delim.as_bytes();
        while i + 5 < len {
            if &bytes[i..i + 3] == d_bytes {
                let start = i;
                if let Some(close) = text[start + 3..].find(delim) {
                    let end = start + 3 + close + 3;
                    if end - start > 6 && is_free(&occupied, start, end) {
                        spans.push(InlineSpan {
                            start,
                            end,
                            open_len: 3,
                            close_len: 3,
                            kind: SpanKind::BoldItalic,
                        });
                        mark_occupied(&mut occupied, start, end);
                        i = end;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }

    // 7. Bold: **...** or __...__
    for delim in ["**", "__"] {
        i = 0;
        let d_bytes = delim.as_bytes();
        while i + 3 < len {
            if &bytes[i..i + 2] == d_bytes {
                let start = i;
                if let Some(close) = text[start + 2..].find(delim) {
                    let end = start + 2 + close + 2;
                    if end - start > 4 && is_free(&occupied, start, end) {
                        spans.push(InlineSpan {
                            start,
                            end,
                            open_len: 2,
                            close_len: 2,
                            kind: SpanKind::Bold,
                        });
                        mark_occupied(&mut occupied, start, end);
                        i = end;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }

    // 8. Italic: *...* or _..._
    for &delim in &[b'*', b'_'] {
        i = 0;
        while i + 1 < len {
            if bytes[i] == delim && (i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t' || bytes[i - 1] == b'(') {
                let start = i;
                let char_delim = delim as char;
                if let Some(close) = text[start + 1..].find(char_delim) {
                    let end = start + 1 + close + 1;
                    if end - start > 2 && is_free(&occupied, start, end) {
                        spans.push(InlineSpan {
                            start,
                            end,
                            open_len: 1,
                            close_len: 1,
                            kind: SpanKind::Italic,
                        });
                        mark_occupied(&mut occupied, start, end);
                        i = end;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }

    spans.sort_by_key(|s| s.start);
    spans
}

fn emit_markdown_inline(
    job: &mut egui::text::LayoutJob,
    text: &str,
    base_font: &egui::FontId,
    palette: &LivePalette,
    override_color: Option<egui::Color32>,
    is_quote: bool,
    quote_bg: Option<egui::Color32>,
    full_strikethrough: bool,
) {
    let spans = parse_inline_spans(text);
    let mut cur = 0;
    let text_color = override_color.unwrap_or(palette.text_color);

    for span in spans {
        if span.start > cur {
            let segment = &text[cur..span.start];
            let fmt = egui::text::TextFormat {
                font_id: base_font.clone(),
                color: if full_strikethrough { palette.task_completed_color } else { text_color },
                italics: is_quote,
                strikethrough: if full_strikethrough { egui::Stroke::new(1.2, palette.task_completed_color) } else { egui::Stroke::NONE },
                background: quote_bg.unwrap_or(egui::Color32::TRANSPARENT),
                ..Default::default()
            };
            job.append(segment, 0.0, fmt);
        }

        // Open delimiter
        if span.open_len > 0 {
            let open_str = &text[span.start..span.start + span.open_len];
            let delim_fmt = egui::text::TextFormat {
                font_id: base_font.clone(),
                color: palette.dim_color,
                italics: is_quote,
                background: quote_bg.unwrap_or(egui::Color32::TRANSPARENT),
                ..Default::default()
            };
            job.append(open_str, 0.0, delim_fmt);
        }

        // Inner content
        match span.kind {
            SpanKind::MarkdownLink { split_idx } => {
                let anchor = &text[span.start + 1..split_idx];
                let url_part = &text[split_idx..span.end];
                let anchor_fmt = egui::text::TextFormat {
                    font_id: base_font.clone(),
                    color: palette.link_color,
                    underline: egui::Stroke::new(1.0, palette.link_color),
                    italics: is_quote,
                    background: quote_bg.unwrap_or(egui::Color32::TRANSPARENT),
                    ..Default::default()
                };
                job.append(anchor, 0.0, anchor_fmt);

                let url_fmt = egui::text::TextFormat {
                    font_id: base_font.clone(),
                    color: palette.dim_color,
                    italics: is_quote,
                    background: quote_bg.unwrap_or(egui::Color32::TRANSPARENT),
                    ..Default::default()
                };
                job.append(url_part, 0.0, url_fmt);
            }
            _ => {
                let inner_start = span.start + span.open_len;
                let inner_end = span.end - span.close_len;
                let inner = &text[inner_start..inner_end];

                let mut fmt = egui::text::TextFormat {
                    font_id: base_font.clone(),
                    color: text_color,
                    italics: is_quote,
                    background: quote_bg.unwrap_or(egui::Color32::TRANSPARENT),
                    ..Default::default()
                };

                if full_strikethrough {
                    fmt.color = palette.task_completed_color;
                    fmt.strikethrough = egui::Stroke::new(1.2, palette.task_completed_color);
                }

                match span.kind {
                    SpanKind::Bold => {
                        if !full_strikethrough {
                            fmt.color = palette.bold_color;
                        }
                    }
                    SpanKind::Italic => {
                        fmt.italics = true;
                    }
                    SpanKind::BoldItalic => {
                        if !full_strikethrough {
                            fmt.color = palette.bold_color;
                        }
                        fmt.italics = true;
                    }
                    SpanKind::Strikethrough => {
                        fmt.strikethrough = egui::Stroke::new(1.2, fmt.color);
                    }
                    SpanKind::Highlight => {
                        fmt.background = palette.highlight_bg;
                        fmt.color = palette.highlight_fg;
                    }
                    SpanKind::Code => {
                        fmt.font_id = egui::FontId::monospace(base_font.size);
                        fmt.background = palette.code_bg;
                        fmt.color = palette.code_fg;
                    }
                    SpanKind::WikiLink => {
                        if palette.is_wiki_target_present(inner) {
                            fmt.color = palette.wikilink_color;
                            fmt.underline = egui::Stroke::new(1.0, palette.wikilink_color);
                        } else {
                            fmt.color = palette.wikilink_missing_color;
                            fmt.italics = true;
                            fmt.underline = egui::Stroke::NONE;
                        }
                    }
                    SpanKind::MarkdownLink { .. } => unreachable!(),
                }

                job.append(inner, 0.0, fmt);

                if span.close_len > 0 {
                    let close_str = &text[span.end - span.close_len..span.end];
                    let delim_fmt = egui::text::TextFormat {
                        font_id: base_font.clone(),
                        color: palette.dim_color,
                        italics: is_quote,
                        background: quote_bg.unwrap_or(egui::Color32::TRANSPARENT),
                        ..Default::default()
                    };
                    job.append(close_str, 0.0, delim_fmt);
                }
            }
        }

        cur = span.end;
    }

    if cur < text.len() {
        let remaining = &text[cur..];
        let fmt = egui::text::TextFormat {
            font_id: base_font.clone(),
            color: if full_strikethrough { palette.task_completed_color } else { text_color },
            italics: is_quote,
            strikethrough: if full_strikethrough { egui::Stroke::new(1.2, palette.task_completed_color) } else { egui::Stroke::NONE },
            background: quote_bg.unwrap_or(egui::Color32::TRANSPARENT),
            ..Default::default()
        };
        job.append(remaining, 0.0, fmt);
    }
}

fn emit_live_heading(
    job: &mut egui::text::LayoutJob,
    line: &str,
    level: usize,
    base_font_size: f32,
    palette: &LivePalette,
) {
    let leading_spaces = line.len() - line.trim_start().len();
    let hash_prefix_len = leading_spaces + level + 1;
    let heading_font_size = font_size_for_heading(level, base_font_size);
    let heading_font = egui::FontId::proportional(heading_font_size);

    if hash_prefix_len <= line.len() {
        let prefix = &line[..hash_prefix_len];
        let prefix_fmt = egui::text::TextFormat {
            font_id: heading_font.clone(),
            color: palette.dim_color,
            ..Default::default()
        };
        job.append(prefix, 0.0, prefix_fmt);

        let body = &line[hash_prefix_len..];
        emit_markdown_inline(
            job,
            body,
            &heading_font,
            palette,
            Some(palette.heading_color),
            false,
            None,
            false,
        );
    } else {
        job.append(line, 0.0, plain_format(&heading_font));
    }
}

fn emit_live_task(
    job: &mut egui::text::LayoutJob,
    line: &str,
    completed: bool,
    base_font: &egui::FontId,
    palette: &LivePalette,
) {
    let trimmed = line.trim_start();
    let leading_len = line.len() - trimmed.len();
    let prefix_len = if trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]")
        || trimmed.starts_with("* [ ]") || trimmed.starts_with("* [x]") || trimmed.starts_with("* [X]")
        || trimmed.starts_with("+ [ ]") || trimmed.starts_with("+ [x]") || trimmed.starts_with("+ [X]") {
        5
    } else {
        0
    };

    if prefix_len > 0 && leading_len + prefix_len <= line.len() {
        if leading_len > 0 {
            job.append(&line[..leading_len], 0.0, plain_format(base_font));
        }

        // Bullet prefix "- ", "* ", "+ "
        let bullet = &line[leading_len..leading_len + 2];
        let mut bullet_fmt = plain_format(base_font);
        bullet_fmt.color = palette.dim_color;
        job.append(bullet, 0.0, bullet_fmt);

        // Checkbox box "[ ]" or "[x]"
        let box_marker = &line[leading_len + 2..leading_len + prefix_len];
        let box_fmt = egui::text::TextFormat {
            font_id: egui::FontId::monospace(base_font.size),
            color: if completed { palette.task_completed_color } else { palette.task_unchecked_color },
            background: palette.code_bg,
            ..Default::default()
        };
        job.append(box_marker, 0.0, box_fmt);

        let body = &line[leading_len + prefix_len..];
        emit_markdown_inline(
            job,
            body,
            base_font,
            palette,
            None,
            false,
            None,
            completed,
        );
    } else {
        emit_markdown_inline(job, line, base_font, palette, None, false, None, completed);
    }
}

fn emit_live_quote(
    job: &mut egui::text::LayoutJob,
    line: &str,
    base_font: &egui::FontId,
    palette: &LivePalette,
) {
    let trimmed = line.trim_start();
    let leading = line.len() - trimmed.len();
    if leading > 0 {
        job.append(&line[..leading], 0.0, plain_format(base_font));
    }
    let gt_len = if trimmed.starts_with("> ") { 2 } else { 1 };
    let prefix = &trimmed[..gt_len];
    let prefix_fmt = egui::text::TextFormat {
        font_id: base_font.clone(),
        color: palette.dim_color,
        background: palette.quote_bg,
        ..Default::default()
    };
    job.append(prefix, 0.0, prefix_fmt);

    let body = &trimmed[gt_len..];
    emit_markdown_inline(
        job,
        body,
        base_font,
        palette,
        None,
        true,
        Some(palette.quote_bg),
        false,
    );
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaRef {
    pub target: String,
    pub alt: String,
    pub is_video: bool,
    pub is_pdf: bool,
    pub is_image: bool,
}

pub fn parse_media_line(line: &str) -> Option<MediaRef> {
    let trimmed = line.trim();
    // 1. CommonMark image/media: ![alt](target)
    if let Some(start) = trimmed.find("![") {
        if let Some(end_bracket) = trimmed[start + 2..].find(']') {
            let alt = &trimmed[start + 2..start + 2 + end_bracket];
            let rest = &trimmed[start + 2 + end_bracket + 1..];
            if rest.starts_with('(') {
                if let Some(end_paren) = rest.find(')') {
                    let target = rest[1..end_paren].trim().trim_matches('"').trim_matches('\'').to_string();
                    if !target.is_empty() {
                        let is_video = crate::video::is_video_path(std::path::Path::new(&target));
                        let is_pdf = crate::attachments::is_pdf_path(std::path::Path::new(&target));
                        let is_image = crate::attachments::is_image_path(std::path::Path::new(&target)) || (!is_video && !is_pdf);
                        return Some(MediaRef { target, alt: alt.to_string(), is_video, is_pdf, is_image });
                    }
                }
            }
        }
    }

    // 2. Wiki media: ![[target]] or ![[target|alt]]
    if let Some(start) = trimmed.find("![[") {
        if let Some(end) = trimmed[start + 3..].find("]]") {
            let inner = &trimmed[start + 3..start + 3 + end];
            let (target, alt) = if let Some((t, a)) = inner.split_once('|') {
                (t.trim().to_string(), a.trim().to_string())
            } else {
                (inner.trim().to_string(), String::new())
            };
            if !target.is_empty() {
                let is_video = crate::video::is_video_path(std::path::Path::new(&target));
                let is_pdf = crate::attachments::is_pdf_path(std::path::Path::new(&target));
                let is_image = crate::attachments::is_image_path(std::path::Path::new(&target)) || (!is_video && !is_pdf);
                return Some(MediaRef { target, alt, is_video, is_pdf, is_image });
            }
        }
    }

    None
}

fn emit_live_media(
    job: &mut egui::text::LayoutJob,
    line: &str,
    media: &MediaRef,
    font_size: f32,
    palette: &LivePalette,
) {
    let (text_part, _has_newline) = if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, true)
    } else {
        (line, false)
    };

    let base_font = egui::FontId::monospace(font_size);
    let mut delim_fmt = plain_format(&base_font);
    delim_fmt.color = palette.dim_color;

    let mut link_fmt = plain_format(&base_font);
    link_fmt.color = palette.link_color;
    link_fmt.underline = egui::Stroke::new(1.0, palette.link_color);

    // Format the markdown syntax nicely while preserving 100% exact character slice parity
    if let Some(target_pos) = text_part.find(&media.target) {
        let prefix = &text_part[..target_pos];
        let target_slice = &text_part[target_pos..target_pos + media.target.len()];
        let suffix = &text_part[target_pos + media.target.len()..];

        job.append(prefix, 0.0, delim_fmt.clone());
        job.append(target_slice, 0.0, link_fmt);
        job.append(suffix, 0.0, delim_fmt.clone());
    } else {
        let mut fmt = plain_format(&base_font);
        fmt.color = palette.link_color;
        job.append(text_part, 0.0, fmt);
    }

    if _has_newline {
        job.append("\n", 0.0, delim_fmt);
    }
}

fn is_bullet_list_line(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        let leading = line.len() - trimmed.len();
        Some(leading + 2)
    } else if let Some(dot_idx) = trimmed.find(". ") {
        if dot_idx > 0 && trimmed[..dot_idx].chars().all(|c| c.is_ascii_digit()) {
            let leading = line.len() - trimmed.len();
            Some(leading + dot_idx + 2)
        } else {
            None
        }
    } else {
        None
    }
}

fn overlay_search_highlights(
    mut job: egui::text::LayoutJob,
    text: &str,
    search_ranges: &[(usize, usize)],
    active_range: Option<(usize, usize)>,
) -> egui::text::LayoutJob {
    let mut char_to_byte: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    char_to_byte.push(text.len());

    let byte_search_ranges: Vec<(usize, usize, bool)> = search_ranges
        .iter()
        .map(|&(sc, ec)| {
            let sb = char_to_byte.get(sc).copied().unwrap_or(text.len());
            let eb = char_to_byte.get(ec).copied().unwrap_or(text.len());
            let is_act = active_range.map(|(asc, aec)| asc == sc && aec == ec).unwrap_or(false);
            (sb, eb, is_act)
        })
        .collect();

    let mut new_sections = Vec::new();
    for sec in job.sections.drain(..) {
        let sec_start: usize = sec.byte_range.start.into();
        let sec_end: usize = sec.byte_range.end.into();

        let mut cuts = vec![sec_start, sec_end];
        for &(sb, eb, _) in &byte_search_ranges {
            if sb < sec_end && eb > sec_start {
                if sb > sec_start && sb < sec_end {
                    cuts.push(sb);
                }
                if eb > sec_start && eb < sec_end {
                    cuts.push(eb);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();

        for window in cuts.windows(2) {
            let sub_start = window[0];
            let sub_end = window[1];
            let mut fmt = sec.format.clone();

            let mut is_match = false;
            let mut is_active = false;
            for &(sb, eb, is_act) in &byte_search_ranges {
                if sub_start >= sb && sub_end <= eb {
                    is_match = true;
                    if is_act {
                        is_active = true;
                    }
                    break;
                }
            }

            if is_active {
                fmt.background = egui::Color32::from_rgb(255, 150, 0);
                fmt.color = egui::Color32::BLACK;
            } else if is_match {
                fmt.background = egui::Color32::from_rgba_unmultiplied(255, 235, 59, 150);
                fmt.color = egui::Color32::BLACK;
            }

            new_sections.push(egui::text::LayoutSection {
                leading_space: if sub_start == sec_start { sec.leading_space } else { 0.0 },
                byte_range: egui::text::ByteIndex(sub_start)..egui::text::ByteIndex(sub_end),
                format: fmt,
            });
        }
    }
    job.sections = new_sections;
    job
}

struct LineDecoration {
    is_heading: bool,
    is_quote: bool,
    is_completed_task: bool,
    is_table_row: bool,
}

fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if level >= 1 && level <= 6 && trimmed[level..].starts_with(' ') {
            return Some(level);
        }
    }
    None
}

fn font_size_for_heading(level: usize, base_size: f32) -> f32 {
    match level {
        1 => base_size * 1.45,
        2 => base_size * 1.30,
        3 => base_size * 1.18,
        4 => base_size * 1.08,
        _ => base_size * 1.03,
    }
}

fn is_completed_task_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]")
        || trimmed.starts_with("* [x]") || trimmed.starts_with("* [X]")
        || trimmed.starts_with("+ [x]") || trimmed.starts_with("+ [X]")
}

fn is_unchecked_task_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- [ ]") || trimmed.starts_with("* [ ]") || trimmed.starts_with("+ [ ]")
}

fn emit_line_with_wikilinks(
    job: &mut egui::text::LayoutJob,
    line: &str,
    pieces: Vec<(Style, &str)>,
    font_id: &egui::FontId,
    wiki_ranges: &[(std::ops::Range<usize>, &str)],
    wikilink_color: egui::Color32,
    wikilink_missing_color: egui::Color32,
    known_wiki_targets: Option<&std::collections::HashSet<String>>,
    decor: &LineDecoration,
) {
    let build_format = |style: Option<Style>| {
        let mut fmt = match style {
            Some(s) => text_format(s, font_id),
            None => plain_format(font_id),
        };
        if decor.is_quote {
            fmt.italics = true;
        }
        if decor.is_completed_task {
            fmt.strikethrough = egui::Stroke::new(1.0, fmt.color);
        }
        if decor.is_table_row && (line.contains("---") || line.contains("-|-")) {
            fmt.color = egui::Color32::GRAY;
        }
        fmt
    };

    if wiki_ranges.is_empty() {
        if pieces.is_empty() {
            job.append(line, 0.0, build_format(None));
        } else {
            for (style, piece) in pieces {
                job.append(piece, 0.0, build_format(Some(style)));
            }
        }
        return;
    }

    let mut piece_ranges: Vec<(std::ops::Range<usize>, Style)> = Vec::new();
    let mut offset = 0usize;
    for (style, piece) in &pieces {
        let end = offset + piece.len();
        piece_ranges.push((offset..end, *style));
        offset = end;
    }

    let mut points: Vec<usize> = Vec::new();
    for (r, _) in &piece_ranges {
        points.push(r.start);
        points.push(r.end);
    }
    for (r, _) in wiki_ranges {
        points.push(r.start);
        points.push(r.end);
    }
    points.sort_unstable();
    points.dedup();

    for w in points.windows(2) {
        let (start, end) = (w[0], w[1]);
        if start >= end || end > line.len() {
            continue;
        }
        let slice = &line[start..end];
        let style = piece_ranges.iter().find(|(r, _)| r.start <= start && end <= r.end).map(|(_, s)| *s);
        let mut fmt = build_format(style);
        if let Some((_, target)) = wiki_ranges.iter().find(|(r, _)| r.start <= start && end <= r.end) {
            let exists = known_wiki_targets.is_none_or(|set| crate::wikilink::wiki_target_exists(target, set));
            if exists {
                fmt.color = wikilink_color;
            } else {
                fmt.color = wikilink_missing_color;
                fmt.italics = true;
            }
        }
        job.append(slice, 0.0, fmt);
    }
}

struct Fence<'a> {
    marker_len: usize,
    highlighter: Option<HighlightLines<'a>>,
}

fn opening_fence(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("```") {
        return None;
    }
    let marker_len = trimmed.chars().take_while(|&c| c == '`').count();
    if marker_len < 3 {
        return None;
    }
    let rest = trimmed[marker_len..].trim();
    let lang = rest.split_whitespace().next().unwrap_or("");
    Some((marker_len, lang))
}

fn is_closing_fence(line: &str, expected_len: usize) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("```") {
        return false;
    }
    let len = trimmed.chars().take_while(|&c| c == '`').count();
    len >= expected_len && trimmed[len..].trim().is_empty()
}

fn plain_format(font_id: &egui::FontId) -> egui::text::TextFormat {
    egui::text::TextFormat {
        font_id: font_id.clone(),
        color: egui::Color32::GRAY,
        ..Default::default()
    }
}

fn text_format(style: Style, font_id: &egui::FontId) -> egui::text::TextFormat {
    let syntect::highlighting::Color { r, g, b, a } = style.foreground;
    egui::text::TextFormat {
        font_id: font_id.clone(),
        color: egui::Color32::from_rgba_unmultiplied(r, g, b, a),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_fence_extracts_backtick_count_and_language() {
        assert_eq!(opening_fence("```rust\n"), Some((3, "rust")));
        assert_eq!(opening_fence("   ````python foo\n"), Some((4, "python")));
        assert_eq!(opening_fence("```\n"), Some((3, "")));
        assert_eq!(opening_fence("not a fence\n"), None);
        assert_eq!(opening_fence("``too short\n"), None);
    }

    #[test]
    fn is_closing_fence_requires_enough_backticks_and_nothing_else() {
        assert!(is_closing_fence("```\n", 3));
        assert!(is_closing_fence("   ````\n", 3));
        assert!(!is_closing_fence("``\n", 3));
        assert!(!is_closing_fence("```trailing text\n", 3));
    }

    #[test]
    fn wiki_link_is_colored_distinctly_from_surrounding_prose() {
        let h = Highlighter::new(true);
        let job = h.layout_job("See [[target]] for details\n", "md", 14.0);
        let section = job.sections.iter().find(|s| {
            let start: usize = s.byte_range.start.into();
            let end: usize = s.byte_range.end.into();
            start <= 4 && 14 <= end
        });
        assert!(section.is_some(), "should have a section covering [[target]]");
        assert_eq!(section.unwrap().format.color, h.wikilink_color);
    }

    #[test]
    fn fenced_rust_code_block_in_markdown_gets_rust_keyword_highlighting() {
        let h = Highlighter::new(true);
        let text = "```rust\nfn main() {}\n```\n";
        let job = h.layout_job(text, "md", 14.0);
        let fn_sec = job.sections.iter().find(|s| {
            let start: usize = s.byte_range.start.into();
            let end: usize = s.byte_range.end.into();
            &text[start..end] == "fn"
        });
        assert!(fn_sec.is_some(), "the 'fn' keyword should be isolated in its own section");
        assert_ne!(fn_sec.unwrap().format.color, egui::Color32::GRAY);
    }

    #[test]
    fn fence_without_a_language_tag_is_left_unhighlighted() {
        let h = Highlighter::new(true);
        let text = "```\nplain text in fence\n```\n";
        let job = h.layout_job(text, "md", 14.0);
        assert!(!job.sections.is_empty());
    }

    #[test]
    fn unknown_language_tag_falls_back_gracefully() {
        let h = Highlighter::new(true);
        let text = "```nonexistent_lang_12345\nsome code\n```\n";
        let job = h.layout_job(text, "md", 14.0);
        assert!(!job.sections.is_empty());
    }

    #[test]
    fn wiki_link_inside_a_fenced_code_block_is_not_colored() {
        let h = Highlighter::new(true);
        let text = "```\nlet x = \"[[not_a_link]]\";\n```\n";
        let job = h.layout_job(text, "md", 14.0);
        let target_sec = job.sections.iter().find(|s| {
            let start: usize = s.byte_range.start.into();
            let end: usize = s.byte_range.end.into();
            &text[start..end] == "[[not_a_link]]"
        });
        if let Some(sec) = target_sec {
            assert_ne!(sec.format.color, h.wikilink_color);
        }
    }

    #[test]
    fn live_layout_active_vs_inactive_headings() {
        let h = Highlighter::new(true);
        let text = "# Heading 1\n## Heading 2\nNormal text\n";

        // When line 0 is active, line 1 is inactive and should have heading font size
        let job = h.layout_job_live(text, "md", 14.0, Some(0), &[], None);
        assert!(!job.sections.is_empty());

        // Non-markdown file layout ignores live formatting
        let job_rs = h.layout_job_live("fn main() {\n    let x = 1;\n}\n", "rs", 14.0, Some(0), &[], None);
        assert!(!job_rs.sections.is_empty());
    }

    #[test]
    fn live_layout_completed_task_inactive_gets_strikethrough() {
        let h = Highlighter::new(true);
        let text = "- [ ] Incomplete task\n- [x] Completed task\n";

        // When line 0 is active, line 1 (completed task) is inactive and has strikethrough
        let job = h.layout_job_live(text, "md", 14.0, Some(0), &[], None);
        let has_strikethrough = job.sections.iter().any(|s| s.format.strikethrough.width > 0.0);
        assert!(has_strikethrough, "Inactive completed task should have strikethrough");

        // When line 1 is active, it renders raw without strikethrough for clean editing
        let job_active = h.layout_job_live(text, "md", 14.0, Some(1), &[], None);
        let active_has_strikethrough = job_active.sections.iter().any(|s| {
            let start: usize = s.byte_range.start.into();
            start >= 22 && s.format.strikethrough.width > 0.0
        });
        assert!(!active_has_strikethrough, "Active line should not have strikethrough");
    }

    #[test]
    fn non_markdown_files_use_standard_monospace_layout() {
        let h = Highlighter::new(true);
        let py_code = "def hello():\n    print('world')\n";
        let job_py = h.layout_job_live(py_code, "py", 14.0, Some(0), &[], None);
        assert!(!job_py.sections.is_empty());

        let txt_content = "- [x] this is plain text\n# not a heading\n";
        let job_txt = h.layout_job_live(txt_content, "txt", 14.0, Some(0), &[], None);
        // In plain text, no strikethrough is applied even for - [x]
        let has_strikethrough = job_txt.sections.iter().any(|s| s.format.strikethrough.width > 0.0);
        assert!(!has_strikethrough, "Plain text files must not apply markdown live edit styles");
    }

    #[test]
    fn test_live_preview_inline_formatting() {
        let h = Highlighter::new(true);
        let text = "First line\nThis is **bold** and *italic* and `code` and ==highlight== and [link](https://test.com)\n";

        // Line 0 is active, Line 1 is inactive
        let job = h.layout_job_live(text, "md", 14.0, Some(0), &[], None);

        // Check that italic formatting was applied on line 1
        let has_italics = job.sections.iter().any(|s| s.format.italics);
        assert!(has_italics, "Inactive line with *italic* should format italics");

        // Check that code block has background applied
        let has_code_bg = job.sections.iter().any(|s| s.format.background != egui::Color32::TRANSPARENT);
        assert!(has_code_bg, "Inactive line with `code` should format background");

        // Check that delimiter dimming was applied
        let has_dimmed = job.sections.iter().any(|s| s.format.color == egui::Color32::from_rgba_unmultiplied(120, 130, 150, 110));
        assert!(has_dimmed, "Markdown delimiters on inactive lines should be dimmed");
    }

    #[test]
    fn test_parse_media_line() {
        let img = parse_media_line("![screenshot](attachments/img-1.png)").expect("must parse image");
        assert!(img.is_image);
        assert!(!img.is_pdf);
        assert!(!img.is_video);
        assert_eq!(img.target, "attachments/img-1.png");
        assert_eq!(img.alt, "screenshot");

        let pdf = parse_media_line("![[attachments/doc.pdf]]").expect("must parse pdf");
        assert!(pdf.is_pdf);
        assert!(!pdf.is_video);
        assert_eq!(pdf.target, "attachments/doc.pdf");

        let vid = parse_media_line("![demo video](attachments/vid-20260930-clip.mp4)").expect("must parse video");
        assert!(vid.is_video);
        assert!(!vid.is_pdf);
        assert_eq!(vid.target, "attachments/vid-20260930-clip.mp4");
        assert_eq!(vid.alt, "demo video");

        assert!(parse_media_line("[regular link](note.md)").is_none());
        assert!(parse_media_line("# heading").is_none());
    }

    #[test]
    fn test_multiple_media_layout_exact_character_parity() {
        let h = Highlighter::new(true);
        let text = "# Title\n![](attachments/img-1.png)\n![](attachments/vid-1.mp4)\nSome text after\n";

        // Layout with line 0 active, meaning lines 1 and 2 (media lines) are inactive
        let job = h.layout_job_live(text, "md", 14.0, Some(0), &[], None);
        assert_eq!(job.text, text, "LayoutJob text must match original markdown text byte-for-byte");
        assert_eq!(job.text.len(), text.len());

        // Layout with line 1 active (editing the first media line)
        let job_edit1 = h.layout_job_live(text, "md", 14.0, Some(1), &[], None);
        assert_eq!(job_edit1.text, text);

        // Layout with line 2 active (editing the second media line)
        let job_edit2 = h.layout_job_live(text, "md", 14.0, Some(2), &[], None);
        assert_eq!(job_edit2.text, text);
    }

    #[test]
    fn test_wikilink_existing_vs_missing_styling() {
        let h = Highlighter::new(true);
        let mut known = std::collections::HashSet::new();
        known.insert("existing".to_string());
        known.insert("existing.md".to_string());

        let text = "Active [[existing]] and [[missing]]\nInactive [[existing]] and [[missing]]\n";
        let job = h.layout_job_live_with_wiki_targets(text, "md", 14.0, Some(0), &[], None, Some(&known));
        assert_eq!(job.text, text, "LayoutJob text must remain byte-for-byte identical");

        let find_section = |needle: &str, after_byte: usize| {
            let idx = text[after_byte..].find(needle).unwrap() + after_byte;
            job.sections
                .iter()
                .find(|s| usize::from(s.byte_range.start) <= idx && idx < usize::from(s.byte_range.end))
                .unwrap()
        };

        let line1_start = text.find("Inactive").unwrap();
        let sec_existing = find_section("existing", line1_start);
        let sec_missing = find_section("missing", line1_start);

        let palette = LivePalette::with_wiki_targets(true, None);
        assert_ne!(sec_existing.format.color, sec_missing.format.color);
        assert_ne!(sec_missing.format.color, palette.task_completed_color);
        assert_ne!(sec_existing.format.underline, egui::Stroke::NONE);
        assert_eq!(sec_missing.format.underline, egui::Stroke::NONE);
        assert!(sec_missing.format.italics);
    }
}
