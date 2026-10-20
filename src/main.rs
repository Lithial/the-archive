//! archive: local code search for coding agents. BM25 passages + go-to-definition.
//! Index lives in ~/.cache/archive/. No network, no telemetry, nothing leaves the machine.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(feature = "honeybadger")]
mod trace;

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
    Index {
        dir: PathBuf,
        /// Only reindex changed files, as every query does first. Used to warm the index at session start.
        #[arg(long)]
        update: bool,
    },
    /// Install the pi extension: def/refs/search_code/why tools, condensed test/build output, grep hints, index warm-up.
    Install {
        #[arg(value_parser = ["pi"])]
        agent: String,
    },
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
    /// Why these lines are the way they are: the commits that last touched them, with message and PR.
    /// Takes path, path:line or path:start-end.
    Why {
        target: String,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Run as an MCP server over stdio (tools: search, def, refs, why, reindex).
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
    /// Where a Honeybadger error comes from: reads a notice as JSON (file or stdin) and finds the
    /// message and marker in the source, with drift since the deployed revision.
    #[cfg(feature = "honeybadger")]
    Trace {
        #[arg(default_value = "-")]
        notice: PathBuf,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Index { dir, update: true } => drop(open(&dir)?),
        Cmd::Index { dir, .. } => {
            let n = index(&dir)?;
            eprintln!("indexed {n} files");
        }
        Cmd::Install { .. } => install_pi()?,
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
                println!("{d}");
            }
        }
        Cmd::Refs { name, limit, dir } => {
            let t = Instant::now();
            let (uses, total) = find_refs(&open(&dir)?, &name, limit)?;
            log(&dir, "refs", &name, t, uses.iter().map(|u| (u.path.as_str(), u.text.len())))?;
            println!("{}", format_refs(&uses, total));
        }
        Cmd::Why { target, dir } => {
            let t = Instant::now();
            let out = why(&dir, &target)?;
            log(&dir, "why", &target, t, std::iter::once((target.split(':').next().unwrap_or(""), out.len())))?;
            println!("{out}");
        }
        Cmd::Mcp { dir } => mcp(&dir)?,
        Cmd::Stats => stats()?,
        Cmd::Run { cmd } => std::process::exit(run(&cmd)?),
        #[cfg(feature = "honeybadger")]
        Cmd::Trace { notice, dir } => {
            let text = if notice.as_os_str() == "-" { std::io::read_to_string(std::io::stdin())? } else { std::fs::read_to_string(&notice)? };
            let t = Instant::now();
            let out = trace::trace(&dir, &serde_json::from_str(&text).context("notice is not JSON")?)?;
            log(&dir, "trace", "", t, std::iter::once(("-", out.len())))?;
            println!("{out}");
        }
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
    const EXTS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".rs", ".gd", ".py", ".go", ".ex", ".exs", ".erl"];
    EXTS.iter().any(|e| {
        l.match_indices(e).any(|(i, _)| {
            let rest = &l[i + e.len()..];
            (rest.starts_with(':') || rest.starts_with('(')) && rest[1..].starts_with(|c: char| c.is_ascii_digit())
        })
    })
}

