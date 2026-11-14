//! Local (non-SSH) workspace file operations and workspace-wide
//! search/replace - ported from kotoshelf's `src-tauri/src/lib.rs`
//! (Tauri app) with `#[tauri::command]` and Tauri-specific plumbing
//! stripped out. Plain functions callable directly from egui event
//! handlers, no IPC/serialization boundary to cross anymore.

use agent_protocol::{is_skipped_dir, MAX_DEPTH};
use std::path::{Path, PathBuf};

/// Every file path in the workspace (directories omitted) - used for
/// wiki-link resolution across the whole workspace regardless of which
/// folders the lazy tree pane has expanded so far.
fn walk_all_paths(dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !is_skipped_dir(&name) {
                walk_all_paths(&path, depth + 1, out);
            }
        } else {
            out.push(path.to_string_lossy().into_owned());
        }
    }
}

pub fn list_all_paths(root: &str) -> Vec<String> {
    let mut out = Vec::new();
    walk_all_paths(Path::new(root), 0, &mut out);
    out
}

pub fn read_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("Failed to read {path}: {e}"))
}

pub fn write_file(path: &str, content: &str) -> Result<(), String> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    std::fs::write(path, content).map_err(|e| format!("Failed to write {path}: {e}"))
}

pub fn create_dir(path: &str) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("Failed to create {path}: {e}"))
}

// ---------------------------------------------------------------------
// App workspace directory (.kotoshelf) & .gitignore integration
// ---------------------------------------------------------------------

pub const APP_WORKSPACE_DIR: &str = ".kotoshelf";
pub const APP_WORKSPACE_GITIGNORE_ENTRY: &str = ".kotoshelf/";

/// App-managed workspace folders/files created under `.kotoshelf/` and their concise descriptions,
/// displayed in the confirmation dialog before creating `.kotoshelf/` and updating `.gitignore`.
pub const APP_WORKSPACE_MANAGED_PATHS: &[(&str, &str)] = &[
    (".kotoshelf/", "Workspace metadata folder"),
    (".kotoshelf/settings.json", "Editor & view settings"),
    (".kotoshelf/history/", "File revision snapshots"),
    (".kotoshelf/search.json", "Search exclude patterns"),
];

/// Returns `true` if `gitignore_content` already has a rule ignoring `dir_name` (e.g. `".kotoshelf"`).
pub fn gitignore_content_ignores_dir(gitignore_content: &str, dir_name: &str) -> bool {
    let bare = dir_name.trim_matches('/');
    for raw_line in gitignore_content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let normalized = line.trim_start_matches('/').trim_end_matches('/');
        if normalized == bare
            || normalized == format!("{bare}/*")
            || normalized == format!("**/{bare}")
            || normalized == format!("**/{bare}/*")
            || line == ".*"
            || line == ".*/"
        {
            return true;
        }
    }
    false
}

