//! Deterministic synthetic file tree + query set for benchmarks.

use crate::util::Rng;
use rayon::prelude::*;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

const WORDS: &[&str] = &[
    "report",
    "draft",
    "final",
    "invoice",
    "budget",
    "photo",
    "project",
    "notes",
    "meeting",
    "design",
    "client",
    "server",
    "user",
    "service",
    "config",
    "test",
    "data",
    "backup",
    "archive",
    "summary",
    "plan",
    "review",
    "model",
    "index",
    "main",
    "util",
    "helper",
    "api",
    "auth",
    "login",
    "payment",
    "order",
    "cart",
    "search",
    "engine",
    "cache",
    "queue",
    "worker",
    "release",
    "video",
    "audio",
    "music",
    "holiday",
    "family",
    "resume",
    "letter",
    "contract",
    "tax",
    "receipt",
    "manual",
    "guide",
    "spec",
    "schema",
    "migration",
    "script",
    "theme",
    "icon",
    "logo",
    "banner",
    "slide",
    "chart",
    "table",
    "export",
    "import",
    "sync",
    "account",
    "profile",
    "settings",
    "dashboard",
    "analytics",
    "metrics",
    "monitor",
    "alert",
    "router",
    "handler",
    "parser",
    "lexer",
    "compiler",
    "runtime",
    "kernel",
    "driver",
    "network",
    "socket",
    "stream",
    "buffer",
    "vector",
    "matrix",
    "graph",
    "tree",
    "node",
    "edge",
    "cluster",
    "shard",
    "replica",
    "ledger",
    "wallet",
    "token",
    "session",
    "cookie",
    "header",
    "footer",
    "layout",
    "widget",
    "button",
    "dialog",
    "modal",
    "panel",
    "sidebar",
    "toolbar",
    "menu",
    "player",
    "editor",
    "viewer",
    "preview",
    "thumbnail",
    "gallery",
    "album",
    "trip",
    "wedding",
    "birthday",
    "recipe",
    "workout",
    "garden",
    "kitchen",
    "travel",
    "flight",
    "hotel",
    "visa",
    "passport",
    "insurance",
    "medical",
    "school",
    "lecture",
    "homework",
    "thesis",
    "paper",
    "poster",
    "proposal",
    "quote",
    "estimate",
    "timesheet",
    "payroll",
    "roadmap",
    "sprint",
    "backlog",
    "ticket",
    "incident",
    "postmortem",
    "benchmark",
    "profile",
    "trace",
    "sample",
    "fixture",
    "mock",
    "stub",
    "seed",
];
const DIRS: &[&str] = &[
    "Documents",
    "Projects",
    "Photos",
    "Music",
    "Downloads",
    "Desktop",
    "Archive",
    "src",
    "lib",
    "assets",
    "docs",
    "tests",
    "old",
    "misc",
    "work",
    "personal",
    "2021",
    "2022",
    "2023",
    "2024",
    "2025",
    "clients",
    "invoices",
    "scans",
    "exports",
    "drafts",
    "shared",
    "team",
    "notes",
];
const NOISE: &[&str] = &[
    "node_modules",
    ".git",
    "build",
    "Caches",
    ".cache",
    "target",
    "__pycache__",
];
const EXTS: &[(&str, u32)] = &[
    ("txt", 6),
    ("md", 6),
    ("pdf", 8),
    ("docx", 4),
    ("xlsx", 3),
    ("png", 8),
    ("jpg", 10),
    ("jpeg", 2),
    ("heic", 4),
    ("mov", 2),
    ("mp4", 2),
    ("mp3", 2),
    ("swift", 4),
    ("rs", 4),
    ("py", 5),
    ("js", 6),
    ("ts", 5),
    ("tsx", 2),
    ("json", 6),
    ("yaml", 2),
    ("toml", 1),
    ("html", 2),
    ("css", 2),
    ("c", 2),
    ("h", 2),
    ("go", 2),
    ("zip", 2),
    ("dmg", 1),
    ("csv", 2),
    ("log", 3),
];

