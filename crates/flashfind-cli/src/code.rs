//! `flashfind projects | code | mcp | gen-repos | bench-code`.

use crate::util::{Rng, peak_rss, percentile};
use crate::{Args, die};
use flashfind_code::{CodeIndex, CodeLive, CodeQuery, CodeSession, Config, Memory, Ranker};
use flashfind_core::walk::ScanOptions;
use flashfind_core::{CancelToken, Index, Query, Session};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn config(a: &mut Args) -> Config {
    let mut cfg = Config::default();
    let roots: Vec<PathBuf> = a
        .opts("--root")
        .into_iter()
        .flat_map(|r| r.split(',').map(PathBuf::from).collect::<Vec<_>>())
        .collect();
    if !roots.is_empty() {
        cfg.roots = roots;
    }
    cfg.skip.extend(a.opts("--skip"));
    if a.flag("--no-gitignore") {
        cfg.gitignore = false;
    }
    if a.flag("--no-default-excludes") {
        cfg.junk = vec![".git".into()];
    }
    cfg.threads = a.num(&["--threads", "-j"], cfg.threads);
    cfg.roots = cfg.roots.iter().map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone())).collect();
    cfg
}

/// Open the cached code index for `cfg.roots` (building it if missing) and
/// bring it up to date: on macOS by replaying FSEvents since it was built;
/// elsewhere by rebuilding when older than `--max-age` (default 600 s).
fn open(a: &mut Args, cfg: &Config) -> Arc<CodeLive> {
    let dir = a.opt(&["--cache"]).map(PathBuf::from).unwrap_or_else(CodeIndex::default_dir);
    let max_age = Duration::from_secs(a.num(&["--max-age"], 600u64));
    let refresh = a.flag("--refresh");
    let verbose = a.flag("--verbose") || a.flag("-v");
    let t0 = Instant::now();
    let live = CodeLive::open(cfg.clone(), Some(dir));
    // Built by this very call (no cache yet): nothing to catch up on.
    let fresh = live.just_built();
    if refresh && !fresh {
        live.compact();
    } else if !fresh {
        // Catch up by re-statting indexed folders (no event-history replay:
        // a busy home folder has ~10k FSEvents a minute).
        let t = Instant::now();
        live.recheck();
        // A day of drift (or, without FSEvents, --max-age): fold it into a fresh base once.
        let limit = if flashfind_core::watch::native_events() {
            Duration::from_secs(86_400)
        } else {
            max_age
        };
        if live.snapshot().base.age() > limit {
            live.compact();
        }
        if verbose {
            let st = live.stats();
            eprintln!(
                "recheck {:.0} ms: {} repos re-walked, {} added, {} removed",
                t.elapsed().as_secs_f64() * 1e3,
                st.rewalks,
                st.repos_added,
                st.repos_removed
            );
        }
    }
    let info = live.snapshot().base.info.clone();
    if fresh || refresh || verbose {
        eprintln!(
            "{} projects, {} entries (index built in {:.0} ms: discover {:.0} + crawl {:.0} + build {:.0}; ready in {:.0} ms)",
            live.stats().repos,
            live.stats().entries,
            info.discover_ms + info.crawl_ms + info.build_ms,
            info.discover_ms,
            info.crawl_ms,
            info.build_ms,
            t0.elapsed().as_secs_f64() * 1e3
        );
    }
    live
}

