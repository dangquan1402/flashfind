//! flashfind for code: find anything across all your code projects.
//!
//! * [`discover`] finds git repositories under configurable roots (default
//!   `~` minus `Library`, media folders, caches, hidden folders).
//! * [`CodeIndex::build`] crawls those repos with gitignore semantics (the
//!   `ignore` crate: `.gitignore`, `.git/info/exclude`, global excludes,
//!   `.ignore`) plus a default junk list (`node_modules`, `target`, `build`,
//!   `DerivedData`, `.venv`, `dist`, `Pods`, `.git` internals, …) into one
//!   flashfind [`Index`], so every query is a single in-process search.
//! * [`CodeIndex::search`] adds `repo:`, `lang:` and `dirty:` filters,
//!   boosts recently active repos and groups results by repo.
//! * [`mcp`] serves it to AI coding agents over MCP (stdio).

pub mod mcp;

use flashfind_core::walk::{KIND_DIR, KIND_FILE, KIND_LINK, KIND_MASK, KIND_OTHER, Listing, NONE};
use flashfind_core::{CancelToken, Index, Query, Session};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Folders never worth indexing in a code search (even when not ignored).
pub const JUNK: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "build",
    "dist",
    "out",
    "DerivedData",
    ".build",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    "Pods",
    "Carthage",
    ".gradle",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    ".cache",
    "coverage",
    "bower_components",
    ".terraform",
    ".idea",
    ".DS_Store",
    "zig-cache",
    ".zig-cache",
    "_build",
    "deps",
    ".dart_tool",
    ".expo",
];

/// Folders under a discovery root that are never searched for repos.
pub const DISCOVERY_SKIP: &[&str] = &[
    "Library",
    "Applications",
    "Movies",
    "Music",
    "Pictures",
    "Photos Library.photoslibrary",
    "Public",
    "System",
    "Volumes",
    "go",
    "miniconda3",
    "anaconda3",
];

#[derive(Clone, Debug)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    /// Extra folder names to skip during discovery.
    pub skip: Vec<String>,
    /// Folder names excluded from the code index (default: [`JUNK`]).
    pub junk: Vec<String>,
    /// Honour .gitignore & co. (default true).
    pub gitignore: bool,
    pub threads: usize,
    /// Discovery depth limit below each root.
    pub max_depth: usize,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let roots = match std::env::var("FLASHFIND_CODE_ROOTS") {
            Ok(v) if !v.trim().is_empty() => std::env::split_paths(&v).collect(),
            _ => vec![home],
        };
        Config {
            roots,
            skip: Vec::new(),
            junk: JUNK.iter().map(|s| s.to_string()).collect(),
            gitignore: true,
            threads: std::thread::available_parallelism().map_or(8, |n| n.get().min(16)),
            max_depth: 12,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repo {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Unix time of the last commit (from `.git/logs/HEAD`), 0 if unknown.
    pub last_commit: i64,
    /// Last activity: max(last commit, `.git/index` mtime).
    pub active: i64,
    /// Entries indexed for this repo.
    #[serde(default)]
    pub files: u64,
    #[serde(skip)]
    range: (u32, u32),
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p).ok().map_or(0, |m| flashfind_core::walk::mtime_of(&m))
}

/// The git dir of a working tree (`.git` folder, or the target of a
/// `.git` file for worktrees/submodules).
fn git_dir(repo: &Path) -> Option<PathBuf> {
    let g = repo.join(".git");
    let md = std::fs::symlink_metadata(&g).ok()?;
    if md.is_dir() {
        return Some(g);
    }
    let s = std::fs::read_to_string(&g).ok()?;
    let p = s.strip_prefix("gitdir:")?.trim();
    let p = Path::new(p);
    Some(if p.is_absolute() { p.to_path_buf() } else { repo.join(p) })
}

