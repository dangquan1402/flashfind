//! "Which project am I in?" — the current-project context for ranking.
//!
//! Explicit always wins: `--cwd DIR` (CLI), `cwd` (MCP find_files), or the
//! process's own working directory when it is inside a known repo (a shell,
//! or an agent that launched the MCP server in its workspace).
//!
//! For the launcher, [`detect`] guesses from the app that was frontmost when
//! the hotkey fired, using only what needs **no extra macOS permission**:
//! * VS Code-family editors (Cursor, VS Code, Windsurf, VSCodium): the window
//!   title when the caller could read it (CGWindowList titles need Screen
//!   Recording permission; without it the title is empty and we skip it),
//!   else `windowsState.lastActiveWindow` from the editor's
//!   `User/globalStorage/storage.json` (updated when windows open/close or
//!   the editor saves state, so it can lag behind window switches);
//! * terminals (iTerm2, Terminal, Ghostty, Warp, WezTerm, kitty, Alacritty):
//!   working directories of the terminal's shell processes (`lsof -d cwd`,
//!   same user, no permission), newest process first;
//! * anything else (Xcode, Zed, JetBrains…): files the app holds open
//!   (`lsof -p`), majority vote by repo.
//! Accessibility (AXTitle of the focused window) would be exact but needs a
//! permission prompt, so it is not used.

use crate::Repo;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Detected {
    /// Repo root of the current project.
    pub root: Option<PathBuf>,
    /// Frontmost app (bundle id or name).
    pub app: String,
    /// How it was found (title, storage.json, terminal cwd, open files, cwd).
    pub how: String,
}

impl Detected {
    pub fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Innermost known repo containing `p`.
pub fn repo_for<'a>(p: &Path, repos: &'a [Repo]) -> Option<&'a Repo> {
    repos.iter().filter(|r| p.starts_with(&r.path)).max_by_key(|r| r.path.as_os_str().len())
}

const VSCODE_FAMILY: &[(&str, &str)] = &[
    ("com.todesktop.230313mzl4w4u92", "Cursor"),
    ("com.microsoft.VSCode", "Code"),
    ("com.microsoft.VSCodeInsiders", "Code - Insiders"),
    ("com.exafunction.windsurf", "Windsurf"),
    ("com.vscodium", "VSCodium"),
];

const TERMINALS: &[&str] = &[
    "com.googlecode.iterm2",
    "com.apple.Terminal",
    "com.mitchellh.ghostty",
    "dev.warp.Warp-Stable",
    "com.github.wez.wezterm",
    "net.kovidgoyal.kitty",
    "org.alacritty",
];

/// Match window-title fragments ("main.rs — flashfind", "flashfind - Cursor")
/// to repo names; the most recently active repo wins ties.
pub fn from_title<'a>(title: &str, repos: &'a [Repo]) -> Option<&'a Repo> {
    let parts: Vec<&str> = title
        .split([' ', '—', '–', '-', '·', '|', '[', ']', '(', ')'])
        .map(str::trim)
        .filter(|s| s.len() >= 2)
        .collect();
    repos
        .iter()
        .filter(|r| parts.iter().any(|p| p.eq_ignore_ascii_case(&r.name)) || title.contains(&format!(" {} ", r.name)) || title.ends_with(&format!(" {}", r.name)))
        .max_by_key(|r| (r.name.len(), r.active))
}

