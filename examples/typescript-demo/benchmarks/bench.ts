import { performance } from "node:perf_hooks";
import { filterAllowedEvents } from "../src/matcher.ts";
import type { EventItem } from "../src/matcher.ts";

const events: EventItem[] = Array.from({ length: 2000 }, (_, i) => ({
  id: i % 5 === 0 ? `blocked_${i % 600}` : `ok_${i}`,
  score: i,
}));
const blockedIds: string[] = Array.from({ length: 600 }, (_, i) => `blocked_${i}`);

// Warmup
filterAllowedEvents(events.slice(0, 100), blockedIds);

const iters = 60;
const latenciesMs: number[] = [];
const totalStart = performance.now();
for (let i = 0; i < iters; i++) {
  const t0 = performance.now();
  const out = filterAllowedEvents(events, blockedIds);
  const t1 = performance.now();
  if (out.length !== 1600) {
    throw new Error(`unexpected output length: ${out.length}`);
  }
  latenciesMs.push(t1 - t0);
}
const totalElapsedSec = Math.max((performance.now() - totalStart) / 1000, 1e-6);
latenciesMs.sort((a, b) => a - b);
const p95 = latenciesMs[Math.floor(latenciesMs.length * 0.95)];
const opsPerSec = iters / totalElapsedSec;

console.log(
  JSON.stringify({
    ops_per_sec: Number(opsPerSec.toFixed(2)),
    p95_ms: Number(p95.toFixed(4)),
  })
);
