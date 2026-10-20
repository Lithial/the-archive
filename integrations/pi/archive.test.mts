import assert from "node:assert/strict";
process.env.ARCHIVE_BIN = "/x/archive";
const { wrap, grepSymbol } = await import("./archive.ts");
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
assert.equal(grepSymbol(`rg -n "useOrganizations" src`), "useOrganizations");
assert.equal(grepSymbol(`grep -rn 'error' .`), undefined);
assert.equal(grepSymbol(`cd x && rg '\\bhas_matching_consent\\b'`), "has_matching_consent");
assert.equal(grepSymbol(`cat useOrganizations.ts`), undefined);
assert.equal(grepSymbol(`fd -H -t f 'promote.sh' . -E node_modules | grep -v node_modules`), undefined, "flag values and -v are not the pattern");
assert.equal(grepSymbol(`rg -n "attentionRequired" apps/circle-spaces-v3/src | head`), "attentionRequired");
assert.equal(grepSymbol(`rg -l "RoomBackground" apps packages --glob '*.tsx' | grep -v stories`), "RoomBackground");
assert.equal(grepSymbol(`rg -n -A30 "room-background-breathing" packages/ui/src/`), undefined, "css class, not a symbol");
console.log("ok");
