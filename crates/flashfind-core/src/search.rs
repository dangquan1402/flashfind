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

use crate::index::{Index, dec_size};
use crate::matcher::{MAX_NON_PREFIX, TYPO_MAX, may_match};
use crate::query::{Mode, Query, Token};
use crate::walk::{KIND_MASK, NONE};
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Name ids per parallel work item.
const NAME_CHUNK: usize = 4 * 1024;
/// Entries per parallel work item in the entry phase (approx.).
const ENTRY_CHUNK: usize = 32 * 1024;
/// A token matched by an ancestor folder instead of the name is credited
/// `ANC_NUM/ANC_DEN` of the folder name's score, minus `ANC_DECAY` per level
/// above the parent folder (nearer folders rank higher).
pub(crate) const ANC_NUM: i32 = 3;
pub(crate) const ANC_DEN: i32 = 4;
pub(crate) const ANC_DECAY: i32 = 8;
/// Per-token scores kept per row (for the folder + name combination).
pub(crate) const TS: usize = 4;
/// "No score" sentinel in per-token arrays.
pub(crate) const NO: i16 = i16::MIN;
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
    /// Names that survived the name phase.
    pub names_matched: usize,
    /// Time in the name phase and the entry phase (µs).
    pub name_us: u32,
    pub entry_us: u32,
}

