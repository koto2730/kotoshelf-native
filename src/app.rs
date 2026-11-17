use crate::api;
use crate::attachments;
use crate::dashboard;
use crate::find::FindState;
use crate::fonts::{FontConfig, FontSelection};
use crate::git;
use crate::images;
use crate::ssh;
use crate::tasks;
use crate::templates;
use crate::theme::ThemeSelection;
use crate::syntax::Highlighter;
use crate::tabs::{show_tab_bar, Tab, TabBarAction, TabPath};
use crate::workspace;
use agent_protocol::TreeNode;
use egui::text::{CCursor, CCursorRange};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use std::collections::HashMap;
use egui::{Key, Modifiers};

/// Rejects a user-typed file/folder name that would escape the folder it is
/// created in: empty, absolute (`/x`, `\x`), drive-prefixed (`C:x`), or
/// containing a `..` segment (either separator). Sub-folders (`sub/new.md`)
/// are still allowed.
fn validate_entry_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Name must not be empty.".to_string());
    }
    if n.starts_with('/') || n.starts_with('\\') {
        return Err(format!("Name must be relative to the current folder: {name}"));
    }
    let first = n.split(['/', '\\']).next().unwrap_or("");
    if first.contains(':') {
        return Err(format!("Name must not start with a drive prefix: {name}"));
    }
    if n.split(['/', '\\']).any(|seg| seg == "..") {
        return Err(format!("Name must not contain \"..\": {name}"));
    }
    Ok(())
}

/// Platform-formatted shortcut text; `;` is shown as a symbol rather than "Semicolon".
fn shortcut_text(ui: &egui::Ui, mods: Modifiers, key: Key) -> String {
    let text = ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(mods, key));
    text.replace("Semicolon", ";")
}

/// Menu item showing its shortcut right-aligned (platform-formatted, e.g. "Ctrl+S").
fn sc_button(ui: &egui::Ui, label: &str, mods: Modifiers, key: Key) -> egui::Button<'static> {
    egui::Button::new(label.to_string()).shortcut_text(shortcut_text(ui, mods, key))
}

/// Checkable menu item with a shortcut label; returns true when toggled.
fn check_item(ui: &mut egui::Ui, value: &mut bool, label: &str, mods: Modifiers, key: Key) -> bool {
    let prefix = if *value { "✔ " } else { "     " };
    let text = shortcut_text(ui, mods, key);
    let resp = ui.add(egui::Button::new(format!("{prefix}{label}")).shortcut_text(text));
    if resp.clicked() {
        *value = !*value;
        true
    } else {
        false
    }
}
use std::path::{Path, PathBuf};

pub struct KotoshelfApp {
    /// `Some` = a local workspace is open. Mutually exclusive with
    /// `ssh_profile` - opening one always clears the other, mirroring
    /// the original React app's `workspaceKind: "local" | "ssh"`.
    workspace: Option<PathBuf>,
    ssh_profile: Option<ssh::SshProfile>,
    /// From `AppConfig` - threaded through every ssh.rs call, since a
    /// system can have more than one `ssh` binary (Windows' bundled
    /// OpenSSH vs. Git for Windows' own build).
    ssh_command_path: String,
    /// The persistent tree-read-accelerator agent's live session, if
    /// connected. `agent_read_tree` clears this back to `None` itself on
    /// any connection-level failure, so a dead session is never retried.
    ssh_agent: Option<ssh::AgentSession>,
    /// Set while `open_ssh_workspace`'s background thread (agent
    /// connect + initial tree read - the slow, first-impression part of
    /// opening a remote workspace) is in flight; polled once per frame
    /// via `try_recv`. Connect can take several seconds (deploying the
    /// agent binary on a first-ever connection), and blocking the UI
    /// thread for that long would freeze the whole window with no
    /// spinner or way to tell it apart from a real hang.
    ssh_connecting: Option<std::sync::mpsc::Receiver<SshConnectOutcome>>,
    /// "Open Remote Workspace…" dialog state - `None` when closed.
    remote_dialog: Option<RemoteDialogState>,
    tree: Vec<TreeNode>,
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// Tab IDs selected via Ctrl+Click / Shift+Click in the tab bar for side-by-side tiling.
    selected_tab_ids: Vec<u64>,
    /// Tab IDs currently arranged side-by-side in the editor area (2 or more = split view).
    split_tab_ids: Vec<u64>,
    status: String,
    highlighter: Highlighter,
    /// Set while a "close this unsaved tab?" confirmation is up - the
    /// tab isn't actually removed until the user answers, since egui has
    /// no blocking `window.confirm()`-equivalent to call synchronously.
    pending_close: Option<usize>,
    /// Same idea as `pending_close`, for "Close All Tabs".
    pending_close_all: bool,
    /// Set when the user tried to quit (native window close button,
    /// Ctrl+Q/Cmd+Q) while a tab had unsaved changes - the OS-level close
    /// request gets cancelled via `ViewportCommand::CancelClose` and this
    /// dialog stands in for it. `quit_confirmed` then lets the *next*
    /// close request (re-sent once the user picks "Quit without saving")
    /// through without re-triggering this same dialog.
    pending_quit: bool,
    quit_confirmed: bool,
    /// `egui_commonmark`'s own cache (fonts/images/state reused across
    /// frames) - owned once here rather than rebuilt per frame, per its
    /// documented usage pattern.
    preview_cache: CommonMarkCache,
    preview_visible: bool,
    find: FindState,
    find_focus_pending: bool,
    /// Set whenever `active` changes to a tab the user just opened or
    /// switched to - consumed by the central panel to call
    /// `request_focus()` on that tab's editor. Without this, a brand new
    /// blank/frameless "page" (see the editor's `.frame(Frame::NONE)`)
    /// gives no visual or keyboard cue that it's focused, so typing
    /// silently goes nowhere until the user thinks to click into it.
    focus_pending: bool,
    /// Source of each `Tab::id` - kept monotonically increasing and
    /// never reused, so a closed tab's editor id can never collide with
    /// a tab opened afterward (which would otherwise leak stale cursor
    /// position/undo history into unrelated content - the same reason
    /// the original React version keys its CodeMirror instance by a
    /// stable per-tab id, not a positional index).
    next_tab_id: u64,
    /// "New File…"/"New Folder…"/"Rename…" all need one text field from
    /// the user before they can do anything - modeled as data (not a
    /// `window.prompt()`-style blocking call, which egui doesn't have)
    /// so it can render across frames like `pending_close` above.
    name_prompt: Option<NamePrompt>,
    /// Set for exactly the one frame after `name_prompt` is (re)opened -
    /// consumed by the modal to call `request_focus()` once. Calling
    /// `request_focus()` on every frame (as the editor's `focus_pending`
    /// does, harmlessly, since it's only set once per activation too)
    /// turned out to actively break IME composition here: re-affirming
    /// focus on an already-focused single-line field each frame resets
    /// the in-progress conversion before it can be committed, so
    /// multi-byte (Japanese) input never lands - discovered when the
    /// user could rename/delete/create fine but not type Japanese names.
    name_prompt_focus_pending: bool,
    /// Set while a "move to trash?" confirmation is up, same idea as
    /// `pending_close`. Carries the target's display name alongside its
    /// path purely so the confirmation label doesn't need a second
    /// lookup once the path may already be gone from `tree`.
    confirm_trash: Option<(TabPath, String)>,
    /// Set when a `[[target]]` wiki-link matched more than one file in
    /// the workspace - one row per candidate, picking closes the modal
    /// and opens that file. `None` when closed.
    wiki_choices: Option<Vec<TabPath>>,
    /// Loaded from `~/.kotoshelf/config.json` at startup, refreshed
    /// after Settings is saved - kept as plain fields (not re-read from
    /// disk on every SendPalette open) so the palette and menu-enabled
    /// state don't need a fallible disk read each frame.
    presets: Vec<api::ApiPreset>,
    tokens: HashMap<String, String>,
    /// `Some((display_name, raw_path))` list while the "Insert
    /// Template" picker is open - `raw_path` is exactly what
    /// `list_all_paths` produced (a local absolute path or an SSH
    /// workspace-relative path), ready to build a `TabPath` from
    /// directly. `None` when closed.
    template_picker: Option<Vec<(String, String)>>,
    /// `Some(highlighted index)` while the SendPalette is open.
    send_palette: Option<SendPaletteState>,
    last_used_preset: Option<usize>,
    settings_dialog: Option<SettingsDialogState>,
    /// The in-flight HTTP request, if any - `send_with_preset` spawns it
    /// on a background thread (an API can be arbitrarily slow, and
    /// `reqwest::blocking` would otherwise freeze the whole window)
    /// exactly like `open_ssh_workspace`'s connect step does.
    send_pending: Option<std::sync::mpsc::Receiver<SendOutcome>>,
    /// Which of the two tabs the left sidebar shows - mirrors the
    /// original React app's `leftPane: "files" | "search"` toggle above
    /// the tree.
    left_pane: LeftPane,
    workspace_search: WorkspaceSearchState,
    /// `(tab_id, start, end)` char range to select once that tab's editor
    /// has rendered at least once - set by `open_search_match` for a
    /// workspace-search result whose file wasn't already open in a tab.
    /// Injecting the selection immediately (before a brand new
    /// `editor_id` has ever been shown) turned out to be silently
    /// dropped - confirmed live: the cursor landed at the match but
    /// nothing was selected. Deferring one frame (apply right after the
    /// editor's first-ever `.show(ui)`, same trick `continue_list_on_enter`
    /// already relies on) fixes it.
    pending_search_selection: Option<(u64, usize, usize)>,
    /// Bytes fetched for an SSH-workspace preview image, keyed by the
    /// synthetic `kotoshelf-ssh://<relpath>` URI `render_preview_markdown`
    /// rewrites that reference to. `None` = fetch failed/too large (kept
    /// as a negative-cache entry so a broken image reference doesn't
    /// re-fire an `ssh` round trip on every single frame the preview is
    /// visible). A local workspace needs no equivalent cache - the
    /// `file://` URI scheme is served by egui's own built-in file loader,
    /// which does its own caching.
    ssh_image_cache: HashMap<String, Option<Vec<u8>>>,
    /// The last title actually sent via `ViewportCommand::Title` - lets
    /// `ui()` skip re-sending an unchanged title every single frame
    /// (`desired_window_title()` is cheap, but there's no reason to poke
    /// winit every frame for a value that only changes on workspace
    /// open/close).
    last_window_title: String,
    /// Loaded from `~/.kotoshelf/theme.json` at startup, saved back
    /// whenever the user picks something in the Theme dialog - see
    /// `theme.rs`. Resolved to an actual light/dark bool fresh every
    /// frame (`theme::resolve_dark`, cheap), not cached, so a `System`
    /// selection picks up an OS-level theme flip live.
    theme_selection: ThemeSelection,
    /// `Some` while the Theme dialog is open.
    theme_dialog_open: bool,
    font_config: FontConfig,
    font_dialog_open: bool,
    /// `Some` while the task-metadata input-assist popup (Space on a
    /// `- [ ]` line) is open.
    task_assist: Option<TaskAssistState>,
    selected_tree_nodes: std::collections::HashSet<String>,
    /// `true` only for the frame after `reveal_path_in_tree` ran (i.e.
    /// wikilink navigation) - the tree scrolls the selected node into
    /// view once, then leaves the user free to scroll the tree pane on
    /// their own without it snapping back every frame a selected node
    /// is merely still selected (e.g. from switching tabs normally).
    tree_scroll_pending: bool,
    image_viewer: Option<ImageViewerState>,
    pdf_render_cache: HashMap<String, Option<Vec<u8>>>,
    video_render_cache: HashMap<String, Option<Vec<u8>>>,
    /// In-memory cache for workspace tasks: `source_path -> (mtime, tasks)`.
    /// Minimizes disk I/O and parsing overhead for incremental task scans.
    task_cache: HashMap<String, (std::time::SystemTime, Vec<dashboard::DueTask>)>,
    /// Whether the `git` CLI is available on the host OS `PATH` (checked once at startup).
    git_available: bool,
    /// Whether `git-lfs` is available on the host OS `PATH` (checked once at startup).
    git_lfs_available: bool,
    /// Whether the currently open local workspace has a `.git` directory/file.
    has_git_repo: bool,
    /// State for the "Git Commit" modal dialog when open.
    git_commit_dialog: Option<GitCommitDialogState>,
    /// Set for one frame when the Git Commit dialog opens so the message field gets focus without breaking IME.
    git_commit_focus_pending: bool,
    #[cfg(windows)]
    ime_keeper: crate::ime_win::ImeKeeper,
    /// Remembers the user's last "Push to remote" checkbox state across commits within the session.
    git_push_default: bool,
    /// Receiver for an in-flight background `git add -A && git commit (&& git push)` task.
    git_pending: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// Last observed `egui::Context::zoom_factor()` so changes via keyboard shortcuts (`Ctrl+Plus`/`Minus`/`0`) or menu can be persisted per workspace.
    last_zoom_factor: f32,
    /// Deferred zoom factor loaded from `.kotoshelf/settings.json` on workspace open, applied on the next `ui()` frame.
    pending_zoom_factor: Option<f32>,
    /// Deferred font apply flag when `.kotoshelf/settings.json` updates `self.font_config` outside a `ui()` call.
    pending_font_apply: bool,
    /// In-memory O(1) index of existing WikiLink target names/paths derived from `self.tree`.
    wiki_target_set: std::collections::HashSet<String>,
    /// `true` once `self.tree` has been loaded and indexed into `self.wiki_target_set`.
    wiki_target_set_ready: bool,
    /// User confirmation status for creating `.kotoshelf/` and updating `.gitignore` in the current local workspace.
    kotoshelf_dir_policy: KotoshelfDirPolicy,
    /// Modal dialog state asking the user before creating `.kotoshelf/` and adding it to `.gitignore` in a Git workspace.
    gitignore_confirm_dialog: Option<GitignoreConfirmDialogState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KotoshelfDirPolicy {
    /// Not yet decided for this workspace; if a Git repo needs confirmation, prompts before creating `.kotoshelf`.
    Undecided,
    /// User approved creating `.kotoshelf` and adding `.kotoshelf/` to `.gitignore` (or it was already ignored).
    AllowedWithGitignore,
    /// User approved creating `.kotoshelf` without modifying `.gitignore`.
    AllowedWithoutGitignore,
    /// User declined creating `.kotoshelf` for the current workspace session.
    Denied,
}

#[derive(Clone, Debug)]
pub(crate) struct GitignoreConfirmDialogState {
    pub kotoshelf_exists: bool,
    pub already_ignored: bool,
}

#[derive(Clone)]
struct GitCommitDialogState {
    message: String,
    push: bool,
    uses_lfs: bool,
    push_target: Option<String>,
}

struct TaskAssistState {
    /// Flat index into `groups` (group headers don't count), for
    /// arrow-key navigation.
    highlight: usize,
    /// `true` only for the frame the popup was opened on. The Space
    /// keypress that triggers it is, unlike e.g. the Send Palette's
    /// digit keys, *supposed* to also land in the document as an
    /// ordinary character (matching Obsidian's own behavior: typing a
    /// space both inserts it and opens the assist). That keypress is
    /// still pending in the editor's input queue this same frame, so
    /// stripping focus from it immediately (as the Send Palette does)
    /// silently drops it - confirmed live: the space between the task
    /// text and the inserted marker was missing every time. Skipping the
    /// focus-surrender for exactly one frame lets the space land
    /// normally before the popup starts intercepting arrow/enter keys
    /// for its own navigation.
    just_opened: bool,
    /// `(group_label, [(item_label, insert_text)])`, resolved once when
    /// the popup opens rather than re-derived every frame - relative
    /// dates ("Tomorrow") are computed against *today* at that moment,
    /// a fresh Task ID is generated once (not regenerated every repaint
    /// while the popup happens to be open), and the "Depends on" group
    /// (if any) is built by scanning the active tab's *current* content
    /// for existing 🆔 markers.
    groups: Vec<(String, Vec<(String, String)>)>,
}

/// Every Obsidian Tasks emoji marker this app recognizes, offered in the
/// popup per the user's explicit "put all of them in there" request -
/// includes markers `tasks.rs` doesn't otherwise parse/act on
/// (➕/❌/🆔/⛔) since they're still valid, useful metadata to attach even
/// though this app's own recurrence/completion engine doesn't read them.
/// The only thing deliberately left out is a full calendar-picker for
/// arbitrary dates - egui has no built-in date-picker widget, and typing
/// an exact date by hand (`📅 2026-03-10`) already works fine without
/// this popup; "Today"/"Tomorrow"/"Next week" cover the common case.
fn build_task_assist_groups(content: &str) -> Vec<(String, Vec<(String, String)>)> {
    let today = chrono::Local::now().date_naive();
    let date_items = |marker: char| -> Vec<(String, String)> {
        vec![
            ("Today".to_string(), format!("{marker} {}", today.format("%Y-%m-%d"))),
            ("Tomorrow".to_string(), format!("{marker} {}", (today + chrono::Duration::days(1)).format("%Y-%m-%d"))),
            ("Next week".to_string(), format!("{marker} {}", (today + chrono::Duration::days(7)).format("%Y-%m-%d"))),
        ]
    };
    let s = |label: &str, insert: &str| (label.to_string(), insert.to_string());

    let mut groups = vec![
        ("Priority".to_string(), vec![s("🔺 Highest", "🔺"), s("⏫ High", "⏫"), s("🔼 Medium", "🔼"), s("🔽 Low", "🔽"), s("⏬ Lowest", "⏬")]),
        ("Due date (the deadline)".to_string(), date_items(tasks::DUE)),
        ("Scheduled date (when you plan to work on it)".to_string(), date_items(tasks::SCHEDULED)),
        ("Start date (earliest this can begin)".to_string(), date_items(tasks::START)),
        (
            "Created date (when this task was added)".to_string(),
            vec![("Today".to_string(), format!("{} {}", tasks::CREATED, today.format("%Y-%m-%d")))],
        ),
        (
            "Cancelled date (when this task was called off)".to_string(),
            vec![("Today".to_string(), format!("{} {}", tasks::CANCELLED, today.format("%Y-%m-%d")))],
        ),
        (
            "Done date (normally added automatically on check)".to_string(),
            vec![("Today".to_string(), format!("{} {}", tasks::DONE, today.format("%Y-%m-%d")))],
        ),
        (
            "Recurring (repeats when checked off)".to_string(),
            vec![
                s("Every day", "🔁 every day"),
                s("Every weekday", "🔁 every weekday"),
                s("Every week", "🔁 every week"),
                s("Every week on Monday", "🔁 every week on monday"),
                s("Every week on Tuesday", "🔁 every week on tuesday"),
                s("Every week on Wednesday", "🔁 every week on wednesday"),
                s("Every week on Thursday", "🔁 every week on thursday"),
                s("Every week on Friday", "🔁 every week on friday"),
                s("Every week on Saturday", "🔁 every week on saturday"),
                s("Every week on Sunday", "🔁 every week on sunday"),
                s("Every month", "🔁 every month"),
                s("Every month on the 1st", "🔁 every month on the 1st"),
                s("Every month on the 15th", "🔁 every month on the 15th"),
                s("Every month on the last day", "🔁 every month on the last day"),
                s("Every year", "🔁 every year"),
            ],
        ),
        (
            "On completion (what happens when checked off)".to_string(),
            vec![s("Delete (remove the line entirely)", "🏁 delete"), s("Keep (leave it checked, the default)", "🏁 keep")],
        ),
        (
            "Task ID (lets other tasks depend on this one)".to_string(),
            vec![("Generate new ID".to_string(), format!("{} {}", tasks::ID, generate_task_id()))],
        ),
    ];

    let existing_ids = task_ids_in(content);
    if !existing_ids.is_empty() {
        groups.push((
            "Depends on (blocks until the referenced task is done)".to_string(),
            existing_ids.into_iter().map(|id| (id.clone(), format!("{} {id}", tasks::DEPENDS_ON))).collect(),
        ));
    }
    groups
}

/// Every distinct `🆔 <id>` already used in `content`, in first-seen
/// order - the source list for the popup's dynamic "Depends on" group.
fn task_ids_in(content: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in content.lines() {
        if let Some(idx) = line.find(tasks::ID) {
            let rest = line[idx + tasks::ID.len_utf8()..].trim_start();
            let id: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            if !id.is_empty() && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// A short, human-typeable task ID (6 base-36 characters) - not
/// cryptographically unique, just distinct enough that two IDs
/// generated for the same document collide only by extreme coincidence,
/// matching how casually Obsidian Tasks' own auto-generated IDs are
/// used. No new dependency: hashes the current time via `DefaultHasher`
/// rather than pulling in a `rand` crate for something this low-stakes.
fn generate_task_id() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    let mut n = hasher.finish();
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut chars = [b'0'; 6];
    for slot in chars.iter_mut().rev() {
        *slot = DIGITS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(chars.to_vec()).unwrap()
}

fn is_pdf_path(path: &TabPath) -> bool {
    let name = path.file_name().to_lowercase();
    name.ends_with(".pdf")
}

fn is_video_path_tab(path: &TabPath) -> bool {
    let name = path.file_name();
    crate::video::is_video_path(Path::new(&name))
}

fn open_in_external_app(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &path.to_string_lossy()])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `true` for a line that looks like a markdown task checkbox
/// (`- [ ]`/`* [ ]`/`+ [ ]`, any leading whitespace, either checked or
/// unchecked) - gates the input-assist popup so it doesn't pop up on
/// every Space press in ordinary prose.
fn is_task_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    for prefix in ["- [", "* [", "+ ["] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.len() >= 2 && rest.as_bytes()[1] == b']';
        }
    }
    false
}

/// `true` once `line` has some title text after the checkbox marker
/// (possibly followed by already-inserted metadata) - `- [ ] ` alone
/// (nothing typed yet) doesn't count, so the very first Space after the
/// checkbox itself doesn't immediately pop the assist up before there's
/// anything to attach metadata to.
fn has_task_title(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.find(']').map(|end| trimmed[end + 1..].trim().is_empty()).map(|empty| !empty).unwrap_or(false)
}

/// The full text of whichever line the cursor is currently on, or `None`
/// if the editor has no recorded cursor yet.
fn current_line_at_cursor(ctx: &egui::Context, id: egui::Id, content: &str) -> Option<String> {
    let pos = egui::TextEdit::load_state(ctx, id)?.cursor.char_range()?.primary.index.0;
    let starts = workspace::line_starts(content);
    let idx = starts.iter().rposition(|&s| s <= pos)?.min(content.lines().count().saturating_sub(1));
    content.lines().nth(idx).map(String::from)
}

#[derive(PartialEq)]
enum LeftPane {
    Files,
    Search,
}

/// Workspace-wide (all files, not just the open buffer) search/replace -
/// the sidebar counterpart to `find.rs`'s in-buffer Find/Replace, sharing
/// its regex/case-sensitivity semantics but operating over every file in
/// the workspace via `workspace::search_workspace`/`ssh::search_workspace`.
#[derive(Default)]
struct WorkspaceSearchState {
    query: String,
    filter_query: String,
    replacement: String,
    is_regex: bool,
    case_sensitive: bool,
    include_ignored: bool,
    results: Vec<workspace::FileSearchResult>,
    status: String,
}

#[derive(Clone, Debug)]
struct TokenEntry {
    key: String,
    value: String,
    masked: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsTab {
    Presets,
    Tokens,
}

struct SettingsDialogState {
    presets: Vec<api::ApiPreset>,
    selected_preset: usize,
    tokens: Vec<TokenEntry>,
    active_tab: SettingsTab,
    new_token_key: String,
    new_token_value: String,
    validation_error: Option<String>,
}

fn validate_json_template(preset_name: &str, template: &str, tokens: &[TokenEntry]) -> Result<(), String> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    // If it doesn't look like JSON (starts with { or [), skip JSON validation
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return Ok(());
    }

    let mut token_map: HashMap<String, String> = tokens
        .iter()
        .map(|t| (t.key.clone(), if t.value.is_empty() { "dummy_token".to_string() } else { t.value.clone() }))
        .collect();
    token_map.entry("API_KEY".to_string()).or_insert_with(|| "dummy_key".to_string());
    token_map.entry("TOKEN".to_string()).or_insert_with(|| "dummy_token".to_string());

    let ctx = api::RenderContext {
        selection: "dummy test selection",
        filename: "test_file",
        tokens: &token_map,
        session_id: Some("test-session-id"),
    };

    let rendered = api::render_template(template, &ctx);
    let re_unresolved = regex::Regex::new(r#"\{\{\s*[a-zA-Z0-9_.]+\s*\}\}"#).unwrap();
    let sanitized = re_unresolved.replace_all(&rendered, "dummy_val").into_owned();

    if serde_json::from_str::<serde_json::Value>(&sanitized).is_ok() {
        return Ok(());
    }

    let sanitized_quoted = re_unresolved.replace_all(&rendered, "\"dummy_val\"").into_owned();
    if serde_json::from_str::<serde_json::Value>(&sanitized_quoted).is_ok() {
        return Ok(());
    }

    // Try wrapping bare unquoted text values if user omitted quotes around {{selection}}
    let re_bare = regex::Regex::new(r#":\s*([a-zA-Z0-9_ -]+)"#).unwrap();
    let sanitized_bare = re_bare.replace_all(&sanitized, r#": "$1""#).into_owned();
    if serde_json::from_str::<serde_json::Value>(&sanitized_bare).is_ok() {
        return Ok(());
    }

    match serde_json::from_str::<serde_json::Value>(&sanitized) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("JSON syntax error in preset '{preset_name}': {e}")),
    }
}

#[derive(Clone)]
struct SendPaletteState {
    selected: usize,
    sel_start: usize,
    sel_end: usize,
    cursor_pos: usize,
}

/// Carries everything `apply_send_outcome` needs that isn't already on
/// `self` by the time the background thread's result arrives - the tab
/// might have been closed, the active tab might have changed, so the
/// insertion target has to travel with the outcome rather than being
/// re-read from `self` at completion time.
struct SendOutcome {
    response_target: String,
    response_json_path: Option<String>,
    session_id_path: Option<String>,
    session_updated_path: Option<String>,
    /// Which tab (by stable id, not index) an `"afterSelection"` result
    /// should be inserted into.
    target_tab_id: u64,
    /// Char offset (into that tab's content *at send time*) to insert
    /// after - the selection's end, or just the cursor position when
    /// there was no selection (matches the original: even when an empty
    /// selection falls back to sending the *whole document*, insertion
    /// still happens at the cursor, not at the end of the document).
    insert_at_char: usize,
    result: Result<api::SendRequestOutput, String>,
}

#[derive(Clone)]
struct NamePrompt {
    title: String,
    value: String,
    action: NamePromptAction,
}

#[derive(Clone)]
enum NamePromptAction {
    NewFile { dir: TabPath },
    NewFolder { dir: TabPath },
    Rename { old_path: TabPath },
    Duplicate { src_path: TabPath },
    Move { src_path: TabPath },
    MoveBulk { src_paths: Vec<TabPath> },
    SaveAs { tab_index: usize },
}

enum TreeAction {
    Open(TabPath),
    ExpandDir { rel_path: String },
    NewFile { dir: TabPath, label: String },
    NewFolder { dir: TabPath, label: String },
    Rename { path: TabPath, name: String },
    Duplicate { path: TabPath, name: String },
    Move { path: TabPath, name: String },
    Delete { path: TabPath, name: String },
    MoveBulk { paths: Vec<TabPath>, name: String },
    DeleteBulk { paths: Vec<TabPath> },
    OpenTerminal { dir: TabPath },
    OpenFileManager { path: TabPath },
}

struct ImageViewerState {
    images: Vec<(String, Vec<u8>)>,
    current_index: usize,
    zoom: f32,
}

/// Result of `open_ssh_workspace`'s background connect+tree-read thread.
struct SshConnectOutcome {
    agent: Option<ssh::AgentSession>,
    tree: Result<Vec<TreeNode>, String>,
    agent_connected: bool,
}

/// "Open Remote Workspace…" dialog state. Profile CRUD is entirely
/// local array edits (mirroring the original's `RemoteWorkspaceDialog`,
/// which does the same against a copy of `AppConfig.sshProfiles`) -
/// nothing is persisted to `~/.kotoshelf/config.json` until Save or Save & Connect.
struct RemoteDialogState {
    profiles: Vec<ssh::SshProfile>,
    selected: usize,
    ssh_command_path: String,
    /// Measured height of the right pane so the left pane (list + Add/Remove
    /// buttons) matches the right pane's vertical height.
    right_pane_height: f32,
    /// In-flight `ssh -vT` test receiver when the user clicks "Test".
    test_pending: Option<std::sync::mpsc::Receiver<ssh::SshTestOutput>>,
    /// Most recent `ssh -vT` test output for the currently selected profile.
    test_result: Option<ssh::SshTestOutput>,
    /// Set to `true` after clicking Copy on the test output.
    test_copied: bool,
}

fn collect_tree_file_paths(nodes: &[TreeNode], out: &mut Vec<String>) {
    for node in nodes {
        if node.is_dir {
            if let Some(children) = &node.children {
                collect_tree_file_paths(children, out);
            }
        } else {
            out.push(node.path.clone());
        }
    }
}

fn ssh_display_target(profile: &ssh::SshProfile) -> String {
    let user = profile.user.as_deref().map(|u| format!("{u}@")).unwrap_or_default();
    // "." is the normalized "login home" root (see ssh::normalize_remote_folder).
    let folder = if profile.remote_path == "." { "~" } else { profile.remote_path.as_str() };
    format!("{user}{}:{folder}", profile.host)
}

fn generate_duplicate_name(name: &str) -> String {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        format!("{stem} copy.{ext}")
    } else {
        format!("{name} copy")
    }
}

/// Each tab's editor `TextEdit` uses this id, keyed by the tab's own
/// stable identity (not its position in `tabs`, which shifts as other
/// tabs close). `find`'s cursor-jump logic loads/stores `TextEditState`
/// under this exact id independently of the widget itself, so both
/// sides must agree on it without needing to pass a `Response` around.
fn editor_id(tab_id: u64) -> egui::Id {
    egui::Id::new(("kotoshelf_editor", tab_id))
}

impl KotoshelfApp {
    pub fn new() -> Self {
        let config = ssh::load_app_config();
        let (tokens, need_config_save) = crate::secure_tokens::sync_tokens_on_load(config.tokens.clone());
        if need_config_save {
            let updated_config = ssh::AppConfig {
                tokens: config.tokens.iter().map(|(k, _)| (k.clone(), String::new())).collect(),
                ..config.clone()
            };
            let _ = ssh::save_app_config(&updated_config);
        }
        let theme_selection = crate::theme::load_selected();
        let font_config = crate::fonts::load_config();
        Self {
            workspace: None,
            ssh_profile: None,
            ssh_command_path: config.ssh_command_path,
            ssh_agent: None,
            ssh_connecting: None,
            remote_dialog: None,
            tree: Vec::new(),
            tabs: Vec::new(),
            active: None,
            selected_tab_ids: Vec::new(),
            split_tab_ids: Vec::new(),
            status: String::new(),
            highlighter: Highlighter::new(crate::theme::resolve_dark(theme_selection)),
            pending_close: None,
            pending_close_all: false,
            pending_quit: false,
            quit_confirmed: false,
            preview_cache: CommonMarkCache::default(),
            preview_visible: false,
            find: FindState::default(),
            focus_pending: false,
            next_tab_id: 0,
            name_prompt: None,
            name_prompt_focus_pending: false,
            confirm_trash: None,
            wiki_choices: None,
            presets: config.presets,
            tokens,
            template_picker: None,
            send_palette: None,
            last_used_preset: None,
            settings_dialog: None,
            send_pending: None,
            left_pane: LeftPane::Files,
            workspace_search: WorkspaceSearchState::default(),
            pending_search_selection: None,
            ssh_image_cache: HashMap::new(),
            last_window_title: String::new(),
            theme_selection,
            theme_dialog_open: false,
            font_config,
            font_dialog_open: false,
            task_assist: None,
            selected_tree_nodes: std::collections::HashSet::new(),
            tree_scroll_pending: false,
            image_viewer: None,
            find_focus_pending: false,
            pdf_render_cache: HashMap::new(),
            video_render_cache: HashMap::new(),
            task_cache: HashMap::new(),
            git_available: git::is_git_available(),
            git_lfs_available: git::is_git_lfs_available(),
            has_git_repo: false,
            git_commit_dialog: None,
            git_commit_focus_pending: false,
            #[cfg(windows)]
            ime_keeper: Default::default(),
            git_push_default: false,
            git_pending: None,
            last_zoom_factor: 1.0,
            pending_zoom_factor: None,
            pending_font_apply: false,
            wiki_target_set: std::collections::HashSet::new(),
            wiki_target_set_ready: false,
            kotoshelf_dir_policy: KotoshelfDirPolicy::Undecided,
            gitignore_confirm_dialog: None,
        }
    }

    /// `"kotoshelf"` alone with nothing open; `"kotoshelf - <path>"` for a
    /// local workspace. For an SSH workspace, deliberately the saved
    /// profile *name* rather than `ssh_display_target`'s `user@host:path`
    /// - the window title is far more exposed at a glance (screen shares,
    /// screenshots, the taskbar) than the in-app status bar, so it
    /// shouldn't leak a remote host/path merely by having the app open.
    fn desired_window_title(&self) -> String {
        if let Some(profile) = &self.ssh_profile {
            format!("kotoshelf - {}", profile.name)
        } else if let Some(dir) = &self.workspace {
            format!("kotoshelf - {}", display_path(dir))
        } else {
            "kotoshelf".to_string()
        }
    }

    fn alloc_tab_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    /// The currently open workspace's root as a `TabPath`, plus a
    /// display label for "New file/folder in <label>/" prompt titles -
    /// `None` when neither a local folder nor an SSH profile is open.
    fn workspace_root(&self) -> Option<(TabPath, String)> {
        if let Some(profile) = &self.ssh_profile {
            let label = profile
                .remote_path
                .trim_end_matches('/')
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(&profile.host)
                .to_string();
            Some((TabPath::Ssh(String::new()), label))
        } else {
            self.workspace.as_ref().map(|dir| (TabPath::Local(dir.clone()), workspace_label(dir)))
        }
    }

    fn open_folder(&mut self) {
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        self.open_workspace_at(dir);
    }

    /// Opens `dir` as the local workspace without the native folder
    /// picker - shared by `open_folder` and CLI-arg startup
    /// (`kotoshelf <path>` / `kotoshelf notes.md`).
    pub(crate) fn open_workspace_at(&mut self, dir: PathBuf) {
        ssh::agent_disconnect(&mut self.ssh_agent);
        self.ssh_profile = None;
        self.ssh_connecting = None;
        self.ssh_image_cache.clear();
        self.tabs.clear();
        self.active = None;
        self.sync_tab_id_lists();
        match agent_protocol::read_tree(&dir.to_string_lossy()) {
            Ok(tree) => {
                self.tree = agent_protocol::filter_tree(tree);
                self.rebuild_wiki_target_set();
                if !self.git_available {
                    self.git_available = git::is_git_available();
                    self.git_lfs_available = git::is_git_lfs_available();
                }
                self.has_git_repo = git::is_git_repo(&dir);
                self.workspace = Some(dir.clone());
                if self.has_git_repo {
                    if workspace::needs_kotoshelf_gitignore_prompt(&dir) {
                        self.kotoshelf_dir_policy = KotoshelfDirPolicy::Undecided;
                        self.gitignore_confirm_dialog = Some(GitignoreConfirmDialogState {
                            kotoshelf_exists: dir.join(workspace::APP_WORKSPACE_DIR).exists(),
                            already_ignored: workspace::is_kotoshelf_gitignored(&dir),
                        });
                    } else if workspace::is_kotoshelf_gitignored(&dir) {
                        self.kotoshelf_dir_policy = KotoshelfDirPolicy::AllowedWithGitignore;
                        self.gitignore_confirm_dialog = None;
                    } else {
                        self.kotoshelf_dir_policy = KotoshelfDirPolicy::AllowedWithoutGitignore;
                        self.gitignore_confirm_dialog = None;
                    }
                } else {
                    self.kotoshelf_dir_policy = KotoshelfDirPolicy::Undecided;
                    self.gitignore_confirm_dialog = None;
                }
                if let Some(ws_settings) = workspace::load_workspace_settings(&dir) {
                    self.font_config = FontConfig {
                        selection: ws_settings.font_selection,
                        size: ws_settings.font_size,
                        show_line_numbers: ws_settings.show_line_numbers,
                        word_wrap: ws_settings.word_wrap,
                    };
                    self.preview_visible = ws_settings.preview_visible;
                    let z = ws_settings.zoom_factor.clamp(0.2, 5.0);
                    self.pending_zoom_factor = Some(z);
                    self.last_zoom_factor = z;
                    self.pending_font_apply = true;
                } else {
                    self.font_config = crate::fonts::load_config();
                    self.pending_font_apply = true;
                }
                self.status = format!("Workspace: {}", dir.display());
                self.ensure_and_open_homepage();
            }
            Err(e) => self.status = format!("Failed to read workspace: {e}"),
        }
    }

