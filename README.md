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
archive index .                              # ~6 s for 4k files; respects .gitignore
archive search "how does the websocket reconnect"
archive def useOrganizations                 # path:line [kind] signature
archive refs hasMatchingConsent              # path:line  in <enclosing fn>  <line>
```

`refs` lists every place a name is used: calls, `new`, JSX components, type references, callbacks passed by name, destructuring, and object keys such as `vi.mock(..., () => ({ save: fn }))`, which a rename would otherwise break without an error. Each hit names the function or class it sits in. Implementation files come before tests. It matches the exact name, from the syntax tree rather than text, so comments and strings are skipped. It does not resolve scopes: two unrelated functions called `save` share one list.

Checked against grep on 100 random symbols in a 4k-file TypeScript monorepo: of 534 lines grep matched (after dropping obvious comment lines), refs missed 8. None were code uses: they were comments, GraphQL query strings, closing tags and a multi-line import. References roughly add 60% to the index (78 MB to 125 MB for 4k files).

### pi

```sh
archive install pi
```

This writes `~/.pi/agent/extensions/archive.ts` and adds a short section to `~/.pi/agent/AGENTS.md` on when to use each tool. Reinstalling replaces both; delete them to uninstall. In any git repo, the extension:

- adds `def`, `refs`, `search_code` and `why` as ordinary pi tools, so they sit in the model's tool list with their descriptions. (An MCP server would work too, but behind a gateway such as pi-mcp-adapter the model has to remember the server exists; in practice it didn't.)
- updates the index in the background when a session starts, so the first query doesn't wait for a rebuild
- rewrites test, build, lint and typecheck commands (`yarn test`, `npx vitest run`, `tsc`, `eslint`, `cargo test`, `go test`, `pytest`, …) to `archive run -- …`. Each `&&` step is wrapped separately. Commands with pipes, redirects, `;` or watch/dev flags are left alone, and `ARCHIVE_RAW=1` opts out
- after a grep whose pattern is shaped like a symbol (`useOrganizations`, `has_consent`), adds one line pointing at `def` and `refs`, at most three times a session. Greps after a pipe or with `-v` are filters, not searches, and get no hint

The extension calls the binary it was installed from, so rerun `archive install pi` if you move it.

### Claude Code, Cursor

Add to `.mcp.json`:

```json
{ "mcpServers": { "archive": { "command": "archive", "args": ["mcp", "--dir", "."] } } }
```

Tools exposed: `search`, `def`, `refs`, `why`, `reindex`, plus `trace` when built with `--features honeybadger`.

### History

```
$ archive why apps/forumspace/src/features/consent/guard.ts:40-55
717f21886 2026-09-28 Martin Moro  lines 54-55
  [[sc-22184]] fix(forumspace): forward existing KAs from the consent page ...
  PR #1505: 2026/09/29 - Frontend Release Doc
8ac63c669 2026-08-03 Martin Moro  lines 40-41, 44-53
  feat(forumspace): Step 1.5 matching-consent gate with route enforcement and seeded toggle
  PR #1258: [[sc-21477]] KA Accounts — edit a profile item in place, ...
