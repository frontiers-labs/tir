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
        "use tir::{Any as TirAny, OpId, Operation, TypeId, ValueId};",
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
        "pub(crate) fn opcode_for_name(name: &str) -> Option<u16> {",
        "    Some(match name {",
    ])
    for instruction in operations:
        rust.append(f'        "{instruction["opname"][2:]}" => {instruction["opcode"]},')
    rust.extend(["        _ => return None,", "    })", "}", ""])
    rust.extend([
        "pub(crate) fn build_generated(",
        "    context: &tir::Context,",
        "    opcode: u16,",
        "    operands: &[ValueId],",
        "    result_type: TypeId,",
        ") -> Option<(OpId, ValueId)> {",
        "    Some(match opcode {",
    ])
    for instruction in operations:
        op_name = instruction["opname"][2:]
        fields = [
            field_name(operand.get("name"), index)
            for index, operand in enumerate(instruction["operands"][2:])
        ]
        chain = "".join(f".{field}(operands[{index}])" for index, field in enumerate(fields))
        rust.extend([
            f"        {instruction['opcode']} if operands.len() == {len(fields)} => {{",
            f"            let op = {op_name}OpBuilder::new(context){chain}.result_type(result_type).build();",
            "            (op.id(), op.result())",
            "        }",
        ])
    rust.extend(["        _ => return None,", "    })", "}", ""])
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
