//! `trace`: turn a Honeybadger notice into source locations. Optional (cargo feature `honeybadger`).
//! archive never calls Honeybadger itself: the agent fetches the notice through the Honeybadger MCP
//! and passes the JSON here, so there is no token or network code in this binary.

use super::git;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

/// Accepts a notice, a `{"results":[notice, ...]}` page from list_fault_notices, or a fault.
pub fn trace(dir: &Path, input: &Value) -> Result<String> {
    let n = input.get("results").and_then(|r| r.get(0)).unwrap_or(input);
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let message = s(&n["message"]);
    let class = s(&n["klass"]);
    let marker = s(&n["request"]["context"]["marker"]);
    let revision = [&n["environment"]["revision"], &n["revision"]].into_iter().map(s).find(|r| !r.is_empty()).unwrap_or_default();

    let mut out = vec![format!("{}{message}", if class.is_empty() { String::new() } else { format!("{class}: ") })];
    let meta: Vec<String> = [s(&n["environment_name"]), s(&n["created_at"]), s(&n["request"]["url"]), s(&n["url"])]
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect();
    if !meta.is_empty() {
        out.push(format!("  {}", meta.join(" · ")));
    }
    if !marker.is_empty() {
        out.push(format!("  marker: {marker}"));
    }
    let mut nested = vec![];
    nested_messages(&n["request"]["context"], &mut nested);
    for m in nested.iter().take(3) {
        out.push(format!("  nested: {m}"));
    }

    // How far the deployed code is from what is checked out.
    let have_rev = !revision.is_empty() && git(dir, &["cat-file", "-e", &format!("{revision}^{{commit}}")]).is_ok();
    if revision.is_empty() {
        out.push("  no revision on the notice; locations below are for HEAD".into());
    } else if !have_rev {
        out.push(format!("  deployed revision {} is not in this clone (git fetch); locations below are for HEAD", short(&revision)));
    } else {
        let behind = git(dir, &["rev-list", "--count", &format!("{revision}..HEAD")]).unwrap_or_default();
        out.push(format!("  deployed revision {} is {} commits behind HEAD", short(&revision), behind.trim()));
    }

    out.push(String::new());
    let rev = have_rev.then_some(revision.as_str());
    // Tests quote messages they expect from libraries, so test-only hits do not count as a source.
    match find_literal(dir, &message).filter(|(_, hits)| hits.iter().any(|h| !is_test(h))) {
        Some((needle, hits)) => {
            out.push(format!("message found{}:", if needle == message { String::new() } else { format!(" (fragment \"{needle}\")") }));
            out.extend(locations(dir, &hits, rev));
        }
        None if !message.is_empty() => out.push("message not in tracked source outside tests: probably thrown by a dependency or built at runtime".into()),
        None => {}
    }
    if !marker.is_empty() {
        // quoted only: backticks would match every markdown mention
        let hits = grep(dir, &[&format!("\"{marker}\""), &format!("'{marker}'")]);
        if !hits.is_empty() {
            out.push(format!("marker \"{marker}\" set at:"));
            out.extend(locations(dir, &hits, rev));
        }
    }

    let trace = n["application_trace"].as_array().filter(|a| !a.is_empty()).or(n["backtrace"].as_array());
    if let Some(frames) = trace.filter(|a| !a.is_empty()) {
        out.push("frames (minified, no source maps):".into());
        for f in frames.iter().take(5) {
            let file = s(&f["file"]);
            let file = file.rsplit('/').next().unwrap_or(&file);
            out.push(format!("  {file}:{}:{} {}", f["number"], f["column"], s(&f["method"])));
        }
        if frames.len() > 5 {
            out.push(format!("  … {} more", frames.len() - 5));
        }
    }
    Ok(out.join("\n"))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(9)]
}

