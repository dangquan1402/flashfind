//! Minimal MCP server (JSON-RPC 2.0 over stdio, one message per line) that
//! exposes the code index to AI coding agents.
//!
//! Tools: `find_files`, `list_projects`, `recent_files`, `record_pick`,
//! `reindex`.
//!
//! Output is compact plain text by default (`format: "json"` for JSON),
//! grouped by repo with the root once and paths relative to it. Borrowed
//! from fff's MCP server (dmtrKovalenko/fff, `crates/fff-mcp`): a
//! `→ Read <path>` line when one result clearly wins, so the agent opens it
//! instead of searching again; zero-result fallbacks (drop a term, fuzzier,
//! closest projects) instead of a bare "0 results"; read-only annotations;
//! number parameters that also accept 20.0 or "20".

use crate::{CodeIndex, CodeLive, CodeQuery, CodeSession, Config, Memory, Ranker, projects_json, result_json};
use flashfind_core::CancelToken;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Server {
    live: Arc<CodeLive>,
    sess: Mutex<CodeSession>,
    memory: Option<Arc<Memory>>,
    /// Recent find_files answers: (time, query, top paths). A file from one
    /// of them that gets modified soon after is recorded as a pick for that
    /// query (the agent found it and then edited it).
    answers: Arc<Mutex<Vec<(i64, String, Vec<String>)>>>,
}

/// How long a find_files answer can turn a later edit into a pick.
const IMPLICIT_PICK_WINDOW: i64 = 15 * 60;

fn ro() -> Value {
    json!({"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false})
}

fn tools() -> Value {
    json!([
        {
            "name": "find_files",
            "description": "Find files or folders by name across all local git projects in one call (fuzzy, typo-tolerant; gitignored files and junk like node_modules/target/dist are excluded). Ranked for you: files of the current project (pass cwd), uncommitted and recently committed files, and files picked before for similar queries come first. If the reply starts with '→ Read <path>', that is the file: read it, don't search again. Query syntax: words match the file name or a parent folder; ext:rs,ts  type:file|dir  mtime:<7d  size:>1mb  'substr  ^prefix  =exact  !exclude; also repo:<name>, lang:<rust|ts|py|go|swift|...>, dirty: (only files with uncommitted changes), file.rs:42 (line suffix is echoed back).",
            "annotations": ro(),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "File name words, e.g. \"auth middleware\", \"repo:api ext:ts router\", \"lang:rust parser\". May be empty when repo/lang/dirty is given: then the matching files are listed newest first."},
                    "repo": {"type": "string", "description": "Restrict to projects whose name matches (exact, prefix or substring)."},
                    "lang": {"type": "string", "description": "Restrict to a language's file extensions."},
                    "dirty": {"type": "boolean", "description": "Only files with uncommitted changes (runs git status)."},
                    "cwd": {"type": "string", "description": "Your working directory: files of that project (and its uncommitted / recently committed files) rank first."},
                    "limit": {"type": "integer", "description": "Max results (default 15, max 200)."},
                    "format": {"type": "string", "enum": ["text", "json"], "description": "text (default, compact) or json."}
                },
                "required": []
            }
        },
        {
            "name": "list_projects",
            "description": "List local git projects, most recently active first (name, branch, last activity, root).",
            "annotations": ro(),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "description": "Substring of the project name."},
                    "limit": {"type": "integer", "description": "Max projects (default 50)."},
                    "format": {"type": "string", "enum": ["text", "json"]}
                }
            }
        },
        {
            "name": "recent_files",
            "description": "Most recently modified files across projects (gitignored/junk excluded), newest first, grouped by repo.",
            "annotations": ro(),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "lang": {"type": "string"},
                    "within": {"type": "string", "description": "Age limit like 30m, 6h, 2d, 1w (default 7d)."},
                    "limit": {"type": "integer", "description": "Max results (default 20)."},
                    "format": {"type": "string", "enum": ["text", "json"]}
                }
            }
        },
        {
            "name": "record_pick",
            "description": "Tell flashfind which file you actually used for a query (after find_files) when it was not the first result, so the next search for that query ranks it first. Files you edit soon after a find_files are learned automatically.",
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The find_files query you searched with."},
                    "path": {"type": "string", "description": "Absolute path of the file you used."}
                },
                "required": ["path"]
            }
        },
        {
            "name": "reindex",
            "description": "Rediscover projects and rebuild the index now (normally unnecessary: the index follows file changes live).",
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}

