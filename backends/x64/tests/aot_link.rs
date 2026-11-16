//! AOT: emit an object, link it with a small C stub runtime and run it.
//! The test program uses only inlined fast-path ops plus `Ret`, so the C
//! `zal_rt_op` only has to serve `Ret`.

use std::process::Command;
use zal_backend_x64::aot_object;
use zal_ir::{Const as CV, Function, FnKind, Op, Program, Ty};

fn have(t: &str) -> bool {
    Command::new(t).arg("--version").output().is_ok()
}

#[test]
fn aot_object_links_and_runs() {
    if !have("cc") {
        eprintln!("skipping: cc missing");
        return;
    }
    use Op::*;
    // sum(n): s=0; i=0; while i<n { s+=i; i++ }; return s   (locals 0=n 1=s 2=i)
    let code = vec![
        Const(0), StoreLocal(1), Const(0), StoreLocal(2),
        LoadLocal(2), LoadLocal(0), JumpIfNotLt(16),
        LoadLocal(1), LoadLocal(2), Add, StoreLocal(1), IncLocal(2, 1), Jump(4),
        Unit, Pop, Unit, LoadLocal(1), Ret,
    ];
    let f = Function {
        name: "sum".into(), kind: FnKind::Fn,
        params: vec![("n".into(), Ty::Int)], ret: Ty::Int, locals: 3,
        lines: vec![0; code.len()], code, effects: vec![], budget: None, slot_tags: vec![], slot_seal: 0,
    };
    let prog = Program { ir_version: 1, module: "t".into(), consts: vec![CV::Int(0)], functions: vec![f], externs: vec![], records: vec![], enums: vec![] };
    prog.verify().unwrap();

    let obj = aot_object(&prog).unwrap();
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("aot-link");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("fn.o"), &obj).unwrap();

    std::fs::write(dir.join("rt.c"), r##"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
typedef struct { uint8_t tag; uint8_t pad[7]; uint64_t a, b; } ZalValue;
typedef struct { void *vm; ZalValue *stack; uint64_t sp, base; uint32_t pc, func, slice; } ZalCtx;
extern uint32_t zal_fn_0(ZalCtx*, uint32_t);
extern void *zal_fn_table[];
extern uint32_t zal_fn_count;
extern const char zal_ir[]; extern uint64_t zal_ir_len;
static int trapped = -1;
// Nothing reaches the generic path here (Ret uses zal_rt_ret): return EXIT_RET=2, leaving the top.
uint32_t zal_rt_op(ZalCtx *c, uint32_t pc) { (void)c; (void)pc; return 2; }
// v2: Ret goes through zal_rt_ret; leave the result on top.
uint32_t zal_rt_ret(ZalCtx *c) { (void)c; return 2; }
// Only plain values occur here: drop is a no-op, clone a byte copy.
void zal_rt_drop(ZalValue *v) { (void)v; }
void zal_rt_clone(ZalValue *d, const ZalValue *s) { *d = *s; }
uint32_t zal_rt_tick(ZalCtx *c, uint32_t target) { (void)c; (void)target; return 0; }
uint32_t zal_rt_trap(ZalCtx *c, uint32_t pc, uint32_t code) { (void)c; (void)pc; trapped = code; return 4; }
int main(void) {
    // Sanity on the exported metadata.
    if (zal_fn_count != 1) { fprintf(stderr, "count\n"); return 2; }
    if (zal_fn_table[0] != (void*)zal_fn_0) { fprintf(stderr, "table\n"); return 2; }
    if (zal_ir_len < 10 || zal_ir[0] != '{') { fprintf(stderr, "ir\n"); return 2; }
    ZalValue stack[256]; memset(stack, 0, sizeof stack);
    for (long n = 0; n <= 100; n++) {
        memset(stack, 0, 3 * sizeof(ZalValue));
        stack[0].tag = 1; stack[0].a = (uint64_t)n;     // local 0 = n
        ZalCtx c = { 0, stack, 3, 0, 0, 0, 0 };
        uint32_t ex = zal_fn_0(&c, 0);
        if (ex != 2) { fprintf(stderr, "exit %u\n", ex); return 2; }
        ZalValue *r = &stack[c.sp - 1];
        long want = n * (n - 1) / 2;
        if (r->tag != 1 || (long)r->a != want) { fprintf(stderr, "n=%ld got %ld want %ld\n", n, (long)r->a, want); return 2; }
    }
    printf("ok\n");
    return 0;
}
"##).unwrap();

    let exe = dir.join("prog");
    let out = Command::new("cc").arg("-Wl,-z,noexecstack").arg(dir.join("rt.c")).arg(dir.join("fn.o")).arg("-o").arg(&exe).output().unwrap();
    assert!(out.status.success(), "link: {}", String::from_utf8_lossy(&out.stderr));
    let run = Command::new(&exe).output().unwrap();
    assert!(run.status.success(), "run: {}{}", String::from_utf8_lossy(&run.stdout), String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "ok");
}
