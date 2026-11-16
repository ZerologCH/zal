//! x86-64 instruction encoder.
//!
//! Covers exactly what the code generator needs: 64/32/8-bit moves, `lea`,
//! the ALU group (`add or and sub xor cmp`), `test`, `imul`, `movzx`,
//! `movsxd`, `setcc`, `push`/`pop`, `call`/`jmp` (rel32, register, memory),
//! `jcc rel32` and `ret`. Branches and RIP-relative operands refer to
//! [`Label`]s that are backpatched by [`Asm::finish`]; calls to external
//! symbols are recorded as [`Reloc`]s for the object writer.
//!
//! Encodings are chosen to match GNU `as` byte-for-byte (verified in
//! `tests/encoder_vs_gas.rs`), e.g. `mov r, r` uses opcode 0x89 and
//! `add r, imm` uses the imm8 form when the immediate fits.

/// General-purpose register, numbered as in the ModRM/REX encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reg(pub u8);

pub const RAX: Reg = Reg(0);
pub const RCX: Reg = Reg(1);
pub const RDX: Reg = Reg(2);
pub const RBX: Reg = Reg(3);
pub const RSP: Reg = Reg(4);
pub const RBP: Reg = Reg(5);
pub const RSI: Reg = Reg(6);
pub const RDI: Reg = Reg(7);
pub const R8: Reg = Reg(8);
pub const R9: Reg = Reg(9);
pub const R10: Reg = Reg(10);
pub const R11: Reg = Reg(11);
pub const R12: Reg = Reg(12);
pub const R13: Reg = Reg(13);
pub const R14: Reg = Reg(14);
pub const R15: Reg = Reg(15);

impl Reg {
    fn lo(self) -> u8 {
        self.0 & 7
    }
    fn hi(self) -> bool {
        self.0 & 8 != 0
    }
}

/// Operand size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sz {
    B,
    D,
    Q,
}

/// A code position that may be bound later.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Label(pub u32);

/// Memory operand: `[base + index*scale + disp]` or `[rip + label]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mem {
    /// `base` may be `None` for an absolute 32-bit address.
    Sib { base: Option<Reg>, index: Option<(Reg, u8)>, disp: i32 },
    Rip(Label),
}

/// `[base + disp]`.
pub fn mem(base: Reg, disp: i32) -> Mem {
    Mem::Sib { base: Some(base), index: None, disp }
}

/// `[base + index*scale + disp]`.
pub fn mem_idx(base: Reg, index: Reg, scale: u8, disp: i32) -> Mem {
    Mem::Sib { base: Some(base), index: Some((index, scale)), disp }
}

/// Condition codes (low nibble of `Jcc`/`SETcc`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cond {
    O = 0,
    No = 1,
    B = 2,
    Ae = 3,
    E = 4,
    Ne = 5,
    Be = 6,
    A = 7,
    S = 8,
    Ns = 9,
    P = 10,
    Np = 11,
    L = 12,
    Ge = 13,
    Le = 14,
    G = 15,
}

/// ALU operation; value is the `/digit` of the immediate forms.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alu {
    Add = 0,
    Or = 1,
    And = 4,
    Sub = 5,
    Xor = 6,
    Cmp = 7,
}

/// Relocation kinds understood by the ELF writer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocKind {
    /// `R_X86_64_PLT32`: call to a (possibly undefined) function.
    Plt32,
    /// `R_X86_64_PC32`: 32-bit PC-relative data reference.
    Pc32,
    /// `R_X86_64_64`: absolute 64-bit address.
    Abs64,
}

/// A reference to an external symbol at `offset` in the code buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reloc {
    pub offset: usize,
    pub symbol: String,
    pub kind: RelocKind,
    pub addend: i64,
}

#[derive(Clone, Copy, Debug)]
enum Fixup {
    /// rel32 at `at`, relative to the end of the instruction `end`.
    Rel32 { at: usize, end: usize, target: Label },
    /// 32-bit `target - base` stored at `at` (jump tables).
    Diff32 { at: usize, target: Label, base: Label },
}

/// Register or memory operand for ModRM-encoded instructions.
#[derive(Clone, Copy, Debug)]
pub enum Rm {
    R(Reg),
    M(Mem),
}

