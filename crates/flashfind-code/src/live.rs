//! Live code index: the cached [`CodeIndex`] as a base, plus one small
//! overlay index per repo that changed since, kept current from FSEvents
//! (macOS) or periodic rebuilds (elsewhere).
//!
//! * A change inside a repo re-walks that repo with gitignore semantics into
//!   an overlay (built under the base root, so paths and scores line up) and
//!   tombstones the repo's base range. Changes inside junk folders
//!   (`node_modules`, `target`, …) are ignored; `.git` internals only matter
//!   if `.git` disappears. Editing `.gitignore` re-walks the repo, so new
//!   rules apply at once.
//! * A change outside every repo looks for new repos in that folder and its
//!   children, and drops known repos under it that are gone.
//! * Lost event history (`MustScanSubDirs`, dropped events) inside a repo
//!   re-walks it; outside repos it triggers a full rebuild.
//! * Compaction = a background full rebuild that swaps in a new base and
//!   re-applies the events that arrived meanwhile. It runs when overlays
//!   grow past a threshold or on a timer.
//! * The FSEvents id from before each crawl is saved with the cache, so a
//!   restart (or a one-shot CLI run, see [`CodeLive::replay`]) replays only
//!   newer events.

use crate::{CodeHit, CodeIndex, CodeQuery, CodeResult, Config, DISCOVERY_SKIP, Repo, git_dir, group, repo_info};
use flashfind_core::watch::{self, HISTORY_DONE, LOSSY};
use flashfind_core::{CancelToken, Session};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

pub struct Snap {
    pub base: Arc<CodeIndex>,
    /// Base repos replaced by an overlay or removed.
    pub dead: Vec<bool>,
    dead_mask: Option<Vec<u64>>,
    /// One repo each.
    pub overlays: Vec<Arc<CodeIndex>>,
    pub generation: u64,
}

impl Snap {
    fn new(base: Arc<CodeIndex>, generation: u64) -> Snap {
        let n = base.repos.len();
        Snap {
            base,
            dead: vec![false; n],
            dead_mask: None,
            overlays: Vec::new(),
            generation,
        }
    }

    /// Live repos: base minus dead, plus overlays.
    pub fn repos(&self) -> Vec<Repo> {
        let mut v: Vec<Repo> = self.base.repos.iter().zip(&self.dead).filter(|(_, d)| !**d).map(|(r, _)| r.clone()).collect();
        v.extend(self.overlays.iter().flat_map(|o| o.repos.iter().cloned()));
        v
    }

    pub fn entries(&self) -> usize {
        let dead: usize = (0..self.base.repos.len())
            .filter(|&i| self.dead[i])
            .map(|i| (self.base.range(i).1 - self.base.range(i).0) as usize)
            .sum();
        let over: usize = self.overlays.iter().map(|o| o.repos.iter().map(|r| r.files as usize).sum::<usize>()).sum();
        self.base.repos.iter().map(|r| r.files as usize).sum::<usize>() - dead + over
    }

    /// Repo containing `p` (deepest root that is a prefix).
    fn locate(&self, p: &Path) -> Option<PathBuf> {
        let alive = self.base.repos.iter().zip(&self.dead).filter(|(_, d)| !**d).map(|(r, _)| &r.path);
        let over = self.overlays.iter().flat_map(|o| o.repos.iter().map(|r| &r.path));
        alive.chain(over).filter(|r| p.starts_with(r)).max_by_key(|r| r.as_os_str().len()).cloned()
    }

    fn known(&self, p: &Path) -> bool {
        self.base.repos.iter().zip(&self.dead).any(|(r, d)| !*d && r.path == p) || self.overlays.iter().any(|o| o.repos.iter().any(|r| r.path == p))
    }
}

#[derive(Clone, Debug, Default)]
pub struct LiveStats {
    pub repos: usize,
    pub entries: usize,
    pub overlays: usize,
    pub removed: usize,
    pub events: u64,
    pub rewalks: u64,
    pub repos_added: u64,
    pub repos_removed: u64,
    pub compactions: u64,
    pub last_apply_ms: f64,
    pub last_event_id: u64,
    pub base_event_id: u64,
    pub watching: bool,
    pub generation: u64,
}