    /// Checks whether the app is allowed to create/write `.kotoshelf/` in the current local workspace.
    /// If the workspace is a Git repository and `.kotoshelf/` is about to be created (or is not yet
    /// in `.gitignore`) without prior user confirmation in this session, opens the confirmation dialog
    /// and returns `false` until the user responds.
    pub(crate) fn ensure_kotoshelf_dir_ready(&mut self) -> bool {
        let Some(dir) = self.workspace.clone() else {
            return false;
        };
        match self.kotoshelf_dir_policy {
            KotoshelfDirPolicy::Denied => false,
            KotoshelfDirPolicy::AllowedWithGitignore | KotoshelfDirPolicy::AllowedWithoutGitignore => true,
            KotoshelfDirPolicy::Undecided => {
                if workspace::needs_kotoshelf_gitignore_prompt(&dir) {
                    if self.gitignore_confirm_dialog.is_none() {
                        self.gitignore_confirm_dialog = Some(GitignoreConfirmDialogState {
                            kotoshelf_exists: dir.join(workspace::APP_WORKSPACE_DIR).exists(),
                            already_ignored: workspace::is_kotoshelf_gitignored(&dir),
                        });
                    }
                    false
                } else {
                    if git::is_git_repo(&dir) {
                        self.kotoshelf_dir_policy = if workspace::is_kotoshelf_gitignored(&dir) {
                            KotoshelfDirPolicy::AllowedWithGitignore
                        } else {
                            KotoshelfDirPolicy::AllowedWithoutGitignore
                        };
                    }
                    true
                }
            }
        }
    }

    /// Applies the user's decision from the `.kotoshelf` / `.gitignore` confirmation dialog.
    pub(crate) fn apply_gitignore_confirm_decision(&mut self, decision: KotoshelfDirPolicy, zoom_factor: f32) {
        self.gitignore_confirm_dialog = None;
        self.kotoshelf_dir_policy = decision;
        let Some(dir) = self.workspace.clone() else {
            return;
        };
        match decision {
            KotoshelfDirPolicy::AllowedWithGitignore => {
                match workspace::initialize_kotoshelf_dir(&dir, true) {
                    Ok(updated_gitignore) => {
                        self.save_current_workspace_settings(zoom_factor);
                        self.refresh_tree();
                        self.status = if updated_gitignore {
                            "Created .kotoshelf/ and added .kotoshelf/ to .gitignore.".to_string()
                        } else {
                            "Created .kotoshelf/ (.gitignore already includes .kotoshelf/).".to_string()
                        };
                    }
                    Err(e) => {
                        self.status = format!("Failed to initialize .kotoshelf/: {e}");
                    }
                }
            }
            KotoshelfDirPolicy::AllowedWithoutGitignore => {
                match workspace::initialize_kotoshelf_dir(&dir, false) {
                    Ok(_) => {
                        self.save_current_workspace_settings(zoom_factor);
                        self.status = "Created .kotoshelf/ (without modifying .gitignore).".to_string();
                    }
                    Err(e) => {
                        self.status = format!("Failed to initialize .kotoshelf/: {e}");
                    }
                }
            }
            KotoshelfDirPolicy::Denied | KotoshelfDirPolicy::Undecided => {
                self.status = "Skipped creating .kotoshelf/ for this workspace.".to_string();
            }
        }
    }

    fn save_current_workspace_settings(&mut self, zoom_factor: f32) {
        if !self.ensure_kotoshelf_dir_ready() {
            return;
        }
        let Some(dir) = &self.workspace else {
            return;
        };
        let gitignore_prompted = matches!(
            self.kotoshelf_dir_policy,
            KotoshelfDirPolicy::AllowedWithGitignore | KotoshelfDirPolicy::AllowedWithoutGitignore
        ) || workspace::load_workspace_settings(dir)
            .map(|s| s.gitignore_prompted)
            .unwrap_or(false);
        let settings = workspace::WorkspaceSettings {
            font_selection: self.font_config.selection.clone(),
            font_size: self.font_config.clamped_size(),
            show_line_numbers: self.font_config.show_line_numbers,
            word_wrap: self.font_config.word_wrap,
            zoom_factor,
            preview_visible: self.preview_visible,
            gitignore_prompted,
        };
        let _ = workspace::save_workspace_settings(dir, &settings);
    }

    fn open_remote_dialog(&mut self) {
        let config = ssh::load_app_config();
        let selected = self
            .ssh_profile
            .as_ref()
            .and_then(|active| {
                config
                    .ssh_profiles
                    .iter()
                    .position(|p| p.name == active.name && p.host == active.host)
            })
            .unwrap_or(0);
        self.remote_dialog = Some(RemoteDialogState {
            selected,
            profiles: config.ssh_profiles,
            ssh_command_path: config.ssh_command_path,
            right_pane_height: 220.0,
            test_pending: None,
            test_result: None,
            test_copied: false,
        });
    }

    /// Loads one directory level (`rel_path`) over SSH and splices its
    /// children into `self.tree` (ported from Tauri's `handleToggleDir`).
    fn load_ssh_dir_shallow(&mut self, rel_path: &str) {
        let Some(profile) = self.ssh_profile.clone() else { return };
        match ssh::read_tree_shallow(&mut self.ssh_agent, &self.ssh_command_path, &profile, rel_path) {
            Ok(children) => {
                ssh::insert_tree_children(&mut self.tree, rel_path, children);
                self.rebuild_wiki_target_set();
            }
            Err(e) => self.status = format!("Failed to read directory {rel_path}: {e}"),
        }
    }

    /// Returns file paths from `Templates/` at the workspace root for an
    /// SSH workspace, loading only the root `Templates` directory if it
    /// hasn't been expanded yet (avoids walking the entire remote workspace).
    fn ssh_root_template_paths(&mut self) -> Vec<String> {
        let templates_dir_path = self
            .tree
            .iter()
            .find(|n| n.is_dir && n.name.eq_ignore_ascii_case("templates") && n.children.is_none())
            .map(|n| n.path.clone());
        if let Some(dir_path) = templates_dir_path {
            self.load_ssh_dir_shallow(&dir_path);
        }
        let mut out = Vec::new();
        collect_tree_file_paths(&self.tree, &mut out);
        out
    }

    /// Kicks off a background thread that connects the tree-read
    /// accelerator agent (best-effort) and reads the initial root tree
    /// shallowly (`read_tree_shallow`), then polls it via `ssh_connecting`.
    pub(crate) fn open_ssh_workspace(&mut self, profile: ssh::SshProfile) {
        // Empty / `~` Remote folder = whatever ssh lands in (the login
        // user's home); see `ssh::normalize_remote_folder`.
        let profile = match ssh::normalize_remote_folder(&profile) {
            Ok(p) => p,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        ssh::agent_disconnect(&mut self.ssh_agent);
        self.workspace = None;
        self.has_git_repo = false;
        self.kotoshelf_dir_policy = KotoshelfDirPolicy::Undecided;
        self.gitignore_confirm_dialog = None;
        self.ssh_profile = Some(profile.clone());
        self.tree = Vec::new();
        self.wiki_target_set.clear();
        self.wiki_target_set_ready = false;
        self.ssh_image_cache.clear();
        self.status = format!("Connecting to {}…", ssh_display_target(&profile));
        let ssh_command_path = self.ssh_command_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut agent = if profile.disable_agent {
                None
            } else {
                ssh::agent_connect(&ssh_command_path, &profile).ok()
            };
            let agent_connected = agent.is_some();
            let mut tree = ssh::read_tree_shallow(&mut agent, &ssh_command_path, &profile, "");
            // If a root-level `Templates` folder exists, pre-fetch that single
            // folder so `ensure_and_open_homepage` can check for
            // `Templates/template_index.md` without an extra UI-thread round-trip.
            if let Ok(ref mut nodes) = tree {
                if let Some(tpl_dir) = nodes
                    .iter()
                    .find(|n| n.is_dir && n.name.eq_ignore_ascii_case("templates"))
                    .map(|n| n.path.clone())
                {
                    if let Ok(children) = ssh::read_tree_shallow(&mut agent, &ssh_command_path, &profile, &tpl_dir) {
                        ssh::insert_tree_children(nodes, &tpl_dir, children);
                    }
                }
            }
            let _ = tx.send(SshConnectOutcome { agent, tree, agent_connected });
        });
        self.ssh_connecting = Some(rx);
    }

    /// Spawns a terminal outside the app (not embedded) already `cd`'d
    /// into the remote workspace - best-effort, and deliberately
    /// synchronous: it just spawns a detached process and returns, no
    /// network round trip to wait on.
    fn open_ssh_terminal(&mut self) {
        let Some(profile) = self.ssh_profile.clone() else {
            self.status = "No remote workspace is open".to_string();
            return;
        };
        match ssh::open_terminal(&self.ssh_command_path, &profile) {
            Ok(()) => {}
            Err(e) => self.status = format!("Failed to open terminal: {e}"),
        }
    }

    fn rebuild_wiki_target_set(&mut self) {
        self.wiki_target_set = crate::wikilink::build_wiki_target_set(&self.tree);
        self.wiki_target_set_ready = true;
        for tab in &mut self.tabs {
            tab.preview_memo = None;
        }
    }

    fn refresh_tree(&mut self) {
        if let Some(profile) = self.ssh_profile.clone() {
            // Ported from Tauri's `refreshTree`: re-fetch the workspace root
            // (one level) plus whatever folders were already expanded/loaded,
            // avoiding a full recursive walk of the entire remote workspace.
            let mut loaded_dirs = Vec::new();
            ssh::collect_loaded_dir_paths(&self.tree, &mut loaded_dirs);
            match ssh::read_tree_shallow(&mut self.ssh_agent, &self.ssh_command_path, &profile, "") {
                Ok(root_nodes) => {
                    self.tree = root_nodes;
                    for dir_path in loaded_dirs {
                        if let Ok(children) =
                            ssh::read_tree_shallow(&mut self.ssh_agent, &self.ssh_command_path, &profile, &dir_path)
                        {
                            ssh::insert_tree_children(&mut self.tree, &dir_path, children);
                        }
                    }
                    self.rebuild_wiki_target_set();
                }
                Err(e) => self.status = format!("Failed to refresh workspace: {e}"),
            }
            return;
        }
        let Some(dir) = self.workspace.clone() else { return };
        if !self.git_available {
            self.git_available = git::is_git_available();
            self.git_lfs_available = git::is_git_lfs_available();
        }
        self.has_git_repo = git::is_git_repo(&dir);
        match agent_protocol::read_tree(&dir.to_string_lossy()) {
            Ok(tree) => {
                self.tree = agent_protocol::filter_tree(tree);
                self.rebuild_wiki_target_set();
            }
            Err(e) => self.status = format!("Failed to refresh workspace: {e}"),
        }
    }

    fn fs_read(&self, path: &TabPath) -> Result<String, String> {
        match path {
            TabPath::Local(p) => workspace::read_file(&p.to_string_lossy()),
            TabPath::Ssh(rel) => {
                let profile = self.ssh_profile.as_ref().ok_or("No SSH workspace open")?;
                ssh::read_file_text_guarded(&self.ssh_command_path, profile, rel)
            }
        }
    }

    fn fs_write(&mut self, path: &TabPath, content: &str) -> Result<(), String> {
        match path {
            TabPath::Local(p) => {
                if self.ensure_kotoshelf_dir_ready() {
                    if let Some(root) = &self.workspace {
                        workspace::snapshot_history(root.as_path(), p.as_path(), content);
                    }
                }
                workspace::write_file(&p.to_string_lossy(), content)
            }
            TabPath::Ssh(rel) => {
                let profile = self.ssh_profile.as_ref().ok_or("No SSH workspace open")?;
                ssh::write_file(&self.ssh_command_path, profile, rel, content)?;
                ssh::insert_tree_file(&mut self.tree, rel, content.len() as u64);
                self.rebuild_wiki_target_set();
                Ok(())
            }
        }
    }

    fn fs_create_dir(&self, path: &TabPath) -> Result<(), String> {
        match path {
            TabPath::Local(p) => workspace::create_dir(&p.to_string_lossy()),
            TabPath::Ssh(rel) => {
                let profile = self.ssh_profile.as_ref().ok_or("No SSH workspace open")?;
                ssh::create_dir(&self.ssh_command_path, profile, rel)
            }
        }
    }

    fn fs_rename(&self, from: &TabPath, to: &TabPath) -> Result<(), String> {
        match (from, to) {
            (TabPath::Local(f), TabPath::Local(t)) => workspace::rename_path(&f.to_string_lossy(), &t.to_string_lossy()),
            (TabPath::Ssh(f), TabPath::Ssh(t)) => {
                let profile = self.ssh_profile.as_ref().ok_or("No SSH workspace open")?;
                ssh::rename_path(&self.ssh_command_path, profile, f, t)
            }
            _ => Err("Cannot rename across local/SSH workspaces".to_string()),
        }
    }

    fn fs_trash(&self, path: &TabPath) -> Result<(), String> {
        match path {
            TabPath::Local(p) => workspace::trash_path(&p.to_string_lossy()),
            TabPath::Ssh(rel) => {
                let profile = self.ssh_profile.as_ref().ok_or("No SSH workspace open")?;
                ssh::trash_path(&self.ssh_command_path, profile, rel)
            }
        }
    }

    /// Opens a "New file in <label>/" prompt targeting `dir` - shared by
    /// the tree panel's root-level toolbar button and every node's
    /// context menu (which targets the clicked directory, or its
    /// clicked-file's parent).
    fn open_new_file_prompt(&mut self, dir: TabPath, label: String) {
        self.name_prompt = Some(NamePrompt {
            title: format!("New file in {label}/"),
            value: String::new(),
            action: NamePromptAction::NewFile { dir },
        });
        self.name_prompt_focus_pending = true;
    }

    fn open_new_folder_prompt(&mut self, dir: TabPath, label: String) {
        self.name_prompt = Some(NamePrompt {
            title: format!("New folder in {label}/"),
            value: String::new(),
            action: NamePromptAction::NewFolder { dir },
        });
        self.name_prompt_focus_pending = true;
    }

    fn apply_tree_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::Open(path) => self.open_file(path),
            TreeAction::ExpandDir { rel_path } => self.load_ssh_dir_shallow(&rel_path),
            TreeAction::NewFile { dir, label } => self.open_new_file_prompt(dir, label),
            TreeAction::NewFolder { dir, label } => self.open_new_folder_prompt(dir, label),
            TreeAction::Rename { path, name } => {
                self.name_prompt = Some(NamePrompt {
                    title: format!("Rename {name}"),
                    value: name,
                    action: NamePromptAction::Rename { old_path: path },
                });
                self.name_prompt_focus_pending = true;
            }
            TreeAction::Duplicate { path, name } => {
                let default_dup_name = generate_duplicate_name(&name);
                self.name_prompt = Some(NamePrompt {
                    title: format!("Duplicate {name}"),
                    value: default_dup_name,
                    action: NamePromptAction::Duplicate { src_path: path },
                });
                self.name_prompt_focus_pending = true;
            }
            TreeAction::Move { path, name } => {
                self.name_prompt = Some(NamePrompt {
                    title: format!("Move {name} to path"),
                    value: path.display_string(),
                    action: NamePromptAction::Move { src_path: path },
                });
                self.name_prompt_focus_pending = true;
            }
            TreeAction::Delete { path, name } => self.confirm_trash = Some((path, name)),
            TreeAction::MoveBulk { paths, name } => {
                let count = paths.len();
                self.name_prompt = Some(NamePrompt {
                    title: format!("Move {count} items to destination directory"),
                    value: name,
                    action: NamePromptAction::MoveBulk { src_paths: paths },
                });
                self.name_prompt_focus_pending = true;
            }
            TreeAction::DeleteBulk { paths } => {
                self.delete_bulk_nodes(&paths);
            }
            TreeAction::OpenTerminal { dir } => {
                self.open_terminal_at(&dir);
            }
            TreeAction::OpenFileManager { path } => {
                self.open_file_manager_at(&path);
            }
        }
    }

    fn apply_name_prompt(&mut self, action: NamePromptAction, name: String) {
        // New/rename/duplicate names must stay inside the folder they are
        // created in. Move/MoveBulk take a destination path by design and
        // are handled by their own logic.
        if matches!(
            action,
            NamePromptAction::NewFile { .. }
                | NamePromptAction::NewFolder { .. }
                | NamePromptAction::Rename { .. }
                | NamePromptAction::Duplicate { .. }
        ) {
            if let Err(e) = validate_entry_name(&name) {
                self.status = e;
                return;
            }
        }
        match action {
            NamePromptAction::NewFile { dir } => self.create_new_file(&dir, &name),
            NamePromptAction::NewFolder { dir } => self.create_new_folder(&dir, &name),
            NamePromptAction::Rename { old_path } => self.rename_node(&old_path, &name),
            NamePromptAction::Duplicate { src_path } => self.duplicate_node(&src_path, &name),
            NamePromptAction::Move { src_path } => self.move_node(&src_path, &name),
            NamePromptAction::MoveBulk { src_paths } => {
                let is_ssh = self.ssh_profile.is_some();
                let dest_dir = node_tab_path(is_ssh, &name);
                self.move_bulk_nodes(&src_paths, &dest_dir);
            }
            NamePromptAction::SaveAs { tab_index } => self.save_as_ssh(tab_index, &name),
        }
    }

    fn resolve_local_abs_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() && path.exists() {
            path.to_path_buf()
        } else if let Some((TabPath::Local(root), _)) = self.workspace_root() {
            let joined = root.join(path);
            if joined.exists() {
                joined
            } else if path.is_absolute() {
                path.to_path_buf()
            } else {
                joined
            }
        } else {
            path.to_path_buf()
        }
    }

    fn open_terminal_at(&mut self, dir: &TabPath) {
        match dir {
            TabPath::Local(raw_path) => {
                let abs_path = self.resolve_local_abs_path(raw_path);
                let target_dir = if abs_path.is_file() {
                    abs_path.parent().unwrap_or(&abs_path).to_path_buf()
                } else {
                    abs_path
                };

                #[cfg(target_os = "windows")]
                {
                    let outcome = std::process::Command::new("powershell.exe")
                        .current_dir(&target_dir)
                        .spawn();
                    if let Err(e) = outcome {
                        self.status = format!("Failed to open terminal: {e}");
                        return;
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("open")
                        .arg("-a")
                        .arg("Terminal")
                        .arg(&target_dir)
                        .spawn();
                }
                #[cfg(target_os = "linux")]
                {
                    let _ = std::process::Command::new("x-terminal-emulator")
                        .arg("--working-directory")
                        .arg(&target_dir)
                        .spawn();
                }
                self.status = format!("Opened terminal at {}", target_dir.display());
            }
            TabPath::Ssh(_) => {
                self.open_ssh_terminal();
            }
        }
    }

    fn open_file_manager_at(&mut self, path: &TabPath) {
        match path {
            TabPath::Local(raw_path) => {
                let abs_path = self.resolve_local_abs_path(raw_path);
                let target_dir = if abs_path.is_file() {
                    abs_path.parent().unwrap_or(&abs_path).to_path_buf()
                } else {
                    abs_path
                };

                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("explorer.exe")
                        .arg(&target_dir)
                        .spawn();
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("open")
                        .arg(&target_dir)
                        .spawn();
                }
                #[cfg(target_os = "linux")]
                {
                    let _ = std::process::Command::new("xdg-open")
                        .arg(&target_dir)
                        .spawn();
                }
                self.status = format!("Opened folder in file manager: {}", target_dir.display());
            }
            TabPath::Ssh(_) => {
                self.status = "File manager is only supported for local workspaces.".to_string();
            }
        }
    }

    fn delete_bulk_nodes(&mut self, paths: &[TabPath]) {
        let count = paths.len();
        let mut deleted_count = 0;
        for path in paths {
            match path {
                TabPath::Local(p) => {
                    if trash::delete(p).is_ok() {
                        deleted_count += 1;
                    }
                }
                TabPath::Ssh(rel) => {
                    if let Some(profile) = &self.ssh_profile {
                        if ssh::trash_path(&self.ssh_command_path, profile, rel).is_ok() {
                            ssh::remove_tree_node(&mut self.tree, rel);
                            deleted_count += 1;
                        }
                    }
                }
            }
        }
        self.selected_tree_nodes.clear();
        if self.ssh_profile.is_some() {
            self.rebuild_wiki_target_set();
        } else {
            self.refresh_tree();
        }
        self.status = format!("Deleted {deleted_count} of {count} items.");
    }

    fn move_bulk_nodes(&mut self, src_paths: &[TabPath], dest_dir: &TabPath) {
        let count = src_paths.len();
        let mut moved_count = 0;
        for src in src_paths {
            let name = src.file_name();
            let target = dest_dir.join(&name);
            if self.fs_rename(src, &target).is_ok() {
                moved_count += 1;
                for tab in &mut self.tabs {
                    if tab.path.as_ref() == Some(src) {
                        tab.path = Some(target.clone());
                        tab.name = target.file_name();
                        tab.title_buffer = tab.name.strip_suffix(".md").unwrap_or(&tab.name).to_string();
                    }
                }
            }
        }
        self.selected_tree_nodes.clear();
        self.refresh_tree();
        self.status = format!("Moved {moved_count} of {count} items.");
    }

    fn rename_tab_by_title(&mut self, tab_idx: usize, new_title: &str) {
        if tab_idx >= self.tabs.len() {
            return;
        }
        let clean_title = new_title.trim();
        if clean_title.is_empty() {
            // Restore title buffer from current tab name
            self.tabs[tab_idx].title_buffer = self.tabs[tab_idx].name.strip_suffix(".md").unwrap_or(&self.tabs[tab_idx].name).to_string();
            return;
        }

        let new_file_name = if clean_title.to_lowercase().ends_with(".md") {
            clean_title.to_string()
        } else {
            format!("{clean_title}.md")
        };

        if let Some(old_path) = self.tabs[tab_idx].path.clone() {
            if old_path.file_name() != new_file_name {
                let new_path = old_path.parent().join(&new_file_name);
                match self.fs_rename(&old_path, &new_path) {
                    Ok(()) => {
                        if let (TabPath::Ssh(from_rel), TabPath::Ssh(to_rel)) = (&old_path, &new_path) {
                            if ssh::rename_tree_node(&mut self.tree, from_rel, to_rel) {
                                self.rebuild_wiki_target_set();
                            } else {
                                self.refresh_tree();
                            }
                        } else {
                            self.refresh_tree();
                        }
                        self.status = format!("Renamed to {new_file_name}");
                        for tab in &mut self.tabs {
                            if tab.path.as_ref() == Some(&old_path) {
                                tab.path = Some(new_path.clone());
                                tab.name = new_path.file_name();
                                tab.title_buffer = clean_title.strip_suffix(".md").unwrap_or(clean_title).to_string();
                            }
                        }
                    }
                    Err(e) => {
                        self.status = format!("Failed to rename: {e}");
                        // Revert title buffer
                        self.tabs[tab_idx].title_buffer = old_path.file_name().strip_suffix(".md").unwrap_or(&old_path.file_name()).to_string();
                    }
                }
            } else {
                self.tabs[tab_idx].title_buffer = clean_title.strip_suffix(".md").unwrap_or(clean_title).to_string();
            }
        } else {
            // Untitled tab
            self.tabs[tab_idx].name = new_file_name;
            self.tabs[tab_idx].title_buffer = clean_title.strip_suffix(".md").unwrap_or(clean_title).to_string();
        }
    }

    fn duplicate_node(&mut self, src_path: &TabPath, new_name: &str) {
        let dst_path = src_path.parent().join(new_name);
        let res = match (src_path, &dst_path) {
            (TabPath::Local(src), TabPath::Local(dst)) => workspace::copy_node(&src.to_string_lossy(), &dst.to_string_lossy()),
            (TabPath::Ssh(src), TabPath::Ssh(dst)) => {
                if let Some(profile) = self.ssh_profile.clone() {
                    ssh::copy_path(&self.ssh_command_path, &profile, src, dst)
                } else {
                    Err("No active SSH profile".to_string())
                }
            }
            _ => Err("Mismatch path type".to_string()),
        };
        match res {
            Ok(()) => {
                self.refresh_tree();
                self.status = format!("Duplicated {}", dst_path.file_name());
            }
            Err(e) => self.status = format!("Failed to duplicate: {e}"),
        }
    }

    fn move_node(&mut self, src_path: &TabPath, new_target: &str) {
        let Some((root, _)) = self.workspace_root() else { return; };
        let dst_path = if new_target.contains('/') || new_target.contains('\\') {
            root.join(new_target)
        } else {
            src_path.parent().join(new_target)
        };

        let res = match (src_path, &dst_path) {
            (TabPath::Local(src), TabPath::Local(dst)) => workspace::move_node(&src.to_string_lossy(), &dst.to_string_lossy()),
            (TabPath::Ssh(src), TabPath::Ssh(dst)) => {
                if let Some(profile) = self.ssh_profile.clone() {
                    ssh::rename_path(&self.ssh_command_path, &profile, src, dst)
                } else {
                    Err("No active SSH profile".to_string())
                }
            }
            _ => Err("Mismatch path type".to_string()),
        };
        match res {
            Ok(()) => {
                self.refresh_tree();
                self.status = format!("Moved to {}", dst_path.display_string());
                for tab in &mut self.tabs {
                    if tab.path.as_ref() == Some(src_path) {
                        tab.path = Some(dst_path.clone());
                        tab.name = dst_path.file_name();
                        tab.title_buffer = tab.name.strip_suffix(".md").unwrap_or(&tab.name).to_string();
                    }
                }
            }
            Err(e) => self.status = format!("Failed to move: {e}"),
        }
    }

    fn create_new_file(&mut self, dir: &TabPath, name: &str) {
        let path = dir.join(name);
        match self.fs_write(&path, "") {
            Ok(()) => {
                if self.ssh_profile.is_none() {
                    self.refresh_tree();
                }
                self.open_file(path);
            }
            Err(e) => self.status = format!("Failed to create file: {e}"),
        }
    }

    fn create_new_folder(&mut self, dir: &TabPath, name: &str) {
        let path = dir.join(name);
        match self.fs_create_dir(&path) {
            Ok(()) => {
                if let TabPath::Ssh(rel) = &path {
                    ssh::insert_tree_dir(&mut self.tree, rel);
                } else {
                    self.refresh_tree();
                }
            }
            Err(e) => self.status = format!("Failed to create folder: {e}"),
        }
    }

    /// Follows a `[[target]]` wiki-link - matching is basename-only,
    /// case-insensitive, against every file in the *whole* workspace
    /// (not just whatever's currently expanded in the tree pane), with
    /// ".md" implicitly appended if `target` doesn't already end with
    /// it. Exactly one match opens it directly; more than one opens a
    /// picker (`wiki_choices`); zero matches creates an empty note,
    /// Obsidian-style, next to the file containing the link (or at the
    /// workspace root for an untitled/no-tab buffer). Mirrors the
    /// original React app's `resolveWikiLink` exactly, including its one
    /// quirk: no `[[target|alias]]`/`[[target#heading]]` support, so a
    /// pipe or hash in `target` is just part of the literal filename.
    fn resolve_wiki_link(&mut self, target: &str) {
        self.resolve_wiki_link_with_ctx(None, target);
    }

    fn resolve_wiki_link_with_ctx(&mut self, ctx: Option<&egui::Context>, target: &str) {
        let Some((root, _)) = self.workspace_root() else {
            self.status = "Open a workspace to resolve wiki links.".to_string();
            return;
        };
        let is_ssh = self.ssh_profile.is_some();
        let all_paths: Vec<String> = if let Some(profile) = self.ssh_profile.clone() {
            match ssh::list_all_paths(&self.ssh_command_path, &profile) {
                Ok(paths) => paths,
                Err(e) => {
                    self.status = format!("{e}");
                    return;
                }
            }
        } else if let Some(dir) = &self.workspace {
            workspace::list_all_paths(&dir.to_string_lossy())
        } else {
            return;
        };

        // Trim surrounding whitespace and handle alias (|) or heading (#) fragments
        let mut clean_target = target.trim();
        if let Some((base, _alias)) = clean_target.split_once('|') {
            clean_target = base.trim();
        }
        if let Some((base, _heading)) = clean_target.split_once('#') {
            clean_target = base.trim();
        }

        if clean_target.is_empty() {
            return;
        }

        let t = clean_target.to_lowercase();
        let t_norm = t.replace('\\', "/");
        let t_with_md = if t_norm.ends_with(".md") { t_norm.clone() } else { format!("{t_norm}.md") };
        let matches: Vec<TabPath> = all_paths
            .into_iter()
            .map(|raw| if is_ssh { TabPath::Ssh(raw) } else { TabPath::Local(PathBuf::from(raw)) })
            .filter(|p| {
                let name = p.file_name().to_lowercase();
                let full_path = p.display_string().replace('\\', "/").to_lowercase();
                name == t_with_md
                    || name == t
                    || full_path == t_norm
                    || full_path == t_with_md
                    || full_path.ends_with(&format!("/{t_norm}"))
                    || full_path.ends_with(&format!("/{t_with_md}"))
            })
            .collect();

        if matches.len() == 1 {
            self.open_file_with_ctx(ctx, matches.into_iter().next().unwrap());
            return;
        }
        if matches.len() > 1 {
            self.wiki_choices = Some(matches);
            return;
        }

        // Security check: reject explicit path traversal in clean_target
        if clean_target.contains("../") || clean_target.contains("..\\") || clean_target.starts_with('/') || clean_target.starts_with('\\') {
            self.status = format!("Security alert: WikiLink target contains path traversal sequence: '{clean_target}'");
            return;
        }

        let is_path_target = clean_target.contains('/') || clean_target.contains('\\');
        let file_name = if t.ends_with(".md") || t.ends_with(".pdf") { clean_target.to_string() } else { format!("{clean_target}.md") };
        let dir = if is_path_target {
            root.clone()
        } else {
            self.active.and_then(|i| self.tabs[i].path.clone()).map(|p| p.parent()).unwrap_or(root.clone())
        };
        let path = dir.join_normalized(&file_name);

        if !path.is_within_root(&root) {
            self.status = format!("Security alert: Target path '{clean_target}' escapes workspace root boundary.");
            return;
        }

        if is_pdf_path(&path) {
            self.open_external_file(&path);
            return;
        }

        // If file already exists, open it directly without wiping its content!
        if self.fs_read(&path).is_ok() {
            self.open_file_with_ctx(ctx, path);
            return;
        }

        match self.fs_write(&path, "") {
            Ok(()) => {
                self.refresh_tree();
                self.status = format!("Created {file_name}");
                self.open_file_with_ctx(ctx, path);
            }
            Err(e) => self.status = format!("{e}"),
        }
    }

    /// Runs `workspace_search.query` over every file in the open
    /// workspace (local or SSH, whichever is active), replacing
    /// `workspace_search.results` with the fresh matches. Synchronous
    /// like every other per-operation SSH call in this module (rename,
    /// trash, save, ...) - only the initial connect is backgrounded.
    fn run_workspace_search(&mut self) {
        let query = self.workspace_search.query.clone();
        let is_regex = self.workspace_search.is_regex;
        let case_sensitive = self.workspace_search.case_sensitive;
        let include_ignored = self.workspace_search.include_ignored;
        let outcome = if let Some(profile) = self.ssh_profile.clone() {
            ssh::search_workspace(&self.ssh_command_path, &profile, &query, is_regex, case_sensitive)
        } else if let Some(dir) = &self.workspace {
            workspace::search_workspace(&dir.to_string_lossy(), &query, is_regex, case_sensitive, include_ignored)
        } else {
            self.workspace_search.status = "Open a workspace to search.".to_string();
            return;
        };
        match outcome {
            Ok(results) => {
                let total: usize = results.iter().map(|r| r.matches.len()).sum();
                self.workspace_search.status = if results.is_empty() {
                    "No matches".to_string()
                } else {
                    format!("{total} match(es) in {} file(s)", results.len())
                };
                self.workspace_search.results = results;
            }
            Err(e) => {
                self.workspace_search.status = e;
                self.workspace_search.results.clear();
            }
        }
    }

    /// Replaces every match in every file from the *current* search
    /// results (not a fresh re-search) - same "replace what you already
    /// searched for" contract as the original's workspace replace.
    fn run_workspace_replace_all(&mut self) {
        let paths: Vec<String> = self.workspace_search.results.iter().map(|r| r.path.clone()).collect();
        if paths.is_empty() {
            self.workspace_search.status = "Nothing to replace - run a search first.".to_string();
            return;
        }
        let query = self.workspace_search.query.clone();
        let replacement = self.workspace_search.replacement.clone();
        let is_regex = self.workspace_search.is_regex;
        let case_sensitive = self.workspace_search.case_sensitive;
        let outcome = if let Some(profile) = self.ssh_profile.clone() {
            ssh::replace_in_files(&self.ssh_command_path, &profile, &paths, &query, &replacement, is_regex, case_sensitive)
        } else {
            workspace::replace_in_files(&paths, &query, &replacement, is_regex, case_sensitive)
        };
        match outcome {
            Ok(results) => {
                let total: usize = results.iter().map(|r| r.count).sum();
                self.workspace_search.status = format!("Replaced {total} match(es) in {} file(s)", results.len());
                // Any open tab whose on-disk content just changed
                // underneath it would otherwise silently drift out of
                // sync with what's on disk - reload it from the fresh
                // written content so the editor reflects the replace
                // instead of the buffer overwriting it back on next save.
                let touched: std::collections::HashSet<&str> = results.iter().map(|r| r.path.as_str()).collect();
                for tab in &mut self.tabs {
                    let Some(path) = &tab.path else { continue };
                    let key = match path {
                        TabPath::Local(p) => p.to_string_lossy().into_owned(),
                        TabPath::Ssh(p) => p.clone(),
                    };
                    if touched.contains(key.as_str()) && !tab.dirty {
                        let fresh = if let Some(profile) = self.ssh_profile.clone() {
                            ssh::read_file(&self.ssh_command_path, &profile, &key).ok()
                        } else {
                            workspace::read_file(&key).ok()
                        };
                        if let Some(content) = fresh {
                            tab.content = content;
                        }
                    }
                }
                self.run_workspace_search();
            }
            Err(e) => self.workspace_search.status = e,
        }
    }

    /// Opens the file a workspace-search result row belongs to and
    /// selects the exact matched range - opening happens via the same
    /// `open_file` every other jump-to-file path uses (tree click,
    /// wiki-link). The selection itself is deferred via
    /// `pending_search_selection` (see its doc comment) rather than
    /// applied immediately, since the target tab's editor may never have
    /// been shown before this call.
    fn open_search_match(&mut self, path: &str, m: &workspace::SearchMatch) {
        let tab_path = if self.ssh_profile.is_some() { TabPath::Ssh(path.to_string()) } else { TabPath::Local(PathBuf::from(path)) };
        self.open_file(tab_path);
        let Some(i) = self.active else { return };
        let tab_id = self.tabs[i].id;
        let line_starts = workspace::line_starts(&self.tabs[i].content);
        let Some((start, end)) = workspace::match_char_range(&line_starts, m) else { return };
        self.pending_search_selection = Some((tab_id, start, end));
    }

    /// The tree pane's contents - split out of the panel's `.show`
    /// closure so it can be selected between via `left_pane` alongside
    /// `show_search_panel` below.
    fn show_tree_pane(&mut self, ui: &mut egui::Ui) {
        let root = self.workspace_root();
        if let Some((root_path, root_label)) = &root {
            ui.horizontal(|ui| {
                if ui.small_button("New File").clicked() {
                    self.open_new_file_prompt(root_path.clone(), root_label.clone());
                }
                if ui.small_button("New Folder").clicked() {
                    self.open_new_folder_prompt(root_path.clone(), root_label.clone());
                }
                if ui.small_button("Refresh").clicked() {
                    self.refresh_tree();
                }
            });
            ui.separator();
        }
        let is_ssh = self.ssh_profile.is_some();
        let scroll_pending = self.tree_scroll_pending;
        self.tree_scroll_pending = false;
        let action = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let Some((root_path, root_label)) = &root else {
                    ui.label("Open a folder or connect to a remote workspace to get started");
                    return None;
                };
                if self.ssh_connecting.is_some() {
                    ui.label("Connecting…");
                }
                let mut action = show_tree(ui, &self.tree, is_ssh, root_label, &mut self.selected_tree_nodes, scroll_pending);
                // Claims whatever's left of the scroll area below the
                // last row - right-clicking there (rather than on a
                // node) means "act on the workspace root", the tree
                // equivalent of the original app's onContextMenu on
                // the tree container background.
                let bg = ui.allocate_response(ui.available_size(), egui::Sense::click());
                bg.context_menu(|ui| {
                    if ui.button("New File…").clicked() {
                        action = Some(TreeAction::NewFile { dir: root_path.clone(), label: root_label.clone() });
                        ui.close();
                    }
                    if ui.button("New Folder…").clicked() {
                        action = Some(TreeAction::NewFolder { dir: root_path.clone(), label: root_label.clone() });
                        ui.close();
                    }
                });
                action
            })
            .inner;
        if let Some(action) = action {
            self.apply_tree_action(action);
        }
    }

    /// Workspace-wide search/replace sidebar - query/replacement fields,
    /// regex/case-sensitivity checkboxes, and a scrollable per-file
    /// match list. Clicking a match opens its file (creating a tab if
    /// needed) and selects the exact matched range via
    /// `open_search_match`.
    fn show_search_panel(&mut self, ui: &mut egui::Ui) {
        if self.workspace_root().is_none() {
            ui.label("Open a folder or connect to a remote workspace to search.");
            return;
        }
        let mut run = false;
        let query_resp = ui.add(
            egui::TextEdit::singleline(&mut self.workspace_search.query)
                .hint_text("Search (space-separated = AND)"),
        );
        if query_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            run = true;
        }
        ui.add(egui::TextEdit::singleline(&mut self.workspace_search.replacement).hint_text("Replace"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.workspace_search.is_regex, "Regex");
            ui.checkbox(&mut self.workspace_search.case_sensitive, "Case");
            ui.checkbox(&mut self.workspace_search.include_ignored, "Include Ignored");
        });
        let filtered_results = workspace::filter_search_results(
            &self.workspace_search.results,
            &self.workspace_search.filter_query,
            self.workspace_search.case_sensitive,
        );
        let mut export_to_tab = false;
        ui.horizontal(|ui| {
            if ui.button("Search").clicked() {
                run = true;
            }
            let has_results = !self.workspace_search.results.is_empty();
            if ui.add_enabled(has_results, egui::Button::new("Replace All")).clicked() {
                self.run_workspace_replace_all();
            }
            if ui
                .add_enabled(!filtered_results.is_empty(), egui::Button::new("📄 New Tab"))
                .on_hover_text("Copy search results to a new Markdown tab")
                .clicked()
            {
                export_to_tab = true;
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.workspace_search.filter_query)
                    .hint_text("Filter in results…"),
            );
            if !self.workspace_search.filter_query.is_empty() && ui.small_button("✕").clicked() {
                self.workspace_search.filter_query.clear();
            }
        });
        if run {
            self.run_workspace_search();
        }
        if export_to_tab {
            let md = workspace::format_search_results_markdown(
                &self.workspace_search.query,
                &self.workspace_search.filter_query,
                &filtered_results,
                self.workspace.as_deref(),
            );
            self.open_untitled_with_content(md);
        }
        if !self.workspace_search.status.is_empty() {
            if self.workspace_search.filter_query.trim().is_empty() {
                ui.label(&self.workspace_search.status);
            } else {
                let filtered_total: usize = filtered_results.iter().map(|r| r.matches.len()).sum();
                ui.label(format!(
                    "{} (filtered: {} in {} file(s))",
                    self.workspace_search.status,
                    filtered_total,
                    filtered_results.len()
                ));
            }
        }
        ui.separator();

        let mut clicked: Option<(String, workspace::SearchMatch)> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for result in &filtered_results {
                ui.label(egui::RichText::new(&result.path).strong());
                for m in &result.matches {
                    if ui.selectable_label(false, format!("  {}: {}", m.line, m.preview)).clicked() {
                        clicked = Some((result.path.clone(), m.clone()));
                    }
                }
            }
        });
        if let Some((path, m)) = clicked {
            self.open_search_match(&path, &m);
        }
    }

    /// Returns tab `i`'s content with every schemeless `![alt](target)`
    /// image reference rewritten to a URI `egui_commonmark`'s viewer can
    /// actually load - an absolute `file://` URI for a local workspace
    /// (which egui's own built-in file loader resolves and caches), or a
    /// synthetic `kotoshelf-ssh://<relpath>` URI for an SSH one, whose
    /// bytes this method fetches (on first reference; `ssh_image_cache`
    /// after that) and registers via `ctx.include_bytes` so the same
