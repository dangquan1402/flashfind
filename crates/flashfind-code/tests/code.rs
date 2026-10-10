use flashfind_code::*;
use flashfind_core::{CancelToken, Session};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("flashfind-code-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A fake repo: `.git` with HEAD + reflog (no git binary needed).
fn fake_repo(root: &Path, rel: &str, commit_ts: i64, files: &[&str]) {
    put(root, &format!("{rel}/.git/HEAD"), "ref: refs/heads/main\n");
    put(
        root,
        &format!("{rel}/.git/logs/HEAD"),
        &format!("0000 1111 Dev <dev@example.com> {commit_ts} +0000\tcommit: init\n"),
    );
    for i in 0..30 {
        put(root, &format!("{rel}/.git/objects/ab/obj{i}"), "x");
    }
    for f in files {
        put(root, &format!("{rel}/{f}"), "x");
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn world(tag: &str) -> (PathBuf, Config) {
    let home = tmpdir(tag);
    let t = now();
    fake_repo(
        &home,
        "dev/webapp",
        t - 3600,
        &[
            "src/router.ts",
            "src/auth/middleware.ts",
            "package.json",
            ".gitignore",
            "node_modules/express/lib/router.js",
            "node_modules/express/package.json",
            "dist/bundle.js",
            "logs/debug.log",
            ".github/workflows/ci.yml",
        ],
    );
    put(&home, "dev/webapp/.gitignore", "*.log\ncoverage/\n");
    fake_repo(
        &home,
        "dev/engine",
        t - 400 * 86_400,
        &["src/router.rs", "src/parser.rs", "Cargo.toml", "target/debug/router.d", "target/release/engine"],
    );
    fake_repo(
        &home,
        "work/clients/api-server",
        t - 2 * 86_400,
        &["app/router.py", "app/models.py", ".venv/lib/site.py"],
    );
    // Not repos / not to be searched.
    put(&home, "Library/Caches/dev/fake/.git/HEAD", "ref: refs/heads/main\n");
    put(&home, "dev/bench.noindex/fake/.git/HEAD", "ref: refs/heads/main\n");
    put(&home, "Desktop/__MACOSX/copy/.git/HEAD", "ref: refs/heads/main\n");
    put(&home, "Documents/router_notes.txt", "x");
    let cfg = Config {
        roots: vec![home.clone()],
        threads: 2,
        ..Config::default()
    };
    (home, cfg)
}

fn search(ci: &CodeIndex, q: &str) -> Vec<String> {
    let mut s = Session::new(ci.idx.clone());
    let mut cq = CodeQuery::parse(q);
    cq.limit = 50;
    ci.search(&mut s, &cq, &CancelToken::new())
        .unwrap()
        .hits
        .iter()
        .map(|h| format!("{}:{}", ci.repos[h.repo].name, h.rel))
        .collect()
}

#[test]
fn discovery_finds_repos_and_skips_library() {
    let (home, cfg) = world("disc");
    let repos = discover(&cfg);
    let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
    // Most recently active first.
    assert_eq!(names, vec!["webapp", "api-server", "engine"]);
    assert_eq!(repos[0].branch.as_deref(), Some("main"));
    assert!(repos[0].last_commit > 0);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn junk_and_gitignored_files_are_excluded() {
    let (home, cfg) = world("junk");
    let ci = CodeIndex::build(&cfg);
    let all = search(&ci, "router");
    assert!(all.contains(&"webapp:src/router.ts".to_string()), "{all:?}");
    assert!(all.contains(&"engine:src/router.rs".to_string()));
    assert!(all.contains(&"api-server:app/router.py".to_string()));
    for bad in ["node_modules", "target/", "dist/", ".git/", "router_notes", ".venv"] {
        assert!(all.iter().all(|p| !p.contains(bad)), "{bad} leaked: {all:?}");
    }
    assert!(search(&ci, "debug.log").is_empty(), "gitignored *.log");
    assert_eq!(search(&ci, "ci.yml"), vec!["webapp:.github/workflows/ci.yml"], "hidden but not ignored");
    // Recently active repo first among equal matches (stem matches).
    assert_eq!(all[0], "webapp:src/router.ts");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn repo_and_lang_filters_and_grouping() {
    let (home, cfg) = world("filters");
    let ci = CodeIndex::build(&cfg);
    assert_eq!(search(&ci, "router repo:engine"), vec!["engine:src/router.rs"]);
    assert_eq!(search(&ci, "router repo:api"), vec!["api-server:app/router.py"], "prefix match");
    assert_eq!(search(&ci, "router lang:rust"), vec!["engine:src/router.rs"]);
    assert_eq!(search(&ci, "lang:py repo:api-server models"), vec!["api-server:app/models.py"]);
    let mut s = Session::new(ci.idx.clone());
    assert!(ci.search(&mut s, &CodeQuery::parse("x repo:nope"), &CancelToken::new()).is_err());
    let e = ci.search(&mut s, &CodeQuery::parse("x repo:engnie"), &CancelToken::new()).unwrap_err();
    assert!(e.contains("closest: engine"), "{e}");
    // Filters only: list the repo's files, newest first.
    let all = search(&ci, "repo:engine");
    assert!(
        all.contains(&"engine:src/parser.rs".to_string()) && all.iter().all(|p| p.starts_with("engine:")),
        "{all:?}"
    );
    // Folder + name across repos.
    assert_eq!(search(&ci, "auth middleware")[0], "webapp:src/auth/middleware.ts");
    // Project folder itself is findable.
    assert!(search(&ci, "=api-server").contains(&"api-server:.".to_string()));
    let mut cq = CodeQuery::parse("router");
    cq.limit = 10;
    let r = ci.search(&mut s, &cq, &CancelToken::new()).unwrap();
    assert_eq!(r.groups.len(), 3);
    let j = ci.result_json(&r);
    assert_eq!(j["groups"][0]["repo"], "webapp");
    assert!(j["groups"][0]["root"].as_str().unwrap().ends_with("dev/webapp"));
    // Save/load round trip.
    let cache = home.join("cache");
    ci.save(&cache).unwrap();
    let back = CodeIndex::load(&cache).unwrap();
    assert_eq!(search(&back, "router repo:engine"), vec!["engine:src/router.rs"]);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn recent_files_newest_first() {
    let (home, cfg) = world("recent");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    put(&home, "dev/engine/src/just_edited.rs", "x");
    let ci = CodeIndex::build(&cfg);
    let mut cq = CodeQuery::parse("");
    cq.limit = 3;
    let r = ci.recent(&cq, Some(3600)).unwrap();
    assert_eq!(r.hits[0].rel, "src/just_edited.rs");
    let cq = CodeQuery::parse("lang:py");
    let r = ci.recent(&cq, None).unwrap();
    assert!(r.hits.iter().all(|h| h.rel.ends_with(".py")));
    let _ = std::fs::remove_dir_all(home);
}

fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn dirty_filter_uses_git_status() {
    let home = tmpdir("dirty");
    let repo = home.join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    if !git(&repo, &["init", "-q"]) {
        eprintln!("git not available; skipping");
        return;
    }
    put(&repo, "a/clean.rs", "1");
    put(&repo, "a/changed.rs", "1");
    assert!(git(&repo, &["add", "."]) && git(&repo, &["commit", "-qm", "init"]));
    put(&repo, "a/changed.rs", "2");
    put(&repo, "a/untracked_new.rs", "1");
    let cfg = Config {
        roots: vec![home.clone()],
        threads: 2,
        ..Config::default()
    };
    let ci = CodeIndex::build(&cfg);
    let mut got = search(&ci, "dirty: .rs");
    got.sort();
    assert_eq!(got, vec!["proj:a/changed.rs", "proj:a/untracked_new.rs"]);
    assert_eq!(search(&ci, "clean").len(), 1);
    assert!(ci.repos[0].last_commit > 0, "reflog parsed from a real repo");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn mcp_protocol() {
    let (home, cfg) = world("mcp");
    let srv = mcp::Server::from_index(CodeIndex::build(&cfg));
    let input = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"find_files","arguments":{"query":"router","repo":"engine"}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_projects","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"recent_files","arguments":{"within":"1d","limit":5}}}),
        json!({"jsonrpc":"2.0","id":6,"method":"nope"}),
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"find_files","arguments":{}}}),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    let mut out = Vec::new();
    srv.serve(input.as_bytes(), &mut out).unwrap();
    let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.len(), 7, "notification gets no reply");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "flashfind");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["find_files", "list_projects", "recent_files", "reindex"]);
    let text: Value = serde_json::from_str(replies[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["groups"][0]["repo"], "engine");
    assert_eq!(text["groups"][0]["files"][0], "src/router.rs");
    let text: Value = serde_json::from_str(replies[3]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["total"], 3);
    assert_eq!(text["projects"][0]["name"], "webapp");
    assert_eq!(replies[4]["result"]["isError"], false);
    assert_eq!(replies[5]["error"]["code"], -32601);
    assert_eq!(replies[6]["result"]["isError"], true);
    let _ = std::fs::remove_dir_all(home);
}
