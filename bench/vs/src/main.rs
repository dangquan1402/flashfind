//! flashfind vs fff vs fsearch: one process, same folder, same queries.
//!
//! Method follows fsearch's own `demo/vs_fff.py`: 300 file names that are
//! unique among the files fd would list (not hidden, not gitignored), each
//! queried as-is and with four one-letter typos (swap, drop, extra, subst;
//! never the first letter). Engines alternate who goes first; everything is
//! warm. Added here: per-keystroke latency (typing each name), folder+name
//! queries, and, on a multi-repo folder, `repo:` queries and junk-in-results.
//!
//!   flashfind-vs --root DIR [--label NAME] [--engines flashfind,fff,fsearch]
//!                [--targets 300] [--keys 40] [--seed 1] [--limit 50]
//!                [--live|--plain] [--cols N --rows N] [--cast FILE]
//!                [--json FILE] [--md FILE] [--no-showcase] [--pace-ms 70]

mod engines;
mod ui;

use engines::{Engine, Kind};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use ui::*;

struct Args(Vec<String>);
impl Args {
    fn get(&self, k: &str) -> Option<String> {
        self.0.iter().position(|a| a == k).and_then(|i| self.0.get(i + 1).cloned())
    }
    fn num(&self, k: &str, d: usize) -> usize {
        self.get(k).map_or(d, |v| v.parse().expect(k))
    }
    fn flag(&self, k: &str) -> bool {
        self.0.iter().any(|a| a == k)
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, self.below(i + 1));
        }
    }
}

/// One typo on a letter, never the first char (port of vs_fff.py `typo`).
fn typo(r: &mut Rng, name: &str, kind: &str) -> Option<String> {
    let mut s: Vec<char> = name.chars().collect();
    let letters: Vec<usize> = (1..s.len()).filter(|&i| s[i].is_ascii_alphabetic()).collect();
    if letters.is_empty() {
        return None;
    }
    let abc: Vec<char> = ('a'..='z').collect();
    match kind {
        "swap" => {
            let pairs: Vec<usize> = letters
                .iter()
                .copied()
                .filter(|&i| i + 1 < s.len() && s[i + 1].is_ascii_alphabetic() && s[i] != s[i + 1])
                .collect();
            if pairs.is_empty() {
                return None;
            }
            let p = pairs[r.below(pairs.len())];
            s.swap(p, p + 1);
        }
        "drop" => {
            let p = letters[r.below(letters.len())];
            s.remove(p);
        }
        "extra" => {
            let p = letters[r.below(letters.len())];
            s.insert(p, abc[r.below(26)]);
        }
        _ => {
            let p = letters[r.below(letters.len())];
            let lo = s[p].to_ascii_lowercase();
            let cands: Vec<char> = abc.iter().copied().filter(|&c| c != lo).collect();
            s[p] = cands[r.below(cands.len())];
        }
    }
    Some(s.into_iter().collect())
}

const JUNK: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    ".build",
    "DerivedData",
    "dist",
    "build",
    "Pods",
    "Library",
    ".git",
    "__pycache__",
];

fn is_junk(rel: &str) -> bool {
    rel.split('/').rev().skip(1).any(|c| JUNK.contains(&c))
}

fn base(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

#[derive(Clone)]
struct Q {
    q: String,
    target: String,
    class: &'static str,
    detail: &'static str,
}

#[derive(Default, Clone)]
struct Stat {
    us: Vec<f64>,
    hits: usize,
    tries: usize,
    junk: usize,
    shown: usize,
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[((p / 100.0) * (s.len() - 1) as f64).round() as usize]
}

fn color_of(k: Kind) -> String {
    match k {
        Kind::Flashfind => blue(),
        Kind::Fff => red(),
        Kind::Fsearch => yellow(),
    }
}

fn sh(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn machine() -> (String, String) {
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    if cfg!(target_os = "macos") {
        let cpu = sh("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem: f64 = sh("sysctl", &["-n", "hw.memsize"]).parse().unwrap_or(0.0);
        (
            format!("{cpu} ({cores} cores, {:.0} GB)", mem / 1e9),
            format!("macOS {}", sh("sw_vers", &["-productVersion"])),
        )
    } else {
        let cpu = std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("model name"))
                    .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
            })
            .unwrap_or_default();
        (format!("{cpu} ({cores} cores)"), sh("uname", &["-sr"]))
    }
}

fn load() -> [f64; 3] {
    let mut l = [0f64; 3];
    unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    l
}

fn term_size() -> Option<(usize, usize)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        (libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0).then_some((ws.ws_col as usize, ws.ws_row as usize))
    }
}

