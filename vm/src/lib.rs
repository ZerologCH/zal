//! ZAL VM v1: executes ZAL IR with cooperative fibers on one thread.
//!
//! The VM provides mechanisms only (fibers, timers, host calls, budgets).
//! Every scheduling *decision* of a workflow — what runs, in which order, how
//! often to retry — is ZAL bytecode. Side effects only happen via
//! `Broker::authorize` + `Host::call`, never inside the VM.

pub mod alloc;
pub mod host;
pub mod native;
mod rt;
pub mod value;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use zal_ir::{Budget, Builtin, Const, Op, Program};

pub use host::{Broker, Host, MockHost, PolicyBroker, Reply};
pub use native::{
    JitModule, NativeBackend, NativeFn, ZalCtx, ZAL_EXIT_CALL, ZAL_EXIT_DONE, ZAL_EXIT_RET, ZAL_EXIT_SUSPEND, ZAL_EXIT_TRAP, ZAL_JUMP,
};
pub use value::Value;

#[derive(Debug, Clone)]
pub struct RunError {
    pub msg: String,
    pub trace: Vec<String>,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error: {}", self.msg)?;
        for t in &self.trace {
            write!(f, "\n  at {t}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub ops: u64,
    pub fibers: u64,
    pub host_calls: u64,
    pub max_inflight: u32,
    pub deferred: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ready,
    Waiting,
    Done,
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wait {
    None,
    /// Re-executes its blocking op on wake (await/race/timeout/sleep).
    Reexec,
    /// Waits for exactly one host completion; spurious wakes are ignored.
    Host(u64),
    /// Waiting for an in-flight slot (max_parallel backpressure).
    Slot,
}

pub(crate) struct Frame {
    pub(crate) func: u32,
    pub(crate) pc: usize,
    pub(crate) base: usize,
}

struct Fiber {
    frames: Vec<Frame>,
    stack: Vec<Value>,
    state: State,
    wait: Wait,
    result: Option<Value>,
    children: Vec<u32>,
    awaiters: Vec<u32>,
    gen: u64,
    deadline: Option<Instant>,
}

enum Step {
    Suspend,
    Finished(Value),
    /// Only from the native-aware loop modes: an exit code for the dispatcher
    /// (`ZAL_NEXT`/`ZAL_JUMP`/`ZAL_EXIT_CALL`/`ZAL_EXIT_RET`).
    Op(u32),
}

/// `exec_loop` modes. `INTERP` is the plain interpreter (no native table);
/// `MIXED` interprets but hands control back to the dispatcher when a call or
/// return reaches a function with native code; `ONE` executes a single op for
/// `zal_rt_op`.
const MODE_INTERP: u8 = 0;
const MODE_MIXED: u8 = 1;
const MODE_ONE: u8 = 2;

/// Back-edges/calls between budget-deadline checks and fair preemption.
const SLICE: u32 = native::ZAL_SLICE;

pub struct Vm<'p, H: Host, B: Broker> {
    prog: &'p Program,
    consts: Vec<Value>,
    /// Fibers of the current run are `fibers[..nfib]`; entries beyond are
    /// kept from earlier runs and reinitialised in place (no move, no drop
    /// of the whole table per run).
    fibers: Vec<Fiber>,
    nfib: usize,
    ready: VecDeque<u32>,
    timers: BinaryHeap<Reverse<(Instant, u32, u64)>>,
    pub host: H,
    pub broker: B,
    calls: host::FxMap<u64, u32>,
    next_call: u64,
    inflight: u32,
    deferred: VecDeque<(u32, u32, Vec<Value>)>,
    budget: Budget,
    deadline: Option<Instant>,
    pub stats: Stats,
    pub output: Vec<String>,
    pub echo_print: bool,
    /// Wall-clock limit imposed by the embedding host, on top of any budget.
    time_limit: Option<Duration>,
    /// Buffers of finished fibers, reused by new ones (no allocation per spawn).
    pool: Vec<(Vec<Value>, Vec<Frame>)>,
    /// Children/awaiter lists of finished fibers, reused by new ones.
    id_pool: Vec<Vec<u32>>,
    /// Verified maximum operand depth per function (IR verifier).
    max_stack: Vec<u32>,
    invalid: Option<String>,
    /// Ops executed per (function, op kind) when enabled; for `zal run --profile`.
    pub profile: Option<HashMap<(u32, String), u64>>,
    /// Machine-code bodies per function (empty = interpret everything).
    native: Vec<Option<NativeFn>>,
    /// Per function [locals, max operand depth + 1], for `zal_rt_call`.
    fninfo: Vec<[u32; 2]>,
    /// Per function: the local slots `zal_rt_ret` must look at (static tag
    /// not plain, or unknown). Empty outer Vec = no trusted slot tags.
    drop_slots: Vec<Box<[u16]>>,
    /// Embedder hook, polled wherever the deadline is checked: `Some(msg)`
    /// traps the run with `msg` (cancellation, memory limits).
    interrupt: Option<fn() -> Option<String>>,
    /// Embedder hook asked before an allocation whose size the program's
    /// data decides (`range`, string concatenation, `str`): false traps.
    alloc_ok: Option<fn(usize) -> bool>,
    /// Keep `print` output in `output`; an embedder without a reader turns
    /// it off so a printing loop does not grow memory.
    pub keep_output: bool,
}

/// Deepest value `str`/`print` format when an allocation guard is installed.
const RENDER_MAX_DEPTH: usize = 10_000;

/// Longest path from `v` to a leaf, counting each shared sub-value once
/// (values are immutable, so they form a DAG, never a cycle). None when `v`
/// has more than `max_nodes` distinct containers. No recursion.
fn value_height(v: &Value, max_nodes: usize) -> Option<usize> {
    fn id(v: &Value) -> Option<usize> {
        match v {
            Value::List(xs) => Some(Rc::as_ptr(xs) as *const u8 as usize),
            Value::Tuple(xs) | Value::Record(xs) => Some(Rc::as_ptr(xs) as *const u8 as usize),
            Value::Map(m) => Some(Rc::as_ptr(m) as *const u8 as usize),
            Value::Variant(_, Some(x)) => Some(Rc::as_ptr(x) as *const u8 as usize),
            _ => None,
        }
    }
    fn kids(v: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
        match v {
            Value::List(xs) => Box::new(xs.iter()),
            Value::Tuple(xs) | Value::Record(xs) => Box::new(xs.iter()),
            Value::Map(m) => Box::new(m.values()),
            Value::Variant(_, Some(x)) => Box::new(std::iter::once(&**x)),
            _ => Box::new(std::iter::empty()),
        }
    }
    let Some(root) = id(v) else { return Some(1) };
    let mut height: HashMap<usize, usize> = HashMap::new();
    let mut todo: Vec<(&Value, bool)> = vec![(v, false)];
    while let Some((n, expanded)) = todo.pop() {
        let Some(nid) = id(n) else { continue };
        if expanded {
            let h = 1 + kids(n).map(|c| id(c).map_or(1, |cid| height.get(&cid).copied().unwrap_or(1))).max().unwrap_or(0);
            height.insert(nid, h);
            continue;
        }
        if height.contains_key(&nid) {
            continue;
        }
        if height.len() >= max_nodes {
            return None;
        }
        todo.push((n, true));
        todo.extend(kids(n).filter(|c| id(c).is_some_and(|cid| !height.contains_key(&cid))).map(|c| (c, false)));
    }
    height.get(&root).copied()
}

/// Move the top `n` stack values into a tuple/record payload (one allocation).
#[inline]
fn take_top(stack: &mut Vec<Value>, n: usize) -> Rc<[Value]> {
    let at = stack.len() - n;
    // SAFETY: the n values are moved out, then the length forgets them.
    unsafe {
        let xs = value::rc_slice_take(stack.as_ptr().add(at), n);
        stack.set_len(at);
        xs
    }
}

fn trap<T>(msg: impl Into<String>) -> Result<T, RunError> {
    Err(RunError { msg: msg.into(), trace: vec![] })
}

impl<'p, H: Host, B: Broker> Vm<'p, H, B> {
    pub fn new(prog: &'p Program, host: H, broker: B) -> Self {
        // Bytecode is verified once; the interpreter's unchecked stack access depends on it.
        let verified = prog.verify();
        let consts = prog
            .consts
            .iter()
            .map(|c| match c {
                Const::Int(n) => Value::Int(*n),
                Const::Str(s) => Value::Str(Rc::from(s.as_str())),
                Const::Bool(b) => Value::Bool(*b),
                Const::Dur(ms) => Value::Dur(*ms),
            })
            .collect();
        Vm {
            prog,
            consts,
            fibers: Vec::with_capacity(16),
            nfib: 0,
            ready: VecDeque::new(),
            timers: BinaryHeap::new(),
            host,
            broker,
            calls: Default::default(),
            next_call: 1,
            inflight: 0,
            deferred: VecDeque::new(),
            budget: Budget::default(),
            deadline: None,
            stats: Stats::default(),
            output: vec![],
            echo_print: false,
            pool: Vec::new(),
            id_pool: Vec::new(),
            time_limit: None,
            max_stack: verified.as_ref().cloned().unwrap_or_default(),
            invalid: verified.err(),
            profile: None,
            native: Vec::new(),
            fninfo: Vec::new(),
            drop_slots: Vec::new(),
            interrupt: None,
            alloc_ok: None,
            keep_output: true,
        }
    }

    /// Install a hook polled at every deadline check (loop back-edges and
    /// calls at the slice limit, and between waits). Returning `Some(msg)`
    /// traps the run, so a host can stop a run that only computes.
    pub fn set_interrupt(&mut self, f: fn() -> Option<String>) {
        self.interrupt = Some(f);
    }

    /// Install a hook asked before allocations sized by program data; it
    /// gets the byte count and returns false to trap instead of allocating.
    pub fn set_alloc_guard(&mut self, f: fn(usize) -> bool) {
        self.alloc_ok = Some(f);
    }

    #[inline]
    fn alloc_allowed(&self, bytes: usize) -> bool {
        self.alloc_ok.is_none_or(|f| f(bytes))
    }

    /// `v` as text, or None when the allocation guard refuses its size. The
    /// guard is asked while the text grows, so a value whose text is far
    /// larger than its memory (shared sub-values) stops early.
    fn render(&self, v: &Value) -> Option<String> {
        let Some(ok) = self.alloc_ok else { return Some(v.to_string()) };
        struct Bounded {
            s: String,
            ok: fn(usize) -> bool,
        }
        impl std::fmt::Write for Bounded {
            fn write_str(&mut self, x: &str) -> std::fmt::Result {
                let need = self.s.len() + x.len();
                if need > self.s.capacity() && !(self.ok)(need.max(self.s.capacity() * 2)) {
                    return Err(std::fmt::Error);
                }
                self.s.push_str(x);
                Ok(())
            }
        }
        // Formatting recurses once per nesting level; refuse values deeper
        // than a thread stack can format.
        if value_height(v, 1 << 24).is_none_or(|h| h > RENDER_MAX_DEPTH) {
            return None;
        }
        let mut b = Bounded { s: String::new(), ok };
        std::fmt::write(&mut b, format_args!("{v}")).ok()?;
        Some(b.s)
    }

    /// Polls the interrupt hook; `Some(msg)` means trap.
    #[inline]
    fn interrupted(&self) -> Option<String> {
        self.interrupt.and_then(|f| f())
    }

    /// Install machine-code bodies: `table[i]` runs function `i` instead of
    /// the interpreter (None = interpret). The code behind the pointers (e.g.
    /// a [`JitModule`]) must stay alive while this VM runs.
    pub fn set_native(&mut self, mut table: Vec<Option<NativeFn>>) {
        table.resize(self.prog.functions.len(), None);
        if table.iter().all(|f| f.is_none()) {
            table.clear();
        }
        self.fninfo = self.prog.functions.iter().zip(&self.max_stack).map(|(f, m)| [f.locals as u32, m + 1]).collect();
        // Slot tags only for a program the checker sealed in this process.
        let sealed = self.prog.functions.first().is_some_and(|f| f.slot_seal != 0 && f.slot_seal == zal_ir::tag_seal(self.prog));
        self.drop_slots = if sealed {
            self.prog
                .functions
                .iter()
                .map(|f| {
                    (0..f.locals)
                        .filter(|&l| {
                            let t = f.slot_tags.get(l as usize).copied().unwrap_or(0xFFFF);
                            // Plain static tags (Unit, Int, Bool, Dur, Task) never own anything;
                            // a slot not yet stored holds Unit.
                            !matches!(t & 0xFF, 0 | 1 | 2 | 4 | 10)
                        })
                        .collect()
                })
                .collect()
        } else {
            Vec::new()
        };
        self.native = table;
    }

    /// Host-imposed wall-clock limit for every run (traps like a budget timeout).
    pub fn set_time_limit(&mut self, d: Duration) {
        self.time_limit = Some(d);
    }

    /// Run `entry` to completion. The broker must grant every effect the entry
    /// declares before anything executes, and again for each individual call.
    pub fn run(&mut self, entry: &str, args: Vec<Value>) -> Result<Value, RunError> {
        if let Some(e) = &self.invalid {
            return trap(e.clone());
        }
        let Some((fidx, f)) = self.prog.function(entry) else {
            return trap(format!("no function `{entry}`"));
        };
        if f.params.len() != args.len() {
            return trap(format!("`{entry}` takes {} argument(s), got {}", f.params.len(), args.len()));
        }
        for e in &f.effects {
            if e.ends_with(".*") {
                continue; // wildcard requests are checked per concrete call
            }
            self.broker.authorize(e).map_err(|m| RunError { msg: format!("preflight denied: {m}"), trace: vec![] })?;
        }
        self.budget = f.budget.clone().unwrap_or_default();
        // The host's caps are ceilings; the declared budget can only go lower.
        let caps = self.broker.caps();
        fn tighter<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            }
        }
        self.budget.timeout_ms = tighter(self.budget.timeout_ms, caps.timeout_ms);
        self.budget.max_parallel = tighter(self.budget.max_parallel, caps.max_parallel);
        self.budget.max_tool_calls = tighter(self.budget.max_tool_calls, caps.max_tool_calls);
        self.deadline = self.budget.timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms.max(0) as u64));
        if let Some(l) = self.time_limit {
            let d = Instant::now() + l;
            self.deadline = Some(self.deadline.map_or(d, |x| x.min(d)));
            if self.budget.timeout_ms.is_none() {
                self.budget.timeout_ms = Some(l.as_millis() as i64);
            }
        }
        self.nfib = 0;
        let root = self.new_fiber(fidx, args);
        let mut turns: u64 = 0;
        loop {
            while let Some(fid) = self.ready.pop_front() {
                if self.fibers[fid as usize].state != State::Ready {
                    continue;
                }
                match self.exec(fid)? {
                    Step::Suspend => {}
                    Step::Finished(v) => self.finish(fid, v)?,
                    Step::Op(_) => unreachable!("dispatcher-internal step left exec"),
                }
                if self.fibers[root as usize].state == State::Done {
                    return Ok(self.fibers[root as usize].result.take().unwrap_or(Value::Unit));
                }
                turns += 1;
                if !self.calls.is_empty() && self.ready.len() > 64 && turns % 64 == 0 {
                    let now = Instant::now();
                    self.deliver(now)?;
                }
            }
            if self.fibers[root as usize].state == State::Done {
                return Ok(self.fibers[root as usize].result.take().unwrap_or(Value::Unit));
            }
            if self.calls.is_empty() && self.timers.is_empty() && self.deferred.is_empty() {
                return trap("deadlock: every task is waiting and nothing can wake them");
            }
            let mut until = self.timers.peek().map(|Reverse((t, _, _))| *t);
            if let Some(d) = self.deadline {
                until = Some(until.map_or(d, |u| u.min(d)));
            }
            if self.calls.is_empty() {
                if let Some(u) = until {
                    let now = Instant::now();
                    if u > now {
                        // With an interrupt hook, sleep in short steps so the
                        // hook is polled while the run waits on a timer.
                        let step = if self.interrupt.is_some() { (u - now).min(Duration::from_millis(10)) } else { u - now };
                        std::thread::sleep(step);
                    }
                }
            } else {
                self.deliver_until(until)?;
            }
            self.fire_timers();
            self.check_deadline()?;
        }
    }

    fn check_deadline(&self) -> Result<(), RunError> {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return trap(format!("budget exceeded: timeout of {} ms", self.budget.timeout_ms.unwrap_or(0)));
        }
        if let Some(m) = self.interrupted() {
            return trap(m);
        }
        Ok(())
    }

    fn deliver(&mut self, now: Instant) -> Result<(), RunError> {
        let done = self.host.poll(Some(now), self.prog);
        self.complete(done)
    }

    fn deliver_until(&mut self, until: Option<Instant>) -> Result<(), RunError> {
        let done = self.host.poll(until, self.prog);
        self.complete(done)
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn complete(&mut self, done: Vec<(u64, Result<Value, String>)>) -> Result<(), RunError> {
        for (id, r) in done {
            let Some(fid) = self.calls.remove(&id) else { continue };
            self.inflight -= 1;
            let v = r.map_err(|m| RunError { msg: format!("host failure: {m}"), trace: vec![] })?;
            let f = &mut self.fibers[fid as usize];
            if f.state == State::Waiting && f.wait == Wait::Host(id) {
                f.stack.push(v);
                f.wait = Wait::None;
                f.state = State::Ready;
                self.ready.push_back(fid);
            }
            self.start_deferred()?;
        }
        Ok(())
    }

    fn fire_timers(&mut self) {
        let now = Instant::now();
        while let Some(Reverse((t, fid, gen))) = self.timers.peek().copied() {
            if t > now {
                break;
            }
            self.timers.pop();
            let f = &self.fibers[fid as usize];
            if f.gen == gen {
                self.wake(fid);
            }
        }
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn new_fiber(&mut self, func: u32, args: Vec<Value>) -> u32 {
        let (mut stack, frames) = self.pool.pop().unwrap_or_default();
        stack.extend(args);
        self.start_fiber(func, stack, frames)
    }

    /// Start a child whose `argc` arguments are the top of `parent`'s stack.
    fn spawn_from(&mut self, func: u32, parent: &mut Vec<Value>, argc: usize) -> u32 {
        let (mut stack, frames) = self.pool.pop().unwrap_or_default();
        let locals = self.prog.functions[func as usize].locals as usize;
        // One reservation for the arguments, locals and verified operand depth
        // (what start_fiber's resize + reserve would do in two steps).
        let need = locals.max(argc) + self.max_stack[func as usize] as usize + 1;
        if stack.capacity() < need {
            stack.reserve(need);
        }
        let at = parent.len() - argc;
        // SAFETY: the argc values move from the parent's top into the fresh
        // (empty, pooled) stack, which has room; the parent forgets them.
        unsafe {
            std::ptr::copy_nonoverlapping(parent.as_ptr().add(at), stack.as_mut_ptr(), argc);
            parent.set_len(at);
            stack.set_len(argc);
        }
        self.start_fiber(func, stack, frames)
    }

    fn start_fiber(&mut self, func: u32, mut stack: Vec<Value>, mut frames: Vec<Frame>) -> u32 {
        let locals = self.prog.functions[func as usize].locals as usize;
        for _ in stack.len()..locals {
            stack.push(Value::Unit);
        }
        stack.reserve(self.max_stack[func as usize] as usize + 1);
        frames.push(Frame { func, pc: 0, base: 0 });
        let id = self.nfib as u32;
        self.nfib += 1;
        if let Some(f) = self.fibers.get_mut(id as usize) {
            // A fiber of an earlier run: reuse its slot (and its lists).
            f.frames = frames;
            f.stack = stack;
            f.state = State::Ready;
            f.wait = Wait::None;
            f.result = None;
            f.children.clear();
            f.awaiters.clear();
            if f.children.capacity() == 0 {
                f.children = self.id_pool.pop().unwrap_or_default();
            }
            if f.awaiters.capacity() == 0 {
                f.awaiters = self.id_pool.pop().unwrap_or_default();
            }
            f.gen = 0;
            f.deadline = None;
        } else {
            self.fibers.push(Fiber {
                frames,
                stack,
                state: State::Ready,
                wait: Wait::None,
                result: None,
                children: self.id_pool.pop().unwrap_or_default(),
                awaiters: self.id_pool.pop().unwrap_or_default(),
                gen: 0,
                deadline: None,
            });
        }
        self.ready.push_back(id);
        self.stats.fibers += 1;
        id
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn wake(&mut self, fid: u32) {
        let f = &mut self.fibers[fid as usize];
        if f.state == State::Waiting && f.wait == Wait::Reexec {
            f.state = State::Ready;
            f.wait = Wait::None;
            self.ready.push_back(fid);
        }
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn finish(&mut self, fid: u32, v: Value) -> Result<(), RunError> {
        let f = &mut self.fibers[fid as usize];
        f.state = State::Done;
        f.result = Some(v);
        f.stack = Vec::new();
        f.frames = Vec::new();
        let mut children = std::mem::take(&mut f.children);
        let mut awaiters = std::mem::take(&mut f.awaiters);
        // Structured concurrency: a finished task takes its unawaited children with it.
        for &c in &children {
            self.cancel_tree(c)?;
        }
        for &a in &awaiters {
            self.wake(a);
        }
        if self.id_pool.len() < 8192 {
            children.clear();
            awaiters.clear();
            self.id_pool.push(children);
            self.id_pool.push(awaiters);
        }
        Ok(())
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn cancel_tree(&mut self, fid: u32) -> Result<(), RunError> {
        let f = &mut self.fibers[fid as usize];
        if matches!(f.state, State::Done | State::Cancelled) {
            return Ok(());
        }
        let wait = f.wait;
        f.state = State::Cancelled;
        f.stack = Vec::new();
        f.frames = Vec::new();
        let children = std::mem::take(&mut f.children);
        let awaiters = std::mem::take(&mut f.awaiters);
        match wait {
            Wait::Host(id) => {
                self.host.cancel(id);
                if self.calls.remove(&id).is_some() {
                    self.inflight -= 1;
                }
            }
            Wait::Slot => self.deferred.retain(|(d, _, _)| *d != fid),
            _ => {}
        }
        for c in children {
            self.cancel_tree(c)?;
        }
        for a in awaiters {
            self.wake(a);
        }
        // A freed slot starts deferred calls; their denials and budget traps
        // must surface, not vanish.
        self.start_deferred()
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn start_deferred(&mut self) -> Result<(), RunError> {
        while self.budget.max_parallel.is_none_or(|m| self.inflight < m) {
            let Some((fid, ext, args)) = self.deferred.pop_front() else { break };
            match self.issue(fid, ext, args)? {
                Some(v) => {
                    let f = &mut self.fibers[fid as usize];
                    f.stack.push(v);
                    f.wait = Wait::None;
                    f.state = State::Ready;
                    self.ready.push_back(fid);
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Authorize and start a host call. Some(v) when the host answered synchronously.
    #[cfg_attr(feature = "profiling", inline(never))]
    fn issue(&mut self, fid: u32, ext_idx: u32, args: Vec<Value>) -> Result<Option<Value>, RunError> {
        let ext = &self.prog.externs[ext_idx as usize];
        self.stats.host_calls += 1;
        if let Some(max) = self.budget.max_tool_calls {
            if self.stats.host_calls > max {
                return trap(format!("budget exceeded: more than {max} tool calls"));
            }
        }
        self.broker
            .authorize(&ext.effect)
            .map_err(|m| RunError { msg: format!("capability denied for `{}`: {m}", ext.name), trace: vec![] })?;
        let id = self.next_call;
        self.next_call += 1;
        match self.host.call(id, ext, args, self.prog) {
            Reply::Done(Ok(v)) => Ok(Some(v)),
            Reply::Done(Err(m)) => trap(format!("host failure in `{}`: {m}", ext.name)),
            Reply::Pending => {
                self.calls.insert(id, fid);
                self.inflight += 1;
                self.stats.max_inflight = self.stats.max_inflight.max(self.inflight);
                let f = &mut self.fibers[fid as usize];
                f.state = State::Waiting;
                f.wait = Wait::Host(id);
                Ok(None)
            }
        }
    }

    fn trace_of(&self, frames: &[Frame], pc_now: usize) -> Vec<String> {
        let mut out = Vec::new();
        for (i, fr) in frames.iter().enumerate().rev() {
            let f = &self.prog.functions[fr.func as usize];
            let pc = if i == frames.len() - 1 { pc_now } else { fr.pc };
            let line = f.lines.get(pc.saturating_sub(1)).copied().unwrap_or(0);
            out.push(format!("{} (line {line})", f.name));
        }
        out
    }

    fn task_of(&self, v: &Value) -> Result<u32, RunError> {
        match v {
            Value::Task(t) => Ok(*t),
            _ => trap("internal: expected a task handle"),
        }
    }

    /// Run one fiber until it blocks or finishes.
    #[cfg_attr(feature = "profiling", inline(never))]
    fn exec(&mut self, fid: u32) -> Result<Step, RunError> {
        let mut stack = std::mem::take(&mut self.fibers[fid as usize].stack);
        let mut frames = std::mem::take(&mut self.fibers[fid as usize].frames);
        let mut ops = 0u64;
        let r = self.exec_inner(fid, &mut stack, &mut frames, &mut ops);
        self.stats.ops += ops;
        match r {
            Ok(Step::Suspend) => {
                let f = &mut self.fibers[fid as usize];
                f.stack = stack;
                f.frames = frames;
                Ok(Step::Suspend)
            }
            Ok(done) => {
                // Recycle the buffers; the values are dropped here.
                stack.clear();
                frames.clear();
                if self.pool.len() < 4096 {
                    self.pool.push((stack, frames));
                }
                Ok(done)
            }
            Err(mut e) => {
                if e.trace.is_empty() {
                    let pc = frames.last().map(|f| f.pc).unwrap_or(0);
                    e.trace = self.trace_of(&frames, pc);
                }
                Err(e)
            }
        }
    }

    fn exec_inner(&mut self, fid: u32, stack: &mut Vec<Value>, frames: &mut Vec<Frame>, ops_out: &mut u64) -> Result<Step, RunError> {
        let mut ops: u64 = 0;
        let r = if self.native.is_empty() {
            let mut slice = 0;
            self.exec_loop::<MODE_INTERP>(fid, stack, frames, &mut ops, &mut slice)
        } else {
            self.exec_mixed(fid, stack, frames, &mut ops)
        };
        *ops_out = ops;
        r
    }

    /// Dispatcher for programs with machine code: runs the top frame natively
    /// when its function has a body, else in the interpreter, and follows
    /// calls and returns across the two until the fiber blocks or finishes.
    /// One session and one context serve the whole turn; native->native
    /// calls and returns go straight from body to body.
    fn exec_mixed(&mut self, fid: u32, stack: &mut Vec<Value>, frames: &mut Vec<Frame>, ops: &mut u64) -> Result<Step, RunError> {
        let mut sess = rt::Session {
            op: Self::op_entry,
            tick: Self::tick_entry,
            prog: self.prog,
            consts: self.consts.as_ptr(),
            max_stack: self.max_stack.as_ptr(),
            fninfo: self.fninfo.as_ptr(),
            drop_slots: if self.drop_slots.is_empty() { std::ptr::null() } else { self.drop_slots.as_ptr() },
            native: self.native.as_ptr(),
            vm: &mut *self as *mut Self as *mut std::ffi::c_void,
            fid,
            profiling: self.profile.is_some(),
            stack: &mut *stack as *mut Vec<Value>,
            frames: &mut *frames as *mut Vec<Frame>,
            ops: 0,
            err: None,
            done: None,
        };
        let mut ctx = ZalCtx {
            vm: &mut sess as *mut rt::Session as *mut std::ffi::c_void,
            stack: std::ptr::null_mut(),
            sp: 0,
            base: 0,
            pc: 0,
            func: 0,
            // One preemption counter for the whole turn, as in the interpreter.
            slice: 0,
        };
        // SAFETY: while machine code runs, `self`, `stack` and `frames` are
        // only reached through `sess`; the dispatcher touches them again only
        // after the body returned.
        let r = unsafe { self.exec_mixed_inner(&mut sess, &mut ctx, stack, frames) };
        *ops += sess.ops;
        r
    }

    unsafe fn exec_mixed_inner(&mut self, sess: &mut rt::Session, ctx: &mut ZalCtx, stack: &mut Vec<Value>, frames: &mut Vec<Frame>) -> Result<Step, RunError> {
        let fid = sess.fid;
        loop {
            let fr = frames.last().unwrap();
            let fi = fr.func;
            let Some(mut body) = self.native[fi as usize] else {
                match self.exec_loop::<MODE_MIXED>(fid, stack, frames, &mut sess.ops, &mut ctx.slice)? {
                    Step::Op(_) => continue,
                    s => return Ok(s),
                }
            };
            ctx.stack = stack.as_mut_ptr();
            ctx.sp = stack.len() as u64;
            ctx.base = fr.base as u64;
            ctx.pc = fr.pc as u32;
            ctx.func = fi;
            // Hot native->native path: after CALL/RET the runtime left ctx
            // pointing at the new top frame.
            let code = loop {
                let code = body(ctx, ctx.pc);
                if code == ZAL_EXIT_CALL || code == ZAL_EXIT_RET {
                    if let Some(b) = *self.native.get_unchecked(ctx.func as usize) {
                        body = b;
                        continue;
                    }
                }
                break code;
            };
            debug_assert!(ctx.sp as usize <= stack.capacity());
            stack.set_len(ctx.sp as usize);
            match code {
                ZAL_EXIT_CALL | ZAL_EXIT_RET => {}
                ZAL_EXIT_SUSPEND => return Ok(Step::Suspend),
                ZAL_EXIT_TRAP => {
                    return Err(sess.err.take().unwrap_or_else(|| RunError { msg: "internal: native code trapped without a recorded error".into(), trace: vec![] }));
                }
                ZAL_EXIT_DONE => return Ok(Step::Finished(sess.done.take().unwrap_or(Value::Unit))),
                c => return trap(format!("internal: native code of `{}` returned exit code {c}", self.prog.functions[ctx.func as usize].name)),
            }
        }
    }

    /// Slow generic path of `zal_rt_op` for this `Vm<H, B>`: one op through
    /// the interpreter's own code (ops the lean path in `rt` does not cover).
    unsafe fn op_entry(ctx: *mut ZalCtx, pc: u32) -> u32 {
        let s = &mut *((*ctx).vm as *mut rt::Session);
        let vm = &mut *(s.vm as *mut Self);
        let stack = &mut *s.stack;
        let frames = &mut *s.frames;
        debug_assert!((*ctx).sp as usize <= stack.capacity());
        stack.set_len((*ctx).sp as usize);
        if !s.profiling {
            if let Some(code) = vm.sched_op(s, ctx, pc, stack, frames) {
                rt::sync_ctx(ctx, stack, frames);
                return code;
            }
        }
        frames.last_mut().unwrap().pc = pc as usize;
        let mut slice = (*ctx).slice;
        let r = vm.exec_loop::<MODE_ONE>(s.fid, stack, frames, &mut s.ops, &mut slice);
        (*ctx).slice = slice;
        let code = match r {
            Ok(Step::Op(c)) => c,
            Ok(Step::Suspend) => ZAL_EXIT_SUSPEND,
            Ok(Step::Finished(v)) => {
                s.done = Some(v);
                ZAL_EXIT_DONE
            }
            Err(e) => {
                s.err = Some(e);
                ZAL_EXIT_TRAP
            }
        };
        rt::sync_ctx(ctx, stack, frames);
        code
    }

    /// Spawn/Await/AwaitAll/Extern for native code without setting up the
    /// interpreter loop: the same steps as their `exec_loop` arms. None =
    /// not handled here (other ops, error paths): run `exec_loop` instead.
    #[inline(always)]
    unsafe fn sched_op(&mut self, s: &mut rt::Session, ctx: *mut ZalCtx, pc: u32, stack: &mut Vec<Value>, frames: &mut [Frame]) -> Option<u32> {
        let fid = s.fid;
        let op = *self.prog.functions[(*ctx).func as usize].code.get(pc as usize)?;
        let suspend_here = |vm: &mut Self, frames: &mut [Frame]| {
            // Re-execute this op on wake.
            frames.last_mut().unwrap().pc = pc as usize;
            let f = &mut vm.fibers[fid as usize];
            f.state = State::Waiting;
            f.wait = Wait::Reexec;
            ZAL_EXIT_SUSPEND
        };
        match op {
            Op::Spawn(fi, argc) => {
                let c = self.spawn_from(fi, stack, argc as usize);
                self.fibers[fid as usize].children.push(c);
                stack.push(Value::Task(c));
                Some(native::ZAL_NEXT)
            }
            Op::Await => {
                let Some(Value::Task(c)) = stack.last() else { return None };
                let c = *c;
                match self.fibers[c as usize].state {
                    State::Done => {
                        let v = self.fibers[c as usize].result.clone().unwrap_or(Value::Unit);
                        *stack.last_mut().unwrap() = v;
                        Some(native::ZAL_NEXT)
                    }
                    State::Cancelled => None,
                    _ => {
                        self.add_awaiter(c, fid);
                        Some(suspend_here(self, frames))
                    }
                }
            }
            Op::AwaitAll(n) => {
                let at = stack.len() - n as usize;
                let mut blocked = None;
                for h in &stack[at..] {
                    let Value::Task(c) = h else { return None };
                    match self.fibers[*c as usize].state {
                        State::Done => {}
                        State::Cancelled => return None,
                        _ => {
                            blocked = Some(*c);
                            break;
                        }
                    }
                }
                if let Some(c) = blocked {
                    self.add_awaiter(c, fid);
                    return Some(suspend_here(self, frames));
                }
                let vs: Rc<[Value]> = stack[at..]
                    .iter()
                    .map(|h| match h {
                        Value::Task(c) => self.fibers[*c as usize].result.clone().unwrap_or(Value::Unit),
                        _ => Value::Unit,
                    })
                    .collect();
                stack.truncate(at);
                stack.push(Value::Tuple(vs));
                Some(native::ZAL_NEXT)
            }
            Op::Builtin(Builtin::AwaitAny, 1) => {
                // The interpreter's AwaitAny arm without the loop set-up;
                // anything unusual (non-task, empty, all cancelled) goes there.
                let Some(Value::List(hs)) = stack.last() else { return None };
                let mut live = false;
                for (i, h) in hs.iter().enumerate() {
                    let Value::Task(c) = h else { return None };
                    match self.fibers[*c as usize].state {
                        State::Done => {
                            let v = self.fibers[*c as usize].result.clone().unwrap_or(Value::Unit);
                            // `let (k, r) = await_any(..)`: the next op splits
                            // the pair; push both directly (no tuple) and
                            // resume after the split.
                            let code = &self.prog.functions[(*ctx).func as usize].code;
                            if code.get(pc as usize + 1) == Some(&Op::SplitTuple(2)) {
                                *stack.last_mut().unwrap() = Value::Int(i as i64);
                                stack.push(v);
                                frames.last_mut().unwrap().pc = pc as usize + 2;
                                return Some(native::ZAL_JUMP);
                            }
                            *stack.last_mut().unwrap() = Value::Tuple(Rc::new([Value::Int(i as i64), v]));
                            return Some(native::ZAL_NEXT);
                        }
                        State::Cancelled => {}
                        _ => live = true,
                    }
                }
                if !live {
                    return None;
                }
                for h in hs.iter() {
                    let Value::Task(c) = h else { unreachable!() };
                    let a = &mut self.fibers[*c as usize].awaiters;
                    if !a.contains(&fid) {
                        a.push(fid);
                    }
                }
                Some(suspend_here(self, frames))
            }
            Op::Extern(ei, argc) => {
                frames.last_mut().unwrap().pc = pc as usize + 1;
                let args = stack.split_off(stack.len() - argc as usize);
                if self.budget.max_parallel.is_some_and(|m| self.inflight >= m) {
                    self.deferred.push_back((fid, ei, args));
                    self.stats.deferred += 1;
                    let f = &mut self.fibers[fid as usize];
                    f.state = State::Waiting;
                    f.wait = Wait::Slot;
                    return Some(ZAL_EXIT_SUSPEND);
                }
                Some(match self.issue(fid, ei, args) {
                    Ok(Some(v)) => {
                        stack.push(v);
                        native::ZAL_NEXT
                    }
                    Ok(None) => ZAL_EXIT_SUSPEND,
                    Err(e) => {
                        s.err = Some(e);
                        ZAL_EXIT_TRAP
                    }
                })
            }
            _ => None,
        }
    }

    /// Slow part of `zal_rt_tick` (slice limit reached): the interpreter's
    /// deadline check and fair preemption, resuming at `pc`.
    unsafe fn tick_entry(ctx: *mut ZalCtx, pc: u32) -> u32 {
        let s = &mut *((*ctx).vm as *mut rt::Session);
        let vm = &mut *(s.vm as *mut Self);
        let stack = &mut *s.stack;
        let frames = &mut *s.frames;
        stack.set_len((*ctx).sp as usize);
        frames.last_mut().unwrap().pc = pc as usize;
        (*ctx).pc = pc;
        (*ctx).slice = 0;
        if vm.deadline.is_some_and(|d| Instant::now() >= d) {
            s.err = Some(RunError { msg: format!("budget exceeded: timeout of {} ms", vm.budget.timeout_ms.unwrap_or(0)), trace: vec![] });
            return ZAL_EXIT_TRAP;
        }
        if let Some(m) = vm.interrupted() {
            s.err = Some(RunError { msg: m, trace: vec![] });
            return ZAL_EXIT_TRAP;
        }
        if !vm.ready.is_empty() {
            vm.fibers[s.fid as usize].state = State::Ready;
            vm.ready.push_back(s.fid);
            return ZAL_EXIT_SUSPEND;
        }
        0
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn exec_loop<const MODE: u8>(&mut self, fid: u32, stack: &mut Vec<Value>, frames: &mut Vec<Frame>, ops_out: &mut u64, slice_io: &mut u32) -> Result<Step, RunError> {
        let prog = self.prog;
        let fr = frames.last().unwrap();
        let mut func = &prog.functions[fr.func as usize];
        let mut pc = fr.pc;
        let mut base = fr.base;
        let mut slice: u32 = *slice_io;
        // MODE_ONE: what the single op did is read off the frame count and pc.
        let (start_pc, start_frames) = (pc, frames.len());
        // Hot-loop state kept in locals so the compiler can hold it in registers.
        let profiling = self.profile.is_some();
        let ops = ops_out;

        macro_rules! save {
            () => {
                frames.last_mut().unwrap().pc = pc;
            };
        }
        macro_rules! suspend_here {
            () => {{
                // Re-execute this op on wake.
                frames.last_mut().unwrap().pc = pc - 1;
                let f = &mut self.fibers[fid as usize];
                f.state = State::Waiting;
                f.wait = Wait::Reexec;
                return Ok(Step::Suspend);
            }};
        }
        macro_rules! fail {
            ($($t:tt)*) => {{
                save!();
                return trap(format!($($t)*));
            }};
        }
        // Unchecked operand stack: the IR verifier proved depth bounds and the
        // frame reserved `max_stack` slots, so these never leave the buffer.
        macro_rules! pop {
            () => {{
                debug_assert!(!stack.is_empty());
                unsafe {
                    let l = stack.len() - 1;
                    stack.set_len(l);
                    stack.as_ptr().add(l).read()
                }
            }};
        }
        macro_rules! push {
            ($v:expr) => {{
                let v = $v;
                debug_assert!(stack.len() < stack.capacity());
                unsafe {
                    let l = stack.len();
                    stack.as_mut_ptr().add(l).write(v);
                    stack.set_len(l + 1);
                }
            }};
        }
        macro_rules! local {
            ($s:expr) => {{
                debug_assert!(base + ($s as usize) < stack.len());
                unsafe { stack.get_unchecked_mut(base + $s as usize) }
            }};
        }
        macro_rules! int2 {
            ($f:expr, $name:expr) => {{
                let b = pop!();
                let a = pop!();
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => match $f(x, y) {
                        Some(r) => push!(Value::Int(r)),
                        None => fail!("integer {} overflow or division by zero", $name),
                    },
                    (Value::Dur(x), Value::Dur(y)) => match $f(x, y) {
                        Some(r) => push!(Value::Dur(r)),
                        None => fail!("duration {} overflow", $name),
                    },
                    (a, b) => fail!("internal: bad operands {a} {b}"),
                }
            }};
        }
        macro_rules! cmp {
            ($op:tt) => {{
                let b = pop!();
                let a = pop!();
                let r = match (&a, &b) {
                    (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => x $op y,
                    (Value::Str(x), Value::Str(y)) => x $op y,
                    _ => fail!("internal: cannot compare {a} and {b}"),
                };
                push!(Value::Bool(r));
            }};
        }

        // Budget deadline and fair preemption are checked on loop back-edges
        // and calls only: every unbounded execution passes one of them, and
        // straight-line code pays nothing.
        macro_rules! tick {
            () => {
                slice += 1;
                if slice >= SLICE {
                    slice = 0;
                    if self.deadline.is_some_and(|d| Instant::now() >= d) {
                        fail!("budget exceeded: timeout of {} ms", self.budget.timeout_ms.unwrap_or(0));
                    }
                    if let Some(m) = self.interrupted() {
                        fail!("{m}");
                    }
                    if !self.ready.is_empty() {
                        // Fair preemption so one busy loop cannot starve sibling tasks.
                        save!();
                        self.fibers[fid as usize].state = State::Ready;
                        self.ready.push_back(fid);
                        return Ok(Step::Suspend);
                    }
                }
            };
        }

        // Every taken backward branch counts as a tick, conditional ones too:
        // jump threading can turn a loop's back edge into a conditional jump,
        // and a crafted IR loop must not escape the timeout either.
        macro_rules! goto {
            ($t:expr) => {{
                let t = $t as usize;
                let back = t < pc;
                pc = t;
                if back {
                    tick!();
                }
            }};
        }

        loop {
            let op = func.code[pc];
            if profiling {
                *ops += 1;
                let p = self.profile.as_mut().unwrap();
                let fi = frames.last().unwrap().func;
                let name = format!("{:?}", op);
                let kind = name.split('(').next().unwrap_or("").to_string();
                *p.entry((fi, kind.clone())).or_insert(0) += 1;
                // Op pairs within straight-line code, for superinstruction selection.
                if pc + 1 < func.code.len() {
                    let next = format!("{:?}", func.code[pc + 1]);
                    let nk = next.split('(').next().unwrap_or("").to_string();
                    *p.entry((u32::MAX, format!("{kind} > {nk}"))).or_insert(0) += 1;
                }
            }
            pc += 1;
            match op {
                Op::Const(i) => push!(self.consts[i as usize].clone()),
                Op::Unit => push!(Value::Unit),
                Op::LoadLocal(s) => push!(local!(s).clone()),
                Op::TakeLocal(s) => {
                    let v = std::mem::replace(local!(s), Value::Unit);
                    push!(v);
                }
                Op::StoreLocal(s) => {
                    let v = pop!();
                    *local!(s) = v;
                }
                Op::Pop => {
                    pop!();
                }
                Op::Add => {
                    let b = pop!();
                    let a = pop!();
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => match x.checked_add(y) {
                            Some(r) => push!(Value::Int(r)),
                            None => fail!("integer overflow in +"),
                        },
                        (Value::Dur(x), Value::Dur(y)) => push!(Value::Dur(x.saturating_add(y))),
                        (Value::Str(x), Value::Str(y)) => {
                            if !self.alloc_allowed(x.len() + y.len()) {
                                fail!("out of memory: string of {} bytes exceeds the run's memory limit", x.len() + y.len());
                            }
                            let mut s = String::with_capacity(x.len() + y.len());
                            s.push_str(&x);
                            s.push_str(&y);
                            push!(Value::Str(Rc::from(s)));
                        }
                        (a, b) => fail!("internal: cannot add {a} and {b}"),
                    }
                }
                Op::Sub => int2!(i64::checked_sub, "-"),
                Op::Mul => int2!(i64::checked_mul, "*"),
                Op::Div => int2!(i64::checked_div, "/"),
                Op::Mod => int2!(i64::checked_rem, "%"),
                Op::Neg => match pop!() {
                    Value::Int(x) => push!(Value::Int(x.checked_neg().unwrap_or(i64::MIN))),
                    Value::Dur(x) => push!(Value::Dur(-x)),
                    v => fail!("internal: cannot negate {v}"),
                },
                Op::Not => match pop!() {
                    Value::Bool(b) => push!(Value::Bool(!b)),
                    v => fail!("internal: `!` on {v}"),
                },
                Op::Eq => {
                    let b = pop!();
                    let a = pop!();
                    push!(Value::Bool(a == b));
                }
                Op::Ne => {
                    let b = pop!();
                    let a = pop!();
                    push!(Value::Bool(a != b));
                }
                Op::Lt => cmp!(<),
                Op::Le => cmp!(<=),
                Op::Gt => cmp!(>),
                Op::Ge => cmp!(>=),
                Op::Jump(t) => {
                    let back = (t as usize) < pc;
                    pc = t as usize;
                    if back {
                        tick!();
                    }
                }
                Op::JumpIfFalse(t) => {
                    if let Value::Bool(false) = pop!() {
                        goto!(t);
                    }
                }
                Op::JumpIfTrue(t) => {
                    if let Value::Bool(true) = pop!() {
                        goto!(t);
                    }
                }
                Op::Call(fi, argc) => {
                    save!();
                    let callee = &prog.functions[fi as usize];
                    let nb = stack.len() - argc as usize;
                    stack.resize(nb + (callee.locals as usize).max(argc as usize), Value::Unit);
                    stack.reserve(self.max_stack[fi as usize] as usize + 1);
                    frames.push(Frame { func: fi, pc: 0, base: nb });
                    func = callee;
                    pc = 0;
                    base = nb;
                    tick!();
                    if frames.len() > 10_000 {
                        fail!("call depth limit exceeded (10000 frames)");
                    }
                    if MODE == MODE_MIXED && self.native[fi as usize].is_some() {
                        *slice_io = slice;
                        return Ok(Step::Op(ZAL_EXIT_CALL));
                    }
                }
                Op::Ret => {
                    let v = pop!();
                    stack.truncate(base);
                    frames.pop();
                    match frames.last() {
                        None => return Ok(Step::Finished(v)),
                        Some(fr) => {
                            let caller = fr.func;
                            func = &prog.functions[caller as usize];
                            pc = fr.pc;
                            base = fr.base;
                            push!(v);
                            if MODE == MODE_MIXED && self.native[caller as usize].is_some() {
                                *slice_io = slice;
                                return Ok(Step::Op(ZAL_EXIT_RET));
                            }
                        }
                    }
                }
                Op::Extern(ei, argc) => {
                    save!();
                    let args = stack.split_off(stack.len() - argc as usize);
                    if self.budget.max_parallel.is_some_and(|m| self.inflight >= m) {
                        self.deferred.push_back((fid, ei, args));
                        self.stats.deferred += 1;
                        let f = &mut self.fibers[fid as usize];
                        f.state = State::Waiting;
                        f.wait = Wait::Slot;
                        return Ok(Step::Suspend);
                    }
                    match self.issue(fid, ei, args)? {
                        Some(v) => push!(v),
                        None => return Ok(Step::Suspend),
                    }
                }
                Op::MakeTuple(n) => {
                    let xs = take_top(stack, n as usize);
                    push!(Value::Tuple(xs));
                }
                Op::TupleGet(i) => match pop!() {
                    Value::Tuple(xs) => match xs.get(i as usize) {
                        Some(x) => push!(x.clone()),
                        None => fail!("internal: tuple index {i}"),
                    },
                    v => fail!("internal: tuple access on {v}"),
                },
                Op::MakeRecord(n) => {
                    let xs = take_top(stack, n as usize);
                    push!(Value::Record(xs));
                }
                Op::FieldGet(i) => match pop!() {
                    Value::Record(xs) => match xs.get(i as usize) {
                        Some(x) => push!(x.clone()),
                        None => fail!("internal: field index {i}"),
                    },
                    v => fail!("internal: field access on {v}"),
                },
                Op::MakeList(n) => {
                    let xs = stack.split_off(stack.len() - n as usize);
                    push!(Value::List(Rc::new(xs)));
                }
                Op::Index => {
                    let i = pop!();
                    let l = pop!();
                    match (l, i) {
                        (Value::List(xs), Value::Int(i)) => match usize::try_from(i).ok().and_then(|i| xs.get(i)) {
                            Some(v) => push!(v.clone()),
                            None => fail!("index {i} out of bounds (length {})", xs.len()),
                        },
                        (l, i) => fail!("internal: index {l}[{i}]"),
                    }
                }
                Op::IncLocal(sl, k) => {
                    let at = base + sl as usize;
                    match &stack[at] {
                        Value::Int(x) => match x.checked_add(k as i64) {
                            Some(r) => stack[at] = Value::Int(r),
                            None => fail!("integer overflow in +"),
                        },
                        v => fail!("internal: increment of {v}"),
                    }
                }
                Op::LoadLocal2(a, b) => {
                    let x = local!(a).clone();
                    let y = local!(b).clone();
                    push!(x);
                    push!(y);
                }
                Op::JumpIfNotLt(t) => {
                    let b = pop!();
                    let a = pop!();
                    let lt = match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => x < y,
                        (Value::Str(x), Value::Str(y)) => x < y,
                        _ => fail!("internal: cannot compare {a} and {b}"),
                    };
                    if !lt {
                        goto!(t);
                    }
                }
                Op::StoreLoad(sl) => {
                    let v = stack.last().unwrap().clone();
                    *local!(sl) = v;
                }
                Op::SplitTuple(n) => match pop!() {
                    // The verifier reserved exactly n slots: any other length would
                    // over- or underrun the unchecked stack.
                    Value::Tuple(xs) if xs.len() != n as usize => fail!("internal: split of {}-tuple into {n}", xs.len()),
                    Value::Tuple(xs) => value::rc_slice_each(xs, |x| push!(x)),
                    v => fail!("internal: split of {v}"),
                },
                Op::JumpIfNotTag(tag, t) => {
                    let hit = match pop!() {
                        Value::Variant(x, _) => x == tag,
                        Value::Bool(b) => b as u16 == tag,
                        v => fail!("internal: tag test on {v}"),
                    };
                    if !hit {
                        goto!(t);
                    }
                }
                Op::IndexLL(a, b) => {
                    let i = match local!(b) {
                        Value::Int(i) => *i,
                        v => fail!("internal: index {v}"),
                    };
                    let v = match local!(a) {
                        Value::List(xs) => match usize::try_from(i).ok().and_then(|i| xs.get(i)) {
                            Some(v) => v.clone(),
                            None => fail!("index {i} out of bounds (length {})", xs.len()),
                        },
                        v => fail!("internal: index into {v}"),
                    };
                    push!(v);
                }
                Op::LoadField(a, f) => {
                    let v = match local!(a) {
                        Value::Record(xs) => match xs.get(f as usize) {
                            Some(x) => x.clone(),
                            None => fail!("internal: field index {f}"),
                        },
                        v => fail!("internal: field access on {v}"),
                    };
                    push!(v);
                }
                Op::LoadPayload(a) => {
                    let v = match local!(a) {
                        Value::Variant(_, Some(p)) => (**p).clone(),
                        v => fail!("internal: payload of {v}"),
                    };
                    push!(v);
                }
                Op::MovePayload(a, b) => {
                    let v = match local!(a) {
                        Value::Variant(_, Some(p)) => (**p).clone(),
                        v => fail!("internal: payload of {v}"),
                    };
                    *local!(b) = v;
                }
                Op::UnwrapOrJump(r, t) => match pop!() {
                    Value::Variant(1, Some(p)) => *local!(r) = Rc::try_unwrap(p).unwrap_or_else(|p| (*p).clone()),
                    Value::Variant(_, _) => goto!(t),
                    v => fail!("internal: unwrap of {v}"),
                },
                Op::MapGetJump(r, t) => {
                    let k = pop!();
                    let m = pop!();
                    let got = match (&m, &k) {
                        (Value::Map(m), Value::Str(k)) => m.get(k).cloned(),
                        _ => fail!("internal: map_get"),
                    };
                    match got {
                        Some(v) => *local!(r) = v,
                        None => goto!(t),
                    }
                }
                Op::LenLocal(a) => {
                    let l = match local!(a) {
                        Value::List(xs) => xs.len(),
                        Value::Map(m) => m.len(),
                        Value::Str(s) => s.chars().count(),
                        v => fail!("internal: len of {v}"),
                    };
                    push!(Value::Int(l as i64));
                }
                Op::UpdLocal(b, a) => match b {
                    Builtin::Push => {
                        let x = pop!();
                        match local!(a) {
                            Value::List(xs) => value::list_push(xs, x),
                            v => fail!("internal: push on {v}"),
                        }
                    }
                    Builtin::Remove => {
                        let Value::Int(i) = pop!() else { fail!("internal: remove index") };
                        match local!(a) {
                            Value::List(xs) => {
                                let len = xs.len();
                                match usize::try_from(i).ok().filter(|i| *i < len) {
                                    Some(i) => {
                                        Rc::make_mut(xs).remove(i);
                                    }
                                    None => fail!("remove: index {i} out of bounds (length {len})"),
                                }
                            }
                            v => fail!("internal: remove on {v}"),
                        }
                    }
                    Builtin::Set => {
                        let x = pop!();
                        let Value::Int(i) = pop!() else { fail!("internal: set index") };
                        match local!(a) {
                            Value::List(xs) => {
                                let len = xs.len();
                                match usize::try_from(i).ok().filter(|i| *i < len) {
                                    Some(i) => Rc::make_mut(xs)[i] = x,
                                    None => fail!("set: index {i} out of bounds (length {len})"),
                                }
                            }
                            v => fail!("internal: set on {v}"),
                        }
                    }
                    Builtin::MapSet => {
                        let v = pop!();
                        let Value::Str(k) = pop!() else { fail!("internal: map key") };
                        match local!(a) {
                            Value::Map(m) => {
                                Rc::make_mut(m).insert(k, v);
                            }
                            x => fail!("internal: map_set on {x}"),
                        }
                    }
                    _ => fail!("internal: UpdLocal({b:?})"),
                },
                Op::ConcatN(n) => {
                    let at = stack.len() - n as usize;
                    let mut total = 0;
                    for v in &stack[at..] {
                        if let Value::Str(x) = v {
                            total += x.len();
                        }
                    }
                    if !self.alloc_allowed(total) {
                        fail!("out of memory: string of {total} bytes exceeds the run's memory limit");
                    }
                    let mut out = String::with_capacity(total);
                    for v in stack.drain(at..) {
                        match v {
                            Value::Str(x) => out.push_str(&x),
                            v => fail!("internal: concat of {v}"),
                        }
                    }
                    push!(Value::Str(Rc::from(out)));
                }
                Op::ForNext(b, exit) => {
                    let at = base + b as usize;
                    let i = match &stack[at + 1] {
                        Value::Int(i) => *i as usize,
                        _ => fail!("internal: loop index"),
                    };
                    let next = match &stack[at] {
                        Value::List(xs) => xs.get(i).cloned(),
                        _ => fail!("internal: loop over non-list"),
                    };
                    match next {
                        Some(v) => {
                            stack[at + 2] = v;
                            stack[at + 1] = Value::Int(i as i64 + 1);
                        }
                        None => goto!(exit),
                    }
                }
                Op::MakeVariant(tag, has) => {
                    let p = if has { Some(Rc::new(pop!())) } else { None };
                    push!(Value::Variant(tag, p));
                }
                Op::TagIs(tag) => match pop!() {
                    Value::Variant(t, _) => push!(Value::Bool(t == tag)),
                    Value::Bool(b) => push!(Value::Bool(b as u16 == tag)),
                    v => fail!("internal: tag test on {v}"),
                },
                Op::Payload => match pop!() {
                    Value::Variant(_, Some(p)) => push!(Rc::try_unwrap(p).unwrap_or_else(|p| (*p).clone())),
                    v => fail!("internal: payload of {v}"),
                },
                Op::Spawn(fi, argc) => {
                    let c = self.spawn_from(fi, stack, argc as usize);
                    self.fibers[fid as usize].children.push(c);
                    push!(Value::Task(c));
                }
                Op::Await => {
                    let c = self.task_of(stack.last().unwrap())?;
                    match self.fibers[c as usize].state {
                        State::Done => {
                            stack.pop();
                            push!(self.fibers[c as usize].result.clone().unwrap_or(Value::Unit));
                        }
                        State::Cancelled => fail!("awaited a cancelled task"),
                        _ => {
                            self.add_awaiter(c, fid);
                            suspend_here!();
                        }
                    }
                }
                Op::AwaitAll(n) => {
                    let at = stack.len() - n as usize;
                    let mut blocked = None;
                    for h in &stack[at..] {
                        let c = self.task_of(h)?;
                        match self.fibers[c as usize].state {
                            State::Done => {}
                            State::Cancelled => fail!("awaited a cancelled task"),
                            _ => {
                                blocked = Some(c);
                                break;
                            }
                        }
                    }
                    if let Some(c) = blocked {
                        self.add_awaiter(c, fid);
                        suspend_here!();
                    }
                    let hs = stack.split_off(at);
                    let vs = hs.iter().map(|h| self.fibers[self.task_of(h).unwrap() as usize].result.clone().unwrap_or(Value::Unit)).collect();
                    push!(Value::Tuple(vs));
                }
                Op::Race(n) => {
                    let at = stack.len() - n as usize;
                    let hs: Vec<u32> = stack[at..].iter().map(|h| self.task_of(h)).collect::<Result<_, _>>()?;
                    match hs.iter().find(|c| self.fibers[**c as usize].state == State::Done) {
                        Some(&w) => {
                            let v = self.fibers[w as usize].result.clone().unwrap_or(Value::Unit);
                            for &c in &hs {
                                if c != w {
                                    self.cancel_tree(c)?;
                                }
                            }
                            stack.truncate(at);
                            push!(v);
                        }
                        None => {
                            if hs.iter().all(|c| self.fibers[*c as usize].state == State::Cancelled) {
                                fail!("every racing task was cancelled");
                            }
                            for &c in &hs {
                                self.add_awaiter(c, fid);
                            }
                            suspend_here!();
                        }
                    }
                }
                Op::AwaitTimeout => {
                    let ms = match stack.last() {
                        Some(Value::Dur(ms)) => *ms,
                        _ => fail!("internal: timeout without duration"),
                    };
                    let c = self.task_of(&stack[stack.len() - 2])?;
                    let st = self.fibers[c as usize].state;
                    let now = Instant::now();
                    let f = &mut self.fibers[fid as usize];
                    if st == State::Done {
                        f.deadline = None;
                        f.gen += 1;
                        stack.truncate(stack.len() - 2);
                        let v = self.fibers[c as usize].result.clone().unwrap_or(Value::Unit);
                        push!(Value::Variant(1, Some(Rc::new(v))));
                    } else if st == State::Cancelled || f.deadline.is_some_and(|d| now >= d) {
                        f.deadline = None;
                        f.gen += 1;
                        self.cancel_tree(c)?;
                        stack.truncate(stack.len() - 2);
                        push!(Value::Variant(0, None));
                    } else {
                        if f.deadline.is_none() {
                            let d = now + Duration::from_millis(ms.max(0) as u64);
                            f.deadline = Some(d);
                            self.timers.push(Reverse((d, fid, f.gen)));
                        }
                        self.add_awaiter(c, fid);
                        suspend_here!();
                    }
                }
                Op::Trap => {
                    let m = pop!();
                    fail!("{m}");
                }
                // Hot builtins run inline; the rest go through `builtin`.
                Op::Builtin(Builtin::Len, _) => {
                    let l = match stack.last().unwrap() {
                        Value::List(xs) => xs.len(),
                        Value::Map(m) => m.len(),
                        Value::Str(s) => s.chars().count(),
                        v => fail!("internal: len of {v}"),
                    };
                    *stack.last_mut().unwrap() = Value::Int(l as i64);
                }
                Op::Builtin(Builtin::Push, _) => {
                    let x = pop!();
                    match stack.last_mut() {
                        Some(Value::List(xs)) => value::list_push(xs, x),
                        _ => fail!("internal: push on non-list"),
                    }
                }
                Op::Builtin(Builtin::MapGet, _) => {
                    let k = pop!();
                    let r = match (stack.last().unwrap(), &k) {
                        (Value::Map(m), Value::Str(k)) => m.get(k).cloned(),
                        _ => fail!("internal: map_get"),
                    };
                    *stack.last_mut().unwrap() = match r {
                        Some(v) => Value::Variant(1, Some(Rc::new(v))),
                        None => Value::Variant(0, None),
                    };
                }
                Op::Builtin(Builtin::MapHas, _) => {
                    let k = pop!();
                    let r = match (stack.last().unwrap(), &k) {
                        (Value::Map(m), Value::Str(k)) => m.contains_key(k),
                        _ => fail!("internal: map_has"),
                    };
                    *stack.last_mut().unwrap() = Value::Bool(r);
                }
                Op::Builtin(Builtin::MapSet, _) => {
                    let v = pop!();
                    let Value::Str(k) = pop!() else { fail!("internal: map key") };
                    match stack.last_mut() {
                        Some(Value::Map(m)) => {
                            Rc::make_mut(m).insert(k, v);
                        }
                        _ => fail!("internal: map_set on non-map"),
                    }
                }
                Op::Builtin(b, argc) => {
                    if let Some(step) = self.builtin(fid, b, argc, stack)? {
                        match step {
                            BuiltinStep::Block => suspend_here!(),
                            BuiltinStep::Fail(m) => fail!("{m}"),
                        }
                    }
                }
            }
            if MODE == MODE_ONE {
                let code = if frames.len() > start_frames {
                    ZAL_EXIT_CALL
                } else if frames.len() < start_frames {
                    ZAL_EXIT_RET
                } else if pc != start_pc + 1 {
                    ZAL_JUMP
                } else {
                    native::ZAL_NEXT
                };
                save!();
                *slice_io = slice;
                return Ok(Step::Op(code));
            }
        }
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn add_awaiter(&mut self, child: u32, fid: u32) {
        let a = &mut self.fibers[child as usize].awaiters;
        if !a.contains(&fid) {
            a.push(fid);
        }
    }

    #[cfg_attr(feature = "profiling", inline(never))]
    fn builtin(&mut self, fid: u32, b: Builtin, argc: u16, stack: &mut Vec<Value>) -> Result<Option<BuiltinStep>, RunError> {
        macro_rules! fail {
            ($($t:tt)*) => { return Ok(Some(BuiltinStep::Fail(format!($($t)*)))) };
        }
        let n = stack.len();
        match b {
            Builtin::Len => {
                let v = stack.pop().unwrap();
                let l = match &v {
                    Value::List(xs) => xs.len(),
                    Value::Map(m) => m.len(),
                    Value::Str(s) => s.chars().count(),
                    _ => fail!("internal: len of {v}"),
                };
                stack.push(Value::Int(l as i64));
            }
            Builtin::Push => {
                let x = stack.pop().unwrap();
                if let Some(Value::List(xs)) = stack.last_mut() {
                    crate::value::list_push(xs, x);
                } else {
                    fail!("internal: push on non-list");
                }
            }
            Builtin::Get => {
                let i = stack.pop().unwrap();
                let l = stack.pop().unwrap();
                let r = match (&l, &i) {
                    (Value::List(xs), Value::Int(i)) => usize::try_from(*i).ok().and_then(|i| xs.get(i)).cloned(),
                    _ => fail!("internal: get"),
                };
                stack.push(match r {
                    Some(v) => Value::Variant(1, Some(Rc::new(v))),
                    None => Value::Variant(0, None),
                });
            }
            Builtin::Set => {
                let x = stack.pop().unwrap();
                let i = stack.pop().unwrap();
                let Value::Int(i) = i else { fail!("internal: set index") };
                if let Some(Value::List(xs)) = stack.last_mut() {
                    let len = xs.len();
                    match usize::try_from(i).ok().filter(|i| *i < len) {
                        Some(i) => Rc::make_mut(xs)[i] = x,
                        None => fail!("set: index {i} out of bounds (length {len})"),
                    }
                }
            }
            Builtin::Remove => {
                let i = stack.pop().unwrap();
                let Value::Int(i) = i else { fail!("internal: remove index") };
                if let Some(Value::List(xs)) = stack.last_mut() {
                    let len = xs.len();
                    match usize::try_from(i).ok().filter(|i| *i < len) {
                        Some(i) => {
                            Rc::make_mut(xs).remove(i);
                        }
                        None => fail!("remove: index {i} out of bounds (length {len})"),
                    }
                }
            }
            Builtin::Range => {
                let Value::Int(k) = stack.pop().unwrap() else { fail!("internal: range") };
                if !self.alloc_allowed((k.max(0) as u64).saturating_mul(std::mem::size_of::<Value>() as u64).min(usize::MAX as u64) as usize) {
                    fail!("out of memory: range({k}) exceeds the run's memory limit");
                }
                stack.push(Value::List(Rc::new((0..k.max(0)).map(Value::Int).collect())));
            }
            Builtin::MapNew => stack.push(Value::Map(Rc::new(value::ZMap::default()))),
            Builtin::MapWithCapacity => {
                let Value::Int(n) = stack.pop().unwrap() else { fail!("internal: map_with_capacity") };
                let n = n.clamp(0, 1 << 24) as usize;
                // Key, value and control byte per slot.
                if !self.alloc_allowed(n * (std::mem::size_of::<(Rc<str>, Value)>() + 1)) {
                    fail!("out of memory: map_with_capacity({n}) exceeds the run's memory limit");
                }
                let m = value::ZMap::with_capacity_and_hasher(n, Default::default());
                stack.push(Value::Map(Rc::new(m)));
            }
            Builtin::MapGet => {
                let k = stack.pop().unwrap();
                let m = stack.pop().unwrap();
                let r = match (&m, &k) {
                    (Value::Map(m), Value::Str(k)) => m.get(k).cloned(),
                    _ => fail!("internal: map_get"),
                };
                stack.push(match r {
                    Some(v) => Value::Variant(1, Some(Rc::new(v))),
                    None => Value::Variant(0, None),
                });
            }
            Builtin::MapSet => {
                let v = stack.pop().unwrap();
                let Value::Str(k) = stack.pop().unwrap() else { fail!("internal: map key") };
                if let Some(Value::Map(m)) = stack.last_mut() {
                    Rc::make_mut(m).insert(k, v);
                }
            }
            Builtin::MapHas => {
                let k = stack.pop().unwrap();
                let m = stack.pop().unwrap();
                let r = match (&m, &k) {
                    (Value::Map(m), Value::Str(k)) => m.contains_key(k),
                    _ => fail!("internal: map_has"),
                };
                stack.push(Value::Bool(r));
            }
            Builtin::MapKeys => {
                let Value::Map(m) = stack.pop().unwrap() else { fail!("internal: map_keys") };
                let mut keys: Vec<Value> = m.keys().map(|k| Value::Str(k.clone())).collect();
                keys.sort_by(|a, b| match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
                stack.push(Value::List(Rc::new(keys)));
            }
            Builtin::ToStr => {
                let v = stack.pop().unwrap();
                let Some(s) = self.render(&v) else { fail!("out of memory: str() result exceeds the run's memory limit or {RENDER_MAX_DEPTH} levels of nesting") };
                stack.push(Value::Str(Rc::from(s)));
            }
            Builtin::Print => {
                let v = stack.pop().unwrap();
                if self.echo_print || self.keep_output {
                    let Some(s) = self.render(&v) else { fail!("out of memory: print() text exceeds the run's memory limit or {RENDER_MAX_DEPTH} levels of nesting") };
                    if self.echo_print {
                        println!("{s}");
                    }
                    if self.keep_output {
                        self.output.push(s);
                    }
                }
                stack.push(Value::Unit);
            }
            Builtin::Join => {
                let Some(Value::List(hs)) = stack.last() else { fail!("internal: join") };
                let hs = hs.clone();
                for h in hs.iter() {
                    let c = self.task_of(h)?;
                    match self.fibers[c as usize].state {
                        State::Done => {}
                        State::Cancelled => fail!("joined a cancelled task"),
                        _ => {
                            self.add_awaiter(c, fid);
                            return Ok(Some(BuiltinStep::Block));
                        }
                    }
                }
                stack.pop();
                let vs = hs.iter().map(|h| self.fibers[self.task_of(h).unwrap() as usize].result.clone().unwrap_or(Value::Unit)).collect();
                stack.push(Value::List(Rc::new(vs)));
            }
            Builtin::AwaitAny => {
                let Some(Value::List(hs)) = stack.last() else { fail!("internal: await_any") };
                let hs = hs.clone();
                if hs.is_empty() {
                    fail!("await_any on an empty list");
                }
                let mut live = false;
                for (i, h) in hs.iter().enumerate() {
                    let c = self.task_of(h)?;
                    match self.fibers[c as usize].state {
                        State::Done => {
                            let v = self.fibers[c as usize].result.clone().unwrap_or(Value::Unit);
                            stack.pop();
                            stack.push(Value::Tuple(Rc::new([Value::Int(i as i64), v])));
                            return Ok(None);
                        }
                        State::Cancelled => {}
                        _ => live = true,
                    }
                }
                if !live {
                    fail!("await_any: every task in the list was cancelled");
                }
                for h in hs.iter() {
                    let c = self.task_of(h)?;
                    self.add_awaiter(c, fid);
                }
                return Ok(Some(BuiltinStep::Block));
            }
            Builtin::Cancel => {
                let h = stack.pop().unwrap();
                let c = self.task_of(&h)?;
                self.cancel_tree(c)?;
                stack.push(Value::Unit);
            }
            Builtin::JsonStr => {
                let Value::Str(x) = stack.pop().unwrap() else { fail!("internal: json_str") };
                // Worst case: every byte escaped as \uXXXX, plus the quotes.
                if !self.alloc_allowed(x.len().saturating_mul(6).saturating_add(2)) {
                    fail!("out of memory: json_str of {} bytes exceeds the run's memory limit", x.len());
                }
                stack.push(Value::Str(Rc::from(serde_json::to_string(&*x).unwrap_or_default())));
            }
            Builtin::JsonGetStr => {
                let Value::Str(field) = stack.pop().unwrap() else { fail!("internal: json_get_str") };
                let Value::Str(doc) = stack.pop().unwrap() else { fail!("internal: json_get_str") };
                let got = serde_json::from_str::<serde_json::Value>(&doc)
                    .ok()
                    .and_then(|j| j.get(&*field).and_then(|v| v.as_str()).map(|s| s.to_string()));
                stack.push(match got {
                    Some(v) => Value::Variant(1, Some(Rc::new(Value::Str(Rc::from(v))))),
                    None => Value::Variant(0, None),
                });
            }
            Builtin::NowMs => {
                let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
                stack.push(Value::Int(ms));
            }
            Builtin::Sleep => {
                let Some(Value::Dur(ms)) = stack.last() else { fail!("internal: sleep") };
                let ms = *ms;
                let now = Instant::now();
                let f = &mut self.fibers[fid as usize];
                match f.deadline {
                    Some(d) if now >= d => {
                        f.deadline = None;
                        f.gen += 1;
                        stack.pop();
                        stack.push(Value::Unit);
                    }
                    Some(_) => return Ok(Some(BuiltinStep::Block)),
                    None if ms <= 0 => {
                        stack.pop();
                        stack.push(Value::Unit);
                    }
                    None => {
                        let d = now + Duration::from_millis(ms as u64);
                        f.deadline = Some(d);
                        self.timers.push(Reverse((d, fid, f.gen)));
                        return Ok(Some(BuiltinStep::Block));
                    }
                }
            }
        }
        debug_assert!(stack.len() + argc as usize >= n);
        Ok(None)
    }
}

enum BuiltinStep {
    Block,
    Fail(String),
}
