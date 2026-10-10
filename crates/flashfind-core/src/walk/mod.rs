//! Directory crawling. Produces one [`Listing`] per directory; the index
//! builder stitches them into a depth-first ordered tree.
//!
//! * macOS: `getattrlistbulk(2)` + `openat(2)` (one syscall returns a whole
//!   batch of entries with name/type/mtime/size), adapted from fsearch (MIT).
//! * everywhere else (and on macOS via [`scan_portable`]): `std::fs::read_dir`.

use std::path::Path;

#[cfg(target_os = "macos")]
mod macos;
mod portable;

/// Low two bits of `kind`.
pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const KIND_LINK: u8 = 2;
pub const KIND_OTHER: u8 = 3;
pub const KIND_MASK: u8 = 3;
/// `UF_HIDDEN` (macOS) – dot-names are detected from the name itself.
pub const FLAG_HIDDEN: u8 = 4;
/// A mount point / firmlink trigger: listed, but not descended into.
pub const FLAG_MOUNT: u8 = 8;

/// No child listing (files, unreadable or skipped directories).
pub const NONE: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub struct RawEnt {
    pub name_off: u32,
    pub name_len: u16,
    pub kind: u8,
    pub size: u64,
    /// Seconds since the Unix epoch. Full i64: no 2038/2106 clamping.
    pub mtime: i64,
    /// Listing id of this directory's own listing, or [`NONE`].
    pub child: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub id: u32,
    pub names: Vec<u8>,
    pub ents: Vec<RawEnt>,
}

impl Listing {
    pub fn name(&self, e: &RawEnt) -> &[u8] {
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }

    pub(crate) fn push(&mut self, name: &[u8], kind: u8, size: u64, mtime: i64) -> Option<usize> {
        if name.is_empty() || name.len() > u16::MAX as usize {
            return None;
        }
        self.ents.push(RawEnt {
            name_off: self.names.len() as u32,
            name_len: name.len() as u16,
            kind,
            size,
            mtime,
            child: NONE,
        });
        self.names.extend_from_slice(name);
        Some(self.ents.len() - 1)
    }
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub threads: usize,
    /// Absolute paths not to descend into.
    pub skip: Vec<Vec<u8>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            threads: 8,
            skip: Vec::new(),
        }
    }
}

impl ScanOptions {
    pub(crate) fn skipped(&self, path: &[u8]) -> bool {
        self.skip
            .iter()
            .any(|s| path.starts_with(s) && (path.len() == s.len() || path[s.len()] == b'/'))
    }
}

/// Crawl `root` with the fastest backend for this platform. Listing id 0 is
/// `root` itself.
pub fn scan(root: &Path, opts: &ScanOptions) -> Vec<Listing> {
    #[cfg(target_os = "macos")]
    {
        macos::scan(root, opts)
    }
    #[cfg(not(target_os = "macos"))]
    {
        portable::scan(root, opts)
    }
}

/// The portable `read_dir` crawler (used for tests and comparisons on macOS).
pub fn scan_portable(root: &Path, opts: &ScanOptions) -> Vec<Listing> {
    portable::scan(root, opts)
}

/// Name of the backend [`scan`] uses.
pub fn backend() -> &'static str {
    if cfg!(target_os = "macos") {
        "getattrlistbulk"
    } else {
        "read_dir"
    }
}

pub(crate) struct Sink {
    pub next_id: std::sync::atomic::AtomicU32,
    pub out: Vec<std::sync::Mutex<Vec<Listing>>>,
}

impl Sink {
    pub fn new(threads: usize) -> Sink {
        Sink {
            next_id: std::sync::atomic::AtomicU32::new(1),
            out: (0..threads + 1).map(|_| Default::default()).collect(),
        }
    }
    pub fn push(&self, l: Listing) {
        let slot = rayon::current_thread_index()
            .unwrap_or(self.out.len() - 1)
            .min(self.out.len() - 1);
        self.out[slot].lock().unwrap().push(l);
    }
    pub fn id(&self) -> u32 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
    pub fn finish(self) -> Vec<Listing> {
        self.out
            .into_iter()
            .flat_map(|m| m.into_inner().unwrap())
            .collect()
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(parent.len() + 1 + name.len());
    p.extend_from_slice(parent);
    if !p.ends_with(b"/") {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}
