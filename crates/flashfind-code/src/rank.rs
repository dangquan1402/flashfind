//! Personal ranking on top of the name matcher.
//!
//! The matcher gives every hit a match score in fixed bands (exact 1000,
//! stem 900, prefix 800, word 700, substring 600, fuzzy ≤560, typo ≤545).
//! Personal signals then add a *percentage of that score*, so they reorder
//! hits within and across nearby bands but a weak match can't ride them past
//! a much better one (the multiplicative design fff uses for frecency):
//!
//! ```text
//! total = match + match · boost% / 100
//! boost% = 60 · query_memory          (0..1: picks for this query or one it prefixes)
//!        + 25 · frecency              (0..1: decayed picks + edits of this file, 10-day half-life)
//!        +  5 · repo frecency + ≤5 repo recency   (mild project-level terms)
//!        + 15 if in the current project (+5 more under the current folder)
//!        + 10 if it has uncommitted changes, + 8 / 4 if in the last 3 / 10 commits
//! ```
//!
//! Hits the matcher would rank too low to be fetched at all (the file you
//! picked for `readme` among 300 READMEs) are pulled in by a second, masked
//! search over just the remembered files, so learning works at any scale.

use crate::git::{GitCache, GitInfo};
use crate::memory::{Memory, Recall};
use crate::{CodeHit, Repo};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Which signals to use (the eval turns them on one by one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    /// Query → file memory.
    pub memory: bool,
    /// Per-file frecency (picks + edits) and the mild repo terms.
    pub frecency: bool,
    /// Current-project boost (from `cwd` / the frontmost app).
    pub context: bool,
    /// Uncommitted and recently committed files.
    pub git: bool,
    /// The pre-0.2 additive repo-recency bonus (+30/20/10/4 within a band).
    pub legacy_repo_recency: bool,
}

impl Features {
    /// Everything on (the default).
    pub const ALL: Features = Features {
        memory: true,
        frecency: true,
        context: true,
        git: true,
        legacy_repo_recency: false,
    };
    /// flashfind 0.1 ranking.
    pub const BASELINE: Features = Features {
        memory: false,
        frecency: false,
        context: false,
        git: false,
        legacy_repo_recency: true,
    };
}

impl Default for Features {
    fn default() -> Self {
        Features::ALL
    }
}

pub const W_MEMORY: f64 = 60.0;
pub const W_FRECENCY: f64 = 25.0;
pub const W_REPO_FRECENCY: f64 = 5.0;
pub const W_PROJECT: f64 = 15.0;
pub const W_FOLDER: f64 = 5.0;
pub const W_DIRTY: f64 = 10.0;
pub const W_RECENT_COMMIT: (f64, f64) = (8.0, 4.0);

/// Mild repo recency (percent) for a repo active `age` seconds ago.
pub fn repo_recency_pct(age: i64) -> f64 {
    match age {
        a if a < 86_400 => 5.0,
        a if a < 7 * 86_400 => 3.0,
        a if a < 30 * 86_400 => 1.5,
        a if a < 180 * 86_400 => 0.5,
        _ => 0.0,
    }
}

/// Long-lived ranking state shared by sessions.
#[derive(Clone)]
pub struct Ranker {
    pub memory: Option<Arc<Memory>>,
    pub features: Features,
    /// Current folder (explicit `--cwd` / MCP `cwd`, or detected project).
    pub cwd: Option<PathBuf>,
    pub git: Arc<GitCache>,
}

impl Ranker {
    pub fn new(memory: Option<Arc<Memory>>) -> Ranker {
        Ranker {
            memory,
            features: Features::ALL,
            cwd: None,
            git: GitCache::new(),
        }
    }

    pub fn baseline() -> Ranker {
        Ranker {
            memory: None,
            features: Features::BASELINE,
            cwd: None,
            git: GitCache::new(),
        }
    }

    /// Per-search inputs for the typed query (`repos`: to resolve `cwd`).
    pub fn input(&self, text: &str, repos: &[Repo]) -> RankInput {
        let f = self.features;
        let recall = self.memory.as_ref().filter(|_| f.memory || f.frecency).map(|m| m.recall());
        let qmem = match (&recall, f.memory) {
            (Some(r), true) => r.query_strengths(text),
            _ => HashMap::new(),
        };
        let frecent = match (&recall, f.frecency) {
            (Some(r), true) => r.top_frecent(200).into_iter().map(|(p, _)| p).collect(),
            _ => Vec::new(),
        };
        let ctx = if f.context || f.git {
            self.cwd.as_deref().and_then(|c| crate::context::repo_for(c, repos)).map(|r| r.path.clone())
        } else {
            None
        };
        let ctx_dir = match (&ctx, &self.cwd) {
            (Some(r), Some(c)) if c != r && c.starts_with(r) => Some(c.to_string_lossy().into_owned() + "/"),
            _ => None,
        };
        let ctx_git = match (&ctx, f.git) {
            (Some(r), true) => self.git.get(r, true),
            _ => None,
        };
        let mut ctx_files: Vec<String> = Vec::new();
        if let Some(g) = &ctx_git {
            ctx_files.extend(g.dirty.iter().take(100).cloned());
            let mut rec: Vec<(&String, &usize)> = g.recent.iter().collect();
            rec.sort_by_key(|(p, i)| (**i, (*p).clone()));
            ctx_files.extend(rec.into_iter().take(100).map(|(p, _)| p.clone()));
        }
        RankInput {
            features: f,
            now: self.memory.as_ref().map_or_else(crate::memory::now, |m| m.now()),
            recall,
            qmem,
            frecent,
            repo_frec: Default::default(),
            ctx_root: ctx.as_ref().map(|r| r.to_string_lossy().into_owned()),
            ctx_dir,
            ctx_files,
            git: f.git.then(|| self.git.clone()),
            git_infos: Default::default(),
        }
    }
}

