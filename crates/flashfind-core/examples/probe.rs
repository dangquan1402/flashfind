use flashfind_core::Index;
use flashfind_core::matcher::*;
use std::time::Instant;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let idx = Index::load(a[1].as_ref()).unwrap();
    idx.prefault();
    for t in &a[2..] {
        let p = Pattern::new(&fold_all(t.as_bytes()), true);
        let masks = idx.name_mask();
        let (mut mp, mut sub, mut cand, mut typo) = (0, 0, 0, 0);
        let t0 = Instant::now();
        for id in 0..idx.u as u32 {
            if !may_match(p.mask, p.typos, masks[id as usize]) {
                continue;
            }
            mp += 1;
            let l = idx.lower(id);
            if p.text.len() <= l.len() && is_subsequence(&p.text, l) {
                sub += 1;
            }
            if p.typo_candidate(idx.sig(id)) {
                cand += 1;
                if p.score_sig(l, idx.orig(id), idx.sig(id)).is_some() {
                    typo += 1;
                }
            }
        }
        let e1 = t0.elapsed();
        let t1 = Instant::now();
        let mut n = 0;
        for id in 0..idx.u as u32 {
            if !may_match(p.mask, p.typos, masks[id as usize]) {
                continue;
            }
            if p.score_sig(idx.lower(id), idx.orig(id), idx.sig(id))
                .is_some()
            {
                n += 1;
            }
        }
        let e2 = t1.elapsed();
        let t2 = Instant::now();
        let mut k = 0;
        for id in 0..idx.u as u32 {
            if may_match(p.mask, p.typos, masks[id as usize]) {
                k += 1;
            }
        }
        let e3 = t2.elapsed();
        println!(
            "{t}: u={} maskpass={mp} subseq={sub} typo_cand={cand} typo_ok={typo} matched={n} | probe {e1:.2?} score-loop {e2:.2?} mask-only {e3:.2?} ({k})",
            idx.u
        );
    }
}
