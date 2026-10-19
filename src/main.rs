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
    /// Where a symbol is used: calls, `new`, JSX, type references, with the enclosing function. Exact name.
    Refs {
        name: String,
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Run as an MCP server over stdio (tools: search, def, refs, reindex).
    Mcp {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Usage totals from ~/.cache/archive/log.jsonl: queries, latency, bytes returned vs whole files.
    Stats,
    /// Run a command and print only what matters: errors with context, plus the summary at the end.
    /// Full output goes to ~/.cache/archive/runs/. Exits with the command's exit code.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
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
        Cmd::Refs { name, limit, dir } => {
            let t = Instant::now();
            let (uses, total) = find_refs(&open(&dir)?, &name, limit)?;
            log(&dir, "refs", &name, t, uses.iter().map(|u| (u.path.as_str(), u.text.len())))?;
            println!("{}", format_refs(&uses, total));
        }
        Cmd::Mcp { dir } => mcp(&dir)?,
        Cmd::Stats => stats()?,
        Cmd::Run { cmd } => std::process::exit(run(&cmd)?),
    }
    Ok(())
}

// ---------- run: condensed command output ----------

const SHORT: usize = 60; // output this short is shown as-is
const TAIL: usize = 12; // summary lines ("Tests: 2 failed, 98 passed") live at the end
const MAX_KEPT: usize = 200;

fn run(cmd: &[String]) -> Result<i32> {
    let t = Instant::now();
    let dir = log_path()?.with_file_name("runs");
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let path = dir.join(format!("{ts}-{}.log", std::process::id()));

    // One pipe for stdout and stderr so errors stay next to the lines that caused them.
    let (mut reader, writer) = std::io::pipe()?;
    let mut child = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .spawn()
        .with_context(|| format!("could not run {}", cmd[0]))?;
    std::io::copy(&mut reader, &mut std::fs::File::create(&path)?)?;
    let code = child.wait()?.code().unwrap_or(1); // killed by a signal counts as failure

    let raw = std::fs::read(&path)?;
    let text = String::from_utf8_lossy(&raw);
    let (out, kept, total) = condense(&text, code == 0);
    println!("{out}");
    println!("[archive run] exit {code} · kept {kept} of {total} lines · full log: {}", path.display());
    append_log(json!({
        "ts": ts, "dir": std::env::current_dir()?, "tool": "run", "query": cmd.join(" "),
        "ms": t.elapsed().as_millis() as u64, "exit": code, "returned_bytes": out.len(), "file_bytes": raw.len(),
    }))?;
    Ok(code)
}

/// Returns (condensed text, lines kept, lines total).
fn condense(text: &str, ok: bool) -> (String, usize, usize) {
    let lines: Vec<String> = text.lines().map(clean_line).collect();
    let n = lines.len();
    if n <= SHORT {
        return (lines.join("\n"), n, n);
    }
    let mut keep = vec![false; n];
    if !ok {
        for i in (0..n).filter(|&i| interesting(&lines[i])) {
            keep[i.saturating_sub(1)..(i + 4).min(n)].fill(true);
        }
    }
    keep[n - TAIL..].fill(true);

    let mut out: Vec<&str> = vec![];
    let mut seen = std::collections::HashSet::new();
    let mut gap = false;
    for (i, l) in lines.iter().enumerate() {
        // Long lines repeat when a type error or warning hits many files; show each once.
        let t = l.trim_start();
        let passing = t.starts_with('✓') || t.starts_with('√') || t.starts_with("PASS ") || t.starts_with("ok ");
        if !keep[i] || passing || l.trim().is_empty() || (l.len() >= 12 && !seen.insert(l.as_str())) {
            gap |= !keep[i] || passing;
            continue;
        }
        if gap && !out.is_empty() {
            out.push("  …");
        }
        gap = false;
        out.push(l);
    }
    let kept = out.iter().filter(|l| **l != "  …").count();
    if out.len() > MAX_KEPT {
        let cut = format!("  … {} more lines cut, see the full log", out.len() - MAX_KEPT);
        let tail = out.split_off(out.len() - 30);
        out.truncate(MAX_KEPT - 30);
        return ([out.join("\n"), cut, tail.join("\n")].join("\n"), MAX_KEPT, n);
    }
    (out.join("\n"), kept, n)
}