fn ext(r: &mut Rng) -> &'static str {
    let total: u32 = EXTS.iter().map(|e| e.1).sum();
    let mut x = (r.next() % total as u64) as u32;
    for (e, w) in EXTS {
        if x < *w {
            return e;
        }
        x -= w;
    }
    "txt"
}

fn cap(w: &str) -> String {
    let mut c = w.chars();
    c.next()
        .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
        .unwrap_or_default()
}

fn file_stem(r: &mut Rng) -> String {
    let w = |r: &mut Rng| WORDS[r.below(WORDS.len())];
    match r.below(10) {
        0..=2 => format!("{}_{}", w(r), w(r)),
        3 => format!("{}_{}_v{}", w(r), w(r), 1 + r.below(9)),
        4 | 5 => format!("{}-{}-{}", w(r), w(r), 2015 + r.below(11)),
        6 => format!("{}{}", cap(w(r)), cap(w(r))),
        7 => format!("IMG_{:04}", r.below(10_000)),
        8 => format!("{} {} {}", cap(w(r)), w(r), 1 + r.below(20)),
        _ => format!("{}{}", w(r), r.below(1000)),
    }
}

pub struct GenOpts {
    pub root: PathBuf,
    pub files: usize,
    pub seed: u64,
    pub queries: PathBuf,
}

/// Fixed "now" so a given seed always yields identical mtimes.
pub const GEN_NOW: i64 = 1_788_000_000;

pub fn run(o: &GenOpts) -> std::io::Result<()> {
    let t0 = std::time::Instant::now();
    if o.root.exists() && std::fs::read_dir(&o.root)?.next().is_some() {
        return Err(std::io::Error::other(format!(
            "{} exists and is not empty; remove it first",
            o.root.display()
        )));
    }
    std::fs::create_dir_all(&o.root)?;
    let mut r = Rng(o.seed);
    // --- Directories: (parent, name, depth). Parents always precede children.
    let ndirs = (o.files / 10).max(1);
    let mut dirs: Vec<(u32, String, u32)> = vec![(u32::MAX, String::new(), 0)];
    let mut used: HashSet<(u32, String)> = HashSet::new();
    while dirs.len() < ndirs {
        let mut p = r.below(dirs.len()) as u32;
        // Bias toward deeper trees: half the time descend from a recent dir.
        if r.below(2) == 0 {
            p = (dirs.len() - 1 - r.below(dirs.len().min(64))) as u32;
        }
        if dirs[p as usize].2 >= 12 {
            continue;
        }
        let name = match r.below(20) {
            0 => NOISE[r.below(NOISE.len())].to_string(),
            1..=8 => DIRS[r.below(DIRS.len())].to_string(),
            9..=14 => WORDS[r.below(WORDS.len())].to_string(),
            _ => format!(
                "{}_{}",
                WORDS[r.below(WORDS.len())],
                WORDS[r.below(WORDS.len())]
            ),
        };
        let mut n = name.clone();
        let mut k = 2;
        while !used.insert((p, n.clone())) {
            n = format!("{name} {k}");
            k += 1;
        }
        let depth = dirs[p as usize].2 + 1;
        dirs.push((p, n, depth));
    }
    let mut dpath: Vec<PathBuf> = Vec::with_capacity(dirs.len());
    let mut drel: Vec<String> = Vec::with_capacity(dirs.len());
    for (i, (p, n, _)) in dirs.iter().enumerate() {
        if i == 0 {
            dpath.push(o.root.clone());
            drel.push(String::new());
        } else {
            dpath.push(dpath[*p as usize].join(n));
            let pr = &drel[*p as usize];
            drel.push(if pr.is_empty() {
                n.clone()
            } else {
                format!("{pr}/{n}")
            });
        }
    }
    // --- Files.
    let mut files: Vec<(u32, String, u64, i64)> = Vec::with_capacity(o.files);
    for _ in 0..o.files {
        let d = r.below(dirs.len()) as u32;
        let base = format!("{}.{}", file_stem(&mut r), ext(&mut r));
        let mut n = base.clone();
        let mut k = 2;
        while !used.insert((d, n.clone())) {
            let (s, e) = base.rsplit_once('.').unwrap();
            n = format!("{s} ({k}).{e}");
            k += 1;
        }
        let size = 10f64.powf(1.0 + (r.next() % 8300) as f64 / 1000.0) as u64;
        let mtime = GEN_NOW - (r.next() % (4 * 365 * 86_400)) as i64;
        files.push((d, n, size, mtime));
    }
    drop(used);
    // --- Create on disk.
    for p in dpath.iter().skip(1) {
        std::fs::create_dir(p)?;
    }
    let created = std::sync::atomic::AtomicUsize::new(0);
    files
        .par_chunks(4096)
        .try_for_each(|ch| -> std::io::Result<()> {
            for (d, n, size, mtime) in ch {
                let f = std::fs::File::create(dpath[*d as usize].join(n))?;
                f.set_len(*size)?;
                f.set_modified(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(*mtime as u64),
                )?;
            }
            let c = created.fetch_add(ch.len(), std::sync::atomic::Ordering::Relaxed) + ch.len();
            if c % (100 * 4096) < 4096 {
                eprintln!("  {c} files…");
            }
            Ok(())
        })?;
    let entries = dirs.len() - 1 + files.len();
    eprintln!(
        "generated {} dirs + {} files = {entries} entries in {:.1?}",
        dirs.len() - 1,
        files.len(),
        t0.elapsed()
    );
    write_queries(o, &mut r, &dirs, &drel, &files)?;
    Ok(())
}