/// Resolves a media target reference (image, PDF, or video) to a URI registered with egui context
pub(crate) fn resolve_media_uri(
    ctx: &egui::Context,
    media_target: &str,
    dir: &TabPath,
    video_render_cache: &mut HashMap<String, Option<Vec<u8>>>,
    pdf_render_cache: &mut HashMap<String, Option<Vec<u8>>>,
    ssh_image_cache: &mut HashMap<String, Option<Vec<u8>>>,
    ssh_profile: &Option<ssh::SshProfile>,
    ssh_command_path: &str,
) -> Option<String> {
    let is_video = crate::video::is_video_path(Path::new(media_target));
    let is_pdf = crate::attachments::is_pdf_path(Path::new(media_target));
    let target_path = dir.join_normalized(media_target);

    match target_path {
        TabPath::Local(p) => {
            if is_video {
                let video_uri = format!("kotoshelf-video://{}", p.to_string_lossy().replace('\\', "/"));
                if !video_render_cache.contains_key(&video_uri) {
                    if video_render_cache.len() >= 50 {
                        if let Some(oldest_key) = video_render_cache.keys().next().cloned() {
                            video_render_cache.remove(&oldest_key);
                        }
                    }
                    let filename = p.file_name().and_then(|n| n.to_str()).unwrap_or("video.mp4");
                    let png_bytes = crate::video::render_video_thumbnail(Some(&p), filename).ok();
                    if let Some(png) = &png_bytes {
                        ctx.include_bytes(video_uri.clone(), png.clone());
                    }
                    video_render_cache.insert(video_uri.clone(), png_bytes);
                }
                Some(video_uri)
            } else if is_pdf {
                let pdf_uri = format!("kotoshelf-pdf://{}", p.to_string_lossy().replace('\\', "/"));
                if !pdf_render_cache.contains_key(&pdf_uri) {
                    if pdf_render_cache.len() >= 50 {
                        if let Some(oldest_key) = pdf_render_cache.keys().next().cloned() {
                            pdf_render_cache.remove(&oldest_key);
                        }
                    }
                    let png_bytes = std::fs::read(&p).ok().and_then(|bytes| crate::pdf::render_first_page_to_png(&bytes).ok());
                    if let Some(png) = &png_bytes {
                        ctx.include_bytes(pdf_uri.clone(), png.clone());
                    }
                    pdf_render_cache.insert(pdf_uri.clone(), png_bytes);
                }
                Some(pdf_uri)
            } else {
                Some(images::local_file_uri(&p))
            }
        }
        TabPath::Ssh(rel) => {
            if is_video {
                let video_uri = format!("kotoshelf-video-ssh://{rel}");
                if !video_render_cache.contains_key(&video_uri) {
                    if video_render_cache.len() >= 50 {
                        if let Some(oldest_key) = video_render_cache.keys().next().cloned() {
                            video_render_cache.remove(&oldest_key);
                        }
                    }
                    let filename = Path::new(&rel).file_name().and_then(|n| n.to_str()).unwrap_or("video.mp4");
                    let png_bytes = crate::video::render_video_thumbnail(None, filename).ok();
                    if let Some(png) = &png_bytes {
                        ctx.include_bytes(video_uri.clone(), png.clone());
                    }
                    video_render_cache.insert(video_uri.clone(), png_bytes);
                }
                Some(video_uri)
            } else if is_pdf {
                let pdf_uri = format!("kotoshelf-pdf-ssh://{rel}");
                if !pdf_render_cache.contains_key(&pdf_uri) {
                    if pdf_render_cache.len() >= 50 {
                        if let Some(oldest_key) = pdf_render_cache.keys().next().cloned() {
                            pdf_render_cache.remove(&oldest_key);
                        }
                    }
                    let png_bytes = ssh_profile
                        .as_ref()
                        .and_then(|profile| ssh::read_file_guarded(ssh_command_path, profile, &rel).ok())
                        .and_then(|bytes| crate::pdf::render_first_page_to_png(&bytes).ok());
                    if let Some(png) = &png_bytes {
                        ctx.include_bytes(pdf_uri.clone(), png.clone());
                    }
                    pdf_render_cache.insert(pdf_uri.clone(), png_bytes);
                }
                Some(pdf_uri)
            } else {
                let uri = format!("kotoshelf-ssh://{rel}");
                if !ssh_image_cache.contains_key(&uri) {
                    let bytes = ssh_profile
                        .as_ref()
                        .and_then(|profile| ssh::read_file_guarded(ssh_command_path, profile, &rel).ok());
                    if let Some(b) = &bytes {
                        ctx.include_bytes(uri.clone(), b.clone());
                    }
                    ssh_image_cache.insert(uri.clone(), bytes);
                }
                Some(uri)
            }
        }
    }
}

    /// Resolves a media target reference (image, PDF, or video) to a URI registered with egui context
    fn resolve_media_texture_uri(
        &mut self,
        ctx: &egui::Context,
        media_target: &str,
        dir: &TabPath,
    ) -> Option<String> {
        Self::resolve_media_uri(
            ctx,
            media_target,
            dir,
            &mut self.video_render_cache,
            &mut self.pdf_render_cache,
            &mut self.ssh_image_cache,
            &self.ssh_profile,
            &self.ssh_command_path,
        )
    }

    /// Returns tab `i`'s content with every schemeless `![alt](target)`
    /// image reference rewritten to a URI `egui_commonmark`'s viewer can
    /// actually load - an absolute `file://` URI for a local workspace,
    /// a synthetic `kotoshelf-video://` URI for videos, a synthetic `kotoshelf-pdf://` URI for PDFs,
    /// or a synthetic `kotoshelf-ssh://<relpath>` URI for an SSH workspace.
    fn render_preview_markdown(&mut self, ctx: &egui::Context, i: usize) -> String {
        let content = self.tabs[i].content.clone();

        // 1. Return cached preview if content hasn't changed (0ms per-frame overhead)
        if let Some((cached_src, cached_rendered)) = &self.tabs[i].preview_memo {
            if cached_src == &content {
                return cached_rendered.clone();
            }
        }

        let dir = self.tabs[i]
            .path
            .as_ref()
            .map(|p| p.parent())
            .or_else(|| self.workspace_root().map(|(root, _)| root))
            .unwrap_or_else(|| TabPath::Local(PathBuf::new()));
        let check_exists = self.workspace_root().is_some() && self.wiki_target_set_ready;
        let known_targets = &self.wiki_target_set;
        let with_wikilinks = crate::wikilink::rewrite_wiki_links_to_markdown_with_exists(&content, |target| {
            !check_exists || crate::wikilink::wiki_target_exists(target, known_targets)
        });
        let rendered = images::rewrite_image_refs(&with_wikilinks, |target| {
            self.resolve_media_texture_uri(ctx, target, &dir).unwrap_or_else(|| target.to_string())
        });

        // Store memoized preview
        self.tabs[i].preview_memo = Some((content, rendered.clone()));
        rendered
    }

    /// Dispatches one OS-dropped file path (from `ctx.input(|i|
    /// i.raw.dropped_files)`) - an image or PDF gets attached next to the
    /// active note, anything else opens as a tab if a local workspace
    /// (or no workspace at all) is active. A dropped path is always a
    /// local OS path, so it can never become a tab in an SSH workspace -
    /// matches the original's rejection message for that case exactly.
    fn handle_dropped_path(&mut self, ctx: &egui::Context, path: PathBuf) {
        if attachments::is_supported_attachment_path(&path) {
            self.attach_dropped_file(ctx, &path);
        } else if self.ssh_profile.is_some() {
            self.status = "Dropped files can't be opened while a remote workspace is open.".to_string();
        } else {
            self.open_file(TabPath::Local(path));
        }
    }

    /// Reads `src` off disk and hands it to `attach_image_bytes` - the
    /// drag-and-drop entry point, which (unlike a clipboard paste) has a
    /// real source file to read and a real original filename to derive
    /// the attachment's name from.
    fn attach_dropped_file(&mut self, ctx: &egui::Context, src: &Path) {
        let is_video = crate::video::is_video_path(src);
        let is_pdf = src.extension().map(|e| e.eq_ignore_ascii_case("pdf")).unwrap_or(false);
        let default_name = if is_video {
            "video.mp4"
        } else if is_pdf {
            "document.pdf"
        } else {
            "image"
        };
        let original_name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| default_name.to_string());
        match std::fs::read(src) {
            Ok(bytes) => self.attach_image_bytes(ctx, &original_name, &bytes),
            Err(e) => self.status = format!("Failed to read {}: {e}", src.display()),
        }
    }

    /// Copies (local workspace) or uploads (SSH workspace) `bytes` into
    /// an `attachments/` directory next to the active tab's own file,
    /// then inserts a relative `![](attachments/<name>)` reference at
    /// the cursor - shared by drag-and-drop (`attach_dropped_image`,
    /// bytes read from the dropped path) and clipboard-paste (bytes
    /// already in memory, no source path to read). Requires an active
    /// tab with a saved path - mirrors the original's two-step guard
    /// exactly ("open a tab" vs. "save this file first"), since an
    /// attachment's destination is defined relative to the note
    /// referencing it, which an unsaved/untitled buffer doesn't have.
    fn attach_image_bytes(&mut self, ctx: &egui::Context, original_name: &str, bytes: &[u8]) {
        let Some(i) = self.active else {
            self.status = "Open a tab before attaching images.".to_string();
            return;
        };
        let Some(tab_path) = self.tabs[i].path.clone() else {
            self.status = "Save this file first to attach images.".to_string();
            return;
        };
        let filename = attachments::attachment_filename(original_name);
        let dest = tab_path.parent().join(attachments::ATTACHMENTS_DIR).join(&filename);

        let result: Result<(), String> = match dest {
            TabPath::Local(dest_path) => workspace::write_bytes_into(&dest_path.to_string_lossy(), bytes),
            TabPath::Ssh(dest_rel) => {
                let Some(profile) = self.ssh_profile.clone() else {
                    // Unreachable in practice: `dest` only comes out `Ssh`
                    // when `tab_path` (hence `self.ssh_profile`) already is.
                    return;
                };
                ssh::upload_bytes(&self.ssh_command_path, &profile, &dest_rel, bytes)
            }
        };

        match result {
            Ok(()) => {
                self.insert_media_link_on_own_line(ctx, &format!("![]({}/{filename})", attachments::ATTACHMENTS_DIR));
                self.status = format!("Attached {filename}");
                self.refresh_tree();
            }
            Err(e) => self.status = e,
        }
    }

    /// Inserts a Markdown media link (`![](attachments/...)`) on its own dedicated line
    /// and appends 7 trailing newlines (`\n\n\n\n\n\n\n`) after it.
    ///
    /// Why:
    /// `egui`'s text shaper (`cosmic-text`) ignores per-section font-size overrides on
    /// bare `\n` characters and sizes line height purely from the row's visible glyphs.
    /// The media thumbnail card is 120px tall (~7 text lines), so inserting 7 newlines
    /// per attached media item guarantees physical vertical spacing below each thumbnail
    /// and places the cursor (and the next dropped file) cleanly below the preview card.
    fn insert_media_link_on_own_line(&mut self, ctx: &egui::Context, link: &str) {
        const MEDIA_TRAILING_NEWLINES: usize = 7;

        let Some(i) = self.active else { return };
        let tab_id = self.tabs[i].id;
        let pos = egui::TextEdit::load_state(ctx, editor_id(tab_id))
            .and_then(|s| s.cursor.char_range())
            .map(|r| r.primary.index.0)
            .unwrap_or_else(|| self.tabs[i].content.chars().count());
        let chars: Vec<char> = self.tabs[i].content.chars().collect();
        let pos = pos.min(chars.len());

        let needs_leading_newline = pos > 0 && chars[pos - 1] != '\n';

        let mut inserted = String::new();
        if needs_leading_newline {
            inserted.push('\n');
        }
        inserted.push_str(link);
        for _ in 0..MEDIA_TRAILING_NEWLINES {
            inserted.push('\n');
        }

        self.tabs[i].content =
            chars[..pos].iter().collect::<String>() + &inserted + &chars[pos..].iter().collect::<String>();
        self.tabs[i].dirty = true;

        // Place the cursor after all 7 trailing newlines so the next dropped file
        // (or user typing) lands directly below the thumbnail card.
        let new_pos = pos + inserted.chars().count();
        let mut state = egui::TextEdit::load_state(ctx, editor_id(tab_id)).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_pos))));
        egui::TextEdit::store_state(ctx, editor_id(tab_id), state);
    }

    /// Inserts `text` at the active tab's current cursor position (or at
    /// the end of the buffer if the editor has no recorded cursor yet -
    /// e.g. the very first frame after opening it). Used by the
    /// drag-and-drop attach flow to place the new `![]( ... )` reference
    /// exactly where the user's caret was, not just appended.
    fn insert_at_cursor(&mut self, ctx: &egui::Context, text: &str) {
        let Some(i) = self.active else { return };
        let tab_id = self.tabs[i].id;
        let pos = egui::TextEdit::load_state(ctx, editor_id(tab_id))
            .and_then(|s| s.cursor.char_range())
            .map(|r| r.primary.index.0)
            .unwrap_or_else(|| self.tabs[i].content.chars().count());
        let chars: Vec<char> = self.tabs[i].content.chars().collect();
        let pos = pos.min(chars.len());
        self.tabs[i].content = chars[..pos].iter().collect::<String>() + text + &chars[pos..].iter().collect::<String>();
        self.tabs[i].dirty = true;
        let new_pos = pos + text.chars().count();
        let mut state = egui::TextEdit::load_state(ctx, editor_id(tab_id)).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_pos))));
        egui::TextEdit::store_state(ctx, editor_id(tab_id), state);
    }

    /// Scans every `.md`/`.txt` file in the open workspace (local or
    /// SSH) for unchecked tasks due or scheduled today or earlier -
    /// synchronous, like `run_workspace_search` above, since it reuses
    /// the exact same proven-fast `list_all_paths` + per-file read
    /// pattern (this only reads text files, never binaries/attachments).
    /// Scans every `.md` file in the open workspace (local or SSH) for
    /// unchecked tasks due or scheduled today or earlier. Uses an
    /// in-memory incremental `task_cache` with file `mtime` so unchanged
    /// files skip disk I/O and parsing entirely.
    fn scan_all_tasks(&mut self) -> Result<Vec<dashboard::DueTask>, String> {
        if let Some(profile) = self.ssh_profile.clone() {
            let files = ssh::fetch_markdown_files(&self.ssh_command_path, &profile)?;
            let mut found = Vec::new();
            for (rel_path, content) in files {
                if dashboard::is_scannable_extension(&rel_path) {
                    found.extend(dashboard::tasks_in_file(&rel_path, &content));
                }
            }
            return Ok(found);
        }
        let is_ssh = false;
        let all_paths: Vec<String> = if let Some(dir) = &self.workspace {
            workspace::list_all_paths(&dir.to_string_lossy())
        } else {
            return Err("Open a workspace to list tasks.".to_string());
        };

        let mut found: Vec<dashboard::DueTask> = Vec::new();
        let mut seen_keys = std::collections::HashSet::new();

        for raw in all_paths {
            if !dashboard::is_scannable_extension(&raw) {
                continue;
            }

            let rel_path = if is_ssh {
                raw.clone()
            } else {
                self.workspace
                    .as_ref()
                    .and_then(|dir| PathBuf::from(&raw).strip_prefix(dir).ok().map(|p| p.to_string_lossy().replace('\\', "/")))
                    .unwrap_or_else(|| raw.clone())
            };
            seen_keys.insert(rel_path.clone());

            let tab_path = if is_ssh { TabPath::Ssh(raw.clone()) } else { TabPath::Local(PathBuf::from(&raw)) };

            // Check if file is unmodified on disk via mtime (local workspaces)
            let mtime_opt = match &tab_path {
                TabPath::Local(p) => std::fs::metadata(p).and_then(|m| m.modified()).ok(),
                TabPath::Ssh(_) => None,
            };

            if let Some(mtime) = mtime_opt {
                if let Some((cached_mtime, cached_tasks)) = self.task_cache.get(&rel_path) {
                    if *cached_mtime == mtime {
                        // Unmodified file - reuse cached tasks immediately
                        found.extend(cached_tasks.clone());
                        continue;
                    }
                }
            }

            // Read and parse modified/new file (collect all unchecked tasks)
            let Ok(content) = self.fs_read(&tab_path) else { continue };
            let file_tasks = dashboard::tasks_in_file(&rel_path, &content);

            if let Some(mtime) = mtime_opt {
                self.task_cache.insert(rel_path.clone(), (mtime, file_tasks.clone()));
            }

            found.extend(file_tasks);
        }

        // Prune deleted files from cache
        self.task_cache.retain(|k, _| seen_keys.contains(k));

        Ok(found)
    }

    fn scan_due_tasks(&mut self, today: chrono::NaiveDate) -> Result<Vec<dashboard::DueTask>, String> {
        let tasks = self.scan_all_tasks()?;
        Ok(tasks.into_iter().filter(|t| t.is_due_on_or_before(today)).collect())
    }

    /// Helper used for task generation tests and fallback insertion
    fn generate_today_tasks(&mut self, ctx: &egui::Context) {
        let Some(i) = self.active else {
            self.status = "Open a file to insert today's tasks into.".to_string();
            return;
        };
        let today = chrono::Local::now().date_naive();
        match self.scan_due_tasks(today) {
            Ok(tasks) => {
                let count = tasks.len();
                let current_content = &self.tabs[i].content;
                if current_content.contains("task") && (current_content.contains('{') || current_content.contains("{{")) {
                    let expanded = dashboard::expand_task_queries(current_content, &tasks, today);
                    self.tabs[i].content = expanded;
                    self.tabs[i].dirty = true;
                    self.status = format!("Expanded task queries ({count} due task(s) found).");
                } else {
                    let rendered = dashboard::render_today_list(&tasks, today);
                    self.insert_at_cursor(ctx, &rendered);
                    self.status = format!("Inserted {count} due task(s).");
                }
                // Restore focus to editor so subsequent clicks register immediately
                self.focus_pending = true;
            }
            Err(e) => self.status = e,
        }
    }

    /// After a `dashboard`-generated line is checked in the preview pane
    /// (`source_path` from its own trailing `(path#section)` link,
    /// `generated_line` the task line itself - link and all, `same_task_
    /// ignoring_checked_state` strips it internally), finds the matching
    /// line (by content, ignoring checked state - line numbers aren't
    /// reliable since the source file may have been edited independently
    /// since the list was generated) in the source file and checks it
    /// there too, applying the same recurrence/on-completion handling
    /// `apply_task_completion` gives a normal in-place checkbox toggle -
    /// the *source* task is the real one, so it's the one that gets the
    /// full cycle-forward treatment; the generated line the user actually
    /// clicked is just a pointer to it and was already given a plain
    /// marker flip by `apply_checkbox_toggle`. Silently does nothing if
    /// the source file can't be read or the matching line can no longer
    /// be found (already checked/edited/deleted since the list was
    /// generated).
    fn propagate_check_to_source(&mut self, source_path: &str, generated_line: &str) {
        let tab_path = if self.ssh_profile.is_some() {
            TabPath::Ssh(source_path.to_string())
        } else if let Some(dir) = &self.workspace {
            TabPath::Local(dir.join(source_path))
        } else {
            return;
        };

        // If the source file is already open in a tab, take its live content
        let open_tab_idx = self.tabs.iter().position(|t| {
            if let Some(p) = &t.path {
                p == &tab_path || p.display_string().replace('\\', "/") == tab_path.display_string().replace('\\', "/")
            } else {
                false
            }
        });

        let content = if let Some(t_idx) = open_tab_idx {
            self.tabs[t_idx].content.clone()
        } else {
            let Ok(c) = self.fs_read(&tab_path) else { return };
            c
        };

        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        let is_checked = {
            let t = generated_line.trim_start();
            t.starts_with("- [x]")
                || t.starts_with("- [X]")
                || t.starts_with("* [x]")
                || t.starts_with("* [X]")
                || t.starts_with("+ [x]")
                || t.starts_with("+ [X]")
        };

        if is_checked {
            let Some(pos) = lines.iter().position(|l| dashboard::same_task_ignoring_checked_state(l, generated_line)) else {
                return;
            };
            for (old, new) in [("- [ ]", "- [x]"), ("* [ ]", "* [x]"), ("+ [ ]", "+ [x]")] {
                if let Some(p) = lines[pos].find(old) {
                    lines[pos].replace_range(p..p + old.len(), new);
                    break;
                }
            }
            apply_task_completion(&mut lines, pos, chrono::Local::now().date_naive());
        } else if let Some(pos) = lines.iter().position(|l| dashboard::same_task_ignoring_checked_state(l, generated_line)) {
            let meta = tasks::parse_task_meta(&lines[pos]);
            let today = chrono::Local::now().date_naive();
            for (old, new) in [
                ("- [x]", "- [ ]"),
                ("- [X]", "- [ ]"),
                ("* [x]", "* [ ]"),
                ("* [X]", "* [ ]"),
                ("+ [x]", "+ [ ]"),
                ("+ [X]", "+ [ ]"),
            ] {
                if let Some(p) = lines[pos].find(old) {
                    lines[pos].replace_range(p..p + old.len(), new);
                    break;
                }
            }
            lines[pos] = tasks::strip_done_date(&lines[pos]);

            // If completing this recurring task previously inserted the next cycle's
            // unchecked occurrence directly above (`pos - 1`), remove that auto-generated
            // line so uncompleting doesn't leave duplicate occurrences.
            if let Some(rule) = &meta.recur {
                if pos > 0 {
                    let original_reference = meta.anchor().unwrap_or(today);
                    let computed_from = if rule.when_done { today } else { original_reference };
                    if let Some(next) = tasks::next_date_after(rule, computed_from) {
                        let shift = next - original_reference;
                        let expected_next = tasks::shift_dates(&lines[pos], shift);
                        if dashboard::same_task_ignoring_checked_state(&lines[pos - 1], &expected_next) {
                            let prev_trimmed = lines[pos - 1].trim_start();
                            if prev_trimmed.starts_with("- [ ]")
                                || prev_trimmed.starts_with("* [ ]")
                                || prev_trimmed.starts_with("+ [ ]")
                            {
                                lines.remove(pos - 1);
                            }
                        }
                    }
                }
            }
        } else {
            // The source line wasn't found under its original dates — happens when
            // the task had `🏁 delete` (`on_completion_delete`), so completing it removed
            // the original line (and, if recurring, left only the shifted next-cycle line).
            let restored = restore_unchecked_source_line(generated_line);
            let meta = tasks::parse_task_meta(&restored);
            if !meta.on_completion_delete {
                return;
            }
            let today = chrono::Local::now().date_naive();
            let mut reverted_recurring = false;
            if let Some(rule) = &meta.recur {
                let original_reference = meta.anchor().unwrap_or(today);
                let computed_from = if rule.when_done { today } else { original_reference };
                if let Some(next) = tasks::next_date_after(rule, computed_from) {
                    let shift = next - original_reference;
                    let expected_next = tasks::shift_dates(&restored, shift);
                    if let Some(next_pos) = lines.iter().position(|l| dashboard::same_task_ignoring_checked_state(l, &expected_next)) {
                        let indent: String = lines[next_pos].chars().take_while(|c| c.is_whitespace()).collect();
                        lines[next_pos] = format!("{indent}{}", restored.trim_start());
                        reverted_recurring = true;
                    }
                }
            }
            if !reverted_recurring {
                let target_section = dashboard::parse_link(generated_line).and_then(|(_, sec)| sec);
                if let Some(sec) = target_section {
                    if let Some(h_idx) = lines.iter().position(|l| dashboard::parse_heading(l).as_deref() == Some(sec.as_str())) {
                        let mut insert_at = h_idx + 1;
                        while insert_at < lines.len() && dashboard::parse_heading(&lines[insert_at]).is_none() {
                            insert_at += 1;
                        }
                        while insert_at > h_idx + 1 && lines[insert_at - 1].trim().is_empty() {
                            insert_at -= 1;
                        }
                        lines.insert(insert_at, restored);
                    } else {
                        lines.push(restored);
                    }
                } else {
                    lines.push(restored);
                }
            }
        }

        let mut rebuilt = lines.join("\n");
        if content.ends_with('\n') {
            rebuilt.push('\n');
        }
        if let Err(e) = self.fs_write(&tab_path, &rebuilt) {
            self.status = format!("Checked here, but failed to update source file: {e}");
            return;
        }
        self.task_cache.remove(&source_path.replace('\\', "/"));

        for tab in &mut self.tabs {
            if let Some(p) = &tab.path {
                if p == &tab_path || p.display_string().replace('\\', "/") == tab_path.display_string().replace('\\', "/") {
                    tab.content = rebuilt.clone();
                    tab.dirty = false;
                }
            }
        }
    }

    /// "Template" menu/toolbar action - scans the workspace for
    /// `Templates/template_*` files (local or SSH) and opens the picker
    /// (`template_picker`) if any were found.
    fn open_template_picker(&mut self) {
        if self.active.is_none() {
            self.status = "Open a file to insert a template into.".to_string();
            return;
        }
        let all_paths: Vec<String> = if self.ssh_profile.is_some() {
            self.ssh_root_template_paths()
        } else if let Some(dir) = &self.workspace {
            workspace::list_all_paths(&dir.to_string_lossy())
        } else {
            self.status = "Open a workspace to use templates.".to_string();
            return;
        };

        let template_root = if self.ssh_profile.is_some() { None } else { self.workspace.as_ref().map(|d| d.to_string_lossy().into_owned()) };
        let mut choices: Vec<(String, String)> = all_paths
            .into_iter()
            .filter(|p| templates::is_template_path(p, template_root.as_deref()))
            .map(|p| (templates::display_name(&p), p))
            .collect();
        if choices.is_empty() {
            self.status = "No templates found in Templates/.".to_string();
            return;
        }
        choices.sort_by(|a, b| a.0.cmp(&b.0));
        self.template_picker = Some(choices);
    }

    /// Reads `raw_path` (as produced by `list_all_paths`, so already the
    /// right shape for a local absolute path or an SSH relative path),
    /// expands its `{{placeholder}}`s, and inserts the result at the
    /// active tab's cursor - the same `insert_at_cursor` drag-and-drop
    /// attach and "Today's Tasks" already share.
    fn insert_template(&mut self, ctx: &egui::Context, raw_path: &str) {
        let tab_path =
            if self.ssh_profile.is_some() { TabPath::Ssh(raw_path.to_string()) } else { TabPath::Local(PathBuf::from(raw_path)) };
        match self.fs_read(&tab_path) {
            Ok(content) => {
                let all_tasks = self.scan_all_tasks().unwrap_or_default();
                let expanded = templates::expand_with_tasks(&content, chrono::Local::now().naive_local(), &all_tasks);
                self.insert_at_cursor(ctx, &expanded);
                self.status = "Template inserted.".to_string();
            }
            Err(e) => self.status = e,
        }
    }

    fn open_settings_dialog(&mut self) {
        let config = ssh::load_app_config();
        let tokens = self
            .tokens
            .iter()
            .map(|(k, v)| TokenEntry {
                key: k.clone(),
                value: v.clone(),
                masked: true,
            })
            .collect();
        self.settings_dialog = Some(SettingsDialogState {
            presets: config.presets,
            selected_preset: 0,
            tokens,
            active_tab: SettingsTab::Presets,
            new_token_key: String::new(),
            new_token_value: String::new(),
            validation_error: None,
        });
    }

    fn show_settings_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.settings_dialog.take() else { return };
        let mut save = false;
        let mut close_dialog = false;
        let mut open = true;

        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([780.0, 600.0])
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                if let Some(err) = &dialog.validation_error {
                    ui.colored_label(egui::Color32::RED, format!("⚠️ {err}"));
                    ui.add_space(4.0);
                }

                ui.horizontal(|ui| {
                    if ui.selectable_label(dialog.active_tab == SettingsTab::Presets, "Presets").clicked() {
                        dialog.active_tab = SettingsTab::Presets;
                    }
                    if ui.selectable_label(dialog.active_tab == SettingsTab::Tokens, "Tokens").clicked() {
                        dialog.active_tab = SettingsTab::Tokens;
                    }
                });
                ui.separator();

                let available_height = ui.available_height() - 45.0;
                egui::ScrollArea::vertical()
                    .max_height(available_height)
                    .show(ui, |ui| {
                        match dialog.active_tab {
                            SettingsTab::Presets => {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.set_min_width(140.0);
                                        ui.label("Presets");
                                        for (i, p) in dialog.presets.iter().enumerate() {
                                            let label = if p.name.is_empty() { "(unnamed)" } else { &p.name };
                                            if ui.selectable_label(dialog.selected_preset == i, label).clicked() {
                                                dialog.selected_preset = i;
                                            }
                                        }
                                        ui.horizontal(|ui| {
                                            if ui.button("+ Add").clicked() {
                                                dialog.presets.push(api::ApiPreset::new(format!("preset-{}", dialog.presets.len() + 1)));
                                                dialog.selected_preset = dialog.presets.len() - 1;
                                            }
                                            if ui.add_enabled(!dialog.presets.is_empty(), egui::Button::new("Remove")).clicked() {
                                                dialog.presets.remove(dialog.selected_preset);
                                                dialog.selected_preset = dialog.selected_preset.saturating_sub(1);
                                            }
                                        });
                                    });
                                    ui.separator();

                                    ui.vertical(|ui| {
                                        ui.set_min_width(560.0);
                                        if let Some(preset) = dialog.presets.get_mut(dialog.selected_preset) {
                                            egui::Grid::new("preset_fields").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                                                ui.label("Name");
                                                ui.add(egui::TextEdit::singleline(&mut preset.name).desired_width(f32::INFINITY));
                                                ui.end_row();

                                                ui.label("URL");
                                                ui.add(egui::TextEdit::singleline(&mut preset.url).desired_width(f32::INFINITY));
                                                ui.end_row();

                                                ui.label("Method");
                                                egui::ComboBox::from_id_salt("preset_method").selected_text(&preset.method).show_ui(ui, |ui| {
                                                    for m in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
                                                        ui.selectable_value(&mut preset.method, m.to_string(), m);
                                                    }
                                                });
                                                ui.end_row();

                                                ui.label("Response target");
                                                let target_label = match preset.response_target.as_str() {
                                                    "afterSelection" => "Insert after selection",
                                                    "statusOnly" => "Status only",
                                                    _ => "New tab",
                                                };
                                                egui::ComboBox::from_id_salt("preset_response_target").selected_text(target_label).show_ui(ui, |ui| {
                                                    for (v, l) in [("newTab", "New tab"), ("afterSelection", "Insert after selection"), ("statusOnly", "Status only")] {
                                                        ui.selectable_value(&mut preset.response_target, v.to_string(), l);
                                                    }
                                                });
                                                ui.end_row();

                                                ui.label("Timeout (seconds, 0 = no limit)");
                                                ui.add(egui::DragValue::new(&mut preset.timeout_secs).range(0..=3600));
                                                ui.end_row();
                                            });
                                            ui.add_space(8.0);
                                            ui.label("Prompt template");
                                            ui.add(egui::TextEdit::multiline(&mut preset.prompt_template).desired_width(f32::INFINITY));

                                            ui.add_space(8.0);
                                            ui.label("Body template (JSON Syntax Highlighted)");
                                            let font_size = self.font_config.clamped_size();
                                            let mut layouter = |ui: &egui::Ui, string: &dyn egui::TextBuffer, wrap_width: f32| {
                                                let mut job = self.highlighter.layout_job_json(string.as_str(), font_size);
                                                job.wrap.max_width = wrap_width;
                                                ui.fonts_mut(|f| f.layout_job(job))
                                            };
                                            ui.add(egui::TextEdit::multiline(&mut preset.body_template).desired_width(f32::INFINITY).layouter(&mut layouter));

                                            ui.add_space(8.0);
                                            ui.label("Response JSON path");
                                            let mut rjp = preset.response_json_path.clone().unwrap_or_default();
                                            if ui.add(egui::TextEdit::singleline(&mut rjp).desired_width(f32::INFINITY)).changed() {
                                                preset.response_json_path = if rjp.is_empty() { None } else { Some(rjp) };
                                            }

                                            ui.add_space(8.0);
                                            ui.label("Headers");
                                            let mut remove_h = None;
                                            for (hi, (hk, hv)) in preset.headers.iter_mut().enumerate() {
                                                ui.horizontal(|ui| {
                                                    ui.add(egui::TextEdit::singleline(hk).desired_width(160.0));
                                                    ui.add(egui::TextEdit::singleline(hv).desired_width(320.0));
                                                    if ui.button("x").clicked() {
                                                        remove_h = Some(hi);
                                                    }
                                                });
                                            }
                                            if let Some(hi) = remove_h {
                                                preset.headers.remove(hi);
                                            }
                                            if ui.button("+ Add Header").clicked() {
                                                preset.headers.push((String::new(), String::new()));
                                            }
                                        } else {
                                            ui.label("Select or add a preset.");
                                        }
                                    });
                                });
                            }
                            SettingsTab::Tokens => {
                                ui.label("API Tokens (Stored securely)");
                                ui.add_space(8.0);

                                let mut remove_idx = None;
                                for (ti, entry) in dialog.tokens.iter_mut().enumerate() {
                                    ui.horizontal(|ui| {
                                        ui.add(egui::TextEdit::singleline(&mut entry.key).hint_text("Key (e.g. OPENAI_KEY)").desired_width(200.0));
                                        if entry.masked {
                                            let display_masked = if entry.value.is_empty() { "(empty)" } else { "••••••••" };
                                            ui.label(display_masked);
                                            if ui.button("✏ Edit").clicked() {
                                                entry.masked = false;
                                            }
                                        } else {
                                            ui.add(egui::TextEdit::singleline(&mut entry.value).password(true).desired_width(320.0));
                                            if ui.button("🔒 Mask").clicked() {
                                                entry.masked = true;
                                            }
                                        }
                                        if ui.button("🗑").clicked() {
                                            remove_idx = Some(ti);
                                        }
                                    });
                                }
                                if let Some(ti) = remove_idx {
                                    dialog.tokens.remove(ti);
                                }

                                ui.separator();
                                ui.horizontal(|ui| {
                                    ui.add(egui::TextEdit::singleline(&mut dialog.new_token_key).hint_text("New Key").desired_width(160.0));
                                    ui.add(egui::TextEdit::singleline(&mut dialog.new_token_value).password(true).hint_text("Token Value / Paste").desired_width(280.0));
                                    if ui.button("+ Add Token").clicked() {
                                        if !dialog.new_token_key.trim().is_empty() {
                                            dialog.tokens.push(TokenEntry {
                                                key: dialog.new_token_key.trim().to_string(),
                                                value: dialog.new_token_value.trim().to_string(),
                                                masked: true,
                                            });
                                            dialog.new_token_key.clear();
                                            dialog.new_token_value.clear();
                                        }
                                    }
                                });
                            }
                        }
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("戻る").clicked() {
                        close_dialog = true;
                    }
                });
            });

        if !open {
            close_dialog = true;
        }

        if save {
            dialog.validation_error = None;
            for preset in &dialog.presets {
                if let Err(e) = validate_json_template(&preset.name, &preset.body_template, &dialog.tokens) {
                    dialog.validation_error = Some(e);
                    save = false;
                    break;
                }
            }
        }

        if save {
            let raw_tokens: HashMap<String, String> = dialog
                .tokens
                .into_iter()
                .filter(|t| !t.key.trim().is_empty())
                .map(|t| (t.key, t.value))
                .collect();
            let (resolved_tokens, config_tokens) = crate::secure_tokens::prepare_tokens_for_save(&raw_tokens);
            let config = ssh::AppConfig {
                presets: dialog.presets.clone(),
                tokens: config_tokens,
                ..ssh::load_app_config()
            };
            if let Err(e) = ssh::save_app_config(&config) {
                self.status = format!("Failed to save settings: {e}");
            } else {
                self.presets = dialog.presets;
                self.tokens = resolved_tokens;
                self.status = "Settings saved securely".to_string();
            }
            self.settings_dialog = None;
        } else if close_dialog {
            self.settings_dialog = None;
        } else {
            self.settings_dialog = Some(dialog);
        }
    }

    fn open_git_commit_dialog(&mut self, ctx: &egui::Context) {
        let Some(workspace_dir) = self.workspace.clone() else {
            self.status = "Open a local Git workspace first.".to_string();
            return;
        };
        if !self.git_available || !git::is_git_repo(&workspace_dir) {
            self.has_git_repo = git::is_git_repo(&workspace_dir);
            self.status = "Git repository (.git) or git command not available.".to_string();
            return;
        }

        let selected_text = if let Some(i) = self.active {
            let tab_id = self.tabs[i].id;
            let range = egui::TextEdit::load_state(ctx, editor_id(tab_id)).and_then(|s| s.cursor.char_range());
            match range {
                Some(r) => {
                    let a = r.primary.index.0.min(r.secondary.index.0);
                    let b = r.primary.index.0.max(r.secondary.index.0);
                    if a < b {
                        Some(self.tabs[i].content.chars().skip(a).take(b - a).collect::<String>())
                    } else {
                        None
                    }
                }
                None => None,
            }
        } else {
            None
        };

        let active_filename = self.active.and_then(|i| self.tabs.get(i)).map(|t| t.name.as_str());
        let now = chrono::Local::now().naive_local();
        let message = git::resolve_initial_commit_message(selected_text.as_deref(), active_filename, now);
        let uses_lfs = git::uses_git_lfs(&workspace_dir);
        let push_target = git::resolve_push_target(&workspace_dir);

        self.git_commit_dialog = Some(GitCommitDialogState {
            message,
            push: self.git_push_default,
            uses_lfs,
            push_target,
        });
        self.git_commit_focus_pending = true;
    }

    fn start_git_commit(&mut self, message: String, push: bool) {
        if self.git_pending.is_some() {
            return;
        }
        let Some(workspace_dir) = self.workspace.clone() else {
            return;
        };
        self.git_push_default = push;

        // Save any dirty tabs that already have an on-disk path before staging with `git add -A`
        if self.tabs.iter().any(|t| t.dirty && t.path.is_some()) {
            self.save_all();
        }

        self.status = if push {
            "Running git commit & push…".to_string()
        } else {
            "Running git commit…".to_string()
        };

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let res = git::commit_and_push(&workspace_dir, &message, push);
            let _ = tx.send(res);
        });
        self.git_pending = Some(rx);
    }

    fn open_send_palette(&mut self, ctx: &egui::Context) {
        if self.presets.is_empty() {
            self.status = "No API presets defined. Add one in Settings first.".to_string();
            return;
        }
        let (sel_start, sel_end, cursor_pos) = if let Some(i) = self.active {
            let tab_id = self.tabs[i].id;
            let range = egui::TextEdit::load_state(ctx, editor_id(tab_id)).and_then(|s| s.cursor.char_range());
            match range {
                Some(r) => {
                    let a = r.primary.index.0;
                    let b = r.secondary.index.0;
                    (a.min(b), a.max(b), a)
                }
                None => (0, 0, 0),
            }
        } else {
            (0, 0, 0)
        };

        let initial_selected = self.last_used_preset.unwrap_or(0).min(self.presets.len().saturating_sub(1));
        self.send_palette = Some(SendPaletteState {
            selected: initial_selected,
            sel_start,
            sel_end,
            cursor_pos,
        });
    }

    fn send_with_preset(&mut self, _ctx: &egui::Context, preset: api::ApiPreset) {
        let palette_state = self.send_palette.take();
        if self.send_pending.is_some() {
            return;
        }
        let Some(i) = self.active else { return };
        let tab_id = self.tabs[i].id;
        let content = self.tabs[i].content.clone();

        let (sel_start, sel_end, cursor_pos) = palette_state
            .map(|p| (p.sel_start, p.sel_end, p.cursor_pos))
            .unwrap_or((0, 0, 0));
        let selection_text = if sel_start < sel_end {
            content.chars().skip(sel_start).take(sel_end - sel_start).collect::<String>()
        } else {
            content.clone()
        };
        let insert_at_char = if sel_start < sel_end { sel_end } else { cursor_pos };

        let filename = strip_extension(&self.tabs[i].name).to_string();
        let session_id = api::extract_session_id(&selection_text);
        let prompt_ctx =
            api::RenderContext { selection: "", filename: &filename, tokens: &self.tokens, session_id: session_id.as_deref() };
        let rendered_prompt = if preset.prompt_template.is_empty() {
            String::new()
        } else {
            api::render_template(&preset.prompt_template, &prompt_ctx)
        };
        let combined_selection =
            if rendered_prompt.is_empty() { selection_text } else { format!("{rendered_prompt}\n\n{selection_text}") };
        let render_ctx = api::RenderContext {
            selection: &combined_selection,
            filename: &filename,
            tokens: &self.tokens,
            session_id: session_id.as_deref(),
        };
        let url = api::render_template(&preset.url, &render_ctx);
        let body =
            if preset.body_template.is_empty() { None } else { Some(api::render_template(&preset.body_template, &render_ctx)) };
        let headers: Vec<(String, String)> =
            preset.headers.iter().map(|(k, v)| (k.clone(), api::render_template(v, &render_ctx))).collect();

        self.status = format!("Sending to '{}'…", preset.name);
        let (tx, rx) = std::sync::mpsc::channel();
        let method = preset.method.clone();
        let response_target = preset.response_target.clone();
        let response_json_path = preset.response_json_path.clone();
        let session_id_path = preset.session_id_path.clone();
        let session_updated_path = preset.session_updated_path.clone();
        let timeout_secs = preset.timeout_secs;
        std::thread::spawn(move || {
            let result = api::send_request(api::SendRequestInput { url, method, headers, body, timeout_secs });
            let _ = tx.send(SendOutcome {
                response_target,
                response_json_path,
                session_id_path,
                session_updated_path,
                target_tab_id: tab_id,
                insert_at_char,
                result,
            });
        });
        self.send_pending = Some(rx);
    }

    /// Applies a completed send's result - matches the original's exact
    /// branching: a non-2xx status always opens a new tab with the raw
    /// error body (regardless of `response_target`) and returns early;
    /// `"statusOnly"` deliberately falls through doing nothing besides
    /// the trailing status message, which runs unconditionally for every
    /// target on success.
    fn apply_send_outcome(&mut self, outcome: SendOutcome) {
        let response = match outcome.result {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("Send failed: {e}");
                return;
            }
        };
        if !(200..300).contains(&response.status) {
            self.open_untitled_with_content(response.body);
            self.status = format!("Send failed: HTTP {} (see new tab for response body)", response.status);
            return;
        }

        let extracted = outcome
            .response_json_path
            .as_deref()
            .and_then(|p| api::extract_json_path(&response.body, p))
            .unwrap_or_else(|| response.body.clone());
        let mut session_lines = Vec::new();
        if let Some(p) = &outcome.session_id_path {
            if let Some(v) = api::extract_json_path(&response.body, p) {
                session_lines.push(format!("SessionID: {v}"));
            }
        }
        if let Some(p) = &outcome.session_updated_path {
            if let Some(v) = api::extract_json_path(&response.body, p) {
                session_lines.push(format!("session_updated: {v}"));
            }
        }
        let extracted_with_session = if session_lines.is_empty() {
            extracted
        } else {
            let sep = if extracted.ends_with('\n') { "" } else { "\n" };
            format!("{extracted}{sep}{}\n", session_lines.join("\n"))
        };

        let target_norm = outcome.response_target.trim().to_lowercase();
        match target_norm.as_str() {
            "afterselection" | "after_selection" | "insertafterselection" => {
                let target_tab = self
                    .tabs
                    .iter()
                    .position(|t| t.id == outcome.target_tab_id)
                    .or(self.active);
                if let Some(idx) = target_tab {
                    let chars: Vec<char> = self.tabs[idx].content.chars().collect();
                    let at = outcome.insert_at_char.min(chars.len());
                    let needs_nl_before = at > 0 && chars[at - 1] != '\n';
                    let needs_nl_after = !extracted_with_session.ends_with('\n');
                    let mut payload = String::new();
                    if needs_nl_before {
                        payload.push('\n');
                    }
                    payload.push_str(&extracted_with_session);
                    if needs_nl_after {
                        payload.push('\n');
                    }
                    let new_content: String =
                        chars[..at].iter().collect::<String>() + &payload + &chars[at..].iter().collect::<String>();
                    self.tabs[idx].content = new_content;
                    self.tabs[idx].dirty = true;
                    self.active = Some(idx);
                } else {
                    // Fall back to a new tab if no open tab exists
                    self.open_untitled_with_content(extracted_with_session);
                }
            }
            "statusonly" | "status_only" => {}
            _ => {
                self.open_untitled_with_content(extracted_with_session);
            }
        }
        self.status = format!("Send OK ({})", response.status);
    }

    fn open_untitled_with_content(&mut self, content: String) {
        let id = self.alloc_tab_id();
        let mut tab = Tab::untitled(id);
        tab.content = content;
        self.tabs.push(tab);
        self.active = Some(self.tabs.len() - 1);
        self.focus_pending = true;
    }

    /// Renames `old_path` in place (same parent dir, new leaf name) and
    /// follows the rename into any open tab - the renamed node itself by
    /// exact path match, and everything nested under a renamed *folder*
    /// by path-prefix rewrite - so open buffers don't silently point at
    /// a path that no longer exists.
    fn rename_node(&mut self, old_path: &TabPath, new_name: &str) {
        let to = old_path.parent().join(new_name);
        match self.fs_rename(old_path, &to) {
            Ok(()) => {
                for tab in &mut self.tabs {
                    let Some(p) = tab.path.clone() else { continue };
                    if &p == old_path {
                        tab.name = to.file_name();
                        tab.path = Some(to.clone());
                        tab.title_buffer = tab.name.strip_suffix(".md").unwrap_or(&tab.name).to_string();
                    } else if let Some(rest) = p.strip_prefix_dir(old_path) {
                        let new_nested = to.join(&rest);
                        tab.name = new_nested.file_name();
                        tab.path = Some(new_nested);
                        tab.title_buffer = tab.name.strip_suffix(".md").unwrap_or(&tab.name).to_string();
                    }
                }
                self.status = format!("Renamed to {}", to.display_string());
                if let (TabPath::Ssh(from_rel), TabPath::Ssh(to_rel)) = (old_path, &to) {
                    if ssh::rename_tree_node(&mut self.tree, from_rel, to_rel) {
                        self.rebuild_wiki_target_set();
                    } else {
                        self.refresh_tree();
                    }
                } else {
                    self.refresh_tree();
                }
            }
            Err(e) => self.status = format!("Rename failed: {e}"),
        }
    }

    /// Tabs pointing at the trashed path are deliberately left open
    /// (matches the original React app's VS Code-like behavior) -
    /// saving one just recreates the file at that path.
    fn trash_node(&mut self, path: &TabPath, name: &str) {
        match self.fs_trash(path) {
            Ok(()) => {
                self.status = format!("Moved {name} to trash");
                if let TabPath::Ssh(rel) = path {
                    ssh::remove_tree_node(&mut self.tree, rel);
                    self.rebuild_wiki_target_set();
                } else {
                    self.refresh_tree();
                }
            }
            Err(e) => self.status = format!("Delete failed: {e}"),
        }
    }

    fn new_file(&mut self) {
        let id = self.alloc_tab_id();
        self.tabs.push(Tab::untitled(id));
        self.active = Some(self.tabs.len() - 1);
        self.focus_pending = true;
    }

    fn open_external_file(&mut self, path: &TabPath) {
        if let Some((root, _)) = self.workspace_root() {
            if !path.is_within_root(&root) {
                self.status = "Security alert: Attempted to open file outside workspace boundary.".to_string();
                return;
            }
        }
        match path {
            TabPath::Local(p) => {
                if let Err(e) = open_in_external_app(p) {
                    self.status = format!("Failed to open file externally: {e}");
                } else {
                    self.status = format!("Opened: {}", path.file_name());
                }
            }
            TabPath::Ssh(rel) => {
                self.status = format!("SSH external viewing: path is remote ({rel})");
            }
        }
    }

    fn find_all_pdfs_in_tab(&self, tab_idx: usize) -> Vec<TabPath> {
        let tab = &self.tabs[tab_idx];
        if let Some(path) = &tab.path {
            if is_pdf_path(path) {
                return vec![path.clone()];
            }
        }
        let content = &tab.content;
        let dir = tab
            .path
            .as_ref()
            .map(|p| p.parent())
            .or_else(|| self.workspace_root().map(|(root, _)| root))
            .unwrap_or_else(|| TabPath::Local(PathBuf::new()));

        let mut results = Vec::new();
        if let Ok(re_cm) = regex::Regex::new(r#"(?:!\[|\[)([^\]]*)(?:\])\(([^)]+\.pdf)\)"#) {
            for caps in re_cm.captures_iter(content) {
                let target = caps[2].trim();
                let path = dir.join_normalized(target);
                if !results.contains(&path) {
                    results.push(path);
                }
            }
        }
        if let Ok(re_wiki) = regex::Regex::new(r#"(?:!\[\[|\[\[)([^\]|]+\.pdf)(?:\|[^\]]*)?\]\]"#) {
            for caps in re_wiki.captures_iter(content) {
                let target = caps.get(1).map(|m| m.as_str().trim()).unwrap_or_default();
                if !target.is_empty() {
                    let path = dir.join_normalized(target);
                    if !results.contains(&path) {
                        results.push(path);
                    }
                }
            }
        }
        results
    }

    fn find_all_videos_in_tab(&self, tab_idx: usize) -> Vec<TabPath> {
        let tab = &self.tabs[tab_idx];
        if let Some(path) = &tab.path {
            if is_video_path_tab(path) {
                return vec![path.clone()];
            }
        }
        let content = &tab.content;
        let dir = tab
            .path
            .as_ref()
            .map(|p| p.parent())
            .or_else(|| self.workspace_root().map(|(root, _)| root))
            .unwrap_or_else(|| TabPath::Local(PathBuf::new()));

        let mut results = Vec::new();
        if let Ok(re_cm) = regex::Regex::new(r#"(?:!\[|\[)([^\]]*)(?:\])\(([^)]+\.(?:mp4|mkv|avi|mov|webm|wmv|flv|m4v|3gp|ts))\)"#) {
            for caps in re_cm.captures_iter(content) {
                let target = caps[2].trim();
                let path = dir.join_normalized(target);
                if !results.contains(&path) {
                    results.push(path);
                }
            }
        }
        if let Ok(re_wiki) = regex::Regex::new(r#"(?:!\[\[|\[\[)([^\]|]+\.(?:mp4|mkv|avi|mov|webm|wmv|flv|m4v|3gp|ts))(?:\|[^\]]*)?\]\]"#) {
            for caps in re_wiki.captures_iter(content) {
                let target = caps.get(1).map(|m| m.as_str().trim()).unwrap_or_default();
                if !target.is_empty() {
                    let path = dir.join_normalized(target);
                    if !results.contains(&path) {
                        results.push(path);
                    }
                }
            }
        }
        results
    }

    pub(crate) fn reveal_path_in_tree(&mut self, ctx: &egui::Context, path: &TabPath) {
        self.left_pane = LeftPane::Files;
        self.selected_tree_nodes.clear();
        self.tree_scroll_pending = true;
        self.selected_tree_nodes.insert(path.display_string());
        if let TabPath::Local(p) = path {
            self.selected_tree_nodes.insert(p.to_string_lossy().to_string());
        } else if let TabPath::Ssh(s) = path {
            self.selected_tree_nodes.insert(s.clone());
        }

        let rel_str = match path {
            TabPath::Local(p) => {
                if let Some((root, _)) = self.workspace_root() {
                    path.strip_prefix_dir(&root).unwrap_or_else(|| p.to_string_lossy().to_string())
                } else {
                    p.to_string_lossy().to_string()
                }
            }
            TabPath::Ssh(rel) => rel.clone(),
        };
        let norm_rel = rel_str.replace('\\', "/");
        self.selected_tree_nodes.insert(norm_rel.clone());

        // 親ディレクトリをすべて展開
        let mut curr = path.clone();
        loop {
            let parent = curr.parent();
            if parent == curr {
                break;
            }
            if let Some((root, _)) = self.workspace_root() {
                if parent == root {
                    break;
                }
            }
            let id = egui::Id::new(&parent.display_string());
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ctx, id, false);
            state.set_open(true);
            state.store(ctx);

            if let TabPath::Local(p) = &parent {
                let id2 = egui::Id::new(&p.to_string_lossy().to_string());
                let mut state2 = egui::collapsing_header::CollapsingState::load_with_default_open(ctx, id2, false);
                state2.set_open(true);
                state2.store(ctx);
            }
            curr = parent;
        }

        // 相対パスの階層も展開
        let parts: Vec<&str> = norm_rel.split('/').collect();
        let mut current = String::new();
        for i in 0..parts.len().saturating_sub(1) {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(parts[i]);
            let id = egui::Id::new(&current);
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ctx, id, false);
            state.set_open(true);
            state.store(ctx);
        }
    }

    pub(crate) fn open_file(&mut self, path: TabPath) {
        self.open_file_with_ctx(None, path);
    }

    pub(crate) fn open_file_with_ctx(&mut self, ctx: Option<&egui::Context>, path: TabPath) {
        if let Some(ctx) = ctx {
            self.reveal_path_in_tree(ctx, &path);
        }
        if is_pdf_path(&path) || is_video_path_tab(&path) {
            self.open_external_file(&path);
            return;
        }
        if let Some(i) = self.tabs.iter().position(|t| t.path.as_ref() == Some(&path)) {
            self.active = Some(i);
            self.focus_pending = true;
            return;
        }
        match self.fs_read(&path) {
            Ok(content) => {
                let id = self.alloc_tab_id();
                self.tabs.push(Tab::from_file(id, path, content));
                self.active = Some(self.tabs.len() - 1);
                self.focus_pending = true;
            }
            Err(e) => self.status = format!("Failed to open: {e}"),
        }
    }

    pub(crate) fn ensure_and_open_homepage(&mut self) {
        let Some((root, _label)) = self.workspace_root() else { return };
        let is_ssh = self.ssh_profile.is_some();

        let all_paths = if is_ssh {
            self.ssh_root_template_paths()
        } else {
            workspace::list_all_paths(&root.display_string())
        };

        // template_index.md のみを展開対象とし、存在しない場合は何もしない
        // ワークスペース直下の Templates/ のみが対象（サブフォルダ内の Templates/ は無視）
        let template_root = if is_ssh { None } else { Some(root.display_string()) };
        let Some(template_path_str) = templates::find_index_template(&all_paths, template_root.as_deref()) else {
            return;
        };

        let index_path = root.join_normalized("index.md");
        let now = chrono::Local::now().naive_local();

        // index.md が既に存在し、作成日（または上書き更新日）が当日であれば何もせずそのまま開く
        if let TabPath::Local(p) = &index_path {
            if let Ok(meta) = std::fs::metadata(p) {
                let is_today = meta
                    .created()
                    .or_else(|_| meta.modified())
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.date_naive() == now.date()
                    })
                    .unwrap_or(false)
                    || meta
                        .modified()
                        .map(|t| {
                            let dt: chrono::DateTime<chrono::Local> = t.into();
                            dt.date_naive() == now.date()
                        })
                        .unwrap_or(false);
                if is_today {
                    self.open_file(index_path);
                    return;
                }
            }
        }

        // SSH: same "already generated today -> just open it" rule, using
        // the remote file's mtime (otherwise every connect would overwrite
        // a remote index.md the user had edited).
        if let (TabPath::Ssh(rel), Some(profile)) = (&index_path, self.ssh_profile.as_ref()) {
            if let Some(secs) = ssh::file_mtime_secs(&self.ssh_command_path, profile, rel) {
                let dt = chrono::DateTime::from_timestamp(secs, 0).map(|d| d.with_timezone(&chrono::Local));
                if dt.map(|d| d.date_naive() == now.date()).unwrap_or(false) {
                    self.open_file(index_path);
                    return;
                }
            }
        }

        let tp = if is_ssh {
            TabPath::Ssh(template_path_str)
        } else {
            TabPath::Local(PathBuf::from(template_path_str))
        };
        let Ok(template_content) = self.fs_read(&tp) else {
            return;
        };

        let due_tasks = self.scan_due_tasks(now.date()).unwrap_or_default();
        let expanded = crate::templates::expand_with_tasks(&template_content, now, &due_tasks);

        let _ = self.fs_write(&index_path, &expanded);
        if !is_ssh {
            self.refresh_tree();
        }
        self.open_file(index_path);
    }

    fn save_active(&mut self) {
        let Some(i) = self.active else { return };
        if self.tabs[i].path.is_none() {
            self.save_as_active();
            return;
        }
        let path = self.tabs[i].path.clone().unwrap();
        let content = self.tabs[i].content.clone();
        match self.fs_write(&path, &content) {
            Ok(()) => {
                self.tabs[i].dirty = false;
                self.status = format!("Saved {}", path.display_string());
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn save_tab_index(&mut self, i: usize) -> bool {
        if i >= self.tabs.len() {
            return false;
        }
        if self.tabs[i].path.is_none() {
            self.active = Some(i);
            self.save_as_active();
            return false;
        }
        let path = self.tabs[i].path.clone().unwrap();
        let content = self.tabs[i].content.clone();
        match self.fs_write(&path, &content) {
            Ok(()) => {
                self.tabs[i].dirty = false;
                self.status = format!("Saved {}", path.display_string());
                true
            }
            Err(e) => {
                self.status = format!("Save failed: {e}");
                false
            }
        }
    }

    fn save_all_dirty_tabs(&mut self) -> bool {
        let mut all_saved = true;
        for i in 0..self.tabs.len() {
            if self.tabs[i].dirty {
                if !self.save_tab_index(i) {
                    all_saved = false;
                }
            }
        }
        all_saved
    }

    /// Local branch uses the native "Save file" dialog; an SSH
    /// workspace has no local-filesystem save target for that dialog to
    /// pick, so it reuses the `name_prompt` modal to ask for a
    /// workspace-relative path instead (see `NamePromptAction::SaveAs`
    /// and the original React app's `window.prompt` equivalent).
    fn save_as_active(&mut self) {
        let Some(i) = self.active else { return };
        if self.ssh_profile.is_some() {
            let initial = self.tabs[i].path.as_ref().map(|p| p.display_string()).unwrap_or_default();
            self.name_prompt = Some(NamePrompt {
                title: "Save as (path relative to the remote workspace)".to_string(),
                value: initial,
                action: NamePromptAction::SaveAs { tab_index: i },
            });
            self.name_prompt_focus_pending = true;
            return;
        }
        let Some(path) = rfd::FileDialog::new().save_file() else {
            return;
        };
        let tab_path = TabPath::Local(path);
        let content = self.tabs[i].content.clone();
        match self.fs_write(&tab_path, &content) {
            Ok(()) => {
                let tab = &mut self.tabs[i];
                tab.dirty = false;
                tab.name = tab_path.file_name();
                tab.title_buffer = tab.name.strip_suffix(".md").unwrap_or(&tab.name).to_string();
                self.status = format!("Saved {}", tab_path.display_string());
                tab.path = Some(tab_path);
                self.refresh_tree();
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn save_as_ssh(&mut self, tab_index: usize, rel_path: &str) {
        let Some(tab) = self.tabs.get(tab_index) else { return };
        let tab_path = TabPath::Ssh(rel_path.trim_start_matches('/').to_string());
        let content = tab.content.clone();
        match self.fs_write(&tab_path, &content) {
            Ok(()) => {
                let tab = &mut self.tabs[tab_index];
                tab.dirty = false;
                tab.name = tab_path.file_name();
                tab.title_buffer = tab.name.strip_suffix(".md").unwrap_or(&tab.name).to_string();
                self.status = format!("Saved {} (SSH)", tab_path.display_string());
                tab.path = Some(tab_path);
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn request_close(&mut self, index: usize) {
        if self.tabs[index].dirty {
            self.pending_close = Some(index);
        } else {
            self.close_tab_now(index);
        }
    }

    fn close_tab_now(&mut self, index: usize) {
        self.tabs.remove(index);
        self.active = match self.active {
            Some(a) if a == index => {
                if self.tabs.is_empty() {
                    None
                } else {
                    Some(a.min(self.tabs.len() - 1))
                }
            }
            Some(a) if a > index => Some(a - 1),
            other => other,
        };
        self.sync_tab_id_lists();
    }

    /// Saves every dirty tab that already has a path. Untitled dirty
    /// tabs are left alone rather than popping a save-file dialog per
    /// tab - "Save All" batching several native dialogs in a row would
    /// be a worse experience than just asking the user to name them
    /// individually via "Save As" first.
    fn save_all(&mut self) {
        let to_save: Vec<(usize, TabPath, String)> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| t.dirty)
            .filter_map(|(i, t)| t.path.clone().map(|p| (i, p, t.content.clone())))
            .collect();
        let mut saved = 0;
        let mut failed = 0;
        for (i, path, content) in to_save {
            match self.fs_write(&path, &content) {
                Ok(()) => {
                    self.tabs[i].dirty = false;
                    saved += 1;
                }
                Err(_) => failed += 1,
            }
        }
        self.status = if failed == 0 {
            format!("Saved {saved} file(s)")
        } else {
            format!("Saved {saved} file(s), {failed} failed")
        };
    }

    /// Quit via Ctrl+Q/Cmd+Q - unlike the native window close button
    /// (handled separately in `ui()` since it arrives as a viewport
    /// close-request event, not a shortcut), this can send `Close`
    /// directly and only needs the same dirty-tab guard first.
    fn request_quit(&mut self, ctx: &egui::Context) {
        if self.tabs.iter().any(|t| t.dirty) {
            self.pending_quit = true;
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// One aggregate confirmation for every dirty tab, rather than
    /// chaining `request_close`'s per-tab dialog one at a time.
    fn request_close_all(&mut self) {
        if self.tabs.iter().any(|t| t.dirty) {
            self.pending_close_all = true;
        } else {
            self.tabs.clear();
            self.active = None;
            self.sync_tab_id_lists();
        }
    }

    fn request_close_others(&mut self, keep_index: usize) {
        if keep_index >= self.tabs.len() {
            return;
        }
        if self.tabs.iter().enumerate().any(|(i, t)| i != keep_index && t.dirty) {
            self.pending_close_all = true;
            return;
        }
        let keep_tab = self.tabs.remove(keep_index);
        self.tabs = vec![keep_tab];
        self.active = Some(0);
        self.sync_tab_id_lists();
    }

    fn request_close_right(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.iter().enumerate().any(|(i, t)| i >= index && t.dirty) {
            self.pending_close_all = true;
            return;
        }
        self.tabs.truncate(index);
        if let Some(active) = self.active {
            if self.tabs.is_empty() {
                self.active = None;
            } else if active >= index {
                self.active = Some(index - 1);
            }
        }
        self.sync_tab_id_lists();
    }

    fn request_close_left(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.iter().enumerate().any(|(i, t)| i <= index && t.dirty) {
            self.pending_close_all = true;
            return;
        }
        let active_id = self.active.map(|a| self.tabs[a].id);
        self.tabs.drain(0..=index);
        self.active = active_id.and_then(|id| self.tabs.iter().position(|t| t.id == id)).or_else(|| if self.tabs.is_empty() { None } else { Some(0) });
        self.sync_tab_id_lists();
    }

    /// Shared by the Ctrl+Tab/Ctrl+Shift+Tab shortcuts and the View menu
    /// items - `delta` is `1` for next, `-1` for previous, wrapping
    /// around either end of the tab strip.
    fn cycle_tab(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        let current = self.active.unwrap_or(0) as isize;
        self.active = Some((((current + delta) % n + n) % n) as usize);
        self.focus_pending = true;
    }

    /// Keeps `selected_tab_ids` and `split_tab_ids` consistent with `self.tabs`.
    /// If fewer than 2 tabs remain in `split_tab_ids`, exits split mode automatically.
    fn sync_tab_id_lists(&mut self) {
        self.selected_tab_ids.retain(|id| self.tabs.iter().any(|t| t.id == *id));
        self.split_tab_ids.retain(|id| self.tabs.iter().any(|t| t.id == *id));
        if self.split_tab_ids.len() < 2 {
            self.split_tab_ids.clear();
        }
    }

    /// Toggles multi-selection of `tabs[index]` (Ctrl+Click / Cmd+Click).
    /// When starting a multi-selection from an empty selection, automatically includes
    /// the previously active tab so Ctrl+clicking a second tab selects both tabs.
    fn toggle_select_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.sync_tab_id_lists();
        if self.selected_tab_ids.is_empty() {
            if let Some(active_idx) = self.active {
                if active_idx < self.tabs.len() && active_idx != index {
                    self.selected_tab_ids.push(self.tabs[active_idx].id);
                }
            }
        }
        let id = self.tabs[index].id;
        if let Some(pos) = self.selected_tab_ids.iter().position(|&x| x == id) {
            self.selected_tab_ids.remove(pos);
        } else {
            self.selected_tab_ids.push(id);
        }
        self.active = Some(index);
        self.focus_pending = true;
    }

    /// Selects a contiguous range of tabs from `self.active` to `index` (Shift+Click).
    fn range_select_tabs(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let anchor = self.active.unwrap_or(index).min(self.tabs.len().saturating_sub(1));
        let (start, end) = (anchor.min(index), anchor.max(index));
        self.selected_tab_ids = self.tabs[start..=end].iter().map(|t| t.id).collect();
        self.active = Some(index);
        self.focus_pending = true;
    }

    /// Tiles all currently multi-selected tabs (`selected_tab_ids`) side-by-side.
    fn tile_selected_tabs(&mut self) {
        self.sync_tab_id_lists();
        if self.selected_tab_ids.len() >= 2 {
            self.split_tab_ids = self.selected_tab_ids.clone();
            self.status = format!("Tiled {} tabs side-by-side.", self.split_tab_ids.len());
        }
    }

    /// Tiles the currently active tab and `tabs[index]` side-by-side.
    fn tile_with_active(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let other_id = self.tabs[index].id;
        if let Some(active_idx) = self.active {
            if active_idx < self.tabs.len() && active_idx != index {
                let active_id = self.tabs[active_idx].id;
                self.split_tab_ids = vec![active_id, other_id];
                self.selected_tab_ids = self.split_tab_ids.clone();
                self.status = "Tiled 2 tabs side-by-side.".to_string();
            }
        }
    }

    /// Tiles all currently open tabs side-by-side in tab bar order.
    fn tile_all_tabs(&mut self) {
        if self.tabs.len() >= 2 {
            self.split_tab_ids = self.tabs.iter().map(|t| t.id).collect();
            self.selected_tab_ids = self.split_tab_ids.clone();
            self.status = format!("Tiled {} tabs side-by-side.", self.split_tab_ids.len());
        }
    }

    /// Appends `tabs[index]` as a new column in the current side-by-side split view.
    fn add_tab_to_split(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let id = self.tabs[index].id;
        if !self.split_tab_ids.contains(&id) {
            self.split_tab_ids.push(id);
            if !self.selected_tab_ids.contains(&id) {
                self.selected_tab_ids.push(id);
            }
            self.status = format!("Tiled {} tabs side-by-side.", self.split_tab_ids.len());
        }
    }

    /// Removes `tabs[index]` from the side-by-side split view without closing the file.
    fn remove_tab_from_split(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let id = self.tabs[index].id;
        self.split_tab_ids.retain(|&x| x != id);
        self.selected_tab_ids.retain(|&x| x != id);
        if self.split_tab_ids.len() < 2 {
            self.split_tab_ids.clear();
            self.status = "Exited split view.".to_string();
        }
    }

    /// Exits side-by-side split view and returns to single-tab editor mode.
    fn untile_tabs(&mut self) {
        self.split_tab_ids.clear();
        self.selected_tab_ids.clear();
        self.status = "Exited split view.".to_string();
    }

    fn fs_read_bytes(&self, path: &TabPath) -> Result<Vec<u8>, String> {
        match path {
            TabPath::Local(p) => std::fs::read(p).map_err(|e| e.to_string()),
            TabPath::Ssh(rel) => {
                let profile = self.ssh_profile.as_ref().ok_or("No SSH workspace open")?;
                ssh::read_file_guarded(&self.ssh_command_path, profile, rel)
            }
        }
    }

    fn open_image_viewer(&mut self, images: Vec<(String, Vec<u8>)>) {
        if images.is_empty() {
            self.status = "No images found to view.".to_string();
            return;
        }
        self.image_viewer = Some(ImageViewerState {
            images,
            current_index: 0,
            zoom: 1.0,
        });
    }

    fn find_all_images_in_tab(&self, tab_idx: usize) -> Vec<(String, Vec<u8>)> {
        let tab = &self.tabs[tab_idx];
        
        if let Some(path) = &tab.path {
            if attachments::is_image_path(std::path::Path::new(&path.file_name())) {
                if let Ok(bytes) = self.fs_read_bytes(path) {
                    return vec![(path.file_name(), bytes)];
                }
            }
        }

        let content = tab.content.clone();
        let dir = tab
            .path
            .as_ref()
            .map(|p| p.parent())
            .or_else(|| self.workspace_root().map(|(root, _)| root))
            .unwrap_or_else(|| TabPath::Local(PathBuf::new()));

        let mut raw_targets = Vec::new();

        if let Ok(re_cm) = regex::Regex::new(r#"!\[([^\]]*)\]\(([^)]+)\)"#) {
            for caps in re_cm.captures_iter(&content) {
                let target = caps[2].trim().trim_matches('"').trim_matches('\'');
                if !images::has_scheme(target) {
                    raw_targets.push(target.to_string());
                }
            }
        }

        if let Ok(re_wiki) = regex::Regex::new(r#"!\[\[([^\]|]+)(?:\|[^\]]*)?\]\]"#) {
            for caps in re_wiki.captures_iter(&content) {
                let target = caps[1].trim();
                if !images::has_scheme(target) {
                    raw_targets.push(target.to_string());
                }
            }
        }

        let mut results = Vec::new();
        for target in raw_targets {
            let resolved_path = dir.join_normalized(&target);
            if let Ok(bytes) = self.fs_read_bytes(&resolved_path) {
                results.push((resolved_path.file_name(), bytes));
            } else if let Some((root, _)) = self.workspace_root() {
                let att_path = root.join("attachments").join(&target);
                if let Ok(bytes) = self.fs_read_bytes(&att_path) {
                    results.push((att_path.file_name(), bytes));
                }
            }
        }

        results
    }

    fn show_image_viewer_dialog(&mut self, ctx: &egui::Context) {
        let Some(viewer) = &mut self.image_viewer else { return };
        if viewer.images.is_empty() {
            self.image_viewer = None;
            return;
        }
        let mut open = true;
        let mut status_msg = None;
        let total = viewer.images.len();
        let current_idx = viewer.current_index.min(total - 1);
        let (title, image_bytes) = &viewer.images[current_idx];

        let window_title = if total > 1 {
            format!("Image Viewer - {} ({}/{})", title, current_idx + 1, total)
        } else {
            format!("Image Viewer - {}", title)
        };

        // Handle keyboard shortcuts (Left/Right arrow to navigate, Escape to close)
        if total > 1 {
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) && viewer.current_index > 0 {
                viewer.current_index -= 1;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) && viewer.current_index + 1 < total {
                viewer.current_index += 1;
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }

        egui::Window::new(&window_title)
            .id(egui::Id::new("image_viewer_dialog"))
            .open(&mut open)
            .resizable(true)
            .default_size([700.0, 600.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if total > 1 {
                        if (ui.button("◀ Prev (←)").clicked()) && viewer.current_index > 0 {
                            viewer.current_index -= 1;
                        }
                        ui.label(format!("{}/{}", viewer.current_index + 1, total));
                        if (ui.button("Next ▶ (→)").clicked()) && viewer.current_index + 1 < total {
                            viewer.current_index += 1;
                        }
                        ui.separator();
                    }

                    if ui.button("📋 Copy Image to Clipboard").clicked() {
                        match images::copy_image_bytes_to_clipboard(image_bytes) {
                            Ok(()) => status_msg = Some("Copied image to clipboard.".to_string()),
                            Err(e) => status_msg = Some(format!("Failed to copy image: {e}")),
                        }
                    }
                    ui.separator();
                    if ui.button("➖ Zoom Out").clicked() {
                        viewer.zoom = (viewer.zoom - 0.2).max(0.2);
                    }
                    ui.label(format!("{:.0}%", viewer.zoom * 100.0));
                    if ui.button("➕ Zoom In").clicked() {
                        viewer.zoom = (viewer.zoom + 0.2).min(5.0);
                    }
                    if ui.button("↺ Reset").clicked() {
                        viewer.zoom = 1.0;
                    }
                });
                ui.separator();

                egui::ScrollArea::both().show(ui, |ui| {
                    let image_source = egui::ImageSource::Bytes {
                        uri: format!("bytes://{}", title).into(),
                        bytes: image_bytes.clone().into(),
                    };
                    let img = egui::Image::new(image_source)
                        .fit_to_original_size(viewer.zoom);
                    ui.add(img);
                });
            });

        if let Some(msg) = status_msg {
            self.status = msg;
        }

        if !open {
            self.image_viewer = None;
        }
    }
}

impl eframe::App for KotoshelfApp {
    /// A live agent (and the local `ssh` child keeping it alive) would
    /// otherwise outlive the app as an orphan process - Windows doesn't
    /// kill child processes when their parent exits, and relying on
    /// `AgentSession::drop` alone isn't guaranteed to run on every exit
    /// path.
    fn on_exit(&mut self) {
        ssh::agent_disconnect(&mut self.ssh_agent);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(windows)]
        {
            let focused = ui.ctx().input(|i| i.focused);
            let text_focus = ui.ctx().egui_wants_keyboard_input();
            self.ime_keeper.update(frame, focused, text_focus);
        }
        #[cfg(not(windows))]
        let _ = frame;
        let title = self.desired_window_title();
        if title != self.last_window_title {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_window_title = title;
        }

        if let Some(zoom) = self.pending_zoom_factor.take() {
            ui.ctx().set_zoom_factor(zoom);
            self.last_zoom_factor = zoom;
        }
        if self.pending_font_apply {
            crate::fonts::apply(ui.ctx(), &self.font_config);
            self.pending_font_apply = false;
        }
        let current_zoom = ui.ctx().zoom_factor();
        if (current_zoom - self.last_zoom_factor).abs() > 1e-4 {
            self.last_zoom_factor = current_zoom;
            self.save_current_workspace_settings(current_zoom);
        }

        // Resolved fresh every frame (not cached) so a `System` selection
        // picks up an OS-level light/dark flip live, the same way the
        // original's `matchMedia` change listener does - diffed against
        // egui's own current `dark_mode` (rather than a separate tracked
        // field) so this is a no-op every frame except the one where it
        // actually changes.
        let dark = crate::theme::resolve_dark(self.theme_selection);
        if ui.style().visuals.dark_mode != dark {
            ui.ctx().set_visuals(if dark { egui::Visuals::dark() } else { egui::Visuals::light() });
            self.highlighter.set_dark(dark);
        }

        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for path in dropped {
            self.handle_dropped_path(ui.ctx(), path);
        }

        // NOT bound to Ctrl+V: confirmed live that egui-winit intercepts
        // Ctrl+V at the OS-integration layer before it ever becomes an
        // observable `egui::Event` - `is_paste_command` (egui-winit's
        // internal check) matches on `modifiers.command && key == V`
        // alone, unconditionally swallowing the key press and, if the
        // clipboard holds no *text*, produces no event at all (no
        // `Event::Paste`, no `Event::Key{V}`, nothing) for this app to
        // react to. Unlike the original's browser/webview `paste` event
        // (which exposes `clipboardData.items` for any MIME type), egui
        // has no image-paste hook at all - so this uses its own
        // unclaimed shortcut instead, driven from a menu item too.
        let paste_image_shortcut =
            ui.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::I));
        if paste_image_shortcut && self.active.is_some() {
            if let Some(png_bytes) = clipboard_image_png_bytes() {
                self.attach_image_bytes(ui.ctx(), "pasted-image.png", &png_bytes);
            } else {
                self.status = "No image on the clipboard.".to_string();
            }
        }

        // Obsidian Tasks-style input assist: pressing Space on a
        // recognized task line (`- [ ] ...`/`* [ ]`/`+ [ ]`) opens a
        // picker for priority/recurring/on-completion markers - a scoped-
        // down version of Obsidian's own auto-suggest (which re-filters
        // on every keystroke; this instead opens once per Space press and
        // closes on pick, per the user's explicit call given how much
        // more involved a live-filtering popup would be to build on
        // egui's `TextEdit`, which has no built-in autocomplete concept).
        if self.task_assist.is_none() && ui.input(|i| i.key_pressed(egui::Key::Space)) {
            if let Some(i) = self.active {
                let tab_id = self.tabs[i].id;
                if let Some(line) = current_line_at_cursor(ui.ctx(), editor_id(tab_id), &self.tabs[i].content) {
                    // Requires an actual title, not just the bare
                    // checkbox - otherwise the very first space typed
                    // after `- [ ] ` (before any title text) pops this
                    // up immediately, which is more annoying than
                    // helpful per the user's own live-testing feedback.
                    if is_task_line(&line) && has_task_title(&line) {
                        let groups = build_task_assist_groups(&self.tabs[i].content);
                        self.task_assist = Some(TaskAssistState { highlight: 0, just_opened: true, groups });
                    }
                }
            }
        }

        if let Some(rx) = &self.ssh_connecting {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.ssh_agent = outcome.agent;
                    match outcome.tree {
                        Ok(tree) => {
                            self.tree = agent_protocol::filter_tree(tree);
                            self.rebuild_wiki_target_set();
                            if let Some(profile) = &self.ssh_profile {
                                let note = if profile.disable_agent {
                                    "agent disabled"
                                } else if outcome.agent_connected {
                                    "agent connected"
                                } else {
                                    "agent unavailable, using shell session"
                                };
                                self.status = format!("Workspace (SSH): {} ({note})", ssh_display_target(profile));
                            }
                            self.ensure_and_open_homepage();
                        }
                        Err(e) => self.status = format!("Failed to read remote workspace: {e}"),
                    }
                    self.ssh_connecting = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still connecting - repaint every frame so the result
                    // is picked up as soon as it lands, not just on the
                    // next input event (egui doesn't repaint on its own).
                    ui.ctx().request_repaint();
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.status = "Connection attempt failed unexpectedly".to_string();
                    self.ssh_connecting = None;
                }
            }
        }

        if let Some(rx) = &self.send_pending {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.send_pending = None;
                    self.apply_send_outcome(outcome);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ui.ctx().request_repaint();
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.send_pending = None;
                    self.status = "Send failed unexpectedly".to_string();
                }
            }
        }

        if let Some(rx) = &self.git_pending {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.git_pending = None;
                    match outcome {
                        Ok(msg) => self.status = msg,
                        Err(err) => self.status = err,
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ui.ctx().request_repaint();
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.git_pending = None;
                    self.status = "Git operation failed unexpectedly".to_string();
                }
            }
        }

        let f2_shortcut = ui.input(|i| i.key_pressed(egui::Key::F2));
        if f2_shortcut {
            if let Some(i) = self.active {
                if let Some(path) = &self.tabs[i].path {
                    self.name_prompt = Some(NamePrompt {
                        title: format!("Rename {}", path.file_name()),
                        value: path.file_name(),
                        action: NamePromptAction::Rename { old_path: path.clone() },
                    });
                    self.name_prompt_focus_pending = true;
                }
            }
        }
        let f5_shortcut = ui.input(|i| i.key_pressed(egui::Key::F5));
        if f5_shortcut {
            self.refresh_tree();
            self.status = "Workspace refreshed.".to_string();
        }

        let save_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S));
        if save_shortcut {
            self.save_active();
        }
        let new_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::N));
        if new_shortcut {
            self.new_file();
        }
        let close_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W));
        if close_shortcut {
            if let Some(i) = self.active {
                self.request_close(i);
            }
        }
        let toggle_preview_shortcut = ui.input(|i| {
            i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::V)
        });
        if toggle_preview_shortcut {
            self.preview_visible = !self.preview_visible;
            self.save_current_workspace_settings(ui.ctx().zoom_factor());
        }
        let toggle_wrap_shortcut = ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::Z));
        if toggle_wrap_shortcut {
            self.font_config.word_wrap = !self.font_config.word_wrap;
            let _ = crate::fonts::save_config(&self.font_config);
            self.save_current_workspace_settings(ui.ctx().zoom_factor());
            self.status = if self.font_config.word_wrap {
                "Word Wrap: ON".to_string()
            } else {
                "Word Wrap: OFF".to_string()
            };
        }
        let find_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F));
        if find_shortcut && self.active.is_some() {
            self.find.open = true;
            self.find.replace_mode = false;
            self.find_focus_pending = true;
        }
        let replace_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::H));
        if replace_shortcut && self.active.is_some() {
            self.find.open = true;
            self.find.replace_mode = true;
            self.find_focus_pending = true;
        }
        let quit_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Q));
        if quit_shortcut {
            self.request_quit(ui.ctx());
        }
        // The native window close button (or OS "Quit") raises this as a
        // `ViewportEvent::Close` on the root viewport rather than going
        // through `request_quit` above - caught here so it gets the same
        // unsaved-changes guard instead of closing unconditionally.
        let close_requested = ui.ctx().input(|i| i.viewport().close_requested());
        if close_requested && !self.quit_confirmed {
            if self.tabs.iter().any(|t| t.dirty) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.pending_quit = true;
            }
        }
        let send_palette_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Semicolon));
        if send_palette_shortcut && self.active.is_some() {
            self.open_send_palette(ui.ctx());
        }
        let search_in_files_shortcut =
            ui.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::F));
        if search_in_files_shortcut {
            self.left_pane = LeftPane::Search;
        }
        // `consume_key` (not `key_pressed`) - the multiline editor treats
        // *any* Tab press, Ctrl held or not, as "insert/remove
        // indentation" (egui's own `TextEdit` doesn't check for Ctrl on
        // that branch). Reading via `key_pressed` wouldn't stop the
        // editor from also seeing the same event later this frame and
        // inserting a tab character into whichever tab we just switched
        // to; `consume_key` removes it from the queue first. The
        // Ctrl+Shift+Tab check must run before the plain Ctrl+Tab one -
        // `consume_key` matches modifiers "logically" (extra Shift is
        // ignored), so checking Ctrl+Tab first would eat Ctrl+Shift+Tab
        // presses too.
        let prev_tab_shortcut = ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Tab)
        });
        let next_tab_shortcut =
            ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Tab));
        if prev_tab_shortcut {
            self.cycle_tab(-1);
        } else if next_tab_shortcut {
            self.cycle_tab(1);
        }

        if let Some(index) = self.pending_close {
            let mut save_and_close = false;
            let mut close_confirmed = false;
            let mut cancelled = false;
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("\"{}\" has unsaved changes. Save changes before closing?", self.tabs[index].name));
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            save_and_close = true;
                        }
                        if ui.button("Close without saving").clicked() {
                            close_confirmed = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                });
            if save_and_close {
                if self.save_tab_index(index) {
                    self.close_tab_now(index);
                }
                self.pending_close = None;
            } else if close_confirmed {
                self.close_tab_now(index);
                self.pending_close = None;
            } else if cancelled {
                self.pending_close = None;
            }
        }

        if self.pending_close_all {
            let dirty_count = self.tabs.iter().filter(|t| t.dirty).count();
            let mut save_and_close_all = false;
            let mut close_confirmed = false;
            let mut cancelled = false;
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("{dirty_count} tab(s) have unsaved changes. Save all changes before closing?"));
                    ui.horizontal(|ui| {
                        if ui.button("Save All").clicked() {
                            save_and_close_all = true;
                        }
                        if ui.button("Close all without saving").clicked() {
                            close_confirmed = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                });
            if save_and_close_all {
                if self.save_all_dirty_tabs() {
                    self.tabs.clear();
                    self.active = None;
                    self.sync_tab_id_lists();
                }
                self.pending_close_all = false;
            } else if close_confirmed {
                self.tabs.clear();
                self.active = None;
                self.sync_tab_id_lists();
                self.pending_close_all = false;
            } else if cancelled {
                self.pending_close_all = false;
            }
        }

        if self.pending_quit {
            let dirty_count = self.tabs.iter().filter(|t| t.dirty).count();
            let mut save_and_quit = false;
            let mut quit_confirmed = false;
            let mut cancelled = false;
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("{dirty_count} tab(s) have unsaved changes. Save changes before quitting?"));
                    ui.horizontal(|ui| {
                        if ui.button("Save & Quit").clicked() {
                            save_and_quit = true;
                        }
                        if ui.button("Quit without saving").clicked() {
                            quit_confirmed = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                });
            if save_and_quit {
                if self.save_all_dirty_tabs() {
                    self.pending_quit = false;
                    self.quit_confirmed = true;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else if quit_confirmed {
                self.pending_quit = false;
                self.quit_confirmed = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            } else if cancelled {
                self.pending_quit = false;
            }
        }

        if let Some(mut prompt) = self.name_prompt.clone() {
            let mut submitted = false;
            let mut cancelled = false;
            let mut just_focused = self.name_prompt_focus_pending;
            egui::Window::new(prompt.title.clone())
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    let resp = ui.add(egui::TextEdit::singleline(&mut prompt.value));
                    // Only the frame this prompt (re)opened - see
                    // `name_prompt_focus_pending`'s doc comment for why
                    // this can't just be called unconditionally here.
                    if just_focused {
                        resp.request_focus();
                        just_focused = false;
                    }
                    let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() || enter_pressed {
                            submitted = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                });
            self.name_prompt_focus_pending = just_focused;
            if submitted {
                let name = prompt.value.trim().to_string();
                self.name_prompt = None;
                if !name.is_empty() {
                    self.apply_name_prompt(prompt.action, name);
                }
            } else if cancelled {
                self.name_prompt = None;
            } else {
                self.name_prompt = Some(prompt);
            }
        }

        if let Some(dialog) = self.gitignore_confirm_dialog.clone() {
            let mut chosen_policy: Option<KotoshelfDirPolicy> = None;
            egui::Window::new("Workspace Folder (.kotoshelf)")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.set_max_width(400.0);
                    if dialog.kotoshelf_exists {
                        ui.label("Git repository detected (.kotoshelf/ is not in .gitignore):");
                    } else {
                        ui.label("Git repository detected. Kotoshelf uses:");
                    }
                    ui.add_space(4.0);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        for (rel_path, desc) in workspace::APP_WORKSPACE_MANAGED_PATHS {
                            ui.horizontal(|ui| {
                                ui.monospace(*rel_path);
                                ui.label(format!("— {desc}"));
                            });
                        }
                    });
                    ui.add_space(6.0);
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        chosen_policy = Some(KotoshelfDirPolicy::Denied);
                    }
                    ui.horizontal(|ui| {
                        if !dialog.already_ignored {
                            let primary_label = if dialog.kotoshelf_exists {
                                "Add to .gitignore"
                            } else {
                                "Create & Add to .gitignore"
                            };
                            let secondary_label = if dialog.kotoshelf_exists {
                                "Keep Without .gitignore"
                            } else {
                                "Create Only"
                            };
                            if ui.button(primary_label).clicked() {
                                chosen_policy = Some(KotoshelfDirPolicy::AllowedWithGitignore);
                            }
                            if ui.button(secondary_label).clicked() {
                                chosen_policy = Some(KotoshelfDirPolicy::AllowedWithoutGitignore);
                            }
                        } else if ui.button("Create").clicked() {
                            chosen_policy = Some(KotoshelfDirPolicy::AllowedWithoutGitignore);
                        }
                        if ui.button("Cancel").clicked() {
                            chosen_policy = Some(KotoshelfDirPolicy::Denied);
                        }
                    });
                });
            if let Some(policy) = chosen_policy {
                let zoom = ui.ctx().zoom_factor();
                self.apply_gitignore_confirm_decision(policy, zoom);
            }
        }

        if let Some(mut dialog) = self.git_commit_dialog.clone() {
            let mut submitted = false;
            let mut cancelled = false;
            let mut just_focused = self.git_commit_focus_pending;
            egui::Window::new("Git Commit")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label("Commit message:");
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut dialog.message)
                            .desired_rows(3)
                            .desired_width(380.0),
                    );
                    if just_focused {
                        resp.request_focus();
                        just_focused = false;
                    }
                    ui.add_space(4.0);
                    let push_label = match &dialog.push_target {
                        Some(target) => format!("Push to remote ({target})"),
                        None => "Push to remote (no target remote configured)".to_string(),
                    };
                    ui.checkbox(&mut dialog.push, push_label);
                    if dialog.uses_lfs && !self.git_lfs_available {
                        ui.add_space(4.0);
                        ui.colored_label(
                            egui::Color32::RED,
                            "Warning: .gitattributes uses Git LFS, but 'git-lfs' is not found in PATH.",
                        );
                    }
                    ui.add_space(6.0);
                    let can_submit = !dialog.message.trim().is_empty()
                        && (!dialog.uses_lfs || self.git_lfs_available);
                    let ctrl_enter = ui.input(|i| {
                        (i.modifiers.ctrl || i.modifiers.command) && i.key_pressed(egui::Key::Enter)
                    });
                    if ctrl_enter && can_submit {
                        submitted = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        cancelled = true;
                    }
                    let commit_btn_label = if dialog.push { "Commit & Push" } else { "Commit" };
                    ui.horizontal(|ui| {
                        if ui.add_enabled(can_submit, egui::Button::new(commit_btn_label)).clicked() {
                            submitted = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                });
            self.git_commit_focus_pending = just_focused;
            if submitted {
                self.git_commit_dialog = None;
                self.start_git_commit(dialog.message, dialog.push);
                self.focus_pending = true;
            } else if cancelled {
                self.git_commit_dialog = None;
                self.focus_pending = true;
            } else {
                self.git_commit_dialog = Some(dialog);
            }
        }

        if let Some((path, name)) = self.confirm_trash.clone() {
            let mut confirmed = false;
            let mut cancelled = false;
            egui::Window::new("Delete")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("Move \"{name}\" to the trash?"));
                    ui.horizontal(|ui| {
                        if ui.button("Move to trash").clicked() {
                            confirmed = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                });
            if confirmed {
                self.confirm_trash = None;
                self.trash_node(&path, &name);
            } else if cancelled {
                self.confirm_trash = None;
            }
        }

        if let Some(choices) = self.wiki_choices.clone() {
            let mut picked = None;
            let mut cancelled = false;
            egui::Window::new("Multiple notes match this wiki link")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    for path in &choices {
                        if ui.button(format!("{}\n{}", path.file_name(), path.display_string())).clicked() {
                            picked = Some(path.clone());
                        }
                    }
                    ui.separator();
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                });
            if let Some(path) = picked {
                self.wiki_choices = None;
                self.open_file(path);
            } else if cancelled {
                self.wiki_choices = None;
            }
        }

        if let Some(choices) = self.template_picker.clone() {
            let mut picked = None;
            let mut cancelled = false;
            egui::Window::new("Insert Template")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    for (name, path) in &choices {
                        if ui.button(name).clicked() {
                            picked = Some(path.clone());
                        }
                    }
                    ui.separator();
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                });
            if let Some(path) = picked {
                self.template_picker = None;
                self.insert_template(ui.ctx(), &path);
            } else if cancelled {
                self.template_picker = None;
            }
        }

        // In-flight send: offer Cancel (button or Escape). The HTTP worker
        // thread can't be interrupted, but dropping the receiver discards
        // its result and unblocks new sends right away.
        if self.send_pending.is_some() && self.send_palette.is_none() {
            let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
            egui::Window::new("Sending")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -40.0))
                .show(ui.ctx(), |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Sending…");
                        if ui.button("Cancel (Esc)").clicked() {
                            cancel = true;
                        }
                    });
                });
            if cancel {
                self.send_pending = None;
                self.status = "Send cancelled.".to_string();
            }
        }

        if let Some(mut state) = self.send_palette.clone() {
            if let Some(focused_id) = ui.ctx().memory(|m| m.focused()) {
                ui.ctx().memory_mut(|m| m.surrender_focus(focused_id));
            }
            let mut picked = None;
            let mut close = false;
            egui::Window::new("Send Palette")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    if self.presets.is_empty() {
                        ui.label("No presets configured. Open Settings to add one.");
                        return;
                    }
                    for (idx, preset) in self.presets.iter().enumerate() {
                        let digit = (idx + 1) % 10;
                        let label = format!("{digit}. {}\n{}", preset.name, preset.url);
                        if ui.selectable_label(idx == state.selected, label).clicked() {
                            picked = Some(idx);
                        }
                    }
                });
            if !self.presets.is_empty() {
                if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    state.selected = (state.selected + 1) % self.presets.len();
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    state.selected = state.selected.checked_sub(1).unwrap_or(self.presets.len() - 1);
                }
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    picked = Some(state.selected);
                }
                const DIGIT_KEYS: [egui::Key; 10] = [
                    egui::Key::Num1,
                    egui::Key::Num2,
                    egui::Key::Num3,
                    egui::Key::Num4,
                    egui::Key::Num5,
                    egui::Key::Num6,
                    egui::Key::Num7,
                    egui::Key::Num8,
                    egui::Key::Num9,
                    egui::Key::Num0,
                ];
                for (idx, key) in DIGIT_KEYS.iter().enumerate() {
                    if idx < self.presets.len() && ui.input(|i| i.key_pressed(*key)) {
                        picked = Some(idx);
                    }
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            self.send_palette = if close { None } else { Some(state) };
            if let Some(idx) = picked {
                self.last_used_preset = Some(idx);
                if let Some(preset) = self.presets.get(idx).cloned() {
                    self.send_with_preset(ui.ctx(), preset);
                }
            }
        }

        if self.theme_dialog_open {
            let mut close = false;
            let mut picked = None;
            egui::Window::new("Theme")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    for (label, value) in
                        [("System (follow OS)", ThemeSelection::System), ("Light", ThemeSelection::Light), ("Dark", ThemeSelection::Dark)]
                    {
                        if ui.selectable_label(self.theme_selection == value, label).clicked() {
                            picked = Some(value);
                        }
                    }
                    ui.separator();
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            if let Some(selection) = picked {
                self.theme_selection = selection;
                if let Err(e) = crate::theme::save_selected(selection) {
                    self.status = format!("Failed to save theme: {e}");
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) || close {
                self.theme_dialog_open = false;
            }
        }

        if self.font_dialog_open {
            let mut close = false;
            let mut changed = false;
            egui::Window::new("Editor Font")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.label("Select a font for both ASCII and multi-byte (Japanese) text:");
                    ui.add_space(4.0);
                    for preset in FontSelection::builtin_presets() {
                        let available = preset.is_available();
                        let label = if available {
                            preset.label()
                        } else {
                            format!("{} (not installed)", preset.label())
                        };
                        let selected = self.font_config.selection == preset;
                        if ui.add_enabled(available, egui::Button::selectable(selected, label)).clicked() {
                            self.font_config.selection = preset;
                            changed = true;
                        }
                    }

                    if let FontSelection::Custom { path, index } = &mut self.font_config.selection {
                        let label = FontSelection::Custom {
                            path: path.clone(),
                            index: *index,
                        }
                        .label();
                        let _ = ui.selectable_label(true, label);
                        ui.horizontal(|ui| {
                            ui.label("Collection index (.ttc):");
                            if ui.add(egui::DragValue::new(index).range(0..=16)).changed() {
                                changed = true;
                            }
                        });
                    }

                    if ui.button("Browse Font File (.ttf / .otf / .ttc)…").clicked() {
                        if let Some(file) = rfd::FileDialog::new()
                            .add_filter("Font Files", &["ttf", "otf", "ttc", "TTF", "OTF", "TTC"])
                            .pick_file()
                        {
                            self.font_config.selection = FontSelection::Custom {
                                path: file.to_string_lossy().into_owned(),
                                index: 0,
                            };
                            changed = true;
                        }
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Editor Font Size:");
                        let mut size = self.font_config.clamped_size();
                        if ui
                            .add(egui::Slider::new(&mut size, 10.0..=32.0).step_by(0.5).suffix(" pt"))
                            .changed()
                        {
                            self.font_config.size = size;
                            changed = true;
                        }
                        if ui.button("Reset (14 pt)").clicked() {
                            self.font_config.size = FontConfig::default().size;
                            changed = true;
                        }
                    });

                    ui.separator();
                    ui.label("Sample Preview:");
                    let preview_font = egui::FontId::monospace(self.font_config.clamped_size());
                    ui.label(
                        egui::RichText::new("ASCII: 0123456789 ABCDEFG abcdefg !@#$%^&*()_+-=[]{}")
                            .font(preview_font.clone()),
                    );
                    ui.label(
                        egui::RichText::new("日本語: ０１２３４５６７８９ ＡＢＣ あいうえお 漢字かな混じり")
                            .font(preview_font),
                    );

                    ui.separator();
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            if changed {
                crate::fonts::apply(ui.ctx(), &self.font_config);
                self.save_current_workspace_settings(ui.ctx().zoom_factor());
                if let Err(e) = crate::fonts::save_config(&self.font_config) {
                    self.status = format!("Failed to save font settings: {e}");
                } else {
                    self.status = format!(
                        "Font updated: {} ({:.1} pt)",
                        self.font_config.selection.label(),
                        self.font_config.clamped_size()
                    );
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) || close {
                self.font_dialog_open = false;
            }
        }

        // Dismiss-and-pass-through: any *typed character* while the popup
        // is open (not counting the frame it just opened on) closes it
        // without touching focus, so the keystroke reaches the editor
        // completely normally this same frame - confirmed live as a real
        // usability problem otherwise: typing a multi-word title like
        // "water plants" opens the popup right after "water " (already a
        // non-empty title), and without this, every character of
        // "plants" typed afterward was silently swallowed (the editor
        // had no focus to receive it, and the popup doesn't bind plain
        // letter keys to anything) rather than continuing the title.
        if let Some(state) = &self.task_assist {
            if !state.just_opened && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(_)))) {
                self.task_assist = None;
                // Restoring focus via `focus_pending` (checked *after*
                // the editor's own `.show()` runs later this frame)
                // would miss this exact frame's pending Text event -
                // requesting it directly through `Memory` instead takes
                // effect immediately, in time for the editor's own
                // `.show()` later this same frame to see itself as
                // focused and actually consume the keystroke rather than
                // dropping it for one more frame.
                if let Some(i) = self.active {
                    let id = editor_id(self.tabs[i].id);
                    ui.ctx().memory_mut(|m| m.request_focus(id));
                }
            }
        }

        if let Some(state) = &mut self.task_assist {
            // Same fix as the Send Palette (see its own comment): without
            // this, the editor's `TextEdit` keeps keyboard focus and
            // Enter/arrow keys leak into the document underneath the
            // popup instead of navigating it. Skipped for exactly the
            // opening frame - see `TaskAssistState::just_opened`'s doc
            // comment for why (the Space that opened this popup is still
            // pending in the editor's own input queue and needs to reach
            // it normally).
            if !state.just_opened {
                if let Some(focused_id) = ui.ctx().memory(|m| m.focused()) {
                    ui.ctx().memory_mut(|m| m.surrender_focus(focused_id));
                }
            }
            state.just_opened = false;
            let total: usize = state.groups.iter().map(|(_, items)| items.len()).sum();
            let mut picked: Option<String> = None;
            let mut close = false;
            egui::Window::new("Task")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                        let mut flat_idx = 0;
                        for (group, items) in &state.groups {
                            ui.label(egui::RichText::new(group.as_str()).strong());
                            for (label, insert) in items {
                                let is_highlighted = flat_idx == state.highlight;
                                let resp = ui.selectable_label(is_highlighted, label.as_str());
                                if is_highlighted {
                                    resp.scroll_to_me(Some(egui::Align::Center));
                                }
                                if resp.clicked() {
                                    picked = Some(insert.clone());
                                }
                                flat_idx += 1;
                            }
                            ui.separator();
                        }
                    });
                });
            if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                state.highlight = (state.highlight + 1) % total;
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                state.highlight = state.highlight.checked_sub(1).unwrap_or(total - 1);
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                picked = state.groups.iter().flat_map(|(_, items)| items.iter()).nth(state.highlight).map(|(_, insert)| insert.clone());
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            if let Some(text) = picked {
                self.insert_at_cursor(ui.ctx(), &text);
                close = true;
            }
            if close {
                self.task_assist = None;
                // `surrender_focus` above (across every frame the popup
                // was open) left the editor without keyboard focus - on
                // close, whether a marker was inserted or this was a
                // plain Escape, the user should be able to keep typing
                // immediately rather than needing to click back into the
                // editor. Reuses the same one-shot flag the editor's own
                // render site already checks for every other "focus this
                // tab's editor" case (new tab, tab switch, ...).
                self.focus_pending = true;
            }
        }

        if self.settings_dialog.is_some() {
            self.show_settings_dialog(ui.ctx());
        }

        if let Some(dialog) = &mut self.remote_dialog {
            if let Some(rx) = &dialog.test_pending {
                match rx.try_recv() {
                    Ok(out) => {
                        dialog.test_result = Some(out);
                        dialog.test_pending = None;
                        dialog.test_copied = false;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        ui.ctx().request_repaint();
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        dialog.test_pending = None;
                    }
                }
            }

            let mut close_dialog = false;
            let mut save_config = false;
            let mut connect_target = None;
            let mut run_test_target = None;
            egui::Window::new("Open Remote Workspace")
                .collapsible(false)
                .resizable(false)
                .default_width(520.0)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.set_max_width(520.0);
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            let left_top = ui.cursor().min.y;
                            ui.set_width(145.0);
                            ui.set_min_height(dialog.right_pane_height);
                            ui.label("Profiles");
                            let list_top = ui.cursor().min.y;
                            let header_used = (list_top - left_top).max(0.0);
                            let button_row_h = ui.spacing().interact_size.y + ui.spacing().item_spacing.y + 4.0;
                            let list_height = (dialog.right_pane_height - header_used - button_row_h).clamp(110.0, 260.0);

                            egui::Frame::canvas(ui.style())
                                .inner_margin(egui::Margin::same(4))
                                .show(ui, |ui| {
                                    let inner_h = (list_height - 8.0).max(80.0);
                                    egui::ScrollArea::vertical()
                                        .id_salt("ssh_profiles_scroll")
                                        .min_scrolled_height(inner_h)
                                        .max_height(inner_h)
                                        .auto_shrink([false, false])
                                        .show(ui, |ui| {
                                            ui.set_min_height(inner_h);
                                            for (i, p) in dialog.profiles.iter().enumerate() {
                                                let label = if p.name.is_empty() { "(unnamed)" } else { &p.name };
                                                if ui.selectable_label(dialog.selected == i, label).clicked() {
                                                    if dialog.selected != i {
                                                        dialog.selected = i;
                                                        dialog.test_result = None;
                                                        dialog.test_copied = false;
                                                    }
                                                }
                                            }
                                        });
                                });

                            ui.horizontal(|ui| {
                                if ui.button("Add").clicked() {
                                    dialog.profiles.push(ssh::SshProfile::new("New Profile"));
                                    dialog.selected = dialog.profiles.len() - 1;
                                    dialog.test_result = None;
                                    dialog.test_copied = false;
                                }
                                if ui.add_enabled(!dialog.profiles.is_empty(), egui::Button::new("Remove")).clicked() {
                                    dialog.profiles.remove(dialog.selected);
                                    dialog.selected = dialog.selected.saturating_sub(1);
                                    dialog.test_result = None;
                                    dialog.test_copied = false;
                                }
                            });
                        });

                        let (sep_rect, _) = ui.allocate_exact_size(
                            egui::vec2(8.0, dialog.right_pane_height.clamp(160.0, 300.0)),
                            egui::Sense::hover(),
                        );
                        ui.painter().vline(
                            sep_rect.center().x,
                            sep_rect.y_range(),
                            ui.visuals().widgets.noninteractive.bg_stroke,
                        );

                        let right_resp = ui.vertical(|ui| {
                            ui.set_width(355.0);
                            if let Some(profile) = dialog.profiles.get_mut(dialog.selected) {
                                egui::Grid::new("ssh_profile_form").num_columns(2).show(ui, |ui| {
                                    ui.label("Name");
                                    ui.text_edit_singleline(&mut profile.name);
                                    ui.end_row();

                                    ui.label("Host");
                                    ui.text_edit_singleline(&mut profile.host);
                                    ui.end_row();

                                    ui.label("Port");
                                    let mut port_str = profile.port.map(|p| p.to_string()).unwrap_or_default();
                                    if ui.text_edit_singleline(&mut port_str).changed() {
                                        profile.port = port_str.trim().parse::<u16>().ok();
                                    }
                                    ui.end_row();

                                    ui.label("User");
                                    let mut user_str = profile.user.clone().unwrap_or_default();
                                    if ui.text_edit_singleline(&mut user_str).changed() {
                                        profile.user = if user_str.is_empty() { None } else { Some(user_str) };
                                    }
                                    ui.end_row();

                                    ui.label("Identity file");
                                    ui.horizontal(|ui| {
                                        let mut idf = profile.identity_file.clone().unwrap_or_default();
                                        if ui.add(egui::TextEdit::singleline(&mut idf).desired_width(180.0)).changed() {
                                            profile.identity_file = if idf.is_empty() { None } else { Some(idf) };
                                        }
                                        if ui.button("Browse…").clicked() {
                                            if let Some(f) = rfd::FileDialog::new().pick_file() {
                                                profile.identity_file = Some(f.to_string_lossy().into_owned());
                                            }
                                        }
                                    });
                                    ui.end_row();

                                    ui.label("Remote folder");
                                    ui.text_edit_singleline(&mut profile.remote_path);
                                    ui.end_row();

                                    let rp = profile.remote_path.trim();
                                    if ssh::check_remote_folder(rp).is_err() {
                                        ui.label("");
                                        ui.colored_label(egui::Color32::from_rgb(200, 60, 60), "\"..\" is not allowed in the remote folder.");
                                        ui.end_row();
                                    } else if rp.is_empty() {
                                        ui.label("");
                                        ui.weak("Empty = home folder (\"~\" or \"~/sub\" also works).");
                                        ui.end_row();
                                    } else if rp == "/" {
                                        ui.label("");
                                        ui.colored_label(
                                            egui::Color32::from_rgb(200, 140, 0),
                                            "⚠ \"/\" opens the entire remote filesystem.",
                                        );
                                        ui.end_row();
                                    }

                                    ui.label("Agent");
                                    ui.checkbox(
                                        &mut profile.disable_agent,
                                        "Do not use remote agent (no ~/.kotoshelf/agent)",
                                    );
                                    ui.end_row();
                                });
                            } else {
                                ui.label("Add a profile to get started.");
                            }
                            ui.separator();
                            ui.label("SSH command path (blank = \"ssh\" from PATH)");
                            ui.text_edit_singleline(&mut dialog.ssh_command_path);
                        });
                        let measured_right_h = right_resp.response.rect.height().clamp(160.0, 300.0);
                        if (dialog.right_pane_height - measured_right_h).abs() > 1.0 {
                            dialog.right_pane_height = measured_right_h;
                            ui.ctx().request_repaint();
                        }
                    });

                    if dialog.test_pending.is_some() || dialog.test_result.is_some() {
                        ui.separator();
                        if dialog.test_pending.is_some() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Running ssh -vT test…");
                            });
                        } else if let Some(res) = &dialog.test_result {
                            let output_to_copy = res.output.clone();
                            let summary_text = res.summary.clone();
                            let is_success = res.success;
                            let mut copy_clicked = false;
                            ui.horizontal(|ui| {
                                let copy_label = if dialog.test_copied { "✔ Copied" } else { "📋 Copy" };
                                if ui
                                    .button(copy_label)
                                    .on_hover_text("Copy test output to clipboard")
                                    .clicked()
                                {
                                    copy_clicked = true;
                                }
                                let (icon, color) = if is_success {
                                    ("✔", egui::Color32::from_rgb(40, 160, 60))
                                } else {
                                    ("✖", egui::Color32::from_rgb(200, 60, 60))
                                };
                                ui.colored_label(color, format!("{icon} {summary_text}"));
                            });
                            if copy_clicked {
                                ui.ctx().copy_text(output_to_copy.clone());
                                dialog.test_copied = true;
                            }
                            egui::ScrollArea::vertical()
                                .id_salt("ssh_test_output_scroll")
                                .max_height(120.0)
                                .show(ui, |ui| {
                                    let mut out_text = output_to_copy.as_str();
                                    ui.add(
                                        egui::TextEdit::multiline(&mut out_text)
                                            .font(egui::TextStyle::Monospace)
                                            .desired_width(500.0)
                                            .desired_rows(5),
                                    );
                                });
                        }
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        let can_connect = dialog
                            .profiles
                            .get(dialog.selected)
                            .map(|p| !p.host.trim().is_empty() && ssh::check_remote_folder(&p.remote_path).is_ok())
                            .unwrap_or(false);
                        let is_testing = dialog.test_pending.is_some();
                        let test_label = if is_testing { "Testing…" } else { "Test" };
                        if ui.add_enabled(can_connect && !is_testing, egui::Button::new(test_label)).clicked() {
                            run_test_target = dialog.profiles.get(dialog.selected).cloned();
                        }
                        if ui.button("Save").clicked() {
                            save_config = true;
                            close_dialog = true;
                        }
                        if ui.add_enabled(can_connect, egui::Button::new("Connect")).clicked() {
                            connect_target = dialog.profiles.get(dialog.selected).cloned();
                            close_dialog = true;
                        }
                        if ui.add_enabled(can_connect, egui::Button::new("Save & Connect")).clicked() {
                            save_config = true;
                            connect_target = dialog.profiles.get(dialog.selected).cloned();
                            close_dialog = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close_dialog = true;
                        }
                    });
                });

            if let Some(profile) = run_test_target {
                if let Some(dialog) = &mut self.remote_dialog {
                    let ssh_cmd = dialog.ssh_command_path.clone();
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let out = ssh::test_connection_verbose(&ssh_cmd, &profile);
                        let _ = tx.send(out);
                    });
                    dialog.test_pending = Some(rx);
                    dialog.test_result = None;
                    dialog.test_copied = false;
                }
            }
            if save_config {
                if let Some(dialog) = &self.remote_dialog {
                    let config = ssh::AppConfig {
                        ssh_profiles: dialog.profiles.clone(),
                        ssh_command_path: dialog.ssh_command_path.clone(),
                        ..ssh::load_app_config()
                    };
                    if let Err(e) = ssh::save_app_config(&config) {
                        self.status = format!("Failed to save SSH profiles: {e}");
                    } else {
                        self.ssh_command_path = dialog.ssh_command_path.clone();
                        self.status = "Saved SSH profiles.".to_string();
                    }
                }
            }
            if let Some(profile) = connect_target {
                if let Some(dialog) = &self.remote_dialog {
                    self.ssh_command_path = dialog.ssh_command_path.clone();
                }
                self.remote_dialog = None;
                self.open_ssh_workspace(profile);
            } else if close_dialog {
                self.remote_dialog = None;
            }
        }

        egui::Panel::top("menubar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("File", |ui| {
                    if ui.add(sc_button(ui, "New File", Modifiers::COMMAND, Key::N)).clicked() {
                        self.new_file();
                        ui.close();
                    }
                    if ui.add_enabled(self.active.is_some(), egui::Button::new("Insert Template…")).clicked() {
                        self.open_template_picker();
                        ui.close();
                    }
                    if ui.button("Open Folder…").clicked() {
                        self.open_folder();
                        ui.close();
                    }
                    if ui.button("Open Remote Workspace…").clicked() {
                        self.open_remote_dialog();
                        ui.close();
                    }
                    if ui.add_enabled(self.ssh_profile.is_some(), egui::Button::new("Open SSH Terminal")).clicked() {
                        self.open_ssh_terminal();
                        ui.close();
                    }
                    ui.separator();
                    let active_dirty = self.active.map(|i| self.tabs[i].dirty).unwrap_or(false);
                    if ui.add_enabled(active_dirty, sc_button(ui, "Save", Modifiers::COMMAND, Key::S)).clicked() {
                        self.save_active();
                        ui.close();
                    }
                    if ui.add_enabled(self.active.is_some(), egui::Button::new("Save As…")).clicked() {
                        self.save_as_active();
                        ui.close();
                    }
                    let any_dirty = self.tabs.iter().any(|t| t.dirty);
                    if ui.add_enabled(any_dirty, egui::Button::new("Save All")).clicked() {
                        self.save_all();
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(self.active.is_some(), sc_button(ui, "Close Tab", Modifiers::COMMAND, Key::W))
                        .clicked()
                    {
                        self.request_close(self.active.unwrap());
                        ui.close();
                    }
                    if ui
                        .add_enabled(!self.tabs.is_empty(), egui::Button::new("Close All Tabs"))
                        .clicked()
                    {
                        self.request_close_all();
                        ui.close();
                    }
                    ui.separator();
                    if ui.add(sc_button(ui, "Exit", Modifiers::COMMAND, Key::Q)).clicked() {
                        self.request_quit(ui.ctx());
                        ui.close();
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui
                        .add_enabled(self.active.is_some(), sc_button(ui, "Find…", Modifiers::COMMAND, Key::F))
                        .clicked()
                    {
                        self.find.open = true;
                        self.find.replace_mode = false;
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.active.is_some(), sc_button(ui, "Replace…", Modifiers::COMMAND, Key::H))
                        .clicked()
                    {
                        self.find.open = true;
                        self.find.replace_mode = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.add(sc_button(ui, "Search in Files…", Modifiers::COMMAND | Modifiers::SHIFT, Key::F)).clicked() {
                        self.left_pane = LeftPane::Search;
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(self.active.is_some(), sc_button(ui, "Paste Image from Clipboard", Modifiers::COMMAND | Modifiers::SHIFT, Key::I))
                        .clicked()
                    {
                        if let Some(png_bytes) = clipboard_image_png_bytes() {
                            self.attach_image_bytes(ui.ctx(), "pasted-image.png", &png_bytes);
                        } else {
                            self.status = "No image on the clipboard.".to_string();
                        }
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    // egui's own built-in whole-UI zoom (Ctrl+Plus/Minus/0,
                    // scales `pixels_per_point` - menu bar, tree, editor,
                    // preview all together) rather than a custom
                    // editor-only font-size zoom - one less thing to
                    // maintain, and it's already wired to the same
                    // keyboard shortcuts a user would expect.
                    egui::gui_zoom::zoom_menu_buttons(ui);
                    ui.separator();
                    if check_item(ui, &mut self.preview_visible, "Preview", Modifiers::COMMAND | Modifiers::SHIFT, Key::V) {
                        self.save_current_workspace_settings(ui.ctx().zoom_factor());
                        ui.close();
                    }
                    if ui.checkbox(&mut self.font_config.show_line_numbers, "Line Numbers").changed() {
                        let _ = crate::fonts::save_config(&self.font_config);
                        self.save_current_workspace_settings(ui.ctx().zoom_factor());
                        ui.close();
                    }
                    if check_item(ui, &mut self.font_config.word_wrap, "Word Wrap", Modifiers::ALT, Key::Z) {
                        let _ = crate::fonts::save_config(&self.font_config);
                        self.save_current_workspace_settings(ui.ctx().zoom_factor());
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Theme…").clicked() {
                        self.theme_dialog_open = true;
                        ui.close();
                    }
                    if ui.button("Font…").clicked() {
                        self.font_dialog_open = true;
                        ui.close();
                    }
                    ui.separator();
                    let multiple_tabs = self.tabs.len() > 1;
                    if ui
                        .add_enabled(multiple_tabs, sc_button(ui, "Next Tab", Modifiers::COMMAND, Key::Tab))
                        .clicked()
                    {
                        self.cycle_tab(1);
                        ui.close();
                    }
                    if ui
                        .add_enabled(multiple_tabs, sc_button(ui, "Previous Tab", Modifiers::COMMAND | Modifiers::SHIFT, Key::Tab))
                        .clicked()
                    {
                        self.cycle_tab(-1);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            self.selected_tab_ids.len() >= 2,
                            egui::Button::new(format!("◫ タブを並べる (選択した {} 個)", self.selected_tab_ids.len())),
                        )
                        .clicked()
                    {
                        self.tile_selected_tabs();
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            multiple_tabs,
                            egui::Button::new(format!("◫ すべてのタブを並べる ({} 個)", self.tabs.len())),
                        )
                        .clicked()
                    {
                        self.tile_all_tabs();
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.split_tab_ids.len() >= 2,
                            egui::Button::new("✕ 並べて表示を解除"),
                        )
                        .clicked()
                    {
                        self.untile_tabs();
                        ui.close();
                    }
                });
                ui.menu_button("Send", |ui| {
                    if ui
                        .add_enabled(self.active.is_some(), sc_button(ui, "Send Palette…", Modifiers::COMMAND, Key::Semicolon))
                        .clicked()
                    {
                        self.open_send_palette(ui.ctx());
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Settings…").clicked() {
                        self.open_settings_dialog();
                        ui.close();
                    }
                });
            });
        });

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open Folder…").clicked() {
                    self.open_folder();
                }
                if ui.button("New File").clicked() {
                    self.new_file();
                }
                let active_dirty = self.active.map(|i| self.tabs[i].dirty).unwrap_or(false);
                if ui.add_enabled(active_dirty, egui::Button::new("Save")).clicked() {
                    self.save_active();
                }
                if ui.checkbox(&mut self.preview_visible, "Preview").changed() {
                    self.save_current_workspace_settings(ui.ctx().zoom_factor());
                }
                if ui.add_enabled(self.active.is_some(), egui::Button::new("Template")).clicked() {
                    self.open_template_picker();
                }
                let can_git_commit =
                    self.git_available && self.has_git_repo && self.git_pending.is_none();
                if ui.add_enabled(can_git_commit, egui::Button::new("git commit")).clicked() {
                    self.open_git_commit_dialog(ui.ctx());
                }
            });
        });

        egui::Panel::top("tabbar").show(ui, |ui| {
            self.sync_tab_id_lists();
            if let Some(action) = show_tab_bar(
                ui,
                &self.tabs,
                self.active,
                &self.selected_tab_ids,
                &self.split_tab_ids,
            ) {
                match action {
                    TabBarAction::Activate(i) => {
                        self.active = Some(i);
                        self.selected_tab_ids.clear();
                        self.focus_pending = true;
                    }
                    TabBarAction::ToggleSelect(i) => self.toggle_select_tab(i),
                    TabBarAction::RangeSelect(i) => self.range_select_tabs(i),
                    TabBarAction::TileSelected => self.tile_selected_tabs(),
                    TabBarAction::TileWithActive(i) => self.tile_with_active(i),
                    TabBarAction::TileAll => self.tile_all_tabs(),
                    TabBarAction::AddToSplit(i) => self.add_tab_to_split(i),
                    TabBarAction::RemoveFromSplit(i) => self.remove_tab_from_split(i),
                    TabBarAction::Untile => self.untile_tabs(),
                    TabBarAction::CloseRequested(i) => self.request_close(i),
                    TabBarAction::CloseOtherRequested(i) => self.request_close_others(i),
                    TabBarAction::CloseRightRequested(i) => self.request_close_right(i),
                    TabBarAction::CloseLeftRequested(i) => self.request_close_left(i),
                    TabBarAction::CloseAllRequested => self.request_close_all(),
                }
            }
        });

        if self.find.open {
            if let Some(i) = self.active {
                let tab_id = self.tabs[i].id;
                egui::Panel::top("findbar").show(ui, |ui| {
                    // Own row per field (not one packed horizontal line) -
                    // cramming Find *and* Replace fields plus every button
                    // and checkbox into a single row left barely any width
                    // for the text itself.
                    ui.horizontal(|ui| {
                        ui.label("Find");
                        let enter_pressed = ui.input(|inp| inp.key_pressed(egui::Key::Enter));
                        let find_resp = ui.add(
                            egui::TextEdit::singleline(&mut self.find.query)
                                .desired_width(ui.available_width() - 260.0),
                        );
                        if self.find_focus_pending {
                            find_resp.request_focus();
                            self.find_focus_pending = false;
                        }
                        let triggered = ((find_resp.has_focus() || find_resp.lost_focus()) && enter_pressed)
                            || ui.button("Find Next").clicked();
                        if triggered {
                            find_resp.request_focus();
                            if let Some((start, end)) = self.find.find_next(ui.ctx(), editor_id(tab_id), &self.tabs[i].content) {
                                self.pending_search_selection = Some((tab_id, start, end));
                                self.find.scroll_pending = true;
                                ui.ctx().request_repaint();
                            }
                        }
                        ui.checkbox(&mut self.find.is_regex, "Regex");
                        ui.checkbox(&mut self.find.case_sensitive, "Case");
                        if ui.button("x").clicked() {
                            self.find.open = false;
                        }
                    });
                    if self.find.replace_mode {
                        ui.horizontal(|ui| {
                            ui.label("Replace");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.find.replacement)
                                    .desired_width(ui.available_width() - 260.0),
                            );
                            if ui.button("Replace").clicked() {
                                if let Some((start, end)) = self.find.replace_current(ui.ctx(), editor_id(tab_id), &mut self.tabs[i].content) {
                                    self.pending_search_selection = Some((tab_id, start, end));
                                    self.find.scroll_pending = true;
                                }
                                self.tabs[i].dirty = true;
                                ui.ctx().request_repaint();
                            }
                            if ui.button("Replace All").clicked() {
                                self.find.replace_all(&mut self.tabs[i].content);
                                self.tabs[i].dirty = true;
                            }
                        });
                    }
                    if !self.find.status.is_empty() {
                        ui.label(&self.find.status);
                    }
                });
            } else {
                self.find.open = false;
            }
        }

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let workspace_display = if let Some(profile) = &self.ssh_profile {
                    ssh_display_target(profile)
                } else if let Some(dir) = &self.workspace {
                    display_path(dir)
                } else {
                    "—".to_string()
                };
                ui.label(workspace_display);
                // `right_to_left`: each widget added lands to the *left*
                // of the previous one, so add in reverse of the desired
                // left-to-right reading order (zoom%, then status).
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.status.is_empty() {
                        ui.label(&self.status);
                        ui.separator();
                    }
                    ui.label(format!("{:.0}%", ui.ctx().zoom_factor() * 100.0));
                });
            });
        });

        egui::Panel::left("tree").resizable(true).default_size(220.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.selectable_label(self.left_pane == LeftPane::Files, "Files").clicked() {
                    self.left_pane = LeftPane::Files;
                }
                if ui.selectable_label(self.left_pane == LeftPane::Search, "Search").clicked() {
                    self.left_pane = LeftPane::Search;
                }
            });
            ui.separator();
            match self.left_pane {
                LeftPane::Files => self.show_tree_pane(ui),
                LeftPane::Search => self.show_search_panel(ui),
            }
        });

        if self.preview_visible && self.active.is_some() {
            let i = self.active.unwrap();
            let mut rendered = self.render_preview_markdown(ui.ctx(), i);
            let before = rendered.clone();
            egui::Panel::right("preview").resizable(true).default_size(360.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Preview");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("🎬 Open Video").clicked() {
                            let videos = self.find_all_videos_in_tab(i);
                            if let Some(first_vid) = videos.first() {
                                let target_path = first_vid.clone();
                                self.open_external_file(&target_path);
                            } else {
                                self.status = "No Video references found in active note.".to_string();
                            }
                        }
                        if ui.button("🔍 Open Image Viewer").clicked() {
                            let images_list = self.find_all_images_in_tab(i);
                            if !images_list.is_empty() {
                                self.open_image_viewer(images_list);
                            } else {
                                self.status = "No image references found in active note.".to_string();
                            }
                        }
                        if ui.button("📄 Open PDF").clicked() {
                            let pdfs = self.find_all_pdfs_in_tab(i);
                            if let Some(first_pdf) = pdfs.first() {
                                let target_path = first_pdf.clone();
                                self.open_external_file(&target_path);
                            } else {
                                self.status = "No PDF references found in active note.".to_string();
                            }
                        }
                    });
                });
                ui.separator();
                // egui::ScrollArea::vertical()
                egui::ScrollArea::both()
                    .id_salt("preview_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let response = CommonMarkViewer::new().show_mut(ui, &mut self.preview_cache, &mut rendered);
                        let mut clicked_wikilink = None;
                        ui.ctx().output_mut(|o| {
                            o.commands.retain(|cmd| {
                                if let egui::OutputCommand::OpenUrl(open_url) = cmd {
                                    let raw_url = open_url.url.trim();
                                    let url = raw_url.trim_start_matches('<').trim_end_matches('>').trim();
                                    if url.starts_with("wikilink:") {
                                        clicked_wikilink = Some(url["wikilink:".len()..].to_string());
                                        return false;
                                    } else if url.ends_with(".md") || !url.contains("://") {
                                        clicked_wikilink = Some(url.to_string());
                                        return false;
                                    }
                                }
                                true
                            });
                        });
                        if let Some(target) = clicked_wikilink {
                            self.resolve_wiki_link_with_ctx(Some(ui.ctx()), &target);
                        }
                        if response.response.changed() {
                            let toggled = apply_checkbox_toggle(&mut self.tabs[i].content, &before, &rendered);
                            self.tabs[i].dirty = true;
                            // A checked box may belong to a line the
                            // "Today's Tasks" dashboard generated - those
                            // carry their own visible `(path#section)`
                            // link (see `dashboard`'s module doc comment
                            // for why it's visible rather than hidden
                            // metadata), so the source file to propagate
                            // to is read directly off the line itself, no
                            // separate lookup needed. Read from `rendered`
                            // (the line as `show_mut` just left it, link
                            // and all) rather than `self.tabs[i].content`
                            // - for a *linked* line the two agree anyway
                            // (`apply_checkbox_toggle` skips the
                            // recurrence stamp for those), but `rendered`
                            // is the one guaranteed never to have grown a
                            // stamp that isn't part of the link.
                            if let Some((idx, _)) = toggled {
                                if let Some(line) = rendered.lines().nth(idx) {
                                    if let Some((source_path, _section)) = dashboard::parse_link(line) {
                                        let line = line.to_string();
                                        self.propagate_check_to_source(&source_path, &line);
                                    }
                                }
                            }
                        }
                    });
            });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            self.sync_tab_id_lists();
            let split_indices: Vec<usize> = self
                .split_tab_ids
                .iter()
                .filter_map(|&id| self.tabs.iter().position(|t| t.id == id))
                .collect();

            if split_indices.len() >= 2 {
                let n = split_indices.len();
                let mut move_swap: Option<(usize, usize)> = None;
                let mut remove_idx: Option<usize> = None;
                let mut activate_idx: Option<usize> = None;

                ui.columns(n, |cols| {
                    for (col_idx, &tab_idx) in split_indices.iter().enumerate() {
                        let col_ui = &mut cols[col_idx];
                        let is_active = self.active == Some(tab_idx);
                        let tab_name = self
                            .tabs
                            .get(tab_idx)
                            .map(|t| {
                                if t.dirty {
                                    format!("{}*", t.name)
                                } else {
                                    t.name.clone()
                                }
                            })
                            .unwrap_or_default();

                        let header_fill = if is_active {
                            col_ui.visuals().selection.bg_fill.gamma_multiply(0.35)
                        } else {
                            col_ui.visuals().faint_bg_color
                        };
                        egui::Frame::NONE
                            .fill(header_fill)
                            .inner_margin(egui::Margin::symmetric(6, 3))
                            .corner_radius(4.0)
                            .show(col_ui, |ui| {
                                ui.horizontal(|ui| {
                                    let badge_prefix = if is_active { "●" } else { "○" };
                                    let title_btn = ui.selectable_label(
                                        is_active,
                                        format!("{badge_prefix} [{}] {tab_name}", col_idx + 1),
                                    );
                                    if title_btn.clicked() {
                                        activate_idx = Some(tab_idx);
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button("✕")
                                                .on_hover_text("並べて表示から外す (ファイルは閉じません)")
                                                .clicked()
                                            {
                                                remove_idx = Some(tab_idx);
                                            }
                                            if ui
                                                .add_enabled(
                                                    col_idx + 1 < n,
                                                    egui::Button::new("▶").small(),
                                                )
                                                .on_hover_text("右へ移動")
                                                .clicked()
                                            {
                                                move_swap = Some((col_idx, col_idx + 1));
                                            }
                                            if ui
                                                .add_enabled(
                                                    col_idx > 0,
                                                    egui::Button::new("◀").small(),
                                                )
                                                .on_hover_text("左へ移動")
                                                .clicked()
                                            {
                                                move_swap = Some((col_idx, col_idx - 1));
                                            }
                                        },
                                    );
                                });
                            });
                        col_ui.separator();
                        self.show_editor_pane(col_ui, tab_idx);
                    }
                });

                if let Some((a, b)) = move_swap {
                    if a < self.split_tab_ids.len() && b < self.split_tab_ids.len() {
                        self.split_tab_ids.swap(a, b);
                    }
                }
                if let Some(idx) = remove_idx {
                    self.remove_tab_from_split(idx);
                }
                if let Some(idx) = activate_idx {
                    self.active = Some(idx);
                    self.focus_pending = true;
                }
            } else if let Some(i) = self.active {
                self.show_editor_pane(ui, i);
            } else {
                ui.label("No file open");
            }
        });

        self.show_image_viewer_dialog(ui.ctx());
    }
}

