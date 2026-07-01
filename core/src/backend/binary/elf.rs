//! Hand-rolled ELF relocatable-object emitter (little-endian, ELF32/ELF64).

use std::collections::HashMap;

use super::format::{ElfClass, ObjectFormatInfo};
use super::{ObjSymbol, ObjectFile, SymBinding, SymKind};

pub const EM_RISCV: u16 = 243;
pub const EM_AARCH64: u16 = 183;

pub(crate) const ET_REL: u16 = 1;
pub(crate) const SHT_PROGBITS: u32 = 1;
pub(crate) const SHT_SYMTAB: u32 = 2;
pub(crate) const SHT_STRTAB: u32 = 3;
pub(crate) const SHT_RELA: u32 = 4;
pub(crate) const SHF_ALLOC: u64 = 0x2;
pub(crate) const SHF_EXECINSTR: u64 = 0x4;
pub(crate) const STB_LOCAL: u8 = 0;
pub(crate) const STB_GLOBAL: u8 = 1;
pub(crate) const STT_NOTYPE: u8 = 0;
pub(crate) const STT_FUNC: u8 = 2;
pub(crate) const SHN_UNDEF: u16 = 0;

/// Class-aware little-endian serializer.
struct Enc<'a> {
    buf: &'a mut Vec<u8>,
    class: ElfClass,
}

impl Enc<'_> {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// An `Elf_Addr`/`Elf_Off`/`Elf_Xword`-sized field: 4 or 8 bytes.
    fn addr(&mut self, v: u64) {
        match self.class {
            ElfClass::Elf32 => self.u32(v as u32),
            ElfClass::Elf64 => self.u64(v),
        }
    }
}

/// An in-progress section: header fields plus body bytes.
struct Section {
    name: String,
    sh_type: u32,
    sh_flags: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
    data: Vec<u8>,
}

struct StrTab {
    data: Vec<u8>,
    offsets: HashMap<String, u32>,
}

impl StrTab {
    fn new() -> Self {
        Self {
            data: vec![0],
            offsets: HashMap::new(),
        }
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(off) = self.offsets.get(s) {
            return *off;
        }
        let off = self.data.len() as u32;
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
        self.offsets.insert(s.to_string(), off);
        off
    }
}

fn sym_entry(enc: &mut Enc, name: u32, info: u8, shndx: u16, value: u64, size: u64) {
    enc.u32(name);
    match enc.class {
        ElfClass::Elf32 => {
            enc.u32(value as u32);
            enc.u32(size as u32);
            enc.u8(info);
            enc.u8(0);
            enc.u16(shndx);
        }
        ElfClass::Elf64 => {
            enc.u8(info);
            enc.u8(0);
            enc.u16(shndx);
            enc.u64(value);
            enc.u64(size);
        }
    }
}

fn sym_info(sym: &ObjSymbol) -> u8 {
    let bind = match sym.binding {
        SymBinding::Local => STB_LOCAL,
        SymBinding::Global => STB_GLOBAL,
    };
    let kind = match sym.kind {
        SymKind::NoType => STT_NOTYPE,
        SymKind::Func => STT_FUNC,
    };
    (bind << 4) | kind
}

