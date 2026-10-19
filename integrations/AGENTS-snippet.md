## Code search: archive

In a git repo, use the archive tools (MCP server `archive`, or the `archive` CLI) before grep or reading whole files:

- `def NAME`: where a symbol is defined, with its signature. Also covers GraphQL types and the `.d.ts` of dependencies listed in `.archivedeps`.
- `refs NAME`: every use, with the enclosing function. Run it before renaming something or changing a signature.
- `search "question"`: ranked passages for a concept when you don't know the name.
- `why path:start-end`: the commits and PRs behind those lines. Use it before changing code whose intent is unclear.

Test, build, lint and typecheck commands are rewritten to `archive run -- <command>`, which prints errors, assertion diffs and the summary, and saves the full output. Its last line gives the log path; read that if you need more. Prefix a command with `ARCHIVE_RAW=1` to get the raw output instead.