fn ago(t: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let d = (now - t).max(0);
    match d {
        _ if t == 0 => "?".into(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d if d < 60 * 86_400 => format!("{}d ago", d / 86_400),
        d => format!("{}mo ago", d / (30 * 86_400)),
    }
}

pub fn cmd_projects(mut a: Args) {
    let cfg = config(&mut a);
    let json = a.flag("--json");
    let limit = a.num(&["--limit"], 1000usize);
    let live = open(&mut a, &cfg);
    let filter = a.rest().first().cloned();
    let v = flashfind_code::projects_json(&live.repos(), filter.as_deref(), limit);
    if json {
        println!("{v}");
        return;
    }
    for p in v["projects"].as_array().unwrap() {
        println!(
            "{:<28} {:<12} {:>9} entries  {}",
            p["name"].as_str().unwrap(),
            ago(p["active"].as_i64().unwrap_or(0)),
            p["entries"],
            p["root"].as_str().unwrap()
        );
    }
    eprintln!("{} projects", v["total"]);
}

/// The pick/touch memory (`--data DIR`, else the default data folder).
pub fn memory(a: &mut Args) -> std::sync::Arc<Memory> {
    match a.opt(&["--data"]) {
        Some(d) => Memory::open(Path::new(&d)),
        None => Memory::open_default(),
    }
}

pub fn cmd_code(mut a: Args) {
    let cfg = config(&mut a);
    let json = a.flag("--json");
    let flat = a.flag("--flat");
    let why = a.flag("--why");
    let baseline = a.flag("--baseline");
    let limit = a.num(&["--limit", "-n"], 20usize);
    let recent = a.opt(&["--recent"]);
    let pick: Option<usize> = a.opt(&["--pick"]).map(|v| v.parse().unwrap_or_else(|_| die("--pick needs a result number (1 = first)")));
    let action = a.opt(&["--action"]).unwrap_or_else(|| "open".into());
    let mem = memory(&mut a);
    let live = open(&mut a, &cfg);
    let ranker = if baseline { Ranker::baseline() } else { Ranker::new(Some(mem.clone())) };
    let mut sess = CodeSession::with_ranker(live.clone(), ranker);
    let text = a.rest().join(" ");
    let mut cq = CodeQuery::parse(&text);
    cq.limit = limit;
    let r = match &recent {
        Some(w) => {
            let secs = flashfind_core::query::parse_duration(w).unwrap_or_else(|e| die(e));
            sess.recent(&cq, Some(secs))
        }
        None => {
            if cq.text.trim().is_empty() && !cq.dirty && cq.langs.is_empty() && cq.repos.is_empty() {
                die("code needs a query (or --recent 7d)");
            }
            sess.search(&cq, &CancelToken::new())
        }
    }
    .unwrap_or_else(|e| die(e));
    if let Some(n) = pick {
        // Record result #n (rank order) and print its path, e.g. `cursor -g $(flashfind code --pick 1 router)`.
        let h = r.hits.get(n.saturating_sub(1)).unwrap_or_else(|| die(format!("no result #{n}")));
        mem.record_pick(&cq.text, &h.path, &action, "cli");
        match cq.line {
            Some(l) => println!("{}:{}{}", h.path, l, cq.col.map(|c| format!(":{c}")).unwrap_or_default()),
            None => println!("{}", h.path),
        }
        return;
    }
    if json {
        println!("{}", flashfind_code::result_json(&r));
        return;
    }
    if flat {
        for (i, h) in r.hits.iter().enumerate() {
            if why && !h.why.is_empty() {
                println!("{:>2} {}    [{} · {}]", i + 1, h.path, h.score, h.why);
            } else if why {
                println!("{:>2} {}    [{}]", i + 1, h.path, h.score);
            } else {
                println!("{}", h.path);
            }
        }
    } else {
        for g in &r.groups {
            println!("{}  ({}, {})", g.repo, g.root, ago(g.active));
            for &i in &g.hits {
                let h = &r.hits[i];
                if why {
                    let w = if h.why.is_empty() { String::new() } else { format!(" · {}", h.why) };
                    println!("    {}    [{}{}]", h.rel, h.score, w);
                } else {
                    println!("    {}", h.rel);
                }
            }
        }
    }
    eprintln!("{} matches in {} µs", r.total, r.elapsed_us);
}

/// `flashfind pick <path> [--query Q] [--action A]`: tell flashfind which
/// file you meant (editors, scripts). `--forget` clears the memory,
/// `--show` prints it.
pub fn cmd_pick(mut a: Args) {
    let query = a.opt(&["--query", "-q"]).unwrap_or_default();
    let action = a.opt(&["--action"]).unwrap_or_else(|| "open".into());
    let forget = a.flag("--forget");
    let show = a.flag("--show");
    let mem = memory(&mut a);
    if forget {
        mem.forget();
        eprintln!("flashfind: memory cleared ({})", mem.path().map_or("in memory".into(), |p| p.display().to_string()));
        return;
    }
    if show {
        let r = mem.recall();
        println!("memory: {} events in {}", mem.len(), mem.path().map_or("-".into(), |p| p.display().to_string()));
        println!("most frecent:");
        for (p, f) in r.top_frecent(15) {
            println!("  {f:6.2}  {p}");
        }
        println!("queries:");
        let mut qs: Vec<_> = r.queries.iter().collect();
        qs.sort_by(|a, b| a.0.cmp(b.0));
        for (q, files) in qs.iter().take(50) {
            let f: Vec<String> = files.iter().map(|(p, c, n)| format!("{} ({n}×, {c:.2})", p)).collect();
            println!("  {q:20} → {}", f.join(", "));
        }
        return;
    }
    let rest = a.rest();
    if rest.is_empty() {
        die("pick needs a path (or --show / --forget)");
    }
    for p in rest {
        let abs = std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(&p));
        mem.record_pick(&query, &abs.to_string_lossy(), &action, "cli");
    }
}