impl KotoshelfApp {
    /// Renders a single editor pane for `self.tabs[i]` (used both in normal
    /// single-tab mode and for each column in side-by-side split view).
    fn show_editor_pane(&mut self, ui: &mut egui::Ui, i: usize) {
        enum NavigationTarget {
            Wiki(String),
            Url(String),
        }

        if i >= self.tabs.len() {
            return;
        }
        let extension = self.tabs[i]
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .unwrap_or_else(|| "txt".to_string());
        let is_markdown = extension == "md" || extension == "markdown";
        let ext_for_layouter = extension.clone();
        let highlighter = &self.highlighter;
        let search_matches = if self.find.open {
            self.find.get_matches(&self.tabs[i].content)
        } else {
            Vec::new()
        };
        let active_match = if self.find.open && self.active == Some(i) {
            search_matches.get(self.find.current).copied()
        } else {
            None
        };
        let tab_id = self.tabs[i].id;
        let id = editor_id(tab_id);
        let active_line_idx = egui::TextEdit::load_state(ui.ctx(), id)
            .and_then(|s| s.cursor.char_range())
            .map(|r| {
                let pos = r.primary.index.0;
                let starts = workspace::line_starts(&self.tabs[i].content);
                starts.iter().rposition(|&s| s <= pos).unwrap_or(0)
            });

        let editor_font_size = self.font_config.clamped_size();
        let word_wrap = self.font_config.word_wrap;
        let show_line_numbers = self.font_config.show_line_numbers;
        let known_wiki_targets = if self.workspace_root().is_some() && self.wiki_target_set_ready {
            Some(&self.wiki_target_set)
        } else {
            None
        };
        let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            let mut job = highlighter.layout_job_live_with_wiki_targets(
                buf.as_str(),
                &ext_for_layouter,
                editor_font_size,
                active_line_idx,
                &search_matches,
                active_match,
                known_wiki_targets,
            );
            job.wrap.max_width = if word_wrap { wrap_width } else { f32::INFINITY };
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let enter_pressed = ui.input(|inp| inp.key_pressed(egui::Key::Enter));
        let available_height = ui.available_height();
        let dir = self.tabs[i]
            .path
            .as_ref()
            .map(|p| p.parent())
            .or_else(|| self.workspace_root().map(|(root, _)| root))
            .unwrap_or_else(|| TabPath::Local(PathBuf::new()));
        let is_dark = self.highlighter.is_dark();
        let scroll_area = if word_wrap {
            egui::ScrollArea::vertical()
        } else {
            egui::ScrollArea::both()
        };
        let (((click_target, propagate_target), rename_target), media_target) = scroll_area
            .id_salt(("editor_scroll", tab_id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut propagate_to_source: Option<(String, String)> = None;
                let mut rename_to: Option<String> = None;

                // If Markdown note, display editable note title header at the top of the editor
                if is_markdown {
                    ui.add_space(2.0);
                    let title_font = egui::FontId::proportional(22.0);
                    let title_edit = egui::TextEdit::singleline(&mut self.tabs[i].title_buffer)
                        .font(title_font)
                        .hint_text("Note Title")
                        .desired_width(f32::INFINITY)
                        .frame(egui::Frame::NONE);
                    let title_resp = ui.add(title_edit);
                    if title_resp.clicked() || title_resp.changed() {
                        self.active = Some(i);
                    }
                    if title_resp.lost_focus() || (title_resp.has_focus() && enter_pressed) {
                        rename_to = Some(self.tabs[i].title_buffer.clone());
                    }
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);
                }

                // 1. Process pending search selection before layout so TextEdit renders with it
                let mut target_scroll_cursor = None;
                if self.pending_search_selection.map(|(pid, _, _)| pid) == Some(tab_id) {
                    if let Some((_, start, end)) = self.pending_search_selection.take() {
                        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                            state.cursor.set_char_range(Some(CCursorRange::two(
                                CCursor::new(start),
                                CCursor::new(end),
                            )));
                            egui::TextEdit::store_state(ui.ctx(), id, state);
                        }
                        target_scroll_cursor = Some(CCursor::new(start));
                        self.find.scroll_pending = true;
                    }
                }

                let editor_focused = ui.ctx().memory(|m| m.has_focus(id));

                // 2. Consume Tab / Shift+Tab before TextEdit so it doesn't wipe selected text
                if editor_focused {
                    let is_shift = ui.input(|inp| inp.modifiers.shift);
                    let tab_consumed = ui.input_mut(|inp| inp.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
                        || ui.input_mut(|inp| inp.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab));
                    if tab_consumed {
                        if let Some(state) = egui::TextEdit::load_state(ui.ctx(), id) {
                            if let Some(range) = state.cursor.char_range() {
                                if let Some(new_range) = apply_tab_indentation(&mut self.tabs[i].content, range, is_shift) {
                                    self.tabs[i].dirty = true;
                                    if let Some(mut new_state) = egui::TextEdit::load_state(ui.ctx(), id) {
                                        new_state.cursor.set_char_range(Some(new_range));
                                        egui::TextEdit::store_state(ui.ctx(), id, new_state);
                                    }
                                }
                            }
                        }
                    }

                    // 3. Ctrl+Enter / Cmd+Enter: Toggle task checkbox on current line and propagate to source
                    let is_ctrl_enter = ui.input_mut(|inp| {
                        (inp.modifiers.ctrl || inp.modifiers.command) && inp.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)
                            || (inp.modifiers.ctrl || inp.modifiers.command) && inp.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)
                    });
                    if is_ctrl_enter {
                        if let Some(state) = egui::TextEdit::load_state(ui.ctx(), id) {
                            if let Some(range) = state.cursor.char_range() {
                                let pos = range.primary.index.0;
                                let starts = workspace::line_starts(&self.tabs[i].content);
                                if let Some(line_num) = starts.iter().rposition(|&s| s <= pos) {
                                    if let Some(prop) = toggle_task_checkbox_on_line(&mut self.tabs[i].content, line_num) {
                                        propagate_to_source = Some(prop);
                                    }
                                    self.tabs[i].dirty = true;
                                }
                            }
                        }
                    }
                }

                let line_starts_before = workspace::line_starts(&self.tabs[i].content);
                let total_lines = line_starts_before.len().max(1);
                let digits = total_lines.to_string().len().max(2);
                let gutter_width = if show_line_numbers {
                    (digits as f32 * (editor_font_size * 0.62) + 20.0).max(38.0)
                } else {
                    0.0
                };

                let output = ui
                    .horizontal_top(|ui| {
                        if show_line_numbers {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.add_space(gutter_width);
                        }
                        egui::TextEdit::multiline(&mut self.tabs[i].content)
                            .id(id)
                            .lock_focus(true)
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY)
                            .min_size(egui::vec2(0.0, 60.0))
                            .hint_text("Start typing…")
                            .layouter(&mut layouter)
                            .show(ui)
                    })
                    .inner;

                if show_line_numbers {
                    let line_starts = workspace::line_starts(&self.tabs[i].content);
                    let line_num_font = egui::FontId::monospace((editor_font_size * 0.85).max(10.0));
                    let normal_color = if is_dark {
                        egui::Color32::from_rgb(110, 118, 129)
                    } else {
                        egui::Color32::from_rgb(140, 149, 159)
                    };
                    let active_color = if is_dark {
                        egui::Color32::from_rgb(210, 218, 226)
                    } else {
                        egui::Color32::from_rgb(36, 41, 47)
                    };
                    let sep_color = if is_dark {
                        egui::Color32::from_rgb(48, 54, 61)
                    } else {
                        egui::Color32::from_rgb(216, 222, 228)
                    };
                    let sep_x = output.galley_pos.x - 6.0;
                    ui.painter().line_segment(
                        [
                            egui::pos2(sep_x, output.response.rect.min.y),
                            egui::pos2(sep_x, output.response.rect.max.y),
                        ],
                        egui::Stroke::new(1.0, sep_color),
                    );
                    let clip_rect = ui.clip_rect();
                    for (l_idx, &line_start_char) in line_starts.iter().enumerate() {
                        let cpos = output
                            .galley
                            .pos_from_cursor(CCursor::new(line_start_char))
                            .translate(output.galley_pos.to_vec2());
                        if cpos.max.y >= clip_rect.min.y - 20.0 && cpos.min.y <= clip_rect.max.y + 20.0 {
                            let color = if active_line_idx == Some(l_idx) {
                                active_color
                            } else {
                                normal_color
                            };
                            ui.painter().text(
                                egui::pos2(output.galley_pos.x - 10.0, cpos.center().y),
                                egui::Align2::RIGHT_CENTER,
                                format!("{}", l_idx + 1),
                                line_num_font.clone(),
                                color,
                            );
                        }
                    }
                }

                if self.focus_pending && self.active == Some(i) {
                    output.response.request_focus();
                    self.focus_pending = false;
                }
                if output.response.clicked() || output.response.changed() {
                    self.active = Some(i);
                }
                if output.response.changed() {
                    self.tabs[i].dirty = true;
                    if enter_pressed {
                        if let Some(range) = output.cursor_range {
                            if let Some(new_pos) =
                                continue_list_on_enter(&mut self.tabs[i].content, range.primary.index.0)
                            {
                                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                                    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_pos))));
                                    egui::TextEdit::store_state(ui.ctx(), id, state);
                                }
                            }
                        }
                    }
                }
                if let Some(cursor) = target_scroll_cursor {
                    let rect = output
                        .galley
                        .pos_from_cursor(cursor)
                        .translate(output.galley_pos.to_vec2());
                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                    self.find.scroll_pending = false;
                } else if self.find.scroll_pending && self.active == Some(i) {
                    if let Some(range) = output.cursor_range {
                        let rect = output
                            .galley
                            .pos_from_cursor(range.primary)
                            .translate(output.galley_pos.to_vec2());
                        ui.scroll_to_rect(rect, Some(egui::Align::Center));
                    }
                    self.find.scroll_pending = false;
                }

                // 4. Render horizontal rules and live media thumbnails for Markdown
                let mut media_click_target = None;
                if is_markdown {
                    let content_str = self.tabs[i].content.clone();
                    let line_starts = workspace::line_starts(&content_str);
                    let card_bg = if is_dark {
                        egui::Color32::from_rgb(28, 33, 40)
                    } else {
                        egui::Color32::from_rgb(240, 243, 246)
                    };
                    let card_border = if is_dark {
                        egui::Color32::from_rgb(60, 70, 85)
                    } else {
                        egui::Color32::from_rgb(210, 215, 225)
                    };
                    let hr_bg = if is_dark {
                        egui::Color32::from_rgba_unmultiplied(88, 166, 255, 20)
                    } else {
                        egui::Color32::from_rgba_unmultiplied(9, 105, 218, 16)
                    };
                    let hr_line_color = if is_dark {
                        egui::Color32::from_rgb(95, 110, 128)
                    } else {
                        egui::Color32::from_rgb(160, 172, 186)
                    };
                    let hr_active_line_color = if is_dark {
                        egui::Color32::from_rgba_unmultiplied(95, 110, 128, 140)
                    } else {
                        egui::Color32::from_rgba_unmultiplied(160, 172, 186, 140)
                    };
                    let clip_rect = ui.clip_rect();
                    let editor_left = output.galley_pos.x;
                    let editor_right = output.response.rect.max.x.max(clip_rect.max.x - 8.0);

                    let mut inside_code_fence = false;
                    for (l_idx, line) in content_str.split_inclusive('\n').enumerate() {
                        if line.trim_start().starts_with("```") {
                            inside_code_fence = !inside_code_fence;
                        }
                        if !inside_code_fence && crate::syntax::is_horizontal_rule(line) {
                            if let Some(&line_start_char) = line_starts.get(l_idx) {
                                let cpos = output
                                    .galley
                                    .pos_from_cursor(CCursor::new(line_start_char))
                                    .translate(output.galley_pos.to_vec2());
                                if cpos.max.y >= clip_rect.min.y - 20.0 && cpos.min.y <= clip_rect.max.y + 20.0 {
                                    let row_rect = egui::Rect::from_min_max(
                                        egui::pos2(editor_left, cpos.min.y + 1.0),
                                        egui::pos2(editor_right, cpos.max.y - 1.0),
                                    );
                                    ui.painter().rect_filled(row_rect, 3.0, hr_bg);
                                    if active_line_idx == Some(l_idx) {
                                        let y = cpos.max.y - 1.5;
                                        ui.painter().line_segment(
                                            [egui::pos2(editor_left, y), egui::pos2(editor_right, y)],
                                            egui::Stroke::new(1.5, hr_active_line_color),
                                        );
                                    } else {
                                        let y = cpos.center().y;
                                        ui.painter().line_segment(
                                            [egui::pos2(editor_left, y), egui::pos2(editor_right, y)],
                                            egui::Stroke::new(1.5, hr_line_color),
                                        );
                                    }
                                }
                            }
                            continue;
                        }

                        if let Some(media) = crate::syntax::parse_media_line(line) {
                            if l_idx < line_starts.len() {
                                let line_start_char = line_starts[l_idx];
                                let cpos = output
                                    .galley
                                    .pos_from_cursor(CCursor::new(line_start_char))
                                    .translate(output.galley_pos.to_vec2());

                                let card_rect = egui::Rect::from_min_size(
                                    cpos.min + egui::vec2(6.0, 22.0),
                                    egui::vec2(240.0, 120.0),
                                );

                                if ui.clip_rect().intersects(card_rect) {
                                    let resp = ui.allocate_rect(card_rect, egui::Sense::click());
                                    if resp.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                    }

                                    let bg = if resp.hovered() {
                                        if is_dark {
                                            egui::Color32::from_rgb(38, 44, 54)
                                        } else {
                                            egui::Color32::from_rgb(230, 235, 242)
                                        }
                                    } else {
                                        card_bg
                                    };
                                    ui.painter().rect(card_rect, 6.0, bg, egui::Stroke::new(1.0, card_border), egui::StrokeKind::Outside);

                                    if let Some(uri) = KotoshelfApp::resolve_media_uri(
                                        ui.ctx(),
                                        &media.target,
                                        &dir,
                                        &mut self.video_render_cache,
                                        &mut self.pdf_render_cache,
                                        &mut self.ssh_image_cache,
                                        &self.ssh_profile,
                                        &self.ssh_command_path,
                                    ) {
                                        let inner_rect = card_rect.shrink(6.0);
                                        let img = egui::Image::new(uri)
                                            .max_size(inner_rect.size())
                                            .corner_radius(4.0);
                                        ui.put(inner_rect, img);
                                    }

                                    if media.is_video {
                                        let center = card_rect.center();
                                        ui.painter().circle_filled(center, 18.0, egui::Color32::from_black_alpha(160));
                                        ui.painter().text(
                                            center,
                                            egui::Align2::CENTER_CENTER,
                                            "▶",
                                            egui::FontId::proportional(16.0),
                                            egui::Color32::WHITE,
                                        );
                                    }

                                    if resp.clicked() {
                                        self.active = Some(i);
                                        media_click_target = Some((media, dir.clone()));
                                    }
                                }
                            }
                        }
                    }
                }

                // 5. Interactive task checkboxes: hover hand cursor and direct click toggle
                let mut direct_toggled_task = false;
                if is_markdown {
                    let content_str = self.tabs[i].content.clone();
                    let line_starts = workspace::line_starts(&content_str);
                    let mouse_pos = ui.input(|inp| inp.pointer.interact_pos().or_else(|| inp.pointer.latest_pos()));
                    let primary_clicked = ui.input(|inp| inp.pointer.primary_clicked());

                    for (l_idx, line) in content_str.split_inclusive('\n').enumerate() {
                        if l_idx < line_starts.len() {
                            let trimmed = line.trim_start();
                            let leading_spaces = line.len() - trimmed.len();
                            let is_task = trimmed.starts_with("- [ ]") || trimmed.starts_with("* [ ]") || trimmed.starts_with("+ [ ]");
                            let is_checked = trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]")
                                || trimmed.starts_with("* [x]") || trimmed.starts_with("+ [x]");

                            if is_task || is_checked {
                                let start_c = line_starts[l_idx] + leading_spaces;
                                let end_c = (start_c + 5).min(content_str.len());
                                let p_start = output.galley.pos_from_cursor(CCursor::new(start_c)).translate(output.galley_pos.to_vec2());
                                let p_end = output.galley.pos_from_cursor(CCursor::new(end_c)).translate(output.galley_pos.to_vec2());
                                let min_x = p_start.min.x.min(p_end.min.x) - 4.0;
                                let max_x = p_start.max.x.max(p_end.max.x) + 4.0;
                                let min_y = p_start.min.y.min(p_end.min.y) - 2.0;
                                let max_y = p_start.max.y.max(p_end.max.y) + 2.0;
                                let box_rect = egui::Rect::from_min_max(egui::pos2(min_x, min_y), egui::pos2(max_x, max_y));

                                if let Some(m_pos) = mouse_pos {
                                    if box_rect.contains(m_pos) && ui.clip_rect().contains(m_pos) {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                        if primary_clicked {
                                            self.active = Some(i);
                                            if let Some(prop) = toggle_task_checkbox_on_line(&mut self.tabs[i].content, l_idx) {
                                                propagate_to_source = Some(prop);
                                            }
                                            self.tabs[i].dirty = true;
                                            direct_toggled_task = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // 6. Scroll past end (bottom overscroll padding so the last line can reach ~50% height)
                let overscroll_height = (available_height - output.response.rect.height())
                    .max(available_height * 0.55)
                    .max(120.0);
                let (_bottom_rect, bottom_resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width().max(100.0), overscroll_height),
                    egui::Sense::click(),
                );
                if bottom_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
                }
                if bottom_resp.clicked() {
                    self.active = Some(i);
                    output.response.request_focus();
                    let end_char = self.tabs[i].content.chars().count();
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(end_char))));
                        egui::TextEdit::store_state(ui.ctx(), id, state);
                    }
                }

                // Click detection:
                // 1. Click on checkbox [- [ ] or - [x]]: toggles task in-place and propagates
                // 2. Click on link [[wiki]] or [anchor](url): single click navigates directly
                // 3. Click elsewhere (e.g. whitespace before/after link): places caret for editing
                let mut nav_target = None;
                if !direct_toggled_task && output.response.clicked() {
                    let content = self.tabs[i].content.clone();
                    let line_starts = workspace::line_starts(&content);
                    let char_idx_opt = output.cursor_range.map(|r| r.primary.index.0).or_else(|| {
                        ui.input(|inp| inp.pointer.interact_pos()).map(|click_pos| {
                            let local_pos = click_pos - output.galley_pos;
                            output.galley.cursor_from_pos(local_pos).index.0
                        })
                    });

                    if let Some(char_idx) = char_idx_opt {
                        let mut handled_checkbox = false;
                        if let Some(line_num) = line_starts.iter().rposition(|&s| s <= char_idx) {
                            let line_start_char = line_starts[line_num];
                            let col_char = char_idx.saturating_sub(line_start_char);
                            let lines: Vec<&str> = content.lines().collect();
                            if line_num < lines.len() {
                                let line_str = lines[line_num];
                                let trimmed = line_str.trim_start();
                                let leading_spaces = line_str.len() - trimmed.len();
                                let is_task = trimmed.starts_with("- [ ]") || trimmed.starts_with("* [ ]") || trimmed.starts_with("+ [ ]");
                                let is_checked = trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]")
                                    || trimmed.starts_with("* [x]") || trimmed.starts_with("+ [x]");

                                if (is_task || is_checked) && col_char >= leading_spaces && col_char <= leading_spaces + 8 {
                                    handled_checkbox = true;
                                    if let Some(prop) = toggle_task_checkbox_on_line(&mut self.tabs[i].content, line_num) {
                                        propagate_to_source = Some(prop);
                                    }
                                    self.tabs[i].dirty = true;
                                }
                            }
                        }

                        // 2. If not a checkbox click, check if clicked directly on a link
                        if !handled_checkbox {
                            let byte_idx = content.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(content.len());
                            if let Some((_, target)) = crate::wikilink::find_wiki_links(&content).into_iter().find(|(range, _)| range.contains(&byte_idx)) {
                                nav_target = Some(NavigationTarget::Wiki(target.to_string()));
                            } else if let Some((_, _, url)) = crate::wikilink::find_markdown_links(&content).into_iter().find(|(range, _, _)| range.contains(&byte_idx)) {
                                if url.starts_with("http://") || url.starts_with("https://") {
                                    nav_target = Some(NavigationTarget::Url(url.to_string()));
                                } else {
                                    nav_target = Some(NavigationTarget::Wiki(url.to_string()));
                                }
                            }
                        }
                    }
                }
                (((nav_target, propagate_to_source), rename_to), media_click_target)
            })
            .inner;
        if let Some(new_title) = rename_target {
            self.rename_tab_by_title(i, &new_title);
        }
        if let Some((source_path, line)) = propagate_target {
            self.propagate_check_to_source(&source_path, &line);
        }
        if let Some((media, dir)) = media_target {
            if media.is_video || media.is_pdf {
                let target_path = dir.join_normalized(&media.target);
                self.open_external_file(&target_path);
            } else {
                let images_list = self.find_all_images_in_tab(i);
                if !images_list.is_empty() {
                    self.open_image_viewer(images_list);
                }
            }
        }
        if let Some(nav) = click_target {
            match nav {
                NavigationTarget::Wiki(target) => {
                    self.resolve_wiki_link_with_ctx(Some(ui.ctx()), &target);
                }
                NavigationTarget::Url(url) => {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                }
            }
        }
    }
}

