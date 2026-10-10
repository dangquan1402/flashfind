//! The three engines behind one interface. Each returns its top `limit`
//! paths relative to the dataset root, so results are compared the same way.

use flashfind_code::{CodeIndex, CodeQuery, Config};
use flashfind_core::{CancelToken, Session};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    Flashfind,
    Fff,
    Fsearch,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Flashfind => "flashfind",
            Kind::Fff => "fff",
            Kind::Fsearch => "fsearch",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "flashfind" => Some(Kind::Flashfind),
            "fff" => Some(Kind::Fff),
            "fsearch" => Some(Kind::Fsearch),
            _ => None,
        }
    }

    pub fn available(self) -> bool {
        self != Kind::Fsearch || cfg!(target_os = "macos")
    }
}

pub enum Engine {
    /// flashfind's code-projects index: gitignore-aware, junk excluded.
    Flashfind { ci: CodeIndex, sess: Session, root: String },
    Fff {
        picker: fff_search::SharedFilePicker,
        _frecency: fff_search::SharedFrecency,
    },
    #[cfg(target_os = "macos")]
    Fsearch { live: fsearch::live::Live, home: String },
}

pub struct Built {
    pub engine: Engine,
    pub entries: usize,
    pub build: Duration,
}

pub fn code_config(root: &Path) -> Config {
    Config {
        roots: vec![root.to_path_buf()],
        ..Config::default()
    }
}

pub fn build(kind: Kind, root: &Path) -> Built {
    let t0 = Instant::now();
    match kind {
        Kind::Flashfind => {
            let ci = CodeIndex::build(&code_config(root));
            let entries = ci.repos.iter().map(|r| r.files as usize).sum();
            let sess = Session::new(ci.idx.clone());
            Built {
                engine: Engine::Flashfind {
                    ci,
                    sess,
                    root: format!("{}/", root.display()),
                },
                entries,
                build: t0.elapsed(),
            }
        }
        Kind::Fff => {
            use fff_search::{FFFMode, FilePickerOptions, SharedFilePicker, SharedFrecency, file_picker::FilePicker};
            let picker = SharedFilePicker::default();
            let frecency = SharedFrecency::default();
            // Name search only: no content indexing / mmap warmup, no watcher
            // (fff's defaults for its Python package turn both on; they cost
            // startup time and memory but do not change name search).
            FilePicker::new_with_shared_state(
                picker.clone(),
                frecency.clone(),
                FilePickerOptions {
                    base_path: root.to_string_lossy().into_owned(),
                    enable_mmap_cache: false,
                    enable_content_indexing: false,
                    watch: false,
                    mode: FFFMode::default(),
                    ..Default::default()
                },
            )
            .expect("fff picker");
            picker.wait_for_scan(Duration::from_secs(1800));
            let build = t0.elapsed();
            let entries = picker
                .read()
                .ok()
                .and_then(|g| g.as_ref().map(|p| p.get_scan_progress().scanned_files_count))
                .unwrap_or(0);
            Built {
                engine: Engine::Fff { picker, _frecency: frecency },
                entries,
                build,
            }
        }
        Kind::Fsearch => {
            #[cfg(target_os = "macos")]
            {
                let home = std::env::var("HOME").unwrap_or_default();
                let rb = root.to_string_lossy().into_owned().into_bytes();
                let threads = std::thread::available_parallelism().map_or(8, |n| n.get());
                let ls = fsearch::walk::scan(&rb, threads);
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as u32;
                let idx = fsearch::index::Index::build(ls, 0, now, home.as_bytes());
                let entries = idx.n;
                let live = fsearch::live::Live::new(idx);
                Built {
                    engine: Engine::Fsearch { live, home },
                    entries,
                    build: t0.elapsed(),
                }
            }
            #[cfg(not(target_os = "macos"))]
            unreachable!("fsearch is macOS-only")
        }
    }
}