/// Returns `true` if `root/.gitignore` (or `<git_root>/.gitignore` when `root` is inside a Git repo)
/// already ignores `.kotoshelf`.
pub fn is_kotoshelf_gitignored(root: &Path) -> bool {
    let local_gitignore = root.join(".gitignore");
    if let Ok(content) = std::fs::read_to_string(&local_gitignore) {
        if gitignore_content_ignores_dir(&content, APP_WORKSPACE_DIR) {
            return true;
        }
    }
    if let Some(git_root) = crate::git::find_git_root(root) {
        if git_root != root {
            if let Ok(content) = std::fs::read_to_string(git_root.join(".gitignore")) {
                if gitignore_content_ignores_dir(&content, APP_WORKSPACE_DIR) {
                    return true;
                }
                if let Ok(rel) = root.strip_prefix(&git_root) {
                    let rel_entry = format!(
                        "{}/{}",
                        rel.to_string_lossy().replace('\\', "/").trim_matches('/'),
                        APP_WORKSPACE_DIR
                    );
                    if gitignore_content_ignores_dir(&content, &rel_entry) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Appends `.kotoshelf/` to `root/.gitignore` if `root` is a Git repository and `.kotoshelf`
/// is not already ignored. Returns `Ok(true)` if `.gitignore` was created or updated,
/// or `Ok(false)` if no change was needed.
pub fn ensure_kotoshelf_in_gitignore(root: &Path) -> Result<bool, String> {
    if !crate::git::is_git_repo(root) {
        return Ok(false);
    }
    if is_kotoshelf_gitignored(root) {
        return Ok(false);
    }
    let gitignore_path = root.join(".gitignore");
    let mut content = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(APP_WORKSPACE_GITIGNORE_ENTRY);
    content.push('\n');
    std::fs::write(&gitignore_path, content)
        .map_err(|e| format!("Failed to update {}: {e}", gitignore_path.display()))?;
    Ok(true)
}

/// Creates `<root>/.kotoshelf` and `<root>/.kotoshelf/history`, and optionally adds `.kotoshelf/`
/// to `<root>/.gitignore` when `add_to_gitignore` is `true`.
/// Returns `Ok(true)` if `.gitignore` was updated.
pub fn initialize_kotoshelf_dir(root: &Path, add_to_gitignore: bool) -> Result<bool, String> {
    let koto_dir = root.join(APP_WORKSPACE_DIR);
    let history_dir = koto_dir.join("history");
    std::fs::create_dir_all(&history_dir)
        .map_err(|e| format!("Failed to create {}: {e}", history_dir.display()))?;
    if add_to_gitignore {
        ensure_kotoshelf_in_gitignore(root)
    } else {
        Ok(false)
    }
}

/// Returns `true` if `root` is a Git repository and the user should be prompted about
/// creating `.kotoshelf/` and/or adding `.kotoshelf/` to `.gitignore`:
/// - `.kotoshelf` missing (`!exists`): always prompt (regardless of `.gitignore` state).
/// - `.kotoshelf` exists + `.gitignore` already ignores `.kotoshelf`: do NOT prompt.
/// - `.kotoshelf` exists + `.gitignore` does NOT ignore `.kotoshelf`: prompt ONLY if
///   `gitignore_prompted` is not `true` in `.kotoshelf/settings.json`.
pub fn needs_kotoshelf_gitignore_prompt(root: &Path) -> bool {
    if !crate::git::is_git_repo(root) {
        return false;
    }
    if !root.join(APP_WORKSPACE_DIR).exists() {
        return true;
    }
    if is_kotoshelf_gitignored(root) {
        return false;
    }
    !load_workspace_settings(root)
        .map(|s| s.gitignore_prompted)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// File history (local workspace only)
// ---------------------------------------------------------------------

/// How many of a note's most recent snapshots survive pruning, and how
/// many days an older one is allowed to stick around regardless of
/// count - two independent caps, either one triggers deletion.
const HISTORY_MAX_VERSIONS: usize = 20;
const HISTORY_MAX_AGE_DAYS: u64 = 30;

/// Snapshots `file_path`'s current on-disk content into
/// `.kotoshelf/history/<relative path>/` before it's overwritten with
/// `new_content` - the local-workspace equivalent of Obsidian's core
/// "File Recovery" plugin: no restore UI, just timestamped copies left
/// on disk for the user to dig through manually if needed. A no-op when
/// the file doesn't exist yet (nothing to preserve) or `new_content`
/// matches what's already there (no point snapshotting a no-op save).
///
/// SSH workspaces don't get this - it would mean a read-before-write
/// round trip over the connection on every single save, just for a
/// safety net the user may never need.
pub fn snapshot_history(root: &Path, file_path: &Path, new_content: &str) {
    let Ok(old_content) = std::fs::read_to_string(file_path) else {
        return;
    };
    if old_content == new_content {
        return;
    }
    let Ok(rel) = file_path.strip_prefix(root) else {
        return;
    };
    let history_dir = root.join(".kotoshelf").join("history").join(rel);
    if std::fs::create_dir_all(&history_dir).is_err() {
        return;
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("md");
    let snapshot_path = history_dir.join(format!("{stamp}.{ext}"));
    let _ = std::fs::write(&snapshot_path, &old_content);
    prune_history_dir(&history_dir);
}

/// Keeps only `HISTORY_MAX_VERSIONS` most recent snapshots in `dir`,
/// and drops anything older than `HISTORY_MAX_AGE_DAYS` regardless of
/// that count - deliberately no whole-vault size budget on top of this,
/// since enforcing one would mean walking every note's history on every
/// save just to compute it.
fn prune_history_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (e.path(), t)))
        .collect();
    files.sort_by_key(|(_, mtime)| *mtime);

    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(60 * 60 * 24 * HISTORY_MAX_AGE_DAYS))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let keep_from_index = files.len().saturating_sub(HISTORY_MAX_VERSIONS);
    for (i, (path, mtime)) in files.iter().enumerate() {
        if i < keep_from_index || *mtime < cutoff {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Writes `bytes` to `dest_path`, creating its parent directory first if
/// needed - used by the drag-and-drop/clipboard-paste attach flow, where
/// the destination is an `attachments/` subdirectory next to the active
/// note that may not exist yet.
pub fn write_bytes_into(dest_path: &str, bytes: &[u8]) -> Result<(), String> {
    let dest = Path::new(dest_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create {parent:?}: {e}"))?;
    }
    std::fs::write(dest, bytes).map_err(|e| format!("Failed to write {dest_path}: {e}"))
}

pub fn copy_node(src: &str, dst: &str) -> Result<(), String> {
    let src_path = Path::new(src);
    let dst_path = Path::new(dst);
    if let Some(parent) = dst_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create parent dir: {e}"))?;
    }
    if src_path.is_dir() {
        copy_dir_all(src_path, dst_path)
    } else {
        std::fs::copy(src_path, dst_path)
            .map(|_| ())
            .map_err(|e| format!("Failed to copy {src} to {dst}: {e}"))
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("Failed to create dir {dst:?}: {e}"))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("Failed to read dir {src:?}: {e}"))?;
    for entry in entries.flatten() {
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let src_child = entry.path();
        let dst_child = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&src_child, &dst_child)?;
        } else {
            std::fs::copy(&src_child, &dst_child).map_err(|e| format!("Failed to copy file: {e}"))?;
        }
    }
    Ok(())
}

pub fn move_node(src: &str, dst: &str) -> Result<(), String> {
    let dst_path = Path::new(dst);
    if let Some(parent) = dst_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create parent dir: {e}"))?;
    }
    std::fs::rename(src, dst).map_err(|e| format!("Failed to move {src} to {dst}: {e}"))
}

pub fn rename_path(from: &str, to: &str) -> Result<(), String> {
    if Path::new(to).exists() {
        return Err(format!("Already exists: {to}"));
    }
    std::fs::rename(from, to).map_err(|e| format!("Rename failed: {e}"))
}

/// Move a file or directory to the OS trash / recycle bin. Deliberately
/// NOT a permanent delete - tree operations should always be recoverable.
pub fn trash_path(path: &str) -> Result<(), String> {
    trash::delete(path).map_err(|e| format!("Delete failed: {e}"))
}

// ---------------------------------------------------------------------
// Workspace-wide search & replace
// ---------------------------------------------------------------------

/// User-editable search scope, persisted at `.kotoshelf/search.json`
/// inside the workspace. VS Code-style glob patterns (matched relative
/// to the workspace root), exclude-only - keeping this to one list
/// instead of separate include/exclude is enough for "don't search
/// *.txt" style requests without users needing to maintain an include
/// list that re-covers every extension they DO want.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SearchConfig {
    pub exclude: Vec<String>,
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig {
            exclude: vec![
                "**/*.png".into(),
                "**/*.jpg".into(),
                "**/*.jpeg".into(),
                "**/*.gif".into(),
                "**/*.webp".into(),
                "**/*.svg".into(),
                "**/*.bmp".into(),
                "**/*.pdf".into(),
                "**/*.zip".into(),
                "**/*.exe".into(),
                "**/*.dll".into(),
            ],
        }
    }
}