/// Serialize `obj` as an ELF relocatable object (`ET_REL`).
pub fn write_elf(obj: &ObjectFile, fmt: &ObjectFormatInfo) -> Vec<u8> {
    let class = fmt.elf_class;

    // Section indices: [0]=NULL, then each object section followed by its
    // .rela section (if any), then .symtab, .strtab, .shstrtab.
    let mut section_index: Vec<u32> = Vec::new(); // ObjSection idx -> ELF idx
    let mut next = 1u32;
    let mut rela_index: Vec<Option<u32>> = Vec::new();
    for section in &obj.sections {
        section_index.push(next);
        next += 1;
        if section.relocs.is_empty() {
            rela_index.push(None);
        } else {
            rela_index.push(Some(next));
            next += 1;
        }
    }
    let symtab_index = next;
    let strtab_index = next + 1;
    let shstrtab_index = next + 2;

    // Symbol table: null, locals, globals (defined first, then synthesized
    // undefined entries for relocation targets without a definition).
    let mut strtab = StrTab::new();
    let mut sym_order: Vec<&ObjSymbol> = obj.symbols.iter().collect();
    sym_order.sort_by_key(|s| matches!(s.binding, SymBinding::Global));
    let undefined: Vec<ObjSymbol> = {
        let mut seen: Vec<&str> = sym_order.iter().map(|s| s.name.as_str()).collect();
        let mut undef = Vec::new();
        for section in &obj.sections {
            for reloc in &section.relocs {
                if !seen.contains(&reloc.symbol.as_str()) {
                    seen.push(&reloc.symbol);
                    undef.push(ObjSymbol {
                        name: reloc.symbol.clone(),
                        section: None,
                        value: 0,
                        size: 0,
                        binding: SymBinding::Global,
                        kind: SymKind::NoType,
                    });
                }
            }
        }
        undef
    };

    let first_global = 1 + sym_order
        .iter()
        .filter(|s| matches!(s.binding, SymBinding::Local))
        .count() as u32;

    let all_syms: Vec<&ObjSymbol> = sym_order.iter().copied().chain(undefined.iter()).collect();
    let mut sym_index: HashMap<&str, u32> = HashMap::new();
    let mut symtab_data = Vec::new();
    {
        let mut enc = Enc {
            buf: &mut symtab_data,
            class,
        };
        sym_entry(&mut enc, 0, 0, SHN_UNDEF, 0, 0);
        for (i, sym) in all_syms.iter().enumerate() {
            let name = strtab.intern(&sym.name);
            let shndx = sym
                .section
                .map(|s| section_index[s] as u16)
                .unwrap_or(SHN_UNDEF);
            sym_entry(&mut enc, name, sym_info(sym), shndx, sym.value, sym.size);
            sym_index.insert(&sym.name, i as u32 + 1);
        }
    }

    let (sym_entsize, rela_entsize) = match class {
        ElfClass::Elf32 => (16u64, 12u64),
        ElfClass::Elf64 => (24u64, 24u64),
    };

    let mut sections: Vec<Section> = Vec::new();
    for (idx, section) in obj.sections.iter().enumerate() {
        sections.push(Section {
            name: section.name.clone(),
            sh_type: SHT_PROGBITS,
            sh_flags: SHF_ALLOC | SHF_EXECINSTR,
            sh_link: 0,
            sh_info: 0,
            sh_addralign: section.align,
            sh_entsize: 0,
            data: section.data.clone(),
        });
        if !section.relocs.is_empty() {
            let mut data = Vec::new();
            let mut enc = Enc {
                buf: &mut data,
                class,
            };
            for reloc in &section.relocs {
                let sym = sym_index.get(reloc.symbol.as_str()).copied().unwrap_or(0);
                enc.addr(reloc.offset);
                match class {
                    ElfClass::Elf32 => {
                        enc.u32((sym << 8) | (reloc.r_type & 0xFF));
                        enc.u32(reloc.addend as i32 as u32);
                    }
                    ElfClass::Elf64 => {
                        enc.u64(((sym as u64) << 32) | reloc.r_type as u64);
                        enc.u64(reloc.addend as u64);
                    }
                }
            }
            sections.push(Section {
                name: format!(".rela{}", section.name),
                sh_type: SHT_RELA,
                sh_flags: 0,
                sh_link: symtab_index,
                sh_info: section_index[idx],
                sh_addralign: match class {
                    ElfClass::Elf32 => 4,
                    ElfClass::Elf64 => 8,
                },
                sh_entsize: rela_entsize,
                data,
            });
        }
    }
    sections.push(Section {
        name: ".symtab".to_string(),
        sh_type: SHT_SYMTAB,
        sh_flags: 0,
        sh_link: strtab_index,
        sh_info: first_global,
        sh_addralign: match class {
            ElfClass::Elf32 => 4,
            ElfClass::Elf64 => 8,
        },
        sh_entsize: sym_entsize,
        data: symtab_data,
    });
    sections.push(Section {
        name: ".strtab".to_string(),
        sh_type: SHT_STRTAB,
        sh_flags: 0,
        sh_link: 0,
        sh_info: 0,
        sh_addralign: 1,
        sh_entsize: 0,
        data: strtab.data,
    });

    let mut shstrtab = StrTab::new();
    let mut name_offsets: Vec<u32> = sections.iter().map(|s| shstrtab.intern(&s.name)).collect();
    let shstrtab_name = shstrtab.intern(".shstrtab");
    sections.push(Section {
        name: ".shstrtab".to_string(),
        sh_type: SHT_STRTAB,
        sh_flags: 0,
        sh_link: 0,
        sh_info: 0,
        sh_addralign: 1,
        sh_entsize: 0,
        data: shstrtab.data,
    });
    name_offsets.push(shstrtab_name);

    let (ehsize, shentsize) = match class {
        ElfClass::Elf32 => (52u64, 40u64),
        ElfClass::Elf64 => (64u64, 64u64),
    };

    // Body layout: header, section bodies (8-aligned), section header table.
    let mut offsets: Vec<u64> = Vec::new();
    let mut pos = ehsize;
    for section in &sections {
        let align = section.sh_addralign.max(1);
        pos = pos.div_ceil(align) * align;
        offsets.push(pos);
        pos += section.data.len() as u64;
    }
    let shoff = pos.div_ceil(8) * 8;
    let shnum = sections.len() as u16 + 1; // + NULL section

    let mut out = Vec::with_capacity((shoff + shnum as u64 * shentsize) as usize);
    {
        let mut enc = Enc {
            buf: &mut out,
            class,
        };
        enc.buf.extend_from_slice(&[0x7F, b'E', b'L', b'F']);
        enc.u8(match class {
            ElfClass::Elf32 => 1,
            ElfClass::Elf64 => 2,
        });
        enc.u8(1); // ELFDATA2LSB
        enc.u8(1); // EV_CURRENT
        enc.buf.extend_from_slice(&[0; 9]);
        enc.u16(ET_REL);
        enc.u16(fmt.elf_machine);
        enc.u32(1); // e_version
        enc.addr(0); // e_entry
        enc.addr(0); // e_phoff
        enc.addr(shoff);
        enc.u32(fmt.elf_flags);
        enc.u16(ehsize as u16);
        enc.u16(0); // e_phentsize
        enc.u16(0); // e_phnum
        enc.u16(shentsize as u16);
        enc.u16(shnum);
        enc.u16(shstrtab_index as u16);
    }
    for (section, offset) in sections.iter().zip(&offsets) {
        out.resize(*offset as usize, 0);
        out.extend_from_slice(&section.data);
    }
    out.resize(shoff as usize, 0);
    {
        let mut enc = Enc {
            buf: &mut out,
            class,
        };
        // NULL section header.
        for _ in 0..shentsize {
            enc.u8(0);
        }
        for ((section, offset), name) in sections.iter().zip(&offsets).zip(&name_offsets) {
            enc.u32(*name);
            enc.u32(section.sh_type);
            enc.addr(section.sh_flags);
            enc.addr(0); // sh_addr
            enc.addr(*offset);
            enc.addr(section.data.len() as u64);
            enc.u32(section.sh_link);
            enc.u32(section.sh_info);
            enc.addr(section.sh_addralign);
            enc.addr(section.sh_entsize);
        }
    }
    out
}
