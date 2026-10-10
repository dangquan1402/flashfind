//! Where does one-shot code search time go?
//!   cargo run --release -p flashfind-code --example prof -- ROOT QUERIES [CACHE_DIR]
use flashfind_code::{CodeIndex, CodeQuery, Config};
use flashfind_core::{CancelToken, Query, Session};
use std::collections::BTreeMap;
use std::time::Instant;

fn p50(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let root = std::path::PathBuf::from(&a[1]);
    let cache = a.get(3).map(std::path::PathBuf::from);
    let t = Instant::now();
    let ci = match cache.as_ref().and_then(|c| CodeIndex::load(c).ok()) {
        Some(ci) => ci,
        None => {
            let ci = CodeIndex::build(&Config { roots: vec![root.clone()], gitignore: std::env::var_os("NOGI").is_none(), ..Config::default() });
            if let Some(c) = &cache {
                ci.save(c).unwrap();
            }
            ci
        }
    };
    eprintln!("index ready in {:?}: n={} u={} info={:?}", t.elapsed(), ci.idx.n, ci.idx.u, ci.info);
    let qs: Vec<(String, String)> = std::fs::read_to_string(&a[2]).unwrap().lines().map(|l| {
        let mut it = l.split('\t');
        (it.next().unwrap().to_string(), it.next().unwrap().to_string())
    }).filter(|(c, _)| c == "exact" || c == "typo").collect();
    if let Ok(out) = std::env::var("DUMP") {
        let all: Vec<String> = std::fs::read_to_string(&a[2]).unwrap().lines().map(|l| l.split('\t').nth(1).unwrap().to_string()).collect();
        let cancel = CancelToken::new();
        let mut o = String::new();
        for q in &all {
            let mut cq = CodeQuery::parse(q);
            cq.limit = 50;
            let mut s = Session::new(ci.idx.clone());
            match ci.search(&mut s, &cq, &cancel) {
                Ok(r) => {
                    o += &format!("{q}\t{}", r.total);
                    for h in &r.hits {
                        o += &format!("\t{}:{}", h.rel, h.score);
                    }
                    o += "\n";
                }
                Err(e) => o += &format!("{q}\tERR {e}\n"),
            }
        }
        std::fs::write(out, o).unwrap();
        return;
    }
    if std::env::var("STAGES").is_ok() {
        stages(&ci, &qs);
        stages2(&ci, &qs);
        return;
    }
    let cancel = CancelToken::new();
    let mut m: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut strat: BTreeMap<String, usize> = BTreeMap::new();
    for _ in 0..2 {
        for (class, q) in &qs {
            let cq = CodeQuery::parse(q);
            let mut cq50 = cq.clone();
            cq50.limit = 50;
            // full code path
            let mut s = Session::new(ci.idx.clone());
            let t = Instant::now();
            let _ = ci.search(&mut s, &cq50, &cancel);
            m.entry(format!("{class} code total")).or_default().push(t.elapsed().as_secs_f64() * 1e6);
            // mask only
            let t = Instant::now();
            let mask = ci.mask(&cq50, None).unwrap();
            m.entry(format!("{class} mask")).or_default().push(t.elapsed().as_secs_f64() * 1e6);
            for (lim, masked) in [(150usize, true), (50, true), (50, false)] {
                let mut qq = Query::parse(q).unwrap();
                qq.limit = lim;
                let mut s = Session::new(ci.idx.clone());
                let t = Instant::now();
                let r = s.search_masked(&qq, &cancel, masked.then_some(&mask[..])).unwrap();
                let k = format!("{class} core lim{lim}{}", if masked { " masked" } else { "" });
                m.entry(k.clone()).or_default().push(t.elapsed().as_secs_f64() * 1e6);
                m.entry(format!("{k} name_us")).or_default().push(r.name_us as f64);
                m.entry(format!("{k} entry_us")).or_default().push(r.entry_us as f64);
                *strat.entry(format!("{k} {:?}", r.strategy)).or_default() += 1;
            }
        }
    }
    for (k, mut v) in m {
        println!("{k:<40} p50 {:>8.0} µs", p50(&mut v));
    }
    for (k, n) in strat {
        println!("{k:<50} {n}");
    }
}

