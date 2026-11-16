//! Byte-for-byte comparison of the encoder against GNU `as`.
//!
//! Every case pairs an Intel-syntax line with the encoder call that should
//! produce the same bytes. All lines are assembled in one file; `.text` is
//! extracted with `objcopy` and compared instruction by instruction.

use std::process::Command;
use zal_backend_x64::enc::*;

const Q: [&str; 16] = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
const D: [&str; 16] = ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d", "r15d"];
const B: [&str; 16] = ["al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b", "r15b"];

fn r(i: usize) -> Reg {
    Reg(i as u8)
}

fn mem_text(m: &Mem) -> String {
    match m {
        Mem::Sib { base, index, disp } => {
            let mut s = String::new();
            if let Some(b) = base {
                s += Q[b.0 as usize];
            }
            if let Some((i, sc)) = index {
                if !s.is_empty() {
                    s += " + ";
                }
                s += &format!("{}*{}", Q[i.0 as usize], sc);
            }
            if base.is_none() && index.is_none() {
                return format!("ds:{disp}");
            }
            if *disp != 0 || base.is_none() {
                s += &format!(" {} {}", if *disp < 0 { "-" } else { "+" }, (*disp as i64).abs());
            }
            format!("[{s}]")
        }
        Mem::Rip(_) => unreachable!(),
    }
}

struct Cases {
    text: Vec<String>,
    bytes: Vec<Vec<u8>>,
}

impl Cases {
    fn add(&mut self, t: String, f: impl FnOnce(&mut Asm)) {
        let mut a = Asm::new();
        f(&mut a);
        a.finish().unwrap();
        assert!(a.relocs.is_empty());
        self.text.push(t);
        self.bytes.push(a.buf);
    }
}

fn mem_forms() -> Vec<Mem> {
    let mut v = Vec::new();
    for b in 0..16 {
        for d in [0, 8, -24, 127, -128, 128, -129, 0x1234_5678, -0x1234_5678] {
            v.push(mem(r(b), d));
        }
    }
    for b in [0, 4, 5, 12, 13, 15] {
        for i in [0, 1, 5, 8, 12, 13, 15] {
            for s in [1, 2, 4, 8] {
                for d in [0, 16, 300] {
                    v.push(mem_idx(r(b), r(i), s, d));
                }
            }
        }
    }
    // No base: [index*scale + disp32] and absolute.
    for i in [1, 5, 13] {
        v.push(Mem::Sib { base: None, index: Some((r(i), 8)), disp: 64 });
    }
    v.push(Mem::Sib { base: None, index: None, disp: 0x1000 });
    v
}

const ALU: [(Alu, &str); 6] = [(Alu::Add, "add"), (Alu::Or, "or"), (Alu::And, "and"), (Alu::Sub, "sub"), (Alu::Xor, "xor"), (Alu::Cmp, "cmp")];
const CONDS: [(Cond, &str); 16] = [
    (Cond::O, "o"), (Cond::No, "no"), (Cond::B, "b"), (Cond::Ae, "ae"), (Cond::E, "e"), (Cond::Ne, "ne"), (Cond::Be, "be"), (Cond::A, "a"),
    (Cond::S, "s"), (Cond::Ns, "ns"), (Cond::P, "p"), (Cond::Np, "np"), (Cond::L, "l"), (Cond::Ge, "ge"), (Cond::Le, "le"), (Cond::G, "g"),
];