/// Everything one search needs to personalize scores.
pub struct RankInput {
    pub features: Features,
    pub now: i64,
    pub recall: Option<Arc<Recall>>,
    /// Path → learned strength for the typed query.
    pub qmem: HashMap<String, f64>,
    /// The most frecent files (candidates).
    pub frecent: Vec<String>,
    repo_frec: Mutex<HashMap<String, f64>>,
    /// Root of the current project's repo.
    pub ctx_root: Option<String>,
    /// Current folder below that root (with trailing '/').
    pub ctx_dir: Option<String>,
    /// Dirty / recently committed files of the current project (candidates).
    pub ctx_files: Vec<String>,
    git: Option<Arc<GitCache>>,
    git_infos: Mutex<HashMap<String, Option<Arc<GitInfo>>>>,
}

impl RankInput {
    pub fn legacy() -> RankInput {
        RankInput {
            features: Features::BASELINE,
            now: crate::memory::now(),
            recall: None,
            qmem: HashMap::new(),
            frecent: Vec::new(),
            repo_frec: Default::default(),
            ctx_root: None,
            ctx_dir: None,
            ctx_files: Vec::new(),
            git: None,
            git_infos: Default::default(),
        }
    }

    /// Whether any personal signal is on (else the empty state stays empty).
    pub fn personal(&self) -> bool {
        let f = self.features;
        f.memory || f.frecency || f.context || f.git
    }

    /// Candidates for the empty state (nothing typed yet): frecent and
    /// recently picked files plus the current project's dirty / recently
    /// committed files.
    pub fn suggestions(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.frecent.iter().map(String::as_str).collect();
        if let Some(r) = &self.recall {
            let mut picks: Vec<(&String, &(u32, i64))> = r.picks.iter().collect();
            picks.sort_by(|a, b| b.1.1.cmp(&a.1.1));
            v.extend(picks.into_iter().take(50).map(|(p, _)| p.as_str()));
        }
        v.extend(self.ctx_files.iter().map(String::as_str));
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Paths worth scoring even if the matcher wouldn't fetch them.
    pub fn candidates(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.qmem.keys().map(String::as_str).collect();
        v.extend(self.frecent.iter().map(String::as_str));
        v.extend(self.ctx_files.iter().map(String::as_str));
        v.sort_unstable();
        v.dedup();
        v
    }

    /// (boost %, reasons) for one hit.
    pub fn boost(&self, h: &CodeHit) -> (f64, Vec<&'static str>) {
        let mut p = 0.0;
        let mut why = Vec::new();
        if self.features.memory
            && let Some(&s) = self.qmem.get(&h.path)
        {
            p += W_MEMORY * s;
            why.push("picked before");
        }
        if self.features.frecency {
            if let Some(r) = &self.recall {
                let f = r.frecency_norm(&h.path);
                p += W_FRECENCY * f;
                if f > 0.25 {
                    why.push("frequent");
                }
                let rf = *self
                    .repo_frec
                    .lock()
                    .unwrap()
                    .entry(h.repo_root.clone())
                    .or_insert_with(|| r.repo_frecency_norm(&h.repo_root));
                p += W_REPO_FRECENCY * rf;
            }
            p += repo_recency_pct(self.now - h.repo_active);
        }
        if self.features.context
            && let Some(root) = &self.ctx_root
            && h.repo_root == *root
        {
            p += W_PROJECT;
            if self.ctx_dir.as_ref().is_some_and(|d| h.path.starts_with(d.as_str())) {
                p += W_FOLDER;
            }
            why.push("this project");
        }
        if let Some(g) = self.git_info(&h.repo_root) {
            if g.is_dirty(&h.path) {
                p += W_DIRTY;
                why.push("uncommitted");
            }
            if let Some(&i) = g.recent.get(&h.path) {
                p += if i < 3 { W_RECENT_COMMIT.0 } else { W_RECENT_COMMIT.1 };
                why.push("recent commit");
            }
        }
        (p, why)
    }

    /// Git info for a repo: the current project's is loaded synchronously,
    /// others come from the background cache when ready.
    pub fn git_info(&self, root: &str) -> Option<Arc<GitInfo>> {
        let git = self.git.as_ref()?;
        self.git_infos
            .lock()
            .unwrap()
            .entry(root.to_string())
            .or_insert_with(|| git.get(Path::new(root), self.ctx_root.as_deref() == Some(root)))
            .clone()
    }
}

/// `score + score · pct / 100`, saturating.
pub fn apply(score: i32, pct: f64) -> i32 {
    let s = score as f64;
    (s + s * pct / 100.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}
