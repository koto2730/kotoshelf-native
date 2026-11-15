//! Remote (SSH) workspace file operations - ported from kotoshelf's
//! `src-tauri/src/lib.rs` SSH command layer, Tauri-specific plumbing
//! (`#[tauri::command]`, `tauri::State`/`AppHandle`) stripped out.
//!
//! Every remote path this module receives/returns is workspace-relative
//! POSIX (`""` = root, always `/`-joined) - deliberately `String`, never
//! `PathBuf`. On Windows, `PathBuf::join` would render with `\`, which
//! would corrupt every shell script built against these paths on the
//! remote (POSIX) host.
//!
//! Scope note: this only ports the always-full `ssh_read_tree` (not the
//! lazy `ssh_read_tree_shallow`) and skips base64/binary read-write
//! (image preview, clipboard paste, uploads) - matches kotoshelf-native's
//! existing local-workspace scope (`workspace.rs` also does an eager
//! full-depth tree, no lazy loading yet), and keeps this port to what
//! "connect, browse, edit, tree CRUD, workspace search/replace" needs.

use agent_protocol::TreeNode;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const SSH_SKIP_DIRS: &[&str] = &[".git", ".kotoshelf", "node_modules", "target"];

/// One saved SSH remote-workspace target. `port`/`user`/`identity_file`
/// are optional so a bare profile can lean entirely on the user's
/// `~/.ssh/config` (Host alias, default user, IdentityFile) - only
/// added to the ssh invocation when set.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshProfile {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub identity_file: Option<String>,
    pub remote_path: String,
}

impl SshProfile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            host: String::new(),
            port: None,
            user: None,
            identity_file: None,
            // Empty = the login user's home folder (resolved at connect time),
            // never the server's filesystem root.
            remote_path: String::new(),
        }
    }
}

/// App-wide config, shared with (and written in the same shape/location
/// as) the original kotoshelf app: `~/.kotoshelf/config.json`.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default)]
    pub presets: Vec<crate::api::ApiPreset>,
    #[serde(default)]
    pub tokens: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub ssh_profiles: Vec<SshProfile>,
    /// Path/name of the ssh executable to invoke. Empty means "ssh"
    /// resolved from PATH.
    #[serde(default)]
    pub ssh_command_path: String,
}

fn app_config_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("Could not determine home directory")?;
    Ok(home.join(".kotoshelf").join("config.json"))
}

pub fn load_app_config() -> AppConfig {
    app_config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_app_config(config: &AppConfig) -> Result<(), String> {
    let path = app_config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("Failed to write {path:?}: {e}"))
}

/// Wraps `s` in single quotes for a POSIX remote shell, escaping any
/// embedded single quotes via the standard `'\''` trick.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Guards every remote path that's supposed to be workspace-relative -
/// without this, an absolute or `..`-escaping path would still "work"
/// (silently concatenated onto the remote root, `mkdir -p` happily
/// materializing a bogus chain) instead of failing loudly.
fn validate_rel_path(rel: &str) -> Result<(), String> {
    if rel.starts_with('/') || rel.starts_with('\\') {
        return Err(format!(
            "Expected a path relative to the remote workspace, got an absolute path: {rel}"
        ));
    }
    let first = rel.split(['/', '\\']).next().unwrap_or("");
    if first.len() == 2 && first.ends_with(':') {
        return Err(format!(
            "Expected a path relative to the remote workspace, got a local path: {rel}"
        ));
    }
    if rel.split('/').any(|seg| seg == "..") {
        return Err(format!("Path escapes the remote workspace: {rel}"));
    }
    Ok(())
}

fn ssh_target(profile: &SshProfile) -> String {
    match &profile.user {
        Some(u) if !u.is_empty() => format!("{u}@{}", profile.host),
        _ => profile.host.clone(),
    }
}

/// A stable path (per host/port/user) for this connection's ControlMaster
/// socket, computed ourselves rather than left to ssh's `%C` token, so a
/// stale/corrupted one can be found and deleted by name.
fn ssh_control_path(profile: &SshProfile) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let dir = dirs::home_dir()?.join(".kotoshelf").join("ssh-sockets");
    std::fs::create_dir_all(&dir).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    profile.host.hash(&mut hasher);
    profile.port.hash(&mut hasher);
    profile.user.hash(&mut hasher);
    Some(dir.join(format!("{:x}", hasher.finish())))
}

fn ssh_common_args(profile: &SshProfile, multiplex: bool) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
    ];
    if multiplex {
        if let Some(control_path) = ssh_control_path(profile) {
            args.push("-o".into());
            args.push("ControlMaster=auto".into());
            args.push("-o".into());
            args.push(format!("ControlPath={}", control_path.to_string_lossy()));
            args.push("-o".into());
            args.push("ControlPersist=600".into());
        }
    }
    if let Some(port) = profile.port {
        args.push("-p".into());
        args.push(port.to_string());
    }
    if let Some(idf) = &profile.identity_file {
        if !idf.is_empty() {
            args.push("-i".into());
            args.push(idf.clone());
        }
    }
    args
}

/// Builds the `ssh` invocation shared by every remote command:
/// non-interactive, short connect timeout, auto-trusting *new* host keys
/// while still refusing a *changed* one, and (when `multiplex`)
/// connection reuse.
fn ssh_base_command(ssh_command_path: &str, profile: &SshProfile, multiplex: bool) -> Command {
    let bin = if ssh_command_path.trim().is_empty() { "ssh" } else { ssh_command_path };
    let mut cmd = Command::new(bin);
    cmd.args(ssh_common_args(profile, multiplex));
    cmd.arg(ssh_target(profile));

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd
}

