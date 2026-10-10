//! Live index: an immutable (mmap'd) base [`Index`] plus an in-memory
//! overlay of what changed since, searched together.
//!
//! * A change to directory `D` (FSEvents, polling, or [`Live::refresh`])
//!   relists `D` only and diffs it against what is known: removed or
//!   replaced base entries are tombstoned (one bit each, subtrees are a
//!   contiguous range), new or changed entries go to the overlay (new
//!   folders are crawled into it).
//! * Lost event history (`MustScanSubDirs`, dropped events, a stale saved
//!   event id) is handled by [`Live::recheck`]: `lstat` every known folder in
//!   the subtree and relist only the ones whose mtime moved – no full recrawl.
//! * [`Live::compact`] folds base − tombstones + overlay back into a fresh
//!   base index (saved + re-mapped if the live index has a path), when the
//!   overlay passes a threshold or on a timer.
//! * The saved index records the FSEvents event id its contents are current
//!   to, so a restart replays only newer events.
//! * While the first crawl runs ([`Live::crawl`]), queries are answered by a
//!   brute-force scan of the listings read so far, with [`Progress`].

use crate::index::{Index, dec_size, enc_mtime, enc_size, entry_prior, now_secs, path_bytes};
use crate::matcher::{bigram_sig, class_mask, fold, may_match};
use crate::query::Query;
use crate::search::{ANC_DECAY, CancelToken, Cancelled, MAX_TOKENS, NO, Row, Strategy, Survivors, TS, combine, run, score_tokens};
use crate::walk::{KIND_DIR, KIND_MASK, Listing, NONE, ScanOptions};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- partial

/// Listings published by a running crawl.
#[derive(Default)]
pub struct Partial {
    data: Mutex<PartialData>,
    pub entries: AtomicU64,
    pub dirs: AtomicU64,
}

#[derive(Default, Clone)]
struct PartialData {
    by_id: Vec<Option<Arc<Listing>>>,
    /// Listing id → (parent listing id, index of its entry there).
    parent: Vec<(u32, u32)>,
}

impl Partial {
    pub fn new() -> Arc<Partial> {
        Arc::new(Partial::default())
    }

    pub(crate) fn add(&self, l: Listing) {
        self.entries.fetch_add(l.ents.len() as u64, Ordering::Relaxed);
        self.dirs.fetch_add(1, Ordering::Relaxed);
        let mut d = self.data.lock().unwrap();
        let id = l.id as usize;
        for (i, e) in l.ents.iter().enumerate() {
            if e.child != NONE {
                let c = e.child as usize;
                if d.parent.len() <= c {
                    d.parent.resize(c + 1, (NONE, NONE));
                }
                d.parent[c] = (l.id, i as u32);
            }
        }
        if d.by_id.len() <= id {
            d.by_id.resize(id + 1, None);
        }
        d.by_id[id] = Some(Arc::new(l));
    }

    fn snapshot(&self) -> PartialData {
        self.data.lock().unwrap().clone()
    }

    /// Listings read so far (for building the index once the crawl ends).
    pub fn listings(&self) -> Vec<Arc<Listing>> {
        self.data.lock().unwrap().by_id.iter().flatten().cloned().collect()
    }
}

impl PartialData {
    fn path(&self, root: &[u8], lid: u32, i: u32, out: &mut Vec<u8>) {
        let mut chain: Vec<(u32, u32)> = vec![(lid, i)];
        let mut l = lid;
        while l != 0 {
            let Some(&(pl, pi)) = self.parent.get(l as usize) else { break };
            if pl == NONE {
                break;
            }
            chain.push((pl, pi));
            l = pl;
        }
        out.clear();
        out.extend_from_slice(root);
        for &(l, i) in chain.iter().rev() {
            let Some(Some(ls)) = self.by_id.get(l as usize) else { continue };
            if !out.ends_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(ls.name(&ls.ents[i as usize]));
        }
    }
}

// ---------------------------------------------------------------- overlay

#[derive(Clone, Debug)]
struct OEnt {
    /// Byte offset of the name within the key path.
    name_at: u32,
    lower: Box<[u8]>,
    mask: u64,
    sig: u64,
    kind: u8,
    size: u64,
    mtime: i64,
    prior: i8,
}

struct State {
    base: Arc<Index>,
    /// Bumps whenever `base` is replaced (invalidates session caches).
    generation: u64,
    dead: Vec<u64>,
    ndead: usize,
    /// Absolute path → entry, for everything added or changed since `base`.
    overlay: BTreeMap<Vec<u8>, OEnt>,
    /// Base folders relisted since `base`: entry → (mtime, listed at).
    relisted: HashMap<u32, (i64, i64)>,
    /// Newest event id applied.
    last_event: u64,
}