/// `message` strings inside the context (a wrapped ApolloError and so on), repeated lines collapsed.
fn nested_messages(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                match (k.as_str(), v.as_str()) {
                    ("message", Some(text)) if !text.starts_with('[') => {
                        let lines: Vec<&str> = text.lines().collect();
                        let first = lines.first().copied().unwrap_or("");
                        let line = if lines.len() > 1 && lines.iter().all(|l| *l == first) { format!("{first} (×{})", lines.len()) } else { text.replace('\n', " / ") };
                        let name = m.get("name").and_then(Value::as_str).map(|n| format!("{n}: ")).unwrap_or_default();
                        let line = format!("{name}{}", line.chars().take(200).collect::<String>());
                        if !out.contains(&line) {
                            out.push(line);
                        }
                    }
                    _ => nested_messages(v, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|v| nested_messages(v, out)),
        _ => {}
    }
}

/// The whole message, or else its longest literal fragment (messages often carry ids and counts).
fn find_literal(dir: &Path, message: &str) -> Option<(String, Vec<String>)> {
    let first = message.lines().next().unwrap_or("").trim();
    if first.len() < 8 {
        return None;
    }
    let hits = grep(dir, &[first]);
    if !hits.is_empty() {
        return Some((first.to_string(), hits));
    }
    let frag = first
        .split(|c: char| c.is_ascii_digit() || "\"'`{}".contains(c))
        .map(str::trim)
        .max_by_key(|f| f.len())
        .filter(|f| f.len() >= 12 && *f != first)?;
    let hits = grep(dir, &[frag]);
    (!hits.is_empty()).then(|| (frag.to_string(), hits))
}

/// `path:line:text` for each tracked-file match, implementation before tests.
fn grep(dir: &Path, needles: &[&str]) -> Vec<String> {
    let mut args = vec!["grep", "-n", "-I", "-F"];
    for n in needles {
        args.extend(["-e", n]);
    }
    let mut hits: Vec<String> = git(dir, &args).unwrap_or_default().lines().map(String::from).collect();
    hits.sort_by_key(|h| is_test(h));
    hits
}

fn is_test(path: &str) -> bool {
    [".test.", ".spec.", "__tests__", "__mocks__", ".stories."].iter().any(|t| path.contains(t))
}

fn locations(dir: &Path, hits: &[String], rev: Option<&str>) -> Vec<String> {
    let (code, tests): (Vec<&String>, Vec<&String>) = hits.iter().partition(|h| !is_test(h));
    let mut out = vec![];
    for h in code.iter().take(5) {
        let mut parts = h.splitn(3, ':');
        let (path, line, text) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("").trim());
        out.push(format!("  {path}:{line}  {}", text.chars().take(160).collect::<String>()));
        if let Some(rev) = rev {
            let stat = git(dir, &["diff", "--numstat", rev, "HEAD", "--", path]).unwrap_or_default();
            out.push(match stat.split_whitespace().collect::<Vec<_>>()[..] {
                [add, del, ..] => format!("    changed since deploy (+{add} -{del}); archive why {path}:{line}"),
                _ => "    unchanged since deploy".into(),
            });
        }
    }
    if code.len() > 5 {
        out.push(format!("  … {} more", code.len() - 5));
    }
    if !tests.is_empty() {
        out.push(format!("  + {} in tests", tests.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collapses_repeated_nested_messages() {
        let ctx = json!({"data":[{"error":{"name":"ApolloError","message":"unauthorized\nunauthorized\nunauthorized","graphQLErrors":[{"message":"[DEPTH]"}]}}]});
        let mut out = vec![];
        nested_messages(&ctx, &mut out);
        assert_eq!(out, vec!["ApolloError: unauthorized (×3)"]);
    }

    #[test]
    fn traces_a_notice_against_this_repo() {
        // The message is a literal in this file, so git grep finds it here (tracked once committed).
        let notice = json!({"results":[{
            "message": "message not in tracked source outside tests: probably thrown by a dependency or built at runtime",
            "request": {"context": {"marker": "trace"}},
            "application_trace": [{"file":"https://x/_next/static/chunks/abc.js","number":1,"column":42,"method":"a"}]
        }]});
        let out = trace(Path::new("."), &notice).unwrap();
        assert!(out.contains("frames (minified"), "{out}");
        assert!(out.contains("abc.js:1:42 a"), "{out}");
        assert!(out.contains("no revision on the notice"), "{out}");
    }
}
