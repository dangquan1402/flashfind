//! macOS crawler: `getattrlistbulk(2)` over directory fds opened with
//! `openat(2)` relative to the parent fd, so paths are never rebuilt and
//! `PATH_MAX` never truncates a deep tree.
//!
//! Adapted from fsearch `src/walk.rs` (MIT, (c) 2026 Noah Dunnagan); see
//! THIRD_PARTY_NOTICES.md. Changes: full i64 mtimes (no u32 clamp), skip
//! list passed in explicitly, no global state.

use super::*;
use std::cell::RefCell;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const DIR_MNTSTATUS_TRIGGER: u32 = 0x0000_0002;
const OPEN_DIR: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 256 * 1024]);
}

struct Fd(i32);
impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Never trigger iCloud downloads while crawling (dataless files).
pub fn no_materialize() {
    unsafe extern "C" {
        fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
    }
    // IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD, OFF
    unsafe { setiopolicy_np(3, 1, 1) };
}

fn raise_fd_limit() {
    let mut r: libc::rlimit = unsafe { std::mem::zeroed() };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) };
    r.rlim_cur = r.rlim_max.min(65536);
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &r) };
}

pub fn scan(root: &Path, opts: &ScanOptions) -> Vec<Listing> {
    let threads = opts.threads.max(1);
    raise_fd_limit();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .start_handler(|_| no_materialize())
        .build()
        .expect("thread pool");
    let sink = Sink::new(threads);
    let root_b = root.as_os_str().as_bytes().to_vec();
    let fd =
        CString::new(root_b.clone()).map_or(-1, |c| unsafe { libc::open(c.as_ptr(), OPEN_DIR) });
    let track = (!opts.skip.is_empty()).then_some(root_b);
    pool.scope(|s| finish_dir(s, fd, track, 0, opts, &sink));
    sink.finish()
}

fn finish_dir<'s>(
    s: &rayon::Scope<'s>,
    fd: i32,
    path: Option<Vec<u8>>,
    id: u32,
    opts: &'s ScanOptions,
    sink: &'s Sink,
) {
    let mut l = Listing {
        id,
        ..Default::default()
    };
    if fd < 0 {
        sink.push(l);
        return;
    }
    list_fd(fd, &mut l);
    let me = Arc::new(Fd(fd));
    let mut kids = Vec::new();
    for i in 0..l.ents.len() {
        let e = &l.ents[i];
        if e.kind & KIND_MASK == KIND_DIR && e.kind & FLAG_MOUNT == 0 {
            let name = l.name(e).to_vec();
            let child_path = path.as_ref().map(|p| join(p, &name));
            if child_path.as_deref().is_some_and(|p| opts.skipped(p)) {
                continue;
            }
            let cid = sink.id();
            l.ents[i].child = cid;
            kids.push((CString::new(name).unwrap_or_default(), cid, child_path));
        }
    }
    sink.push(l);
    for (name, cid, child_path) in kids {
        let parent = me.clone();
        s.spawn(move |s| {
            let fd = unsafe { libc::openat(parent.0, name.as_ptr(), OPEN_DIR) };
            drop(parent);
            finish_dir(s, fd, child_path, cid, opts, sink);
        });
    }
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

fn rd64(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

fn list_fd(fd: i32, l: &mut Listing) {
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | ATTR_CMN_ERROR
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_MODTIME
        | libc::ATTR_CMN_FLAGS;
    al.dirattr = libc::ATTR_DIR_MOUNTSTATUS;
    al.fileattr = libc::ATTR_FILE_DATALENGTH;
    BUF.with_borrow_mut(|buf| {
        loop {
            let n = unsafe {
                libc::getattrlistbulk(
                    fd,
                    &mut al as *mut _ as *mut libc::c_void,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
            if n <= 0 {
                break;
            }
            let mut p = 0usize;
            for _ in 0..n {
                let len = rd32(buf, p) as usize;
                if len == 0 || p + len > buf.len() {
                    break;
                }
                parse_entry(&buf[p..p + len], l);
                p += len;
            }
        }
    });
}

fn parse_entry(b: &[u8], l: &mut Listing) {
    let common = rd32(b, 4);
    let dirattr = rd32(b, 12);
    let fileattr = rd32(b, 16);
    let mut f = 24;
    if common & ATTR_CMN_ERROR != 0 {
        f += 4;
    }
    if common & libc::ATTR_CMN_NAME == 0 {
        return;
    }
    let off = rd32(b, f) as i32 as isize;
    let nlen = rd32(b, f + 4) as usize;
    let start = (f as isize + off) as usize;
    if start + nlen > b.len() {
        return;
    }
    let name = &b[start..start + nlen.saturating_sub(1)];
    f += 8;
    let mut kind = KIND_OTHER;
    if common & libc::ATTR_CMN_OBJTYPE != 0 {
        kind = match rd32(b, f) {
            1 => KIND_FILE,
            2 => KIND_DIR,
            5 => KIND_LINK,
            _ => KIND_OTHER,
        };
        f += 4;
    }
    let mut mtime = 0i64;
    if common & libc::ATTR_CMN_MODTIME != 0 {
        mtime = rd64(b, f) as i64;
        f += 16;
    }
    if common & libc::ATTR_CMN_FLAGS != 0 {
        if rd32(b, f) & libc::UF_HIDDEN != 0 {
            kind |= FLAG_HIDDEN;
        }
        f += 4;
    }
    if dirattr & libc::ATTR_DIR_MOUNTSTATUS != 0 {
        if rd32(b, f) & (libc::DIR_MNTSTATUS_MNTPOINT | DIR_MNTSTATUS_TRIGGER) != 0 {
            kind |= FLAG_MOUNT;
        }
        f += 4;
    }
    let mut size = 0u64;
    if fileattr & libc::ATTR_FILE_DATALENGTH != 0 {
        size = rd64(b, f);
    }
    l.push(name, kind, size, mtime);
}
