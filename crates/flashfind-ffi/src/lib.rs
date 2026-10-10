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

// ------------------------------------------------------------ code projects

use flashfind_code::{CodeLive, CodeQuery, CodeSession, Config as CodeConfig};

pub struct FfCode(Arc<CodeLive>);
pub struct FfCodeSession(Mutex<CodeSession>);

#[repr(C)]
pub struct FfCodeHit {
    pub path: *const c_char,
    /// Path relative to the repo root.
    pub rel: *const c_char,
    pub repo: *const c_char,
    pub repo_root: *const c_char,
    /// Index of this hit's group (repo) in display order.
    pub group: u32,
    pub kind: u8,
    pub score: i32,
    pub size: u64,
    pub mtime: i64,
    /// Repo's last activity (Unix seconds).
    pub repo_active: i64,
}

#[repr(C)]
pub struct FfCodePage {
    pub hits: *const FfCodeHit,
    pub count: usize,
    pub total: u64,
    pub groups: u32,
    pub elapsed_us: u64,
}

pub type FfCodePageCallback = Option<unsafe extern "C" fn(ctx: *mut c_void, page: *const FfCodePage)>;

#[repr(C)]
#[derive(Default)]
pub struct FfCodeStats {
    pub repos: u64,
    pub entries: u64,
    pub overlays: u64,
    pub removed: u64,
    pub events: u64,
    pub rewalks: u64,
    pub repos_added: u64,
    pub repos_removed: u64,
    pub compactions: u64,
    pub last_event_id: u64,
    pub watching: u8,
    pub last_apply_ms: f64,
}

/// Open the code-projects index. `roots`: ':'-separated folders, or NULL for
/// $FLASHFIND_CODE_ROOTS / ~. `cache_dir`: NULL for ~/.cache/flashfind.
/// Loads the cache (replaying FSEvents since it was built on macOS) or
/// discovers + crawls (seconds for a big home): call off the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_open(roots: *const c_char, cache_dir: *const c_char, err: *mut *mut c_char) -> *mut FfCode {
    let mut cfg = CodeConfig::default();
    if let Some(r) = unsafe { cstr(roots) }
        && !r.is_empty()
    {
        cfg.roots = std::env::split_paths(r).collect();
    }
    cfg.roots = cfg.roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    if cfg.roots.is_empty() {
        set_err(err, "no existing root folder");
        return std::ptr::null_mut();
    }
    let cache = unsafe { cstr(cache_dir) }
        .map(std::path::PathBuf::from)
        .unwrap_or_else(flashfind_code::CodeIndex::default_dir);
    let live = CodeLive::open(cfg, Some(cache));
    if flashfind_core::watch::native_events() && live.snapshot().base.age() > std::time::Duration::from_secs(5) {
        live.replay(std::time::Duration::from_secs(3));
    }
    Box::into_raw(Box::new(FfCode(live)))
}

