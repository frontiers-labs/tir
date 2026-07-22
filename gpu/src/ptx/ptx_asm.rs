//! PTX assembly front-end: parse real `.ptx` text into a TIR module and print a
//! module back as PTX. The shared flat assembler cannot represent PTX structure
//! (typed `.reg` declarations, `.visible .entry` kernels with `.param` lists,
//! predication `@%p`, labels), so this module handles that structure directly.
//!
//! Per-instruction operand syntax is driven entirely by the generated
//! [`asm_syntax`](crate::asm_syntax) table, so adding an instruction to the TMDL
//! definitions is enough to parse and print it — no code here changes.
//!
//! Kernel scaffolding (module directives, the `.entry` header/parameter list,
//! `.reg` declarations) is preserved verbatim as string attributes; the
//! instruction bodies are modeled as PTX dialect ops. Labels become `ptx.label`
//! ops; predication is carried as `pred`/`pred_not` attributes.

use std::collections::HashMap;

use tir::attributes::{AttributeValue, NamedAttribute, RegisterAttr};
use tir::backend::asm_syntax::{AsmSyntaxPart, InstrSyntax};
use tir::backend::{SectionOp, SymbolEndOp, SymbolOp};
use tir::builtin::{ModuleEndOpBuilder, ModuleOp, ModuleOpBuilder};
use tir::{Context, IRBuilder, OpId, OpInstance, Operation};

use super::{LabelOp, LabelOpBuilder};

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// Identifier, directive (`.reg`), dotted mnemonic (`ld.param.u64`) or
    /// register (`rd0`); anything starting with a letter, `_`, `$` or `.`.
    Ident(String),
    /// A numeric literal, kept as raw text to round-trip its formatting.
    Num(String),
    Percent,
    Comma,
    LBrack,
    RBrack,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Lt,
    Gt,
    Plus,
    Minus,
    Semi,
    Colon,
    At,
    Bang,
}

/// Remove `//` line comments and `/* */` block comments, preserving newlines.
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$' || c == '.'
}
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.'
}

/// Tokenize a fragment of PTX (a statement, or a literal template part).
fn lex(src: &str) -> Vec<Tok> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // A numeric literal starts with a digit. A leading `+`/`-` is a separate
        // token (an offset separator or an immediate sign captured by the matcher),
        // so `[%rd+16]` tokenizes as `rd`, `+`, `16` rather than `rd`, `+16`.
        if c.is_ascii_digit() {
            let mut s = String::new();
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == 'x')
            {
                s.push(chars[i]);
                i += 1;
            }
            toks.push(Tok::Num(s));
            continue;
        }
        if is_ident_start(c) {
            let mut s = String::new();
            while i < chars.len() && is_ident_char(chars[i]) {
                s.push(chars[i]);
                i += 1;
            }
            toks.push(Tok::Ident(s));
            continue;
        }
        let tok = match c {
            '%' => Tok::Percent,
            ',' => Tok::Comma,
            '[' => Tok::LBrack,
            ']' => Tok::RBrack,
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '{' => Tok::LBrace,
            '}' => Tok::RBrace,
            '<' => Tok::Lt,
            '>' => Tok::Gt,
            '+' => Tok::Plus,
            '-' => Tok::Minus,
            ';' => Tok::Semi,
            ':' => Tok::Colon,
            '@' => Tok::At,
            '!' => Tok::Bang,
            _ => {
                i += 1;
                continue;
            }
        };
        toks.push(tok);
        i += 1;
    }
    toks
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Kernel {
    name: String,
    header: String,
    regs: String,
    body: String,
}

/// Split a cleaned module into its leading directives (preamble) and kernels.
fn split_kernels(src: &str) -> (String, Vec<Kernel>) {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut kernels = Vec::new();
    let mut preamble_end = n;
    let mut search = 0;

    // Locate each top-level `{ ... }` (a kernel body) and take the text before it
    // (back to the previous body or the preamble split) as the header.
    let mut prev_close = 0usize;
    while let Some(open) = find_top_level_brace(&chars, search) {
        let close = match match_brace(&chars, open) {
            Some(c) => c,
            None => break,
        };
        let header_region: String = chars[prev_close..open].iter().collect();
        // The header proper starts at its first linkage/entry directive.
        let hstart = header_start(&header_region);
        let header = header_region[hstart..].trim().to_string();
        if prev_close == 0 {
            preamble_end = hstart;
        }
        let name = kernel_name(&header);
        let body: String = chars[open + 1..close].iter().collect();
        let (regs, body) = split_regs(&body);
        kernels.push(Kernel {
            name,
            header,
            regs,
            body,
        });
        prev_close = close + 1;
        search = close + 1;
    }

    let preamble: String = chars[..preamble_end.min(n)].iter().collect();
    (preamble.trim_end().to_string(), kernels)
}

