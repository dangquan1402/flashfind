//! Immutable name index.
//!
//! Layout: struct-of-arrays in one 8-byte-aligned buffer, so the same
//! accessors work on a freshly built index (owned `Vec<u64>`) and on a
//! saved one (`mmap`, zero-copy load).
//!
//! Entries are in depth-first pre-order, so every folder's subtree is the
//! contiguous range `e .. end[e]`: `in:` scoping is a range check and
//! ancestor walks never need a hash map. Names are interned (one copy per
//! distinct name) with a 64-bit character-class mask, and an inverted
//! name → entries list lets a query score each distinct name once.
//!
//! On-disk format `FFIDX004` (native endian, little-endian on every
//! supported target): 8-byte magic, `u64` header words, then sections.

use crate::matcher::{bigram_sig, class_mask, fold};
use crate::walk::{FLAG_HIDDEN, KIND_DIR, KIND_MASK, Listing, NONE};
use rayon::prelude::*;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::Path;

const MAGIC: &[u8; 8] = b"FFIDX005";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum S {
    NameLower,
    NameOff,
    NameMask,
    NameSig,
    NameEnt,
    NameMulti,
    UpperBits,
    UpperRank,
    OrigOff,
    NameOrig,
    EntName,
    EntParent,
    EntKind,
    EntPrior,
    EntSize,
    EntMtime,
    DirEntry,
    DirEnd,
    Root,
}
const NSECT: usize = 19;
/// Element size of each section (validation on load).
const SIZES: [usize; NSECT] = [1, 4, 8, 8, 4, 4, 8, 4, 4, 1, 4, 4, 1, 1, 4, 4, 4, 4, 1];
/// Header: magic + [n, u, d, created, event_id, reserved] + NSECT × (offset, len) in u64 words.
const HDR_WORDS: usize = 1 + 6 + NSECT * 2;
/// `name_ent` tag: the low 31 bits index `name_multi` instead of being the entry.
const MULTI: u32 = 1 << 31;

/// Sizes are stored in 32 bits: exact below 2 GiB, MiB granularity above
/// (up to 2 PiB). Size filters on files over 2 GiB are therefore rounded
/// down to the MiB.
#[inline(always)]
pub fn enc_size(s: u64) -> u32 {
    if s < 1 << 31 {
        s as u32
    } else {
        (1 << 31) | (s >> 20).min((1 << 31) - 1) as u32
    }
}
#[inline(always)]
pub fn dec_size(v: u32) -> u64 {
    if v >> 31 == 0 { v as u64 } else { ((v & 0x7fff_ffff) as u64) << 20 }
}
/// Mtimes are stored as unsigned 32-bit Unix seconds (1970 – 2106);
/// pre-1970 times clamp to 0. Query arithmetic stays 64-bit.
#[inline(always)]
pub fn enc_mtime(t: i64) -> u32 {
    t.clamp(0, u32::MAX as i64) as u32
}

enum Buf {
    Owned(Vec<u64>),
    Mapped(memmap2::Mmap),
}

impl Buf {
    fn bytes(&self) -> &[u8] {
        match self {
            Buf::Owned(v) => unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) },
            Buf::Mapped(m) => m,
        }
    }
}

pub struct Index {
    buf: Buf,
    sect: [(usize, usize); NSECT],
    /// Entries, distinct names, directories.
    pub n: usize,
    pub u: usize,
    pub d: usize,
    /// Unix time the crawl started.
    pub created: i64,
    /// FSEvents event id (macOS) the index is current up to; 0 = unknown.
    pub event_id: u64,
}

/// Plain-old-data element types allowed in sections.
pub trait Pod: Copy {}
impl Pod for u8 {}
impl Pod for u16 {}
impl Pod for i8 {}
impl Pod for u32 {}
impl Pod for u64 {}
impl Pod for i64 {}

/// Counters from a build.
#[derive(Clone, Debug, Default)]
pub struct BuildStats {
    pub entries: usize,
    pub names: usize,
    pub dirs: usize,
    pub bytes: usize,
}

