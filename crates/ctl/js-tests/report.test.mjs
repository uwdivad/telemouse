// esc() and the report card. The card is fed a ReportSummary fixture whose
// keys are the analyzer's (a Rust test in crates/ctl checks the fixture
// against crates/analyze's structs), wrapped so that reading a field the
// summary does not have fails the test instead of rendering "undefined".

import test from "node:test";
import assert from "node:assert/strict";
import { loadPanel, fixture } from "./harness.mjs";

test("esc() escapes the five HTML-special characters and nothing else", () => {
  const { ctl } = loadPanel();
  assert.equal(ctl.esc(`<a href="x">Tom & 'Jerry'</a>`),
    "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39;&lt;/a&gt;");
  assert.equal(ctl.esc("plain text 123 — ü"), "plain text 123 — ü");
  assert.equal(ctl.esc(42), "42");
  assert.equal(ctl.esc(null), "null");
});

/** `v`, where reading an own property that is not there throws. */
function strict(v, where = "summary") {
  if (v === null || typeof v !== "object") return v;
  return new Proxy(v, {
    get(t, key) {
      if (typeof key === "symbol" || key in t) return strict(t[key], where + "." + String(key));
      throw new Error("renderReport read " + where + "." + String(key) + ", which a ReportSummary does not have");
    },
  });
}

test("renderReport reads only fields a ReportSummary has", () => {
  const { ctl } = loadPanel();
  const html = ctl.renderReport(strict(fixture("report-summary.json")));
  assert.ok(html.includes("Your aim, in plain numbers"));
  assert.ok(html.includes("1,000 Hz") || html.includes("1000 Hz"), "report rate");
  assert.ok(html.includes("12 min 34 s"), "length in words");
});

test("renderReport escapes what came from the recording", () => {
  const { ctl } = loadPanel();
  const html = ctl.renderReport(fixture("report-summary.json"));
  // A marker label is the user's text.
  assert.ok(html.includes("sens A &lt;b&gt;"), html);
  assert.ok(!html.includes("sens A <b>"));
});

test("a sink's losses are worded by core's sink names", () => {
  const { ctl } = loadPanel();
  const s = fixture("report-summary.json");
  s.quality.sink_losses = [["kafka", 4], ["udp", 1], ["jsonl", 2]];
  const html = ctl.renderReport(s);
  assert.ok(html.includes("4 batches were not delivered to Kafka"), html);
  assert.ok(html.includes("1 batch was not delivered to the live view"), html);
  // A recording-file loss is data loss: the verdict says so.
  assert.ok(html.includes('class="verdict bad"'), html);
  s.quality.sink_losses = [["kafka", 4]];
  assert.ok(ctl.renderReport(s).includes('class="verdict warn"'), "a Kafka loss alone is a warning");
});

test("an empty recording says so instead of drawing zeros", () => {
  const { ctl } = loadPanel();
  const s = fixture("report-summary.json");
  s.session.event_count = 0;
  const html = ctl.renderReport(strict(s));
  assert.ok(html.includes("No mouse movement was recorded"));
});

test("a game without a sensitivity offers to add it", () => {
  const { ctl } = loadPanel();
  const s = fixture("report-summary.json");
  s.session.aim_profile_missing = true;
  s.session.game = "Other<Game>.exe";
  const html = ctl.renderReport(strict(s));
  assert.ok(html.includes('data-add-game="Other&lt;Game&gt;.exe"'), html);
});