/// Follow file changes (FSEvents on macOS; periodic rebuilds elsewhere).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_watch(c: *const FfCode, err: *mut *mut c_char) -> i32 {
    let Some(c) = (unsafe { c.as_ref() }) else {
        set_err(err, "code is NULL");
        return FF_ERROR;
    };
    match c.0.watch() {
        Ok(()) => FF_OK,
        Err(e) => {
            set_err(err, &e.to_string());
            FF_ERROR
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_unwatch(c: *const FfCode) {
    if let Some(c) = unsafe { c.as_ref() } {
        c.0.unwatch();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_get_stats(c: *const FfCode, out: *mut FfCodeStats) {
    let (Some(c), Some(out)) = (unsafe { c.as_ref() }, unsafe { out.as_mut() }) else {
        return;
    };
    let s = c.0.stats();
    *out = FfCodeStats {
        repos: s.repos as u64,
        entries: s.entries as u64,
        overlays: s.overlays as u64,
        removed: s.removed as u64,
        events: s.events,
        rewalks: s.rewalks,
        repos_added: s.repos_added,
        repos_removed: s.repos_removed,
        compactions: s.compactions,
        last_event_id: s.last_event_id,
        watching: s.watching as u8,
        last_apply_ms: s.last_apply_ms,
    };
}

/// Projects as JSON (`{total, projects:[…]}`), most recently active first.
/// Free with ff_string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_projects_json(c: *const FfCode, filter: *const c_char, limit: u32) -> *mut c_char {
    let Some(c) = (unsafe { c.as_ref() }) else { return std::ptr::null_mut() };
    let v = flashfind_code::projects_json(&c.0.repos(), unsafe { cstr(filter) }, if limit == 0 { 1000 } else { limit as usize });
    CString::new(v.to_string()).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
}

/// Fold live changes into a fresh saved base (call before quitting).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_checkpoint(c: *const FfCode) {
    if let Some(c) = unsafe { c.as_ref() } {
        c.0.checkpoint();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_free(c: *mut FfCode) {
    if !c.is_null() {
        let b = unsafe { Box::from_raw(c) };
        b.0.unwatch();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_session_new(c: *const FfCode) -> *mut FfCodeSession {
    match unsafe { c.as_ref() } {
        Some(c) => Box::into_raw(Box::new(FfCodeSession(Mutex::new(CodeSession::new(c.0.clone()))))),
        None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_session_free(s: *mut FfCodeSession) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s) });
    }
}

/// Code search: the normal query language plus `repo:`, `lang:`, `dirty:`.
/// A query of filters only lists matching files, newest first. Delivers
/// the page through `cb` once (pointers valid during the callback only).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ff_code_search(
    s: *mut FfCodeSession,
    query: *const c_char,
    limit: u32,
    cancel: *const FfCancel,
    cb: FfCodePageCallback,
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
    let mut cq = CodeQuery::parse(qs);
    cq.limit = if limit == 0 { 50 } else { limit as usize };
    let token = unsafe { cancel.as_ref() }.map(|c| c.0.clone()).unwrap_or_default();
    let res = {
        let mut g = sess.0.lock().unwrap_or_else(|p| p.into_inner());
        if token.is_cancelled() {
            return FF_CANCELLED;
        }
        if cq.text.trim().is_empty() && cq.repos.is_empty() && cq.langs.is_empty() && !cq.dirty {
            Ok(flashfind_code::CodeResult::default())
        } else {
            g.search(&cq, &token)
        }
    };
    let res = match res {
        Ok(r) => r,
        Err(_) if token.is_cancelled() => return FF_CANCELLED,
        Err(e) => {
            set_err(err, &e);
            return FF_ERROR;
        }
    };
    if token.is_cancelled() {
        return FF_CANCELLED;
    }
    let cs = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();
    let mut group_of = vec![0u32; res.hits.len()];
    for (gi, g) in res.groups.iter().enumerate() {
        for &i in &g.hits {
            group_of[i] = gi as u32;
        }
    }
    // Display order: by group, then rank within the group.
    let order: Vec<usize> = res.groups.iter().flat_map(|g| g.hits.iter().copied()).collect();
    let strs: Vec<[CString; 4]> = order
        .iter()
        .map(|&i| {
            let h = &res.hits[i];
            [cs(&h.path), cs(&h.rel), cs(&h.repo_name), cs(&h.repo_root)]
        })
        .collect();
    let hits: Vec<FfCodeHit> = order
        .iter()
        .zip(&strs)
        .map(|(&i, st)| {
            let h = &res.hits[i];
            FfCodeHit {
                path: st[0].as_ptr(),
                rel: st[1].as_ptr(),
                repo: st[2].as_ptr(),
                repo_root: st[3].as_ptr(),
                group: group_of[i],
                kind: match h.kind {
                    "file" => 0,
                    "dir" => 1,
                    "link" => 2,
                    _ => 3,
                },
                score: h.score,
                size: h.size,
                mtime: h.mtime,
                repo_active: h.repo_active,
            }
        })
        .collect();
    let page = FfCodePage {
        hits: hits.as_ptr(),
        count: hits.len(),
        total: res.total,
        groups: res.groups.len() as u32,
        elapsed_us: t0.elapsed().as_micros() as u64,
    };
    if let Some(cb) = cb {
        unsafe { cb(ctx, &page) };
    }
    FF_OK
}
