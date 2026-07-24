use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::Type;
use crate::ast::{
    self, AbiOverflow, AbiRegister, AbiRegisterSequence, AbiValueKind, BinOp, BuiltinFunction,
    Expr, Lit, RegisterDef, RegisterTrait, UnOp,
};
use crate::error::TMDLError;
use crate::utils::{
    resolve_effective_asm_for_instruction, resolve_effective_encoding_for_instruction,
    resolve_effective_schedule_for_instruction, resolve_operands_for_instruction,
    resolve_params_for_instruction,
};

pub fn generate_markdown(
    dialect: &str,
    files: &[ast::File],
    mut output: impl Write,
) -> Result<(), TMDLError> {
    write_overview(dialect, files, &mut output)?;
    let item_cache = item_cache(files);
    write_instructions(files, &item_cache, &mut output)
}

pub fn generate_markdown_book(
    dialect: &str,
    files: &[ast::File],
    output_dir: &Path,
) -> Result<(), TMDLError> {
    fs::create_dir_all(output_dir)?;
    let instruction_files = files
        .iter()
        .filter(|file| file.instructions().next().is_some())
        .collect::<Vec<_>>();

    let mut index = fs::File::create(output_dir.join("index.md"))?;
    write_overview(dialect, files, &mut index)?;
    if !instruction_files.is_empty() {
        writeln!(index, "\n## Instruction families")?;
        for file in &instruction_files {
            let stem = file_stem(file)?;
            writeln!(index, "\n- [{}](./{stem}.md)", humanize(stem))?;
        }
    }

    let item_cache = item_cache(files);
    for file in instruction_files {
        let stem = file_stem(file)?;
        let mut output = fs::File::create(output_dir.join(format!("{stem}.md")))?;
        writeln!(output, "# {}", humanize(stem))?;
        if let Some(doc) = &file.doc {
            writeln!(output, "\n{doc}")?;
        }
        write_instructions(std::slice::from_ref(file), &item_cache, &mut output)?;
    }

    Ok(())
}

