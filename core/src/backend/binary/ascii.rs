//! Text rendering of object bytes for lit tests: instead of a binary stream,
//! each instruction is printed as a bracketed byte list, so FileCheck can
//! match encodings without dealing with unescaped binary.

use std::fmt::Write;

use super::ObjectFile;

/// Render `obj` as FileCheck-friendly text:
///
/// ```text
/// .section .text
/// add:
///   [0x33, 0x85, 0xC5, 0x00]
/// ```
pub fn render_ascii(obj: &ObjectFile) -> String {
    let mut out = String::new();
    for (idx, section) in obj.sections.iter().enumerate() {
        let _ = writeln!(out, ".section {}", section.name);

        let mut symbols: Vec<_> = obj
            .symbols
            .iter()
            .filter(|s| s.section == Some(idx))
            .collect();
        symbols.sort_by_key(|s| s.value);
        let mut next_symbol = symbols.into_iter().peekable();

        for (offset, len) in &section.insn_spans {
            while next_symbol.peek().is_some_and(|s| s.value <= *offset) {
                let _ = writeln!(out, "{}:", next_symbol.next().expect("peeked").name);
            }
            let bytes = &section.data[*offset as usize..(*offset + *len as u64) as usize];
            let rendered: Vec<String> = bytes.iter().map(|b| format!("0x{b:02X}")).collect();
            let _ = writeln!(out, "  [{}]", rendered.join(", "));
        }
    }
    out
}
