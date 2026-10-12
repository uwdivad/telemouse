// The poll loop is single-flight: however many callers ask while a poll is
// in flight, one request pair is in the air, one more round runs for them,
// and the 2 s chain is scheduled once.

import test from "node:test";
import assert from "node:assert/strict";
import { loadPanel, deferred, response, settle } from "./harness.mjs";

function state(components = []) {
  return {
    self_pid: 1,
    now_unix_s: 1_800_000_000,
    config: { mtime_unix_s: 0, status: "loaded", seeded: false, path: "telemouse.toml" },
    recording: { enabled: true, dir: "recordings" },
    components,
    processes: [],
  };
}

/** A fetch whose /api/state answers are handed out by the test, in order. */
function controlledFetch() {
  const pending = [];
  const fetch = (url) => {
    if (url.startsWith("/api/state")) {
      const d = deferred();
      pending.push({ url, d });
      return d.promise;
    }
    if (url === "/api/sessions") return Promise.resolve(response([]));
    if (url === "/api/config") return new Promise(() => {});
    return Promise.resolve(response({}));
  };
  return { fetch, pending };
}

test("a poll asked for while one is in flight does not start a second", async () => {
  const { fetch, pending } = controlledFetch();
  const { ctl, timers } = loadPanel({ fetch });
  // Load started the first poll.
  assert.equal(pending.length, 1);
  // Three more callers (an action finishing, the guide, a wake-up).
  const a = ctl.poll(), b = ctl.poll(), c = ctl.poll();
  await settle();
  assert.equal(pending.length, 1, "still one request in the air");
  assert.ok(a === b && b === c, "every caller waits on the same run");

  pending[0].d.resolve(response(state()));
  await settle();
  // Asked for during the first round: exactly one more, for them.
  assert.equal(pending.length, 2, "one follow-up round");
  assert.equal(timers.filter((t) => t.ms === 2000).length, 0, "no timer while a round runs");

  pending[1].d.resolve(response(state()));
  await a;
  await settle();
  assert.equal(pending.length, 2, "no third round nobody asked for");
  assert.equal(timers.filter((t) => t.ms === 2000).length, 1, "the chain is scheduled once");
});

test("the chain's own timer starts the next poll, and a failed one keeps it going", async () => {
  const { fetch, pending } = controlledFetch();
  const { timers } = loadPanel({ fetch });
  pending[0].d.reject(new Error("connection refused"));
  await settle();
  const chain = timers.filter((t) => t.ms === 2000);
  assert.equal(chain.length, 1);
  chain[0].fn();
  await settle();
  assert.equal(pending.length, 2);
});

test("the log cursor goes out with the next poll, one per component", async () => {
  const { fetch, pending } = controlledFetch();
  const { ctl } = loadPanel({ fetch });
  pending[0].d.resolve(response(state([
    { id: "capture", kind: "service", log: ["one", "two"], log_seq: 2, flags: [], running: false, bin_found: true },
    { id: "viz", kind: "service", log: [], log_seq: 0, flags: [], running: false, bin_found: true },
  ])));
  await settle();
  assert.equal(ctl.logCursor(), "capture:2,viz:0");
  ctl.poll();
  await settle();
  assert.equal(pending.length, 2);
  const last = pending[pending.length - 1].url;
  assert.equal(decodeURIComponent(last), "/api/state?log_since=capture:2,viz:0");
});