fn search_config_path(root: &Path) -> PathBuf {
    root.join(".kotoshelf").join("search.json")
}

pub fn load_search_config(root: &str) -> SearchConfig {
    let path = search_config_path(Path::new(root));
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_search_config(root: &str, config: &SearchConfig) -> Result<(), String> {
    let path = search_config_path(Path::new(root));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("Failed to write {path:?}: {e}"))
}

// ---------------------------------------------------------------------
// Per-workspace view & editor settings (.kotoshelf/settings.json)
// ---------------------------------------------------------------------

/// Per-workspace editor and view settings persisted at `.kotoshelf/settings.json`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSettings {
    #[serde(default)]
    pub font_selection: crate::fonts::FontSelection,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default = "default_true")]
    pub show_line_numbers: bool,
    #[serde(default = "default_true")]
    pub word_wrap: bool,
    #[serde(default = "default_zoom_factor")]
    pub zoom_factor: f32,
    #[serde(default)]
    pub preview_visible: bool,
    #[serde(default)]
    pub gitignore_prompted: bool,
}

fn default_font_size() -> f32 {
    14.0
}

fn default_true() -> bool {
    true
}

fn default_zoom_factor() -> f32 {
    1.0
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        Self {
            font_selection: crate::fonts::FontSelection::default(),
            font_size: default_font_size(),
            show_line_numbers: true,
            word_wrap: true,
            zoom_factor: default_zoom_factor(),
            preview_visible: false,
            gitignore_prompted: false,
        }
    }
}

