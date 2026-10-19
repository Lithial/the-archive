import assert from "node:assert/strict";
process.env.ARCHIVE_BIN = "/x/archive";
const { wrap, grepSymbols } = await import("./archive.ts");
const cases: [string, string | undefined][] = [
  ["yarn test", "'/x/archive' run -- yarn test"],
  ["cd apps/my-circles && npx vitest run src/a.test.ts 2>&1", "cd apps/my-circles && '/x/archive' run -- npx vitest run src/a.test.ts"],
  ["CI=1 pnpm run typecheck", "CI=1 '/x/archive' run -- pnpm run typecheck"],
  ["cargo test && cargo clippy", "'/x/archive' run -- cargo test && '/x/archive' run -- cargo clippy"],
  ["yarn test 2>&1 | tail -40", undefined],
  ["tsc --watch", undefined],
  ["yarn dev", undefined],
  ["ARCHIVE_RAW=1 yarn test", undefined],
  ["ls && git status", undefined],
  ["yarn test > out.txt", undefined],
  ["yarn testing-tool", undefined],
];
for (const [i, o] of cases) assert.equal(wrap(i), o, i);
assert.deepEqual(grepSymbols(`rg -n "useOrganizations" src`), ["useOrganizations"]);
assert.deepEqual(grepSymbols(`grep -rn 'error' .`), []);
assert.deepEqual(grepSymbols(`cd x && rg '\\bhas_matching_consent\\b'`), ["has_matching_consent"]);
assert.deepEqual(grepSymbols(`cat useOrganizations.ts`), []);
console.log("ok");
