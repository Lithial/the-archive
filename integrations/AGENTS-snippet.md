## Code search: archive

In a git repo, use these tools before grep or reading whole files:

- `def`: where a symbol is defined, with its signature. Also covers GraphQL types and the `.d.ts` of dependencies listed in `.archivedeps`.
- `refs`: every use of a symbol, with the enclosing function. Run it before renaming something or changing a signature.
- `search_code`: ranked passages for a concept when you don't know the name.
- `why`: the commits and PRs behind a line range. Use it before changing code whose intent is unclear.

Run test, build, lint and typecheck commands plainly (`yarn test`, `npx vitest run x.test.ts`, `tsc`). They are rewritten to `archive run -- <command>`, which prints errors, assertion diffs and the summary, and saves the full output; its last line gives the log path. Don't redirect their output to a file and `tail` it: that keeps the noise and loses the errors. Prefix a command with `ARCHIVE_RAW=1` to get the raw output.