impl SearchResult {
    fn empty(strategy: Strategy) -> Self {
        SearchResult {
            hits: Vec::new(),
            total: 0,
            total_exact: true,
            strategy,
            names_scored: 0,
            names_matched: 0,
            name_us: 0,
            entry_us: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Row {
    pub id: u32,
    pub score: i32,
    pub bits: u8,
    /// Score of each of the first [`TS`] tokens against the name ([`NO`] if
    /// it did not match).
    pub ts: [i16; TS],
}

#[derive(Clone, PartialEq, Debug)]
struct NameKey {
    toks: Vec<(Mode, Vec<u8>, u8)>,
    negs: Vec<Vec<u8>>,
    exts: Vec<Vec<u8>>,
}

impl NameKey {
    fn of(q: &Query) -> Self {
        NameKey {
            toks: q.tokens.iter().take(MAX_TOKENS).map(|t| (t.mode, t.pat.text.clone(), t.pat.typos)).collect(),
            negs: q.negs.clone(),
            exts: q.exts.clone(),
        }
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

pub(crate) struct Survivors {
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
        TopK {
            k,
            buf: Vec::with_capacity((2 * k).min(1 << 16)),
            floor: i32::MIN,
            total: 0,
        }
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
    /// Tombstoned entries (live overlay), one bit per entry.
    dead: Option<&'a [u64]>,
    q: &'a Query,
    toks: &'a [Token],
    negs: Vec<memchr::memmem::Finder<'a>>,
    cancel: &'a CancelToken,
    /// False during the clean-only first pass.
    typos: bool,
}

impl Ctx<'_> {
    #[inline(always)]
    fn k(&self, t: &Token) -> u8 {
        if self.typos { t.pat.typos } else { 0 }
    }

    /// Name-level predicates and scores for one name id.
    #[inline]
    fn score_name(&self, id: u32, mask: u64) -> Option<Row> {
        let toks = self.toks;
        if toks.len() == 1 {
            let t = &toks[0];
            if !may_match(t.pat.mask, self.k(t), mask) {
                return None;
            }
        } else if !toks.is_empty() && !toks.iter().any(|t| may_match(t.pat.mask, self.k(t), mask)) {
            return None;
        }
        self.score_name_unmasked(id)
    }

    /// `score_name` minus the single-token mask test (already applied).
    #[inline]
    fn score_name_unmasked(&self, id: u32) -> Option<Row> {
        let toks = self.toks;
        let mask = self.idx.name_mask()[id as usize];
        let lower = self.idx.lower(id);
        if !self.q.exts.is_empty() && !has_ext(lower, &self.q.exts) {
            return None;
        }
        if self.negs.iter().any(|n| n.find(lower).is_some()) {
            return None;
        }
        if toks.is_empty() {
            return Some(Row {
                id,
                score: 0,
                bits: 0,
                ts: [NO; TS],
            });
        }
        let orig = self.idx.orig(id);
        let sig = self.idx.sig(id);
        score_tokens(toks, lower, orig, sig, mask, self.typos).map(|(score, bits, ts)| Row { id, score, bits, ts })
    }

    fn full_names(&self) -> Result<Vec<Row>, Cancelled> {
        let masks = self.idx.name_mask();
        let u = self.idx.u;
        let chunks = u.div_ceil(NAME_CHUNK);
        // Loosest mask requirement over all tokens: (need, typos).
        let pre: Vec<(u64, u8)> = self.toks.iter().map(|t| (t.pat.mask, self.k(t))).collect();
        let parts: Vec<Vec<Row>> = (0..chunks)
            .into_par_iter()
            .map_init(
                || vec![0u32; NAME_CHUNK],
                |cand, c| {
                    if self.cancel.is_cancelled() {
                        return Vec::new();
                    }
                    let lo = c * NAME_CHUNK;
                    let hi = (lo + NAME_CHUNK).min(u);
                    // Branchless gather of mask survivors (the mask test is
                    // ~50% unpredictable; branching on it costs more than it).
                    let mut n = 0usize;
                    match pre.as_slice() {
                        [] => {
                            for id in lo..hi {
                                cand[n] = id as u32;
                                n += 1;
                            }
                        }
                        [(need, 0)] => {
                            for (i, &m) in masks[lo..hi].iter().enumerate() {
                                cand[n] = (lo + i) as u32;
                                n += may_match(*need, 0, m) as usize;
                            }
                        }
                        [(need, k)] if self.toks[0].mode == crate::query::Mode::Fuzzy => {
                            // A fuzzy name with typos survives cleanly (every
                            // class present) or as a typo candidate, which
                            // needs its bigram signature to pass too.
                            let pat = &self.toks[0].pat;
                            let sigs = self.idx.name_sig();
                            for (i, (&m, &sg)) in masks[lo..hi].iter().zip(&sigs[lo..hi]).enumerate() {
                                cand[n] = (lo + i) as u32;
                                n += (may_match(*need, 0, m) | (may_match(*need, *k, m) && pat.sig_ok(sg))) as usize;
                            }
                        }
                        [(need, k)] => {
                            for (i, &m) in masks[lo..hi].iter().enumerate() {
                                cand[n] = (lo + i) as u32;
                                n += may_match(*need, *k, m) as usize;
                            }
                        }
                        many => {
                            for (i, &m) in masks[lo..hi].iter().enumerate() {
                                cand[n] = (lo + i) as u32;
                                n += many.iter().any(|&(need, k)| may_match(need, k, m)) as usize;
                            }
                        }
                    }
                    let mut out = Vec::new();
                    for &id in &cand[..n] {
                        if let Some(r) = self.score_name_unmasked(id) {
                            out.push(r);
                        }
                    }
                    out
                },
            )
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
    fn entry_ok(&self, e: usize, kinds: &[u8], sizes: &[u32], mtimes: &[u32]) -> bool {
        let q = self.q;
        if let Some(d) = self.dead
            && (d[e >> 6] >> (e & 63)) & 1 != 0
        {
            return false;
        }
        let (sz, mt) = (dec_size(sizes[e]), mtimes[e] as i64);
        (1u8 << (kinds[e] & KIND_MASK)) & q.kinds != 0 && sz >= q.size.0 && sz <= q.size.1 && mt >= q.mtime.0 && mt <= q.mtime.1
    }

    /// Expand name rows to entries and keep the top `limit`.
    fn entries(&self, rows: &[Row], range: (u32, u32), anc: Option<&Anc>) -> Result<(Vec<Hit>, u64), Cancelled> {
        let idx = self.idx;
        let (kinds, sizes, mtimes, prior, parent) = (idx.ent_kind(), idx.ent_size_raw(), idx.ent_mtime_raw(), idx.ent_prior(), idx.ent_parent());
        let ntok = self.toks.len().min(MAX_TOKENS);
        let all: u8 = if ntok == 0 { 0 } else { ((1u16 << ntok) - 1) as u8 };
        let full_range = range == (0, idx.n as u32);
        let limit = self.q.limit.max(1);
        // Chunk rows so each work item expands roughly ENTRY_CHUNK entries.
        let mut bounds = vec![0usize];
        let mut acc = 0usize;
        for (i, r) in rows.iter().enumerate() {
            acc += idx.count_of(r.id);
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
                            if p == NONE {
                                if r.bits != all {
                                    continue;
                                }
                            } else {
                                let p = p as usize;
                                let Some(s) = combine(r, all, ntok, anc.bits[p], &anc.score[p * anc.nt..p * anc.nt + anc.nt]) else {
                                    continue;
                                };
                                score = s;
                            }
                        } else if r.bits != all {
                            continue;
                        }
                        tk.push(Hit {
                            entry: e,
                            score: score + prior[eu] as i32,
                        });
                    }
                }
                tk
            })
            .reduce(|| TopK::new(limit), TopK::merge);
        self.check()?;
        Ok(top.finish())
    }

    fn no_entry_filters(&self) -> bool {
        let q = self.q;
        self.dead.is_none() && q.kinds == crate::query::KINDS_ALL && q.size == (0, u64::MAX) && q.mtime == (i64::MIN, i64::MAX)
    }

    fn count_in_range(&self, id: u32, range: (u32, u32)) -> u64 {
        if range == (0, self.idx.n as u32) {
            return self.idx.count_of(id) as u64;
        }
        let ents = self.idx.entries_of(id);
        if range == (0, self.idx.n as u32) {
            return ents.len() as u64;
        }
        (ents.partition_point(|&e| e < range.1) - ents.partition_point(|&e| e < range.0)) as u64
    }

    /// Single-token rows sorted by score (desc): expand sequentially and stop
    /// as soon as the best remaining name cannot reach the current k-th hit.
    /// Returns (hits, entries seen, stopped early).
    fn entries_ranked(&self, rows: &[Row], range: (u32, u32), limit: usize) -> Result<(Vec<Hit>, u64, bool), Cancelled> {
        let idx = self.idx;
        let (kinds, sizes, mtimes, prior) = (idx.ent_kind(), idx.ent_size_raw(), idx.ent_mtime_raw(), idx.ent_prior());
        let full_range = range == (0, idx.n as u32);
        let mut tk = TopK::new(limit);
        // Wave 1 (sequential): best rows until the page is full.
        let mut i = 0;
        while i < rows.len() && tk.buf.len() < limit {
            let r = &rows[i];
            let mut ents = idx.entries_of(r.id);
            if !full_range {
                let a = ents.partition_point(|&e| e < range.0);
                let b = ents.partition_point(|&e| e < range.1);
                ents = &ents[a..b];
            }
            for &e in ents {
                let eu = e as usize;
                if self.entry_ok(eu, kinds, sizes, mtimes) {
                    tk.push(Hit {
                        entry: e,
                        score: r.score + prior[eu] as i32,
                    });
                }
            }
            i += 1;
        }
        if i == rows.len() {
            let (h, t) = tk.finish();
            return Ok((h, t, false));
        }
        tk.trim();
        let kth = tk.buf.iter().map(|h| h.score).min().unwrap_or(i32::MIN);
        // Rows that could still reach the page: a contiguous run (sorted desc).
        let j = i + rows[i..].partition_point(|r| r.score + MAX_PRIOR >= kth);
        // Wave 2 (parallel) over that run.
        let (more, seen) = self.entries(&rows[i..j], range, None)?;
        let total = tk.total + seen;
        let mut buf = tk.buf;
        buf.extend(more);
        buf.sort_unstable_by(better);
        buf.truncate(limit);
        Ok((buf, total, j < rows.len()))
    }

    /// Per directory id: which tokens some ancestor folder (or the folder
    /// itself) matches, and the best decayed score per token.
    fn ancestors(&self, rows: &[Row]) -> Anc {
        let idx = self.idx;
        let nt = self.toks.len().min(TS);
        thread_local! {
            static SLOT: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        SLOT.with_borrow_mut(|slot| {
            if slot.len() < idx.u {
                slot.resize(idx.u, u32::MAX);
            }
            for (i, r) in rows.iter().enumerate() {
                slot[r.id as usize] = i as u32;
            }
            let (dir_entry, parent, ent_name) = (idx.dir_entry(), idx.ent_parent(), idx.ent_name());
            let mut bits = vec![0u8; idx.d];
            let mut score = vec![NO; idx.d * nt];
            // Dir ids follow DFS order: a parent's id is always smaller. The
            // root's own name never counts.
            for did in 1..idx.d {
                let e = dir_entry[did] as usize;
                let p = parent[e];
                let (up_bits, up) = if p == NONE || p == 0 {
                    (0, None)
                } else {
                    (bits[p as usize], Some(p as usize * nt))
                };
                let row = slot[ent_name[e] as usize];
                let own = (row != u32::MAX).then(|| &rows[row as usize]);
                bits[did] = up_bits | own.map_or(0, |r| r.bits);
                for t in 0..nt {
                    let a = up.map_or(NO, |u| score[u + t]);
                    let a = if a == NO { NO } else { a - ANC_DECAY as i16 };
                    let o = own.map_or(NO, |r| r.ts[t]);
                    score[did * nt + t] = a.max(o);
                }
            }
            for r in rows {
                slot[r.id as usize] = u32::MAX;
            }
            Anc { bits, score, nt }
        })
    }

    fn entry_scan(&self, range: (u32, u32)) -> Result<(Vec<Hit>, u64), Cancelled> {
        let idx = self.idx;
        let (kinds, sizes, mtimes, prior) = (idx.ent_kind(), idx.ent_size_raw(), idx.ent_mtime_raw(), idx.ent_prior());
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
                        tk.push(Hit {
                            entry: e as u32,
                            score: prior[e] as i32,
                        });
                    }
                    tk
                },
            )
            .reduce(|| TopK::new(limit), TopK::merge);
        self.check()?;
        Ok(top.finish())
    }
}

