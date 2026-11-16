#!/usr/bin/env python3
"""Runs every case of cases.tsv on the VM and on a native backend and compares them.

--mode rust (default): the per-program binary from `zal native` in .build/bins.
--mode jit:<backend>:  `zal run --backend <backend>`.
--mode aot:<backend>:  the per-program executable from `zal build --backend` in .build/aot-<backend>.
"""
import argparse, json, os, re, subprocess, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("--zal"); ap.add_argument("--here"); ap.add_argument("--filter", default="")
ap.add_argument("--mode", default="rust")
a = ap.parse_args()
H = a.here
TIMEOUT = 60


def sh(cmd):
    t0 = time.monotonic()
    try:
        p = subprocess.run(cmd, capture_output=True, timeout=TIMEOUT)
        rc, out, err = p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")
    except subprocess.TimeoutExpired:
        rc, out, err = -999, "", "TIMEOUT"
    return rc, out, err, (time.monotonic() - t0) * 1000


def split_result(out):
    """stdout = print lines + the pretty JSON result; returns (prints, canonical result)."""
    lines = out.rstrip("\n").split("\n")
    for i in range(len(lines)):
        try:
            v = json.loads("\n".join(lines[i:]))
            return lines[:i], json.dumps(v, sort_keys=True, ensure_ascii=False)
        except ValueError:
            continue
    return lines, None


def first_msg(err):
    for l in err.splitlines():
        l = l.strip()
        if l:
            return re.sub(r"^runtime error: ", "", l)
    return ""


def klass(rc):
    if rc == 0: return "ok"
    if rc == -999: return "hang"
    if rc < 0 or rc >= 124: return "crash"
    return "fail"


cases = []
for line in open(os.path.join(H, "cases.tsv")):
    if line.startswith("#") or not line.strip():
        continue
    f = line.rstrip("\n").split("|")
    f += [""] * (7 - len(f))
    prog, entry, args, mock, pol, bounds, note = f[:7]
    if a.filter and not re.search(a.filter, prog + " " + entry):
        continue
    cases.append(dict(prog=prog, entry=entry, args=args, mock=mock, pol=pol, bounds=bounds, note=note))

results, details = [], []
build_logs = {}


KIND, _, BE = a.mode.partition(":")
BIN_DIR = {"rust": ".build/bins", "aot": f".build/aot-{BE}"}.get(KIND)
LOG_DIR = {"rust": ".build/logs", "aot": f".build/logs-aot-{BE}"}.get(KIND)


def build_error(stem):
    p = os.path.join(H, LOG_DIR, stem + ".log")
    if not os.path.exists(p):
        return "no binary and no log"
    txt = open(p).read()
    errs = [l.strip() for l in txt.splitlines() if re.match(r"\s*(error|error\[)", l)]
    return "; ".join(errs[:2]) if errs else txt.strip()[:200]


for c in cases:
    stem = c["prog"]
    src = os.path.join(H, "progs", stem + ".zal")
    extra = []
    if c["mock"] != "-": extra += ["--mock", os.path.join(H, "mocks", c["mock"])]
    if c["pol"] != "-": extra += ["--policy", os.path.join(H, "policies", c["pol"])]
    vm = sh([a.zal, "run", src, "--entry", c["entry"], "--args", c["args"]] + extra)
    binp = os.path.join(H, BIN_DIR, stem) if BIN_DIR else None
    status, reason, msgcmp = "PASS", "", ""
    nat = None
    if binp and not os.path.exists(binp):
        status, reason = "FAIL", "BUILD: native build failed: " + build_error(stem)
    else:
        if binp:
            nat = sh([binp, "--entry", c["entry"], "--args", c["args"]] + extra)
        else:
            nat = sh([a.zal, "run", src, "--entry", c["entry"], "--args", c["args"], "--backend", BE] + extra)
        vk, nk = klass(vm[0]), klass(nat[0])
        if vk == "ok" and nk == "ok":
            vp, vv = split_result(vm[1]); np_, nv = split_result(nat[1])
            if vv != nv:
                status, reason = "FAIL", "VALUE"
            elif vp != np_:
                status, reason = "FAIL", "PRINT"
            else:
                reason = "ok"
        elif vk in ("fail",) and nk in ("fail",):
            reason = "both trap"
            m1, m2 = first_msg(vm[2]), first_msg(nat[2])
            if m1 == m2: msgcmp = "same message"
            else:
                msgcmp = "message differs"
        else:
            status, reason = "FAIL", {
                ("ok", "fail"): "CLASS: VM ok, native trap",
                ("fail", "ok"): "CLASS: VM trap, native ok",
            }.get((vk, nk), f"CLASS: VM {vk}, native {nk}")
        if c["bounds"] not in ("", "-"):
            lo, _, hi = c["bounds"].partition("-")
            for who, r in (("VM", vm), ("native", nat)):
                ms = r[3]
                if (lo and ms < float(lo)) or (hi and ms > float(hi)):
                    status = "FAIL"
                    reason = (reason + "; " if reason and reason != "ok" else "") + f"TIME: {who} {ms:.0f} ms not in [{c['bounds']}]"
    if status == "PASS" and msgcmp == "message differs":
        tag = "PASS~"
    else:
        tag = status
    short_args = c["args"] if len(c["args"]) < 36 else c["args"][:33] + "..."
    print(f"{tag:<6} {c['prog']:<26} {c['entry']:<24} {short_args:<38} {reason}{' / ' + msgcmp if msgcmp else ''}", flush=True)
    results.append((tag, c, reason, msgcmp))
    if tag in ("FAIL", "PASS~"):
        def fmt(r):
            if r is None: return "(not run)"
            return f"rc={r[0]} {r[3]:.0f}ms\n  stdout: {r[1].strip()[:400]!r}\n  stderr: {r[2].strip()[:300]!r}"
        details.append(f"### {tag} {c['prog']} / {c['entry']} {c['args']}\n- mock={c['mock']} policy={c['pol']} note={c['note']}\n- reason: {reason} {msgcmp}\n- VM: {fmt(vm)}\n- native: {fmt(nat)}\n")

n = len(results)
npass = sum(1 for r in results if r[0] == "PASS")
nmsg = sum(1 for r in results if r[0] == "PASS~")
nfail = sum(1 for r in results if r[0] == "FAIL")
progs = sorted({r[1]["prog"] for r in results})
built = [p for p in progs if not BIN_DIR or os.path.exists(os.path.join(H, BIN_DIR, p))]
print(f"\n==== SUMMARY ({a.mode}) ====")
print(f"programs: {len(progs)} ({len(built)} built natively, {len(progs) - len(built)} native build failures)")
print(f"cases (program+entry+args): {n}; distinct entries: {len({(r[1]['prog'], r[1]['entry']) for r in results})}")
print(f"PASS: {npass}   PASS with differing trap message: {nmsg}   FAIL: {nfail}")
by = {}
for r in results:
    if r[0] == "FAIL":
        by.setdefault(r[2].split(":")[0].split(";")[0], []).append(r)
for k, v in sorted(by.items()):
    print(f"  FAIL {k}: {len(v)}")
os.makedirs(os.path.join(H, "out"), exist_ok=True)
with open(os.path.join(H, "out/results.tsv"), "w") as f:
    for tag, c, reason, msgcmp in results:
        f.write("\t".join([tag, c["prog"], c["entry"], c["args"], reason, msgcmp]) + "\n")
with open(os.path.join(H, "out/failures.md"), "w") as f:
    f.write("# Differential failures\n\n" + "\n".join(details))
sys.exit(1 if nfail else 0)
