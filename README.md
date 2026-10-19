# cq

Local code search for coding agents. BM25 passage search plus go-to-definition, as a CLI and an MCP server. One binary, one SQLite file, no network.

The idea: an agent that greps reads whole files into its context. An agent with a ranked `search` and an exact `def` tool pulls the passage it needs. cq is the smallest thing that does that.

## Install

```sh
cargo install --git https://github.com/lithial/cq
```

## Use

```sh
cq index .                              # ~3 s for 4k files; respects .gitignore
cq search "how does the websocket reconnect"
cq def useOrganizations                 # path:line [kind] signature
```

Hook it into Claude Code or Cursor by adding to `.mcp.json`:

```json
{ "mcpServers": { "cq": { "command": "cq", "args": ["mcp", "--dir", "."] } } }
```

Tools exposed: `search`, `def`, `reindex`.

## How it works

- Files are split into 40-line passages with 10 lines of overlap and stored in SQLite FTS5. Ranking is BM25.
- Identifiers and paths are split (`useRemovePerson.ts` → `use Remove Person`) so plain-English queries hit file and symbol names.
- Test, story and fixture files rank at 0.8× so the implementation wins a tie.
- `def` uses tree-sitter. TypeScript and JavaScript only for now; another language is a grammar crate and a few node names.
- The index lives in `~/.cache/cq/<path>.db`. Reindex is a full rebuild.

## What it doesn't do

- No embeddings. Queries are keyword matches, so "how does X work" needs words that appear in the code.
- No daemon, no file watcher, no HTTP API.
- No telemetry, feedback, update checks or any outbound connection. The dependency list is the whole story: clap, rusqlite, serde_json, ignore, tree-sitter.

## License

MIT