pub(crate) struct Anc {
    bits: Vec<u8>,
    score: Vec<i16>,
    nt: usize,
}

/// Score each positive token against one name: (sum, matched bits,
/// per-token scores for the first [`TS`] tokens).
#[inline]
pub(crate) fn score_tokens(toks: &[Token], lower: &[u8], orig: &[u8], sig: crate::matcher::Sig, mask: u64, typos: bool) -> Option<(i32, u8, [i16; TS])> {
    let mut score = 0;
    let mut bits = 0u8;
    let mut ts = [NO; TS];
    for (i, t) in toks.iter().enumerate().take(MAX_TOKENS) {
        let k = if typos { t.pat.typos } else { 0 };
        if toks.len() > 1 && !may_match(t.pat.mask, k, mask) {
            continue;
        }
        if let Some(s) = t.score_m(lower, orig, sig, mask, typos) {
            score += s;
            bits |= 1 << i;
            if i < TS {
                ts[i] = s as i16;
            }
        }
    }
    (bits != 0).then_some((score, bits, ts))
}

/// Combined score of a multi-token row given its parent folder's ancestor
/// bits/scores: each token counts the better of the name's own match and the
/// (scaled) nearest-folder match; `None` if some token is matched by neither.
#[inline]
pub(crate) fn combine(r: &Row, all: u8, ntok: usize, anc_bits: u8, anc: &[i16]) -> Option<i32> {
    let missing = all & !r.bits;
    if missing & !anc_bits != 0 {
        return None;
    }
    if ntok > TS {
        // Beyond TS tokens only the name's total is known: add the folder
        // credit for the missing ones (no per-token max).
        let mut s = r.score;
        for (t, &a) in anc.iter().enumerate() {
            if missing & (1 << t) != 0 {
                s += a as i32 * ANC_NUM / ANC_DEN;
            }
        }
        let rest = missing >> TS;
        return Some(s + rest.count_ones() as i32 * 200);
    }
    let mut s = 0;
    for t in 0..ntok {
        let own = r.ts[t];
        let a = anc[t];
        let a = if a == NO { NO as i32 } else { a as i32 * ANC_NUM / ANC_DEN };
        s += (own as i32).max(a);
    }
    Some(s)
}