impl State {
    fn new(base: Arc<Index>, generation: u64) -> State {
        let dead = vec![0u64; base.n.div_ceil(64)];
        let last_event = base.event_id;
        State {
            base,
            generation,
            dead,
            ndead: 0,
            overlay: BTreeMap::new(),
            relisted: HashMap::new(),
            last_event,
        }
    }
    #[inline]
    fn alive(&self, e: u32) -> bool {
        (self.dead[e as usize >> 6] >> (e & 63)) & 1 == 0
    }
    fn kill(&mut self, lo: u32, hi: u32) {
        for e in lo..hi {
            let (w, b) = (e as usize >> 6, 1u64 << (e & 63));
            if self.dead[w] & b == 0 {
                self.dead[w] |= b;
                self.ndead += 1;
            }
        }
    }
    fn base_dir(&self, path: &[u8]) -> Option<u32> {
        let e = self.base.lookup(path)?;
        (self.alive(e) && self.base.ent_kind()[e as usize] & KIND_MASK == KIND_DIR).then_some(e)
    }
    fn remove_overlay_tree(&mut self, path: &[u8]) {
        self.overlay.remove(path);
        let mut pre = path.to_vec();
        pre.push(b'/');
        let keys: Vec<Vec<u8>> = self
            .overlay
            .range(pre.clone()..)
            .take_while(|(k, _)| k.starts_with(&pre))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.overlay.remove(&k);
        }
    }
    /// Overlay entries that are direct children of `dir`.
    fn overlay_children<'a>(&'a self, dir: &[u8]) -> impl Iterator<Item = (&'a Vec<u8>, &'a OEnt)> + 'a {
        let mut pre = dir.to_vec();
        if !pre.ends_with(b"/") {
            pre.push(b'/');
        }
        let n = pre.len();
        self.overlay
            .range(pre.clone()..)
            .take_while(move |(k, _)| k.starts_with(&pre))
            .filter(move |(k, _)| !k[n..].contains(&b'/'))
    }
    fn dirty(&self) -> bool {
        !self.overlay.is_empty() || self.ndead > 0 || !self.relisted.is_empty()
    }
}

/// Change computed outside the write lock, applied under it.
#[derive(Debug)]
enum Op {
    Kill(u32, u32),
    RemoveTree(Vec<u8>),
    Upsert(Vec<u8>, u8, u64, i64),
    /// A whole new folder: (path, kind, size, mtime) rows, the folder first.
    AddTree(Vec<(Vec<u8>, u8, u64, i64)>),
    BaseDirMtime(u32, i64, i64),
    OverlayMtime(Vec<u8>, i64),
}

// ---------------------------------------------------------------- live

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Crawling = 0,
    Building = 1,
    Ready = 2,
}

#[derive(Clone, Debug)]
pub struct LiveOptions {
    pub threads: usize,
    /// Compact when overlay + tombstones exceed this many entries.
    pub compact_threshold: usize,
    /// Compact a dirty index at least this often.
    pub compact_interval: Duration,
    /// FSEvents coalescing latency.
    pub latency: Duration,
    /// Polling period where FSEvents is unavailable (or if forced).
    pub poll: Duration,
    pub force_poll: bool,
}

impl Default for LiveOptions {
    fn default() -> Self {
        LiveOptions {
            threads: 8,
            compact_threshold: 100_000,
            compact_interval: Duration::from_secs(600),
            latency: Duration::from_millis(30),
            poll: Duration::from_secs(2),
            force_poll: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub phase: u8,
    pub watching: bool,
    /// Entries crawled so far (crawling) / in the base index (ready).
    pub entries: u64,
    pub dirs: u64,
    pub overlay: u64,
    pub dead: u64,
    pub last_event_id: u64,
    pub generation: u64,
    pub compactions: u64,
    pub dirs_relisted: u64,
    /// Since the crawl (or open) started.
    pub elapsed_ms: f64,
}

pub struct Live {
    root: Vec<u8>,
    root_is_slash: bool,
    state: RwLock<State>,
    partial: RwLock<Option<Arc<Partial>>>,
    phase: AtomicU8,
    /// Serialises writers (watcher, refresh, compaction).
    writer: Mutex<()>,
    path: Mutex<Option<PathBuf>>,
    opts: LiveOptions,
    started: Instant,
    watch: Mutex<Option<WatchHandle>>,
    watching: AtomicBool,
    compactions: AtomicU64,
    relisted: AtomicU64,
    last_compact: Mutex<Instant>,
}

/// One search hit, materialised while the state was locked.
#[derive(Clone, Debug)]
pub struct LiveHit {
    pub score: i32,
    pub path: Vec<u8>,
    pub kind: u8,
    pub size: u64,
    pub mtime: i64,
    /// Base-index entry, or `u32::MAX` for overlay/partial hits.
    pub entry: u32,
}

impl LiveHit {
    pub fn path_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.path)
    }
}

#[derive(Clone, Debug)]
pub struct LiveResult {
    pub hits: Vec<LiveHit>,
    pub total: u64,
    pub total_exact: bool,
    pub strategy: Strategy,
    /// Answered from a crawl still in progress.
    pub partial: bool,
    pub overlay_hits: u64,
}

fn sidecar(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".events");
    PathBuf::from(s)
}

impl Live {
    fn with_base(base: Arc<Index>, path: Option<PathBuf>, opts: LiveOptions, phase: Phase) -> Live {
        let root = base.root().to_vec();
        Live {
            root_is_slash: root == b"/",
            root,
            state: RwLock::new(State::new(base, 0)),
            partial: RwLock::new(None),
            phase: AtomicU8::new(phase as u8),
            writer: Mutex::new(()),
            path: Mutex::new(path),
            opts,
            started: Instant::now(),
            watch: Mutex::new(None),
            watching: AtomicBool::new(false),
            compactions: AtomicU64::new(0),
            relisted: AtomicU64::new(0),
            last_compact: Mutex::new(Instant::now()),
        }
    }

