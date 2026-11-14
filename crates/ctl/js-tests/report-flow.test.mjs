// What a click on "Report" asks the server for. The card is drawn from the
// stored summary when the server still vouches for it (GET /api/reports/{id}
// answers 200 only then), and the analyzer is started only when it does not.
// Also here: how often the recordings folder is listed, since both are about
// the page not asking for work nobody needs.

import test from "node:test";
import assert from "node:assert/strict";
import { loadPanel, fixture, response, settle } from "./harness.mjs";

function state({ exits = 0, running = false } = {}) {
  return {
    self_pid: 1,
    now_unix_s: 1_800_000_000,
    config: { mtime_unix_s: 0, status: "loaded", seeded: false, path: "telemouse.toml" },
    recording: { enabled: true, dir: "recordings" },
    components: [
      { id: "report", kind: "task", label: "Analyze: report", bin: "telemouse-analyze", bin_found: true, takes_session: true,
        running, exits, last_exit: exits ? { code: 0, at_unix_s: 1_800_000_000 } : null, log: ["analyzer output line"], log_seq: 1, flags: [] },
    ],
    processes: [],
  };
}

/** A panel whose server answers from `routes(url, method)`; everything else is `{}`. */
function panel(routes) {
  const world = { st: state() };
  const fetch = (url, init) => {
    const method = (init && init.method) || "GET";
    const own = routes(url, method, world);
    if (own) return Promise.resolve(own);
    if (url.startsWith("/api/state")) return Promise.resolve(response(world.st));
    if (url === "/api/sessions") return Promise.resolve(response(["s-1.jsonl"]));
    if (url === "/api/config") return new Promise(() => {});
    return Promise.resolve(response({}));
  };
  return { world, ...loadPanel({ fetch }) };
}

const sent = (requests, method, url) => requests.filter((r) => r.method === method && r.url === url).length;

test("a recording whose summary is stored is shown without starting the analyzer", async () => {
  const summary = fixture("report-summary.json");
  const { ctl, requests, elements } = panel((url, method) =>
    url === "/api/reports/s-1" && method === "GET" ? response(summary) : null);
  await settle();
  await ctl.showReport("s-1.jsonl");
  await settle();
  assert.equal(sent(requests, "GET", "/api/reports/s-1"), 1);
  assert.equal(requests.filter((r) => r.method === "POST").length, 0, "nothing was run: " + JSON.stringify(requests));
  assert.equal(ctl.report.loading, false);
  assert.equal(ctl.report.text, false, "the task's old output is not this recording's text");
  assert.ok(elements.get("rcBody").innerHTML.includes("Your aim, in plain numbers"));
  assert.ok(elements.get("rcRaw").textContent.includes("nothing was run again"));
  assert.ok(!elements.get("rcRaw").textContent.includes("analyzer output line"));
});

test("a recording with no current summary gets the text run, then the summary run", async () => {
  const summary = fixture("report-summary.json");
  const { ctl, requests, world, elements } = panel((url, method, w) => {
    if (url === "/api/reports/s-1" && method === "GET") return response({ error: "no summary yet" }, 404);
    if (url === "/api/reports/s-1" && method === "POST") return response(summary);
    if (url === "/api/components/report/start") { w.st = state({ running: true }); return response({ pid: 9 }); }
    return null;
  });
  await settle();
  await ctl.showReport("s-1.jsonl");
  await settle();
  assert.equal(sent(requests, "POST", "/api/components/report/start"), 1, "the analyzer was started once");
  assert.equal(sent(requests, "POST", "/api/reports/s-1"), 0, "not before the text run has ended");
  assert.equal(ctl.report.loading, true);
  assert.equal(ctl.report.text, true);

  // The task exits cleanly; the next poll sees it.
  world.st = state({ exits: 1 });
  await ctl.poll();
  await settle();
  assert.equal(sent(requests, "GET", "/api/reports/s-1"), 2, "asked again before running anything");
  assert.equal(sent(requests, "POST", "/api/reports/s-1"), 1, "one summary run, since none was stored");
  assert.equal(ctl.report.loading, false);
  assert.ok(elements.get("rcBody").innerHTML.includes("Your aim, in plain numbers"));
  assert.ok(elements.get("rcRaw").textContent.includes("analyzer output line"), "the run's own text is under Details");
});

test("an explicit Run always runs, and reuses a summary the server still vouches for", async () => {
  const summary = fixture("report-summary.json");
  const { ctl, requests, world } = panel((url, method, w) => {
    if (url === "/api/reports/s-1" && method === "GET") return response(summary);
    if (url === "/api/components/report/start") { w.st = state({ running: true }); return response({ pid: 9 }); }
    return null;
  });
  await settle();
  await ctl.runTool("report");
  await settle();
  assert.equal(sent(requests, "POST", "/api/components/report/start"), 1);
  world.st = state({ exits: 1 });
  await ctl.poll();
  await settle();
  assert.equal(sent(requests, "GET", "/api/reports/s-1"), 1);
  assert.equal(sent(requests, "POST", "/api/reports/s-1"), 0, "the stored summary stood in for a second analyzer run");
  assert.equal(ctl.report.loading, false);
});

test("a failed lookup is not a card: the run decides", async () => {
  const { ctl, requests } = panel((url, method) =>
    url === "/api/reports/s-1" && method === "GET" ? response({ error: "boom" }, 500) : null);
  await settle();
  await ctl.showReport("s-1.jsonl");
  await settle();
  assert.equal(sent(requests, "POST", "/api/components/report/start"), 1);
});

test("the recordings are listed while something may have changed, then only now and then", async () => {
  const { ctl, requests, world } = panel(() => null);
  await settle();
  const listed = () => sent(requests, "GET", "/api/sessions");
  assert.equal(listed(), 1, "the first poll lists");
  for (let i = 0; i < 4; i++) { await ctl.poll(); await settle(); }
  assert.equal(listed(), 5, "and so do the next few, while a new file may still appear");
  for (let i = 0; i < 14; i++) { await ctl.poll(); await settle(); }
  assert.equal(listed(), 5, "an unchanged panel stops asking");
  await ctl.poll(); await settle();
  assert.equal(listed(), 6, "but still looks once in a while");
  // Something started or stopped: the very poll that sees it lists again.
  world.st = state({ exits: 1 });
  await ctl.poll(); await settle();
  assert.equal(listed(), 7);
});
