use std::path::PathBuf;

/// Where a tab's content lives on disk - deliberately two different
/// shapes, not one `PathBuf`. `Ssh` paths are workspace-relative POSIX
/// strings ("" would be the workspace root, though a *file* tab is
/// never literally at the root); on Windows, storing that in a `PathBuf`
/// and joining onto it would render with `\`, corrupting every remote
/// shell script built from it. Mirrors the original React app's tab
/// paths being plain strings dispatched on `workspaceKind` at the
/// point of use, just made explicit as a type here instead of implicit
/// in which app state happens to be active when a tab was opened.
#[derive(Clone, Debug, PartialEq)]
pub enum TabPath {
    Local(PathBuf),
    Ssh(String),
}

impl TabPath {
    pub fn extension(&self) -> Option<String> {
        match self {
            TabPath::Local(p) => p.extension().map(|e| e.to_string_lossy().into_owned()),
            TabPath::Ssh(rel) => rel.rsplit_once('.').map(|(_, ext)| ext.to_string()),
        }
    }

    pub fn file_name(&self) -> String {
        match self {
            TabPath::Local(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned()),
            TabPath::Ssh(rel) => rel.rsplit('/').next().unwrap_or(rel).to_string(),
        }
    }

    pub fn display_string(&self) -> String {
        match self {
            TabPath::Local(p) => p.display().to_string(),
            TabPath::Ssh(rel) => rel.clone(),
        }
    }

    /// Builds the path for `name` inside this directory - `self` is
    /// assumed to be a directory (a tree node's own path if it's a
    /// folder, or a file's parent otherwise; see `TabPath::parent`).
    pub fn join(&self, name: &str) -> TabPath {
        match self {
            TabPath::Local(p) => TabPath::Local(p.join(name)),
            TabPath::Ssh(rel) => TabPath::Ssh(if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") }),
        }
    }

    /// The containing directory - the workspace root's own "parent" is
    /// itself, since every caller of this immediately wants *a*
    /// directory to build a new child path under, not `Option::None` to
    /// handle specially.
    pub fn parent(&self) -> TabPath {
        match self {
            TabPath::Local(p) => TabPath::Local(p.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| p.clone())),
            TabPath::Ssh(rel) => match rel.rsplit_once('/') {
                Some((parent, _)) => TabPath::Ssh(parent.to_string()),
                None => TabPath::Ssh(String::new()),
            },
        }
    }

    /// Resolves `target` (a possibly multi-segment relative path, using
    /// `.`/`..` freely - e.g. an image reference like
    /// `../shared/diagram.png`) against this directory. Unlike `join`
    /// (a single path segment, used for tree "new file/folder" prompts,
    /// which never needs dot-segment handling), this normalizes `.`/`..`
    /// in `target` itself. For a local path, normalization piggybacks on
    /// `canonicalize()` (touches the filesystem - fine here since the
    /// caller is about to read the file anyway); for SSH, normalized
    /// purely as string segments since there's no local filesystem to
    /// ask.
    pub fn join_normalized(&self, target: &str) -> TabPath {
        let norm_target = target.replace('\\', "/");
        match self {
            TabPath::Local(dir) => {
                let joined = dir.join(&norm_target);
                let resolved = joined.canonicalize().unwrap_or(joined);
                TabPath::Local(resolved)
            }
            TabPath::Ssh(dir) => {
                let mut segments: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
                for seg in norm_target.split('/') {
                    match seg {
                        "" | "." => {}
                        ".." => {
                            segments.pop();
                        }
                        s => segments.push(s),
                    }
                }
                TabPath::Ssh(segments.join("/"))
            }
        }
    }

    /// Returns `true` if `self` is located within or equal to `root` directory,
    /// preventing path traversal escapes outside the workspace root boundary.
    pub fn is_within_root(&self, root: &TabPath) -> bool {
        match (self, root) {
            (TabPath::Local(p), TabPath::Local(r)) => {
                let p_canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                let r_canon = r.canonicalize().unwrap_or_else(|_| r.clone());
                p_canon.starts_with(&r_canon) || p.starts_with(r)
            }
            (TabPath::Ssh(p), TabPath::Ssh(r)) => {
                let p_norm = p.trim_start_matches('/');
                let r_norm = r.trim_start_matches('/');
                if r_norm.is_empty() {
                    !p_norm.starts_with("../") && !p_norm.contains("/../")
                } else {
                    p_norm == r_norm || p_norm.starts_with(&format!("{r_norm}/"))
                }
            }
            _ => false,
        }
    }

