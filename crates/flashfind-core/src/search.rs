//! Search execution: name phase (score each distinct name once), entry
//! phase (expand names to entries, apply entry filters + location prior,
//! top-k), plus three shortcuts:
//!
//! * **Narrowing** – a [`Session`] remembers the surviving names of recent
//!   queries; when the new query only narrows one of them (typing another
//!   character), only those survivors are re-scored.
//! * **Prefix range** – names are also stored in sorted order, so a 1–2
//!   byte fuzzy token (or a `^prefix` / `=exact` token) is answered from the
//!   contiguous range of names starting with it. Because score bands never
//!   overlap (see `matcher.rs`), the result is provably identical to a full
//!   scan whenever the k-th hit outranks every possible non-prefix match;
//!   otherwise it falls back to the full scan.
//! * **Cancellation** – every phase polls a [`CancelToken`] between chunks.

use crate::index::Index;
use crate::matcher::{MAX_NON_PREFIX, may_match};
use crate::query::{Mode, Query, Token};
use crate::walk::{KIND_MASK, NONE};
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Name ids per parallel work item.
const NAME_CHUNK: usize = 16 * 1024;
/// Entries per parallel work item in the entry phase (approx.).
const ENTRY_CHUNK: usize = 32 * 1024;
/// Score credited for a token matched by an ancestor folder instead of the name.
const ANCESTOR_SCORE: i32 = 200;
/// Prior range bound (see `index::own_penalty`).
const MAX_PRIOR: i32 = 40;
/// Positive tokens beyond this are ignored (bitmask width).
pub const MAX_TOKENS: usize = 8;
/// Survivor sets kept per session.
const CACHE: usize = 8;

#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("search cancelled")
    }
}
impl std::error::Error for Cancelled {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub entry: u32,
    pub score: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Every distinct name scored.
    Full,
    /// Only the survivors of an earlier query re-scored.
    Narrowed,
    /// Same name predicates as an earlier query: survivors reused as is.
    Cached,
    /// Answered from the sorted-name prefix range.
    PrefixRange,
    /// No name predicate: entries scanned directly.
    EntryScan,
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    /// Matching entries (a lower bound if `!total_exact`).
    pub total: u64,
    pub total_exact: bool,
    pub strategy: Strategy,
    /// Distinct names whose score was computed.
    pub names_scored: usize,
}

impl SearchResult {
    fn empty(strategy: Strategy) -> Self {
        SearchResult { hits: Vec::new(), total: 0, total_exact: true, strategy, names_scored: 0 }
    }
}

#[derive(Clone, Copy, Debug)]
struct Row {
    id: u32,
    score: i32,
    bits: u8,
}

#[derive(Clone, PartialEq, Debug)]
struct NameKey {
    toks: Vec<(Mode, Vec<u8>, u8)>,
    negs: Vec<Vec<u8>>,
    exts: Vec<Vec<u8>>,
}

impl NameKey {
    fn of(q: &Query) -> Self {
        NameKey { toks: q.tokens.iter().take(MAX_TOKENS).map(|t| (t.mode, t.pat.text.clone(), t.pat.typos)).collect(), negs: q.negs.clone(), exts: q.exts.clone() }
    }

    /// Does every name matching `self` also match `prev`?
    fn narrows(&self, prev: &NameKey) -> bool {
        if self.toks.len() != prev.toks.len() {
            return false;
        }
        for ((m, t, k), (pm, pt, pk)) in self.toks.iter().zip(&prev.toks) {
            if m != pm || k != pk {
                return false;
            }
            let ok = match m {
                Mode::Fuzzy | Mode::Substring => memchr::memmem::find(t, pt).is_some(),
                Mode::Prefix => t.starts_with(pt),
                Mode::Suffix => t.ends_with(pt),
                Mode::Exact => t == pt,
            };
            if !ok {
                return false;
            }
        }
        prev.negs.iter().all(|n| self.negs.contains(n)) && (prev.exts.is_empty() || (!self.exts.is_empty() && self.exts.iter().all(|e| prev.exts.contains(e))))
    }
}

struct Survivors {
    key: NameKey,
    rows: Arc<Vec<Row>>,
}

/// Per-thread top-k buffer: amortised `select_nth_unstable`, not a heap.
struct TopK {
    k: usize,
    buf: Vec<Hit>,
    floor: i32,
    total: u64,
}

#[inline]
fn better(a: &Hit, b: &Hit) -> std::cmp::Ordering {
    b.score.cmp(&a.score).then(a.entry.cmp(&b.entry))
}

