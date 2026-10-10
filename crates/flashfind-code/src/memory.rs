//! What you picked and what you touched: a small, durable, local log that
//! the ranking learns from.
//!
//! * **Picks**: a file you opened / copied / revealed from a result list,
//!   with the query you typed. Recorded by the app, the CLI
//!   (`flashfind pick`), the C API and the MCP server (`record_pick`).
//! * **Touches**: files modified while a live index is watching (FSEvents),
//!   with a 5-minute cooldown per file so burst edits don't inflate scores.
//!
//! Stored as append-only JSON lines in `$FLASHFIND_DATA`, else
//! `$XDG_DATA_HOME/flashfind`, else `~/.local/share/flashfind`
//! (`memory.jsonl`). Several processes (app, MCP server, CLI) share it:
//! each appends whole lines and re-reads the tail when the file grows.
//!
//! Derived signals ([`Recall`]):
//! * query → file memory: for the typed query, files previously picked for
//!   the same query, a query it is a prefix of (`aut` → picks made with
//!   `auth`), or a shorter query it refines; top 5 files per query, learning
//!   from the first pick, decayed with a 30-day half-life;
//! * per-file frecency: Σ weight·2^(-age/10 days) over the last 60 days
//!   (pick = 1, touch = 0.5), the idea fff uses (dmtrKovalenko/fff,
//!   `crates/fff-core/src/dbs/frecency.rs`).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const FRECENCY_HALF_LIFE_DAYS: f64 = 10.0;
const QUERY_HALF_LIFE_DAYS: f64 = 30.0;
const MAX_AGE_DAYS: i64 = 60;
const TOUCH_COOLDOWN: i64 = 300;
const TOP_PER_QUERY: usize = 5;
const COMPACT_LINES: usize = 50_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    /// Unix seconds.
    pub t: i64,
    /// "pick" or "touch".
    pub k: String,
    /// Absolute path.
    pub p: String,
    /// Query typed (picks).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub q: String,
    /// open | copy | reveal | cursor | read | …
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub a: String,
    /// app | cli | mcp | ffi | eval
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s: String,
}

#[derive(Default)]
struct State {
    read: u64,
    events: Vec<Event>,
    last_touch: HashMap<String, i64>,
    version: u64,
    recall: Option<(u64, i64, Arc<Recall>)>,
}

/// The pick/touch log (thread-safe; cheap to share as `Arc<Memory>`).
pub struct Memory {
    path: Option<PathBuf>,
    st: Mutex<State>,
    clock: Mutex<Option<i64>>,
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Normalize a query for memory keys: lowercase, single spaces, no
/// trailing `:line[:col]`, no leading/trailing punctuation.
pub fn norm_query(q: &str) -> String {
    let mut words: Vec<String> = q.split_whitespace().map(|w| w.to_lowercase()).collect();
    if let Some(last) = words.last_mut() {
        *last = strip_line_suffix(last).0.to_string();
    }
    words.retain(|w| !w.is_empty());
    words.join(" ")
}

/// `foo.rs:12:3` → (`foo.rs`, Some(12), Some(3)).
pub fn strip_line_suffix(s: &str) -> (&str, Option<u32>, Option<u32>) {
    let mut parts = s.rsplitn(3, ':');
    let a = parts.next();
    let b = parts.next();
    let c = parts.next();
    match (c, b, a) {
        (Some(base), Some(l), Some(col)) if !base.is_empty() && l.parse::<u32>().is_ok() && col.parse::<u32>().is_ok() => {
            (base, l.parse().ok(), col.parse().ok())
        }
        (_, Some(_), Some(l)) if l.parse::<u32>().is_ok() => {
            let base = &s[..s.len() - l.len() - 1];
            if base.is_empty() { (s, None, None) } else { (base, l.parse().ok(), None) }
        }
        _ => (s, None, None),
    }
}

impl Memory {
    /// Default data folder (see module docs).
    pub fn default_dir() -> PathBuf {
        if let Some(p) = std::env::var_os("FLASHFIND_DATA") {
            return PathBuf::from(p);
        }
        if let Some(p) = std::env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()) {
            return PathBuf::from(p).join("flashfind");
        }
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local/share/flashfind")
    }

    /// Open (or create) `dir/memory.jsonl`.
    pub fn open(dir: &Path) -> Arc<Memory> {
        let _ = std::fs::create_dir_all(dir);
        let m = Memory {
            path: Some(dir.join("memory.jsonl")),
            st: Mutex::new(State::default()),
            clock: Mutex::new(None),
        };
        m.refresh();
        m.maybe_compact();
        Arc::new(m)
    }

    pub fn open_default() -> Arc<Memory> {
        Memory::open(&Memory::default_dir())
    }