pub struct CodeLive {
    cfg: Config,
    cache: Option<PathBuf>,
    snap: RwLock<Arc<Snap>>,
    update: Mutex<()>,
    /// Events seen while a compaction runs (re-applied to the new base).
    pending: Mutex<Option<Vec<(PathBuf, u32)>>>,
    stats: Mutex<LiveStats>,
    generation: AtomicU64,
    compacting: AtomicBool,
    last_compact: Mutex<Instant>,
    stop: Arc<AtomicBool>,
    me: Weak<CodeLive>,
    built_here: bool,
    #[cfg(target_os = "macos")]
    streams: Mutex<Vec<watch::Stream>>,
    /// Compact when this many repos are overlaid.
    pub compact_overlays: usize,
    /// Compact a changed index at least this often.
    pub compact_interval: Duration,
    /// Rebuild period where FSEvents is unavailable.
    pub poll: Duration,
    /// FSEvents coalescing latency.
    pub latency: Duration,
}

fn hidden_or_skipped(name: &str, cfg: &Config) -> bool {
    name.starts_with('.')
        || name.ends_with(".noindex")
        || name.ends_with(".app")
        || name.ends_with(".photoslibrary")
        || DISCOVERY_SKIP.contains(&name)
        || cfg.skip.iter().any(|s| s == name)
        || cfg.junk.iter().any(|s| s == name)
}

impl CodeLive {
    fn with(cfg: Config, cache: Option<PathBuf>, base: CodeIndex, built_here: bool) -> Arc<CodeLive> {
        let ev = base.event_id;
        Arc::new_cyclic(|me| CodeLive {
            me: me.clone(),
            built_here,
            cfg,
            cache,
            snap: RwLock::new(Arc::new(Snap::new(Arc::new(base), 0))),
            update: Mutex::new(()),
            pending: Mutex::new(None),
            stats: Mutex::new(LiveStats {
                base_event_id: ev,
                last_event_id: ev,
                ..Default::default()
            }),
            generation: AtomicU64::new(0),
            compacting: AtomicBool::new(false),
            last_compact: Mutex::new(Instant::now()),
            stop: Arc::new(AtomicBool::new(false)),
            #[cfg(target_os = "macos")]
            streams: Mutex::new(Vec::new()),
            compact_overlays: 64,
            compact_interval: Duration::from_secs(1800),
            poll: Duration::from_secs(300),
            latency: Duration::from_millis(30),
        })
    }

    pub fn from_index(cfg: Config, base: CodeIndex) -> Arc<CodeLive> {
        CodeLive::with(cfg, None, base, true)
    }

    /// Load the cached index for `cfg.roots` (or build and cache it).
    pub fn open(cfg: Config, cache: Option<PathBuf>) -> Arc<CodeLive> {
        let loaded = cache.as_ref().and_then(|d| CodeIndex::load(d).ok()).filter(|c| c.roots == cfg.roots);
        let built_here = loaded.is_none();
        let base = loaded.unwrap_or_else(|| {
            let c = CodeIndex::build_tracked(&cfg);
            if let Some(d) = &cache {
                let _ = c.save(d);
            }
            c
        });
        CodeLive::with(cfg, cache, base, built_here)
    }