impl Engine {
    pub fn kind(&self) -> Kind {
        match self {
            Engine::Flashfind { .. } => Kind::Flashfind,
            Engine::Fff { .. } => Kind::Fff,
            #[cfg(target_os = "macos")]
            Engine::Fsearch { .. } => Kind::Fsearch,
        }
    }

    /// Top `limit` paths, relative to the dataset root.
    pub fn search(&mut self, q: &str, limit: usize) -> Vec<String> {
        match self {
            Engine::Flashfind { ci, sess, root } => {
                let mut cq = CodeQuery::parse(q);
                cq.limit = limit;
                match ci.search(sess, &cq, &CancelToken::new()) {
                    Ok(r) => r
                        .hits
                        .into_iter()
                        .map(|h| h.path.strip_prefix(root.as_str()).map(str::to_string).unwrap_or(h.path))
                        .collect(),
                    Err(_) => Vec::new(),
                }
            }
            Engine::Fff { picker, .. } => {
                use fff_search::{FuzzySearchOptions, PaginationArgs, QueryParser};
                let Ok(guard) = picker.read() else { return Vec::new() };
                let Some(p) = guard.as_ref() else { return Vec::new() };
                let parsed = QueryParser::default().parse(q);
                // Same options as fff's Python binding defaults.
                let r = p.fuzzy_search(
                    &parsed,
                    None,
                    FuzzySearchOptions {
                        max_threads: 0,
                        current_file: None,
                        project_path: Some(p.base_path()),
                        combo_boost_score_multiplier: 100,
                        min_combo_count: 3,
                        pagination: PaginationArgs { offset: 0, limit },
                    },
                );
                r.items.iter().map(|i| i.relative_path(p)).collect()
            }
            #[cfg(target_os = "macos")]
            Engine::Fsearch { live, home } => {
                let Ok(mut fq) = fsearch::query::Query::parse(q, home) else {
                    return Vec::new();
                };
                fq.limit = limit;
                let s = fsearch::query::Searcher { live };
                let hits = s.search(&fq);
                let mut b = Vec::new();
                hits.iter()
                    .map(|h| {
                        live.base.path(h.idx as usize, &mut b);
                        String::from_utf8_lossy(b.strip_prefix(b"/").unwrap_or(&b)).into_owned()
                    })
                    .collect()
            }
        }
    }

    /// Warm start: persist the index, then time load + first answer.
    /// fff keeps no index on disk (it rescans on every start): None.
    pub fn warm_start(&self, root: &Path, tmp: &Path) -> Option<Duration> {
        match self {
            Engine::Flashfind { ci, .. } => {
                let dir = tmp.join("flashfind-cache");
                ci.save(&dir).ok()?;
                let t = Instant::now();
                let ci2 = CodeIndex::load(&dir).ok()?;
                let mut e = Engine::Flashfind {
                    sess: Session::new(ci2.idx.clone()),
                    ci: ci2,
                    root: format!("{}/", root.display()),
                };
                e.search("makefile", 50);
                Some(t.elapsed())
            }
            Engine::Fff { .. } => None,
            #[cfg(target_os = "macos")]
            Engine::Fsearch { live, home } => {
                let f = tmp.join("fsearch.idx");
                live.base.save(&f).ok()?;
                let t = Instant::now();
                let idx = fsearch::index::Index::load(&f)?;
                let mut e = Engine::Fsearch {
                    live: fsearch::live::Live::new(idx),
                    home: home.clone(),
                };
                e.search("makefile", 50);
                Some(t.elapsed())
            }
        }
    }
}

/// Physical memory footprint of this process (what Activity Monitor shows on
/// macOS; peak RSS elsewhere).
pub fn footprint_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        if libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, &mut info as *mut _ as *mut libc::rusage_info_t) == 0 {
            return info.ri_phys_footprint;
        }
        0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let pages: u64 = s.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
        pages * 4096
    }
}

pub fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("flashfind-vs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}
