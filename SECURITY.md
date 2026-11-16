# Security model

1. **ZAL requests, it never grants.** `requires` is checked statically so a
   program cannot even compile a call it did not declare, but the authoritative
   decision is made outside the program by the `Broker`, before the run
   (preflight over all concrete declared effects) and again on every call.
2. **Fail closed.** No policy means no grants. Broker errors, unknown effects,
   host failures and IR version mismatches abort the run.
3. **Wildcards are not pre-approved.** `requires tool.*` compiles, but each
   concrete `tool.x` is authorized per call.
4. **The VM is not a sandbox.** It has no file, network or process access of
   its own; all effects go through `Host`. Isolation of tools stays with the
   embedding host (sandboxing, network egress control, credential handling).
5. **Pure functions.** `fn` cannot have effects, which keeps decision logic
   testable and replayable.
6. **No shared mutable state between fibers** by construction (§7 of SPEC).
7. **Resource limits.** Budgets bound wall time, in-flight host calls and call
   count; call depth is capped at 10 000 frames; integer overflow traps.
8. **Secrets.** v0.1 has no secret type; the plan is a `secret.*` effect whose
   values never enter IR, traces or logs. Until then, do not pass secrets as
   ZAL values.

Report vulnerabilities to info@zerolog.ch.
