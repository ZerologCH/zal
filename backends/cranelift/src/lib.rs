//! ZAL backend A: verified IR v1 straight to machine code with Cranelift.
//!
//! - [`jit`] compiles every function into executable memory and returns the
//!   native table (`fn_ptrs[function_index]`) for the runtime's dispatcher.
//! - [`object`] writes one relocatable object with `zal_fn_<i>`,
//!   `zal_fn_table`, `zal_fn_count`, `zal_ir`, `zal_ir_len` (RFC 0002 §5);
//!   the `zal_rt_*` entry points stay undefined imports.

pub mod abi;
mod codegen;

use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, DataDescription, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use zal_ir::Program;

pub use abi::{NativeFn, RtSymbols, ZalCtx};
use zal_vm::native::{JitModule, NativeBackend};
pub use codegen::{has_fast_path, RT_V2};

/// Exact Cranelift release this backend is built and tested with.
pub const CRANELIFT_VERSION: &str = "0.137.1";

/// Host ISA, built once per mode (CPU feature detection and flag parsing are
/// not free and every `zal run` / VM construction JIT-compiles).
fn host_isa(pic: bool) -> Result<OwnedTargetIsa, String> {
    use std::sync::OnceLock;
    static ISA: [OnceLock<Result<OwnedTargetIsa, String>>; 2] = [OnceLock::new(), OnceLock::new()];
    ISA[pic as usize].get_or_init(|| build_host_isa(pic)).clone()
}

fn build_host_isa(pic: bool) -> Result<OwnedTargetIsa, String> {
    let mut fb = settings::builder();
    let set = |fb: &mut settings::Builder, k: &str, v: &str| fb.set(k, v).map_err(|e| format!("cranelift flag {k}: {e}"));
    set(&mut fb, "opt_level", "speed")?;
    set(&mut fb, "is_pic", if pic { "true" } else { "false" })?;
    set(&mut fb, "use_colocated_libcalls", "false")?;
    // The verifier proves our own IR well-formed; keep it in debug builds only.
    set(&mut fb, "enable_verifier", if cfg!(debug_assertions) || std::env::var_os("ZAL_CL_VERIFY").is_some() { "true" } else { "false" })?;
    let flags = settings::Flags::new(fb);
    let ib = cranelift_native::builder().map_err(|e| format!("host ISA: {e}"))?;
    ib.finish(flags).map_err(|e| format!("host ISA: {e}"))
}

