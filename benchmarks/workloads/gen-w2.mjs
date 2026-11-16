// Generates W2 (DAG planning + execution) workloads as ZAL `--args` arrays:
// [nodes, max_parallel]. Deterministic: same parameters, same file.
// Usage: node gen-w2.mjs <name> <layers> <width> <fanin> <max_parallel>
import { writeFileSync } from 'node:fs';

const [name, layers, width, fanin, maxPar] = process.argv.slice(2);
const L = +layers, W = +width, F = +fanin;
const nodes = [];
for (let l = 0; l < L; l++) {
  for (let w = 0; w < W; w++) {
    const deps = [];
    if (l > 0) for (let k = 0; k < Math.min(F, W); k++) deps.push(`n${l - 1}_${(w + k) % W}`);
    nodes.push({ id: `n${l}_${w}`, tool: ['build', 'test', 'security_scan', 'preview'][w % 4], input: `in${l}_${w}`, deps, max_attempts: 3 });
  }
}
writeFileSync(new URL(`./w2-${name}.json`, import.meta.url), JSON.stringify([nodes, +maxPar]));
console.log(`w2-${name}.json: ${nodes.length} nodes, ${nodes.reduce((a, n) => a + n.deps.length, 0)} edges, max_parallel ${maxPar}`);