fn run_ssh_capture_attempt(
    ssh_command_path: &str,
    profile: &SshProfile,
    remote_script: &str,
    multiplex: bool,
) -> Result<String, String> {
    let output = ssh_base_command(ssh_command_path, profile, multiplex)
        .arg(remote_script)
        .output()
        .map_err(|e| format!("Failed to run ssh: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh failed ({}): {}", output.status, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Called before every stale-socket retry below: deletes the
/// ControlMaster socket (covers a stale/corrupted one, e.g. after a
/// remote reboot) and waits 2s (covers the very first connection
/// attempt right after login racing a local SSH-agent service that
/// isn't fully up yet - an instant retry just reproduces the same
/// failure).
fn prepare_stale_socket_retry(profile: &SshProfile) {
    if let Some(path) = ssh_control_path(profile) {
        let _ = std::fs::remove_file(&path);
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
}

/// On failure, retries once over a fresh, non-multiplexed connection.
/// The retry's result (not the first attempt's) is what's reported.
fn run_ssh_capture(ssh_command_path: &str, profile: &SshProfile, remote_script: &str) -> Result<String, String> {
    match run_ssh_capture_attempt(ssh_command_path, profile, remote_script, true) {
        Ok(out) => Ok(out),
        Err(_) => {
            prepare_stale_socket_retry(profile);
            run_ssh_capture_attempt(ssh_command_path, profile, remote_script, false)
        }
    }
}

fn run_ssh_with_stdin_attempt(
    ssh_command_path: &str,
    profile: &SshProfile,
    remote_script: &str,
    input: &[u8],
    multiplex: bool,
) -> Result<(), String> {
    let mut child = ssh_base_command(ssh_command_path, profile, multiplex)
        .arg(remote_script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run ssh: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("Failed to open ssh stdin")?
        .write_all(input)
        .map_err(|e| format!("Failed to write to ssh stdin: {e}"))?;
    let output = child.wait_with_output().map_err(|e| format!("ssh failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh failed ({}): {}", output.status, stderr.trim()));
    }
    Ok(())
}

fn run_ssh_with_stdin(
    ssh_command_path: &str,
    profile: &SshProfile,
    remote_script: &str,
    input: &[u8],
) -> Result<(), String> {
    match run_ssh_with_stdin_attempt(ssh_command_path, profile, remote_script, input, true) {
        Ok(()) => Ok(()),
        Err(_) => {
            prepare_stale_socket_retry(profile);
            run_ssh_with_stdin_attempt(ssh_command_path, profile, remote_script, input, false)
        }
    }
}

/// Binary-safe variant of `run_ssh_capture_attempt` - the search-fetch
/// script's output is a mix of text (path/size lines) and raw file bytes,
/// so `String::from_utf8_lossy`ing it up front (as `run_ssh_capture` does)
/// would corrupt or misalign anything not valid UTF-8.
fn run_ssh_capture_bytes_attempt(
    ssh_command_path: &str,
    profile: &SshProfile,
    remote_script: &str,
    multiplex: bool,
) -> Result<Vec<u8>, String> {
    let output = ssh_base_command(ssh_command_path, profile, multiplex)
        .arg(remote_script)
        .output()
        .map_err(|e| format!("Failed to run ssh: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh failed ({}): {}", output.status, stderr.trim()));
    }
    Ok(output.stdout)
}

fn run_ssh_capture_bytes(ssh_command_path: &str, profile: &SshProfile, remote_script: &str) -> Result<Vec<u8>, String> {
    match run_ssh_capture_bytes_attempt(ssh_command_path, profile, remote_script, true) {
        Ok(out) => Ok(out),
        Err(_) => {
            prepare_stale_socket_retry(profile);
            run_ssh_capture_bytes_attempt(ssh_command_path, profile, remote_script, false)
        }
    }
}

// ---------------------------------------------------------------------
// Tree read/parse
// ---------------------------------------------------------------------

#[derive(Default)]
struct SshDirBuilder {
    children: std::collections::BTreeMap<String, SshEntry>,
}

enum SshEntry {
    Dir(SshDirBuilder),
    File { size: u64 },
}

fn ssh_insert_path(root: &mut SshDirBuilder, parts: &[&str], is_dir: bool, size: u64) {
    let Some((head, rest)) = parts.split_first() else { return };
    if rest.is_empty() {
        if is_dir {
            root.children.entry((*head).to_string()).or_insert_with(|| SshEntry::Dir(SshDirBuilder::default()));
        } else {
            root.children.insert((*head).to_string(), SshEntry::File { size });
        }
        return;
    }
    let entry = root.children.entry((*head).to_string()).or_insert_with(|| SshEntry::Dir(SshDirBuilder::default()));
    if let SshEntry::Dir(sub) = entry {
        ssh_insert_path(sub, rest, is_dir, size);
    }
}

fn ssh_tree_nodes(dir: &SshDirBuilder, prefix: &str) -> Vec<TreeNode> {
    let mut nodes: Vec<TreeNode> = dir
        .children
        .iter()
        .map(|(name, entry)| {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            match entry {
                SshEntry::Dir(sub) => {
                    TreeNode { name: name.clone(), children: Some(ssh_tree_nodes(sub, &path)), path, is_dir: true, size: 0 }
                }
                SshEntry::File { size } => TreeNode { name: name.clone(), path, is_dir: false, children: None, size: *size },
            }
        })
        .collect();
    nodes.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    nodes
}

/// Parses tab-separated "D\t0\tpath" / "F\tsize\tpath" lines.
fn parse_ssh_tree(raw: &str) -> Vec<TreeNode> {
    let mut root = SshDirBuilder::default();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        let mut fields = line.splitn(3, '\t');
        let (Some(marker), Some(size_str), Some(path)) = (fields.next(), fields.next(), fields.next()) else { continue };
        if path.is_empty() {
            continue;
        }
        let size = size_str.trim().parse::<u64>().unwrap_or(0);
        let parts: Vec<&str> = path.split('/').collect();
        ssh_insert_path(&mut root, &parts, marker == "D", size);
    }
    ssh_tree_nodes(&root, "")
}

/// The `find`-side counterpart of `agent_protocol::is_skipped_dir` - the
/// fixed `SSH_SKIP_DIRS` names (any type) OR-ed with a dot-prefix rule
/// scoped to directories only (`-type d -name '.*'`, its own group so
/// the `-type d` doesn't leak onto the other alternatives) - a
/// dot-prefixed *file* like `.gitignore` stays visible, matching the
/// local (non-SSH) behavior.
fn ssh_prune_expr() -> String {
    let named = SSH_SKIP_DIRS.iter().map(|d| format!("-name {}", shell_quote(d))).collect::<Vec<_>>().join(" -o ");
    format!("{named} -o \\( -type d -name '.*' \\)")
}

/// One `find` pass (dirs and files together) then a per-entry shell loop
/// that tags each D(irectory)/F(ile) and sizes files via `stat -c%s`
/// (GNU) falling back to `stat -f%z` (BSD/macOS) - deliberately not
/// `wc -c`, which counts bytes by reading the whole file (hangs on a
/// multi-GB file just to answer "how big is this").
fn ssh_read_tree_script(remote_path: &str) -> String {
    let prune = ssh_prune_expr();
    let mut s = String::new();
    s.push_str("cd ");
    s.push_str(&shell_quote(remote_path));
    s.push_str(" && find . -mindepth 1 \\( ");
    s.push_str(&prune);
    s.push_str(" \\) -prune -o -print | while IFS= read -r p; do ");
    s.push_str("rel=${p#./}; ");
    s.push_str("if [ -d \"$p\" ]; then printf 'D\\t0\\t%s\\n' \"$rel\"; ");
    s.push_str(
        "else printf 'F\\t%s\\t%s\\n' \"$(stat -c%s \"$p\" 2>/dev/null || stat -f%z \"$p\" 2>/dev/null || echo 18446744073709551615)\" \"$rel\"; fi; ",
    );
    s.push_str("done");
    s
}

pub fn read_tree(ssh_command_path: &str, profile: &SshProfile) -> Result<Vec<TreeNode>, String> {
    let script = ssh_read_tree_script(&profile.remote_path);
    let out = run_ssh_capture(ssh_command_path, profile, &script)?;
    Ok(parse_ssh_tree(&out))
}

/// Every file path in the workspace, `find`-only (no `stat`) - used for
/// wiki-link resolution, which needs to search the whole workspace by
/// name regardless of which folders happen to be expanded in the tree
/// pane. Workspace-relative, matching every other SSH path in this
/// module (mirrors `workspace::list_all_paths`'s local counterpart,
/// which returns absolute paths instead).
pub fn list_all_paths(ssh_command_path: &str, profile: &SshProfile) -> Result<Vec<String>, String> {
    let prune = ssh_prune_expr();
    let mut script = String::new();
    script.push_str("cd ");
    script.push_str(&shell_quote(&profile.remote_path));
    script.push_str(" && find . -mindepth 1 \\( ");
    script.push_str(&prune);
    script.push_str(" \\) -prune -o -type f -print");
    let out = run_ssh_capture(ssh_command_path, profile, &script)?;
    Ok(out.lines().map(|l| l.trim_end_matches('\r').trim_start_matches("./").to_string()).filter(|s| !s.is_empty()).collect())
}

// ---------------------------------------------------------------------
// File / directory operations
// ---------------------------------------------------------------------

fn full_path(profile: &SshProfile, rel_path: &str) -> String {
    format!("{}/{rel_path}", profile.remote_path.trim_end_matches('/'))
}

pub fn read_file(ssh_command_path: &str, profile: &SshProfile, rel_path: &str) -> Result<String, String> {
    validate_rel_path(rel_path)?;
    let script = format!("cat -- {}", shell_quote(&full_path(profile, rel_path)));
    run_ssh_capture(ssh_command_path, profile, &script)
}

/// Modification time (Unix seconds) of `rel_path` on the remote, or `None`
/// if it doesn't exist / can't be stat'ed. GNU `stat -c %Y` with a BSD/
/// macOS `stat -f %m` fallback.
pub fn file_mtime_secs(ssh_command_path: &str, profile: &SshProfile, rel_path: &str) -> Option<i64> {
    validate_rel_path(rel_path).ok()?;
    let p = shell_quote(&full_path(profile, rel_path));
    let script = format!("stat -c %Y {p} 2>/dev/null || stat -f %m {p} 2>/dev/null");
    run_ssh_capture(ssh_command_path, profile, &script).ok()?.trim().parse().ok()
}

pub fn write_file(ssh_command_path: &str, profile: &SshProfile, rel_path: &str, content: &str) -> Result<(), String> {
    validate_rel_path(rel_path)?;
    let full = full_path(profile, rel_path);
    let script = format!("mkdir -p \"$(dirname {})\" && cat > {}", shell_quote(&full), shell_quote(&full));
    run_ssh_with_stdin(ssh_command_path, profile, &script, content.as_bytes())
}

/// Files above this size are refused rather than fetched whole into
/// memory over one `ssh` round trip - matches the original kotoshelf's
/// `LARGE_FILE_WARN_BYTES` threshold for image previews.
const SSH_IMAGE_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Reads `rel_path`'s raw bytes, refusing (rather than truncating) if the
/// remote file is over `SSH_IMAGE_MAX_BYTES` - one round trip stats then
/// conditionally reads, so an oversized file's bytes are never actually
/// sent. Binary-safe (`run_ssh_capture_bytes`), unlike `read_file` above
/// which lossily decodes to UTF-8 text - this is for the preview pane's
/// images, not editable note content.
pub fn read_file_guarded(ssh_command_path: &str, profile: &SshProfile, rel_path: &str) -> Result<Vec<u8>, String> {
    validate_rel_path(rel_path)?;
    let full = full_path(profile, rel_path);
    let quoted = shell_quote(&full);
    let script = format!(
        "size=$(stat -c%s -- {quoted} 2>/dev/null || stat -f%z -- {quoted} 2>/dev/null || echo 0); \
         if [ \"$size\" -gt {SSH_IMAGE_MAX_BYTES} ]; then printf 'TOOLARGE\\t%s\\n' \"$size\"; \
         else printf 'OK\\n'; cat -- {quoted}; fi"
    );
    let bytes = run_ssh_capture_bytes(ssh_command_path, profile, &script)?;
    parse_guarded_response(&bytes)
}

fn parse_guarded_response(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let nl = bytes.iter().position(|&b| b == b'\n').ok_or("Malformed response from remote host")?;
    let header = String::from_utf8_lossy(&bytes[..nl]);
    if let Some(size) = header.strip_prefix("TOOLARGE\t") {
        return Err(format!("Too large to preview ({} bytes)", size.trim()));
    }
    if header.trim() != "OK" {
        return Err(format!("Unexpected response from remote host: {header}"));
    }
    Ok(bytes[nl + 1..].to_vec())
}

pub fn create_dir(ssh_command_path: &str, profile: &SshProfile, rel_path: &str) -> Result<(), String> {
    validate_rel_path(rel_path)?;
    let script = format!("mkdir -p -- {}", shell_quote(&full_path(profile, rel_path)));
    run_ssh_capture(ssh_command_path, profile, &script).map(|_| ())
}

/// Uploads raw bytes to `rel_path`, creating any missing parent
/// directories first - unlike `write_file` above (which assumes the
/// parent already exists, true for normal editor saves that go into
/// paths the tree UI already created), a drag-and-drop attachment's
/// `attachments/` subdirectory next to a note may not exist yet on its
/// first use.
pub fn upload_bytes(ssh_command_path: &str, profile: &SshProfile, rel_path: &str, bytes: &[u8]) -> Result<(), String> {
    validate_rel_path(rel_path)?;
    let full = full_path(profile, rel_path);
    let parent = full.rsplit_once('/').map(|(p, _)| p).unwrap_or(&full);
    let script = format!("mkdir -p -- {} && cat > {}", shell_quote(parent), shell_quote(&full));
    run_ssh_with_stdin(ssh_command_path, profile, &script, bytes)
}

/// Guards against clobbering an existing target, matching the local
/// `rename_path`'s "Already exists" check.
pub fn rename_path(ssh_command_path: &str, profile: &SshProfile, from_rel: &str, to_rel: &str) -> Result<(), String> {
    validate_rel_path(from_rel)?;
    validate_rel_path(to_rel)?;
    let from = full_path(profile, from_rel);
    let to = full_path(profile, to_rel);
    let script = format!(
        "if [ -e {} ]; then echo __EXISTS__; else mv -- {} {}; fi",
        shell_quote(&to),
        shell_quote(&from),
        shell_quote(&to),
    );
    let out = run_ssh_capture(ssh_command_path, profile, &script)?;
    if out.trim() == "__EXISTS__" {
        return Err(format!("Already exists: {to_rel}"));
    }
    Ok(())
}

pub fn copy_path(ssh_command_path: &str, profile: &SshProfile, from_rel: &str, to_rel: &str) -> Result<(), String> {
    validate_rel_path(from_rel)?;
    validate_rel_path(to_rel)?;
    let from = full_path(profile, from_rel);
    let to = full_path(profile, to_rel);
    let script = format!(
        "mkdir -p $(dirname -- {}) && cp -r -- {} {}",
        shell_quote(&to),
        shell_quote(&from),
        shell_quote(&to),
    );
    let _ = run_ssh_capture(ssh_command_path, profile, &script)?;
    Ok(())
}

/// Moves into `.kotoshelf/.trash/` inside the remote workspace,
/// timestamped to avoid name collisions - there's no OS trash on an
/// arbitrary remote host, but tree operations should still be
/// recoverable rather than a silent permanent delete.
pub fn trash_path(ssh_command_path: &str, profile: &SshProfile, rel_path: &str) -> Result<(), String> {
    validate_rel_path(rel_path)?;
    let root = profile.remote_path.trim_end_matches('/');
    let full = full_path(profile, rel_path);
    let trash_dir = format!("{root}/.kotoshelf/.trash");
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);

    let mut script = String::new();
    script.push_str("mkdir -p ");
    script.push_str(&shell_quote(&trash_dir));
    script.push_str(" && ts=$(date +%s) && mv -- ");
    script.push_str(&shell_quote(&full));
    script.push(' ');
    script.push_str(&shell_quote(&trash_dir));
    script.push_str("/\"$ts-\"");
    script.push_str(&shell_quote(name));

    run_ssh_capture(ssh_command_path, profile, &script).map(|_| ())
}

/// Round-trips `cd <remote_path> && pwd` so "Connect" surfaces an
/// auth/host/path failure immediately instead of only on first file op.
pub fn test_connection(ssh_command_path: &str, profile: &SshProfile) -> Result<String, String> {
    let script = format!("cd {} && pwd", shell_quote(&profile.remote_path));
    run_ssh_capture(ssh_command_path, profile, &script).map(|s| s.trim().to_string())
}

/// Validates a profile's "Remote folder" text: any `..` segment is refused
/// (so `~/..` or `/home/me/../other` can never climb out of where the user
/// pointed the workspace). Empty, `~` / `~/sub` (home-relative) and absolute
/// paths are fine.
pub fn check_remote_folder(remote_path: &str) -> Result<(), String> {
    if remote_path.split(['/', '\\']).any(|seg| seg.trim() == "..") {
        return Err("Remote folder must not contain \"..\".".to_string());
    }
    Ok(())
}

/// Maps the dialog's Remote folder text to the root every remote command
/// `cd`s into, leaving "where is home" entirely to ssh/the server: an
/// empty value or `~` becomes `.` (ssh starts every command in the login
/// user's home, exactly like plain `ssh host`), `~/sub` becomes `./sub`,
/// anything else is kept as typed. No `$HOME` lookup and no guessed
/// absolute path - an empty root can never turn into `/`.
pub fn normalize_remote_folder(profile: &SshProfile) -> Result<SshProfile, String> {
    let raw = profile.remote_path.trim();
    check_remote_folder(raw)?;
    let root = if raw.is_empty() || raw == "~" {
        ".".to_string()
    } else if let Some(sub) = raw.strip_prefix("~/") {
        let sub = sub.trim_matches('/');
        if sub.is_empty() { ".".to_string() } else { format!("./{sub}") }
    } else {
        raw.to_string()
    };
    let mut p = profile.clone();
    p.remote_path = root;
    Ok(p)
}

// ---------------------------------------------------------------------
// Workspace-wide search & replace
// ---------------------------------------------------------------------

/// Files above this size are skipped entirely by search (not truncated -
/// a partial match scan of a huge log file would be misleading). Matches
/// the original kotoshelf's `SSH_SEARCH_MAX_FILE_BYTES`.
const SSH_SEARCH_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// One `find` pass (pruned the same way as the tree read) piped through a
/// shell loop that, for every file at or under the size cap, prints
/// `<relpath>\n<byte-len>\n` followed immediately by the file's raw
/// bytes (no trailing separator - the byte-len is what lets the client
/// know where one file's content ends and the next file's path line
/// begins). Oversized files are skipped without even a placeholder line,
/// so a single `ssh` round trip fetches every searchable file's content
/// at once rather than one round trip per file.
fn ssh_search_fetch_script(remote_path: &str) -> String {
    let prune = ssh_prune_expr();
    let mut s = String::new();
    s.push_str("cd ");
    s.push_str(&shell_quote(remote_path));
    s.push_str(" && find . -mindepth 1 \\( ");
    s.push_str(&prune);
    s.push_str(" \\) -prune -o -type f -print | while IFS= read -r p; do ");
    s.push_str("rel=${p#./}; ");
    s.push_str("size=$(stat -c%s \"$p\" 2>/dev/null || stat -f%z \"$p\" 2>/dev/null || echo 0); ");
    s.push_str(&format!("if [ \"$size\" -le {SSH_SEARCH_MAX_FILE_BYTES} ]; then "));
    s.push_str("printf '%s\\n%s\\n' \"$rel\" \"$size\"; cat -- \"$p\"; fi; ");
    s.push_str("done");
    s
}

/// Decodes `ssh_search_fetch_script`'s length-prefixed stream into
/// `(relative_path, content_bytes)` pairs. Stops cleanly (rather than
/// panicking or erroring) on a short/malformed trailing chunk - the
/// remote side can only ever be cut off mid-stream by a connection drop,
/// never mid-file by design (the size is always sent before the bytes),
/// so whatever was fully decoded before that point is still valid.
fn parse_search_fetch(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let Some(nl1) = bytes[i..].iter().position(|&b| b == b'\n') else { break };
        let path = String::from_utf8_lossy(&bytes[i..i + nl1]).into_owned();
        i += nl1 + 1;
        let Some(nl2) = bytes[i..].iter().position(|&b| b == b'\n') else { break };
        let size_str = String::from_utf8_lossy(&bytes[i..i + nl2]).into_owned();
        i += nl2 + 1;
        let Ok(size) = size_str.trim().parse::<usize>() else { break };
        if i + size > bytes.len() {
            break;
        }
        out.push((path, bytes[i..i + size].to_vec()));
        i += size;
    }
    out
}