/// Applies a checkbox toggle egui_commonmark's `show_mut` made to
/// `after` (a copy of `before` with exactly one `[ ]`/`[x]` flipped) onto
/// `content` (the tab's *real* text) instead - `before`/`after` are
/// built from `render_preview_markdown`'s image-URI-rewritten copy, not
/// `content` itself, so byte offsets between them and `content` don't
/// line up. Line numbers do, though: image-ref rewriting only ever
/// changes text *within* a line (the rewrite regex can't match across a
/// newline), so this finds which line differs between `before`/`after`
/// and flips the same marker on that same line number in `content`.
///
/// Returns `Some((idx, checked))` for the line that was toggled (its
/// index *before* any recurrence line-insertion above it) so the caller
/// can look at that same line afterward - e.g. to propagate the check to
/// a `dashboard`-generated line's source file.
fn apply_checkbox_toggle(content: &mut String, before: &str, after: &str) -> Option<(usize, bool)> {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let idx = before_lines.iter().zip(&after_lines).position(|(b, a)| b != a)?;
    let after_line = after_lines.get(idx)?;
    let checked = after_line.contains("[x]") || after_line.contains("[X]");
    let (old_marker, new_marker) = if checked { ("[ ]", "[x]") } else { ("[x]", "[ ]") };

    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    if let Some(pos) = lines
        .get(idx)
        .and_then(|l| l.find(old_marker).or_else(|| if !checked { l.find("[X]") } else { None }))
    {
        lines[idx].replace_range(pos..pos + 3, new_marker);

        if dashboard::parse_link(&lines[idx]).is_none() {
            if checked {
                apply_task_completion(&mut lines, idx, chrono::Local::now().date_naive());
            } else {
                lines[idx] = tasks::strip_done_date(&lines[idx]);
            }
        }

        let mut rebuilt = lines.join("\n");
        if content.ends_with('\n') {
            rebuilt.push('\n');
        }
        *content = rebuilt;
    } else {
        // If content line at `idx` doesn't match directly (e.g. {{task}} was expanded in preview),
        // look for a matching line in content if present
        let target_key = dashboard::task_key(after_line);
        if let Some(c_idx) = lines.iter().position(|l| dashboard::task_key(l) == target_key) {
            if let Some(pos) = lines[c_idx]
                .find(old_marker)
                .or_else(|| if !checked { lines[c_idx].find("[X]") } else { None })
            {
                lines[c_idx].replace_range(pos..pos + 3, new_marker);
                if dashboard::parse_link(&lines[c_idx]).is_none() {
                    if checked {
                        apply_task_completion(&mut lines, c_idx, chrono::Local::now().date_naive());
                    } else {
                        lines[c_idx] = tasks::strip_done_date(&lines[c_idx]);
                    }
                }
                let mut rebuilt = lines.join("\n");
                if content.ends_with('\n') {
                    rebuilt.push('\n');
                }
                *content = rebuilt;
            }
        }
    }
    Some((idx, checked))
}