    /// Wrap an existing index (no saving on compaction).
    pub fn from_index(idx: Arc<Index>, opts: LiveOptions) -> Arc<Live> {
        Arc::new(Live::with_base(idx, None, opts, Phase::Ready))
    }

    /// Open a saved index; compactions are saved back to `path`. The event
    /// id to resume from is the newer of the index header and its
    /// `.events` sidecar (written when the live state equals the file).
    pub fn open(path: &Path, opts: LiveOptions) -> std::io::Result<Arc<Live>> {
        let idx = Index::load(path)?;
        let (created, n) = (idx.created, idx.n);
        let live = Live::with_base(Arc::new(idx), Some(path.to_path_buf()), opts, Phase::Ready);
        if let Ok(s) = std::fs::read_to_string(sidecar(path)) {
            let v: Vec<i64> = s.split_whitespace().filter_map(|w| w.parse().ok()).collect();
            if let [c, nn, ev] = v[..]
                && c == created
                && nn as usize == n
            {
                let mut st = live.state.write().unwrap();
                st.last_event = st.last_event.max(ev as u64);
            }
        }
        Ok(Arc::new(live))
    }

    /// Start crawling `root` in the background. The returned index is
    /// searchable at once (partial results while crawling, see
    /// [`Live::progress`]); when the crawl ends the index is built, saved
    /// to `path` (if any) and swapped in.
    pub fn crawl(root: &Path, path: Option<PathBuf>, opts: LiveOptions) -> Arc<Live> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let live = Arc::new(Live::with_base(Arc::new(Index::empty(&root)), path, opts, Phase::Crawling));
        let partial = Partial::new();
        *live.partial.write().unwrap() = Some(partial.clone());
        let l2 = live.clone();
        std::thread::Builder::new()
            .name("flashfind-crawl".into())
            .spawn(move || {
                let created = now_secs();
                let event_id = crate::watch::current_event_id();
                let so = ScanOptions {
                    threads: l2.opts.threads,
                    skip: Vec::new(),
                };
                crate::walk::scan_into(&root, &so, Some(partial.clone()));
                l2.phase.store(Phase::Building as u8, Ordering::Release);
                let ls = partial.listings();
                let refs: Vec<&Listing> = ls.iter().map(|a| a.as_ref()).collect();
                let idx = Index::build_refs(&refs, &root, created, event_id);
                drop(refs);
                let idx = l2.persist(idx);
                {
                    let _w = l2.writer.lock().unwrap();
                    let mut st = l2.state.write().unwrap();
                    let g = st.generation + 1;
                    *st = State::new(Arc::new(idx), g);
                }
                *l2.partial.write().unwrap() = None;
                l2.phase.store(Phase::Ready as u8, Ordering::Release);
            })
            .expect("spawn crawl thread");
        live
    }

    /// Save `idx` to the live path (if any) and return the mmap'd copy, so
    /// the owned build buffer is freed.
    fn persist(&self, idx: Index) -> Index {
        let Some(p) = self.path.lock().unwrap().clone() else { return idx };
        if idx.save(&p).is_err() {
            return idx;
        }
        let _ = std::fs::write(sidecar(&p), format!("{} {} {}\n", idx.created, idx.n, idx.event_id));
        match Index::load(&p) {
            Ok(m) => {
                m.prefault();
                m
            }
            Err(_) => idx,
        }
    }

    pub fn root(&self) -> &[u8] {
        &self.root
    }
    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Acquire) {
            0 => Phase::Crawling,
            1 => Phase::Building,
            _ => Phase::Ready,
        }
    }
    /// Block until the first crawl has finished (or `timeout`).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let t = Instant::now();
        while self.phase() != Phase::Ready {
            if t.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }
    pub fn base(&self) -> Arc<Index> {
        self.state.read().unwrap().base.clone()
    }

    pub fn progress(&self) -> Progress {
        let st = self.state.read().unwrap();
        let mut p = Progress {
            phase: self.phase() as u8,
            watching: self.watching.load(Ordering::Relaxed),
            entries: st.base.n as u64,
            dirs: st.base.d as u64,
            overlay: st.overlay.len() as u64,
            dead: st.ndead as u64,
            last_event_id: st.last_event,
            generation: st.generation,
            compactions: self.compactions.load(Ordering::Relaxed),
            dirs_relisted: self.relisted.load(Ordering::Relaxed),
            elapsed_ms: self.started.elapsed().as_secs_f64() * 1e3,
        };
        drop(st);
        if let Some(part) = self.partial.read().unwrap().as_ref() {
            p.entries = part.entries.load(Ordering::Relaxed);
            p.dirs = part.dirs.load(Ordering::Relaxed);
        }
        p
    }

    // ------------------------------------------------------------ updates

    fn in_root(&self, path: &[u8]) -> bool {
        path.starts_with(&self.root) && (path.len() == self.root.len() || self.root_is_slash || path[self.root.len()] == b'/')
    }

    /// Relist these directories (an FSEvents batch, or by hand).
    pub fn refresh<P: AsRef<[u8]>>(&self, dirs: &[P]) -> usize {
        if self.phase() != Phase::Ready {
            return 0;
        }
        let _w = self.writer.lock().unwrap();
        self.refresh_locked(dirs.iter().map(|d| d.as_ref().to_vec()).collect())
    }

    fn refresh_locked(&self, dirs: Vec<Vec<u8>>) -> usize {
        let mut set: Vec<Vec<u8>> = dirs.into_iter().filter(|d| self.in_root(d)).collect::<HashSet<_>>().into_iter().collect();
        set.sort();
        if set.is_empty() {
            return 0;
        }
        let ops: Vec<Op> = {
            let st = self.state.read().unwrap();
            set.par_iter().flat_map_iter(|d| self.diff_dir(&st, d)).collect()
        };
        self.relisted.fetch_add(set.len() as u64, Ordering::Relaxed);
        let mut st = self.state.write().unwrap();
        for op in ops {
            self.apply(&mut st, op);
        }
        set.len()
    }

    /// Re-check every known folder under `dir` by mtime and relist the ones
    /// that changed (event history lost, or polling).
    pub fn recheck(&self, dir: &[u8]) -> usize {
        if self.phase() != Phase::Ready {
            return 0;
        }
        let _w = self.writer.lock().unwrap();
        self.recheck_locked(dir)
    }

    fn recheck_locked(&self, dir: &[u8]) -> usize {
        let dir = if self.in_root(dir) { dir.to_vec() } else { self.root.clone() };
        // (path, stored mtime, listed at)
        let cands: Vec<(Vec<u8>, i64, i64)> = {
            let st = self.state.read().unwrap();
            let mut v = Vec::new();
            let b = &st.base;
            if let Some(e) = st.base_dir(&dir) {
                let (kinds, end) = (b.ent_kind(), b.end(e));
                let mut path = Vec::new();
                for x in e..end {
                    if kinds[x as usize] & KIND_MASK == KIND_DIR && st.alive(x) {
                        b.path(x, &mut path);
                        let (m, at) = st.relisted.get(&x).copied().unwrap_or((b.mtime(x), b.created));
                        v.push((path.clone(), m, at));
                    }
                }
            }
            let mut pre = dir.clone();
            pre.push(b'/');
            if let Some(o) = st.overlay.get(&dir)
                && o.kind & KIND_MASK == KIND_DIR
            {
                v.push((dir.clone(), o.mtime, 0));
            }
            for (k, o) in st.overlay.range(pre.clone()..).take_while(|(k, _)| k.starts_with(&pre)) {
                if o.kind & KIND_MASK == KIND_DIR {
                    v.push((k.clone(), o.mtime, 0));
                }
            }
            v
        };
        let changed: Vec<Vec<u8>> = cands
            .into_par_iter()
            .filter_map(|(p, m, at)| {
                let md = std::fs::symlink_metadata(bytes_path(&p)).ok()?;
                let now = crate::walk::mtime_of(&md);
                // Same-second changes after the listing are invisible to an
                // mtime compare, hence the `at` slack.
                (now != m || now + 1 >= at).then_some(p)
            })
            .collect();
        // Vanished folders are handled by their parent's relist.
        self.refresh_locked(changed)
    }

    fn known_child_meta(&self, st: &State, path: &[u8]) -> Option<(Option<u32>, bool)> {
        let be = st.base_dir(path);
        let ov = st.overlay.get(path).is_some_and(|o| o.kind & KIND_MASK == KIND_DIR);
        (be.is_some() || ov).then_some((be, ov))
    }

    /// Diff one directory against the known state.
    fn diff_dir(&self, st: &State, path: &[u8]) -> Vec<Op> {
        let mut ops = Vec::new();
        let Some((be, ov)) = self.known_child_meta(st, path) else { return ops };
        let Some(listing) = crate::walk::list_dir(&bytes_path(path)) else {
            return ops;
        };
        let own_mtime = std::fs::symlink_metadata(bytes_path(path)).map(|m| crate::walk::mtime_of(&m)).unwrap_or(0);
        // name → (base entry or NONE, kind, enc size, enc mtime)
        let mut known: HashMap<Vec<u8>, (u32, u8, u32, u32)> = HashMap::new();
        if let Some(e) = be {
            let b = &st.base;
            for c in b.children(e) {
                if st.alive(c) {
                    let name = b.orig(b.ent_name()[c as usize]).to_vec();
                    known.insert(name, (c, b.ent_kind()[c as usize], b.ent_size_raw()[c as usize], b.ent_mtime_raw()[c as usize]));
                }
            }
        }
        for (k, o) in st.overlay_children(path) {
            known.insert(k[o.name_at as usize..].to_vec(), (NONE, o.kind, enc_size(o.size), enc_mtime(o.mtime)));
        }
        let now = now_secs();
        for raw in &listing.ents {
            let name = listing.name(raw);
            let cpath = crate::walk::join(path, name);
            let is_dir = raw.kind & KIND_MASK == KIND_DIR;
            match known.remove(name) {
                Some((e, k, s, m)) if k & KIND_MASK == raw.kind & KIND_MASK => {
                    if is_dir || (s == enc_size(raw.size) && m == enc_mtime(raw.mtime)) {
                        continue;
                    }
                    if e != NONE {
                        ops.push(Op::Kill(e, e + 1));
                    }
                    ops.push(Op::Upsert(cpath, raw.kind, raw.size, raw.mtime));
                }
                old => {
                    if let Some((e, ..)) = old {
                        if e != NONE {
                            ops.push(Op::Kill(e, st.base.end(e)));
                        }
                        ops.push(Op::RemoveTree(cpath.clone()));
                    }
                    if is_dir && raw.kind & crate::walk::FLAG_MOUNT == 0 {
                        ops.push(Op::AddTree(self.crawl_tree(&cpath, raw.kind, raw.mtime)));
                    } else {
                        ops.push(Op::Upsert(cpath, raw.kind, raw.size, raw.mtime));
                    }
                }
            }
        }
        for (name, (e, ..)) in known {
            if e != NONE {
                ops.push(Op::Kill(e, st.base.end(e)));
            }
            ops.push(Op::RemoveTree(crate::walk::join(path, &name)));
        }
        if let Some(e) = be {
            ops.push(Op::BaseDirMtime(e, own_mtime, now));
        } else if ov {
            ops.push(Op::OverlayMtime(path.to_vec(), own_mtime));
        }
        ops
    }

    /// Crawl a newly appeared folder into overlay rows.
    fn crawl_tree(&self, dir: &[u8], kind: u8, mtime: i64) -> Vec<(Vec<u8>, u8, u64, i64)> {
        let mut rows = vec![(dir.to_vec(), kind, 0, mtime)];
        let so = ScanOptions {
            threads: self.opts.threads.clamp(1, 4),
            skip: Vec::new(),
        };
        let ls = crate::walk::scan(&bytes_path(dir), &so);
        let mut by_id: Vec<Option<&Listing>> = Vec::new();
        for l in &ls {
            if by_id.len() <= l.id as usize {
                by_id.resize(l.id as usize + 1, None);
            }
            by_id[l.id as usize] = Some(l);
        }
        let mut stack: Vec<(u32, Vec<u8>)> = vec![(0, dir.to_vec())];
        while let Some((id, p)) = stack.pop() {
            let Some(Some(l)) = by_id.get(id as usize) else { continue };
            for r in &l.ents {
                let cp = crate::walk::join(&p, l.name(r));
                if r.child != NONE {
                    stack.push((r.child, cp.clone()));
                }
                rows.push((cp, r.kind, r.size, r.mtime));
            }
        }
        rows
    }

    fn make_oent(&self, path: &[u8], kind: u8, size: u64, mtime: i64) -> OEnt {
        let name_at = path.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1);
        let rel = &path[self.root.len().min(path.len())..];
        let comps: Vec<&[u8]> = rel.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
        let mut inh = 0;
        for (i, c) in comps.iter().enumerate().take(comps.len().saturating_sub(1)) {
            inh = entry_prior(c, KIND_DIR, i as u32 + 1, inh, self.root_is_slash).1;
        }
        let name = &path[name_at..];
        let prior = entry_prior(name, kind, comps.len().max(1) as u32, inh, self.root_is_slash).0;
        let lower: Box<[u8]> = name.iter().map(|&b| fold(b)).collect();
        OEnt {
            name_at: name_at as u32,
            mask: class_mask(&lower),
            sig: bigram_sig(&lower),
            lower,
            kind,
            size,
            mtime,
            prior,
        }
    }

    fn apply(&self, st: &mut State, op: Op) {
        match op {
            Op::Kill(a, b) => st.kill(a, b),
            Op::RemoveTree(p) => st.remove_overlay_tree(&p),
            Op::Upsert(p, k, s, m) => {
                let o = self.make_oent(&p, k, s, m);
                st.overlay.insert(p, o);
            }
            Op::AddTree(rows) => {
                if let Some((p, ..)) = rows.first() {
                    st.remove_overlay_tree(&p.clone());
                }
                for (p, k, s, m) in rows {
                    let o = self.make_oent(&p, k, s, m);
                    st.overlay.insert(p, o);
                }
            }
            Op::BaseDirMtime(e, m, at) => {
                st.relisted.insert(e, (m, at));
            }
            Op::OverlayMtime(p, m) => {
                if let Some(o) = st.overlay.get_mut(&p) {
                    o.mtime = m;
                }
            }
        }
    }

    // --------------------------------------------------------- compaction

    /// Fold the overlay and tombstones into a new base index (saved and
    /// re-mapped when the live index has a path). Returns false if clean.
    pub fn compact(&self) -> bool {
        if self.phase() != Phase::Ready {
            return false;
        }
        let _w = self.writer.lock().unwrap();
        self.compact_locked()
    }

    fn compact_locked(&self) -> bool {
        let (idx, gen_next) = {
            let st = self.state.read().unwrap();
            if !st.dirty() {
                return false;
            }
            let ls = self.materialize(&st);
            let refs: Vec<&Listing> = ls.iter().collect();
            let idx = Index::build_refs(&refs, &bytes_path(&self.root), st.base.created, st.last_event);
            (idx, st.generation + 1)
        };
        let idx = self.persist(idx);
        let mut st = self.state.write().unwrap();
        let last = st.last_event;
        *st = State::new(Arc::new(idx), gen_next);
        st.last_event = last;
        drop(st);
        self.compactions.fetch_add(1, Ordering::Relaxed);
        *self.last_compact.lock().unwrap() = Instant::now();
        true
    }

    /// Current tree (base − tombstones + overlay) as crawler listings.
    fn materialize(&self, st: &State) -> Vec<Listing> {
        let b = &st.base;
        // Overlay grouped by parent folder.
        let mut kids: HashMap<&[u8], Vec<(&Vec<u8>, &OEnt)>> = HashMap::new();
        for (k, o) in &st.overlay {
            let parent = &k[..(o.name_at as usize).saturating_sub(1).max(if self.root_is_slash { 1 } else { 0 })];
            kids.entry(parent).or_default().push((k, o));
        }
        let mut out = Vec::new();
        let mut next = 1u32;
        let mut work: Vec<(u32, Vec<u8>, Option<u32>)> = vec![(0, self.root.clone(), Some(0))];
        while let Some((lid, path, be)) = work.pop() {
            let mut l = Listing { id: lid, ..Default::default() };
            if let Some(e) = be {
                for c in b.children(e) {
                    if !st.alive(c) {
                        continue;
                    }
                    let k = b.ent_kind()[c as usize];
                    let m = st.relisted.get(&c).map_or(b.mtime(c), |r| r.0);
                    let name = b.orig(b.ent_name()[c as usize]);
                    if let Some(i) = l.push(name, k, b.size(c), m)
                        && k & KIND_MASK == KIND_DIR
                    {
                        l.ents[i].child = next;
                        work.push((next, crate::walk::join(&path, name), Some(c)));
                        next += 1;
                    }
                }
            }
            if let Some(v) = kids.get(path.as_slice()) {
                for (k, o) in v {
                    let name = &k[o.name_at as usize..];
                    if let Some(i) = l.push(name, o.kind, o.size, o.mtime)
                        && o.kind & KIND_MASK == KIND_DIR
                    {
                        l.ents[i].child = next;
                        work.push((next, (*k).clone(), None));
                        next += 1;
                    }
                }
            }
            out.push(l);
        }
        out
    }

    /// Compact if dirty, else record the last event id next to the saved
    /// index, so the next start replays only newer events.
    pub fn checkpoint(&self) {
        let _w = self.writer.lock().unwrap();
        if !self.compact_locked()
            && let Some(p) = self.path.lock().unwrap().clone()
        {
            let st = self.state.read().unwrap();
            if !st.dirty() {
                let _ = std::fs::write(sidecar(&p), format!("{} {} {}\n", st.base.created, st.base.n, st.last_event));
            }
        }
    }

    fn maybe_compact(&self) {
        let (size, dirty) = {
            let st = self.state.read().unwrap();
            (st.overlay.len() + st.ndead, st.dirty())
        };
        let due = self.last_compact.lock().unwrap().elapsed() >= self.opts.compact_interval;
        if dirty && (size >= self.opts.compact_threshold || due) {
            let _w = self.writer.lock().unwrap();
            self.compact_locked();
        }
    }

    /// Apply one batch of change events.
    pub fn apply_events(&self, evs: &[crate::watch::Event]) {
        if evs.is_empty() || self.phase() != Phase::Ready {
            return;
        }
        let _w = self.writer.lock().unwrap();
        let mut dirs = Vec::new();
        let mut rescan = Vec::new();
        let mut max_id = 0;
        for ev in evs {
            max_id = max_id.max(ev.id);
            if ev.flags & crate::watch::HISTORY_DONE != 0 && ev.flags & crate::watch::LOSSY == 0 {
                continue;
            }
            if ev.flags & (crate::watch::USER_DROPPED | crate::watch::KERNEL_DROPPED | crate::watch::EVENT_IDS_WRAPPED | crate::watch::ROOT_CHANGED) != 0 {
                rescan.push(self.root.clone());
            } else if ev.flags & crate::watch::LOSSY != 0 {
                rescan.push(ev.path.clone());
            } else {
                dirs.push(ev.path.clone());
            }
        }
        rescan.sort();
        rescan.dedup();
        for r in &rescan {
            self.recheck_locked(r);
        }
        // A vanished folder's own event is useless; relist its parent too.
        let mut all = dirs.clone();
        for d in &dirs {
            if !bytes_path(d).exists()
                && let Some(p) = d.iter().rposition(|&b| b == b'/')
            {
                all.push(d[..p.max(1)].to_vec());
            }
        }
        self.refresh_locked(all);
        let mut st = self.state.write().unwrap();
        st.last_event = st.last_event.max(max_id);
    }

    // ------------------------------------------------------------ watching

    /// Start watching for changes (FSEvents on macOS, polling elsewhere or
    /// when `force_poll`). If the first crawl is still running, the watcher
    /// starts once it is done and replays events since the crawl began.
    pub fn watch(self: &Arc<Self>) -> std::io::Result<()> {
        let mut g = self.watch.lock().unwrap();
        if g.is_some() {
            return Ok(());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let weak = Arc::downgrade(self);
        let (s2, opts) = (stop.clone(), self.opts.clone());
        let th = std::thread::Builder::new()
            .name("flashfind-watch".into())
            .spawn(move || watcher(weak, s2, opts))?;
        *g = Some(WatchHandle { stop, _thread: th });
        self.watching.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn unwatch(&self) {
        if let Some(h) = self.watch.lock().unwrap().take() {
            h.stop.store(true, Ordering::Relaxed);
        }
        self.watching.store(false, Ordering::Relaxed);
    }
}

struct WatchHandle {
    stop: Arc<AtomicBool>,
    _thread: std::thread::JoinHandle<()>,
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(h) = self.watch.get_mut().unwrap().take() {
            h.stop.store(true, Ordering::Relaxed);
        }
    }
}

