//! fsearch baseline, driven through fsearch's public library API so that it
//! indexes only the benchmark tree (the daemon always crawls `/`).
//!
//!   fsearch-baseline build --root DIR --index FILE --stats FILE [--threads N]
//!   fsearch-baseline query --index FILE --queries FILE --out FILE [--repeat R] [--limit N]
//!
//! Output schema matches `flashfind bench` / `flashfind index --stats-json`.

use fsearch::{index::Index, live::Live, query::Query, query::Searcher};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn peak_rss() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let v = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") { v } else { v * 1024 }
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v[((p / 100.0 * (v.len() - 1) as f64).round() as usize).min(v.len() - 1)]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let home = std::env::var("HOME").unwrap_or_default();
    match args.get(1).map(String::as_str) {
        Some("build") => {
            let root = std::fs::canonicalize(arg(&args, "--root").expect("--root")).unwrap();
            let file = arg(&args, "--index").expect("--index");
            let threads: usize = arg(&args, "--threads").map_or(8, |t| t.parse().unwrap());
            let rb = root.to_str().unwrap().as_bytes().to_vec();
            let t0 = Instant::now();
            let ls = fsearch::walk::scan(&rb, threads);
            let crawl = t0.elapsed();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as u32;
            let t1 = Instant::now();
            let idx = Index::build(ls, 0, now, home.as_bytes());
            let build = t1.elapsed();
            let t2 = Instant::now();
            idx.save(file.as_ref()).unwrap();
            let save = t2.elapsed();
            let bytes = std::fs::metadata(&file).unwrap().len();
            let v = json!({
                "engine": "fsearch", "backend": "getattrlistbulk", "threads": threads, "entries": idx.n,
                "crawl_ms": crawl.as_secs_f64() * 1e3, "build_ms": build.as_secs_f64() * 1e3, "save_ms": save.as_secs_f64() * 1e3,
                "index_bytes": bytes, "peak_rss_bytes": peak_rss(),
            });
            eprintln!("fsearch: {} entries crawl {crawl:.2?} build {build:.2?} save {save:.2?} {:.1} MB", idx.n, bytes as f64 / 1e6);
            std::fs::write(arg(&args, "--stats").expect("--stats"), serde_json::to_string_pretty(&v).unwrap()).unwrap();
        }
        Some("query") => {
            let file = arg(&args, "--index").expect("--index");
            let repeat: usize = arg(&args, "--repeat").map_or(5, |t| t.parse().unwrap());
            let limit: usize = arg(&args, "--limit").map_or(50, |t| t.parse().unwrap());
            let t0 = Instant::now();
            let idx = Index::load(file.as_ref()).expect("load index");
            let bytes = idx.bytes();
            let live = Live::new(idx); // prefaults, like flashfind bench
            let load_ms = t0.elapsed().as_secs_f64() * 1e3;
            let s = Searcher { live: &live };
            let lines: Vec<Value> = std::fs::read_to_string(arg(&args, "--queries").expect("--queries")).unwrap().lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect();
            let mut lat: BTreeMap<String, Vec<f64>> = BTreeMap::new();
            let mut hit: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
            let mut p = Vec::new();
            let mut paths: Vec<String> = Vec::new();
            for pass in 0..repeat {
                for v in &lines {
                    let class = v["class"].as_str().unwrap().to_string();
                    let expect = v["expect"].as_str();
                    let qs: Vec<String> = if class == "keys" { v["seq"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect() } else { vec![v["q"].as_str().unwrap().to_string()] };
                    for (i, qs1) in qs.iter().enumerate() {
                        let t = Instant::now();
                        let mut q = Query::parse(qs1, &home).unwrap();
                        q.limit = limit;
                        let hits = s.search(&q);
                        paths.clear();
                        for h in &hits {
                            live.base.path(h.idx as usize, &mut p);
                            // fsearch paths are relative to the scanned root, with a leading '/'.
                            paths.push(String::from_utf8_lossy(&p[1..]).into_owned());
                        }
                        let us = t.elapsed().as_secs_f64() * 1e6;
                        lat.entry(class.clone()).or_default().push(us);
                        if pass == 0 && i + 1 == qs.len() && let Some(e) = expect {
                            let h = hit.entry(class.clone()).or_default();
                            h.0 += 1;
                            h.1 += (paths.first().map(String::as_str) == Some(e)) as usize;
                            h.2 += paths.iter().take(10).any(|x| x == e) as usize;
                        }
                    }
                }
            }
            let mut classes = serde_json::Map::new();
            let mut all: Vec<f64> = Vec::new();
            for (c, mut v) in lat {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                all.extend(&v);
                let h = hit.get(&c).copied();
                classes.insert(c, json!({"n": v.len(), "p50_us": pct(&v, 50.0), "p95_us": pct(&v, 95.0), "p99_us": pct(&v, 99.0), "max_us": v.last(),
                    "targets": h.map(|h| h.0), "hit1": h.map(|h| h.1 as f64 / h.0.max(1) as f64), "hit10": h.map(|h| h.2 as f64 / h.0.max(1) as f64)}));
            }
            all.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let out = json!({"engine": "fsearch", "index_load_ms": load_ms, "index_bytes": bytes, "peak_rss_bytes": peak_rss(), "repeat": repeat, "limit": limit,
                "overall": {"n": all.len(), "p50_us": pct(&all, 50.0), "p95_us": pct(&all, 95.0), "p99_us": pct(&all, 99.0)}, "classes": classes});
            std::fs::write(arg(&args, "--out").expect("--out"), serde_json::to_string_pretty(&out).unwrap()).unwrap();
        }
        _ => {
            eprintln!("usage: fsearch-baseline build|query ...");
            std::process::exit(2);
        }
    }
}