    /// Memory only (tests, eval).
    pub fn in_memory() -> Arc<Memory> {
        Arc::new(Memory {
            path: None,
            st: Mutex::new(State::default()),
            clock: Mutex::new(None),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Fix the clock (simulations).
    pub fn set_clock(&self, t: Option<i64>) {
        *self.clock.lock().unwrap() = t;
    }

    pub fn now(&self) -> i64 {
        self.clock.lock().unwrap().unwrap_or_else(now)
    }

    /// Pick up lines other processes appended since we last looked.
    pub fn refresh(&self) {
        let Some(p) = &self.path else { return };
        let Ok(md) = std::fs::metadata(p) else { return };
        let mut st = self.st.lock().unwrap();
        let len = md.len();
        if len == st.read {
            return;
        }
        if len < st.read {
            // Rewritten (compaction or `forget`): reload.
            st.events.clear();
            st.last_touch.clear();
            st.read = 0;
        }
        let Ok(mut f) = std::fs::File::open(p) else { return };
        if f.seek(std::io::SeekFrom::Start(st.read)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.take(len - st.read).read_to_end(&mut buf).is_err() {
            return;
        }
        // Only whole lines: a writer may be mid-line.
        let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        for line in buf[..end].split(|&b| b == b'\n') {
            if let Ok(e) = serde_json::from_slice::<Event>(line) {
                if e.k == "touch" {
                    st.last_touch.insert(e.p.clone(), e.t);
                }
                st.events.push(e);
            }
        }
        st.read += end as u64;
        st.version += 1;
    }

    fn append(&self, e: Event) {
        let mut line = serde_json::to_vec(&e).unwrap();
        line.push(b'\n');
        if let Some(p) = &self.path {
            self.refresh();
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                // One write per line keeps concurrent appenders from interleaving.
                if f.write_all(&line).is_ok() {
                    self.refresh();
                    return;
                }
            }
        }
        let mut st = self.st.lock().unwrap();
        if e.k == "touch" {
            st.last_touch.insert(e.p.clone(), e.t);
        }
        st.events.push(e);
        st.version += 1;
    }

    /// Record that `path` was chosen for `query` (`action`: open, copy,
    /// reveal, cursor, read…; `source`: app, cli, mcp, ffi).
    pub fn record_pick(&self, query: &str, path: &str, action: &str, source: &str) {
        self.append(Event {
            t: self.now(),
            k: "pick".into(),
            p: path.to_string(),
            q: norm_query(query),
            a: action.to_string(),
            s: source.to_string(),
        });
    }

    /// Record a modification (cooldown 5 min per file). Returns whether it
    /// was recorded.
    pub fn record_touch(&self, path: &str) -> bool {
        let t = self.now();
        self.refresh();
        if self.st.lock().unwrap().last_touch.get(path).is_some_and(|&l| t - l < TOUCH_COOLDOWN) {
            return false;
        }
        self.append(Event {
            t,
            k: "touch".into(),
            p: path.to_string(),
            q: String::new(),
            a: String::new(),
            s: String::new(),
        });
        true
    }

    /// Delete everything.
    pub fn forget(&self) {
        if let Some(p) = &self.path {
            let _ = std::fs::write(p, b"");
        }
        let mut st = self.st.lock().unwrap();
        *st = State {
            version: st.version + 1,
            ..State::default()
        };
    }

    pub fn len(&self) -> usize {
        self.refresh();
        self.st.lock().unwrap().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn events(&self) -> Vec<Event> {
        self.refresh();
        self.st.lock().unwrap().events.clone()
    }

    /// Drop events older than 60 days when the log is long.
    fn maybe_compact(&self) {
        let Some(p) = &self.path else { return };
        let st = self.st.lock().unwrap();
        if st.events.len() < COMPACT_LINES {
            return;
        }
        let cutoff = now() - MAX_AGE_DAYS * 86_400;
        let keep: Vec<&Event> = st.events.iter().filter(|e| e.t >= cutoff).collect();
        let tmp = p.with_extension("jsonl.tmp");
        let mut out = Vec::new();
        for e in keep {
            out.extend(serde_json::to_vec(e).unwrap());
            out.push(b'\n');
        }
        drop(st);
        if std::fs::write(&tmp, &out).is_ok() && std::fs::rename(&tmp, p).is_ok() {
            self.refresh();
        }
    }

    /// Derived signals at the current time (cached until the log changes or
    /// a minute passes).
    pub fn recall(&self) -> Arc<Recall> {
        self.refresh();
        let t = self.now();
        let mut st = self.st.lock().unwrap();
        if let Some((g, at, r)) = &st.recall
            && *g == st.version
            && (t - at).abs() < 60
        {
            return r.clone();
        }
        let r = Arc::new(Recall::build(&st.events, t));
        st.recall = Some((st.version, t, r.clone()));
        r
    }
}

/// Snapshot of what the log says, ready for ranking.
#[derive(Default, Debug)]
pub struct Recall {
    pub now: i64,
    /// Normalized query → [(path, decayed pick count, picks)], best first.
    pub queries: HashMap<String, Vec<(String, f64, u32)>>,
    /// Path → raw frecency (Σ weight · 2^(-age/10d)).
    pub frecency: HashMap<String, f64>,
    /// Path → (picks, last pick time).
    pub picks: HashMap<String, (u32, i64)>,
}

fn decay(age_s: i64, half_life_days: f64) -> f64 {
    (-(age_s.max(0) as f64 / 86_400.0) * std::f64::consts::LN_2 / half_life_days).exp()
}

impl Recall {
    pub fn build(events: &[Event], now: i64) -> Recall {
        let cutoff = now - MAX_AGE_DAYS * 86_400;
        let mut r = Recall {
            now,
            ..Default::default()
        };
        let mut q: HashMap<String, HashMap<String, (f64, u32)>> = HashMap::new();
        for e in events.iter().filter(|e| e.t >= cutoff && e.t <= now + 60) {
            let age = now - e.t;
            match e.k.as_str() {
                "pick" => {
                    *r.frecency.entry(e.p.clone()).or_default() += decay(age, FRECENCY_HALF_LIFE_DAYS);
                    let p = r.picks.entry(e.p.clone()).or_default();
                    p.0 += 1;
                    p.1 = p.1.max(e.t);
                    if !e.q.is_empty() {
                        let s = q.entry(e.q.clone()).or_default().entry(e.p.clone()).or_default();
                        s.0 += decay(age, QUERY_HALF_LIFE_DAYS);
                        s.1 += 1;
                    }
                }
                "touch" => *r.frecency.entry(e.p.clone()).or_default() += 0.5 * decay(age, FRECENCY_HALF_LIFE_DAYS),
                _ => {}
            }
        }
        for (k, m) in q {
            let mut v: Vec<(String, f64, u32)> = m.into_iter().map(|(p, (c, n))| (p, c, n)).collect();
            v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v.truncate(TOP_PER_QUERY);
            r.queries.insert(k, v);
        }
        r
    }

    /// Learned strength (0..1) of each file for the typed query.
    ///
    /// Stored query `S` vs typed `q`: equal → weight 1; `q` is a prefix of
    /// `S` (still typing) → 0.4 + 0.6·|q|/|S|; `S` is a prefix of `q`
    /// (refining) → 0.5. strength = 1 − e^(−Σ weight · decayed picks), so one
    /// exact pick gives 0.63 and three give 0.95. Within one query, files
    /// picked less often than the favourite are scaled down.
    pub fn query_strengths(&self, typed: &str) -> HashMap<String, f64> {
        let q = norm_query(typed);
        let mut acc: HashMap<String, f64> = HashMap::new();
        if q.is_empty() {
            return acc;
        }
        for (s, files) in &self.queries {
            let w = if *s == q {
                1.0
            } else if q.chars().count() >= 2 && s.starts_with(&q) {
                0.4 + 0.6 * q.len() as f64 / s.len() as f64
            } else if s.chars().count() >= 3 && q.starts_with(s.as_str()) {
                0.5
            } else {
                continue;
            };
            let best = files.first().map_or(1.0, |f| f.1.max(1e-9));
            for (p, c, _) in files {
                *acc.entry(p.clone()).or_default() += w * c * (c / best).sqrt();
            }
        }
        acc.into_iter().map(|(p, x)| (p, 1.0 - (-x).exp())).collect()
    }

    /// Frecency of a path normalized to 0..1 (1 − e^(−f/3)).
    pub fn frecency_norm(&self, path: &str) -> f64 {
        self.frecency.get(path).map_or(0.0, |f| 1.0 - (-f / 3.0).exp())
    }

    /// Σ frecency of files under `root` (normalized 0..1 with 1 − e^(−F/10)).
    pub fn repo_frecency_norm(&self, root: &str) -> f64 {
        let mut f = 0.0;
        for (p, v) in &self.frecency {
            if p.len() > root.len() && p.starts_with(root) && p.as_bytes()[root.len()] == b'/' {
                f += v;
            }
        }
        1.0 - (-f / 10.0).exp()
    }

    /// The most frecent paths, best first.
    pub fn top_frecent(&self, n: usize) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = self.frecency.iter().map(|(p, f)| (p.clone(), *f)).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}