fn build_cases() -> Cases {
    let mut c = Cases { text: vec![], bytes: vec![] };
    // Register-register forms over all 16x16 pairs.
    for a in 0..16 {
        for b in 0..16 {
            c.add(format!("mov {}, {}", Q[a], Q[b]), |x| x.mov_rr(r(a), r(b)));
            c.add(format!("mov {}, {}", D[a], D[b]), |x| x.mov_store(Sz::D, r(a), r(b)));
            c.add(format!("mov {}, {}", B[a], B[b]), |x| x.mov_store(Sz::B, r(a), r(b)));
            c.add(format!("test {}, {}", Q[a], Q[b]), |x| x.test_rr(Sz::Q, r(a), r(b)));
            c.add(format!("test {}, {}", D[a], D[b]), |x| x.test_rr(Sz::D, r(a), r(b)));
            c.add(format!("imul {}, {}", Q[a], Q[b]), |x| x.imul(r(a), r(b)));
            c.add(format!("imul {}, {}, 7", Q[a], Q[b]), |x| x.imul_imm(r(a), r(b), 7));
            c.add(format!("imul {}, {}, 1000", Q[a], Q[b]), |x| x.imul_imm(r(a), r(b), 1000));
            c.add(format!("movzx {}, {}", D[a], B[b]), |x| x.movzx_b(r(a), r(b)));
            c.add(format!("movsxd {}, {}", Q[a], D[b]), |x| x.movsxd(r(a), r(b)));
            for (op, n) in ALU {
                c.add(format!("{n} {}, {}", Q[a], Q[b]), |x| x.alu_rr(op, r(a), r(b)));
                c.add(format!("{n} {}, {}", D[a], D[b]), |x| x.alu_store(op, Sz::D, r(a), r(b)));
                c.add(format!("{n} {}, {}", B[a], B[b]), |x| x.alu_store(op, Sz::B, r(a), r(b)));
            }
        }
    }
    // Register-immediate forms.
    for a in 0..16 {
        for imm in [0i32, 1, -1, 127, -128, 128, -129, 1000, i32::MAX, i32::MIN] {
            for (op, n) in ALU {
                c.add(format!("{n} {}, {imm}", Q[a]), |x| x.alu_imm(op, Sz::Q, r(a), imm));
                c.add(format!("{n} {}, {imm}", D[a]), |x| x.alu_imm(op, Sz::D, r(a), imm));
            }
            c.add(format!("mov {}, {imm}", Q[a]), |x| x.mov_imm(Sz::Q, r(a), imm));
            c.add(format!("test {}, {imm}", Q[a]), |x| x.test_imm(Sz::Q, r(a), imm));
            c.add(format!("test {}, {imm}", D[a]), |x| x.test_imm(Sz::D, r(a), imm));
        }
        for imm in [0i32, 1, 127, -128, 0x55] {
            for (op, n) in ALU {
                c.add(format!("{n} {}, {imm}", B[a]), |x| x.alu_imm(op, Sz::B, r(a), imm));
            }
            c.add(format!("mov {}, {imm}", B[a]), |x| x.mov_imm(Sz::B, r(a), imm));
            c.add(format!("test {}, {imm}", B[a]), |x| x.test_imm(Sz::B, r(a), imm));
        }
        for imm in [0u32, 1, 0x7fff_ffff, 0xffff_ffff] {
            c.add(format!("mov {}, {imm}", D[a]), |x| x.mov_r32_imm(r(a), imm));
        }
        for imm in [0x1_0000_0000u64, 0xdead_beef_cafe_babe, u64::MAX] {
            c.add(format!("movabs {}, {imm}", Q[a]), |x| x.movabs(r(a), imm));
        }
        c.add(format!("push {}", Q[a]), |x| x.push(r(a)));
        c.add(format!("pop {}", Q[a]), |x| x.pop(r(a)));
        c.add(format!("call {}", Q[a]), |x| x.call_ind(r(a)));
        c.add(format!("jmp {}", Q[a]), |x| x.jmp_ind(r(a)));
        c.add(format!("inc {}", Q[a]), |x| x.inc(Sz::Q, r(a)));
        c.add(format!("dec {}", Q[a]), |x| x.dec(Sz::Q, r(a)));
        for (cc, n) in CONDS {
            c.add(format!("set{n} {}", B[a]), |x| x.setcc(cc, r(a)));
        }
    }
    // Memory forms.
    for m in mem_forms() {
        let t = mem_text(&m);
        for a in [0usize, 3, 4, 5, 6, 7, 8, 12, 13, 15] {
            c.add(format!("mov {}, qword ptr {t}", Q[a]), |x| x.mov_load(Sz::Q, r(a), m));
            c.add(format!("mov qword ptr {t}, {}", Q[a]), |x| x.mov_store(Sz::Q, m, r(a)));
            c.add(format!("mov {}, dword ptr {t}", D[a]), |x| x.mov_load(Sz::D, r(a), m));
            c.add(format!("mov byte ptr {t}, {}", B[a]), |x| x.mov_store(Sz::B, m, r(a)));
            c.add(format!("lea {}, {t}", Q[a]), |x| x.lea(r(a), m));
            c.add(format!("movzx {}, byte ptr {t}", D[a]), |x| x.movzx_b(r(a), m));
            c.add(format!("movsxd {}, dword ptr {t}", Q[a]), |x| x.movsxd(r(a), m));
            c.add(format!("imul {}, qword ptr {t}", Q[a]), |x| x.imul(r(a), m));
            for (op, n) in ALU {
                c.add(format!("{n} {}, qword ptr {t}", Q[a]), |x| x.alu_load(op, Sz::Q, r(a), m));
                c.add(format!("{n} qword ptr {t}, {}", Q[a]), |x| x.alu_store(op, Sz::Q, m, r(a)));
            }
        }
        for (op, n) in ALU {
            c.add(format!("{n} byte ptr {t}, 2"), |x| x.alu_imm(op, Sz::B, m, 2));
            c.add(format!("{n} qword ptr {t}, -5"), |x| x.alu_imm(op, Sz::Q, m, -5));
            c.add(format!("{n} qword ptr {t}, 100000"), |x| x.alu_imm(op, Sz::Q, m, 100000));
        }
        c.add(format!("mov byte ptr {t}, 2"), |x| x.mov_imm(Sz::B, m, 2));
        c.add(format!("mov qword ptr {t}, -7"), |x| x.mov_imm(Sz::Q, m, -7));
        c.add(format!("mov dword ptr {t}, 77777"), |x| x.mov_imm(Sz::D, m, 77777));
        c.add(format!("test byte ptr {t}, 1"), |x| x.test_imm(Sz::B, m, 1));
        c.add(format!("inc qword ptr {t}"), |x| x.inc(Sz::Q, m));
        c.add(format!("dec qword ptr {t}"), |x| x.dec(Sz::Q, m));
        c.add(format!("call qword ptr {t}"), |x| x.call_ind(m));
        c.add(format!("jmp qword ptr {t}"), |x| x.jmp_ind(m));
        c.add(format!("setl byte ptr {t}"), |x| x.setcc(Cond::L, m));
    }
    c.add("ret".into(), |x| x.ret());
    c.add("int3".into(), |x| x.int3());
    // Label-relative forms: label bound before (backward) and after (forward).
    let mut n = 0;
    let mut lbl = |c: &mut Cases, body: &dyn Fn(&mut Asm, Label), text: &dyn Fn(&str) -> String| {
        for back in [true, false] {
            let name = format!(".Lt{n}");
            n += 1;
            let t = if back { format!("{name}:\n{}", text(&name)) } else { format!("{}\n{name}:", text(&name)) };
            c.add(t, |x| {
                let l = x.new_label();
                if back {
                    x.bind(l);
                    body(x, l);
                } else {
                    body(x, l);
                    x.bind(l);
                }
            });
        }
    };
    lbl(&mut c, &|x, l| x.jmp(l), &|n| format!("{{disp32}} jmp {n}"));
    lbl(&mut c, &|x, l| x.call(l), &|n| format!("call {n}"));
    for (cc, cn) in CONDS {
        lbl(&mut c, &|x, l| x.jcc(cc, l), &|n| format!("{{disp32}} j{cn} {n}"));
    }
    for a in [0usize, 4, 5, 12, 13, 15] {
        lbl(&mut c, &|x, l| x.lea(r(a), Mem::Rip(l)), &|n| format!("lea {}, [rip + {n}]", Q[a]));
        lbl(&mut c, &|x, l| x.mov_load(Sz::Q, r(a), Mem::Rip(l)), &|n| format!("mov {}, qword ptr [rip + {n}]", Q[a]));
        lbl(&mut c, &|x, l| x.movsxd(r(a), Mem::Rip(l)), &|n| format!("movsxd {}, dword ptr [rip + {n}]", Q[a]));
    }
    lbl(&mut c, &|x, l| x.alu_imm(Alu::Cmp, Sz::B, Mem::Rip(l), 9), &|n| format!("cmp byte ptr [rip + {n}], 9"));
    lbl(&mut c, &|x, l| x.alu_imm(Alu::Add, Sz::Q, Mem::Rip(l), 100000), &|n| format!("add qword ptr [rip + {n}], 100000"));
    lbl(&mut c, &|x, l| x.mov_imm(Sz::Q, Mem::Rip(l), 1), &|n| format!("mov qword ptr [rip + {n}], 1"));
    lbl(&mut c, &|x, l| x.call_ind(Mem::Rip(l)), &|n| format!("call qword ptr [rip + {n}]"));
    lbl(&mut c, &|x, l| x.jmp_ind(Mem::Rip(l)), &|n| format!("jmp qword ptr [rip + {n}]"));
    c
}

