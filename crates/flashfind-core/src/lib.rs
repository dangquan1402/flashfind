//! flashfind-core: a whole-disk file name index and search engine built for
//! in-process, per-keystroke use (see the crate README).

pub mod index;
pub mod matcher;
pub mod query;
pub mod search;
pub mod walk;

pub use index::{Index, build_from_disk};
pub use query::{Clock, Query};
pub use search::{CancelToken, Cancelled, Hit, SearchResult, Session, Strategy, search};
