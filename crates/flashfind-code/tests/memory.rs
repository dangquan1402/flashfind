use flashfind_code::memory::{Memory, norm_query, strip_line_suffix};
use flashfind_code::*;
use flashfind_core::CancelToken;
use std::path::{Path, PathBuf};

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("flashfind-mem-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

fn put(root: &Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "x").unwrap();
}

fn fake_repo(root: &Path, rel: &str, files: &[&str]) {
    put(root, &format!("{rel}/.git/HEAD"));
    std::fs::write(root.join(rel).join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    for f in files {
        put(root, &format!("{rel}/{f}"));
    }
}

/// 30 repos that all have README.md and src/router.ts.
fn many(tag: &str) -> (PathBuf, std::sync::Arc<CodeLive>) {
    let home = tmpdir(tag);
    for i in 0..30 {
        fake_repo(&home, &format!("dev/repo{i:02}"), &["README.md", "src/router.ts", "src/reporter.ts"]);
    }
    let cfg = Config {
        roots: vec![home.clone()],
        threads: 2,
        ..Config::default()
    };
    let live = CodeLive::from_index(cfg.clone(), CodeIndex::build(&cfg));
    (home, live)
}

fn top(sess: &mut CodeSession, q: &str) -> Vec<String> {
    let mut cq = CodeQuery::parse(q);
    cq.limit = 10;
    sess.search(&cq, &CancelToken::new()).unwrap().hits.iter().map(|h| h.path.clone()).collect()
}

#[test]
fn norm_and_line_suffix() {
    assert_eq!(norm_query("  Auth   Middleware.ts:12 "), "auth middleware.ts");
    assert_eq!(strip_line_suffix("main.rs:12:3"), ("main.rs", Some(12), Some(3)));
    assert_eq!(strip_line_suffix("main.rs:12"), ("main.rs", Some(12), None));
    assert_eq!(strip_line_suffix("main.rs"), ("main.rs", None, None));
    assert_eq!(strip_line_suffix(":12"), (":12", None, None));
    let q = CodeQuery::parse("repo:x router.ts:40");
    assert_eq!((q.text.as_str(), q.line), ("router.ts", Some(40)));
    let q = CodeQuery::parse("limit:5 router");
    assert_eq!((q.text.as_str(), q.line), ("limit:5 router", None));
}

#[test]
fn strengths_learn_from_first_pick_and_prefixes() {
    let m = Memory::in_memory();
    m.record_pick("auth", "/a/auth.ts", "open", "test");
    let r = m.recall();
    let s = r.query_strengths("auth");
    assert!((s["/a/auth.ts"] - 0.632).abs() < 0.01, "{s:?}");
    // Still typing: a prefix of a remembered query.
    assert!(r.query_strengths("aut")["/a/auth.ts"] > 0.5);
    // Refining: the remembered query is a prefix of what is typed.
    assert!(r.query_strengths("auth midd")["/a/auth.ts"] > 0.3);
    // Unrelated / too short.
    assert!(r.query_strengths("rout").is_empty());
    assert!(r.query_strengths("a").is_empty());
    m.record_pick("auth", "/a/auth.ts", "open", "test");
    m.record_pick("auth", "/a/auth.ts", "open", "test");
    assert!(m.recall().query_strengths("auth")["/a/auth.ts"] > 0.94);
}

#[test]
fn persisted_and_shared_between_instances() {
    let d = tmpdir("persist");
    let a = Memory::open(&d);
    let b = Memory::open(&d);
    a.record_pick("readme", "/x/README.md", "open", "test");
    assert_eq!(b.len(), 1, "other instance sees the append");
    assert!(b.recall().query_strengths("readme").contains_key("/x/README.md"));
    assert!(a.record_touch("/x/main.rs"));
    assert!(!b.record_touch("/x/main.rs"), "cooldown is shared");
    b.forget();
    assert_eq!(a.len(), 0);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_pick_wins_the_next_search_even_far_down_the_list() {
    let (home, live) = many("pick");
    let m = Memory::in_memory();
    let mut sess = CodeSession::with_ranker(live.clone(), Ranker::new(Some(m.clone())));
    let before = top(&mut sess, "readme");
    let want = home.join("dev/repo27/README.md").to_string_lossy().into_owned();
    assert_ne!(before[0], want);
    m.record_pick("readme", &want, "open", "test");
    assert_eq!(top(&mut sess, "readme")[0], want, "learned from one pick");
    assert_eq!(top(&mut sess, "readm")[0], want, "prefix of the remembered query");
    // Baseline ranking ignores memory.
    let mut base = CodeSession::with_ranker(live.clone(), Ranker::baseline());
    assert_eq!(top(&mut base, "readme"), before);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn weak_matches_cannot_ride_memory_past_strong_ones() {
    let (home, live) = many("weak");
    let m = Memory::in_memory();
    let mut sess = CodeSession::with_ranker(live.clone(), Ranker::new(Some(m.clone())));
    // `reporter.ts` is only a fuzzy match for "router"; picking it 5× must not
    // beat exact `router.ts` matches.
    let weak = home.join("dev/repo03/src/reporter.ts").to_string_lossy().into_owned();
    for _ in 0..5 {
        m.record_pick("router", &weak, "open", "test");
    }
    let t = top(&mut sess, "router");
    assert!(t[0].ends_with("router.ts"), "{t:?}");
    assert_eq!(rank::apply(545, 60.0), 872);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn watcher_edits_become_touches_and_frecency_ranks() {
    let (home, live) = many("touch");
    let m = Memory::in_memory();
    live.set_memory(Some(m.clone()));
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    live.on_touch(std::sync::Arc::new(move |p: &str| s2.lock().unwrap().push(p.to_string())));
    let dir = home.join("dev/repo19");
    std::fs::write(dir.join("README.md"), "edited").unwrap();
    live.apply(&[(dir.clone(), 0)]);
    let want = dir.join("README.md").to_string_lossy().into_owned();
    assert_eq!(seen.lock().unwrap().as_slice(), [want.clone()]);
    assert_eq!(m.events().iter().filter(|e| e.k == "touch").count(), 1);
    // Cooldown: the same edit burst is one touch.
    live.apply(&[(dir.clone(), 0)]);
    assert_eq!(m.events().iter().filter(|e| e.k == "touch").count(), 1);
    // Frecency alone (no query memory) moves it to the top.
    let mut f = Features::BASELINE;
    f.legacy_repo_recency = false;
    f.frecency = true;
    let mut r = Ranker::new(Some(m.clone()));
    r.features = f;
    let mut sess = CodeSession::with_ranker(live.clone(), r);
    assert_eq!(top(&mut sess, "readme")[0], want);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn frecency_decays_with_a_ten_day_half_life() {
    let m = Memory::in_memory();
    let t = memory::now();
    m.set_clock(Some(t - 10 * 86_400));
    m.record_pick("x", "/a", "open", "test");
    m.set_clock(Some(t));
    let f = m.recall().frecency["/a"];
    assert!((f - 0.5).abs() < 0.01, "{f}");
}

fn only(f: impl Fn(&mut Features)) -> Features {
    let mut x = Features::BASELINE;
    x.legacy_repo_recency = false;
    f(&mut x);
    x
}

#[test]
fn current_project_ranks_first() {
    let (home, live) = many("ctx");
    let mut r = Ranker::new(None);
    r.features = only(|f| f.context = true);
    r.cwd = Some(home.join("dev/repo12/src"));
    let mut sess = CodeSession::with_ranker(live.clone(), r);
    let t = top(&mut sess, "readme");
    assert!(t[0].ends_with("dev/repo12/README.md"), "{t:?}");
    // From the frontmost app's window title.
    let repos = live.repos();
    assert_eq!(context::from_title("router.ts — repo07", &repos).map(|r| r.name.as_str()), Some("repo07"));
    assert_eq!(context::from_title("Slack | general", &repos).map(|r| r.name.as_str()), None);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn dirty_and_recently_committed_files_rank_higher() {
    let (home, live) = many("git");
    let mut r = Ranker::new(None);
    r.features = only(|f| f.git = true);
    let root = home.join("dev/repo21");
    let mut g = git::GitInfo::default();
    g.dirty.insert(root.join("src/router.ts").to_string_lossy().into_owned());
    g.recent.insert(root.join("README.md").to_string_lossy().into_owned(), 0);
    r.git.pin(&root, g);
    // Git info of the current project is used right away.
    r.cwd = Some(root.clone());
    let mut sess = CodeSession::with_ranker(live.clone(), r);
    let want = root.join("src/router.ts").to_string_lossy().into_owned();
    assert_eq!(top(&mut sess, "router")[0], want);
    assert!(top(&mut sess, "readme")[0].ends_with("repo21/README.md"));
    let mut cq = CodeQuery::parse("router");
    cq.limit = 3;
    let h = sess.search(&cq, &flashfind_core::CancelToken::new()).unwrap();
    assert_eq!(h.hits[0].why, "uncommitted");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn real_git_status_and_log() {
    let d = tmpdir("realgit");
    let ok = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&d).args(args).output().is_ok_and(|o| o.status.success());
    if !ok(&["init", "-q"]) {
        return; // no git
    }
    put(&d, "a.rs");
    put(&d, "b.rs");
    ok(&["add", "a.rs"]);
    ok(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "a"]);
    let g = git::GitInfo::load(&d);
    let a = d.join("a.rs").to_string_lossy().into_owned();
    let b = d.join("b.rs").to_string_lossy().into_owned();
    assert_eq!(g.recent.get(&a), Some(&0));
    assert!(g.is_dirty(&b) && !g.is_dirty(&a));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn empty_query_suggests_what_you_were_working_on() {
    let (home, live) = many("empty");
    let m = Memory::in_memory();
    let mut r = Ranker::new(Some(m.clone()));
    let root = home.join("dev/repo08");
    let mut g = git::GitInfo::default();
    let dirty = root.join("src/reporter.ts").to_string_lossy().into_owned();
    g.dirty.insert(dirty.clone());
    r.git.pin(&root, g);
    r.cwd = Some(root.clone());
    let picked = home.join("dev/repo02/README.md").to_string_lossy().into_owned();
    for _ in 0..3 {
        m.record_pick("readme", &picked, "open", "test");
    }
    let mut sess = CodeSession::with_ranker(live.clone(), r);
    let mut cq = CodeQuery::parse("");
    cq.limit = 10;
    let res = sess.search(&cq, &flashfind_core::CancelToken::new()).unwrap();
    let paths: Vec<&str> = res.hits.iter().map(|h| h.path.as_str()).collect();
    assert!(paths.contains(&dirty.as_str()) && paths.contains(&picked.as_str()), "{paths:?}");
    assert!(res.hits.iter().all(|h| h.kind == "file"));
    // Legacy sessions keep the old behaviour (empty page via filters only).
    let mut legacy = CodeSession::new(live.clone());
    assert!(legacy.search(&cq, &flashfind_core::CancelToken::new()).unwrap().hits.len() <= 10);
    let _ = std::fs::remove_dir_all(home);
}