/// Read repo metadata without spawning git.
pub fn repo_info(path: &Path) -> Option<Repo> {
    let gd = git_dir(path)?;
    let branch = std::fs::read_to_string(gd.join("HEAD")).ok().map(|h| {
        h.trim()
            .strip_prefix("ref: refs/heads/")
            .map(str::to_string)
            .unwrap_or_else(|| h.trim().chars().take(12).collect())
    });
    let last_commit = std::fs::read(gd.join("logs/HEAD"))
        .ok()
        .and_then(|b| {
            let s = String::from_utf8_lossy(&b);
            let line = s.lines().rev().find(|l| !l.trim().is_empty())?.to_string();
            // "<old> <new> Name <email> <ts> <tz>\t<msg>"
            let head = line.split('\t').next()?;
            let after = head.rsplit_once('>')?.1;
            after.split_whitespace().next()?.parse::<i64>().ok()
        })
        .unwrap_or(0);
    let active = last_commit
        .max(mtime(&gd.join("index")))
        .max(if last_commit == 0 { mtime(&gd.join("HEAD")) } else { 0 });
    Some(Repo {
        name: path
            .file_name()
            .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned()),
        path: path.to_path_buf(),
        branch,
        last_commit,
        active,
        files: 0,
        range: (0, 0),
    })
}

/// Find git repositories under `cfg.roots`. A repo's own subtree is not
/// searched for further repos (submodules are part of their parent).
pub fn discover(cfg: &Config) -> Vec<Repo> {
    let found: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    for root in &cfg.roots {
        let Ok(root) = std::fs::canonicalize(root) else { continue };
        if git_dir(&root).is_some() {
            found.lock().unwrap().push(root.clone());
            continue;
        }
        let skip: Vec<String> = DISCOVERY_SKIP
            .iter()
            .map(|s| s.to_string())
            .chain(cfg.skip.iter().cloned())
            .chain(cfg.junk.iter().cloned())
            .collect();
        let f2 = found.clone();
        let walker = ignore::WalkBuilder::new(&root)
            .standard_filters(false)
            .hidden(true)
            .follow_links(false)
            .max_depth(Some(cfg.max_depth))
            .threads(cfg.threads)
            .filter_entry(move |e| {
                if !e.file_type().is_some_and(|t| t.is_dir()) {
                    return false;
                }
                if e.depth() > 0 {
                    let name = e.file_name().to_string_lossy();
                    // `.noindex` is macOS's "don't index this folder" convention.
                    if skip.iter().any(|s| *s == name) || name.ends_with(".app") || name.ends_with(".photoslibrary") || name.ends_with(".noindex") {
                        return false;
                    }
                    if git_dir(e.path()).is_some() {
                        f2.lock().unwrap().push(e.path().to_path_buf());
                        return false;
                    }
                }
                true
            })
            .build_parallel();
        walker.run(|| Box::new(|_| ignore::WalkState::Continue));
    }
    let mut paths = std::mem::take(&mut *found.lock().unwrap());
    paths.sort();
    paths.dedup();
    let mut repos: Vec<Repo> = paths.iter().filter_map(|p| repo_info(p)).collect();
    repos.sort_by(|a, b| b.active.cmp(&a.active).then(a.path.cmp(&b.path)));
    repos
}

#[cfg(unix)]
fn pbytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}
#[cfg(not(unix))]
fn pbytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

/// Deepest common ancestor folder of `paths`.
fn common_root(paths: &[PathBuf]) -> PathBuf {
    let mut it = paths.iter();
    let Some(first) = it.next() else { return PathBuf::from("/") };
    let mut root = first.parent().unwrap_or(first).to_path_buf();
    for p in it {
        while !p.starts_with(&root) {
            if !root.pop() {
                return PathBuf::from("/");
            }
        }
    }
    root
}

struct Ent {
    path: PathBuf,
    kind: u8,
    size: u64,
    mtime: i64,
}

