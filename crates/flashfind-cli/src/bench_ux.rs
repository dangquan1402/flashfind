//! `flashfind bench-ux`: offline replay of a pick history, scoring how fast
//! the file you meant comes up under each ranking variant.
//!
//! Two sources of "intended files":
//! * `--synthetic` (default): a generated home of repos with realistic name
//!   collisions (every repo has README.md, index.ts, config.rs, …) and a
//!   simulated user who works in a few active projects for 30 days, opens
//!   files from a per-project working set (Zipf), edits some of them and
//!   commits at the end of the day;
//! * `--real`: the git history of your most active real repos (last
//!   `--days` days): every file changed in a commit is treated as a file you
//!   opened shortly before that commit, in time order, with the project of
//!   the previous task as the current context.
//!
//! The simulated user types the file's stem one character at a time and
//! opens it as soon as it is visible in the top 8 (arrow keys to reach it).
//! Per task and variant we record:
//! * rank of the file for the full stem query (right-file-first = rank 1),
//! * keystrokes-to-open = min over prefixes k of (k chars + rank−1 arrows),
//!   k = 0 meaning picked from the empty state.
//!
//! Every variant replays the same tasks with its own memory, recording the
//! pick under the query actually typed. Numbers are only as realistic as the
//! user model; see bench/ux/README.md.

use crate::util::Rng;
use crate::{Args, die};
use flashfind_code::git::GitInfo;
use flashfind_code::{CodeIndex, CodeLive, CodeQuery, CodeSession, Config, Features, Memory, Ranker};
use flashfind_core::CancelToken;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const VISIBLE: usize = 8;
const DEPTH: usize = 50;

#[derive(Clone, Debug)]
pub struct Task {
    pub t: i64,
    /// Absolute path of the intended file.
    pub path: String,
    pub repo: PathBuf,
    /// What the user types (full).
    pub query: String,
    /// Files modified right after (touch events), with their time.
    pub touches: Vec<(i64, String)>,
    /// Measured (false = warm-up only).
    pub measure: bool,
    /// Current project when the task starts (where the previous task was).
    pub ctx: Option<PathBuf>,
    /// Git state of repos at task time (dirty files, recent commits).
    pub git: Vec<(PathBuf, GitInfo)>,
}

#[derive(Clone, Copy)]
struct Variant {
    name: &'static str,
    f: Features,
    /// Empty-state suggestions count as k = 0.
    empty_state: bool,
}

fn variants() -> Vec<Variant> {
    let mut v = vec![Variant {
        name: "0.1 baseline",
        f: Features::BASELINE,
        empty_state: false,
    }];
    let mut f = Features::BASELINE;
    f.legacy_repo_recency = true;
    f.memory = true;
    v.push(Variant {
        name: "+ query memory (F1)",
        f,
        empty_state: false,
    });
    f.legacy_repo_recency = false;
    f.frecency = true;
    v.push(Variant {
        name: "+ file frecency, mild repo terms (F2)",
        f,
        empty_state: false,
    });
    f.context = true;
    f.git = true;
    v.push(Variant {
        name: "+ current project, dirty, recent commits (F3)",
        f,
        empty_state: false,
    });
    v
}

#[derive(Default, Clone)]
struct Score {
    n: usize,
    first: usize,
    rank_sum: f64,
    rr_sum: f64,
    kto_sum: f64,
    chars_sum: f64,
}

fn stem_query(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let stem = match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    };
    stem.to_lowercase()
}

fn rank_of(sess: &mut CodeSession, q: &str, want: &str, depth: usize) -> usize {
    let mut cq = CodeQuery::parse(q);
    cq.limit = depth;
    match sess.search(&cq, &CancelToken::new()) {
        Ok(r) => r.hits.iter().position(|h| h.path == want).map_or(depth + 1, |i| i + 1),
        Err(_) => depth + 1,
    }
}

