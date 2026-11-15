//! Template expansion - the "Template" button scans a `Templates/`
//! folder in the open workspace for `template_*` files, and inserting
//! one at the cursor expands `{{placeholder}}` variables in its content
//! first. This module is intentionally its own thing rather than reusing
//! `api::render_template`: that one's placeholders (`{{selection}}`,
//! `{{tokens.NAME}}`, ...) are about an outgoing API request, not a note
//! template, and don't overlap in meaning.
//!
//! Supported placeholders (user-specified list, 2026-08-12):
//! - `{{today}}` / `{{yyyy-mm-dd}}` - today's date, `YYYY-MM-DD` (both
//!   produce the same text; `{{yyyy-mm-dd}}` just names the format
//!   explicitly).
//! - `{{yesterday}}` / `{{tommorow}}` - today minus/plus one day, same
//!   format (`tommorow` is the user's own spelling, matched literally -
//!   whatever they actually type in a template has to match this).
//! - `{{today: n}}` / `{{today: -n}}` - today shifted by an arbitrary
//!   number of days (`{{yesterday}}`/`{{tommorow}}` are just fixed
//!   shorthand for `-1`/`+1` of this same mechanism); whitespace after
//!   the colon is optional.
//! - `{{time}}` - current time, `HH:MM` (24-hour).
//!
//! Anything else that looks like a placeholder is left exactly as
//! written, matching `api::render_template`'s "unrecognized placeholders
//! stay literal" convention.

use chrono::{Duration, NaiveDate, NaiveDateTime};
use regex::Regex;

/// `true` for a file directly inside the workspace-root `Templates/`
/// folder (name matched case-insensitively) whose own name starts with
/// `template_` - e.g. `Templates/template_daily.md`. A `Templates` folder
/// anywhere else (`notes/Templates/...`) is ignored.
///
/// `root` is the workspace root for a local workspace (`path` is then an
/// absolute path under it); `None` for an SSH workspace, whose paths are
/// already workspace-relative POSIX paths.
pub fn is_template_path(path: &str, root: Option<&str>) -> bool {
    let norm = |s: &str| s.replace('\\', "/");
    let p = norm(path);
    let rel = match root {
        Some(r) => {
            let r = norm(r);
            let r = r.trim_end_matches('/');
            let prefix = format!("{r}/");
            if p.len() <= prefix.len() || !p[..prefix.len()].eq_ignore_ascii_case(&prefix) {
                return false;
            }
            p[prefix.len()..].to_string()
        }
        None => p.trim_start_matches("./").to_string(),
    };
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    parts.len() == 2 && parts[0].eq_ignore_ascii_case("templates") && parts[1].starts_with("template_")
}

/// The root `Templates/template_index.md` among `paths`, if any (see
/// `is_template_path` for the root-only rule).
pub fn find_index_template(paths: &[String], root: Option<&str>) -> Option<String> {
    paths
        .iter()
        .find(|p| {
            is_template_path(p, root)
                && std::path::Path::new(p.as_str())
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("template_index.md"))
        })
        .cloned()
}
/// The picker's display label for a template file - `template_` prefix
/// and extension both stripped (`template_daily.md` -> `daily`), so the
/// list reads as a plain menu of template names rather than filenames.
pub fn display_name(path: &str) -> String {
    let stem = std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    stem.strip_prefix("template_").unwrap_or(&stem).to_string()
}

fn format_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}
fn format_year(d: NaiveDate) -> String {
    d.format("%Y").to_string()
}
fn format_month(d: NaiveDate) -> String {
    d.format("%m").to_string()
}
fn format_day(d: NaiveDate) -> String {
    d.format("%d").to_string()
}

/// Expands every recognized `{{placeholder}}` in `content` - see the
/// module doc comment for the full list. `now` supplies both the date
/// (for `{{today}}` and friends) and the time (for `{{time}}`) so every
/// placeholder in one expansion agrees on "now", rather than each
/// separately calling `chrono::Local::now()` and risking a mismatch
/// across a real (if vanishingly unlikely) midnight/second boundary.
use crate::dashboard::{self, DueTask};

pub fn expand_with_tasks(content: &str, now: NaiveDateTime, due_tasks: &[DueTask]) -> String {
    let expanded_dates = expand(content, now);
    dashboard::expand_task_queries(&expanded_dates, due_tasks, now.date())
}

pub fn expand(content: &str, now: NaiveDateTime) -> String {
    let today = now.date();
    let mut out = content.replace("{{today}}", &format_date(today));
    out = out.replace("{{yyyy-mm-dd}}", &format_date(today));
    out = out.replace("{{yesterday}}", &format_date(today - Duration::days(1)));
    out = out.replace("{{tommorow}}", &format_date(today + Duration::days(1)));
    out = out.replace("{{time}}", &now.format("%H:%M").to_string());
    out = out.replace("{{yyyy}}", &format_year(today)); // for backwards compatibility with the old `{{yyyy}}` placeholder
    out = out.replace("{{mm}}", &format_month(today)); // for backwards compatibility with the old `{{mm}}` placeholder
    out = out.replace("{{dd}}", &format_day(today)); // for backwards compatibility with the old `{{day}}` placeholder
    expand_relative_today(&out, today)
}

