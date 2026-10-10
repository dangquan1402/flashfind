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

pub mod context;
pub mod git;
pub mod live;
pub mod mcp;
pub mod memory;
pub mod rank;

pub use live::{CodeLive, CodeSession, LiveStats};
pub use memory::{Memory, Recall};
pub use rank::{Features, RankInput, Ranker};

use flashfind_core::walk::{KIND_DIR, KIND_FILE, KIND_LINK, KIND_MASK, KIND_OTHER, Listing};
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
    // Zip-extraction debris (AppleDouble copies of whole trees).
    "__MACOSX",
    // OrbStack / Docker image and volume mounts.
    "OrbStack",
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
pub(crate) fn git_dir(repo: &Path) -> Option<PathBuf> {
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
fn bytes_to_path(b: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(b))
}
#[cfg(not(unix))]
fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
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

/// Children of a listing sorted by name (the order the index keeps).
fn sort_listing(l: &mut Listing) {
    let names = &l.names;
    l.ents
        .sort_by(|a, b| names[a.name_off as usize..a.name_off as usize + a.name_len as usize].cmp(&names[b.name_off as usize..b.name_off as usize + b.name_len as usize]));
}

type ByteMap<'a, V> = HashMap<&'a [u8], V, std::hash::BuildHasherDefault<flashfind_core::index::Fx>>;

/// (parent, name) of an absolute byte path; `None` for "/" or no slash.
fn split_path(p: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = memchr::memrchr(b'/', p)?;
    let name = &p[i + 1..];
    if name.is_empty() {
        return None;
    }
    Some((if i == 0 { &p[..1] } else { &p[..i] }, name))
}

fn kind_of(ft: &std::fs::FileType) -> u8 {
    if ft.is_symlink() {
        KIND_LINK
    } else if ft.is_dir() {
        KIND_DIR
    } else if ft.is_file() {
        KIND_FILE
    } else {
        KIND_OTHER
    }
}

fn lstat_attrs(p: &[u8]) -> (u64, i64) {
    std::fs::symlink_metadata(bytes_to_path(p)).map_or((0, 0), |m| (m.len(), flashfind_core::walk::mtime_of(&m)))
}

/// Children of one visited folder that the walker accepted (gitignore and
/// junk rules), collected on the thread that read the folder.
struct Run {
    dir: Vec<u8>,
    names: Vec<u8>,
    kids: Vec<(u32, u16, u8)>,
}

thread_local! {
    static RUN: std::cell::RefCell<Option<Run>> = const { std::cell::RefCell::new(None) };
}

/// A finished run: its folder's listing, children sorted by name, with
/// sizes and mtimes (macOS: one getattrlistbulk call for the folder; else
/// lstat per child).
fn finish_run(run: Run) -> (Vec<u8>, Listing) {
    let name = |k: &(u32, u16, u8)| &run.names[k.0 as usize..k.0 as usize + k.1 as usize];
    let mut kids = run.kids.clone();
    kids.sort_unstable_by(|a, b| name(a).cmp(name(b)));
    let mut bulk = if cfg!(target_os = "macos") && !kids.is_empty() { flashfind_core::walk::list_dir(bytes_to_path(&run.dir).as_ref()) } else { None };
    if let Some(b) = bulk.as_mut() {
        sort_listing(b);
    }
    let mut l = Listing::default();
    l.names.reserve(run.names.len());
    l.ents.reserve(kids.len());
    let mut path = run.dir.clone();
    for k in &kids {
        let n = name(k);
        let found = bulk.as_ref().and_then(|b| {
            let i = b.ents.binary_search_by(|r| b.name(r).cmp(n)).ok()?;
            Some((b.ents[i].size, b.ents[i].mtime))
        });
        let (size, mt) = found.unwrap_or_else(|| {
            path.truncate(run.dir.len());
            path.push(b'/');
            path.extend_from_slice(n);
            lstat_attrs(&path)
        });
        l.push(n, k.2, if k.2 == KIND_FILE { size } else { 0 }, mt);
    }
    (run.dir, l)
}

/// What the crawl produced: one listing per visited folder, the walk roots,
/// and children recorded outside their folder's run (not expected).
#[derive(Default)]
struct Crawl {
    runs: Vec<(Vec<u8>, Listing)>,
    roots: Vec<Vec<u8>>,
    strays: Vec<(Vec<u8>, u8)>,
}

