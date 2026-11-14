//! Obsidian Tasks-compatible metadata parsing and recurrence arithmetic
//! for markdown task lines (`- [ ] water plants 🔁 every week 📅 2026-01-01`).
//!
//! The line syntax (emoji markers, `when done`, `on completion`) follows
//! the Obsidian Tasks plugin: https://publish.obsidian.md/tasks/. The
//! date-shift *mechanics* (weekday-mask cycling for weekly, day-of-month
//! clamping so "the 31st, monthly" lands on Feb 28/29 rather than
//! erroring) are ported from `../kotodo`'s `RepeatRule.java` - a from-
//! scratch recurrence engine for an unrelated Android todo app, reused
//! here for its already-tested edge-case handling.
//!
//! Scope agreed with the user: interval-based day/week/month/year
//! recurrence, an explicit weekday list or `every weekday` shorthand for
//! weekly, and the `when done` modifier. NOT supported: explicit day-of-
//! month/month lists in the rule text ("on the 15th", "on the last day") -
//! monthly/yearly always recur on the *anchor's own* day-of-month/month
//! (clamped), matching kotodo's "empty mask defaults to the anchor"
//! behavior degenerated to a single value since this module never
//! populates a multi-value mask from parsed text. `on completion` only
//! recognizes `delete` (`keep`/omitted is the default no-op).

use chrono::{Datelike, Duration, NaiveDate, Weekday};

pub const DUE: char = '📅';
pub const SCHEDULED: char = '⏳';
pub const START: char = '🛫';
pub const DONE: char = '✅';
pub const RECUR: char = '🔁';
pub const ON_COMPLETION: char = '🏁';
/// Not read by any parsing/arithmetic in this module (this app has no
/// creation-date/cancellation/dependency logic of its own) - defined
/// here purely so `app.rs`'s task-assist popup can offer every Obsidian
/// Tasks marker the user asked for, in one place with the others.
pub const CREATED: char = '➕';
pub const CANCELLED: char = '❌';
pub const ID: char = '🆔';
pub const DEPENDS_ON: char = '⛔';

/// Obsidian Tasks' five priority levels plus the implicit "no marker at
/// all" default - `rank()` gives the sort order used by the Today's
/// Tasks dashboard (urgent first): Highest, High, Medium, None, Low,
/// Lowest. `None` sits in the middle rather than last since it's the
/// *default*, not a deliberately lower priority than Low/Lowest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Highest,
    High,
    Medium,
    None,
    Low,
    Lowest,
}

impl Priority {
    pub fn rank(self) -> u8 {
        match self {
            Priority::Highest => 0,
            Priority::High => 1,
            Priority::Medium => 2,
            Priority::None => 3,
            Priority::Low => 4,
            Priority::Lowest => 5,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Priority::Highest => "Highest",
            Priority::High => "High",
            Priority::Medium => "Medium",
            Priority::None => "Normal",
            Priority::Low => "Low",
            Priority::Lowest => "Lowest",
        }
    }
}

// pub fn is_priority_marker(line: &str) -> bool {
//     line.chars().any(|c| matches!(c, '🔺' | '⏫' | '🔼' | '🔽' | '⏬'))
// }