pub fn cmd_mcp(mut a: Args) {
    let cfg = config(&mut a);
    let dir = a.opt(&["--cache"]).map(PathBuf::from).unwrap_or_else(CodeIndex::default_dir);
    let poll = Duration::from_secs(a.num(&["--max-age"], 300u64));
    let srv = flashfind_code::mcp::Server::new(cfg, Some(dir), poll);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if let Err(e) = srv.serve(stdin.lock(), stdout.lock()) {
        die(e);
    }
}

// ------------------------------------------------------------ synthetic

pub(crate) const W: &[&str] = &[
    "auth",
    "router",
    "parser",
    "lexer",
    "server",
    "client",
    "config",
    "cache",
    "queue",
    "worker",
    "store",
    "model",
    "view",
    "controller",
    "service",
    "handler",
    "session",
    "token",
    "payment",
    "invoice",
    "order",
    "cart",
    "search",
    "index",
    "engine",
    "render",
    "layout",
    "theme",
    "button",
    "modal",
    "form",
    "input",
    "upload",
    "export",
    "import",
    "report",
    "chart",
    "graph",
    "metrics",
    "logger",
    "tracing",
    "schema",
    "migration",
    "seed",
    "fixture",
    "util",
    "helper",
    "format",
    "date",
    "time",
    "color",
    "image",
    "video",
    "audio",
    "player",
    "editor",
    "canvas",
    "shader",
    "physics",
    "network",
    "socket",
    "stream",
    "buffer",
    "codec",
    "crypto",
    "hash",
    "signer",
    "wallet",
    "ledger",
    "account",
    "profile",
    "user",
    "team",
    "billing",
    "plan",
    "usage",
    "quota",
    "limit",
    "retry",
    "backoff",
    "scheduler",
    "cron",
    "job",
    "task",
    "pipeline",
    "build",
    "deploy",
    "release",
    "notify",
    "email",
    "sms",
    "push",
    "webhook",
    "gateway",
    "proxy",
    "dns",
    "tls",
    "cert",
    "vault",
    "secret",
    "flag",
    "experiment",
    "analytics",
    "event",
];

const LANGS: &[(&str, &str, &str)] = &[
    ("rust", "rs", "target/\n*.log\n"),
    ("ts", "ts", "node_modules/\ndist/\n*.log\n.env.local\ncoverage/\n"),
    ("js", "js", "node_modules/\nbuild/\n*.log\n"),
    ("py", "py", ".venv/\n__pycache__/\n*.pyc\n*.log\n"),
    ("swift", "swift", "DerivedData/\n.build/\n*.xcuserstate\n"),
    ("go", "go", "bin/\n*.log\n"),
];

fn pick<'a>(r: &mut Rng, v: &'a [&'a str]) -> &'a str {
    v[r.below(v.len())]
}

fn put(path: &Path, size: usize) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, vec![b'x'; size])
}

fn stem(r: &mut Rng, lang: &str) -> String {
    let (a, b) = (pick(r, W), pick(r, W));
    match (lang, r.below(3)) {
        ("swift", _) => format!("{}{}", cap(a), cap(b)),
        ("ts" | "js", 0) => format!("{a}{}", cap(b)),
        (_, 0) => a.to_string(),
        _ => format!("{a}_{b}"),
    }
}

fn cap(w: &str) -> String {
    let mut c = w.chars();
    c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
}

