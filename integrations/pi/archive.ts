// archive for pi. Installed by `archive install pi`; delete this file to uninstall.
// In a git repo it:
//   - adds the tools def, refs, why and search_code, backed by the archive binary
//   - warms the index in the background at session start
//   - rewrites test/build/lint/typecheck commands to `archive run -- ...` (ARCHIVE_RAW=1 opts out)
//   - after a grep for an identifier, adds a one-line hint pointing at def/refs
import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";

const BIN = process.env.ARCHIVE_BIN || "__ARCHIVE_BIN__";

// Leading env assignments, an optional package-runner prefix, then a tool whose output is long and mostly noise.
const RUNNER =
  /^((?:[A-Za-z_]\w*=\S*\s+)*)((?:(?:npx|bunx)\s+|(?:pnpm|yarn)\s+exec\s+)?(?:vitest|jest|tsc|eslint|playwright\s+test|pytest|cargo\s+(?:test|build|check|clippy)|go\s+(?:test|build|vet)|(?:yarn|pnpm|npm|bun)(?:\s+run)?\s+(?:test|build|lint|typecheck|type-check|check))\b.*)$/;
// Pipes, redirects to files, `;`, `||`, subshells: the agent is shaping output itself, so leave it alone.
const SHELLY = /[|;<>`]|\$\(|&(?!&)|\n/;
const WATCH = /(^|\s)(--watch|-w|watch|dev|serve)(\s|$)/;
const IDENTIFIER = /^[A-Za-z_$][\w$]{3,}$/;
// camelCase, PascalCase with a second capital, or snake_case: plain words like "error" are not symbols
const SYMBOL_SHAPE = /[a-z][A-Z]|^[A-Z][a-z0-9]+[A-Z]|[a-zA-Z]_[a-zA-Z]/;

function repoRoot(dir: string): string | undefined {
  for (let d = dir; ; d = dirname(d)) {
    if (existsSync(join(d, ".git"))) return d; // a file in worktrees, a directory otherwise
    if (dirname(d) === d) return undefined;
  }
}

const quote = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;

/** The command with each `&&`-separated test/build step wrapped, or undefined to leave it as is. */
export function wrap(command: string): string | undefined {
  if (command.includes("ARCHIVE_RAW=1") || command.includes(" run -- ")) return undefined;
  // `2>&1` is redundant under archive run, which merges both streams
  const cmd = command.replace(/\s+2>&1\s*$/, "");
  if (SHELLY.test(cmd.replace(/&&/g, ""))) return undefined;
  let changed = false;
  const steps = cmd.split(/\s*&&\s*/).map((step) => {
    const m = RUNNER.exec(step.trim());
    if (!m || WATCH.test(m[2])) return step;
    changed = true;
    return `${m[1]}${quote(BIN)} run -- ${m[2]}`;
  });
  return changed ? steps.join(" && ") : undefined;
}

/** The pattern of the first grep/rg in a command, when it is shaped like a symbol. */
export function grepSymbol(command: string): string | undefined {
  // A grep after a pipe filters another command's output; it is not a code search.
  const m = /(?:^|&&\s*|;\s*)(?:rg|grep|git\s+grep)\s+(.*)/.exec(command);
  if (!m) return undefined;
  const words = m[1].split(/\s*\|\s*/)[0].match(/"[^"]*"|'[^']*'|\S+/g) ?? [];
  if (words.includes("-v")) return undefined;
  // The pattern is the first word that is not a flag or a flag's value.
  const takesValue = /^-(e|g|t|T|E|A|B|C|m|f|r|d|include|exclude|glob|type|type-not|max-depth|regexp|file|context|after-context|before-context|max-count|threads|color|colour|replace|sort|iglob|pre)$/;
  for (let i = 0; i < words.length; i++) {
    if (words[i].startsWith("-")) {
      if (takesValue.test(words[i])) i++;
      continue;
    }
    const w = words[i].replace(/^['"]|['"]$/g, "").replace(/^\\b|\\b$/g, "");
    return IDENTIFIER.test(w) && SYMBOL_SHAPE.test(w) ? w : undefined;
  }
  return undefined;
}

async function run(pi: ExtensionAPI, cwd: string, args: string[]) {
  const root = repoRoot(cwd);
  if (!root) throw new Error("not inside a git repository");
  const r = await pi.exec(BIN, [...args, "--dir", root], { cwd: root, timeout: 120_000 });
  // archive prints "index was stale, rebuilt" on stderr; only surface stderr on failure
  if (r.code !== 0) throw new Error(r.stderr.trim() || `archive exited ${r.code}`);
  return { content: [{ type: "text" as const, text: r.stdout.trim() || "(no results)" }], details: undefined };
}

export default function (pi: ExtensionAPI) {
  let hinted = new Set<string>();

  const tool = (name: string, label: string, description: string, args: (p: any) => string[], parameters: any) =>
    pi.registerTool(
      defineTool({
        name,
        label,
        description,
        parameters,
        execute: (_id, p, _signal, _update, ctx) => run(pi, ctx.cwd, args(p)),
      }),
    );

  const limit = Type.Optional(Type.Integer({ description: "Max results" }));
  tool(
    "def",
    "Definition",
    "Where a symbol is defined: path:line, kind and signature, from the syntax tree. Covers TS/JS, GDScript, GraphQL types and the .d.ts of dependencies listed in .archivedeps. Substring match. Use instead of grep when you know the name.",
    (p) => ["def", p.name],
    Type.Object({ name: Type.String({ description: "Symbol name" }) }),
  );
  tool(
    "refs",
    "References",
    "Every use of a symbol: calls, new, JSX, type references, object keys, with the enclosing function. Exact name, from the syntax tree, so comments and strings are skipped. Run it before renaming something or changing a signature.",
    (p) => ["refs", p.name, ...(p.limit ? ["--limit", String(p.limit)] : [])],
    Type.Object({ name: Type.String({ description: "Symbol name" }), limit }),
  );
  tool(
    "search_code",
    "Code search",
    "Ranked passages (BM25) across the repo as path:start-end plus text. For a concept or behaviour when you don't know the symbol name. Cheaper than grep plus reading whole files.",
    (p) => ["search", p.query, ...(p.limit ? ["--limit", String(p.limit)] : [])],
    Type.Object({ query: Type.String({ description: "Natural-language or keyword query" }), limit }),
  );
  tool(
    "why",
    "History",
    "Why lines are the way they are: the commits that last touched them, newest first, with message body and the PR they were merged in. Use before changing code whose intent is unclear.",
    (p) => ["why", p.target],
    Type.Object({ target: Type.String({ description: "path, path:line or path:start-end" }) }),
  );

  pi.on("session_start", async (_event, ctx) => {
    hinted = new Set();
    const root = repoRoot(ctx.cwd);
    if (!root) return;
    // Build or update the index now so the first query doesn't wait for it.
    spawn(BIN, ["index", root, "--update"], { detached: true, stdio: "ignore" }).on("error", () => {}).unref();
  });

  pi.on("tool_call", async (event, ctx) => {
    if (event.toolName !== "bash" || !repoRoot(ctx.cwd)) return undefined;
    const input = event.input as { command?: string };
    const wrapped = wrap(String(input.command ?? ""));
    if (wrapped) input.command = wrapped;
    return undefined;
  });

  pi.on("tool_result", async (event, ctx) => {
    if (hinted.size >= 3 || !repoRoot(ctx.cwd)) return undefined;
    const input = event.input as { command?: string; pattern?: string };
    const name =
      event.toolName === "bash"
        ? grepSymbol(String(input.command ?? ""))
        : event.toolName === "grep" && IDENTIFIER.test(String(input.pattern)) && SYMBOL_SHAPE.test(String(input.pattern))
          ? String(input.pattern)
          : undefined;
    if (!name || hinted.has(name)) return undefined;
    hinted.add(name);
    const text = `[archive] ${name} looks like a symbol. The def and refs tools are exact and cheaper than grep: def {name: "${name}"} for the definition, refs {name: "${name}"} for every use with its enclosing function.`;
    return { content: [...event.content, { type: "text", text }] };
  });
}
