//! C ABI for in-process use (e.g. from a Swift app). See
//! `include/flashfind.h` for the documented contract.
//!
//! Threading model: an `ff_index` is immutable and may be shared by any
//! number of sessions/threads. An `ff_session` is used by one search at a
//! time (it is internally locked); start each keystroke's search with a
//! fresh `ff_cancel` and cancel the previous one from any thread.

#![allow(clippy::missing_safety_doc)]

use flashfind_core::{CancelToken, Index, Live, LiveOptions, LiveSession, Query};
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct FfIndex(Arc<Index>);
pub struct FfLive(Arc<Live>);
pub struct FfSession(Mutex<LiveSession>);

#[repr(C)]
#[derive(Default)]
pub struct FfProgress {
    /// 0 crawling, 1 building, 2 ready.
    pub phase: u8,
    pub watching: u8,
    pub entries: u64,
    pub dirs: u64,
    pub overlay: u64,
    pub dead: u64,
    pub last_event_id: u64,
    pub generation: u64,
    pub compactions: u64,
    pub elapsed_ms: f64,
}
pub struct FfCancel(CancelToken);

#[repr(C)]
pub struct FfHit {
    /// UTF-8 (possibly lossy) absolute path, NUL-terminated; valid only during the callback.
    pub path: *const c_char,
    pub path_len: u32,
    pub kind: u8,
    pub score: i32,
    pub size: u64,
    pub mtime: i64,
    pub entry: u32,
}

#[repr(C)]
pub struct FfPage {
    pub hits: *const FfHit,
    pub count: usize,
    /// Matching entries; a lower bound when `total_exact == 0`.
    pub total: u64,
    pub total_exact: u8,
    /// 0 full, 1 narrowed, 2 cached, 3 prefix-range, 4 entry-scan.
    pub strategy: u8,
    pub elapsed_us: u64,
}

pub type FfPageCallback = Option<unsafe extern "C" fn(ctx: *mut c_void, page: *const FfPage)>;

pub const FF_OK: i32 = 0;
pub const FF_CANCELLED: i32 = 1;
pub const FF_ERROR: i32 = -1;

