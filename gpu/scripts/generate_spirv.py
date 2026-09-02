#!/usr/bin/env python3
import json
import pathlib
import re
import sys


CLASSES = {
    "Atomic",
    "Arithmetic",
    "Bit",
    "Composite",
    "Conversion",
    "Memory",
    "Relational_and_Logical",
}


def core_value_operations(grammar):
    for instruction in grammar["instructions"]:
        operands = instruction.get("operands", [])
        if instruction.get("class") not in CLASSES:
            continue
        if not instruction.get("version") or instruction["opcode"] >= 1000:
            continue
        if len(operands) < 3 or operands[0]["kind"] != "IdResultType":
            continue
        if operands[1]["kind"] != "IdResult":
            continue
        if not all(
            operand["kind"] in {"IdRef", "IdScope", "IdMemorySemantics"}
            and "quantifier" not in operand
            for operand in operands[2:]
        ):
            continue
        yield instruction


def field_name(name, index):
    value = re.sub(r"[^a-zA-Z0-9]+", "_", name or f"operand_{index}").strip("_").lower()
    if value in {"type", "self", "match", "ref", "loop", "move"}:
        value += "_value"
    return value


def generate(grammar_path, output_dir):
    grammar = json.loads(grammar_path.read_text())
    operations = list(core_value_operations(grammar))
    rust = [
        "#![allow(clippy::too_many_arguments)]",
        "",
        "use tir::helpers::operation;",
        "use tir::{Any as TirAny, OpId, TypeId, ValueId};",
        "",
        "use tir as tir;",
        "",
    ]
    op_names = []
    for instruction in operations:
        op_name = instruction["opname"][2:]
        type_name = f"{op_name}Op"
        op_names.append(type_name)
        fields = [
            field_name(operand.get("name"), index)
            for index, operand in enumerate(instruction["operands"][2:])
        ]
        rust.extend([
            "operation! {",
            f"    {type_name} {{",
            f'        name: "{op_name}",',
            '        dialect: "spirv",',
            "        operands: O { " + " ".join(f'{field}: "TirAny",' for field in fields) + " },",
            '        results: R { result: "TirAny", }',
            "    }",
            "}",
            "",
        ])
    rust.extend([
        "/// Every generated op: its SPIR-V opcode, its name in the `spirv` dialect, and",
        "/// how many id operands follow the result type and result id.",
        "static GENERATED: &[(u16, &str, usize)] = &[",
    ])
    for instruction in operations:
        arity = len(instruction["operands"]) - 2
        rust.append(f'    ({instruction["opcode"]}, "{instruction["opname"][2:]}", {arity}),')
    rust.extend([
        "];",
        "",
        "pub(crate) fn opcode_for_name(name: &str) -> Option<u16> {",
        "    GENERATED",
        "        .iter()",
        "        .find(|(_, candidate, _)| *candidate == name)",
        "        .map(|&(opcode, _, _)| opcode)",
        "}",
        "",
        "pub(crate) fn build_generated(",
        "    context: &tir::Context,",
        "    opcode: u16,",
        "    operands: &[ValueId],",
        "    result_type: TypeId,",
        ") -> Option<(OpId, ValueId)> {",
        "    let &(_, name, arity) = GENERATED.iter().find(|&&(code, _, _)| code == opcode)?;",
        "    if operands.len() != arity {",
        "        return None;",
        "    }",
        "    let result = context.create_value(result_type, None).id();",
        "    let instance = tir::OpInstance::new_dynamic(",
        '        ("spirv", name),',
        "        context.as_context_ref(),",
        "        operands.to_vec(),",
        "        vec![result],",
        "        vec![],",
        "        vec![],",
        "    );",
        "    let op = context.add_operation(instance);",
        "    Some((op.id, result))",
        "}",
        "",
    ])
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "generated.rs").write_text("\n".join(rust))
    manual_ops = [
        "ModuleOp", "ModuleEndOp", "CapabilityOp", "GlobalVariableOp",
        "EntryPointOp", "ExecutionModeOp", "ConstantOp", "LoadOp", "StoreOp",
        "ControlBarrierOp", "MemoryBarrierOp",
        "CompositeExtractOp", "AccessChainOp", "ReturnOp",
    ]
    operation_list = "[\n" + "\n".join(
        f"    {name}," for name in manual_ops + op_names
    ) + "\n]\n"
    (output_dir / "generated_ops.rs").write_text(operation_list)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: generate_spirv.py GRAMMAR_JSON OUTPUT_DIR")
    generate(pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]))