/// Stitch walker entries into crawler listings below `root`.
fn listings(root: &Path, mut ents: Vec<Ent>) -> Vec<Listing> {
    ents.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let dir_mtime: HashMap<&Path, i64> = ents
        .iter()
        .filter(|e| e.kind & KIND_MASK == KIND_DIR)
        .map(|e| (e.path.as_path(), e.mtime))
        .collect();
    let mut ls: Vec<Listing> = vec![Listing::default()];
    let mut ids: HashMap<PathBuf, u32> = HashMap::new();
    ids.insert(root.to_path_buf(), 0);
    fn ensure(dir: &Path, root: &Path, ls: &mut Vec<Listing>, ids: &mut HashMap<PathBuf, u32>, mt: &HashMap<&Path, i64>) -> Option<u32> {
        if let Some(&i) = ids.get(dir) {
            return Some(i);
        }
        if !dir.starts_with(root) || dir == root {
            return None;
        }
        let parent = ensure(dir.parent()?, root, ls, ids, mt)?;
        let id = ls.len() as u32;
        ls.push(Listing { id, ..Default::default() });
        let name = pbytes(Path::new(dir.file_name()?));
        let i = ls[parent as usize].push(&name, KIND_DIR, 0, mt.get(dir).copied().unwrap_or(0))?;
        ls[parent as usize].ents[i].child = id;
        ids.insert(dir.to_path_buf(), id);
        Some(id)
    }
    for e in &ents {
        if e.kind & KIND_MASK == KIND_DIR {
            ensure(&e.path, root, &mut ls, &mut ids, &dir_mtime);
        } else if let (Some(parent), Some(name)) = (e.path.parent(), e.path.file_name())
            && let Some(pid) = ensure(parent, root, &mut ls, &mut ids, &dir_mtime)
        {
            ls[pid as usize].push(&pbytes(Path::new(name)), e.kind, e.size, e.mtime);
        }
    }
    let _ = NONE;
    ls
}

/// Statistics of a build.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildInfo {
    pub repos: usize,
    pub entries: usize,
    pub discover_ms: f64,
    pub crawl_ms: f64,
    pub build_ms: f64,
    pub index_bytes: usize,
    pub built_at: i64,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    roots: Vec<PathBuf>,
    repos: Vec<Repo>,
    info: BuildInfo,
}

pub struct CodeIndex {
    pub idx: Arc<Index>,
    /// Sorted by their subtree start in the index.
    pub repos: Vec<Repo>,
    pub roots: Vec<PathBuf>,
    pub info: BuildInfo,
    /// Entries outside every repo (folders between the root and the repos).
    outside: Vec<u64>,
    dirty: Mutex<HashMap<usize, (Instant, Vec<u32>)>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodeHit {
    /// Index into [`CodeIndex::repos`].
    pub repo: usize,
    pub path: String,
    /// Path relative to the repo root.
    pub rel: String,
    pub score: i32,
    pub kind: &'static str,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, Default)]
pub struct CodeResult {
    pub hits: Vec<CodeHit>,
    pub total: u64,
    pub elapsed_us: u64,
    /// (repo, hit indices) in order of each repo's best hit.
    pub groups: Vec<(usize, Vec<usize>)>,
}

/// A query with code filters split out.
#[derive(Clone, Debug, Default)]
pub struct CodeQuery {
    pub text: String,
    pub repos: Vec<String>,
    pub langs: Vec<String>,
    pub dirty: bool,
    pub limit: usize,
}

/// `lang:` → extensions.
pub fn lang_exts(lang: &str) -> Option<&'static [&'static str]> {
    Some(match lang.to_ascii_lowercase().as_str() {
        "rust" | "rs" => &["rs"],
        "ts" | "typescript" => &["ts", "tsx", "mts", "cts"],
        "js" | "javascript" => &["js", "jsx", "mjs", "cjs"],
        "py" | "python" => &["py", "pyi", "ipynb"],
        "go" | "golang" => &["go"],
        "swift" => &["swift"],
        "c" => &["c", "h"],
        "cpp" | "c++" | "cxx" => &["cc", "cpp", "cxx", "hpp", "hh", "hxx", "h"],
        "objc" | "objective-c" => &["m", "mm", "h"],
        "java" => &["java"],
        "kotlin" | "kt" => &["kt", "kts"],
        "ruby" | "rb" => &["rb"],
        "php" => &["php"],
        "cs" | "csharp" | "c#" => &["cs"],
        "zig" => &["zig"],
        "lua" => &["lua"],
        "shell" | "sh" | "bash" => &["sh", "bash", "zsh"],
        "md" | "markdown" | "docs" => &["md", "mdx", "rst"],
        "json" => &["json"],
        "yaml" | "yml" => &["yaml", "yml"],
        "toml" => &["toml"],
        "html" => &["html", "htm"],
        "css" => &["css", "scss", "sass", "less"],
        "vue" => &["vue"],
        "svelte" => &["svelte"],
        "dart" => &["dart"],
        "scala" => &["scala"],
        "elixir" | "ex" => &["ex", "exs"],
        "sql" => &["sql"],
        _ => return None,
    })
}

impl CodeQuery {
    /// Split `repo:a,b`, `lang:rust`, `dirty:`/`dirty:yes` out of `s`; the
    /// rest is a normal flashfind query.
    pub fn parse(s: &str) -> CodeQuery {
        let mut q = CodeQuery {
            limit: 20,
            ..Default::default()
        };
        let mut rest = Vec::new();
        for w in s.split_whitespace() {
            let lw = w.to_ascii_lowercase();
            if let Some(v) = lw.strip_prefix("repo:").or_else(|| lw.strip_prefix("project:")) {
                q.repos.extend(w[w.len() - v.len()..].split(',').filter(|x| !x.is_empty()).map(str::to_string));
            } else if let Some(v) = lw.strip_prefix("lang:") {
                q.langs.extend(v.split(',').filter(|x| !x.is_empty()).map(str::to_string));
            } else if let Some(v) = lw.strip_prefix("dirty:") {
                q.dirty = !matches!(v, "no" | "false" | "0");
            } else if lw == "dirty" && s.split_whitespace().count() == 1 {
                q.dirty = true;
            } else {
                rest.push(w);
            }
        }
        q.text = rest.join(" ");
        q
    }
}

fn kind_name(k: u8) -> &'static str {
    match k & KIND_MASK {
        KIND_FILE => "file",
        KIND_DIR => "dir",
        KIND_LINK => "link",
        KIND_OTHER => "other",
        _ => "other",
    }
}

