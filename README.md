# ZAL — ZeroLog Agent Language

ZAL is a small, statically typed language for agent workflows: task graphs,
tool calls, concurrency, budgets and capability requests. Programs are checked
before they run (types, effects, declared capabilities), compiled to a
versioned IR, and executed by a VM that sends every side effect through an
external authorization point.

```zal
module examples.build_verify

type Artifact = { id: Str, size: Int }
type Report = { passed: Bool, findings: Int }

extern tool build(repo: Str) -> Result<Artifact, Str>
extern tool test(a: Artifact) -> Result<Report, Str>
extern tool security_scan(a: Artifact) -> Result<Report, Str>
extern tool preview(a: Artifact) -> Result<Str, Str>

workflow build_and_verify(repo: Str) -> Result<Str, Str>
    requires tool.build, tool.test, tool.security_scan, tool.preview
    budget { timeout: 30min, max_parallel: 8, max_tool_calls: 50 }
{
    let artifact = retry(3, backoff: 10ms) { build(repo) }?
    let (tests, scan) = parallel { test(artifact), security_scan(artifact) }
    let t = tests?
    let s = scan?
    ensure t.passed, "tests failed"
    ensure s.passed && s.findings == 0, "security scan found " + str(s.findings) + " issue(s)"
    preview(artifact)
}
```

## Status

Version 0.1, stage 0, working and tested:

- compiler (types, effects, capabilities), IR v1 with verifier, VM;
- two direct machine-code backends (RFC 0002), no Rust source in between:
  **Cranelift** (`--backend cranelift`) and ZAL's **own x86-64 code generator**
  (`--backend x64`: own encoder, W^X JIT memory, own ELF object writer).
  `zal run … --backend <name>` runs JIT-compiled code, `zal build … --backend
  <name> -o app` links a standalone executable against `libzal_aot.a`
  (`cargo build --release -p zal-aot`). The older `zal native` (Rust source
  on `zal-rt`) remains only as a reference backend;
- automatic parallelization of independent `readonly` tool calls;
- `zal fmt`, `zal check --json` with fix hints, agent cheat sheet
  (docs/AGENTS.md) — 7 of 8 agent tasks written correctly on the first try by
  a small model (tests/agent-eval/REPORT.md);
- VM vs native differential suite (tests/differential) and IR fuzzing.

Performance, honestly: ZAL native is
about 2.3× faster than a lean generic Rust engine on fixed workflows and
1.3–1.5× slower than hand-written Rust; it is not 10× faster than Rust.

## Build and try

```sh
cargo build --release
Z=target/release/zal
$Z check examples/build_verify.zal
$Z run   examples/build_verify.zal --entry build_and_verify --args '["example-repo"]' \
         --mock examples/build_verify.mock.json --policy examples/build_verify.policy.json --stats
$Z test  examples/dag_pipeline.zal --mock examples/dag_pipeline.mock.json
$Z ir    examples/build_verify.zal          # bytecode listing
$Z fmt   examples/build_verify.zal --check  # canonical formatting (--write: in place; comments are kept)
$Z check --json examples/build_verify.zal   # one JSON diagnostic per line with a fix `hint`; exit 1 on errors
cargo test                                   # full suite
$Z native examples/dag_pipeline.zal          # native binary (prints its path)
$Z fmt examples/dag_pipeline.zal             # canonical formatting
$Z check --json examples/build_verify.zal    # machine-readable diagnostics
```

Without `--policy` nothing is granted and a run with effects is refused.

## Documents

- [SPEC.md](SPEC.md) — the language as implemented
- [ARCHITECTURE.md](ARCHITECTURE.md) — compiler, IR, VM, host ABI
- [SECURITY.md](SECURITY.md) — capability model
- [BOOTSTRAP.md](BOOTSTRAP.md) — why stage 0 is Rust and what self-hosting would mean
- `docs/decisions/`, `docs/rfcs/` — design decisions (German)
- [ROADMAP.md](ROADMAP.md) — planned releases
- [CHANGELOG.md](CHANGELOG.md) — shipped changes

## License

ZAL is source-available under the [Business Source License 1.1](LICENSE).

- **Free:** personal use and home labs, learning and research, and open source
  projects not run by an organization for its business.
- **Commercial license required:** production use by organizations, including
  integrating ZAL into their products, services, internal systems or
  infrastructure, or offering it as a hosted service. Contact us via
  https://zerolog.ch.
- **Becomes open source:** each version converts to the Apache License 2.0 four
  years after its release (0.1.0 on 2026-11-16).
