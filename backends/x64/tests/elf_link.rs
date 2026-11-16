//! The ELF writer's objects pass `readelf`/`objdump` and link with `cc`.

use std::process::Command;
use zal_backend_x64::elf::*;
use zal_backend_x64::enc::*;

fn have(t: &str) -> bool {
    Command::new(t).arg("--version").output().is_ok()
}

fn run(cmd: &mut Command) -> String {
    let o = cmd.output().unwrap();
    assert!(o.status.success(), "{cmd:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

#[test]
fn object_with_all_relocation_kinds_links_and_runs() {
    if !have("cc") || !have("readelf") || !have("objdump") {
        eprintln!("skipping: toolchain missing");
        return;
    }
    let mut a = Asm::new();
    // long helper(long x) { return x * 3; }  (local function, reached via .data table)
    let helper = a.pos();
    a.lea(RAX, mem_idx(RDI, RDI, 2, 0));
    a.ret();
    let helper_len = a.pos() - helper;
    // long zal_test_entry(void)
    let entry = a.pos();
    a.push(RBX);
    a.mov_r32_imm(RDI, 40);
    a.mov_r32_imm(RSI, 2);
    a.call_sym("ext_add"); // PLT32 to an undefined symbol -> 42
    a.mov_rr(RBX, RAX);
    a.lea_sym(RCX, "my_data"); // PC32 -> &my_data
    a.alu_load(Alu::Add, Sz::Q, RBX, mem(RCX, 0)); // + 1000
    a.lea_sym(RCX, "fn_table");
    a.mov_r32_imm(RDI, 5);
    a.call_ind(mem(RCX, 0)); // helper(5) = 15 through an R_X86_64_64 pointer
    a.alu_rr(Alu::Add, RBX, RAX);
    a.lea_sym(RCX, "my_data"); // (R_X86_64_64 in .text would need DT_TEXTREL in a PIE)
    a.alu_load(Alu::Add, Sz::Q, RBX, mem(RCX, 8)); // + 7
    a.mov_rr(RAX, RBX);
    a.pop(RBX);
    a.ret();
    a.finish().unwrap();

    let mut o = ObjectWriter::new();
    o.text = a.buf.clone();
    o.define("helper", Section::Text, helper as u64, helper_len as u64, SymKind::Func, false);
    o.define("zal_test_entry", Section::Text, entry as u64, (a.buf.len() - entry) as u64, SymKind::Func, true);
    for r in &a.relocs {
        o.reloc(Section::Text, r.offset as u64, &r.symbol, r.kind, r.addend);
    }
    o.data.extend_from_slice(&1000i64.to_le_bytes());
    o.data.extend_from_slice(&7i64.to_le_bytes());
    o.define("my_data", Section::Data, 0, 16, SymKind::Object, true);
    o.define("fn_table", Section::Data, 16, 8, SymKind::Object, true);
    o.data.extend_from_slice(&[0; 8]);
    o.reloc(Section::Data, 16, "helper", RelocKind::Abs64, 0);
    o.rodata.extend_from_slice(b"hello\0");
    o.define("greeting", Section::Rodata, 0, 6, SymKind::Object, true);

    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("elf-link");
    std::fs::create_dir_all(&dir).unwrap();
    let obj = dir.join("t.o");
    std::fs::write(&obj, o.finish()).unwrap();

    let re = run(Command::new("readelf").arg("-aW").arg(&obj));
    assert!(!re.contains("Warning") && !re.contains("Error"), "{re}");
    for s in [".text", ".data", ".rodata", ".symtab", ".strtab", ".shstrtab", ".rela.text", ".rela.data", ".note.GNU-stack"] {
        assert!(re.contains(s), "missing {s}\n{re}");
    }
    for r in ["R_X86_64_PLT32", "R_X86_64_PC32", "R_X86_64_64"] {
        assert!(re.contains(r), "missing {r}\n{re}");
    }
    let od = run(Command::new("objdump").arg("-dr").arg(&obj));
    assert!(od.contains("ext_add") && od.contains("call"), "{od}");

    std::fs::write(
        dir.join("main.c"),
        r#"#include <stdio.h>
extern long zal_test_entry(void);
extern const char greeting[];
long ext_add(long a, long b) { return a + b; }
int main(void) { printf("%ld %s\n", zal_test_entry(), greeting); return 0; }
"#,
    )
    .unwrap();
    let exe = dir.join("t");
    let out = run(Command::new("cc").arg("-Wl,-z,noexecstack").arg("-Wl,--fatal-warnings").arg(dir.join("main.c")).arg(&obj).arg("-o").arg(&exe));
    assert!(out.is_empty(), "linker said: {out}");
    let out = run(&mut Command::new(&exe));
    assert_eq!(out.trim(), "1064 hello");
}