fn extract_priority(line: &str) -> Priority {
    // Checked in this order (not first-occurring-marker order) because
    // some priority emoji could theoretically collide with other
    // Unicode content in a title - highest-priority match wins, matching
    // "if somehow multiple are present, prefer the most urgent" rather
    // than "whichever appears first in the string".
    for (marker, priority) in [
        ('🔺', Priority::Highest),
        ('⏫', Priority::High),
        ('🔼', Priority::Medium),
        ('🔽', Priority::Low),
        ('⏬', Priority::Lowest),
    ] {
        if line.contains(marker) {
            return priority;
        }
    }
    Priority::None
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecurUnit {
    Day,
    Week,
    Month,
    Year,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecurRule {
    pub unit: RecurUnit,
    pub interval: u32,
    /// Only meaningful for `Week` - an explicit weekday list (`on
    /// Monday, Friday`) or the `every weekday` shorthand (Mon-Fri).
    /// Empty means "the same weekday as the anchor date".
    pub weekdays: Vec<Weekday>,
    /// Explicit day-of-month (e.g. `Some(15)` for 15th, `Some(-1)` for last day)
    pub day_of_month: Option<i32>,
    /// Explicit ordinal weekday (e.g. `Some((1, Weekday::Mon))` for 1st Monday, `Some((-1, Weekday::Fri))` for last Friday)
    pub ordinal_weekday: Option<(i8, Weekday)>,
    pub when_done: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskMeta {
    pub due: Option<NaiveDate>,
    pub scheduled: Option<NaiveDate>,
    pub start: Option<NaiveDate>,
    pub recur: Option<RecurRule>,
    pub on_completion_delete: bool,
    pub priority: Priority,
}

impl Default for TaskMeta {
    fn default() -> Self {
        TaskMeta { due: None, scheduled: None, start: None, recur: None, on_completion_delete: false, priority: Priority::None }
    }
}

impl TaskMeta {
    /// `true` when none of the recognized emoji markers were found -
    /// callers use this to leave a plain `- [ ] buy milk` checkbox (no
    /// metadata at all) exactly as the ordinary check/uncheck toggle
    /// already handles it, with none of this module's extra behavior.
    pub fn is_empty(&self) -> bool {
        self.due.is_none()
            && self.scheduled.is_none()
            && self.start.is_none()
            && self.recur.is_none()
            && !self.on_completion_delete
            && matches!(self.priority, Priority::None)
    }

    /// 📅 due, else ⏳ scheduled, else 🛫 start - matches both Obsidian
    /// Tasks' own documented date priority and kotodo's `anchorDate()`.
    pub fn anchor(&self) -> Option<NaiveDate> {
        self.due.or(self.scheduled).or(self.start)
    }
}

pub fn parse_task_meta(line: &str) -> TaskMeta {
    TaskMeta {
        due: extract_date(line, DUE),
        scheduled: extract_date(line, SCHEDULED),
        start: extract_date(line, START),
        recur: extract_recur(line),
        on_completion_delete: extract_on_completion_delete(line),
        priority: extract_priority(line),
    }
}

fn extract_date(line: &str, marker: char) -> Option<NaiveDate> {
    let idx = line.find(marker)?;
    let rest = line[idx + marker.len_utf8()..].trim_start();
    let date_str: String = rest.chars().take(10).collect();
    NaiveDate::parse_from_str(&date_str, "%Y-%m-%d").ok()
}

fn extract_on_completion_delete(line: &str) -> bool {
    let Some(idx) = line.find(ON_COMPLETION) else { return false };
    line[idx + ON_COMPLETION.len_utf8()..].trim_start().starts_with("delete")
}

/// `🔁 every [N] day(s)|week(s)|month(s)|year(s) [on <weekday list|day|ordinal>] [when done]`,
/// or shorthands like `every weekday`, `every Mon, Wed`, `every month on the 15th`, `every month on the last day`, `every month on the 1st Monday`.
fn extract_recur(line: &str) -> Option<RecurRule> {
    let idx = line.find(RECUR)?;
    let rest = line[idx + RECUR.len_utf8()..].trim_start();
    let end = rest.find([DUE, SCHEDULED, START, DONE, ON_COMPLETION]).unwrap_or(rest.len());
    let mut text = rest[..end].trim().to_lowercase();

    let when_done = if text.ends_with("when done") {
        text = text.trim_end_matches("when done").trim().to_string();
        true
    } else if text.starts_with("when done") {
        text = text.strip_prefix("when done").unwrap().trim().to_string();
        true
    } else {
        false
    };

    let text_without_every = text.strip_prefix("every")?.trim();

    if text_without_every == "weekday" || text_without_every == "workday" {
        return Some(RecurRule {
            unit: RecurUnit::Week,
            interval: 1,
            weekdays: vec![Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri],
            day_of_month: None,
            ordinal_weekday: None,
            when_done,
        });
    }

    let contains_unit = ["day", "week", "month", "year"].iter().any(|u| text_without_every.contains(u));
    if !contains_unit {
        let direct_weekdays = parse_weekday_list(text_without_every);
        if !direct_weekdays.is_empty() {
            return Some(RecurRule {
                unit: RecurUnit::Week,
                interval: 1,
                weekdays: direct_weekdays,
                day_of_month: None,
                ordinal_weekday: None,
                when_done,
            });
        }
    }

    let (interval_text, rest_text) = match text_without_every.split_once(char::is_whitespace) {
        Some((n, r)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => (n, r.trim()),
        _ => ("1", text_without_every),
    };
    let interval: u32 = interval_text.parse().ok()?;

    let (unit_word, extra_text) = match rest_text.split_once(" on ") {
        Some((u, w)) => (u.trim(), Some(w.trim())),
        None => (rest_text, None),
    };

    let unit = match unit_word.trim_end_matches('s') {
        "day" => RecurUnit::Day,
        "week" => RecurUnit::Week,
        "month" => RecurUnit::Month,
        "year" => RecurUnit::Year,
        _ => return None,
    };

    let mut weekdays = Vec::new();
    let mut day_of_month = None;
    let mut ordinal_weekday = None;

    if let Some(extra) = extra_text {
        let clean_extra = extra.strip_prefix("the ").unwrap_or(extra).trim();
        if clean_extra == "last day" {
            day_of_month = Some(-1);
        } else if let Some(parsed_ordinal) = try_parse_ordinal_weekday(clean_extra) {
            ordinal_weekday = Some(parsed_ordinal);
        } else if let Some(d) = parse_day_number(clean_extra) {
            day_of_month = Some(d);
        } else {
            weekdays = parse_weekday_list(extra);
        }
    }

    Some(RecurRule {
        unit,
        interval,
        weekdays,
        day_of_month,
        ordinal_weekday,
        when_done,
    })
}

fn parse_day_number(s: &str) -> Option<i32> {
    let clean = s.trim_end_matches("st").trim_end_matches("nd").trim_end_matches("rd").trim_end_matches("th");
    let n: i32 = clean.parse().ok()?;
    if (1..=31).contains(&n) {
        Some(n)
    } else {
        None
    }
}

fn parse_ordinal(s: &str) -> Option<i8> {
    match s.to_lowercase().as_str() {
        "1st" | "first" => Some(1),
        "2nd" | "second" => Some(2),
        "3rd" | "third" => Some(3),
        "4th" | "fourth" => Some(4),
        "5th" | "fifth" => Some(5),
        "last" => Some(-1),
        _ => None,
    }
}

fn try_parse_ordinal_weekday(s: &str) -> Option<(i8, Weekday)> {
    let (ord_str, day_str) = s.split_once(char::is_whitespace)?;
    let ord = parse_ordinal(ord_str)?;
    let wday = parse_weekday(day_str)?;
    Some((ord, wday))
}

fn parse_weekday_list(text: &str) -> Vec<Weekday> {
    text.split(',').filter_map(|s| parse_weekday(s.trim())).collect()
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    Some(match s.to_lowercase().as_str() {
        "monday" | "mon" => Weekday::Mon,
        "tuesday" | "tue" => Weekday::Tue,
        "wednesday" | "wed" => Weekday::Wed,
        "thursday" | "thu" => Weekday::Thu,
        "friday" | "fri" => Weekday::Fri,
        "saturday" | "sat" => Weekday::Sat,
        "sunday" | "sun" => Weekday::Sun,
        _ => return None,
    })
}

const CYCLE_SEARCH_LIMIT: u32 = 4;

pub fn next_date_after(rule: &RecurRule, from: NaiveDate) -> Option<NaiveDate> {
    let interval = rule.interval.max(1);
    match rule.unit {
        RecurUnit::Day => Some(from + Duration::days(interval as i64)),
        RecurUnit::Week => next_weekly(rule, from, interval),
        RecurUnit::Month => next_monthly(rule, from, interval),
        RecurUnit::Year => next_yearly(from, interval, from.day()),
    }
}

fn next_weekly(rule: &RecurRule, from: NaiveDate, interval: u32) -> Option<NaiveDate> {
    let days: Vec<Weekday> = if rule.weekdays.is_empty() { vec![from.weekday()] } else { rule.weekdays.clone() };
    let anchor_week = week_start(from);
    let horizon = interval * 7 + 7;
    for offset in 1..=horizon {
        let candidate = from + Duration::days(offset as i64);
        let weeks = (week_start(candidate) - anchor_week).num_weeks();
        if weeks.rem_euclid(interval as i64) == 0 && days.contains(&candidate.weekday()) {
            return Some(candidate);
        }
    }
    None
}

fn week_start(date: NaiveDate) -> NaiveDate {
    date - Duration::days(date.weekday().num_days_from_monday() as i64)
}

fn next_monthly(rule: &RecurRule, from: NaiveDate, interval: u32) -> Option<NaiveDate> {
    for cycle in 0..=CYCLE_SEARCH_LIMIT {
        let candidate_month = add_months_clamped(from, cycle * interval, 1)?;
        let year = candidate_month.year();
        let month = candidate_month.month();
        let dim = days_in_month(year, month);

        let candidate_day = if let Some(dom) = rule.day_of_month {
            if dom < 0 {
                dim
            } else {
                (dom as u32).min(dim)
            }
        } else if let Some((ord, target_wday)) = rule.ordinal_weekday {
            match calculate_ordinal_weekday(year, month, ord, target_wday) {
                Some(d) => d,
                None => from.day().min(dim),
            }
        } else {
            from.day().min(dim)
        };

        if let Some(candidate) = NaiveDate::from_ymd_opt(year, month, candidate_day) {
            if candidate > from {
                return Some(candidate);
            }
        }
    }
    None
}

fn calculate_ordinal_weekday(year: i32, month: u32, ord: i8, target_wday: Weekday) -> Option<u32> {
    let dim = days_in_month(year, month);
    if ord > 0 {
        let mut count = 0;
        for d in 1..=dim {
            if let Some(dt) = NaiveDate::from_ymd_opt(year, month, d) {
                if dt.weekday() == target_wday {
                    count += 1;
                    if count == (ord as u8) {
                        return Some(d);
                    }
                }
            }
        }
    } else if ord == -1 {
        for d in (1..=dim).rev() {
            if let Some(dt) = NaiveDate::from_ymd_opt(year, month, d) {
                if dt.weekday() == target_wday {
                    return Some(d);
                }
            }
        }
    }
    None
}

fn next_yearly(from: NaiveDate, interval: u32, anchor_day: u32) -> Option<NaiveDate> {
    let anchor_month = from.month();
    for cycle in 0..=CYCLE_SEARCH_LIMIT {
        let year = from.year() + (cycle * interval) as i32;
        let candidate = clamp_to_month(year, anchor_month, anchor_day)?;
        if candidate > from {
            return Some(candidate);
        }
    }
    None
}

/// Adds `months` to `from`'s year/month, landing on `day` (clamped to
/// that target month's actual length).
fn add_months_clamped(from: NaiveDate, months: u32, day: u32) -> Option<NaiveDate> {
    let total = from.year() * 12 + (from.month() as i32 - 1) + months as i32;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) as u32 + 1;
    clamp_to_month(year, month, day)
}

fn clamp_to_month(year: i32, month: u32, day: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(year, month, day.min(days_in_month(year, month)))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next_month_first = if month == 12 { NaiveDate::from_ymd_opt(year + 1, 1, 1) } else { NaiveDate::from_ymd_opt(year, month + 1, 1) };
    (next_month_first.unwrap() - Duration::days(1)).day()
}

/// Shifts every present 📅/⏳/🛫 date on `line` by `delta` (all of them,
/// by the same amount, so their relative spacing is preserved - the
/// same effect as Obsidian Tasks' "if scheduled is 2 days before due,
/// that spacing persists").
pub fn shift_dates(line: &str, delta: Duration) -> String {
    let mut result = line.to_string();
    for marker in [DUE, SCHEDULED, START] {
        if let Some(date) = extract_date(&result, marker) {
            result = replace_date(&result, marker, date + delta);
        }
    }
    result
}

fn replace_date(line: &str, marker: char, new_date: NaiveDate) -> String {
    let Some(idx) = line.find(marker) else { return line.to_string() };
    let after_marker = idx + marker.len_utf8();
    let rest = &line[after_marker..];
    let date_start = after_marker + (rest.len() - rest.trim_start().len());
    let old_date_len = line[date_start..].chars().take(10).collect::<String>().len();
    let date_end = date_start + old_date_len;
    format!("{}{}{}", &line[..date_start], new_date.format("%Y-%m-%d"), &line[date_end..])
}

/// Removes an existing `✅ YYYY-MM-DD` marker (and its leading
/// whitespace) from `line` - used when building a recurring task's next
/// occurrence, which must start undone.
pub fn strip_done_date(line: &str) -> String {
    let Some(idx) = line.find(DONE) else { return line.to_string() };
    let after_marker = idx + DONE.len_utf8();
    let rest = &line[after_marker..];
    let date_start = after_marker + (rest.len() - rest.trim_start().len());
    let date_len = line[date_start..].chars().take(10).collect::<String>().len();
    let mut result = line[..idx].trim_end().to_string();
    result.push_str(&line[date_start + date_len..]);
    result
}

/// Appends `✅ <today>` if `line` doesn't already have a done-date -
/// idempotent so re-stamping an already-stamped line is a no-op.
pub fn stamp_done_date(line: &str, today: NaiveDate) -> String {
    if line.contains(DONE) {
        return line.to_string();
    }
    format!("{} {} {}", line.trim_end(), DONE, today.format("%Y-%m-%d"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn extracts_priority_markers() {
        assert_eq!(extract_priority("- [ ] task 🔺"), Priority::Highest);
        assert_eq!(extract_priority("- [ ] task ⏫"), Priority::High);
        assert_eq!(extract_priority("- [ ] task 🔼"), Priority::Medium);
        assert_eq!(extract_priority("- [ ] task 🔽"), Priority::Low);
        assert_eq!(extract_priority("- [ ] task ⏬"), Priority::Lowest);
        assert_eq!(extract_priority("- [ ] task"), Priority::None);
    }

    #[test]
    fn priority_rank_orders_urgent_first_with_none_in_the_middle() {
        let mut ranks: Vec<u8> =
            [Priority::Highest, Priority::High, Priority::Medium, Priority::None, Priority::Low, Priority::Lowest]
                .iter()
                .map(|p| p.rank())
                .collect();
        let sorted = { let mut r = ranks.clone(); r.sort(); r };
        assert_eq!(ranks, sorted); // already in rank order as listed
        ranks.dedup();
        assert_eq!(ranks.len(), 6); // all distinct
    }

    #[test]
    fn parses_all_three_reference_dates() {
        let meta = parse_task_meta("- [ ] task 📅 2026-03-10 ⏳ 2026-03-08 🛫 2026-03-01");
        assert_eq!(meta.due, Some(date(2026, 3, 10)));
        assert_eq!(meta.scheduled, Some(date(2026, 3, 8)));
        assert_eq!(meta.start, Some(date(2026, 3, 1)));
    }

    #[test]
    fn anchor_prefers_due_then_scheduled_then_start() {
        assert_eq!(
            TaskMeta { due: Some(date(2026, 1, 3)), scheduled: Some(date(2026, 1, 1)), ..Default::default() }.anchor(),
            Some(date(2026, 1, 3))
        );
        assert_eq!(TaskMeta { scheduled: Some(date(2026, 1, 1)), start: Some(date(2026, 1, 2)), ..Default::default() }.anchor(), Some(date(2026, 1, 1)));
        assert_eq!(TaskMeta { start: Some(date(2026, 1, 2)), ..Default::default() }.anchor(), Some(date(2026, 1, 2)));
    }

    #[test]
    fn a_plain_checkbox_has_empty_meta() {
        assert!(parse_task_meta("- [ ] buy milk").is_empty());
    }

    #[test]
    fn parses_simple_interval_recurrence() {
        let rule = extract_recur("🔁 every 3 days").unwrap();
        assert_eq!(rule.unit, RecurUnit::Day);
        assert_eq!(rule.interval, 3);
        assert!(!rule.when_done);
    }

    #[test]
    fn parses_when_done_modifier() {
        let rule = extract_recur("🔁 every week when done").unwrap();
        assert_eq!(rule.unit, RecurUnit::Week);
        assert!(rule.when_done);
    }

    #[test]
    fn parses_explicit_weekday_list() {
        let rule = extract_recur("🔁 every week on Tuesday, Friday").unwrap();
        assert_eq!(rule.weekdays, vec![Weekday::Tue, Weekday::Fri]);
    }

    #[test]
    fn every_weekday_shorthand_means_mon_to_fri() {
        let rule = extract_recur("🔁 every weekday").unwrap();
        assert_eq!(rule.unit, RecurUnit::Week);
        assert_eq!(rule.weekdays, vec![Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri]);
    }

    #[test]
    fn recur_text_stops_at_the_next_marker() {
        let rule = extract_recur("🔁 every month 📅 2026-01-31").unwrap();
        assert_eq!(rule.unit, RecurUnit::Month);
        assert_eq!(rule.interval, 1);
    }

    #[test]
    fn on_completion_delete_is_recognized() {
        assert!(extract_on_completion_delete("- [ ] task 🏁 delete"));
        assert!(!extract_on_completion_delete("- [ ] task 🏁 keep"));
        assert!(!extract_on_completion_delete("- [ ] task"));
    }

    #[test]
    fn daily_steps_by_the_interval() {
        let rule = RecurRule { unit: RecurUnit::Day, interval: 3, weekdays: vec![], day_of_month: None, ordinal_weekday: None, when_done: false };
        assert_eq!(next_date_after(&rule, date(2026, 1, 1)), Some(date(2026, 1, 4)));
    }

    #[test]
    fn weekly_with_no_explicit_weekday_repeats_on_the_anchors_own_weekday() {
        // 2026-01-01 is a Thursday.
        let rule = RecurRule { unit: RecurUnit::Week, interval: 1, weekdays: vec![], day_of_month: None, ordinal_weekday: None, when_done: false };
        assert_eq!(next_date_after(&rule, date(2026, 1, 1)), Some(date(2026, 1, 8)));
    }

    #[test]
    fn weekly_with_explicit_weekdays_finds_the_next_matching_one() {
        // 2026-01-01 is a Thursday; next of {Tue, Fri} after it is Fri 1-02.
        let rule = RecurRule { unit: RecurUnit::Week, interval: 1, weekdays: vec![Weekday::Tue, Weekday::Fri], day_of_month: None, ordinal_weekday: None, when_done: false };
        assert_eq!(next_date_after(&rule, date(2026, 1, 1)), Some(date(2026, 1, 2)));
    }

    #[test]
    fn biweekly_skips_the_off_week() {
        let rule = RecurRule { unit: RecurUnit::Week, interval: 2, weekdays: vec![Weekday::Mon], day_of_month: None, ordinal_weekday: None, when_done: false };
        let from = date(2026, 1, 1); // Thursday
        let next = next_date_after(&rule, from).unwrap();
        assert_eq!(next.weekday(), Weekday::Mon);
        assert!((next - week_start(from)).num_weeks() % 2 == 0);
    }

    #[test]
    fn monthly_clamps_the_31st_to_february() {
        let rule = RecurRule { unit: RecurUnit::Month, interval: 1, weekdays: vec![], day_of_month: None, ordinal_weekday: None, when_done: false };
        assert_eq!(next_date_after(&rule, date(2026, 1, 31)), Some(date(2026, 2, 28)));
    }

    #[test]
    fn monthly_clamps_to_february_29_in_a_leap_year() {
        let rule = RecurRule { unit: RecurUnit::Month, interval: 1, weekdays: vec![], day_of_month: None, ordinal_weekday: None, when_done: false };
        assert_eq!(next_date_after(&rule, date(2028, 1, 31)), Some(date(2028, 2, 29)));
    }

    #[test]
    fn yearly_steps_by_the_interval_and_clamps_leap_day() {
        let rule = RecurRule { unit: RecurUnit::Year, interval: 1, weekdays: vec![], day_of_month: None, ordinal_weekday: None, when_done: false };
        assert_eq!(next_date_after(&rule, date(2028, 2, 29)), Some(date(2029, 2, 28)));
    }

    #[test]
    fn parses_and_calculates_explicit_day_of_month_recurrence() {
        let rule = extract_recur("🔁 every month on the 15th").unwrap();
        assert_eq!(rule.unit, RecurUnit::Month);
        assert_eq!(rule.day_of_month, Some(15));
        assert_eq!(next_date_after(&rule, date(2026, 1, 1)), Some(date(2026, 1, 15)));
    }

    #[test]
    fn parses_and_calculates_last_day_of_month_recurrence() {
        let rule = extract_recur("🔁 every month on the last day").unwrap();
        assert_eq!(rule.unit, RecurUnit::Month);
        assert_eq!(rule.day_of_month, Some(-1));
        assert_eq!(next_date_after(&rule, date(2026, 1, 1)), Some(date(2026, 1, 31)));
        assert_eq!(next_date_after(&rule, date(2026, 2, 1)), Some(date(2026, 2, 28)));
    }

    #[test]
    fn parses_and_calculates_ordinal_weekday_recurrence() {
        let rule = extract_recur("🔁 every month on the 1st Monday").unwrap();
        assert_eq!(rule.unit, RecurUnit::Month);
        assert_eq!(rule.ordinal_weekday, Some((1, Weekday::Mon)));
        // 2026-01-01 is Thursday; 1st Monday is Jan 5
        assert_eq!(next_date_after(&rule, date(2026, 1, 1)), Some(date(2026, 1, 5)));
    }

    #[test]
    fn shift_dates_moves_every_present_date_by_the_same_amount() {
        let line = "- [ ] task 📅 2026-01-10 🛫 2026-01-08";
        let shifted = shift_dates(line, Duration::days(7));
        assert_eq!(shifted, "- [ ] task 📅 2026-01-17 🛫 2026-01-15");
    }

    #[test]
    fn strip_done_date_removes_the_marker_and_date() {
        assert_eq!(strip_done_date("- [x] task ✅ 2026-01-10"), "- [x] task");
        assert_eq!(strip_done_date("- [ ] task"), "- [ ] task");
    }

    #[test]
    fn stamp_done_date_appends_once() {
        let stamped = stamp_done_date("- [x] task", date(2026, 1, 10));
        assert_eq!(stamped, "- [x] task ✅ 2026-01-10");
        // Already stamped - unchanged.
        assert_eq!(stamp_done_date(&stamped, date(2026, 1, 11)), stamped);
    }
}