fn find_top_level_brace(chars: &[char], from: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, &c) in chars.iter().enumerate().skip(from) {
        match c {
            '{' if depth == 0 => return Some(i),
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn match_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Index of the kernel header within `region` — the last linkage/entry directive
/// run before the body. Anything before it is module-level (preamble).
fn header_start(region: &str) -> usize {
    for kw in [".visible", ".weak", ".extern", ".entry", ".func"] {
        if let Some(pos) = region.find(kw) {
            // Back up over an immediately-preceding linkage directive.
            return pos;
        }
    }
    0
}

/// Extract the kernel name from its header (`.entry NAME (...)` or
/// `.func (retparam) NAME (...)`).
fn kernel_name(header: &str) -> String {
    let toks = lex(header);
    let mut i = 0;
    while i < toks.len() {
        if matches!(&toks[i], Tok::Ident(s) if s == ".entry" || s == ".func") {
            i += 1;
            // Skip a `.func` return-parameter group `( ... )`.
            if matches!(toks.get(i), Some(Tok::LParen)) {
                let mut depth = 0;
                while i < toks.len() {
                    match toks[i] {
                        Tok::LParen => depth += 1,
                        Tok::RParen => {
                            depth -= 1;
                            i += 1;
                            if depth == 0 {
                                break;
                            }
                            continue;
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            if let Some(Tok::Ident(name)) = toks.get(i) {
                return name.clone();
            }
        }
        i += 1;
    }
    "unknown".to_string()
}

/// Separate leading `.`-directive declarations (`.reg`, `.local`, `.shared`, ...)
/// from the instruction statements of a kernel body.
fn split_regs(body: &str) -> (String, String) {
    let mut regs = String::new();
    let mut rest = String::new();
    for stmt in split_statements(body) {
        match stmt {
            Statement::Directive(text) => {
                regs.push('\t');
                regs.push_str(&text);
                regs.push('\n');
            }
            Statement::Label(name) => {
                rest.push_str(&format!("\0L{name}\n"));
            }
            Statement::Instruction(text) => {
                rest.push_str(&text);
                rest.push('\n');
            }
        }
    }
    (regs.trim_end().to_string(), rest)
}

enum Statement {
    Directive(String),
    Label(String),
    Instruction(String),
}

/// Split a body into statements: `;`-terminated directives/instructions and
/// `ident:` labels.
fn split_statements(body: &str) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in body.chars() {
        match c {
            ';' => {
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    if s.starts_with('.') {
                        out.push(Statement::Directive(format!("{s};")));
                    } else {
                        out.push(Statement::Instruction(format!("{s};")));
                    }
                }
                cur.clear();
            }
            ':' => {
                let s = cur.trim().to_string();
                if !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
                {
                    out.push(Statement::Label(s));
                    cur.clear();
                } else {
                    cur.push(c);
                }
            }
            _ => cur.push(c),
        }
    }
    out
}

/// A statement queued for op construction, produced by `split_regs`.
enum BodyItem {
    Label(String),
    Instruction(String),
}

fn body_items(rest: &str) -> Vec<BodyItem> {
    rest.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            if let Some(name) = l.strip_prefix("\0L") {
                BodyItem::Label(name.to_string())
            } else {
                BodyItem::Instruction(l.to_string())
            }
        })
        .collect()
}

/// Split a register token into its bank letters and index (`rd10` -> `("rd", 10)`).
/// The class is derived from the bank so one operand-shape covers every type:
/// `%rd`→RD, `%r`→R, `%f`→F, `%fd`→FD, `%p`→P, `%rs`→RS. Returns `None` for a
/// non-register token (a symbol/special register), so the caller can fall through
/// to another form.
fn split_reg(reg: &str) -> Option<(String, u16)> {
    let split = reg.find(|c: char| c.is_ascii_digit())?;
    let (bank, num) = reg.split_at(split);
    if bank.is_empty() || !bank.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    if !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((bank.to_string(), num.parse().ok()?))
}

/// Canonical text of a single token (for reconstructing a captured group).
fn tok_text(t: &Tok) -> &'static str {
    match t {
        Tok::Percent => "%",
        Tok::Comma => ", ",
        Tok::LBrack => "[",
        Tok::RBrack => "]",
        Tok::LBrace => "{",
        Tok::RBrace => "}",
        Tok::LParen => "(",
        Tok::RParen => ")",
        Tok::Plus => "+",
        Tok::Minus => "-",
        Tok::Lt => "<",
        Tok::Gt => ">",
        _ => "",
    }
}

/// Capture a balanced `{...}` or `[...]` group starting at `start`, reconstructing
/// its text (register vectors and complex addresses used by tensor/texture ops are
/// preserved verbatim rather than modeled per element). Returns the rendered text
/// and the position after the group.
fn capture_group(toks: &[Tok], start: usize) -> Option<(String, usize)> {
    let (open, close) = match toks.get(start)? {
        Tok::LBrace => (Tok::LBrace, Tok::RBrace),
        Tok::LBrack => (Tok::LBrack, Tok::RBrack),
        _ => return None,
    };
    let mut depth = 0i32;
    let mut out = String::new();
    for (i, t) in toks.iter().enumerate().skip(start) {
        if *t == open {
            depth += 1;
        } else if *t == close {
            depth -= 1;
        }
        match t {
            Tok::Ident(s) | Tok::Num(s) => out.push_str(s),
            other => out.push_str(tok_text(other)),
        }
        if depth == 0 {
            return Some((out, i + 1));
        }
    }
    None
}

/// Try to match one instruction's tokens against a syntax entry, capturing
/// operand attributes. Returns `None` if the form does not match.
fn match_syntax(syntax: &InstrSyntax, toks: &[Tok]) -> Option<Vec<NamedAttribute>> {
    let mut pos = 0;
    let mut attrs = Vec::new();
    for part in syntax.parts {
        match part {
            AsmSyntaxPart::Text(text) => {
                for expected in lex(text) {
                    if toks.get(pos) != Some(&expected) {
                        return None;
                    }
                    pos += 1;
                }
            }
            AsmSyntaxPart::Operand { name, class } => match class {
                // A register operand: the actual class is derived from the input
                // register's bank, so one operand-shape template serves all types.
                Some(_) => {
                    let Some(Tok::Ident(reg)) = toks.get(pos) else {
                        return None;
                    };
                    let (bank, index) = split_reg(reg)?;
                    let class = super::register_info().class(&bank.to_uppercase())?;
                    attrs.push(NamedAttribute::new(
                        *name,
                        AttributeValue::Register(RegisterAttr::Physical { class, index }),
                    ));
                    pos += 1;
                }
                None => {
                    // An immediate/symbol/special-register/vector slot. A `{...}`
                    // register vector (tensor/texture) or `[...]` address group is
                    // captured verbatim; otherwise an optional `%` sigil or sign then
                    // a name or number.
                    let text = if matches!(toks.get(pos), Some(Tok::LBrace | Tok::LBrack)) {
                        let (rendered, next) = capture_group(toks, pos)?;
                        pos = next;
                        rendered
                    } else {
                        let mut text = String::new();
                        match toks.get(pos) {
                            Some(Tok::Percent) => {
                                text.push('%');
                                pos += 1;
                            }
                            Some(Tok::Minus) => {
                                text.push('-');
                                pos += 1;
                            }
                            _ => {}
                        }
                        match toks.get(pos) {
                            Some(Tok::Ident(s)) => text.push_str(s),
                            Some(Tok::Num(s)) => text.push_str(s),
                            _ => return None,
                        }
                        pos += 1;
                        text
                    };
                    attrs.push(NamedAttribute::new(*name, AttributeValue::Str(text)));
                }
            },
        }
    }
    (pos == toks.len()).then_some(attrs)
}

/// Index of syntax entries by their leading mnemonic token.
fn syntax_index() -> HashMap<String, Vec<&'static InstrSyntax>> {
    let mut map: HashMap<String, Vec<&'static InstrSyntax>> = HashMap::new();
    for entry in crate::ptx::asm_syntax() {
        if let Some(Tok::Ident(first)) = lex_first(entry) {
            map.entry(first).or_default().push(entry);
        }
    }
    map
}