/// Workspace-wide search over an SSH remote - one `ssh` round trip fetches
/// every searchable file's content (see `ssh_search_fetch_script`), then
/// matching happens locally with the exact same regex/cap/preview logic
/// the local-workspace search uses (`workspace::build_regex`/
/// `scan_matches`/`push_capped`/`MAX_TOTAL_MATCHES`) - the SSH and local
/// search results are shaped identically, only how the file bytes were
/// obtained differs.
pub fn search_workspace(
    ssh_command_path: &str,
    profile: &SshProfile,
    query: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> Result<Vec<crate::workspace::FileSearchResult>, String> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let regexes = crate::workspace::build_query_regexes(query, is_regex, case_sensitive)?;
    let script = ssh_search_fetch_script(&profile.remote_path);
    let bytes = run_ssh_capture_bytes(ssh_command_path, profile, &script)?;
    let files = parse_search_fetch(&bytes);

    let mut results = Vec::new();
    let mut total = 0usize;
    for (path, content_bytes) in files {
        let Ok(content) = String::from_utf8(content_bytes) else {
            continue; // binary file - skip rather than fail the whole search
        };
        let matches = crate::workspace::scan_matches_multi(&content, &regexes);
        if crate::workspace::push_capped(&mut results, &mut total, path, matches) {
            break;
        }
    }
    Ok(results)
}