#[derive(Default)]
pub struct Fx(u64);
impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        let mut c = bytes.chunks_exact(8);
        for w in &mut c {
            h = (h.rotate_left(5) ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
        for &b in c.remainder() {
            h = (h.rotate_left(5) ^ b as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
        self.0 = h;
    }
}

/// Folder names whose subtrees are mostly noise for a person searching.
const NOISY: &[&[u8]] = &[
    b"node_modules",
    b".git",
    b".svn",
    b".hg",
    b"target",
    b"deriveddata",
    b"caches",
    b".cache",
    b"__pycache__",
    b".npm",
    b".cargo",
    b".rustup",
    b"site-packages",
    b".gradle",
    b".m2",
    b"pods",
    b".venv",
    b"venv",
    b".tox",
    b"build",
    b"dist",
    b".next",
    b"vendor",
    b"bower_components",
    b".terraform",
];
/// Top-level system folders when indexing `/`.
const SYSTEM: &[&[u8]] = &[b"system", b"private", b"usr", b"bin", b"sbin", b"opt", b"cores", b"dev", b"library", b"volumes"];

/// Static location prior, in [-50, 40] so it never crosses a score band.
fn own_penalty(folded: &[u8], kind: u8, depth: u32, root_is_slash: bool) -> (i32, i32) {
    // (prior for the entry itself, penalty inherited by its subtree)
    let hidden = folded.first() == Some(&b'.') || kind & FLAG_HIDDEN != 0;
    let mut own = 0;
    let mut inherit = 0;
    if hidden {
        own -= 15;
        inherit -= 15;
    }
    if kind & KIND_MASK == KIND_DIR {
        if NOISY.contains(&folded) {
            inherit -= 30;
        }
        if folded.ends_with(b".app") {
            own += 25;
            inherit -= 30;
        }
        if root_is_slash && depth == 1 && SYSTEM.contains(&folded) {
            inherit -= 20;
        }
    }
    (own, inherit)
}

/// Prior of one entry and the penalty its subtree inherits, from its raw
/// name, kind, depth (1 = child of the root) and the inherited penalty.
/// Shared by the index builder and the live overlay.
pub(crate) fn entry_prior(name: &[u8], kind: u8, depth: u32, inherited: i32, root_is_slash: bool) -> (i8, i32) {
    let (own, sub) = if kind & KIND_MASK == KIND_DIR {
        let folded: Vec<u8> = name.iter().map(|&b| fold(b)).collect();
        own_penalty(&folded, kind, depth, root_is_slash)
    } else {
        // Files: only the hidden test applies (no allocation on the hot path).
        own_penalty(&name[..name.len().min(1)], kind, depth, root_is_slash)
    };
    let prior = (own + inherited - (depth.saturating_sub(1) as i32).min(15)).clamp(-50, 40);
    (prior as i8, (inherited + sub).max(-60))
}

struct Builder {
    words: Vec<u64>,
    sect: [(usize, usize); NSECT],
}

impl Builder {
    fn put<T: Pod>(&mut self, s: S, data: &[T]) {
        let bytes = std::mem::size_of_val(data);
        let off = self.words.len() * 8;
        let add = bytes.div_ceil(8);
        self.words.resize(self.words.len() + add, 0);
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr() as *const u8, (self.words.as_mut_ptr() as *mut u8).add(off), bytes) };
        self.sect[s as usize] = (off, data.len());
    }
}

impl Index {
    #[inline(always)]
    fn get<T: Pod>(&self, s: S) -> &[T] {
        let (off, len) = self.sect[s as usize];
        let b = self.buf.bytes();
        debug_assert!(off + len * std::mem::size_of::<T>() <= b.len());
        unsafe { std::slice::from_raw_parts(b.as_ptr().add(off) as *const T, len) }
    }