fn run_variant(live: &Arc<CodeLive>, tasks: &[Task], v: Variant, verbose: bool) -> Score {
    let mem = Memory::in_memory();
    let mut ranker = Ranker::new(Some(mem.clone()));
    ranker.features = v.f;
    let mut sess = CodeSession::with_ranker(live.clone(), ranker);
    let mut sc = Score::default();
    for (ti, task) in tasks.iter().enumerate() {
        mem.set_clock(Some(task.t));
        if let Some(r) = sess.ranker_mut() {
            r.cwd = task.ctx.clone();
            for (repo, g) in &task.git {
                r.git.pin(repo, g.clone());
            }
        }
        let full = rank_of(&mut sess, &task.query, &task.path, DEPTH);
        // Keystrokes: cheapest prefix at which the file is visible.
        let chars: Vec<char> = task.query.chars().collect();
        let mut best: Option<(usize, usize)> = None; // (cost, k)
        let ks = if v.empty_state { 0 } else { 1 };
        for k in ks..=chars.len() {
            if let Some((c, _)) = best
                && k >= c
            {
                break;
            }
            let q: String = chars[..k].iter().collect();
            let r = if k == chars.len() { full } else { rank_of(&mut sess, &q, &task.path, VISIBLE) };
            if r <= VISIBLE {
                let cost = k + r - 1;
                if best.is_none_or(|(c, _)| cost < c) {
                    best = Some((cost, k));
                }
            }
        }
        let (kto, k) = best.unwrap_or((chars.len() + full.min(DEPTH + 1) - 1, chars.len()));
        if task.measure {
            sc.n += 1;
            sc.first += (full == 1) as usize;
            sc.rank_sum += full as f64;
            sc.rr_sum += 1.0 / full as f64;
            sc.kto_sum += kto as f64;
            sc.chars_sum += chars.len() as f64;
        }
        let typed: String = chars[..k].iter().collect();
        mem.record_pick(&typed, &task.path, "open", "eval");
        for (t, p) in &task.touches {
            mem.set_clock(Some(*t));
            mem.record_touch(p);
        }
        if verbose && ti % 100 == 0 {
            eprintln!("  {} task {ti}/{}", v.name, tasks.len());
        }
    }
    sc
}

// ------------------------------------------------------------ synthetic world

const TS_FIXED: &[&str] = &["README.md", "package.json", "tsconfig.json", "src/index.ts", "src/app.ts", "src/config.ts", "src/router.ts", "src/api/client.ts"];
const RS_FIXED: &[&str] = &["README.md", "Cargo.toml", "src/main.rs", "src/lib.rs", "src/config.rs", "src/error.rs"];
const PY_FIXED: &[&str] = &["README.md", "pyproject.toml", "pkg/__init__.py", "pkg/config.py", "pkg/cli.py", "tests/conftest.py"];

