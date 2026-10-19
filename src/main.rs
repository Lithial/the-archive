//! archive: local code search for coding agents. BM25 passages + go-to-definition.
//! Index lives in ~/.cache/archive/. No network, no telemetry, nothing leaves the machine.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const WINDOW: usize = 40; // lines per passage
const STEP: usize = 30; // overlap of 10 lines so a hit at a boundary is not cut in half
const MAX_FILE_BYTES: u64 = 1 << 20;
// Query-side only; questions like "how do we X" otherwise rank on "how"/"do".
const STOP: &[&str] = &[
    "the", "and", "for", "are", "how", "does", "where", "what", "when", "which", "with", "from", "this",
    "that", "into", "can", "you", "our", "your", "get", "set", "use", "used", "using", "should", "would",
];

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// (Re)index a folder. Respects .gitignore.
    Index { dir: PathBuf },
    /// Ranked passages (BM25) with file:line ranges.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 8)]
        limit: usize,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Go to definition: file:line + signature.
    Def {
        name: String,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Run as an MCP server over stdio (tools: search, def, reindex).
    Mcp {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Usage totals from ~/.cache/archive/log.jsonl: queries, latency, bytes returned vs whole files.
    Stats,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Index { dir } => {
            let n = index(&dir)?;
            eprintln!("indexed {n} files");
        }
        Cmd::Search { query, limit, dir } => {
            let t = Instant::now();
            let hits = search(&open(&dir)?, &query, limit)?;
            log(&dir, "search", &query, t, hits.iter().map(|h| (h.path.as_str(), h.body.len())))?;
            for h in hits {
                println!("{}:{}-{}  (score {:.1})\n{}\n", h.path, h.start, h.end, h.score, h.body);
            }
        }
        Cmd::Def { name, dir } => {
            let t = Instant::now();
            let defs = def(&open(&dir)?, &name)?;
            log(&dir, "def", &name, t, defs.iter().map(|d| (d.path.as_str(), d.signature.len())))?;
            for d in defs {
                println!("{}:{}  [{}] {}", d.path, d.line, d.kind, d.signature);
            }
        }
        Cmd::Mcp { dir } => mcp(&dir)?,
        Cmd::Stats => stats()?,
    }
    Ok(())
}

// ---------- usage log ----------

fn log_path() -> Result<PathBuf> {
    Ok(db_path(Path::new("."))?.with_file_name("log.jsonl"))
}

/// One JSONL line per query. `file_bytes` is the size of the distinct files the hits came
/// from: what an agent that grepped and then read those files would have pulled into context.
fn log<'a>(dir: &Path, tool: &str, query: &str, start: Instant, hits: impl Iterator<Item = (&'a str, usize)>) -> Result<()> {
    let root = dir.canonicalize()?;
    let mut returned = 0u64;
    let mut files = std::collections::BTreeSet::new();
    for (path, len) in hits {
        returned += len as u64;
        files.insert(path.to_string());
    }
    let file_bytes: u64 = files.iter().filter_map(|p| std::fs::metadata(root.join(p)).ok()).map(|m| m.len()).sum();
    let line = json!({
        "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
        "dir": root, "tool": tool, "query": query, "ms": start.elapsed().as_millis() as u64,
        "hits": files.len(), "returned_bytes": returned, "file_bytes": file_bytes,
    });
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(log_path()?)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn stats() -> Result<()> {
    let Ok(text) = std::fs::read_to_string(log_path()?) else {
        println!("no queries logged yet");
        return Ok(());
    };
    let (mut n, mut ms, mut hits, mut returned, mut files) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut lat = vec![];
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        n += 1;
        let m = v["ms"].as_u64().unwrap_or(0);
        ms += m;
        lat.push(m);
        hits += v["hits"].as_u64().unwrap_or(0);
        returned += v["returned_bytes"].as_u64().unwrap_or(0);
        files += v["file_bytes"].as_u64().unwrap_or(0);
    }
    if n == 0 {
        println!("no queries logged yet");
        return Ok(());
    }
    lat.sort_unstable();
    let p = |q: f64| lat[((lat.len() - 1) as f64 * q) as usize];
    println!("queries        {n}");
    println!("latency        p50 {} ms, p95 {} ms, total {:.1} s", p(0.5), p(0.95), ms as f64 / 1000.0);
    println!("hit files      {hits}");
    println!("returned       {} KB  (~{} tokens)", returned / 1024, returned / 4);
    println!("whole files    {} KB  (~{} tokens)  what reading every hit file would cost", files / 1024, files / 4);
    if files > returned {
        println!("upper bound saved ~{} tokens ({:.1}x less than reading every hit file)", (files - returned) / 4, files as f64 / returned.max(1) as f64);
    }
    println!("\nestimate: tokens ≈ bytes / 4; 'whole files' assumes a grep-then-read agent opens every file a hit came from, so it is an upper bound");
    Ok(())
}

// ---------- storage ----------