    /// True when [`CodeLive::open`] had no usable cache and crawled just now
    /// (so there is nothing to catch up on).
    pub fn just_built(&self) -> bool {
        self.built_here
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn snapshot(&self) -> Arc<Snap> {
        self.snap.read().unwrap().clone()
    }

    pub fn stats(&self) -> LiveStats {
        let s = self.snapshot();
        let mut st = self.stats.lock().unwrap().clone();
        st.repos = s.repos().len();
        st.entries = s.entries();
        st.overlays = s.overlays.len();
        st.removed = s.dead.iter().filter(|d| **d).count();
        st.generation = s.generation;
        st
    }

    pub fn repos(&self) -> Vec<Repo> {
        self.snapshot().repos()
    }

    /// Apply directory-level change notifications. Returns the number of
    /// repos re-walked, added or removed.
    pub fn apply(&self, events: &[(PathBuf, u32)]) -> usize {
        if events.is_empty() {
            return 0;
        }
        let t0 = Instant::now();
        let _g = self.update.lock().unwrap();
        if let Some(p) = self.pending.lock().unwrap().as_mut() {
            p.extend_from_slice(events);
        }
        let snap = self.snapshot();
        let (mut rewalk, mut remove) = (BTreeSet::<PathBuf>::new(), BTreeSet::<PathBuf>::new());
        let mut full = false;
        for (p, flags) in events {
            if flags & HISTORY_DONE != 0 && p.as_os_str().is_empty() {
                continue;
            }
            let lossy = flags & LOSSY != 0;
            let Some(root) = self.cfg.roots.iter().find(|r| p.starts_with(r)) else {
                // Above the roots (e.g. a volume-level event).
                full |= lossy && self.cfg.roots.iter().any(|r| r.starts_with(p));
                continue;
            };
            if let Some(repo) = snap.locate(p) {
                let rel = p.strip_prefix(&repo).unwrap_or(Path::new(""));
                let comps: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                if !repo.exists() || git_dir(&repo).is_none() {
                    remove.insert(repo);
                } else if comps.first().map(String::as_str) == Some(".git") {
                    // Commits, index updates: no files changed in the work tree.
                } else if !lossy && comps.iter().any(|c| self.cfg.junk.iter().any(|j| j == c)) {
                    // node_modules churn and the like.
                } else {
                    rewalk.insert(repo);
                }
                continue;
            }
            // Outside every repo.
            let rel = p.strip_prefix(root).unwrap_or(Path::new(""));
            let comps: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            if comps.last().map(String::as_str) == Some(".git") {
                // `git init` / clone finished in the parent.
                if let Some(parent) = p.parent()
                    && !comps[..comps.len() - 1].iter().any(|c| hidden_or_skipped(c, &self.cfg))
                    && git_dir(parent).is_some()
                    && !snap.known(parent)
                {
                    rewalk.insert(parent.to_path_buf());
                }
                continue;
            }
            if comps.iter().any(|c| hidden_or_skipped(c, &self.cfg)) {
                continue;
            }
            if lossy {
                full = true;
                continue;
            }
            for r in snap.repos() {
                if r.path.starts_with(p) && (!r.path.exists() || git_dir(&r.path).is_none()) {
                    remove.insert(r.path.clone());
                }
            }
            if git_dir(p).is_some() {
                if !snap.known(p) {
                    rewalk.insert(p.clone());
                }
            } else if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().is_ok_and(|t| t.is_dir()) && !hidden_or_skipped(&name, &self.cfg) {
                        let c = e.path();
                        if git_dir(&c).is_some() && !snap.known(&c) {
                            rewalk.insert(c);
                        }
                    }
                }
            }
        }
        for r in &remove {
            rewalk.remove(r);
        }
        let changed = rewalk.len() + remove.len();
        if changed > 0 {
            self.swap_in(&snap, &rewalk, &remove);
        }
        drop(_g);
        {
            let mut st = self.stats.lock().unwrap();
            st.events += events.len() as u64;
            if changed > 0 {
                st.last_apply_ms = t0.elapsed().as_secs_f64() * 1e3;
            }
        }
        let snap = self.snapshot();
        let stale = !snap.overlays.is_empty() && self.last_compact.lock().unwrap().elapsed() > self.compact_interval;
        if full || snap.overlays.len() > self.compact_overlays || stale {
            self.request_compact();
        }
        changed
    }

    fn swap_in(&self, snap: &Snap, rewalk: &BTreeSet<PathBuf>, remove: &BTreeSet<PathBuf>) {
        let mut dead = snap.dead.clone();
        let mut overlays = snap.overlays.clone();
        let (mut added, mut walked) = (0u64, 0u64);
        let base_idx = |p: &Path| snap.base.repos.iter().position(|r| r.path == p);
        for r in remove {
            if let Some(i) = base_idx(r) {
                dead[i] = true;
            }
            overlays.retain(|o| o.repos.iter().all(|x| &x.path != r));
        }
        // Re-walk changed repos in parallel: one busy batch (a branch switch
        // in a big repo, agents editing several worktrees) must not queue
        // every other change behind it.
        use rayon::prelude::*;
        let built: Vec<(&PathBuf, Arc<CodeIndex>)> = rewalk
            .par_iter()
            .filter_map(|r| {
                let info = repo_info(r)?;
                let mut root = snap.base.root.clone();
                while !r.starts_with(&root) && root.pop() {}
                Some((r, Arc::new(CodeIndex::build_repos_at(&self.cfg, vec![info], &root, 0.0))))
            })
            .collect();
        for (r, ci) in built {
            walked += 1;
            let was_known = snap.known(r);
            if let Some(i) = base_idx(r) {
                dead[i] = true;
            }
            overlays.retain(|o| o.repos.iter().all(|x| &x.path != r));
            if !ci.repos.is_empty() {
                overlays.push(ci);
            }
            added += (!was_known) as u64;
        }
        let dead_mask = if dead.iter().any(|d| *d) {
            let mut m = vec![0u64; snap.base.idx.n.div_ceil(64)];
            for (i, _) in dead.iter().enumerate().filter(|(_, d)| **d) {
                let (a, b) = snap.base.range(i);
                for e in a..b {
                    m[e as usize >> 6] |= 1 << (e & 63);
                }
            }
            Some(m)
        } else {
            None
        };
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *self.snap.write().unwrap() = Arc::new(Snap {
            base: snap.base.clone(),
            dead,
            dead_mask,
            overlays,
            generation,
        });
        let mut st = self.stats.lock().unwrap();
        st.rewalks += walked;
        st.repos_added += added;
        st.repos_removed += remove.len() as u64;
    }

    /// Rebuild the base now (discovery + crawl), swap it in, save it, and
    /// re-apply events that arrived meanwhile.
    pub fn compact(&self) {
        *self.pending.lock().unwrap() = Some(Vec::new());
        let ci = CodeIndex::build_tracked(&self.cfg);
        if let Some(d) = &self.cache {
            let _ = ci.save(d);
        }
        let ev = ci.event_id;
        let replay = {
            let _g = self.update.lock().unwrap();
            let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
            *self.snap.write().unwrap() = Arc::new(Snap::new(Arc::new(ci), generation));
            self.pending.lock().unwrap().take().unwrap_or_default()
        };
        {
            let mut st = self.stats.lock().unwrap();
            st.compactions += 1;
            st.base_event_id = ev;
        }
        *self.last_compact.lock().unwrap() = Instant::now();
        self.apply(&replay);
    }

    fn request_compact(&self) {
        if self.compacting.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(keep) = self.me.upgrade() else { return };
        std::thread::spawn(move || {
            keep.compact();
            keep.compacting.store(false, Ordering::Release);
        });
    }

    /// Apply a batch of raw watcher events (also used by tests).
    pub fn on_batch(&self, batch: &[watch::Event]) {
        let mut st_id = 0;
        let ev: Vec<(PathBuf, u32)> = batch
            .iter()
            .map(|e| {
                st_id = st_id.max(e.id);
                (bytes_path(&e.path), e.flags)
            })
            .collect();
        self.apply(&ev);
        let mut st = self.stats.lock().unwrap();
        st.last_event_id = st.last_event_id.max(st_id);
    }

    /// Keep the index current: FSEvents on macOS (replaying from the saved
    /// event id), a periodic rebuild every `poll` elsewhere.
    pub fn watch(self: &Arc<Self>) -> std::io::Result<()> {
        self.stop.store(false, Ordering::Release);
        let weak: Weak<CodeLive> = Arc::downgrade(self);
        let stop = self.stop.clone();
        #[cfg(target_os = "macos")]
        {
            let mut streams = self.streams.lock().unwrap();
            if !streams.is_empty() {
                return Ok(());
            }
            let since = self.snapshot().base.event_id;
            let (tx, rx) = std::sync::mpsc::channel::<Vec<watch::Event>>();
            for r in &self.cfg.roots {
                let b = r.to_string_lossy().into_owned().into_bytes();
                match watch::watch(&b, since, self.latency.as_secs_f64(), tx.clone()) {
                    Some(s) => streams.push(s),
                    None => return Err(std::io::Error::other(format!("FSEvents stream failed for {}", r.display()))),
                }
            }
            drop(tx);
            std::thread::spawn(move || {
                while let Ok(mut batch) = rx.recv() {
                    while let Ok(more) = rx.try_recv() {
                        batch.extend(more);
                    }
                    let Some(live) = weak.upgrade() else { break };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    live.on_batch(&batch);
                }
            });
        }
        #[cfg(not(target_os = "macos"))]
        {
            let poll = self.poll;
            std::thread::spawn(move || {
                loop {
                    let t = Instant::now();
                    while t.elapsed() < poll {
                        std::thread::sleep(Duration::from_millis(200));
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                    }
                    let Some(live) = weak.upgrade() else { return };
                    live.compact();
                }
            });
        }
        self.stats.lock().unwrap().watching = true;
        Ok(())
    }

    pub fn unwatch(&self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(target_os = "macos")]
        self.streams.lock().unwrap().clear();
        self.stats.lock().unwrap().watching = false;
    }

    /// One-shot catch-up for short-lived processes (the CLI): replay the
    /// FSEvents history since the cached index was built, then stop.
    /// Returns the number of events seen (0 where FSEvents is unavailable).
    pub fn replay(&self, timeout: Duration) -> usize {
        #[cfg(target_os = "macos")]
        {
            let since = self.snapshot().base.event_id;
            if since == 0 {
                return 0;
            }
            let (tx, rx) = std::sync::mpsc::channel::<Vec<watch::Event>>();
            let mut streams = Vec::new();
            for r in &self.cfg.roots {
                let b = r.to_string_lossy().into_owned().into_bytes();
                if let Some(s) = watch::watch(&b, since, 0.0, tx.clone()) {
                    streams.push(s);
                }
            }
            drop(tx);
            let t0 = Instant::now();
            let (mut all, mut done) = (Vec::new(), 0);
            while done < streams.len() && t0.elapsed() < timeout {
                match rx.recv_timeout(timeout.saturating_sub(t0.elapsed())) {
                    Ok(batch) => {
                        done += batch.iter().filter(|e| e.flags & HISTORY_DONE != 0).count();
                        all.extend(batch.into_iter().filter(|e| e.flags & HISTORY_DONE == 0));
                    }
                    Err(_) => break,
                }
            }
            drop(streams);
            let n = all.len();
            self.on_batch(&all);
            n
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = timeout;
            0
        }
    }

    /// Fast catch-up without event history: stat every indexed folder in
    /// parallel and treat the ones whose mtime changed (or that vanished) as
    /// change events. Catches added / removed / renamed entries and new or
    /// deleted repos under indexed folders; it does not see in-place file
    /// edits (a watcher does). ~50 ms for ~270k entries on an M-series Mac,
    /// versus seconds to replay a busy home folder's FSEvents history.
    /// Returns the number of repos re-walked, added or removed.
    pub fn recheck(&self) -> usize {
        use rayon::prelude::*;
        let snap = self.snapshot();
        let mut indexes: Vec<(&CodeIndex, Option<&[u64]>)> = vec![(&snap.base, snap.dead_mask.as_deref())];
        indexes.extend(snap.overlays.iter().map(|o| (&**o, None)));
        let mut events: Vec<(PathBuf, u32)> = Vec::new();
        for (ci, dead) in indexes {
            let idx = &ci.idx;
            let built = ci.info.built_at;
            let changed: Vec<(PathBuf, u32)> = (0..idx.n as u32)
                .into_par_iter()
                .filter(|&e| idx.dir_id(e).is_some() && dead.is_none_or(|m| m[e as usize >> 6] & (1 << (e & 63)) == 0))
                .filter_map(|e| {
                    let mut b = Vec::new();
                    idx.path(e, &mut b);
                    let p = bytes_path(&b);
                    let then = idx.mtime(e);
                    let now = std::fs::symlink_metadata(&p).ok().map(|m| flashfind_core::walk::mtime_of(&m));
                    // Same-second edits around the crawl are ambiguous: recheck those too.
                    let stale = match now {
                        Some(m) => m != then || m >= built - 2,
                        None => true,
                    };
                    stale.then_some((p, 0))
                })
                .collect();
            events.extend(changed);
        }
        self.apply(&events)
    }

    /// Fold overlays into a fresh saved base (call before quitting).
    pub fn checkpoint(&self) {
        let s = self.snapshot();
        if !s.overlays.is_empty() || s.dead.iter().any(|d| *d) {
            self.compact();
        }
    }
}

