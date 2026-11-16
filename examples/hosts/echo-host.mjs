// Minimal JSONL tool host for `zal run --host`. Answers every call after a
// short delay with the first argument (wrapped as Ok for Result-returning
// tools) and honours cancel messages. Reference for real adapters.
import readline from 'node:readline';

const pending = new Map();
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  const msg = JSON.parse(line);
  if (msg.cancel) {
    clearTimeout(pending.get(msg.id));
    pending.delete(msg.id);
    return;
  }
  const first = Object.values(msg.args)[0];
  const t = setTimeout(() => {
    pending.delete(msg.id);
    process.stdout.write(JSON.stringify({ id: msg.id, ok: { ok: first } }) + '\n');
  }, 5);
  pending.set(msg.id, t);
});