fn db_path(dir: &Path) -> Result<PathBuf> {
    let root = dir.canonicalize().with_context(|| format!("no such dir {}", dir.display()))?;
    let name = root.to_string_lossy().replace(['/', '\\', ':'], "_");
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| anyhow!("HOME not set"))?
        .join("archive");
    std::fs::create_dir_all(&cache)?;
    Ok(cache.join(format!("{name}.db")))
}

/// Opens the index, rebuilding it first if any file or directory is newer than it.
/// Directory mtimes catch deletes and checkouts. Lives here so no caller can skip it.
fn open(dir: &Path) -> Result<Connection> {
    let p = db_path(dir)?;
    let built = match std::fs::metadata(&p) {
        Ok(m) => Some(m.modified()?),
        Err(_) => None,
    };
    let stale = match built {
        None => true,
        Some(built) => walk(dir).any(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map_or(true, |t| t > built)),
    };
    if stale {
        let n = index(dir)?;
        eprintln!("archive: index was stale, rebuilt ({n} files)");
    }
    Ok(Connection::open(p)?)
}

fn walk(root: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(root).hidden(true).build().filter_map(Result::ok)
}

fn index(dir: &Path) -> Result<usize> {
    let root = dir.canonicalize()?;
    let p = db_path(&root)?;
    let _ = std::fs::remove_file(&p);
    let mut db = Connection::open(&p)?;
    db.execute_batch(
        "CREATE VIRTUAL TABLE chunks USING fts5(path UNINDEXED, start UNINDEXED, end UNINDEXED, body, words, tokenize='unicode61');
         CREATE TABLE symbols(name TEXT, kind TEXT, path TEXT, line INTEGER, signature TEXT);
         CREATE INDEX symbols_name ON symbols(name);",
    )?;
    let tx = db.transaction()?;
    let mut n = 0;
    for entry in walk(&root) {
        if !entry.file_type().map_or(false, |t| t.is_file()) {
            continue;
        }
        if entry.metadata()?.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue }; // skips binaries
        if text.bytes().any(|b| b == 0) {
            continue;
        }
        let rel = entry.path().strip_prefix(&root)?.to_string_lossy().into_owned();
        let lines: Vec<&str> = text.lines().collect();
        let mut start = 0;
        while start < lines.len() {
            let end = (start + WINDOW).min(lines.len());
            let body = lines[start..end].join("\n");
            tx.execute(
                "INSERT INTO chunks(path,start,end,body,words) VALUES(?1,?2,?3,?4,?5)",
                // path goes in too: file names carry most of the signal in real repos
                params![rel, start + 1, end, body, format!("{}\n{}", split_idents(&rel), split_idents(&body))],
            )?;
            if end == lines.len() {
                break;
            }
            start += STEP;
        }
        if let Some(lang) = ts_language(entry.path()) {
            for s in symbols(&text, lang) {
                tx.execute(
                    "INSERT INTO symbols VALUES(?1,?2,?3,?4,?5)",
                    params![s.name, s.kind, rel, s.line, s.signature],
                )?;
            }
        }
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

/// `getUserName` -> `get User Name`, so a query for "user name" matches identifiers.
fn split_idents(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 4);
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_uppercase() && prev_lower {
            out.push(' ');
        }
        out.push(if matches!(c, '_' | '/' | '.' | '-') { ' ' } else { c });
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
    }
    out
}

// ---------- search ----------

struct Hit {
    path: String,
    start: i64,
    end: i64,
    score: f64,
    body: String,
}