/// Synthetic home with `repos` git projects (sources + realistic junk) and
/// non-project clutter, plus a query file.
pub fn cmd_gen_repos(mut a: Args) {
    let root = PathBuf::from(a.opt(&["--root"]).unwrap_or_else(|| die("--root is required")));
    let repos = a.num(&["--repos"], 200usize);
    let seed = a.num(&["--seed"], 7u64);
    let qfile = PathBuf::from(a.opt(&["--queries"]).unwrap_or_else(|| die("--queries is required")));
    let t0 = Instant::now();
    let mut r = Rng(seed);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let mut names = HashSet::new();
    let mut targets: Vec<(String, String, String, String)> = Vec::new(); // (repo, abs path, stem, parent dir)
    let mut nfiles = 0usize;
    let mut njunk = 0usize;
    let groups = ["dev", "work/acme", "work/globex", "oss", "src/github.com/someone"];
    let res: std::io::Result<()> = (|| {
        for i in 0..repos {
            let mut name = format!("{}-{}", pick(&mut r, W), pick(&mut r, W));
            if !names.insert(name.to_lowercase()) {
                name = format!("{name}-{i}");
                names.insert(name.to_lowercase());
            }
            let (lang, ext, ignore) = LANGS[r.below(LANGS.len())];
            let rp = root.join(groups[r.below(groups.len())]).join(&name);
            // .git internals
            let age = match r.below(10) {
                0..=1 => r.below(86_400) as i64,
                2..=4 => r.below(30 * 86_400) as i64,
                _ => r.below(700 * 86_400) as i64,
            };
            put(&rp.join(".git/HEAD"), 0)?;
            std::fs::write(rp.join(".git/HEAD"), "ref: refs/heads/main\n")?;
            std::fs::create_dir_all(rp.join(".git/logs"))?;
            std::fs::write(
                rp.join(".git/logs/HEAD"),
                format!("0000 1111 Dev <dev@example.com> {} +0000\tcommit: work\n", now - age),
            )?;
            for j in 0..(40 + r.below(200)) {
                put(&rp.join(format!(".git/objects/{:02x}/{:038x}", j % 256, r.next() as u128 * 7919)), 0)?;
                njunk += 1;
            }
            std::fs::write(rp.join(".gitignore"), ignore)?;
            put(&rp.join("README.md"), 10)?;
            // sources
            let mods: Vec<String> = (0..2 + r.below(8)).map(|_| pick(&mut r, W).to_string()).collect();
            let nsrc = 30 + r.below(220);
            let mut used = HashSet::new();
            for _ in 0..nsrc {
                let m = &mods[r.below(mods.len())];
                let s = stem(&mut r, lang);
                let dir = match r.below(6) {
                    0 => "tests".to_string(),
                    1 => format!("src/{m}/internal"),
                    _ => format!("src/{m}"),
                };
                let rel = format!("{dir}/{s}.{ext}");
                if !used.insert(rel.to_lowercase()) {
                    continue;
                }
                put(&rp.join(&rel), 20)?;
                nfiles += 1;
                if r.below(25) == 0 {
                    let parent = dir.rsplit('/').next().unwrap().to_string();
                    targets.push((name.clone(), rp.join(&rel).to_string_lossy().into_owned(), s.clone(), parent));
                }
            }
            // junk that a plain whole-disk search would show
            let junk_dirs: Vec<(String, usize)> = match lang {
                "ts" | "js" => (0..10 + r.below(40))
                    .map(|_| (format!("node_modules/{}", pick(&mut r, W)), 15 + r.below(60)))
                    .collect(),
                "rust" => vec![
                    ("target/debug/deps".into(), 200 + r.below(800)),
                    ("target/debug/build".into(), 50 + r.below(200)),
                ],
                "py" => (0..10 + r.below(30))
                    .map(|_| (format!(".venv/lib/python3.12/site-packages/{}", pick(&mut r, W)), 10 + r.below(40)))
                    .collect(),
                "swift" => vec![
                    (".build/debug".into(), 100 + r.below(500)),
                    ("DerivedData/Build/Intermediates".into(), 100 + r.below(400)),
                ],
                _ => vec![("bin".into(), 5 + r.below(20))],
            };
            for (d, n) in junk_dirs {
                for k in 0..n {
                    let s = if r.below(3) == 0 {
                        stem(&mut r, lang)
                    } else {
                        format!("{}{k}", pick(&mut r, W))
                    };
                    let jext = match lang {
                        "rust" => ["d", "rlib", "o", "rmeta"][r.below(4)],
                        "ts" | "js" => ["js", "d.ts", "json", "md", "ts"][r.below(5)],
                        "py" => ["py", "pyc"][r.below(2)],
                        _ => "o",
                    };
                    let _ = put(&rp.join(&d).join(format!("{s}.{jext}")), 0);
                    njunk += 1;
                }
            }
            for k in 0..r.below(6) {
                put(&rp.join(format!("logs/{}-{k}.log", pick(&mut r, W))), 0)?;
                njunk += 1;
            }
        }
        // Non-project clutter.
        for k in 0..20_000 {
            let s = format!("{}_{}{k}", pick(&mut r, W), pick(&mut r, W));
            let d = [
                "Library/Caches/com.example.app",
                "Library/Application Support/Code/User/History",
                "Library/Developer/Xcode/DerivedData/x/Build",
            ][k % 3];
            put(
                &root
                    .join(d)
                    .join(format!("{}/{s}.{}", k % 97, ["json", "ts", "rs", "py", "swift", "dat"][k % 6])),
                0,
            )?;
            njunk += 1;
        }
        for k in 0..300 {
            put(&root.join(format!("Downloads/{}-{k}.pdf", pick(&mut r, W))), 0)?;
        }
        // Queries.
        let mut f = std::io::BufWriter::new(std::fs::File::create(&qfile)?);
        for (repo, path, s, parent) in &targets {
            writeln!(f, "{}", json!({"class": "name", "q": s, "expect": path}))?;
            if let Some(t) = crate::treegen::typo(&mut r, s, "typo_sub")
                && s.len() >= 5
            {
                writeln!(f, "{}", json!({"class": "typo", "q": t, "expect": path}))?;
            }
            writeln!(f, "{}", json!({"class": "repo_scoped", "q": format!("repo:{repo} {s}"), "expect": path}))?;
            writeln!(f, "{}", json!({"class": "folder_name", "q": format!("{parent} {s}"), "expect": path}))?;
        }
        Ok(())
    })();
    res.unwrap_or_else(|e| die(e));
    eprintln!(
        "generated {repos} repos: {nfiles} source files, {njunk} junk/ignored files, {} targets in {:.1}s",
        targets.len(),
        t0.elapsed().as_secs_f64()
    );
}