// ---------- why: git history for a line range ----------

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().context("could not run git")?;
    if !out.status.success() {
        return Err(anyhow!("git {}: {}", args[0], String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `path`, `path:12` or `path:12-30` -> (path, Some((start, end)))
fn parse_target(t: &str) -> Result<(&str, Option<(usize, usize)>)> {
    let Some((path, range)) = t.rsplit_once(':').filter(|(_, r)| r.starts_with(|c: char| c.is_ascii_digit())) else {
        return Ok((t, None));
    };
    let (a, b) = range.split_once('-').unwrap_or((range, range));
    let (a, b): (usize, usize) = (a.parse()?, b.parse()?);
    if a == 0 || b < a {
        return Err(anyhow!("bad line range {range}"));
    }
    Ok((path, Some((a, b))))
}

struct Blame {
    sha: String,
    lines: Vec<usize>,
    author: String,
    time: i64,
    summary: String,
}

fn parse_blame(porcelain: &str) -> Vec<Blame> {
    let mut out: Vec<Blame> = vec![];
    let mut cur: Option<usize> = None;
    for l in porcelain.lines() {
        if l.starts_with('\t') {
            continue;
        }
        let mut f = l.split(' ');
        let first = f.next().unwrap_or("");
        if first.len() == 40 && first.chars().all(|c| c.is_ascii_hexdigit()) {
            let line: usize = f.nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            let i = out.iter().position(|b| b.sha == first).unwrap_or_else(|| {
                out.push(Blame { sha: first.into(), lines: vec![], author: String::new(), time: 0, summary: String::new() });
                out.len() - 1
            });
            out[i].lines.push(line);
            cur = Some(i);
        } else if let Some(i) = cur {
            let rest = l.split_once(' ').map_or("", |x| x.1);
            match first {
                "author" => out[i].author = rest.into(),
                "author-time" => out[i].time = rest.parse().unwrap_or(0),
                "summary" => out[i].summary = rest.into(),
                _ => {}
            }
        }
    }
    out
}

/// 3,4,5,9 -> "3-5, 9"
fn line_ranges(lines: &[usize]) -> String {
    let mut v = lines.to_vec();
    v.sort_unstable();
    let mut parts: Vec<String> = vec![];
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        parts.push(if i == j { v[i].to_string() } else { format!("{}-{}", v[i], v[j]) });
        i = j + 1;
    }
    parts.join(", ")
}

/// The PR a commit arrived in. Squash merges carry `(#123)` in the subject; otherwise it is the
/// oldest merge commit leading to HEAD or the remote default branch (repos that commit straight to
/// a staging branch only get a PR when it merges into main). GitHub puts the PR title in its body.
fn pr_for(dir: &Path, sha: &str, summary: &str) -> Option<String> {
    if summary.contains("(#") {
        return None; // already in the subject
    }
    let not = format!("^{sha}");
    let mut args = vec!["log", "--merges", "--ancestry-path", "--reverse", "--format=%s%x1f%b%x1e", &not, "HEAD"];
    if git(dir, &["rev-parse", "--verify", "-q", "origin/HEAD"]).is_ok() {
        args.push("origin/HEAD");
    }
    let merges = git(dir, &args).ok()?;
    // skip local merges like "Merge branch 'main' into staging"; only GitHub's PR merges name a PR
    let (subject, body, num) = merges.split('\x1e').find_map(|m| {
        let (s, b) = m.trim().split_once('\x1f')?;
        let num = s.strip_prefix("Merge pull request ")?.split_whitespace().next()?;
        Some((s, b, num))
    })?;
    let title = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(subject);
    Some(format!("PR {num}: {title}"))
}

fn why(dir: &Path, target: &str) -> Result<String> {
    let (path, range) = parse_target(target)?;
    let mut args = vec!["blame", "--porcelain", "-w"];
    let r;
    if let Some((a, b)) = range {
        r = format!("{a},{b}");
        args.extend(["-L", &r]);
    }
    args.extend(["--", path]);
    let mut blames = parse_blame(&git(dir, &args)?);
    blames.sort_by_key(|b| -b.time); // newest first: the most recent change is usually the question
    let shown = blames.len().min(8);
    let mut out = vec![];
    for b in &blames[..shown] {
        let lines = line_ranges(&b.lines);
        if b.sha.bytes().all(|c| c == b'0') {
            out.push(format!("uncommitted  lines {lines}"));
            continue;
        }
        let date = git(dir, &["show", "-s", "--format=%as", &b.sha]).unwrap_or_default();
        out.push(format!("{} {} {}  lines {lines}\n  {}", &b.sha[..9], date.trim(), b.author, b.summary));
        let body = git(dir, &["show", "-s", "--format=%b", &b.sha]).unwrap_or_default();
        let body: Vec<&str> = body.lines().map(str::trim_end).filter(|l| !l.trim().is_empty() && !l.to_lowercase().starts_with("co-authored-by")).collect();
        for l in body.iter().take(6) {
            out.push(format!("  | {l}"));
        }
        if body.len() > 6 {
            out.push(format!("  | … git show {}", &b.sha[..9]));
        }
        if let Some(pr) = pr_for(dir, &b.sha, &b.summary) {
            out.push(format!("  {pr}"));
        }
    }
    if blames.len() > shown {
        out.push(format!("… {} older commits; narrow the line range", blames.len() - shown));
    }
    Ok(out.join("\n"))
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

/// Above this many changed files (a branch switch) a full rebuild is faster than per-file updates.
const MAX_INCREMENTAL: usize = 300;

/// Opens the index, bringing it up to date first: files whose mtime or size changed are
/// reindexed, deleted ones dropped. Lives here so no caller can skip it.
fn open(dir: &Path) -> Result<Connection> {
    let p = db_path(dir)?;
    let built = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
    let mut db = Connection::open(&p)?;
    // a session's warm-up and its first query can both update at once; wait rather than fail
    db.busy_timeout(std::time::Duration::from_secs(30))?;
    let has_files: bool = db.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'files'", [], |r| r.get(0))?;
    let full = match built {
        Some(built) if has_files => {
            let newer = |m: std::fs::Metadata| m.modified().map_or(true, |t| t > built);
            // Hidden files and node_modules are outside the walk; check them directly.
            // An install or upgrade rewrites a package's package.json.
            [".archiveignore", ".archivedeps"].iter().any(|f| std::fs::metadata(dir.join(f)).map_or(false, newer))
                || dep_dirs(dir).iter().any(|(d, _)| std::fs::metadata(d.join("package.json")).map_or(true, newer))
        }
        _ => true, // missing, or built by a version without the files table
    };
    if !full {
        let root = dir.canonicalize()?;
        let on_disk = source_files(&root);
        let mut known: std::collections::HashMap<String, (i64, i64)> = Default::default();
        for row in db.prepare("SELECT path, mtime, size FROM files")?.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))? {
            let (k, v) = row?;
            known.insert(k, v);
        }
        let changed: Vec<&(PathBuf, String, i64, i64)> = on_disk.iter().filter(|(_, rel, m, s)| known.remove(rel) != Some((*m, *s))).collect();
        let deleted: Vec<String> = known.into_keys().collect();
        if changed.len() + deleted.len() <= MAX_INCREMENTAL {
            if !changed.is_empty() || !deleted.is_empty() {
                let tx = db.transaction()?;
                let gone = json!(changed.iter().map(|f| &f.1).chain(&deleted).collect::<Vec<_>>()).to_string();
                for t in ["chunks", "symbols", "refs", "files"] {
                    tx.execute(&format!("DELETE FROM {t} WHERE path IN (SELECT value FROM json_each(?1))"), [&gone])?;
                }
                for f in &changed {
                    insert_file(&tx, &root, f)?;
                }
                tx.commit()?;
            }
            return Ok(db);
        }
    }
    drop(db);
    let n = index(dir)?;
    eprintln!("archive: index was stale, rebuilt ({n} files)");
    Ok(Connection::open(p)?)
}

/// (path, relative path, mtime ns, size) of every file the index covers.
fn source_files(root: &Path) -> Vec<(PathBuf, String, i64, i64)> {
    walk(root)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| {
            let m = e.metadata().ok().filter(|m| m.len() <= MAX_FILE_BYTES)?;
            let mtime = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as i64;
            let rel = e.path().strip_prefix(root).ok()?.to_string_lossy().into_owned();
            Some((e.into_path(), rel, mtime, m.len() as i64))
        })
        .collect()
}

/// Records the file, then its rows. Binaries get only the `files` row, so they don't look changed next time.
fn insert_file(tx: &rusqlite::Transaction, root: &Path, f: &(PathBuf, String, i64, i64)) -> Result<()> {
    tx.prepare_cached("INSERT INTO files VALUES(?1,?2,?3)")?.execute(params![f.1, f.2, f.3])?;
    if let Some(rows) = file_rows(root, &f.0) {
        insert_rows(tx, &rows)?;
    }
    Ok(())
}