fn has_ext(lower: &[u8], exts: &[Vec<u8>]) -> bool {
    exts.iter()
        .any(|e| lower.len() > e.len() && lower.ends_with(e) && lower[lower.len() - e.len() - 1] == b'.')
}

/// Rows by score descending, then name id: counting sort (scores in the
/// prefix bands take few distinct values).
fn sort_rows_desc(mut rows: Vec<Row>) -> Vec<Row> {
    let (Some(lo), Some(hi)) = (rows.iter().map(|r| r.score).min(), rows.iter().map(|r| r.score).max()) else {
        return rows;
    };
    let span = (hi - lo) as usize + 1;
    if span > 4096 || rows.len() < 64 {
        rows.sort_unstable_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(&b.id)));
        return rows;
    }
    let mut count = vec![0usize; span + 1];
    for r in &rows {
        count[(hi - r.score) as usize + 1] += 1;
    }
    for i in 0..span {
        count[i + 1] += count[i];
    }
    let mut out = vec![
        Row {
            id: 0,
            score: 0,
            bits: 0,
            ts: [NO; TS]
        };
        rows.len()
    ];
    // Stable: equal scores keep name order (ties are broken per entry later).
    for r in &rows {
        let slot = &mut count[(hi - r.score) as usize];
        out[*slot] = *r;
        *slot += 1;
    }
    rows.clear();
    out
}

