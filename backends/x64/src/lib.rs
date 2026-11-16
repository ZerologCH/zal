//! ZAL backend B: compiles verified IR v1 straight to x86-64 machine code
//! with an own encoder (`enc`), JIT memory manager (`jit`) and ELF64
//! relocatable object writer (`elf`). See `docs/rfcs/0002-native-backends.md`.
//!
//! The runtime ABI types come from `zal_vm::native` (stream R); this crate
//! only generates code and owns the executable memory.

pub mod codegen;
pub mod v2;
pub mod elf;
pub mod enc;
pub mod jit;

use codegen::{compile_fn, RtMode};
use elf::{ObjectWriter, Section, SymKind};
use jit::ExecMem;
use zal_ir::Program;
use zal_vm::native::{runtime_symbols, JitModule, NativeBackend, NativeFn};

/// Absolute addresses of the runtime entry points embedded into JIT code,
/// looked up by symbol name (`zal_rt_*`).
#[derive(Clone, Default)]
pub struct RtFns {
    addrs: Vec<(&'static str, usize)>,
}

impl RtFns {
    /// Resolve every entry point from the VM's `runtime_symbols()`.
    pub fn from_runtime() -> RtFns {
        RtFns { addrs: runtime_symbols().into_iter().map(|(n, p)| (n, p as usize)).collect() }
    }

    /// Override or add one entry point (tests use stubs).
    pub fn with(mut self, name: &'static str, addr: usize) -> RtFns {
        self.addrs.retain(|(n, _)| *n != name);
        self.addrs.push((name, addr));
        self
    }

    pub fn get(&self, name: &str) -> Option<usize> {
        self.addrs.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
    }
}

/// JIT-compile `prog`: each function goes into its own executable page; the
/// returned table maps function index to its body. The `ExecMem` pages are
/// moved into the module's `keep` box so they outlive the table.
pub fn jit_program(prog: &Program, rt: &RtFns) -> Result<JitModule, String> {
    // Code generation relies on the verifier's proof (targets, locals, depth).
    prog.verify()?;
    let mut mem: Vec<ExecMem> = Vec::with_capacity(prog.functions.len());
    let mut table: Vec<Option<NativeFn>> = Vec::with_capacity(prog.functions.len());
    for f in &prog.functions {
        let c = compile_fn(prog, f, RtMode::Jit(rt))?;
        assert!(c.relocs.is_empty(), "JIT code must not need relocations");
        let m = ExecMem::new(&c.code).map_err(|e| e.to_string())?;
        let ptr = m.ptr();
        mem.push(m);
        table.push(Some(unsafe { std::mem::transmute::<*const u8, NativeFn>(ptr) }));
    }
    Ok(JitModule::new(table, Box::new(mem)))
}

/// Produce an AOT relocatable object exporting `zal_fn_<i>` for every
/// function, a `zal_fn_table` of pointers, `zal_fn_count`, and the embedded
/// IR as `zal_ir` / `zal_ir_len` (RFC §5). Calls to the runtime stay as
/// `R_X86_64_PLT32` relocations against `zal_rt_*`.
pub fn aot_object(prog: &Program) -> Result<Vec<u8>, String> {
    prog.verify()?;
    let mut o = ObjectWriter::new();
    for (i, f) in prog.functions.iter().enumerate() {
        let c = compile_fn(prog, f, RtMode::Aot)?;
        let base = o.text.len() as u64;
        o.text.extend_from_slice(&c.code);
        o.define(&format!("zal_fn_{i}"), Section::Text, base, c.code.len() as u64, SymKind::Func, true);
        for r in &c.relocs {
            o.reloc(Section::Text, base + r.offset as u64, &r.symbol, r.kind, r.addend);
        }
    }
    // zal_fn_table: n function pointers (R_X86_64_64 to each zal_fn_<i>).
    let n = prog.functions.len();
    let table_at = o.data.len() as u64;
    o.define("zal_fn_table", Section::Data, table_at, (n * 8) as u64, SymKind::Object, true);
    for i in 0..n {
        let off = table_at + (i * 8) as u64;
        o.data.extend_from_slice(&[0u8; 8]);
        o.reloc(Section::Data, off, &format!("zal_fn_{i}"), crate::enc::RelocKind::Abs64, 0);
    }
    // zal_fn_count (u32) in .rodata.
    let count_at = o.rodata.len() as u64;
    o.rodata.extend_from_slice(&(n as u32).to_le_bytes());
    o.define("zal_fn_count", Section::Rodata, count_at, 4, SymKind::Object, true);
    // Embedded IR: `zal_ir_len` is a u64 (runtime/aot), kept 8-aligned and
    // placed before the variable-length JSON.
    let json = prog.to_json();
    while o.rodata.len() % 8 != 0 {
        o.rodata.push(0);
    }
    let len_at = o.rodata.len() as u64;
    o.rodata.extend_from_slice(&(json.len() as u64).to_le_bytes());
    o.define("zal_ir_len", Section::Rodata, len_at, 8, SymKind::Object, true);
    let ir_at = o.rodata.len() as u64;
    o.rodata.extend_from_slice(json.as_bytes());
    o.define("zal_ir", Section::Rodata, ir_at, json.len() as u64, SymKind::Object, true);
    Ok(o.finish())
}

/// Stream B's backend, selected by `--backend x64`.
pub struct X64Backend;

/// Entry point the CLI/VM looks up to obtain the backend.
pub fn backend() -> Box<dyn NativeBackend> {
    Box::new(X64Backend)
}

impl NativeBackend for X64Backend {
    fn name(&self) -> &'static str {
        "x64"
    }

    fn jit(&self, p: &Program) -> Result<JitModule, String> {
        jit_program(p, &RtFns::from_runtime())
    }

    fn object(&self, p: &Program) -> Result<Vec<u8>, String> {
        aot_object(p)
    }
}
