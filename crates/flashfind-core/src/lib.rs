//! flashfind-core: a whole-disk file name index and search engine built for
//! in-process, per-keystroke use (see the crate README).

pub mod index;
pub mod live;
pub mod matcher;
pub mod query;
pub mod search;
pub mod walk;
pub mod watch;

pub use index::{Index, build_from_disk};
pub use live::{Live, LiveHit, LiveOptions, LiveResult, LiveSession, Phase, Progress};
pub use query::{Clock, Query};
pub use search::{CancelToken, Cancelled, Hit, SearchResult, Session, Strategy, search};

/// Hand memory freed by big transient work (crawls, index builds,
/// compactions) back to the OS instead of letting the allocator keep it
/// (macOS: `malloc_zone_pressure_relief`, as fsearch does; glibc:
/// `malloc_trim`). Cheap; a no-op elsewhere.
pub fn release_memory() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        unsafe { malloc_trim(0) };
    }
}