```

`why` runs `git blame` on a file or line range and groups the lines by commit, newest first, with up to six lines of each commit message's body. The PR is read from the subject for squash merges (`(#123)`), or else from the oldest GitHub `Merge pull request` commit leading to `HEAD` or `origin/HEAD`, whose body holds the PR title. That works for repos that commit straight to a staging branch too. Everything comes from local git, so no network and no token; GitLab-style merges are not recognised. About 50 ms per call.

### GraphQL schemas

`def` also reads `.graphql`/`.gql` files. A type, input or enum comes back as its whole block with descriptions; a query, mutation or subscription comes back as one line with its arguments:

```
$ archive def personUpdatePassword
apps/my-circles/schema.graphql:852  [graphql mutation] personUpdatePassword(input: PersonUpdatePasswordInput!): PersonUpdatePasswordPayload!
```

That is about 1 KB instead of a 300 KB schema file. When several apps carry the same definition, it is shown once with `same in:` listing the other copies, so differing copies stand out. Root types renamed in a `schema { }` block are followed. The parser expects printed SDL (definitions at column 0, closing `}` at column 0), which is what introspection dumps and codegen produce; hand-written schemas laid out differently may be missed.

### Dependency types

List packages in `.archivedeps` at the repo root, one per line, and `def` also returns their type declarations, with the version actually installed:

```
$ cat .archivedeps
@apollo/client
zustand

$ archive def create
node_modules/zustand/react.d.ts:13  [variable · zustand@5.0.12] create: Create
apps/my-circles/node_modules/zustand/react.d.ts:13  [variable · zustand@5.0.11] create: Create
```

Every `node_modules` next to a `package.json` is checked, so an app pinned to a different version shows up separately. Only `.d.ts` files are read, for `def`; they are left out of `search` and `refs` so library code does not outrank yours. Repo symbols sort before dependency symbols. An install or upgrade triggers a full rebuild. On a 4k-file monorepo with @apollo/client and zustand listed, a full rebuild takes about 3.7 s on 6 cores.

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

## Honeybadger errors (optional)

Off by default. Build it in with:

```sh
cargo install --git https://github.com/lithial/the-archive --features honeybadger
```

`archive trace` takes a Honeybadger notice as JSON (a file, stdin, or the `notice` argument of the `trace` MCP tool) and says where it came from:

```
$ archive trace notice.json
GraphQL error while fetching organizations and circles
  production · 2026-10-13T14:37:30Z · https://my.circl.es/en · https://app.honeybadger.io/…
  marker: useOrganizations
  nested: ApolloError: unauthorized (×49)
  deployed revision 8a1ca6a95 is 86 commits behind HEAD

message found:
  apps/my-circles/src/queries/useOrganizations.ts:49  logger.error("GraphQL error while fetching organizations and circles", { error });
    changed since deploy (+13 -7); archive why apps/my-circles/src/queries/useOrganizations.ts:49
  + 3 in tests
marker "useOrganizations" set at:
  apps/my-circles/src/queries/useOrganizations.ts:14  const { logger } = useLogger("useOrganizations");
    changed since deploy (+13 -7); archive why apps/my-circles/src/queries/useOrganizations.ts:14
frames (minified, no source maps):
  0qa8_35mttvm7.js:1:51918 a
  …
```

It finds the message with `git grep` (or its longest literal fragment when the message has ids or counts in it), and the context `marker` as a quoted string. It compares those files between the notice's `revision` and `HEAD`. Matches only in tests don't count, so a library error such as Apollo's `Response not successful` is reported as coming from a dependency.

archive never talks to Honeybadger. The agent fetches the notice through the Honeybadger MCP (`list_fault_notices`, limit 1), which already has your OAuth login, so archive holds no token and makes no network calls. Frames stay minified: Honeybadger's API returns only the raw backtrace, and it has no endpoint to download uploaded source maps.

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
- The index lives in `~/.cache/archive/<path>.db`. Every `search`/`def`/`refs`/MCP call first compares each file's mtime and size with what was indexed, then reindexes only the files that changed and drops deleted ones. On a 4k-file repo the check is ~60 ms and a query right after editing a file is 100–200 ms. A branch switch touching more than 300 files, or a change to `.archiveignore`/`.archivedeps`, triggers a full rebuild instead (~3.5 s; files are parsed on every core). `archive index` forces a full rebuild.

## What it doesn't do

- No embeddings. Queries are keyword matches, so "how does X work" needs words that appear in the code.
- No daemon, no file watcher, no HTTP API.
- No telemetry, feedback, update checks or any outbound connection. The dependency list is the whole story: clap, rusqlite, serde_json, ignore, tree-sitter.

## License

MIT
