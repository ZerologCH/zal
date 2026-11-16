//! Host ABI v1: how the VM reaches tools, and how capabilities are authorized.
//!
//! The VM never executes a side effect itself. Every `extern` call goes
//! through `Broker::authorize` (the external policy authority, fail-closed) and
//! then through `Host::call`. Both are traits so an embedding host can plug in its
//! own tool proxy / policy service while tests use in-process doubles.

use std::collections::{BinaryHeap, HashMap, HashSet};

/// Fast non-cryptographic map for keys the run itself controls (tool names,
/// call ids): SipHash showed up in per-call profiles.
pub type FxMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<crate::value::FxHasher>>;
use std::cmp::Reverse;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value as J};
use zal_ir::{Extern, Program, Ty};

use crate::value::{from_json, to_json, zero, Value};

pub enum Reply {
    Done(Result<Value, String>),
    Pending,
}

pub trait Host {
    /// Start a call. `Err` in a reply is an infrastructure failure (it traps the run);
    /// tool-level failures are ordinary `Err(..)` values of the extern's Result type.
    fn call(&mut self, id: u64, ext: &Extern, args: Vec<Value>, prog: &Program) -> Reply;
    /// Wait for completions until `until` (None = until at least one completes).
    fn poll(&mut self, until: Option<Instant>, prog: &Program) -> Vec<(u64, Result<Value, String>)>;
    fn cancel(&mut self, id: u64);
}

pub trait Broker {
    /// Authoritative decision for one call. Must fail closed on any doubt.
    fn authorize(&mut self, effect: &str) -> Result<(), String>;
    /// Host-side ceilings. A program's own budget can only tighten them: a
    /// workflow that declares no or a huge budget still runs under these.
    fn caps(&self) -> zal_ir::Budget {
        zal_ir::Budget::default()
    }
}

/// Grants from a policy document: `{"grant": ["tool.build", "tool.*"]}`.
/// Optional `"caps": {"timeout_ms": .., "max_parallel": .., "max_tool_calls": ..}`
/// bounds every run regardless of the program's declared budget.
pub struct PolicyBroker {
    grants: Vec<String>,
    pub caps: zal_ir::Budget,
}

impl PolicyBroker {
    pub fn new(grants: Vec<String>) -> Self {
        PolicyBroker { grants, caps: zal_ir::Budget::default() }
    }
    pub fn from_json(s: &str) -> Result<Self, String> {
        let j: J = serde_json::from_str(s).map_err(|e| format!("policy: {e}"))?;
        let arr = j.get("grant").and_then(|g| g.as_array()).ok_or("policy needs a `grant` array")?;
        let grants = arr.iter().map(|g| g.as_str().map(String::from).ok_or("grant entries are strings")).collect::<Result<_, _>>()?;
        let mut caps = zal_ir::Budget::default();
        if let Some(c) = j.get("caps") {
            let c = c.as_object().ok_or("policy `caps` must be an object")?;
            for (k, v) in c {
                let n = v.as_u64().filter(|n| *n > 0).ok_or_else(|| format!("policy cap `{k}` must be a positive integer"))?;
                match k.as_str() {
                    "timeout_ms" => caps.timeout_ms = Some(i64::try_from(n).map_err(|_| "policy cap `timeout_ms` too large")?),
                    "max_parallel" => caps.max_parallel = Some(u32::try_from(n).map_err(|_| "policy cap `max_parallel` too large")?),
                    "max_tool_calls" => caps.max_tool_calls = Some(n),
                    _ => return Err(format!("unknown policy cap `{k}` (timeout_ms, max_parallel, max_tool_calls)")),
                }
            }
        }
        Ok(PolicyBroker { grants, caps })
    }
}

impl Broker for PolicyBroker {
    fn authorize(&mut self, effect: &str) -> Result<(), String> {
        let ok = self.grants.iter().any(|g| g == effect || (g.ends_with(".*") && effect.starts_with(&g[..g.len() - 1])));
        if ok {
            Ok(())
        } else {
            Err(format!("capability `{effect}` not granted"))
        }
    }
    fn caps(&self) -> zal_ir::Budget {
        self.caps.clone()
    }
}

// ---------------------------------------------------------------- mock host

#[derive(Clone, Default)]
pub struct MockTool {
    pub latency: Duration,
    /// JSON result, marshalled through the extern's return type.
    pub result: Option<J>,
    /// Return argument `echo_arg` (default 0), wrapped in Ok for Result types.
    pub echo: bool,
    pub echo_arg: usize,
    /// The first N calls return Err("mock failure") (Result types only).
    pub fail_first: u32,
    /// Return `<first argument><suffix>` (wrapped in Ok for Result types).
    pub suffix: Option<String>,
    /// Latency by the first (Str) argument, overriding `latency`.
    pub latency_by_arg0: FxMap<String, Duration>,
}

