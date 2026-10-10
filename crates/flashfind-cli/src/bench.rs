//! `flashfind bench`: replay a query set against a saved index.
//! Output schema is shared with bench/fsearch-baseline.

use crate::Args;
use crate::util::{peak_rss, percentile};
use flashfind_core::{CancelToken, Index, Query, Session};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

pub fn run(mut a: Args) {
    let file = a.opt(&["-i", "--index"]).unwrap_or_else(|| crate::die("-i is required"));
    let qfile = a.opt(&["--queries"]).unwrap_or_else(|| crate::die("--queries is required"));
    let repeat = a.num(&["--repeat"], 5usize);
    let limit = a.num(&["--limit"], 50usize);
    let out = a.opt(&["--out"]);
    let label = a.opt(&["--label"]).unwrap_or_else(|| "flashfind".into());
    let t0 = Instant::now();
    let idx = Arc::new(Index::load(file.as_ref()).unwrap_or_else(|e| crate::die(e)));
    idx.prefault();
    let load_ms = t0.elapsed().as_secs_f64() * 1e3;
    let lines: Vec<Value> = std::fs::read_to_string(&qfile)
        .unwrap_or_else(|e| crate::die(e))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut lat: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut hit1: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let cancel = CancelToken::new();
    let mut paths: Vec<String> = Vec::with_capacity(limit);
    // One session per pass models one search box receiving unrelated queries.
    for pass in 0..repeat {
        let mut sess = Session::new(idx.clone());
        for v in &lines {
            let class = v["class"].as_str().unwrap().to_string();
            let expect = v["expect"].as_str();
            let qs: Vec<String> = if class == "keys" {
                v["seq"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
            } else {
                vec![v["q"].as_str().unwrap().to_string()]
            };
            for (i, s) in qs.iter().enumerate() {
                let t = Instant::now();
                let mut q = Query::parse(s).unwrap();
                q.limit = limit;
                let r = sess.search(&q, &cancel).unwrap();
                // Materialise result paths: part of "first page ready".
                paths.clear();
                paths.extend(r.hits.iter().map(|h| idx.rel_path(h.entry)));
                let us = t.elapsed().as_secs_f64() * 1e6;
                lat.entry(class.clone()).or_default().push(us);
                let last = i + 1 == qs.len();
                if pass == 0
                    && last
                    && let Some(e) = expect
                {
                    let h = hit1.entry(class.clone()).or_default();
                    h.0 += 1;
                    h.1 += (paths.first().map(String::as_str) == Some(e)) as usize;
                    h.2 += paths.iter().take(10).any(|p| p == e) as usize;
                    if paths.first().map(String::as_str) != Some(e) && std::env::var_os("FLASHFIND_MISSES").is_some() {
                        eprintln!("MISS [{class}] {s:?}\n  want {e}");
                        for (p, h) in paths.iter().zip(&r.hits).take(3) {
                            eprintln!("  got  {} {p}", h.score);
                        }
                    }
                }
            }
        }
    }
    let v = summarize(&label, load_ms, idx.bytes() as u64, peak_rss(), repeat, limit, lat, hit1);
    let s = serde_json::to_string_pretty(&v).unwrap();
    match out {
        Some(f) => std::fs::write(f, s).unwrap_or_else(|e| crate::die(e)),
        None => println!("{s}"),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn summarize(
    label: &str,
    load_ms: f64,
    index_bytes: u64,
    rss: u64,
    repeat: usize,
    limit: usize,
    lat: BTreeMap<String, Vec<f64>>,
    hit1: BTreeMap<String, (usize, usize, usize)>,
) -> Value {
    let mut classes = serde_json::Map::new();
    let mut all: Vec<f64> = Vec::new();
    for (c, mut v) in lat {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        all.extend(&v);
        let h = hit1.get(&c).copied();
        classes.insert(
            c,
            json!({
                "n": v.len(), "p50_us": percentile(&v, 50.0), "p95_us": percentile(&v, 95.0), "p99_us": percentile(&v, 99.0), "max_us": v.last(),
                "targets": h.map(|h| h.0), "hit1": h.map(|h| h.1 as f64 / h.0.max(1) as f64), "hit10": h.map(|h| h.2 as f64 / h.0.max(1) as f64),
            }),
        );
    }
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({
        "engine": label, "index_load_ms": load_ms, "index_bytes": index_bytes, "peak_rss_bytes": rss, "repeat": repeat, "limit": limit,
        "overall": {"n": all.len(), "p50_us": percentile(&all, 50.0), "p95_us": percentile(&all, 95.0), "p99_us": percentile(&all, 99.0)},
        "classes": classes,
    })
}
