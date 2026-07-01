//! Minimal ELF relocatable-object parser (little-endian, ELF32/ELF64).
//!
//! Parses into a raw structural model independent of [`super::ObjectFile`],
//! so `tir readobj` can dump any relocatable ELF, not just ones we emitted.

use std::error::Error;
use std::fmt::{self, Display};

use super::elf::{EM_AARCH64, EM_RISCV, SHT_RELA, SHT_SYMTAB};
use super::format::ElfClass;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElfReadError {
    NotAnElf,
    UnsupportedEncoding(String),
    Truncated(&'static str),
}

impl Display for ElfReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ElfReadError::NotAnElf => write!(f, "not an ELF file"),
            ElfReadError::UnsupportedEncoding(what) => write!(f, "unsupported ELF: {what}"),
            ElfReadError::Truncated(what) => write!(f, "truncated ELF: {what}"),
        }
    }
}

impl Error for ElfReadError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfFile {
    pub class: ElfClass,
    pub etype: u16,
    pub machine: u16,
    pub flags: u32,
    pub sections: Vec<ElfSection>,
    pub symbols: Vec<ElfSymbol>,
    pub relocations: Vec<ElfRela>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfSection {
    pub name: String,
    pub sh_type: u32,
    pub flags: u64,
    pub size: u64,
    pub addralign: u64,
    pub link: u32,
    pub info: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfSymbol {
    pub name: String,
    pub value: u64,
    pub size: u64,
    pub binding: u8,
    pub sym_type: u8,
    /// Section name for defined symbols, `None` for undefined.
    pub section: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfRela {
    /// Name of the section the relocation applies to (e.g. `.text`).
    pub section: String,
    pub offset: u64,
    pub symbol: String,
    pub r_type: u32,
    pub addend: i64,
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    class: ElfClass,
}

impl<'a> Reader<'a> {
    fn at(bytes: &'a [u8], pos: usize, class: ElfClass) -> Self {
        Self { bytes, pos, class }
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], ElfReadError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(ElfReadError::Truncated(what))?;
        if end > self.bytes.len() {
            return Err(ElfReadError::Truncated(what));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, ElfReadError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, ElfReadError> {
        Ok(u16::from_le_bytes(self.take(2, what)?.try_into().unwrap()))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, ElfReadError> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into().unwrap()))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, ElfReadError> {
        Ok(u64::from_le_bytes(self.take(8, what)?.try_into().unwrap()))
    }

    fn addr(&mut self, what: &'static str) -> Result<u64, ElfReadError> {
        match self.class {
            ElfClass::Elf32 => Ok(self.u32(what)? as u64),
            ElfClass::Elf64 => self.u64(what),
        }
    }
}

fn str_at(table: &[u8], offset: usize) -> String {
    let end = table[offset..]
        .iter()
        .position(|b| *b == 0)
        .map(|p| offset + p)
        .unwrap_or(table.len());
    String::from_utf8_lossy(&table[offset..end]).into_owned()
}

pub fn parse_elf(bytes: &[u8]) -> Result<ElfFile, ElfReadError> {
    if bytes.len() < 16 || bytes[..4] != [0x7F, b'E', b'L', b'F'] {
        return Err(ElfReadError::NotAnElf);
    }
    let class = match bytes[4] {
        1 => ElfClass::Elf32,
        2 => ElfClass::Elf64,
        _ => {
            return Err(ElfReadError::UnsupportedEncoding(
                "unknown ELF class".into(),
            ));
        }
    };
    if bytes[5] != 1 {
        return Err(ElfReadError::UnsupportedEncoding(
            "big-endian objects are not supported".into(),
        ));
    }

    let mut r = Reader::at(bytes, 16, class);
    let etype = r.u16("e_type")?;
    let machine = r.u16("e_machine")?;
    r.u32("e_version")?;
    r.addr("e_entry")?;
    r.addr("e_phoff")?;
    let shoff = r.addr("e_shoff")?;
    let flags = r.u32("e_flags")?;
    r.u16("e_ehsize")?;
    r.u16("e_phentsize")?;
    r.u16("e_phnum")?;
    let shentsize = r.u16("e_shentsize")? as u64;
    let shnum = r.u16("e_shnum")? as u64;
    let shstrndx = r.u16("e_shstrndx")? as usize;

    // Raw header fields per section, before names are resolved.
    struct RawSection {
        name_off: u32,
        sh_type: u32,
        flags: u64,
        offset: u64,
        size: u64,
        link: u32,
        info: u32,
        addralign: u64,
    }

    let mut raw: Vec<RawSection> = Vec::new();
    for i in 0..shnum {
        let mut r = Reader::at(bytes, (shoff + i * shentsize) as usize, class);
        let name_off = r.u32("sh_name")?;
        let sh_type = r.u32("sh_type")?;
        let flags = r.addr("sh_flags")?;
        r.addr("sh_addr")?;
        let offset = r.addr("sh_offset")?;
        let size = r.addr("sh_size")?;
        let link = r.u32("sh_link")?;
        let info = r.u32("sh_info")?;
        let addralign = r.addr("sh_addralign")?;
        raw.push(RawSection {
            name_off,
            sh_type,
            flags,
            offset,
            size,
            link,
            info,
            addralign,
        });
    }

    let section_data = |s: &RawSection| -> Result<Vec<u8>, ElfReadError> {
        if s.sh_type == 8 {
            // SHT_NOBITS occupies no file space.
            return Ok(Vec::new());
        }
        let start = s.offset as usize;
        let end = start
            .checked_add(s.size as usize)
            .ok_or(ElfReadError::Truncated("section data"))?;
        if end > bytes.len() {
            return Err(ElfReadError::Truncated("section data"));
        }
        Ok(bytes[start..end].to_vec())
    };

    let shstrtab = raw
        .get(shstrndx)
        .map(section_data)
        .transpose()?
        .unwrap_or_default();

    let mut sections: Vec<ElfSection> = Vec::new();
    for s in &raw {
        sections.push(ElfSection {
            name: str_at(&shstrtab, s.name_off as usize),
            sh_type: s.sh_type,
            flags: s.flags,
            size: s.size,
            addralign: s.addralign,
            link: s.link,
            info: s.info,
            data: section_data(s)?,
        });
    }

    let section_name = |idx: usize| -> String {
        sections
            .get(idx)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("section({idx})"))
    };

    // Symbols: first SHT_SYMTAB section, names via its sh_link strtab.
    let mut symbols: Vec<ElfSymbol> = Vec::new();
    let mut symbol_names: Vec<String> = Vec::new();
    if let Some((idx, symtab)) = sections
        .iter()
        .enumerate()
        .find(|(_, s)| s.sh_type == SHT_SYMTAB)
    {
        let strtab = sections
            .get(raw[idx].link as usize)
            .map(|s| s.data.clone())
            .unwrap_or_default();
        let entsize = match class {
            ElfClass::Elf32 => 16,
            ElfClass::Elf64 => 24,
        };
        let count = symtab.data.len() / entsize;
        for i in 0..count {
            let mut r = Reader::at(&symtab.data, i * entsize, class);
            let name_off = r.u32("st_name")?;
            let (value, size, info, shndx);
            match class {
                ElfClass::Elf32 => {
                    value = r.u32("st_value")? as u64;
                    size = r.u32("st_size")? as u64;
                    info = r.u8("st_info")?;
                    r.u8("st_other")?;
                    shndx = r.u16("st_shndx")?;
                }
                ElfClass::Elf64 => {
                    info = r.u8("st_info")?;
                    r.u8("st_other")?;
                    shndx = r.u16("st_shndx")?;
                    value = r.u64("st_value")?;
                    size = r.u64("st_size")?;
                }
            }
            let name = str_at(&strtab, name_off as usize);
            symbol_names.push(name.clone());
            if i == 0 {
                continue; // the mandatory null symbol
            }
            symbols.push(ElfSymbol {
                name,
                value,
                size,
                binding: info >> 4,
                sym_type: info & 0xF,
                section: (shndx != 0 && shndx < 0xFF00).then(|| section_name(shndx as usize)),
            });
        }
    }

    let mut relocations: Vec<ElfRela> = Vec::new();
    for (idx, section) in sections.iter().enumerate() {
        if section.sh_type != SHT_RELA {
            continue;
        }
        let target = section_name(raw[idx].info as usize);
        let entsize = match class {
            ElfClass::Elf32 => 12,
            ElfClass::Elf64 => 24,
        };
        let count = section.data.len() / entsize;
        for i in 0..count {
            let mut r = Reader::at(&section.data, i * entsize, class);
            let offset = r.addr("r_offset")?;
            let (sym, r_type, addend);
            match class {
                ElfClass::Elf32 => {
                    let info = r.u32("r_info")?;
                    sym = (info >> 8) as usize;
                    r_type = info & 0xFF;
                    addend = r.u32("r_addend")? as i32 as i64;
                }
                ElfClass::Elf64 => {
                    let info = r.u64("r_info")?;
                    sym = (info >> 32) as usize;
                    r_type = info as u32;
                    addend = r.u64("r_addend")? as i64;
                }
            }
            relocations.push(ElfRela {
                section: target.clone(),
                offset,
                symbol: symbol_names.get(sym).cloned().unwrap_or_default(),
                r_type,
                addend,
            });
        }
    }

    Ok(ElfFile {
        class,
        etype,
        machine,
        flags,
        sections,
        symbols,
        relocations,
    })
}

/// Human-readable name for relocation types we know about.
pub fn reloc_name(machine: u16, r_type: u32) -> Option<&'static str> {
    match (machine, r_type) {
        (EM_RISCV, 16) => Some("R_RISCV_BRANCH"),
        (EM_RISCV, 17) => Some("R_RISCV_JAL"),
        (EM_RISCV, 18) => Some("R_RISCV_CALL"),
        (EM_RISCV, 19) => Some("R_RISCV_CALL_PLT"),
        (EM_AARCH64, 280) => Some("R_AARCH64_CONDBR19"),
        (EM_AARCH64, 282) => Some("R_AARCH64_JUMP26"),
        (EM_AARCH64, 283) => Some("R_AARCH64_CALL26"),
        _ => None,
    }
}
