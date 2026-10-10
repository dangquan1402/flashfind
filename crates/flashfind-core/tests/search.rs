use flashfind_core::query::Clock;
use flashfind_core::search::search_reference;
use flashfind_core::walk::{KIND_DIR, KIND_FILE, Listing, NONE, ScanOptions};
use flashfind_core::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const CLOCK: Clock = Clock {
    now: 1_790_000_000,
    utc_offset: 0,
};

fn q(s: &str) -> Query {
    Query::parse_with(s, CLOCK, Some("/home/me")).unwrap()
}

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("flashfind-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn touch(root: &Path, rel: &str, size: u64, mtime: Option<i64>) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let f = std::fs::File::create(&p).unwrap();
    f.set_len(size).unwrap();
    if let Some(t) = mtime {
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t as u64)).unwrap();
    }
}

fn disk_index(tag: &str, files: &[(&str, u64, Option<i64>)]) -> (PathBuf, Index) {
    let root = tmpdir(tag);
    for (rel, size, mt) in files {
        touch(&root, rel, *size, *mt);
    }
    let idx = build_from_disk(&root, &ScanOptions { threads: 4, skip: vec![] });
    (root, idx)
}

fn top(idx: &Index, s: &str) -> Vec<String> {
    let r = search(idx, &q(s), &CancelToken::new()).unwrap();
    r.hits.iter().map(|h| idx.rel_path(h.entry)).collect()
}

#[test]
fn finds_and_ranks_bands() {
    let (_root, idx) = disk_index(
        "bands",
        &[
            ("docs/report", 1, None),
            ("docs/report.pdf", 1, None),
            ("docs/reports-2024.xlsx", 1, None),
            ("docs/q3-report-final.doc", 1, None),
            ("docs/airportcodes.txt", 1, None),
            ("misc/rxexpxoxrxt.bin", 1, None),
        ],
    );
    let r = top(&idx, "report");
    assert_eq!(r[0], "docs/report");
    assert_eq!(r[1], "docs/report.pdf");
    assert_eq!(r[2], "docs/reports-2024.xlsx");
    assert_eq!(r[3], "docs/q3-report-final.doc");
    assert!(r.contains(&"misc/rxexpxoxrxt.bin".to_string()), "fuzzy subsequence match");
    // "rport" is one edit from "report" (typo match); a subsequence scattered
    // over six gaps ranks below it.
    assert_eq!(&r[4..], ["docs/airportcodes.txt", "misc/rxexpxoxrxt.bin"]);
}

#[test]
fn location_prior() {
    let (_root, idx) = disk_index(
        "prior",
        &[
            ("a/b/c/d/e/f/g/budget.xlsx", 1, None),
            ("work/budget.xlsx", 1, None),
            ("proj/node_modules/x/budget.xlsx", 1, None),
        ],
    );
    let r = top(&idx, "budget.xlsx");
    assert_eq!(r[0], "work/budget.xlsx");
    assert_eq!(r[2], "proj/node_modules/x/budget.xlsx");
}

#[test]
fn typo_tolerance_beats_fsearch_rules() {
    let (_root, idx) = disk_index("typo", &[("src/flashfind.rs", 1, None), ("src/other.rs", 1, None)]);
    for t in ["flashfnid", "lfashfind", "glashfind", "flasfind", "flashffind"] {
        assert_eq!(top(&idx, t).first().map(String::as_str), Some("src/flashfind.rs"), "{t}");
    }
}

#[test]
fn multi_token_matches_ancestors() {
    let (_root, idx) = disk_index(
        "multi",
        &[
            ("projects/alpha/notes.md", 1, None),
            ("personal/notes.md", 1, None),
            ("projects/alpha_notes.md", 1, None),
        ],
    );
    let r = top(&idx, "alpha notes");
    assert_eq!(r.len(), 2, "{r:?}");
    // Folder + exact name beats both words squeezed into one longer name.
    assert_eq!(r[0], "projects/alpha/notes.md");
    assert!(r.contains(&"projects/alpha_notes.md".to_string()));
    // Exact stem under a matching folder beats a word match in a longer name.
    assert_eq!(top(&idx, "projects/notes"), vec!["projects/alpha/notes.md", "projects/alpha_notes.md"]);
}

