# Differential suite: VM vs native AOT backend

`./run.sh [-j N] [-f REGEX] [--rebuild]` builds each program in `progs/` once with
`zal native` (cached in `.build/`, keyed on program + compiler/rt sources), then runs every
case of `cases.tsv` on the VM (`zal run`) and on the native binary and compares them.

- `cases.tsv`: `program|entry|args|mock|policy|ms-bounds|note` (mock/policy are files in `mocks/`, `policies/`).
- Verdict per case: `PASS` (same JSON result and same print lines, or both fail), `PASS~` (both fail,
  message text differs), `FAIL` with reason `BUILD` (native build broken), `VALUE`, `PRINT`,
  `CLASS` (ok vs trap vs crash/hang) or `TIME` (wall time outside the bounds, e.g. to prove parallelism).
- Output: table on stdout, `out/results.tsv`, `out/failures.md` (VM and native output of each failure).
- Programs are split by feature on purpose: a native build error hides every entry of its program,
  so each known build-breaking feature lives in its own small file (d09, d14, d17-d19, d41-d49, d56-d61).
- Timing bounds assume a mostly idle machine; they use 50-100 ms mock latencies.

## Backend modes (RFC 0002)

`./run.sh --backend MODE` compares the VM with a machine-code backend instead of `zal native`:

- `cranelift`, `x64` — JIT: every case runs `zal run --backend <name>`.
- `cranelift-aot`, `x64-aot` — AOT: each program is linked once with
  `zal build --backend <name> -o .build/aot-<name>/<prog>` (runtime `libzal_aot.a`), then the binary runs.
- `fake` — test backend without code generation (every op through `zal_rt_op`); checks the ABI and this runner.

The CLI for a mode is built with `--features <name>` into `target/diff-<name>/`. If that build fails or the
backend is not registered, the mode prints `SKIP` and exits 0. Verdicts and output files are the same as above.
