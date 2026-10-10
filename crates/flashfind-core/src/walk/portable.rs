//! Portable crawler: `std::fs::read_dir` fanned out over a rayon scope.

use super::*;
use std::path::PathBuf;

pub fn scan(root: &Path, opts: &ScanOptions, partial: Option<Arc<Partial>>) -> Vec<Listing> {
    let threads = opts.threads.max(1);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("thread pool");
    let sink = Sink::new(threads, partial);
    let root_dev = dev_of(root);
    pool.scope(|s| list(s, root.to_path_buf(), 0, root_dev, opts, &sink));
    sink.finish()
}

#[cfg(unix)]
fn dev_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).ok().map(|m| m.dev())
}
#[cfg(not(unix))]
fn dev_of(_: &Path) -> Option<u64> {
    None
}

#[cfg(unix)]
fn bytes(p: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(p.as_bytes())
}
#[cfg(not(unix))]
fn bytes(p: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(p.to_string_lossy().into_owned().into_bytes())
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn list_dir(path: &Path) -> Option<Listing> {
    let mut l = Listing::default();
    let rd = std::fs::read_dir(path).ok()?;
    let dev = dev_of(path);
    for de in rd.flatten() {
        let name = de.file_name();
        let Ok(md) = de.metadata() else { continue };
        let (kind, size, mtime) = attrs(&md, dev);
        l.push(&bytes(&name), kind, size, mtime);
    }
    Some(l)
}

fn attrs(md: &std::fs::Metadata, dev: Option<u64>) -> (u8, u64, i64) {
    let ft = md.file_type();
    #[allow(unused_mut)]
    let mut kind = if ft.is_symlink() {
        KIND_LINK
    } else if ft.is_dir() {
        KIND_DIR
    } else if ft.is_file() {
        KIND_FILE
    } else {
        KIND_OTHER
    };
    #[cfg(unix)]
    if kind == KIND_DIR {
        use std::os::unix::fs::MetadataExt;
        if dev.is_some_and(|d| d != md.dev()) {
            kind |= FLAG_MOUNT;
        }
    }
    #[cfg(not(unix))]
    let _ = dev;
    let size = if kind & KIND_MASK == KIND_FILE { md.len() } else { 0 };
    (kind, size, mtime_of(md))
}

/// Mtime in Unix seconds (negative before 1970), 0 if unavailable.
pub fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .map(|t| match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs().min(i64::MAX as u64) as i64,
            Err(e) => -(e.duration().as_secs().min(i64::MAX as u64) as i64),
        })
        .unwrap_or(0)
}

fn list<'s>(s: &rayon::Scope<'s>, path: PathBuf, id: u32, dev: Option<u64>, opts: &'s ScanOptions, sink: &'s Sink) {
    let mut l = Listing { id, ..Default::default() };
    let mut kids = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&path) {
        for de in rd.flatten() {
            let name = de.file_name();
            let name_b = bytes(&name);
            // DirEntry::metadata does not follow symlinks.
            let Ok(md) = de.metadata() else { continue };
            let (kind, size, mtime) = attrs(&md, dev);
            let Some(i) = l.push(&name_b, kind, size, mtime) else {
                continue;
            };
            if kind == KIND_DIR {
                let child = path.join(&name);
                if opts.skipped(&bytes(child.as_os_str())) {
                    continue;
                }
                let cid = sink.id();
                l.ents[i].child = cid;
                kids.push((child, cid));
            }
        }
    }
    sink.push(l);
    for (child, cid) in kids {
        s.spawn(move |s| list(s, child, cid, dev, opts, sink));
    }
}
