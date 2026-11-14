//! In-buffer Find/Replace, reusing the regex/match logic already ported
//! for workspace-wide search (`workspace::build_regex`/`scan_matches`/
//! `apply_replace`) rather than duplicating it.

use crate::workspace;
use egui::text::{CCursor, CCursorRange};

pub struct FindState {
    pub open: bool,
    pub replace_mode: bool,
    pub query: String,
    pub replacement: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    /// Index into the current match list - which match "Find Next"
    /// jumps to next. Reset whenever the query/buffer changes meaningfully
    /// enough that stale indices would point at the wrong thing.
    pub current: usize,
    pub status: String,
    /// Set by `find_next`/`replace_current` when they move the cursor to
    /// a match - consumed by the editor panel (which runs later in the
    /// same frame) to scroll that position into view. Set here rather
    /// than scrolled immediately because the `Galley` needed to know
    /// *where* the cursor lands on screen only exists after the
    /// `TextEdit` widget itself has laid out this frame's text, which
    /// hasn't happened yet at the point the find bar runs.
    pub scroll_pending: bool,
}

impl Default for FindState {
    fn default() -> Self {
        Self {
            open: false,
            replace_mode: false,
            query: String::new(),
            replacement: String::new(),
            is_regex: false,
            case_sensitive: false,
            current: 0,
            status: String::new(),
            scroll_pending: false,
        }
    }
}

/// One match's position as global char offsets into the whole buffer -
/// `scan_matches` reports (line, col) pairs, which need converting to a
/// single offset to build a `CCursorRange` for `TextEditState`.
fn match_char_ranges(content: &str, re: &regex::Regex) -> Vec<(usize, usize)> {
    let matches = workspace::scan_matches(content, re);
    if matches.is_empty() {
        return Vec::new();
    }
    let line_starts = workspace::line_starts(content);
    matches.iter().filter_map(|m| workspace::match_char_range(&line_starts, m)).collect()
}

impl FindState {
    /// Returns all matched char ranges `(start, end)` in `content`.
    pub fn get_matches(&self, content: &str) -> Vec<(usize, usize)> {
        if self.query.is_empty() {
            return Vec::new();
        }
        let Ok(re) = workspace::build_regex(&self.query, self.is_regex, self.case_sensitive) else {
            return Vec::new();
        };
        match_char_ranges(content, &re)
    }

    /// Runs the query against `content`, moves `current` to the next
    /// match after the caret (wrapping around), and - if `editor_id` is
    /// given - jumps the editor's selection there. Returns the matched (start, end) range.
    pub fn find_next(&mut self, ctx: &egui::Context, editor_id: egui::Id, content: &str) -> Option<(usize, usize)> {
        let re = match workspace::build_regex(&self.query, self.is_regex, self.case_sensitive) {
            Ok(re) => re,
            Err(e) => {
                self.status = e;
                return None;
            }
        };
        let ranges = match_char_ranges(content, &re);
        if ranges.is_empty() {
            self.status = "No matches".to_string();
            return None;
        }

        // Start searching from after the current selection
        let cur_range = egui::TextEdit::load_state(ctx, editor_id).and_then(|s| s.cursor.char_range());
        let cur_start = cur_range.map(|r| r.primary.index.0.min(r.secondary.index.0));
        let cur_end = cur_range.map(|r| r.primary.index.0.max(r.secondary.index.0));

        let next = if let (Some(c_start), Some(c_end)) = (cur_start, cur_end) {
            if c_start != c_end {
                // If already selecting a match, advance to the next match strictly after this one
                ranges.iter().position(|(s, _)| *s >= c_end).unwrap_or(0)
            } else {
                ranges.iter().position(|(s, _)| *s >= c_start).unwrap_or(0)
            }
        } else {
            0
        };

        self.current = next;
        let (start, end) = ranges[next];
        self.status = format!("{} of {} matches", next + 1, ranges.len());

        if let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id) {
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(start),
                CCursor::new(end),
            )));
            egui::TextEdit::store_state(ctx, editor_id, state);
        }
        Some((start, end))
    }

    /// Replaces the currently-selected match (if the selection matches
    /// the query) and advances to the next one - mirrors a typical
    /// "Replace" button (replace-one-then-find-next), not a bulk op.
    pub fn replace_current(&mut self, ctx: &egui::Context, editor_id: egui::Id, content: &mut String) -> Option<(usize, usize)> {
        let re = match workspace::build_regex(&self.query, self.is_regex, self.case_sensitive) {
            Ok(re) => re,
            Err(e) => {
                self.status = e;
                return None;
            }
        };
        let ranges = match_char_ranges(content, &re);
        let &(start, end) = ranges.get(self.current)?;
        let chars: Vec<char> = content.chars().collect();
        let matched: String = chars[start..end].iter().collect();
        // Re-run the single match through the same replace logic
        // workspace-wide replace uses, so `$1`-style regex backreferences
        // in `self.replacement` behave identically here.
        let (replaced_piece, _) = workspace::apply_replace(&matched, &re, &self.replacement);
        let new_content: String = chars[..start]
            .iter()
            .collect::<String>()
            + &replaced_piece
            + &chars[end..].iter().collect::<String>();
        *content = new_content;
        self.find_next(ctx, editor_id, content)
    }

    pub fn replace_all(&mut self, content: &mut String) {
        let re = match workspace::build_regex(&self.query, self.is_regex, self.case_sensitive) {
            Ok(re) => re,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let (replaced, count) = workspace::apply_replace(content, &re, &self.replacement);
        *content = replaced;
        self.status = format!("Replaced {count} match(es)");
    }
}