fn uri_path(u: &str) -> Option<PathBuf> {
    let p = u.strip_prefix("file://")?;
    let mut out = Vec::new();
    let b = p.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&p[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

/// `windowsState.lastActiveWindow.folder` of a VS Code-family editor.
pub fn vscode_last_folder(app_dir: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let f = PathBuf::from(home).join("Library/Application Support").join(app_dir).join("User/globalStorage/storage.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(f).ok()?).ok()?;
    let ws = &v["windowsState"];
    let folder = ws["lastActiveWindow"]["folder"]
        .as_str()
        .or_else(|| ws["openedWindows"].as_array().and_then(|a| a.iter().find_map(|w| w["folder"].as_str())))
        .or_else(|| v["backupWorkspaces"]["folders"][0]["folderUri"].as_str())?;
    uri_path(folder)
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new(cmd).args(args).stderr(std::process::Stdio::null()).output().ok()?;
    // lsof exits 1 when some pid is gone or not ours but still prints the rest.
    (o.status.success() || !o.stdout.is_empty()).then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// All descendants of `pid` (via `ps`), newest (highest pid) first.
fn descendants(pid: i32) -> Vec<i32> {
    let Some(out) = run("ps", &["-axo", "pid=,ppid="]) else { return Vec::new() };
    let pairs: Vec<(i32, i32)> = out
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    let mut found = vec![pid];
    let mut i = 0;
    while i < found.len() {
        let p = found[i];
        found.extend(pairs.iter().filter(|(_, pp)| *pp == p).map(|(c, _)| *c));
        i += 1;
    }
    found.remove(0);
    found.sort_unstable_by(|a, b| b.cmp(a));
    found
}

/// Paths from `lsof -F n` output.
fn lsof_paths(args: &[&str]) -> Vec<PathBuf> {
    run("lsof", args)
        .map(|s| s.lines().filter_map(|l| l.strip_prefix('n')).filter(|p| p.starts_with('/')).map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// Guess the current project from the frontmost app (see module docs).
/// `title`: the focused window's title if the caller could read it.
pub fn detect(pid: i32, bundle: &str, title: Option<&str>, repos: &[Repo]) -> Detected {
    let mut d = Detected {
        app: bundle.to_string(),
        ..Default::default()
    };
    if let Some(t) = title.filter(|t| !t.trim().is_empty())
        && let Some(r) = from_title(t, repos)
    {
        d.root = Some(r.path.clone());
        d.how = "window title".into();
        return d;
    }
    if let Some((_, dir)) = VSCODE_FAMILY.iter().find(|(b, _)| *b == bundle) {
        if let Some(f) = vscode_last_folder(dir)
            && let Some(r) = repo_for(&f, repos)
        {
            d.root = Some(r.path.clone());
            d.how = format!("{dir} storage.json (last active window)");
            return d;
        }
    }
    if pid > 0 {
        let kids = descendants(pid);
        if TERMINALS.contains(&bundle) || !kids.is_empty() {
            // Newest shell/process first: usually the tab you just used.
            for chunk in kids.chunks(64) {
                let list = chunk.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
                let cwds = lsof_paths(&["-a", "-d", "cwd", "-F", "n", "-p", &list]);
                // lsof lists in pid order; prefer the highest pid that is in a repo.
                if let Some(r) = cwds.iter().rev().find_map(|c| repo_for(c, repos)) {
                    d.root = Some(r.path.clone());
                    d.how = if TERMINALS.contains(&bundle) { "terminal shell cwd".into() } else { "child process cwd".into() };
                    return d;
                }
            }
        }
        let open = lsof_paths(&["-F", "n", "-p", &pid.to_string()]);
        let mut votes: std::collections::HashMap<&Path, usize> = Default::default();
        for p in &open {
            if let Some(r) = repo_for(p, repos) {
                *votes.entry(&r.path).or_default() += 1;
            }
        }
        if let Some((root, _)) = votes.into_iter().max_by_key(|(_, n)| *n) {
            d.root = Some(root.to_path_buf());
            d.how = "open files".into();
            return d;
        }
    }
    d.how = "not detected".into();
    d
}

/// The frontmost app as (pid, bundle id), via `lsappinfo` (macOS, no permission).
pub fn frontmost() -> Option<(i32, String)> {
    let asn = run("lsappinfo", &["front"])?;
    let info = run("lsappinfo", &["info", "-only", "pid", "-only", "bundleid", asn.trim()])?;
    let mut pid = 0;
    let mut bundle = String::new();
    for l in info.lines() {
        if let Some(v) = l.trim().strip_prefix("\"pid\"=") {
            pid = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.trim().strip_prefix("\"CFBundleIdentifier\"=") {
            bundle = v.trim().trim_matches('"').to_string();
        }
    }
    (pid > 0).then_some((pid, bundle))
}