    /// Folded (ASCII-lowercase) name bytes; names are numbered in sorted
    /// folded order, so this arena is itself sorted.
    pub fn name_lower(&self) -> &[u8] {
        self.get(S::NameLower)
    }
    pub fn name_off(&self) -> &[u32] {
        self.get(S::NameOff)
    }
    pub fn name_mask(&self) -> &[u64] {
        self.get(S::NameMask)
    }
    /// Hashed bigram signature per name (typo prefilter).
    pub fn name_sig(&self) -> &[u64] {
        self.get(S::NameSig)
    }
    #[inline(always)]
    pub fn sig(&self, id: u32) -> crate::matcher::Sig {
        self.name_sig()[id as usize]
    }
    pub fn ent_name(&self) -> &[u32] {
        self.get(S::EntName)
    }
    /// Parent directory id ([`NONE`] for the root).
    pub fn ent_parent(&self) -> &[u32] {
        self.get(S::EntParent)
    }
    pub fn ent_kind(&self) -> &[u8] {
        self.get(S::EntKind)
    }
    pub fn ent_prior(&self) -> &[i8] {
        self.get(S::EntPrior)
    }
    /// Encoded sizes (see [`dec_size`]).
    pub fn ent_size_raw(&self) -> &[u32] {
        self.get(S::EntSize)
    }
    /// Mtimes as u32 Unix seconds (see [`enc_mtime`]).
    pub fn ent_mtime_raw(&self) -> &[u32] {
        self.get(S::EntMtime)
    }
    #[inline]
    pub fn size(&self, e: u32) -> u64 {
        dec_size(self.ent_size_raw()[e as usize])
    }
    #[inline]
    pub fn mtime(&self, e: u32) -> i64 {
        self.ent_mtime_raw()[e as usize] as i64
    }
    /// Directory id → entry (ascending: directories are numbered in DFS order).
    pub fn dir_entry(&self) -> &[u32] {
        self.get(S::DirEntry)
    }
    /// Directory id → exclusive end of its subtree.
    pub fn dir_end(&self) -> &[u32] {
        self.get(S::DirEnd)
    }
    pub fn root(&self) -> &[u8] {
        self.get(S::Root)
    }

    /// Directory id of entry `e`, if it is a directory.
    #[inline]
    pub fn dir_id(&self, e: u32) -> Option<u32> {
        if self.ent_kind()[e as usize] & KIND_MASK != KIND_DIR {
            return None;
        }
        let de = self.dir_entry();
        let i = de.partition_point(|&x| x < e);
        (i < de.len() && de[i] == e).then_some(i as u32)
    }

    /// Exclusive end of entry `e`'s subtree (`e + 1` for non-directories).
    #[inline]
    pub fn end(&self, e: u32) -> u32 {
        match self.dir_id(e) {
            Some(d) => self.dir_end()[d as usize],
            None => e + 1,
        }
    }

    /// Folded bytes of name id `id`.
    #[inline(always)]
    pub fn lower(&self, id: u32) -> &[u8] {
        let o = self.name_off();
        &self.name_lower()[o[id as usize] as usize..o[id as usize + 1] as usize]
    }
    /// Original-case bytes of name id `id`. Only names containing an ASCII
    /// capital store a second copy; the rest are their folded form.
    #[inline(always)]
    pub fn orig(&self, id: u32) -> &[u8] {
        let bits: &[u64] = self.get(S::UpperBits);
        let (w, b) = ((id >> 6) as usize, 1u64 << (id & 63));
        if bits[w] & b == 0 {
            return self.lower(id);
        }
        let rank: &[u32] = self.get(S::UpperRank);
        let r = (rank[w] + (bits[w] & (b - 1)).count_ones()) as usize;
        let o: &[u32] = self.get(S::OrigOff);
        &self.get::<u8>(S::NameOrig)[o[r] as usize..o[r + 1] as usize]
    }
    /// Entries carrying name id `id`, ascending.
    #[inline(always)]
    pub fn entries_of(&self, id: u32) -> &[u32] {
        let v = &self.get::<u32>(S::NameEnt)[id as usize];
        if *v & MULTI == 0 {
            return std::slice::from_ref(v);
        }
        let m: &[u32] = self.get(S::NameMulti);
        let o = (*v & !MULTI) as usize;
        &m[o + 1..o + 1 + m[o] as usize]
    }
    /// Number of entries carrying name id `id`.
    #[inline(always)]
    pub fn count_of(&self, id: u32) -> usize {
        let v = self.get::<u32>(S::NameEnt)[id as usize];
        if v & MULTI == 0 {
            1
        } else {
            self.get::<u32>(S::NameMulti)[(v & !MULTI) as usize] as usize
        }
    }

