#!/usr/bin/env python3
"""Generate a static ZAL pipeline equivalent to a W2 workload JSON.

Same nodes, tools, inputs and dependency order as the dynamic scheduler
(zal-src/scheduler/dag.zal): a node's input is its own input followed by
"|" + output of each dependency in declaration order; each node retries its
tool up to max_attempts times. Usage: gen-static.py <w2.json> <module> > out.zal
"""
import json, sys
nodes, _ = json.load(open(sys.argv[1]))
mod = sys.argv[2]
by_layer = {}
for n in nodes:
    layer = int(n["id"][1:].split("_")[0])
    by_layer.setdefault(layer, []).append(n)
out = [f"module {mod}", "",
       "extern tool dispatch(tool: Str, node: Str, input: Str) -> Result<Str, Str>", "",
       "// One retried tool call per node, as run_node in scheduler/dag.zal.",
       "task node(tool: Str, id: Str, input: Str, max_attempts: Int) -> Result<Str, Str> requires tool.dispatch {",
       "    retry(max_attempts) { dispatch(tool, id, input) }",
       "}", "",
       f"workflow run_static() -> Result<Int, Str> requires tool.dispatch {{"]
var = lambda i: "o_" + i.replace("n", "", 1)
for layer in sorted(by_layer):
    ns = by_layer[layer]
    calls = []
    for n in ns:
        parts = [json.dumps(n["input"])]
        for d in n["deps"]:
            parts += ['"|"', var(d)]
        calls.append(f'node({json.dumps(n["tool"])}, {json.dumps(n["id"])}, {" + ".join(parts)}, {n["max_attempts"]})')
    names = ", ".join(var(n["id"]) + "_r" for n in ns)
    out.append(f"    let ({names}) = parallel {{")
    for c in calls:
        out.append(f"        {c}")
    out.append("    }")
    for n in ns:
        out.append(f"    let {var(n['id'])} = {var(n['id'])}_r?")
out.append(f"    Ok({len(nodes)})")
out.append("}")
print("\n".join(out))