const INSTRUCTIONS: &str = "flashfind finds files by name across every local git project (gitignore-aware, live-updated). \
Rules: (1) To locate a file, call find_files once instead of listing directories or globbing; pass cwd (your working directory) so the current project ranks first. \
(2) If the reply starts with '→ Read <path>', read that file next; do not search again. \
(3) Prefer repo:<name> to scope to one project, and bare file-name words (e.g. 'auth middleware') over paths. \
(4) On zero results the reply already tried looser queries; follow its suggestion rather than retrying variants. \
(5) If you used a file that was not the first result, call record_pick so the next search ranks it first. \
list_projects shows which repos exist; recent_files shows what changed lately.";

/// Integer argument that also accepts 20.0 and "20" (LLMs send all three).
fn num(a: &Value, k: &str) -> Option<u64> {
    match a.get(k)? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f.round() as u64)),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| *f >= 0.0).map(|f| f.round() as u64),
        _ => None,
    }
}

fn home_abbrev(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

/// Is the top hit a clear winner? (only hit, or ≥8% ahead of the next).
pub fn confident(r: &crate::CodeResult) -> bool {
    match r.hits.as_slice() {
        [] => false,
        [_] => true,
        [a, b, ..] => a.kind == "file" && a.score as f64 >= b.score as f64 * 1.08,
    }
}

/// Compact text: optional `→ Read` line, a count line, then per repo
/// `name  root` and indented relative paths (with why-tags).
pub fn result_text(r: &crate::CodeResult, line: Option<u32>, note: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(n) = note {
        out.push_str(n);
        out.push('\n');
    }
    if confident(r) {
        let h = &r.hits[0];
        let at = line.map(|l| format!(":{l}")).unwrap_or_default();
        out.push_str(&format!("→ Read {}{at} (best match)\n", h.path));
    }
    let shown = r.hits.len() as u64;
    out.push_str(&if r.total > shown { format!("{shown} of {} matches\n", r.total) } else { format!("{shown} matches\n") });
    for g in &r.groups {
        out.push_str(&format!("{}  {}\n", g.repo, home_abbrev(&g.root)));
        for &i in &g.hits {
            let h = &r.hits[i];
            let dir = if h.kind == "dir" { "/" } else { "" };
            if h.why.is_empty() {
                out.push_str(&format!("  {}{dir}\n", h.rel));
            } else {
                out.push_str(&format!("  {}{dir}  ({})\n", h.rel, h.why));
            }
        }
    }
    out.truncate(out.trim_end().len());
    out
}

fn projects_text(v: &Value) -> String {
    let now = crate::memory::now();
    let mut out = format!("{} projects\n", v["total"]);
    for p in v["projects"].as_array().into_iter().flatten() {
        let age = (now - p["active"].as_i64().unwrap_or(0)).max(0);
        let ago = match age {
            a if a < 3600 => format!("{}m", a / 60),
            a if a < 86_400 => format!("{}h", a / 3600),
            a => format!("{}d", a / 86_400),
        };
        out.push_str(&format!(
            "{}  {}  {} ago  {}\n",
            p["name"].as_str().unwrap_or(""),
            p["branch"].as_str().unwrap_or("-"),
            ago,
            home_abbrev(p["root"].as_str().unwrap_or(""))
        ));
    }
    out.truncate(out.trim_end().len());
    out
}

fn parse_age(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    Some(
        n * match unit {
            "s" => 1,
            "m" | "min" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 7 * 86_400,
            "mo" => 30 * 86_400,
            "y" => 365 * 86_400,
            _ => return None,
        },
    )
}

impl Server {
    /// Open the cached index (or build it) and keep it current: FSEvents on
    /// macOS, a rebuild every `poll` elsewhere.
    pub fn new(cfg: Config, cache: Option<PathBuf>, poll: Duration) -> Server {
        let _ = poll; // see CodeLive::poll (default 300 s)
        let live = CodeLive::open(cfg, cache);
        if let Err(e) = live.watch() {
            eprintln!("flashfind mcp: not watching for changes: {e}");
        }
        live.recheck();
        Server::with_live(live)
    }

    pub fn with_live(live: Arc<CodeLive>) -> Server {
        Server::with_memory(live, Some(Memory::open_default()))
    }

    /// `memory: None` = no learning (tests).
    pub fn with_memory(live: Arc<CodeLive>, memory: Option<Arc<Memory>>) -> Server {
        let sess = match &memory {
            Some(m) => CodeSession::with_ranker(live.clone(), Ranker::new(Some(m.clone()))),
            None => CodeSession::new(live.clone()),
        };
        let answers: Arc<Mutex<Vec<(i64, String, Vec<String>)>>> = Arc::default();
        if let Some(m) = &memory {
            live.set_memory(Some(m.clone()));
            let (m, a) = (m.clone(), answers.clone());
            live.on_touch(Arc::new(move |p: &str| {
                let now = crate::memory::now();
                let mut a = a.lock().unwrap();
                a.retain(|(t, _, _)| now - t < IMPLICIT_PICK_WINDOW);
                if let Some((_, q, paths)) = a.iter_mut().rev().find(|(_, _, ps)| ps.iter().any(|x| x == p)) {
                    paths.retain(|x| x != p);
                    m.record_pick(q, p, "edit", "mcp");
                }
            }));
        }
        Server {
            sess: Mutex::new(sess),
            live,
            memory,
            answers,
        }
    }

    pub fn from_index(ci: CodeIndex) -> Server {
        let cfg = Config {
            roots: ci.roots.clone(),
            ..Config::default()
        };
        Server::with_memory(CodeLive::from_index(cfg, ci), None)
    }

    fn call(&self, name: &str, a: &Value) -> Result<Value, String> {
        let s = |k: &str| a.get(k).and_then(Value::as_str).map(str::to_string);
        let limit = |d: u64| num(a, "limit").unwrap_or(d).clamp(1, 200) as usize;
        let json_out = s("format").is_some_and(|f| f.eq_ignore_ascii_case("json"));
        match name {
            "find_files" => {
                let mut text = s("query").unwrap_or_default();
                if text.trim().is_empty() && s("repo").is_none() && s("lang").is_none() && a.get("dirty").is_none() {
                    return Err("query is required (or give repo/lang/dirty to list files)".into());
                }
                if let Some(r) = s("repo") {
                    text.push_str(&format!(" repo:{r}"));
                }
                if let Some(l) = s("lang") {
                    text.push_str(&format!(" lang:{l}"));
                }
                if a.get("dirty").and_then(Value::as_bool) == Some(true) {
                    text.push_str(" dirty:yes");
                }
                let mut cq = CodeQuery::parse(&text);
                cq.limit = limit(15);
                let cwd = s("cwd").map(PathBuf::from).or_else(|| std::env::current_dir().ok());
                let mut sess = self.sess.lock().unwrap();
                sess.set_cwd(cwd.and_then(|c| std::fs::canonicalize(c).ok()));
                let (r, note) = match sess.search(&cq, &CancelToken::new()) {
                    Ok(r) if r.hits.is_empty() && !cq.text.trim().is_empty() => self.fallback(&mut sess, &cq, r),
                    Ok(r) => (r, None),
                    Err(e) => return Err(e),
                };
                drop(sess);
                if self.memory.is_some() && !cq.text.trim().is_empty() {
                    let top: Vec<String> = r.hits.iter().take(10).map(|h| h.path.clone()).collect();
                    let mut a = self.answers.lock().unwrap();
                    a.push((crate::memory::now(), cq.text.clone(), top));
                    if a.len() > 50 {
                        a.remove(0);
                    }
                }
                if json_out {
                    let mut v = result_json(&r);
                    if let Some(n) = note {
                        v["note"] = json!(n);
                    }
                    if confident(&r) {
                        v["best"] = json!(r.hits[0].path);
                    }
                    return Ok(v);
                }
                Ok(Value::String(result_text(&r, cq.line, note.as_deref())))
            }
            "list_projects" => {
                let v = projects_json(&self.live.repos(), s("filter").as_deref(), limit(50));
                Ok(if json_out { v } else { Value::String(projects_text(&v)) })
            }
            "recent_files" => {
                let mut text = String::new();
                if let Some(r) = s("repo") {
                    text.push_str(&format!("repo:{r} "));
                }
                if let Some(l) = s("lang") {
                    text.push_str(&format!("lang:{l}"));
                }
                let mut cq = CodeQuery::parse(&text);
                cq.limit = limit(20);
                let within = match s("within") {
                    Some(w) => Some(parse_age(&w).ok_or_else(|| format!("bad within: {w}"))?),
                    None => Some(7 * 86_400),
                };
                let r = self.sess.lock().unwrap().recent(&cq, within)?;
                Ok(if json_out { result_json(&r) } else { Value::String(result_text(&r, None, None)) })
            }
            "record_pick" => {
                let path = s("path").ok_or("path is required")?;
                let abs = std::fs::canonicalize(&path).map_err(|e| format!("{path}: {e}"))?;
                let m = self.memory.as_ref().ok_or("memory is disabled")?;
                let q = CodeQuery::parse(&s("query").unwrap_or_default()).text;
                m.record_pick(&q, &abs.to_string_lossy(), "read", "mcp");
                Ok(Value::String(format!("ok: {} is now preferred for '{q}'", abs.display())))
            }
            "reindex" => {
                let t = std::time::Instant::now();
                self.live.compact();
                let st = self.live.stats();
                Ok(json!({"repos": st.repos, "entries": st.entries, "ms": t.elapsed().as_millis() as u64}))
            }
            _ => Err(format!("unknown tool {name}")),
        }
    }

    /// Zero results: try looser queries before giving up, and say what was done.
    fn fallback(&self, sess: &mut CodeSession, cq: &CodeQuery, empty: crate::CodeResult) -> (crate::CodeResult, Option<String>) {
        let words: Vec<&str> = cq.text.split_whitespace().collect();
        let run = |sess: &mut CodeSession, text: &str| -> Option<crate::CodeResult> {
            let mut q = cq.clone();
            q.text = text.to_string();
            sess.search(&q, &CancelToken::new()).ok().filter(|r| !r.hits.is_empty())
        };
        // 1. Drop one term (the one whose removal still finds something, shortest first).
        if words.len() > 1 {
            let mut order: Vec<usize> = (0..words.len()).collect();
            order.sort_by_key(|&i| words[i].len());
            for i in order {
                let rest: Vec<&str> = words.iter().enumerate().filter(|&(j, _)| j != i).map(|(_, w)| *w).collect();
                if let Some(r) = run(sess, &rest.join(" ")) {
                    return (r, Some(format!("0 results for '{}'; showing results for '{}' (dropped '{}').", cq.text, rest.join(" "), words[i])));
                }
            }
        }
        // 2. Fuzzier: strip operators/extensions, then shorten long words.
        let loose: Vec<String> = words
            .iter()
            .map(|w| {
                let w = w.trim_start_matches(['\'', '^', '=']).trim_end_matches('$');
                let w = w.rsplit_once('.').map_or(w, |(a, b)| if a.len() >= 3 && b.len() <= 5 { a } else { w });
                w.to_string()
            })
            .filter(|w| !w.is_empty())
            .collect();
        let lq = loose.join(" ");
        if lq != cq.text
            && !lq.is_empty()
            && let Some(r) = run(sess, &lq)
        {
            return (r, Some(format!("0 results for '{}'; showing fuzzier '{lq}'.", cq.text)));
        }
        let short: Vec<String> = loose.iter().map(|w| if w.chars().count() > 5 { w.chars().take(w.chars().count() * 2 / 3).collect() } else { w.clone() }).collect();
        let sq = short.join(" ");
        if sq != lq
            && let Some(r) = run(sess, &sq)
        {
            return (r, Some(format!("0 results for '{}'; showing prefix matches for '{sq}'.", cq.text)));
        }
        // 3. Nothing: point at projects with similar names.
        let snap = self.live.snapshot();
        let mut near: Vec<String> = words.iter().flat_map(|w| snap.base.similar_repos(w, 3)).collect();
        near.sort();
        near.dedup();
        let hint = if near.is_empty() {
            "Try fewer or shorter words, or list_projects to see which projects exist.".to_string()
        } else {
            format!("Projects with similar names: {}. Try repo:<name> or list_projects.", near.join(", "))
        };
        (empty, Some(format!("0 results for '{}' (also tried dropping terms and fuzzier spellings). {hint}", cq.text)))
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let id = id?;
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "flashfind", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match self.call(name, &args) {
                    Ok(Value::String(t)) => json!({"content": [{"type": "text", "text": t}], "isError": false}),
                    Ok(v) => json!({"content": [{"type": "text", "text": v.to_string()}], "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
                })
            }
            _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        })
    }

    /// Serve until EOF.
    pub fn serve(&self, input: impl BufRead, mut out: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(&msg),
                Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
            };
            if let Some(r) = reply {
                writeln!(out, "{r}")?;
                out.flush()?;
            }
        }
        Ok(())
    }
}
