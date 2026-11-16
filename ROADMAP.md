# Roadmap

This is the plan for the next ZAL releases. It is a statement of intent, not a
promise: scope and order can change as we learn, and there are no fixed dates.
Shipped work moves to [CHANGELOG.md](CHANGELOG.md).

Until 1.0, minor releases (0.x) may break the language, the IR format and the
host protocol. Each such break is listed in the changelog with a migration note.

## Released

**0.1.0 (2026-11-07)**: stage-0 compiler with type, effect and capability
checking; IR v1 with a verifier; VM with structured concurrency and budgets;
JSONL host protocol; Cranelift and native x86-64 backends; `zal fmt` and
`zal check --json`. Details in the changelog.

## 0.2: durable workflows

The goal is workflows that survive a crash or restart without repeating work
that already finished.

- `checkpoint`: journal the results of completed host calls per run and replay
  them on resume, so a restarted workflow continues where it stopped
  (SPEC §11).
- Replay safety checks: the compiler flags calls that make a function
  non-replayable (for example `now_ms()`) inside checkpointed code.
- A reference journal store on the local file system, behind a small storage
  interface so hosts can plug in their own.
- Host protocol v2 with run IDs and resume handshakes. The v1 protocol keeps
  working for workflows without checkpoints.

## 0.3: editor support and modules

- A language server (LSP): diagnostics, hover types, go to definition, rename
  and the existing fix hints as code actions (SPEC §11).
- Qualified imports, so larger programs can split into modules without name
  clashes (SPEC §11).
- Editor packages with syntax highlighting for common editors.

## 0.4: richer language

- Generics in user code (SPEC §11).
- Closures (SPEC §11).
- Nested budgets: a sub-block can get its own, tighter timeout and call limits
  inside a workflow budget (SPEC §11).
- Memory, token and cost budgets in addition to time, parallelism and tool
  calls (SPEC §11).

## Towards 1.0: stability

- Freeze the language core, IR v1 and host protocol v2, with a compatibility
  promise for the 1.x line.
- Prebuilt release binaries for Linux x86-64, with checksums.
- A standard library of common workflow patterns (fan-out, fan-in, retries
  with jitter, circuit breakers) written in ZAL.
- Expanded test suites: more differential tests between the VM and the native
  backends, and longer fuzzing runs.

## Later and optional

- More native targets beyond x86-64 (for example aarch64), after the backend
  interface is stable.
- Self-hosting: the compiler front end rewritten in ZAL. This is optional and
  not a precondition for anything above (see [BOOTSTRAP.md](BOOTSTRAP.md)).

## Licensing

ZAL is published under the Business Source License 1.1 (see
[LICENSE](LICENSE)). Each version becomes Apache License 2.0 four years after
its release. Personal and home-lab use, learning and research, and open source
projects are free. Organizations that want to use ZAL in production or
integrate it into their products or infrastructure need a commercial license;
contact us via https://zerolog.ch.

## Feedback

Ideas and bug reports are welcome as GitHub issues. Security issues: see
[SECURITY.md](SECURITY.md).
