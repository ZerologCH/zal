#!/usr/bin/env bash
# Differential suite: VM (reference) vs a native backend.
#
#   tests/differential/run.sh [-j N] [-f REGEX] [--rebuild] [--backend MODE]
#
#   --backend MODE  what to compare against the VM (default: rust, the generated-Rust `zal native`):
#                   cranelift | x64           JIT: `zal run --backend <name>`
#                   cranelift-aot | x64-aot   AOT: `zal build --backend <name> -o bin`, then run bin
#                   fake                      test backend (generic path only, no codegen)
#                   A mode whose backend is not built is skipped (exit 0).
#   -j N        parallel native builds (default 3; each worker owns a cargo target dir)
#   -f REGEX    only cases whose "program entry" line matches REGEX
#   --rebuild   ignore the native build cache
#
# Needs: cargo (env CARGO, default the build-redirect shim ~/.local/zl-shims/cargo), python3. Cases are in cases.tsv,
# programs in progs/, mock hosts in mocks/, capability policies in policies/.
# Output: PASS/FAIL table on stdout, details in out/failures.md, out/results.tsv.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
export CARGO=${CARGO:-$HOME/.local/zl-shims/cargo}
JOBS=3; FILTER=""; REBUILD=0; MODE=rust
while [ $# -gt 0 ]; do
  case "$1" in
    -j) JOBS=$2; shift 2;;
    -f) FILTER=$2; shift 2;;
    --rebuild) REBUILD=1; shift;;
    --backend) MODE=$2; shift 2;;
    *) echo "unknown option $1" >&2; exit 2;;
  esac
done
BUILD=$HERE/.build; OUT=$HERE/out
mkdir -p "$BUILD/bins" "$BUILD/logs" "$OUT"

echo ">> repo at $(cd "$REPO" && git rev-parse --short HEAD)$(cd "$REPO" && git diff --quiet -- compiler rt tools vm || echo "+uncommitted changes")" >&2
PROGS=$(cut -d'|' -f1 "$HERE/cases.tsv" | grep -v '^#' | sort -u)
if [ -n "$FILTER" ]; then PROGS=$(grep -E "$FILTER" "$HERE/cases.tsv" | grep -v '^#' | cut -d'|' -f1 | sort -u); fi

if [ "$MODE" != rust ]; then
  case "$MODE" in
    cranelift|x64|fake) KIND=jit; BE=$MODE;;
    cranelift-aot|x64-aot) KIND=aot; BE=${MODE%-aot};;
    *) echo "unknown backend mode $MODE" >&2; exit 2;;
  esac
  FEAT=""; [ "$BE" != fake ] && FEAT="--features $BE"
  echo ">> building the zal CLI ($FEAT) and the AOT runtime" >&2
  # The CLI with this backend gets its own target dir so the plain CLI stays intact.
  TD=$REPO/target/diff-$BE
  # shellcheck disable=SC2086
  if ! (cd "$REPO" && CARGO_TARGET_DIR=$TD timeout 1800 "$CARGO" build --release -p zal-cli -p zal-aot --quiet $FEAT) 2>"$BUILD/logs/cli-$BE.log"; then
    echo "SKIP: backend $BE not built (build of zal --features $BE failed, see $BUILD/logs/cli-$BE.log)"; exit 0
  fi
  ZAL=$TD/release/zal
  export ZAL_AOT_LIB=$TD/release/libzal_aot.a
  probe=$(printf 'module p\nfn f() -> Int { 1 }\n' > "$BUILD/probe.zal"; "$ZAL" run "$BUILD/probe.zal" --entry f --backend "$BE" 2>&1)
  case "$probe" in *"backend not built"*|*"unknown backend"*) echo "SKIP: backend $BE not built ($probe)"; exit 0;; esac
  if [ "$KIND" = aot ]; then
    ADIR=$BUILD/aot-$BE; mkdir -p "$ADIR" "$BUILD/logs-aot-$BE"
    echo ">> AOT builds ($BE)" >&2
    for p in $PROGS; do
      if ! (cd "$REPO" && timeout 600 "$ZAL" build "$HERE/progs/$p.zal" --backend "$BE" -o "$ADIR/$p") >"$BUILD/logs-aot-$BE/$p.log" 2>&1; then
        # A stale binary from an earlier run must not stand in for a failed build.
        [ -e "$ADIR/$p" ] && mv -f "$ADIR/$p" "$ADIR/$p.stale"
        echo "   AOT BUILD FAILED $p" >&2
      fi
    done
  fi
  echo ">> running cases ($MODE)" >&2
  exec python3 -I "$HERE/difftool.py" --zal "$ZAL" --here "$HERE" --filter "$FILTER" --mode "$KIND:$BE"
fi

echo ">> building the zal CLI" >&2
(cd "$REPO" && timeout 900 "$CARGO" build --release -p zal-cli --quiet) || { echo "CLI build failed" >&2; exit 2; }
ZAL=$REPO/target/release/zal

# Cache key: program source plus everything the native backend is made of.
SRC_KEY=$(cat "$REPO"/compiler/src/*.rs "$REPO"/rt/src/*.rs "$REPO"/tools/cli/src/main.rs | sha1sum | cut -c1-16)

build_one() { # worker-index program-file
  local w=$1 f=$2 stem key home
  stem=$(basename "$f" .zal)
  key=$( (cat "$f"; echo "$SRC_KEY") | sha1sum | cut -c1-16)
  if [ "$REBUILD" = 0 ] && [ -f "$BUILD/bins/$stem.key" ] && [ "$(cat "$BUILD/bins/$stem.key")" = "$key" ]; then return; fi
  rm -f "$BUILD/bins/$stem" "$BUILD/bins/$stem.key"
  home=$BUILD/home$w
  mkdir -p "$home"; [ -e "$home/rt" ] || ln -s "$REPO/rt" "$home/rt"
  local bin
  if bin=$(cd "$REPO" && ZAL_HOME="$home" timeout 600 "$ZAL" native "$f" 2>"$BUILD/logs/$stem.log" | tail -1) && [ -x "$bin" ]; then
    cp "$bin" "$BUILD/bins/$stem"; echo "$key" > "$BUILD/bins/$stem.key"; rm -f "$BUILD/logs/$stem.log"
    echo "   built $stem" >&2
  else
    echo "   NATIVE BUILD FAILED $stem" >&2
  fi
}

echo ">> native builds ($JOBS workers)" >&2
i=0
for w in $(seq 0 $((JOBS-1))); do
  ( j=0; for p in $PROGS; do
      if [ $((j % JOBS)) = "$w" ]; then build_one "$w" "$HERE/progs/$p.zal"; fi
      j=$((j+1)); done ) &
done
wait

echo ">> running cases" >&2
exec python3 -I "$HERE/difftool.py" --zal "$ZAL" --here "$HERE" --filter "$FILTER"