    pub fn bytes(&self) -> usize {
        self.buf.bytes().len()
    }

    /// Bytes per section (for size reports).
    pub fn section_sizes(&self) -> Vec<(&'static str, usize)> {
        const NAMES: [&str; NSECT] = [
            "name_lower",
            "name_off",
            "name_mask",
            "name_sig",
            "name_ent",
            "name_multi",
            "upper_bits",
            "upper_rank",
            "orig_off",
            "name_orig",
            "ent_name",
            "ent_parent",
            "ent_kind",
            "ent_prior",
            "ent_size",
            "ent_mtime",
            "dir_entry",
            "dir_end",
            "root",
        ];
        (0..NSECT).map(|i| (NAMES[i], self.sect[i].1 * SIZES[i])).collect()
    }

    /// Absolute path of entry `e` (no depth limit – fsearch truncated at 256
    /// levels).
    pub fn path(&self, e: u32, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(self.root());
        let mut chain = Vec::new();
        let (parent, dir_entry, ent_name) = (self.ent_parent(), self.dir_entry(), self.ent_name());
        let mut k = e;
        while k != 0 {
            chain.push(k);
            let p = parent[k as usize];
            if p == NONE {
                break;
            }
            k = dir_entry[p as usize];
        }
        for &c in chain.iter().rev() {
            if !out.ends_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(self.orig(ent_name[c as usize]));
        }
    }

    pub fn path_string(&self, e: u32) -> String {
        let mut v = Vec::new();
        self.path(e, &mut v);
        String::from_utf8_lossy(&v).into_owned()
    }

    /// Path relative to the root (no leading slash).
    pub fn rel_path(&self, e: u32) -> String {
        let full = self.path_string(e);
        let root = String::from_utf8_lossy(self.root()).into_owned();
        full.strip_prefix(&root).unwrap_or(&full).trim_start_matches('/').to_string()
    }

    pub fn depth(&self, e: u32) -> u32 {
        let (parent, dir_entry) = (self.ent_parent(), self.dir_entry());
        let mut d = 0;
        let mut k = e;
        while k != 0 {
            let p = parent[k as usize];
            if p == NONE {
                break;
            }
            k = dir_entry[p as usize];
            d += 1;
        }
        d
    }

