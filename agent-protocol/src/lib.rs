//! Shared between `src-tauri` (the client, both for local workspaces and
//! as the SSH agent's caller) and `agent` (the binary deployed to a
//! remote host and run persistently over one `ssh` subprocess's
//! stdin/stdout - see kotoshelf's SSH-perf plan). Kept dependency-free
//! beyond serde so `agent` cross-compiles cleanly for remote targets
//! without pulling in Tauri.

use std::io::{self, Read, Write};
use std::path::Path;

/// One node of the workspace file tree. `children` is `Some` for
/// directories (possibly empty) and `None` for files, so the frontend can
/// distinguish "empty dir" from "file" without consulting `is_dir` twice.
/// `size` is 0 for directories - carried alongside the listing (rather
/// than fetched separately per file) so the frontend can flag large
/// files without an extra round trip per open, which matters most for
/// SSH workspaces where "a round trip" means a network hop.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub children: Option<Vec<TreeNode>>,
    pub size: u64,
}

/// Directories that never belong in a notes workspace listing.
pub const SKIP_DIRS: &[&str] = &[".git", ".kotoshelf", "node_modules", "target"];

/// `true` for a directory name the tree/workspace-scan should never
/// show - either one of the fixed `SKIP_DIRS` names, or (the user's own
/// explicit request, 2026-08-12) any dot-prefixed "hidden folder"
/// convention (`.obsidian`, `.vscode`, `.trash`, ...) generally, not
/// just the specific handful already named. Directories only - a
/// dotfile like `.gitignore` is left visible, since the ask was
/// specifically about folders.
pub fn is_skipped_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || name.starts_with('.')
}

/// Hard recursion limit as a guard against pathological or cyclic
/// (junction/symlink) directory structures.
pub const MAX_DEPTH: usize = 24;

/// `max_depth` bounds recursion: a directory at `depth >= max_depth` gets
/// `children: None` instead of being walked, the same `None` a file
/// already uses. `read_tree` (unbounded) and `read_tree_shallow`
/// (`max_depth: 1`) share this one walker. Paths come back **absolute**
/// (`dir.join(name)`), matching kotoshelf's local-workspace convention -
/// callers that need workspace-relative paths (the SSH agent, over the
/// wire) relativize the result with `relativize` afterward rather than
/// this function knowing about that convention itself.
pub fn build_tree(dir: &Path, depth: usize, max_depth: usize) -> Vec<TreeNode> {
    if depth >= max_depth {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut nodes: Vec<TreeNode> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = path.is_dir();
        if is_dir && is_skipped_dir(&name) {
            continue;
        }
        let children = if is_dir && depth + 1 < max_depth {
            Some(build_tree(&path, depth + 1, max_depth))
        } else {
            None
        };
        let size = if is_dir {
            0
        } else {
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
        };
        nodes.push(TreeNode {
            name,
            path: path.to_string_lossy().into_owned(),
            is_dir,
            children,
            size,
        });
    }

    // Directories first, then files; alphabetical (case-insensitive)
    // within each group - the ordering users expect from VS Code.
    nodes.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    nodes
}

/// Recursively strips any directory node `is_skipped_dir` would reject -
/// a client-side safety net applied to a tree regardless of where it
/// came from. Needed specifically because an SSH workspace's tree can
/// come from the *persistent agent*, a separate binary deployed once to
/// the remote host and cached there by version string
/// (`~/.kotoshelf/agent/kotoshelf-agent-<version>`) - rebuilding this
/// crate's own `is_skipped_dir` does NOT retroactively fix an already-
/// deployed remote agent binary built before the fix, and redeploying it
/// requires cross-compiling for the remote's platform (not always
/// possible from every dev machine). Filtering again here, after the
/// tree comes back from wherever it came from, means the *client's* skip
/// rules always win regardless of how stale the far end is - confirmed
/// necessary live (2026-08-12): a `.venv` folder kept showing over SSH
/// through the persistent agent specifically, even after `build_tree`
/// itself (used by the *local*, non-SSH path, and by the agent binary's
/// own `read_tree` call internally - but only once *that* binary is
/// actually rebuilt and redeployed) was already fixed.
pub fn filter_tree(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    nodes
        .into_iter()
        .filter(|n| !(n.is_dir && is_skipped_dir(&n.name)))
        .map(|n| TreeNode { children: n.children.map(filter_tree), ..n })
        .collect()
}

pub fn read_tree(root: &str) -> Result<Vec<TreeNode>, String> {
    let path = Path::new(root);
    if !path.is_dir() {
        return Err(format!("Not a directory: {root}"));
    }
    Ok(build_tree(path, 0, MAX_DEPTH))
}