fn insert_rows(tx: &rusqlite::Transaction, f: &FileRows) -> Result<()> {
    let mut chunk = tx.prepare_cached("INSERT INTO chunks(path,start,end,body,words) VALUES(?1,?2,?3,?4,?5)")?;
    for (start, end, body, words) in &f.chunks {
        chunk.execute(params![f.rel, start, end, body, words])?;
    }
    let mut sym = tx.prepare_cached("INSERT INTO symbols VALUES(?1,?2,?3,?4,?5)")?;
    for s in &f.syms {
        sym.execute(params![s.name, s.kind, f.rel, s.line, s.signature])?;
    }
    let mut rf = tx.prepare_cached("INSERT INTO refs VALUES(?1,?2,?3,?4,?5,?6)")?;
    for r in &f.refs {
        rf.execute(params![r.name, f.rel, r.line, r.caller, r.text, r.plain])?;
    }
    Ok(())
}

/// Packages listed in `.archivedeps`, one per line, found in the `node_modules` next to any
/// package.json in the repo, so each app's own copy (and version) is indexed.
/// Returns (package dir, path relative to the repo root).
fn dep_dirs(root: &Path) -> Vec<(PathBuf, String)> {
    let Ok(list) = std::fs::read_to_string(root.join(".archivedeps")) else { return vec![] };
    let pkgs: Vec<&str> = list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let mut out = vec![];
    for e in walk(root).filter(|e| e.file_name() == "package.json") {
        let ws = e.path().parent().unwrap_or(root);
        for p in &pkgs {
            let d = ws.join("node_modules").join(p);
            if d.join("package.json").is_file() {
                let rel = d.strip_prefix(root).unwrap_or(&d).to_string_lossy().into_owned();
                out.push((d, rel));
            }
        }
    }
    out
}

/// Type declarations of one dependency, with the version from its package.json.
fn index_dep(tx: &rusqlite::Transaction, dir: &Path, rel: &str) -> Result<usize> {
    let pkg: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package.json"))?)?;
    let version = pkg["version"].as_str().unwrap_or("?");
    let name = pkg["name"].as_str().unwrap_or(rel);
    let mut n = 0;
    // node_modules is gitignored, so walk it with ignore rules off; follow pnpm's symlinks
    let files = ignore::WalkBuilder::new(dir).standard_filters(false).follow_links(true).build().filter_map(Result::ok);
    for e in files {
        let f = e.file_name().to_string_lossy();
        if ![".d.ts", ".d.mts", ".d.cts"].iter().any(|x| f.ends_with(x)) || e.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let path = format!("{rel}/{}", e.path().strip_prefix(dir)?.to_string_lossy());
        for s in symbols(&text, tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()) {
            tx.execute(
                "INSERT INTO symbols VALUES(?1,?2,?3,?4,?5)",
                params![s.name, format!("{} · {name}@{version}", s.kind), path, s.line, s.signature],
            )?;
        }
        n += 1;
    }
    Ok(n)
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
    // Build beside the old index and swap it in, so queries running meanwhile still see a whole one.
    let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut db = Connection::open(&tmp)?;
    db.execute_batch(
        "CREATE VIRTUAL TABLE chunks USING fts5(path UNINDEXED, start UNINDEXED, end UNINDEXED, body, words, tokenize='unicode61');
         CREATE TABLE symbols(name TEXT, kind TEXT, path TEXT, line INTEGER, signature TEXT);
         CREATE INDEX symbols_name ON symbols(name);
         CREATE TABLE refs(name TEXT, path TEXT, line INTEGER, caller TEXT, text TEXT, plain INTEGER);
         CREATE TABLE files(path TEXT PRIMARY KEY, mtime INTEGER, size INTEGER);",
         // the other indexes are built after the inserts; that is faster than maintaining them per row
    )?;
    let tx = db.transaction()?;
    let files = source_files(&root);
    // Parsing is most of the work and runs on every core; SQLite has one writer, this thread.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut n = 0;
    std::thread::scope(|s| -> Result<()> {
        let (send, recv) = std::sync::mpsc::sync_channel(256);
        for _ in 0..threads {
            let (send, next, files, root) = (send.clone(), &next, &files, &root);
            s.spawn(move || {
                while let Some(f) = files.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                    if send.send((f, file_rows(root, &f.0))).is_err() {
                        break; // writer failed; its error is returned below
                    }
                }
            });
        }
        drop(send);
        let mut file = tx.prepare_cached("INSERT INTO files VALUES(?1,?2,?3)")?;
        for (f, rows) in recv {
            file.execute(params![f.1, f.2, f.3])?;
            if let Some(rows) = rows {
                insert_rows(&tx, &rows)?;
                n += 1;
            }
        }
        Ok(())
    })?;
    for (d, rel) in dep_dirs(&root) {
        n += index_dep(&tx, &d, &rel)?;
    }
    tx.execute_batch(
        "CREATE INDEX refs_name ON refs(name);
         CREATE INDEX refs_path ON refs(path);
         CREATE INDEX symbols_path ON symbols(path);",
    )?;
    tx.commit()?;
    drop(db);
    if std::fs::rename(&tmp, &p).is_err() {
        // Windows won't replace a file another process has open; that process only reads it briefly
        let _ = std::fs::remove_file(&p);
        std::fs::rename(&tmp, &p)?;
    }
    Ok(n)
}

// ---------- install ----------

const PI_EXTENSION: &str = include_str!("../integrations/pi/archive.ts");
const AGENTS_NOTE: &str = include_str!("../integrations/AGENTS-snippet.md");

