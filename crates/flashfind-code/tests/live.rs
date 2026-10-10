use flashfind_code::*;
use flashfind_core::CancelToken;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("flashfind-codelive-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn fake_repo(root: &Path, rel: &str, files: &[&str]) {
    put(root, &format!("{rel}/.git/HEAD"), "ref: refs/heads/main\n");
    put(
        root,
        &format!("{rel}/.git/logs/HEAD"),
        "0000 1111 Dev <d@example.com> 1700000000 +0000\tcommit: x\n",
    );
    for f in files {
        put(root, &format!("{rel}/{f}"), "x");
    }
}

fn world(tag: &str) -> (PathBuf, Arc<CodeLive>) {
    let home = tmpdir(tag);
    fake_repo(
        &home,
        "dev/alpha",
        &["src/main.rs", "src/old_name.rs", "node_modules/pkg/index.js", ".gitignore"],
    );
    put(&home, "dev/alpha/.gitignore", "*.log\n");
    fake_repo(&home, "dev/beta", &["lib/beta_core.py"]);
    let cfg = Config {
        roots: vec![home.clone()],
        threads: 2,
        ..Config::default()
    };
    let live = CodeLive::open(cfg, Some(home.join(".cache")));
    (home, live)
}

fn find(s: &mut CodeSession, q: &str) -> Vec<String> {
    let mut cq = CodeQuery::parse(q);
    cq.limit = 50;
    match s.search(&cq, &CancelToken::new()) {
        Ok(r) => r.hits.iter().map(|h| format!("{}:{}", h.repo_name, h.rel)).collect(),
        Err(e) => vec![format!("ERR {e}")],
    }
}