#[test]
fn filters() {
    let day = 86_400;
    let (root, idx) = disk_index(
        "filters",
        &[
            ("a/photo.jpg", 2_000_000, Some(CLOCK.now - 2 * day)),
            ("a/photo.png", 10, Some(CLOCK.now - 400 * day)),
            ("b/photo.pdf", 6_000_000, Some(CLOCK.now - 10 * day)),
            ("b/photos/readme.txt", 1, None),
        ],
    );
    assert_eq!(top(&idx, "photo ext:pdf"), vec!["b/photo.pdf"]);
    assert_eq!(top(&idx, "photo type:image").len(), 2);
    assert_eq!(top(&idx, "photo type:dir"), vec!["b/photos"]);
    assert_eq!(top(&idx, "photo size:>5mb"), vec!["b/photo.pdf"]);
    assert_eq!(top(&idx, "photo size:1mb..3mb"), vec!["a/photo.jpg"]);
    assert_eq!(top(&idx, "photo mtime:3d type:file"), vec!["a/photo.jpg"]);
    assert_eq!(top(&idx, "photo mtime:<30d type:file").len(), 2);
    assert_eq!(top(&idx, "photo mtime:>1y"), vec!["a/photo.png"]);
    assert_eq!(top(&idx, "photo mtime:>140y").len(), 0);
    let scope = format!("photo in:{}/b", std::fs::canonicalize(&root).unwrap().display());
    assert_eq!(top(&idx, &scope).len(), 2);
    assert_eq!(top(&idx, "photo !pdf !png !photos"), vec!["a/photo.jpg"]);
    // Filter-only query: entry scan.
    let r = search(&idx, &q("size:>5mb"), &CancelToken::new()).unwrap();
    assert_eq!(r.strategy, Strategy::EntryScan);
    assert_eq!(r.hits.len(), 1);
}