fn install_pi() -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?);
    let agent = std::env::var_os("PI_CODING_AGENT_DIR").map_or_else(|| home.join(".pi/agent"), PathBuf::from);
    // The extension calls this exact binary, so it works even when archive is not on PATH.
    let bin = std::env::current_exe()?.canonicalize()?;
    let ext = agent.join("extensions/archive.ts");
    std::fs::create_dir_all(ext.parent().unwrap())?;
    std::fs::write(&ext, PI_EXTENSION.replace("__ARCHIVE_BIN__", &bin.to_string_lossy().replace('\\', "\\\\")))?;
    println!("wrote {}", ext.display());

    let agents_md = agent.join("AGENTS.md");
    let current = std::fs::read_to_string(&agents_md).unwrap_or_default();
    let start = "<!-- archive -->";
    let block = format!("{start}\n{}<!-- /archive -->\n", AGENTS_NOTE);
    // Replace our block on reinstall instead of appending a second copy.
    let updated = match (current.find(start), current.find("<!-- /archive -->\n")) {
        (Some(a), Some(b)) if b > a => format!("{}{block}{}", &current[..a], &current[b + "<!-- /archive -->\n".len()..]),
        _ if current.is_empty() => block,
        _ => format!("{}\n\n{block}", current.trim_end()),
    };
    std::fs::write(&agents_md, updated)?;
    println!("updated {}", agents_md.display());
    println!("restart pi or run /reload. Remove both to uninstall.");
    Ok(())
}

struct FileRows {
    rel: String,
    chunks: Vec<(usize, usize, String, String)>,
    syms: Vec<Sym>,
    refs: Vec<Ref>,
}

/// Everything one file contributes to the index. None for binaries and unreadable files.
fn file_rows(root: &Path, p: &Path) -> Option<FileRows> {
    let text = std::fs::read_to_string(p).ok()?;
    if text.bytes().any(|b| b == 0) {
        return None;
    }
    let rel = p.strip_prefix(root).ok()?.to_string_lossy().into_owned();
    let rel_words = split_idents(&rel);
    let lines: Vec<&str> = text.lines().collect();
    let mut chunks = vec![];
    let mut start = 0;
    while start < lines.len() {
        let end = (start + WINDOW).min(lines.len());
        let body = lines[start..end].join("\n");
        // path goes in too: file names carry most of the signal in real repos
        let words = format!("{rel_words}\n{}", split_idents(&body));
        chunks.push((start + 1, end, body, words));
        if end == lines.len() {
            break;
        }
        start += STEP;
    }
    let (mut syms, mut refs) = (vec![], vec![]);
    if rel.ends_with(".graphql") || rel.ends_with(".gql") {
        syms = graphql_symbols(&text);
    }
    if let Some(tree) = ts_language(p).and_then(|l| parse(&text, l)) {
        syms = symbols_in(&text, &tree);
        refs = refs_in(&text, &tree);
    }
    Some(FileRows { rel, chunks, syms, refs })
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
    /// other places with the identical definition (copied schemas, the same package in two apps)
    also: Vec<String>,
}

impl std::fmt::Display for Def {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}:{}  [{}] {}", self.path, self.line, self.kind, self.signature)?;
        if !self.also.is_empty() {
            write!(f, "\n  same in: {}", self.also.join(", "))?;
        }
        Ok(())
    }
}

fn def(db: &Connection, name: &str) -> Result<Vec<Def>> {
    let mut stmt = db.prepare(
        "SELECT path, line, kind, signature FROM symbols
         WHERE name = ?1 OR name LIKE ?2
         ORDER BY name = ?1 DESC, path LIKE '%node_modules/%', path LIMIT 60",
    )?;
    let rows = stmt.query_map(params![name, format!("%{name}%")], |r| {
        Ok(Def { path: r.get(0)?, line: r.get(1)?, kind: r.get(2)?, signature: r.get(3)?, also: vec![] })
    })?;
    let mut out: Vec<Def> = vec![];
    for d in rows {
        let d = d?;
        // kind without the @version, so two versions with the same signature still collapse
        let base = |k: &str| k.split(" · ").next().unwrap_or("").to_string();
        let full = out.len() >= 20;
        match out.iter_mut().find(|o| o.signature == d.signature && base(&o.kind) == base(&d.kind)) {
            Some(o) => o.also.push(format!("{}:{}", d.path, d.line)),
            None if !full => out.push(d),
            None => {}
        }
    }
    Ok(out)
}

const GQL_KINDS: &[&str] = &["type", "input", "enum", "interface", "union", "scalar", "directive"];

/// GraphQL SDL by line, no grammar: printed schemas put definitions at column 0 and close them
/// with `}` at column 0. Each type is one symbol whose signature is the whole block, so an agent
/// gets one input type instead of a 300 KB file. Query/Mutation/Subscription fields are symbols too.
fn graphql_symbols(text: &str) -> Vec<Sym> {
    const MAX_LINES: usize = 80;
    let lines: Vec<&str> = text.lines().collect();
    let ident = |s: &str| s.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>();
    // root type name -> operation; `schema { query: RootQueryType }` renames them
    let mut roots: Vec<(String, String)> = ["query", "mutation", "subscription"]
        .iter()
        .map(|op| (op[..1].to_uppercase() + &op[1..], op.to_string()))
        .collect();
    if let Some(s) = lines.iter().position(|l| l.starts_with("schema ") || l.starts_with("schema{")) {
        for l in lines[s + 1..].iter().take_while(|l| !l.starts_with('}')) {
            if let Some((op, ty)) = l.split_once(':') {
                let op = op.trim().to_string();
                roots.retain(|(_, o)| *o != op);
                roots.push((ident(ty.trim()), op));
            }
        }
    }
    let mut out = vec![];
    for (i, l) in lines.iter().enumerate() {
        let head = l.strip_prefix("extend ").unwrap_or(l);
        let Some(kw) = GQL_KINDS.iter().find(|k| head.starts_with(&format!("{k} "))) else { continue };
        let name = ident(head[kw.len() + 1..].trim_start_matches('@'));
        if name.is_empty() {
            continue;
        }
        let end = if l.trim_end().ends_with('{') {
            (i + 1..lines.len()).find(|&j| lines[j].starts_with('}')).unwrap_or(lines.len() - 1)
        } else {
            i
        };
        // a """description""" directly above belongs to the type
        let mut start = i;
        if i > 0 && lines[i - 1].starts_with("\"\"\"") {
            start = i - 1;
            if lines[start].trim() == "\"\"\"" {
                start = (0..start).rev().find(|&j| lines[j].starts_with("\"\"\"")).unwrap_or(start);
            }
        }
        let block = &lines[start..=end];
        let mut sig = block[..block.len().min(MAX_LINES)].join("\n");
        if block.len() > MAX_LINES {
            sig += &format!("\n  … {} more lines, read {}+", block.len() - MAX_LINES, start + MAX_LINES + 1);
        }
        out.push(Sym { name: name.clone(), kind: format!("graphql {kw}"), line: i + 1, signature: sig });

        if let Some((_, op)) = roots.iter().find(|(t, _)| *kw == "type" && *t == name) {
            // fields sit at the block's first indent; deeper lines are their multi-line args
            let body = &lines[i + 1..end];
            let indent = body.iter().find(|l| !l.trim().is_empty()).map_or(0, |l| l.len() - l.trim_start().len());
            let is_field = |l: &str| l.len() > indent && l[..indent].trim().is_empty() && l[indent..].starts_with(|c: char| c.is_alphabetic());
            for (k, f) in body.iter().enumerate().filter(|(_, l)| is_field(l)) {
                let stop = body[k + 1..].iter().position(|l| is_field(l) || l.trim_start().starts_with('"')).map_or(body.len(), |p| k + 1 + p);
                out.push(Sym {
                    name: ident(f.trim_start()),
                    kind: format!("graphql {op}"),
                    line: i + 2 + k,
                    signature: body[k..stop].iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ").trim().replace("( ", "(").replace(" )", ")"),
                });
            }
        }
    }
    out
}