#[allow(dead_code)]
pub fn stages(ci: &CodeIndex, qs: &[(String, String)]) {
    use flashfind_core::matcher::may_match;
    let idx = &ci.idx;
    let masks = idx.name_mask();
    let (mut pass_mask, mut pass_clean_mask, mut subseq, mut cand, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut by_k = [0u64; 3];
    let t = Instant::now();
    for (_, q) in qs.iter().take(200) {
        let qq = Query::parse(q).unwrap();
        let Some(tok) = qq.tokens.first() else { continue };
        let p = &tok.pat;
        by_k[p.typos as usize] += 1;
        for id in 0..idx.u as u32 {
            n += 1;
            let m = masks[id as usize];
            if may_match(p.mask, 0, m) {
                pass_clean_mask += 1;
                if flashfind_core::matcher::is_subsequence(&p.text, idx.lower(id)) {
                    subseq += 1;
                }
            }
            if may_match(p.mask, p.typos, m) {
                pass_mask += 1;
                if p.typo_candidate(idx.sig(id)) {
                    cand += 1;
                }
            }
        }
    }
    let q = 200f64;
    println!("stages over 200 queries ({:?}), per query: names {:.0}  mask(k) {:.0}  mask(0) {:.0}  subseq {:.1}  typo-cand {:.0}   typos-by-budget {:?}",
        t.elapsed(), n as f64 / q, pass_mask as f64 / q, pass_clean_mask as f64 / q, subseq as f64 / q, cand as f64 / q, by_k);
}

#[allow(dead_code)]
pub fn stages2(ci: &CodeIndex, qs: &[(String, String)]) {
    use flashfind_core::matcher::may_match;
    let idx = &ci.idx;
    let masks = idx.name_mask();
    let mut cand = vec![0u32; idx.u];
    let (mut to, mut tt, mut npass) = (0f64, 0f64, 0u64);
    let (mut tg, mut ts, mut tc, mut nsurv, mut nscore, mut ncand) = (0f64, 0f64, 0f64, 0u64, 0u64, 0u64);
    let qn = qs.len().min(300);
    for (_, q) in qs.iter().take(qn) {
        let qq = Query::parse(q).unwrap();
        let Some(tok) = qq.tokens.first() else { continue };
        let p = &tok.pat;
        let t = Instant::now();
        let mut n = 0;
        for (i, &m) in masks.iter().enumerate() {
            cand[n] = i as u32;
            n += may_match(p.mask, p.typos, m) as usize;
        }
        tg += t.elapsed().as_secs_f64();
        nsurv += n as u64;
        let t = Instant::now();
        let mut c2 = 0;
        for &id in &cand[..n] {
            c2 += (may_match(p.mask, 0, masks[id as usize]) || p.typo_candidate(idx.sig(id))) as u64;
        }
        tc += t.elapsed().as_secs_f64();
        ncand += c2;
        let t = Instant::now();
        for &id in &cand[..n] {
            if tok.score_m(idx.lower(id), idx.orig(id), idx.sig(id), masks[id as usize], true).is_some() {
                nscore += 1;
            }
        }
        ts += t.elapsed().as_secs_f64();
        let t = Instant::now();
        let mut x = 0usize;
        for &id in &cand[..n] {
            x += idx.orig(id).len();
        }
        to += t.elapsed().as_secs_f64();
        let t = Instant::now();
        for &id in &cand[..n] {
            let ok = p.typo_candidate_in(idx.lower(id), idx.sig(id));
            x += ok as usize;
            npass += ok as u64;
        }
        tt += t.elapsed().as_secs_f64();
        std::hint::black_box(x);
    }
    println!("piece-verified per query: {:.0}", npass as f64 / qn as f64);
    println!("orig fetch {:.0} µs, typo_candidate_in {:.0} µs", to / qn as f64 * 1e6, tt / qn as f64 * 1e6);
    let q = qn as f64;
    println!("single-thread per query: gather {:.0} µs ({:.0} surv) | sig-check {:.0} µs ({:.0} pass) | score {:.0} µs ({:.1} match)",
        tg / q * 1e6, nsurv as f64 / q, tc / q * 1e6, ncand as f64 / q, ts / q * 1e6, nscore as f64 / q);
}