fn is_junk(p: &str) -> bool {
    [
        "/node_modules/",
        "/target/",
        "/.git/",
        "/.venv/",
        "/.build/",
        "/DerivedData/",
        "/Library/",
        "/dist/",
        "/bin/",
        "/logs/",
    ]
    .iter()
    .any(|j| p.contains(j))
        || p.ends_with(".log")
}

/// Code index vs plain whole-tree index on a `gen-repos` tree.
pub fn cmd_bench_code(mut a: Args) {
    let root = std::fs::canonicalize(a.opt(&["--root"]).unwrap_or_else(|| die("--root is required"))).unwrap_or_else(|e| die(e));
    let qfile = a.opt(&["--queries"]).unwrap_or_else(|| die("--queries is required"));
    let out = a.opt(&["--out"]);
    let repeat = a.num(&["--repeat"], 3usize);
    let cfg = Config {
        roots: vec![root.clone()],
        ..Config::default()
    };
    // Plain: every file under the root, same engine.
    let t = Instant::now();
    let plain = Arc::new(flashfind_core::build_from_disk(&root, &ScanOptions { threads: 8, skip: vec![] }));
    let plain_ms = t.elapsed().as_secs_f64() * 1e3;
    let ci = CodeIndex::build(&cfg);
    let lines: Vec<Value> = std::fs::read_to_string(&qfile)
        .unwrap_or_else(|e| die(e))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut rows = Vec::new();
    let classes = ["name", "typo", "repo_scoped", "folder_name"];
    let cancel = CancelToken::new();
    for class in classes {
        let qs: Vec<&Value> = lines.iter().filter(|v| v["class"] == class).collect();
        if qs.is_empty() {
            continue;
        }
        let (mut lat_c, mut lat_p) = (Vec::new(), Vec::new());
        let (mut h1c, mut h1p, mut junk_c, mut junk_p, mut shown_c, mut shown_p) = (0, 0, 0, 0, 0, 0);
        let (mut h10c, mut h10p) = (0, 0);
        for pass in 0..repeat {
            let mut sc = Session::new(ci.idx.clone());
            let mut sp = Session::new(plain.clone());
            for v in &qs {
                let q = v["q"].as_str().unwrap();
                let expect = v["expect"].as_str().unwrap();
                let t = Instant::now();
                let mut cq = CodeQuery::parse(q);
                cq.limit = 10;
                let r = ci.search(&mut sc, &cq, &cancel).unwrap();
                lat_c.push(t.elapsed().as_secs_f64() * 1e6);
                // Plain search has no repo: filter; it gets the bare words.
                let t = Instant::now();
                let mut pq = Query::parse(&cq.text).unwrap();
                pq.limit = 10;
                let rp = sp.search(&pq, &cancel).unwrap();
                let pp: Vec<String> = rp.hits.iter().map(|h| plain.path_string(h.entry)).collect();
                lat_p.push(t.elapsed().as_secs_f64() * 1e6);
                if pass == 0 {
                    h1c += (r.hits.first().map(|h| h.path.as_str()) == Some(expect)) as usize;
                    h1p += (pp.first().map(String::as_str) == Some(expect)) as usize;
                    h10c += r.hits.iter().any(|h| h.path == expect) as usize;
                    h10p += pp.iter().any(|p| p == expect) as usize;
                    junk_c += r.hits.iter().filter(|h| is_junk(&h.path)).count();
                    junk_p += pp.iter().filter(|p| is_junk(p)).count();
                    shown_c += r.hits.len();
                    shown_p += pp.len();
                }
            }
        }
        lat_c.sort_by(f64::total_cmp);
        lat_p.sort_by(f64::total_cmp);
        let n = qs.len() as f64;
        rows.push(json!({
            "class": class, "n": qs.len(),
            "code_p50_us": percentile(&lat_c, 50.0), "code_p95_us": percentile(&lat_c, 95.0),
            "plain_p50_us": percentile(&lat_p, 50.0), "plain_p95_us": percentile(&lat_p, 95.0),
            "code_hit1": h1c as f64 / n, "plain_hit1": h1p as f64 / n,
            "code_hit10": h10c as f64 / n, "plain_hit10": h10p as f64 / n,
            "code_junk_top10": junk_c as f64 / shown_c.max(1) as f64, "plain_junk_top10": junk_p as f64 / shown_p.max(1) as f64,
        }));
    }
    let v = json!({
        "repos": ci.info.repos,
        "code_entries": ci.info.entries, "plain_entries": plain.n,
        "code_index_bytes": ci.info.index_bytes, "plain_index_bytes": plain.bytes(),
        "discover_ms": ci.info.discover_ms, "code_crawl_ms": ci.info.crawl_ms, "code_build_ms": ci.info.build_ms,
        "plain_crawl_build_ms": plain_ms, "peak_rss": peak_rss(),
        "classes": rows,
    });
    let md = code_md(&v);
    match out {
        Some(f) => {
            std::fs::write(&f, serde_json::to_string_pretty(&v).unwrap()).unwrap_or_else(|e| die(e));
            if let Some(m) = a.opt(&["--md"]) {
                std::fs::write(m, &md).unwrap_or_else(|e| die(e));
            }
        }
        None => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
    }
    println!("{md}");
}