/// Score bonus for a repo active `age` seconds ago (stays inside a score band).
pub fn recency_boost(age: i64) -> i32 {
    match age {
        a if a < 86_400 => 30,
        a if a < 7 * 86_400 => 20,
        a if a < 30 * 86_400 => 10,
        a if a < 180 * 86_400 => 4,
        _ => 0,
    }
}

impl CodeIndex {
    /// Discover repos and index them.
    pub fn build(cfg: &Config) -> CodeIndex {
        let t0 = Instant::now();
        let repos = discover(cfg);
        let discover_ms = t0.elapsed().as_secs_f64() * 1e3;
        CodeIndex::build_repos(cfg, repos, discover_ms)
    }

    /// Index already discovered repos.
    pub fn build_repos(cfg: &Config, mut repos: Vec<Repo>, discover_ms: f64) -> CodeIndex {
        let t1 = Instant::now();
        let paths: Vec<PathBuf> = repos.iter().map(|r| r.path.clone()).collect();
        let root = if paths.len() == 1 {
            paths[0].parent().unwrap_or(&paths[0]).to_path_buf()
        } else {
            common_root(&paths)
        };
        let (tx, rx) = std::sync::mpsc::channel::<Vec<Ent>>();
        if let Some((first, rest)) = paths.split_first() {
            let mut wb = ignore::WalkBuilder::new(first);
            for p in rest {
                wb.add(p);
            }
            let junk = cfg.junk.clone();
            wb.hidden(false)
                .git_ignore(cfg.gitignore)
                .git_exclude(cfg.gitignore)
                .git_global(cfg.gitignore)
                .ignore(cfg.gitignore)
                .parents(cfg.gitignore)
                .require_git(false)
                .follow_links(false)
                .threads(cfg.threads)
                .filter_entry(move |e| {
                    if e.depth() == 0 {
                        return true;
                    }
                    let n = e.file_name().to_string_lossy();
                    !(junk.iter().any(|j| *j == n) && e.file_type().is_some_and(|t| t.is_dir()) || n == ".git" || n == ".DS_Store")
                });
            wb.build_parallel().run(|| {
                let tx = tx.clone();
                let mut buf: Vec<Ent> = Vec::with_capacity(4096);
                struct Flush(std::sync::mpsc::Sender<Vec<Ent>>, Vec<Ent>);
                impl Drop for Flush {
                    fn drop(&mut self) {
                        let _ = self.0.send(std::mem::take(&mut self.1));
                    }
                }
                let mut f = Flush(tx, std::mem::take(&mut buf));
                Box::new(move |r| {
                    let Ok(e) = r else { return ignore::WalkState::Continue };
                    let Some(ft) = e.file_type() else { return ignore::WalkState::Continue };
                    let md = e.metadata().ok();
                    let kind = if ft.is_symlink() {
                        KIND_LINK
                    } else if ft.is_dir() {
                        KIND_DIR
                    } else if ft.is_file() {
                        KIND_FILE
                    } else {
                        KIND_OTHER
                    };
                    let (size, mt) = md.map_or((0, 0), |m| (if kind == KIND_FILE { m.len() } else { 0 }, flashfind_core::walk::mtime_of(&m)));
                    f.1.push(Ent {
                        path: e.into_path(),
                        kind,
                        size,
                        mtime: mt,
                    });
                    if f.1.len() >= 4096 {
                        let _ = f.0.send(std::mem::take(&mut f.1));
                    }
                    ignore::WalkState::Continue
                })
            });
        }
        drop(tx);
        let ents: Vec<Ent> = rx.into_iter().flatten().collect();
        let crawl_ms = t1.elapsed().as_secs_f64() * 1e3;
        let t2 = Instant::now();
        let ls = listings(&root, ents);
        let refs: Vec<&Listing> = ls.iter().collect();
        let idx = Index::build_refs(&refs, &root, now(), 0);
        drop(refs);
        drop(ls);
        let build_ms = t2.elapsed().as_secs_f64() * 1e3;
        let info = BuildInfo {
            repos: repos.len(),
            entries: idx.n,
            discover_ms,
            crawl_ms,
            build_ms,
            index_bytes: idx.bytes(),
            built_at: now(),
        };
        repos.sort_by(|a, b| a.path.cmp(&b.path));
        CodeIndex::assemble(Arc::new(idx), repos, cfg.roots.clone(), info)
    }