#[test]
fn edits_inside_a_repo_are_rewalked() {
    let (home, live) = world("edit");
    let mut s = CodeSession::new(live.clone());
    assert_eq!(find(&mut s, "old_name"), vec!["alpha:src/old_name.rs"]);
    std::fs::rename(home.join("dev/alpha/src/old_name.rs"), home.join("dev/alpha/src/new_name.rs")).unwrap();
    put(&home, "dev/alpha/src/added_file.rs", "x");
    assert_eq!(live.apply(&[(home.join("dev/alpha/src"), 0)]), 1);
    assert_eq!(find(&mut s, "new_name"), vec!["alpha:src/new_name.rs"]);
    assert_eq!(find(&mut s, "added_file"), vec!["alpha:src/added_file.rs"]);
    assert!(find(&mut s, "old_name").iter().all(|p| !p.contains("old_name.rs")));
    // No duplicates from the tombstoned base copy.
    assert_eq!(find(&mut s, "=main.rs"), vec!["alpha:src/main.rs"]);
    // repo: filter works on an overlay repo and still on base repos.
    assert_eq!(find(&mut s, "repo:alpha main"), vec!["alpha:src/main.rs"]);
    assert_eq!(find(&mut s, "repo:beta core"), vec!["beta:lib/beta_core.py"]);
    assert!(find(&mut s, "x repo:nothere")[0].starts_with("ERR no project matches"));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn junk_and_git_internal_events_are_ignored() {
    let (home, live) = world("junk");
    let before = live.stats().rewalks;
    put(&home, "dev/alpha/node_modules/pkg/lib/new.js", "x");
    put(&home, "dev/alpha/.git/objects/ab/cd", "x");
    live.apply(&[(home.join("dev/alpha/node_modules/pkg/lib"), 0), (home.join("dev/alpha/.git/objects/ab"), 0)]);
    assert_eq!(live.stats().rewalks, before);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn gitignore_changes_apply() {
    let (home, live) = world("gi");
    let mut s = CodeSession::new(live.clone());
    put(&home, "dev/alpha/debug.log", "x");
    put(&home, "dev/alpha/secret.tmp", "x");
    live.apply(&[(home.join("dev/alpha"), 0)]);
    assert!(find(&mut s, "debug.log").is_empty(), "*.log ignored");
    assert_eq!(find(&mut s, "secret.tmp"), vec!["alpha:secret.tmp"]);
    put(&home, "dev/alpha/.gitignore", "*.tmp\n");
    live.apply(&[(home.join("dev/alpha"), 0)]);
    assert!(find(&mut s, "secret.tmp").is_empty(), "new rule honoured");
    assert_eq!(find(&mut s, "debug.log"), vec!["alpha:debug.log"], "old rule dropped");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn new_and_removed_repos() {
    let (home, live) = world("repos");
    let mut s = CodeSession::new(live.clone());
    assert_eq!(live.stats().repos, 2);
    fake_repo(&home, "work/gamma", &["app/gamma_view.swift"]);
    // FSEvents reports the folder that changed: `work` (new child) and
    // `work/gamma` (its contents) — either is enough.
    live.apply(&[(home.join("work"), 0)]);
    assert_eq!(live.stats().repos, 3);
    assert_eq!(find(&mut s, "gamma_view"), vec!["gamma:app/gamma_view.swift"]);
    // Hidden / skipped places never become projects.
    fake_repo(&home, "Library/Caches/x", &["a.rs"]);
    fake_repo(&home, ".hidden/y", &["b.rs"]);
    live.apply(&[(home.join("Library/Caches"), 0), (home.join(".hidden"), 0)]);
    assert_eq!(live.stats().repos, 3);
    // Remove a base repo and an overlay repo.
    std::fs::remove_dir_all(home.join("dev/beta")).unwrap();
    std::fs::remove_dir_all(home.join("work/gamma")).unwrap();
    live.apply(&[(home.join("dev"), 0), (home.join("work"), 0)]);
    assert_eq!(live.stats().repos, 1);
    assert!(find(&mut s, "beta_core").is_empty());
    assert!(find(&mut s, "gamma_view").is_empty());
    let names: Vec<String> = live.repos().iter().map(|r| r.name.clone()).collect();
    assert_eq!(names, vec!["alpha"]);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn compaction_saves_and_reopens() {
    let (home, live) = world("compact");
    let mut s = CodeSession::new(live.clone());
    put(&home, "dev/beta/lib/beta_extra.py", "x");
    fake_repo(&home, "work/delta", &["delta_main.go"]);
    live.apply(&[(home.join("dev/beta/lib"), 0), (home.join("work"), 0)]);
    let before: Vec<Vec<String>> = ["beta", "delta_main", "main"].iter().map(|q| find(&mut s, q)).collect();
    assert_eq!(live.stats().overlays, 2);
    live.checkpoint();
    let st = live.stats();
    assert_eq!((st.overlays, st.removed, st.compactions, st.repos), (0, 0, 1, 3));
    let after: Vec<Vec<String>> = ["beta", "delta_main", "main"].iter().map(|q| find(&mut s, q)).collect();
    assert_eq!(before, after);
    // The cache now holds the compacted base.
    let cfg = live.config().clone();
    drop(s);
    drop(live);
    let again = CodeLive::open(cfg, Some(home.join(".cache")));
    let mut s = CodeSession::new(again.clone());
    assert_eq!(find(&mut s, "delta_main"), vec!["delta:delta_main.go"]);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn recent_merges_overlays() {
    let (home, live) = world("recent");
    let mut s = CodeSession::new(live.clone());
    std::thread::sleep(Duration::from_millis(1100));
    put(&home, "dev/beta/lib/fresh_edit.py", "x");
    live.apply(&[(home.join("dev/beta/lib"), 0)]);
    let mut cq = CodeQuery::parse("");
    cq.limit = 2;
    let r = s.recent(&cq, None).unwrap();
    assert_eq!(r.hits[0].rel, "lib/fresh_edit.py");
    // Filter-only search goes through the same path.
    assert_eq!(find(&mut s, "repo:beta")[0], "beta:lib/fresh_edit.py");
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(target_os = "macos")]
#[test]
fn fsevents_end_to_end() {
    let (home, live) = world("fsev");
    live.watch().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let mut s = CodeSession::new(live.clone());
    let wait = |s: &mut CodeSession, q: &str, want: bool| -> Duration {
        let t = Instant::now();
        loop {
            if find(s, q).iter().any(|p| !p.starts_with("ERR")) == want {
                return t.elapsed();
            }
            assert!(t.elapsed() < Duration::from_secs(5), "{q} not {}", if want { "found" } else { "gone" });
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let mut lat = Vec::new();
    for i in 0..8 {
        put(&home, &format!("dev/alpha/src/live_{i}_zq.rs"), "x");
        lat.push(wait(&mut s, &format!("=live_{i}_zq.rs"), true));
        std::fs::rename(
            home.join(format!("dev/alpha/src/live_{i}_zq.rs")),
            home.join(format!("dev/alpha/src/moved_{i}_zq.rs")),
        )
        .unwrap();
        lat.push(wait(&mut s, &format!("=moved_{i}_zq.rs"), true));
        std::fs::remove_file(home.join(format!("dev/alpha/src/moved_{i}_zq.rs"))).unwrap();
        lat.push(wait(&mut s, &format!("=moved_{i}_zq.rs"), false));
    }
    // A new repo appears.
    fake_repo(&home, "dev/epsilon", &["eps_file_zq.ts"]);
    lat.push(wait(&mut s, "=eps_file_zq.ts", true));
    // Junk stays out.
    put(&home, "dev/alpha/node_modules/pkg/junk_zq.js", "x");
    std::thread::sleep(Duration::from_millis(300));
    assert!(find(&mut s, "junk_zq").is_empty());
    lat.sort();
    eprintln!(
        "code event-to-searchable: p50 {:?} max {:?} (n={})",
        lat[lat.len() / 2],
        lat[lat.len() - 1],
        lat.len()
    );
    live.unwatch();
    let _ = std::fs::remove_dir_all(home);
}
