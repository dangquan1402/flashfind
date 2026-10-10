//! Cheap, cached git signals per repo: files with uncommitted changes and
//! files touched by the last few commits (fff's git-status / git-recency
//! boosts, `crates/fff-core/src/git_recency.rs`).
//!
//! One `git status` + one `git log` per repo, cached for [`TTL`]. The
//! current project's repo is refreshed synchronously (tens of ms once);
//! other repos that show up in results refresh on a background worker and
//! get their boost from the next keystroke on.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const TTL: Duration = Duration::from_secs(15);
/// Commits looked at for recency.
pub const RECENT_COMMITS: usize = 10;
/// Commits touching more files than this (renames, formatting) are skipped.
const BULK_COMMIT: usize = 40;

#[derive(Clone, Debug, Default)]
pub struct GitInfo {
    /// Absolute paths with uncommitted changes (tracked or untracked).
    pub dirty: HashSet<String>,
    /// Untracked folders (absolute, trailing '/'): everything under them is dirty.
    pub dirty_dirs: Vec<String>,
    /// Absolute path → index of the newest recent commit touching it (0 = HEAD).
    pub recent: HashMap<String, usize>,
}

impl GitInfo {
    pub fn is_dirty(&self, p: &str) -> bool {
        self.dirty.contains(p) || self.dirty_dirs.iter().any(|d| p.starts_with(d.as_str()))
    }

    /// Read from git (empty when git is missing or this is not a work tree).
    pub fn load(repo: &Path) -> GitInfo {
        let mut g = GitInfo::default();
        let root = repo.to_string_lossy().into_owned();
        let abs = |rel: &str| format!("{}/{}", root.trim_end_matches('/'), rel);
        if let Ok(o) = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])
            .stderr(std::process::Stdio::null())
            .output()
        {
            let mut it = o.stdout.split(|&b| b == 0).filter(|s| s.len() > 3);
            while let Some(rec) = it.next() {
                let (xy, p) = rec.split_at(3);
                if xy[0] == b'R' || xy[0] == b'C' {
                    it.next();
                }
                let p = String::from_utf8_lossy(p);
                if p.ends_with('/') {
                    g.dirty_dirs.push(abs(&p));
                } else {
                    g.dirty.insert(abs(&p));
                }
            }
        }
        if let Ok(o) = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["log", "--no-merges", "--name-only", "--format=%x01", "-n", &RECENT_COMMITS.to_string(), "HEAD"])
            .stderr(std::process::Stdio::null())
            .output()
        {
            let s = String::from_utf8_lossy(&o.stdout);
            for (i, c) in s.split('\u{1}').filter(|c| !c.trim().is_empty()).enumerate() {
                let files: Vec<&str> = c.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                if files.len() > BULK_COMMIT {
                    continue;
                }
                for f in files {
                    g.recent.entry(abs(f)).or_insert(i);
                }
            }
        }
        g
    }
}

struct Entry {
    at: Instant,
    info: Arc<GitInfo>,
    pinned: bool,
}

#[derive(Default)]
struct Inner {
    map: HashMap<PathBuf, Entry>,
    queue: VecDeque<PathBuf>,
    queued: HashSet<PathBuf>,
    workers: usize,
}

/// Shared per-process cache.
#[derive(Default)]
pub struct GitCache {
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl GitCache {
    pub fn new() -> Arc<GitCache> {
        Arc::new(GitCache::default())
    }

    /// Fixed info for `repo` (tests and the replay eval).
    pub fn pin(&self, repo: &Path, info: GitInfo) {
        self.inner.lock().unwrap().map.insert(
            repo.to_path_buf(),
            Entry {
                at: Instant::now(),
                info: Arc::new(info),
                pinned: true,
            },
        );
    }

    /// Cached info; with `block`, (re)loads a stale entry now; otherwise
    /// schedules a background refresh and returns what is cached (maybe
    /// stale, maybe nothing).
    pub fn get(self: &Arc<Self>, repo: &Path, block: bool) -> Option<Arc<GitInfo>> {
        {
            let mut g = self.inner.lock().unwrap();
            let cached = g.map.get(repo).map(|e| (e.pinned || e.at.elapsed() < TTL, e.info.clone()));
            match cached {
                Some((true, i)) => return Some(i),
                Some((false, i)) if !block => {
                    self.enqueue(&mut g, repo);
                    return Some(i);
                }
                None if !block => {
                    self.enqueue(&mut g, repo);
                    return None;
                }
                _ => {}
            }
        }
        let info = Arc::new(GitInfo::load(repo));
        self.inner.lock().unwrap().map.insert(
            repo.to_path_buf(),
            Entry {
                at: Instant::now(),
                info: info.clone(),
                pinned: false,
            },
        );
        Some(info)
    }

    fn enqueue(self: &Arc<Self>, g: &mut Inner, repo: &Path) {
        if !g.queued.insert(repo.to_path_buf()) {
            return;
        }
        g.queue.push_back(repo.to_path_buf());
        if g.workers < 2 {
            g.workers += 1;
            let me = Arc::downgrade(self);
            std::thread::spawn(move || {
                loop {
                    let Some(c) = me.upgrade() else { return };
                    let next = {
                        let mut g = c.inner.lock().unwrap();
                        loop {
                            if let Some(r) = g.queue.pop_front() {
                                break Some(r);
                            }
                            let (ng, to) = c.cv.wait_timeout(g, Duration::from_secs(30)).unwrap();
                            g = ng;
                            if to.timed_out() && g.queue.is_empty() {
                                g.workers -= 1;
                                break None;
                            }
                        }
                    };
                    let Some(r) = next else { return };
                    let info = Arc::new(GitInfo::load(&r));
                    let mut g = c.inner.lock().unwrap();
                    g.queued.remove(&r);
                    if !g.map.get(&r).is_some_and(|e| e.pinned) {
                        g.map.insert(
                            r,
                            Entry {
                                at: Instant::now(),
                                info,
                                pinned: false,
                            },
                        );
                    }
                }
            });
        } else {
            self.cv.notify_one();
        }
    }
}