impl TopK {
    fn new(k: usize) -> Self {
        TopK { k, buf: Vec::with_capacity((2 * k).min(1 << 16)), floor: i32::MIN, total: 0 }
    }
    #[inline]
    fn push(&mut self, h: Hit) {
        self.total += 1;
        if h.score < self.floor {
            return;
        }
        self.buf.push(h);
        if self.buf.len() >= 2 * self.k.max(16) {
            self.trim();
        }
    }
    fn trim(&mut self) {
        if self.buf.len() > self.k {
            self.buf.select_nth_unstable_by(self.k - 1, better);
            self.buf.truncate(self.k);
            self.floor = self.buf.iter().map(|h| h.score).min().unwrap_or(i32::MIN);
        }
    }
    fn merge(mut self, o: TopK) -> TopK {
        self.total += o.total;
        self.buf.extend(o.buf);
        self.trim();
        self
    }
    fn finish(mut self) -> (Vec<Hit>, u64) {
        self.buf.sort_unstable_by(better);
        self.buf.truncate(self.k);
        (self.buf, self.total)
    }
}

struct Ctx<'a> {
    idx: &'a Index,
    q: &'a Query,
    toks: &'a [Token],
    cancel: &'a CancelToken,
}

impl Ctx<'_> {
    /// Name-level predicates and scores for one name id.
    #[inline]
    fn score_name(&self, id: u32, mask: u64) -> Option<Row> {
        let toks = self.toks;
        if toks.len() == 1 {
            let t = &toks[0];
            if !may_match(t.pat.mask, t.pat.typos, mask) {
                return None;
            }
        } else if !toks.is_empty() && !toks.iter().any(|t| may_match(t.pat.mask, t.pat.typos, mask)) {
            return None;
        }
        let lower = self.idx.lower(id);
        if !self.q.exts.is_empty() && !has_ext(lower, &self.q.exts) {
            return None;
        }
        if self.q.negs.iter().any(|n| memchr::memmem::find(lower, n).is_some()) {
            return None;
        }
        if toks.is_empty() {
            return Some(Row { id, score: 0, bits: 0 });
        }
        let orig = self.idx.orig(id);
        let mut score = 0;
        let mut bits = 0u8;
        for (i, t) in toks.iter().enumerate() {
            if toks.len() > 1 && !may_match(t.pat.mask, t.pat.typos, mask) {
                continue;
            }
            if let Some(s) = t.score(lower, orig) {
                score += s;
                bits |= 1 << i;
            }
        }
        (bits != 0).then_some(Row { id, score, bits })
    }

    fn full_names(&self) -> Result<Vec<Row>, Cancelled> {
        let masks = self.idx.name_mask();
        let u = self.idx.u;
        let chunks = u.div_ceil(NAME_CHUNK);
        let parts: Vec<Vec<Row>> = (0..chunks)
            .into_par_iter()
            .map(|c| {
                if self.cancel.is_cancelled() {
                    return Vec::new();
                }
                let lo = c * NAME_CHUNK;
                let hi = (lo + NAME_CHUNK).min(u);
                let mut out = Vec::new();
                for id in lo..hi {
                    if let Some(r) = self.score_name(id as u32, masks[id]) {
                        out.push(r);
                    }
                }
                out
            })
            .collect();
        self.check()?;
        Ok(parts.concat())
    }

    fn rescore(&self, prev: &[Row]) -> Result<Vec<Row>, Cancelled> {
        let masks = self.idx.name_mask();
        let parts: Vec<Vec<Row>> = prev
            .par_chunks(NAME_CHUNK / 4)
            .map(|ch| {
                if self.cancel.is_cancelled() {
                    return Vec::new();
                }
                ch.iter().filter_map(|r| self.score_name(r.id, masks[r.id as usize])).collect()
            })
            .collect();
        self.check()?;
        Ok(parts.concat())
    }

    #[inline]
    fn check(&self) -> Result<(), Cancelled> {
        if self.cancel.is_cancelled() { Err(Cancelled) } else { Ok(()) }
    }

    #[inline]
    fn entry_ok(&self, e: usize, kinds: &[u8], sizes: &[u64], mtimes: &[i64]) -> bool {
        let q = self.q;
        (1u8 << (kinds[e] & KIND_MASK)) & q.kinds != 0 && sizes[e] >= q.size.0 && sizes[e] <= q.size.1 && mtimes[e] >= q.mtime.0 && mtimes[e] <= q.mtime.1
    }

    /// Expand name rows to entries and keep the top `limit`.
    fn entries(&self, rows: &[Row], range: (u32, u32), anc: Option<&[u8]>) -> Result<(Vec<Hit>, u64), Cancelled> {
        let idx = self.idx;
        let (kinds, sizes, mtimes, prior, parent) = (idx.ent_kind(), idx.ent_size(), idx.ent_mtime(), idx.ent_prior(), idx.ent_parent());
        let ntok = self.toks.len().min(MAX_TOKENS);
        let all: u8 = if ntok == 0 { 0 } else { ((1u16 << ntok) - 1) as u8 };
        let full_range = range == (0, idx.n as u32);
        let limit = self.q.limit.max(1);
        // Chunk rows so each work item expands roughly ENTRY_CHUNK entries.
        let off = idx.name_ent_off();
        let mut bounds = vec![0usize];
        let mut acc = 0usize;
        for (i, r) in rows.iter().enumerate() {
            acc += (off[r.id as usize + 1] - off[r.id as usize]) as usize;
            if acc >= ENTRY_CHUNK {
                bounds.push(i + 1);
                acc = 0;
            }
        }
        if *bounds.last().unwrap() != rows.len() {
            bounds.push(rows.len());
        }
        let top = bounds
            .par_windows(2)
            .map(|w| {
                let mut tk = TopK::new(limit);
                if self.cancel.is_cancelled() {
                    return tk;
                }
                for r in &rows[w[0]..w[1]] {
                    let mut ents = idx.entries_of(r.id);
                    if !full_range {
                        let a = ents.partition_point(|&e| e < range.0);
                        let b = ents.partition_point(|&e| e < range.1);
                        ents = &ents[a..b];
                    }
                    for &e in ents {
                        let eu = e as usize;
                        if !self.entry_ok(eu, kinds, sizes, mtimes) {
                            continue;
                        }
                        let mut score = r.score;
                        if let Some(anc) = anc {
                            let p = parent[eu];
                            let ab = if p == NONE { 0 } else { anc[p as usize] };
                            let missing = all & !r.bits;
                            if missing & !ab != 0 {
                                continue;
                            }
                            score += ANCESTOR_SCORE * missing.count_ones() as i32;
                        } else if r.bits != all {
                            continue;
                        }
                        tk.push(Hit { entry: e, score: score + prior[eu] as i32 });
                    }
                }
                tk
            })
            .reduce(|| TopK::new(limit), TopK::merge);
        self.check()?;
        Ok(top.finish())
    }

    /// Ancestor token bits per directory id (multi-token queries).
    fn ancestors(&self, rows: &[Row]) -> Vec<u8> {
        let idx = self.idx;
        let mut name_bits = vec![0u8; idx.u];
        for r in rows {
            name_bits[r.id as usize] = r.bits;
        }
        let (dir_entry, parent, ent_name) = (idx.dir_entry(), idx.ent_parent(), idx.ent_name());
        let mut anc = vec![0u8; idx.d];
        // Dir ids follow DFS order: a parent's id is always smaller. The
        // root's own name never counts.
        for did in 1..idx.d {
            let e = dir_entry[did] as usize;
            let p = parent[e];
            let up = if p == NONE { 0 } else { anc[p as usize] };
            anc[did] = up | name_bits[ent_name[e] as usize];
        }
        anc
    }

    fn entry_scan(&self, range: (u32, u32)) -> Result<(Vec<Hit>, u64), Cancelled> {
        let idx = self.idx;
        let (kinds, sizes, mtimes, prior) = (idx.ent_kind(), idx.ent_size(), idx.ent_mtime(), idx.ent_prior());
        let limit = self.q.limit.max(1);
        let lo = range.0.max(1) as usize;
        let hi = range.1 as usize;
        let top = (lo..hi.max(lo))
            .into_par_iter()
            .with_min_len(ENTRY_CHUNK)
            .fold(
                || TopK::new(limit),
                |mut tk, e| {
                    if self.entry_ok(e, kinds, sizes, mtimes) {
                        tk.push(Hit { entry: e as u32, score: prior[e] as i32 });
                    }
                    tk
                },
            )
            .reduce(|| TopK::new(limit), TopK::merge);
        self.check()?;
        Ok(top.finish())
    }
}

