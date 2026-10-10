//! `flashfind` – index, search, benchmark.

mod bench;
mod bench_ux;
mod code;
mod report;
mod treegen;
mod util;

use flashfind_core::walk::ScanOptions;
use flashfind_core::{CancelToken, Index, Query, Session};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const USAGE: &str = "flashfind – find anything across all your code projects (and fast whole-disk file search)

USAGE (code projects):
  flashfind projects [--root DIR]... [--json] [--refresh] [filter]
  flashfind code [--root DIR]... [--json] [--flat] [--why] [--limit N] [--recent 7d] [--refresh] <query...>
        query adds repo:<name>  lang:<rust|ts|py|go|swift|...>  dirty:  and a file:LINE suffix to the syntax below
        --pick N        record result N as the file you meant and print its path (`cursor -g $(flashfind code --pick 1 q)`)
        --why           show scores and why personal ranking moved a result;  --baseline: 0.1 ranking
  flashfind pick <path>... [--query Q] [--action open|copy|reveal|cursor]   remember a pick (learned ranking)
  flashfind pick --show | --forget          inspect / clear the memory ($FLASHFIND_DATA or ~/.local/share/flashfind)
  flashfind mcp [--root DIR]... [--max-age SECS]      MCP server on stdio (find_files, list_projects, recent_files)
  flashfind gen-repos --root DIR --repos N [--seed S] --queries FILE
  flashfind bench-code --root DIR --queries FILE [--out JSON --md MD]
  flashfind bench-live-code --dir DIR-IN-A-REPO [-n N]   change-to-searchable latency (temp files, cleaned up)
  flashfind bench-ux [--real] [--seed S] [--md FILE]    replay a pick history: right-file-first, keystrokes-to-open per ranking variant
  flashfind bench-vs --root DIR [--label NAME] ...       live terminal race vs fff and fsearch (bench/vs, built separately)
  Roots default to $FLASHFIND_CODE_ROOTS or ~ (Library, media and hidden folders skipped);
  the code index is cached in $FLASHFIND_CACHE or ~/.cache/flashfind and follows changes live
  (FSEvents on macOS; elsewhere rebuilt when older than --max-age, default 600 s).

USAGE (whole disk):
  flashfind index <root> [-o FILE] [--threads N] [--portable] [--skip PATH]... [--stats-json FILE]
  flashfind search [-i FILE] [--limit N] [--json] [--explain] <query...>
  flashfind watch [<root>] [-i FILE] [--poll MS]   live index: crawl <root> (or open FILE), watch for
        changes (FSEvents on macOS, polling elsewhere), answer one query per stdin line
  flashfind gen --root DIR --files N [--seed S] --queries FILE
  flashfind bench -i FILE --queries FILE [--repeat R] [--limit N] [--out FILE] [--label NAME]
  flashfind report --machine FILE --ff-build F --ff-query F [--fs-build F --fs-query F] --out FILE

Index file defaults to $FLASHFIND_INDEX or ~/.flashfind/index.ffx.
Query syntax: see `flashfind help query`.";

const QUERY_HELP: &str = "words are fuzzy (typo-tolerant from 4 chars); all must match the name or a parent folder
  'x substring   ^x prefix   x$ suffix   =x or ^x$ exact   !x / -x exclude
  ext:pdf,md  type:file|dir|app|image|video|audio|doc|code|archive|font
  size:>5mb size:<1k size:1mb..10mb      mtime:<7d mtime:3d mtime:>1y mtime:2026-01-15 mtime:today
  in:~/Documents   limit:100";

fn default_index() -> PathBuf {
    if let Ok(p) = std::env::var("FLASHFIND_INDEX") {
        return p.into();
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".flashfind/index.ffx")
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("flashfind: {msg}");
    std::process::exit(2)
}

pub struct Args(Vec<String>);