/// Walk the repos with the `ignore` crate (it alone decides what is
/// included) without materializing a path per entry: its parallel walker
/// visits a folder and then, on the same thread, runs `filter_entry` on each
/// child that passed the ignore rules; those children are appended to the
/// thread's current run, which is finished into a listing when the thread's
/// next visit begins (or the thread ends).
fn crawl_listings(cfg: &Config, paths: &[PathBuf]) -> Crawl {
    let out: Arc<Mutex<Crawl>> = Arc::default();
    let Some((first, rest)) = paths.split_first() else { return Crawl::default() };
    let mut wb = ignore::WalkBuilder::new(first);
    for p in rest {
        wb.add(p);
    }
    let junk = cfg.junk.clone();
    let strays = out.clone();
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
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if junk.iter().any(|j| *j == n) && is_dir || n == ".git" || n == ".DS_Store" {
                return false;
            }
            let kind = e.file_type().map_or(KIND_OTHER, |t| kind_of(&t));
            let name = pbytes(Path::new(e.file_name()));
            let parent = e.path().parent().map(pbytes);
            let placed = RUN.with_borrow_mut(|c| match c {
                Some(run) if parent.as_deref() == Some(&run.dir[..]) && name.len() <= u16::MAX as usize => {
                    run.kids.push((run.names.len() as u32, name.len() as u16, kind));
                    run.names.extend_from_slice(&name);
                    true
                }
                _ => false,
            });
            if !placed {
                strays.lock().unwrap().strays.push((pbytes(e.path()), kind));
            }
            true
        });
    wb.build_parallel().run(|| {
        struct Sink {
            out: Arc<Mutex<Crawl>>,
            runs: Vec<(Vec<u8>, Listing)>,
            roots: Vec<Vec<u8>>,
        }
        impl Sink {
            fn close_run(&mut self) {
                if let Some(run) = RUN.with_borrow_mut(|c| c.take()) {
                    self.runs.push(finish_run(run));
                }
            }
        }
        impl Drop for Sink {
            fn drop(&mut self) {
                self.close_run();
                let mut o = self.out.lock().unwrap();
                o.runs.append(&mut self.runs);
                o.roots.append(&mut self.roots);
            }
        }
        let mut sink = Sink {
            out: out.clone(),
            runs: Vec::new(),
            roots: Vec::new(),
        };
        Box::new(move |r| {
            sink.close_run();
            let Ok(e) = r else { return ignore::WalkState::Continue };
            if e.depth() == 0 {
                sink.roots.push(pbytes(e.path()));
            }
            if e.file_type().is_some_and(|t| t.is_dir() && !t.is_symlink()) {
                RUN.set(Some(Run {
                    dir: pbytes(e.path()),
                    names: Vec::new(),
                    kids: Vec::new(),
                }));
            }
            ignore::WalkState::Continue
        })
    });
    // Every worker's sink has been dropped once `run` returns.
    std::mem::take(&mut *out.lock().unwrap())
}

