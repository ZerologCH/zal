//! ELF64 relocatable object writer (x86-64, little endian).
//!
//! Sections: `.text`, `.data`, `.rodata`, `.note.GNU-stack` (non-executable
//! stack marker), `.symtab`, `.strtab`, `.shstrtab` and `.rela.text`,
//! `.rela.data`, `.rela.rodata`. Relocations: `R_X86_64_64`, `R_X86_64_PC32`,
//! `R_X86_64_PLT32`.

use crate::enc::RelocKind;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Text,
    Data,
    Rodata,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymKind {
    NoType,
    Object,
    Func,
}

#[derive(Clone, Debug)]
struct Symbol {
    name: String,
    /// `None` = undefined.
    section: Option<Section>,
    value: u64,
    size: u64,
    kind: SymKind,
    global: bool,
}

#[derive(Clone, Debug)]
struct Rela {
    offset: u64,
    sym: usize,
    kind: RelocKind,
    addend: i64,
}

/// Builder for one relocatable object.
#[derive(Default)]
pub struct ObjectWriter {
    pub text: Vec<u8>,
    pub data: Vec<u8>,
    pub rodata: Vec<u8>,
    syms: Vec<Symbol>,
    relas: [Vec<Rela>; 3],
}

const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHF_WRITE: u64 = 1;
const SHF_ALLOC: u64 = 2;
const SHF_EXECINSTR: u64 = 4;
const SHF_INFO_LINK: u64 = 0x40;

pub const R_X86_64_64: u32 = 1;
pub const R_X86_64_PC32: u32 = 2;
pub const R_X86_64_PLT32: u32 = 4;

fn sec_index(s: Section) -> u16 {
    match s {
        Section::Text => 1,
        Section::Data => 2,
        Section::Rodata => 3,
    }
}

impl ObjectWriter {
    pub fn new() -> ObjectWriter {
        ObjectWriter::default()
    }

    pub fn section_mut(&mut self, s: Section) -> &mut Vec<u8> {
        match s {
            Section::Text => &mut self.text,
            Section::Data => &mut self.data,
            Section::Rodata => &mut self.rodata,
        }
    }

    /// Define a symbol at `value` in `section`.
    pub fn define(&mut self, name: &str, section: Section, value: u64, size: u64, kind: SymKind, global: bool) -> usize {
        if let Some(i) = self.syms.iter().position(|s| s.name == name) {
            assert!(self.syms[i].section.is_none(), "symbol {name} defined twice");
            self.syms[i] = Symbol { name: name.into(), section: Some(section), value, size, kind, global };
            return i;
        }
        self.syms.push(Symbol { name: name.into(), section: Some(section), value, size, kind, global });
        self.syms.len() - 1
    }

    /// Index of a symbol by name, creating an undefined global if needed.
    pub fn symbol(&mut self, name: &str) -> usize {
        if let Some(i) = self.syms.iter().position(|s| s.name == name) {
            return i;
        }
        self.syms.push(Symbol { name: name.into(), section: None, value: 0, size: 0, kind: SymKind::NoType, global: true });
        self.syms.len() - 1
    }

    /// Add a relocation in `section` at `offset` against symbol `name`.
    pub fn reloc(&mut self, section: Section, offset: u64, name: &str, kind: RelocKind, addend: i64) {
        let sym = self.symbol(name);
        self.relas[sec_index(section) as usize - 1].push(Rela { offset, sym, kind, addend });
    }