fn watcher(weak: Weak<Live>, stop: Arc<AtomicBool>, opts: LiveOptions) {
    // Wait for the first crawl.
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match weak.upgrade() {
            Some(l) if l.phase() == Phase::Ready => break,
            Some(_) => std::thread::sleep(Duration::from_millis(20)),
            None => return,
        }
    }
    #[cfg(target_os = "macos")]
    if !opts.force_poll {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<crate::watch::Event>>();
        let (root, since) = match weak.upgrade() {
            Some(l) => (l.root.clone(), l.state.read().unwrap().last_event),
            None => return,
        };
        if let Some(stream) = crate::watch::watch(&root, since, opts.latency.as_secs_f64(), tx) {
            while !stop.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(mut batch) => {
                        while let Ok(more) = rx.try_recv() {
                            batch.extend(more);
                        }
                        let Some(l) = weak.upgrade() else { break };
                        l.apply_events(&batch);
                        l.maybe_compact();
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        let Some(l) = weak.upgrade() else { break };
                        l.maybe_compact();
                    }
                    Err(_) => break,
                }
            }
            drop(stream);
            return;
        }
    }
    // Polling fallback.
    let mut last = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
        if last.elapsed() < opts.poll {
            continue;
        }
        last = Instant::now();
        let Some(l) = weak.upgrade() else { break };
        let root = l.root.clone();
        l.recheck(&root);
        l.maybe_compact();
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