fn gen_world(root: &Path, repos: usize, r: &mut Rng) -> Vec<(PathBuf, Vec<String>)> {
    let w = crate::code::W;
    let mut out = Vec::new();
    for i in 0..repos {
        let kind = i % 3;
        let name = format!("{}-{}", w[r.below(w.len())], w[r.below(w.len())]);
        let dir = root.join(format!("dev/{name}-{i}"));
        let mut files: Vec<String> = match kind {
            0 => TS_FIXED.iter().map(|s| s.to_string()).collect(),
            1 => RS_FIXED.iter().map(|s| s.to_string()).collect(),
            _ => PY_FIXED.iter().map(|s| s.to_string()).collect(),
        };
        for _ in 0..14 {
            let a = w[r.below(w.len())];
            let b = w[r.below(w.len())];
            files.push(match (kind, r.below(4)) {
                (0, 0) => format!("src/components/{}{}.tsx", cap(a), cap(b)),
                (0, 1) => format!("src/api/{a}.ts"),
                (0, 2) => format!("tests/{a}.test.ts"),
                (0, _) => format!("src/{a}/{b}.ts"),
                (1, 0) => format!("src/{a}/mod.rs"),
                (1, 1) => format!("tests/{a}.rs"),
                (1, _) => format!("src/{a}.rs"),
                (_, 0) => format!("tests/test_{a}.py"),
                (_, _) => format!("pkg/{a}.py"),
            });
        }
        files.sort();
        files.dedup();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        for f in &files {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
        out.push((dir, files));
    }
    out
}

fn cap(w: &str) -> String {
    let mut c = w.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn zipf(r: &mut Rng, n: usize) -> usize {
    // P(i) ∝ 1/(i+1)
    let h: f64 = (1..=n).map(|i| 1.0 / i as f64).sum();
    let mut x = (r.next() % 1_000_000) as f64 / 1e6 * h;
    for i in 0..n {
        x -= 1.0 / (i + 1) as f64;
        if x <= 0.0 {
            return i;
        }
    }
    n - 1
}

fn typo(q: &str, r: &mut Rng) -> String {
    let mut c: Vec<char> = q.chars().collect();
    if c.len() >= 6 {
        let i = 1 + r.below(c.len() - 3);
        c.swap(i, i + 1);
    }
    c.into_iter().collect()
}

fn synthetic_tasks(world: &[(PathBuf, Vec<String>)], days: usize, per_day: usize, r: &mut Rng, t0: i64) -> Vec<Task> {
    let canon: Vec<PathBuf> = world.iter().map(|(d, _)| std::fs::canonicalize(d).unwrap()).collect();
    // 6 active projects, each with a 12-file working set.
    let active: Vec<usize> = (0..6).map(|_| r.below(world.len())).collect();
    let mut working: HashMap<usize, Vec<String>> = HashMap::new();
    for &a in &active {
        let files = &world[a].1;
        let mut ws: Vec<String> = Vec::new();
        while ws.len() < 12.min(files.len()) {
            let f = &files[r.below(files.len())];
            if !ws.contains(f) {
                ws.push(f.clone());
            }
        }
        working.insert(a, ws);
    }
    let mut tasks = Vec::new();
    let mut current = active[0];
    let mut dirty: HashMap<usize, Vec<String>> = HashMap::new();
    let mut commits: HashMap<usize, Vec<Vec<String>>> = HashMap::new();
    for day in 0..days {
        // End of the previous day: most repos with changes get a commit.
        for &a in &active {
            if let Some(d) = dirty.get_mut(&a)
                && !d.is_empty()
                && r.below(10) < 7
            {
                commits.entry(a).or_default().insert(0, std::mem::take(d));
            }
        }
        let main = active[zipf(r, active.len())];
        for j in 0..per_day {
            let t = t0 + (day as i64) * 86_400 + 9 * 3600 + (j as i64) * 900;
            let repo = if r.below(10) < 6 { main } else if r.below(10) < 5 { current } else { active[zipf(r, active.len())] };
            let ctx = Some(canon[current].clone());
            current = repo;
            let git: Vec<(PathBuf, GitInfo)> = active
                .iter()
                .map(|&a| {
                    let mut g = GitInfo::default();
                    for f in dirty.get(&a).into_iter().flatten() {
                        g.dirty.insert(f.clone());
                    }
                    for (i, c) in commits.get(&a).into_iter().flatten().take(10).enumerate() {
                        for f in c {
                            g.recent.entry(f.clone()).or_insert(i);
                        }
                    }
                    (canon[a].clone(), g)
                })
                .collect();
            let ws = working.get_mut(&repo).unwrap();
            // Slow drift of the working set.
            if r.below(40) == 0 {
                let files = &world[repo].1;
                let i = r.below(ws.len());
                ws[i] = files[r.below(files.len())].clone();
            }
            let rel = ws[zipf(r, ws.len())].clone();
            let mut q = stem_query(&rel);
            if r.below(10) == 0 {
                q = typo(&q, r);
            }
            let path = canon[repo].join(&rel).to_string_lossy().into_owned();
            let mut touches = Vec::new();
            if r.below(2) == 0 {
                touches.push((t + 120, path.clone()));
                if r.below(3) == 0 {
                    let o = &ws[r.below(ws.len())];
                    touches.push((t + 300, canon[repo].join(o).to_string_lossy().into_owned()));
                }
            }
            for (_, p) in &touches {
                let d = dirty.entry(repo).or_default();
                if !d.contains(p) {
                    d.push(p.clone());
                }
            }
            tasks.push(Task {
                t,
                path,
                repo: canon[repo].clone(),
                query: q,
                touches,
                measure: day >= 3,
                ctx,
                git,
            });
        }
    }
    tasks
}

// ------------------------------------------------------------ real history

fn real_tasks(live: &Arc<CodeLive>, repos_n: usize, days: i64, max_tasks: usize) -> Vec<Task> {
    let now = flashfind_code::memory::now();
    let mut repos = live.repos();
    repos.sort_by(|a, b| b.active.cmp(&a.active));
    let snap = live.snapshot();
    let exists = |p: &str| {
        snap.base.idx.lookup(p.as_bytes()).is_some() || snap.overlays.iter().any(|o| o.idx.lookup(p.as_bytes()).is_some())
    };
    let mut ev: Vec<(i64, PathBuf, String)> = Vec::new();
    for r in repos.iter().take(repos_n) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&r.path)
            .args(["log", "--no-merges", "--name-only", "--format=%x01%ct", &format!("--since={days}.days")])
            .stderr(std::process::Stdio::null())
            .output();
        let Ok(o) = out else { continue };
        let s = String::from_utf8_lossy(&o.stdout);
        for chunk in s.split('\u{1}').filter(|c| !c.trim().is_empty()) {
            let mut lines = chunk.lines();
            let Some(t) = lines.next().and_then(|l| l.trim().parse::<i64>().ok()) else { continue };
            let files: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).collect();
            if files.len() > 20 {
                continue; // bulk commits (renames, formatting)
            }
            for (i, f) in files.iter().enumerate() {
                let p = r.path.join(f).to_string_lossy().into_owned();
                if exists(&p) {
                    // "Opened" a bit before the commit, in file order.
                    ev.push((t - 600 + i as i64 * 20, r.path.clone(), p));
                }
            }
        }
    }
    ev.sort();
    if ev.len() > max_tasks {
        ev.drain(..ev.len() - max_tasks);
    }
    let n = ev.len();
    // Recent-commit state per repo as of each task: files of the 10 newest
    // commits strictly before it (dirty state is unknown: left empty).
    let mut by_commit: HashMap<PathBuf, Vec<(i64, Vec<String>)>> = HashMap::new();
    for (t, repo, path) in &ev {
        let commit_t = *t; // files of one commit are 20 s apart
        let v = by_commit.entry(repo.clone()).or_default();
        match v.last_mut() {
            Some((ct, files)) if (commit_t - *ct).abs() < 600 => files.push(path.clone()),
            _ => v.push((commit_t, vec![path.clone()])),
        }
    }
    let mut prev: Option<PathBuf> = None;
    let mut out = Vec::new();
    for (i, (t, repo, path)) in ev.into_iter().enumerate() {
        let git = by_commit
            .get(&repo)
            .map(|cs| {
                let mut g = GitInfo::default();
                for (k, (_, files)) in cs.iter().rev().filter(|(ct, _)| *ct + 600 < t).take(10).enumerate() {
                    for f in files {
                        g.recent.entry(f.clone()).or_insert(k);
                    }
                }
                vec![(repo.clone(), g)]
            })
            .unwrap_or_default();
        out.push(Task {
            t: t.min(now),
            query: stem_query(&path),
            touches: vec![(t + 300, path.clone())],
            path,
            repo: repo.clone(),
            measure: i >= n / 5,
            ctx: prev.replace(repo),
            git,
        });
    }
    out
}