/// Strips ANSI colour codes and keeps only the last carriage-return segment (progress bars).
fn clean_line(l: &str) -> String {
    let l = l.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
    let mut out = String::with_capacity(l.len());
    let mut chars = l.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn interesting(l: &str) -> bool {
    // Stack frames inside dependencies and the runtime are noise; frames in your code are not.
    if l.contains("node_modules/") || l.contains("node:internal") || l.contains("/rustc/") {
        return false;
    }
    // expected/received diff lines are the whole point of a failed assertion
    let t = l.trim_start();
    if t.starts_with("+ ") || t.starts_with("- ") {
        return true;
    }
    let low = l.to_lowercase();
    const WORDS: &[&str] = &["error", "fail", "panic", "exception", "assert", "expected", "received", "✕", "✗", "×", "● "];
    if WORDS.iter().any(|w| low.contains(w)) {
        return true;
    }
    // file:line or file(line,col) in source we care about
    const EXTS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".rs", ".gd", ".py", ".go"];
    EXTS.iter().any(|e| {
        l.match_indices(e).any(|(i, _)| {
            let rest = &l[i + e.len()..];
            (rest.starts_with(':') || rest.starts_with('(')) && rest[1..].starts_with(|c: char| c.is_ascii_digit())
        })
    })
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
    append_log(json!({
        "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
        "dir": root, "tool": tool, "query": query, "ms": start.elapsed().as_millis() as u64,
        "hits": files.len(), "returned_bytes": returned, "file_bytes": file_bytes,
    }))
}

fn append_log(line: Value) -> Result<()> {
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(log_path()?)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn stats() -> Result<()> {
    let text = std::fs::read_to_string(log_path()?).unwrap_or_default();
    // tool -> (count, latencies, returned bytes, baseline bytes)
    let mut by_tool: std::collections::BTreeMap<String, (u64, Vec<u64>, u64, u64)> = Default::default();
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let e = by_tool.entry(v["tool"].as_str().unwrap_or("?").to_string()).or_default();
        e.0 += 1;
        e.1.push(v["ms"].as_u64().unwrap_or(0));
        e.2 += v["returned_bytes"].as_u64().unwrap_or(0);
        e.3 += v["file_bytes"].as_u64().unwrap_or(0);
    }
    if by_tool.is_empty() {
        println!("nothing logged yet");
        return Ok(());
    }
    for (tool, (n, mut lat, returned, base)) in by_tool {
        lat.sort_unstable();
        let p = |q: f64| lat[((lat.len() - 1) as f64 * q) as usize];
        let baseline = if tool == "run" { "raw output" } else { "whole hit files" };
        println!("{tool:<7} {n:>5} calls  p50 {:>5} ms  p95 {:>6} ms   ~{:>8} tokens returned vs ~{:>9} {baseline}  ({:.1}x)",
            p(0.5), p(0.95), returned / 4, base / 4, base as f64 / returned.max(1) as f64);
    }
    println!("\nestimates: tokens ≈ bytes / 4. search/def baseline assumes an agent opens every file a hit came from;");
    println!("run baseline is the full output, which a harness may already truncate. Both are upper bounds.");
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
        Some(built) => {
            let newer = |m: std::fs::Metadata| m.modified().map_or(true, |t| t > built);
            // .archiveignore is a hidden file, so the walk skips it; check it directly.
            std::fs::metadata(dir.join(".archiveignore")).map_or(false, newer)
                || walk(dir).any(|e| e.metadata().map_or(true, newer))
        }
    };
    if stale {
        let n = index(dir)?;
        eprintln!("archive: index was stale, rebuilt ({n} files)");
    }
    Ok(Connection::open(p)?)
}

/// Respects .gitignore plus `.archiveignore` (same syntax) for things you track but don't want searched, like vendor code.
fn walk(root: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(root)
        .hidden(true)
        .add_custom_ignore_filename(".archiveignore")
        .build()
        .filter_map(Result::ok)
}

