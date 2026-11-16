# W9 confirmation set 3 (registered 2026-11-08, before any measurement)

Since the second confirmation set the native runtime changed in general ways
(runtime allocator, eager first poll of spawned tasks, reused task slots,
borrowed read-only loop variables, last-use moves, word-wise string hashing).
No change is specific to a graph. Three new shapes, never run before:

| Variant | Graph (gen-w2.mjs) | Nodes |
|---|---|---|
| static_t3x5 | `t3x5 3 5 2 5` | 15 |
| static_t6x3 | `t6x3 6 3 3 3` | 18 |
| static_t2x7 | `t2x7 2 7 4 7` | 14 |

Same metric and criterion as W9-static-pipelines.md (p50 per run, ≥ 2000
iterations, 10 interleaved repetitions against rust-generic on the matching
w2 JSON, median of medians ≥ 10.0, identical checksums), measured once.