/// Contiguous range of name ids (names are numbered in sorted order) whose
/// names start with `t`.
pub fn prefix_range(idx: &Index, t: &[u8]) -> std::ops::Range<usize> {
    let pp = |lo: usize, hi: usize, f: &dyn Fn(&[u8]) -> bool| {
        let (mut lo, mut hi) = (lo, hi);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if f(idx.lower(mid as u32)) { lo = mid + 1 } else { hi = mid }
        }
        lo
    };
    let lo = pp(0, idx.u, &|n| n < t);
    let hi = pp(lo, idx.u, &|n| n.starts_with(t));
    lo..hi
}

pub(crate) fn run(
    idx: &Index,
    q: &Query,
    mut cache: Option<&mut Vec<Survivors>>,
    cancel: &CancelToken,
    shortcuts: bool,
    dead: Option<&[u64]>,
) -> Result<SearchResult, Cancelled> {
    if cancel.is_cancelled() {
        return Err(Cancelled);
    }
    let range = match &q.scope {
        None => (0u32, idx.n as u32),
        Some(p) => match idx.lookup(p) {
            Some(e) => (e + 1, idx.end(e)),
            None => return Ok(SearchResult::empty(Strategy::Full)),
        },
    };
    let toks = &q.tokens[..q.tokens.len().min(MAX_TOKENS)];
    let negs = q.negs.iter().map(|n| memchr::memmem::Finder::new(n)).collect();
    let mut ctx = Ctx {
        idx,
        dead,
        q,
        toks,
        negs,
        cancel,
        typos: true,
    };
    if q.is_empty_name_query() {
        let t = std::time::Instant::now();
        let (hits, total) = ctx.entry_scan(range)?;
        return Ok(SearchResult {
            hits,
            total,
            total_exact: true,
            strategy: Strategy::EntryScan,
            names_scored: 0,
            names_matched: 0,
            name_us: 0,
            entry_us: t.elapsed().as_micros() as u32,
        });
    }
    // --- Prefix-range shortcut: names starting with the token are a
    // contiguous slice of `name_sorted`; score them, expand in score order and
    // stop once no remaining name can enter the page. If the page's k-th hit
    // beats every possible non-prefix match, it is the exact answer.
    if shortcuts && toks.len() == 1 && toks[0].mode != Mode::Suffix && toks[0].mode != Mode::Substring {
        let t = &toks[0];
        let complete = matches!(t.mode, Mode::Prefix | Mode::Exact);
        let t0 = std::time::Instant::now();
        let r = prefix_range(idx, &t.pat.text);
        let nids = r.len();
        let rows: Vec<Row> = if q.exts.is_empty() && q.negs.is_empty() {
            // Fast path: every name in the range is an exact/stem/prefix
            // match whose score depends only on its length and whether the
            // rest is a single extension. Ids in the range are contiguous, so
            // offsets and bytes are read sequentially.
            let (off, bytes) = (idx.name_off(), idx.name_lower());
            let m = t.pat.text.len();
            let exact_only = t.mode == Mode::Exact;
            let mut rows = Vec::with_capacity(nids);
            for id in r.clone() {
                let (a, b) = (off[id] as usize, off[id + 1] as usize);
                let n = b - a;
                let score = if n == m {
                    crate::matcher::EXACT
                } else if exact_only {
                    continue;
                } else if t.mode == Mode::Fuzzy && bytes[a + m] == b'.' && !bytes[a + m + 1..b].contains(&b'.') {
                    crate::matcher::STEM - ((n - m - 1) as i32).min(10)
                } else {
                    crate::matcher::PREFIX - crate::matcher::prefix_pen(n - m)
                };
                let mut ts = [NO; TS];
                ts[0] = score as i16;
                rows.push(Row {
                    id: id as u32,
                    score,
                    bits: 1,
                    ts,
                });
            }
            rows
        } else {
            let masks = idx.name_mask();
            r.clone().filter_map(|id| ctx.score_name(id as u32, masks[id])).collect()
        };
        let rows = sort_rows_desc(rows);
        let name_us = t0.elapsed().as_micros() as u32;
        let t1 = std::time::Instant::now();
        let limit = q.limit.max(1);
        let (hits, total, stopped) = ctx.entries_ranked(&rows, range, limit)?;
        let proven = hits.len() >= limit && hits[limit - 1].score > MAX_NON_PREFIX + MAX_PRIOR;
        if complete || proven {
            let mut total = total;
            // `^prefix` / `=exact`: the range is the whole answer, so count it.
            if complete && stopped {
                total = if ctx.no_entry_filters() {
                    rows.iter().map(|r| ctx.count_in_range(r.id, range)).sum()
                } else {
                    ctx.entries(&rows, range, None)?.1
                };
            }
            return Ok(SearchResult {
                hits,
                total,
                total_exact: complete,
                strategy: Strategy::PrefixRange,
                names_scored: nids,
                names_matched: rows.len(),
                name_us,
                entry_us: t1.elapsed().as_micros() as u32,
            });
        }
    }
    // --- Clean-first gate: a single 5–8 byte fuzzy token forgives one typo,
    // but typo matches score at most TYPO_MAX (+ prior). If the clean pass
    // alone already fills the page above that, typo matching cannot change
    // the page, so it is skipped (total becomes a lower bound).
    if shortcuts && toks.len() == 1 && toks[0].mode == Mode::Fuzzy && toks[0].pat.typos == 1 {
        let t0 = std::time::Instant::now();
        ctx.typos = false;
        let mut key = NameKey::of(q);
        key.toks[0].2 = 0;
        let (rows, strategy, scored) = name_phase(&ctx, key, cache.as_deref_mut())?;
        let name_us = t0.elapsed().as_micros() as u32;
        let t1 = std::time::Instant::now();
        let (hits, total) = ctx.entries(&rows, range, None)?;
        let limit = q.limit.max(1);
        if hits.len() >= limit && hits[limit - 1].score > TYPO_MAX + MAX_PRIOR {
            return Ok(SearchResult {
                hits,
                total,
                total_exact: false,
                strategy,
                names_scored: scored,
                names_matched: rows.len(),
                name_us,
                entry_us: t1.elapsed().as_micros() as u32,
            });
        }
        ctx.typos = true;
    }
    // --- Name phase: cached, narrowed or full.
    let t0 = std::time::Instant::now();
    let key = NameKey::of(q);
    let (rows, strategy, scored) = name_phase(&ctx, key, cache)?;
    let name_us = t0.elapsed().as_micros() as u32;
    let t1 = std::time::Instant::now();
    let anc = (toks.len() > 1).then(|| ctx.ancestors(&rows));
    let pruned: Option<Vec<Row>> = anc.as_ref().map(|a| {
        // A token no folder name matches must be matched by the name itself.
        let any_dir = a.bits.iter().fold(0u8, |m, &b| m | b);
        let all: u8 = ((1u16 << toks.len()) - 1) as u8;
        rows.iter().copied().filter(|r| (all & !r.bits) & !any_dir == 0).collect()
    });
    let (hits, total) = ctx.entries(pruned.as_deref().unwrap_or(&rows), range, anc.as_ref())?;
    Ok(SearchResult {
        hits,
        total,
        total_exact: true,
        strategy,
        names_scored: scored,
        names_matched: rows.len(),
        name_us,
        entry_us: t1.elapsed().as_micros() as u32,
    })
}

