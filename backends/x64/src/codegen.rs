//! Code generator: one re-entrant `extern "C" fn(ctx, entry_pc) -> u32` per
//! IR function, following RFC 0002.
//!
//! Register convention inside a body:
//! - `rbx` = `ctx` (callee-saved, survives runtime calls).
//! - `r12` = `ctx->stack` base, `r13` = `ctx->sp`; both reloaded at the start
//!   of every inlined fast path and written back before leaving it.
//! - everything else is scratch.
//!
//! Every op has a generic path (`call zal_rt_op`); the ops listed in RFC §4
//! additionally get an inlined fast path guarded by operand tags that falls
//! back to the generic path when an operand is not plain.

use crate::enc::*;
use crate::RtFns;
use zal_ir::{Builtin, Const, Function, Op, Program};
use zal_vm::native::{
    CTX_OFF_BASE, CTX_OFF_PC, CTX_OFF_SLICE, CTX_OFF_SP, CTX_OFF_STACK, ZAL_BUILTIN_MAP_SET, ZAL_BUILTIN_PUSH,
    ZAL_BUILTIN_REMOVE, ZAL_BUILTIN_SET, ZAL_MAKE_LIST, ZAL_MAKE_RECORD, ZAL_MAKE_TUPLE, ZAL_MAKE_VARIANT, ZAL_SLICE,
    TAG_BOOL, TAG_DUR, TAG_INT, TAG_UNIT, VALUE_PAYLOAD_OFFSET, ZAL_JUMP,
    ZAL_TRAP_ADD, ZAL_TRAP_MUL, ZAL_TRAP_SUB,
};

const OFF_STACK: i32 = CTX_OFF_STACK as i32;
const OFF_SP: i32 = CTX_OFF_SP as i32;
const OFF_BASE: i32 = CTX_OFF_BASE as i32;
const OFF_PC: i32 = CTX_OFF_PC as i32;
const OFF_SLICE: i32 = CTX_OFF_SLICE as i32;
const OFF_PAYLOAD: i32 = VALUE_PAYLOAD_OFFSET as i32;
const RT_JUMP: u32 = ZAL_JUMP;
const TRAP_ADD: u32 = ZAL_TRAP_ADD;
const TRAP_SUB: u32 = ZAL_TRAP_SUB;
const TRAP_MUL: u32 = ZAL_TRAP_MUL;

const CTX: Reg = RBX;
const STK: Reg = R12; // ctx->stack base pointer
const SP: Reg = R13; // ctx->sp (operand-stack depth)
const BASE: Reg = R14; // ctx->base (frame base; constant within one body entry)

/// Where the runtime entry points live: absolute addresses (JIT) or symbol
/// names emitted as `R_X86_64_PLT32` calls (AOT).
#[derive(Clone, Copy)]
pub enum RtMode<'a> {
    Jit(&'a RtFns),
    Aot,
}

/// Statically known tag of a stack value, tracked within a basic block to drop
/// redundant runtime tag guards.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Bool,
    Dur,
    Unit,
    Str,
    Unknown,
}

impl Kind {
    /// Plain = copyable/droppable as raw bytes (tags 0,1,2,4).
    fn plain(self) -> bool {
        matches!(self, Kind::Int | Kind::Bool | Kind::Dur | Kind::Unit)
    }
}

struct Cg<'a> {
    a: Asm,
    f: &'a Function,
    prog: &'a Program,
    mode: RtMode<'a>,
    /// One label per op index (+1 sentinel one-past-the-end).
    pcs: Vec<Label>,
    epilogue: Label,
    dispatch: Label,
    table: Label,
    /// `reset[pc]` = control may arrive here from a jump or the entry
    /// dispatcher, so no tag may be assumed. (Resume points after a runtime
    /// call are covered separately: `win` is cleared after every op that is not
    /// statically modelled.)
    reset: Vec<bool>,
    /// Abstract stack of the topmost values whose tag is known since the last
    /// reset. The top of `win` is the top of the operand stack; values below
    /// what `win` holds are unknown.
    win: Vec<Kind>,
}

/// `[STK + three*8 + disp]`, where `three` holds 3*slot_index (slots are 24 B).
fn slot(three: Reg, disp: i32) -> Mem {
    mem_idx(STK, three, 8, disp)
}