fn lex_first(entry: &InstrSyntax) -> Option<Tok> {
    match entry.parts.first() {
        Some(AsmSyntaxPart::Text(t)) => lex(t).into_iter().next(),
        _ => None,
    }
}

/// Parse a full instruction statement (already stripped of any trailing newline),
/// returning `(op_name, attributes)`.
fn parse_instruction(
    index: &HashMap<String, Vec<&'static InstrSyntax>>,
    line: &str,
) -> Result<(&'static str, Vec<NamedAttribute>), String> {
    let toks = lex(line);
    let mut cursor = 0;
    let mut pred: Option<(String, bool)> = None;

    // Optional predicate guard: `@%p1` or `@!%p1`.
    if toks.first() == Some(&Tok::At) {
        cursor = 1;
        let neg = toks.get(cursor) == Some(&Tok::Bang);
        if neg {
            cursor += 1;
        }
        if toks.get(cursor) != Some(&Tok::Percent) {
            return Err(format!("malformed predicate in `{line}`"));
        }
        cursor += 1;
        let Some(Tok::Ident(p)) = toks.get(cursor) else {
            return Err(format!("malformed predicate in `{line}`"));
        };
        pred = Some((format!("%{p}"), neg));
        cursor += 1;
    }

    let rest = &toks[cursor..];
    let Some(Tok::Ident(mnemonic)) = rest.first() else {
        return Err(format!("no mnemonic in `{line}`"));
    };
    let candidates = index
        .get(mnemonic)
        .ok_or_else(|| format!("unsupported PTX instruction `{line}`"))?;
    for syntax in candidates {
        if let Some(mut attrs) = match_syntax(syntax, rest) {
            if let Some((p, neg)) = &pred {
                attrs.push(NamedAttribute::new("pred", AttributeValue::Str(p.clone())));
                attrs.push(NamedAttribute::new("pred_not", AttributeValue::Bool(*neg)));
            }
            return Ok((syntax.op_name, attrs));
        }
    }
    Err(format!("no matching form for `{line}`"))
}

