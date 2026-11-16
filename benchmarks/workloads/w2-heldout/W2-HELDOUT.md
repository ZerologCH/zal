# W2 held-out graphs (registered 2026-11-08, before any measurement)

The W2 graphs `w2-small`, `w2-medium`, `w2-large` were measured many times and
the native backend and runtime are tuned on them from now on: they are pilot
data, not evidence. These four graphs are generated now, with the existing
generator, and are not run by anyone until the final confirmation run.

| File | Shape (gen-w2.mjs) | Nodes |
|---|---|---|
| w2-h-wide.json | `h-wide 20 250 3 32` | 5000 |
| w2-h-deep.json | `h-deep 1000 4 2 8` | 4000 |
| w2-h-dense.json | `h-dense 50 100 8 64` | 5000 |
| w2-h-tiny.json | `h-tiny 3 3 2 4` | 9 |

Semantics: `zal-src/scheduler/dag.zal` (the program may be rewritten, the
observable behaviour may not: same results, same per-node inputs, LIFO ready
order, immediate retry, first failure cancels the rest), identical checksum
across implementations.

Metric and criterion (as W9): p50 per run over ≥ 200 iterations (≥ 30 for
graphs above 1000 nodes), 10 interleaved repetitions ZAL native /
`rust-generic`, median of medians; the research claim "10× vs generic Rust on
run-time graphs" holds for a graph only if that ratio is ≥ 10.0 with identical
checksums. Reported in full, including graphs that miss it.
