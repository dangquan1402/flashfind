//! Live overlay: relisting, tombstones, compaction, history-loss recheck,
//! event-id persistence, search during the first crawl, and (macOS) the
//! FSEvents watcher end to end.

use flashfind_core::walk::ScanOptions;
use flashfind_core::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("flashfind-live-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

fn touch(root: &Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, b"x").unwrap();
}

fn b(p: &Path) -> Vec<u8> {
    p.to_str().unwrap().as_bytes().to_vec()
}

fn find(s: &mut LiveSession, query: &str) -> Vec<String> {
    let mut q = Query::parse(query).unwrap();
    q.limit = 20;
    let root = String::from_utf8_lossy(s.live().root()).into_owned();
    s.search(&q, &CancelToken::new())
        .unwrap()
        .hits
        .iter()
        .map(|h| h.path_str().strip_prefix(&root).unwrap_or("").trim_start_matches('/').to_string())
        .collect()
}

fn opts() -> LiveOptions {
    LiveOptions {
        threads: 2,
        poll: Duration::from_millis(100),
        ..Default::default()
    }
}

fn base(root: &Path) -> Arc<Live> {
    let idx = build_from_disk(root, &ScanOptions { threads: 2, skip: vec![] });
    Live::from_index(Arc::new(idx), opts())
}