fn name_phase(ctx: &Ctx, key: NameKey, cache: Option<&mut Vec<Survivors>>) -> Result<(Arc<Vec<Row>>, Strategy, usize), Cancelled> {
    let idx = ctx.idx;
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
    if let Some(c) = cache
        && strategy != Strategy::Cached
    {
        c.retain(|s| s.key != key);
        c.push(Survivors { key, rows: rows.clone() });
        if c.len() > CACHE {
            c.remove(0);
        }
    }
    Ok((rows, strategy, scored))
}

/// One-shot search (no narrowing cache).
pub fn search(idx: &Index, q: &Query, cancel: &CancelToken) -> Result<SearchResult, Cancelled> {
    run(idx, q, None, cancel, true, None)
}

/// Full scan with every shortcut disabled (reference for tests/benches).
#[doc(hidden)]
pub fn search_reference(idx: &Index, q: &Query, cancel: &CancelToken) -> Result<SearchResult, Cancelled> {
    run(idx, q, None, cancel, false, None)
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
        run(&idx, q, Some(&mut self.cache), cancel, true, None)
    }
    /// Like [`Session::search`], but entries whose bit is set in `excluded`
    /// (one bit per entry) are skipped. The narrowing cache stays valid
    /// across different masks (it only holds name-level results).
    pub fn search_masked(&mut self, q: &Query, cancel: &CancelToken, excluded: Option<&[u64]>) -> Result<SearchResult, Cancelled> {
        let idx = self.idx.clone();
        if let Some(m) = excluded {
            assert!(m.len() * 64 >= idx.n, "mask too short");
        }
        run(&idx, q, Some(&mut self.cache), cancel, true, excluded)
    }
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}