fn typo(r: &mut Rng, s: &str, kind: &str) -> Option<String> {
    let mut c: Vec<char> = s.chars().collect();
    let letters: Vec<usize> = (1..c.len())
        .filter(|&i| c[i].is_ascii_alphabetic())
        .collect();
    if letters.is_empty() {
        return None;
    }
    let p = letters[r.below(letters.len())];
    let alpha: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().collect();
    match kind {
        "typo_swap" => {
            let pairs: Vec<usize> = letters
                .iter()
                .copied()
                .filter(|&i| i + 1 < c.len() && c[i + 1].is_ascii_alphabetic() && c[i] != c[i + 1])
                .collect();
            if pairs.is_empty() {
                return None;
            }
            let p = pairs[r.below(pairs.len())];
            c.swap(p, p + 1);
        }
        "typo_drop" => {
            c.remove(p);
        }
        "typo_insert" => c.insert(p, alpha[r.below(26)]),
        "typo_sub" => {
            let mut x = alpha[r.below(26)];
            while x == c[p].to_ascii_lowercase() {
                x = alpha[r.below(26)];
            }
            c[p] = x;
        }
        "typo_first" => {
            // A typo on the first character: substitution or swap with the second.
            if r.below(2) == 0 && c.len() > 1 && c[0] != c[1] {
                c.swap(0, 1);
            } else {
                let mut x = alpha[r.below(26)];
                while x == c[0].to_ascii_lowercase() {
                    x = alpha[r.below(26)];
                }
                c[0] = x;
            }
        }
        _ => unreachable!(),
    }
    Some(c.into_iter().collect())
}