use std::sync::Arc;

fn us(x: f64) -> String {
    if x >= 1000.0 {
        format!("{:.2} ms", x / 1000.0)
    } else {
        format!("{x:.0} µs")
    }
}

fn code_md(v: &Value) -> String {
    let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
    let mut s = String::new();
    s += "| | code index (`flashfind code`) | plain whole-tree index |\n|---|---|---|\n";
    s += &format!("| entries indexed | {} | {} |\n", v["code_entries"], v["plain_entries"]);
    s += &format!(
        "| junk/ignored entries excluded | {:.1}% | – |\n",
        100.0 * (1.0 - f("code_entries") / f("plain_entries").max(1.0))
    );
    s += &format!(
        "| index size | {:.1} MB | {:.1} MB |\n",
        f("code_index_bytes") / 1e6,
        f("plain_index_bytes") / 1e6
    );
    s += &format!(
        "| discover + crawl + build | {:.0} + {:.0} + {:.0} ms | {:.0} ms (crawl + build) |\n\n",
        f("discover_ms"),
        f("code_crawl_ms"),
        f("code_build_ms"),
        f("plain_crawl_build_ms")
    );
    s += "| class | n | code p50 | p95 | plain p50 | p95 | code hit@1 (@10) | plain hit@1 (@10) | junk in top 10: code | plain |\n|---|---|---|---|---|---|---|---|---|---|\n";
    for r in v["classes"].as_array().unwrap() {
        let g = |k: &str| r[k].as_f64().unwrap_or(0.0);
        s += &format!(
            "| {} | {} | {} | {} | {} | {} | {:.0}% ({:.0}%) | {:.0}% ({:.0}%) | {:.1}% | {:.1}% |\n",
            r["class"].as_str().unwrap(),
            r["n"],
            us(g("code_p50_us")),
            us(g("code_p95_us")),
            us(g("plain_p50_us")),
            us(g("plain_p95_us")),
            100.0 * g("code_hit1"),
            100.0 * g("code_hit10"),
            100.0 * g("plain_hit1"),
            100.0 * g("plain_hit10"),
            100.0 * g("code_junk_top10"),
            100.0 * g("plain_junk_top10")
        );
    }
    s
}