fn tool(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok()
}

#[test]
fn encoder_matches_gnu_as() {
    if !tool("as") || !tool("objcopy") {
        eprintln!("skipping: GNU as/objcopy not installed");
        return;
    }
    let c = build_cases();
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("enc-vs-gas");
    std::fs::create_dir_all(&dir).unwrap();
    let mut src = String::from(".intel_syntax noprefix\n.text\n");
    for t in &c.text {
        src += t;
        src.push('\n');
    }
    std::fs::write(dir.join("t.s"), &src).unwrap();
    let st = Command::new("as").args(["--64", "-o"]).arg(dir.join("t.o")).arg(dir.join("t.s")).output().unwrap();
    assert!(st.status.success(), "as failed: {}", String::from_utf8_lossy(&st.stderr));
    let st = Command::new("objcopy").args(["-O", "binary", "--only-section=.text"]).arg(dir.join("t.o")).arg(dir.join("t.bin")).output().unwrap();
    assert!(st.status.success());
    let gas = std::fs::read(dir.join("t.bin")).unwrap();
    let mut off = 0;
    let mut bad = Vec::new();
    for (t, b) in c.text.iter().zip(&c.bytes) {
        let g = gas.get(off..off + b.len()).unwrap_or(&[]);
        if g != &b[..] {
            bad.push(format!("{t:?}: ours {:02x?} gas {:02x?}", b, &gas[off..(off + b.len() + 4).min(gas.len())]));
            if bad.len() > 20 {
                break;
            }
        }
        off += b.len();
    }
    assert!(bad.is_empty(), "{} mismatches (first shown; later offsets drift):\n{}", bad.len(), bad.join("\n"));
    assert_eq!(off, gas.len(), "total length differs");
    eprintln!("{} instructions identical to GNU as ({} bytes)", c.text.len(), gas.len());
}
