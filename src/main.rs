// Suppresses the console window Windows otherwise opens behind a plain
// Rust binary (default "console" subsystem) when launched from Explorer/
// a Start Menu shortcut - confirmed live after installing the packaged
// build. Left off in debug builds so `eprintln!`/`println!` debugging
// during development still has somewhere to go.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod app;
mod attachments;
mod dashboard;
mod find;
mod fonts;
mod git;
#[cfg(windows)]
mod ime_win;
mod images;
mod pdf;
mod secure_tokens;
mod ssh;
mod syntax;
mod tabs;
mod tasks;
mod templates;
mod theme;
mod video;
mod wikilink;
mod workspace;

use app::KotoshelfApp;
use std::path::PathBuf;
use tabs::TabPath;

/// What `kotoshelf <arg>` on the command line should do, resolved
/// once at startup. Mirrors the original React app's `InitialTarget`/
/// `resolve_initial_target`.
enum InitialTarget {
    /// Open this directory as the workspace.
    Workspace(PathBuf),
    /// Open this file as a tab, and its parent directory as the
    /// workspace (so the tree is populated).
    File { path: PathBuf, workspace: PathBuf },
    /// `kotoshelf --ssh <profile-name>` - the CLI equivalent of
    /// picking a saved profile in "Open Remote Workspace…" and hitting
    /// Connect.
    SshProfile(ssh::SshProfile),
}

/// Relative paths (including ".") are resolved against the shell's cwd,
/// not the app's install directory - otherwise `kotoshelf .`
/// would mean something different depending on where the binary happens
/// to live.
fn resolve_initial_target() -> Option<InitialTarget> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Checked before the generic positional-arg scan below so `--ssh`
    // itself (which starts with '-') doesn't fall through to being
    // skipped as "looks like a flag".
    if let Some(idx) = args.iter().position(|a| a == "--ssh") {
        let name = args.get(idx + 1)?;
        let profile = ssh::load_app_config().ssh_profiles.into_iter().find(|p| &p.name == name)?;
        return Some(InitialTarget::SshProfile(profile));
    }

    // First positional argument, skipping anything that looks like a
    // flag.
    let arg = args.into_iter().find(|a| !a.starts_with('-'))?;
    let cwd = std::env::current_dir().ok()?;
    let resolved = cwd.join(&arg);
    let canonical = resolved.canonicalize().unwrap_or(resolved);
    if canonical.is_dir() {
        Some(InitialTarget::Workspace(canonical))
    } else if canonical.is_file() {
        let workspace = canonical.parent()?.to_path_buf();
        Some(InitialTarget::File { path: canonical, workspace })
    } else {
        None // arg didn't resolve to anything on disk - ignore rather than error
    }
}

fn main() -> eframe::Result {
    let initial_target = resolve_initial_target();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "kotoshelf",
        native_options,
        Box::new(move |cc| {
            fonts::setup(&cc.egui_ctx);
            // Install egui image loaders so images/thumbnails load immediately in the editor
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let mut app = KotoshelfApp::new();
            match initial_target {
                Some(InitialTarget::Workspace(path)) => app.open_workspace_at(path),
                Some(InitialTarget::File { path, workspace }) => {
                    app.open_workspace_at(workspace);
                    app.open_file(TabPath::Local(path));
                }
                Some(InitialTarget::SshProfile(profile)) => app.open_ssh_workspace(profile),
                None => {}
            }
            Ok(Box::new(app))
        }),
    )
}