struct Use {
    path: String,
    line: i64,
    caller: String,
    text: String,
}

/// Exact name only: substring matches on "get" would return half the repo.
fn find_refs(db: &Connection, name: &str, limit: usize) -> Result<(Vec<Use>, usize)> {
    // Bare mentions only count for names defined in the repo or an indexed dependency; otherwise
    // `refs map` would list every variable called map. Filtered here, not at index time, so an
    // incremental update that adds the definition brings them back.
    let only = "name = ?1 AND (NOT plain OR EXISTS (SELECT 1 FROM symbols WHERE name = ?1))";
    let total: usize = db.query_row(&format!("SELECT count(*) FROM refs WHERE {only}"), [name], |r| r.get(0))?;
    let mut stmt = db.prepare(&format!(
        // implementation before tests, same order as search
        "SELECT path, line, caller, text FROM refs WHERE {only}
         ORDER BY (path GLOB '*.test.*' OR path GLOB '*.spec.*' OR path GLOB '*__tests__*' OR path GLOB '*.stories.*'), path, line LIMIT ?2"
    ))?;
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
        "ex" | "exs" => Some(tree_sitter_elixir::LANGUAGE.into()),
        "erl" | "hrl" => Some(tree_sitter_erlang::LANGUAGE.into()),
        _ => None,
    }
}

fn parse(src: &str, lang: tree_sitter::Language) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).ok()?;
    parser.parse(src, None)
}

fn symbols(src: &str, lang: tree_sitter::Language) -> Vec<Sym> {
    parse(src, lang).map_or(vec![], |t| symbols_in(src, &t))
}

fn symbols_in(src: &str, tree: &tree_sitter::Tree) -> Vec<Sym> {
    let mut out = vec![];
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if let Some(name_node) = def_name(src, node) {
            let text = &src[node.byte_range()];
            let kind: String = match elixir_def(src, node) {
                Some("defmodule") => "module".into(),
                Some("defp") => "private function".into(),
                Some("defmacrop") => "private macro".into(),
                Some("defguardp") => "private guard".into(),
                Some("def") => "function".into(),
                Some(d) => d[3..].into(), // defmacro -> macro, defprotocol -> protocol, ...
                None => match ["_declaration", "_declarator", "_definition", "_statement", "_signature", "_decl"]
                    .iter()
                    .fold(node.kind(), |k, s| k.trim_end_matches(s))
                {
                    "fun" => "function".into(),   // Erlang fun_decl
                    "pp_define" => "macro".into(), // Erlang -define
                    k => k.replace('_', " "),
                },
            };
            out.push(Sym {
                name: src[name_node.byte_range()].to_string(),
                kind,
                line: node.start_position().row + 1,
                // A declaration without a body is all signature, often over several lines.
                signature: if node.kind().ends_with("_signature") || node.kind() == "variable_declarator" && node.child_by_field_name("value").is_none() {
                    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(400).collect()
                } else {
                    text.lines().next().unwrap_or("").trim().chars().take(160).collect()
                },
            });
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

const ELIXIR_DEFS: &[&str] = &["def", "defp", "defmacro", "defmacrop", "defguard", "defguardp", "defmodule", "defprotocol", "defimpl", "defdelegate"];

/// For an Elixir `def foo(...)` call, the defining keyword. Elixir has no def syntax: `def` is a
/// macro call, so the tree says `call` with target `def` and the head as first argument.
fn elixir_def<'a>(src: &str, node: tree_sitter::Node) -> Option<&'a str> {
    if node.kind() != "call" {
        return None;
    }
    let target = node.child_by_field_name("target").filter(|t| t.kind() == "identifier")?;
    ELIXIR_DEFS.iter().copied().find(|d| *d == &src[target.byte_range()])
}

/// `foo(a, b)` in `def foo(a, b) when guard` is the head, not a call to foo.
fn in_elixir_def_head(src: &str, node: tree_sitter::Node) -> bool {
    let mut cur = node;
    while let Some(n) = cur.parent() {
        match n.kind() {
            "arguments" => return n.parent().map_or(false, |d| elixir_def(src, d).is_some()),
            // only the left side of `head when guard`; the guard is a real call
            "binary_operator" if n.child_by_field_name("left") == Some(cur) => cur = n,
            "call" if n.child_by_field_name("target") == Some(cur) => cur = n,
            _ => return false,
        }
    }
    false
}

/// The name node of an Elixir definition: the module alias, or the function identifier inside
/// `foo(args)`, `foo(args) when guard`, or a bare `foo`.
fn elixir_def_name<'t>(src: &str, node: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
    let name = elixir_head_name(src, node)?;
    // a multi-clause function is one symbol: report the first clause of a run only
    let prev = node.prev_named_sibling().and_then(|p| elixir_head_name(src, p));
    (prev.map(|p| &src[p.byte_range()]) != Some(&src[name.byte_range()])).then_some(name)
}

fn elixir_head_name<'t>(src: &str, node: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
    elixir_def(src, node)?;
    // `arguments` is a child kind, not a field, in this grammar
    let mut cursor = node.walk();
    let args = node.named_children(&mut cursor).find(|c| c.kind() == "arguments")?;
    let mut head = args.named_child(0)?;
    loop {
        head = match head.kind() {
            "alias" | "identifier" => return Some(head),
            "call" => head.child_by_field_name("target")?,
            "binary_operator" => head.named_child(0)?, // `foo(x) when guard`, `Mod, for: T`
            _ => return None,
        };
    }
}