/// `{{today: n}}` / `{{today: -n}}` (any integer, optional whitespace
/// after the colon) -> `today` shifted by `n` days, same `YYYY-MM-DD`
/// format as every other date placeholder here.
fn expand_relative_today(content: &str, today: NaiveDate) -> String {
    let re = Regex::new(r"\{\{today:\s*(-?\d+)\s*\}\}").unwrap();
    re.replace_all(content, |caps: &regex::Captures| {
        let n: i64 = caps[1].parse().unwrap_or(0);
        format_date(today + Duration::days(n))
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn now(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        date(y, m, d).and_hms_opt(h, min, 0).unwrap()
    }

    #[test]
    fn recognizes_a_prefixed_file_directly_inside_a_templates_folder() {
        assert!(is_template_path("C:\\ws\\Templates\\template_daily.md", Some("C:\\ws")));
        assert!(is_template_path("Templates/template_reading.md", None)); // SSH-style relative POSIX path
    }

    #[test]
    fn rejects_files_outside_templates_or_without_the_prefix() {
        assert!(!is_template_path("C:\\ws\\notes\\template_daily.md", Some("C:\\ws"))); // wrong folder
        assert!(!is_template_path("C:\\ws\\Templates\\daily.md", Some("C:\\ws"))); // missing prefix
        assert!(!is_template_path("C:\\ws\\Templates\\sub\\template_daily.md", Some("C:\\ws"))); // nested, not direct
        assert!(!is_template_path("C:\\ws\\notes\\Templates\\template_daily.md", Some("C:\\ws"))); // Templates not at the root
        assert!(!is_template_path("notes/Templates/template_daily.md", None));
        assert!(!is_template_path("C:\\other\\Templates\\template_daily.md", Some("C:\\ws"))); // outside the workspace
    }

    #[test]
    fn display_name_strips_prefix_and_extension() {
        assert_eq!(display_name("C:\\ws\\Templates\\template_daily.md"), "daily");
        assert_eq!(display_name("Templates/template_reading.md"), "reading");
    }

    #[test]
    fn expand_replaces_today_and_the_explicit_format_alias() {
        let n = now(2026, 8, 12, 14, 7);
        assert_eq!(expand("# {{today}}\n\n- [ ] \n", n), "# 2026-08-12\n\n- [ ] \n");
        assert_eq!(expand("{{yyyy-mm-dd}}", n), "2026-08-12");
    }

    #[test]
    fn expand_replaces_yesterday_and_tommorow() {
        let n = now(2026, 8, 12, 14, 7);
        assert_eq!(expand("{{yesterday}}", n), "2026-08-11");
        assert_eq!(expand("{{tommorow}}", n), "2026-08-13"); // matches the user's own spelling literally
    }

    #[test]
    fn expand_replaces_time_as_24_hour_hh_mm() {
        assert_eq!(expand("{{time}}", now(2026, 8, 12, 9, 5)), "09:05");
    }

    #[test]
    fn expand_replaces_relative_today_offsets_positive_and_negative() {
        let n = now(2026, 8, 12, 0, 0);
        assert_eq!(expand("{{today: 3}}", n), "2026-08-15");
        assert_eq!(expand("{{today: -3}}", n), "2026-08-09");
        assert_eq!(expand("{{today:-1}}", n), "2026-08-11"); // no space after the colon too
        assert_eq!(expand("{{today: 0}}", n), "2026-08-12");
    }

    #[test]
    fn expand_leaves_unrecognized_placeholders_and_plain_text_untouched() {
        let n = now(2026, 8, 12, 0, 0);
        assert_eq!(expand("hello {{unknown}} world", n), "hello {{unknown}} world");
    }

    #[test]
    fn expand_with_tasks_expands_single_and_double_brace_task_queries() {
        use crate::tasks::Priority;
        let n = now(2026, 8, 12, 0, 0);
        let due_tasks = vec![
            DueTask {
                source_path: "todo.md".to_string(),
                section: None,
                priority: Priority::High,
                due: Some(chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()),
                scheduled: None,
                line: "- [ ] past task 📅 2026-08-10".to_string(),
            },
            DueTask {
                source_path: "todo.md".to_string(),
                section: None,
                priority: Priority::Highest,
                due: Some(chrono::NaiveDate::from_ymd_opt(2026, 8, 20).unwrap()),
                scheduled: None,
                line: "- [ ] future task 📅 2026-08-20".to_string(),
            },
        ];

        // 1. With today filter: past task included, future task excluded
        let template_today = "{{task:priority(today)}}";
        let expanded_today = expand_with_tasks(template_today, n, &due_tasks);
        assert!(expanded_today.contains("past task"));
        assert!(!expanded_today.contains("future task"));

        // 2. Nested {{today}}: {{task:priority({{today}})}}
        let template_nested = "{{task:priority({{today}})}}";
        let expanded_nested = expand_with_tasks(template_nested, n, &due_tasks);
        assert!(expanded_nested.contains("past task"));
        assert!(!expanded_nested.contains("future task"));

        // 3. No date (ALL expansion): both past and future tasks included
        let template_all = "{{task:priority}}";
        let expanded_all = expand_with_tasks(template_all, n, &due_tasks);
        assert!(expanded_all.contains("past task"));
        assert!(expanded_all.contains("future task"));
    }
}