// ---------------------------------------------------------------- search

/// Search state for one search field over a [`Live`] index.
pub struct LiveSession {
    live: Arc<Live>,
    cache: Vec<Survivors>,
    generation: u64,
}

fn entry_filters_ok(q: &Query, kind: u8, size: u64, mtime: i64) -> bool {
    (1u8 << (kind & KIND_MASK)) & q.kinds != 0 && size >= q.size.0 && size <= q.size.1 && mtime >= q.mtime.0 && mtime <= q.mtime.1
}

fn in_scope(q: &Query, path: &[u8]) -> bool {
    match &q.scope {
        None => true,
        Some(s) => path.starts_with(s) && path.len() > s.len() && (s.ends_with(b"/") || path[s.len()] == b'/'),
    }
}

fn has_ext(lower: &[u8], exts: &[Vec<u8>]) -> bool {
    exts.iter()
        .any(|e| lower.len() > e.len() && lower.ends_with(e) && lower[lower.len() - e.len() - 1] == b'.')
}

/// Score one free-standing entry (overlay or partial crawl) given its
/// folded name, original name and folder components below the root
/// (nearest last). Mirrors the base index's scoring.
fn score_free(q: &Query, lower: &[u8], orig: &[u8], mask: u64, sig: u64, folders: &mut dyn FnMut() -> Vec<Vec<u8>>) -> Option<i32> {
    if !q.exts.is_empty() && !has_ext(lower, &q.exts) {
        return None;
    }
    if q.negs.iter().any(|n| memchr::memmem::find(lower, n).is_some()) {
        return None;
    }
    let toks = &q.tokens[..q.tokens.len().min(MAX_TOKENS)];
    if toks.is_empty() {
        return Some(0);
    }
    if toks.len() == 1 && !may_match(toks[0].pat.mask, toks[0].pat.typos, mask) {
        return None;
    }
    let ntok = toks.len();
    let all: u8 = ((1u16 << ntok) - 1) as u8;
    let (score, bits, ts) = score_tokens(toks, lower, orig, sig, mask, true).unwrap_or((0, 0, [NO; TS]));
    if bits == all {
        return Some(score);
    }
    // As in the base index, the name itself must match at least one token.
    if ntok == 1 || bits == 0 {
        return None;
    }
    // Ancestor folders, nearest first, decayed per level (as in the base).
    let comps = folders();
    let nt = ntok.min(TS);
    let mut anc_bits = 0u8;
    let mut anc = [NO; TS];
    for (lvl, c) in comps.iter().rev().enumerate() {
        let cl: Vec<u8> = c.iter().map(|&b| fold(b)).collect();
        if let Some((_, b, cts)) = score_tokens(toks, &cl, c, bigram_sig(&cl), class_mask(&cl), true) {
            anc_bits |= b;
            for t in 0..nt {
                if cts[t] != NO {
                    let v = cts[t] - (ANC_DECAY * lvl as i32) as i16;
                    anc[t] = anc[t].max(v);
                }
            }
        }
    }
    let row = Row { id: 0, score, bits, ts };
    combine(&row, all, ntok, anc_bits, &anc[..nt])
}