/// One level of `dir`'s immediate children - subdirectories come back
/// with `children: None` ("not loaded yet") rather than being walked.
pub fn read_tree_shallow(dir: &str) -> Result<Vec<TreeNode>, String> {
    let path = Path::new(dir);
    if !path.is_dir() {
        return Err(format!("Not a directory: {dir}"));
    }
    Ok(build_tree(path, 0, 1))
}

/// Rewrites every `path` in `nodes` (recursively) from absolute to
/// relative-to-`root`, forward-slash-joined with no leading slash at the
/// root - the wire convention `ssh_read_tree`/`ssh_read_tree_shallow`'s
/// callers already expect (other SSH commands take a `rel_path` joined
/// against `profile.remote_path` server-side). Only the agent's request
/// handlers call this; `read_tree`/`read_tree_shallow` stay
/// absolute-path so `src-tauri`'s *local* (non-SSH) commands can keep
/// using them unchanged.
pub fn relativize(nodes: Vec<TreeNode>, root: &Path) -> Vec<TreeNode> {
    nodes
        .into_iter()
        .map(|n| {
            let abs = Path::new(&n.path);
            let rel = abs
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(n.path);
            TreeNode {
                path: rel,
                children: n.children.map(|c| relativize(c, root)),
                ..n
            }
        })
        .collect()
}

// ---------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Request {
    pub id: u64,
    pub body: RequestBody,
}