/// `measure` child: cold build, footprint, warm start, as one JSON line.
fn measure(a: &Args) {
    let kind = Kind::parse(&a.get("--engine").expect("--engine")).expect("engine");
    let root = PathBuf::from(a.get("--root").expect("--root"));
    let before = engines::footprint_bytes();
    let t = Instant::now();
    let mut b = engines::build(kind, &root);
    b.engine.search("makefile", 50);
    let ready = t.elapsed();
    let mem = engines::footprint_bytes().saturating_sub(before);
    let tmp = engines::tmpdir(kind.name());
    let warm = b.engine.warm_start(&root, &tmp);
    let _ = std::fs::remove_dir_all(&tmp);
    println!(
        "{}",
        json!({"engine": kind.name(), "entries": b.entries, "build_ms": b.build.as_secs_f64() * 1e3, "ready_ms": ready.as_secs_f64() * 1e3,
        "memory_mb": mem as f64 / 1048576.0, "warm_ms": warm.map(|d| d.as_secs_f64() * 1e3)})
    );
}

struct Ctx {
    t: Term,
    label: String,
    files: usize,
    mach: String,
    os: String,
    kinds: Vec<Kind>,
}

impl Ctx {
    fn header(&self) -> Vec<String> {
        let title = self.kinds.iter().map(|k| k.name()).collect::<Vec<_>>().join(" vs ");
        let l = load();
        vec![
            String::new(),
            format!(
                "  {BOLD}{}{title}{RESET}  {}{} · {} files · {} · load {:.1}{RESET}",
                white(),
                grey(),
                self.label,
                thousands(self.files),
                self.mach.split(" (").next().unwrap_or(&self.mach),
                l[0]
            ),
            String::new(),
        ]
    }
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut o = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            o.push(',');
        }
        o.push(c);
    }
    o
}

fn bar(done: usize, total: usize, w: usize) -> String {
    let n = if total == 0 { w } else { done * w / total };
    format!("{}{}{}{}{RESET}", blue(), "█".repeat(n), grey(), "░".repeat(w - n))
}