fn workspace_settings_path(root: &Path) -> PathBuf {
    root.join(".kotoshelf").join("settings.json")
}

pub fn load_workspace_settings(root: &Path) -> Option<WorkspaceSettings> {
    let path = workspace_settings_path(root);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_workspace_settings(root: &Path, settings: &WorkspaceSettings) -> Result<(), String> {
    let path = workspace_settings_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("Failed to write {path:?}: {e}"))
}

fn build_exclude_matcher(patterns: &[String]) -> globset::GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    for p in patterns {
        if let Ok(glob) = globset::GlobBuilder::new(p).literal_separator(false).build() {
            builder.add(glob);
        }
        // Invalid pattern from a hand-edited config: skip it rather than
        // fail the whole search over one typo'd glob.
    }
    builder.build().unwrap_or_else(|_| globset::GlobSet::empty())
}

fn walk_searchable_files(
    dir: &Path,
    root: &Path,
    depth: usize,
    exclude: &globset::GlobSet,
    include_ignored: bool,
    out: &mut Vec<PathBuf>,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        if !include_ignored && exclude.is_match(rel) {
            continue;
        }
        if path.is_dir() {
            if include_ignored || !is_skipped_dir(&name) {
                walk_searchable_files(&path, root, depth + 1, exclude, include_ignored, out);
            }
        } else {
            out.push(path);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchMatch {
    pub line: usize, // 1-based, matches editor line numbers
    pub col: usize,  // 0-based char offset into the line
    pub len: usize,  // match length in chars
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileSearchResult {
    pub path: String,
    pub matches: Vec<SearchMatch>,
}

pub fn build_regex(query: &str, is_regex: bool, case_sensitive: bool) -> Result<regex::Regex, String> {
    let pattern = if is_regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| format!("Invalid pattern: {e}"))
}

/// Builds one or more regexes for `query`:
/// - When `is_regex` is `true`, compiles `query` as a single regex.
/// - When `is_regex` is `false`, splits `query` on whitespace into multiple keywords (AND search)
///   so `"rust async"` requires every keyword to appear in the file.
pub fn build_query_regexes(
    query: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> Result<Vec<regex::Regex>, String> {
    if is_regex {
        return Ok(vec![build_regex(query, true, case_sensitive)?]);
    }
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.len() <= 1 {
        return Ok(vec![build_regex(query.trim(), false, case_sensitive)?]);
    }
    terms
        .into_iter()
        .map(|term| build_regex(term, false, case_sensitive))
        .collect()
}

/// Hard cap on matches returned so a query like "e" over a large
/// workspace can't produce an unbounded response. `pub(crate)` so the SSH
/// search variant in `ssh.rs` shares the exact same cap rather than
/// picking its own number.
pub(crate) const MAX_TOTAL_MATCHES: usize = 2000;

/// Regex-scans one file's content for matches, line by line. Column/
/// length in chars (not bytes), 200-char trimmed preview.
pub fn scan_matches(content: &str, re: &regex::Regex) -> Vec<SearchMatch> {
    let mut matches = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        for m in re.find_iter(line) {
            let col = line[..m.start()].chars().count();
            let len = line[m.start()..m.end()].chars().count();
            matches.push(SearchMatch {
                line: line_no + 1,
                col,
                len,
                preview: line.trim().chars().take(200).collect(),
            });
        }
    }
    matches
}

/// Scans `content` against one or more compiled keyword regexes:
/// - For a single regex, behaves identically to [`scan_matches`].
/// - For multiple keywords, requires **every** keyword to appear somewhere in `content` (file-level AND),
///   and then collects matches for all keyword occurrences within that file, sorted by `(line, col)`.
pub fn scan_matches_multi(content: &str, regexes: &[regex::Regex]) -> Vec<SearchMatch> {
    if regexes.is_empty() {
        return Vec::new();
    }
    if regexes.len() == 1 {
        return scan_matches(content, &regexes[0]);
    }
    if !regexes.iter().all(|re| re.is_match(content)) {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let preview: String = line.trim().chars().take(200).collect();
        for re in regexes {
            for m in re.find_iter(line) {
                let col = line[..m.start()].chars().count();
                let len = line[m.start()..m.end()].chars().count();
                matches.push(SearchMatch {
                    line: line_no + 1,
                    col,
                    len,
                    preview: preview.clone(),
                });
            }
        }
    }
    matches.sort_by_key(|m| (m.line, m.col));
    matches.dedup_by(|a, b| a.line == b.line && a.col == b.col);
    matches
}

/// Filters existing `results` in-memory by `filter_query` (space-separated AND terms matched against
/// the file `path` or match `preview`), enabling instant browser-style page filtering over search results.
pub fn filter_search_results(
    results: &[FileSearchResult],
    filter_query: &str,
    case_sensitive: bool,
) -> Vec<FileSearchResult> {
    let terms: Vec<String> = filter_query
        .split_whitespace()
        .map(|t| if case_sensitive { t.to_string() } else { t.to_lowercase() })
        .collect();
    if terms.is_empty() {
        return results.to_vec();
    }
    let mut filtered = Vec::new();
    for file_res in results {
        let path_cmp = if case_sensitive {
            file_res.path.clone()
        } else {
            file_res.path.to_lowercase()
        };
        let matching_lines: Vec<SearchMatch> = file_res
            .matches
            .iter()
            .filter(|m| {
                let preview_cmp = if case_sensitive {
                    m.preview.clone()
                } else {
                    m.preview.to_lowercase()
                };
                terms
                    .iter()
                    .all(|term| path_cmp.contains(term.as_str()) || preview_cmp.contains(term.as_str()))
            })
            .cloned()
            .collect();
        if !matching_lines.is_empty() {
            filtered.push(FileSearchResult {
                path: file_res.path.clone(),
                matches: matching_lines,
            });
        }
    }
    filtered
}

/// Formats `results` into a Markdown document with `[[relative_path]]` WikiLinks so the user can
/// open the search results in a new editor tab, use in-editor `Ctrl+F` (`Find`), or jump to matched files.
pub fn format_search_results_markdown(
    query: &str,
    filter_query: &str,
    results: &[FileSearchResult],
    workspace_root: Option<&Path>,
) -> String {
    let total_matches: usize = results.iter().map(|r| r.matches.len()).sum();
    let mut out = if filter_query.trim().is_empty() {
        format!(
            "# Search Results: `{}` ({} matches in {} files)\n\n",
            query.trim(),
            total_matches,
            results.len()
        )
    } else {
        format!(
            "# Search Results: `{}` (filter: `{}` — {} matches in {} files)\n\n",
            query.trim(),
            filter_query.trim(),
            total_matches,
            results.len()
        )
    };
    for file_res in results {
        let rel_path = workspace_root
            .and_then(|root| Path::new(&file_res.path).strip_prefix(root).ok())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| file_res.path.replace('\\', "/"));
        out.push_str(&format!("## [[{rel_path}]]\n\n"));
        for m in &file_res.matches {
            out.push_str(&format!("- L{}: {}\n", m.line, m.preview));
        }
        out.push('\n');
    }
    out
}

/// Precomputed once per file: `line_starts[n]` is the char offset where
/// line `n + 1` begins (0-based index, matching `SearchMatch::line`'s
/// 1-based numbering once you subtract 1) - shared by `find.rs`'s
/// in-buffer match navigation and the workspace-search-result-click
/// handler in `app.rs`, both of which need to turn a `SearchMatch`'s
/// (line, col) back into a single char offset into the whole buffer.
pub fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        offset += line.chars().count();
        starts.push(offset);
    }
    starts
}

