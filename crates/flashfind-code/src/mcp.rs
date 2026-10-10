//! Minimal MCP server (JSON-RPC 2.0 over stdio, one message per line) that
//! exposes the code index to AI coding agents.
//!
//! Tools: `find_files`, `list_projects`, `recent_files`, `reindex`.
//! Results are compact JSON grouped by repo (repo root once, paths relative
//! to it) to keep agent context small.

use crate::{CodeIndex, CodeQuery, Config};
use flashfind_core::{CancelToken, Session};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Server {
    cfg: Config,
    cache: Option<PathBuf>,
    max_age: Duration,
    state: Mutex<(Arc<CodeIndex>, Session)>,
    rebuilding: Arc<std::sync::atomic::AtomicBool>,
    fresh: Arc<Mutex<Option<CodeIndex>>>,
}

fn tools() -> Value {
    json!([
        {
            "name": "find_files",
            "description": "Find files or folders by name across all local git projects (fuzzy, typo-tolerant; gitignored files and junk like node_modules/target/dist are excluded). Query syntax: words match the file name or a parent folder; ext:rs,ts  type:file|dir  mtime:<7d  size:>1mb  'substr  ^prefix  =exact  !exclude; also repo:<name>, lang:<rust|ts|py|go|swift|...>, dirty: (only files with uncommitted changes). Returns {total, groups:[{repo, root, files:[paths relative to root]}]}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "e.g. \"auth middleware\", \"repo:api ext:ts router\", \"lang:rust parser\""},
                    "repo": {"type": "string", "description": "Restrict to projects whose name matches (exact, prefix or substring)."},
                    "lang": {"type": "string", "description": "Restrict to a language's file extensions."},
                    "dirty": {"type": "boolean", "description": "Only files with uncommitted changes (runs git status)."},
                    "limit": {"type": "integer", "description": "Max results (default 20, max 200)."}
                },
                "required": ["query"]
            }
        },
        {
            "name": "list_projects",
            "description": "List local git projects, most recently active first: {total, projects:[{name, root, branch, last_commit, active, entries}]} (times are Unix seconds).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "description": "Substring of the project name."},
                    "limit": {"type": "integer", "description": "Max projects (default 50)."}
                }
            }
        },
        {
            "name": "recent_files",
            "description": "Most recently modified files across projects (gitignored/junk excluded), newest first, grouped by repo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "lang": {"type": "string"},
                    "within": {"type": "string", "description": "Age limit like 30m, 6h, 2d, 1w (default 7d)."},
                    "limit": {"type": "integer", "description": "Max results (default 20)."}
                }
            }
        },
        {
            "name": "reindex",
            "description": "Rediscover projects and rebuild the index now (normally automatic).",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
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
    pub fn new(cfg: Config, cache: Option<PathBuf>, max_age: Duration) -> Server {
        let ci = cache
            .as_ref()
            .and_then(|d| CodeIndex::load(d).ok())
            .filter(|c| c.roots == cfg.roots && c.age() < max_age * 4)
            .unwrap_or_else(|| {
                let c = CodeIndex::build(&cfg);
                if let Some(d) = &cache {
                    let _ = c.save(d);
                }
                c
            });
        let idx = ci.idx.clone();
        Server {
            cfg,
            cache,
            max_age,
            state: Mutex::new((Arc::new(ci), Session::new(idx))),
            rebuilding: Arc::default(),
            fresh: Arc::default(),
        }
    }

    pub fn from_index(ci: CodeIndex) -> Server {
        let idx = ci.idx.clone();
        Server {
            cfg: Config {
                roots: ci.roots.clone(),
                ..Config::default()
            },
            cache: None,
            max_age: Duration::from_secs(u64::MAX / 4),
            state: Mutex::new((Arc::new(ci), Session::new(idx))),
            rebuilding: Arc::default(),
            fresh: Arc::default(),
        }
    }

    /// Swap in a finished background rebuild; start one if the index is old.
    fn refresh(&self) {
        if let Some(ci) = self.fresh.lock().unwrap().take() {
            let idx = ci.idx.clone();
            *self.state.lock().unwrap() = (Arc::new(ci), Session::new(idx));
        }
        let old = self.state.lock().unwrap().0.age() >= self.max_age;
        if old && !self.rebuilding.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let (cfg, cache, fresh, flag) = (self.cfg.clone(), self.cache.clone(), self.fresh.clone(), self.rebuilding.clone());
            std::thread::spawn(move || {
                let c = CodeIndex::build(&cfg);
                if let Some(d) = &cache {
                    let _ = c.save(d);
                }
                *fresh.lock().unwrap() = Some(c);
                flag.store(false, std::sync::atomic::Ordering::Release);
            });
        }
    }

    fn call(&self, name: &str, a: &Value) -> Result<Value, String> {
        let s = |k: &str| a.get(k).and_then(Value::as_str).map(str::to_string);
        let limit = |d: u64| a.get("limit").and_then(Value::as_u64).unwrap_or(d).clamp(1, 200) as usize;
        match name {
            "find_files" => {
                self.refresh();
                let mut text = s("query").ok_or("query is required")?;
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
                cq.limit = limit(20);
                let mut g = self.state.lock().unwrap();
                let (ci, sess) = &mut *g;
                let r = ci.search(sess, &cq, &CancelToken::new())?;
                Ok(ci.result_json(&r))
            }
            "list_projects" => {
                self.refresh();
                let g = self.state.lock().unwrap();
                Ok(g.0.projects_json(s("filter").as_deref(), limit(50)))
            }
            "recent_files" => {
                self.refresh();
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
                let g = self.state.lock().unwrap();
                let r = g.0.recent(&cq, within)?;
                Ok(g.0.result_json(&r))
            }
            "reindex" => {
                let c = CodeIndex::build(&self.cfg);
                if let Some(d) = &self.cache {
                    let _ = c.save(d);
                }
                let info = json!({"repos": c.info.repos, "entries": c.info.entries, "ms": c.info.discover_ms + c.info.crawl_ms + c.info.build_ms});
                let idx = c.idx.clone();
                *self.state.lock().unwrap() = (Arc::new(c), Session::new(idx));
                Ok(info)
            }
            _ => Err(format!("unknown tool {name}")),
        }
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
                "instructions": "Use find_files to locate files by (fuzzy) name across every local git project before grepping or listing directories; list_projects to see which repos exist; recent_files for what changed lately."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match self.call(name, &args) {
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