fn index(dir: &Path) -> Result<usize> {
    let root = dir.canonicalize()?;
    let p = db_path(&root)?;
    let _ = std::fs::remove_file(&p);
    let mut db = Connection::open(&p)?;
    db.execute_batch(
        "CREATE VIRTUAL TABLE chunks USING fts5(path UNINDEXED, start UNINDEXED, end UNINDEXED, body, words, tokenize='unicode61');
         CREATE TABLE symbols(name TEXT, kind TEXT, path TEXT, line INTEGER, signature TEXT);
         CREATE INDEX symbols_name ON symbols(name);
         CREATE TABLE refs(name TEXT, path TEXT, line INTEGER, caller TEXT, text TEXT, plain INTEGER);",
         // refs_name index is built after the inserts; that is faster than maintaining it per row
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
            for s in symbols(&text, lang.clone()) {
                tx.execute(
                    "INSERT INTO symbols VALUES(?1,?2,?3,?4,?5)",
                    params![s.name, s.kind, rel, s.line, s.signature],
                )?;
            }
            for r in refs(&text, lang) {
                tx.execute("INSERT INTO refs VALUES(?1,?2,?3,?4,?5,?6)", params![r.name, rel, r.line, r.caller, r.text, r.plain])?;
            }
        }
        n += 1;
    }
    tx.execute_batch(
        "DELETE FROM refs WHERE plain AND name NOT IN (SELECT name FROM symbols);
         CREATE INDEX refs_name ON refs(name);",
    )?;
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

struct Use {
    path: String,
    line: i64,
    caller: String,
    text: String,
}

/// Exact name only: substring matches on "get" would return half the repo.
fn find_refs(db: &Connection, name: &str, limit: usize) -> Result<(Vec<Use>, usize)> {
    let total: usize = db.query_row("SELECT count(*) FROM refs WHERE name = ?1", [name], |r| r.get(0))?;
    let mut stmt = db.prepare(
        // implementation before tests, same order as search
        "SELECT path, line, caller, text FROM refs WHERE name = ?1
         ORDER BY (path GLOB '*.test.*' OR path GLOB '*.spec.*' OR path GLOB '*__tests__*' OR path GLOB '*.stories.*'), path, line LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![name, limit as i64], |r| {
        Ok(Use { path: r.get(0)?, line: r.get(1)?, caller: r.get(2)?, text: r.get(3)? })
    })?;
    Ok((rows.collect::<Result<_, _>>()?, total))
}

