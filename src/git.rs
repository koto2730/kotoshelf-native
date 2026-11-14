//! Local Git and Git-LFS operations for workspace history management.
//!
//! Invokes the system `git` binary asynchronously (via background thread in
//! `app.rs`) with `CREATE_NO_WINDOW` on Windows and null stdin so neither
//! `git` nor `git-lfs` hooks/filters open console windows or block waiting
//! for terminal prompts.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FALLBACK_GIT_PATHS: &[&str] = &[
    "/usr/bin/git",
    "/usr/local/bin/git",
    "/bin/git",
    "/opt/homebrew/bin/git",
];

fn resolve_git_binary() -> &'static str {
    use std::sync::OnceLock;
    static GIT_BIN: OnceLock<&'static str> = OnceLock::new();
    GIT_BIN.get_or_init(|| {
        let mut cmd = Command::new("git");
        cmd.arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        if cmd.status().map(|s| s.success()).unwrap_or(false) {
            return "git";
        }
        for &candidate in FALLBACK_GIT_PATHS {
            if Path::new(candidate).is_file() {
                if Command::new(candidate)
                    .arg("--version")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
                {
                    return candidate;
                }
            }
        }
        "git"
    })
}

/// Builds a `git` command configured for non-interactive GUI execution.
fn git_command(workspace_dir: Option<&Path>) -> Command {
    let mut cmd = Command::new(resolve_git_binary());
    if let Some(dir) = workspace_dir {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null());
    // Prevent terminal password prompts from hanging in the background;
    // OS-level GUI credential helpers (e.g. Git Credential Manager) still work.
    cmd.env("GIT_TERMINAL_PROMPT", "0");

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd
}