impl Drop for CodeLive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
fn bytes_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}
#[cfg(not(unix))]
fn bytes_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

/// A search session over a [`CodeLive`]: one engine session per index,
/// reused across keystrokes (narrowing caches stay warm while the base is
/// unchanged).
pub struct CodeSession {
    live: Arc<CodeLive>,
    base: Option<(usize, Session)>,
    over: Vec<(usize, Session)>,
}

fn not_found(e: &str) -> bool {
    e.starts_with("no project matches")
}

impl CodeSession {
    pub fn new(live: Arc<CodeLive>) -> CodeSession {
        CodeSession {
            live,
            base: None,
            over: Vec::new(),
        }
    }

    pub fn live(&self) -> &Arc<CodeLive> {
        &self.live
    }

    fn run(&mut self, cq: &CodeQuery, f: &mut dyn FnMut(&CodeIndex, &mut Session, Option<&[u64]>) -> Result<CodeResult, String>) -> Result<CodeResult, String> {
        let t0 = Instant::now();
        let snap = self.live.snapshot();
        let bp = Arc::as_ptr(&snap.base) as usize;
        if self.base.as_ref().is_none_or(|(p, _)| *p != bp) {
            self.base = Some((bp, Session::new(snap.base.idx.clone())));
        }
        let mut over: Vec<(usize, Session)> = Vec::with_capacity(snap.overlays.len());
        let mut old = std::mem::take(&mut self.over);
        for o in &snap.overlays {
            let p = Arc::as_ptr(o) as usize;
            let s = match old.iter().position(|(q, _)| *q == p) {
                Some(i) => old.swap_remove(i).1,
                None => Session::new(o.idx.clone()),
            };
            over.push((p, s));
        }
        let mut results = Vec::new();
        let mut first_err: Option<String> = None;
        let (_, bs) = self.base.as_mut().unwrap();
        match f(&snap.base, bs, snap.dead_mask.as_deref()) {
            Ok(r) => results.push(r),
            Err(e) if not_found(&e) => first_err = Some(e),
            Err(e) => return Err(e),
        }
        for (o, (_, s)) in snap.overlays.iter().zip(over.iter_mut()) {
            match f(o, s, None) {
                Ok(r) => results.push(r),
                Err(e) if not_found(&e) => {}
                Err(e) => return Err(e),
            }
        }
        self.over = over;
        if results.is_empty() {
            return Err(first_err.unwrap_or_else(|| "no results".into()));
        }
        let total = results.iter().map(|r| r.total).sum();
        let mut hits: Vec<CodeHit> = results.into_iter().flat_map(|r| r.hits).collect();
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        hits.truncate(cq.limit.max(1));
        let groups = group(&hits);
        Ok(CodeResult {
            hits,
            total,
            elapsed_us: t0.elapsed().as_micros() as u64,
            groups,
        })
    }

    pub fn search(&mut self, cq: &CodeQuery, cancel: &CancelToken) -> Result<CodeResult, String> {
        self.run(cq, &mut |ci, s, extra| ci.search_with(s, cq, cancel, extra))
    }

    pub fn recent(&mut self, cq: &CodeQuery, within: Option<i64>) -> Result<CodeResult, String> {
        self.run(cq, &mut |ci, _, extra| ci.recent_with(cq, within, extra))
    }
}
