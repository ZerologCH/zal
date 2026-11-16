# Agent usability evaluation — 2026-11-08

Eight independent agents (Claude Haiku 5.5, a small model), each with a fresh
context, got one typical agent task and only `docs/AGENTS.md` (plus SPEC.md
if needed). Each wrote a ZAL program, checked it with `zal check --json`
(fix rounds allowed, max 4), and ran it against a mock host. Results were then
re-verified independently (check, run, extra negative cases).

| Task | First `zal check` ok | Checks until ok | Run correct | Notes |
|---|---|---|---|---|
| 01 research step (readonly reads, grep, summarise, budget) | yes | 1 | yes | |
| 02 deploy with retry, backoff, timeout | yes | 1 | yes | also correct Err when deploy keeps failing |
| 03 parallel tests (spawn/join, max_parallel) | yes | 1 | yes | also correct when all fail |
| 04 build DAG (record, parallel, ensure) | yes | 1 | yes | |
| 05 map aggregation + pure fn | yes | 1 | yes | |
| 06 race with fallback | yes | 1 | yes | |
| 07 enum state machine + test block | yes | 1 | yes | `zal test` passes |
| 08 resumable batch with Map | no | 2 | yes | `return` directly in a match arm was rejected |

**7 of 8 compiled on the first attempt, 8 of 8 after at most one repair round;
8 of 8 behaved correctly.** The one stumbling block was a language rough edge,
fixed the same day: `return` / `break` / `continue` may now stand directly as a
match arm.

Limitations: 8 tasks, one model, tasks written by us; this shows the language
and docs are learnable for a small model, not a statistically general result.