impl From<Reg> for Rm {
    fn from(r: Reg) -> Rm {
        Rm::R(r)
    }
}
impl From<Mem> for Rm {
    fn from(m: Mem) -> Rm {
        Rm::M(m)
    }
}

/// Machine-code buffer with labels and external relocations.
#[derive(Default)]
pub struct Asm {
    pub buf: Vec<u8>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
    pub relocs: Vec<Reloc>,
}

impl Asm {
    pub fn new() -> Asm {
        Asm::default()
    }

    pub fn pos(&self) -> usize {
        self.buf.len()
    }

    pub fn new_label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() as u32 - 1)
    }

    pub fn bind(&mut self, l: Label) {
        assert!(self.labels[l.0 as usize].is_none(), "label bound twice");
        self.labels[l.0 as usize] = Some(self.buf.len());
    }

    pub fn label_pos(&self, l: Label) -> Option<usize> {
        self.labels[l.0 as usize]
    }

    /// Resolve all label fixups. Fails if a referenced label is unbound or
    /// out of rel32 range.
    pub fn finish(&mut self) -> Result<(), String> {
        for f in std::mem::take(&mut self.fixups) {
            let get = |l: Label| self.labels[l.0 as usize].ok_or_else(|| format!("unbound label {}", l.0));
            let (at, v) = match f {
                Fixup::Rel32 { at, end, target } => (at, get(target)? as i64 - end as i64),
                Fixup::Diff32 { at, target, base } => (at, get(target)? as i64 - get(base)? as i64),
            };
            let v = i32::try_from(v).map_err(|_| "displacement out of rel32 range".to_string())?;
            self.buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        Ok(())
    }

    pub fn byte(&mut self, b: u8) {
        self.buf.push(b);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Pad with `int3` to a multiple of `n`.
    pub fn align(&mut self, n: usize) {
        while self.buf.len() % n != 0 {
            self.buf.push(0xCC);
        }
    }

    /// 32-bit `target - base` (for position-independent jump tables).
    pub fn diff32(&mut self, target: Label, base: Label) {
        let at = self.pos();
        self.u32(0);
        self.fixups.push(Fixup::Diff32 { at, target, base });
    }

    // ---- core ModRM emission -------------------------------------------

    /// Emit optional REX, opcode bytes and ModRM/SIB/displacement.
    /// `reg` is the ModRM.reg field (register number or /digit).
    /// `byte_regs` says whether the reg field / the rm register are 8-bit
    /// registers (spl/bpl/sil/dil then need a REX prefix).
    /// Returns the position of a RIP displacement to patch, if any.
    fn emit_rm(&mut self, w: bool, opcode: &[u8], reg: u8, rm: Rm, byte_regs: (bool, bool)) -> Option<usize> {
        let r = reg & 8 != 0;
        let (x, b) = match rm {
            Rm::R(rr) => (false, rr.hi()),
            Rm::M(Mem::Sib { base, index, .. }) => (index.is_some_and(|(i, _)| i.hi()), base.is_some_and(|b| b.hi())),
            Rm::M(Mem::Rip(_)) => (false, false),
        };
        // spl/bpl/sil/dil need a (possibly empty) REX prefix.
        let force = (byte_regs.0 && (4..8).contains(&reg)) || (byte_regs.1 && matches!(rm, Rm::R(rr) if (4..8).contains(&rr.0)));
        let rex = 0x40 | (w as u8) << 3 | (r as u8) << 2 | (x as u8) << 1 | b as u8;
        if rex != 0x40 || force {
            self.byte(rex);
        }
        self.bytes(opcode);
        let reg3 = (reg & 7) << 3;
        match rm {
            Rm::R(rr) => {
                self.byte(0xC0 | reg3 | rr.lo());
                None
            }
            Rm::M(Mem::Rip(_)) => {
                self.byte(reg3 | 0b101);
                let at = self.pos();
                self.u32(0);
                Some(at)
            }
            Rm::M(Mem::Sib { base, index, disp }) => {
                if let Some((i, _)) = index {
                    assert!(i != RSP, "rsp cannot be an index register");
                }
                let scale_bits = |s: u8| match s {
                    1 => 0u8,
                    2 => 1,
                    4 => 2,
                    8 => 3,
                    _ => panic!("bad scale {s}"),
                };
                match base {
                    None => {
                        // [index*scale + disp32] or absolute [disp32]: SIB with base=101, mod=00.
                        self.byte(reg3 | 0b100);
                        let (ib, sb) = match index {
                            Some((i, s)) => (i.lo(), scale_bits(s)),
                            None => (0b100, 0),
                        };
                        self.byte(sb << 6 | ib << 3 | 0b101);
                        self.u32(disp as u32);
                    }
                    Some(bs) => {
                        // rbp/r13 as base cannot use mod=00 (that means rip/disp32).
                        let md = if disp == 0 && bs.lo() != 0b101 {
                            0b00
                        } else if (-128..=127).contains(&disp) {
                            0b01
                        } else {
                            0b10
                        };
                        let need_sib = index.is_some() || bs.lo() == 0b100;
                        if need_sib {
                            self.byte(md << 6 | reg3 | 0b100);
                            let (ib, sb) = match index {
                                Some((i, s)) => (i.lo(), scale_bits(s)),
                                None => (0b100, 0),
                            };
                            self.byte(sb << 6 | ib << 3 | bs.lo());
                        } else {
                            self.byte(md << 6 | reg3 | bs.lo());
                        }
                        match md {
                            0b01 => self.byte(disp as i8 as u8),
                            0b10 => self.u32(disp as u32),
                            _ => {}
                        }
                    }
                }
                None
            }
        }
    }

    /// Register a RIP-relative fixup once the whole instruction is emitted.
    fn rip_fix(&mut self, at: Option<usize>, rm: Rm) {
        if let (Some(at), Rm::M(Mem::Rip(l))) = (at, rm) {
            let end = self.pos();
            self.fixups.push(Fixup::Rel32 { at, end, target: l });
        }
    }

    fn op_rm(&mut self, sz: Sz, op8: &[u8], op: &[u8], reg: u8, rm: Rm) {
        let at = match sz {
            Sz::B => self.emit_rm(false, op8, reg, rm, (true, true)),
            Sz::D => self.emit_rm(false, op, reg, rm, (false, false)),
            Sz::Q => self.emit_rm(true, op, reg, rm, (false, false)),
        };
        self.rip_fix(at, rm);
    }

    fn op_rm_imm(&mut self, sz: Sz, op8: &[u8], op: &[u8], reg: u8, rm: Rm, imm: &[u8]) {
        let at = match sz {
            Sz::B => self.emit_rm(false, op8, reg, rm, (false, true)),
            Sz::D => self.emit_rm(false, op, reg, rm, (false, false)),
            Sz::Q => self.emit_rm(true, op, reg, rm, (false, false)),
        };
        self.bytes(imm);
        self.rip_fix(at, rm);
    }

    // ---- moves ----------------------------------------------------------

    /// `mov dst, src` (register to register/memory), opcode 88/89.
    pub fn mov_store(&mut self, sz: Sz, dst: impl Into<Rm>, src: Reg) {
        self.op_rm(sz, &[0x88], &[0x89], src.0, dst.into());
    }

    /// `mov dst, [mem]`, opcode 8A/8B.
    pub fn mov_load(&mut self, sz: Sz, dst: Reg, src: Mem) {
        self.op_rm(sz, &[0x8A], &[0x8B], dst.0, Rm::M(src));
    }

    /// `mov r64, r64`.
    pub fn mov_rr(&mut self, dst: Reg, src: Reg) {
        self.mov_store(Sz::Q, dst, src);
    }

    /// `mov r/m, imm` (C6/C7; the 64-bit form sign-extends imm32).
    pub fn mov_imm(&mut self, sz: Sz, dst: impl Into<Rm>, imm: i32) {
        let rm = dst.into();
        match (sz, rm) {
            (Sz::B, Rm::R(r)) => {
                // B0+r ib (the short form GNU as picks).
                if r.hi() || (4..8).contains(&r.0) {
                    self.byte(0x40 | r.hi() as u8);
                }
                self.bytes(&[0xB0 + r.lo(), imm as u8]);
            }
            (Sz::B, _) => self.op_rm_imm(sz, &[0xC6], &[0xC6], 0, rm, &[imm as u8]),
            _ => self.op_rm_imm(sz, &[0xC7], &[0xC7], 0, rm, &(imm as u32).to_le_bytes()),
        }
    }

    /// `mov r32, imm32` (B8+r; zero-extends into the 64-bit register).
    pub fn mov_r32_imm(&mut self, dst: Reg, imm: u32) {
        if dst.hi() {
            self.byte(0x41);
        }
        self.byte(0xB8 + dst.lo());
        self.u32(imm);
    }

    /// `movabs r64, imm64` (REX.W B8+r).
    pub fn movabs(&mut self, dst: Reg, imm: u64) {
        self.byte(0x48 | dst.hi() as u8);
        self.byte(0xB8 + dst.lo());
        self.u64(imm);
    }

    /// Shortest exact load of a 64-bit constant (as GNU `as` would pick for
    /// the equivalent explicit mnemonic).
    pub fn mov_const(&mut self, dst: Reg, v: i64) {
        if (0..=u32::MAX as i64).contains(&v) {
            self.mov_r32_imm(dst, v as u32);
        } else if i32::try_from(v).is_ok() {
            self.mov_imm(Sz::Q, dst, v as i32);
        } else {
            self.movabs(dst, v as u64);
        }
    }

    /// `movzx r32, byte r/m` (0F B6).
    pub fn movzx_b(&mut self, dst: Reg, src: impl Into<Rm>) {
        let rm = src.into();
        let force = matches!(rm, Rm::R(r) if (4..8).contains(&r.0));
        let at = self.emit_rm(false, &[0x0F, 0xB6], dst.0, rm, (false, force));
        self.rip_fix(at, rm);
    }

    /// `movsxd r64, dword r/m` (REX.W 63).
    pub fn movsxd(&mut self, dst: Reg, src: impl Into<Rm>) {
        let rm = src.into();
        let at = self.emit_rm(true, &[0x63], dst.0, rm, (false, false));
        self.rip_fix(at, rm);
    }

    /// `lea r64, [mem]`.
    pub fn lea(&mut self, dst: Reg, src: Mem) {
        let at = self.emit_rm(true, &[0x8D], dst.0, Rm::M(src), (false, false));
        self.rip_fix(at, Rm::M(src));
    }

    // ---- arithmetic -------------------------------------------------------

    /// `op dst, src` with a register source (opcode base+1, base+0 for bytes).
    pub fn alu_store(&mut self, op: Alu, sz: Sz, dst: impl Into<Rm>, src: Reg) {
        let base = (op as u8) << 3;
        self.op_rm(sz, &[base], &[base + 1], src.0, dst.into());
    }

    /// `op dst, [mem]` (opcode base+3, base+2 for bytes).
    pub fn alu_load(&mut self, op: Alu, sz: Sz, dst: Reg, src: Mem) {
        let base = (op as u8) << 3;
        self.op_rm(sz, &[base + 2], &[base + 3], dst.0, Rm::M(src));
    }

    /// `op r64, r64`.
    pub fn alu_rr(&mut self, op: Alu, dst: Reg, src: Reg) {
        self.alu_store(op, Sz::Q, dst, src);
    }

    /// `op r/m, imm` (80 /n ib, 83 /n ib when the immediate fits, else 81 /n id).
    pub fn alu_imm(&mut self, op: Alu, sz: Sz, dst: impl Into<Rm>, imm: i32) {
        let rm = dst.into();
        let d = op as u8;
        let acc = matches!(rm, Rm::R(RAX));
        match sz {
            // Accumulator short forms (04+8n ib / 05+8n id), as GNU as prefers them.
            Sz::B if acc => self.bytes(&[d << 3 | 4, imm as u8]),
            Sz::B => self.op_rm_imm(sz, &[0x80], &[0x80], d, rm, &[imm as u8]),
            _ if acc && !(-128..=127).contains(&imm) => {
                if sz == Sz::Q {
                    self.byte(0x48);
                }
                self.byte(d << 3 | 5);
                self.u32(imm as u32);
            }
            _ if (-128..=127).contains(&imm) => self.op_rm_imm(sz, &[0x83], &[0x83], d, rm, &[imm as u8]),
            _ => self.op_rm_imm(sz, &[0x81], &[0x81], d, rm, &(imm as u32).to_le_bytes()),
        }
    }

    /// `test r/m, r`.
    pub fn test_rr(&mut self, sz: Sz, a: impl Into<Rm>, b: Reg) {
        self.op_rm(sz, &[0x84], &[0x85], b.0, a.into());
    }

    /// `test r/m, imm` (F6 /0 ib, F7 /0 id).
    pub fn test_imm(&mut self, sz: Sz, a: impl Into<Rm>, imm: i32) {
        let rm = a.into();
        let acc = matches!(rm, Rm::R(RAX));
        match sz {
            Sz::B if acc => self.bytes(&[0xA8, imm as u8]),
            Sz::B => self.op_rm_imm(sz, &[0xF6], &[0xF6], 0, rm, &[imm as u8]),
            _ if acc => {
                if sz == Sz::Q {
                    self.byte(0x48);
                }
                self.byte(0xA9);
                self.u32(imm as u32);
            }
            _ => self.op_rm_imm(sz, &[0xF7], &[0xF7], 0, rm, &(imm as u32).to_le_bytes()),
        }
    }

    /// `imul r64, r/m64` (0F AF); sets OF on signed overflow.
    pub fn imul(&mut self, dst: Reg, src: impl Into<Rm>) {
        let rm = src.into();
        let at = self.emit_rm(true, &[0x0F, 0xAF], dst.0, rm, (false, false));
        self.rip_fix(at, rm);
    }

    /// `imul r64, r/m64, imm` (6B ib / 69 id).
    pub fn imul_imm(&mut self, dst: Reg, src: impl Into<Rm>, imm: i32) {
        let rm = src.into();
        let at = if (-128..=127).contains(&imm) {
            let at = self.emit_rm(true, &[0x6B], dst.0, rm, (false, false));
            self.byte(imm as u8);
            at
        } else {
            let at = self.emit_rm(true, &[0x69], dst.0, rm, (false, false));
            self.u32(imm as u32);
            at
        };
        self.rip_fix(at, rm);
    }

    /// `inc r/m` / `dec r/m` (FE/FF /0, /1).
    pub fn inc(&mut self, sz: Sz, dst: impl Into<Rm>) {
        self.op_rm_imm(sz, &[0xFE], &[0xFF], 0, dst.into(), &[]);
    }
    pub fn dec(&mut self, sz: Sz, dst: impl Into<Rm>) {
        self.op_rm_imm(sz, &[0xFE], &[0xFF], 1, dst.into(), &[]);
    }

    /// `setcc r/m8` (0F 90+cc).
    pub fn setcc(&mut self, c: Cond, dst: impl Into<Rm>) {
        let rm = dst.into();
        let force = matches!(rm, Rm::R(r) if (4..8).contains(&r.0));
        let at = self.emit_rm(false, &[0x0F, 0x90 + c as u8], 0, rm, (false, force));
        self.rip_fix(at, rm);
    }

    // ---- stack and control flow ---------------------------------------------

    pub fn push(&mut self, r: Reg) {
        if r.hi() {
            self.byte(0x41);
        }
        self.byte(0x50 + r.lo());
    }

    pub fn pop(&mut self, r: Reg) {
        if r.hi() {
            self.byte(0x41);
        }
        self.byte(0x58 + r.lo());
    }

    pub fn ret(&mut self) {
        self.byte(0xC3);
    }

    pub fn int3(&mut self) {
        self.byte(0xCC);
    }

    /// `jmp rel32` to a label.
    pub fn jmp(&mut self, l: Label) {
        self.byte(0xE9);
        let at = self.pos();
        self.u32(0);
        self.fixups.push(Fixup::Rel32 { at, end: at + 4, target: l });
    }

    /// `jcc rel32` to a label.
    pub fn jcc(&mut self, c: Cond, l: Label) {
        self.bytes(&[0x0F, 0x80 + c as u8]);
        let at = self.pos();
        self.u32(0);
        self.fixups.push(Fixup::Rel32 { at, end: at + 4, target: l });
    }

    /// `call rel32` to a label in this buffer.
    pub fn call(&mut self, l: Label) {
        self.byte(0xE8);
        let at = self.pos();
        self.u32(0);
        self.fixups.push(Fixup::Rel32 { at, end: at + 4, target: l });
    }

    /// `call rel32` to an external symbol (`R_X86_64_PLT32`, addend -4).
    pub fn call_sym(&mut self, sym: &str) {
        self.byte(0xE8);
        let offset = self.pos();
        self.u32(0);
        self.relocs.push(Reloc { offset, symbol: sym.to_string(), kind: RelocKind::Plt32, addend: -4 });
    }

    /// `call r/m64` (FF /2).
    pub fn call_ind(&mut self, target: impl Into<Rm>) {
        let rm = target.into();
        let at = self.emit_rm(false, &[0xFF], 2, rm, (false, false));
        self.rip_fix(at, rm);
    }

    /// `jmp r/m64` (FF /4).
    pub fn jmp_ind(&mut self, target: impl Into<Rm>) {
        let rm = target.into();
        let at = self.emit_rm(false, &[0xFF], 4, rm, (false, false));
        self.rip_fix(at, rm);
    }

    /// `lea dst, [rip + sym]` with an `R_X86_64_PC32` relocation (AOT data
    /// references).
    pub fn lea_sym(&mut self, dst: Reg, sym: &str) {
        self.byte(0x48 | (dst.hi() as u8) << 2);
        self.byte(0x8D);
        self.byte((dst.lo() << 3) | 0b101);
        let offset = self.pos();
        self.u32(0);
        self.relocs.push(Reloc { offset, symbol: sym.to_string(), kind: RelocKind::Pc32, addend: -4 });
    }

    /// `movabs dst, sym` with an `R_X86_64_64` relocation.
    pub fn movabs_sym(&mut self, dst: Reg, sym: &str) {
        self.byte(0x48 | dst.hi() as u8);
        self.byte(0xB8 + dst.lo());
        let offset = self.pos();
        self.u64(0);
        self.relocs.push(Reloc { offset, symbol: sym.to_string(), kind: RelocKind::Abs64, addend: 0 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(f: impl FnOnce(&mut Asm)) -> Vec<u8> {
        let mut a = Asm::new();
        f(&mut a);
        a.finish().unwrap();
        a.buf
    }

    #[test]
    fn basic_known_encodings() {
        assert_eq!(enc(|a| a.mov_rr(RAX, RBX)), [0x48, 0x89, 0xD8]);
        assert_eq!(enc(|a| a.mov_load(Sz::Q, R12, mem(RBX, 8))), [0x4C, 0x8B, 0x63, 0x08]);
        assert_eq!(enc(|a| a.mov_load(Sz::Q, RAX, mem(R13, 0))), [0x49, 0x8B, 0x45, 0x00]);
        assert_eq!(enc(|a| a.mov_load(Sz::Q, RAX, mem(R12, 0))), [0x49, 0x8B, 0x04, 0x24]);
        assert_eq!(enc(|a| a.alu_imm(Alu::Cmp, Sz::B, mem(R13, -24), 1)), [0x41, 0x80, 0x7D, 0xE8, 0x01]);
        assert_eq!(enc(|a| a.push(R15)), [0x41, 0x57]);
        assert_eq!(enc(|a| a.ret()), [0xC3]);
        assert_eq!(enc(|a| a.setcc(Cond::L, RSI)), [0x40, 0x0F, 0x9C, 0xC6]);
        assert_eq!(enc(|a| a.movzx_b(RAX, RAX)), [0x0F, 0xB6, 0xC0]);
    }

    #[test]
    fn labels_backpatch() {
        let b = enc(|a| {
            let l = a.new_label();
            a.bind(l);
            a.jcc(Cond::E, l);
            a.jmp(l);
        });
        assert_eq!(b, [0x0F, 0x84, 0xFA, 0xFF, 0xFF, 0xFF, 0xE9, 0xF5, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn rip_relative_with_immediate() {
        // cmp byte [rip+L], 7 ; L: -> disp counts from the end incl. imm8.
        let b = enc(|a| {
            let l = a.new_label();
            a.alu_imm(Alu::Cmp, Sz::B, Mem::Rip(l), 7);
            a.bind(l);
        });
        assert_eq!(b, [0x80, 0x3D, 0, 0, 0, 0, 0x07]);
    }
}