    fn assemble(idx: Arc<Index>, mut repos: Vec<Repo>, roots: Vec<PathBuf>, info: BuildInfo) -> CodeIndex {
        for r in &mut repos {
            r.range = match idx.lookup(&pbytes(&r.path)) {
                Some(e) => (e, idx.end(e)),
                None => (0, 0),
            };
            r.files = (r.range.1 - r.range.0) as u64;
        }
        repos.retain(|r| r.range.1 > r.range.0);
        repos.sort_by_key(|r| r.range.0);
        let mut outside = vec![!0u64; idx.n.div_ceil(64)];
        for r in &repos {
            for e in r.range.0..r.range.1 {
                outside[e as usize >> 6] &= !(1 << (e & 63));
            }
        }
        CodeIndex {
            idx,
            repos,
            roots,
            info,
            outside,
            dirty: Mutex::new(HashMap::new()),
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        self.idx.save(&dir.join("code.ffx"))?;
        let meta = Meta {
            roots: self.roots.clone(),
            repos: self.repos.clone(),
            info: self.info.clone(),
        };
        let tmp = dir.join("code.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&meta).unwrap())?;
        std::fs::rename(tmp, dir.join("code.json"))
    }

    pub fn load(dir: &Path) -> std::io::Result<CodeIndex> {
        let idx = Index::load(&dir.join("code.ffx"))?;
        let meta: Meta = serde_json::from_slice(&std::fs::read(dir.join("code.json"))?).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(CodeIndex::assemble(Arc::new(idx), meta.repos, meta.roots, meta.info))
    }

    /// Default cache folder: `$FLASHFIND_CACHE` or `~/.cache/flashfind`.
    pub fn default_dir() -> PathBuf {
        if let Some(p) = std::env::var_os("FLASHFIND_CACHE") {
            return PathBuf::from(p);
        }
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".cache/flashfind")
    }

    pub fn age(&self) -> Duration {
        Duration::from_secs((now() - self.info.built_at).max(0) as u64)
    }

    /// Repo containing entry `e` (innermost).
    pub fn repo_of(&self, e: u32) -> Option<usize> {
        let mut i = self.repos.partition_point(|r| r.range.0 <= e);
        while i > 0 {
            i -= 1;
            let r = &self.repos[i].range;
            if e < r.1 {
                return Some(i);
            }
        }
        None
    }

    /// Repos matching a `repo:` value: exact name, else prefix, else substring.
    pub fn match_repos(&self, want: &str) -> Vec<usize> {
        let w = want.to_ascii_lowercase();
        let names: Vec<String> = self.repos.iter().map(|r| r.name.to_ascii_lowercase()).collect();
        for f in [|n: &str, w: &str| n == w, |n: &str, w: &str| n.starts_with(w), |n: &str, w: &str| n.contains(w)] {
            let v: Vec<usize> = (0..names.len()).filter(|&i| f(&names[i], &w)).collect();
            if !v.is_empty() {
                return v;
            }
        }
        Vec::new()
    }

    /// Entries with uncommitted changes in repo `i` (`git status`, cached 5 s).
    pub fn dirty_entries(&self, i: usize) -> Vec<u32> {
        if let Some((t, v)) = self.dirty.lock().unwrap().get(&i)
            && t.elapsed() < Duration::from_secs(5)
        {
            return v.clone();
        }
        let r = &self.repos[i];
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&r.path)
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])
            .stderr(std::process::Stdio::null())
            .output();
        let mut v = Vec::new();
        if let Ok(o) = out {
            let mut it = o.stdout.split(|&b| b == 0).filter(|s| s.len() > 3);
            while let Some(rec) = it.next() {
                let (xy, p) = rec.split_at(3);
                if xy[0] == b'R' || xy[0] == b'C' {
                    it.next(); // the source path of a rename/copy
                }
                let mut full = pbytes(&r.path);
                full.push(b'/');
                full.extend_from_slice(p);
                if full.ends_with(b"/") {
                    full.pop();
                }
                if let Some(e) = self.idx.lookup(&full) {
                    // An untracked folder: everything in it.
                    v.extend(e..self.idx.end(e));
                }
            }
        }
        v.sort_unstable();
        v.dedup();
        self.dirty.lock().unwrap().insert(i, (Instant::now(), v.clone()));
        v
    }

    fn mask(&self, cq: &CodeQuery) -> Result<Vec<u64>, String> {
        let mut mask = self.outside.clone();
        let mut selected: Option<Vec<usize>> = None;
        if !cq.repos.is_empty() {
            let mut v = Vec::new();
            for r in &cq.repos {
                let m = self.match_repos(r);
                if m.is_empty() {
                    return Err(format!("no project matches repo:{r}"));
                }
                v.extend(m);
            }
            v.sort();
            v.dedup();
            let mut keep = vec![0u64; mask.len()];
            for &i in &v {
                let (a, b) = self.repos[i].range;
                for e in a..b {
                    keep[e as usize >> 6] |= 1 << (e & 63);
                }
            }
            for (m, k) in mask.iter_mut().zip(&keep) {
                *m |= !k;
            }
            selected = Some(v);
        }
        if cq.dirty {
            let which: Vec<usize> = selected.unwrap_or_else(|| (0..self.repos.len()).collect());
            use rayon::prelude::*;
            let ents: Vec<u32> = which.par_iter().flat_map_iter(|&i| self.dirty_entries(i)).collect();
            let mut keep = vec![0u64; mask.len()];
            for e in ents {
                keep[e as usize >> 6] |= 1 << (e & 63);
            }
            for (m, k) in mask.iter_mut().zip(&keep) {
                *m |= !k;
            }
        }
        Ok(mask)
    }

    fn hit(&self, e: u32, score: i32) -> Option<CodeHit> {
        let repo = self.repo_of(e)?;
        let path = self.idx.path_string(e);
        let root = self.repos[repo].path.to_string_lossy();
        let rel = path.strip_prefix(root.as_ref()).unwrap_or(&path).trim_start_matches('/').to_string();
        Some(CodeHit {
            repo,
            rel: if rel.is_empty() { ".".into() } else { rel },
            path,
            score,
            kind: kind_name(self.idx.ent_kind()[e as usize]),
            size: self.idx.size(e),
            mtime: self.idx.mtime(e),
        })
    }

    /// Search with code filters, recency boost and repo grouping.
    pub fn search(&self, sess: &mut Session, cq: &CodeQuery, cancel: &CancelToken) -> Result<CodeResult, String> {
        let t0 = Instant::now();
        let mut q = Query::parse(&cq.text)?;
        for l in &cq.langs {
            let exts = lang_exts(l).ok_or_else(|| format!("unknown lang:{l}"))?;
            q.exts.extend(exts.iter().map(|e| e.as_bytes().to_vec()));
        }
        let limit = cq.limit.max(1);
        // Fetch extra so the recency boost can reorder within score bands.
        q.limit = (limit * 3).max(30);
        let mask = self.mask(cq)?;
        let r = sess.search_masked(&q, cancel, Some(&mask)).map_err(|e| e.to_string())?;
        let t = now();
        let mut hits: Vec<CodeHit> = r
            .hits
            .iter()
            .filter_map(|h| self.hit(h.entry, h.score))
            .map(|mut h| {
                h.score += recency_boost(t - self.repos[h.repo].active);
                h
            })
            .collect();
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        hits.truncate(limit);
        let groups = group(&hits);
        Ok(CodeResult {
            hits,
            total: r.total,
            elapsed_us: t0.elapsed().as_micros() as u64,
            groups,
        })
    }

    /// Most recently modified files (optionally in some repos / languages,
    /// modified within `within` seconds).
    pub fn recent(&self, cq: &CodeQuery, within: Option<i64>) -> Result<CodeResult, String> {
        let t0 = Instant::now();
        let mask = self.mask(cq)?;
        let mut exts: Vec<&str> = Vec::new();
        for l in &cq.langs {
            exts.extend(lang_exts(l).ok_or_else(|| format!("unknown lang:{l}"))?.iter());
        }
        let since = within.map_or(i64::MIN, |w| now() - w);
        let idx = &self.idx;
        let (kinds, mt) = (idx.ent_kind(), idx.ent_mtime_raw());
        let mut cand: Vec<(i64, u32)> = Vec::new();
        for e in 0..idx.n as u32 {
            if (mask[e as usize >> 6] >> (e & 63)) & 1 != 0 || kinds[e as usize] & KIND_MASK != KIND_FILE {
                continue;
            }
            let m = mt[e as usize] as i64;
            if m < since {
                continue;
            }
            if !exts.is_empty() {
                let n = idx.lower(idx.ent_name()[e as usize]);
                if !exts
                    .iter()
                    .any(|x| n.len() > x.len() && n.ends_with(x.as_bytes()) && n[n.len() - x.len() - 1] == b'.')
                {
                    continue;
                }
            }
            cand.push((m, e));
        }
        let total = cand.len() as u64;
        let k = cq.limit.max(1).min(cand.len());
        if k > 0 && cand.len() > k {
            cand.select_nth_unstable_by(k - 1, |a, b| b.cmp(a));
            cand.truncate(k);
        }
        cand.sort_by(|a, b| b.cmp(a));
        let hits: Vec<CodeHit> = cand.iter().filter_map(|&(_, e)| self.hit(e, 0)).collect();
        let groups = group(&hits);
        Ok(CodeResult {
            hits,
            total,
            elapsed_us: t0.elapsed().as_micros() as u64,
            groups,
        })
    }

    /// Compact, agent-friendly JSON: results grouped by repo.
    pub fn result_json(&self, r: &CodeResult) -> serde_json::Value {
        let groups: Vec<serde_json::Value> = r
            .groups
            .iter()
            .map(|(ri, hs)| {
                let repo = &self.repos[*ri];
                serde_json::json!({
                    "repo": repo.name,
                    "root": repo.path,
                    "files": hs.iter().map(|&i| &r.hits[i].rel).collect::<Vec<_>>(),
                })
            })
            .collect();
        serde_json::json!({"total": r.total, "shown": r.hits.len(), "us": r.elapsed_us, "groups": groups})
    }

    pub fn projects_json(&self, filter: Option<&str>, limit: usize) -> serde_json::Value {
        let mut v: Vec<&Repo> = self
            .repos
            .iter()
            .filter(|r| filter.is_none_or(|f| r.name.to_ascii_lowercase().contains(&f.to_ascii_lowercase())))
            .collect();
        v.sort_by(|a, b| b.active.cmp(&a.active).then(a.name.cmp(&b.name)));
        let shown: Vec<serde_json::Value> = v
            .iter()
            .take(limit)
            .map(|r| serde_json::json!({"name": r.name, "root": r.path, "branch": r.branch, "last_commit": r.last_commit, "active": r.active, "entries": r.files}))
            .collect();
        serde_json::json!({"total": v.len(), "projects": shown})
    }
}

fn group(hits: &[CodeHit]) -> Vec<(usize, Vec<usize>)> {
    let mut order: Vec<(usize, Vec<usize>)> = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        match order.iter_mut().find(|(r, _)| *r == h.repo) {
            Some((_, v)) => v.push(i),
            None => order.push((h.repo, vec![i])),
        }
    }
    order
}