#[allow(dead_code)]
fn _unused(_: &Index) {}

/// Event-to-searchable latency of the live code index: create / rename /
/// delete temp files in `--dir` (must be inside an indexed repo), wait
/// until each change shows up in search, then clean up.
pub fn cmd_bench_live_code(mut a: Args) {
    let cfg = config(&mut a);
    let dir = PathBuf::from(a.opt(&["--dir"]).unwrap_or_else(|| die("--dir (inside an indexed repo) is required")));
    let n = a.num(&["-n"], 10usize);
    let dir = std::fs::canonicalize(&dir).unwrap_or_else(|e| die(e));
    let cache = a.opt(&["--cache"]).map(PathBuf::from).unwrap_or_else(CodeIndex::default_dir);
    let t0 = Instant::now();
    let live = CodeLive::open(cfg, Some(cache));
    live.watch().unwrap_or_else(|e| die(e));
    let replayed = live.recheck();
    let ready = t0.elapsed();
    std::thread::sleep(Duration::from_millis(300));
    let mut s = CodeSession::new(live.clone());
    let st = live.stats();
    eprintln!(
        "{} repos, {} entries; open + watch + recheck ({replayed} repos re-walked) {:.0} ms",
        st.repos,
        st.entries,
        ready.as_secs_f64() * 1e3
    );
    let tag = format!("ffzq{}", std::process::id());
    let found = |s: &mut CodeSession, name: &str| -> bool {
        let mut cq = CodeQuery::parse(&format!("={name}"));
        cq.limit = 5;
        s.search(&cq, &CancelToken::new()).is_ok_and(|r| !r.hits.is_empty())
    };
    let wait = |s: &mut CodeSession, name: &str, want: bool| -> f64 {
        let t = Instant::now();
        while found(s, name) != want {
            if t.elapsed() > Duration::from_secs(10) {
                return f64::NAN;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        t.elapsed().as_secs_f64() * 1e3
    };
    let (mut create, mut rename, mut delete, mut search_us) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut created: Vec<PathBuf> = Vec::new();
    for i in 0..n {
        let a1 = dir.join(format!("{tag}_a{i}.txt"));
        let b1 = dir.join(format!("{tag}_b{i}.txt"));
        std::fs::write(&a1, b"x").unwrap_or_else(|e| die(e));
        created.push(a1.clone());
        create.push(wait(&mut s, &format!("{tag}_a{i}.txt"), true));
        std::fs::rename(&a1, &b1).unwrap();
        created.push(b1.clone());
        rename.push(wait(&mut s, &format!("{tag}_b{i}.txt"), true));
        std::fs::remove_file(&b1).unwrap();
        delete.push(wait(&mut s, &format!("{tag}_b{i}.txt"), false));
        let t = Instant::now();
        let mut cq = CodeQuery::parse("readme");
        cq.limit = 20;
        let _ = s.search(&cq, &CancelToken::new());
        search_us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    for p in &created {
        let _ = std::fs::remove_file(p);
    }
    live.unwatch();
    let stat = |v: &mut Vec<f64>| {
        v.retain(|x| x.is_finite());
        v.sort_by(f64::total_cmp);
        (percentile(v, 50.0), v.last().copied().unwrap_or(f64::NAN), v.len())
    };
    for (name, v) in [("create", &mut create), ("rename", &mut rename), ("delete", &mut delete)] {
        let (p50, max, k) = stat(v);
        println!("{name:<7} -> searchable: p50 {p50:.1} ms, max {max:.1} ms ({k}/{n})");
    }
    let (p50, max, _) = stat(&mut search_us);
    let st = live.stats();
    println!(
        "search 'readme' with overlay: p50 {p50:.0} µs, max {max:.0} µs; repo re-walk {:.1} ms (last); {} re-walks",
        st.last_apply_ms, st.rewalks
    );
}