fn build_op(context: &Context, name: &'static str, attributes: Vec<NamedAttribute>) -> OpId {
    let inst = OpInstance::new_dynamic(
        ("ptx", name),
        context.as_context_ref(),
        vec![],
        vec![],
        vec![],
        attributes,
        &[],
    );
    context.add_operation(inst).id
}

/// Parse PTX assembly text into a module.
pub fn parse(context: &Context, text: &str) -> Result<ModuleOp, String> {
    let cleaned = strip_comments(text);
    let (preamble, kernels) = split_kernels(&cleaned);
    let index = syntax_index();

    let module = ModuleOpBuilder::new(context).build();
    let mut mb = IRBuilder::new(module.body());
    let section = mb.insert(
        tir::backend::SectionOpBuilder::new(context)
            .attr("ptx_preamble", AttributeValue::Str(preamble))
            .build(),
    );
    mb.insert(ModuleEndOpBuilder::new(context).build());

    let section_body = section.body();
    for kernel in kernels {
        let symbol = tir::backend::SymbolOpBuilder::new(context)
            .attr("name", AttributeValue::Str(kernel.name))
            .attr("ptx_header", AttributeValue::Str(kernel.header))
            .attr("ptx_regs", AttributeValue::Str(kernel.regs))
            .build();
        section_body.insert(section_body.len(), symbol.id());
        let body = symbol.body();
        for item in body_items(&kernel.body) {
            let id = match item {
                BodyItem::Label(name) => LabelOpBuilder::new(context)
                    .attr("name", AttributeValue::Str(name))
                    .build()
                    .id(),
                BodyItem::Instruction(line) => {
                    let (op_name, attrs) = parse_instruction(&index, line.trim())?;
                    build_op(context, op_name, attrs)
                }
            };
            body.insert(body.len(), id);
        }
        body.insert(
            body.len(),
            tir::backend::SymbolEndOpBuilder::new(context).build().id(),
        );
    }

    Ok(module)
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

fn attr_str(op: &OpInstance, name: &str) -> Option<String> {
    op.attributes.iter().find(|a| a.name == name).and_then(|a| {
        if let AttributeValue::Str(s) = &a.value {
            Some(s.clone())
        } else {
            None
        }
    })
}

fn attr_bool(op: &OpInstance, name: &str) -> bool {
    matches!(
        op.attributes
            .iter()
            .find(|a| a.name == name)
            .map(|a| &a.value),
        Some(AttributeValue::Bool(true))
    )
}

fn render_operand(op: &OpInstance, name: &str) -> Result<String, String> {
    let value = op
        .attributes
        .iter()
        .find(|a| a.name == name)
        .map(|a| &a.value)
        .ok_or_else(|| format!("op `{}` missing operand `{name}`", op.name().as_str()))?;
    Ok(match value {
        AttributeValue::Register(RegisterAttr::Physical { class, index }) => {
            format!("{}{}", class.name().to_lowercase(), index)
        }
        AttributeValue::Register(RegisterAttr::Virtual { id, .. }) => format!("%virt{id}"),
        AttributeValue::Str(s) => s.clone(),
        AttributeValue::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    })
}

fn print_instruction(
    op: &OpInstance,
    syntax: &InstrSyntax,
    out: &mut String,
) -> Result<(), String> {
    out.push('\t');
    if let Some(pred) = attr_str(op, "pred") {
        out.push('@');
        if attr_bool(op, "pred_not") {
            out.push('!');
        }
        out.push_str(&pred);
        out.push(' ');
    }
    for part in syntax.parts {
        match part {
            AsmSyntaxPart::Text(text) => out.push_str(text),
            AsmSyntaxPart::Operand { name, .. } => out.push_str(&render_operand(op, name)?),
        }
    }
    out.push('\n');
    Ok(())
}

/// Print a module as PTX assembly text.
pub fn print(context: &Context, module: &ModuleOp) -> Result<String, String> {
    let by_op: HashMap<&'static str, &'static InstrSyntax> = crate::ptx::asm_syntax()
        .iter()
        .map(|s| (s.op_name, s))
        .collect();

    let mut out = String::new();
    for op_id in module.body().op_ids() {
        let op = context.get_op(op_id);
        if !op.is::<SectionOp>() {
            continue;
        }
        if let Some(preamble) = attr_str(&op, "ptx_preamble")
            && !preamble.is_empty()
        {
            out.push_str(&preamble);
            out.push('\n');
        }
        for sym_id in region_ops(context, &op) {
            let sym = context.get_op(sym_id);
            if !sym.is::<SymbolOp>() {
                continue;
            }
            out.push('\n');
            if let Some(header) = attr_str(&sym, "ptx_header") {
                out.push_str(&header);
                out.push('\n');
            }
            out.push_str("{\n");
            if let Some(regs) = attr_str(&sym, "ptx_regs")
                && !regs.is_empty()
            {
                out.push_str(&regs);
                out.push_str("\n\n");
            }
            for body_id in region_ops(context, &sym) {
                let body_op = context.get_op(body_id);
                if body_op.is::<SymbolEndOp>() {
                    continue;
                }
                if body_op.is::<LabelOp>() {
                    let name = attr_str(&body_op, "name").unwrap_or_default();
                    out.push('\n');
                    out.push_str(&name);
                    out.push_str(":\n");
                } else {
                    let name = body_op.name().as_str();
                    let syntax = by_op
                        .get(name)
                        .ok_or_else(|| format!("no syntax for op `{name}`"))?;
                    print_instruction(&body_op, syntax, &mut out)?;
                }
            }
            out.push_str("}\n");
        }
    }
    Ok(out)
}

/// Op ids of the first block of an op's single region.
fn region_ops(context: &Context, op: &OpInstance) -> Vec<OpId> {
    let Some(region) = op.regions.first() else {
        return Vec::new();
    };
    context
        .get_region(*region)
        .iter(context.clone())
        .next()
        .map(|block| block.op_ids())
        .unwrap_or_default()
}