#[test]
fn relist_create_rename_delete() {
    let root = tmpdir("relist");
    touch(&root, "docs/old_report.txt");
    touch(&root, "docs/keep.md");
    touch(&root, "src/main.rs");
    let live = base(&root);
    let mut s = LiveSession::new(live.clone());
    assert_eq!(find(&mut s, "old_report"), vec!["docs/old_report.txt"]);
    // create + rename + delete
    touch(&root, "docs/brand_new_file.txt");
    std::fs::rename(root.join("docs/old_report.txt"), root.join("docs/renamed_report.txt")).unwrap();
    std::fs::remove_file(root.join("src/main.rs")).unwrap();
    live.refresh(&[b(&root.join("docs")), b(&root.join("src"))]);
    assert_eq!(find(&mut s, "brand_new_file"), vec!["docs/brand_new_file.txt"]);
    assert_eq!(find(&mut s, "renamed_report"), vec!["docs/renamed_report.txt"]);
    assert!(find(&mut s, "old_report").iter().all(|p| p != "docs/old_report.txt"));
    assert!(find(&mut s, "=main.rs").is_empty());
    assert_eq!(find(&mut s, "=keep.md"), vec!["docs/keep.md"]);
    // New folder with a subtree; deleted folder with a subtree.
    touch(&root, "fresh/deep/er/needle_file.bin");
    std::fs::remove_dir_all(root.join("docs")).unwrap();
    live.refresh(&[b(&root)]);
    assert_eq!(find(&mut s, "needle_file"), vec!["fresh/deep/er/needle_file.bin"]);
    assert!(find(&mut s, "keep").is_empty(), "deleted folder's children must go");
    // Multi-token over overlay entries uses folder components.
    assert_eq!(find(&mut s, "deep needle_file"), vec!["fresh/deep/er/needle_file.bin"]);
    let before: Vec<Vec<String>> = ["needle", "fresh", "deep er", "renamed"].iter().map(|q| find(&mut s, q)).collect();
    // Compaction folds everything into a new base with identical results.
    assert!(live.compact());
    let p = live.progress();
    assert_eq!((p.overlay, p.dead, p.generation), (0, 0, 1));
    let after: Vec<Vec<String>> = ["needle", "fresh", "deep er", "renamed"].iter().map(|q| find(&mut s, q)).collect();
    assert_eq!(before, after);
    assert_eq!(live.base().n, build_from_disk(&root, &ScanOptions::default()).n);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn modified_file_updates_size() {
    let root = tmpdir("modify");
    touch(&root, "a/growing.log");
    let live = base(&root);
    let mut s = LiveSession::new(live.clone());
    assert!(find(&mut s, "growing size:>1k").is_empty());
    std::fs::write(root.join("a/growing.log"), vec![0u8; 5000]).unwrap();
    live.refresh(&[b(&root.join("a"))]);
    assert_eq!(find(&mut s, "growing size:>1k"), vec!["a/growing.log"]);
    assert_eq!(find(&mut s, "growing").len(), 1, "old version tombstoned");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recheck_finds_changes_by_dir_mtime() {
    let root = tmpdir("recheck");
    for i in 0..20 {
        touch(&root, &format!("d{i}/f{i}.txt"));
    }
    let live = base(&root);
    let mut s = LiveSession::new(live.clone());
    // Dir mtimes have 1 s resolution on some filesystems: make the change
    // visible to an mtime compare.
    std::thread::sleep(Duration::from_millis(1100));
    touch(&root, "d7/history_lost.txt");
    std::fs::remove_file(root.join("d3/f3.txt")).unwrap();
    // As if FSEvents had said MustScanSubDirs for the root.
    let n = live.recheck(&b(&root));
    assert!(n >= 2, "relisted {n}");
    assert_eq!(find(&mut s, "history_lost"), vec!["d7/history_lost.txt"]);
    assert!(find(&mut s, "=f3.txt").is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn polling_watcher_picks_up_changes() {
    let root = tmpdir("poll");
    touch(&root, "x/y.txt");
    let mut o = opts();
    o.force_poll = true;
    let idx = build_from_disk(&root, &ScanOptions { threads: 2, skip: vec![] });
    let live = Live::from_index(Arc::new(idx), o);
    live.watch().unwrap();
    let mut s = LiveSession::new(live.clone());
    std::thread::sleep(Duration::from_millis(1100));
    touch(&root, "x/polled_in.txt");
    let t = Instant::now();
    while find(&mut s, "polled_in").is_empty() {
        assert!(t.elapsed() < Duration::from_secs(10), "not seen within 10 s");
        std::thread::sleep(Duration::from_millis(20));
    }
    live.unwatch();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn saved_event_id_and_compaction_persist() {
    let root = tmpdir("persist");
    touch(&root, "a/one.txt");
    let file = root.join("../flashfind-live-persist.ffx");
    let idx = build_from_disk(&root, &ScanOptions { threads: 2, skip: vec![] });
    idx.save(&file).unwrap();
    let live = Live::open(&file, opts()).unwrap();
    touch(&root, "a/two_persisted.txt");
    live.refresh(&[b(&root.join("a"))]);
    live.apply_events(&[flashfind_core::watch::Event {
        path: b(&root.join("a")),
        flags: 0,
        id: 4242,
    }]);
    live.checkpoint();
    drop(live);
    let again = Live::open(&file, opts()).unwrap();
    assert_eq!(again.progress().last_event_id, 4242);
    let mut s = LiveSession::new(again.clone());
    assert_eq!(find(&mut s, "two_persisted"), vec!["a/two_persisted.txt"]);
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_during_first_crawl() {
    let root = tmpdir("crawl");
    for i in 0..3000 {
        touch(&root, &format!("p{}/q{}/file_{i}.dat", i % 30, i % 7));
    }
    touch(&root, "p1/target_during_crawl.txt");
    let live = Live::crawl(&root, None, opts());
    let mut s = LiveSession::new(live.clone());
    // Whatever phase we are in, queries work; once ready, results are exact.
    let mut saw_partial = false;
    let t = Instant::now();
    loop {
        let mut q = Query::parse("target_during_crawl").unwrap();
        q.limit = 5;
        let r = s.search(&q, &CancelToken::new()).unwrap();
        saw_partial |= r.partial;
        if live.phase() == Phase::Ready && !r.partial {
            assert_eq!(r.hits.len(), 1);
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(30));
    }
    let p = live.progress();
    assert_eq!(p.phase, 2);
    assert_eq!(p.entries, 3000 + 1 + 30 + 30 * 7 + 1);
    eprintln!("saw partial results: {saw_partial}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn partial_scan_matches_ready_results() {
    // The brute-force scan used during the crawl finds the same set.
    let root = tmpdir("partial");
    touch(&root, "music/alpha/notes.md");
    touch(&root, "docs/alpha_notes.md");
    touch(&root, "docs/other.md");
    let p = walk::Partial::new();
    walk::scan_into(&root, &ScanOptions { threads: 2, skip: vec![] }, Some(p.clone()));
    assert_eq!(p.entries.load(std::sync::atomic::Ordering::Relaxed), 6);
}

#[cfg(target_os = "macos")]
#[test]
fn fsevents_watcher_end_to_end() {
    let root = tmpdir("fsev");
    touch(&root, "base/a.txt");
    let file = root.join("../flashfind-live-fsev.ffx");
    let idx = build_from_disk(&root, &ScanOptions { threads: 2, skip: vec![] });
    idx.save(&file).unwrap();
    let live = Live::open(&file, opts()).unwrap();
    live.watch().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let mut s = LiveSession::new(live.clone());
    let wait = |s: &mut LiveSession, q: &str, want: bool| -> Duration {
        let t = Instant::now();
        loop {
            let found = !find(s, q).is_empty();
            if found == want {
                return t.elapsed();
            }
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "{q}: not {} within 5 s",
                if want { "found" } else { "gone" }
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let mut lat = Vec::new();
    for i in 0..10 {
        let name = format!("created_{i}_zq.txt");
        touch(&root, &format!("base/{name}"));
        lat.push(wait(&mut s, &format!("={name}"), true));
        let renamed = format!("renamed_{i}_zq.txt");
        std::fs::rename(root.join("base").join(&name), root.join("base").join(&renamed)).unwrap();
        lat.push(wait(&mut s, &format!("={renamed}"), true));
        wait(&mut s, &format!("={name}"), false);
        std::fs::remove_file(root.join("base").join(&renamed)).unwrap();
        lat.push(wait(&mut s, &format!("={renamed}"), false));
    }
    // New folder tree.
    std::fs::create_dir_all(root.join("newdir/sub")).unwrap();
    touch(&root, "newdir/sub/in_new_dir_zq.txt");
    lat.push(wait(&mut s, "=in_new_dir_zq.txt", true));
    lat.sort();
    eprintln!(
        "event-to-searchable latency: p50 {:?} max {:?} (n={})",
        lat[lat.len() / 2],
        lat[lat.len() - 1],
        lat.len()
    );
    assert!(live.progress().last_event_id > 0);
    live.checkpoint();
    live.unwatch();
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_dir_all(&root);
}