    /// If `self` is strictly inside directory `dir`, returns the
    /// remaining suffix (no leading separator) - used to follow a
    /// folder rename into every open tab nested under it.
    pub fn strip_prefix_dir(&self, dir: &TabPath) -> Option<String> {
        match (self, dir) {
            (TabPath::Local(p), TabPath::Local(d)) => p.strip_prefix(d).ok().map(|r| r.to_string_lossy().replace('\\', "/")),
            (TabPath::Ssh(p), TabPath::Ssh(d)) => {
                let prefix = if d.is_empty() { String::new() } else { format!("{d}/") };
                p.strip_prefix(&prefix).filter(|rest| !rest.is_empty()).map(|s| s.to_string())
            }
            _ => None,
        }
    }
}

pub struct Tab {
    /// Stable identity for this tab's lifetime, assigned by the caller
    /// (`KotoshelfApp::next_tab_id`) and never reused - used to key the
    /// editor widget's persisted state (cursor position, undo history)
    /// so it doesn't leak across tabs. Not the same as this tab's index
    /// in `tabs`, which shifts as other tabs close.
    pub id: u64,
    /// `None` = a new, never-saved-to-disk buffer ("Untitled").
    pub path: Option<TabPath>,
    pub name: String,
    pub content: String,
    pub dirty: bool,
    /// Memoized preview rendering: `(source_content, rendered_preview)`.
    /// Recomputed only when `content` changes, eliminating 60fps per-frame regex overhead.
    pub preview_memo: Option<(String, String)>,
    /// Buffer for editing note title header in editor
    pub title_buffer: String,
}

impl Tab {
    pub fn untitled(id: u64) -> Self {
        Self {
            id,
            path: None,
            name: "Untitled".to_string(),
            content: String::new(),
            dirty: false,
            preview_memo: None,
            title_buffer: "Untitled".to_string(),
        }
    }

    pub fn from_file(id: u64, path: TabPath, content: String) -> Self {
        let name = path.file_name();
        let title_buffer = name.strip_suffix(".md").unwrap_or(&name).to_string();
        Self {
            id,
            path: Some(path),
            name,
            content,
            dirty: false,
            preview_memo: None,
            title_buffer,
        }
    }
}

pub enum TabBarAction {
    Activate(usize),
    ToggleSelect(usize),
    RangeSelect(usize),
    TileSelected,
    TileWithActive(usize),
    TileAll,
    AddToSplit(usize),
    RemoveFromSplit(usize),
    Untile,
    CloseRequested(usize),
    CloseOtherRequested(usize),
    CloseRightRequested(usize),
    CloseLeftRequested(usize),
    CloseAllRequested,
}