/// The name node if `node` defines a symbol. Plain variables only count when they hold a function.
fn def_name<'t>(src: &str, node: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
    let kind = node.kind();
    if kind == "call" {
        return elixir_def_name(src, node);
    }
    // Erlang: this grammar gives one fun_decl per clause; report the first clause of a run only
    if kind == "fun_decl" {
        let clause_name = |n: tree_sitter::Node<'t>| n.named_child(0).and_then(|c| c.named_child(0)).filter(|n| n.kind() == "atom");
        let name = clause_name(node)?;
        let prev = node.prev_named_sibling().filter(|p| p.kind() == "fun_decl").and_then(clause_name);
        return (prev.map(|p| &src[p.byte_range()]) != Some(&src[name.byte_range()])).then_some(name);
    }
    if kind == "function_clause" {
        return None; // the fun_decl above owns the name; see refs_in for the caller
    }
    if kind == "record_decl" {
        return node.named_child(0);
    }
    if kind == "pp_define" {
        return node.named_child(0).and_then(|lhs| lhs.named_child(0)); // macro_lhs -> name
    }
    if matches!(kind, "type_alias" | "opaque") {
        return node.named_child(0).and_then(|t| t.named_child(0)); // type_name -> atom
    }
    let named = matches!(
        kind,
        "function_declaration" | "class_declaration" | "abstract_class_declaration" | "method_definition"
            // bodiless forms: .d.ts files, overloads, interface methods
            | "function_signature" | "method_signature"
            | "interface_declaration" | "type_alias_declaration" | "enum_declaration" | "variable_declarator"
            // GDScript
            | "class_name_statement" | "class_definition" | "function_definition" | "signal_statement" | "enum_definition"
            | "variable_statement" | "const_statement"
    );
    if !named {
        return None;
    }
    // `declare const useStore: UseBoundStore<...>` in a .d.ts has a type and no value
    let is_fn_var = kind != "variable_declarator"
        || node.child_by_field_name("value").map_or(
            node.parent().and_then(|p| p.parent()).map_or(false, |g| g.kind() == "ambient_declaration"),
            |v| {
            matches!(v.kind(), "arrow_function" | "function_expression" | "function")
        });
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
#[cfg(test)]
fn refs(src: &str, lang: tree_sitter::Language) -> Vec<Ref> {
    parse(src, lang).map_or(vec![], |t| refs_in(src, &t))
}

