//! Name matching and scoring.
//!
//! Every positive token is scored against a case-folded name. Scores fall in
//! disjoint *bands* so that ranking is predictable and so that the
//! short-query early exit in `search.rs` can prove it returns exactly the
//! same top-k as a full scan:
//!
//! | band                      | score            |
//! |---------------------------|------------------|
//! | exact name                | 1000             |
//! | exact stem (`name.ext`)   | 900 − ≤10        |
//! | prefix                    | 800 − ≤40        |
//! | substring at word start   | 700 − ≤40        |
//! | substring                 | 600 − ≤60        |
//! | fuzzy subsequence         | 60 ..= 560       |
//! | typo (OSA distance 1 / 2) | ≤ 480 / ≤ 360    |
//!
//! Typo tolerance uses a bit-parallel optimal-string-alignment (Damerau)
//! distance (Hyyrö 2003): a token of 4–7 bytes forgives one edit, 8+ bytes
//! two, *anywhere* in the word – including the first character and adjacent
//! swaps, which fsearch does not forgive.

/// Lowercase ASCII; other bytes unchanged (keeps byte lengths identical).
#[inline(always)]
pub fn fold(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

pub fn fold_all(s: &[u8]) -> Vec<u8> {
    s.iter().map(|&b| fold(b)).collect()
}

/// Character-class bit for a folded byte: a–z → 0..26, 0–9 → 26..36,
/// `.` 36, `-` 37, `_` 38, space 39, other ASCII 40, non-ASCII 41.
#[inline(always)]
pub fn class_bit(b: u8) -> u64 {
    let i = match b {
        b'a'..=b'z' => b - b'a',
        b'0'..=b'9' => 26 + b - b'0',
        b'.' => 36,
        b'-' => 37,
        b'_' => 38,
        b' ' => 39,
        0x80..=0xff => 41,
        _ => 40,
    };
    1u64 << i
}

pub fn class_mask(folded: &[u8]) -> u64 {
    folded.iter().fold(0, |m, &b| m | class_bit(b))
}

/// Edits forgiven for a token of `len` bytes.
pub fn typo_budget(len: usize) -> u8 {
    match len {
        0..=3 => 0,
        4..=7 => 1,
        8..=64 => 2,
        _ => 0,
    }
}

/// Branchless prefilter: can a name with class mask `m` possibly match a
/// token with mask `need` and edit budget `k`? Each edit removes at most one
/// of the token's classes from the required set.
#[inline(always)]
pub fn may_match(need: u64, k: u8, m: u64) -> bool {
    (need & !m).count_ones() <= k as u32
}

pub const EXACT: i32 = 1000;
pub const STEM: i32 = 900;
pub const PREFIX: i32 = 800;
pub const WORD: i32 = 700;
pub const SUBSTR: i32 = 600;
pub const FUZZY_MAX: i32 = 560;
pub const TYPO1: i32 = 450;
pub const TYPO2: i32 = 330;
/// Highest score any non-prefix match (word/substring/fuzzy/typo) can get.
pub const MAX_NON_PREFIX: i32 = WORD - 1;

#[inline]
fn len_pen(extra: usize, cap: i32) -> i32 {
    (extra as i32).min(cap)
}

#[inline(always)]
fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b >= 0x80
}

/// Word start in the *original-case* name: after a separator, a
/// lower→upper (camelCase) or letter↔digit transition.
#[inline]
fn boundary(orig: &[u8], p: usize) -> bool {
    if p == 0 {
        return true;
    }
    let (a, b) = (orig[p - 1], orig[p]);
    !is_alnum(a) || (a.is_ascii_lowercase() && b.is_ascii_uppercase()) || (a.is_ascii_alphabetic() != b.is_ascii_alphabetic() && is_alnum(b))
}

/// Precomputed per-token matcher state.
#[derive(Clone, Debug)]
pub struct Pattern {
    pub text: Vec<u8>,
    pub mask: u64,
    pub typos: u8,
    peq: Option<Box<[u64; 256]>>,
}

impl Pattern {
    pub fn new(folded: &[u8], allow_typos: bool) -> Pattern {
        let typos = if allow_typos { typo_budget(folded.len()) } else { 0 };
        let peq = (typos > 0).then(|| {
            let mut t = Box::new([0u64; 256]);
            for (i, &c) in folded.iter().enumerate() {
                t[c as usize] |= 1 << i;
            }
            t
        });
        Pattern { text: folded.to_vec(), mask: class_mask(folded), typos, peq }
    }

