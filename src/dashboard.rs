//! "Today's Tasks" workspace-wide dashboard - scans every `.md`/`.txt`
//! file in the open workspace for unchecked task lines due (📅) or
//! scheduled (⏳) today or earlier, groups them by priority, and renders
//! a markdown block to insert at the cursor.
//!
//! Each generated line carries a **visible** trailing link back to its
//! source - `task text (workspace/relative/path.md#Section)` - so
//! checking it off can find and complete the original task in its
//! source file (otherwise the same overdue item would just resurface
//! every morning, the user's own stated reason for wanting this
//! feature). This is a deliberate design change from an earlier,
//! invisible-metadata approach: two attempts at hiding the reference
//! entirely (an HTML comment, then a CommonMark link-reference-
//! definition - the classic "invisible markdown comment" trick) both
//! rendered as plain visible text under `egui_commonmark`, which
//! evidently doesn't parse those block-level constructs at all. Making
//! the link visible and useful on its own (a real, workspace-relative,
//! restart-durable reference the user can also read) sidesteps that
//! limitation entirely instead of fighting it, per explicit user
//! feedback (2026-08-12) after trying the feature live.

use crate::tasks::{self, Priority};
use chrono::NaiveDate;
use regex::Regex;

#[derive(Clone, Debug)]
pub struct DueTask {
    /// Workspace-relative, forward-slash-joined, no leading slash
    /// (`"notes/shopping.md"`) - true regardless of whether the
    /// workspace is local or SSH, so the embedded link in
    /// `render_today_list`'s output means the same thing either way and
    /// survives the workspace being moved (unlike an absolute path).
    pub source_path: String,
    /// The nearest markdown heading (any level, `#`..`######`) above
    /// this task in its source file, title text only (no leading `#`s) -
    /// `None` if the task appears before any heading.
    pub section: Option<String>,
    pub priority: Priority,
    pub due: Option<NaiveDate>,
    pub scheduled: Option<NaiveDate>,
    /// The task line's full text, unmodified (including its own
    /// `- [ ]` prefix), with NO link appended yet - `render_today_list`
    /// appends the `(source_path#section)` link when rendering.
    pub line: String,
}

impl DueTask {
    pub fn is_due_on_or_before(&self, date: NaiveDate) -> bool {
        self.due.is_some_and(|d| d <= date) || self.scheduled.is_some_and(|d| d <= date)
    }
}

/// `true` for a file extension the dashboard scans - markdown and plain
/// text notes only, never source code (unlike `workspace::
/// search_workspace`, which scans everything not explicitly excluded, a
/// todo dashboard has no business treating a `.rs`/`.py` file's
/// `- [ ]`-shaped comment as a real task).
pub fn is_scannable_extension(path: &str) -> bool {
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    ext == "md"
}