#[test]
fn deep_paths_are_not_truncated() {
    let root = tmpdir("deep");
    let mut rel = PathBuf::new();
    for i in 0..300 {
        rel.push(format!("d{}", i % 10));
    }
    // Create level by level (PATH_MAX would stop a single create_dir_all).
    let mut cur = root.clone();
    std::env::set_current_dir(&root).unwrap();
    for c in rel.components() {
        std::fs::create_dir(c.as_os_str()).unwrap();
        std::env::set_current_dir(c.as_os_str()).unwrap();
        cur.push(c);
    }
    std::fs::File::create("deepest_target.txt").unwrap();
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    let idx = build_from_disk(&root, &ScanOptions::default());
    let r = search(&idx, &q("deepest_target"), &CancelToken::new()).unwrap();
    assert_eq!(r.hits.len(), 1, "crawler must reach depth 300");
    let p = idx.path_string(r.hits[0].entry);
    assert_eq!(idx.depth(r.hits[0].entry), 301);
    assert!(p.ends_with("/d9/deepest_target.txt"));
    assert_eq!(
        p.matches('/').count() - std::fs::canonicalize(&root).unwrap().to_string_lossy().matches('/').count(),
        301
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn save_load_roundtrip() {
    let (root, idx) = disk_index("save", &[("x/alpha.txt", 1, None), ("y/beta.txt", 1, None)]);
    let f = root.join("idx.ffx");
    idx.save(&f).unwrap();
    let back = Index::load(&f).unwrap();
    assert_eq!((back.n, back.u, back.d), (idx.n, idx.u, idx.d));
    assert_eq!(top(&back, "beta"), top(&idx, "beta"));
    assert_eq!(back.lookup(back.root()), Some(0));
    let e = back.lookup(format!("{}/x/alpha.txt", String::from_utf8_lossy(back.root())).as_bytes()).unwrap();
    assert_eq!(back.rel_path(e), "x/alpha.txt");
    std::fs::write(root.join("bad.ffx"), b"nope").unwrap();
    assert!(Index::load(&root.join("bad.ffx")).is_err());
}

#[test]
fn cancellation() {
    let idx = synthetic(50_000, 3);
    let c = CancelToken::new();
    c.cancel();
    assert_eq!(search(&idx, &q("abc"), &c).unwrap_err(), Cancelled);
}

// ---------- synthetic in-memory index for equivalence checks ----------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn synthetic(n: usize, seed: u64) -> Index {
    let mut r = Rng(seed);
    let syll = ["ab", "ca", "de", "fo", "ri", "tu", "na", "le", "mo", "pi", "s", "x", "_", "-", ".", "q"];
    let word = |r: &mut Rng| -> Vec<u8> {
        let k = 1 + r.below(5);
        let mut w = Vec::new();
        for _ in 0..k {
            w.extend_from_slice(syll[r.below(syll.len())].as_bytes());
        }
        if r.below(3) == 0 {
            w.extend_from_slice([&b".txt"[..], b".rs", b".md"][r.below(3)]);
        }
        w
    };
    let mut ls: Vec<Listing> = vec![Listing { id: 0, ..Default::default() }];
    let mut count = 0;
    while count < n {
        let li = r.below(ls.len());
        let name = word(&mut r);
        let is_dir = r.below(6) == 0;
        let kind = if is_dir { KIND_DIR } else { KIND_FILE };
        let size = r.next() % 10_000_000;
        let mtime = 1_700_000_000 + (r.next() % 90_000_000) as i64;
        let new_id = ls.len() as u32;
        let l = &mut ls[li];
        let off = l.names.len() as u32;
        l.names.extend_from_slice(&name);
        l.ents.push(walk::RawEnt {
            name_off: off,
            name_len: name.len() as u16,
            kind,
            size,
            mtime,
            child: if is_dir { new_id } else { NONE },
        });
        if is_dir {
            ls.push(Listing {
                id: new_id,
                ..Default::default()
            });
        }
        count += 1;
    }
    Index::build(ls, Path::new("/syn"), 0)
}

fn same(a: &SearchResult, b: &SearchResult, what: &str) {
    assert_eq!(a.hits, b.hits, "{what}");
}

#[test]
fn shortcuts_equal_reference() {
    let idx = synthetic(60_000, 11);
    let c = CancelToken::new();
    let mut prefix_used = 0;
    for s in [
        "abcad",
        "rituna",
        "fole.",
        "abab",
        "a",
        "ab",
        "q",
        "x.",
        "_",
        "s",
        "ca",
        "fo",
        "^ab",
        "^rit",
        "=abca",
        "pi limit:5",
        "a limit:3",
        "z",
        "ab ext:rs",
        "a type:dir",
        "a size:>5mb",
        "ab !ca",
    ] {
        let quick = search(&idx, &q(s), &c).unwrap();
        let reference = search_reference(&idx, &q(s), &c).unwrap();
        same(&quick, &reference, s);
        if quick.strategy == Strategy::PrefixRange {
            prefix_used += 1;
            if quick.total_exact {
                assert_eq!(quick.total, reference.total, "{s}");
            } else {
                assert!(quick.total <= reference.total, "{s}");
            }
        }
    }
    assert!(prefix_used >= 5, "prefix shortcut should trigger on most short queries ({prefix_used})");
}

#[test]
fn narrowing_equals_reference() {
    let idx = Arc::new(synthetic(80_000, 5));
    let c = CancelToken::new();
    let mut sess = Session::new(idx.clone());
    let mut narrowed = 0;
    for target in ["abcadefo", "rituna_le", "mopi.txt", "deca rituna", "fole ext:md", "tunale !ca"] {
        // Type it one char at a time, then delete back.
        let chars: Vec<char> = target.chars().collect();
        let mut seq: Vec<String> = (1..=chars.len()).map(|i| chars[..i].iter().collect()).collect();
        seq.extend((1..chars.len()).rev().map(|i| chars[..i].iter().collect::<String>()));
        for s in seq {
            let a = sess.search(&q(&s), &c).unwrap();
            let b = search_reference(&idx, &q(&s), &c).unwrap();
            same(&a, &b, &s);
            if a.total_exact {
                assert_eq!(a.total, b.total, "{s}");
            } else {
                assert!(a.total <= b.total, "{s}");
            }
            narrowed += matches!(a.strategy, Strategy::Narrowed | Strategy::Cached) as usize;
        }
    }
    assert!(narrowed > 20, "narrowing should kick in while typing ({narrowed})");
}

#[test]
fn portable_and_native_crawlers_agree() {
    let (root, idx) = disk_index("crawl", &[("a/b/c.txt", 5, None), ("a/d.txt", 1, None), (".hidden/x", 1, None)]);
    let root = std::fs::canonicalize(root).unwrap();
    let ls = walk::scan_portable(&root, &ScanOptions::default());
    let idx2 = Index::build(ls, &root, 0);
    let mut a: Vec<String> = (1..idx.n as u32).map(|e| idx.rel_path(e)).collect();
    let mut b: Vec<String> = (1..idx2.n as u32).map(|e| idx2.rel_path(e)).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
    assert_eq!(a.len(), 6);
}
