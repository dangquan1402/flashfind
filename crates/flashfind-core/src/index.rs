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
//! On-disk format `FFIDX003` (native endian, little-endian on every
//! supported target): 8-byte magic, `u64` header words, then sections.

use crate::matcher::{bigram_sig, class_mask, fold};
use crate::walk::{FLAG_HIDDEN, KIND_DIR, KIND_MASK, Listing, NONE};
use rayon::prelude::*;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::Path;

const MAGIC: &[u8; 8] = b"FFIDX003";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum S {
    NameLower,
    NameOrig,
    NameOff,
    NameMask,
    NameSorted,
    NameEntOff,
    NameEnts,
    EntName,
    EntParent,
    EntKind,
    EntPrior,
    EntSize,
    EntMtime,
    EntEnd,
    DirEntry,
    Root,
    NameBigram,
}
const NSECT: usize = 17;
/// Header: magic + [n, u, d, created] + NSECT × (offset, len) in u64 words.
const HDR_WORDS: usize = 1 + 4 + NSECT * 2;

enum Buf {
    Owned(Vec<u64>),
    Mapped(memmap2::Mmap),
}

impl Buf {
    fn bytes(&self) -> &[u8] {
        match self {
            Buf::Owned(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8)
            },
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
    /// Unix time the crawl finished.
    pub created: i64,
}

/// Plain-old-data element types allowed in sections.
pub trait Pod: Copy {}
impl Pod for u8 {}
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
struct Fx(u64);
impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        let mut c = bytes.chunks_exact(8);
        for w in &mut c {
            h = (h.rotate_left(5) ^ u64::from_le_bytes(w.try_into().unwrap()))
                .wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
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
const SYSTEM: &[&[u8]] = &[
    b"system", b"private", b"usr", b"bin", b"sbin", b"opt", b"cores", b"dev", b"library",
    b"volumes",
];

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
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr() as *const u8,
                (self.words.as_mut_ptr() as *mut u8).add(off),
                bytes,
            )
        };
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

    pub fn name_lower(&self) -> &[u8] {
        self.get(S::NameLower)
    }
    pub fn name_orig(&self) -> &[u8] {
        self.get(S::NameOrig)
    }
    pub fn name_off(&self) -> &[u32] {
        self.get(S::NameOff)
    }
    pub fn name_mask(&self) -> &[u64] {
        self.get(S::NameMask)
    }
    /// Name ids in lexicographic (folded) order.
    /// Hashed bigram signature per name (typo prefilter).
    pub fn name_bigram(&self) -> &[u64] {
        self.get(S::NameBigram)
    }
    /// Bigram signature of name id `id`.
    #[inline(always)]
    pub fn sig(&self, id: u32) -> crate::matcher::Sig {
        let b = self.name_bigram();
        let i = id as usize * 2;
        (b[i] as u128) | ((b[i + 1] as u128) << 64)
    }
    pub fn name_sorted(&self) -> &[u32] {
        self.get(S::NameSorted)
    }
    pub fn name_ent_off(&self) -> &[u32] {
        self.get(S::NameEntOff)
    }
    pub fn name_ents(&self) -> &[u32] {
        self.get(S::NameEnts)
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
    pub fn ent_size(&self) -> &[u64] {
        self.get(S::EntSize)
    }
    pub fn ent_mtime(&self) -> &[i64] {
        self.get(S::EntMtime)
    }
    /// Exclusive end of each entry's subtree.
    pub fn ent_end(&self) -> &[u32] {
        self.get(S::EntEnd)
    }
    /// Directory id → entry.
    pub fn dir_entry(&self) -> &[u32] {
        self.get(S::DirEntry)
    }
    pub fn root(&self) -> &[u8] {
        self.get(S::Root)
    }

    /// Folded bytes of name id `id`.
    #[inline(always)]
    pub fn lower(&self, id: u32) -> &[u8] {
        let o = self.name_off();
        &self.name_lower()[o[id as usize] as usize..o[id as usize + 1] as usize]
    }
    #[inline(always)]
    pub fn orig(&self, id: u32) -> &[u8] {
        let o = self.name_off();
        &self.name_orig()[o[id as usize] as usize..o[id as usize + 1] as usize]
    }
    /// Entries carrying name id `id`, ascending.
    #[inline(always)]
    pub fn entries_of(&self, id: u32) -> &[u32] {
        let o = self.name_ent_off();
        &self.name_ents()[o[id as usize] as usize..o[id as usize + 1] as usize]
    }

    pub fn bytes(&self) -> usize {
        self.buf.bytes().len()
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
        full.strip_prefix(&root)
            .unwrap_or(&full)
            .trim_start_matches('/')
            .to_string()
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
        let end = self.ent_end();
        let stop = end[e as usize];
        let mut c = e + 1;
        std::iter::from_fn(move || {
            if c >= stop {
                return None;
            }
            let r = c;
            c = end[c as usize];
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
            e = self
                .children(e)
                .find(|&c| self.orig(self.ent_name()[c as usize]) == comp)?;
        }
        Some(e)
    }

    /// Build from crawler listings (listing id 0 = `root`).
    pub fn build(listings: Vec<Listing>, root: &Path, created: i64) -> Index {
        let root_b = path_bytes(root);
        let root_is_slash = root_b == b"/";
        let mut by_id: Vec<Option<Listing>> = Vec::new();
        for l in listings {
            let id = l.id as usize;
            if by_id.len() <= id {
                by_id.resize_with(id + 1, || None);
            }
            by_id[id] = Some(l);
        }
        // --- DFS pre-order, interning names on the way.
        let total: usize = 1 + by_id.iter().flatten().map(|l| l.ents.len()).sum::<usize>();
        let mut ent_name: Vec<u32> = Vec::with_capacity(total);
        let mut ent_parent: Vec<u32> = Vec::with_capacity(total);
        let mut ent_kind: Vec<u8> = Vec::with_capacity(total);
        let mut ent_prior: Vec<i8> = Vec::with_capacity(total);
        let mut ent_size: Vec<u64> = Vec::with_capacity(total);
        let mut ent_mtime: Vec<i64> = Vec::with_capacity(total);
        let mut ent_end: Vec<u32> = vec![0; total];
        let mut dir_entry: Vec<u32> = Vec::new();
        let mut intern: HashMap<&[u8], u32, BuildHasherDefault<Fx>> = HashMap::default();
        let mut names: Vec<&[u8]> = Vec::new();
        let mut id_of = |name: &'static [u8], names: &mut Vec<&'static [u8]>| -> u32 {
            *intern.entry(name).or_insert_with(|| {
                names.push(name);
                (names.len() - 1) as u32
            })
        };
        // SAFETY: listings live in `by_id` until the end of this function;
        // the 'static borrow never escapes it (names are copied out below).
        let st = |b: &[u8]| -> &'static [u8] {
            unsafe { std::mem::transmute::<&[u8], &'static [u8]>(b) }
        };
        let root_name: Vec<u8> = root
            .file_name()
            .map(|n| path_bytes(Path::new(n)))
            .unwrap_or_else(|| b"/".to_vec());
        let root_name_static = st(&root_name);
        ent_name.push(id_of(root_name_static, &mut names));
        ent_parent.push(NONE);
        ent_kind.push(KIND_DIR);
        ent_prior.push(0);
        ent_size.push(0);
        ent_mtime.push(0);
        dir_entry.push(0);
        // Frame: (listing id, next child, entry of this dir, dir id, inherited penalty, depth)
        let mut stack: Vec<(usize, usize, u32, u32, i32, u32)> = Vec::new();
        if by_id.first().is_some_and(|l| l.is_some()) {
            stack.push((0, 0, 0, 0, 0, 0));
        }
        while let Some(top) = stack.last_mut() {
            let (lid, pos, dent, did, inh, depth) = *top;
            let l = by_id[lid].as_ref().unwrap();
            if pos >= l.ents.len() {
                ent_end[dent as usize] = ent_name.len() as u32;
                stack.pop();
                continue;
            }
            top.1 += 1;
            let raw = &l.ents[pos];
            let name = st(l.name(raw));
            let e = ent_name.len() as u32;
            ent_name.push(id_of(name, &mut names));
            ent_parent.push(did);
            ent_kind.push(raw.kind);
            ent_size.push(raw.size);
            ent_mtime.push(raw.mtime);
            let folded: Vec<u8> = name.iter().map(|&b| fold(b)).collect();
            let (own, sub) = own_penalty(&folded, raw.kind, depth + 1, root_is_slash);
            let prior = (own + inh - (depth as i32).min(15)).clamp(-50, 40);
            ent_prior.push(prior as i8);
            if raw.kind & KIND_MASK == KIND_DIR {
                let new_did = dir_entry.len() as u32;
                dir_entry.push(e);
                let child = raw.child as usize;
                if raw.child != NONE && child < by_id.len() && by_id[child].is_some() {
                    stack.push((child, 0, e, new_did, (inh + sub).max(-60), depth + 1));
                    continue;
                }
            }
            ent_end[e as usize] = e + 1;
        }
        if ent_end[0] == 0 {
            ent_end[0] = ent_name.len() as u32;
        }
        let n = ent_name.len();
        ent_end.truncate(n);
        let u = names.len();
        // --- Name arenas.
        let total_bytes: usize = names.iter().map(|s| s.len()).sum();
        let mut name_orig = Vec::with_capacity(total_bytes);
        let mut name_off = Vec::with_capacity(u + 1);
        name_off.push(0u32);
        for s in &names {
            name_orig.extend_from_slice(s);
            name_off.push(name_orig.len() as u32);
        }
        let name_lower: Vec<u8> = name_orig.par_iter().map(|&b| fold(b)).collect();
        let name_mask: Vec<u64> = (0..u)
            .into_par_iter()
            .map(|i| class_mask(&name_lower[name_off[i] as usize..name_off[i + 1] as usize]))
            .collect();
        let name_bigram: Vec<u64> = (0..u)
            .into_par_iter()
            .flat_map_iter(|i| {
                let s = bigram_sig(&name_lower[name_off[i] as usize..name_off[i + 1] as usize]);
                [s as u64, (s >> 64) as u64]
            })
            .collect();
        let mut name_sorted: Vec<u32> = (0..u as u32).collect();
        name_sorted.par_sort_unstable_by(|&a, &b| {
            let la = &name_lower[name_off[a as usize] as usize..name_off[a as usize + 1] as usize];
            let lb = &name_lower[name_off[b as usize] as usize..name_off[b as usize + 1] as usize];
            la.cmp(lb).then(a.cmp(&b))
        });
        // --- Inverted name → entries (CSR, ascending entries).
        let mut name_ent_off = vec![0u32; u + 1];
        for &id in &ent_name {
            name_ent_off[id as usize + 1] += 1;
        }
        for i in 0..u {
            name_ent_off[i + 1] += name_ent_off[i];
        }
        let mut fill = name_ent_off.clone();
        let mut name_ents = vec![0u32; n];
        for (e, &id) in ent_name.iter().enumerate() {
            name_ents[fill[id as usize] as usize] = e as u32;
            fill[id as usize] += 1;
        }
        let d = dir_entry.len();
        drop(intern);
        let mut b = Builder {
            words: vec![0u64; HDR_WORDS],
            sect: [(0, 0); NSECT],
        };
        b.put(S::NameLower, &name_lower);
        b.put(S::NameOrig, &name_orig);
        b.put(S::NameOff, &name_off);
        b.put(S::NameMask, &name_mask);
        b.put(S::NameSorted, &name_sorted);
        b.put(S::NameEntOff, &name_ent_off);
        b.put(S::NameEnts, &name_ents);
        b.put(S::EntName, &ent_name);
        b.put(S::EntParent, &ent_parent);
        b.put(S::EntKind, &ent_kind);
        b.put(S::EntPrior, &ent_prior);
        b.put(S::EntSize, &ent_size);
        b.put(S::EntMtime, &ent_mtime);
        b.put(S::EntEnd, &ent_end);
        b.put(S::DirEntry, &dir_entry);
        b.put(S::Root, &root_b);
        b.put(S::NameBigram, &name_bigram);
        drop(names);
        drop(by_id);
        let mut words = b.words;
        words[0] = u64::from_le_bytes(*MAGIC);
        words[1] = n as u64;
        words[2] = u as u64;
        words[3] = d as u64;
        words[4] = created as u64;
        for (i, &(off, len)) in b.sect.iter().enumerate() {
            words[5 + i * 2] = off as u64;
            words[6 + i * 2] = len as u64;
        }
        Index {
            buf: Buf::Owned(words),
            sect: b.sect,
            n,
            u,
            d,
            created,
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
        let bad = |why: &str| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {why}", path.display()),
            )
        };
        if m.len() < HDR_WORDS * 8 || &m[..8] != MAGIC {
            return Err(bad("not a flashfind index (or an older format)"));
        }
        let w = |i: usize| u64::from_ne_bytes(m[i * 8..i * 8 + 8].try_into().unwrap());
        let mut sect = [(0usize, 0usize); NSECT];
        let sizes = [1usize, 1, 4, 8, 4, 4, 4, 4, 4, 1, 1, 8, 8, 4, 4, 1, 8];
        for i in 0..NSECT {
            let (off, len) = (w(5 + i * 2) as usize, w(6 + i * 2) as usize);
            if off % 8 != 0
                || off
                    .checked_add(len.saturating_mul(sizes[i]))
                    .is_none_or(|end| end > m.len())
            {
                return Err(bad("corrupt section table"));
            }
            sect[i] = (off, len);
        }
        let (n, u, d, created) = (w(1) as usize, w(2) as usize, w(3) as usize, w(4) as i64);
        if sect[S::EntName as usize].1 != n
            || sect[S::NameOff as usize].1 != u + 1
            || sect[S::NameBigram as usize].1 != 2 * u
            || sect[S::DirEntry as usize].1 != d
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

/// Crawl + build in one call.
pub fn build_from_disk(root: &Path, opts: &crate::walk::ScanOptions) -> Index {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let ls = crate::walk::scan(&root, opts);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Index::build(ls, &root, now)
}