/// `tag = "type", content = "data"` (adjacent tagging), not plain
/// `tag = "type"` (internal tagging): a newtype variant wrapping a `Vec`
/// (`ResponseBody::Tree`) serializes as a JSON array, which internal
/// tagging can't inject a sibling `"type"` key into - `serde_json` hits
/// unbounded recursion (a hang, not a clean error) trying anyway. Hit
/// this for real during development; keep both enums adjacently tagged.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum RequestBody {
    /// Handshake / liveness check after spawning the agent.
    Ping,
    ReadTree { root: String },
    ReadTreeShallow { root: String, rel_path: String },
    /// Asks the agent to exit its dispatch loop and return, so the
    /// client can wait for a clean process exit instead of killing it.
    Shutdown,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Response {
    pub id: u64,
    pub body: ResponseBody,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ResponseBody {
    Pong,
    Tree(Vec<TreeNode>),
    Ok,
    Error { message: String },
}

/// 4-byte little-endian length prefix + a `serde_json`-encoded payload.
/// No new dependency beyond `serde_json` (already used throughout
/// kotoshelf for `AppConfig`/`TreeNode`/etc.), and no async runtime
/// needed since both sides just block on read/write of a known-length
/// chunk - it's called from either a plain `std::io::Read`/`Write` over
/// a `Child`'s piped stdin/stdout (`src-tauri`) or over the agent's own
/// stdin/stdout (`agent`).
pub fn write_frame<W: Write, T: serde::Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// `Ok(None)` on a clean EOF at a frame boundary (the other side closed
/// the pipe between frames, not mid-frame) - distinguishes "the agent
/// exited normally" from a real I/O error the caller should surface.
pub fn read_frame<R: Read, T: serde::de::DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_tree_strips_hidden_dirs_recursively_but_keeps_everything_else() {
        let nodes = vec![
            TreeNode { name: ".venv".into(), path: "/ws/.venv".into(), is_dir: true, children: Some(vec![]), size: 0 },
            TreeNode {
                name: "notes".into(),
                path: "/ws/notes".into(),
                is_dir: true,
                children: Some(vec![TreeNode {
                    name: ".venv".into(),
                    path: "/ws/notes/.venv".into(),
                    is_dir: true,
                    children: Some(vec![]),
                    size: 0,
                }]),
                size: 0,
            },
            TreeNode { name: ".gitignore".into(), path: "/ws/.gitignore".into(), is_dir: false, children: None, size: 3 },
        ];
        let filtered = filter_tree(nodes);
        let names: Vec<&str> = filtered.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["notes", ".gitignore"]);
        let notes_children = filtered[0].children.as_ref().unwrap();
        assert!(notes_children.is_empty(), "the nested .venv should be stripped too");
    }

    #[test]
    fn is_skipped_dir_matches_fixed_names_and_any_dot_prefix() {
        assert!(is_skipped_dir(".git"));
        assert!(is_skipped_dir("node_modules"));
        assert!(is_skipped_dir(".obsidian")); // not in the fixed list, but dot-prefixed
        assert!(is_skipped_dir(".trash"));
        assert!(!is_skipped_dir("notes"));
        assert!(!is_skipped_dir("Templates"));
    }

    /// Real filesystem regression guard for the user's own request
    /// (2026-08-12): "don't show dot-prefixed hidden folders in the
    /// tree" - a `.obsidian` folder (not one of the fixed `SKIP_DIRS`
    /// names) must not appear, while an ordinary folder and a dotFILE
    /// stay visible.
    #[test]
    fn build_tree_omits_any_dot_prefixed_directory_but_keeps_dotfiles() {
        let dir = std::env::temp_dir().join(format!("agent_protocol_test_{}_{}", std::process::id(), line!()));
        std::fs::create_dir_all(dir.join(".obsidian")).unwrap();
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join(".obsidian").join("workspace.json"), "{}").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("notes").join("today.md"), "# Today\n").unwrap();

        let nodes = build_tree(&dir, 0, MAX_DEPTH);
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(!names.contains(&".obsidian"), "hidden folder should be omitted: {names:?}");
        assert!(names.contains(&"notes"));
        assert!(names.contains(&".gitignore"), "a dotFILE (not a folder) should stay visible: {names:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frame_round_trips_through_a_byte_buffer() {
        let req = Request {
            id: 42,
            body: RequestBody::ReadTreeShallow {
                root: "/home/user/notes".into(),
                rel_path: "sub".into(),
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded: Request = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded.id, 42);
        match decoded.body {
            RequestBody::ReadTreeShallow { root, rel_path } => {
                assert_eq!(root, "/home/user/notes");
                assert_eq!(rel_path, "sub");
            }
            _ => panic!("wrong variant"),
        }
    }

    /// Regression guard for the internal-tagging recursion bug (see the
    /// doc comment on `RequestBody`): a `Tree` response actually
    /// carrying nodes - not the empty/unit-variant cases the other
    /// tests exercise - is the shape that hung `serde_json::to_vec`
    /// before switching to adjacent tagging.
    #[test]
    fn tree_response_with_nodes_serializes_without_hanging() {
        let resp = Response {
            id: 7,
            body: ResponseBody::Tree(vec![TreeNode {
                name: "foo.md".into(),
                path: "foo.md".into(),
                is_dir: false,
                children: None,
                size: 3,
            }]),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &resp).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded: Response = read_frame(&mut cursor).unwrap().unwrap();
        match decoded.body {
            ResponseBody::Tree(nodes) => assert_eq!(nodes.len(), 1),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn read_frame_reports_clean_eof_as_none() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let decoded: Option<Request> = read_frame(&mut cursor).unwrap();
        assert!(decoded.is_none());
    }

    #[test]
    fn relativize_strips_root_and_keeps_root_level_paths_bare() {
        let nodes = vec![TreeNode {
            name: "foo.md".into(),
            path: "/home/user/notes/foo.md".into(),
            is_dir: false,
            children: None,
            size: 10,
        }, TreeNode {
            name: "sub".into(),
            path: "/home/user/notes/sub".into(),
            is_dir: true,
            children: Some(vec![TreeNode {
                name: "bar.md".into(),
                path: "/home/user/notes/sub/bar.md".into(),
                is_dir: false,
                children: None,
                size: 5,
            }]),
            size: 0,
        }];
        let out = relativize(nodes, Path::new("/home/user/notes"));
        assert_eq!(out[0].path, "foo.md");
        assert_eq!(out[1].path, "sub");
        assert_eq!(out[1].children.as_ref().unwrap()[0].path, "sub/bar.md");
    }

    /// A workspace rooted at "." (ssh's login directory): `read_dir(".")`
    /// yields `./x` style paths, which must still come out root-relative.
    #[test]
    fn relativize_handles_a_dot_root() {
        let nodes = vec![TreeNode {
            name: "foo.md".into(),
            path: "./foo.md".into(),
            is_dir: false,
            children: None,
            size: 1,
        }, TreeNode {
            name: "sub".into(),
            path: "./sub".into(),
            is_dir: true,
            children: Some(vec![TreeNode {
                name: "bar.md".into(),
                path: "./sub/bar.md".into(),
                is_dir: false,
                children: None,
                size: 1,
            }]),
            size: 0,
        }];
        let out = relativize(nodes, Path::new("."));
        assert_eq!(out[0].path, "foo.md");
        assert_eq!(out[1].children.as_ref().unwrap()[0].path, "sub/bar.md");
    }

    /// kotoshelf has a documented history of exactly this bug in the SSH
    /// path-handling code (a trailing-slash workspace root breaking
    /// root-relative paths) - `profile.remote_path` is free-typed by the
    /// user in the remote-workspace dialog and a trailing slash is a
    /// realistic input, not a hypothetical.
    #[test]
    fn relativize_is_unaffected_by_a_trailing_slash_on_root() {
        let nodes = vec![TreeNode {
            name: "foo.md".into(),
            path: "/home/user/notes/foo.md".into(),
            is_dir: false,
            children: None,
            size: 10,
        }];
        let out = relativize(nodes, Path::new("/home/user/notes/"));
        assert_eq!(out[0].path, "foo.md");
    }
}