fn main() {
    let a = Args(std::env::args().collect());
    if a.0.get(1).map(String::as_str) == Some("measure") {
        return measure(&a);
    }
    let Some(root) = a.get("--root") else {
        eprintln!("usage: flashfind-vs --root DIR [--label NAME] [--engines flashfind,fff,fsearch] [--live|--plain] [--cast F] [--json F] [--md F]");
        std::process::exit(2);
    };
    let root = std::fs::canonicalize(&root).expect("root");
    let label = a.get("--label").unwrap_or_else(|| root.file_name().unwrap().to_string_lossy().into_owned());
    let kinds: Vec<Kind> = a
        .get("--engines")
        .unwrap_or_else(|| "flashfind,fff,fsearch".into())
        .split(',')
        .filter_map(Kind::parse)
        .filter(|k| {
            let ok = k.available();
            if !ok {
                eprintln!("note: {} is macOS-only, skipped", k.name());
            }
            ok
        })
        .collect();
    let tty = unsafe { libc::isatty(1) == 1 };
    let live = (tty || a.flag("--live")) && !a.flag("--plain");
    let (tw, th) = term_size().unwrap_or((120, 40));
    let (w, h) = (a.num("--cols", tw.min(132)), a.num("--rows", th));
    let (mach, os) = machine();
    let mut cx = Ctx {
        t: Term::new(live, tty || a.flag("--live") || a.flag("--color"), w, h, a.get("--cast").as_deref()),
        label: label.clone(),
        files: 0,
        mach,
        os,
        kinds: kinds.clone(),
    };
    let mut rng = Rng(a.num("--seed", 1) as u64);
    let limit = a.num("--limit", 50);
    let load0 = load();
    cx.t.start();

    // 1. Dataset: fd's view (not hidden, gitignore honoured), plus a full
    //    pass over everything so each engine starts from a warm FS cache.
    let t0 = Instant::now();
    let mut all_entries = 0usize;
    for e in ignore::WalkBuilder::new(&root).standard_filters(false).threads(8).build().flatten() {
        let _ = e.metadata();
        all_entries += 1;
    }
    let mut visible: Vec<String> = Vec::new();
    let mut repos: Vec<String> = Vec::new();
    for e in ignore::WalkBuilder::new(&root).build().flatten() {
        let rel = e.path().strip_prefix(&root).unwrap().to_string_lossy().into_owned();
        if e.file_type().is_some_and(|t| t.is_dir()) {
            if e.path().join(".git").exists() {
                repos.push(rel);
            }
        } else if e.file_type().is_some_and(|t| t.is_file()) {
            visible.push(rel);
        }
    }
    repos.sort_by_key(|r| std::cmp::Reverse(r.len()));
    let repo_of = |rel: &str| -> Option<String> { repos.iter().find(|r| r.is_empty() || rel.starts_with(&format!("{r}/"))).cloned() };
    let multi = repos.len() > 1;
    cx.files = visible.len();
    let scan_ms = t0.elapsed().as_secs_f64() * 1e3;

    // 2. Queries.
    let mut count: HashMap<&str, usize> = HashMap::new();
    for p in &visible {
        *count.entry(base(p)).or_default() += 1;
    }
    let special = |n: &str| n.contains(|c: char| " :*?[]{}!$^='\"\\,".contains(c)) || n.starts_with('-') || n.starts_with('.');
    let mut pool: Vec<&String> = visible
        .iter()
        .filter(|p| {
            let n = base(p);
            count[n] == 1
                && (6..=32).contains(&n.len())
                && n.is_ascii()
                && !special(n)
                && n.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 5
                && repo_of(p).is_some()
        })
        .collect();
    pool.sort();
    rng.shuffle(&mut pool);
    let targets: Vec<String> = pool.into_iter().take(a.num("--targets", 300)).cloned().collect();
    let mut qs: Vec<Q> = Vec::new();
    for t in &targets {
        let n = base(t).to_string();
        qs.push(Q {
            q: n.clone(),
            target: t.clone(),
            class: "exact",
            detail: "exact",
        });
        for k in ["swap", "drop", "extra", "subst"] {
            if let Some(q) = typo(&mut rng, &n, k) {
                qs.push(Q {
                    q,
                    target: t.clone(),
                    class: "typo",
                    detail: k,
                });
            }
        }
        let parent = t.rsplit('/').nth(1).unwrap_or("");
        if parent.len() >= 3 && !special(parent) {
            qs.push(Q {
                q: format!("{parent} {n}"),
                target: t.clone(),
                class: "folder",
                detail: "folder",
            });
        }
    }
    rng.shuffle(&mut qs);
    let mut repo_qs: Vec<Q> = Vec::new();
    if multi {
        for t in &targets {
            if let Some(r) = repo_of(t).filter(|r| !r.is_empty()) {
                let rn = r.rsplit('/').next().unwrap();
                repo_qs.push(Q {
                    q: format!("repo:{rn} {}", base(t)),
                    target: t.clone(),
                    class: "repo",
                    detail: "repo:",
                });
            }
        }
    }
    let keys: Vec<&String> = targets.iter().take(a.num("--keys", 40)).collect();
    if let Some(p) = a.get("--dump-queries") {
        let mut o = String::new();
        for q in qs.iter().chain(repo_qs.iter()) {
            o += &format!("{}\t{}\t{}\n", q.class, q.q, q.target);
        }
        for t in &keys {
            let name: Vec<char> = base(t).chars().collect();
            for n in 1..=name.len() {
                o += &format!("keys\t{}\t{}\n", name[..n].iter().collect::<String>(), t);
            }
        }
        std::fs::write(p, o).unwrap();
        cx.t.finish();
        return;
    }

    // 3. Cold build + memory per engine, each in its own child process.
    let exe = std::env::current_exe().unwrap();
    let mut meas: BTreeMap<&str, Value> = BTreeMap::new();
    for (i, &k) in kinds.iter().enumerate() {
        let mut child = std::process::Command::new(&exe)
            .args(["measure", "--engine", k.name(), "--root"])
            .arg(&root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn measure");
        let t = Instant::now();
        let spin = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            if cx.t.live && cx.t.due() {
                let mut l = cx.header();
                l.push(format!(
                    "  {BOLD}{}Index from scratch{RESET}  {}each engine in its own process, warm file-system cache ({} entries walked in {}){RESET}",
                    white(),
                    grey(),
                    thousands(all_entries),
                    ms(scan_ms * 1e3)
                ));
                l.push(String::new());
                for (j, &kk) in kinds.iter().enumerate() {
                    let s = if j < i {
                        let m = &meas[kk.name()];
                        format!(
                            "{BOLD}{}✓{RESET} {}{}{RESET}   {}{} entries · {:.0} MB{RESET}",
                            green(),
                            white(),
                            ms(m["ready_ms"].as_f64().unwrap() * 1e3),
                            grey(),
                            thousands(m["entries"].as_u64().unwrap_or(0) as usize),
                            m["memory_mb"].as_f64().unwrap()
                        )
                    } else if j == i {
                        format!(
                            "{}{}{RESET} {}{:.1} s{RESET}",
                            blue(),
                            spin[(t.elapsed().as_millis() / 80) as usize % 10],
                            white(),
                            t.elapsed().as_secs_f64()
                        )
                    } else {
                        format!("{}waiting{RESET}", grey())
                    };
                    l.push(format!("    {}{:<10}{RESET} {s}", color_of(kk), kk.name()));
                }
                cx.t.frame(&l);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = child.wait_with_output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(json!({"error": "measure failed"}));
        if !cx.t.live {
            cx.t.out(&format!("index {:<10} {}\n", k.name(), v));
        }
        meas.insert(k.name(), v);
    }

    // 4. Engines for the race, built in-process.
    let mut eng: Vec<Engine> = kinds.iter().map(|&k| engines::build(k, &root).engine).collect();
    for i in 0..30 {
        let q = &qs[i % qs.len()].q;
        for e in eng.iter_mut() {
            e.search(q, limit);
        }
    }
    if eng.iter().any(|e| e.kind() == Kind::Fff) {
        std::thread::sleep(Duration::from_secs(1)); // let fff's post-scan git work settle
    }

    let mut stats: BTreeMap<(Kind, &'static str), Stat> = BTreeMap::new();
    let pace = Duration::from_millis(a.num("--pace-ms", 70) as u64);

    // 5. Showcase: type three queries, side by side (live only, not measured).
    if cx.t.live && !a.flag("--no-showcase") && !targets.is_empty() {
        let mut demo: Vec<String> = vec![base(&targets[0]).to_string()];
        if let Some(q) = qs.iter().find(|q| q.class == "typo" && q.detail == "swap") {
            demo.push(q.q.clone());
        }
        if let Some(q) = repo_qs.first() {
            demo.push(q.q.clone());
        } else if let Some(q) = qs.iter().find(|q| q.class == "folder") {
            demo.push(q.q.clone());
        }
        for d in &demo {
            let chars: Vec<char> = d.chars().collect();
            for n in 1..=chars.len() {
                let q: String = chars[..n].iter().collect();
                let res: Vec<(Kind, Vec<String>, f64)> = eng
                    .iter_mut()
                    .map(|e| {
                        let t = Instant::now();
                        let r = e.search(&q, limit);
                        (e.kind(), r, t.elapsed().as_secs_f64() * 1e6)
                    })
                    .collect();
                showcase_frame(&mut cx, &q, &res);
                std::thread::sleep(if n == chars.len() { pace * 25 } else { pace });
            }
        }
    }

    // 6. The race: every query on every engine, rotating who goes first.
    let mut rows: Vec<String> = Vec::new();
    let total = qs.len();
    for (i, q) in qs.iter().enumerate() {
        let mut res: Vec<(Kind, f64, bool)> = Vec::new();
        let n = eng.len();
        for j in 0..n {
            let e = &mut eng[(i + j) % n];
            let t = Instant::now();
            let r = e.search(&q.q, limit);
            let us = t.elapsed().as_secs_f64() * 1e6;
            let hit = r.first().is_some_and(|p| base(p) == base(&q.target));
            let s = stats.entry((e.kind(), q.class)).or_default();
            s.us.push(us);
            s.tries += 1;
            s.hits += hit as usize;
            let top = &r[..r.len().min(10)];
            s.shown += top.len();
            s.junk += top.iter().filter(|p| is_junk(p)).count();
            res.push((e.kind(), us, hit));
        }
        res.sort_by_key(|r| kinds.iter().position(|k| *k == r.0));
        rows.push(race_row(&q.q, q.detail, &res));
        if rows.len() > 400 {
            rows.drain(..200);
        }
        if cx.t.live && (cx.t.due() || i + 1 == total) {
            race_frame(
                &mut cx,
                "Find a file by name",
                "names + 4 kinds of typo + folder+name",
                i + 1,
                total,
                &rows,
                &stats,
                &["exact", "typo", "folder"],
            );
            if i + 1 == total {
                cx.t.marker("race");
            }
        } else if !cx.t.live && (i % 100 == 0 || i + 1 == total) {
            cx.t.out(&format!("{:>5}/{total}  {}\n", i + 1, rows.last().unwrap()));
        }
    }

    // 7. Typing: every prefix of each name, as a launcher would see it.
    let nkeys: usize = keys.iter().map(|t| base(t).chars().count()).sum();
    let mut done = 0;
    rows.clear();
    for (i, t) in keys.iter().enumerate() {
        let name: Vec<char> = base(t).chars().collect();
        for n in 1..=name.len() {
            let q: String = name[..n].iter().collect();
            let mut res = Vec::new();
            for j in 0..eng.len() {
                let len = eng.len();
                let e = &mut eng[(i + j) % len];
                let tm = Instant::now();
                let r = e.search(&q, limit);
                let us = tm.elapsed().as_secs_f64() * 1e6;
                let hit = n == name.len() && r.first().is_some_and(|p| base(p) == base(t));
                let s = stats.entry((e.kind(), "keys")).or_default();
                s.us.push(us);
                if n == name.len() {
                    s.tries += 1;
                    s.hits += hit as usize;
                }
                res.push((e.kind(), us, hit || n < name.len()));
            }
            res.sort_by_key(|r| kinds.iter().position(|k| *k == r.0));
            rows.push(race_row(&q, "key", &res));
            done += 1;
            if cx.t.live && (cx.t.due() || done == nkeys) {
                race_frame(
                    &mut cx,
                    "While typing",
                    "every prefix of each name, one search per keystroke",
                    done,
                    nkeys,
                    &rows,
                    &stats,
                    &["keys"],
                );
            }
        }
    }

    // 8. repo: queries (flashfind only; fff and fsearch have no repo filter).
    if let Some(fi) = eng.iter().position(|e| e.kind() == Kind::Flashfind) {
        for q in &repo_qs {
            let t = Instant::now();
            let r = eng[fi].search(&q.q, limit);
            let us = t.elapsed().as_secs_f64() * 1e6;
            let s = stats.entry((Kind::Flashfind, "repo")).or_default();
            s.us.push(us);
            s.tries += 1;
            s.hits += r.first().is_some_and(|p| base(p) == base(&q.target)) as usize;
        }
    }
    let load1 = load();

    // 9. Summary card + table.
    let card = summary_card(&mut cx, &stats, &meas, qs.len(), nkeys, multi);
    if cx.t.live {
        animate_card(&mut cx, &card, &stats);
        cx.t.marker("summary");
        std::thread::sleep(Duration::from_millis(1500));
    }
    let table = summary_table(&kinds, &stats, &meas, multi);
    cx.t.out(&format!("\r\n{}\r\n", table.replace('\n', "\r\n")));
    cx.t.finish();

    let md = markdown(
        &label,
        &cx,
        &kinds,
        &stats,
        &meas,
        multi,
        qs.len(),
        nkeys,
        repo_qs.len(),
        load0,
        load1,
        all_entries,
    );
    if let Some(p) = a.get("--md") {
        std::fs::write(p, &md).unwrap();
    }
    if let Some(p) = a.get("--json") {
        let mut cls = serde_json::Map::new();
        for ((k, c), s) in &stats {
            cls.insert(
                format!("{}/{c}", k.name()),
                json!({"n": s.us.len(), "p50_us": pct(&s.us, 50.0), "p95_us": pct(&s.us, 95.0), "p99_us": pct(&s.us, 99.0),
                "top1": s.hits as f64 / s.tries.max(1) as f64, "tries": s.tries, "junk_top10": s.junk as f64 / s.shown.max(1) as f64}),
            );
        }
        let v = json!({"dataset": label, "visible_files": cx.files, "all_entries": all_entries, "repos": repos.len(), "machine": cx.mach, "os": cx.os,
            "load_before": load0, "load_after": load1, "queries": qs.len(), "keystrokes": nkeys, "repo_queries": repo_qs.len(), "limit": limit,
            "index": meas, "classes": cls});
        std::fs::write(p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }
}

fn race_row(q: &str, detail: &str, res: &[(Kind, f64, bool)]) -> String {
    let best = res.iter().map(|r| r.1).fold(f64::INFINITY, f64::min);
    let mut s = format!("  {}{}{RESET} {}{}{RESET}", white(), pad(&trunc(q, 34), 34), grey(), pad(detail, 7));
    for &(_, us, hit) in res {
        let mark = if hit {
            format!("{}✓{RESET}", grey())
        } else {
            format!("{}✗{RESET}", red())
        };
        let t = if us == best && res.len() > 1 {
            format!("{BOLD}{}{}{RESET}", green(), rpad(&ms(us), 9))
        } else {
            format!("{}{}{RESET}", grey(), rpad(&ms(us), 9))
        };
        s.push_str(&format!("  {t} {mark}"));
    }
    if let Some(f) = res.iter().find(|r| r.0 == Kind::Flashfind)
        && res.len() > 1
    {
        let other = res
            .iter()
            .filter(|r| r.0 != Kind::Flashfind)
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        if f.1 <= other.1 {
            s.push_str(&format!("   {}{:.1}×{RESET}", green(), other.1 / f.1.max(0.01)));
        } else {
            s.push_str(&format!("   {}{} {:.1}×{RESET}", red(), other.0.name(), f.1 / other.1.max(0.01)));
        }
    }
    s
}

#[allow(clippy::too_many_arguments)]
fn race_frame(
    cx: &mut Ctx,
    title: &str,
    sub: &str,
    done: usize,
    total: usize,
    rows: &[String],
    stats: &BTreeMap<(Kind, &'static str), Stat>,
    classes: &[&str],
) {
    let mut l = cx.header();
    l.push(format!(
        "  {BOLD}{}{title}{RESET}  {}  {}{} / {}{RESET}  {}{sub}{RESET}",
        white(),
        bar(done, total, 28),
        white(),
        thousands(done),
        thousands(total),
        grey()
    ));
    l.push(String::new());
    let mut head = format!("  {}{}{} {}", grey(), pad("query", 34), RESET, pad("", 7));
    for &k in &cx.kinds {
        head.push_str(&format!("  {BOLD}{}{}{RESET}  ", color_of(k), rpad(k.name(), 9)));
    }
    head.push_str(&format!("   {}flashfind vs best other{RESET}", grey()));
    l.push(head);
    l.push(format!("  {}{}{RESET}", grey(), "─".repeat(cx.t.w.saturating_sub(4).min(110))));
    let room = cx.t.h.saturating_sub(l.len() + 9).max(4);
    for r in &rows[rows.len().saturating_sub(room)..] {
        l.push(r.clone());
    }
    for _ in rows.len()..room {
        l.push(String::new());
    }
    l.push(format!("  {}{}{RESET}", grey(), "─".repeat(cx.t.w.saturating_sub(4).min(110))));
    let agg = |k: Kind| -> Stat {
        let mut s = Stat::default();
        for c in classes {
            if let Some(x) = stats.get(&(k, *c)) {
                s.us.extend(&x.us);
                s.hits += x.hits;
                s.tries += x.tries;
            }
        }
        s
    };
    let aggs: Vec<Stat> = cx.kinds.iter().map(|&k| agg(k)).collect();
    let p50s: Vec<f64> = aggs.iter().map(|s| pct(&s.us, 50.0)).collect();
    let best_i = (0..p50s.len()).min_by(|&a, &b| p50s[a].partial_cmp(&p50s[b]).unwrap_or(std::cmp::Ordering::Equal));
    for (lab, f) in [("median", 50.0), ("p95", 95.0)] {
        let mut s = format!("  {}{}{RESET} {}", grey(), pad(lab, 34), pad("", 7));
        for (i, st) in aggs.iter().enumerate() {
            let v = pct(&st.us, f);
            let c = if f == 50.0 && Some(i) == best_i && aggs.len() > 1 {
                format!("{BOLD}{}", green())
            } else {
                white()
            };
            s.push_str(&format!("  {c}{}{RESET}  ", rpad(&ms(v), 9)));
        }
        l.push(s);
    }
    let mut s = format!("  {}{}{RESET} {}", grey(), pad("right file first", 34), pad("", 7));
    for st in &aggs {
        s.push_str(&format!(
            "  {}{}{RESET}  ",
            white(),
            rpad(&format!("{:.0}%", 100.0 * st.hits as f64 / st.tries.max(1) as f64), 9)
        ));
    }
    l.push(s);
    let mut s = format!("  {}{}{RESET} {}", grey(), pad("latency, last 9 queries", 34), pad("", 7));
    for (i, st) in aggs.iter().enumerate() {
        s.push_str(&format!("  {}{}{RESET}  ", color_of(cx.kinds[i]), rpad(&spark(&st.us, 9), 9)));
    }
    l.push(s);
    cx.t.frame(&l);
}

fn showcase_frame(cx: &mut Ctx, q: &str, res: &[(Kind, Vec<String>, f64)]) {
    let mut l = cx.header();
    l.push(format!("  {}❯{RESET} {BOLD}{}{q}{RESET}{}▌{RESET}", blue(), white(), blue()));
    l.push(String::new());
    let n = res.len().max(1);
    let col = (cx.t.w.saturating_sub(4 + 3 * (n - 1))) / n;
    let best = res.iter().map(|r| r.2).fold(f64::INFINITY, f64::min);
    let sep = format!(" {}│{RESET} ", grey());
    let mut head = Vec::new();
    for (k, _, us) in res {
        let badge = ms(*us);
        let c = if *us == best { format!("{BOLD}{}", green()) } else { red() };
        head.push(format!(
            "{BOLD}{}{}{RESET}{}{c}{badge}{RESET}",
            color_of(*k),
            k.name(),
            " ".repeat(col.saturating_sub(k.name().len() + badge.len()))
        ));
    }
    l.push(format!("  {}", head.join(&sep)));
    l.push(format!(
        "  {}",
        (0..n).map(|_| format!("{}{}{RESET}", grey(), "─".repeat(col))).collect::<Vec<_>>().join(&sep)
    ));
    for i in 0..12 {
        let cells: Vec<String> = res
            .iter()
            .map(|(_, r, _)| match r.get(i) {
                Some(p) => {
                    let (d, name) = p.rsplit_once('/').unwrap_or(("", p));
                    let name = trunc(name, col);
                    let d = trunc(d, col.saturating_sub(name.chars().count() + 2));
                    pad(&format!("{}{name}{RESET}  {}{d}{RESET}", white(), grey()), col)
                }
                None => " ".repeat(col),
            })
            .collect();
        l.push(format!("  {}", cells.join(&sep)));
    }
    cx.t.frame(&l);
}

struct Metric {
    title: String,
    note: String,
    lower_better_unit: &'static str, // "ms" or "MB"
    vals: Vec<(Kind, Option<f64>)>,  // ms or MB
}

fn summary_card(
    cx: &mut Ctx,
    stats: &BTreeMap<(Kind, &'static str), Stat>,
    meas: &BTreeMap<&str, Value>,
    nq: usize,
    nkeys: usize,
    _multi: bool,
) -> Vec<Metric> {
    let kinds = cx.kinds.clone();
    let p50 = |k: Kind, cls: &[&str]| -> Option<f64> {
        let v: Vec<f64> = cls.iter().filter_map(|c| stats.get(&(k, *c))).flat_map(|s| s.us.iter().copied()).collect();
        (!v.is_empty()).then(|| pct(&v, 50.0) / 1000.0)
    };
    let m = |k: Kind, f: &str| meas.get(k.name()).and_then(|v| v[f].as_f64());
    vec![
        Metric {
            title: "Find a file by name".into(),
            note: format!("median of {} exact names", thousands(stats.get(&(kinds[0], "exact")).map_or(0, |s| s.us.len()))),
            lower_better_unit: "ms",
            vals: kinds.iter().map(|&k| (k, p50(k, &["exact"]))).collect(),
        },
        Metric {
            title: "With a typo".into(),
            note: "swap · drop · extra · substitute".into(),
            lower_better_unit: "ms",
            vals: kinds.iter().map(|&k| (k, p50(k, &["typo"]))).collect(),
        },
        Metric {
            title: "While typing".into(),
            note: format!("median of {} keystrokes", thousands(nkeys)),
            lower_better_unit: "ms",
            vals: kinds.iter().map(|&k| (k, p50(k, &["keys"]))).collect(),
        },
        Metric {
            title: "Ready, cold".into(),
            note: "crawl + index + first answer".into(),
            lower_better_unit: "ms",
            vals: kinds.iter().map(|&k| (k, m(k, "ready_ms"))).collect(),
        },
        Metric {
            title: "Ready, warm".into(),
            note: "saved index → first answer · fff keeps no index (rescans)".into(),
            lower_better_unit: "ms",
            vals: kinds.iter().map(|&k| (k, m(k, "warm_ms"))).collect(),
        },
        Metric {
            title: "Memory".into(),
            note: format!("index for this folder · {} queries total", thousands(nq + nkeys)),
            lower_better_unit: "MB",
            vals: kinds.iter().map(|&k| (k, m(k, "memory_mb"))).collect(),
        },
    ]
}

fn fmt_val(v: f64, unit: &str) -> String {
    if unit == "MB" { format!("{v:.0} MB") } else { ms(v * 1000.0) }
}

fn card_lines(cx: &Ctx, card: &[Metric], frac: f64, stats: &BTreeMap<(Kind, &'static str), Stat>) -> Vec<String> {
    let mut l = cx.header();
    let width = cx.t.w.saturating_sub(34).max(20);
    for m in card {
        let have: Vec<(Kind, f64)> = m.vals.iter().filter_map(|(k, v)| v.map(|v| (*k, v))).collect();
        let hi = have.iter().map(|x| x.1).fold(0.0, f64::max).max(1e-9);
        let lo = have.iter().cloned().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let ours = have.iter().find(|x| x.0 == Kind::Flashfind).map(|x| x.1);
        let verdict = match (ours, lo) {
            (Some(o), Some(lo)) if have.len() > 1 => {
                let next = have.iter().filter(|x| x.0 != Kind::Flashfind).map(|x| x.1).fold(f64::INFINITY, f64::min);
                let word = if m.lower_better_unit == "MB" { "less" } else { "faster" };
                if lo.0 == Kind::Flashfind {
                    format!("{BOLD}{}{:.1}× {word}{RESET}", green(), next / o.max(1e-9))
                } else {
                    format!("{BOLD}{}{} {:.1}× {word}{RESET}", red(), lo.0.name(), o / lo.1.max(1e-9))
                }
            }
            _ => String::new(),
        };
        l.push(format!("  {BOLD}{}{}{RESET}  {verdict}  {}{}{RESET}", white(), m.title, grey(), m.note));
        for (k, v) in &m.vals {
            let line = match v {
                Some(v) => {
                    let n = ((width as f64 * v / hi * frac).round() as usize).max(1);
                    let val = if frac >= 0.999 {
                        format!(" {BOLD}{}{}{RESET}", color_of(*k), fmt_val(*v, m.lower_better_unit))
                    } else {
                        String::new()
                    };
                    format!("{}{}{RESET}{val}", color_of(*k), "█".repeat(n))
                }
                None => format!("{}n/a{RESET}", grey()),
            };
            l.push(format!("    {}{:<10}{RESET}{line}", white(), k.name()));
        }
        l.push(String::new());
    }
    let mut s = format!("  {BOLD}{}Right file first{RESET}  ", white());
    for (lab, c) in [("exact", "exact"), ("typo", "typo"), ("folder+name", "folder")] {
        s.push_str(&format!("{}{lab}{RESET} ", grey()));
        for &k in &cx.kinds {
            if let Some(st) = stats.get(&(k, c)) {
                s.push_str(&format!("{BOLD}{}{:.0}%{RESET} ", color_of(k), 100.0 * st.hits as f64 / st.tries.max(1) as f64));
            }
        }
        s.push_str("  ");
    }
    l.push(s);
    if let Some(st) = stats.get(&(Kind::Flashfind, "repo")) {
        l.push(format!(
            "  {BOLD}{}repo:{RESET} {}queries (flashfind only){RESET} {BOLD}{}{:.0}%{RESET} {}first, median {}{RESET}",
            white(),
            grey(),
            blue(),
            100.0 * st.hits as f64 / st.tries.max(1) as f64,
            grey(),
            ms(pct(&st.us, 50.0))
        ));
    }
    let junk: Vec<String> = cx
        .kinds
        .iter()
        .filter_map(|&k| {
            let (j, n) = ["exact", "typo", "folder"]
                .iter()
                .filter_map(|c| stats.get(&(k, *c)))
                .fold((0, 0), |a, s| (a.0 + s.junk, a.1 + s.shown));
            (n > 0).then(|| format!("{BOLD}{}{:.1}%{RESET}", color_of(k), 100.0 * j as f64 / n as f64))
        })
        .collect();
    l.push(format!(
        "  {BOLD}{}Junk in top 10{RESET}  {}  {}node_modules, target, .venv, build, Library, .git…{RESET}",
        white(),
        junk.join(" "),
        grey()
    ));
    l
}

fn animate_card(cx: &mut Ctx, card: &[Metric], stats: &BTreeMap<(Kind, &'static str), Stat>) {
    for i in 1..=24 {
        let l = card_lines(cx, card, i as f64 / 24.0, stats);
        cx.t.frame(&l);
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn summary_table(kinds: &[Kind], stats: &BTreeMap<(Kind, &'static str), Stat>, meas: &BTreeMap<&str, Value>, multi: bool) -> String {
    let mut s = String::from("| class | engine | n | p50 | p95 | p99 | right file first | junk in top 10 |\n|---|---|---|---|---|---|---|---|\n");
    let mut classes = vec!["exact", "typo", "folder", "keys"];
    if multi {
        classes.push("repo");
    }
    for c in classes {
        let best = kinds
            .iter()
            .filter_map(|&k| stats.get(&(k, c)).map(|s| pct(&s.us, 50.0)))
            .fold(f64::INFINITY, f64::min);
        for &k in kinds {
            if let Some(st) = stats.get(&(k, c)) {
                let p = pct(&st.us, 50.0);
                let p50 = if p == best && kinds.len() > 1 { format!("**{}**", ms(p)) } else { ms(p) };
                let junk = if c == "keys" || c == "repo" {
                    "–".into()
                } else {
                    format!("{:.1}%", 100.0 * st.junk as f64 / st.shown.max(1) as f64)
                };
                s += &format!(
                    "| {c} | {} | {} | {p50} | {} | {} | {:.0}% | {junk} |\n",
                    k.name(),
                    st.us.len(),
                    ms(pct(&st.us, 95.0)),
                    ms(pct(&st.us, 99.0)),
                    100.0 * st.hits as f64 / st.tries.max(1) as f64
                );
            }
        }
    }
    s += "\n| engine | entries indexed | ready, cold | ready, warm | memory |\n|---|---|---|---|---|\n";
    for &k in kinds {
        let m = &meas[k.name()];
        s += &format!(
            "| {} | {} | {} | {} | {:.0} MB |\n",
            k.name(),
            thousands(m["entries"].as_u64().unwrap_or(0) as usize),
            ms(m["ready_ms"].as_f64().unwrap_or(f64::NAN) * 1e3),
            m["warm_ms"].as_f64().map_or("n/a (no saved index)".into(), |v| ms(v * 1e3)),
            m["memory_mb"].as_f64().unwrap_or(f64::NAN)
        );
    }
    s
}

#[allow(clippy::too_many_arguments)]
fn markdown(
    label: &str,
    cx: &Ctx,
    kinds: &[Kind],
    stats: &BTreeMap<(Kind, &'static str), Stat>,
    meas: &BTreeMap<&str, Value>,
    multi: bool,
    nq: usize,
    nkeys: usize,
    nrepo: usize,
    l0: [f64; 3],
    l1: [f64; 3],
    all: usize,
) -> String {
    format!(
        "### {label}\n\n{} visible files (not hidden, not gitignored; {} entries in total) · {} · {} · load {:.1} before, {:.1} after · {} name queries, {} keystrokes{} · top 50 · warm\n\n{}",
        thousands(cx.files),
        thousands(all),
        cx.mach,
        cx.os,
        l0[0],
        l1[0],
        thousands(nq),
        thousands(nkeys),
        if multi { format!(", {nrepo} repo: queries") } else { String::new() },
        summary_table(kinds, stats, meas, multi)
    )
}