/// Toggles the task checkbox on line `line_num` in `content`, applies task completion/recurrence,
/// and returns `Some((source_path, line))` if propagation to a source note is required.
fn toggle_task_checkbox_on_line(
    content: &mut String,
    line_num: usize,
) -> Option<(String, String)> {
    let starts = workspace::line_starts(content);
    if line_num >= starts.len() {
        return None;
    }
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    if line_num >= lines.len() {
        return None;
    }

    let line_str = &lines[line_num];
    let trimmed = line_str.trim_start();
    let is_task = trimmed.starts_with("- [ ]") || trimmed.starts_with("* [ ]") || trimmed.starts_with("+ [ ]");
    let is_checked = trimmed.starts_with("- [x]")
        || trimmed.starts_with("- [X]")
        || trimmed.starts_with("* [x]")
        || trimmed.starts_with("* [X]")
        || trimmed.starts_with("+ [x]")
        || trimmed.starts_with("+ [X]");

    let mut propagate = None;
    if is_task {
        for (old, new) in [("- [ ]", "- [x]"), ("* [ ]", "* [x]"), ("+ [ ]", "+ [x]")] {
            if let Some(p) = lines[line_num].find(old) {
                lines[line_num].replace_range(p..p + old.len(), new);
                break;
            }
        }
        let raw_toggled_line = lines[line_num].clone();
        if dashboard::parse_link(&raw_toggled_line).is_none() {
            let today = chrono::Local::now().date_naive();
            apply_task_completion(&mut lines, line_num, today);
        }
        let mut rebuilt = lines.join("\n");
        if content.ends_with('\n') {
            rebuilt.push('\n');
        }
        *content = rebuilt;

        if let Some((source_path, _)) = dashboard::parse_link(&raw_toggled_line) {
            propagate = Some((source_path, raw_toggled_line));
        }
    } else if is_checked {
        for (old, new) in [
            ("- [x]", "- [ ]"),
            ("- [X]", "- [ ]"),
            ("* [x]", "* [ ]"),
            ("* [X]", "* [ ]"),
            ("+ [x]", "+ [ ]"),
            ("+ [X]", "+ [ ]"),
        ] {
            if let Some(p) = lines[line_num].find(old) {
                lines[line_num].replace_range(p..p + old.len(), new);
                break;
            }
        }
        if dashboard::parse_link(&lines[line_num]).is_none() {
            lines[line_num] = tasks::strip_done_date(&lines[line_num]);
        }
        let raw_toggled_line = lines[line_num].clone();
        let mut rebuilt = lines.join("\n");
        if content.ends_with('\n') {
            rebuilt.push('\n');
        }
        *content = rebuilt;

        if let Some((source_path, _)) = dashboard::parse_link(&raw_toggled_line) {
            propagate = Some((source_path, raw_toggled_line));
        }
    }
    propagate
}

