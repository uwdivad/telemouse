// The page script loads in a bare context, publishes its test handle, and
// every element it looks up by id exists in index.html.

import test from "node:test";
import assert from "node:assert/strict";
import { loadPanel, PANEL_JS, INDEX_HTML } from "./harness.mjs";

test("panel.js runs to its last line and publishes window.telemouseCtl", () => {
  const { ctl, requests } = loadPanel();
  for (const name of ["esc", "renderReport", "poll", "mergeLogs", "logCursor"]) {
    assert.equal(typeof ctl[name], "function", name);
  }
  // Awake on load: the first poll went out at once.
  assert.ok(requests.some((r) => r.url === "/api/state"), JSON.stringify(requests));
});

test("a page loaded hidden does nothing until it is looked at", () => {
  const { requests, timers } = loadPanel({ hidden: true });
  assert.equal(requests.length, 0);
  assert.equal(timers.length, 0);
});

test("every $(id) the script uses is an element of index.html", () => {
  const ids = new Set([...INDEX_HTML.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1]));
  const used = new Set([...PANEL_JS.matchAll(/\$\("([A-Za-z0-9_]+)"\)/g)].map((m) => m[1]));
  // Made by the script itself (the "no games yet" row).
  ids.add("sNoGames");
  const missing = [...used].filter((id) => !ids.has(id));
  assert.deepEqual(missing, [], "ids the page does not have");
  // Built from a prefix: wizStep0..2.
  for (let i = 0; i < 3; i++) assert.ok(ids.has("wizStep" + i));
});

test("the inline script tag the server replaces is in the shell exactly once", () => {
  assert.equal(INDEX_HTML.split('<script src="panel.js"></script>').length - 1, 1);
  assert.ok(!PANEL_JS.includes("</script"), "would end the inline script early");
});

test("the constants the panel owns come from the page config", () => {
  const { ctl } = loadPanel({ config: { log_lines: 3 } });
  // LOG_LINES drives how much mergeLogs keeps.
  const c = [{ id: "capture", log: ["a", "b", "c", "d", "e"], log_seq: 5 }];
  ctl.mergeLogs(c);
  assert.deepEqual([...c[0].log], ["c", "d", "e"]);
});