    /// Full fuzzy score (bands above) or `None`.
    pub fn score(&self, name: &[u8], orig: &[u8]) -> Option<i32> {
        let t = &self.text[..];
        let (m, n) = (t.len(), name.len());
        if m == 0 {
            return Some(0);
        }
        if m <= n {
            if name == t {
                return Some(EXACT);
            }
            if name.starts_with(t) {
                if name[m] == b'.' && !name[m + 1..].contains(&b'.') {
                    return Some(STEM - len_pen(n - m - 1, 10));
                }
                return Some(PREFIX - len_pen(n - m, 40));
            }
            if let Some(s) = self.substr(name, orig) {
                return Some(s);
            }
        }
        let fz = fuzzy(t, name, orig);
        if self.typos > 0 && fz.is_none_or(|s| s < TYPO1) {
            if let Some(ts) = self.typo(name) {
                return Some(fz.map_or(ts, |f| f.max(ts)));
            }
        }
        fz
    }

    fn substr(&self, name: &[u8], orig: &[u8]) -> Option<i32> {
        let t = &self.text[..];
        let extra = name.len() - t.len();
        let mut best: Option<i32> = None;
        for p in memchr::memmem::find_iter(name, t) {
            let s = if boundary(orig, p) { WORD - len_pen(extra, 40) } else { SUBSTR - len_pen(extra, 40) - (p as i32).min(20) };
            best = Some(best.map_or(s, |b: i32| b.max(s)));
            if s >= WORD - 40 {
                break;
            }
        }
        best
    }

    /// Typo score: OSA distance of the token to the best substring of the
    /// name, within budget.
    fn typo(&self, name: &[u8]) -> Option<i32> {
        let peq = self.peq.as_deref()?;
        let m = self.text.len();
        let d = osa_distance(peq, m, name, false);
        if d == 0 || d > self.typos as u32 {
            return None;
        }
        let anchored = osa_distance(peq, m, name, true) == d;
        let base = if d == 1 { TYPO1 } else { TYPO2 };
        let extra = name.len().saturating_sub(m);
        Some(base + if anchored { 30 } else { 0 } - len_pen(extra, 40))
    }
}

/// Greedy fuzzy subsequence (fzf-v1 style: forward scan, then a backward
/// scan from the end to tighten the window), scored by gaps and word starts.
pub fn fuzzy(t: &[u8], name: &[u8], orig: &[u8]) -> Option<i32> {
    let m = t.len();
    if m == 0 || m > name.len() {
        return None;
    }
    // Forward: find the end of the earliest complete match.
    let mut j = 0usize;
    for &c in t {
        j += memchr::memchr(c, &name[j..])? + 1;
    }
    let end = j - 1;
    // Backward from `end`: latest start that still contains the token.
    let mut pos = [0u16; 64];
    let track = m <= 64;
    let mut k = end + 1;
    for i in (0..m).rev() {
        k = memchr::memrchr(t[i], &name[..k])?;
        if track {
            pos[i] = k as u16;
        }
    }
    let start = k;
    let mut score = FUZZY_MAX;
    if !track {
        let span = (end - start + 1 - m) as i32;
        return Some((score - span.min(400)).max(60));
    }
    if !boundary(orig, start) {
        score -= 15;
    }
    score -= (start as i32).min(15);
    for i in 1..m {
        let gap = pos[i] as i32 - pos[i - 1] as i32 - 1;
        if gap > 0 {
            score -= 12 + gap.min(20);
            if boundary(orig, pos[i] as usize) {
                score += 8;
            }
        }
    }
    score -= ((name.len() - m) as i32 / 2).min(30);
    Some(score.clamp(60, FUZZY_MAX))
}

