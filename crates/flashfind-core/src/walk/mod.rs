//! Directory crawling. Produces one [`Listing`] per directory; the index
//! builder stitches them into a depth-first ordered tree.
//!
//! * macOS: `getattrlistbulk(2)` + `openat(2)` (one syscall returns a whole
//!   batch of entries with name/type/mtime/size), adapted from fsearch (MIT).
//! * everywhere else (and on macOS via [`scan_portable`]): `std::fs::read_dir`.

pub use crate::live::Partial;
pub use portable::mtime_of;
use std::path::Path;
use std::sync::Arc;

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

    pub fn push(&mut self, name: &[u8], kind: u8, size: u64, mtime: i64) -> Option<usize> {
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
        ScanOptions { threads: 8, skip: Vec::new() }
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
    scan_into(root, opts, None)
}

/// Crawl, publishing every listing to `partial` as soon as it is read (so
/// it can be searched while the crawl runs). With `partial`, the returned
/// vector is empty: the listings live in `partial`.
pub fn scan_into(root: &Path, opts: &ScanOptions, partial: Option<Arc<Partial>>) -> Vec<Listing> {
    #[cfg(target_os = "macos")]
    {
        macos::scan(root, opts, partial)
    }
    #[cfg(not(target_os = "macos"))]
    {
        portable::scan(root, opts, partial)
    }
}

/// The portable `read_dir` crawler (used for tests and comparisons on macOS).
pub fn scan_portable(root: &Path, opts: &ScanOptions) -> Vec<Listing> {
    portable::scan(root, opts, None)
}

/// List one directory (no recursion; children's `child` is [`NONE`]).
/// `None` if it cannot be opened.
pub fn list_dir(path: &Path) -> Option<Listing> {
    #[cfg(target_os = "macos")]
    {
        macos::list_dir(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        portable::list_dir(path)
    }
}

/// Name of the backend [`scan`] uses.
pub fn backend() -> &'static str {
    if cfg!(target_os = "macos") { "getattrlistbulk" } else { "read_dir" }
}

pub(crate) struct Sink {
    pub next_id: std::sync::atomic::AtomicU32,
    pub out: Vec<std::sync::Mutex<Vec<Listing>>>,
    pub partial: Option<Arc<Partial>>,
}

impl Sink {
    pub fn new(threads: usize, partial: Option<Arc<Partial>>) -> Sink {
        Sink {
            next_id: std::sync::atomic::AtomicU32::new(1),
            out: (0..threads + 1).map(|_| Default::default()).collect(),
            partial,
        }
    }
    pub fn push(&self, l: Listing) {
        if let Some(p) = &self.partial {
            p.add(l);
            return;
        }
        let slot = rayon::current_thread_index().unwrap_or(self.out.len() - 1).min(self.out.len() - 1);
        self.out[slot].lock().unwrap().push(l);
    }
    pub fn id(&self) -> u32 {
        self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
    pub fn finish(self) -> Vec<Listing> {
        self.out.into_iter().flat_map(|m| m.into_inner().unwrap()).collect()
    }
}

pub(crate) fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(parent.len() + 1 + name.len());
    p.extend_from_slice(parent);
    if !p.ends_with(b"/") {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

/// This process's physical footprint in MB (macOS `ri_phys_footprint`,
/// else resident set), for diagnostics.
pub fn footprint_mb() -> f64 {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        if libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, &mut info as *mut _ as *mut libc::rusage_info_t) == 0 {
            return info.ri_phys_footprint as f64 / 1048576.0;
        }
        0.0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let pages: u64 = s.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
        pages as f64 * 4096.0 / 1048576.0
    }
}