    /// Direct children of entry `e`.
    pub fn children(&self, e: u32) -> impl Iterator<Item = u32> + '_ {
        let stop = self.end(e);
        let mut c = e + 1;
        std::iter::from_fn(move || {
            if c >= stop {
                return None;
            }
            let r = c;
            c = self.end(c);
            Some(r)
        })
    }

    /// Resolve an absolute path to its entry.
    pub fn lookup(&self, path: &[u8]) -> Option<u32> {
        let root = self.root();
        let rest = path.strip_prefix(root)?;
        if !(rest.is_empty() || rest[0] == b'/' || root.ends_with(b"/")) {
            return None;
        }
        let mut e = 0u32;
        for comp in rest.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
            e = self.children(e).find(|&c| self.orig(self.ent_name()[c as usize]) == comp)?;
        }
        Some(e)
    }

    /// Index of an empty root (used while the first crawl runs).
    pub fn empty(root: &Path) -> Index {
        Index::build_refs(&[], root, 0, 0)
    }

    /// Build from crawler listings (listing id 0 = `root`).
    pub fn build(listings: Vec<Listing>, root: &Path, created: i64) -> Index {
        let refs: Vec<&Listing> = listings.iter().collect();
        Index::build_refs(&refs, root, created, 0)
    }

    /// Build from borrowed listings, recording the event id the crawl
    /// started at (so a watcher can replay everything after it).
    pub fn build_refs(listings: &[&Listing], root: &Path, created: i64, event_id: u64) -> Index {
        let root_b = path_bytes(root);
        let root_is_slash = root_b == b"/";
        let mut by_id: Vec<Option<&Listing>> = Vec::new();
        for &l in listings {
            let id = l.id as usize;
            if by_id.len() <= id {
                by_id.resize(id + 1, None);
            }
            by_id[id] = Some(l);
        }
        // --- DFS pre-order, interning names on the way.
        let total: usize = 1 + by_id.iter().flatten().map(|l| l.ents.len()).sum::<usize>();
        let mut ent_name: Vec<u32> = Vec::with_capacity(total);
        let mut ent_parent: Vec<u32> = Vec::with_capacity(total);
        let mut ent_kind: Vec<u8> = Vec::with_capacity(total);
        let mut ent_prior: Vec<i8> = Vec::with_capacity(total);
        let mut ent_size: Vec<u32> = Vec::with_capacity(total);
        let mut ent_mtime: Vec<u32> = Vec::with_capacity(total);
        let mut dir_entry: Vec<u32> = Vec::new();
        let mut dir_end: Vec<u32> = Vec::new();
        let mut intern: HashMap<&[u8], u32, BuildHasherDefault<Fx>> = HashMap::with_capacity_and_hasher(total, Default::default());
        let mut names: Vec<&[u8]> = Vec::new();
        let root_name: Vec<u8> = root.file_name().map(|n| path_bytes(Path::new(n))).unwrap_or_else(|| b"/".to_vec());
        // Names borrow from the listings (and `root_name`), which outlive
        // every use below.
        let mut id_of = |name: &[u8]| -> u32 {
            // SAFETY: see above; the map and vector are dropped before return.
            let name: &'static [u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(name) };
            *intern.entry(name).or_insert_with(|| {
                names.push(name);
                (names.len() - 1) as u32
            })
        };
        ent_name.push(id_of(&root_name));
        ent_parent.push(NONE);
        ent_kind.push(KIND_DIR);
        ent_prior.push(0);
        ent_size.push(0);
        ent_mtime.push(0);
        dir_entry.push(0);
        dir_end.push(0);
        // Frame: (listing id, next child, dir id, inherited penalty, depth)
        let mut stack: Vec<(usize, usize, u32, i32, u32)> = Vec::new();
        if by_id.first().is_some_and(|l| l.is_some()) {
            stack.push((0, 0, 0, 0, 0));
        }
        while let Some(top) = stack.last_mut() {
            let (lid, pos, did, inh, depth) = *top;
            let l = by_id[lid].unwrap();
            if pos >= l.ents.len() {
                dir_end[did as usize] = ent_name.len() as u32;
                stack.pop();
                continue;
            }
            top.1 += 1;
            let raw = &l.ents[pos];
            let name = l.name(raw);
            let e = ent_name.len() as u32;
            ent_name.push(id_of(name));
            ent_parent.push(did);
            ent_kind.push(raw.kind);
            ent_size.push(enc_size(raw.size));
            ent_mtime.push(enc_mtime(raw.mtime));
            let (prior, sub) = entry_prior(name, raw.kind, depth + 1, inh, root_is_slash);
            ent_prior.push(prior);
            if raw.kind & KIND_MASK == KIND_DIR {
                let new_did = dir_entry.len() as u32;
                dir_entry.push(e);
                dir_end.push(e + 1);
                let child = raw.child as usize;
                if raw.child != NONE && child < by_id.len() && by_id[child].is_some() {
                    stack.push((child, 0, new_did, sub, depth + 1));
                }
            }
        }
        let n = ent_name.len();
        if dir_end[0] == 0 {
            dir_end[0] = n as u32;
        }
        let u = names.len();
        drop(intern);
        // --- Number names in sorted folded order (ties: original bytes), so
        // a prefix is a contiguous id range and `name_lower` is sorted.
        let mut orig_tmp = Vec::with_capacity(names.iter().map(|s| s.len()).sum());
        let mut off_tmp = Vec::with_capacity(u + 1);
        off_tmp.push(0usize);
        for s in &names {
            orig_tmp.extend_from_slice(s);
            off_tmp.push(orig_tmp.len());
        }
        let lower_tmp: Vec<u8> = orig_tmp.par_iter().map(|&b| fold(b)).collect();
        let lw = |i: u32| &lower_tmp[off_tmp[i as usize]..off_tmp[i as usize + 1]];
        let ow = |i: u32| &orig_tmp[off_tmp[i as usize]..off_tmp[i as usize + 1]];
        let mut perm: Vec<u32> = (0..u as u32).collect();
        perm.par_sort_unstable_by(|&a, &b| lw(a).cmp(lw(b)).then_with(|| ow(a).cmp(ow(b))));
        let mut rank = vec![0u32; u];
        for (r, &old) in perm.iter().enumerate() {
            rank[old as usize] = r as u32;
        }
        ent_name.par_iter_mut().for_each(|id| *id = rank[*id as usize]);
        drop(rank);
        let mut name_lower = Vec::with_capacity(lower_tmp.len());
        let mut name_off = Vec::with_capacity(u + 1);
        name_off.push(0u32);
        let words = u.div_ceil(64).max(1);
        let mut upper_bits = vec![0u64; words];
        let mut upper_rank = vec![0u32; words];
        let mut orig_off = vec![0u32];
        let mut name_orig = Vec::new();
        for (r, &old) in perm.iter().enumerate() {
            let (l, o) = (lw(old), ow(old));
            name_lower.extend_from_slice(l);
            name_off.push(name_lower.len() as u32);
            if l != o {
                upper_bits[r >> 6] |= 1 << (r & 63);
                name_orig.extend_from_slice(o);
                orig_off.push(name_orig.len() as u32);
            }
        }
        let mut acc = 0u32;
        for w in 0..words {
            upper_rank[w] = acc;
            acc += upper_bits[w].count_ones();
        }
        drop(names);
        drop(perm);
        drop(orig_tmp);
        drop(lower_tmp);
        let name_mask: Vec<u64> = (0..u)
            .into_par_iter()
            .map(|i| class_mask(&name_lower[name_off[i] as usize..name_off[i + 1] as usize]))
            .collect();
        let name_sig: Vec<u64> = (0..u)
            .into_par_iter()
            .map(|i| bigram_sig(&name_lower[name_off[i] as usize..name_off[i + 1] as usize]))
            .collect();
        // --- Inverted name → entries: a single entry inline, else a
        // `[count, entries…]` run in `name_multi` (ascending entries).
        let mut count = vec![0u32; u];
        for &id in &ent_name {
            count[id as usize] += 1;
        }
        let mut name_ent = vec![0u32; u];
        let mut multi_len = 0usize;
        for (id, &c) in count.iter().enumerate() {
            if c > 1 {
                name_ent[id] = MULTI | multi_len as u32;
                multi_len += 1 + c as usize;
            }
        }
        let mut name_multi = vec![0u32; multi_len];
        let mut fill = vec![0u32; u];
        for (e, &id) in ent_name.iter().enumerate() {
            let id = id as usize;
            if count[id] == 1 {
                name_ent[id] = e as u32;
            } else {
                let o = (name_ent[id] & !MULTI) as usize;
                name_multi[o] = count[id];
                name_multi[o + 1 + fill[id] as usize] = e as u32;
                fill[id] += 1;
            }
        }
        drop(count);
        drop(fill);
        let d = dir_entry.len();
        let mut b = Builder {
            words: vec![0u64; HDR_WORDS],
            sect: [(0, 0); NSECT],
        };
        b.put(S::NameLower, &name_lower);
        b.put(S::NameOff, &name_off);
        b.put(S::NameMask, &name_mask);
        b.put(S::NameSig, &name_sig);
        b.put(S::NameEnt, &name_ent);
        b.put(S::NameMulti, &name_multi);
        b.put(S::UpperBits, &upper_bits);
        b.put(S::UpperRank, &upper_rank);
        b.put(S::OrigOff, &orig_off);
        b.put(S::NameOrig, &name_orig);
        b.put(S::EntName, &ent_name);
        b.put(S::EntParent, &ent_parent);
        b.put(S::EntKind, &ent_kind);
        b.put(S::EntPrior, &ent_prior);
        b.put(S::EntSize, &ent_size);
        b.put(S::EntMtime, &ent_mtime);
        b.put(S::DirEntry, &dir_entry);
        b.put(S::DirEnd, &dir_end);
        b.put(S::Root, &root_b);
        let mut words = b.words;
        words[0] = u64::from_le_bytes(*MAGIC);
        words[1] = n as u64;
        words[2] = u as u64;
        words[3] = d as u64;
        words[4] = created as u64;
        words[5] = event_id;
        for (i, &(off, len)) in b.sect.iter().enumerate() {
            words[7 + i * 2] = off as u64;
            words[8 + i * 2] = len as u64;
        }
        Index {
            buf: Buf::Owned(words),
            sect: b.sect,
            n,
            u,
            d,
            created,
            event_id,
        }
    }

    pub fn stats(&self) -> BuildStats {
        BuildStats {
            entries: self.n,
            names: self.u,
            dirs: self.d,
            bytes: self.bytes(),
        }
    }

    /// Atomic save (temp file + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(self.buf.bytes())?;
            f.sync_data()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// Zero-copy load via `mmap`.
    pub fn load(path: &Path) -> std::io::Result<Index> {
        let f = std::fs::File::open(path)?;
        let m = unsafe { memmap2::Mmap::map(&f)? };
        let bad = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {why}", path.display()));
        if m.len() < HDR_WORDS * 8 || &m[..8] != MAGIC {
            return Err(bad("not a flashfind index (or an older format)"));
        }
        let w = |i: usize| u64::from_ne_bytes(m[i * 8..i * 8 + 8].try_into().unwrap());
        let mut sect = [(0usize, 0usize); NSECT];
        for i in 0..NSECT {
            let (off, len) = (w(7 + i * 2) as usize, w(8 + i * 2) as usize);
            if off % 8 != 0 || off.checked_add(len.saturating_mul(SIZES[i])).is_none_or(|end| end > m.len()) {
                return Err(bad("corrupt section table"));
            }
            sect[i] = (off, len);
        }
        let (n, u, d, created, event_id) = (w(1) as usize, w(2) as usize, w(3) as usize, w(4) as i64, w(5));
        let c = |s: S| sect[s as usize].1;
        if c(S::EntName) != n
            || c(S::EntSize) != n
            || c(S::NameOff) != u + 1
            || c(S::NameSig) != u
            || c(S::NameEnt) != u
            || c(S::UpperBits) != u.div_ceil(64).max(1)
            || c(S::DirEntry) != d
            || c(S::DirEnd) != d
        {
            return Err(bad("inconsistent counts"));
        }
        #[cfg(unix)]
        let _ = m.advise(memmap2::Advice::WillNeed);
        Ok(Index {
            buf: Buf::Mapped(m),
            sect,
            n,
            u,
            d,
            created,
            event_id,
        })
    }

    /// Touch every page so the first query does not pay page faults.
    pub fn prefault(&self) {
        let b = self.buf.bytes();
        let mut acc = 0u8;
        for i in (0..b.len()).step_by(4096) {
            acc ^= unsafe { std::ptr::read_volatile(b.as_ptr().add(i)) };
        }
        std::hint::black_box(acc);
    }
}

#[cfg(unix)]
pub(crate) fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}
#[cfg(not(unix))]
pub(crate) fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Crawl + build in one call.
pub fn build_from_disk(root: &Path, opts: &crate::walk::ScanOptions) -> Index {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let created = now_secs();
    let event_id = crate::watch::current_event_id();
    let ls = crate::walk::scan(&root, opts);
    let refs: Vec<&Listing> = ls.iter().collect();
    Index::build_refs(&refs, &root, created, event_id)
}
