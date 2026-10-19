// archive for pi. Installed by `archive install pi`; delete this file to uninstall.
// In a git repo it:
//   - registers the archive MCP server (search, def, refs, why) for the repo root
//   - warms the index in the background at session start
//   - rewrites test/build/lint/typecheck commands to `archive run -- ...` (ARCHIVE_RAW=1 opts out)
//   - after a grep for an identifier, adds a one-line hint pointing at def/refs
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
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
    if (existsSync(join(d, ".git"))) return d;
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

/** Identifier-shaped arguments of a grep/rg command. */
export function grepSymbols(command: string): string[] {
  if (!/(^|&&\s*|\s)(rg|grep|git\s+grep)\s/.test(command)) return [];
  const words = command.match(/"[^"]*"|'[^']*'|\S+/g) ?? [];
  return words
    .map((w) => w.replace(/^['"]|['"]$/g, "").replace(/^\\b|\\b$/g, ""))
    .filter((w) => IDENTIFIER.test(w) && SYMBOL_SHAPE.test(w));
}

export default function (pi: ExtensionAPI) {
  let root: string | undefined;
  let hinted = new Set<string>();

  pi.on("session_start", async (_event, ctx) => {
    hinted = new Set();
    const next = repoRoot(ctx.cwd);
    if (next === root) return;
    // Registering an already-registered name replaces it, so a switch to another repo just moves it.
    root = next;
    if (!root) return;
    pi.registerMcpServer("archive", {
      command: BIN,
      args: ["mcp", "--dir", root],
      description: "Local code index: search passages, def/refs for symbols, why (git history) for lines.",
    });
    // Build or update the index now so the first query doesn't wait for it.
    spawn(BIN, ["index", root, "--update"], { detached: true, stdio: "ignore" }).on("error", () => {}).unref();
  });

  pi.on("tool_call", async (event) => {
    if (!root || event.toolName !== "bash") return undefined;
    const input = event.input as { command?: string };
    const wrapped = wrap(String(input.command ?? ""));
    if (wrapped) input.command = wrapped;
    return undefined;
  });

  pi.on("tool_result", async (event) => {
    if (!root || hinted.size >= 3) return undefined;
    const input = event.input as { command?: string; pattern?: string };
    const names =
      event.toolName === "bash"
        ? grepSymbols(String(input.command ?? ""))
        : event.toolName === "grep" && IDENTIFIER.test(String(input.pattern)) && SYMBOL_SHAPE.test(String(input.pattern))
          ? [String(input.pattern)]
          : [];
    const name = names.find((n) => !hinted.has(n));
    if (!name) return undefined;
    hinted.add(name);
    const text = `[archive] For a symbol, the archive tools are more exact than grep: def ${name} gives the definition, refs ${name} every use with its enclosing function.`;
    return { content: [...event.content, { type: "text", text }] };
  });
}