/// Bit-parallel optimal string alignment (restricted Damerau-Levenshtein)
/// distance between pattern (≤ 64 bytes, given as its `peq` table and
/// length) and the best substring of `text` (`anchored = false`) or the best
/// prefix of `text` (`anchored = true`). Myers 1999 / Hyyrö 2003.
pub fn osa_distance(peq: &[u64; 256], m: usize, text: &[u8], anchored: bool) -> u32 {
    debug_assert!(m >= 1 && m <= 64);
    let full = if m == 64 { u64::MAX } else { (1u64 << m) - 1 };
    let high = 1u64 << (m - 1);
    let mut vp = full;
    let mut vn = 0u64;
    let mut d0_prev = 0u64;
    let mut pm_prev = 0u64;
    let mut score = m as u32;
    let mut best = score;
    let carry = anchored as u64;
    for &c in text {
        let pm = peq[c as usize];
        let tr = (((!d0_prev) & pm) << 1) & pm_prev;
        let d0 = ((((pm & vp).wrapping_add(vp)) ^ vp) | pm | vn | tr) & full;
        let hp = (vn | !(d0 | vp)) & full;
        let hn = d0 & vp;
        if hp & high != 0 {
            score += 1;
        } else if hn & high != 0 {
            score -= 1;
        }
        let hps = (hp << 1) | carry;
        let hns = hn << 1;
        vp = (hns | !(d0 | hps)) & full;
        vn = hps & d0;
        d0_prev = d0;
        pm_prev = pm;
        best = best.min(score);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference OSA distance (pattern vs. best substring/prefix of text).
    fn osa_ref(p: &[u8], t: &[u8], anchored: bool) -> u32 {
        let (m, n) = (p.len(), t.len());
        let mut d = vec![vec![0u32; n + 1]; m + 1];
        for i in 0..=m {
            d[i][0] = i as u32;
        }
        for j in 0..=n {
            d[0][j] = if anchored { j as u32 } else { 0 };
        }
        for i in 1..=m {
            for j in 1..=n {
                let cost = (p[i - 1] != t[j - 1]) as u32;
                let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && p[i - 1] == t[j - 2] && p[i - 2] == t[j - 1] {
                    v = v.min(d[i - 2][j - 2] + 1);
                }
                d[i][j] = v;
            }
        }
        (0..=n).map(|j| d[m][j]).min().unwrap()
    }

    fn peq(p: &[u8]) -> Box<[u64; 256]> {
        let mut t = Box::new([0u64; 256]);
        for (i, &c) in p.iter().enumerate() {
            t[c as usize] |= 1 << i;
        }
        t
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
        fn word(&mut self, max: usize) -> Vec<u8> {
            let len = 1 + (self.next() as usize % max);
            (0..len).map(|_| b"abcde"[(self.next() % 5) as usize]).collect()
        }
    }

    #[test]
    fn osa_matches_reference() {
        let mut r = Rng(7);
        for _ in 0..20000 {
            let p = r.word(12);
            let t = r.word(20);
            let pq = peq(&p);
            for anchored in [false, true] {
                assert_eq!(osa_distance(&pq, p.len(), &t, anchored), osa_ref(&p, &t, anchored), "p={:?} t={:?} anchored={anchored}", String::from_utf8_lossy(&p), String::from_utf8_lossy(&t));
            }
        }
    }

    #[test]
    fn osa_long_pattern() {
        let p: Vec<u8> = (0..64).map(|i| b'a' + (i % 26) as u8).collect();
        let mut t = p.clone();
        t.swap(10, 11);
        assert_eq!(osa_distance(&peq(&p), 64, &t, false), 1);
        assert_eq!(osa_distance(&peq(&p), 64, &t, true), 1);
    }

    fn s(tok: &str, name: &str) -> Option<i32> {
        Pattern::new(&fold_all(tok.as_bytes()), true).score(&fold_all(name.as_bytes()), name.as_bytes())
    }

    #[test]
    fn bands_order() {
        let exact = s("report", "report").unwrap();
        let stem = s("report", "report.pdf").unwrap();
        let prefix = s("report", "reports-2024").unwrap();
        let word = s("report", "q3-report-final").unwrap();
        let sub = s("port", "airportcodes").unwrap();
        let fz = s("rpt", "report").unwrap();
        assert_eq!(exact, EXACT);
        assert!(exact > stem && stem > prefix && prefix > word, "{exact} {stem} {prefix} {word}");
        assert!(word > SUBSTR - 100 && sub < WORD && sub > fz, "{sub} {fz}");
        assert!(fz <= FUZZY_MAX);
    }

    #[test]
    fn camel_case_is_word_start() {
        assert!(s("service", "UserService.swift").unwrap() >= WORD - 40);
    }

    #[test]
    fn typos_anywhere() {
        // swap, drop, insert, substitute – including the first character.
        for q in ["flashfnid", "flashfid", "flashfiind", "flashfund", "lfashfind", "glashfind"] {
            let sc = s(q, "flashfind.rs");
            assert!(sc.is_some(), "{q}");
        }
        // Short tokens get no typos.
        assert_eq!(s("abc", "xyz"), None);
        assert!(s("tset", "test.py").is_some());
        // Two edits only for 8+ bytes.
        assert!(s("invocie_drfat", "invoice_draft.pdf").is_some());
        assert_eq!(s("tsxt", "tabcdef"), None);
    }

    #[test]
    fn typo_ranks_below_clean() {
        let clean = s("budget", "budget_2024.xlsx").unwrap();
        let typo = s("budgte", "budget_2024.xlsx").unwrap();
        assert!(clean > typo);
        assert!(typo >= TYPO1 - 40);
    }

    #[test]
    fn fuzzy_prefers_tight() {
        let tight = s("mnts", "meeting_notes.md");
        let tighter = s("notes", "my_notes.md").unwrap();
        assert!(tight.is_some());
        assert!(tighter > tight.unwrap());
    }

    #[test]
    fn prefilter_respects_budget() {
        let p = Pattern::new(b"flashfind", true);
        let name = b"lashfind";
        assert!(may_match(p.mask, p.typos, class_mask(name)));
        assert!(!may_match(p.mask, 0, class_mask(b"xyz")));
    }
}