fn has_ext(lower: &[u8], exts: &[Vec<u8>]) -> bool {
    exts.iter().any(|e| lower.len() > e.len() && lower.ends_with(e) && lower[lower.len() - e.len() - 1] == b'.')
}

/// Contiguous range of `name_sorted` whose names start with `t`.
pub fn prefix_range(idx: &Index, t: &[u8]) -> std::ops::Range<usize> {
    let s = idx.name_sorted();
    let lo = s.partition_point(|&id| idx.lower(id) < t);
    let len = s[lo..].partition_point(|&id| idx.lower(id).starts_with(t));
    lo..lo + len
}

fn run(idx: &Index, q: &Query, mut cache: Option<&mut Vec<Survivors>>, cancel: &CancelToken, shortcuts: bool) -> Result<SearchResult, Cancelled> {
    if cancel.is_cancelled() {
        return Err(Cancelled);
    }
    let range = match &q.scope {
        None => (0u32, idx.n as u32),
        Some(p) => match idx.lookup(p) {
            Some(e) => (e + 1, idx.ent_end()[e as usize]),
            None => return Ok(SearchResult::empty(Strategy::Full)),
        },
    };
    let toks = &q.tokens[..q.tokens.len().min(MAX_TOKENS)];
    let ctx = Ctx { idx, q, toks, cancel };
    if q.is_empty_name_query() {
        let (hits, total) = ctx.entry_scan(range)?;
        return Ok(SearchResult { hits, total, total_exact: true, strategy: Strategy::EntryScan, names_scored: 0 });
    }
    // --- Prefix-range shortcut.
    if shortcuts && toks.len() == 1 {
        let t = &toks[0];
        let complete = matches!(t.mode, Mode::Prefix | Mode::Exact);
        if complete || (t.mode == Mode::Fuzzy && t.pat.text.len() <= 2) {
            let r = prefix_range(idx, &t.pat.text);
            let sorted = idx.name_sorted();
            let masks = idx.name_mask();
            let ids = &sorted[r];
            let mut rows: Vec<Row> = ids.par_iter().filter_map(|&id| ctx.score_name(id, masks[id as usize])).collect();
            rows.sort_unstable_by_key(|r| r.id);
            let (hits, total) = ctx.entries(&rows, range, None)?;
            let limit = q.limit.max(1);
            let proven = hits.len() >= limit && hits[limit - 1].score > MAX_NON_PREFIX + MAX_PRIOR;
            if complete || proven {
                return Ok(SearchResult { hits, total, total_exact: complete, strategy: Strategy::PrefixRange, names_scored: ids.len() });
            }
        }
    }
    // --- Name phase: cached, narrowed or full.
    let key = NameKey::of(q);
    let mut strategy = Strategy::Full;
    let mut rows: Option<Arc<Vec<Row>>> = None;
    let mut scored = idx.u;
    if let Some(c) = cache.as_deref() {
        if let Some(s) = c.iter().rev().find(|s| s.key == key) {
            rows = Some(s.rows.clone());
            strategy = Strategy::Cached;
            scored = 0;
        } else if let Some(s) = c.iter().rev().find(|s| key.narrows(&s.key)) {
            scored = s.rows.len();
            rows = Some(Arc::new(ctx.rescore(&s.rows)?));
            strategy = Strategy::Narrowed;
        }
    }
    let rows = match rows {
        Some(r) => r,
        None => Arc::new(ctx.full_names()?),
    };
    if let Some(c) = cache.as_deref_mut()
        && strategy != Strategy::Cached
    {
        c.retain(|s| s.key != key);
        c.push(Survivors { key, rows: rows.clone() });
        if c.len() > CACHE {
            c.remove(0);
        }
    }
    let anc = (toks.len() > 1).then(|| ctx.ancestors(&rows));
    let (hits, total) = ctx.entries(&rows, range, anc.as_deref())?;
    Ok(SearchResult { hits, total, total_exact: true, strategy, names_scored: scored })
}

/// One-shot search (no narrowing cache).
pub fn search(idx: &Index, q: &Query, cancel: &CancelToken) -> Result<SearchResult, Cancelled> {
    run(idx, q, None, cancel, true)
}

/// Full scan with every shortcut disabled (reference for tests/benches).
#[doc(hidden)]
pub fn search_reference(idx: &Index, q: &Query, cancel: &CancelToken) -> Result<SearchResult, Cancelled> {
    run(idx, q, None, cancel, false)
}

/// Per-search-box state: keeps recent survivor sets so each keystroke only
/// re-scores what the previous one left.
pub struct Session {
    idx: Arc<Index>,
    cache: Vec<Survivors>,
}

impl Session {
    pub fn new(idx: Arc<Index>) -> Session {
        Session { idx, cache: Vec::new() }
    }
    pub fn index(&self) -> &Arc<Index> {
        &self.idx
    }
    pub fn search(&mut self, q: &Query, cancel: &CancelToken) -> Result<SearchResult, Cancelled> {
        let idx = self.idx.clone();
        run(&idx, q, Some(&mut self.cache), cancel, true)
    }
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}
