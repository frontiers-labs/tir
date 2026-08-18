//! ELF object writing/reading and the ASCII object rendering.

use tir::backend::binary::{
    parse_elf, render_ascii, write_elf, ElfClass, ElfFile, ElfReadError, ElfRela, ElfSection,
    ObjReloc, ObjSection, ObjSymbol, ObjectFile, ObjectFormatInfo, SectionKind, SymBinding,
    SymKind, EM_RISCV,
};

fn sample_object() -> ObjectFile {
    ObjectFile {
        sections: vec![ObjSection {
            name: ".text".to_string(),
            kind: SectionKind::Text,
            align: 4,
            data: vec![
                0x33, 0x85, 0xC5, 0x00, // add a0, a1, a2
                0x67, 0x80, 0x00, 0x00, // ret
                0xEF, 0x00, 0x00, 0x00, // jal ra, <reloc>
            ],
            relocs: vec![ObjReloc {
                offset: 8,
                symbol: "callee".to_string(),
                r_type: 17,
                addend: 0,
            }],
            insn_spans: vec![(0, 4), (4, 4), (8, 4)],
        }],
        symbols: vec![ObjSymbol {
            name: "caller".to_string(),
            section: Some(0),
            value: 0,
            size: 12,
            binding: SymBinding::Global,
            kind: SymKind::Func,
        }],
    }
}

fn format_info(class: ElfClass) -> ObjectFormatInfo {
    ObjectFormatInfo {
        elf_machine: EM_RISCV,
        elf_class: class,
        elf_flags: 0,
        reloc_for: |_| None,
        absolute_reloc: |_| None,
        pc_rel_scale: |_| 0,
        pc_rel_from_end: |_| false,
    }
}

fn roundtrip(class: ElfClass) -> ElfFile {
    let obj = sample_object();
    let bytes = write_elf(&obj, &format_info(class));
    parse_elf(&bytes).expect("emitted ELF parses back")
}

fn roundtrip_section(name: &str, kind: SectionKind, data: Vec<u8>) -> ElfSection {
    let object = ObjectFile {
        sections: vec![ObjSection {
            name: name.to_string(),
            kind,
            align: 4,
            data,
            relocs: Vec::new(),
            insn_spans: Vec::new(),
        }],
        symbols: Vec::new(),
    };
    let bytes = write_elf(&object, &format_info(ElfClass::Elf64));
    parse_elf(&bytes)
        .expect("emitted ELF parses back")
        .sections
        .into_iter()
        .find(|section| section.name == name)
        .unwrap()
}

fn check_roundtrip(class: ElfClass) {
    let elf = roundtrip(class);
    assert_eq!(elf.class, class);
    assert_eq!(elf.machine, EM_RISCV);
    assert_eq!(elf.etype, 1, "ET_REL");
    assert_eq!(elf.flags, 0);

    let names: Vec<&str> = elf.sections.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        ["", ".text", ".rela.text", ".symtab", ".strtab", ".shstrtab"]
    );

    let text = &elf.sections[1];
    assert_eq!(text.data, sample_object().sections[0].data);
    assert_eq!(text.flags, 0x6, "SHF_ALLOC | SHF_EXECINSTR");
    assert_eq!(text.addralign, 4);

    // Defined function symbol plus the synthesized undefined reloc target.
    let caller = elf.symbols.iter().find(|s| s.name == "caller").unwrap();
    assert_eq!(caller.section.as_deref(), Some(".text"));
    assert_eq!(caller.value, 0);
    assert_eq!(caller.size, 12);
    assert_eq!(caller.binding, 1, "STB_GLOBAL");
    assert_eq!(caller.sym_type, 2, "STT_FUNC");

    let callee = elf.symbols.iter().find(|s| s.name == "callee").unwrap();
    assert_eq!(callee.section, None);
    assert_eq!(callee.binding, 1, "STB_GLOBAL");
    assert_eq!(callee.sym_type, 0, "STT_NOTYPE");

    assert_eq!(
        elf.relocations,
        vec![ElfRela {
            section: ".text".to_string(),
            offset: 8,
            symbol: "callee".to_string(),
            r_type: 17,
            addend: 0,
        }]
    );
}

#[test]
fn elf64_roundtrip() {
    check_roundtrip(ElfClass::Elf64);
}

#[test]
fn elf32_roundtrip() {
    check_roundtrip(ElfClass::Elf32);
}

#[test]
fn bss_is_encoded_as_writable_nobits() {
    let bss = roundtrip_section(".bss", SectionKind::UninitializedData, vec![0; 4]);

    assert_eq!(bss.sh_type, 8, "SHT_NOBITS");
    assert_eq!(bss.flags, 0x3, "SHF_WRITE | SHF_ALLOC");
    assert_eq!(bss.size, 4);
    assert!(bss.data.is_empty());
}

#[test]
fn initialized_data_is_writable() {
    let data = roundtrip_section(".data", SectionKind::Data, vec![1, 2, 3, 4]);

    assert_eq!(data.sh_type, 1, "SHT_PROGBITS");
    assert_eq!(data.flags, 0x3, "SHF_WRITE | SHF_ALLOC");
    assert_eq!(data.data, vec![1, 2, 3, 4]);
}

#[test]
fn read_only_data_is_not_writable() {
    let rodata = roundtrip_section(".rodata", SectionKind::ReadOnlyData, vec![1, 2, 3, 4]);

    assert_eq!(rodata.sh_type, 1, "SHT_PROGBITS");
    assert_eq!(rodata.flags, 0x2, "SHF_ALLOC");
    assert_eq!(rodata.data, vec![1, 2, 3, 4]);
}

#[test]
fn ascii_rendering_is_stable() {
    let rendered = render_ascii(&sample_object());
    assert_eq!(
        rendered,
        ".section .text\n\
         caller:\n\
         \x20 [0x33, 0x85, 0xC5, 0x00]\n\
         \x20 [0x67, 0x80, 0x00, 0x00]\n\
         \x20 [0xEF, 0x00, 0x00, 0x00]\n"
    );
}

#[test]
fn parse_rejects_non_elf() {
    assert_eq!(parse_elf(b"not an elf"), Err(ElfReadError::NotAnElf));
}
