import test from "node:test";
import assert from "node:assert/strict";
import { filterAllowedEvents } from "../src/matcher.ts";

test("filters blocked ids while preserving order", () => {
  const events = [
    { id: "e1", score: 10 },
    { id: "b1", score: 20 },
    { id: "e2", score: 30 },
    { id: "b2", score: 40 },
  ];
  const out = filterAllowedEvents(events, ["b1", "b2"]);
  assert.deepEqual(out, [
    { id: "e1", score: 10 },
    { id: "e2", score: 30 },
  ]);
});

test("handles empty inputs without mutating", () => {
  const events = [{ id: "e1", score: 1 }];
  const blocked: string[] = [];
  assert.deepEqual(filterAllowedEvents(events, blocked), [{ id: "e1", score: 1 }]);
  assert.deepEqual(filterAllowedEvents([], ["b1"]), []);
});