/// Rebuilds an unchecked source task line from a linked dashboard/task-list line
/// (stripping the trailing `[[source.md#Section]]` or `[source.md](...)` link,
/// normalizing any preview-rewritten embedded wikilinks back to `[[...]]`,
/// ensuring the checkbox is `- [ ]`, and stripping any `✅ YYYY-MM-DD` stamp).
fn restore_unchecked_source_line(generated_line: &str) -> String {
    let without_link = dashboard::strip_link(generated_line);
    let normalized = dashboard::normalize_embedded_wikilinks(&without_link);
    let mut line = tasks::strip_done_date(&normalized);
    for (old, new) in [
        ("- [x]", "- [ ]"),
        ("- [X]", "- [ ]"),
        ("* [x]", "* [ ]"),
        ("* [X]", "* [ ]"),
        ("+ [x]", "+ [ ]"),
        ("+ [X]", "+ [ ]"),
    ] {
        if let Some(p) = line.find(old) {
            line.replace_range(p..p + old.len(), new);
            break;
        }
    }
    line
}

/// After `lines[idx]` was just checked, applies Obsidian Tasks-style
/// completion behavior if that line carries any recognized metadata
/// (🔁/📅/⏳/🛫/🏁) - a plain `- [x] buy milk` with none of those is left
/// exactly as the marker-swap above already left it. For a recurring
/// task, inserts the next occurrence as a new *unchecked* line directly
/// above (matching Obsidian's documented default placement) with every
/// present date shifted by the same amount and any done-date stripped;
/// the just-completed line then either gets a `✅ <today>` stamp (default)
/// or is removed entirely (`🏁 delete`).
fn apply_task_completion(lines: &mut Vec<String>, idx: usize, today: chrono::NaiveDate) {
    let Some(line) = lines.get(idx) else { return };
    let meta = tasks::parse_task_meta(line);
    if meta.is_empty() {
        return;
    }

    let mut completed_idx = idx;
    if let Some(rule) = &meta.recur {
        // `next_date_after` is computed from `today` in "when done" mode
        // (the whole point of that modifier), but the *shift* applied to
        // the line's existing date fields must always be measured
        // against the ORIGINAL reference date, not against whatever base
        // `next` was computed from - otherwise (in "when done" mode,
        // where those differ) the reference field would land on
        // `original + (next - today)` instead of exactly `next`.
        // Confirmed by a failing test before this fix: a task due Jan 1,
        // completed Jan 20 with `every week when done`, produced Jan 8
        // (wrong: original + 7) instead of Jan 27 (right: today + 7).
        let original_reference = meta.anchor().unwrap_or(today);
        let computed_from = if rule.when_done { today } else { original_reference };
        if let Some(next) = tasks::next_date_after(rule, computed_from) {
            let shift = next - original_reference;
            let mut next_line = lines[idx].replacen("[x]", "[ ]", 1);
            next_line = tasks::shift_dates(&next_line, shift);
            next_line = tasks::strip_done_date(&next_line);
            lines.insert(idx, next_line);
            completed_idx = idx + 1;
        }
    }

    if meta.on_completion_delete {
        lines.remove(completed_idx);
    } else {
        lines[completed_idx] = tasks::stamp_done_date(&lines[completed_idx], today);
    }
}

/// After Enter is pressed inside a list item, continues the marker onto
/// the new line - `- `/`* `/`+ ` bullets, `- [ ] `/`- [x] ` checkboxes
/// (always continued unchecked), or `N. ` numbered lists (incrementing
/// N) - matching common editor behavior (VS Code, Obsidian). Enter on
/// an *empty* list item (just the marker, nothing after it) removes the
/// marker instead of continuing it, so there's a way to leave the list
/// without deleting it by hand. `cursor_char` is the char offset right
/// after the newline `TextEdit` already inserted this frame (from
/// `TextEditOutput::cursor_range`); returns the char offset the cursor
/// should end up at if this changed anything, `None` if the previous
/// line wasn't a list item at all.
fn continue_list_on_enter(content: &mut String, cursor_char: usize) -> Option<usize> {
    let chars: Vec<char> = content.chars().collect();
    if cursor_char == 0 || cursor_char > chars.len() || chars[cursor_char - 1] != '\n' {
        return None;
    }
    let mut line_start = cursor_char - 1;
    while line_start > 0 && chars[line_start - 1] != '\n' {
        line_start -= 1;
    }
    let prev_line: String = chars[line_start..cursor_char - 1].iter().collect();

    let re = regex::Regex::new(r"^(\s*)(?:([-*+])(\s+\[[ xX]\])?|(\d+)\.)\s+").unwrap();
    let caps = re.captures(&prev_line)?;
    let whole = caps.get(0)?.as_str();
    let rest = &prev_line[whole.len()..];

    if rest.trim().is_empty() {
        let new_content: String =
            chars[..line_start].iter().collect::<String>() + &chars[cursor_char..].iter().collect::<String>();
        *content = new_content;
        return Some(line_start);
    }

    let indent = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let new_marker = if let Some(n) = caps.get(4) {
        let next = n.as_str().parse::<u64>().unwrap_or(0) + 1;
        format!("{indent}{next}. ")
    } else {
        let bullet = caps.get(2).map(|m| m.as_str()).unwrap_or("-");
        if caps.get(3).is_some() {
            format!("{indent}{bullet} [ ] ")
        } else {
            format!("{indent}{bullet} ")
        }
    };

    let new_content: String =
        chars[..cursor_char].iter().collect::<String>() + &new_marker + &chars[cursor_char..].iter().collect::<String>();
    let new_pos = cursor_char + new_marker.chars().count();
    *content = new_content;
    Some(new_pos)
}

/// Applies tab insertion or multi-line block indent/outdent on Tab / Shift+Tab.
fn apply_tab_indentation(content: &mut String, range: CCursorRange, shift: bool) -> Option<CCursorRange> {
    let chars: Vec<char> = content.chars().collect();
    let start_idx = range.primary.index.0.min(range.secondary.index.0);
    let end_idx = range.primary.index.0.max(range.secondary.index.0);

    // If single cursor without selection and not shift: insert a tab character
    if start_idx == end_idx && !shift {
        let mut new_chars = Vec::with_capacity(chars.len() + 1);
        new_chars.extend_from_slice(&chars[..start_idx]);
        new_chars.push('\t');
        new_chars.extend_from_slice(&chars[start_idx..]);
        *content = new_chars.into_iter().collect();
        return Some(CCursorRange::one(CCursor::new(start_idx + 1)));
    }

    // Identify line boundaries
    let mut line_starts = vec![0usize];
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            line_starts.push(i + 1);
        }
    }

    let mut start_line = 0;
    let mut end_line = 0;
    for (line_idx, &l_start) in line_starts.iter().enumerate() {
        if l_start <= start_idx {
            start_line = line_idx;
        }
        if l_start <= end_idx {
            end_line = line_idx;
        }
    }
    // If multi-line selection ends exactly at the start of a line (after newline), exclude that line
    if start_idx < end_idx && end_line > start_line && line_starts.get(end_line) == Some(&end_idx) {
        end_line -= 1;
    }

    let lines: Vec<&str> = content.split('\n').collect();
    let mut new_lines = Vec::new();
    let mut new_start_idx = start_idx;
    let mut new_end_idx = end_idx;

    for (line_idx, line) in lines.iter().enumerate() {
        if line_idx >= start_line && line_idx <= end_line {
            if shift {
                let removed = if line.starts_with('\t') {
                    1
                } else if line.starts_with("    ") {
                    4
                } else if line.starts_with(' ') {
                    1
                } else {
                    0
                };
                new_lines.push(line[removed..].to_string());
                if line_idx == start_line {
                    let col = new_start_idx.saturating_sub(line_starts[line_idx]);
                    new_start_idx = new_start_idx.saturating_sub(removed.min(col));
                }
                new_end_idx = new_end_idx.saturating_sub(removed);
            } else {
                new_lines.push(format!("\t{line}"));
                if line_idx == start_line {
                    new_start_idx += 1;
                }
                new_end_idx += 1;
            }
        } else {
            new_lines.push(line.to_string());
        }
    }

    *content = new_lines.join("\n");
    if start_idx == end_idx {
        Some(CCursorRange::one(CCursor::new(new_start_idx)))
    } else {
        Some(CCursorRange::two(CCursor::new(new_start_idx), CCursor::new(new_end_idx)))
    }
}

/// A tab's `{{filename}}` template value - its name with the extension
/// stripped (e.g. "notes.md" -> "notes").
fn strip_extension(name: &str) -> &str {
    name.rsplit_once('.').map(|(base, _)| base).unwrap_or(name)
}

/// Display label for a "New file/folder in <label>/" prompt title -
/// just the workspace's own directory name.
/// A local path's user-facing display string - strips the `\\?\`
/// "verbatim" prefix Windows' `Path::canonicalize()` adds (see
/// `images.rs`'s `local_file_uri`, which hits the same thing building
/// `file://` URIs) so the window title/status bar show the ordinary
/// `C:\Users\...` form instead of `\\?\C:\Users\...`.
fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or_else(|| s.into_owned())
}

/// If the OS clipboard currently holds image data (not just text),
/// returns it PNG-encoded - `arboard::Clipboard::get_image` hands back
/// raw RGBA pixels, not a ready-to-write file, so this also does the
/// encoding (reusing the `image` crate already pulled in for the
/// preview pane's own PNG/JPEG decoding). `None` covers every failure
/// case uniformly (no clipboard access, clipboard holds text/nothing,
/// malformed image data) - the caller falls back to normal text-paste
/// behavior in all of those, same as the original only special-cased an
/// actual image being present.
fn clipboard_image_png_bytes() -> Option<Vec<u8>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let img = clipboard.get_image().ok()?;
    let buf = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())?;
    let mut png_bytes = Vec::new();
    image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .ok()?;
    Some(png_bytes)
}

fn workspace_label(workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.to_string_lossy().into_owned())
}

/// A directory `TabPath`'s own display label - for a real tree node this
/// is always just its name, but the workspace *root* has no such node
/// (an SSH root is `TabPath::Ssh("")`, with nothing to take a name
/// from), so `root_label` is the fallback for that case (mirrors the
/// original React app's `basename(dir) || basename(workspace)`).
fn tab_path_leaf_label(path: &TabPath, root_label: &str) -> String {
    match path {
        TabPath::Local(p) => workspace_label(p),
        TabPath::Ssh(rel) => {
            if rel.is_empty() {
                root_label.to_string()
            } else {
                rel.rsplit('/').next().unwrap_or(rel).to_string()
            }
        }
    }
}

fn node_tab_path(is_ssh: bool, raw: &str) -> TabPath {
    if is_ssh { TabPath::Ssh(raw.to_string()) } else { TabPath::Local(PathBuf::from(raw)) }
}

/// The "New File…"/"New Folder…"/"Rename…"/"Delete" menu shared by every
/// tree node's right-click context menu. `target_dir`/`target_label` are
/// where a *new* file/folder created from this menu would land - the
/// node's own path if it's a directory, its parent otherwise (mirrors
/// the original app's `menuTargetDir`).
fn node_context_menu(
    ui: &mut egui::Ui,
    node_path: &TabPath,
    node_name: &str,
    target_dir: &TabPath,
    target_label: &str,
    action: &mut Option<TreeAction>,
    selected_nodes: &std::collections::HashSet<String>,
    is_ssh: bool,
) {
    if selected_nodes.len() > 1 {
        let count = selected_nodes.len();
        let selected_paths: Vec<TabPath> = selected_nodes.iter().map(|p| node_tab_path(is_ssh, p)).collect();

        if ui.button(format!("Move {count} items…")).clicked() {
            *action = Some(TreeAction::MoveBulk { paths: selected_paths.clone(), name: target_dir.display_string() });
            ui.close();
        }
        if ui.button(format!("Delete {count} items (to trash)")).clicked() {
            *action = Some(TreeAction::DeleteBulk { paths: selected_paths });
            ui.close();
        }
        ui.separator();
    }

    if ui.button("New File…").clicked() {
        *action = Some(TreeAction::NewFile { dir: target_dir.clone(), label: target_label.to_string() });
        ui.close();
    }
    if ui.button("New Folder…").clicked() {
        *action = Some(TreeAction::NewFolder { dir: target_dir.clone(), label: target_label.to_string() });
        ui.close();
    }
    ui.separator();
    if ui.button("Open Terminal Here").clicked() {
        *action = Some(TreeAction::OpenTerminal { dir: target_dir.clone() });
        ui.close();
    }
    if !is_ssh {
        if ui.button("Show in File Manager").clicked() {
            *action = Some(TreeAction::OpenFileManager { path: target_dir.clone() });
            ui.close();
        }
    }
    ui.separator();
    if ui.button("Rename…").clicked() {
        *action = Some(TreeAction::Rename { path: node_path.clone(), name: node_name.to_string() });
        ui.close();
    }
    if ui.button("Duplicate").clicked() {
        *action = Some(TreeAction::Duplicate { path: node_path.clone(), name: node_name.to_string() });
        ui.close();
    }
    if ui.button("Move…").clicked() {
        *action = Some(TreeAction::Move { path: node_path.clone(), name: node_name.to_string() });
        ui.close();
    }
    if ui.button("Delete (to trash)").clicked() {
        *action = Some(TreeAction::Delete { path: node_path.clone(), name: node_name.to_string() });
        ui.close();
    }
}

