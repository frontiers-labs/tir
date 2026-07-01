use std::error::Error;
use std::ffi::OsString;
use std::io::Read;

use clap::Parser;
use tir::backend::binary::{ElfClass, ElfFile, parse_elf, reloc_name};

#[derive(Parser)]
pub struct ToolArgs {
    /// Input object file; `-` reads from stdin
    input: OsString,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let bytes = if args.input == "-" {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        std::fs::read(&args.input)?
    };
    let elf = parse_elf(&bytes)?;
    print!("{}", render(&elf));
    Ok(())
}

fn render(elf: &ElfFile) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let class = match elf.class {
        ElfClass::Elf32 => "ELF32",
        ElfClass::Elf64 => "ELF64",
    };
    let etype = match elf.etype {
        1 => "REL".to_string(),
        2 => "EXEC".to_string(),
        3 => "DYN".to_string(),
        other => format!("type({other})"),
    };
    let _ = writeln!(out, "File: {class} LSB {etype}");
    let _ = writeln!(
        out,
        "Machine: {} ({})",
        machine_name(elf.machine),
        elf.machine
    );
    let _ = writeln!(out, "Flags: {:#x}", elf.flags);

    let _ = writeln!(out);
    for section in elf.sections.iter().filter(|s| !s.name.is_empty()) {
        let _ = writeln!(
            out,
            "Section {}: type={} flags={} size={:#x} align={}",
            section.name,
            section_type(section.sh_type),
            section_flags(section.flags),
            section.size,
            section.addralign,
        );
    }

    let _ = writeln!(out);
    for sym in &elf.symbols {
        let _ = writeln!(
            out,
            "Symbol {}: value={:#x} size={:#x} bind={} type={} section={}",
            sym.name,
            sym.value,
            sym.size,
            match sym.binding {
                0 => "LOCAL".to_string(),
                1 => "GLOBAL".to_string(),
                2 => "WEAK".to_string(),
                other => format!("bind({other})"),
            },
            match sym.sym_type {
                0 => "NOTYPE".to_string(),
                1 => "OBJECT".to_string(),
                2 => "FUNC".to_string(),
                3 => "SECTION".to_string(),
                other => format!("type({other})"),
            },
            sym.section.as_deref().unwrap_or("UND"),
        );
    }

    if !elf.relocations.is_empty() {
        let _ = writeln!(out);
        for reloc in &elf.relocations {
            let name = reloc_name(elf.machine, reloc.r_type)
                .map(str::to_string)
                .unwrap_or_else(|| format!("reloc({})", reloc.r_type));
            let _ = writeln!(
                out,
                "Reloc {}+{:#x}: {} {} + {}",
                reloc.section, reloc.offset, name, reloc.symbol, reloc.addend,
            );
        }
    }
    out
}

fn machine_name(machine: u16) -> String {
    match machine {
        183 => "EM_AARCH64".to_string(),
        243 => "EM_RISCV".to_string(),
        other => format!("machine({other})"),
    }
}

fn section_type(sh_type: u32) -> String {
    match sh_type {
        1 => "PROGBITS".to_string(),
        2 => "SYMTAB".to_string(),
        3 => "STRTAB".to_string(),
        4 => "RELA".to_string(),
        8 => "NOBITS".to_string(),
        9 => "REL".to_string(),
        other => format!("type({other})"),
    }
}

fn section_flags(flags: u64) -> String {
    let mut s = String::new();
    if flags & 0x1 != 0 {
        s.push('W');
    }
    if flags & 0x2 != 0 {
        s.push('A');
    }
    if flags & 0x4 != 0 {
        s.push('X');
    }
    if s.is_empty() {
        s.push('0');
    }
    s
}
