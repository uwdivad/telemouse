// What ?log_since= sends is appended to a per-component tail, bounded to the
// lines the page shows, and a gap or a restarted panel starts over.

import test from "node:test";
import assert from "node:assert/strict";
import { loadPanel } from "./harness.mjs";

const comp = (log, log_seq, id = "capture") => ({ id, log, log_seq });

test("new lines follow on from the held tail", () => {
  const { ctl } = loadPanel();
  const first = [comp(["a", "b"], 2)];
  ctl.mergeLogs(first);
  assert.deepEqual([...first[0].log], ["a", "b"]);
  const next = [comp(["c"], 3)];
  ctl.mergeLogs(next);
  assert.deepEqual([...next[0].log], ["a", "b", "c"]);
  const quiet = [comp([], 3)];
  ctl.mergeLogs(quiet);
  assert.deepEqual([...quiet[0].log], ["a", "b", "c"], "nothing new keeps the tail");
  assert.equal(ctl.logCursor(), "capture:3");
});

test("the tail is bounded to log_lines", () => {
  const { ctl } = loadPanel({ config: { log_lines: 4 } });
  ctl.mergeLogs([comp(["1", "2", "3"], 3)]);
  const more = [comp(["4", "5", "6"], 6)];
  ctl.mergeLogs(more);
  assert.deepEqual([...more[0].log], ["3", "4", "5", "6"]);
});

test("a client that fell behind the server's ring starts over from what came", () => {
  const { ctl } = loadPanel();
  ctl.mergeLogs([comp(["a"], 1)]);
  // 500 lines arrived, the server sent its last 2: lines 499 and 500.
  const jump = [comp(["y", "z"], 501)];
  ctl.mergeLogs(jump);
  assert.deepEqual([...jump[0].log], ["y", "z"], "no stale lines glued to a gap");
});

test("a cursor from before a panel restart is dropped", () => {
  const { ctl } = loadPanel();
  ctl.mergeLogs([comp(["a", "b", "c"], 3)]);
  ctl.mergeLogs([comp([], 1)]);
  assert.equal(ctl.logCursor(), "", "the next poll asks for the whole tail");
});

test("components keep separate cursors", () => {
  const { ctl } = loadPanel();
  ctl.mergeLogs([comp(["a"], 1, "capture"), comp(["x", "y"], 2, "viz")]);
  assert.equal(ctl.logCursor(), "capture:1,viz:2");
});