impl<'a> Cg<'a> {
    fn new(prog: &'a Program, f: &'a Function, mode: RtMode<'a>) -> Cg<'a> {
        let mut a = Asm::new();
        let pcs = (0..=f.code.len()).map(|_| a.new_label()).collect();
        let epilogue = a.new_label();
        let dispatch = a.new_label();
        let table = a.new_label();
        // A pc is a reset point if it is a branch target (control can arrive
        // with unknown tags). pc 0 and every resume point are handled by
        // clearing `win` after unmodelled ops.
        let mut reset = vec![false; f.code.len() + 1];
        for op in &f.code {
            if let Some(t) = branch_target(*op) {
                if (t as usize) < reset.len() {
                    reset[t as usize] = true;
                }
            }
        }
        Cg { a, f, prog, mode, pcs, epilogue, dispatch, table, reset, win: Vec::new() }
    }

    /// Kind of the operand `from_top` below the top (0 = top), or Unknown.
    fn kind(&self, from_top: usize) -> Kind {
        let n = self.win.len();
        if from_top < n {
            self.win[n - 1 - from_top]
        } else {
            Kind::Unknown
        }
    }

    /// Apply the static stack effect of `op` to `win`. Ops whose output tags
    /// are not modelled clear `win` (their pushed values become unknown and a
    /// following op sees nothing to assume).
    fn track(&mut self, op: Op) {
        let pop = |w: &mut Vec<Kind>, n: usize| {
            for _ in 0..n {
                w.pop();
            }
        };
        match op {
            Op::Const(c) => {
                let k = match self.prog.consts[c as usize] {
                    Const::Int(_) => Kind::Int,
                    Const::Bool(_) => Kind::Bool,
                    Const::Dur(_) => Kind::Dur,
                    Const::Str(_) => Kind::Str,
                };
                self.win.push(k);
            }
            Op::Unit => self.win.push(Kind::Unit),
            Op::LoadLocal(_) | Op::TakeLocal(_) => self.win.push(Kind::Unknown),
            Op::LoadLocal2(_, _) => {
                self.win.push(Kind::Unknown);
                self.win.push(Kind::Unknown);
            }
            Op::StoreLoad(_) => {
                pop(&mut self.win, 1);
                self.win.push(Kind::Unknown);
            }
            Op::Pop | Op::StoreLocal(_) | Op::JumpIfFalse(_) | Op::JumpIfTrue(_) => pop(&mut self.win, 1),
            Op::IncLocal(_, _) | Op::Jump(_) => {}
            Op::Add | Op::Sub | Op::Mul => {
                pop(&mut self.win, 2);
                self.win.push(Kind::Int);
            }
            Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Eq | Op::Ne => {
                pop(&mut self.win, 2);
                self.win.push(Kind::Bool);
            }
            Op::JumpIfNotLt(_) => pop(&mut self.win, 2),
            Op::TagIs(_) => {
                pop(&mut self.win, 1);
                self.win.push(Kind::Bool);
            }
            // Not modelled: pushed values (if any) are unknown.
            _ => self.win.clear(),
        }
    }

    /// Call the runtime entry point `sym` (absolute address in JIT mode, a
    /// `R_X86_64_PLT32` call in AOT mode). Arguments are already in place.
    fn call_rt(&mut self, sym: &'static str) {
        match self.mode {
            RtMode::Jit(fns) => {
                let addr = fns.get(sym).unwrap_or_else(|| panic!("runtime symbol {sym} not provided"));
                self.a.movabs(RAX, addr as u64);
                self.a.call_ind(RAX);
            }
            RtMode::Aot => self.a.call_sym(sym),
        }
    }

    /// Whether the runtime provides `sym` (always in AOT; in JIT only if the
    /// symbol table has it — the isolated stub tests provide just the v1 set).
    fn has(&self, sym: &str) -> bool {
        match self.mode {
            RtMode::Jit(fns) => fns.get(sym).is_some(),
            RtMode::Aot => true,
        }
    }

    /// `dst = &stack[base + i]` (pointer to local `i`). Clobbers rax.
    fn local_ptr(&mut self, dst: Reg, i: u16) {
        self.local_three(dst, i);
        self.a.lea(dst, mem_idx(STK, dst, 8, 0));
    }

    /// `dst = &stack[sp + k]` (k = 0: the free slot above the top).
    fn top_ptr(&mut self, dst: Reg, k: i32) {
        self.times3(dst, SP);
        self.a.lea(dst, mem_idx(STK, dst, 8, 24 * k));
    }

    /// If local `i` holds an owned value, drop it in place (leaves the slot to
    /// be overwritten). Clobbers rax, rcx and caller-saved registers.
    fn drop_local_if_owned(&mut self, i: u16) {
        let owned = self.a.new_label();
        let ok = self.a.new_label();
        self.local_three(RCX, i);
        self.guard_plain(slot(RCX, 0), owned);
        self.a.jmp(ok);
        self.a.bind(owned);
        self.a.lea(RDI, mem_idx(STK, RCX, 8, 0));
        self.call_rt("zal_rt_drop");
        self.a.bind(ok);
    }

    /// Push a copy of local `i`: raw bytes for a plain value, `zal_rt_clone`
    /// for an owned one (the generic path when the helper is absent).
    fn load_local(&mut self, pc: usize, i: u16) {
        let slow = self.a.new_label();
        let done = self.a.new_label();
        self.local_three(RCX, i);
        self.guard_plain(slot(RCX, 0), slow);
        self.times3(RDX, SP);
        self.copy_local_to_top();
        self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
        self.a.jmp(done);
        self.a.bind(slow);
        if self.has("zal_rt_clone") {
            self.a.lea(RSI, mem_idx(STK, RCX, 8, 0));
            self.top_ptr(RDI, 0);
            self.call_rt("zal_rt_clone");
            self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
        } else {
            self.generic(pc);
        }
        self.a.bind(done);
    }

    /// `Make*` through `zal_rt_make` (updates ctx->sp in place).
    fn make(&mut self, pc: usize, kind: u32, n: u32) {
        self.sync_sp();
        self.a.mov_rr(RDI, CTX);
        self.a.mov_r32_imm(RSI, kind);
        self.a.mov_r32_imm(RDX, n);
        self.call_rt("zal_rt_make");
        self.helper_or_generic(pc, |c| c.reload_stack());
    }

    /// After a typed helper returned in eax: nonzero = not applicable, take
    /// the generic path (memory is untouched).
    fn helper_or_generic(&mut self, pc: usize, ok: impl FnOnce(&mut Self)) {
        let gen = self.a.new_label();
        let done = self.a.new_label();
        self.a.test_rr(Sz::D, RAX, RAX);
        self.a.jcc(Cond::Ne, gen);
        ok(self);
        self.a.jmp(done);
        self.a.bind(gen);
        self.generic(pc);
        self.a.bind(done);
    }

    /// `rdi = ctx; esi = pc`.
    fn args_ctx_pc(&mut self, pc: usize) {
        self.a.mov_rr(RDI, CTX);
        self.a.mov_r32_imm(RSI, pc as u32);
    }

    /// Write the cached `sp` back to the context (before any runtime call that
    /// reads the operand stack).
    fn sync_sp(&mut self) {
        self.a.mov_store(Sz::Q, mem(CTX, OFF_SP), SP);
    }

    /// Reload `stack` and `sp` from the context (after a runtime op, which may
    /// have reallocated the buffer or changed the depth). `base` is unchanged
    /// by any non-exiting op, so it stays in `r14`.
    fn reload_stack(&mut self) {
        self.a.mov_load(Sz::Q, STK, mem(CTX, OFF_STACK));
        self.a.mov_load(Sz::Q, SP, mem(CTX, OFF_SP));
    }

    /// `dst = 3 * src` (slot stride helper).
    fn times3(&mut self, dst: Reg, src: Reg) {
        self.a.lea(dst, mem_idx(src, src, 2, 0));
    }

    /// Guard that the tag byte at `m` is a plain tag (0,1,2,4); else jump to
    /// `bad`. Clobbers nothing but flags.
    fn guard_plain(&mut self, m: Mem, bad: Label) {
        self.a.alu_imm(Alu::Cmp, Sz::B, m, 4);
        self.a.jcc(Cond::A, bad);
        self.a.alu_imm(Alu::Cmp, Sz::B, m, 3);
        self.a.jcc(Cond::E, bad);
    }

    fn guard_tag(&mut self, m: Mem, tag: u8, bad: Label) {
        self.a.alu_imm(Alu::Cmp, Sz::B, m, tag as i32);
        self.a.jcc(Cond::Ne, bad);
    }

    /// Emit the generic path for the op at `pc`: call the runtime, then branch
    /// on the result. Falls through to `pc+1` on 0.
    fn generic(&mut self, pc: usize) {
        self.sync_sp();
        self.args_ctx_pc(pc);
        self.call_rt("zal_rt_op");
        self.reload_stack();
        // 0 -> next (fall through); 6 -> dispatch via ctx->pc; else return eax.
        let not_jump = self.a.new_label();
        self.a.test_rr(Sz::D, RAX, RAX);
        self.a.jcc(Cond::E, self.pcs[pc + 1]);
        self.a.alu_imm(Alu::Cmp, Sz::D, RAX, RT_JUMP as i32);
        self.a.jcc(Cond::Ne, not_jump);
        // Taken branch: resume pc is in ctx->pc; dispatch expects it in eax.
        self.a.mov_load(Sz::D, RAX, mem(CTX, OFF_PC));
        self.a.jmp(self.dispatch);
        self.a.bind(not_jump);
        self.a.jmp(self.epilogue);
    }

    /// Trap tail: `zal_rt_trap(ctx, pc, code)` then return its exit code.
    fn trap(&mut self, pc: usize, code: u32) {
        self.sync_sp();
        self.args_ctx_pc(pc);
        self.a.mov_r32_imm(RDX, code);
        self.call_rt("zal_rt_trap");
        self.a.jmp(self.epilogue);
    }

    /// Binary Int op with overflow trap. `apply` emits `rax = a OP [b_payload]`
    /// and leaves OF set on overflow.
    fn int_arith(&mut self, pc: usize, trap_code: u32, apply: impl FnOnce(&mut Asm, Mem)) {
        // Both operands statically Int: no tag guards, no generic fallback.
        let known = self.kind(0) == Kind::Int && self.kind(1) == Kind::Int;
        let gen = (!known).then(|| self.a.new_label());
        let tr = self.a.new_label();
        let done = self.a.new_label();
        let three = RCX;
        self.times3(three, SP); // 3*sp
        if let Some(g) = gen {
            self.guard_tag(slot(three, -48), TAG_INT, g);
            self.guard_tag(slot(three, -24), TAG_INT, g);
        }
        self.a.mov_load(Sz::Q, RAX, slot(three, -40)); // a payload
        apply(&mut self.a, slot(three, -16)); // b payload
        self.a.jcc(Cond::O, tr);
        self.a.mov_imm(Sz::B, slot(three, -48), TAG_INT as i32);
        self.a.mov_store(Sz::Q, slot(three, -40), RAX);
        self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
        self.a.jmp(done);
        self.a.bind(tr);
        self.trap(pc, trap_code);
        if let Some(g) = gen {
            self.a.bind(g);
            self.generic(pc);
        }
        self.a.bind(done);
    }

    /// Int comparison producing a Bool via `setcc`.
    fn int_cmp(&mut self, pc: usize, cc: Cond) {
        let known = self.kind(0) == Kind::Int && self.kind(1) == Kind::Int;
        let gen = (!known).then(|| self.a.new_label());
        let done = (!known).then(|| self.a.new_label());
        let three = RCX;
        self.times3(three, SP);
        if let Some(g) = gen {
            self.guard_tag(slot(three, -48), TAG_INT, g);
            self.guard_tag(slot(three, -24), TAG_INT, g);
        }
        self.a.mov_load(Sz::Q, RAX, slot(three, -40));
        self.a.alu_load(Alu::Cmp, Sz::Q, RAX, slot(three, -16));
        self.a.setcc(cc, R8);
        self.a.mov_imm(Sz::B, slot(three, -48), TAG_BOOL as i32);
        self.a.mov_store(Sz::B, slot(three, -40), R8);
        self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
        if let (Some(g), Some(d)) = (gen, done) {
            self.a.jmp(d);
            self.a.bind(g);
            self.generic(pc);
            self.a.bind(d);
        }
    }

    /// Conditional branch popping one Bool. `take_when` is the setcc condition
    /// (on the payload being nonzero) that makes the branch taken.
    fn jump_if_bool(&mut self, pc: usize, target: u32, take_nonzero: bool) {
        let known = self.kind(0) == Kind::Bool;
        let gen = (!known).then(|| self.a.new_label());
        let done = (!known).then(|| self.a.new_label());
        let three = RCX;
        self.times3(three, SP);
        if let Some(g) = gen {
            self.guard_tag(slot(three, -24), TAG_BOOL, g);
        }
        self.a.movzx_b(R8, slot(three, -16)); // payload byte
        self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
        self.a.test_rr(Sz::D, R8, R8);
        self.branch(pc, target, if take_nonzero { Cond::Ne } else { Cond::E });
        if let (Some(g), Some(d)) = (gen, done) {
            self.a.jmp(d);
            self.a.bind(g);
            self.generic(pc);
            self.a.bind(d);
        }
    }

    /// Take a (possibly backward) branch to `target` when `cc` holds. A
    /// backward edge first accounts a tick.
    fn branch(&mut self, pc: usize, target: u32, cc: Cond) {
        let not_taken = self.a.new_label();
        self.a.jcc(opp(cc), not_taken);
        self.goto(pc, target);
        self.a.bind(not_taken);
    }

    /// Unconditional transfer to `target` (tick on a backward edge).
    fn goto(&mut self, pc: usize, target: u32) {
        if (target as usize) <= pc {
            // v2: count the slice inline; only at ZAL_SLICE call zal_rt_tick
            // (which then skips its own increment). Tick gets the target pc.
            self.a.alu_imm(Alu::Add, Sz::D, mem(CTX, OFF_SLICE), 1);
            self.a.alu_imm(Alu::Cmp, Sz::D, mem(CTX, OFF_SLICE), ZAL_SLICE as i32);
            self.a.jcc(Cond::B, self.pcs[target as usize]);
            self.sync_sp();
            self.args_ctx_pc(target as usize);
            self.call_rt("zal_rt_tick");
            self.a.test_rr(Sz::D, RAX, RAX);
            self.a.jcc(Cond::Ne, self.epilogue);
        }
        self.a.jmp(self.pcs[target as usize]);
    }

    fn emit(&mut self) -> Result<(), String> {
        // Prologue: save callee-saved regs (keeps rsp 16-aligned at calls),
        // park ctx in rbx, dispatch to entry_pc.
        self.a.push(RBX);
        self.a.push(R12);
        self.a.push(R13);
        self.a.push(R14);
        self.a.push(R15);
        self.a.mov_rr(CTX, RDI);
        // Cache stack/sp/base; valid at every op entry (including resume).
        self.reload_stack();
        self.a.mov_load(Sz::Q, BASE, mem(CTX, OFF_BASE));
        self.a.mov_rr(RAX, RSI); // entry_pc (zero-extended)
        self.a.jmp(self.dispatch);

        for pc in 0..self.f.code.len() {
            let lbl = self.pcs[pc];
            self.a.bind(lbl);
            let op = self.f.code[pc];
            if self.reset[pc] {
                self.win.clear();
            }
            self.emit_op(pc, op);
            self.track(op);
        }

        // Epilogue.
        self.a.bind(self.epilogue);
        self.a.pop(R15);
        self.a.pop(R14);
        self.a.pop(R13);
        self.a.pop(R12);
        self.a.pop(RBX);
        self.a.ret();

        // Indirect dispatch through the pc table (index in eax).
        self.a.bind(self.dispatch);
        self.a.lea(RCX, Mem::Rip(self.table));
        self.a.movsxd(RDX, mem_idx(RCX, RAX, 4, 0));
        self.a.alu_rr(Alu::Add, RDX, RCX);
        self.a.jmp_ind(RDX);

        // The table: 4-byte (target - table_base) per pc.
        self.a.align(4);
        self.a.bind(self.table);
        for pc in 0..self.f.code.len() {
            self.a.diff32(self.pcs[pc], self.table);
        }
        // Sentinel one-past-the-end never used as a target but keeps labels bound.
        self.a.bind(self.pcs[self.f.code.len()]);

        self.a.finish()
    }

    fn emit_op(&mut self, pc: usize, op: Op) {
        match op {
            // Peephole: `LoadLocal i; Ret` returns a copy of a local whose frame
            // is destroyed right after; move it instead (no clone, no drop).
            // Control after this op always reaches the Ret, so this is exact.
            Op::LoadLocal(i) if matches!(self.f.code.get(pc + 1), Some(Op::Ret)) => {
                self.local_three(RCX, i);
                self.times3(RDX, SP);
                self.copy_local_to_top();
                self.a.mov_imm(Sz::B, slot(RCX, 0), TAG_UNIT as i32);
                self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
            }
            // Peephole: `StoreLoad b; JumpIfNotTag t` clones the value into b and
            // then drops the copy on top. Move it into b and test b's tag in
            // place; the JumpIfNotTag at pc+1 stays for any other entry.
            Op::StoreLoad(b) if self.has("zal_rt_tag") && self.has("zal_rt_drop") && self.has("zal_rt_clone")
                && matches!(self.f.code.get(pc + 1), Some(Op::JumpIfNotTag(_, _))) =>
            {
                let Some(&Op::JumpIfNotTag(tag, t)) = self.f.code.get(pc + 1) else { unreachable!() };
                let variant = self.a.new_label();
                let odd = self.a.new_label();
                self.drop_local_if_owned(b);
                self.local_three(RCX, b);
                self.times3(RDX, SP);
                for off in [0, 8, 16] {
                    self.a.mov_load(Sz::Q, RAX, mem_idx(STK, RDX, 8, -24 + off));
                    self.a.mov_store(Sz::Q, mem_idx(STK, RCX, 8, off), RAX);
                }
                self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
                self.guard_tag(slot(RCX, 0), TAG_BOOL, variant);
                self.a.movzx_b(R8, slot(RCX, OFF_PAYLOAD));
                self.a.alu_imm(Alu::Cmp, Sz::D, R8, tag as i32);
                self.branch(pc + 1, t, Cond::Ne);
                self.a.jmp(self.pcs[pc + 2]);
                self.a.bind(variant);
                self.a.lea(RDI, mem_idx(STK, RCX, 8, 0));
                self.call_rt("zal_rt_tag");
                self.a.alu_imm(Alu::Cmp, Sz::D, RAX, 0xFFFF);
                self.a.jcc(Cond::E, odd);
                self.a.alu_imm(Alu::Cmp, Sz::D, RAX, tag as i32);
                self.branch(pc + 1, t, Cond::Ne);
                self.a.jmp(self.pcs[pc + 2]);
                // Neither variant nor Bool: restore the StoreLoad result and let
                // the JumpIfNotTag's generic path produce the interpreter's trap.
                self.a.bind(odd);
                self.local_ptr(RSI, b);
                self.top_ptr(RDI, 0);
                self.call_rt("zal_rt_clone");
                self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
                self.a.jmp(self.pcs[pc + 1]);
            }
            Op::Unit => {
                self.times3(RCX, SP);
                self.a.mov_imm(Sz::B, slot(RCX, 0), TAG_UNIT as i32);
                self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
            }
            Op::Const(c) => match &self.prog.consts[c as usize] {
                Const::Int(v) => self.push_scalar(TAG_INT, *v),
                Const::Dur(v) => self.push_scalar(TAG_DUR, *v),
                Const::Bool(b) => self.push_scalar(TAG_BOOL, *b as i64),
                Const::Str(_) if self.has("zal_rt_const") => {
                    self.top_ptr(RDI, 0);
                    self.a.mov_r32_imm(RSI, c);
                    self.a.mov_rr(RDX, CTX);
                    self.call_rt("zal_rt_const");
                    self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
                }
                Const::Str(_) => self.generic(pc), // owns a refcount
            },
            Op::Pop => {
                if self.kind(0).plain() {
                    self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
                } else if self.has("zal_rt_drop") {
                    // Any tag: zal_rt_drop is a no-op for plain values.
                    self.top_ptr(RDI, -1);
                    self.call_rt("zal_rt_drop");
                    self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
                } else {
                    let gen = self.a.new_label();
                    let done = self.a.new_label();
                    self.times3(RCX, SP);
                    self.guard_plain(slot(RCX, -24), gen);
                    self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
                    self.a.jmp(done);
                    self.a.bind(gen);
                    self.generic(pc);
                    self.a.bind(done);
                }
            }
            Op::LoadLocal(i) => self.load_local(pc, i),
            Op::StoreLocal(i) if self.has("zal_rt_drop") => {
                // Move the top into local i; an owned old value is dropped first.
                self.drop_local_if_owned(i);
                self.local_three(RCX, i);
                self.times3(RDX, SP);
                for off in [0, 8, 16] {
                    self.a.mov_load(Sz::Q, RAX, mem_idx(STK, RDX, 8, -24 + off));
                    self.a.mov_store(Sz::Q, mem_idx(STK, RCX, 8, off), RAX);
                }
                self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
            }
            Op::StoreLocal(i) => {
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.local_three(RCX, i);
                self.guard_plain(slot(RCX, 0), gen);
                self.times3(RDX, SP);
                for off in [0, 8, 16] {
                    self.a.mov_load(Sz::Q, RAX, mem_idx(STK, RDX, 8, -24 + off));
                    self.a.mov_store(Sz::Q, mem_idx(STK, RCX, 8, off), RAX);
                }
                self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
                self.a.jmp(done);
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            Op::IncLocal(i, k) => {
                let gen = self.a.new_label();
                let tr = self.a.new_label();
                let done = self.a.new_label();
                self.local_three(RCX, i);
                self.guard_tag(slot(RCX, 0), TAG_INT, gen);
                self.a.mov_load(Sz::Q, RAX, slot(RCX, OFF_PAYLOAD));
                self.a.alu_imm(Alu::Add, Sz::Q, RAX, k);
                self.a.jcc(Cond::O, tr);
                self.a.mov_store(Sz::Q, slot(RCX, OFF_PAYLOAD), RAX);
                self.a.jmp(done);
                self.a.bind(tr);
                self.trap(pc, TRAP_ADD);
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            Op::Add => self.int_arith(pc, TRAP_ADD, |a, b| a.alu_load(Alu::Add, Sz::Q, RAX, b)),
            Op::Sub => self.int_arith(pc, TRAP_SUB, |a, b| a.alu_load(Alu::Sub, Sz::Q, RAX, b)),
            Op::Mul => self.int_arith(pc, TRAP_MUL, |a, b| a.imul(RAX, b)),
            Op::Lt => self.int_cmp(pc, Cond::L),
            Op::Le => self.int_cmp(pc, Cond::Le),
            Op::Gt => self.int_cmp(pc, Cond::G),
            Op::Ge => self.int_cmp(pc, Cond::Ge),
            Op::Eq => self.int_cmp(pc, Cond::E),
            Op::Ne => self.int_cmp(pc, Cond::Ne),
            Op::JumpIfFalse(t) => self.jump_if_bool(pc, t, false),
            Op::JumpIfTrue(t) => self.jump_if_bool(pc, t, true),
            Op::JumpIfNotLt(t) => {
                let known = self.kind(0) == Kind::Int && self.kind(1) == Kind::Int;
                let gen = (!known).then(|| self.a.new_label());
                let done = (!known).then(|| self.a.new_label());
                self.times3(RCX, SP);
                if let Some(g) = gen {
                    self.guard_tag(slot(RCX, -48), TAG_INT, g);
                    self.guard_tag(slot(RCX, -24), TAG_INT, g);
                }
                self.a.mov_load(Sz::Q, RAX, slot(RCX, -40));
                self.a.alu_load(Alu::Cmp, Sz::Q, RAX, slot(RCX, -16));
                self.a.setcc(Cond::Ge, R8); // not less? (sets only the low byte)
                self.a.alu_imm(Alu::Sub, Sz::Q, SP, 2);
                self.a.test_rr(Sz::B, R8, R8);
                self.branch(pc, t, Cond::Ne);
                if let (Some(g), Some(d)) = (gen, done) {
                    self.a.jmp(d);
                    self.a.bind(g);
                    self.generic(pc);
                    self.a.bind(d);
                }
            }
            Op::Jump(t) => self.goto(pc, t),
            // Move a local to the top and leave Unit behind: ownership moves
            // with the bytes, so no tag guard is needed.
            Op::TakeLocal(i) => {
                self.local_three(RCX, i);
                self.times3(RDX, SP);
                self.copy_local_to_top();
                self.a.mov_imm(Sz::B, slot(RCX, 0), TAG_UNIT as i32);
                self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
            }
            // `StoreLocal a; LoadLocal a`: top stays, local gets a copy. Needs
            // a plain top (copy) and a plain old local (drop).
            Op::StoreLoad(i) if self.has("zal_rt_drop") && self.has("zal_rt_clone") => {
                // local i = clone(top), old local dropped; the top stays.
                let owned = self.a.new_label();
                let done = self.a.new_label();
                self.drop_local_if_owned(i);
                self.local_three(RCX, i);
                self.times3(RDX, SP);
                self.guard_plain(mem_idx(STK, RDX, 8, -24), owned);
                for off in [0, 8, 16] {
                    self.a.mov_load(Sz::Q, RAX, mem_idx(STK, RDX, 8, -24 + off));
                    self.a.mov_store(Sz::Q, mem_idx(STK, RCX, 8, off), RAX);
                }
                self.a.jmp(done);
                self.a.bind(owned);
                self.a.lea(RDI, mem_idx(STK, RCX, 8, 0));
                self.a.lea(RSI, mem_idx(STK, RDX, 8, -24));
                self.call_rt("zal_rt_clone");
                self.a.bind(done);
            }
            Op::StoreLoad(i) => {
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.local_three(RCX, i);
                self.guard_plain(slot(RCX, 0), gen);
                self.times3(RDX, SP);
                self.guard_plain(mem_idx(STK, RDX, 8, -24), gen);
                for off in [0, 8, 16] {
                    self.a.mov_load(Sz::Q, RAX, mem_idx(STK, RDX, 8, -24 + off));
                    self.a.mov_store(Sz::Q, mem_idx(STK, RCX, 8, off), RAX);
                }
                self.a.jmp(done);
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            // `LoadLocal a; LoadLocal b` with both locals plain.
            Op::LoadLocal2(x, y) if self.has("zal_rt_clone") => {
                self.load_local(pc, x);
                self.load_local(pc, y);
            }
            Op::LoadLocal2(x, y) => {
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.local_three(RCX, x);
                self.guard_plain(slot(RCX, 0), gen);
                self.local_three(R8, y);
                self.guard_plain(mem_idx(STK, R8, 8, 0), gen);
                self.times3(RDX, SP);
                self.copy_local_to_top();
                for off in [0, 8, 16] {
                    self.a.mov_load(Sz::Q, RAX, mem_idx(STK, R8, 8, off));
                    self.a.mov_store(Sz::Q, mem_idx(STK, RDX, 8, 24 + off), RAX);
                }
                self.a.alu_imm(Alu::Add, Sz::Q, SP, 2);
                self.a.jmp(done);
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            Op::MakeList(n) | Op::MakeTuple(n) | Op::MakeRecord(n) if self.has("zal_rt_make") => {
                let kind = match op {
                    Op::MakeList(_) => ZAL_MAKE_LIST,
                    Op::MakeTuple(_) => ZAL_MAKE_TUPLE,
                    _ => ZAL_MAKE_RECORD,
                };
                self.make(pc, kind, n as u32);
            }
            Op::MakeVariant(tag, has) if self.has("zal_rt_make") => {
                self.make(pc, ZAL_MAKE_VARIANT, tag as u32 | ((has as u32) << 16));
            }
            Op::Builtin(b, argc) if self.has("zal_rt_builtin") => {
                self.sync_sp();
                self.a.mov_rr(RDI, CTX);
                self.a.mov_r32_imm(RSI, b as u32);
                self.a.mov_r32_imm(RDX, argc as u32);
                self.call_rt("zal_rt_builtin");
                // Updates ctx->sp and may move ctx->stack.
                self.helper_or_generic(pc, |c| c.reload_stack());
            }
            Op::MapGetJump(r, t) if self.has("zal_rt_map_get_jump") => {
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.sync_sp();
                self.a.mov_rr(RDI, CTX);
                self.a.mov_r32_imm(RSI, r as u32);
                self.call_rt("zal_rt_map_get_jump");
                self.a.alu_imm(Alu::Cmp, Sz::D, RAX, 1);
                self.a.jcc(Cond::A, gen); // 2: generic, nothing changed
                self.a.mov_load(Sz::Q, SP, mem(CTX, OFF_SP)); // map and key consumed
                self.a.jcc(Cond::B, done); // 0: found, fall through
                self.goto(pc, t); // 1: missing
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            // Stack-operand accessors: the helper reads the top value and writes
            // the result over it (dst aliases the source, which is consumed).
            Op::Payload if self.has("zal_rt_payload") => {
                self.top_ptr(RSI, -1);
                self.a.mov_rr(RDI, RSI);
                self.call_rt("zal_rt_payload");
                self.helper_or_generic(pc, |_| {});
            }
            Op::FieldGet(i) if self.has("zal_rt_field") => {
                self.top_ptr(RSI, -1);
                self.a.mov_rr(RDI, RSI);
                self.a.mov_r32_imm(RDX, i as u32);
                self.call_rt("zal_rt_field");
                self.helper_or_generic(pc, |_| {});
            }
            Op::Index if self.has("zal_rt_index") => {
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.times3(RCX, SP);
                self.guard_tag(slot(RCX, -24), TAG_INT, gen);
                self.a.mov_load(Sz::Q, RDX, slot(RCX, -16)); // i (plain, discarded after)
                self.top_ptr(RSI, -2); // the list; dst aliases it
                self.a.mov_rr(RDI, RSI);
                self.call_rt("zal_rt_index");
                self.a.test_rr(Sz::D, RAX, RAX);
                self.a.jcc(Cond::Ne, gen);
                self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
                self.a.jmp(done);
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            Op::TagIs(tag) if self.has("zal_rt_tag") && self.has("zal_rt_drop") => {
                let variant = self.a.new_label();
                let set = self.a.new_label();
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.times3(RCX, SP);
                self.guard_tag(slot(RCX, -24), TAG_BOOL, variant);
                self.a.movzx_b(RAX, slot(RCX, -16));
                self.a.jmp(set);
                self.a.bind(variant);
                self.top_ptr(RDI, -1);
                self.call_rt("zal_rt_tag");
                self.a.alu_imm(Alu::Cmp, Sz::D, RAX, 0xFFFF);
                self.a.jcc(Cond::E, gen);
                self.a.mov_store(Sz::D, R15, RAX);
                self.top_ptr(RDI, -1);
                self.call_rt("zal_rt_drop");
                self.a.mov_store(Sz::D, RAX, R15);
                self.a.bind(set);
                // top = Bool(tag == x), written over the consumed scrutinee.
                self.a.alu_imm(Alu::Cmp, Sz::D, RAX, tag as i32);
                self.a.setcc(Cond::E, R8);
                self.times3(RCX, SP);
                self.a.mov_imm(Sz::B, slot(RCX, -24), TAG_BOOL as i32);
                self.a.mov_store(Sz::B, slot(RCX, -16), R8);
                self.a.jmp(done);
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            Op::Call(callee, argc) if self.has("zal_rt_call") => {
                // Push the callee frame and leave; the dispatcher runs the callee
                // and re-enters this body at pc + 1.
                self.sync_sp();
                self.args_ctx_pc(pc);
                self.a.mov_r32_imm(RDX, callee);
                self.a.mov_r32_imm(RCX, argc as u32);
                self.call_rt("zal_rt_call");
                self.a.jmp(self.epilogue);
            }
            Op::Ret if self.has("zal_rt_ret") => {
                self.sync_sp();
                self.a.mov_rr(RDI, CTX);
                self.call_rt("zal_rt_ret");
                self.a.jmp(self.epilogue);
            }
            Op::LenLocal(a) if self.has("zal_rt_len") => {
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.local_ptr(RDI, a);
                self.call_rt("zal_rt_len");
                self.a.test_rr(Sz::Q, RAX, RAX);
                self.a.jcc(Cond::S, gen); // -1: not a List/Map/Str
                self.times3(RCX, SP);
                self.a.mov_imm(Sz::B, slot(RCX, 0), TAG_INT as i32);
                self.a.mov_store(Sz::Q, slot(RCX, OFF_PAYLOAD), RAX);
                self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
                self.a.jmp(done);
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            Op::IndexLL(a, b) if self.has("zal_rt_index") => {
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.local_three(RCX, b);
                self.guard_tag(slot(RCX, 0), TAG_INT, gen);
                self.a.mov_load(Sz::Q, RDX, slot(RCX, OFF_PAYLOAD)); // i
                self.local_ptr(RSI, a); // list
                self.top_ptr(RDI, 0); // dst = free slot
                self.call_rt("zal_rt_index");
                self.a.test_rr(Sz::D, RAX, RAX);
                self.a.jcc(Cond::Ne, gen); // out of bounds / not a list: generic traps
                self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
                self.a.jmp(done);
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            Op::LoadField(a, f) if self.has("zal_rt_field") => {
                self.local_ptr(RSI, a);
                self.top_ptr(RDI, 0);
                self.a.mov_r32_imm(RDX, f as u32);
                self.call_rt("zal_rt_field");
                self.helper_or_generic(pc, |c| c.a.alu_imm(Alu::Add, Sz::Q, SP, 1));
            }
            Op::LoadPayload(a) if self.has("zal_rt_payload") => {
                self.local_ptr(RSI, a);
                self.top_ptr(RDI, 0);
                self.call_rt("zal_rt_payload");
                self.helper_or_generic(pc, |c| c.a.alu_imm(Alu::Add, Sz::Q, SP, 1));
            }
            Op::MovePayload(a, b) if self.has("zal_rt_payload") => {
                if a == b {
                    // dst aliases the variant: the helper consumes it.
                    self.local_ptr(RSI, a);
                    self.a.mov_rr(RDI, RSI);
                    self.call_rt("zal_rt_payload");
                    self.helper_or_generic(pc, |_| {});
                } else {
                    // Overwriting local b needs no drop only when it is plain.
                    let gen = self.a.new_label();
                    let done = self.a.new_label();
                    self.local_three(RCX, b);
                    self.guard_plain(slot(RCX, 0), gen);
                    self.a.lea(RDI, mem_idx(STK, RCX, 8, 0));
                    self.local_ptr(RSI, a);
                    self.call_rt("zal_rt_payload");
                    self.a.test_rr(Sz::D, RAX, RAX);
                    self.a.jcc(Cond::E, done);
                    self.a.bind(gen);
                    self.generic(pc);
                    self.a.bind(done);
                }
            }
            Op::ForNext(b, exit) if self.has("zal_rt_for_next") => {
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.local_ptr(RDI, b);
                self.call_rt("zal_rt_for_next");
                self.a.test_rr(Sz::D, RAX, RAX);
                self.a.jcc(Cond::E, done); // advanced: fall through
                self.a.alu_imm(Alu::Cmp, Sz::D, RAX, 1);
                self.a.jcc(Cond::Ne, gen);
                self.goto(pc, exit); // exhausted (ticks if backward, as the VM)
                self.a.bind(gen);
                self.generic(pc);
                self.a.bind(done);
            }
            Op::UpdLocal(bi, a) if self.has("zal_rt_upd") && upd_code(bi).is_some() => {
                let (code, n) = upd_code(bi).unwrap();
                self.local_ptr(RDI, a);
                self.top_ptr(RSI, -(n as i32)); // first (deepest) argument
                self.a.mov_r32_imm(RDX, code);
                self.call_rt("zal_rt_upd");
                // 0: arguments were moved out; drop sp without dropping them.
                self.helper_or_generic(pc, |c| c.a.alu_imm(Alu::Sub, Sz::Q, SP, n as i32));
            }
            Op::ConcatN(n) if self.has("zal_rt_concat") => {
                self.sync_sp();
                self.a.mov_rr(RDI, CTX);
                self.a.mov_r32_imm(RSI, n as u32);
                self.call_rt("zal_rt_concat");
                // The helper updated ctx->sp in place.
                self.helper_or_generic(pc, |c| c.a.mov_load(Sz::Q, SP, mem(CTX, OFF_SP)));
            }
            Op::JumpIfNotTag(tag, t) => {
                // Bool scrutinee inline (plain, no drop); variants via the generic path.
                let gen = self.a.new_label();
                let done = self.a.new_label();
                self.times3(RCX, SP);
                self.guard_tag(slot(RCX, -24), TAG_BOOL, gen);
                self.a.movzx_b(R8, slot(RCX, -16));
                self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
                self.a.alu_imm(Alu::Cmp, Sz::D, R8, tag as i32);
                self.branch(pc, t, Cond::Ne);
                self.a.jmp(done);
                self.a.bind(gen);
                if self.has("zal_rt_tag") && self.has("zal_rt_drop") {
                    let generic = self.a.new_label();
                    self.top_ptr(RDI, -1);
                    self.call_rt("zal_rt_tag");
                    self.a.alu_imm(Alu::Cmp, Sz::D, RAX, 0xFFFF);
                    self.a.jcc(Cond::E, generic); // neither variant nor Bool
                    self.a.mov_store(Sz::D, R15, RAX); // r15 is callee-saved
                    self.top_ptr(RDI, -1);
                    self.call_rt("zal_rt_drop"); // pop the scrutinee
                    self.a.alu_imm(Alu::Sub, Sz::Q, SP, 1);
                    self.a.alu_imm(Alu::Cmp, Sz::D, R15, tag as i32);
                    self.branch(pc, t, Cond::Ne);
                    self.a.jmp(done);
                    self.a.bind(generic);
                }
                self.generic(pc);
                self.a.bind(done);
            }
            // Everything else (calls, builtins, containers, variants, fibers,
            // Str/Dur arithmetic, Div/Mod, …) runs the generic path, which is
            // the interpreter op executed by the runtime.
            _ => {
                self.generic(pc);
                // Terminators never fall through; a Ret/Trap generic returns an
                // exit code, so no successor label is needed.
            }
        }
    }

    /// Push a 24-byte plain scalar: tag + 8-byte payload at the top slot.
    fn push_scalar(&mut self, tag: u8, v: i64) {
        self.times3(RCX, SP);
        self.a.mov_imm(Sz::B, slot(RCX, 0), tag as i32);
        self.a.mov_const(RAX, v);
        self.a.mov_store(Sz::Q, slot(RCX, OFF_PAYLOAD), RAX);
        self.a.alu_imm(Alu::Add, Sz::Q, SP, 1);
    }

    /// `dst = 3 * (base + i)` for local `i` (base cached in `r14`).
    fn local_three(&mut self, dst: Reg, i: u16) {
        if i == 0 {
            self.times3(dst, BASE);
        } else {
            self.a.lea(RAX, mem(BASE, i as i32));
            self.times3(dst, RAX);
        }
    }

    /// Copy the local slot (3-index in rcx) to the top slot (3-index in rdx).
    fn copy_local_to_top(&mut self) {
        for off in [0, 8, 16] {
            self.a.mov_load(Sz::Q, RAX, mem_idx(STK, RCX, 8, off));
            self.a.mov_store(Sz::Q, mem_idx(STK, RDX, 8, off), RAX);
        }
    }
}

/// The static branch target of `op`, if it has one (for building the set of
/// reset points where tag knowledge must be dropped).
fn branch_target(op: Op) -> Option<u32> {
    match op {
        Op::Jump(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfTrue(t)
        | Op::JumpIfNotLt(t)
        | Op::ForNext(_, t)
        | Op::JumpIfNotTag(_, t)
        | Op::UnwrapOrJump(_, t)
        | Op::MapGetJump(_, t) => Some(t),
        _ => None,
    }
}

/// `zal_rt_upd` builtin code and argument count for an `UpdLocal`.
fn upd_code(b: Builtin) -> Option<(u32, u32)> {
    match b {
        Builtin::Push => Some((ZAL_BUILTIN_PUSH, 1)),
        Builtin::Remove => Some((ZAL_BUILTIN_REMOVE, 1)),
        Builtin::Set => Some((ZAL_BUILTIN_SET, 2)),
        Builtin::MapSet => Some((ZAL_BUILTIN_MAP_SET, 2)),
        _ => None,
    }
}

/// Opposite condition (for `branch`'s skip jump).
fn opp(c: Cond) -> Cond {
    match c {
        Cond::O => Cond::No,
        Cond::No => Cond::O,
        Cond::B => Cond::Ae,
        Cond::Ae => Cond::B,
        Cond::E => Cond::Ne,
        Cond::Ne => Cond::E,
        Cond::Be => Cond::A,
        Cond::A => Cond::Be,
        Cond::S => Cond::Ns,
        Cond::Ns => Cond::S,
        Cond::P => Cond::Np,
        Cond::Np => Cond::P,
        Cond::L => Cond::Ge,
        Cond::Ge => Cond::L,
        Cond::Le => Cond::G,
        Cond::G => Cond::Le,
    }
}

/// Compiled machine code for one function plus its external relocations.
pub struct Compiled {
    pub code: Vec<u8>,
    pub relocs: Vec<Reloc>,
}

/// Compile one function.
pub fn compile_fn(prog: &Program, f: &Function, mode: RtMode) -> Result<Compiled, String> {
    let mut cg = Cg::new(prog, f, mode);
    cg.emit()?;
    Ok(Compiled { code: cg.a.buf, relocs: cg.a.relocs })
}