fn write_overview(
    dialect: &str,
    files: &[ast::File],
    mut output: impl Write,
) -> Result<(), TMDLError> {
    writeln!(output, "# {} ISA Reference", display_dialect(dialect))?;

    if let Some(doc) = files.iter().find_map(|file| file.doc.as_deref()) {
        writeln!(output, "\n{doc}")?;
    }

    writeln!(output, "\n## Instruction sets")?;
    for isa in files.iter().flat_map(ast::File::isas) {
        writeln!(output, "\n### `{}`", isa.name)?;
        if let Some(doc) = &isa.doc {
            writeln!(output, "\n{doc}")?;
        }
        if !isa.parameters.is_empty() {
            writeln!(output, "\n| Parameter | Type | Default |")?;
            writeln!(output, "| --- | --- | --- |")?;
            let mut parameters: Vec<_> = isa.parameters.iter().collect();
            parameters.sort_by_key(|(name, _)| *name);
            for (name, (ty, value)) in parameters {
                writeln!(
                    output,
                    "| `{name}` | `{}` | `{}` |",
                    format_type(ty),
                    value.as_ref().map(format_expr).unwrap_or_default()
                )?;
            }
        }
    }

    let register_classes = files
        .iter()
        .flat_map(ast::File::register_classes)
        .collect::<Vec<_>>();
    if !register_classes.is_empty() {
        writeln!(output, "\n## Registers")?;
    }
    for register_class in register_classes {
        writeln!(output, "\n### `{}`", register_class.name)?;
        if let Some(doc) = &register_class.doc {
            writeln!(output, "\n{doc}")?;
        }
        write_availability(&mut output, &register_class.for_isas)?;
        if !register_class.registers.is_empty() {
            writeln!(output, "\n| Register | Encoding | Alias | Traits |")?;
            writeln!(output, "| --- | ---: | --- | --- |")?;
            let mut alias_indices = HashMap::<String, u16>::new();
            for register in &register_class.registers {
                match register {
                    RegisterDef::Single(register) => {
                        let encoding = register
                            .encoding_index()
                            .map(|index| index.to_string())
                            .unwrap_or_default();
                        let alias = register
                            .alias
                            .as_ref()
                            .map(|alias| format!("`{alias}`"))
                            .unwrap_or_default();
                        writeln!(
                            output,
                            "| `{}` | {encoding} | {alias} | {} |",
                            register.name,
                            format_register_traits(&register.traits)
                        )?;
                    }
                    RegisterDef::Range(range) => {
                        let encoding =
                            match (trailing_index(&range.start), trailing_index(&range.end)) {
                                (Some(start), Some(end)) => format!("{start}–{end}"),
                                _ => String::new(),
                            };
                        let alias = format_range_alias(
                            range.alias_pattern.as_deref(),
                            trailing_index(&range.start),
                            trailing_index(&range.end),
                            &mut alias_indices,
                        );
                        writeln!(
                            output,
                            "| `{}`–`{}` | {encoding} | {alias} | {} |",
                            range.start,
                            range.end,
                            format_register_traits(&range.traits)
                        )?;
                    }
                }
            }
        }
    }

    let abis = files.iter().flat_map(ast::File::abis).collect::<Vec<_>>();
    if !abis.is_empty() {
        writeln!(output, "\n## Calling conventions")?;
    }
    for abi in abis {
        write!(output, "\n### `{}`", abi.name)?;
        if let Some(alias) = &abi.alias {
            write!(output, " (`{alias}`)")?;
        }
        writeln!(output)?;
        if let Some(doc) = &abi.doc {
            writeln!(output, "\n{doc}")?;
        }
        write_availability(&mut output, &abi.for_isas)?;
        if let Some(stack) = &abi.stack {
            writeln!(
                output,
                "\n| Stack alignment | Growth | Red zone | Slot size |"
            )?;
            writeln!(output, "| ---: | --- | ---: | ---: |")?;
            writeln!(
                output,
                "| `{}` | {} | `{}` | `{}` |",
                stack.align.as_ref().map(format_expr).unwrap_or_default(),
                stack
                    .grows
                    .map(|growth| format!("{growth:?}").to_lowercase())
                    .unwrap_or_default(),
                stack.red_zone.as_ref().map(format_expr).unwrap_or_default(),
                stack
                    .slot_size
                    .as_ref()
                    .map(format_expr)
                    .unwrap_or_default()
            )?;
        }
        if !abi.roles.is_empty() {
            writeln!(output, "\n| Role | Register |")?;
            writeln!(output, "| --- | --- |")?;
            for role in &abi.roles {
                let role_name = match role.name.as_str() {
                    "sp" => "Stack pointer",
                    "ra" => "Return address",
                    "fp" => "Frame pointer",
                    other => other,
                };
                writeln!(
                    output,
                    "| {role_name} | `{}` |",
                    format_abi_register(&role.register)
                )?;
            }
        }
        if !abi.args.is_empty() || !abi.rets.is_empty() {
            writeln!(output, "\n| Values | Registers | Overflow |")?;
            writeln!(output, "| --- | --- | --- |")?;
            for (passes, suffix) in [(&abi.args, "arguments"), (&abi.rets, "results")] {
                for pass in passes {
                    let registers = pass
                        .registers
                        .iter()
                        .map(format_abi_register_sequence)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let overflow = match &pass.overflow {
                        Some(AbiOverflow::Stack) => "stack".to_string(),
                        Some(AbiOverflow::Kind(kind)) => {
                            format!("{} registers", format_abi_value_kind(*kind))
                        }
                        None => String::new(),
                    };
                    writeln!(
                        output,
                        "| {} {suffix} | {registers} | {overflow} |",
                        format_abi_value_kind(pass.kind)
                    )?;
                }
            }
        }
        if let Some(registers) = &abi.callee_saved {
            writeln!(
                output,
                "\n**Callee-saved:** {}",
                registers
                    .iter()
                    .map(format_abi_register_sequence)
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
        if let Some(registers) = &abi.reserved {
            writeln!(
                output,
                "\n**Reserved:** {}",
                registers
                    .iter()
                    .map(format_abi_register_sequence)
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
    }

    Ok(())
}

fn item_cache(files: &[ast::File]) -> HashMap<&str, &ast::Item> {
    files
        .iter()
        .flat_map(|file| file.items.iter().map(|item| (item.name(), item)))
        .collect()
}

fn write_instructions(
    files: &[ast::File],
    item_cache: &HashMap<&str, &ast::Item>,
    mut output: impl Write,
) -> Result<(), TMDLError> {
    let mut instructions = BTreeMap::<String, Vec<&ast::Instruction>>::new();
    for instruction in files.iter().flat_map(ast::File::instructions) {
        let parameters = resolve_params_for_instruction(instruction, item_cache);
        let mnemonic = string_parameter(&parameters, "MNEMONIC")
            .or_else(|| string_parameter(&parameters, "OPNAME"))
            .unwrap_or_else(|| instruction.name.clone());
        instructions.entry(mnemonic).or_default().push(instruction);
    }

    if !instructions.is_empty() {
        writeln!(output, "\n## Instructions")?;
    }
    for (mnemonic, forms) in instructions {
        writeln!(output, "\n### `{mnemonic}`")?;
        for instruction in forms {
            writeln!(output, "\n#### `{}`", instruction.name)?;
            if let Some(doc) = &instruction.doc {
                writeln!(output, "\n{doc}")?;
            }
            write_availability(&mut output, &instruction.for_isas)?;

            let parameters = resolve_params_for_instruction(instruction, item_cache);
            if let Some(template) = resolve_effective_asm_for_instruction(instruction, item_cache)
                .and_then(resolve_string)
            {
                writeln!(
                    output,
                    "\n**Syntax:** `{}`",
                    format_assembly(&template, &parameters)
                )?;
            }

            let operands = resolve_operands_for_instruction(instruction, item_cache);
            if !operands.is_empty() {
                writeln!(output, "\n| Operand | Type |")?;
                writeln!(output, "| --- | --- |")?;
                for (name, ty) in operands {
                    writeln!(output, "| `{name}` | `{}` |", format_type(&ty))?;
                }
            }

            if let Some(schedule) =
                resolve_effective_schedule_for_instruction(instruction, item_cache)
            {
                let classes = schedule
                    .classes
                    .iter()
                    .map(|class| format!("`{class}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(output, "\n**Scheduling class:** {classes}")?;
            }

            let mut encoding = resolve_effective_encoding_for_instruction(instruction, item_cache)
                .iter()
                .collect::<Vec<_>>();
            encoding.sort_by_key(|arm| std::cmp::Reverse(arm.end.unwrap_or(arm.start)));
            if !encoding.is_empty() {
                writeln!(output, "\n**Encoding**\n")?;
                writeln!(output, "| Bits | Value |")?;
                writeln!(output, "| --- | --- |")?;
                for arm in encoding {
                    let bits = match arm.end {
                        Some(end) if end != arm.start => format!("{end}–{}", arm.start),
                        _ => arm.start.to_string(),
                    };
                    writeln!(output, "| {bits} | `{}` |", format_expr(&arm.value))?;
                }
            }

            if is_unmodeled(&instruction.behavior) {
                writeln!(output, "\n**Behavior:** Semantics not modeled.")?;
            } else {
                writeln!(output, "\n**Behavior**\n")?;
                writeln!(output, "```tmdl")?;
                write!(output, "{}", format_behavior(&instruction.behavior))?;
                writeln!(output, "```")?;
            }
        }
    }

    Ok(())
}

fn file_stem(file: &ast::File) -> Result<&str, TMDLError> {
    Path::new(&file.file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| TMDLError::Codegen(format!("invalid input file name '{}'", file.file_name)))
}

fn humanize(name: &str) -> String {
    let mut words = name.replace('_', " ");
    if let Some(first) = words.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    words
}

fn display_dialect(dialect: &str) -> &str {
    match dialect {
        "riscv" => "RISC-V",
        "arm64" => "AArch64",
        "x86_64" => "x86-64",
        "ptx" => "PTX",
        other => other,
    }
}

fn write_availability(mut output: impl Write, isas: &[String]) -> Result<(), TMDLError> {
    if !isas.is_empty() {
        let availability = isas
            .iter()
            .map(|isa| format!("`{isa}`"))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(output, "\n**Available in:** {availability}")?;
    }
    Ok(())
}

fn trailing_index(name: &str) -> Option<u16> {
    let digits = name.trim_start_matches(|character: char| !character.is_ascii_digit());
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn format_register_traits(traits: &[RegisterTrait]) -> String {
    traits
        .iter()
        .map(|trait_| match trait_ {
            RegisterTrait::HardwiredZero => "hardwired zero",
            RegisterTrait::ProgramCounter => "program counter",
            RegisterTrait::StatusFlag => "status flag",
            RegisterTrait::Float => "floating point",
            RegisterTrait::Polymorphic => "polymorphic",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_range_alias(
    pattern: Option<&str>,
    start: Option<u16>,
    end: Option<u16>,
    next_indices: &mut HashMap<String, u16>,
) -> String {
    let Some(pattern) = pattern else {
        return String::new();
    };
    if !pattern.contains("{}") {
        return format!("`{pattern}`");
    }
    let stem = pattern.replace("{}", "");
    let first = *next_indices.entry(stem.clone()).or_default();
    let count = match (start, end) {
        (Some(start), Some(end)) => end - start + 1,
        _ => 1,
    };
    let last = first + count - 1;
    next_indices.insert(stem.clone(), last + 1);
    format!("`{stem}{first}`–`{stem}{last}`")
}

fn format_abi_register(register: &AbiRegister) -> String {
    format!("{}::{}", register.class, register.name)
}

fn format_abi_register_sequence(sequence: &AbiRegisterSequence) -> String {
    let start = format_abi_register(&sequence.start);
    match &sequence.end {
        Some(end) => format!("`{start}`–`{}`", format_abi_register(end)),
        None => format!("`{start}`"),
    }
}

fn format_abi_value_kind(kind: AbiValueKind) -> &'static str {
    match kind {
        AbiValueKind::Int => "Integer",
        AbiValueKind::Float => "Floating-point",
        AbiValueKind::Vector => "Vector",
    }
}

fn string_parameter(
    parameters: &HashMap<String, (Type, Option<Expr>)>,
    name: &str,
) -> Option<String> {
    parameters
        .get(name)
        .and_then(|(_, value)| value.as_ref())
        .and_then(resolve_string)
}

fn resolve_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(Lit::Str(value)) => Some(value.value().to_string()),
        Expr::Block(block) if block.last_expr_return => block.stmts.last().and_then(resolve_string),
        _ => None,
    }
}

fn format_assembly(template: &str, parameters: &HashMap<String, (Type, Option<Expr>)>) -> String {
    let mut output = String::new();
    let mut remainder = template;
    while let Some(open) = remainder.find('{') {
        output.push_str(&remainder[..open]);
        let Some(close) = remainder[open + 1..].find('}') else {
            output.push_str(&remainder[open..]);
            return output;
        };
        let close = open + 1 + close;
        let placeholder = &remainder[open + 1..close];
        if let Some(parameter) = placeholder.strip_prefix("self.") {
            if let Some(value) = parameters
                .get(parameter)
                .and_then(|(_, value)| value.as_ref())
            {
                output.push_str(&resolve_string(value).unwrap_or_else(|| format_expr(value)));
            }
        } else {
            output.push_str(placeholder);
        }
        remainder = &remainder[close + 1..];
    }
    output.push_str(remainder);
    output
}

fn format_type(ty: &Type) -> String {
    match ty {
        Type::String => "String".to_string(),
        Type::Integer => "Integer".to_string(),
        Type::Bits(width) => format!("bits<{width}>"),
        Type::BitsExpr(width) => format!("bits<{}>", format_expr(width)),
        Type::Struct(name) => name.clone(),
        Type::Var(var) => format!("{var:?}"),
        Type::Fn(argument, result) => {
            format!("{} -> {}", format_type(argument), format_type(result))
        }
        Type::Con(name, arguments) => {
            let arguments = arguments
                .iter()
                .map(format_type)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name}<{arguments}>")
        }
    }
}

fn format_expr(expr: &Expr) -> String {
    format_expr_with_precedence(expr, 0)
}

fn format_expr_with_precedence(expr: &Expr, parent_precedence: u8) -> String {
    match expr {
        Expr::Lit(Lit::Int(value)) => value.value().to_string(),
        Expr::Lit(Lit::Str(value)) => format!("\"{}\"", value.value()),
        Expr::Ident(value) => value.name.clone(),
        Expr::Path(path) => std::iter::once(path.base.as_str())
            .chain(path.remainder.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("::"),
        Expr::Field(field) => format!("{}.{}", format_expr(&field.base), field.member),
        Expr::Slice(slice) => format!(
            "{}[{}..{}]",
            format_expr(&slice.base),
            slice.start,
            slice.end
        ),
        Expr::IndexAccess(index) => format!("{}[{}]", format_expr(&index.base), index.index),
        Expr::Assign(assign) => {
            format!(
                "{} = {}",
                format_expr(&assign.dest),
                format_expr(&assign.value)
            )
        }
        Expr::Binary(binary) => {
            let (operator, precedence) = format_binary_operator(&binary.op);
            let expression = format!(
                "{} {operator} {}",
                format_expr_with_precedence(&binary.lhs, precedence),
                format_expr_with_precedence(&binary.rhs, precedence + 1)
            );
            if precedence < parent_precedence {
                format!("({expression})")
            } else {
                expression
            }
        }
        Expr::Unary(unary) => match unary.op {
            UnOp::BitwiseNot => format!("~{}", format_expr_with_precedence(&unary.x, 100)),
        },
        Expr::Call(call) => format!(
            "{}({})",
            format_expr(&call.callee),
            call.arguments
                .iter()
                .map(format_expr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::BuiltinFunction(builtin) => format_builtin(builtin).to_string(),
        Expr::Lambda(lambda) => format!(
            "|{}| {}",
            lambda.params.join(", "),
            format_expr(&lambda.body)
        ),
        Expr::Block(block) => {
            let body = block
                .stmts
                .iter()
                .map(format_expr)
                .collect::<Vec<_>>()
                .join("; ");
            format!("{{ {body} }}")
        }
        Expr::If(if_expr) => {
            let mut output = format!(
                "if {} {}",
                format_expr(&if_expr.cond),
                format_expr(&if_expr.then)
            );
            if let Some(else_expr) = &if_expr.else_ {
                output.push_str(&format!(" else {}", format_expr(else_expr)));
            }
            output
        }
        Expr::Try(try_expr) => {
            let mut output = format!("try {}", format_expr(&try_expr.body));
            for handler in &try_expr.handlers {
                output.push_str(&format!(" except {}", handler.kind));
                if let Some(binding) = &handler.binding {
                    output.push_str(&format!("({binding})"));
                }
                output.push(' ');
                output.push_str(&format_expr(&handler.body));
            }
            output
        }
        Expr::Invalid => "<invalid>".to_string(),
    }
}

fn format_behavior(expr: &Expr) -> String {
    let mut output = String::new();
    format_statement(expr, 0, &mut output);
    output
}

fn is_unmodeled(expr: &Expr) -> bool {
    match expr {
        Expr::Block(block) if block.stmts.len() == 1 => is_unmodeled(&block.stmts[0]),
        Expr::Call(call) => matches!(
            call.callee.as_ref(),
            Expr::BuiltinFunction(BuiltinFunction::Todo)
        ),
        _ => false,
    }
}

fn format_statement(expr: &Expr, indent: usize, output: &mut String) {
    match expr {
        Expr::Block(block) => {
            for statement in &block.stmts {
                format_statement(statement, indent, output);
            }
        }
        Expr::If(if_expr) => {
            output.push_str(&"    ".repeat(indent));
            output.push_str(&format!("if {} {{\n", format_expr(&if_expr.cond)));
            format_statement(&if_expr.then, indent + 1, output);
            output.push_str(&"    ".repeat(indent));
            output.push('}');
            if let Some(else_expr) = &if_expr.else_ {
                output.push_str(" else {\n");
                format_statement(else_expr, indent + 1, output);
                output.push_str(&"    ".repeat(indent));
                output.push('}');
            }
            output.push('\n');
        }
        _ => {
            output.push_str(&"    ".repeat(indent));
            output.push_str(&format_expr(expr));
            output.push_str(";\n");
        }
    }
}

fn format_binary_operator(operator: &BinOp) -> (&'static str, u8) {
    match operator {
        BinOp::BitwiseOr => ("|", 1),
        BinOp::BitwiseXor => ("^", 2),
        BinOp::BitwiseAnd => ("&", 3),
        BinOp::Equal => ("==", 4),
        BinOp::NotEqual => ("!=", 4),
        BinOp::LessThan => ("<", 5),
        BinOp::GreaterThan => (">", 5),
        BinOp::LessThenEqual => ("<=", 5),
        BinOp::GreaterThanEqual => (">=", 5),
        BinOp::UnsignedLessThan => ("<u", 5),
        BinOp::UnsignedGreaterThan => (">u", 5),
        BinOp::UnsignedLessThenEqual => ("<=u", 5),
        BinOp::UnsignedGreaterThanEqual => (">=u", 5),
        BinOp::ShiftLeftLogical => ("<<", 6),
        BinOp::ShiftRightLogical => (">>", 6),
        BinOp::ShiftRightArithmetic => (">>>", 6),
        BinOp::Add => ("+", 7),
        BinOp::Sub => ("-", 7),
        BinOp::Mul => ("*", 8),
        BinOp::Div => ("/", 8),
        BinOp::UnsignedDiv => ("/u", 8),
    }
}

fn format_builtin(builtin: &BuiltinFunction) -> &'static str {
    match builtin {
        BuiltinFunction::Clamp => "clamp",
        BuiltinFunction::Extract => "extract",
        BuiltinFunction::Bitcast => "bitcast",
        BuiltinFunction::Log2Ceil => "log2Ceil",
        BuiltinFunction::Regnum => "regnum",
        BuiltinFunction::SExt => "sext",
        BuiltinFunction::ZExt => "zext",
        BuiltinFunction::Load => "load",
        BuiltinFunction::Store => "store",
        BuiltinFunction::LoadReserved => "load_reserved",
        BuiltinFunction::StoreConditional => "store_conditional",
        BuiltinFunction::AtomicRmw => "atomic_rmw",
        BuiltinFunction::Fence => "fence",
        BuiltinFunction::FenceI => "fence_i",
        BuiltinFunction::Trap => "trap",
        BuiltinFunction::Split => "split",
        BuiltinFunction::Concat => "concat",
        BuiltinFunction::Map => "map",
        BuiltinFunction::Reduce => "reduce",
        BuiltinFunction::Zip => "zip",
        BuiltinFunction::FAdd => "fadd",
        BuiltinFunction::FSub => "fsub",
        BuiltinFunction::FMul => "fmul",
        BuiltinFunction::FDiv => "fdiv",
        BuiltinFunction::Todo => "todo",
    }
}