/// Declare and define every IR function as `zal_fn_<i>`.
fn compile_all<M: Module>(m: &mut M, prog: &Program, jit: Option<&[zal_vm::native::ZalValue]>) -> Result<Vec<FuncId>, String> {
    // Inline code writes the operand stack without bounds checks; that is only
    // sound for verified IR.
    prog.verify()?;
    let mut imports = codegen::declare_imports(m)?;
    // Inline data access only in-process (the layout of a separately built
    // AOT runtime is not guaranteed); ZAL_NO_INLINE_DATA=1 disables it.
    if let Some(k) = jit.filter(|_| std::env::var("ZAL_NO_INLINE_DATA").map_or(true, |v| v != "1")) {
        imports.lay = abi::data_layout();
        imports.kconst = Some(k.as_ptr() as usize);
        let ptr = m.target_config().pointer_type();
        let mut sig = m.make_signature();
        sig.params.push(cranelift_codegen::ir::AbiParam::new(ptr));
        sig.params.push(cranelift_codegen::ir::AbiParam::new(ptr));
        imports.list_one = Some(m.declare_function("zal_rt_list_one", Linkage::Import, &sig).map_err(|e| e.to_string())?);
        // Slot tags are trusted only for a program the checker sealed in
        // this process and nobody changed since.
        if prog.functions.first().is_some_and(|f| f.slot_seal != 0) {
            imports.seal = zal_ir::tag_seal(prog);
        }
    }
    let sig = codegen::body_signature(m);
    let mut ids = Vec::with_capacity(prog.functions.len());
    for i in 0..prog.functions.len() {
        let id = m
            .declare_function(&format!("zal_fn_{i}"), Linkage::Export, &sig)
            .map_err(|e| format!("declare zal_fn_{i}: {e}"))?;
        ids.push(id);
    }
    // Proven operand depth per op (verified programs only): operand slots
    // then sit at constant offsets from the frame base.
    let depths = prog.verify_depths().ok().map(|(_, d)| d);
    let mut ctx = m.make_context();
    let mut fctx = FunctionBuilderContext::new();
    for (i, &id) in ids.iter().enumerate() {
        ctx.func.signature = sig.clone();
        let dep = depths.as_ref().map(|d| d[i].as_slice());
        codegen::translate(m, &imports, prog, i, dep, &mut ctx.func, &mut fctx);
        let dump = std::env::var("ZAL_CL_DISASM").ok().filter(|f| *f == prog.functions[i].name);
        ctx.set_disasm(dump.is_some());
        m.define_function(id, &mut ctx).map_err(|e| {
            let detail = match &e {
                cranelift_module::ModuleError::Compilation(ce) => format!("{ce:?}"),
                other => other.to_string(),
            };
            format!("cranelift: {} (#{i}): {detail}", prog.functions[i].name)
        })?;
        if let Ok(path) = std::env::var("ZAL_CL_SRCMAP") {
            // Profiling aid: code offset ranges per op pc, one line each.
            let base = ctx.func.params.base_srcloc();
            if let Some(c) = ctx.compiled_code() {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| e.to_string())?;
                for r in c.buffer.get_srclocs_sorted() {
                    let loc = if r.loc.is_rel() { r.loc.relocate(base) } else { r.loc.as_abs() };
                    if !loc.is_default() {
                        let _ = writeln!(f, "{} {} {} {}", i, loc.bits(), r.start, r.end);
                    }
                }
            }
        }
        if dump.is_some() {
            // Debug aid: machine code of one function (ZAL_CL_DISASM=<name>).
            if let Some(v) = ctx.compiled_code().and_then(|c| c.vcode.clone()) {
                eprintln!("{v}");
            }
            // Raw machine code for objdump (ZAL_CL_DISASM_BIN=<path>).
            if let (Ok(path), Some(c)) = (std::env::var("ZAL_CL_DISASM_BIN"), ctx.compiled_code()) {
                let _ = std::fs::write(path, c.code_buffer());
            }
        }
        m.clear_context(&mut ctx);
    }
    Ok(ids)
}

/// JIT-compiled program. Owns the executable memory; the table entries are
/// valid as long as this value lives.
pub struct JitCode {
    module: Option<JITModule>,
    table: Vec<Option<NativeFn>>,
    /// Values the code clones inline (`jit_constants`); never moved.
    _consts: Box<[zal_vm::native::ZalValue]>,
}

impl JitCode {
    /// `fn_ptrs[function_index]`; `None` = run in the interpreter.
    pub fn table(&self) -> &[Option<NativeFn>] {
        &self.table
    }
}

impl Drop for JitCode {
    fn drop(&mut self) {
        if let Some(m) = self.module.take() {
            // SAFETY: the table dies with us; callers must not keep copies of
            // the pointers beyond the lifetime of `JitCode`.
            unsafe { m.free_memory() };
        }
    }
}

// SAFETY: the module is only touched in `drop`; the code itself is immutable.
unsafe impl Send for JitCode {}

/// Compile `prog` into executable memory, linked against the `zal-vm` runtime.
pub fn jit(prog: &Program) -> Result<JitCode, String> {
    jit_inner(prog, &abi::runtime_symbols()[..], true)
}

