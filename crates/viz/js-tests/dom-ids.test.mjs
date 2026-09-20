/**
 * Every element id app.js looks up exists in index.html.
 *
 * The engine tests run app.js against a stub DOM whose getElementById
 * creates any element on demand (harness.mjs), so a lookup of an id the
 * page does not have passes there and throws in a browser — which is how a
 * missing stats tile once broke the stats bar ten times a second without a
 * failing test. This test reads both files as text: `$("id")`,
 * `getElementById("id")`, and the string list the stats bar caches in one
 * loop, against every `id="…"` in the markup. The dashboard and the OBS
 * overlay are the same file, so one check covers both.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const APP_JS = readFileSync(join(here, "..", "src", "app.js"), "utf8");
const INDEX_HTML = readFileSync(join(here, "..", "src", "index.html"), "utf8");

/** Ids the script asks the document for, by literal. */
export function idsLookedUp(js) {
  const ids = new Set();
  for (const m of js.matchAll(/\$\("([A-Za-z0-9_-]+)"\)/g)) ids.add(m[1]);
  for (const m of js.matchAll(/getElementById\("([A-Za-z0-9_-]+)"\)/g)) ids.add(m[1]);
  // The stats bar's cache: `for (const id of [ "a", "b", … ]) SE[id] = $(id);`
  for (const block of js.matchAll(/for \(const id of \[([^\]]*)\]\) SE\[id\] = \$\(id\)/g)) {
    for (const m of block[1].matchAll(/"([A-Za-z0-9_-]+)"/g)) ids.add(m[1]);
  }
  return ids;
}

/** Ids the markup declares. */
export function idsDeclared(html) {
  const ids = new Set();
  for (const m of html.matchAll(/\sid="([^"]+)"/g)) ids.add(m[1]);
  return ids;
}

test("every id app.js looks up exists in index.html", () => {
  const wanted = idsLookedUp(APP_JS);
  const have = idsDeclared(INDEX_HTML);
  assert.ok(wanted.size > 30, `found only ${wanted.size} lookups; the regexes no longer match app.js`);
  const missing = [...wanted].filter((id) => !have.has(id)).sort();
  assert.deepEqual(missing, [], `ids app.js uses that index.html lacks: ${missing.join(", ")}`);
});

test("the stats-bar cache list is found by the test", () => {
  // If the loop is ever rewritten, this test must be updated too, or the
  // cache list silently drops out of the check above.
  assert.ok(idsLookedUp(APP_JS).has("sSpeedCm"));
});