fn write_queries(
    o: &GenOpts,
    r: &mut Rng,
    dirs: &[(u32, String, u32)],
    drel: &[String],
    files: &[(u32, String, u64, i64)],
) -> std::io::Result<()> {
    let mut count: HashMap<String, u32> = HashMap::new();
    for (_, n, _) in dirs.iter().skip(1) {
        *count.entry(n.to_lowercase()).or_default() += 1;
    }
    for (_, n, _, _) in files {
        *count.entry(n.to_lowercase()).or_default() += 1;
    }
    let mut stems: HashMap<String, u32> = HashMap::new();
    for (_, n, _, _) in files {
        if let Some((s, _)) = n.rsplit_once('.') {
            *stems.entry(s.to_lowercase()).or_default() += 1;
        }
    }
    let rel = |d: u32, n: &str| {
        if drel[d as usize].is_empty() {
            n.to_string()
        } else {
            format!("{}/{n}", drel[d as usize])
        }
    };
    // Targets: unique basename and stem, 6–24 char stem, ≥5 letters, plain ASCII, no spaces.
    let pool: Vec<usize> = (0..files.len())
        .filter(|&i| {
            let n = &files[i].1;
            let Some((s, _)) = n.rsplit_once('.') else {
                return false;
            };
            count[&n.to_lowercase()] == 1
                && stems[&s.to_lowercase()] == 1
                && (6..=24).contains(&s.len())
                && s.is_ascii()
                && !s.contains(' ')
                && !s.contains('(')
                && s.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 5
                && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        })
        .collect();
    if pool.len() < 600 {
        return Err(std::io::Error::other(format!(
            "only {} unique targets; generate more files",
            pool.len()
        )));
    }
    let mut picks: Vec<usize> = Vec::new();
    let mut taken = HashSet::new();
    let mut pick = |r: &mut Rng| loop {
        let i = pool[r.below(pool.len())];
        if taken.insert(i) {
            picks.push(i);
            return i;
        }
    };
    let mut out = std::io::BufWriter::new(std::fs::File::create(&o.queries)?);
    let mut emit = |v: serde_json::Value| writeln!(out, "{v}");
    let classes: &[(&str, usize)] = &[
        ("exact", 100),
        ("stem", 100),
        ("typo_swap", 60),
        ("typo_drop", 60),
        ("typo_insert", 60),
        ("typo_sub", 60),
        ("typo_first", 60),
        ("multi", 60),
        ("ext", 40),
    ];
    for &(class, n) in classes {
        let mut done = 0;
        while done < n {
            let i = pick(r);
            let (d, name, _, _) = &files[i];
            let stem = name.rsplit_once('.').unwrap().0;
            let q = match class {
                "exact" => name.clone(),
                "stem" => stem.to_string(),
                "multi" => {
                    if *d == 0
                        || dirs[*d as usize].1.contains(' ')
                        || dirs[*d as usize].1.starts_with('.')
                    {
                        continue;
                    }
                    format!("{} {stem}", dirs[*d as usize].1)
                }
                "ext" => format!("ext:{} {stem}", name.rsplit_once('.').unwrap().1),
                t => match typo(r, stem, t) {
                    Some(q) if q.to_lowercase() != stem.to_lowercase() => q,
                    _ => continue,
                },
            };
            emit(json!({"class": class, "q": q, "expect": rel(*d, name)}))?;
            done += 1;
        }
    }
    let letters = "abcdefghijklmnopqrstuvwxyz";
    for _ in 0..26 {
        let c = &letters[r.below(26)..][..1];
        emit(json!({"class": "short1", "q": c}))?;
    }
    for _ in 0..40 {
        let i = pick(r);
        let s = &files[i].1;
        emit(json!({"class": "short2", "q": s[..2].to_lowercase()}))?;
        emit(json!({"class": "prefix3", "q": s[..3].to_lowercase()}))?;
    }
    for _ in 0..30 {
        let w = WORDS[r.below(WORDS.len())];
        emit(json!({"class": "filter_size", "q": format!("size:>1mb {w}")}))?;
        emit(json!({"class": "filter_mtime", "q": format!("mtime:<1y {w}")}))?;
    }
    for _ in 0..40 {
        let i = pick(r);
        let (d, name, _, _) = &files[i];
        let stem = name.rsplit_once('.').unwrap().0;
        let seq: Vec<String> = (1..=stem.len()).map(|k| stem[..k].to_string()).collect();
        emit(json!({"class": "keys", "seq": seq, "expect": rel(*d, name)}))?;
    }
    eprintln!(
        "wrote queries to {} ({} targets in pool)",
        o.queries.display(),
        pool.len()
    );
    let _ = Path::new("");
    Ok(())
}