fn cmp_hits(a: &LiveHit, b: &LiveHit) -> std::cmp::Ordering {
    b.score.cmp(&a.score).then(a.entry.cmp(&b.entry)).then_with(|| a.path.cmp(&b.path))
}

impl LiveSession {
    pub fn new(live: Arc<Live>) -> LiveSession {
        LiveSession {
            live,
            cache: Vec::new(),
            generation: u64::MAX,
        }
    }
    pub fn live(&self) -> &Arc<Live> {
        &self.live
    }

    pub fn search(&mut self, q: &Query, cancel: &CancelToken) -> Result<LiveResult, Cancelled> {
        let partial = self.live.partial.read().unwrap().clone();
        if let Some(p) = partial {
            return self.search_partial(&p, q, cancel);
        }
        let st = self.live.state.read().unwrap();
        if st.generation != self.generation {
            self.cache.clear();
            self.generation = st.generation;
        }
        let dead = (st.ndead > 0).then_some(st.dead.as_slice());
        let r = run(&st.base, q, Some(&mut self.cache), cancel, true, dead)?;
        let limit = q.limit.max(1);
        let b = &st.base;
        let mut hits: Vec<LiveHit> = r
            .hits
            .iter()
            .map(|h| {
                let mut path = Vec::new();
                b.path(h.entry, &mut path);
                LiveHit {
                    score: h.score,
                    path,
                    kind: b.ent_kind()[h.entry as usize] & KIND_MASK,
                    size: b.size(h.entry),
                    mtime: b.mtime(h.entry),
                    entry: h.entry,
                }
            })
            .collect();
        let mut overlay_total = 0u64;
        if !st.overlay.is_empty() {
            for (k, o) in &st.overlay {
                if !in_scope(q, k) || !entry_filters_ok(q, o.kind, o.size, o.mtime) {
                    continue;
                }
                let orig = &k[o.name_at as usize..];
                let root_len = self.live.root.len();
                let mut folders = || {
                    let rel = &k[root_len.min(o.name_at as usize)..o.name_at as usize];
                    rel.split(|&b| b == b'/').filter(|c| !c.is_empty()).map(|c| c.to_vec()).collect()
                };
                if let Some(s) = score_free(q, &o.lower, orig, o.mask, o.sig, &mut folders) {
                    overlay_total += 1;
                    hits.push(LiveHit {
                        score: s + o.prior as i32,
                        path: k.clone(),
                        kind: o.kind & KIND_MASK,
                        size: o.size,
                        mtime: o.mtime,
                        entry: u32::MAX,
                    });
                }
            }
            if cancel.is_cancelled() {
                return Err(Cancelled);
            }
            hits.sort_by(cmp_hits);
            hits.truncate(limit);
        }
        Ok(LiveResult {
            hits,
            total: r.total + overlay_total,
            total_exact: r.total_exact,
            strategy: r.strategy,
            partial: false,
            overlay_hits: overlay_total,
        })
    }