fn set_err(err: *mut *mut c_char, msg: &str) {
    if !err.is_null() {
        let c = CString::new(msg.replace('\0', " ")).unwrap_or_default();
        unsafe { *err = c.into_raw() };
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() { None } else { unsafe { CStr::from_ptr(p) }.to_str().ok() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Load a saved index (mmap). Returns NULL and sets `*err` on failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_open(path: *const c_char, err: *mut *mut c_char) -> *mut FfIndex {
    let Some(p) = (unsafe { cstr(path) }) else {
        set_err(err, "path is NULL or not UTF-8");
        return std::ptr::null_mut();
    };
    match Index::load(Path::new(p)) {
        Ok(i) => Box::into_raw(Box::new(FfIndex(Arc::new(i)))),
        Err(e) => {
            set_err(err, &e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Crawl `root` and build an index in memory (blocking).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_build(root: *const c_char, threads: u32, err: *mut *mut c_char) -> *mut FfIndex {
    let Some(r) = (unsafe { cstr(root) }) else {
        set_err(err, "root is NULL or not UTF-8");
        return std::ptr::null_mut();
    };
    let opts = flashfind_core::walk::ScanOptions {
        threads: if threads == 0 { 8 } else { threads as usize },
        skip: Vec::new(),
    };
    let idx = flashfind_core::build_from_disk(Path::new(r), &opts);
    Box::into_raw(Box::new(FfIndex(Arc::new(idx))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_save(idx: *const FfIndex, path: *const c_char, err: *mut *mut c_char) -> i32 {
    let (Some(i), Some(p)) = (unsafe { idx.as_ref() }, unsafe { cstr(path) }) else {
        set_err(err, "NULL argument");
        return FF_ERROR;
    };
    match i.0.save(Path::new(p)) {
        Ok(()) => FF_OK,
        Err(e) => {
            set_err(err, &e.to_string());
            FF_ERROR
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_entries(idx: *const FfIndex) -> u64 {
    unsafe { idx.as_ref() }.map_or(0, |i| i.0.n as u64)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_index_free(idx: *mut FfIndex) {
    if !idx.is_null() {
        drop(unsafe { Box::from_raw(idx) });
    }
}

/// A search session (one per search field). Keeps its own reference to the index.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_session_new(idx: *const FfIndex) -> *mut FfSession {
    match unsafe { idx.as_ref() } {
        Some(i) => Box::into_raw(Box::new(FfSession(Mutex::new(LiveSession::new(Live::from_index(
            i.0.clone(),
            LiveOptions::default(),
        )))))),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_session_free(s: *mut FfSession) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ff_cancel_new() -> *mut FfCancel {
    Box::into_raw(Box::new(FfCancel(CancelToken::new())))
}

/// Safe to call from any thread while a search using this token runs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_cancel_cancel(c: *const FfCancel) {
    if let Some(c) = unsafe { c.as_ref() } {
        c.0.cancel();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_cancel_free(c: *mut FfCancel) {
    if !c.is_null() {
        drop(unsafe { Box::from_raw(c) });
    }
}

/// Run `query` and deliver the top `limit` hits (0 = query's own `limit:` or
/// 50) through `cb` exactly once, before returning FF_OK. Returns
/// FF_CANCELLED (callback not called) if `cancel` fired, FF_ERROR (with
/// `*err` set) for a bad query.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_search(
    s: *mut FfSession,
    query: *const c_char,
    limit: u32,
    cancel: *const FfCancel,
    cb: FfPageCallback,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> i32 {
    let t0 = std::time::Instant::now();
    let Some(sess) = (unsafe { s.as_ref() }) else {
        set_err(err, "session is NULL");
        return FF_ERROR;
    };
    let Some(qs) = (unsafe { cstr(query) }) else {
        set_err(err, "query is NULL or not UTF-8");
        return FF_ERROR;
    };
    let mut q = match Query::parse(qs) {
        Ok(q) => q,
        Err(e) => {
            set_err(err, &e);
            return FF_ERROR;
        }
    };
    if limit > 0 {
        q.limit = limit as usize;
    }
    let token = unsafe { cancel.as_ref() }.map(|c| c.0.clone()).unwrap_or_default();
    let mut guard = sess.0.lock().unwrap_or_else(|p| p.into_inner());
    let res = match guard.search(&q, &token) {
        Ok(r) => r,
        Err(_) => return FF_CANCELLED,
    };
    drop(guard);
    if token.is_cancelled() {
        return FF_CANCELLED;
    }
    let paths: Vec<CString> = res
        .hits
        .iter()
        .map(|h| CString::new(h.path_str().replace('\0', "")).unwrap_or_default())
        .collect();
    let hits: Vec<FfHit> = res
        .hits
        .iter()
        .zip(&paths)
        .map(|(h, p)| FfHit {
            path: p.as_ptr(),
            path_len: p.as_bytes().len() as u32,
            kind: h.kind,
            score: h.score,
            size: h.size,
            mtime: h.mtime,
            entry: h.entry,
        })
        .collect();
    let page = FfPage {
        hits: hits.as_ptr(),
        count: hits.len(),
        total: res.total,
        total_exact: res.total_exact as u8,
        strategy: res.strategy as u8,
        elapsed_us: t0.elapsed().as_micros() as u64,
    };
    if let Some(cb) = cb {
        unsafe { cb(ctx, &page) };
    }
    FF_OK
}

fn live_opts(threads: u32, poll_ms: u32) -> LiveOptions {
    let mut o = LiveOptions::default();
    if threads > 0 {
        o.threads = threads as usize;
    }
    if poll_ms > 0 {
        o.poll = std::time::Duration::from_millis(poll_ms as u64);
    }
    o
}

/// Open a saved index for live use (compactions are saved back to it).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_open(path: *const c_char, err: *mut *mut c_char) -> *mut FfLive {
    let Some(p) = (unsafe { cstr(path) }) else {
        set_err(err, "path is NULL or not UTF-8");
        return std::ptr::null_mut();
    };
    match Live::open(Path::new(p), LiveOptions::default()) {
        Ok(l) => Box::into_raw(Box::new(FfLive(l))),
        Err(e) => {
            set_err(err, &e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Start crawling `root` in the background; searchable immediately
/// (partial results until `ff_live_progress` reports phase 2). `index_path`
/// may be NULL (memory only).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_crawl(root: *const c_char, index_path: *const c_char, threads: u32, err: *mut *mut c_char) -> *mut FfLive {
    let Some(r) = (unsafe { cstr(root) }) else {
        set_err(err, "root is NULL or not UTF-8");
        return std::ptr::null_mut();
    };
    let path = unsafe { cstr(index_path) }.map(std::path::PathBuf::from);
    Box::into_raw(Box::new(FfLive(Live::crawl(Path::new(r), path, live_opts(threads, 0)))))
}

/// Watch for changes (FSEvents on macOS; polling every `poll_ms` elsewhere).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_watch(l: *const FfLive, err: *mut *mut c_char) -> i32 {
    let Some(l) = (unsafe { l.as_ref() }) else {
        set_err(err, "live is NULL");
        return FF_ERROR;
    };
    match l.0.watch() {
        Ok(()) => FF_OK,
        Err(e) => {
            set_err(err, &e.to_string());
            FF_ERROR
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_unwatch(l: *const FfLive) {
    if let Some(l) = unsafe { l.as_ref() } {
        l.0.unwatch();
    }
}

/// Relist one directory now (e.g. after the app itself changed it).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_refresh(l: *const FfLive, dir: *const c_char) -> i32 {
    match (unsafe { l.as_ref() }, unsafe { cstr(dir) }) {
        (Some(l), Some(d)) => {
            l.0.refresh(&[d.as_bytes()]);
            FF_OK
        }
        _ => FF_ERROR,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_progress(l: *const FfLive, out: *mut FfProgress) {
    let (Some(l), Some(out)) = (unsafe { l.as_ref() }, unsafe { out.as_mut() }) else {
        return;
    };
    let p = l.0.progress();
    *out = FfProgress {
        phase: p.phase,
        watching: p.watching as u8,
        entries: p.entries,
        dirs: p.dirs,
        overlay: p.overlay,
        dead: p.dead,
        last_event_id: p.last_event_id,
        generation: p.generation,
        compactions: p.compactions,
        elapsed_ms: p.elapsed_ms,
    };
}

/// Fold live changes into a new base index (saved if the index has a path).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_compact(l: *const FfLive) -> i32 {
    match unsafe { l.as_ref() } {
        Some(l) => l.0.compact() as i32,
        None => FF_ERROR,
    }
}

/// Compact if dirty, else record the last event id (call before quitting).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_checkpoint(l: *const FfLive) {
    if let Some(l) = unsafe { l.as_ref() } {
        l.0.checkpoint();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_session_new(l: *const FfLive) -> *mut FfSession {
    match unsafe { l.as_ref() } {
        Some(l) => Box::into_raw(Box::new(FfSession(Mutex::new(LiveSession::new(l.0.clone()))))),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_live_free(l: *mut FfLive) {
    if !l.is_null() {
        let b = unsafe { Box::from_raw(l) };
        b.0.unwatch();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn collect(ctx: *mut c_void, page: *const FfPage) {
        let out = unsafe { &mut *(ctx as *mut Vec<String>) };
        let p = unsafe { &*page };
        for i in 0..p.count {
            let h = unsafe { &*p.hits.add(i) };
            out.push(unsafe { CStr::from_ptr(h.path) }.to_string_lossy().into_owned());
        }
    }

    #[test]
    fn roundtrip_through_c_abi() {
        let root = std::env::temp_dir().join(format!("flashfind-ffi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/invoice_2024.pdf"), b"x").unwrap();
        std::fs::write(root.join("docs/notes.md"), b"x").unwrap();
        let croot = CString::new(root.to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        unsafe {
            let idx = ff_index_build(croot.as_ptr(), 2, &mut err);
            assert!(!idx.is_null());
            let file = CString::new(root.join("i.ffx").to_str().unwrap()).unwrap();
            assert_eq!(ff_index_save(idx, file.as_ptr(), &mut err), FF_OK);
            let loaded = ff_index_open(file.as_ptr(), &mut err);
            assert!(!loaded.is_null());
            ff_index_free(idx);
            let s = ff_session_new(loaded);
            let c = ff_cancel_new();
            let mut out: Vec<String> = Vec::new();
            let q = CString::new("invoce").unwrap();
            let rc = ff_search(s, q.as_ptr(), 10, c, Some(collect), &mut out as *mut _ as *mut c_void, &mut err);
            assert_eq!(rc, FF_OK);
            assert!(out[0].ends_with("docs/invoice_2024.pdf"), "{out:?}");
            // Cancelled before start: no callback.
            ff_cancel_cancel(c);
            out.clear();
            let rc = ff_search(s, q.as_ptr(), 10, c, Some(collect), &mut out as *mut _ as *mut c_void, &mut err);
            assert_eq!(rc, FF_CANCELLED);
            assert!(out.is_empty());
            // Bad query reports an error string.
            let bad = CString::new("size:12zz").unwrap();
            assert_eq!(ff_search(s, bad.as_ptr(), 0, std::ptr::null(), None, std::ptr::null_mut(), &mut err), FF_ERROR);
            assert!(!err.is_null());
            ff_string_free(err);
            ff_cancel_free(c);
            ff_session_free(s);
            ff_index_free(loaded);
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