/// Replaces every match of `query` in each of `paths` (workspace-relative,
/// as returned by `search_workspace` above) - one read + one write round
/// trip per file that actually has a match, reusing `read_file`/
/// `write_file` and the same `workspace::apply_replace` the local variant
/// uses. Files with zero matches are left untouched and omitted from the
/// result, matching the local `replace_in_files`'s contract.
pub fn replace_in_files(
    ssh_command_path: &str,
    profile: &SshProfile,
    paths: &[String],
    query: &str,
    replacement: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> Result<Vec<crate::workspace::ReplaceResult>, String> {
    let re = crate::workspace::build_regex(query, is_regex, case_sensitive)?;
    let mut results = Vec::new();
    for rel in paths {
        let Ok(content) = read_file(ssh_command_path, profile, rel) else {
            continue;
        };
        let (replaced, count) = crate::workspace::apply_replace(&content, &re, replacement);
        if count == 0 {
            continue;
        }
        write_file(ssh_command_path, profile, rel, &replaced)?;
        results.push(crate::workspace::ReplaceResult { path: rel.clone(), count });
    }
    Ok(results)
}

/// Opens an interactive terminal already `cd`'d into the remote
/// workspace folder over SSH. Best-effort per platform.
///
/// Deliberately NOT multiplexed (`ssh_common_args(profile, false)`):
/// `-t` (pty allocation) combined with `ControlMaster=auto` is a
/// known-flaky combination on at least some Windows ssh builds. A
/// terminal is opened rarely enough that paying a full handshake each
/// time isn't worth that risk.
pub fn open_terminal(ssh_command_path: &str, profile: &SshProfile) -> Result<(), String> {
    let bin = if ssh_command_path.trim().is_empty() { "ssh".to_string() } else { ssh_command_path.to_string() };

    let mut ssh_args: Vec<String> = vec!["-t".into()];
    ssh_args.extend(ssh_common_args(profile, false));
    ssh_args.push(ssh_target(profile));
    // TERM is exported as part of the remote command itself (not our own
    // spawned process's env) so it reaches the remote shell even if an
    // already-open terminal window hands this off to an existing process
    // whose environment predates us - without it, remote capability
    // detection (ls, git, a colored prompt) silently skips color output.
    ssh_args.push(format!(
        "export TERM=xterm-256color; cd {} && exec \"${{SHELL:-/bin/sh}}\" -l",
        shell_quote(&profile.remote_path)
    ));

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        Command::new(&bin)
            .args(&ssh_args)
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map_err(|e| format!("Failed to open a terminal: {e}"))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let full_cmd = std::iter::once(bin.clone())
            .chain(ssh_args.iter().cloned())
            .map(|a| shell_quote(&a))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "tell application \"Terminal\" to do script \"{}\"",
            full_cmd.replace('\\', "\\\\").replace('"', "\\\"")
        );
        Command::new("osascript").arg("-e").arg(script).spawn().map_err(|e| format!("Failed to open Terminal.app: {e}"))?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let candidates: &[(&str, &[&str])] =
            &[("x-terminal-emulator", &["-e"]), ("gnome-terminal", &["--"]), ("konsole", &["-e"]), ("xterm", &["-e"])];
        for (term, prefix_args) in candidates {
            let mut cmd = Command::new(term);
            cmd.args(*prefix_args).arg(&bin).args(&ssh_args);
            if cmd.spawn().is_ok() {
                return Ok(());
            }
        }
        Err("No supported terminal emulator found (tried x-terminal-emulator, gnome-terminal, konsole, xterm)".into())
    }
}