fn refs_in(src: &str, tree: &tree_sitter::Tree) -> Vec<Ref> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = vec![];
    // Rc: every node on the stack carries its enclosing function name; cloning a String per node was slow
    let mut stack: Vec<(tree_sitter::Node, std::rc::Rc<str>)> = vec![(tree.root_node(), "".into())];
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
            | "alias" // Elixir module name: `Repo.get!`, `alias Foo`, `%Foo{}`, `use GenServer`
        )
            && node.parent().and_then(|p| def_name(src, p)) != Some(node)
            && !parent_import()
            // Elixir: the call arm counts `foo()` once; `def foo(a)` is a definition
            && node.parent().map_or(true, |p| p.kind() != "call" || p.child_by_field_name("target") != Some(node))
            && !in_elixir_def_head(src, node);
        let name = match node.kind() {
            _ if plain => Some(text(node)),
            "call_expression" => node.child_by_field_name("function").and_then(last),
            "new_expression" => node.child_by_field_name("constructor").and_then(last),
            // lowercase JSX names are HTML tags
            "jsx_opening_element" | "jsx_self_closing_element" => {
                node.child_by_field_name("name").and_then(last).filter(|n| n.starts_with(char::is_uppercase))
            }
            // skip the name in `interface Foo`, which def already covers
            "type_identifier" if node.parent().and_then(|p| def_name(src, p)) != Some(node) => Some(text(node)),
            // Elixir: foo(), Mod.foo(), &foo/2 and %Struct{}; `def foo` itself is a call too, skipped
            "call" if node.child_by_field_name("target").is_some() => match node.child_by_field_name("target") {
                Some(t) if t.kind() == "identifier" && elixir_def(src, node).is_none() && !in_elixir_def_head(src, node) => Some(text(t)),
                Some(t) if t.kind() == "dot" => t.named_child(1).filter(|n| n.kind() == "identifier").map(text),
                _ => None,
            },
            "binary_operator" if node.parent().map_or(false, |p| p.kind() == "unary_operator" && src[p.byte_range()].starts_with('&')) => {
                node.named_child(0).filter(|n| n.kind() == "identifier").map(text)
            }
            // GDScript: foo(), obj.foo(), super.foo(). Erlang: foo(), mod:foo() (atom)
            "call" | "attribute_call" | "base_call" => node.named_child(0).filter(|n| matches!(n.kind(), "identifier" | "atom")).map(text),
            // Erlang: fun foo/2, #rec{}, R#rec.f, ?MACRO
            "internal_fun" => node.named_child(0).filter(|n| n.kind() == "atom").map(text),
            "record_name" => Some(text(node).trim_start_matches('#').to_string()),
            "macro_call_expr" => node.named_child(0).map(text),
            _ => None,
        };
        if let Some(name) = name {
            let row = node.start_position().row;
            out.push(Ref {
                name,
                plain,
                line: row + 1,
                caller: caller.to_string(),
                text: lines.get(row).unwrap_or(&"").trim().chars().take(160).collect(),
            });
        }
        // Erlang: every clause names its function, even the ones def_name folds into the first
        let inner = match node.kind() {
            "function_clause" => node.named_child(0).map_or(caller, |n| src[n.byte_range()].into()),
            "call" => elixir_head_name(src, node).map_or(caller, |n| src[n.byte_range()].into()),
            _ => def_name(src, node).map_or(caller, |n| src[n.byte_range()].into()),
        };
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
        {"name":"why","description":"Git history for a file or line range: the commits that last changed those lines, newest first, with message body and the PR they were merged in. target is path, path:line or path:start-end. Use before changing code whose intent is unclear.",
         "inputSchema":{"type":"object","properties":{"target":{"type":"string"}},"required":["target"]}},
        {"name":"reindex","description":"Rebuild the index after large edits.","inputSchema":{"type":"object","properties":{}}}
    ]);
    #[cfg(feature = "honeybadger")]
    let tools = {
        let mut t = tools;
        t.as_array_mut().unwrap().push(json!(
            {"name":"trace","description":"Where a Honeybadger error comes from. First fetch a notice with the Honeybadger MCP (list_fault_notices, limit 1), then pass its JSON here. Returns the file:line that logs or throws the message, where the context marker is set, how far the deployed revision is behind HEAD and whether those lines changed since.",
             "inputSchema":{"type":"object","properties":{"notice":{"description":"notice JSON (object or string)"}},"required":["notice"]}}));
        t
    };
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
            Ok(defs.iter().map(Def::to_string).collect::<Vec<_>>().join("\n"))
        }
        "refs" => {
            let name = args["name"].as_str().ok_or_else(|| anyhow!("name required"))?;
            let limit = args["limit"].as_u64().unwrap_or(50) as usize;
            let t = Instant::now();
            let (uses, total) = find_refs(&open(dir)?, name, limit)?;
            log(dir, "refs", name, t, uses.iter().map(|u| (u.path.as_str(), u.text.len())))?;
            Ok(format_refs(&uses, total))
        }
        "why" => {
            let target = args["target"].as_str().ok_or_else(|| anyhow!("target required"))?;
            let t = Instant::now();
            let out = why(dir, target)?;
            log(dir, "why", target, t, std::iter::once((target.split(':').next().unwrap_or(""), out.len())))?;
            Ok(out)
        }
        "reindex" => Ok(format!("indexed {} files", index(dir)?)),
        #[cfg(feature = "honeybadger")]
        "trace" => {
            let n = &args["notice"];
            let n = match n.as_str() {
                Some(s) => serde_json::from_str(s).context("notice is not JSON")?,
                None => n.clone(),
            };
            let t = Instant::now();
            let out = trace::trace(dir, &n)?;
            log(dir, "trace", "", t, std::iter::once(("-", out.len())))?;
            Ok(out)
        }
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
    fn finds_dts_symbols() {
        let src = "export declare function useQuery<T>(\n  query: DocumentNode,\n  options?: Opts<T>\n): Result<T>;\nexport declare const create: <T>() => Store<T>;\ninterface Client { query<T>(o: Q): Promise<T> }\n";
        let s = symbols(src, tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into());
        let find = |n: &str| s.iter().find(|s| s.name == n).unwrap_or_else(|| panic!("{n} missing"));
        assert_eq!(find("useQuery").signature, "function useQuery<T>( query: DocumentNode, options?: Opts<T> ): Result<T>;");
        assert!(find("create").signature.contains("Store<T>"));
        assert!(find("query").signature.contains("Promise<T>"));
    }

    #[test]
    fn finds_graphql_types() {
        let src = "\"\"\"\nPassword change\n\"\"\"\ninput PersonUpdatePasswordInput {\n  password: String!\n}\n\ntype Mutation {\n  \"\"\"Change it\"\"\"\n  personUpdatePassword(\n    input: PersonUpdatePasswordInput!\n  ): Payload\n  ping: Boolean\n}\nscalar Date\nextend type Person @key(fields: \"id\") {\n  x: Int\n}\n";
        let s = graphql_symbols(src);
        let got: Vec<_> = s.iter().map(|s| (s.name.as_str(), s.kind.as_str(), s.line)).collect();
        assert_eq!(
            got,
            vec![
                ("PersonUpdatePasswordInput", "graphql input", 4), ("Mutation", "graphql type", 8),
                ("personUpdatePassword", "graphql mutation", 10), ("ping", "graphql mutation", 13),
                ("Date", "graphql scalar", 15), ("Person", "graphql type", 16),
            ]
        );
        assert!(s[0].signature.starts_with("\"\"\"\nPassword change") && s[0].signature.ends_with('}'));
        assert_eq!(s[2].signature, "personUpdatePassword(input: PersonUpdatePasswordInput!): Payload");
        let s = graphql_symbols("schema {\n  query: RootQueryType\n}\ntype RootQueryType {\n  me: Person\n}\n");
        assert!(s.iter().any(|s| s.name == "me" && s.kind == "graphql query"), "renamed root type");
    }

    #[test]
    fn why_helpers() {
        assert_eq!(parse_target("src/a.ts").unwrap(), ("src/a.ts", None));
        assert_eq!(parse_target("src/a.ts:12").unwrap(), ("src/a.ts", Some((12, 12))));
        assert_eq!(parse_target("C:/x.ts:3-9").unwrap(), ("C:/x.ts", Some((3, 9))));
        assert!(parse_target("a.ts:9-3").is_err());
        assert_eq!(line_ranges(&[9, 3, 4, 5, 11, 12]), "3-5, 9, 11-12");
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let p = format!("{a} 1 10 2\nauthor Ann\nauthor-time 100\nsummary fix x\n\tline\n{a} 2 11\n\tline\n{b} 5 12 1\nauthor Bob\nauthor-time 200\nsummary add y\n\tline\n");
        let bl = parse_blame(&p);
        assert_eq!(bl.len(), 2);
        assert_eq!((bl[0].lines.clone(), bl[0].author.as_str(), bl[0].summary.as_str()), (vec![10, 11], "Ann", "fix x"));
        assert_eq!((bl[1].lines.clone(), bl[1].time), (vec![12], 200));
    }

    #[test]
    fn finds_gdscript_refs() {
        let src = "func _ready():\n\tspawn(1)\n\tself.hit()\n\tsuper.init()\n";
        let r = refs(src, tree_sitter_gdscript::LANGUAGE.into());
        let got: Vec<_> = r.iter().filter(|r| !r.plain).map(|r| (r.name.as_str(), r.line, r.caller.as_str())).collect();
        assert_eq!(got, vec![("spawn", 2, "_ready"), ("hit", 3, "_ready"), ("init", 4, "_ready")]);
    }

    #[test]
    fn elixir_symbols_and_refs() {
        let src = r#"defmodule Circles.Rooms do
  use GenServer
  @spec join(binary(), map()) :: {:ok, term()}
  def join(room_id, user) when is_map(user) do
    Repo.get!(Room, room_id) |> validate(user)
    %Room{id: id} = fetch_room(room_id)
    Enum.map(ids, &validate/1)
  end
  defp validate(room, _user), do: room
  defp validate(nil, _user), do: nil
  defmacro __using__(_), do: nil
  defguard is_room(x) when is_map(x)
  defdelegate count(q), to: Repo
  defimpl Inspect, for: Room do
  end
end
"#;
        let lang: tree_sitter::Language = tree_sitter_elixir::LANGUAGE.into();
        let mut s = symbols(src, lang.clone());
        s.sort_by_key(|s| s.line);
        let kinds: Vec<(&str, &str)> = s.iter().map(|s| (s.name.as_str(), s.kind.as_str())).collect();
        assert_eq!(
            kinds,
            [
                ("Circles.Rooms", "module"),
                ("join", "function"),
                ("validate", "private function"),
                ("__using__", "macro"),
                ("is_room", "guard"),
                ("count", "delegate"),
                ("Inspect", "impl"),
            ]
        );
        assert_eq!(s[1].signature, "def join(room_id, user) when is_map(user) do");
        let r = refs(src, lang);
        let at = |n: &str| r.iter().filter(|x| x.name == n).map(|x| (x.line, x.caller.as_str())).collect::<Vec<_>>();
        assert_eq!(at("validate"), [(5, "join"), (7, "join")], "call and &validate/1; the defp is not a use");
        assert_eq!(at("get!"), [(5, "join")]);
        assert_eq!(at("Room"), [(5, "join"), (6, "join"), (14, "Inspect")], "argument, %Room{{}}, defimpl for:");
        assert_eq!(at("Repo"), [(5, "join"), (13, "count")]);
        assert_eq!(at("GenServer"), [(2, "Circles.Rooms")]);
        assert!(at("Circles.Rooms").is_empty() && at("Inspect").is_empty(), "module and defimpl names are definitions");
        assert!(at("def").is_empty() && at("defp").is_empty(), "def keywords are not calls");
        assert_eq!(at("join"), [(3, "Circles.Rooms")], "the @spec, not the def");
    }

    #[test]
    fn erlang_symbols_and_refs() {
        let src = "-module(rooms).\n-record(room, {id, name}).\n-define(TIMEOUT, 5000).\n-type t() :: ok.\njoin(RoomId, User) ->\n  Room = rooms_db:get(RoomId),\n  validate(Room, User, ?TIMEOUT);\njoin(_, _) ->\n  #room{id = 1}.\nvalidate(_R, _U, _T) -> ok.\n";
        let lang: tree_sitter::Language = tree_sitter_erlang::LANGUAGE.into();
        let mut s = symbols(src, lang.clone());
        s.sort_by_key(|s| s.line);
        let kinds: Vec<(&str, &str)> = s.iter().map(|s| (s.name.as_str(), s.kind.as_str())).collect();
        assert_eq!(kinds, [("room", "record"), ("TIMEOUT", "macro"), ("t", "type alias"), ("join", "function"), ("validate", "function")]);
        let r = refs(src, lang);
        let at = |n: &str| r.iter().filter(|x| x.name == n).map(|x| (x.line, x.caller.as_str())).collect::<Vec<_>>();
        assert_eq!(at("validate"), [(7, "join")]);
        assert_eq!(at("get"), [(6, "join")]);
        assert_eq!(at("room"), [(9, "join")]);
        assert_eq!(at("TIMEOUT"), [(7, "join")]);
    }

    #[test]
    fn open_updates_changed_and_deleted_files() {
        let tmp = std::env::temp_dir().join(format!("archive-test-{}", std::process::id()));
        let (repo, cache) = (tmp.join("repo"), tmp.join("cache"));
        std::fs::create_dir_all(&repo).unwrap();
        std::env::set_var("XDG_CACHE_HOME", &cache); // only this test touches the cache
        std::fs::write(repo.join("a.ts"), "export function alpha() {}\nconst x = { beta };\n").unwrap();
        let defs = |n: &str| def(&open(&repo).unwrap(), n).unwrap().len();
        let uses = |n: &str| find_refs(&open(&repo).unwrap(), n, 50).unwrap().1;
        assert_eq!(defs("alpha"), 1);
        assert_eq!(uses("beta"), 0, "bare mention of an undefined name is hidden");

        std::fs::write(repo.join("b.ts"), "export function beta() { alpha() }\n").unwrap();
        assert_eq!(defs("beta"), 1, "new file picked up");
        assert_eq!(uses("alpha"), 1);
        assert_eq!(uses("beta"), 1, "mention in the unchanged file shows once beta is defined");

        std::fs::remove_file(repo.join("a.ts")).unwrap();
        assert_eq!(defs("alpha"), 0, "deleted file dropped");
        assert_eq!(uses("beta"), 0);
        let db = open(&repo).unwrap();
        let n: i64 = db.query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        std::fs::remove_dir_all(&tmp).unwrap();
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
