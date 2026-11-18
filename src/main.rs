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

/// wgpu setup that adapts to weaker GPUs/drivers (e.g. ARM SBCs and phones
/// whose Vulkan/GLES driver only exposes `max_inter_stage_shader_variables`
/// = 15, where wgpu's default of 16 makes `request_device` fail with
/// `LimitsExceeded`).
///
/// It keeps egui's own device request untouched and then lowers, per limit,
/// only the values the adapter can't provide (`or_worse_values_from`), so a
/// GPU that supports 16 still gets 16 and nothing else changes.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};

    let mut setup = WgpuSetupCreateNew::without_display_handle();
    let default_descriptor = setup.device_descriptor.clone();
    setup.device_descriptor = std::sync::Arc::new(move |adapter| {
        let mut descriptor = default_descriptor(adapter);
        descriptor.required_limits = descriptor.required_limits.or_worse_values_from(&adapter.limits());
        descriptor
    });
    WgpuConfiguration { wgpu_setup: WgpuSetup::CreateNew(setup), ..Default::default() }
}

fn main() -> eframe::Result {
    let initial_target = resolve_initial_target();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]),
        wgpu_options: wgpu_options(),
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

#[cfg(test)]
mod tests {
    use eframe::egui_wgpu::wgpu::Limits;

    /// The ARM failure: a driver that only allows 15 inter-stage variables
    /// must get 15 requested, while a driver that allows 16 keeps 16.
    #[test]
    fn requested_limits_are_lowered_only_where_the_adapter_is_weaker() {
        let requested = Limits::default();
        assert_eq!(requested.max_inter_stage_shader_variables, 16);

        let weak = Limits { max_inter_stage_shader_variables: 15, ..Limits::default() };
        let lowered = requested.clone().or_worse_values_from(&weak);
        assert_eq!(lowered.max_inter_stage_shader_variables, 15);
        assert_eq!(lowered.max_bind_groups, requested.max_bind_groups);

        let strong = Limits { max_inter_stage_shader_variables: 31, ..Limits::default() };
        let kept = requested.clone().or_worse_values_from(&strong);
        assert_eq!(kept.max_inter_stage_shader_variables, 16);
    }
}
