# 0001 — ZAL ist eine eigene Sprache, der Orchestrator wird in ZAL geschrieben

Datum: 2026-11-07 · Status: angenommen (Plan v2, §0)

- ZAL hat eigene Grammatik, Typsystem, Effektsystem, IR und VM. Quelltexte sind `.zal`.
- Stage 0 von Compiler und VM ist in Rust. Rust enthält keine Workflow-, Scheduling- oder Retry-Logik.
- Orchestrierungslogik liegt in `zal-src/` (zuerst `scheduler/dag.zal`).
- Rechte werden in ZAL angefordert und ausserhalb entschieden (Broker), vor dem Lauf und bei jedem Aufruf.
