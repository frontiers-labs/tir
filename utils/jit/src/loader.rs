//! Maps an [`ObjectFile`] into executable memory and resolves its relocations
//! against runtime addresses.
//!
//! Layout is a single mapping holding, in order, the text sections, one
//! trampoline per external branch target, then the data sections. External
//! calls route through trampolines so a 64-bit host address is reachable from a
//! short pc-relative branch. The whole mapping is made read+execute once
//! patched, so writable globals are not supported (adequate for the
//! microbenchmarks this JIT targets).

use std::collections::HashMap;

use tir::backend::binary::{ObjectFile, ObjectFormatInfo, SectionKind, SymBinding};

use crate::reloc;
use crate::{ExecMap, JitError};

/// A section placed at a known offset within the mapping.
struct Placed {
    index: usize,
    offset: usize,
}

pub struct Loaded {
    pub map: ExecMap,
    /// Defined symbol name → runtime address.
    pub symbols: HashMap<String, usize>,
}

pub fn load(
    obj: &ObjectFile,
    fmt: &ObjectFormatInfo,
    host_symbols: &HashMap<String, usize>,
) -> Result<Loaded, JitError> {
    let machine = fmt.elf_machine;

    // Defined symbols: name → (section index, value offset).
    let defined: HashMap<&str, &_> = obj
        .symbols
        .iter()
        .filter(|s| s.section.is_some())
        .map(|s| (s.name.as_str(), s))
        .collect();

    // External branch targets that need a trampoline.
    let mut ext_branch: Vec<&str> = Vec::new();
    for section in &obj.sections {
        for r in &section.relocs {
            if !defined.contains_key(r.symbol.as_str())
                && reloc::needs_trampoline(machine, r.r_type)
                && !ext_branch.contains(&r.symbol.as_str())
            {
                ext_branch.push(&r.symbol);
            }
        }
    }

    // Lay out: text sections, trampolines, data sections.
    let mut cursor = 0usize;
    let mut placed: Vec<Placed> = Vec::new();
    let place = |kind: SectionKind, cursor: &mut usize, placed: &mut Vec<Placed>| {
        for (index, section) in obj.sections.iter().enumerate() {
            if section.kind != kind {
                continue;
            }
            *cursor = align_up(*cursor, section.align.max(1) as usize);
            placed.push(Placed {
                index,
                offset: *cursor,
            });
            *cursor += section.data.len();
        }
    };
    place(SectionKind::Text, &mut cursor, &mut placed);

    let tramp_size = if ext_branch.is_empty() {
        0
    } else {
        reloc::trampoline_size(machine).ok_or(JitError::RelocUnsupported { machine, r_type: 0 })?
    };
    cursor = align_up(cursor, 16);
    let mut tramp_off: HashMap<&str, usize> = HashMap::new();
    for name in &ext_branch {
        tramp_off.insert(name, cursor);
        cursor += tramp_size;
    }

    place(SectionKind::Data, &mut cursor, &mut placed);

    let total = cursor.max(1);
    let map = ExecMap::new(total)?;
    let base = map.ptr() as usize;

    let section_addr = |index: usize| -> Option<usize> {
        placed
            .iter()
            .find(|p| p.index == index)
            .map(|p| base + p.offset)
    };

    // Copy section bytes.
    for p in &placed {
        let data = &obj.sections[p.index].data;
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), map.ptr().add(p.offset), data.len());
        }
    }

    // Emit trampolines.
    for name in &ext_branch {
        let host = *host_symbols
            .get(*name)
            .ok_or_else(|| JitError::UnresolvedSymbol(name.to_string()))?;
        let code = reloc::trampoline(machine, host as u64)?;
        let off = tramp_off[name];
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), map.ptr().add(off), code.len());
        }
    }

    // Defined symbol addresses.
    let mut symbols: HashMap<String, usize> = HashMap::new();
    for s in &obj.symbols {
        if let Some(sec) = s.section {
            let addr = section_addr(sec).ok_or_else(|| JitError::Emit {
                message: format!("symbol '{}' in unplaced section", s.name),
            })? + s.value as usize;
            if s.binding == SymBinding::Global || !symbols.contains_key(&s.name) {
                symbols.insert(s.name.clone(), addr);
            }
        }
    }

    // Apply relocations.
    for (index, section) in obj.sections.iter().enumerate() {
        let Some(sec_addr) = section_addr(index) else {
            continue;
        };
        for r in &section.relocs {
            let p = (sec_addr + r.offset as usize) as u64;
            let s = resolve(
                &r.symbol,
                machine,
                r.r_type,
                &defined,
                &section_addr,
                &tramp_off,
                base,
                host_symbols,
            )?;
            let site = (sec_addr + r.offset as usize) as *mut u8;
            reloc::apply(machine, r.r_type, site, s, p, r.addend)?;
        }
    }

    reloc::flush_icache(map.ptr(), total);
    map.make_executable()?;

    Ok(Loaded { map, symbols })
}

#[allow(clippy::too_many_arguments)]
fn resolve(
    symbol: &str,
    machine: u16,
    r_type: u32,
    defined: &HashMap<&str, &tir::backend::binary::ObjSymbol>,
    section_addr: &impl Fn(usize) -> Option<usize>,
    tramp_off: &HashMap<&str, usize>,
    base: usize,
    host_symbols: &HashMap<String, usize>,
) -> Result<u64, JitError> {
    if let Some(sym) = defined.get(symbol) {
        let sec = sym.section.expect("defined symbol has a section");
        let addr = section_addr(sec).ok_or_else(|| JitError::Emit {
            message: format!("symbol '{symbol}' in unplaced section"),
        })? + sym.value as usize;
        return Ok(addr as u64);
    }
    if reloc::needs_trampoline(machine, r_type) {
        return Ok((base + tramp_off[symbol]) as u64);
    }
    host_symbols
        .get(symbol)
        .map(|a| *a as u64)
        .ok_or_else(|| JitError::UnresolvedSymbol(symbol.to_string()))
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
