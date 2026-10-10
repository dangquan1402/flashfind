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
//! ```
//!
//! Hits the matcher would rank too low to be fetched at all (the file you
//! picked for `readme` among 300 READMEs) are pulled in by a second, masked
//! search over just the remembered files, so learning works at any scale.

use crate::memory::{Memory, Recall};
use std::collections::HashMap;
use std::sync::Arc;

/// Which signals to use (the eval turns them on one by one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    /// Query → file memory.
    pub memory: bool,
    /// The pre-0.2 additive repo-recency bonus (+30/20/10/4 within a band).
    pub legacy_repo_recency: bool,
}

impl Features {
    /// Everything on (the default).
    pub const ALL: Features = Features {
        memory: true,
        legacy_repo_recency: false,
    };
    /// flashfind 0.1 ranking.
    pub const BASELINE: Features = Features {
        memory: false,
        legacy_repo_recency: true,
    };
}

impl Default for Features {
    fn default() -> Self {
        Features::ALL
    }
}

pub const W_MEMORY: f64 = 60.0;

/// Long-lived ranking state shared by sessions.
#[derive(Clone)]
pub struct Ranker {
    pub memory: Option<Arc<Memory>>,
    pub features: Features,
}

impl Ranker {
    pub fn new(memory: Option<Arc<Memory>>) -> Ranker {
        Ranker {
            memory,
            features: Features::ALL,
        }
    }

    pub fn baseline() -> Ranker {
        Ranker {
            memory: None,
            features: Features::BASELINE,
        }
    }

    /// Per-search inputs for the typed query.
    pub fn input(&self, text: &str) -> RankInput {
        let f = self.features;
        let recall = self.memory.as_ref().filter(|_| f.memory).map(|m| m.recall());
        let qmem = match (&recall, f.memory) {
            (Some(r), true) => r.query_strengths(text),
            _ => HashMap::new(),
        };
        RankInput {
            features: f,
            recall,
            qmem,
        }
    }
}

/// Everything one search needs to personalize scores.
pub struct RankInput {
    pub features: Features,
    pub recall: Option<Arc<Recall>>,
    /// Path → learned strength for the typed query.
    pub qmem: HashMap<String, f64>,
}

impl RankInput {
    pub fn legacy() -> RankInput {
        RankInput {
            features: Features::BASELINE,
            recall: None,
            qmem: HashMap::new(),
        }
    }

    /// Paths worth scoring even if the matcher wouldn't fetch them.
    pub fn candidates(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.qmem.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// (boost %, reasons) for one hit.
    pub fn boost(&self, path: &str) -> (f64, Vec<&'static str>) {
        let mut p = 0.0;
        let mut why = Vec::new();
        if self.features.memory
            && let Some(&s) = self.qmem.get(path)
        {
            p += W_MEMORY * s;
            why.push("picked before");
        }
        (p, why)
    }
}

/// `score + score · pct / 100`, saturating.
pub fn apply(score: i32, pct: f64) -> i32 {
    let s = score as f64;
    (s + s * pct / 100.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}