impl Args {
    pub fn flag(&mut self, name: &str) -> bool {
        if let Some(i) = self.0.iter().position(|a| a == name) {
            self.0.remove(i);
            true
        } else {
            false
        }
    }
    pub fn opt(&mut self, names: &[&str]) -> Option<String> {
        let i = self.0.iter().position(|a| names.contains(&a.as_str()))?;
        if i + 1 >= self.0.len() {
            die(format!("{} needs a value", self.0[i]));
        }
        let v = self.0.remove(i + 1);
        self.0.remove(i);
        Some(v)
    }
    pub fn opts(&mut self, name: &str) -> Vec<String> {
        let mut v = Vec::new();
        while let Some(x) = self.opt(&[name]) {
            v.push(x);
        }
        v
    }
    pub fn num<T: std::str::FromStr>(&mut self, names: &[&str], default: T) -> T {
        self.opt(names)
            .map(|v| v.parse().unwrap_or_else(|_| die(format!("bad number for {}: {v}", names[0]))))
            .unwrap_or(default)
    }
    pub fn rest(self) -> Vec<String> {
        self.0
    }
}

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        println!("{USAGE}");
        return;
    }
    let cmd = argv.remove(0);
    let mut a = Args(argv);
    match cmd.as_str() {
        "index" => cmd_index(a),
        "search" | "s" => cmd_search(a),
        "gen" => {
            let root = a.opt(&["--root"]).unwrap_or_else(|| die("--root is required"));
            let files = a.num(&["--files", "-n"], 1_000_000usize);
            let seed = a.num(&["--seed"], 42u64);
            let queries = a.opt(&["--queries"]).unwrap_or_else(|| die("--queries is required"));
            treegen::run(&treegen::GenOpts {
                root: root.into(),
                files,
                seed,
                queries: queries.into(),
            })
            .unwrap_or_else(|e| die(e));
        }
        "bench" => bench::run(a),
        "watch" => cmd_watch(a),
        "projects" => code::cmd_projects(a),
        "code" | "c" => code::cmd_code(a),
        "pick" => code::cmd_pick(a),
        "mcp" => code::cmd_mcp(a),
        "gen-repos" => code::cmd_gen_repos(a),
        "bench-code" => code::cmd_bench_code(a),
        "bench-live-code" => code::cmd_bench_live_code(a),
        "bench-vs" => bench_vs(),
        "bench-ux" => bench_ux::cmd_bench_ux(a),
        "report" => report::run(a),
        "help" | "-h" | "--help" => {
            if a.rest().first().map(String::as_str) == Some("query") {
                println!("{QUERY_HELP}");
            } else {
                println!("{USAGE}");
            }
        }
        "--version" | "-V" => println!("flashfind {}", env!("CARGO_PKG_VERSION")),
        _ => die(format!("unknown command {cmd}\n\n{USAGE}")),
    }
}

fn cmd_index(mut a: Args) {
    let out = a.opt(&["-o", "--out"]).map(PathBuf::from).unwrap_or_else(default_index);
    let threads = a.num(&["--threads", "-j"], 8usize);
    let portable = a.flag("--portable");
    let skip: Vec<Vec<u8>> = a.opts("--skip").into_iter().map(|s| s.into_bytes()).collect();
    let stats_json = a.opt(&["--stats-json"]);
    let rest = a.rest();
    let root = rest.first().unwrap_or_else(|| die("index needs a <root>"));
    let root = std::fs::canonicalize(root).unwrap_or_else(|e| die(format!("{root}: {e}")));
    let opts = ScanOptions { threads, skip };
    let t0 = Instant::now();
    let ls = if portable {
        flashfind_core::walk::scan_portable(&root, &opts)
    } else {
        flashfind_core::walk::scan(&root, &opts)
    };
    let crawl = t0.elapsed();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let t1 = Instant::now();
    let idx = Index::build(ls, &root, now);
    let build = t1.elapsed();
    let t2 = Instant::now();
    idx.save(&out).unwrap_or_else(|e| die(format!("save {}: {e}", out.display())));
    let save = t2.elapsed();
    let rss = util::peak_rss();
    let backend = if portable { "read_dir" } else { flashfind_core::walk::backend() };
    eprintln!(
        "indexed {} entries ({} distinct names, {} dirs) from {} [{backend}, {threads} threads]\n  crawl {crawl:.2?}  build {build:.2?}  save {save:.2?}  index {:.1} MB  peak RSS {:.0} MB  -> {}",
        idx.n,
        idx.u,
        idx.d,
        root.display(),
        idx.bytes() as f64 / 1e6,
        rss as f64 / 1e6,
        out.display()
    );
    if let Some(f) = stats_json {
        let v = serde_json::json!({
            "engine": "flashfind", "backend": backend, "threads": threads,
            "entries": idx.n, "names": idx.u, "dirs": idx.d,
            "crawl_ms": crawl.as_secs_f64() * 1e3, "build_ms": build.as_secs_f64() * 1e3, "save_ms": save.as_secs_f64() * 1e3,
            "index_bytes": idx.bytes(), "peak_rss_bytes": rss,
        });
        std::fs::write(&f, serde_json::to_string_pretty(&v).unwrap()).unwrap_or_else(|e| die(e));
    }
}

const KINDS: [&str; 4] = ["file", "dir", "link", "other"];

