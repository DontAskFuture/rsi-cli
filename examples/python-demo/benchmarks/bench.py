import json
import time
from src.dedupe import dedupe_preserve_order


def run_benchmark() -> None:
    payload = [f"item_{i % 500}" for i in range(4000)]
    # Warmup
    dedupe_preserve_order(payload[:200])

    latencies_ms = []
    iters = 40
    total_start = time.perf_counter()
    for _ in range(iters):
        t0 = time.perf_counter()
        out = dedupe_preserve_order(payload)
        t1 = time.perf_counter()
        if len(out) != 500:
            raise AssertionError("unexpected output cardinality")
        latencies_ms.append((t1 - t0) * 1000.0)
    total_elapsed = max(time.perf_counter() - total_start, 1e-6)

    latencies_ms.sort()
    p95 = latencies_ms[int(len(latencies_ms) * 0.95)]
    ops_per_sec = iters / total_elapsed
    print(json.dumps({"ops_per_sec": round(ops_per_sec, 2), "p95_ms": round(p95, 4)}))


if __name__ == "__main__":
    run_benchmark()