pub fn cmd_bench_ux(mut a: Args) {
    let real = a.flag("--real");
    let verbose = a.flag("-v");
    let seed = a.num(&["--seed"], 7u64);
    let repos_n = a.num(&["--repos"], 80usize);
    let days = a.num(&["--days"], 30usize);
    let per_day = a.num(&["--per-day"], 20usize);
    let max_tasks = a.num(&["--max-tasks"], 1500usize);
    let out_md = a.opt(&["--md"]);
    let root = a.opt(&["--root"]);
    let (live, tasks, label) = if real {
        let mut cfg = Config::default();
        if let Some(r) = &root {
            cfg.roots = vec![PathBuf::from(r)];
        }
        cfg.roots = cfg.roots.iter().map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone())).collect();
        let live = CodeLive::open(cfg, Some(CodeIndex::default_dir()));
        if !live.just_built() {
            live.recheck();
        }
        let tasks = real_tasks(&live, a.num(&["--real-repos"], 15usize), days as i64 * 2, max_tasks);
        let label = format!("real git history: {} repos indexed, last {} days of commits in the 15 most active", live.repos().len(), days * 2);
        (live, tasks, label)
    } else {
        let root = PathBuf::from(root.unwrap_or_else(|| std::env::temp_dir().join(format!("flashfind-ux-{seed}")).to_string_lossy().into_owned()));
        let _ = std::fs::remove_dir_all(&root);
        let mut r = Rng(seed);
        let world = gen_world(&root, repos_n, &mut r);
        let cfg = Config {
            roots: vec![std::fs::canonicalize(&root).unwrap()],
            ..Config::default()
        };
        let live = CodeLive::from_index(cfg.clone(), CodeIndex::build(&cfg));
        let t0 = flashfind_code::memory::now() - (days as i64 + 1) * 86_400;
        let tasks = synthetic_tasks(&world, days, per_day, &mut r, t0);
        (live, tasks, format!("synthetic: {repos_n} repos (seed {seed}), 6 active, {days} days × {per_day} opens"))
    };
    if tasks.is_empty() {
        die("no tasks (no git history found?)");
    }
    let measured = tasks.iter().filter(|t| t.measure).count();
    let mut md = format!(
        "### {label}\n\n{} tasks ({} measured after warm-up), {} entries indexed\n\n| variant | right file first | mean rank | MRR | keystrokes to open | (chars in full query) |\n|---|---|---|---|---|---|\n",
        tasks.len(),
        measured,
        live.stats().entries
    );
    for v in variants() {
        let s = run_variant(&live, &tasks, v, verbose);
        let n = s.n.max(1) as f64;
        let line = format!(
            "| {} | {:.1}% | {:.2} | {:.3} | {:.2} | {:.2} |\n",
            v.name,
            100.0 * s.first as f64 / n,
            s.rank_sum / n,
            s.rr_sum / n,
            s.kto_sum / n,
            s.chars_sum / n
        );
        eprint!("{line}");
        md.push_str(&line);
    }
    println!("{md}");
    if let Some(p) = out_md {
        let prev = std::fs::read_to_string(&p).unwrap_or_default();
        std::fs::write(&p, prev + &md + "\n").unwrap_or_else(|e| die(e));
    }
}