    /// Brute-force scan of the listings read so far (first crawl).
    fn search_partial(&mut self, p: &Partial, q: &Query, cancel: &CancelToken) -> Result<LiveResult, Cancelled> {
        let snap = p.snapshot();
        let root = &self.live.root;
        let limit = q.limit.max(1);
        let lists: Vec<&Arc<Listing>> = snap.by_id.iter().flatten().collect();
        let found: Vec<(i32, u32, u32)> = lists
            .par_iter()
            .flat_map_iter(|l| {
                let mut out = Vec::new();
                if cancel.is_cancelled() {
                    return out;
                }
                let mut lower = Vec::new();
                let mut path = Vec::new();
                for (i, r) in l.ents.iter().enumerate() {
                    if !entry_filters_ok(q, r.kind, r.size, r.mtime) {
                        continue;
                    }
                    let name = l.name(r);
                    lower.clear();
                    lower.extend(name.iter().map(|&b| fold(b)));
                    let mask = class_mask(&lower);
                    if q.scope.is_some() {
                        snap.path(root, l.id, i as u32, &mut path);
                        if !in_scope(q, &path) {
                            continue;
                        }
                    }
                    let mut folders = || {
                        snap.path(root, l.id, i as u32, &mut path);
                        let rel = &path[root.len().min(path.len())..];
                        let mut v: Vec<Vec<u8>> = rel.split(|&b| b == b'/').filter(|c| !c.is_empty()).map(|c| c.to_vec()).collect();
                        v.pop();
                        v
                    };
                    if let Some(s) = score_free(q, &lower, name, mask, bigram_sig(&lower), &mut folders) {
                        out.push((s, l.id, i as u32));
                    }
                }
                out
            })
            .collect();
        if cancel.is_cancelled() {
            return Err(Cancelled);
        }
        let total = found.len() as u64;
        let mut found = found;
        let k = limit.min(found.len());
        if k > 0 && found.len() > k {
            found.select_nth_unstable_by(k - 1, |a, b| b.0.cmp(&a.0));
            found.truncate(k);
        }
        let mut hits: Vec<LiveHit> = found
            .into_iter()
            .map(|(s, lid, i)| {
                let mut path = Vec::new();
                snap.path(root, lid, i, &mut path);
                let l = snap.by_id[lid as usize].as_ref().unwrap();
                let r = &l.ents[i as usize];
                LiveHit {
                    score: s,
                    path,
                    kind: r.kind & KIND_MASK,
                    size: r.size,
                    mtime: r.mtime,
                    entry: u32::MAX,
                }
            })
            .collect();
        hits.sort_by(cmp_hits);
        Ok(LiveResult {
            hits,
            total,
            total_exact: true,
            strategy: Strategy::EntryScan,
            partial: true,
            overlay_hits: 0,
        })
    }
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<Live>();
    let _ = dec_size;
    let _ = path_bytes;
}