/// Converts one `SearchMatch` into a `(start, end)` char-offset range
/// into the buffer `line_starts` was computed from.
pub fn match_char_range(line_starts: &[usize], m: &SearchMatch) -> Option<(usize, usize)> {
    let line_start = *line_starts.get(m.line - 1)?;
    let start = line_start + m.col;
    Some((start, start + m.len))
}

/// Pushes `matches` for `path` onto `results`, applying the running
/// `MAX_TOTAL_MATCHES` cap. Returns `true` once the cap is hit, so the
/// caller knows to stop scanning further files. `pub(crate)` so the SSH
/// search variant in `ssh.rs` can reuse it instead of duplicating the
/// cap-enforcement logic.
pub(crate) fn push_capped(
    results: &mut Vec<FileSearchResult>,
    total: &mut usize,
    path: String,
    mut matches: Vec<SearchMatch>,
) -> bool {
    if matches.is_empty() {
        return false;
    }
    *total += matches.len();
    let over = total.saturating_sub(MAX_TOTAL_MATCHES);
    if over > 0 {
        matches.truncate(matches.len().saturating_sub(over));
    }
    if !matches.is_empty() {
        results.push(FileSearchResult { path, matches });
    }
    over > 0
}

pub fn search_workspace(
    root: &str,
    query: &str,
    is_regex: bool,
    case_sensitive: bool,
    include_ignored: bool,
) -> Result<Vec<FileSearchResult>, String> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let regexes = build_query_regexes(query, is_regex, case_sensitive)?;

    let root_path = Path::new(root);
    let config = load_search_config(root);
    let exclude = build_exclude_matcher(&config.exclude);
    let mut files = Vec::new();
    walk_searchable_files(root_path, root_path, 0, &exclude, include_ignored, &mut files);

    let mut results = Vec::new();
    let mut total = 0usize;
    for path in files {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue; // binary or unreadable - skip rather than fail the whole search
        };
        let matches = scan_matches_multi(&content, &regexes);
        if push_capped(&mut results, &mut total, path.to_string_lossy().into_owned(), matches) {
            break;
        }
    }
    Ok(results)
}