/// Recursively renders `nodes`, returning whatever tree action (open a
/// file, or a context-menu choice) was triggered this frame. Directories
/// are collapsible; already walked full-depth via `agent_protocol::read_tree`/
/// `ssh::read_tree` (Phase 1 keeps this simple rather than porting the
/// lazy shallow-load tree yet). `is_ssh` picks which `TabPath` variant to
/// build from each node's raw `path` string; `root_label` only matters
/// for a top-level file's parent (the workspace root itself).
fn show_tree(
    ui: &mut egui::Ui,
    nodes: &[TreeNode],
    is_ssh: bool,
    root_label: &str,
    selected_nodes: &mut std::collections::HashSet<String>,
    scroll_pending: bool,
) -> Option<TreeAction> {
    let mut action = None;
    for node in nodes {
        let node_path = node_tab_path(is_ssh, &node.path);
        let is_selected = selected_nodes.contains(&node.path)
            || selected_nodes.contains(&node_path.display_string())
            || selected_nodes.contains(&node.path.replace('\\', "/"));

        if node.is_dir {
            let id = egui::Id::new(&node_path.display_string());
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
            if !state.is_open() {
                let state_raw = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), egui::Id::new(&node.path), false);
                if state_raw.is_open() {
                    state.set_open(true);
                }
            }
            if is_ssh && state.is_open() && node.children.is_none() && action.is_none() {
                action = Some(TreeAction::ExpandDir { rel_path: node.path.clone() });
            }
            let label = node.name.clone();
            let header = state.show_header(ui, |ui| {
                let resp = ui.selectable_label(is_selected, &node.name);
                if is_selected && scroll_pending {
                    resp.scroll_to_me(Some(egui::Align::Center));
                }
                if resp.clicked() {
                    let is_multi = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                    if is_multi {
                        if is_selected {
                            selected_nodes.remove(&node.path);
                        } else {
                            selected_nodes.insert(node.path.clone());
                        }
                    } else {
                        selected_nodes.clear();
                        selected_nodes.insert(node.path.clone());
                    }
                }
                resp.context_menu(|ui| {
                    node_context_menu(ui, &node_path, &node.name, &node_path, &label, &mut action, selected_nodes, is_ssh);
                });
            }).body(|ui| {
                if let Some(children) = &node.children {
                    if let Some(a) = show_tree(ui, children, is_ssh, root_label, selected_nodes, scroll_pending) {
                        action = Some(a);
                    }
                } else if is_ssh {
                    ui.weak("Loading…");
                }
            });
            header.0.context_menu(|ui| {
                node_context_menu(ui, &node_path, &node.name, &node_path, &label, &mut action, selected_nodes, is_ssh);
            });
        } else {
            let resp = ui.selectable_label(is_selected, &node.name);
            if is_selected && scroll_pending {
                resp.scroll_to_me(Some(egui::Align::Center));
            }
            if resp.clicked() {
                let is_multi = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                if is_multi {
                    if is_selected {
                        selected_nodes.remove(&node.path);
                    } else {
                        selected_nodes.insert(node.path.clone());
                    }
                } else {
                    selected_nodes.clear();
                    selected_nodes.insert(node.path.clone());
                    action = Some(TreeAction::Open(node_path.clone()));
                }
            }
            let parent_dir = node_path.parent();
            let parent_label = tab_path_leaf_label(&parent_dir, root_label);
            resp.context_menu(|ui| {
                node_context_menu(ui, &node_path, &node.name, &parent_dir, &parent_label, &mut action, selected_nodes, is_ssh);
            });
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::{
        apply_checkbox_toggle, apply_task_completion, build_task_assist_groups, continue_list_on_enter, generate_task_id, has_task_title,
        is_task_line, task_ids_in, toggle_task_checkbox_on_line, KotoshelfApp,
    };
    use crate::dashboard;
    use crate::tabs::Tab;

    #[test]
    fn task_ids_in_finds_every_distinct_id_in_first_seen_order() {
        let content = "- [ ] a 🆔 abc123\n- [ ] b 🆔 def456\n- [ ] c 🆔 abc123\n- [ ] no id here\n";
        assert_eq!(task_ids_in(content), vec!["abc123".to_string(), "def456".to_string()]);
    }

    #[test]
    fn task_ids_in_is_empty_when_none_are_present() {
        assert!(task_ids_in("- [ ] a\n- [ ] b\n").is_empty());
    }

    #[test]
    fn generate_task_id_produces_six_base36_characters() {
        let id = generate_task_id();
        assert_eq!(id.len(), 6);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn build_task_assist_groups_omits_depends_on_when_no_ids_exist_yet() {
        let groups = build_task_assist_groups("- [ ] buy milk\n");
        assert!(!groups.iter().any(|(label, _)| label.starts_with("Depends on")));
    }

    #[test]
    fn build_task_assist_groups_includes_depends_on_when_an_id_exists() {
        let groups = build_task_assist_groups("- [ ] a 🆔 xyz789\n");
        let depends = groups.iter().find(|(label, _)| label.starts_with("Depends on")).expect("should have a Depends on group");
        assert_eq!(depends.1, vec![("xyz789".to_string(), "⛔ xyz789".to_string())]);
    }

    #[test]
    fn build_task_assist_groups_covers_every_obsidian_tasks_marker_the_user_asked_for() {
        let groups = build_task_assist_groups("- [ ] x\n");
        let all_inserts: Vec<&str> = groups.iter().flat_map(|(_, items)| items.iter()).map(|(_, insert)| insert.as_str()).collect();
        for marker in ['🔺', '⏫', '🔼', '🔽', '⏬', '📅', '⏳', '🛫', '➕', '❌', '✅', '🔁', '🏁', '🆔'] {
            assert!(all_inserts.iter().any(|i| i.contains(marker)), "missing marker {marker} in the popup's items");
        }
    }

    #[test]
    fn has_task_title_requires_text_after_the_checkbox() {
        assert!(!has_task_title("- [ ] "));
        assert!(!has_task_title("- [ ]"));
        assert!(has_task_title("- [ ] water plants"));
        assert!(has_task_title("- [ ] water plants 🔁 every week")); // already has metadata too
    }

    /// Exercises the full "Today's Tasks" round trip - generate, then
    /// check a generated line off - the exact same way the preview
    /// pane's checkbox handler in `update()` does, without needing a
    /// live GUI. Written after live GUI testing (2026-08-12) found the
    /// mechanism harder to verify reliably by driving the real window
    /// (this desktop's window manager wouldn't reliably hand automation
    /// tools keyboard/mouse focus, and blind coordinate clicks risk
    /// landing on the user's own other windows) - this test exercises
    /// the identical code path deterministically instead.
    ///
    /// Also covers the user's own live-usage feedback (2026-08-12) that
    /// reshaped this feature: the source link is a visible, workspace-
    /// relative `(path#section)` suffix read directly off the generated
    /// line (no more session-only lookup table), and only the *source*
    /// task gets the full recurrence/on-completion cycle - the generated
    /// pointer line itself is just a plain checkbox flip.
    #[test]
    fn generating_then_checking_a_dashboard_task_propagates_the_check_to_its_source_file() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_native_test_{}_{}", std::process::id(), generate_task_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shopping_path = dir.join("shopping.md");
        std::fs::write(
            &shopping_path,
            "# Shopping\n\n- [ ] buy milk 📅 2000-01-01 🔼\n- [ ] buy a car 📅 2099-01-01\n- [x] already done 📅 2000-01-01\n",
        )
        .unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        let ctx = egui::Context::default();
        let tab_id = app.alloc_tab_id();
        app.tabs.push(Tab::untitled(tab_id));
        app.active = Some(0);

        app.generate_today_tasks(&ctx);
        assert!(app.tabs[0].content.contains("buy milk"), "the due task should have been inserted: {}", app.tabs[0].content);
        assert!(!app.tabs[0].content.contains("buy a car"), "a task due in 2099 shouldn't be listed yet");
        assert!(
            app.tabs[0].content.contains("[[shopping.md#Shopping]]"),
            "the generated line should carry a visible, workspace-relative source link: {}",
            app.tabs[0].content
        );

        // Mirrors exactly what `show_mut`'s checkbox click does to the
        // preview's copy of the text, then exactly what the preview
        // panel's `response.response.changed()` branch does with it.
        let before = app.tabs[0].content.clone();
        let after = before.replacen("- [ ] buy milk", "- [x] buy milk", 1);
        let toggled = apply_checkbox_toggle(&mut app.tabs[0].content, &before, &after);
        let (idx, checked) = toggled.expect("the checkbox line should have been found and toggled");
        assert!(checked);
        assert!(
            !app.tabs[0].content.contains('✅'),
            "the generated pointer line shouldn't get its own done-date stamp - only its source should: {}",
            app.tabs[0].content
        );
        let line = after.lines().nth(idx).unwrap().to_string();
        let (source_path, section) = dashboard::parse_link(&line).expect("the checked line should carry a source link");
        assert_eq!(section.as_deref(), Some("Shopping"));
        app.propagate_check_to_source(&source_path, &line);

        let updated = std::fs::read_to_string(&shopping_path).unwrap();
        assert!(updated.contains("- [x] buy milk"), "the source file's own task should now be checked: {updated}");
        assert!(updated.contains('✅'), "checking it should have stamped a done date, just like an in-place toggle would: {updated}");
        // The unrelated tasks in the same source file must be untouched.
        assert!(updated.contains("- [ ] buy a car 📅 2099-01-01"));
        assert!(updated.contains("- [x] already done 📅 2000-01-01"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checking_recurring_task_propagates_and_creates_next_cycle() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_recur_prop_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let routine_path = dir.join("routine.md");
        std::fs::write(&routine_path, "# Habits\n- [ ] stretch 📅 2026-08-20 🔁 every day\n").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        let ctx = egui::Context::default();
        let tab_id = app.alloc_tab_id();
        app.tabs.push(Tab::untitled(tab_id));
        app.active = Some(0);

        app.generate_today_tasks(&ctx);
        assert!(app.tabs[0].content.contains("stretch"));

        // Render preview (which converts wikilinks to markdown wikilinks)
        let rendered = app.render_preview_markdown(&ctx, 0);
        assert!(rendered.contains("[routine.md#Habits](<wikilink:routine.md#Habits>)"));

        let line = rendered.lines().find(|l| l.contains("stretch")).unwrap();
        let (source_path, _) = dashboard::parse_link(line).expect("must parse markdown link");
        let checked_line = line.replacen("- [ ]", "- [x]", 1);
        app.propagate_check_to_source(&source_path, &checked_line);

        let updated = std::fs::read_to_string(&routine_path).unwrap();
        // Completed task should be checked and done-stamped
        assert!(updated.contains("- [x] stretch"), "original should be checked: {updated}");
        assert!(updated.contains('✅'), "original should have done date: {updated}");
        // Next occurrence should be created directly above
        assert!(updated.contains("- [ ] stretch 📅 2026-08-21 🔁 every day"), "next occurrence should be generated: {updated}");

        // Test with Japanese filename, parentheses, spaces, and on-completion delete (user's exact scenario)
        let copy_path = dir.join("todo2_2 - コピー (2).md");
        std::fs::write(&copy_path, "- [ ] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete\n").unwrap();
        app.generate_today_tasks(&ctx);
        let rendered2 = app.render_preview_markdown(&ctx, 0);
        let line2 = rendered2.lines().find(|l| l.contains("cycle1")).unwrap();
        let (source_path2, _) = dashboard::parse_link(line2).expect("must parse markdown link for Japanese filename with parens");
        assert_eq!(source_path2, "todo2_2 - コピー (2).md");
        let checked_line2 = line2.replacen("- [ ]", "- [x]", 1);
        app.propagate_check_to_source(&source_path2, &checked_line2);

        let updated2 = std::fs::read_to_string(&copy_path).unwrap();
        assert!(updated2.contains("- [ ] cycle1 🔼 📅 2026-08-15 🔁 every day 🏁 delete"), "next cycle should exist: {updated2}");
        assert!(!updated2.contains("2026-08-14"), "completed task should be deleted because of 🏁 delete: {updated2}");

        // Test with embedded wikilink inside task title: " - [ ] [[AI関連の論文を読んでいこう]] ⏫ 🔁 every week on thursday 🏁 delete 📅 2026-08-20"
        let notes_dir = dir.join("Notes").join("生成AI関連論文");
        std::fs::create_dir_all(&notes_dir).unwrap();
        let plan_path = notes_dir.join("予定をタスク化.md");
        std::fs::write(&plan_path, " - [ ] [[AI関連の論文を読んでいこう]] ⏫ 🔁 every week on thursday 🏁 delete 📅 2026-08-20\n").unwrap();
        app.generate_today_tasks(&ctx);
        let rendered3 = app.render_preview_markdown(&ctx, 0);
        let line3 = rendered3.lines().find(|l| l.contains("AI関連の論文を読んでいこう")).unwrap();
        let (source_path3, _) = dashboard::parse_link(line3).expect("must parse markdown link");
        assert!(source_path3.contains("予定をタスク化.md"));
        let checked_line3 = line3.replacen("- [ ]", "- [x]", 1);
        app.propagate_check_to_source(&source_path3, &checked_line3);

        let updated3 = std::fs::read_to_string(&plan_path).unwrap();
        assert!(updated3.contains("2026-08-27"), "next week thursday (2026-08-27) should be generated: {updated3}");
        assert!(!updated3.contains("2026-08-20"), "old task should be deleted: {updated3}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dropping_pdf_attaches_and_renders_preview() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_pdf_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note_path = dir.join("note.md");
        std::fs::write(&note_path, "# My Note\n").unwrap();

        let sample_pdf_path = dir.join("paper.pdf");
        // Simple mock PDF bytes
        std::fs::write(&sample_pdf_path, b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj 2 0 obj<</Type/Pages/Count 1/Kids[3 0 R]>>endobj 3 0 obj<</Type/Page/MediaBox[0 0 200 200]/Parent 2 0 R/Resources<<>>>>endobj\nxref\n0 4\n0000000000 65535 f\n0000000009 00000 n\n0000000052 00000 n\n0000000101 00000 n\ntrailer<</Size 4/Root 1 0 R>>\nstartxref\n178\n%%EOF").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        let ctx = egui::Context::default();
        app.open_file(crate::tabs::TabPath::Local(note_path.clone()));
        assert_eq!(app.tabs.len(), 1);

        app.handle_dropped_path(&ctx, sample_pdf_path.clone());

        // Check that attachments/ directory and attached pdf file were created
        let attachments_dir = dir.join("attachments");
        assert!(attachments_dir.is_dir());
        let attached_files: Vec<_> = std::fs::read_dir(&attachments_dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(attached_files.len(), 1);
        assert!(attached_files[0].starts_with("pdf-"));
        assert!(attached_files[0].ends_with("-paper.pdf"));

        // Check that markdown reference was inserted into tab content
        let content = &app.tabs[0].content;
        assert!(content.contains("![](attachments/pdf-"));
        assert!(content.contains("-paper.pdf)"));

        // Render preview and verify that it converts to kotoshelf-pdf URI
        let preview = app.render_preview_markdown(&ctx, 0);
        assert!(preview.contains("kotoshelf-pdf://"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// User feedback (2026-08-12): "when opening a wikilink, if the
    /// target is already open, [switch] to that tab" - `open_file`
    /// already has a dedup check (`tabs.iter().position(|t| t.path ==
    /// ...)`), and `resolve_wiki_link`'s single-match branch calls it
    /// directly, so this test exists to actually prove (not just read)
    /// whether that already works end-to-end for a wikilink specifically,
    /// including through a *second* click on the same link.
    #[test]
    fn resolving_a_wikilink_to_an_already_open_file_reuses_its_tab_instead_of_duplicating_it() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_native_test_{}_{}", std::process::id(), generate_task_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "See [[b]] for details.\n").unwrap();
        std::fs::write(dir.join("b.md"), "# B\n").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        app.open_file(crate::tabs::TabPath::Local(dir.join("a.md")));
        app.open_file(crate::tabs::TabPath::Local(dir.join("b.md")));
        assert_eq!(app.tabs.len(), 2, "sanity check: both files should be open as separate tabs");

        // Switch away first so resolving the link is a real "jump back",
        // not a no-op because b.md already happens to be active.
        app.active = Some(0);
        app.resolve_wiki_link("b");
        assert_eq!(app.tabs.len(), 2, "resolving [[b]] should reuse the existing tab, not open a duplicate");
        assert_eq!(app.active, Some(1), "should have switched to b.md's existing tab");

        // Clicking it again (already active) must still be a no-op, not
        // a second duplicate.
        app.resolve_wiki_link("b");
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active, Some(1));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolving_a_wikilink_with_nested_relative_path_creates_from_workspace_root() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_test_nested_{}", std::process::id()));
        let daily_dir = dir.join("Daily").join("2026").join("08");
        std::fs::create_dir_all(&daily_dir).unwrap();
        let active_file = daily_dir.join("note.md");
        std::fs::write(&active_file, "link").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        app.open_file(crate::tabs::TabPath::Local(active_file));

        app.resolve_wiki_link("Notes/読書メモ/2026-08-14のメモ");

        let expected_path = dir.join("Notes").join("読書メモ").join("2026-08-14のメモ.md");
        assert!(expected_path.exists(), "nested wikilink target should be created relative to workspace root");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_task_line_recognizes_dash_star_and_plus_checkboxes() {
        assert!(is_task_line("- [ ] buy milk"));
        assert!(is_task_line("* [x] done"));
        assert!(is_task_line("+ [ ] indented"));
        assert!(is_task_line("  - [ ] indented with leading whitespace"));
    }

    #[test]
    fn is_task_line_rejects_ordinary_lines() {
        assert!(!is_task_line("just prose"));
        assert!(!is_task_line("- a plain bullet, no checkbox"));
        assert!(!is_task_line(""));
    }

    #[test]
    fn checkbox_toggle_checks_the_right_line_in_the_real_content() {
        // `content` has the original short image target; `before`/`after`
        // are what render_preview_markdown would produce (the same
        // target rewritten to a longer `file://` URI on that same line) -
        // byte offsets differ between `content` and `before`/`after` from
        // that point on, but the line numbers still line up, since
        // rewriting never adds or removes a line.
        let mut content = "# Notes\n\n![x](img.png)\n- [ ] one\n- [ ] two\n".to_string();
        let before = "# Notes\n\n![x](file:///C:/very/long/rewritten/path.png)\n- [ ] one\n- [ ] two\n";
        let after = "# Notes\n\n![x](file:///C:/very/long/rewritten/path.png)\n- [x] one\n- [ ] two\n";
        apply_checkbox_toggle(&mut content, before, after);
        assert_eq!(content, "# Notes\n\n![x](img.png)\n- [x] one\n- [ ] two\n");
    }

    #[test]
    fn checkbox_toggle_can_uncheck() {
        let mut content = "- [x] done\n".to_string();
        let before = "- [x] done\n";
        let after = "- [ ] done\n";
        apply_checkbox_toggle(&mut content, before, after);
        assert_eq!(content, "- [ ] done\n");
    }

    #[test]
    fn checkbox_toggle_is_a_no_op_when_nothing_differs() {
        let mut content = "- [ ] one\n".to_string();
        let same = "- [ ] one\n";
        apply_checkbox_toggle(&mut content, same, same);
        assert_eq!(content, "- [ ] one\n");
    }

    fn date(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn completing_a_plain_checkbox_does_nothing_extra() {
        let mut lines = vec!["- [x] buy milk".to_string()];
        apply_task_completion(&mut lines, 0, date(2026, 1, 1));
        assert_eq!(lines, vec!["- [x] buy milk".to_string()]);
    }

    #[test]
    fn completing_a_dated_task_stamps_a_done_date() {
        let mut lines = vec!["- [x] pay rent 📅 2026-01-01".to_string()];
        apply_task_completion(&mut lines, 0, date(2026, 1, 5));
        assert_eq!(lines, vec!["- [x] pay rent 📅 2026-01-01 ✅ 2026-01-05".to_string()]);
    }

    #[test]
    fn on_completion_delete_removes_the_line_instead_of_stamping() {
        let mut lines = vec!["before".to_string(), "- [x] one-off 📅 2026-01-01 🏁 delete".to_string(), "after".to_string()];
        apply_task_completion(&mut lines, 1, date(2026, 1, 5));
        assert_eq!(lines, vec!["before".to_string(), "after".to_string()]);
    }

    #[test]
    fn completing_a_recurring_task_inserts_the_next_occurrence_above_and_stamps_the_original() {
        let mut lines = vec!["- [x] water plants 🔁 every week 📅 2026-01-01".to_string()];
        apply_task_completion(&mut lines, 0, date(2026, 1, 1));
        assert_eq!(
            lines,
            vec![
                "- [ ] water plants 🔁 every week 📅 2026-01-08".to_string(),
                "- [x] water plants 🔁 every week 📅 2026-01-01 ✅ 2026-01-01".to_string(),
            ]
        );
    }

    #[test]
    fn a_recurring_task_with_on_completion_delete_still_recurs_but_the_completed_line_is_removed() {
        let mut lines = vec!["- [x] take out trash 🔁 every week 📅 2026-01-01 🏁 delete".to_string()];
        apply_task_completion(&mut lines, 0, date(2026, 1, 1));
        assert_eq!(lines, vec!["- [ ] take out trash 🔁 every week 📅 2026-01-08 🏁 delete".to_string()]);
    }

    #[test]
    fn when_done_recurrence_anchors_on_todays_completion_date_not_the_original_due_date() {
        // Due Jan 1, but not actually completed until Jan 20 (badly
        // overdue) - "when done" means the next occurrence is one week
        // after the *completion* date, not one week after the original
        // due date.
        let mut lines = vec!["- [x] water plants 🔁 every week when done 📅 2026-01-01".to_string()];
        apply_task_completion(&mut lines, 0, date(2026, 1, 20));
        assert_eq!(
            lines[0],
            "- [ ] water plants 🔁 every week when done 📅 2026-01-27".to_string()
        );
    }

    #[test]
    fn continues_a_dash_bullet() {
        let mut content = "- one\n".to_string();
        let cursor = content.chars().count(); // right after the newline
        let new_pos = continue_list_on_enter(&mut content, cursor).unwrap();
        assert_eq!(content, "- one\n- ");
        assert_eq!(new_pos, content.chars().count());
    }

    #[test]
    fn continues_a_checkbox_unchecked_even_if_the_previous_one_was_checked() {
        let mut content = "- [x] done\n".to_string();
        let cursor = content.chars().count();
        continue_list_on_enter(&mut content, cursor).unwrap();
        assert_eq!(content, "- [x] done\n- [ ] ");
    }

    #[test]
    fn increments_a_numbered_list() {
        let mut content = "3. third\n".to_string();
        let cursor = content.chars().count();
        continue_list_on_enter(&mut content, cursor).unwrap();
        assert_eq!(content, "3. third\n4. ");
    }

    #[test]
    fn preserves_indentation() {
        let mut content = "  - nested\n".to_string();
        let cursor = content.chars().count();
        continue_list_on_enter(&mut content, cursor).unwrap();
        assert_eq!(content, "  - nested\n  - ");
    }

    #[test]
    fn enter_on_an_empty_item_removes_the_marker_instead_of_continuing_it() {
        let mut content = "- one\n- \n".to_string();
        // "- \n" is the second line (empty item) - cursor is right after
        // *its* newline, i.e. the very end of the string.
        let cursor = content.chars().count();
        let new_pos = continue_list_on_enter(&mut content, cursor).unwrap();
        assert_eq!(content, "- one\n");
        assert_eq!(new_pos, content.chars().count());
    }

    #[test]
    fn does_nothing_on_a_non_list_line() {
        let mut content = "just text\n".to_string();
        let cursor = content.chars().count();
        assert!(continue_list_on_enter(&mut content, cursor).is_none());
        assert_eq!(content, "just text\n"); // unchanged
    }

    #[test]
    fn image_viewer_state_initializes_and_resets_zoom() {
        let mut app = KotoshelfApp::new();
        app.open_image_viewer(vec![("test.png".to_string(), vec![1, 2, 3, 4])]);
        assert!(app.image_viewer.is_some());
        let viewer = app.image_viewer.as_ref().unwrap();
        assert_eq!(viewer.images.len(), 1);
        assert_eq!(viewer.images[0].0, "test.png");
        assert_eq!(viewer.zoom, 1.0);
        assert_eq!(viewer.images[0].1, vec![1, 2, 3, 4]);
    }

    #[test]
    fn wikilink_rejects_path_traversal_outside_workspace_root() {
        let mut app = KotoshelfApp::new();
        app.workspace = Some(std::path::PathBuf::from("C:\\workspace"));
        app.resolve_wiki_link("../../secret.txt");
        assert!(app.status.contains("Security alert"));

        app.resolve_wiki_link("/etc/passwd");
        assert!(app.status.contains("Security alert"));
    }

    #[test]
    fn validate_json_template_handles_various_templates() {
        use super::{validate_json_template, TokenEntry};

        let tokens = vec![TokenEntry {
            key: "OPENAI_KEY".to_string(),
            value: "sk-test-12345".to_string(),
            masked: true,
        }];

        // 1. Plain non-JSON text -> Ok
        assert!(validate_json_template("p1", "Hello {{selection}}", &tokens).is_ok());
        assert!(validate_json_template("p1", "", &tokens).is_ok());

        // 2. Valid standard JSON with selectionJson (quoted) -> Ok
        let json1 = r#"{
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "{{selectionJson}}"}
            ],
            "temperature": 0.7
        }"#;
        assert!(validate_json_template("p1", json1, &tokens).is_ok());

        // 3. Valid JSON with selection and tokens placeholder -> Ok
        let json2 = r#"{
            "prompt": "{{selection}}",
            "token": "{{tokens.OPENAI_KEY}}"
        }"#;
        assert!(validate_json_template("p1", json2, &tokens).is_ok());

        // 4. Valid JSON with quoted selectionJson and unquoted sessionIdJson -> Ok
        let json3 = r#"{
            "input": "{{selectionJson}}",
            "session": {{sessionIdJson}}
        }"#;
        assert!(validate_json_template("p1", json3, &tokens).is_ok());

        // 5. Invalid/broken JSON -> Err
        let broken = r#"{"model": "gpt-4o", "messages": ["#;
        assert!(validate_json_template("p1", broken, &tokens).is_err());
    }

    #[test]
    fn apply_tab_indentation_works_for_single_and_multi_line() {
        use super::apply_tab_indentation;
        use egui::text::{CCursor, CCursorRange};

        // 1. Single cursor inserts a tab
        let mut text = "hello".to_string();
        let range = CCursorRange::one(CCursor::new(2));
        let new_range = apply_tab_indentation(&mut text, range, false).unwrap();
        assert_eq!(text, "he\tllo");
        assert_eq!(new_range.primary.index.0, 3);

        // 2. Multi-line block tab indentation
        let mut block = "line1\nline2\nline3".to_string();
        let sel = CCursorRange::two(CCursor::new(0), CCursor::new(block.len()));
        let _ = apply_tab_indentation(&mut block, sel, false).unwrap();
        assert_eq!(block, "\tline1\n\tline2\n\tline3");

        // 3. Multi-line block outdent (Shift+Tab)
        let sel_out = CCursorRange::two(CCursor::new(0), CCursor::new(block.len()));
        let _ = apply_tab_indentation(&mut block, sel_out, true).unwrap();
        assert_eq!(block, "line1\nline2\nline3");

        // 4. Single line outdent with leading space
        let mut space_line = "    indented".to_string();
        let s_range = CCursorRange::one(CCursor::new(5));
        let new_s_range = apply_tab_indentation(&mut space_line, s_range, true).unwrap();
        assert_eq!(space_line, "indented");
        assert_eq!(new_s_range.primary.index.0, 1);

        // 5. Partial selection covering two lines
        let mut partial = "aaa\nbbb\nccc".to_string();
        let p_sel = CCursorRange::two(CCursor::new(2), CCursor::new(6)); // 'a' in line 1 to 'b' in line 2
        let _ = apply_tab_indentation(&mut partial, p_sel, false).unwrap();
        assert_eq!(partial, "\taaa\n\tbbb\nccc");
        let _ = apply_tab_indentation(&mut partial, p_sel, true).unwrap();
        assert_eq!(partial, "aaa\nbbb\nccc");
    }

    #[test]
    fn wikilink_does_not_overwrite_existing_file() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_wikilink_keep_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("existing.md");
        std::fs::write(&existing, "IMPORTANT ORIGINAL CONTENT").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        app.resolve_wiki_link("existing");

        // Ensure the content was NOT wiped
        let read_back = std::fs::read_to_string(&existing).unwrap();
        assert_eq!(read_back, "IMPORTANT ORIGINAL CONTENT");
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].content, "IMPORTANT ORIGINAL CONTENT");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_task_cache_reuses_mtime_and_skips_disk_read() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_task_cache_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note_path = dir.join("tasks.md");
        std::fs::write(&note_path, "- [ ] task 1 📅 2026-09-01\n").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();

        // 1. Initial scan: populates cache
        let tasks1 = app.scan_due_tasks(today).unwrap();
        assert_eq!(tasks1.len(), 1);
        assert!(app.task_cache.contains_key("tasks.md"));

        // 2. Unmodified file: reuses cache without re-reading
        let tasks2 = app.scan_due_tasks(today).unwrap();
        assert_eq!(tasks2.len(), 1);

        // 3. Modify file: updates cache
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&note_path, "- [ ] task 1 📅 2026-09-01\n- [ ] task 2 📅 2026-09-10\n").unwrap();
        let tasks3 = app.scan_due_tasks(today).unwrap();
        assert_eq!(tasks3.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preview_memoization_caches_rendered_output() {
        let mut tab = crate::tabs::Tab::untitled(1);
        tab.content = "# Hello World\nSee [[target]]".to_string();

        let mut app = KotoshelfApp::new();
        app.tabs.push(tab);
        let ctx = egui::Context::default();

        // 1. First render computes and memoizes
        let out1 = app.render_preview_markdown(&ctx, 0);
        assert!(out1.contains("[target](<wikilink:target>)"));
        assert!(app.tabs[0].preview_memo.is_some());

        // 2. Second call with unchanged content returns cached output
        let out2 = app.render_preview_markdown(&ctx, 0);
        assert_eq!(out1, out2);

        // 3. Modifying content invalidates cache and updates memo
        app.tabs[0].content = "# Updated\nSee [[target2]]".to_string();
        let out3 = app.render_preview_markdown(&ctx, 0);
        assert!(out3.contains("[target2](<wikilink:target2>)"));
    }

    #[test]
    fn test_rename_tab_by_title() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_title_rename_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note_path = dir.join("old_note.md");
        std::fs::write(&note_path, "# Original Note\n").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        app.open_file(crate::tabs::TabPath::Local(note_path.clone()));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].name, "old_note.md");
        assert_eq!(app.tabs[0].title_buffer, "old_note");

        // 1. Rename via title (without .md suffix)
        app.rename_tab_by_title(0, "new_renamed_note");
        assert_eq!(app.tabs[0].name, "new_renamed_note.md");
        assert_eq!(app.tabs[0].title_buffer, "new_renamed_note");
        assert!(dir.join("new_renamed_note.md").exists());
        assert!(!dir.join("old_note.md").exists());

        // 2. Rename via title (with .md suffix)
        app.rename_tab_by_title(0, "another_note.md");
        assert_eq!(app.tabs[0].name, "another_note.md");
        assert_eq!(app.tabs[0].title_buffer, "another_note");
        assert!(dir.join("another_note.md").exists());
        assert!(!dir.join("new_renamed_note.md").exists());

        // 3. Empty title reverts to current name
        app.rename_tab_by_title(0, "   ");
        assert_eq!(app.tabs[0].name, "another_note.md");
        assert_eq!(app.tabs[0].title_buffer, "another_note");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_video_attachment_and_find() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_video_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note_path = dir.join("note.md");
        let video_sample_path = dir.join("sample.mp4");
        std::fs::write(&video_sample_path, b"fake mp4 video bytes").unwrap();
        std::fs::write(&note_path, "# Video Note\n").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        app.open_file(crate::tabs::TabPath::Local(note_path.clone()));
        assert_eq!(app.tabs.len(), 1);

        let ctx = egui::Context::default();
        // Drop video file onto note
        app.handle_dropped_path(&ctx, video_sample_path.clone());

        // Check that video was attached into attachments/
        assert!(app.tabs[0].content.contains("![](attachments/vid-"));
        assert!(app.tabs[0].content.contains("-sample.mp4)"));

        // Check find_all_videos_in_tab
        let videos = app.find_all_videos_in_tab(0);
        assert_eq!(videos.len(), 1);
        assert!(videos[0].display_string().contains("vid-"));
        assert!(videos[0].display_string().ends_with("-sample.mp4"));

        // Check render preview markdown includes video uri
        let preview = app.render_preview_markdown(&ctx, 0);
        assert!(preview.contains("kotoshelf-video://"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_toggle_task_checkbox_on_line() {
        let mut content = "# Note\n- [ ] Task 1\n- [x] Task 2\n- [ ] Task with link [[source.md]]\n".to_string();

        // 1. Toggle line 1: unchecked -> checked
        let prop1 = toggle_task_checkbox_on_line(&mut content, 1);
        assert!(prop1.is_none());
        assert!(content.contains("- [x] Task 1"));

        // 2. Toggle line 2: checked -> unchecked
        let prop2 = toggle_task_checkbox_on_line(&mut content, 2);
        assert!(prop2.is_none());
        assert!(content.contains("- [ ] Task 2"));

        // 3. Toggle line 3: task with trailing link propagates to source
        let prop3 = toggle_task_checkbox_on_line(&mut content, 3);
        assert!(prop3.is_some());
        let (source, line) = prop3.unwrap();
        assert_eq!(source, "source.md");
        assert!(line.contains("- [x] Task with link"));
    }

    #[test]
    fn test_toggle_expanded_task() {
        let due_tasks = vec![
            crate::dashboard::DueTask {
                source_path: "todo.md".to_string(),
                section: Some("Work".to_string()),
                priority: crate::tasks::Priority::High,
                due: Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap()),
                scheduled: None,
                line: "- [ ] high task 📅 2026-10-06".to_string(),
            }
        ];
        let template = "# Daily\n\n{{task: priority}}\n";
        let mut expanded = crate::templates::expand_with_tasks(template, chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap().and_hms_opt(9, 0, 0).unwrap(), &due_tasks);
        println!("EXPANDED:\n{}", expanded);

        // Find the line index of the task in expanded
        let lines: Vec<&str> = expanded.lines().collect();
        let task_idx = lines.iter().position(|l| l.contains("- [ ] high task")).expect("task must be present in expanded text");
        println!("task_idx: {}, line: {}", task_idx, lines[task_idx]);

        let prop = toggle_task_checkbox_on_line(&mut expanded, task_idx);
        println!("After toggle:\n{}", expanded);
        assert!(prop.is_some(), "propagate should be Some");
        assert!(expanded.contains("- [x] high task"));
        assert!(!expanded.contains('✅'), "linked pointer task line should not get done-date stamp");

        // Toggle back: checked -> unchecked
        let prop_back = toggle_task_checkbox_on_line(&mut expanded, task_idx);
        assert!(prop_back.is_some(), "unchecking linked task should also propagate");
        let (source, line_back) = prop_back.unwrap();
        assert_eq!(source, "todo.md");
        assert!(line_back.contains("- [ ] high task"));
        assert!(expanded.contains("- [ ] high task"));
    }

    #[test]
    fn uncompleting_linked_task_reverts_source_task_including_done_date_recurrence_and_delete() {
        let dir = std::env::temp_dir().join(format!(
            "kotoshelf_uncomplete_prop_{}_{}",
            std::process::id(),
            generate_task_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // 1. Dated task with metadata (gets `✅ YYYY-MM-DD` on completion)
        let shopping_path = dir.join("shopping.md");
        std::fs::write(
            &shopping_path,
            "# Shopping\n- [ ] buy milk 📅 2026-08-01 🔼\n",
        )
        .unwrap();

        // 2. Recurring task without `🏁 delete` (inserts next cycle above and stamps `✅` on completion)
        let routine_path = dir.join("routine.md");
        std::fs::write(
            &routine_path,
            "# Habits\n- [ ] stretch 📅 2026-08-20 🔁 every day\n",
        )
        .unwrap();

        // 3. Recurring task with `🏁 delete` (replaces line with next cycle on completion)
        let cycle_path = dir.join("cycle.md");
        std::fs::write(
            &cycle_path,
            "# Daily\n- [ ] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete\n",
        )
        .unwrap();

        // 4. One-off task with `🏁 delete` (deletes line on completion)
        let oneoff_path = dir.join("oneoff.md");
        std::fs::write(
            &oneoff_path,
            "# Errands\n- [ ] return library book 📅 2026-08-10 🏁 delete\n",
        )
        .unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());

        // Case 1: Dated task complete -> uncomplete
        app.propagate_check_to_source(
            "shopping.md",
            "- [x] buy milk 📅 2026-08-01 🔼 [[shopping.md#Shopping]]",
        );
        let after_check_1 = std::fs::read_to_string(&shopping_path).unwrap();
        assert!(after_check_1.contains("- [x] buy milk 📅 2026-08-01 🔼 ✅ "));
        app.propagate_check_to_source(
            "shopping.md",
            "- [ ] buy milk 📅 2026-08-01 🔼 [[shopping.md#Shopping]]",
        );
        let after_uncheck_1 = std::fs::read_to_string(&shopping_path).unwrap();
        assert_eq!(after_uncheck_1, "# Shopping\n- [ ] buy milk 📅 2026-08-01 🔼\n");

        // Case 2: Recurring task complete -> uncomplete (removes auto-inserted next cycle and unchecks original)
        app.propagate_check_to_source(
            "routine.md",
            "- [x] stretch 📅 2026-08-20 🔁 every day [[routine.md#Habits]]",
        );
        let after_check_2 = std::fs::read_to_string(&routine_path).unwrap();
        assert!(after_check_2.contains("- [ ] stretch 📅 2026-08-21 🔁 every day"));
        assert!(after_check_2.contains("- [x] stretch 📅 2026-08-20 🔁 every day ✅ "));
        app.propagate_check_to_source(
            "routine.md",
            "- [ ] stretch 📅 2026-08-20 🔁 every day [[routine.md#Habits]]",
        );
        let after_uncheck_2 = std::fs::read_to_string(&routine_path).unwrap();
        assert_eq!(
            after_uncheck_2,
            "# Habits\n- [ ] stretch 📅 2026-08-20 🔁 every day\n"
        );

        // Case 3: Recurring task with `🏁 delete` complete -> uncomplete (reverts shifted date back to original)
        app.propagate_check_to_source(
            "cycle.md",
            "- [x] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete [[cycle.md#Daily]]",
        );
        let after_check_3 = std::fs::read_to_string(&cycle_path).unwrap();
        assert_eq!(
            after_check_3,
            "# Daily\n- [ ] cycle1 🔼 📅 2026-08-15 🔁 every day 🏁 delete\n"
        );
        app.propagate_check_to_source(
            "cycle.md",
            "- [ ] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete [[cycle.md#Daily]]",
        );
        let after_uncheck_3 = std::fs::read_to_string(&cycle_path).unwrap();
        assert_eq!(
            after_uncheck_3,
            "# Daily\n- [ ] cycle1 🔼 📅 2026-08-14 🔁 every day 🏁 delete\n"
        );

        // Case 4: One-off task with `🏁 delete` complete -> uncomplete (re-inserts into original section)
        app.propagate_check_to_source(
            "oneoff.md",
            "- [x] return library book 📅 2026-08-10 🏁 delete [[oneoff.md#Errands]]",
        );
        let after_check_4 = std::fs::read_to_string(&oneoff_path).unwrap();
        assert_eq!(after_check_4, "# Errands\n");
        app.propagate_check_to_source(
            "oneoff.md",
            "- [ ] return library book 📅 2026-08-10 🏁 delete [[oneoff.md#Errands]]",
        );
        let after_uncheck_4 = std::fs::read_to_string(&oneoff_path).unwrap();
        assert_eq!(
            after_uncheck_4,
            "# Errands\n- [ ] return library book 📅 2026-08-10 🏁 delete\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn multi_tab_selection_and_tiling_supports_three_or_more_tabs() {
        let mut app = KotoshelfApp::new();
        for name in ["a.md", "b.md", "c.md", "d.md"] {
            let id = app.alloc_tab_id();
            app.tabs.push(Tab {
                id,
                name: name.to_string(),
                title_buffer: name.trim_end_matches(".md").to_string(),
                path: None,
                content: format!("# {name}\nline 1\nline 2\n"),
                dirty: false,
                preview_memo: None,
            });
        }
        app.active = Some(0);
        let id0 = app.tabs[0].id;
        let id1 = app.tabs[1].id;
        let id2 = app.tabs[2].id;
        let id3 = app.tabs[3].id;

        // Ctrl+Click on tab 1 when tab 0 is active selects both tab 0 and tab 1
        app.toggle_select_tab(1);
        assert_eq!(app.selected_tab_ids, vec![id0, id1]);

        // Ctrl+Click on tab 2 adds a 3rd tab
        app.toggle_select_tab(2);
        assert_eq!(app.selected_tab_ids, vec![id0, id1, id2]);

        // Tile selected tabs -> 3 columns side-by-side
        app.tile_selected_tabs();
        assert_eq!(app.split_tab_ids, vec![id0, id1, id2]);

        // Add 4th tab to split view
        app.add_tab_to_split(3);
        assert_eq!(app.split_tab_ids, vec![id0, id1, id2, id3]);

        // Remove tab 1 from split view -> 3 columns remain
        app.remove_tab_from_split(1);
        assert_eq!(app.split_tab_ids, vec![id0, id2, id3]);

        // Close tab 2 (index 2, id2) -> 2 columns remain
        app.close_tab_now(2);
        assert_eq!(app.split_tab_ids, vec![id0, id3]);

        // Close tab 2 (now index 2, id3) -> drops below 2 columns, automatically exits split view
        app.close_tab_now(2);
        assert!(app.split_tab_ids.is_empty());
    }

    #[test]
    fn range_select_tabs_selects_inclusive_span() {
        let mut app = KotoshelfApp::new();
        for name in ["1.md", "2.md", "3.md", "4.md"] {
            let id = app.alloc_tab_id();
            app.tabs.push(Tab {
                id,
                name: name.to_string(),
                title_buffer: name.to_string(),
                path: None,
                content: String::new(),
                dirty: false,
                preview_memo: None,
            });
        }
        app.active = Some(1);
        app.range_select_tabs(3);
        assert_eq!(
            app.selected_tab_ids,
            vec![app.tabs[1].id, app.tabs[2].id, app.tabs[3].id]
        );
        app.tile_selected_tabs();
        assert_eq!(app.split_tab_ids.len(), 3);
        app.untile_tabs();
        assert!(app.split_tab_ids.is_empty());
        assert!(app.selected_tab_ids.is_empty());
    }

    #[test]
    fn dropping_multiple_files_places_each_media_link_on_its_own_line_with_trailing_newline() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_multi_drop_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let note_path = dir.join("note.md");
        std::fs::write(&note_path, "# My Note").unwrap();

        let img1 = dir.join("one.png");
        let img2 = dir.join("two.png");
        let img3 = dir.join("three.png");
        std::fs::write(&img1, b"png1").unwrap();
        std::fs::write(&img2, b"png2").unwrap();
        std::fs::write(&img3, b"png3").unwrap();

        let mut app = KotoshelfApp::new();
        app.workspace = Some(dir.clone());
        let ctx = egui::Context::default();
        app.open_file(crate::tabs::TabPath::Local(note_path.clone()));

        app.handle_dropped_path(&ctx, img1);
        app.handle_dropped_path(&ctx, img2);
        app.handle_dropped_path(&ctx, img3);

        let content = &app.tabs[0].content;
        let lines: Vec<&str> = content.lines().collect();
        // Line 0: "# My Note"
        // Line 1: first media link, followed by 6 blank lines (7 '\n's total)
        // Line 8: second media link, followed by 6 blank lines
        // Line 15: third media link, followed by 6 blank lines
        assert_eq!(lines[0], "# My Note");
        assert!(lines[1].starts_with("![](attachments/img-") && lines[1].ends_with("-one.png)"));
        for blank in &lines[2..8] {
            assert!(blank.is_empty(), "expected blank padding line for thumbnail height, got {blank:?}");
        }
        assert!(lines[8].starts_with("![](attachments/img-") && lines[8].ends_with("-two.png)"));
        for blank in &lines[9..15] {
            assert!(blank.is_empty(), "expected blank padding line for thumbnail height, got {blank:?}");
        }
        assert!(lines[15].starts_with("![](attachments/img-") && lines[15].ends_with("-three.png)"));
        assert!(content.ends_with("\n\n\n\n\n\n\n"), "each media link must be followed by 7 newlines");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn close_tabs_right_and_left_include_clicked_tab() {
        let mut app = KotoshelfApp::new();
        for name in ["0.md", "1.md", "2.md", "3.md", "4.md"] {
            let id = app.alloc_tab_id();
            app.tabs.push(Tab {
                id,
                name: name.to_string(),
                title_buffer: name.to_string(),
                path: None,
                content: String::new(),
                dirty: false,
                preview_memo: None,
            });
        }
        app.active = Some(3);

        // Close tab 3 and everything to its right -> keeps 0.md, 1.md, 2.md (active becomes index 2)
        app.request_close_right(3);
        let remaining_after_right: Vec<&str> = app.tabs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(remaining_after_right, vec!["0.md", "1.md", "2.md"]);
        assert_eq!(app.active, Some(2));

        // Close tab 1 and everything to its left -> closes 0.md and 1.md, keeps only 2.md (active becomes index 0)
        app.request_close_left(1);
        let remaining_after_left: Vec<&str> = app.tabs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(remaining_after_left, vec!["2.md"]);
        assert_eq!(app.active, Some(0));
    }

    #[test]
    fn git_workspace_prompts_for_kotoshelf_and_gitignore_before_creating_dir() {
        use super::KotoshelfDirPolicy;
        let dir = std::env::temp_dir().join(format!(
            "kotoshelf_git_gitignore_prompt_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();

        let mut app = KotoshelfApp::new();
        app.open_workspace_at(dir.clone());

        // Dialog must be shown and .kotoshelf must NOT be created yet (even though home.md was created)
        assert!(app.gitignore_confirm_dialog.is_some());
        assert_eq!(app.kotoshelf_dir_policy, KotoshelfDirPolicy::Undecided);
        assert!(
            !dir.join(".kotoshelf").exists(),
            ".kotoshelf should not be created before user confirms dialog"
        );

        // User confirms: create .kotoshelf and add to .gitignore
        app.apply_gitignore_confirm_decision(KotoshelfDirPolicy::AllowedWithGitignore, 1.0);
        assert!(app.gitignore_confirm_dialog.is_none());
        assert_eq!(
            app.kotoshelf_dir_policy,
            KotoshelfDirPolicy::AllowedWithGitignore
        );
        assert!(dir.join(".kotoshelf").is_dir());
        assert!(dir.join(".kotoshelf").join("history").is_dir());
        assert!(dir.join(".kotoshelf").join("settings.json").is_file());
        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(gitignore.lines().any(|l| l.trim() == ".kotoshelf/"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_workspace_denied_policy_never_creates_kotoshelf_or_gitignore() {
        use super::KotoshelfDirPolicy;
        let dir = std::env::temp_dir().join(format!(
            "kotoshelf_git_gitignore_denied_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();

        let mut app = KotoshelfApp::new();
        app.open_workspace_at(dir.clone());
        assert!(app.gitignore_confirm_dialog.is_some());

        // User declines creating .kotoshelf
        app.apply_gitignore_confirm_decision(KotoshelfDirPolicy::Denied, 1.0);
        assert!(app.gitignore_confirm_dialog.is_none());
        assert_eq!(app.kotoshelf_dir_policy, KotoshelfDirPolicy::Denied);

        // Even after saving settings or writing a file, .kotoshelf and .gitignore must remain absent
        app.save_current_workspace_settings(1.25);
        app.fs_write(
            &crate::tabs::TabPath::Local(dir.join("home.md")),
            "# Updated Home",
        )
        .unwrap();
        assert!(!dir.join(".kotoshelf").exists());
        assert!(!dir.join(".gitignore").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_workspace_create_only_records_gitignore_prompted_and_skips_dialog_next_time() {
        use super::KotoshelfDirPolicy;
        let dir = std::env::temp_dir().join(format!(
            "kotoshelf_git_gitignore_prompted_once_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();

        // 1. First open: .kotoshelf does not exist -> dialog shown
        let mut app = KotoshelfApp::new();
        app.open_workspace_at(dir.clone());
        assert!(app.gitignore_confirm_dialog.is_some());

        // User chooses "Create Only" (AllowedWithoutGitignore)
        app.apply_gitignore_confirm_decision(KotoshelfDirPolicy::AllowedWithoutGitignore, 1.0);
        assert!(dir.join(".kotoshelf").is_dir());
        assert!(!dir.join(".gitignore").exists());
        let saved =
            crate::workspace::load_workspace_settings(&dir).expect("settings.json should exist");
        assert!(saved.gitignore_prompted);

        // 2. Re-open the same workspace: .kotoshelf exists, .gitignore does not,
        //    but settings.json has gitignore_prompted = true -> NO dialog shown
        let mut app2 = KotoshelfApp::new();
        app2.open_workspace_at(dir.clone());
        assert!(
            app2.gitignore_confirm_dialog.is_none(),
            "should not prompt again once gitignore_prompted is saved in settings.json"
        );
        assert_eq!(
            app2.kotoshelf_dir_policy,
            KotoshelfDirPolicy::AllowedWithoutGitignore
        );

        // 3. Legacy workspace where .kotoshelf exists, .gitignore has no entry, and settings.json
        //    either does not exist or has gitignore_prompted = false -> dialog IS shown
        let _ = std::fs::remove_file(dir.join(".kotoshelf").join("settings.json"));
        let mut app3 = KotoshelfApp::new();
        app3.open_workspace_at(dir.clone());
        assert!(
            app3.gitignore_confirm_dialog.is_some(),
            "should prompt when .kotoshelf exists without .gitignore entry and settings.json lacks gitignore_prompted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_entry_name_rejects_escapes_but_allows_subfolders() {
        for bad in ["", "  ", "../x.md", "a/../../x", "a\\..\\x", "/etc/x", "\\x", "C:\\x", "C:x"] {
            assert!(crate::app::validate_entry_name(bad).is_err(), "should reject {bad:?}");
        }
        for ok in ["note.md", "sub/new.md", "a b/c.md", "..hidden", "日本語.md", "v1.2..3.md"] {
            assert!(crate::app::validate_entry_name(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn ensure_and_open_homepage_follows_template_index_rules() {
        let dir = std::env::temp_dir().join(format!(
            "kotoshelf_index_template_rules_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("templates")).unwrap();

        // 1. template_index.md 無し (template_homepage.md があっても無視) -> なにもせず
        std::fs::write(
            dir.join("templates").join("template_homepage.md"),
            "# Legacy Homepage {{today}}\n",
        )
        .unwrap();
        let mut app = KotoshelfApp::new();
        app.open_workspace_at(dir.clone());
        assert!(!dir.join("index.md").exists());
        assert!(app.tabs.is_empty());

        // 1b. ルート直下でない Templates/ 内の template_index.md は無視 -> なにもせず
        std::fs::create_dir_all(dir.join("notes").join("Templates")).unwrap();
        std::fs::write(
            dir.join("notes").join("Templates").join("template_index.md"),
            "# Nested {{today}}\n",
        )
        .unwrap();
        let mut app_nested = KotoshelfApp::new();
        app_nested.open_workspace_at(dir.clone());
        assert!(!dir.join("index.md").exists());

        // 2. template_index.md 有り & index.md 無し -> template_index.md から新規作成
        std::fs::write(
            dir.join("templates").join("template_index.md"),
            "# Daily Index {{today}}\n",
        )
        .unwrap();
        let mut app2 = KotoshelfApp::new();
        app2.open_workspace_at(dir.clone());
        assert!(dir.join("index.md").is_file());
        let created_content = std::fs::read_to_string(dir.join("index.md")).unwrap();
        assert!(created_content.starts_with("# Daily Index "));

        // 3. template_index.md 有り & index.md 有り (当日) -> 上書きせずそのまま
        std::fs::write(dir.join("index.md"), "# User Edited Today\n").unwrap();
        let mut app3 = KotoshelfApp::new();
        app3.open_workspace_at(dir.clone());
        let after_same_day = std::fs::read_to_string(dir.join("index.md")).unwrap();
        assert_eq!(after_same_day, "# User Edited Today\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