/// Truncates tab name if it exceeds `max_chars` to keep tab width compact, appending ellipsis `…`.
pub fn truncate_tab_name(name: &str, max_chars: usize) -> String {
    let char_count = name.chars().count();
    if char_count <= max_chars {
        name.to_string()
    } else {
        let prefix: String = name.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}

/// Renders the tab strip, returning at most one action for the caller to
/// apply - kept as data instead of mutating `tabs`/`active` directly here
/// so the caller (which also owns close-confirmation state) stays the
/// single place that decides what a close request actually does.
pub fn show_tab_bar(
    ui: &mut egui::Ui,
    tabs: &[Tab],
    active: Option<usize>,
    selected_tab_ids: &[u64],
    split_tab_ids: &[u64],
) -> Option<TabBarAction> {
    let mut action = None;
    let total_tabs = tabs.len();
    let is_split_active = split_tab_ids.len() >= 2;
    let multi_select_count = selected_tab_ids.len();

    egui::ScrollArea::horizontal()
        .id_salt("tabbar_scroll")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in tabs.iter().enumerate() {
                    let is_active = active == Some(i);
                    let is_multi_selected = selected_tab_ids.contains(&tab.id);
                    let split_pos = if is_split_active {
                        split_tab_ids.iter().position(|&id| id == tab.id)
                    } else {
                        None
                    };
                    let dirty_mark = if tab.dirty { "* " } else { "" };
                    let truncated = truncate_tab_name(&tab.name, 18);
                    let split_badge = split_pos.map(|pos| format!("[{}] ", pos + 1)).unwrap_or_default();
                    let select_badge = if multi_select_count >= 2 && is_multi_selected {
                        "☑ "
                    } else {
                        ""
                    };
                    let base_tooltip = tab.path.as_ref().map(|p| p.display_string()).unwrap_or_else(|| tab.name.clone());
                    let tooltip = format!("{base_tooltip}\n(Ctrl+Click / Shift+Click: 複数タブ選択)");

                    let mut frame = egui::Frame::group(ui.style());
                    if is_multi_selected && multi_select_count >= 2 {
                        frame.stroke = egui::Stroke::new(1.5, ui.visuals().selection.bg_fill);
                    } else if split_pos.is_some() {
                        frame.stroke = egui::Stroke::new(1.2, ui.visuals().hyperlink_color);
                    }

                    frame.show(ui, |ui| {
                        let label_text = format!("{select_badge}{split_badge}{dirty_mark}{truncated}");
                        let resp = ui
                            .selectable_label(is_active || (multi_select_count >= 2 && is_multi_selected), label_text)
                            .on_hover_text(tooltip);
                        if resp.clicked() {
                            let modifiers = ui.input(|inp| inp.modifiers);
                            if modifiers.command || modifiers.ctrl {
                                action = Some(TabBarAction::ToggleSelect(i));
                            } else if modifiers.shift {
                                action = Some(TabBarAction::RangeSelect(i));
                            } else {
                                action = Some(TabBarAction::Activate(i));
                            }
                        }
                        resp.context_menu(|ui| {
                            if multi_select_count >= 2 {
                                if ui.button(format!("◫ タブを並べる ({multi_select_count}個)")).clicked() {
                                    action = Some(TabBarAction::TileSelected);
                                    ui.close();
                                }
                                ui.separator();
                            } else if total_tabs > 1 {
                                if is_split_active {
                                    if split_pos.is_none() {
                                        if ui.button("◫ 並べて表示にこのタブを追加").clicked() {
                                            action = Some(TabBarAction::AddToSplit(i));
                                            ui.close();
                                        }
                                    } else if ui.button("並べて表示からこのタブを外す").clicked() {
                                        action = Some(TabBarAction::RemoveFromSplit(i));
                                        ui.close();
                                    }
                                } else if active.is_some() && active != Some(i) {
                                    if ui.button("◫ アクティブタブと並べる").clicked() {
                                        action = Some(TabBarAction::TileWithActive(i));
                                        ui.close();
                                    }
                                }
                                if total_tabs >= 2 && ui.button(format!("◫ 開いているタブをすべて並べる ({total_tabs}個)")).clicked() {
                                    action = Some(TabBarAction::TileAll);
                                    ui.close();
                                }
                                if is_split_active && ui.button("✕ 並べて表示を解除").clicked() {
                                    action = Some(TabBarAction::Untile);
                                    ui.close();
                                }
                                ui.separator();
                            }

                            if ui.button("Close Tab").clicked() {
                                action = Some(TabBarAction::CloseRequested(i));
                                ui.close();
                            }
                            if total_tabs > 1 && ui.button("Close Other Tabs").clicked() {
                                action = Some(TabBarAction::CloseOtherRequested(i));
                                ui.close();
                            }
                            if i + 1 < total_tabs && ui.button("Close Tabs to the Right").clicked() {
                                action = Some(TabBarAction::CloseRightRequested(i));
                                ui.close();
                            }
                            if i > 0 && ui.button("Close Tabs to the Left").clicked() {
                                action = Some(TabBarAction::CloseLeftRequested(i));
                                ui.close();
                            }
                            if ui.button("Close All Tabs").clicked() {
                                action = Some(TabBarAction::CloseAllRequested);
                                ui.close();
                            }
                        });

                        if ui.small_button("x").clicked() {
                            action = Some(TabBarAction::CloseRequested(i));
                        }
                    });
                }

                if multi_select_count >= 2 {
                    ui.separator();
                    if ui
                        .button(format!("◫ タブを並べる ({multi_select_count})"))
                        .on_hover_text("選択したタブを横に並べて表示します")
                        .clicked()
                    {
                        action = Some(TabBarAction::TileSelected);
                    }
                }
                if is_split_active {
                    ui.separator();
                    if ui
                        .button("✕ 並べて表示を解除")
                        .on_hover_text("1画面（単一タブ）表示に戻します")
                        .clicked()
                    {
                        action = Some(TabBarAction::Untile);
                    }
                }
            });
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_join_builds_a_posix_relative_path() {
        assert_eq!(TabPath::Ssh(String::new()).join("notes.md"), TabPath::Ssh("notes.md".to_string()));
        assert_eq!(TabPath::Ssh("notes".to_string()).join("today.md"), TabPath::Ssh("notes/today.md".to_string()));
    }

    #[test]
    fn local_join_uses_pathbuf_join() {
        let dir = TabPath::Local(PathBuf::from("C:\\ws"));
        assert_eq!(dir.join("notes.md"), TabPath::Local(PathBuf::from("C:\\ws\\notes.md")));
    }

    #[test]
    fn ssh_join_normalized_handles_dot_and_dotdot_segments() {
        let dir = TabPath::Ssh("notes/sub".to_string());
        assert_eq!(dir.join_normalized("img.png"), TabPath::Ssh("notes/sub/img.png".to_string()));
        assert_eq!(dir.join_normalized("./img.png"), TabPath::Ssh("notes/sub/img.png".to_string()));
        assert_eq!(dir.join_normalized("../shared/img.png"), TabPath::Ssh("notes/shared/img.png".to_string()));
        assert_eq!(dir.join_normalized("../../top.png"), TabPath::Ssh("top.png".to_string()));
    }

    #[test]
    fn ssh_join_normalized_does_not_pop_past_the_root() {
        let root = TabPath::Ssh(String::new());
        assert_eq!(root.join_normalized("../img.png"), TabPath::Ssh("img.png".to_string()));
    }

    #[test]
    fn ssh_parent_of_a_top_level_file_is_the_root() {
        assert_eq!(TabPath::Ssh("notes.md".to_string()).parent(), TabPath::Ssh(String::new()));
    }

    #[test]
    fn ssh_parent_of_a_nested_file_is_its_directory() {
        assert_eq!(TabPath::Ssh("notes/today.md".to_string()).parent(), TabPath::Ssh("notes".to_string()));
    }

    #[test]
    fn ssh_strip_prefix_dir_finds_descendants_of_a_renamed_folder() {
        let dir = TabPath::Ssh("notes".to_string());
        let nested = TabPath::Ssh("notes/sub/today.md".to_string());
        assert_eq!(nested.strip_prefix_dir(&dir), Some("sub/today.md".to_string()));
        // The dir itself isn't its own descendant.
        assert_eq!(dir.strip_prefix_dir(&dir), None);
        // An unrelated path isn't a descendant either.
        assert_eq!(TabPath::Ssh("other/today.md".to_string()).strip_prefix_dir(&dir), None);
    }

    #[test]
    fn ssh_strip_prefix_dir_from_root() {
        let root = TabPath::Ssh(String::new());
        assert_eq!(TabPath::Ssh("notes/today.md".to_string()).strip_prefix_dir(&root), Some("notes/today.md".to_string()));
    }

    #[test]
    fn local_strip_prefix_dir_normalizes_to_forward_slashes() {
        let dir = TabPath::Local(PathBuf::from("C:\\ws\\notes"));
        let nested = TabPath::Local(PathBuf::from("C:\\ws\\notes\\sub\\today.md"));
        assert_eq!(nested.strip_prefix_dir(&dir), Some("sub/today.md".to_string()));
    }

    #[test]
    fn extension_and_file_name_work_for_both_variants() {
        let local = TabPath::Local(PathBuf::from("C:\\ws\\notes\\today.md"));
        assert_eq!(local.extension(), Some("md".to_string()));
        assert_eq!(local.file_name(), "today.md");

        let ssh = TabPath::Ssh("notes/today.md".to_string());
        assert_eq!(ssh.extension(), Some("md".to_string()));
        assert_eq!(ssh.file_name(), "today.md");
    }

    #[test]
    fn is_within_root_checks_boundary() {
        let root = TabPath::Local(PathBuf::from("/workspace"));
        let inside = TabPath::Local(PathBuf::from("/workspace/sub/note.md"));
        let outside = TabPath::Local(PathBuf::from("/etc/passwd"));
        assert!(inside.is_within_root(&root));
        assert!(!outside.is_within_root(&root));

        let ssh_root = TabPath::Ssh("".to_string());
        let ssh_inside = TabPath::Ssh("notes/sub/doc.md".to_string());
        let ssh_outside = TabPath::Ssh("../outside.md".to_string());
        assert!(ssh_inside.is_within_root(&ssh_root));
        assert!(!ssh_outside.is_within_root(&ssh_root));
    }

    #[test]
    fn test_truncate_tab_name() {
        assert_eq!(truncate_tab_name("short.md", 18), "short.md");
        assert_eq!(truncate_tab_name("123456789012345678", 18), "123456789012345678");
        assert_eq!(truncate_tab_name("1234567890123456789", 18), "12345678901234567…");
        assert_eq!(truncate_tab_name("日本語のノートタイトルがとても長いです.md", 10), "日本語のノートタイ…");
    }
}