/// Link the crawl's folder listings below `root` (listing 0): child folder
/// entries point at their listing; walk roots hang off a chain of ancestor
/// listings (mtime 0, as they were not walked).
fn assemble_listings(root: &Path, crawl: Crawl) -> Vec<Listing> {
    let rootb = pbytes(root);
    let under = |p: &[u8]| p.len() > rootb.len() && p.starts_with(&rootb) && (rootb.ends_with(b"/") || p[rootb.len()] == b'/');
    let Crawl { runs, roots, strays } = crawl;
    let (dirs, mut ls): (Vec<Vec<u8>>, Vec<Listing>) = std::iter::once((rootb.clone(), Listing::default())).chain(runs).unzip();
    // A run for `root` itself replaces the empty listing 0.
    let mut ids: ByteMap<u32> = ByteMap::default();
    ids.reserve(dirs.len());
    ids.insert(&dirs[0], 0);
    for (i, d) in dirs.iter().enumerate().skip(1) {
        if *d == rootb {
            ls.swap(0, i);
            ls[i] = Listing::default();
            continue;
        }
        ids.insert(d, i as u32);
    }
    for (i, l) in ls.iter_mut().enumerate() {
        l.id = i as u32;
    }
    let mut path = Vec::new();
    for i in 0..ls.len() {
        let dir = if i == 0 { &rootb } else { &dirs[i] };
        let l = &mut ls[i];
        for e in 0..l.ents.len() {
            if l.ents[e].kind & KIND_MASK != KIND_DIR {
                continue;
            }
            path.clear();
            path.extend_from_slice(dir);
            if !dir.ends_with(b"/") {
                path.push(b'/');
            }
            path.extend_from_slice(l.name(&l.ents[e]));
            if let Some(&c) = ids.get(&path[..]) {
                l.ents[e].child = c;
            }
        }
    }
    // Walk roots (and any stray entries) below ancestor chains.
    let mut chain: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut touched: Vec<u32> = Vec::new();
    fn ensure(dir: &[u8], under: &dyn Fn(&[u8]) -> bool, ls: &mut Vec<Listing>, ids: &ByteMap<u32>, chain: &mut HashMap<Vec<u8>, u32>, touched: &mut Vec<u32>) -> Option<u32> {
        if let Some(&i) = ids.get(dir).or_else(|| chain.get(dir)) {
            return Some(i);
        }
        if !under(dir) {
            return None;
        }
        let (parent, name) = split_path(dir)?;
        let pid = ensure(parent, under, ls, ids, chain, touched)?;
        let id = ls.len() as u32;
        ls.push(Listing { id, ..Default::default() });
        let i = ls[pid as usize].push(name, KIND_DIR, 0, 0)?;
        ls[pid as usize].ents[i].child = id;
        touched.push(pid);
        chain.insert(dir.to_vec(), id);
        Some(id)
    }
    let add = |p: &[u8], kind: u8, ls: &mut Vec<Listing>, chain: &mut HashMap<Vec<u8>, u32>, touched: &mut Vec<u32>| {
        let Some((parent, name)) = split_path(p) else { return };
        let Some(pid) = ensure(parent, &under, ls, &ids, chain, touched) else { return };
        let child = if kind == KIND_DIR { ids.get(p).copied() } else { None };
        let l = &mut ls[pid as usize];
        if let Some(e) = l.ents.iter().position(|r| l.name(r) == name) {
            if let Some(c) = child {
                l.ents[e].child = c;
            }
            return;
        }
        let (size, mt) = lstat_attrs(p);
        if let Some(i) = l.push(name, kind, if kind == KIND_FILE { size } else { 0 }, mt) {
            if let Some(c) = child {
                l.ents[i].child = c;
            }
            touched.push(pid);
        }
    };
    for r in &roots {
        if *r != rootb {
            add(r, KIND_DIR, &mut ls, &mut chain, &mut touched);
        }
    }
    for (p, kind) in &strays {
        add(p, *kind, &mut ls, &mut chain, &mut touched);
    }
    touched.sort_unstable();
    touched.dedup();
    for i in touched {
        sort_listing(&mut ls[i as usize]);
    }
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
    /// FSEvents id taken just before the crawl: replay from here.
    #[serde(default)]
    event_id: u64,
    #[serde(default)]
    root: Option<PathBuf>,
}

pub struct CodeIndex {
    pub idx: Arc<Index>,
    /// Folder the index is rooted at (common ancestor of the repos).
    pub root: PathBuf,
    /// FSEvents id taken just before this index was crawled.
    pub event_id: u64,
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
    /// Index into the owning [`CodeIndex::repos`] (not meaningful after a
    /// live merge; use `repo_root`).
    pub repo: usize,
    pub repo_name: String,
    pub repo_root: String,
    pub repo_active: i64,
    pub path: String,
    /// Path relative to the repo root.
    pub rel: String,
    pub score: i32,
    pub kind: &'static str,
    pub size: u64,
    pub mtime: i64,
    /// Why personal ranking moved it (e.g. "picked before"); empty if not.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
}

#[derive(Clone, Debug, Default)]
pub struct CodeResult {
    pub hits: Vec<CodeHit>,
    pub total: u64,
    pub elapsed_us: u64,
    /// Hits grouped by repo, in order of each repo's best hit.
    pub groups: Vec<Group>,
}