/// Deterministic in-process host for tests and benchmarks.
#[derive(Default)]
pub struct MockHost {
    pub tools: FxMap<String, MockTool>,
    calls: FxMap<String, u32>,
    pending: BinaryHeap<Reverse<(Instant, u64)>>,
    results: FxMap<u64, Value>,
    pub log: Vec<String>,
    pub record: bool,
    /// Sum of the char lengths of Str argument `checksum_arg` over all calls,
    /// so benchmarks can prove every implementation did identical work.
    pub checksum_arg: Option<usize>,
    pub checksum: u64,
}

impl MockHost {
    pub fn from_json(s: &str) -> Result<Self, String> {
        let j: J = serde_json::from_str(s).map_err(|e| format!("mock config: {e}"))?;
        let mut h = MockHost::default();
        h.checksum_arg = j.get("checksum_arg").and_then(|v| v.as_u64()).map(|v| v as usize);
        if let Some(tools) = j.get("tools").and_then(|t| t.as_object()) {
            for (name, cfg) in tools {
                h.tools.insert(
                    name.clone(),
                    MockTool {
                        latency: Duration::from_millis(cfg.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0)),
                        result: cfg.get("result").cloned(),
                        echo: cfg.get("echo").and_then(|v| v.as_bool()).unwrap_or(false) || cfg.get("echo_arg").is_some(),
                        echo_arg: cfg.get("echo_arg").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                        fail_first: cfg.get("fail_first").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                        suffix: cfg.get("suffix").and_then(|v| v.as_str()).map(String::from),
                        latency_by_arg0: cfg
                            .get("latency_by_arg0")
                            .and_then(|v| v.as_object())
                            .map(|o| o.iter().filter_map(|(k, v)| v.as_u64().map(|ms| (k.clone(), Duration::from_millis(ms)))).collect())
                            .unwrap_or_default(),
                    },
                );
            }
        }
        Ok(h)
    }

    /// One call's result. `cfg` is the tool's entry (looked up once per call).
    fn produce(calls: &mut FxMap<String, u32>, cfg: Option<&MockTool>, ext: &Extern, args: Vec<Value>, prog: &Program) -> Result<Value, String> {
        let attempt = match calls.get_mut(&ext.name) {
            Some(n) => {
                *n += 1;
                *n
            }
            None => {
                calls.insert(ext.name.clone(), 1);
                1
            }
        };
        let Some(cfg) = cfg else {
            return Ok(zero(&ext.ret, prog));
        };
        if attempt <= cfg.fail_first {
            if let Ty::Result(_, e) = &ext.ret {
                let err = match **e {
                    Ty::Str => crate::value::str_val("mock failure"),
                    ref t => zero(t, prog),
                };
                return Ok(Value::Variant(1, Some(Rc::new(err))));
            }
            return Err(format!("mock failure in `{}`", ext.name));
        }
        if let Some(sfx) = &cfg.suffix {
            let base = match args.first() {
                Some(Value::Str(x)) => x.to_string(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            let v = crate::value::str_val(&(base + sfx));
            return Ok(match ext.ret {
                Ty::Result(_, _) => Value::Variant(0, Some(Rc::new(v))),
                _ => v,
            });
        }
        if cfg.echo {
            let first = args.into_iter().nth(cfg.echo_arg).unwrap_or(Value::Unit);
            return Ok(match ext.ret {
                Ty::Result(_, _) => Value::Variant(0, Some(Rc::new(first))),
                _ => first,
            });
        }
        match &cfg.result {
            Some(j) => from_json(j, &ext.ret, prog),
            None => Ok(zero(&ext.ret, prog)),
        }
    }
}

impl Host for MockHost {
    fn call(&mut self, id: u64, ext: &Extern, args: Vec<Value>, prog: &Program) -> Reply {
        if self.record {
            self.log.push(ext.name.clone());
        }
        if let Some(Value::Str(s)) = self.checksum_arg.and_then(|i| args.get(i)) {
            self.checksum += s.len() as u64;
        }
        let cfg = self.tools.get(&ext.name);
        let latency = match (cfg, args.first()) {
            (Some(t), Some(Value::Str(a))) if !t.latency_by_arg0.is_empty() && t.latency_by_arg0.contains_key(&**a) => t.latency_by_arg0[&**a],
            (Some(t), _) => t.latency,
            _ => Duration::ZERO,
        };
        let r = Self::produce(&mut self.calls, cfg, ext, args, prog);
        if latency.is_zero() {
            return Reply::Done(r);
        }
        match r {
            Ok(v) => {
                self.results.insert(id, v);
                self.pending.push(Reverse((Instant::now() + latency, id)));
                Reply::Pending
            }
            Err(e) => Reply::Done(Err(e)),
        }
    }

    fn poll(&mut self, until: Option<Instant>, _prog: &Program) -> Vec<(u64, Result<Value, String>)> {
        loop {
            let now = Instant::now();
            // Release only results due by the caller's horizon: when the thread
            // wakes late, a timer due before a result must still win, as it
            // would on an unloaded machine.
            let cutoff = until.map_or(now, |u| u.min(now));
            let mut out = Vec::new();
            while let Some(Reverse((at, id))) = self.pending.peek().copied() {
                if at > cutoff {
                    break;
                }
                self.pending.pop();
                if let Some(v) = self.results.remove(&id) {
                    out.push((id, Ok(v)));
                }
            }
            if !out.is_empty() {
                return out;
            }
            if until.is_some_and(|u| u <= now) {
                return out;
            }
            let next = self.pending.peek().map(|Reverse((at, _))| *at);
            let wake = match (next, until) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => return out,
            };
            if wake > now {
                std::thread::sleep(wake - now);
            }
        }
    }

    fn cancel(&mut self, id: u64) {
        // Cancelled results are dropped when their timer comes due.
        self.results.remove(&id);
    }
}