fn search(db: &Connection, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let terms: Vec<String> = split_idents(query)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2 && !STOP.contains(&t.to_lowercase().as_str()))
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if terms.is_empty() {
        return Ok(vec![]);
    }
    let q = terms.join(" OR ");
    let mut stmt = db.prepare(
        // Tests, stories and fixtures rank 0.8x so the implementation wins a tie.
        "SELECT path, start, end, body,
           -bm25(chunks, 0, 0, 0, 1.0, 1.0)
             * CASE WHEN path GLOB '*.test.*' OR path GLOB '*.spec.*' OR path GLOB '*.stories.*'
                      OR path GLOB '*__tests__*' OR path GLOB '*__mocks__*' OR path GLOB '*fixtures*'
                    THEN 0.8 ELSE 1.0 END AS score
         FROM chunks WHERE chunks MATCH ?1 ORDER BY score DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![q, limit as i64], |r| {
        Ok(Hit { path: r.get(0)?, start: r.get(1)?, end: r.get(2)?, body: r.get(3)?, score: r.get(4)? })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

struct Def {
    path: String,
    line: i64,
    kind: String,
    signature: String,
}

fn def(db: &Connection, name: &str) -> Result<Vec<Def>> {
    let mut stmt = db.prepare(
        "SELECT path, line, kind, signature FROM symbols
         WHERE name = ?1 OR name LIKE ?2 ORDER BY name = ?1 DESC, path LIMIT 20",
    )?;
    let rows = stmt.query_map(params![name, format!("%{name}%")], |r| {
        Ok(Def { path: r.get(0)?, line: r.get(1)?, kind: r.get(2)?, signature: r.get(3)? })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

// ---------- symbols (tree-sitter, TS/JS) ----------

struct Sym {
    name: String,
    kind: String,
    line: usize,
    signature: String,
}

fn ts_language(p: &Path) -> Option<tree_sitter::Language> {
    match p.extension()?.to_str()? {
        "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" | "jsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        _ => None,
    }
}

fn symbols(src: &str, lang: tree_sitter::Language) -> Vec<Sym> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(src, None) else { return vec![] };
    let mut out = vec![];
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        let named = matches!(
            kind,
            "function_declaration" | "class_declaration" | "abstract_class_declaration" | "method_definition"
                | "interface_declaration" | "type_alias_declaration" | "enum_declaration" | "variable_declarator"
        );
        if named {
            if let Some(name_node) = node.child_by_field_name("name") {
                let is_fn_var = kind != "variable_declarator"
                    || node.child_by_field_name("value").map_or(false, |v| {
                        matches!(v.kind(), "arrow_function" | "function_expression" | "function")
                    });
                if is_fn_var {
                    let text = &src[node.byte_range()];
                    out.push(Sym {
                        name: src[name_node.byte_range()].to_string(),
                        kind: kind.trim_end_matches("_declaration").trim_end_matches("_declarator").replace('_', " "),
                        line: node.start_position().row + 1,
                        signature: text.lines().next().unwrap_or("").trim().chars().take(160).collect(),
                    });
                }
            }
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

// ---------- MCP over stdio ----------

fn mcp(dir: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let tools = json!([
        {"name":"search","description":"BM25 search over the indexed folder. Returns ranked passages as path:start-end plus text. Use before reading whole files.",
         "inputSchema":{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","default":8}},"required":["query"]}},
        {"name":"def","description":"Go to definition of a TS/JS symbol: path:line, kind, signature. Substring match.",
         "inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},
        {"name":"reindex","description":"Rebuild the index after large edits.","inputSchema":{"type":"object","properties":{}}}
    ]);
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = serde_json::from_str(&line)?;
        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"archive","version":env!("CARGO_PKG_VERSION")}}),
            "tools/list" => json!({"tools": tools}),
            "tools/call" => match call(dir, &req["params"]) {
                Ok(text) => json!({"content":[{"type":"text","text":text}]}),
                Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
            },
            "ping" => json!({}),
            _ => {
                if id.is_none() {
                    continue; // notification
                }
                let resp = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}});
                writeln!(stdout, "{resp}")?;
                continue;
            }
        };
        let Some(id) = id else { continue };
        writeln!(stdout, "{}", json!({"jsonrpc":"2.0","id":id,"result":result}))?;
        stdout.flush()?;
    }
    Ok(())
}

fn call(dir: &Path, params: &Value) -> Result<String> {
    let args = &params["arguments"];
    match params["name"].as_str().unwrap_or("") {
        "search" => {
            let q = args["query"].as_str().ok_or_else(|| anyhow!("query required"))?;
            let limit = args["limit"].as_u64().unwrap_or(8) as usize;
            let t = Instant::now();
            let hits = search(&open(dir)?, q, limit)?;
            log(dir, "search", q, t, hits.iter().map(|h| (h.path.as_str(), h.body.len())))?;
            if hits.is_empty() {
                return Ok("no matches".into());
            }
            Ok(hits.iter().map(|h| format!("{}:{}-{}\n{}\n", h.path, h.start, h.end, h.body)).collect::<Vec<_>>().join("\n---\n"))
        }
        "def" => {
            let name = args["name"].as_str().ok_or_else(|| anyhow!("name required"))?;
            let t = Instant::now();
            let defs = def(&open(dir)?, name)?;
            log(dir, "def", name, t, defs.iter().map(|d| (d.path.as_str(), d.signature.len())))?;
            if defs.is_empty() {
                return Ok("no definition found".into());
            }
            Ok(defs.iter().map(|d| format!("{}:{}  [{}] {}", d.path, d.line, d.kind, d.signature)).collect::<Vec<_>>().join("\n"))
        }
        "reindex" => Ok(format!("indexed {} files", index(dir)?)),
        other => Err(anyhow!("unknown tool {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_camel_and_snake() {
        assert_eq!(split_idents("getUserName foo_bar HTTPServer"), "get User Name foo bar HTTPServer");
        assert_eq!(split_idents("src/useRemovePerson.ts"), "src use Remove Person ts");
    }

    #[test]
    fn finds_ts_symbols() {
        let src = "export class Foo {\n  bar(x: number) { return x }\n}\nconst baz = () => 1;\nconst notFn = 2;\nfunction qux() {}\n";
        let s = symbols(src, tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into());
        let mut names: Vec<_> = s.iter().map(|s| (s.name.as_str(), s.line)).collect();
        names.sort_by_key(|n| n.1);
        assert_eq!(names, vec![("Foo", 1), ("bar", 2), ("baz", 4), ("qux", 6)]);
    }
}
