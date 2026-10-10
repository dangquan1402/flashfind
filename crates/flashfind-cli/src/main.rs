//! `flashfind` – index, search, benchmark.

mod bench;
mod report;
mod treegen;
mod util;

use flashfind_core::walk::ScanOptions;
use flashfind_core::{CancelToken, Index, Query, Session};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const USAGE: &str = "flashfind – fast whole-disk file name search

USAGE:
  flashfind index <root> [-o FILE] [--threads N] [--portable] [--skip PATH]... [--stats-json FILE]
  flashfind search [-i FILE] [--limit N] [--json] [--explain] <query...>
  flashfind gen --root DIR --files N [--seed S] --queries FILE
  flashfind bench -i FILE --queries FILE [--repeat R] [--limit N] [--out FILE] [--label NAME]
  flashfind report --machine FILE --ff-build F --ff-query F [--fs-build F --fs-query F] --out FILE

Index file defaults to $FLASHFIND_INDEX or ~/.flashfind/index.ffx.
Query syntax: see `flashfind help query`.";

const QUERY_HELP: &str =
    "words are fuzzy (typo-tolerant from 4 chars); all must match the name or a parent folder
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
            .map(|v| {
                v.parse()
                    .unwrap_or_else(|_| die(format!("bad number for {}: {v}", names[0])))
            })
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
            let root = a
                .opt(&["--root"])
                .unwrap_or_else(|| die("--root is required"));
            let files = a.num(&["--files", "-n"], 1_000_000usize);
            let seed = a.num(&["--seed"], 42u64);
            let queries = a
                .opt(&["--queries"])
                .unwrap_or_else(|| die("--queries is required"));
            treegen::run(&treegen::GenOpts {
                root: root.into(),
                files,
                seed,
                queries: queries.into(),
            })
            .unwrap_or_else(|e| die(e));
        }
        "bench" => bench::run(a),
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
    let out = a
        .opt(&["-o", "--out"])
        .map(PathBuf::from)
        .unwrap_or_else(default_index);
    let threads = a.num(&["--threads", "-j"], 8usize);
    let portable = a.flag("--portable");
    let skip: Vec<Vec<u8>> = a
        .opts("--skip")
        .into_iter()
        .map(|s| s.into_bytes())
        .collect();
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let t1 = Instant::now();
    let idx = Index::build(ls, &root, now);
    let build = t1.elapsed();
    let t2 = Instant::now();
    idx.save(&out)
        .unwrap_or_else(|e| die(format!("save {}: {e}", out.display())));
    let save = t2.elapsed();
    let rss = util::peak_rss();
    let backend = if portable {
        "read_dir"
    } else {
        flashfind_core::walk::backend()
    };
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

fn cmd_search(mut a: Args) {
    let file = a
        .opt(&["-i", "--index"])
        .map(PathBuf::from)
        .unwrap_or_else(default_index);
    let limit: Option<usize> = a
        .opt(&["--limit", "-n"])
        .map(|v| v.parse().unwrap_or_else(|_| die("bad --limit")));
    let json = a.flag("--json");
    let explain = a.flag("--explain");
    let qs = a.rest().join(" ");
    let t0 = Instant::now();
    let idx = Arc::new(
        Index::load(&file)
            .unwrap_or_else(|e| die(format!("{e}\n(run `flashfind index <root>` first)"))),
    );
    let load = t0.elapsed();
    let mut q = Query::parse(&qs).unwrap_or_else(|e| die(e));
    if let Some(l) = limit {
        q.limit = l;
    }
    let mut s = Session::new(idx.clone());
    let reps: usize = std::env::var("FLASHFIND_REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut r = s.search(&q, &CancelToken::new()).unwrap();
    let t1 = Instant::now();
    for _ in 1..reps {
        s.clear();
        r = s.search(&q, &CancelToken::new()).unwrap();
    }
    let took = if reps > 1 {
        t1.elapsed() / (reps as u32 - 1)
    } else {
        t1.elapsed()
    };
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
                serde_json::json!({"path": idx.path_string(h.entry), "score": h.score, "kind": KINDS[(idx.ent_kind()[e] & 3) as usize], "size": idx.ent_size()[e], "mtime": idx.ent_mtime()[e]})
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
