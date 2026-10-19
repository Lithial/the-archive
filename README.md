# The Archive

Local code search for coding agents. BM25 passage search plus go-to-definition, as a CLI and an MCP server. One binary, one SQLite file, no network.

The idea: an agent that greps reads whole files into its context. An agent with a ranked `search` and an exact `def` tool pulls the passage it needs. archive is the smallest thing that does that.

## Install

Grab a binary from [releases](https://github.com/Lithial/the-archive/releases) (Linux, macOS Intel/Apple Silicon, Windows) and put it on your PATH, or build it:

```sh
cargo install --git https://github.com/lithial/the-archive
```

## Use

```sh
archive index .                              # ~3 s for 4k files; respects .gitignore
archive search "how does the websocket reconnect"
archive def useOrganizations                 # path:line [kind] signature
```

Hook it into Claude Code or Cursor by adding to `.mcp.json`:

```json
{ "mcpServers": { "archive": { "command": "archive", "args": ["mcp", "--dir", "."] } } }
```

Tools exposed: `search`, `def`, `reindex`.

## Condensed command output

```sh
archive run -- yarn test
archive run -- npx tsc --noEmit
```

Runs the command with stdout and stderr merged, saves the full output to `~/.cache/archive/runs/`, and prints only:

- error, failure, panic and assertion lines with a little context, plus expected/received diff lines
- lines pointing at your source (`file.ts:12`, `file.ts(12,5)`); frames inside `node_modules` and the runtime are dropped
- the last 12 lines, where runners print their summary

Passing-test lines, colour codes, progress-bar redraws and repeated lines are removed. Output under 60 lines is shown as-is, and a successful run shows only its tail. The exit code is the command's, so `&&` chains still work. The last line points at the full log in case the agent needs more.

On a vitest run in a real monorepo (9 failing suites, 1 failing assertion) this kept 68 of 213 lines, 5.7 KB of 15.3 KB, including the full expected/received diff.

`run` is CLI-only on purpose. As an MCP tool it would let an agent run arbitrary commands without your harness's permission prompts.

## Stats

Every query and `run` appends one line to `~/.cache/archive/log.jsonl`: tool, query, latency, bytes returned, and the size of the distinct files the hits came from. `archive stats` sums them:

```
queries        7
latency        p50 36 ms, p95 36 ms, total 0.3 s
returned       51 KB  (~13190 tokens)
whole files    1327 KB  (~339887 tokens)  what reading every hit file would cost
upper bound saved ~326697 tokens (25.8x less than reading every hit file)
```

The saving is an estimate and an upper bound: tokens are bytes/4, and it assumes a grep-then-read agent would have opened every file a hit came from. It is not measured against a real agent run. The log never leaves the machine; delete it whenever you like.

## How it works

- Files are split into 40-line passages with 10 lines of overlap and stored in SQLite FTS5. Ranking is BM25.
- Respects `.gitignore`. To leave out tracked folders like vendor code, add a `.archiveignore` in the repo root with the same syntax (`vendor/`, `third_party/`, `*.min.js`). Editing it triggers a rebuild on the next query.
- Identifiers and paths are split (`useRemovePerson.ts` → `use Remove Person`) so plain-English queries hit file and symbol names.
- Test, story and fixture files rank at 0.8× so the implementation wins a tie.
- `def` uses tree-sitter. TypeScript, JavaScript and GDScript so far; another language is a grammar crate and a few node names.
- The index lives in `~/.cache/archive/<path>.db`. Every `search`/`def`/MCP call first checks whether any file or directory is newer than the index and rebuilds if so, so a checkout, merge or worktree switch is never missed. The check is ~40 ms; a rebuild is a few seconds. `archive index` forces one.

## What it doesn't do

- No embeddings. Queries are keyword matches, so "how does X work" needs words that appear in the code.
- No daemon, no file watcher, no HTTP API.
- No telemetry, feedback, update checks or any outbound connection. The dependency list is the whole story: clap, rusqlite, serde_json, ignore, tree-sitter.

## License

MIT