// ---------------------------------------------------------------------
// SSH agent (perf) - a persistent binary deployed to the remote host,
// reached over one `ssh` subprocess's stdin/stdout, so tree reads stop
// paying a fresh `ssh` process spawn + remote shell startup every time.
// Best-effort throughout: any failure to detect/deploy/spawn/talk to the
// agent just means the caller falls back to `read_tree` above, so
// nothing here can turn a working SSH workspace into a broken one.
// ---------------------------------------------------------------------

fn agent_session_key(profile: &SshProfile) -> String {
    format!(
        "{}:{}:{}:{}",
        profile.host,
        profile.port.unwrap_or(0),
        profile.user.as_deref().unwrap_or(""),
        profile.remote_path
    )
}

pub struct AgentSession {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    key: String,
    next_id: u64,
}

impl AgentSession {
    fn call(&mut self, body: agent_protocol::RequestBody) -> Result<agent_protocol::ResponseBody, String> {
        self.next_id += 1;
        let id = self.next_id;
        agent_protocol::write_frame(&mut self.stdin, &agent_protocol::Request { id, body })
            .map_err(|e| format!("agent write failed: {e}"))?;
        let resp: agent_protocol::Response = agent_protocol::read_frame(&mut self.stdout)
            .map_err(|e| format!("agent read failed: {e}"))?
            .ok_or("agent closed the connection")?;
        Ok(resp.body)
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `None` = no usable agent for this profile (caller falls back to
/// `read_tree`). `Some(Err(...))` is still a real answer from the agent
/// (e.g. "Not a directory"), not a reason to fall back further - only a
/// connection-level problem drops the session.
pub fn agent_read_tree(session: &mut Option<AgentSession>, profile: &SshProfile) -> Option<Result<Vec<TreeNode>, String>> {
    let s = session.as_mut()?;
    if s.key != agent_session_key(profile) {
        return None;
    }
    match s.call(agent_protocol::RequestBody::ReadTree { root: profile.remote_path.clone() }) {
        Ok(agent_protocol::ResponseBody::Tree(nodes)) => Some(Ok(nodes)),
        Ok(agent_protocol::ResponseBody::Error { message }) => Some(Err(message)),
        Ok(_) => {
            *session = None;
            None
        }
        Err(_) => {
            *session = None;
            None
        }
    }
}

/// Ends the live agent session (if any): asks it to exit gracefully
/// first (best-effort), then drops it (kills + waits on the local ssh
/// child as a guaranteed backstop).
pub fn agent_disconnect(session: &mut Option<AgentSession>) {
    if let Some(mut s) = session.take() {
        let _ = s.call(agent_protocol::RequestBody::Shutdown);
    }
}

/// Maps `uname -sm` output to the prebuilt agent's Rust target triple.
/// `None` for anything not covered (Windows remotes are out of scope for
/// the agent) - the caller treats that as "no agent for this host".
fn agent_target_triple(uname: &str) -> Option<&'static str> {
    let mut parts = uname.split_whitespace();
    let os = parts.next()?;
    let arch = parts.next()?;
    match (os, arch) {
        ("Linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("Linux", "aarch64") | ("Linux", "arm64") => Some("aarch64-unknown-linux-gnu"),
        ("Darwin", "arm64") => Some("aarch64-apple-darwin"),
        ("Darwin", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// Looks for a prebuilt agent binary for `triple` at
/// `~/.kotoshelf/agent-bin/<triple>/kotoshelf-agent` - the same manual
/// drop-in location kotoshelf itself checks first (before its own
/// bundled-resource fallback, which kotoshelf-native has no equivalent
/// of yet, not being packaged). A binary built for kotoshelf's `agent`
/// crate works here unmodified - `agent-protocol`/`agent` are identical
/// between the two projects.
fn resolve_agent_binary(triple: &str) -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("Could not determine home directory")?;
    let manual = home.join(".kotoshelf").join("agent-bin").join(triple).join("kotoshelf-agent");
    if manual.is_file() {
        return Ok(manual);
    }
    Err(format!("No prebuilt agent binary for {triple} (checked ~/.kotoshelf/agent-bin/{triple}/)"))
}

/// Connects (deploying the agent binary to the remote host first if not
/// already cached there) and returns the session for `agent_read_tree`
/// to use. Errors here are meant to be non-fatal to the caller: SSH
/// still works without an agent via `read_tree`, just slower.
pub fn agent_connect(ssh_command_path: &str, profile: &SshProfile) -> Result<AgentSession, String> {
    let uname = run_ssh_capture(ssh_command_path, profile, "uname -sm")?;
    let triple = agent_target_triple(uname.trim())
        .ok_or_else(|| format!("Unsupported remote platform for the SSH agent: {}", uname.trim()))?;
    let local_bin = resolve_agent_binary(triple)?;

    let home = run_ssh_capture(ssh_command_path, profile, "printf '%s' \"$HOME\"")?;
    let home = home.trim();
    if home.is_empty() {
        return Err("Could not resolve the remote $HOME".into());
    }
    let remote_bin = format!("{home}/.kotoshelf/agent/kotoshelf-agent-{}", env!("CARGO_PKG_VERSION"));

    let exists = run_ssh_capture(ssh_command_path, profile, &format!("test -x {} && echo OK", shell_quote(&remote_bin)))
        .map(|s| s.trim() == "OK")
        .unwrap_or(false);

    if !exists {
        let bytes = std::fs::read(&local_bin).map_err(|e| format!("Failed to read local agent binary {local_bin:?}: {e}"))?;
        let script = format!(
            "mkdir -p {} && cat > {} && chmod +x {}",
            shell_quote(&format!("{home}/.kotoshelf/agent")),
            shell_quote(&remote_bin),
            shell_quote(&remote_bin),
        );
        run_ssh_with_stdin(ssh_command_path, profile, &script, &bytes)?;
    }

    // Best-effort cleanup of binaries from previous versions - failure
    // doesn't block connecting.
    let cleanup = format!(
        "find {} -maxdepth 1 -name 'kotoshelf-agent-*' ! -name {} -delete",
        shell_quote(&format!("{home}/.kotoshelf/agent")),
        shell_quote(&format!("kotoshelf-agent-{}", env!("CARGO_PKG_VERSION"))),
    );
    let _ = run_ssh_capture(ssh_command_path, profile, &cleanup);

    // Not multiplexed: this one connection stays open for the agent's
    // whole lifetime (ControlMaster reuse doesn't apply), and combining
    // ControlMaster with fully piped non-tty stdio has been flaky.
    let mut cmd = ssh_base_command(ssh_command_path, profile, false);
    cmd.arg(&remote_bin);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("Failed to start the agent: {e}"))?;
    let stdin = child.stdin.take().ok_or("Failed to open agent stdin")?;
    let stdout = child.stdout.take().ok_or("Failed to open agent stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to open agent stderr")?;

    // Drained continuously on a background thread so a chatty remote
    // shell can never fill the OS pipe buffer and stall.
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    {
        let buf = stderr_buf.clone();
        let mut stderr = stderr;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            *buf.lock().unwrap() = s;
        });
    }

    let mut session =
        AgentSession { child, stdin, stdout: std::io::BufReader::new(stdout), key: agent_session_key(profile), next_id: 0 };

    match session.call(agent_protocol::RequestBody::Ping) {
        Ok(agent_protocol::ResponseBody::Pong) => {}
        Ok(_) => return Err("Unexpected response to agent handshake".into()),
        Err(e) => {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let stderr_text = stderr_buf.lock().unwrap().trim().to_string();
            return Err(if stderr_text.is_empty() {
                format!("Agent handshake failed: {e}")
            } else {
                format!("Agent handshake failed: {e} (remote stderr: {stderr_text})")
            });
        }
    }

    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_expr_includes_a_directory_scoped_dot_prefix_rule() {
        let expr = ssh_prune_expr();
        for name in SSH_SKIP_DIRS {
            assert!(expr.contains(&format!("-name {}", shell_quote(name))), "missing fixed skip name {name}: {expr}");
        }
        // Scoped to directories, not `-name '.*'` alone, so a dotfile
        // like `.gitignore` isn't pruned away too.
        assert!(expr.contains("-type d"), "dot-prefix rule should be directory-scoped: {expr}");
        assert!(expr.contains(".*"), "should still match any dot-prefixed name: {expr}");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    #[test]
    fn validate_rel_path_rejects_absolute_paths() {
        assert!(validate_rel_path("/etc/passwd").is_err());
        assert!(validate_rel_path("\\Windows\\System32").is_err());
    }

    #[test]
    fn validate_rel_path_rejects_windows_drive_prefixed_paths() {
        assert!(validate_rel_path("C:\\Users\\me\\file.txt").is_err());
    }

    #[test]
    fn validate_rel_path_rejects_parent_dir_escapes() {
        assert!(validate_rel_path("../../etc/passwd").is_err());
        assert!(validate_rel_path("notes/../../etc/passwd").is_err());
    }

    #[test]
    fn remote_folder_rejects_parent_segments_but_allows_home_and_absolute() {
        for bad in ["..", "~/..", "~/../x", "/home/me/../other", "a\\..\\b"] {
            assert!(check_remote_folder(bad).is_err(), "should reject {bad:?}");
        }
        for ok in ["", "~", "~/notes", "/", "/home/me/notes", "notes..bak", "~/a..b"] {
            assert!(check_remote_folder(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn normalize_remote_folder_leaves_home_to_ssh() {
        let n = |p: &str| {
            let mut prof = SshProfile::new("t");
            prof.remote_path = p.to_string();
            normalize_remote_folder(&prof).map(|x| x.remote_path)
        };
        assert_eq!(n("").unwrap(), ".");
        assert_eq!(n("  ").unwrap(), ".");
        assert_eq!(n("~").unwrap(), ".");
        assert_eq!(n("~/").unwrap(), ".");
        assert_eq!(n("~/notes/").unwrap(), "./notes");
        assert_eq!(n("/home/me/x").unwrap(), "/home/me/x");
        assert_eq!(n("/").unwrap(), "/");
        assert!(n("~/../x").is_err());
    }

    #[test]
    fn validate_rel_path_accepts_ordinary_relative_paths() {
        assert!(validate_rel_path("notes/today.md").is_ok());
        assert!(validate_rel_path("").is_ok());
    }

    #[test]
    fn parse_ssh_tree_builds_nested_structure_from_flat_lines() {
        let raw = "D\t0\tnotes\nF\t123\tnotes/today.md\nF\t45\ttop.txt\n";
        let tree = parse_ssh_tree(raw);
        // Dirs sort before files; alphabetical within each group.
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "notes");
        assert!(tree[0].is_dir);
        let children = tree[0].children.as_ref().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].path, "notes/today.md");
        assert_eq!(children[0].size, 123);
        assert_eq!(tree[1].name, "top.txt");
        assert_eq!(tree[1].size, 45);
    }

    #[test]
    fn parse_ssh_tree_ignores_malformed_lines() {
        let raw = "not a valid line\nF\t10\tok.txt\n";
        let tree = parse_ssh_tree(raw);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "ok.txt");
    }

    #[test]
    fn parse_search_fetch_decodes_length_prefixed_stream() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"notes/today.md\n5\n");
        bytes.extend_from_slice(b"hello");
        bytes.extend_from_slice(b"top.txt\n0\n");
        let files = parse_search_fetch(&bytes);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], ("notes/today.md".to_string(), b"hello".to_vec()));
        assert_eq!(files[1], ("top.txt".to_string(), Vec::new()));
    }

    #[test]
    fn parse_search_fetch_stops_cleanly_on_a_truncated_trailing_chunk() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ok.txt\n2\n");
        bytes.extend_from_slice(b"hi");
        bytes.extend_from_slice(b"cut.txt\n100\n"); // claims 100 bytes but none follow
        bytes.extend_from_slice(b"short");
        let files = parse_search_fetch(&bytes);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], ("ok.txt".to_string(), b"hi".to_vec()));
    }

    #[test]
    fn parse_search_fetch_handles_an_empty_stream() {
        assert!(parse_search_fetch(&[]).is_empty());
    }

    #[test]
    fn parse_guarded_response_extracts_bytes_after_ok() {
        let mut bytes = b"OK\n".to_vec();
        bytes.extend_from_slice(b"\x89PNG...");
        assert_eq!(parse_guarded_response(&bytes).unwrap(), b"\x89PNG...".to_vec());
    }

    #[test]
    fn parse_guarded_response_reports_too_large() {
        let err = parse_guarded_response(b"TOOLARGE\t99999999\n").unwrap_err();
        assert!(err.contains("Too large"));
        assert!(err.contains("99999999"));
    }

    #[test]
    fn parse_guarded_response_rejects_malformed_header() {
        assert!(parse_guarded_response(b"???\n").is_err());
        assert!(parse_guarded_response(b"no newline at all").is_err());
    }

    #[test]
    fn agent_target_triple_maps_known_platforms() {
        assert_eq!(agent_target_triple("Linux x86_64"), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(agent_target_triple("Linux aarch64"), Some("aarch64-unknown-linux-gnu"));
        assert_eq!(agent_target_triple("Darwin arm64"), Some("aarch64-apple-darwin"));
        assert_eq!(agent_target_triple("Darwin x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(agent_target_triple("Windows_NT"), None);
    }
}