pub struct ReplaceResult {
    pub path: String,
    pub count: usize,
}

/// Replaces every match of `query` with `replacement` in `content`,
/// returning the new content and how many matches were replaced.
pub fn apply_replace(content: &str, re: &regex::Regex, replacement: &str) -> (String, usize) {
    let count = re.find_iter(content).count();
    (re.replace_all(content, replacement).into_owned(), count)
}

/// Replace every match of `query` with `replacement` in each of `paths`.
/// Files with zero matches are left untouched (and omitted from the
/// result) so the caller can tell which files actually changed.
pub fn replace_in_files(
    paths: &[String],
    query: &str,
    replacement: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> Result<Vec<ReplaceResult>, String> {
    let re = build_regex(query, is_regex, case_sensitive)?;
    let mut results = Vec::new();
    for path in paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let (replaced, count) = apply_replace(&content, &re, replacement);
        if count == 0 {
            continue;
        }
        std::fs::write(path, replaced.as_bytes())
            .map_err(|e| format!("Failed to write {path}: {e}"))?;
        results.push(ReplaceResult { path: path.clone(), count });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_matches_reports_char_offsets_not_byte_offsets() {
        let re = build_regex("wa", false, false).unwrap();
        let matches = scan_matches("\u{6c17}持ち good", &re);
        // No match here - just confirms multi-byte content doesn't panic
        // on the byte-index slicing inside scan_matches.
        assert!(matches.is_empty());

        let matches = scan_matches("héllo world", &re);
        assert_eq!(matches.len(), 0);
        let re2 = build_regex("world", false, false).unwrap();
        let matches2 = scan_matches("héllo world", &re2);
        assert_eq!(matches2.len(), 1);
        // "héllo " is 6 chars even though "é" is 2 bytes in UTF-8 - col
        // must be a char count, not a byte offset.
        assert_eq!(matches2[0].col, 6);
    }

    #[test]
    fn apply_replace_counts_and_replaces_all_matches() {
        let re = build_regex("foo", false, false).unwrap();
        let (replaced, count) = apply_replace("foo bar foo baz", &re, "X");
        assert_eq!(count, 2);
        assert_eq!(replaced, "X bar X baz");
    }

    fn history_test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kotoshelf_history_test_{label}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn snapshot_history_skips_new_files_and_unchanged_content() {
        let root = history_test_dir("noop");
        let file = root.join("note.md");
        let history_dir = root.join(".kotoshelf").join("history").join("note.md");

        // Nothing on disk yet - no old content to preserve.
        snapshot_history(&root, &file, "new content");
        assert!(!history_dir.exists());

        std::fs::write(&file, "v1").unwrap();

        // Saving the same content back is a no-op save, not a real edit.
        snapshot_history(&root, &file, "v1");
        assert!(!history_dir.exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn snapshot_history_preserves_the_old_content_before_an_overwrite() {
        let root = history_test_dir("snapshot");
        let file = root.join("note.md");
        std::fs::write(&file, "v1").unwrap();

        snapshot_history(&root, &file, "v2");

        let history_dir = root.join(".kotoshelf").join("history").join("note.md");
        let entries: Vec<_> = std::fs::read_dir(&history_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(std::fs::read_to_string(entries[0].path()).unwrap(), "v1");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prune_history_dir_enforces_the_count_cap() {
        let dir = history_test_dir("count_cap");
        let now = std::time::SystemTime::now();
        for i in 0..HISTORY_MAX_VERSIONS + 5 {
            let path = dir.join(format!("v{i:03}.md"));
            std::fs::write(&path, "x").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(now - std::time::Duration::from_secs(60 * (HISTORY_MAX_VERSIONS as u64 + 5 - i as u64))).unwrap();
        }

        prune_history_dir(&dir);

        let remaining: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(remaining.len(), HISTORY_MAX_VERSIONS);
        // The 5 oldest (v000..v004) should be gone; the newest survives.
        assert!(!remaining.contains(&"v000.md".to_string()));
        assert!(remaining.contains(&format!("v{:03}.md", HISTORY_MAX_VERSIONS + 4)));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_history_dir_drops_versions_older_than_the_age_cutoff_even_under_the_count_cap() {
        let dir = history_test_dir("age_cutoff");
        let now = std::time::SystemTime::now();

        let fresh = dir.join("fresh.md");
        std::fs::write(&fresh, "x").unwrap();
        std::fs::File::options().write(true).open(&fresh).unwrap().set_modified(now).unwrap();

        let stale = dir.join("stale.md");
        std::fs::write(&stale, "x").unwrap();
        let stale_mtime = now - std::time::Duration::from_secs(60 * 60 * 24 * (HISTORY_MAX_AGE_DAYS + 1));
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(stale_mtime).unwrap();

        prune_history_dir(&dir);

        let remaining: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(remaining, vec!["fresh.md".to_string()]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn workspace_settings_round_trip_and_missing_returns_none() {
        let dir = history_test_dir("ws_settings");
        assert!(load_workspace_settings(&dir).is_none());

        let settings = WorkspaceSettings {
            font_selection: crate::fonts::FontSelection::BizUdGothic,
            font_size: 16.5,
            show_line_numbers: false,
            word_wrap: false,
            zoom_factor: 1.25,
            preview_visible: true,
            gitignore_prompted: true,
        };
        save_workspace_settings(&dir, &settings).unwrap();

        let loaded = load_workspace_settings(&dir).expect("should load saved settings");
        assert_eq!(loaded, settings);
        assert!(dir.join(".kotoshelf").join("settings.json").is_file());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn multi_keyword_search_requires_all_terms_and_filter_narrows_results() {
        let regexes = build_query_regexes("rust async", false, false).unwrap();
        assert_eq!(regexes.len(), 2);

        let only_rust = "Learning Rust programming\n";
        assert!(scan_matches_multi(only_rust, &regexes).is_empty());

        let both = "Learning Rust programming\nUsing async await in production\n";
        let matches = scan_matches_multi(both, &regexes);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[1].line, 2);

        let results = vec![FileSearchResult {
            path: "notes/dev.md".to_string(),
            matches,
        }];
        let filtered = filter_search_results(&results, "production", false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].matches.len(), 1);
        assert_eq!(filtered[0].matches[0].line, 2);

        let md = format_search_results_markdown("rust async", "production", &filtered, None);
        assert!(md.contains("## [[notes/dev.md]]"));
        assert!(md.contains("- L2: Using async await in production"));
    }

    #[test]
    fn gitignore_detection_and_kotoshelf_initialization_in_git_repo() {
        let dir = history_test_dir("gitignore_kotoshelf");

        // Non-git directory: does not need prompt and does not create .gitignore
        assert!(!needs_kotoshelf_gitignore_prompt(&dir));
        assert!(!ensure_kotoshelf_in_gitignore(&dir).unwrap());
        assert!(!dir.join(".gitignore").exists());

        // Turn into a Git repository (.git folder)
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(needs_kotoshelf_gitignore_prompt(&dir));
        assert!(!is_kotoshelf_gitignored(&dir));

        // Initialize .kotoshelf with add_to_gitignore = true
        std::fs::write(dir.join(".gitignore"), "target/\nnode_modules/").unwrap(); // intentionally no trailing newline
        let updated = initialize_kotoshelf_dir(&dir, true).unwrap();
        assert!(updated);
        assert!(dir.join(".kotoshelf").is_dir());
        assert!(dir.join(".kotoshelf").join("history").is_dir());
        assert!(is_kotoshelf_gitignored(&dir));
        assert!(!needs_kotoshelf_gitignore_prompt(&dir));

        let gitignore_text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(gitignore_text, "target/\nnode_modules/\n.kotoshelf/\n");

        // Calling ensure_kotoshelf_in_gitignore again is idempotent (no duplicate line)
        assert!(!ensure_kotoshelf_in_gitignore(&dir).unwrap());
        let gitignore_text_after = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(gitignore_text_after, gitignore_text);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gitignore_content_ignores_dir_matches_common_patterns() {
        assert!(gitignore_content_ignores_dir(".kotoshelf\n", ".kotoshelf"));
        assert!(gitignore_content_ignores_dir(".kotoshelf/\n", ".kotoshelf"));
        assert!(gitignore_content_ignores_dir("/.kotoshelf/\n", ".kotoshelf"));
        assert!(gitignore_content_ignores_dir("**/.kotoshelf/\n", ".kotoshelf"));
        assert!(gitignore_content_ignores_dir(".kotoshelf/*\n", ".kotoshelf"));
        assert!(!gitignore_content_ignores_dir("# .kotoshelf/\n", ".kotoshelf"));
        assert!(!gitignore_content_ignores_dir("!.kotoshelf/\n", ".kotoshelf"));
        assert!(!gitignore_content_ignores_dir(".kotoshelf_other/\n", ".kotoshelf"));
    }
}