// ---------------------------------------------------------------- JSONL host

/// Talks to an external tool proxy over stdin/stdout, one JSON object per line.
///
/// Request:  `{"id":1,"tool":"build","effect":"tool.build","args":{"repo":"x"}}`
/// Cancel:   `{"id":1,"cancel":true}`
/// Response: `{"id":1,"ok":<value>}` or `{"id":1,"error":"<infrastructure failure>"}`
pub struct JsonlHost {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    pending: HashMap<u64, Ty>,
    cancelled: HashSet<u64>,
}

impl JsonlHost {
    pub fn spawn(cmd: &str) -> Result<Self, String> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("cannot start host `{cmd}`: {e}"))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(JsonlHost { child, stdin, rx, pending: HashMap::new(), cancelled: HashSet::new() })
    }

    /// One response line. A protocol violation (bad JSON, no id, an id never
    /// issued) fails every pending call: waiting on would hang the run.
    fn handle(&mut self, line: &str, prog: &Program, out: &mut Vec<(u64, Result<Value, String>)>) {
        let r = (|| -> Result<Option<(u64, Result<Value, String>)>, String> {
            let j: J = serde_json::from_str(line).map_err(|e| format!("host sent invalid JSON: {e}"))?;
            let id = j.get("id").and_then(J::as_u64).ok_or("host response without a numeric `id`")?;
            let Some(ty) = self.pending.remove(&id) else {
                // Late answers to cancelled calls are expected; anything else is not.
                return if self.cancelled.remove(&id) { Ok(None) } else { Err(format!("host answered unknown call id {id}")) };
            };
            if let Some(e) = j.get("error") {
                return Ok(Some((id, Err(e.as_str().unwrap_or("host error").to_string()))));
            }
            Ok(Some(match j.get("ok") {
                Some(v) => (id, from_json(v, &ty, prog)),
                None => (id, Err("host response has neither `ok` nor `error`".into())),
            }))
        })();
        match r {
            Ok(x) => out.extend(x),
            Err(m) => {
                for id in self.pending.keys().copied().collect::<Vec<_>>() {
                    out.push((id, Err(format!("host protocol error: {m}"))));
                }
                self.pending.clear();
            }
        }
    }
}

impl Host for JsonlHost {
    fn call(&mut self, id: u64, ext: &Extern, args: Vec<Value>, prog: &Program) -> Reply {
        let a: serde_json::Map<String, J> = ext.params.iter().zip(args.iter()).map(|((n, t), v)| (n.clone(), to_json(v, t, prog))).collect();
        let req = json!({ "id": id, "tool": ext.name, "effect": ext.effect, "args": a });
        if let Err(e) = writeln!(self.stdin, "{req}").and_then(|_| self.stdin.flush()) {
            return Reply::Done(Err(format!("host pipe closed: {e}")));
        }
        self.pending.insert(id, ext.ret.clone());
        Reply::Pending
    }

    fn poll(&mut self, until: Option<Instant>, prog: &Program) -> Vec<(u64, Result<Value, String>)> {
        let mut out = Vec::new();
        let first = match until {
            None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(u) => self.rx.recv_timeout(u.saturating_duration_since(Instant::now())),
        };
        match first {
            Ok(line) => self.handle(&line, prog, &mut out),
            Err(RecvTimeoutError::Timeout) => return out,
            Err(RecvTimeoutError::Disconnected) => {
                for id in self.pending.keys().copied().collect::<Vec<_>>() {
                    out.push((id, Err("host process exited".into())));
                }
                self.pending.clear();
                return out;
            }
        }
        while let Ok(line) = self.rx.try_recv() {
            self.handle(&line, prog, &mut out);
        }
        out
    }

    fn cancel(&mut self, id: u64) {
        if self.pending.remove(&id).is_some() {
            self.cancelled.insert(id);
        }
        let _ = writeln!(self.stdin, "{}", json!({ "id": id, "cancel": true }));
        let _ = self.stdin.flush();
    }
}

impl Drop for JsonlHost {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