/// Returns `true` if the `git` CLI executable is available on the OS `PATH`
/// (or at standard Unix system paths when launched from a desktop launcher).
pub fn is_git_available() -> bool {
    git_command(None)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns `true` if `git-lfs` is installed and accessible via `git lfs version`.
pub fn is_git_lfs_available() -> bool {
    git_command(None)
        .args(["lfs", "version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Searches `dir` and its ancestor directories for a `.git` directory or file.
pub fn find_git_root(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Returns `true` if `workspace_dir` (or one of its parent directories, or via
/// `git rev-parse --is-inside-work-tree`) is inside a Git working tree.
pub fn is_git_repo(workspace_dir: &Path) -> bool {
    if find_git_root(workspace_dir).is_some() {
        return true;
    }
    git_command(Some(workspace_dir))
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns `true` if `.gitattributes` in `workspace_dir` (or the enclosing git root) configures `filter=lfs`.
pub fn uses_git_lfs(workspace_dir: &Path) -> bool {
    let check_dir = |d: &Path| -> bool {
        std::fs::read_to_string(d.join(".gitattributes"))
            .map(|content| content.contains("filter=lfs"))
            .unwrap_or(false)
    };
    if check_dir(workspace_dir) {
        return true;
    }
    if let Some(root) = find_git_root(workspace_dir) {
        if root != workspace_dir && check_dir(&root) {
            return true;
        }
    }
    false
}

/// Builds the default commit message when no editor text is selected:
/// `"Update <filename>: YYYY-MM-DD HH:MM"` (falls back to `"notes"` when no file is active).
pub fn default_commit_message(active_filename: Option<&str>, now: chrono::NaiveDateTime) -> String {
    let name = active_filename
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("notes");
    format!("Update {name}: {}", now.format("%Y-%m-%d %H:%M"))
}

/// Resolves the initial commit message for the Git Commit dialog:
/// uses `selected_text` if non-empty after trimming, otherwise generates
/// the default message with `active_filename` and `now`.
pub fn resolve_initial_commit_message(
    selected_text: Option<&str>,
    active_filename: Option<&str>,
    now: chrono::NaiveDateTime,
) -> String {
    if let Some(sel) = selected_text {
        let trimmed = sel.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    default_commit_message(active_filename, now)
}

fn extract_output_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() && !stdout.is_empty() {
        format!("{stderr} ({stdout})")
    } else if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exit code {:?}", output.status.code())
    }
}

fn git_config_get(workspace_dir: &Path, key: &str) -> Option<String> {
    let out = git_command(Some(workspace_dir))
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!val.is_empty()).then_some(val)
}

/// Resolves the push remote following Git's standard priority:
/// 1. `branch.<branch>.pushRemote`
/// 2. `remote.pushDefault`
/// 3. `branch.<branch>.remote`
/// 4. `"origin"` if and only if a remote named `"origin"` exists.
///
/// Never falls back to an arbitrary non-`"origin"` remote to prevent pushing to an unintended destination.
fn default_push_remote(workspace_dir: &Path, branch: Option<&str>) -> Option<String> {
    if let Some(b) = branch {
        if let Some(r) = git_config_get(workspace_dir, &format!("branch.{b}.pushRemote")) {
            return Some(r);
        }
    }
    if let Some(r) = git_config_get(workspace_dir, "remote.pushDefault") {
        return Some(r);
    }
    if let Some(b) = branch {
        if let Some(r) = git_config_get(workspace_dir, &format!("branch.{b}.remote")) {
            return Some(r);
        }
    }
    let out = git_command(Some(workspace_dir)).arg("remote").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let has_origin = String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim() == "origin");
    has_origin.then(|| "origin".to_string())
}

/// Returns the current checked-out branch name (excluding detached `HEAD`).
fn current_branch(workspace_dir: &Path) -> Option<String> {
    let out = git_command(Some(workspace_dir))
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// Returns the configured upstream tracking ref (e.g. `"origin/main"`) if one exists.
fn upstream_branch(workspace_dir: &Path) -> Option<String> {
    let out = git_command(Some(workspace_dir))
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let upstream = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!upstream.is_empty()).then_some(upstream)
}

/// Returns `true` if the current branch has an upstream tracking branch configured.
#[cfg(test)]
fn has_upstream_branch(workspace_dir: &Path) -> bool {
    upstream_branch(workspace_dir).is_some()
}

/// Resolves the human-readable push destination (`"<remote>/<branch>"`) that `commit_and_push`
/// will target for `workspace_dir`, or `None` if no safe push destination is configured.
pub fn resolve_push_target(workspace_dir: &Path) -> Option<String> {
    let branch = current_branch(workspace_dir);
    if let Some(b) = branch.as_deref() {
        if let Some(push_remote) = git_config_get(workspace_dir, &format!("branch.{b}.pushRemote"))
            .or_else(|| git_config_get(workspace_dir, "remote.pushDefault"))
        {
            return Some(format!("{push_remote}/{b}"));
        }
    }
    if let Some(upstream) = upstream_branch(workspace_dir) {
        return Some(upstream);
    }
    let b = branch?;
    let remote = default_push_remote(workspace_dir, Some(&b))?;
    Some(format!("{remote}/{b}"))
}

/// Runs `git push`, automatically passing `--set-upstream <remote> <branch>` when the current
/// branch does not yet have an upstream tracking branch configured and a safe target remote is resolved.
fn run_git_push(workspace_dir: &Path) -> std::io::Result<Output> {
    if upstream_branch(workspace_dir).is_none() {
        if let Some(branch) = current_branch(workspace_dir) {
            if let Some(remote) = default_push_remote(workspace_dir, Some(&branch)) {
                return git_command(Some(workspace_dir))
                    .args(["push", "--set-upstream", &remote, &branch])
                    .output();
            }
        }
    }
    git_command(Some(workspace_dir)).arg("push").output()
}

/// Stages all changes (`git add -A`), creates a commit (`git commit -m <message>`),
/// and optionally pushes to the remote (`git push`).
///
/// Git-LFS tracked files are handled transparently by `git add` (clean filter)
/// and `git push` (`pre-push` hook) as long as `git-lfs` is installed.
pub fn commit_and_push(workspace_dir: &Path, message: &str, push: bool) -> Result<String, String> {
    let msg = message.trim();
    if msg.is_empty() {
        return Err("Commit message cannot be empty.".to_string());
    }

    if uses_git_lfs(workspace_dir) && !is_git_lfs_available() {
        return Err(
            "This repository uses Git LFS (.gitattributes), but 'git-lfs' is not installed."
                .to_string(),
        );
    }

    // 1. Stage all changes (including new, modified, and deleted files)
    let add_out = git_command(Some(workspace_dir))
        .args(["add", "-A"])
        .output()
        .map_err(|e| format!("Failed to run git add: {e}"))?;
    if !add_out.status.success() {
        return Err(format!("git add failed: {}", extract_output_message(&add_out)));
    }

    // 2. Commit staged changes
    let commit_out = git_command(Some(workspace_dir))
        .args(["commit", "-m", msg])
        .output()
        .map_err(|e| format!("Failed to run git commit: {e}"))?;
    let mut committed = true;
    if !commit_out.status.success() {
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&commit_out.stdout),
            String::from_utf8_lossy(&commit_out.stderr)
        );
        if combined.contains("nothing to commit") || combined.contains("nothing added to commit") {
            if !push {
                return Err("Nothing to commit (working tree clean).".to_string());
            }
            // Working tree is already clean (e.g. previous commit succeeded locally
            // before push failed); proceed to push existing commits.
            committed = false;
        } else {
            return Err(format!(
                "git commit failed: {}",
                extract_output_message(&commit_out)
            ));
        }
    }

    // 3. Optionally push to remote (triggers git-lfs pre-push hook if configured)
    if push {
        let prefix = if committed {
            "Committed locally, but "
        } else {
            ""
        };
        let push_out = run_git_push(workspace_dir)
            .map_err(|e| format!("{prefix}failed to run git push: {e}"))?;
        if !push_out.status.success() {
            return Err(format!(
                "{prefix}git push failed: {}",
                extract_output_message(&push_out)
            ));
        }
        if committed {
            Ok("Git: Committed and pushed to remote.".to_string())
        } else {
            Ok("Git: Pushed to remote (working tree clean).".to_string())
        }
    } else {
        Ok("Git: Committed.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_commit_message_uses_active_filename_and_timestamp() {
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 10, 19)
            .unwrap()
            .and_hms_opt(16, 5, 0)
            .unwrap();
        assert_eq!(
            default_commit_message(Some("todo.md"), dt),
            "Update todo.md: 2026-10-19 16:05"
        );
        assert_eq!(
            default_commit_message(None, dt),
            "Update notes: 2026-10-19 16:05"
        );
        assert_eq!(
            default_commit_message(Some("   "), dt),
            "Update notes: 2026-10-19 16:05"
        );
    }

    #[test]
    fn resolve_initial_commit_message_prefers_non_empty_selection() {
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 10, 19)
            .unwrap()
            .and_hms_opt(16, 5, 0)
            .unwrap();
        assert_eq!(
            resolve_initial_commit_message(Some("  Fix typo in section 2 \n"), Some("memo.md"), dt),
            "Fix typo in section 2"
        );
        assert_eq!(
            resolve_initial_commit_message(Some("   "), Some("memo.md"), dt),
            "Update memo.md: 2026-10-19 16:05"
        );
        assert_eq!(
            resolve_initial_commit_message(None, Some("memo.md"), dt),
            "Update memo.md: 2026-10-19 16:05"
        );
    }

    #[test]
    fn is_git_repo_and_uses_git_lfs_detect_markers() {
        let dir = std::env::temp_dir().join(format!("kotoshelf_git_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(!is_git_repo(&dir));
        assert!(!uses_git_lfs(&dir));

        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(is_git_repo(&dir));

        let nested_sub = dir.join("sub").join("notes");
        std::fs::create_dir_all(&nested_sub).unwrap();
        assert!(
            is_git_repo(&nested_sub),
            "subdirectory inside a git repository should also be detected as a git repo"
        );

        std::fs::write(
            dir.join(".gitattributes"),
            "*.pdf filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        assert!(uses_git_lfs(&dir));
        assert!(uses_git_lfs(&nested_sub));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_and_push_commits_and_pushes_to_local_bare_remote() {
        if !is_git_available() {
            return;
        }
        let base = std::env::temp_dir().join(format!(
            "kotoshelf_git_commit_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let remote_dir = base.join("remote.git");
        let work_dir = base.join("work");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::create_dir_all(&work_dir).unwrap();

        // Initialize bare remote and local working repo
        assert!(git_command(Some(&remote_dir))
            .args(["init", "--bare"])
            .status()
            .unwrap()
            .success());
        assert!(git_command(Some(&work_dir))
            .args(["init", "-b", "main"])
            .status()
            .unwrap()
            .success());
        assert!(git_command(Some(&work_dir))
            .args(["config", "user.email", "test@example.com"])
            .status()
            .unwrap()
            .success());
        assert!(git_command(Some(&work_dir))
            .args(["config", "user.name", "Test User"])
            .status()
            .unwrap()
            .success());
        assert!(git_command(Some(&work_dir))
            .args(["remote", "add", "origin", &remote_dir.to_string_lossy()])
            .status()
            .unwrap()
            .success());

        // First commit + push without a pre-existing upstream tracking branch:
        // `commit_and_push` should automatically run `git push --set-upstream origin main`.
        std::fs::write(work_dir.join("note.md"), "# First\n").unwrap();
        assert!(!has_upstream_branch(&work_dir));
        let res1 = commit_and_push(&work_dir, "Initial commit with push", true);
        assert!(res1.is_ok(), "first commit+push without upstream failed: {res1:?}");
        assert!(has_upstream_branch(&work_dir));

        // Commit locally without push, then call commit_and_push(..., true) on a clean tree
        std::fs::write(work_dir.join("note.md"), "# Second\n").unwrap();
        let res2 = commit_and_push(&work_dir, "Second commit local only", false);
        assert!(res2.is_ok(), "local commit failed: {res2:?}");

        let res_push_clean = commit_and_push(&work_dir, "Push pending commit", true);
        assert!(
            res_push_clean.is_ok(),
            "push with clean working tree failed: {res_push_clean:?}"
        );

        // Clean working tree with push=false should still report nothing to commit
        let res_clean = commit_and_push(&work_dir, "No changes", false);
        assert!(res_clean.is_err());
        assert!(res_clean.unwrap_err().contains("Nothing to commit"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_push_target_respects_git_config_and_rejects_arbitrary_non_origin_remote() {
        if !is_git_available() {
            return;
        }
        let repo_dir = std::env::temp_dir().join(format!(
            "kotoshelf_git_target_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&repo_dir);
        std::fs::create_dir_all(&repo_dir).unwrap();

        assert!(git_command(Some(&repo_dir))
            .args(["init", "-b", "main"])
            .status()
            .unwrap()
            .success());

        // Only a non-origin remote ("upstream") is configured: should NOT guess it
        assert!(git_command(Some(&repo_dir))
            .args(["remote", "add", "upstream", "https://example.com/upstream.git"])
            .status()
            .unwrap()
            .success());
        assert_eq!(resolve_push_target(&repo_dir), None);

        // Once "origin" is added, it resolves to "origin/main"
        assert!(git_command(Some(&repo_dir))
            .args(["remote", "add", "origin", "https://example.com/origin.git"])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            resolve_push_target(&repo_dir),
            Some("origin/main".to_string())
        );

        // `remote.pushDefault` takes precedence over `origin`
        assert!(git_command(Some(&repo_dir))
            .args(["config", "remote.pushDefault", "myfork"])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            resolve_push_target(&repo_dir),
            Some("myfork/main".to_string())
        );

        // `branch.main.pushRemote` takes highest precedence
        assert!(git_command(Some(&repo_dir))
            .args(["config", "branch.main.pushRemote", "branchremote"])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            resolve_push_target(&repo_dir),
            Some("branchremote/main".to_string())
        );

        let _ = std::fs::remove_dir_all(&repo_dir);
    }
}