fn format_refs(uses: &[Use], total: usize) -> String {
    if uses.is_empty() {
        return "no references found".into();
    }
    let mut out: Vec<String> = uses
        .iter()
        .map(|u| {
            let caller = if u.caller.is_empty() { "top level".to_string() } else { u.caller.clone() };
            format!("{}:{}  in {}  {}", u.path, u.line, caller, u.text)
        })
        .collect();
    if total > uses.len() {
        out.push(format!("… {} more, raise --limit to see them", total - uses.len()));
    }
    out.join("\n")
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
        "gd" => Some(tree_sitter_gdscript::LANGUAGE.into()),
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
        if let Some(name_node) = def_name(node) {
            let text = &src[node.byte_range()];
            out.push(Sym {
                name: src[name_node.byte_range()].to_string(),
                kind: ["_declaration", "_declarator", "_definition", "_statement"]
                    .iter()
                    .fold(node.kind(), |k, s| k.trim_end_matches(s))
                    .replace('_', " "),
                line: node.start_position().row + 1,
                signature: text.lines().next().unwrap_or("").trim().chars().take(160).collect(),
            });
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

/// The name node if `node` defines a symbol. Plain variables only count when they hold a function.
fn def_name(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let kind = node.kind();
    let named = matches!(
        kind,
        "function_declaration" | "class_declaration" | "abstract_class_declaration" | "method_definition"
            | "interface_declaration" | "type_alias_declaration" | "enum_declaration" | "variable_declarator"
            // GDScript
            | "class_name_statement" | "class_definition" | "function_definition" | "signal_statement" | "enum_definition"
            | "variable_statement" | "const_statement"
    );
    if !named {
        return None;
    }
    let is_fn_var = kind != "variable_declarator"
        || node.child_by_field_name("value").map_or(false, |v| matches!(v.kind(), "arrow_function" | "function_expression" | "function"));
    is_fn_var.then(|| node.child_by_field_name("name")).flatten()
}

struct Ref {
    name: String,
    /// a bare mention (`onClick={save}`), not a call or type use; kept only for names defined in the repo
    plain: bool,
    line: usize,
    caller: String,
    text: String,
}

/// Uses of a name: calls, `new`, JSX components, type references. `caller` is the enclosing
/// function or class, which is the part an agent wants before a refactor.
fn refs(src: &str, lang: tree_sitter::Language) -> Vec<Ref> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(src, None) else { return vec![] };
    let lines: Vec<&str> = src.lines().collect();
    let mut out = vec![];
    let mut stack = vec![(tree.root_node(), String::new())];
    while let Some((node, caller)) = stack.pop() {
        let text = |n: tree_sitter::Node| src[n.byte_range()].to_string();
        // For a.b.c() the name is c.
        let last = |n: tree_sitter::Node| match n.kind() {
            "identifier" | "type_identifier" => Some(text(n)),
            "member_expression" | "nested_identifier" => n.child_by_field_name("property").map(text),
            _ => None,
        };
        // imports and `</Foo>` restate a use that is already counted
        let parent_import = || node.parent().map_or(false, |p| p.kind().starts_with("import") || p.kind() == "jsx_closing_element");
        // Property names count (object keys, `mock.fn = ...`, `fn: () => void` in props): a rename breaks
        // `vi.mock(..., () => ({ save: fn }))` without an error.
        let plain = matches!(
            node.kind(),
            "identifier" | "property_identifier" | "shorthand_property_identifier" | "shorthand_property_identifier_pattern"
        )
            && node.parent().and_then(def_name) != Some(node)
            && !parent_import();
        let name = match node.kind() {
            _ if plain => Some(text(node)),
            "call_expression" => node.child_by_field_name("function").and_then(last),
            "new_expression" => node.child_by_field_name("constructor").and_then(last),
            // lowercase JSX names are HTML tags
            "jsx_opening_element" | "jsx_self_closing_element" => {
                node.child_by_field_name("name").and_then(last).filter(|n| n.starts_with(char::is_uppercase))
            }
            // skip the name in `interface Foo`, which def already covers
            "type_identifier" if node.parent().and_then(def_name) != Some(node) => Some(text(node)),
            // GDScript: foo(), obj.foo(), super.foo()
            "call" | "attribute_call" | "base_call" => node.named_child(0).filter(|n| n.kind() == "identifier").map(text),
            _ => None,
        };
        if let Some(name) = name {
            let row = node.start_position().row;
            out.push(Ref {
                name,
                plain,
                line: row + 1,
                caller: caller.clone(),
                text: lines.get(row).unwrap_or(&"").trim().chars().take(160).collect(),
            });
        }
        let inner = def_name(node).map_or(caller, text);
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push((child, inner.clone()));
        }
    }
    // plain sorts last so a call on the same line wins the dedup
    out.sort_by(|a, b| (a.line, &a.name, a.plain).cmp(&(b.line, &b.name, b.plain)));
    // one row per name per line; `foo(foo())` is still one place to change
    out.dedup_by(|a, b| a.line == b.line && a.name == b.name);
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
        {"name":"refs","description":"Where a TS/JS/GDScript symbol is used: calls, new, JSX components, type references. Returns path:line, the enclosing function, and the line. Exact name. Use before renaming or changing a signature.",
         "inputSchema":{"type":"object","properties":{"name":{"type":"string"},"limit":{"type":"integer","default":50}},"required":["name"]}},
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
        "refs" => {
            let name = args["name"].as_str().ok_or_else(|| anyhow!("name required"))?;
            let limit = args["limit"].as_u64().unwrap_or(50) as usize;
            let t = Instant::now();
            let (uses, total) = find_refs(&open(dir)?, name, limit)?;
            log(dir, "refs", name, t, uses.iter().map(|u| (u.path.as_str(), u.text.len())))?;
            Ok(format_refs(&uses, total))
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

    #[test]
    fn finds_ts_refs() {
        let src = "import { load } from './x'\ninterface User { id: string }\nfunction load(u: User) {\n  return api.fetchUser(u.id)\n}\nconst Page = () => <Avatar user={load(x)} onClick={load}></Avatar>;\nnew Cache<User>();\nfetchUser(1);\n";
        let r = refs(src, tree_sitter_typescript::LANGUAGE_TSX.into());
        let plain: Vec<_> = r.iter().filter(|r| r.plain && r.name == "load").map(|r| r.line).collect();
        assert_eq!(plain, vec![] as Vec<usize>, "import, definition and same-line call are not plain uses");
        let r = refs(&src.replace("onClick={load}", "").replace("load(x)", "load"), tree_sitter_typescript::LANGUAGE_TSX.into());
        assert!(r.iter().any(|r| r.plain && r.name == "load" && r.line == 6), "callback passed by name");
        let r = refs(
            "vi.mock('./a', () => ({ load: fn }));\nconst { load: l, save } = api;\nmock.load = fn;\ninterface P { load: () => void }\n",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        );
        let got: Vec<_> = r.iter().filter(|r| r.plain).map(|r| (r.name.as_str(), r.line)).collect();
        for want in [("load", 1), ("load", 2), ("save", 2), ("load", 3), ("load", 4)] {
            assert!(got.contains(&want), "{want:?} missing from {got:?}");
        }
        let r = refs(src, tree_sitter_typescript::LANGUAGE_TSX.into());
        let got: Vec<_> = r.iter().filter(|r| !r.plain).map(|r| (r.name.as_str(), r.line - 1, r.caller.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("User", 2, "load"), ("fetchUser", 3, "load"), ("Avatar", 5, "Page"), ("load", 5, "Page"),
                ("Cache", 6, ""), ("User", 6, ""), ("fetchUser", 7, ""),
            ]
        );
    }

    #[test]
    fn finds_gdscript_refs() {
        let src = "func _ready():\n\tspawn(1)\n\tself.hit()\n\tsuper.init()\n";
        let r = refs(src, tree_sitter_gdscript::LANGUAGE.into());
        let got: Vec<_> = r.iter().filter(|r| !r.plain).map(|r| (r.name.as_str(), r.line, r.caller.as_str())).collect();
        assert_eq!(got, vec![("spawn", 2, "_ready"), ("hit", 3, "_ready"), ("init", 4, "_ready")]);
    }

    #[test]
    fn condense_keeps_errors_and_summary() {
        let mut log: Vec<String> = (0..300).map(|i| format!("\x1b[32m✓\x1b[0m passing test number {i}")).collect();
        log[100] = "src/app.ts(12,5): error TS2322: Type 'string' is not assignable to type 'number'.".into();
        log[150] = "    at run (node_modules/vitest/dist/index.js:40:3)".into();
        log[200] = "src/app.ts(12,5): error TS2322: Type 'string' is not assignable to type 'number'.".into();
        log[250] = "AssertionError: expected 1 to equal 2".into();
        log[258] = "+   \"id\": 1,".into(); // past the context window, kept because it is a diff line
        log.push("Tests  1 failed | 299 passed".into());
        let text = log.join("\n");

        let (out, _, total) = condense(&text, false);
        assert_eq!(total, 301);
        assert_eq!(out.matches("TS2322").count(), 1, "repeated error shown once");
        assert!(out.contains("\"id\": 1"), "diff line kept");
        assert!(!out.contains("node_modules"), "dependency frames dropped");
        assert!(out.contains("Tests  1 failed"), "summary kept");
        assert!(!out.contains('\x1b'), "ansi stripped");
        assert!(out.lines().count() < 25, "most of the 301 lines dropped:\n{out}");

        let (ok_out, kept, _) = condense(&text, true);
        assert!(kept <= TAIL, "success shows at most the tail");
        assert!(ok_out.contains("Tests  1 failed"));

        assert_eq!(condense("a\nb", false).0, "a\nb", "short output untouched");
        assert_eq!(clean_line("10%\r50%\r100% done"), "100% done");
    }

    #[test]
    fn finds_gdscript_symbols() {
        let src = "class_name Player\nextends Node\nsignal died(cause)\nenum State { IDLE }\nvar speed := 1\nconst MAX = 3\nfunc _ready():\n\tpass\nstatic func make() -> Player:\n\treturn null\nclass Inner:\n\tfunc hi():\n\t\tpass\n";
        let s = symbols(src, tree_sitter_gdscript::LANGUAGE.into());
        let mut got: Vec<_> = s.iter().map(|s| (s.name.as_str(), s.kind.as_str(), s.line)).collect();
        got.sort_by_key(|g| g.2);
        assert_eq!(
            got,
            vec![
                ("Player", "class name", 1), ("died", "signal", 3), ("State", "enum", 4), ("speed", "variable", 5),
                ("MAX", "const", 6), ("_ready", "function", 7), ("make", "function", 9), ("Inner", "class", 11),
                ("hi", "function", 12),
            ]
        );
    }
}
