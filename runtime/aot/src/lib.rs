//! AOT runtime (RFC 0002 §5): `libzal_aot.a` provides `main` for executables
//! produced by `zal build --backend <name> -o <bin>`.
//!
//! The backend's object file exports `zal_ir` / `zal_ir_len` (the program's
//! IR as JSON bytes; `zal_ir_len` is a `u64`), `zal_fn_table` (`n` function
//! pointers, null = interpret) and `zal_fn_count` (`u32`). `main` loads the
//! IR, installs the table and behaves like `zal run` for
//! `--entry --args --args-file --mock --policy`.
//!
//! Link: `cc out.o libzal_aot.a -lpthread -ldl -lm -o out`.

use std::ffi::{c_char, c_int, CStr};

use zal_ir::Program;
use zal_vm::host::{MockHost, PolicyBroker};
use zal_vm::value::{from_json, to_json};
use zal_vm::{NativeFn, Vm};

extern "C" {
    static zal_ir: [u8; 0];
    static zal_ir_len: u64;
    static zal_fn_table: [Option<NativeFn>; 0];
    static zal_fn_count: u32;
}

/// Options shared with `zal run` (the subset an AOT binary supports).
#[derive(Default, Debug, PartialEq)]
pub struct RunArgs {
    pub entry: Option<String>,
    pub args: Option<String>,
    pub args_file: Option<String>,
    pub mock: Option<String>,
    pub policy: Option<String>,
}

pub fn parse_args(argv: &[String]) -> Result<RunArgs, String> {
    let mut o = RunArgs::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--entry" => o.entry = Some(val()?),
            "--args" => o.args = Some(val()?),
            "--args-file" => o.args_file = Some(val()?),
            "--mock" => o.mock = Some(val()?),
            "--policy" => o.policy = Some(val()?),
            s => return Err(format!("unknown option {s}\nusage: <bin> --entry <name> [--args '<json array>' | --args-file f.json] [--mock m.json] [--policy p.json]")),
        }
    }
    Ok(o)
}

fn read(p: &str) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))
}

/// Run like `zal run`: print lines during the run, then the pretty JSON result.
pub fn run(prog: &Program, table: Vec<Option<NativeFn>>, o: &RunArgs) -> Result<(), String> {
    let entry = o.entry.as_deref().ok_or("--entry <name> is required")?;
    let (_, f) = prog.function(entry).ok_or(format!("no function `{entry}`"))?;
    let text = match &o.args_file {
        Some(f) => read(f)?,
        None => o.args.clone().unwrap_or_else(|| "[]".into()),
    };
    let raw: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("--args: {e}"))?;
    let arr = raw.as_array().ok_or("--args must be a JSON array")?;
    if arr.len() != f.params.len() {
        return Err(format!("`{entry}` takes {} argument(s), got {}", f.params.len(), arr.len()));
    }
    let args = arr.iter().zip(&f.params).map(|(j, (_, t))| from_json(j, t, prog)).collect::<Result<Vec<_>, _>>()?;
    // No policy file means no grants: fail closed, as `zal run`.
    let broker = match &o.policy {
        Some(f) => PolicyBroker::from_json(&read(f)?)?,
        None => PolicyBroker::new(vec![]),
    };
    let host = match &o.mock {
        Some(m) => MockHost::from_json(&read(m)?)?,
        None => MockHost::default(),
    };
    let ret = f.ret.clone();
    let mut vm = Vm::new(prog, host, broker);
    vm.echo_print = true;
    vm.set_native(table);
    let v = vm.run(entry, args).map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&to_json(&v, &ret, prog)).unwrap());
    Ok(())
}

fn real_main(argv: Vec<String>) -> c_int {
    // SAFETY: the linked backend object defines these symbols (RFC 0002 §5).
    let (ir, table) = unsafe {
        let ir = std::slice::from_raw_parts(zal_ir.as_ptr(), zal_ir_len as usize);
        let table = std::slice::from_raw_parts(zal_fn_table.as_ptr(), zal_fn_count as usize).to_vec();
        (ir, table)
    };
    let r = std::str::from_utf8(ir)
        .map_err(|e| format!("embedded IR: {e}"))
        .and_then(Program::from_json)
        .and_then(|p| {
            if table.len() != p.functions.len() {
                return Err(format!("embedded function table has {} entries, program has {} functions", table.len(), p.functions.len()));
            }
            let o = parse_args(&argv)?;
            run(&p, table, &o)
        });
    match r {
        Ok(()) => 0,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            1
        }
    }
}

/// C entry point of every AOT executable.
///
/// # Safety
/// Called by the C runtime with valid `argc`/`argv`.
#[no_mangle]
pub unsafe extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args: Vec<String> = (1..argc.max(0) as usize).map(|i| CStr::from_ptr(*argv.add(i)).to_string_lossy().into_owned()).collect();
    real_main(args)
}