fn cmd_watch(mut a: Args) {
    use flashfind_core::{Live, LiveOptions, LiveSession};
    use std::io::BufRead;
    let file = a.opt(&["-i", "--index"]).map(PathBuf::from).unwrap_or_else(default_index);
    let limit = a.num(&["--limit", "-n"], 10usize);
    let mut opts = LiveOptions::default();
    if let Some(ms) = a.opt(&["--poll"]) {
        opts.poll = std::time::Duration::from_millis(ms.parse().unwrap_or_else(|_| die("bad --poll")));
        opts.force_poll = true;
    }
    let rest = a.rest();
    let live = match rest.first() {
        Some(root) => Live::crawl(std::path::Path::new(root), Some(file), opts),
        None => Live::open(&file, opts).unwrap_or_else(|e| die(format!("{}: {e}", file.display()))),
    };
    live.watch().unwrap_or_else(|e| die(e));
    let mut s = LiveSession::new(live.clone());
    let show = |live: &Live| {
        let p = live.progress();
        let phase = ["crawling", "building", "ready"][p.phase.min(2) as usize];
        eprintln!(
            "[{phase}] {} entries, overlay {}, removed {}, compactions {}, last event {}",
            p.entries, p.overlay, p.dead, p.compactions, p.last_event_id
        );
    };
    show(&live);
    eprintln!("type a query per line (empty line: status, Ctrl-D: quit)");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            show(&live);
            continue;
        }
        let mut q = match Query::parse(&line) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("{e}");
                continue;
            }
        };
        q.limit = limit;
        let t = Instant::now();
        let r = s.search(&q, &CancelToken::new()).unwrap();
        for h in &r.hits {
            println!("{}", h.path_str());
        }
        eprintln!(
            "{} matches in {:.2?}{}",
            r.total,
            t.elapsed(),
            if r.partial { " (partial: still crawling)" } else { "" }
        );
    }
    live.checkpoint();
    live.unwatch();
}

fn cmd_search(mut a: Args) {
    let file = a.opt(&["-i", "--index"]).map(PathBuf::from).unwrap_or_else(default_index);
    let limit: Option<usize> = a.opt(&["--limit", "-n"]).map(|v| v.parse().unwrap_or_else(|_| die("bad --limit")));
    let json = a.flag("--json");
    let explain = a.flag("--explain");
    let qs = a.rest().join(" ");
    let t0 = Instant::now();
    let idx = Arc::new(Index::load(&file).unwrap_or_else(|e| die(format!("{e}\n(run `flashfind index <root>` first)"))));
    let load = t0.elapsed();
    let mut q = Query::parse(&qs).unwrap_or_else(|e| die(e));
    if let Some(l) = limit {
        q.limit = l;
    }
    let mut s = Session::new(idx.clone());
    let reps: usize = std::env::var("FLASHFIND_REPEAT").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut r = s.search(&q, &CancelToken::new()).unwrap();
    let t1 = Instant::now();
    for _ in 1..reps {
        s.clear();
        r = s.search(&q, &CancelToken::new()).unwrap();
    }
    let took = if reps > 1 { t1.elapsed() / (reps as u32 - 1) } else { t1.elapsed() };
    if explain {
        eprintln!(
            "names scored {} matched {} | name phase {} µs, entry phase {} µs",
            r.names_scored, r.names_matched, r.name_us, r.entry_us
        );
    }
    if json {
        let hits: Vec<_> = r
            .hits
            .iter()
            .map(|h| {
                let e = h.entry as usize;
                serde_json::json!({"path": idx.path_string(h.entry), "score": h.score, "kind": KINDS[(idx.ent_kind()[e] & 3) as usize], "size": idx.size(h.entry), "mtime": idx.mtime(h.entry)})
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"took_us": took.as_micros() as u64, "total": r.total, "total_exact": r.total_exact, "strategy": format!("{:?}", r.strategy), "hits": hits})
        );
        return;
    }
    for h in &r.hits {
        if explain {
            println!("{:6} {}", h.score, idx.path_string(h.entry));
        } else {
            println!("{}", idx.path_string(h.entry));
        }
    }
    eprintln!(
        "{}{} matches in {took:.2?} ({:?}; index load {load:.2?})",
        r.total,
        if r.total_exact { "" } else { "+" },
        r.strategy
    );
}

/// `flashfind bench-vs ...`: forwards to the `flashfind-vs` harness (bench/vs),
/// which links fff and fsearch and so is built separately (bench/vs.sh).
fn bench_vs() {
    let exe = std::env::current_exe().ok();
    let mut cands: Vec<std::path::PathBuf> = std::env::var_os("FLASHFIND_VS").map(Into::into).into_iter().collect();
    if let Some(dir) = exe.as_ref().and_then(|e| e.parent()) {
        cands.push(dir.join("flashfind-vs"));
        // target/release/flashfind -> bench/vs/target/release/flashfind-vs
        cands.push(dir.join("../../bench/vs/target/release/flashfind-vs"));
    }
    let Some(bin) = cands.into_iter().find(|p| p.is_file()) else {
        eprintln!("flashfind bench-vs needs the harness: run bench/vs.sh once (builds bench/vs with fff + fsearch), or set FLASHFIND_VS");
        std::process::exit(2);
    };
    let args: Vec<String> = std::env::args().skip(2).collect();
    let st = std::process::Command::new(bin).args(&args).status().unwrap_or_else(|e| die(e));
    std::process::exit(st.code().unwrap_or(1));
}
