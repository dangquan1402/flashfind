//! Change sources for [`crate::live::Live`]: an FSEvents stream on macOS
//! (directory granularity, replayable from a saved event id) and a polling
//! fallback everywhere else (re-checks directory mtimes).
//!
//! The FSEvents binding is adapted from fsearch `src/fsevents.rs` (MIT,
//! (c) 2026 Noah Dunnagan); see THIRD_PARTY_NOTICES.md. Changes: watches an
//! arbitrary root instead of `/`, releases its CF objects, reports more flags.

/// Event history for this path is incomplete: re-check the whole subtree.
pub const MUST_SCAN_SUBDIRS: u32 = 0x1;
pub const USER_DROPPED: u32 = 0x2;
pub const KERNEL_DROPPED: u32 = 0x4;
pub const EVENT_IDS_WRAPPED: u32 = 0x8;
pub const HISTORY_DONE: u32 = 0x10;
pub const ROOT_CHANGED: u32 = 0x20;
pub const MOUNT: u32 = 0x40;
pub const UNMOUNT: u32 = 0x80;
/// Any of these means "rescan from the reported path down".
pub const LOSSY: u32 = MUST_SCAN_SUBDIRS | USER_DROPPED | KERNEL_DROPPED | EVENT_IDS_WRAPPED | ROOT_CHANGED | MOUNT | UNMOUNT;

#[derive(Clone, Debug)]
pub struct Event {
    /// Directory whose contents changed (no trailing slash).
    pub path: Vec<u8>,
    pub flags: u32,
    pub id: u64,
}

/// Current FSEvents event id (0 where FSEvents does not exist).
pub fn current_event_id() -> u64 {
    #[cfg(target_os = "macos")]
    {
        unsafe { fsevents::FSEventsGetCurrentEventId() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// Whether [`Stream`] is backed by a real kernel event source here.
pub fn native_events() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(target_os = "macos")]
pub use fsevents::{Stream, watch};

#[cfg(target_os = "macos")]
mod fsevents {
    use super::Event;
    use std::ffi::{CStr, c_void};
    use std::sync::mpsc::Sender;

    const CREATE_FLAG_NO_DEFER: u32 = 0x2;
    const SINCE_NOW: u64 = u64::MAX;

    #[repr(C)]
    struct Context {
        version: isize,
        info: *mut c_void,
        retain: *const c_void,
        release: *const c_void,
        copy_description: *const c_void,
    }

    type Callback = extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn FSEventStreamCreate(
            alloc: *const c_void,
            cb: Callback,
            ctx: *const Context,
            paths: *const c_void,
            since: u64,
            latency: f64,
            flags: u32,
        ) -> *mut c_void;
        fn FSEventStreamSetDispatchQueue(s: *mut c_void, q: *mut c_void);
        fn FSEventStreamStart(s: *mut c_void) -> u8;
        fn FSEventStreamStop(s: *mut c_void);
        fn FSEventStreamInvalidate(s: *mut c_void);
        fn FSEventStreamRelease(s: *mut c_void);
        pub fn FSEventsGetCurrentEventId() -> u64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(alloc: *const c_void, bytes: *const u8, n: isize, enc: u32, external: u8) -> *const c_void;
        fn CFArrayCreate(alloc: *const c_void, vals: *const *const c_void, n: isize, cbs: *const c_void) -> *const c_void;
        fn CFRelease(p: *const c_void);
        static kCFTypeArrayCallBacks: c_void;
    }

    unsafe extern "C" {
        fn dispatch_queue_create(label: *const i8, attr: *const c_void) -> *mut c_void;
    }

    extern "C" fn on_events(_s: *mut c_void, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, ids: *const u64) {
        let tx = unsafe { &*(info as *const Sender<Vec<Event>>) };
        let paths = paths as *const *const i8;
        let batch = (0..n)
            .map(|i| unsafe {
                let mut path = CStr::from_ptr(*paths.add(i)).to_bytes().to_vec();
                while path.len() > 1 && path.ends_with(b"/") {
                    path.pop();
                }
                Event {
                    path,
                    flags: *flags.add(i),
                    id: *ids.add(i),
                }
            })
            .collect();
        let _ = tx.send(batch);
    }

    /// A running stream; dropping it stops it.
    pub struct Stream(*mut c_void);

    // The stream is only started and stopped, never shared mid-call.
    unsafe impl Send for Stream {}
    unsafe impl Sync for Stream {}

    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe {
                FSEventStreamStop(self.0);
                FSEventStreamInvalidate(self.0);
                FSEventStreamRelease(self.0);
            }
        }
    }

    /// Watch `root` from event id `since` (0 = from now). Batches of
    /// directory-level events arrive on `tx` until the stream is dropped.
    pub fn watch(root: &[u8], since: u64, latency: f64, tx: Sender<Vec<Event>>) -> Option<Stream> {
        unsafe {
            let s = CFStringCreateWithBytes(std::ptr::null(), root.as_ptr(), root.len() as isize, 0x0800_0100, 0);
            if s.is_null() {
                return None;
            }
            let arr = CFArrayCreate(std::ptr::null(), &s, 1, &kCFTypeArrayCallBacks as *const c_void);
            // The sender is leaked: a callback may still be in flight when
            // the stream stops, and streams are replaced rarely.
            let ctx = Context {
                version: 0,
                info: Box::into_raw(Box::new(tx)) as *mut c_void,
                retain: std::ptr::null(),
                release: std::ptr::null(),
                copy_description: std::ptr::null(),
            };
            // Not IgnoreSelf: linked into an app, the app's own renames and
            // moves are exactly what its search must see.
            let since = if since == 0 { SINCE_NOW } else { since };
            let st = FSEventStreamCreate(std::ptr::null(), on_events, &ctx, arr, since, latency, CREATE_FLAG_NO_DEFER);
            CFRelease(arr);
            CFRelease(s);
            if st.is_null() {
                return None;
            }
            let q = dispatch_queue_create(c"flashfind.fsevents".as_ptr(), std::ptr::null());
            FSEventStreamSetDispatchQueue(st, q);
            if FSEventStreamStart(st) == 0 {
                FSEventStreamInvalidate(st);
                FSEventStreamRelease(st);
                return None;
            }
            Some(Stream(st))
        }
    }
}