#[derive(Clone, Debug, Default)]
pub struct Group {
    pub repo: String,
    pub root: String,
    pub active: i64,
    /// Indices into `hits`.
    pub hits: Vec<usize>,
}

/// A query with code filters split out.
#[derive(Clone, Debug, Default)]
pub struct CodeQuery {
    pub text: String,
    pub repos: Vec<String>,
    pub langs: Vec<String>,
    pub dirty: bool,
    pub limit: usize,
    /// `file.rs:12[:3]` at the end of the query: jump target.
    pub line: Option<u32>,
    pub col: Option<u32>,
}

const FILTER_KEYS: &[&str] = &["ext", "type", "size", "mtime", "in", "limit", "repo", "project", "lang", "dirty"];

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
        if let Some(last) = rest.last_mut() {
            let (base, line, col) = memory::strip_line_suffix(last);
            if line.is_some() && !FILTER_KEYS.contains(&base.to_ascii_lowercase().as_str()) && !base.contains(':') {
                *last = base;
                q.line = line;
                q.col = col;
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
    pub fn build_repos(cfg: &Config, repos: Vec<Repo>, discover_ms: f64) -> CodeIndex {
        let paths: Vec<PathBuf> = repos.iter().map(|r| r.path.clone()).collect();
        let root = if paths.len() == 1 {
            paths[0].parent().unwrap_or(&paths[0]).to_path_buf()
        } else {
            common_root(&paths)
        };
        CodeIndex::build_repos_at(cfg, repos, &root, discover_ms)
    }

    /// Index `repos` under a given `root` (an overlay for one repo uses the
    /// base index's root so paths, depths and priors line up).
    pub fn build_repos_at(cfg: &Config, mut repos: Vec<Repo>, root: &Path, discover_ms: f64) -> CodeIndex {
        let event_id = flashfind_core::watch::current_event_id();
        let t1 = Instant::now();
        let paths: Vec<PathBuf> = repos.iter().map(|r| r.path.clone()).collect();
        let root = root.to_path_buf();
        let timing = std::env::var_os("FLASHFIND_TIMING").is_some();
        let fp = |what: &str| {
            if timing {
                eprintln!("  footprint {what}: {:.1} MB", flashfind_core::walk::footprint_mb());
            }
        };
        let crawl = crawl_listings(cfg, &paths);
        fp("after walk");
        let crawl_ms = t1.elapsed().as_secs_f64() * 1e3;
        let t2 = Instant::now();
        let ls = assemble_listings(&root, crawl);
        fp("after assemble");
        let t_ls = t2.elapsed();
        let refs: Vec<&Listing> = ls.iter().collect();
        let idx = Index::build_refs(&refs, &root, now(), 0);
        if timing {
            eprintln!("assemble {:?}, index build {:?}", t_ls, t2.elapsed() - t_ls);
        }
        drop(refs);
        drop(ls);
        fp("after index build");
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
        let ci = CodeIndex::assemble(Arc::new(idx), repos, cfg.roots.clone(), info, root, event_id);
        flashfind_core::release_memory();
        ci
    }

    /// Discover + index, remembering the FSEvents id from before discovery.
    pub fn build_tracked(cfg: &Config) -> CodeIndex {
        let ev = flashfind_core::watch::current_event_id();
        let mut ci = CodeIndex::build(cfg);
        ci.event_id = ev;
        ci
    }

    fn assemble(idx: Arc<Index>, mut repos: Vec<Repo>, roots: Vec<PathBuf>, info: BuildInfo, root: PathBuf, event_id: u64) -> CodeIndex {
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
            root,
            event_id,
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
            event_id: self.event_id,
            root: Some(self.root.clone()),
        };
        let tmp = dir.join("code.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&meta).unwrap())?;
        std::fs::rename(tmp, dir.join("code.json"))
    }

    pub fn load(dir: &Path) -> std::io::Result<CodeIndex> {
        let idx = Index::load(&dir.join("code.ffx"))?;
        let meta: Meta = serde_json::from_slice(&std::fs::read(dir.join("code.json"))?).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let root = meta.root.unwrap_or_else(|| PathBuf::from(String::from_utf8_lossy(idx.root()).into_owned()));
        Ok(CodeIndex::assemble(Arc::new(idx), meta.repos, meta.roots, meta.info, root, meta.event_id))
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

    /// Project names closest to `want` (edit distance), for error messages.
    pub fn similar_repos(&self, want: &str, k: usize) -> Vec<String> {
        fn lev(a: &[u8], b: &[u8]) -> usize {
            let mut prev: Vec<usize> = (0..=b.len()).collect();
            for (i, &x) in a.iter().enumerate() {
                let mut cur = vec![i + 1; b.len() + 1];
                for (j, &y) in b.iter().enumerate() {
                    cur[j + 1] = (prev[j] + (x != y) as usize).min(prev[j + 1] + 1).min(cur[j] + 1);
                }
                prev = cur;
            }
            prev[b.len()]
        }
        let w = want.to_ascii_lowercase();
        let mut v: Vec<(usize, &str)> = self
            .repos
            .iter()
            .map(|r| (lev(w.as_bytes(), r.name.to_ascii_lowercase().as_bytes()), r.name.as_str()))
            .filter(|&(d, _)| d <= (w.len() / 2).max(2))
            .collect();
        v.sort();
        v.dedup_by(|a, b| a.1 == b.1);
        v.into_iter().take(k).map(|(_, n)| n.to_string()).collect()
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

    /// Subtree range of repo `i` in the index.
    pub fn range(&self, i: usize) -> (u32, u32) {
        self.repos[i].range
    }

    pub fn mask(&self, cq: &CodeQuery, extra: Option<&[u64]>) -> Result<Vec<u64>, String> {
        let mut mask = self.outside.clone();
        if let Some(x) = extra {
            for (m, e) in mask.iter_mut().zip(x) {
                *m |= e;
            }
        }
        let mut selected: Option<Vec<usize>> = None;
        if !cq.repos.is_empty() {
            let mut v = Vec::new();
            for r in &cq.repos {
                let m = self.match_repos(r);
                if m.is_empty() {
                    let near = self.similar_repos(r, 5);
                    return Err(if near.is_empty() {
                        format!("no project matches repo:{r}")
                    } else {
                        format!("no project matches repo:{r} (closest: {})", near.join(", "))
                    });
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
        let r = &self.repos[repo];
        let root = r.path.to_string_lossy();
        let rel = path.strip_prefix(root.as_ref()).unwrap_or(&path).trim_start_matches('/').to_string();
        Some(CodeHit {
            repo,
            repo_name: r.name.clone(),
            repo_root: root.clone().into_owned(),
            repo_active: r.active,
            rel: if rel.is_empty() { ".".into() } else { rel },
            path,
            score,
            kind: kind_name(self.idx.ent_kind()[e as usize]),
            size: self.idx.size(e),
            mtime: self.idx.mtime(e),
            why: String::new(),
        })
    }

    /// Search with code filters, recency boost and repo grouping.
    pub fn search(&self, sess: &mut Session, cq: &CodeQuery, cancel: &CancelToken) -> Result<CodeResult, String> {
        self.search_with(sess, cq, cancel, None)
    }

    /// [`CodeIndex::search`] with extra excluded entries (live tombstones).
    pub fn search_with(&self, sess: &mut Session, cq: &CodeQuery, cancel: &CancelToken, extra: Option<&[u64]>) -> Result<CodeResult, String> {
        self.search_ranked(sess, cq, cancel, extra, &RankInput::legacy())
    }

    /// Search with personal ranking (see [`rank`]).
    pub fn search_ranked(&self, sess: &mut Session, cq: &CodeQuery, cancel: &CancelToken, extra: Option<&[u64]>, rk: &RankInput) -> Result<CodeResult, String> {
        // Only filters (e.g. `repo:x`, `dirty:`): list matching files, newest first.
        if cq.text.trim().is_empty() {
            return self.recent_with(cq, None, extra);
        }
        let t0 = Instant::now();
        let mut q = Query::parse(&cq.text)?;
        for l in &cq.langs {
            let exts = lang_exts(l).ok_or_else(|| format!("unknown lang:{l}"))?;
            q.exts.extend(exts.iter().map(|e| e.as_bytes().to_vec()));
        }
        let limit = cq.limit.max(1);
        // Fetch extra so personal boosts can reorder.
        q.limit = (limit * 3).max(30);
        let mask = self.mask(cq, extra)?;
        let r = sess.search_masked(&q, cancel, Some(&mask)).map_err(|e| e.to_string())?;
        let mut raw: Vec<(u32, i32)> = r.hits.iter().map(|h| (h.entry, h.score)).collect();
        // Remembered files the matcher would not fetch: score just those.
        let cands = rk.candidates();
        if !cands.is_empty() {
            let mut allow = vec![!0u64; mask.len()];
            let mut n = 0;
            for p in &cands {
                if let Some(e) = self.idx.lookup(p.as_bytes()) {
                    allow[e as usize >> 6] &= !(1 << (e & 63));
                    n += 1;
                }
            }
            if n > 0 {
                for (a, m) in allow.iter_mut().zip(&mask) {
                    *a |= m;
                }
                let mut q2 = q.clone();
                q2.limit = n;
                let r2 = sess.search_masked(&q2, cancel, Some(&allow)).map_err(|e| e.to_string())?;
                raw.extend(r2.hits.iter().map(|h| (h.entry, h.score)));
            }
        }
        // The current project's own best matches, even if other repos crowd the top.
        if let Some(root) = &rk.ctx_root
            && let Some(i) = self.repos.iter().position(|r| r.path.as_os_str() == root.as_str())
        {
            let (a, b) = self.repos[i].range;
            let mut m = vec![!0u64; mask.len()];
            for e in a..b {
                m[e as usize >> 6] &= !(1 << (e & 63));
            }
            for (x, y) in m.iter_mut().zip(&mask) {
                *x |= y;
            }
            let mut q3 = q.clone();
            q3.limit = limit.max(20);
            let r3 = sess.search_masked(&q3, cancel, Some(&m)).map_err(|e| e.to_string())?;
            raw.extend(r3.hits.iter().map(|h| (h.entry, h.score)));
        }
        raw.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        raw.dedup_by_key(|x| x.0);
        let t = now();
        let mut hits: Vec<CodeHit> = raw
            .iter()
            .filter_map(|&(e, s)| self.hit(e, s))
            .map(|mut h| {
                if rk.features.legacy_repo_recency {
                    h.score += recency_boost(t - self.repos[h.repo].active);
                }
                let (pct, why) = rk.boost(&h);
                h.score = rank::apply(h.score, pct);
                h.why = why.join(", ");
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
        self.recent_with(cq, within, None)
    }

    pub fn recent_with(&self, cq: &CodeQuery, within: Option<i64>, extra: Option<&[u64]>) -> Result<CodeResult, String> {
        let t0 = Instant::now();
        let mask = self.mask(cq, extra)?;
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
        // Score = seconds since 2001 (fits i32 until 2069), so merged lists stay newest first.
        let hits: Vec<CodeHit> = cand
            .iter()
            .filter_map(|&(m, e)| self.hit(e, (m - 978_307_200).clamp(0, i32::MAX as i64) as i32))
            .collect();
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
        result_json(r)
    }

    pub fn projects_json(&self, filter: Option<&str>, limit: usize) -> serde_json::Value {
        projects_json(&self.repos, filter, limit)
    }
}

/// Compact, agent-friendly JSON: results grouped by repo.
pub fn result_json(r: &CodeResult) -> serde_json::Value {
    let groups: Vec<serde_json::Value> = r
        .groups
        .iter()
        .map(|g| serde_json::json!({"repo": g.repo, "root": g.root, "files": g.hits.iter().map(|&i| &r.hits[i].rel).collect::<Vec<_>>()}))
        .collect();
    serde_json::json!({"total": r.total, "shown": r.hits.len(), "us": r.elapsed_us, "groups": groups})
}

pub fn projects_json(repos: &[Repo], filter: Option<&str>, limit: usize) -> serde_json::Value {
    let mut v: Vec<&Repo> = repos
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

/// Group hits by repo, in order of each repo's best hit.
pub fn group(hits: &[CodeHit]) -> Vec<Group> {
    let mut order: Vec<Group> = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        match order.iter_mut().find(|g| g.root == h.repo_root) {
            Some(g) => g.hits.push(i),
            None => order.push(Group {
                repo: h.repo_name.clone(),
                root: h.repo_root.clone(),
                active: h.repo_active,
                hits: vec![i],
            }),
        }
    }
    order
}