    /// Serialize the object file.
    pub fn finish(&self) -> Vec<u8> {
        // Symbol table: null, three section symbols, locals, then globals.
        let mut order: Vec<usize> = (0..self.syms.len()).filter(|&i| !self.syms[i].global).collect();
        let first_global = 4 + order.len();
        order.extend((0..self.syms.len()).filter(|&i| self.syms[i].global));
        let mut final_index = vec![0usize; self.syms.len()];
        for (k, &i) in order.iter().enumerate() {
            final_index[i] = 4 + k;
        }
        let mut strtab = vec![0u8];
        let mut symtab = vec![0u8; 24];
        for s in 1..=3u16 {
            sym_entry(&mut symtab, 0, 3, s, 0, 0); // STT_SECTION, STB_LOCAL
        }
        for &i in &order {
            let s = &self.syms[i];
            let name = strtab.len() as u32;
            strtab.extend_from_slice(s.name.as_bytes());
            strtab.push(0);
            let ty = match s.kind {
                SymKind::NoType => 0,
                SymKind::Object => 1,
                SymKind::Func => 2,
            };
            let info = (s.global as u8) << 4 | ty;
            sym_entry(&mut symtab, name, info, s.section.map_or(0, sec_index), s.value, s.size);
        }
        let rela_bytes = |rs: &[Rela]| {
            let mut v = Vec::with_capacity(rs.len() * 24);
            for r in rs {
                let ty = match r.kind {
                    RelocKind::Abs64 => R_X86_64_64,
                    RelocKind::Pc32 => R_X86_64_PC32,
                    RelocKind::Plt32 => R_X86_64_PLT32,
                };
                v.extend_from_slice(&r.offset.to_le_bytes());
                v.extend_from_slice(&((final_index[r.sym] as u64) << 32 | ty as u64).to_le_bytes());
                v.extend_from_slice(&r.addend.to_le_bytes());
            }
            v
        };

        struct Sh {
            name: &'static str,
            ty: u32,
            flags: u64,
            data: Vec<u8>,
            link: u32,
            info: u32,
            align: u64,
            entsize: u64,
        }
        let sh = |name, ty, flags, data, link, info, align, entsize| Sh { name, ty, flags, data, link, info, align, entsize };
        // Indices: 1 .text 2 .data 3 .rodata 4 .note.GNU-stack 5 .symtab 6 .strtab
        // 7 .shstrtab 8 .rela.text 9 .rela.data 10 .rela.rodata
        let secs = vec![
            sh(".text", SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, self.text.clone(), 0, 0, 16, 0),
            sh(".data", SHT_PROGBITS, SHF_ALLOC | SHF_WRITE, self.data.clone(), 0, 0, 8, 0),
            sh(".rodata", SHT_PROGBITS, SHF_ALLOC, self.rodata.clone(), 0, 0, 8, 0),
            sh(".note.GNU-stack", SHT_PROGBITS, 0, vec![], 0, 0, 1, 0),
            sh(".symtab", SHT_SYMTAB, 0, symtab, 6, first_global as u32, 8, 24),
            sh(".strtab", SHT_STRTAB, 0, strtab, 0, 0, 1, 0),
            sh(".shstrtab", SHT_STRTAB, 0, vec![], 0, 0, 1, 0),
            sh(".rela.text", SHT_RELA, SHF_INFO_LINK, rela_bytes(&self.relas[0]), 5, 1, 8, 24),
            sh(".rela.data", SHT_RELA, SHF_INFO_LINK, rela_bytes(&self.relas[1]), 5, 2, 8, 24),
            sh(".rela.rodata", SHT_RELA, SHF_INFO_LINK, rela_bytes(&self.relas[2]), 5, 3, 8, 24),
        ];
        let mut shstrtab = vec![0u8];
        let mut names = Vec::new();
        for s in &secs {
            names.push(shstrtab.len() as u32);
            shstrtab.extend_from_slice(s.name.as_bytes());
            shstrtab.push(0);
        }
        let shnum = secs.len() + 1;
        let shstrndx = 7u16;

        let mut out = vec![0u8; 64];
        let mut offsets = Vec::new();
        for s in &secs {
            let data = if s.name == ".shstrtab" { &shstrtab } else { &s.data };
            while out.len() % s.align as usize != 0 {
                out.push(0);
            }
            offsets.push(out.len() as u64);
            out.extend_from_slice(data);
        }
        while out.len() % 8 != 0 {
            out.push(0);
        }
        let shoff = out.len() as u64;
        out.extend_from_slice(&[0u8; 64]); // null section header
        for (k, s) in secs.iter().enumerate() {
            let size = if s.name == ".shstrtab" { shstrtab.len() } else { s.data.len() } as u64;
            out.extend_from_slice(&names[k].to_le_bytes());
            out.extend_from_slice(&s.ty.to_le_bytes());
            out.extend_from_slice(&s.flags.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes()); // addr
            out.extend_from_slice(&offsets[k].to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&s.link.to_le_bytes());
            out.extend_from_slice(&s.info.to_le_bytes());
            out.extend_from_slice(&s.align.to_le_bytes());
            out.extend_from_slice(&s.entsize.to_le_bytes());
        }
        // ELF header.
        let h = &mut out[..64];
        h[..16].copy_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        h[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
        h[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        h[20..24].copy_from_slice(&1u32.to_le_bytes());
        h[40..48].copy_from_slice(&shoff.to_le_bytes());
        h[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        h[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
        h[60..62].copy_from_slice(&(shnum as u16).to_le_bytes());
        h[62..64].copy_from_slice(&shstrndx.to_le_bytes());
        out
    }
}

fn sym_entry(t: &mut Vec<u8>, name: u32, info: u8, shndx: u16, value: u64, size: u64) {
    t.extend_from_slice(&name.to_le_bytes());
    t.push(info);
    t.push(0); // st_other: default visibility
    t.extend_from_slice(&shndx.to_le_bytes());
    t.extend_from_slice(&value.to_le_bytes());
    t.extend_from_slice(&size.to_le_bytes());
}