/// Scans one file's full `content` for all unchecked task lines (with NO
/// link appended yet).
pub fn tasks_in_file(source_path: &str, content: &str) -> Vec<DueTask> {
    if !is_scannable_extension(source_path) {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut current_section: Option<String> = None;
    let mut in_fence = false;
    for line in content.lines() {
        if is_fence_line(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(heading) = parse_heading(line) {
            current_section = Some(heading);
            continue;
        }
        if !is_unchecked_task_line(line) {
            continue;
        }
        if parse_link(line).is_some() {
            continue;
        }
        let meta = tasks::parse_task_meta(line);
        found.push(DueTask {
            source_path: source_path.to_string(),
            section: current_section.clone(),
            priority: meta.priority,
            due: meta.due,
            scheduled: meta.scheduled,
            line: line.to_string(),
        });
    }
    found
}

/// Scans one file's full `content` (`source_path` naming it - workspace-
/// relative, see `DueTask::source_path` - both for the embedded link and
/// to gate the extension check) for unchecked task lines due or
/// scheduled today or earlier.
pub fn due_tasks_in_file(source_path: &str, content: &str, today: NaiveDate) -> Vec<DueTask> {
    tasks_in_file(source_path, content)
        .into_iter()
        .filter(|t| t.is_due_on_or_before(today))
        .collect()
}

/// `true` for a fenced-code-block delimiter line (three or more
/// backticks, ignoring leading whitespace) - opening and closing fences
/// look identical for this purpose, since `due_tasks_in_file` only needs
/// to toggle in/out of "inside a fence", not track language tags or
/// exact backtick-count matching the way `syntax.rs`'s highlighter does.
fn is_fence_line(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// `- [ ]`/`* [ ]`/`+ [ ]` specifically *unchecked* - a done task is
/// never "due today" in any useful sense, so this dashboard has no
/// reason to ever surface one. Also requires whitespace (or end of line)
/// immediately after the checkbox, matching Obsidian Tasks' own
/// recognition rule - confirmed live (2026-08-12) that a real Obsidian
/// vault does NOT treat `- [ ]月曜：...` (title glued directly onto `]`,
/// no space) as a task at all. The user writes some of their own
/// routine/habit lines exactly this way on purpose, so without this
/// check they got swept into "Today's Tasks" even though Obsidian itself
/// - and the user - never considered them real tasks.
fn is_unchecked_task_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    for prefix in ["- [ ]", "* [ ]", "+ [ ]"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t');
        }
    }
    false
}

/// `Some(title)` if `line` is a markdown ATX heading (`#` through
/// `######`, a space, then the title) - `None` for everything else,
/// including a bare `#` with no title text.
pub fn parse_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = trimmed[hashes..].strip_prefix(' ')?;
    let title = rest.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Renders `tasks` (already collected from however many files) into one
/// markdown block, grouped by priority (most urgent first, empty groups
/// omitted). Each task line is followed by its own visible
/// `(source_path#section)` link (see the module doc comment for why it's
/// visible rather than hidden). An empty `tasks` still renders a short
/// "nothing due" note rather than an empty string, so running this with
/// nothing outstanding still visibly confirms it actually ran.
pub fn render_today_list(tasks: &[DueTask], today: NaiveDate) -> String {
    let mut out = format!("## Today's Tasks ({})\n\n", today.format("%Y-%m-%d"));
    if tasks.is_empty() {
        out.push_str("Nothing due.\n");
        return out;
    }
    let order = [Priority::Highest, Priority::High, Priority::Medium, Priority::None, Priority::Low, Priority::Lowest];
    for p in order {
        let group: Vec<&DueTask> = tasks.iter().filter(|t| t.priority.rank() == p.rank()).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("### {}\n\n", p.label()));
        for t in group {
            out.push_str(&t.line);
            out.push(' ');
            out.push_str(&format_link(&t.source_path, t.section.as_deref()));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Parses a date argument string (e.g. "today", "yesterday", "tommorow", "tomorrow",
/// "today+1", "today-1", "today: 2", or "2026-10-16") relative to `today`.
pub fn parse_date_arg(raw: &str, today: NaiveDate) -> Option<NaiveDate> {
    let s = raw.trim().to_lowercase();
    if s == "today" {
        return Some(today);
    }
    if s == "yesterday" {
        return Some(today - chrono::Duration::days(1));
    }
    if s == "tommorow" || s == "tomorrow" {
        return Some(today + chrono::Duration::days(1));
    }
    if let Some(rest) = s.strip_prefix("today") {
        let rest = rest.trim();
        if let Some(num_str) = rest.strip_prefix('+').or_else(|| rest.strip_prefix(':')) {
            if let Ok(n) = num_str.trim().parse::<i64>() {
                return Some(today + chrono::Duration::days(n));
            }
        } else if let Some(num_str) = rest.strip_prefix('-') {
            if let Ok(n) = num_str.trim().parse::<i64>() {
                return Some(today - chrono::Duration::days(n));
            }
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d") {
        return Some(d);
    }
    None
}

pub fn render_task_by_priority(
    tasks: &[DueTask],
    priority_filter: Option<&str>,
    date_filter: Option<NaiveDate>,
) -> String {
    let mut out = String::new();
    let filter = priority_filter.map(|s| s.to_lowercase());
    let order = [Priority::Highest, Priority::High, Priority::Medium, Priority::None, Priority::Low, Priority::Lowest];

    let filtered_tasks: Vec<&DueTask> = tasks
        .iter()
        .filter(|t| {
            if let Some(d) = date_filter {
                t.is_due_on_or_before(d)
            } else {
                true // 日付指定なしの場合は全未完了タスクを展開 (ALL展開)
            }
        })
        .collect();

    match filter.as_deref() {
        None | Some("priority") | Some("all") => {
            for p in order {
                let group: Vec<&&DueTask> = filtered_tasks.iter().filter(|t| t.priority.rank() == p.rank()).collect();
                if group.is_empty() {
                    continue;
                }
                out.push_str(&format!("### {}\n\n", p.label()));
                for t in group {
                    out.push_str(&t.line);
                    out.push(' ');
                    out.push_str(&format_link(&t.source_path, t.section.as_deref()));
                    out.push('\n');
                }
                out.push('\n');
            }
        }
        Some(target) => {
            let target_priority = match target {
                "highest" => Some(Priority::Highest),
                "high" => Some(Priority::High),
                "medium" => Some(Priority::Medium),
                "low" => Some(Priority::Low),
                "lowest" => Some(Priority::Lowest),
                "none" | "normal" => Some(Priority::None),
                _ => None,
            };
            if let Some(p) = target_priority {
                let group: Vec<&&DueTask> = filtered_tasks.iter().filter(|t| t.priority.rank() == p.rank()).collect();
                for t in group {
                    out.push_str(&t.line);
                    out.push(' ');
                    out.push_str(&format_link(&t.source_path, t.section.as_deref()));
                    out.push('\n');
                }
            }
        }
    }
    out.trim_end().to_string()
}

/// Expands `{{task}}`, `{{task: priority}}`, `{{task:priority(today)}}`,
/// `{{task:priority(2026-10-16)}}`, `{{task(today)}}`, `{{task:priority:today}}` etc.
/// in `content` using the supplied list of `tasks` and base date `today`.
pub fn expand_task_queries(content: &str, tasks: &[DueTask], today: NaiveDate) -> String {
    let re = Regex::new(
        r"(?:-\s*)?\{\{\s*task(?:(?:\s*\(\s*([^)]+?)\s*\))|(?::\s*([a-zA-Z0-9_\-+:]+?)(?:\s*\(\s*([^)]+?)\s*\))?))?\s*\}\}"
    ).unwrap();
    re.replace_all(content, |caps: &regex::Captures| {
        let (priority_arg, date_arg) = if let Some(date_paren) = caps.get(1) {
            // {{task(date)}}
            (None, Some(date_paren.as_str()))
        } else if let Some(colon_part) = caps.get(2) {
            let colon_str = colon_part.as_str();
            let paren_part = caps.get(3).map(|m| m.as_str());
            if let Some(date_paren) = paren_part {
                // {{task:priority(date)}}
                (Some(colon_str), Some(date_paren))
            } else if let Some((p, d)) = colon_str.split_once(':') {
                // {{task:priority:today}}
                (Some(p), Some(d))
            } else if parse_date_arg(colon_str, today).is_some() {
                // {{task:today}} or {{task:2026-10-16}}
                (None, Some(colon_str))
            } else {
                // {{task:priority}} or {{task:high}}
                (Some(colon_str), None)
            }
        } else {
            // {{task}}
            (None, None)
        };

        let date_filter = date_arg.and_then(|d| parse_date_arg(d, today));
        render_task_by_priority(tasks, priority_arg, date_filter)
    })
    .into_owned()
}

fn format_link(source_path: &str, section: Option<&str>) -> String {
    match section {
        Some(s) if !s.is_empty() => format!("[[{source_path}#{s}]]"),
        _ => format!("[[{source_path}]]"),
    }
}

/// Matches the trailing `[[path]]`/`[[path#section]]` or `[...](wikilink:path#section)`
/// link `render_today_list` appends to generated task lines - anchored at the end of line ($).
fn link_regex() -> Regex {
    Regex::new(r"\s*(?:\[\[\s*([^\[\]]+?\.(?:md|txt))(?:#([^\[\]]+?))?\s*\]\]|\[[^\]]*\]\(\s*<wikilink:([^>#]+?\.(?:md|txt))(?:#([^>]+?))?>\s*\)|\[[^\]]*\]\(\s*wikilink:([^)#\s]+?\.(?:md|txt))(?:#([^)#\s]+?))?\s*\))\s*$").unwrap()
}

/// If `line` ends with a `render_today_list`-style WikiLink or Markdown WikiLink, returns
/// `(source_path, section)` - `None` for an ordinary line.
pub fn parse_link(line: &str) -> Option<(String, Option<String>)> {
    let caps = link_regex().captures(line)?;
    let path = if let Some(p) = caps.get(1) {
        p.as_str().trim().to_string()
    } else if let Some(p) = caps.get(3) {
        p.as_str().trim().to_string()
    } else if let Some(p) = caps.get(5) {
        p.as_str().trim().to_string()
    } else {
        return None;
    };
    let section = if let Some(s) = caps.get(2) {
        Some(s.as_str().trim().to_string())
    } else if let Some(s) = caps.get(4) {
        Some(s.as_str().trim().to_string())
    } else if let Some(s) = caps.get(6) {
        Some(s.as_str().trim().to_string())
    } else {
        None
    };
    Some((path, section))
}

/// `line` with its trailing link (if any) removed - the source file's
/// own copy of a linked task never has one, so comparisons need it
/// stripped from the generated side first.
pub fn strip_link(line: &str) -> &str {
    match link_regex().find(line) {
        Some(m) => &line[..m.start()],
        None => line,
    }
}

/// Reverts `[target](<wikilink:target>)` back to `[[target]]` so that task content comparison
/// matches whether the line was read from raw source text or rendered preview text.
pub fn normalize_embedded_wikilinks(text: &str) -> String {
    let re = Regex::new(r"\[([^\]]+)\]\(\s*<?wikilink:([^>#\)\s]+)(?:#([^>#\)\s]+))?>?\s*\)").unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let label = &caps[1];
        let target = &caps[2];
        let section = caps.get(3).map(|m| m.as_str());
        let full_target = match section {
            Some(s) if !s.is_empty() => format!("{target}#{s}"),
            _ => target.to_string(),
        };
        if label == &full_target || label == target {
            format!("[[{full_target}]]")
        } else {
            format!("[[{full_target}|{label}]]")
        }
    })
    .into_owned()
}

/// The content-match key for a task line, ignoring its checkbox's
/// checked-state (`[ ]` vs `[x]`/`[X]`), any leading indentation/marker style
/// (`-`/`*`/`+`), any completion stamp (`✅ YYYY-MM-DD`), and its trailing link if any -
/// used both to compare a generated line against its source file's own copy
/// (`same_task_ignoring_checked_state`) and, in `due_tasks_in_file`, to
/// recognize an already-linked (pointer) line.
pub fn task_key(line: &str) -> String {
    let trimmed = line.trim_start();
    let without_marker = [
        "- [ ]", "- [x]", "- [X]",
        "* [ ]", "* [x]", "* [X]",
        "+ [ ]", "+ [x]", "+ [X]",
    ]
    .iter()
    .find_map(|p| trimmed.strip_prefix(p))
    .unwrap_or(trimmed);
    let stripped_link = strip_link(without_marker).trim();
    let without_done = crate::tasks::strip_done_date(stripped_link);
    let stripped = strip_link(without_done.trim()).trim();
    normalize_embedded_wikilinks(stripped)
}

/// `true` if `source_line` (a real line from the source file) and
/// `generated_line` (a dashboard-generated task line, already toggled to
/// `[x]`, possibly still carrying its link) are "the same task" - see
/// `task_key` for exactly what's ignored.
pub fn same_task_ignoring_checked_state(source_line: &str, generated_line: &str) -> bool {
    task_key(source_line) == task_key(generated_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn task(source_path: &str, section: Option<&str>, priority: Priority, line: &str) -> DueTask {
        let meta = crate::tasks::parse_task_meta(line);
        DueTask {
            source_path: source_path.to_string(),
            section: section.map(String::from),
            priority,
            due: meta.due,
            scheduled: meta.scheduled,
            line: line.to_string(),
        }
    }

    #[test]
    fn only_scans_md_extensions() {
        let today = date(2026, 1, 10);
        let content = "- [ ] overdue thing 📅 2026-01-01\n";
        assert!(due_tasks_in_file("notes/todo.rs", content, today).is_empty());
        assert!(due_tasks_in_file("notes/todo.py", content, today).is_empty());
        assert!(due_tasks_in_file("notes/todo.txt", content, today).is_empty());
        assert!(!due_tasks_in_file("notes/todo.md", content, today).is_empty());
    }

    #[test]
    fn finds_unchecked_tasks_due_or_scheduled_today_or_earlier() {
        let today = date(2026, 1, 10);
        let content = "\
- [ ] overdue (due) 📅 2026-01-01
- [ ] due today 📅 2026-01-10
- [ ] due later 📅 2026-01-20
- [ ] scheduled today ⏳ 2026-01-10
- [ ] scheduled earlier ⏳ 2026-01-05
- [ ] scheduled later ⏳ 2026-01-20
- [x] already done 📅 2026-01-01
- [ ] no date at all
";
        let found = due_tasks_in_file("notes/todo.md", content, today);
        let lines: Vec<&str> = found.iter().map(|t| t.line.as_str()).collect();
        assert_eq!(
            lines,
            vec![
                "- [ ] overdue (due) 📅 2026-01-01",
                "- [ ] due today 📅 2026-01-10",
                "- [ ] scheduled today ⏳ 2026-01-10",
                "- [ ] scheduled earlier ⏳ 2026-01-05",
            ]
        );
    }

    #[test]
    fn a_checkbox_glued_directly_to_its_title_is_not_recognized_as_a_task() {
        let today = date(2026, 1, 10);
        let content = "- [ ]月曜：ヒップリフト ⏫ 🔁 every week on Monday 🏁 delete 📅 2000-01-01\n";
        assert!(due_tasks_in_file("routine.md", content, today).is_empty());
    }

    #[test]
    fn tracks_the_nearest_preceding_heading_as_the_section() {
        let today = date(2026, 1, 10);
        let content = "\
# Work

## Reports
- [ ] file taxes 📅 2026-01-01

## Chores
- [ ] take out trash 📅 2026-01-01

- [ ] still under Chores, no closer heading 📅 2026-01-01
";
        let found = due_tasks_in_file("notes/todo.md", content, today);
        assert_eq!(found[0].section.as_deref(), Some("Reports"));
        assert_eq!(found[1].section.as_deref(), Some("Chores"));
        assert_eq!(found[2].section.as_deref(), Some("Chores"));
    }

    /// Regression guard for a real bug found live (2026-08-13): a user
    /// migrating from Obsidian Tasks had `#`-prefixed *comment* lines
    /// inside their `\`\`\`tasks` query blocks (the plugin's own query
    /// language, unrelated to markdown headings) - `parse_heading` was
    /// misreading those as real headings, so tasks after the block
    /// picked up the comment text as their section.
    #[test]
    fn a_hash_comment_inside_a_fenced_code_block_is_not_mistaken_for_a_heading() {
        let today = date(2026, 1, 10);
        let content = "\
# Daily Notes

```tasks
# Hide postpone, edit and backlinks
not done
```

- [ ] Kimi K-3 📅 2026-01-01
";
        let found = due_tasks_in_file("Daily/2026/01/2026-01-08.md", content, today);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].section.as_deref(), Some("Daily Notes"), "should still be the real heading, not the fenced comment");
    }

    /// Reproduces the user's actual "Highest" dashboard block verbatim
    /// (2026-08-13 report) followed by a real task, to check whether the
    /// fence toggle survives this exact content - the query text itself
    /// contains several parens (`((scheduled on or before today) OR
    /// (due on or before today)) AND`, `(priority highest)`), which are
    /// all inside the fence and so should never reach `parse_link`/
    /// `parse_heading` at all.
    #[test]
    fn the_users_actual_highest_dashboard_block_does_not_swallow_the_task_after_it() {
        let today = date(2026, 8, 13);
        let content = "\
##### Highest 今日おこること
```tasks
# Hide any values for all date fields
hide due date
hide scheduled date
hide start date
hide created date
hide done date
hide cancelled date
show recurrence rule
# Hide postpone, edit and backinks
hide postpone button
hide edit button
show backlinks
not done
((scheduled on or before today) OR (due on or before today)) AND
(priority highest)
sort by priority
sort by due
```

- [ ] Kimi K-3 🔺 📅 2026-08-09
";
        let found = due_tasks_in_file("Dashboard.md", content, today);
        assert_eq!(found.len(), 1, "the real task after the query block should still be found");
        assert_eq!(found[0].priority, Priority::Highest);
        assert_eq!(found[0].section.as_deref(), Some("Highest 今日おこること"));
    }

    /// The user's dashboard note almost certainly has one such block per
    /// priority level stacked back to back (Highest, then High, ...) -
    /// this checks that Highest AND High tasks both survive scanning
    /// when their dashboard blocks appear one after another in the same
    /// file, and directly asserts on `is_unchecked_task_line` and
    /// `tasks::parse_task_meta` for each real task line too, per the
    /// user's own hypothesis (2026-08-13) that the lines themselves might
    /// not be recognized as tasks at all.
    #[test]
    fn highest_and_high_tasks_are_both_found_when_their_dashboard_blocks_are_stacked() {
        let today = date(2026, 8, 13);
        let highest_line = "- [ ] Kimi K-3 🔺 📅 2026-08-09";
        let high_line = "- [ ] Reply to client ⏫ 📅 2026-08-10";
        assert!(is_unchecked_task_line(highest_line));
        assert!(is_unchecked_task_line(high_line));
        assert_eq!(tasks::parse_task_meta(highest_line).priority, Priority::Highest);
        assert_eq!(tasks::parse_task_meta(high_line).priority, Priority::High);

        let content = format!(
            "\
##### Highest 今日おこること
```tasks
not done
((scheduled on or before today) OR (due on or before today)) AND
(priority highest)
sort by priority
sort by due
```

{highest_line}

##### High 今日おこること
```tasks
not done
((scheduled on or before today) OR (due on or before today)) AND
(priority high)
sort by priority
sort by due
```

{high_line}
"
        );
        let found = due_tasks_in_file("Dashboard.md", &content, today);
        let priorities: Vec<Priority> = found.iter().map(|t| t.priority).collect();
        assert_eq!(priorities, vec![Priority::Highest, Priority::High], "both tasks should be found, in order");
    }

    #[test]
    fn a_task_shaped_line_inside_a_fenced_code_block_is_not_scanned() {
        let today = date(2026, 1, 10);
        let content = "\
```tasks
- [ ] example query syntax, not a real task 📅 2026-01-01
```
";
        assert!(due_tasks_in_file("notes/todo.md", content, today).is_empty());
    }

    #[test]
    fn section_is_none_when_the_task_precedes_any_heading() {
        let today = date(2026, 1, 10);
        let content = "- [ ] buy milk 📅 2026-01-01\n# Later Heading\n";
        let found = due_tasks_in_file("notes/todo.md", content, today);
        assert_eq!(found[0].section, None);
    }

    #[test]
    fn a_line_that_already_carries_a_link_is_skipped_as_its_own_source() {
        // A leftover, still-unchecked block from an earlier run sitting
        // in a file that's itself being rescanned.
        let today = date(2026, 1, 10);
        let content = "- [ ] buy milk 📅 2026-01-01 [[notes/shopping.md#Groceries]]\n";
        assert!(due_tasks_in_file("today.md", content, today).is_empty());
    }

    /// Regression guard for a real false positive found live (2026-08-13):
    /// an ordinary task ending in its own unrelated parenthetical aside
    /// was mistaken for an already-linked pointer line (the earlier
    /// `link_regex` matched *any* trailing `(...)`) and silently dropped
    /// from the scan - specifically hit the user's Highest/High-priority
    /// tasks, since those happened to be the ones with a trailing note.
    #[test]
    fn a_task_ending_in_an_unrelated_parenthetical_is_still_found() {
        let today = date(2026, 1, 10);
        let content = "- [ ] call the client (follow up first) 🔺 📅 2026-01-01\n";
        let found = due_tasks_in_file("notes/todo.md", content, today);
        assert_eq!(found.len(), 1, "should NOT be mistaken for an already-linked line");
        assert_eq!(found[0].priority, Priority::Highest);
    }

    #[test]
    fn render_groups_by_priority_most_urgent_first_and_omits_empty_groups() {
        let today = date(2026, 1, 10);
        let tasks = vec![
            task("a.md", None, Priority::Low, "- [ ] low task"),
            task("b.md", None, Priority::Highest, "- [ ] urgent task"),
            task("c.md", None, Priority::None, "- [ ] normal task"),
        ];
        let rendered = render_today_list(&tasks, today);
        let highest_pos = rendered.find("### Highest").unwrap();
        let normal_pos = rendered.find("### Normal").unwrap();
        let low_pos = rendered.find("### Low").unwrap();
        assert!(highest_pos < normal_pos && normal_pos < low_pos);
        assert!(!rendered.contains("### High\n")); // no High-priority tasks - group omitted
    }

    #[test]
    fn render_with_no_tasks_still_says_so() {
        let rendered = render_today_list(&[], date(2026, 1, 10));
        assert!(rendered.contains("Nothing due"));
    }

    #[test]
    fn render_appends_a_visible_link_with_section_when_known() {
        let tasks = vec![task("notes/shopping.md", Some("Groceries"), Priority::None, "- [ ] buy milk")];
        let rendered = render_today_list(&tasks, date(2026, 1, 10));
        assert!(rendered.lines().any(|l| l == "- [ ] buy milk [[notes/shopping.md#Groceries]]"), "got: {rendered}");
    }

    #[test]
    fn render_appends_a_link_without_a_section_fragment_when_unknown() {
        let tasks = vec![task("notes/shopping.md", None, Priority::None, "- [ ] buy milk")];
        let rendered = render_today_list(&tasks, date(2026, 1, 10));
        assert!(rendered.lines().any(|l| l == "- [ ] buy milk [[notes/shopping.md]]"), "got: {rendered}");
    }

    #[test]
    fn parse_link_extracts_path_and_section() {
        assert_eq!(
            parse_link("- [ ] buy milk [[notes/shopping.md#Groceries]]"),
            Some(("notes/shopping.md".to_string(), Some("Groceries".to_string())))
        );
        assert_eq!(
            parse_link("- [ ] buy milk [[ notes/shopping.md#Groceries ]]"),
            Some(("notes/shopping.md".to_string(), Some("Groceries".to_string())))
        );
        assert_eq!(parse_link("- [ ] buy milk [[notes/shopping.md]]"), Some(("notes/shopping.md".to_string(), None)));
        assert_eq!(
            parse_link("- [x] buy milk [notes/shopping.md#Groceries](<wikilink:notes/shopping.md#Groceries>)"),
            Some(("notes/shopping.md".to_string(), Some("Groceries".to_string())))
        );
        assert_eq!(
            parse_link("- [ ] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete [[todo2_2 - コピー (2).md]]"),
            Some(("todo2_2 - コピー (2).md".to_string(), None))
        );
        assert_eq!(
            parse_link("- [x] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete [todo2_2 - コピー (2).md](<wikilink:todo2_2 - コピー (2).md>)"),
            Some(("todo2_2 - コピー (2).md".to_string(), None))
        );
        assert_eq!(parse_link("- [ ] just a task"), None);
    }

    #[test]
    fn parse_link_ignores_a_trailing_parenthetical_that_is_not_a_path() {
        assert_eq!(parse_link("- [ ] call the client (follow up first)"), None);
        assert_eq!(parse_link("- [ ] buy milk (2%)"), None);
    }

    #[test]
    fn strip_link_removes_only_the_trailing_link() {
        assert_eq!(strip_link("- [ ] buy milk [[notes/shopping.md#Groceries]]"), "- [ ] buy milk");
        assert_eq!(strip_link("- [ ] buy milk (2%) [[notes/shopping.md]]"), "- [ ] buy milk (2%)");
        assert_eq!(strip_link("- [x] buy milk [notes/shopping.md#Groceries](<wikilink:notes/shopping.md#Groceries>)"), "- [x] buy milk");
        assert_eq!(
            strip_link("- [ ] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete [todo2_2 - コピー (2).md](<wikilink:todo2_2 - コピー (2).md>)"),
            "- [ ] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete"
        );
        assert_eq!(strip_link("- [ ] just a task"), "- [ ] just a task");
    }

    #[test]
    fn task_key_strips_marker_checked_state_and_link() {
        assert_eq!(task_key("- [ ] buy milk 📅 2026-01-01"), "buy milk 📅 2026-01-01");
        assert_eq!(task_key("- [x] buy milk 📅 2026-01-01 ✅ 2026-01-10"), "buy milk 📅 2026-01-01");
        assert_eq!(task_key("- [X] buy milk 📅 2026-01-01 [[notes/shopping.md#Groceries]]"), "buy milk 📅 2026-01-01");
        assert_eq!(task_key("* [ ] buy milk"), "buy milk");
    }

    #[test]
    fn matches_a_task_regardless_of_which_side_is_checked_or_carries_the_link() {
        assert!(same_task_ignoring_checked_state(
            "- [ ] buy milk 📅 2026-01-01",
            "- [x] buy milk 📅 2026-01-01 [[notes/shopping.md#Groceries]]"
        ));
        assert!(same_task_ignoring_checked_state(
            "- [x] buy milk 📅 2026-01-01 ✅ 2026-01-10",
            "- [ ] buy milk 📅 2026-01-01 [[notes/shopping.md#Groceries]]"
        ));
        assert!(same_task_ignoring_checked_state(
            " - [ ] [[AI関連の論文を読んでいこう]] ⏫ 🔁 every week on thursday 🏁 delete 📅 2026-08-20",
            " - [x] [AI関連の論文を読んでいこう](<wikilink:AI関連の論文を読んでいこう>) ⏫ 🔁 every week on thursday 🏁 delete 📅 2026-08-20 [Notes/生成AI関連論文/予定をタスク化.md](<wikilink:Notes/生成AI関連論文/予定をタスク化.md>)"
        ));
        assert!(!same_task_ignoring_checked_state("- [ ] buy milk", "- [x] buy bread"));
    }

    #[test]
    fn expands_task_queries_for_all_and_filtered_priorities() {
        let today = date(2026, 10, 16);
        let tasks = vec![
            task("a.md", None, Priority::Highest, "- [ ] urgent task 📅 2026-10-15"),
            task("b.md", None, Priority::High, "- [ ] high task 📅 2026-10-16"),
            task("c.md", None, Priority::None, "- [ ] normal task 📅 2026-10-20"),
            task("d.md", None, Priority::Low, "- [ ] no date task"),
        ];

        // 1. All tasks (no date filter)
        let content_all = "- {{task: priority}}";
        let expanded_all = expand_task_queries(content_all, &tasks, today);
        assert!(expanded_all.contains("### Highest"));
        assert!(expanded_all.contains("urgent task"));
        assert!(expanded_all.contains("normal task"));
        assert!(expanded_all.contains("no date task"));

        // 2. Filter by high priority, all dates
        let content_high = "- {{task: high}}";
        let expanded_high = expand_task_queries(content_high, &tasks, today);
        assert!(expanded_high.contains("- [ ] high task"));
        assert!(!expanded_high.contains("urgent task"));

        // 3. Filter by date: today (2026-10-16) - includes due <= 2026-10-16 (urgent and high)
        let content_today = "{{task:priority(today)}}";
        let expanded_today = expand_task_queries(content_today, &tasks, today);
        assert!(expanded_today.contains("urgent task"));
        assert!(expanded_today.contains("high task"));
        assert!(!expanded_today.contains("normal task")); // due 2026-10-20 (later)
        assert!(!expanded_today.contains("no date task")); // no date

        // 4. Filter by explicit date: 2026-10-15
        let content_date = "{{task:priority(2026-10-15)}}";
        let expanded_date = expand_task_queries(content_date, &tasks, today);
        assert!(expanded_date.contains("urgent task"));
        assert!(!expanded_date.contains("high task"));

        // 5. Colon syntax: {{task:priority:today}}
        let content_colon = "{{task:priority:today}}";
        let expanded_colon = expand_task_queries(content_colon, &tasks, today);
        assert!(expanded_colon.contains("urgent task"));
        assert!(expanded_colon.contains("high task"));
        assert!(!expanded_colon.contains("normal task"));

        // 6. Task without priority keyword: {{task(today)}}
        let content_paren_only = "{{task(today)}}";
        let expanded_paren_only = expand_task_queries(content_paren_only, &tasks, today);
        assert!(expanded_paren_only.contains("urgent task"));
        assert!(expanded_paren_only.contains("high task"));

        // 7. Relative date: {{task:priority(today+5)}} -> 2026-10-21 (includes normal task)
        let content_rel = "{{task:priority(today+5)}}";
        let expanded_rel = expand_task_queries(content_rel, &tasks, today);
        assert!(expanded_rel.contains("normal task"));
    }
}