/// As [`jit`], resolving the `zal_rt_*` imports to `rt` (stub runtimes in tests).
/// Inline data access (addendum v4) is off here: stub runtimes use values
/// that do not follow the real data layout.
pub fn jit_with(prog: &Program, rt: &[(&'static str, *const u8)]) -> Result<JitCode, String> {
    jit_inner(prog, rt, false)
}

fn jit_inner(prog: &Program, rt: &[(&'static str, *const u8)], v4: bool) -> Result<JitCode, String> {
    let isa = host_isa(false)?;
    let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
    for &(name, ptr) in rt {
        jb.symbol(name, ptr);
    }
    let mut m = JITModule::new(jb);
    let consts = zal_vm::native::jit_constants(prog);
    let ids = compile_all(&mut m, prog, if v4 { Some(&consts) } else { None })?;
    m.finalize_definitions().map_err(|e| format!("cranelift finalize: {e}"))?;
    let table = ids
        .iter()
        .map(|&id| {
            let p = m.get_finalized_function(id);
            // SAFETY: defined above with `body_signature` = NativeFn's C ABI.
            if let Ok(path) = std::env::var("ZAL_CL_SRCMAP") {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "base {} {}", ids.iter().position(|x| *x == id).unwrap_or(0), p as usize);
                }
            }
            Some(unsafe { std::mem::transmute::<*const u8, NativeFn>(p) })
        })
        .collect();
    Ok(JitCode { module: Some(m), table, _consts: consts })
}

/// Compile `prog` into a relocatable object file for the host target.
pub fn object(prog: &Program) -> Result<Vec<u8>, String> {
    let isa = host_isa(true)?;
    let ob = ObjectBuilder::new(isa, prog.module.as_bytes().to_vec(), default_libcall_names())
        .map_err(|e| format!("object builder: {e}"))?;
    let mut m = ObjectModule::new(ob);
    let ids = compile_all(&mut m, prog, None)?;
    let ptr_bytes = m.target_config().pointer_bytes() as usize;

    let define = |m: &mut ObjectModule, name: &str, bytes: Vec<u8>, align: u64, funcs: &[FuncId]| -> Result<(), String> {
        let id = m.declare_data(name, Linkage::Export, false, false).map_err(|e| format!("declare {name}: {e}"))?;
        let mut d = DataDescription::new();
        d.define(bytes.into_boxed_slice());
        d.set_align(align);
        for (i, &f) in funcs.iter().enumerate() {
            let fr = m.declare_func_in_data(f, &mut d);
            d.write_function_addr((i * ptr_bytes) as u32, fr);
        }
        m.define_data(id, &d).map_err(|e| format!("define {name}: {e}"))
    };
    define(&mut m, "zal_fn_table", vec![0; ids.len().max(1) * ptr_bytes], ptr_bytes as u64, &ids)?;
    define(&mut m, "zal_fn_count", (ids.len() as u32).to_le_bytes().to_vec(), 4, &[])?;
    let ir = prog.to_json().into_bytes();
    define(&mut m, "zal_ir_len", (ir.len() as u64).to_le_bytes().to_vec(), 8, &[])?;
    define(&mut m, "zal_ir", ir, 1, &[])?;

    let product = m.finish();
    product.emit().map_err(|e| format!("object emit: {e}"))
}

/// Backend A for the runtime's `--backend cranelift` / `zal build --backend cranelift`.
pub struct CraneliftBackend;

impl NativeBackend for CraneliftBackend {
    fn name(&self) -> &'static str {
        "cranelift"
    }

    fn jit(&self, p: &Program) -> Result<JitModule, String> {
        let code = jit(p)?;
        let table = code.table().to_vec();
        Ok(JitModule::new(table, Box::new(code)))
    }

    fn object(&self, p: &Program) -> Result<Vec<u8>, String> {
        object(p)
    }
}

/// Registration hook for the CLI (RFC 0002 §7): `zal_backend_cranelift::backend()`.
pub fn backend() -> Box<dyn NativeBackend> {
    Box::new(CraneliftBackend)
}
