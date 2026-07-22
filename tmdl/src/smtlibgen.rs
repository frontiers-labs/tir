use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::Type;
use crate::ast;
use crate::error::TMDLError;
use crate::sem_expr_state;
use crate::utils::{
    get_encoding_arms, isa_param_values, item_supports_isa, parse_literal_value,
    resolve_isa_param_values, resolve_operand_widths, resolve_operands_for_instruction,
    resolve_params_for_instruction,
};
use tir::graph::{Dag, NodeId};

#[derive(serde::Serialize)]
pub(crate) struct SmtMetadata {
    version: u32,
    isa: String,
    dialect: String,
    smt_prelude: String,
    flat_state: Vec<FlatStateFieldMetadata>,
    register_classes: Vec<RegisterClassMetadata>,
    instructions: Vec<InstructionMetadata>,
}

#[derive(serde::Serialize)]
struct InstructionMetadata {
    name: String,
    writes_pc: bool,
    width_bits: u16,
    operands: Vec<OperandMetadata>,
    supported: bool,
    write_classes: Vec<String>,
    uses_reservation: bool,
    pc_source_operands: Vec<usize>,
    memory_accesses: Vec<MemoryAccessMetadata>,
    trap_kinds: Vec<String>,
    encoding: Vec<EncodingFieldMetadata>,
    execute: Option<String>,
    flat_execute: Option<BTreeMap<String, String>>,
}

#[derive(serde::Serialize)]
struct FlatStateFieldMetadata {
    name: String,
    sort: String,
}

#[derive(serde::Serialize)]
struct RegisterClassMetadata {
    name: String,
    storage: String,
    index_width: u16,
    value_width: u16,
    storage_width: u16,
    zero_index: Option<u16>,
    bit_offset: u16,
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture mutex poisoned").extend(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct OperandMetadata {
    name: String,
    kind: String,
    class: Option<String>,
    width: u16,
}

#[derive(Clone, serde::Serialize)]
struct MemoryAccessMetadata {
    kind: &'static str,
    bytes: u64,
    address: String,
    flat_address: String,
}

#[derive(serde::Serialize)]
struct EncodingFieldMetadata {
    word_low: u16,
    word_high: u16,
    operand: Option<String>,
    operand_low: u16,
    value: String,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Register-file layout of one (non-PC) register class, resolved against the
/// target ISA's parameters.
struct ClassInfo {
    idx_width: u16,
    val_width: u16,
    /// Encoding index of a hardwired-zero register (RISC-V `x0`, AArch64
    /// `xzr`), if the class has one: reads yield 0 and writes are dropped.
    zero_index: Option<u16>,
    /// State field holding this class's registers. A derived class (AArch64
    /// `GPRsp : GPR`) aliases its base's physical file, so both classes
    /// read and write one array; only its accessors differ (`GPRsp` has no
    /// hardwired zero at slot 31, `GPR` does).
    storage: String,
    /// Bit within the storage element where this class's view begins (x86
    /// high-byte `ah` -> 8). Reads/writes address `[bit_offset, bit_offset+width)`.
    bit_offset: u16,
    /// Preserve the storage element's untouched bits on write (x86 8/16-bit
    /// writes) instead of zero-extending the value across it (the default).
    merge: bool,
}

struct SmtCtx<'a> {
    isa: &'a str,
    /// Register value width of the target ISA; immediates and the PC use it.
    xlen: u16,
    /// Widest instruction encoding in the ISA (bits). Fixed-width ISAs use one
    /// value (RISC-V/AArch64: 32); variable-length ISAs (x86) use the maximum,
    /// so the shared `decode_*`/`encode_*` word type holds every instruction.
    word_width: u16,
    /// Lowercase class name -> layout. BTreeMap so the emitted state datatype
    /// has a deterministic field order.
    classes: BTreeMap<String, ClassInfo>,
    pc_classes: std::collections::HashSet<String>,
    isa_params: HashMap<String, i64>,
    /// The target ISA's trap-entry sequence, inlined at `trap(...)` calls.
    trap_handler: Option<&'a ast::TrapHandler>,
}

/// The trap handler of `isa` or the nearest one in its requires closure.
fn find_trap_handler<'a>(
    isa: &str,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Option<&'a ast::TrapHandler> {
    let mut pending = vec![isa.to_string()];
    let mut visited = std::collections::HashSet::new();
    while let Some(name) = pending.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(ast::Item::Isa(isa)) = item_cache.get(name.as_str()) else {
            continue;
        };
        if let Some(handler) = &isa.trap_handler {
            return Some(handler);
        }
        match &isa.requires {
            None => {}
            Some(ast::IsaRequirement::Single(parent)) => pending.push(parent.clone()),
            Some(ast::IsaRequirement::Any(parents)) | Some(ast::IsaRequirement::All(parents)) => {
                pending.extend(parents.iter().cloned());
            }
        }
    }
    None
}

/// Instruction operands with `bits<expr>` widths resolved for the target ISA
/// (the ISA's own parameter values win over the cross-ISA maximum, so an
/// instruction shared by RV32I and RV64I sees XLEN=32 on RV32I).
fn resolved_operands<'a>(
    ctx: &SmtCtx<'_>,
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, Type)> {
    let mut params = resolve_isa_param_values(inst, item_cache);
    params.extend(ctx.isa_params.iter().map(|(k, v)| (k.clone(), *v)));
    resolve_operand_widths(resolve_operands_for_instruction(inst, item_cache), &params)
}

impl SmtCtx<'_> {
    fn idx_width(&self, class: &str) -> u16 {
        self.classes
            .get(&class.to_lowercase())
            .map_or(5, |c| c.idx_width)
    }

    fn val_width(&self, class: &str) -> u16 {
        let class = class.to_lowercase();
        if self.pc_classes.contains(&class) {
            return self.xlen;
        }
        self.classes.get(&class).map_or(self.xlen, |c| c.val_width)
    }
}

/// Resolve a register-class parameter (`ENCODING_LEN`, `WIDTH`) to a number:
/// either a literal or a `self.PARAM` reference into the target ISA.
fn eval_class_param(
    rc: &ast::RegisterClass,
    name: &str,
    isa_params: &HashMap<String, i64>,
) -> Option<i64> {
    match rc.parameters.get(name)? {
        (_, Some(ast::Expr::Lit(ast::Lit::Int(li)))) => Some(parse_literal_value(li) as i64),
        (_, Some(ast::Expr::Field(f))) if matches!(&*f.base, ast::Expr::Ident(id) if id.name == "self") => {
            isa_params.get(f.member.as_str()).copied()
        }
        _ => None,
    }
}

/// Whether a class's `WRITE_POLICY` is `"merge"` (preserve untouched storage
/// bits on write); absent or `"zero_extend"` is the default zero-extend.
fn class_write_merge(rc: &ast::RegisterClass) -> bool {
    matches!(
        rc.parameters.get("WRITE_POLICY"),
        Some((_, Some(ast::Expr::Lit(ast::Lit::Str(s))))) if s.value() == "merge"
    )
}

pub fn generate_smtlib<'a>(
    dialect: &str,
    isa: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    mut output: Box<dyn Write>,
) -> Result<SmtMetadata, TMDLError> {
    let isa_params = isa_param_values(isa, item_cache);
    let xlen = isa_params.get("XLEN").copied().unwrap_or(64) as u16;

    let mut classes = BTreeMap::new();
    let mut pc_classes = std::collections::HashSet::new();
    let class_param = |name: &str, rc: &ast::RegisterClass, default: u16| {
        eval_class_param(rc, name, &isa_params).unwrap_or(default as i64) as u16
    };
    let enc_len_of: HashMap<String, u16> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .map(|rc| (rc.name.to_lowercase(), class_param("ENCODING_LEN", rc, 5)))
        .collect();
    let width_of: HashMap<String, u16> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .map(|rc| (rc.name.to_lowercase(), class_param("WIDTH", rc, xlen)))
        .collect();
    // A class draws its physical storage from its `base` (inheritance shares the
    // file with identical indices; a narrower base is a sub-register view, e.g.
    // x86 `eax` in `rax`) or an explicit `file` alias. A `file` alias folds into
    // the file's storage only as a strictly narrower sub-view sharing the index
    // width (x86 `ah` -> `GPR`, 8-bit in a 4-indexed 64-bit file). Same-width
    // groupings (RISC-V `VRM2` over `VR`), narrower-indexed aliases (compressed
    // 3-bit -> 5-bit) and wider ones (`FPR64` over `FPR32`) keep their own
    // storage, preserving the pre-existing per-file arrays.
    let base_of: HashMap<String, String> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .filter_map(|rc| {
            let child = rc.name.to_lowercase();
            if let Some(b) = &rc.base {
                return Some((child, b.to_lowercase()));
            }
            let file = rc.file.as_ref()?.to_lowercase();
            let compatible = enc_len_of.get(&child) == enc_len_of.get(&file)
                && width_of.get(&child) < width_of.get(&file);
            compatible.then_some((child, file))
        })
        .collect();
    let storage_of = |name: &str| {
        let mut current = name.to_string();
        while let Some(base) = base_of.get(&current) {
            if *base == current {
                break;
            }
            current = base.clone();
        }
        current
    };
    for rc in files.iter().flat_map(|f| f.register_classes()) {
        if !item_supports_isa(&rc.for_isas, isa, item_cache) {
            continue;
        }
        let name = rc.name.to_lowercase();
        if is_pc_class(rc) {
            pc_classes.insert(name);
            continue;
        }
        // A class with no encoding slots (x86 EFLAGS: read/written by name, never
        // encoded in an instruction) still needs a nonzero index width to hold
        // its per-register slot number, since a `(_ BitVec 0)` array index is
        // illegal in SMT.
        let mut idx_width = eval_class_param(rc, "ENCODING_LEN", &isa_params).unwrap_or(5) as u16;
        if idx_width == 0 {
            let max_idx = rc
                .register_indices()
                .into_iter()
                .map(|(_, i)| i)
                .max()
                .unwrap_or(0);
            idx_width = (16 - max_idx.leading_zeros() as u16).max(1);
        }
        classes.insert(
            name.clone(),
            ClassInfo {
                idx_width,
                val_width: eval_class_param(rc, "WIDTH", &isa_params).unwrap_or(xlen as i64) as u16,
                zero_index: rc.hardwired_zero_register_index(),
                storage: storage_of(&name),
                bit_offset: eval_class_param(rc, "BIT_OFFSET", &isa_params).unwrap_or(0) as u16,
                merge: class_write_merge(rc),
            },
        );
    }
    let word_width = files
        .iter()
        .flat_map(|f| f.instructions())
        .filter(|i| item_supports_isa(&i.for_isas, isa, item_cache))
        .map(|i| encoding_width(i, item_cache))
        .max()
        .unwrap_or(32);
    let ctx = SmtCtx {
        isa,
        xlen,
        word_width,
        classes,
        pc_classes,
        isa_params,
        trap_handler: find_trap_handler(isa, item_cache),
    };

    let state_bytes = Arc::new(Mutex::new(Vec::new()));
    let mut state_output: Box<dyn Write> = Box::new(Capture(state_bytes.clone()));
    build_state(&ctx, &mut state_output)?;
    let state_smt = String::from_utf8(state_bytes.lock().expect("capture mutex poisoned").clone())
        .expect("SMT output is UTF-8");
    let smt_prelude = format!("{HEADER}\n{state_smt}");
    write!(output, "{smt_prelude}")?;
    let instructions = build_instructions(dialect, &ctx, item_cache, files, &mut output)?;
    build_decoder(dialect, &ctx, item_cache, files, &mut output)?;
    Ok(SmtMetadata {
        version: 1,
        isa: isa.to_string(),
        dialect: dialect.to_string(),
        smt_prelude,
        flat_state: flat_state_fields(&ctx),
        register_classes: register_class_metadata(&ctx),
        instructions,
    })
}

// ---------------------------------------------------------------------------
// State (register file) declaration
// ---------------------------------------------------------------------------

fn is_pc_class(rc: &ast::RegisterClass) -> bool {
    rc.resolve_registers()
        .any(|r| r.traits.contains(&ast::RegisterTrait::ProgramCounter))
}

fn flat_state_fields(ctx: &SmtCtx<'_>) -> Vec<FlatStateFieldMetadata> {
    let mut fields = ctx
        .classes
        .iter()
        .filter(|(name, info)| info.storage == **name)
        .map(|(name, info)| FlatStateFieldMetadata {
            name: name.clone(),
            sort: format!(
                "(Array (_ BitVec {}) (_ BitVec {}))",
                info.idx_width, info.val_width
            ),
        })
        .collect::<Vec<_>>();
    fields.extend([
        FlatStateFieldMetadata {
            name: "mem".to_string(),
            sort: format!("(Array (_ BitVec {}) (_ BitVec 8))", ctx.xlen),
        },
        FlatStateFieldMetadata {
            name: "resv".to_string(),
            sort: "Bool".to_string(),
        },
        FlatStateFieldMetadata {
            name: "resa".to_string(),
            sort: format!("(_ BitVec {})", ctx.xlen),
        },
        FlatStateFieldMetadata {
            name: "pc".to_string(),
            sort: format!("(_ BitVec {})", ctx.xlen),
        },
    ]);
    fields
}

fn register_class_metadata(ctx: &SmtCtx<'_>) -> Vec<RegisterClassMetadata> {
    ctx.classes
        .iter()
        .map(|(name, info)| RegisterClassMetadata {
            name: name.clone(),
            storage: info.storage.clone(),
            index_width: info.idx_width,
            value_width: info.val_width,
            storage_width: ctx.classes[&info.storage].val_width,
            zero_index: info.zero_index,
            bit_offset: info.bit_offset,
        })
        .collect()
}

/// Render a `(mk-TMDLState ...)` over `st`, defaulting each field to
/// `(<field> st)` and replacing named fields from `overrides`. Field order
/// matches the datatype: register arrays, mem, resv, resa, pc.
fn mk_state(arrays: &[&String], overrides: &[(&str, &str)]) -> String {
    let field = |name: &str| {
        overrides
            .iter()
            .find(|(n, _)| *n == name)
            .map_or_else(|| format!("({} st)", name), |(_, e)| e.to_string())
    };
    let mut fields: Vec<String> = arrays.iter().map(|n| field(n.as_str())).collect();
    for f in ["mem", "resv", "resa", "pc"] {
        fields.push(field(f));
    }
    format!("(mk-TMDLState {})", fields.join(" "))
}

fn build_state(ctx: &SmtCtx<'_>, output: &mut Box<dyn Write>) -> Result<(), TMDLError> {
    // Derived classes alias their base's array, so only storage-owning
    // classes contribute a state field.
    let arrays: Vec<&String> = ctx
        .classes
        .iter()
        .filter(|(name, info)| info.storage == **name)
        .map(|(name, _)| name)
        .collect();

    let mut fields = arrays
        .iter()
        .map(|name| {
            let info = &ctx.classes[*name];
            format!(
                "({} (Array (_ BitVec {}) (_ BitVec {})))",
                name, info.idx_width, info.val_width
            )
        })
        .collect::<Vec<_>>();
    fields.push(format!(
        "(mem (Array (_ BitVec {}) (_ BitVec 8)))",
        ctx.xlen
    ));
    fields.push("(resv Bool)".to_string());
    fields.push(format!("(resa (_ BitVec {}))", ctx.xlen));
    fields.push(format!("(pc (_ BitVec {}))", ctx.xlen));

    writeln!(
        output,
        "(declare-datatypes () ((TMDLState (mk-TMDLState {}))))",
        fields.join(" ")
    )?;

    for (name, info) in &ctx.classes {
        let idx_width = info.idx_width;
        let val_width = info.val_width;
        let storage = &info.storage;
        let off = info.bit_offset;
        // A class narrower than its storage file is a sub-register view of it
        // (x86 `eax`/`ax`/`al` in `rax`): reads extract `[off, off+width)` of the
        // file element. A zero-extend write clears the rest of the element (x86
        // 32-bit, AArch64 scalar FP); a merge write preserves it (x86 8/16-bit).
        let storage_val_width = ctx.classes.get(storage).map_or(val_width, |c| c.val_width);
        let selected = format!("(select ({storage} st) r)");
        let is_subview = val_width < storage_val_width || off > 0;
        let select = if is_subview {
            format!("((_ extract {} {off}) {selected})", off + val_width - 1)
        } else {
            selected.clone()
        };
        let read_body = match info.zero_index {
            Some(z) => {
                format!("(ite (= r (_ bv{z} {idx_width}))\n    (_ bv0 {val_width})\n    {select})")
            }
            None => select,
        };
        writeln!(
            output,
            "\n(define-fun read_{name} ((st TMDLState) (r (_ BitVec {idx_width}))) (_ BitVec {val_width})\n  {read_body})",
        )?;

        let stored_val = if info.merge || off > 0 {
            // Splice `val` into `[off, off+width)`, keeping the element's other
            // bits from the current slot: concat of high ++ val ++ low.
            let mut parts = Vec::new();
            if off + val_width < storage_val_width {
                parts.push(format!(
                    "((_ extract {} {}) {selected})",
                    storage_val_width - 1,
                    off + val_width
                ));
            }
            parts.push("val".to_string());
            if off > 0 {
                parts.push(format!("((_ extract {} 0) {selected})", off - 1));
            }
            parts
                .into_iter()
                .reduce(|a, b| format!("(concat {a} {b})"))
                .expect("at least the val segment")
        } else if val_width < storage_val_width {
            format!("((_ zero_extend {}) val)", storage_val_width - val_width)
        } else {
            "val".to_string()
        };
        let stored = format!("(store ({storage} st) r {stored_val})");
        let store = mk_state(&arrays, &[(storage.as_str(), stored.as_str())]);
        let write_body = match info.zero_index {
            Some(z) => format!("(ite (= r (_ bv{z} {idx_width}))\n    st\n    {store})"),
            None => store,
        };
        writeln!(
            output,
            "\n(define-fun write_{name} ((st TMDLState) (r (_ BitVec {idx_width})) (val (_ BitVec {val_width}))) TMDLState\n  {write_body})",
        )?;
    }

    writeln!(
        output,
        "\n(define-fun write_pc ((st TMDLState) (val (_ BitVec {val_width}))) TMDLState\n  {body})",
        val_width = ctx.xlen,
        body = mk_state(&arrays, &[("pc", "val")])
    )?;

    // Byte-addressable little-endian memory accessors, one pair per access
    // width, mirroring the interpreter's `Memory` convention.
    let xlen = ctx.xlen;
    for bytes in MEM_ACCESS_BYTES {
        let val_width = bytes * 8;
        let byte_at = |i: u16| {
            if i == 0 {
                "(select (mem st) addr)".to_string()
            } else {
                format!("(select (mem st) (bvadd addr (_ bv{i} {xlen})))")
            }
        };
        let read_body = (0..bytes)
            .rev()
            .map(byte_at)
            .reduce(|acc, b| format!("(concat {} {})", acc, b))
            .expect("at least one byte");
        writeln!(
            output,
            "\n(define-fun read_mem_{bytes} ((st TMDLState) (addr (_ BitVec {xlen}))) (_ BitVec {val_width})\n  {read_body})",
        )?;

        let mut mem = "(mem st)".to_string();
        for i in 0..bytes {
            let slot = if i == 0 {
                "addr".to_string()
            } else {
                format!("(bvadd addr (_ bv{i} {xlen}))")
            };
            let byte = format!("((_ extract {} {}) val)", i * 8 + 7, i * 8);
            mem = format!("(store {} {} {})", mem, slot, byte);
        }
        writeln!(
            output,
            "\n(define-fun write_mem_{bytes} ((st TMDLState) (addr (_ BitVec {xlen})) (val (_ BitVec {val_width}))) TMDLState\n  {body})",
            body = mk_state(&arrays, &[("mem", mem.as_str())])
        )?;
    }

    // Reservation constructors: `set_res` records the reserved address and
    // marks the reservation valid; `clear_res` invalidates it (LR/SC/AMO).
    writeln!(
        output,
        "\n(define-fun set_res ((st TMDLState) (a (_ BitVec {xlen}))) TMDLState\n  {})",
        mk_state(&arrays, &[("resv", "true"), ("resa", "a")])
    )?;
    writeln!(
        output,
        "\n(define-fun clear_res ((st TMDLState)) TMDLState\n  {})",
        mk_state(&arrays, &[("resv", "false")])
    )?;

    Ok(())
}

/// Memory access widths with dedicated SMT accessors.
const MEM_ACCESS_BYTES: [u16; 4] = [1, 2, 4, 8];

// ---------------------------------------------------------------------------
// Instruction encoding and execution
// ---------------------------------------------------------------------------

fn build_instructions<'a>(
    dialect: &str,
    ctx: &SmtCtx<'_>,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    files: &'a [ast::File],
    output: &mut Box<dyn Write>,
) -> Result<Vec<InstructionMetadata>, TMDLError> {
    let mut instruction_variants = vec![];
    let mut encode_arms = vec![];
    let mut execute_arms = vec![];
    let mut metadata = vec![];

    // `(class, register-name) -> encoding index` so register paths without a
    // numeric index (e.g. `PC::pc`) lower to a stable slot.
    let register_index_map: HashMap<(String, String), u32> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .flat_map(|rc| {
            let class = rc.name.clone();
            rc.register_indices()
                .into_iter()
                .map(move |(name, idx)| ((class.clone(), name), u32::from(idx)))
        })
        .collect();

    for i in files.iter().flat_map(|f| f.instructions()) {
        if !item_supports_isa(&i.for_isas, ctx.isa, item_cache) {
            continue;
        }
        let name = i.name.to_lowercase();
        let uppercase_name = name.to_uppercase();

        let operands = resolved_operands(ctx, i, item_cache);
        let smt_operands = build_smt_operands(ctx, &operands);
        let smt_operands_joined = smt_operands.join(" ");
        let operand_params = if smt_operands_joined.is_empty() {
            "()".to_string()
        } else {
            format!("({smt_operands_joined})")
        };
        let execute_params = if smt_operands_joined.is_empty() {
            "((st TMDLState))".to_string()
        } else {
            format!("((st TMDLState) {smt_operands_joined})")
        };
        let (smt_encoding, enc_width) = build_smt_encoding(ctx, item_cache, i, &operands);
        let behavior = build_smt_behavior(ctx, item_cache, i, &operands, &register_index_map);
        // Untranslatable behaviors (e.g. memory accesses) get an identity body
        // plus a machine-readable marker so verification tooling can tell
        // "proven unchanged" apart from "not modeled".
        let supported = behavior.is_some();
        let (smt_behavior, marker, writes_pc) = match &behavior {
            Some(behavior) => (behavior.body.clone(), String::new(), behavior.writes_pc),
            None => (
                "st".to_string(),
                format!("\n; UNSUPPORTED-BEHAVIOR: {}", name),
                false,
            ),
        };
        write!(output, "{}", marker)?;

        // Machine-readable operand inventory for verification tooling.
        let operand_meta = operands
            .iter()
            .map(|(op_name, ty)| {
                let kind = match ty {
                    Type::Struct(rc) => {
                        format!("reg:{}:{}", rc.to_lowercase(), ctx.idx_width(rc))
                    }
                    Type::Bits(n) => format!("bits:{}", n),
                    _ => "int".to_string(),
                };
                format!("{}:{}", op_name.to_lowercase(), kind)
            })
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(
            output,
            "\n; INSTRUCTION: {} writes-pc={} width={} {}",
            name, writes_pc, enc_width, operand_meta
        )?;

        let operand_metadata = operands
            .iter()
            .map(|(operand_name, ty)| match ty {
                Type::Struct(class) => OperandMetadata {
                    name: operand_name.to_lowercase(),
                    kind: "register".to_string(),
                    class: Some(class.to_lowercase()),
                    width: ctx.idx_width(class),
                },
                Type::Bits(width) => OperandMetadata {
                    name: operand_name.to_lowercase(),
                    kind: "bits".to_string(),
                    class: None,
                    width: *width,
                },
                _ => OperandMetadata {
                    name: operand_name.to_lowercase(),
                    kind: "int".to_string(),
                    class: None,
                    width: ctx.xlen,
                },
            })
            .collect();
        let pc_source_operands = behavior.as_ref().map_or_else(Vec::new, |behavior| {
            operands
                .iter()
                .enumerate()
                .filter_map(|(index, (operand_name, ty))| {
                    let Type::Struct(_) = ty else {
                        return None;
                    };
                    behavior
                        .pc_source_names
                        .contains(&operand_name.to_lowercase())
                        .then_some(index)
                })
                .collect()
        });
        metadata.push(InstructionMetadata {
            name: name.clone(),
            writes_pc,
            width_bits: enc_width,
            operands: operand_metadata,
            supported,
            write_classes: behavior
                .as_ref()
                .map_or_else(Vec::new, |behavior| behavior.write_classes.clone()),
            uses_reservation: behavior
                .as_ref()
                .is_some_and(|behavior| behavior.uses_reservation),
            pc_source_operands,
            memory_accesses: behavior
                .as_ref()
                .map_or_else(Vec::new, |behavior| behavior.memory_accesses.clone()),
            trap_kinds: behavior
                .as_ref()
                .map_or_else(Vec::new, |behavior| behavior.trap_kinds.clone()),
            encoding: build_encoding_metadata(item_cache, i, &operands),
            execute: behavior.as_ref().map(|behavior| behavior.body.clone()),
            flat_execute: behavior
                .as_ref()
                .and_then(|behavior| behavior.flat_execute.clone()),
        });

        let operand_names = operands
            .iter()
            .map(|(k, _v)| k.to_lowercase())
            .collect::<Vec<_>>();
        let operand_list = operand_names.join(" ");

        writeln!(
            output,
            "\n(define-fun encode_{name} {operand_params} (_ BitVec {enc_width})\n  {smt_encoding})\n\n(define-fun execute_{name} {execute_params} TMDLState\n  {smt_behavior})"
        )?;

        // The shared `encode_{dialect}` returns the ISA's widest encoding, so a
        // narrower instruction's word is zero-extended into it.
        let pad = |call: String| {
            if enc_width < ctx.word_width {
                format!("((_ zero_extend {}) {call})", ctx.word_width - enc_width)
            } else {
                call
            }
        };

        // SMT-LIB requires datatype accessor names to be unique within the
        // whole datatype.  Prefix each accessor with the instruction name so
        // that `ADD_rd` and `SUB_rd` don't collide.  Match arms use positional
        // pattern binding, so they are unaffected by this renaming.
        let variant_operands = operands
            .iter()
            .map(|(op_name, ty)| {
                format!(
                    "({}_{} {})",
                    name,
                    op_name.to_lowercase(),
                    smt_ty_of(ctx, ty)
                )
            })
            .collect::<Vec<_>>()
            .join(" ");

        if variant_operands.is_empty() {
            instruction_variants.push(format!("({uppercase_name})"));
        } else {
            instruction_variants.push(format!("({uppercase_name} {variant_operands})"));
        }

        // Build ite-based dispatch arms using the prefixed accessor names.
        // Z3's SMT-LIB `match` does not support pattern variable binding, so
        // we use `(_ is VARIANT)` discriminators and named accessors instead.
        let accessor_args = operand_names
            .iter()
            .map(|op| format!("({name}_{op} instr)"))
            .collect::<Vec<_>>()
            .join(" ");

        if operand_list.is_empty() {
            // Nullary functions and constructors are referenced bare in SMT-LIB.
            encode_arms.push(format!(
                "((_ is {uppercase_name}) instr) {}",
                pad(format!("encode_{name}"))
            ));
            execute_arms.push(format!(
                "((_ is {uppercase_name}) instr) (execute_{name} state)"
            ));
        } else {
            encode_arms.push(format!(
                "((_ is {uppercase_name}) instr) {}",
                pad(format!("(encode_{name} {accessor_args})"))
            ));
            execute_arms.push(format!(
                "((_ is {uppercase_name}) instr) (execute_{name} state {accessor_args})"
            ));
        }
    }

    writeln!(
        output,
        "\n(declare-datatypes () ((TMDLInstr {})))",
        instruction_variants.join(" ")
    )?;

    // Fold arms into nested ites; the last instruction is the fallback.
    // encode_* and execute_* already exist at this point so the ite can call them.
    let word_width = ctx.word_width;
    let encode_body = encode_arms
        .iter()
        .rev()
        .fold(format!("(_ bv0 {word_width})"), |else_branch, arm| {
            format!("(ite {} {})", arm, else_branch)
        });
    writeln!(
        output,
        "\n(define-fun encode_{dialect} ((instr TMDLInstr)) (_ BitVec {word_width})\n  {encode_body})"
    )?;

    let execute_body = execute_arms
        .iter()
        .rev()
        .fold("state".to_string(), |else_branch, arm| {
            format!("(ite {} {})", arm, else_branch)
        });
    writeln!(
        output,
        "\n(define-fun execute_{dialect} ((state TMDLState) (instr TMDLInstr)) TMDLState\n  {execute_body})"
    )?;

    Ok(metadata)
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

fn build_smt_operands(ctx: &SmtCtx<'_>, operands: &[(String, Type)]) -> Vec<String> {
    operands
        .iter()
        .map(|(name, ty)| format!("({} {})", name.to_lowercase(), smt_ty_of(ctx, ty)))
        .collect()
}

fn smt_ty_of(ctx: &SmtCtx<'_>, ty: &Type) -> String {
    match ty {
        Type::Struct(rc) => format!("(_ BitVec {})", ctx.idx_width(rc)),
        Type::Bits(_) | Type::Integer => format!("(_ BitVec {})", ctx.xlen),
        Type::String => "String".to_string(),
        _ => unreachable!("HM type vars should not appear as operand types"),
    }
}

/// Total bit width of an instruction's encoding (highest covered bit + 1).
fn encoding_width<'a>(
    instruction: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> u16 {
    get_encoding_arms(instruction, item_cache)
        .iter()
        .map(|arm| arm.end.unwrap_or(arm.start) + 1)
        .max()
        .unwrap_or(32)
}

fn build_encoding_metadata<'a>(
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    instruction: &'a ast::Instruction,
    operands: &[(String, Type)],
) -> Vec<EncodingFieldMetadata> {
    let params = resolve_params_for_instruction(instruction, item_cache);
    get_encoding_arms(instruction, item_cache)
        .into_iter()
        .map(|arm| {
            let word_high = arm.end.unwrap_or(arm.start);
            let (operand, operand_low, value) = match &arm.value {
                ast::Expr::Lit(ast::Lit::Int(lit)) => {
                    (None, 0, parse_literal_value_u128(lit).to_string())
                }
                ast::Expr::Ident(id)
                    if !operands.iter().any(|(name, _)| name == &id.name)
                        && params.contains_key(&id.name) =>
                {
                    let value = params
                        .get(&id.name)
                        .and_then(|(_, value)| value.as_ref())
                        .and_then(|value| match value {
                            ast::Expr::Lit(ast::Lit::Int(lit)) => {
                                Some(parse_literal_value_u128(lit))
                            }
                            _ => None,
                        })
                        .unwrap_or(0);
                    (None, 0, value.to_string())
                }
                ast::Expr::Ident(id) => (Some(id.name.to_lowercase()), 0, "0".to_string()),
                ast::Expr::Slice(slice) => {
                    let operand = match &*slice.base {
                        ast::Expr::Ident(id) => Some(id.name.to_lowercase()),
                        _ => None,
                    };
                    (operand, slice.start, "0".to_string())
                }
                ast::Expr::IndexAccess(index) => {
                    let operand = match &*index.base {
                        ast::Expr::Ident(id) => Some(id.name.to_lowercase()),
                        _ => None,
                    };
                    (operand, index.index, "0".to_string())
                }
                _ => (None, 0, "0".to_string()),
            };
            EncodingFieldMetadata {
                word_low: arm.start,
                word_high,
                operand,
                operand_low,
                value,
            }
        })
        .collect()
}

/// Returns the encoding expression and its bit width.
fn build_smt_encoding<'a>(
    ctx: &SmtCtx<'_>,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    instruction: &'a ast::Instruction,
    operands: &[(String, Type)],
) -> (String, u16) {
    let operands = operands.iter().cloned().collect::<HashMap<_, _>>();
    let params = resolve_params_for_instruction(instruction, item_cache);
    let encoding_arms = get_encoding_arms(instruction, item_cache);

    let mut pieces: Vec<(u16, String)> = Vec::new();
    for arm in &encoding_arms {
        let start = arm.start;
        let end = arm.end.unwrap_or(start);
        let width: u16 = end - start + 1;
        let high_bit = end;

        let piece = match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => render_lit_bitvec(width, li),
            ast::Expr::Ident(id) => {
                let name = &id.name;
                if let Some(ty) = operands.get(name) {
                    let vname = name.to_lowercase();
                    match ty {
                        Type::Struct(rc) => cast_bv(&vname, ctx.idx_width(rc), width),
                        Type::Bits(_) | Type::Integer => cast_bv(&vname, ctx.xlen, width),
                        Type::String => zero_bv(width),
                        _ => unreachable!("HM type vars should not appear as operand types"),
                    }
                } else if let Some((pty, pval)) = params.get(name) {
                    match pval {
                        Some(ast::Expr::Lit(ast::Lit::Int(li))) => render_lit_bitvec(width, li),
                        _ => match pty {
                            Type::Bits(_) | Type::Integer => zero_bv(width),
                            _ => zero_bv(width),
                        },
                    }
                } else {
                    zero_bv(width)
                }
            }
            ast::Expr::Slice(s) => {
                let base_str = match &*s.base {
                    ast::Expr::Ident(id) => id.name.to_lowercase(),
                    _ => "(_ bv0 64)".to_string(),
                };
                format!("((_ extract {} {}) {})", s.end, s.start, base_str)
            }
            ast::Expr::IndexAccess(s) => {
                let base_str = match &*s.base {
                    ast::Expr::Ident(id) => id.name.to_lowercase(),
                    _ => "(_ bv0 64)".to_string(),
                };
                format!("((_ extract {} {}) {})", s.index, s.index, base_str)
            }
            _ => zero_bv(width),
        };

        pieces.push((high_bit, piece));
    }

    pieces.sort_by_key(|piece| std::cmp::Reverse(piece.0));

    let width = pieces.iter().map(|(hi, _)| hi + 1).max().unwrap_or(32);
    let mut iter = pieces.into_iter().map(|(_, piece)| piece);
    let expr = iter
        .next()
        .map(|first| iter.fold(first, |acc, piece| format!("(concat {} {})", acc, piece)))
        .unwrap_or_else(|| format!("(_ bv0 {width})"));
    (expr, width)
}

// ---------------------------------------------------------------------------
// Behavior (execution semantics)
// ---------------------------------------------------------------------------

/// Sort of an emitted SMT expression. Mirrors the width/signedness tracking of
/// the sem-expr interpreter (`tir::sem::exec`), which evaluates behaviors
/// over `APInt`s of varying width: every value is a bitvector of the
/// interpreter's width, except comparisons which stay `Bool` until they cross
/// back into arithmetic.
#[derive(Clone, Copy, PartialEq)]
enum SmtSort {
    Bool,
    Bv { width: u32, signed: bool },
}

#[derive(Clone)]
struct SmtVal {
    expr: String,
    sort: SmtSort,
}

impl SmtVal {
    fn bv(expr: String, width: u32, signed: bool) -> Self {
        SmtVal {
            expr,
            sort: SmtSort::Bv { width, signed },
        }
    }

    fn boolean(expr: String) -> Self {
        SmtVal {
            expr,
            sort: SmtSort::Bool,
        }
    }

    /// Comparison results materialize as width-1 integers, matching the
    /// interpreter's `APInt::new(1, ...)`.
    fn as_bv(&self) -> (String, u32, bool) {
        match &self.sort {
            SmtSort::Bool => (format!("(ite {} (_ bv1 1) (_ bv0 1))", self.expr), 1, false),
            SmtSort::Bv { width, signed } => (self.expr.clone(), *width, *signed),
        }
    }

    fn as_bool(&self) -> String {
        match &self.sort {
            SmtSort::Bool => self.expr.clone(),
            SmtSort::Bv { width, .. } => {
                format!("(distinct {} (_ bv0 {}))", self.expr, width)
            }
        }
    }
}

/// Coerce an expression to exactly `target` bits: widen when narrower,
/// truncate when wider. Register writes use it so a value computed at a wider
/// width still fits a narrow destination (e.g. a 1-bit PSTATE flag).
fn fit_smt(expr: &str, width: u32, signed: bool, target: u32) -> String {
    if width > target {
        format!("((_ extract {} 0) {})", target - 1, expr)
    } else {
        widen_smt(expr, width, signed, target)
    }
}

/// Mirror of `exec::widen`: sign-extend signed values, zero-extend unsigned
/// ones, no-op when already at least `target` wide.
fn widen_smt(expr: &str, width: u32, signed: bool, target: u32) -> String {
    if width >= target {
        expr.to_string()
    } else if signed {
        format!("((_ sign_extend {}) {})", target - width, expr)
    } else {
        format!("((_ zero_extend {}) {})", target - width, expr)
    }
}

/// Widen both operands to a common width, mirroring `exec::coerce_ints`.
fn coerce_smt(a: &SmtVal, b: &SmtVal) -> (String, String, u32, bool, bool) {
    let (ea, wa, sa) = a.as_bv();
    let (eb, wb, sb) = b.as_bv();
    let w = wa.max(wb);
    (
        widen_smt(&ea, wa, sa, w),
        widen_smt(&eb, wb, sb, w),
        w,
        sa,
        sb,
    )
}

enum SmtSymbolInfo {
    Register {
        class: String,
        number: u32,
    },
    Variable {
        name: String,
    },
    /// `regnum(op)`: the operand's encoding index. In the SMT encoding a
    /// register operand is already passed as its index parameter, so this
    /// resolves to that parameter directly (width `ENCODING_LEN`).
    RegNum {
        name: String,
    },
}

#[derive(Clone)]
struct FlatState {
    fields: BTreeMap<String, String>,
}

impl FlatState {
    fn initial(ctx: &SmtCtx<'_>) -> Self {
        let mut fields = BTreeMap::new();
        for (name, info) in &ctx.classes {
            if info.storage == *name {
                fields.insert(name.clone(), format!("st0_{name}"));
            }
        }
        for name in ["mem", "resv", "resa", "pc"] {
            fields.insert(name.to_string(), format!("st0_{name}"));
        }
        Self { fields }
    }

    fn write_register(&self, ctx: &SmtCtx<'_>, class: &str, index: &str, value: &str) -> Self {
        let info = &ctx.classes[class];
        let storage = &info.storage;
        let current = &self.fields[storage];
        let storage_width = ctx.classes[storage].val_width;
        let selected = format!("(select {current} {index})");
        let stored_value = if info.merge || info.bit_offset > 0 {
            let mut parts = Vec::new();
            if info.bit_offset + info.val_width < storage_width {
                parts.push(format!(
                    "((_ extract {} {}) {selected})",
                    storage_width - 1,
                    info.bit_offset + info.val_width
                ));
            }
            parts.push(value.to_string());
            if info.bit_offset > 0 {
                parts.push(format!(
                    "((_ extract {} 0) {selected})",
                    info.bit_offset - 1
                ));
            }
            parts
                .into_iter()
                .reduce(|high, low| format!("(concat {high} {low})"))
                .expect("register write has a value segment")
        } else if info.val_width < storage_width {
            format!(
                "((_ zero_extend {}) {value})",
                storage_width - info.val_width
            )
        } else {
            value.to_string()
        };
        let stored = format!("(store {current} {index} {stored_value})");
        let stored = match info.zero_index {
            Some(zero) => format!(
                "(ite (= {index} (_ bv{zero} {})) {current} {stored})",
                info.idx_width
            ),
            None => stored,
        };
        let mut next = self.clone();
        next.fields.insert(storage.clone(), stored);
        next
    }

    fn write_memory(&self, xlen: u16, bytes: u16, address: &str, value: &str) -> Self {
        let mut memory = self.fields["mem"].clone();
        for offset in 0..bytes {
            let slot = if offset == 0 {
                address.to_string()
            } else {
                format!("(bvadd {address} (_ bv{offset} {xlen}))")
            };
            let byte = format!("((_ extract {} {}) {value})", offset * 8 + 7, offset * 8);
            memory = format!("(store {memory} {slot} {byte})");
        }
        let mut next = self.clone();
        next.fields.insert("mem".to_string(), memory);
        next
    }

    fn select(condition: &str, then_state: &Self, else_state: &Self) -> Self {
        let fields = then_state
            .fields
            .iter()
            .map(|(name, then_value)| {
                let else_value = &else_state.fields[name];
                let value = if then_value == else_value {
                    then_value.clone()
                } else {
                    format!("(ite {condition} {then_value} {else_value})")
                };
                (name.clone(), value)
            })
            .collect();
        Self { fields }
    }
}

#[derive(Clone, Copy)]
enum SmtStateRef<'a> {
    Datatype(&'a str),
    Flat(&'a FlatState),
}

impl SmtStateRef<'_> {
    fn pc(self) -> String {
        match self {
            Self::Datatype(state) => format!("(pc {state})"),
            Self::Flat(state) => state.fields["pc"].clone(),
        }
    }

    fn field(self, name: &str) -> String {
        match self {
            Self::Datatype(state) => format!("({name} {state})"),
            Self::Flat(state) => state.fields[name].clone(),
        }
    }

    fn read_register(self, ctx: &SmtCtx<'_>, class: &str, index: &str) -> String {
        if matches!(self, Self::Datatype(_)) {
            return format!(
                "(read_{class} {} {index})",
                match self {
                    Self::Datatype(state) => state,
                    _ => unreachable!(),
                }
            );
        }
        let info = &ctx.classes[class];
        let storage = self.field(&info.storage);
        let selected = format!("(select {storage} {index})");
        let storage_width = ctx.classes[&info.storage].val_width;
        let value = if info.val_width < storage_width || info.bit_offset > 0 {
            format!(
                "((_ extract {} {}) {selected})",
                info.bit_offset + info.val_width - 1,
                info.bit_offset
            )
        } else {
            selected
        };
        match info.zero_index {
            Some(zero) => format!(
                "(ite (= {index} (_ bv{zero} {})) (_ bv0 {}) {value})",
                info.idx_width, info.val_width
            ),
            None => value,
        }
    }

    fn read_memory(self, xlen: u16, bytes: u16, address: &str) -> String {
        if let Self::Datatype(state) = self {
            return format!("(read_mem_{bytes} {state} {address})");
        }
        let memory = self.field("mem");
        (0..bytes)
            .rev()
            .map(|offset| {
                let slot = if offset == 0 {
                    address.to_string()
                } else {
                    format!("(bvadd {address} (_ bv{offset} {xlen}))")
                };
                format!("(select {memory} {slot})")
            })
            .reduce(|high, low| format!("(concat {high} {low})"))
            .expect("memory access has at least one byte")
    }

    fn sc_success(self, address: &str) -> String {
        format!(
            "(and {} (= {} {address}))",
            self.field("resv"),
            self.field("resa")
        )
    }
}

struct SmtSymbolResolver<'a> {
    symbols: HashMap<u32, SmtSymbolInfo>,
    operands: &'a HashMap<String, Type>,
    /// Let-bound variables (exception payloads), shadowing operands.
    locals: &'a HashMap<String, SmtVal>,
    state: SmtStateRef<'a>,
    ctx: &'a SmtCtx<'a>,
}

impl SmtSymbolResolver<'_> {
    fn resolve(&self, symbol_id: u32) -> Option<SmtVal> {
        let symbol = self.symbols.get(&symbol_id)?;
        let ctx = self.ctx;

        match symbol {
            SmtSymbolInfo::Register { class, number } => {
                let class = class.to_lowercase();
                if ctx.pc_classes.contains(&class) {
                    Some(SmtVal::bv(self.state.pc(), ctx.xlen as u32, false))
                } else {
                    Some(SmtVal::bv(
                        self.state.read_register(
                            ctx,
                            &class,
                            &format!("(_ bv{} {})", number, ctx.idx_width(&class)),
                        ),
                        ctx.val_width(&class) as u32,
                        false,
                    ))
                }
            }
            SmtSymbolInfo::Variable { name } if self.locals.contains_key(name) => {
                Some(self.locals[name].clone())
            }
            SmtSymbolInfo::RegNum { name } => match self.operands.get(name)? {
                Type::Struct(rc) => Some(SmtVal::bv(
                    name.to_lowercase(),
                    ctx.idx_width(rc) as u32,
                    false,
                )),
                _ => None,
            },
            SmtSymbolInfo::Variable { name } => match self.operands.get(name)? {
                Type::Struct(rc) => {
                    let rc = rc.to_lowercase();
                    if ctx.pc_classes.contains(&rc) {
                        Some(SmtVal::bv(self.state.pc(), ctx.xlen as u32, false))
                    } else {
                        Some(SmtVal::bv(
                            self.state.read_register(ctx, &rc, &name.to_lowercase()),
                            ctx.val_width(&rc) as u32,
                            false,
                        ))
                    }
                }
                // Immediate operands are passed as zero-extended XLEN-wide
                // function parameters; their semantic width is the declared
                // field width, which `sext`/`zext` in behaviors rely on.
                Type::Bits(n) => {
                    let n = (*n as u32).min(ctx.xlen as u32);
                    if n == ctx.xlen as u32 {
                        Some(SmtVal::bv(name.to_lowercase(), n, false))
                    } else {
                        Some(SmtVal::bv(
                            format!("((_ extract {} 0) {})", n - 1, name.to_lowercase()),
                            n,
                            false,
                        ))
                    }
                }
                Type::Integer => Some(SmtVal::bv(name.to_lowercase(), ctx.xlen as u32, false)),
                _ => None,
            },
        }
    }
}

/// Evaluate a symbol-free subtree to a constant, mirroring the interpreter's
/// width rules. Width expressions like `log2Ceil(self.XLEN) - 1` reach the
/// emitter unfolded, so structural `Constant` matching is not enough.
fn eval_const_subtree(
    graph: &impl Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    node: NodeId,
) -> Option<(u64, u32)> {
    use tir::sem::{SymKind, SymPayload};

    let child = |idx: usize| eval_const_subtree(graph, graph.children(node).nth(idx)?);
    let arith = |f: fn(u64, u64) -> u64| -> Option<(u64, u32)> {
        let (a, wa) = child(0)?;
        let (b, wb) = child(1)?;
        let w = wa.max(wb);
        let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
        Some((f(a, b) & mask, w))
    };

    match graph.get_node(node) {
        SymKind::Constant => match graph.get_leaf_data(node)? {
            SymPayload::Int(i) => Some((i.to_u64(), i.width())),
            _ => None,
        },
        SymKind::Add => arith(u64::wrapping_add),
        SymKind::Sub => arith(u64::wrapping_sub),
        SymKind::Mul => arith(u64::wrapping_mul),
        SymKind::Log2Ceil => {
            let (v, w) = child(0)?;
            let result = if v <= 1 {
                0u64
            } else {
                64 - (v - 1).leading_zeros() as u64
            };
            Some((result, w))
        }
        _ => None,
    }
}

/// Store-conditional success: a valid reservation covering exactly `addr`.
/// Shared by the `bits<1>` value facet and the memory-write effect, so both
/// gate on the identical predicate.
fn sc_success(state: &str, addr: &str) -> String {
    format!("(and (resv {state}) (= (resa {state}) {addr}))")
}

/// The AMO result word for op code 0..8 (per the design document) at the
/// access width, over the old memory value and the operand.
fn amo_combine(op: u8, old: &str, val: &str) -> Option<String> {
    Some(match op {
        0 => format!("(bvadd {old} {val})"),
        1 => val.to_string(),
        2 => format!("(bvxor {old} {val})"),
        3 => format!("(bvand {old} {val})"),
        4 => format!("(bvor {old} {val})"),
        5 => format!("(ite (bvslt {old} {val}) {old} {val})"),
        6 => format!("(ite (bvsgt {old} {val}) {old} {val})"),
        7 => format!("(ite (bvult {old} {val}) {old} {val})"),
        8 => format!("(ite (bvugt {old} {val}) {old} {val})"),
        _ => return None,
    })
}

fn emit_sem_expr(
    graph: &impl Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    node: NodeId,
    resolver: &SmtSymbolResolver<'_>,
) -> Option<SmtVal> {
    use tir::sem::{SymKind, SymPayload};

    let child_node = |idx: usize| graph.children(node).nth(idx);
    let child = |idx: usize| emit_sem_expr(graph, child_node(idx)?, resolver);
    let const_child =
        |idx: usize| -> Option<u64> { Some(eval_const_subtree(graph, child_node(idx)?)?.0) };
    // Result signedness `signed && signed` mirrors `APInt` binary ops.
    let arith = |op: &str| -> Option<SmtVal> {
        let (a, b, w, sa, sb) = coerce_smt(&child(0)?, &child(1)?);
        Some(SmtVal::bv(format!("({} {} {})", op, a, b), w, sa && sb))
    };
    let cmp = |op: &str| -> Option<SmtVal> {
        let (a, b, _, _, _) = coerce_smt(&child(0)?, &child(1)?);
        Some(SmtVal::boolean(format!("({} {} {})", op, a, b)))
    };
    // Result width is the left operand's width; the amount is reinterpreted at
    // that width, matching the interpreter's `amount.to_u64()`.
    let shift = |op: &str, signed: fn(bool) -> bool| -> Option<SmtVal> {
        let (lhs, wl, sl) = child(0)?.as_bv();
        let (amt, wamt, samt) = child(1)?.as_bv();
        let amt = if wamt > wl {
            format!("((_ extract {} 0) {})", wl - 1, amt)
        } else {
            widen_smt(&amt, wamt, samt, wl)
        };
        Some(SmtVal::bv(
            format!("({} {} {})", op, lhs, amt),
            wl,
            signed(sl),
        ))
    };
    // `(read_mem_N st addr)` at the entry state, shared by plain loads,
    // load-reserved, and the old-value facet of an atomic RMW.
    let read_mem = |addr_idx: usize, bytes_idx: usize| -> Option<SmtVal> {
        let (addr, w, s) = child(addr_idx)?.as_bv();
        let bytes = const_child(bytes_idx)? as u16;
        if !MEM_ACCESS_BYTES.contains(&bytes) {
            return None;
        }
        let xlen = resolver.ctx.xlen as u32;
        Some(SmtVal::bv(
            resolver
                .state
                .read_memory(resolver.ctx.xlen, bytes, &fit_smt(&addr, w, s, xlen)),
            bytes as u32 * 8,
            false,
        ))
    };

    if let Some(op) = tir::sem::scalar_op(*graph.get_node(node)) {
        return match op.smt {
            tir::sem::SmtTemplate::Binary(name) => arith(name),
            tir::sem::SmtTemplate::Compare(name) => cmp(name),
            tir::sem::SmtTemplate::Shift(name) => match op.kind {
                tir::sem::SymKind::ShiftRightArithmetic => shift(name, |_| true),
                tir::sem::SymKind::ShiftRightLogic => shift(name, |_| false),
                _ => shift(name, |signed| signed),
            },
            tir::sem::SmtTemplate::Unary(name) => {
                let (value, width, signed) = child(0)?.as_bv();
                Some(SmtVal::bv(format!("({name} {value})"), width, signed))
            }
            tir::sem::SmtTemplate::Concat => {
                let (high, high_width, _) = child(0)?.as_bv();
                let (low, low_width, _) = child(1)?.as_bv();
                Some(SmtVal::bv(
                    format!("(concat {high} {low})"),
                    high_width + low_width,
                    false,
                ))
            }
        };
    }

    match graph.get_node(node) {
        SymKind::Symbol => match graph.get_leaf_data(node)? {
            SymPayload::SymbolId(id) => resolver.resolve(*id),
            _ => None,
        },
        SymKind::Constant => match graph.get_leaf_data(node)? {
            SymPayload::Int(i) => {
                let w = i.width();
                let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
                Some(SmtVal::bv(
                    format!("(_ bv{} {})", i.to_u64() & mask, w),
                    w,
                    i.is_signed(),
                ))
            }
            _ => None,
        },
        SymKind::If => {
            let cond = child(0)?.as_bool();
            let (t, e, w, st, se) = coerce_smt(&child(1)?, &child(2)?);
            Some(SmtVal::bv(
                format!("(ite {} {} {})", cond, t, e),
                w,
                st || se,
            ))
        }
        SymKind::ZExt => {
            let (e, w, _) = child(0)?.as_bv();
            let target = const_child(1)? as u32;
            if target < w {
                return None;
            }
            Some(SmtVal::bv(
                widen_smt(&e, w, false, target),
                target.max(w),
                false,
            ))
        }
        SymKind::SExt => {
            let (e, w, _) = child(0)?.as_bv();
            let target = const_child(1)? as u32;
            if target < w {
                return None;
            }
            Some(SmtVal::bv(
                widen_smt(&e, w, true, target),
                target.max(w),
                true,
            ))
        }
        SymKind::Bitcast => child(0),
        SymKind::Extract => {
            let (e, w, _) = child(0)?.as_bv();
            let high = const_child(1)? as u32;
            let low = const_child(2)? as u32;
            if high < low {
                return None;
            }
            let mul = child_node(0)?;
            if low >= w && matches!(graph.get_node(mul), SymKind::Mul) {
                // `extract(a * b, 2N-1, N)` is the TMDL idiom for the high half
                // of a full multiply (e.g. RISC-V `mulh`); the interpreter
                // recomputes it as a signed full-width product.
                let m0 = emit_sem_expr(graph, graph.children(mul).next()?, resolver)?;
                let m1 = emit_sem_expr(graph, graph.children(mul).nth(1)?, resolver)?;
                let (a, b, wm, _, _) = coerce_smt(&m0, &m1);
                if high >= 2 * wm {
                    return None;
                }
                Some(SmtVal::bv(
                    format!(
                        "((_ extract {} {}) (bvmul ((_ sign_extend {}) {}) ((_ sign_extend {}) {})))",
                        high, low, wm, a, wm, b
                    ),
                    high - low + 1,
                    false,
                ))
            } else if high < w {
                Some(SmtVal::bv(
                    format!("((_ extract {} {}) {})", high, low, e),
                    high - low + 1,
                    false,
                ))
            } else {
                None
            }
        }
        SymKind::Log2Ceil => {
            let (v, w) = eval_const_subtree(graph, node)?;
            Some(SmtVal::bv(format!("(_ bv{} {})", v, w), w, false))
        }
        SymKind::Clamp => {
            let input = child(0)?;
            let (_, _, signed) = input.as_bv();
            let (lt, gt) = if signed {
                ("bvslt", "bvsgt")
            } else {
                ("bvult", "bvugt")
            };
            let (i1, min, w1, _, _) = coerce_smt(&input, &child(1)?);
            let (i2, max, w2, _, _) = coerce_smt(&input, &child(2)?);
            let w = w1.max(w2);
            let (i1, min, i2, max) = (
                widen_smt(&i1, w1, signed, w),
                widen_smt(&min, w1, false, w),
                widen_smt(&i2, w2, signed, w),
                widen_smt(&max, w2, false, w),
            );
            Some(SmtVal::bv(
                format!(
                    "(ite ({} {} {}) {} (ite ({} {} {}) {} {}))",
                    lt, i1, min, min, gt, i2, max, max, i1
                ),
                w,
                signed,
            ))
        }
        SymKind::LoadMemory => read_mem(0, 1),
        // Stores are effect statements, handled by `BehaviorEmitter::store`.
        SymKind::StoreMemory | SymKind::Sqrt | SymKind::Fma => None,
        // IEEE float arithmetic has no bit-vector model here.
        SymKind::FAdd
        | SymKind::FSub
        | SymKind::FMul
        | SymKind::FDiv
        | SymKind::SIToFP
        | SymKind::UIToFP => None,
        SymKind::FPToSI | SymKind::FPToUI => None,
        SymKind::Map | SymKind::Zip | SymKind::IterConcat => None,
        SymKind::Split | SymKind::Reduce | SymKind::Arg => None,
        // Load-reserved reads memory (the reservation is a state effect, set by
        // `BehaviorEmitter`); the atomic RMW's value facet is the OLD word.
        SymKind::LoadReserved => read_mem(0, 1),
        SymKind::AtomicRmw => read_mem(1, 2),
        // Store-conditional's `bits<1>` value is its success predicate.
        SymKind::StoreConditional => {
            let (addr, w, s) = child(0)?.as_bv();
            let addr = fit_smt(&addr, w, s, resolver.ctx.xlen as u32);
            Some(SmtVal::boolean(resolver.state.sc_success(&addr)))
        }
        // Fence is a statement-only effect (identity), handled by the emitter.
        SymKind::Fence => None,
        _ => unreachable!("operator has no SMT template"),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MemOpKind {
    Load,
    Store,
}

/// The single atomic call (LR/SC/AMO) inside a behavior expression. sema
/// guarantees at most one per statement, so the first found is the only one.
enum AtomicOp {
    LoadReserved {
        addr: NodeId,
    },
    StoreConditional {
        addr: NodeId,
        bytes: u64,
        value: NodeId,
    },
    AtomicRmw {
        op: u8,
        addr: NodeId,
        bytes: u64,
        value: NodeId,
    },
}

fn atomic_of_node(
    graph: &impl Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    node: NodeId,
) -> Option<AtomicOp> {
    let children = graph.children(node).collect::<Vec<_>>();
    let constant = |index: usize| eval_const_subtree(graph, *children.get(index)?).map(|v| v.0);
    match graph.get_node(node) {
        tir::sem::SymKind::LoadReserved => Some(AtomicOp::LoadReserved {
            addr: *children.first()?,
        }),
        tir::sem::SymKind::StoreConditional => Some(AtomicOp::StoreConditional {
            addr: *children.first()?,
            bytes: constant(1)?,
            value: *children.get(2)?,
        }),
        tir::sem::SymKind::AtomicRmw => Some(AtomicOp::AtomicRmw {
            op: constant(0)? as u8,
            addr: *children.get(1)?,
            bytes: constant(2)?,
            value: *children.get(3)?,
        }),
        _ => None,
    }
}

/// The atomic call within `e`, descending through pure wrappers
/// (`sext`/`zext`/`extract`/`if`/...) as sema permits.
fn find_atomic(
    graph: &impl Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    node: NodeId,
) -> Option<AtomicOp> {
    if let Some(op) = atomic_of_node(graph, node) {
        return Some(op);
    }
    graph
        .children(node)
        .find_map(|child| find_atomic(graph, child))
}

/// Statement emitter folding a behavior into a TMDLState transition.
struct BehaviorEmitter<'a> {
    ctx: &'a SmtCtx<'a>,
    operands: &'a HashMap<String, Type>,
    behavior: &'a sem_expr_state::BehaviorGraph,
    /// Exception payloads visible while a handler body is compiled.
    locals: std::cell::RefCell<HashMap<String, SmtVal>>,
    /// Uniquifies exception-payload `let` bindings across nested trys.
    let_counter: std::cell::Cell<usize>,
    /// Handler PC writes are trap entries, not architectural branches: they
    /// must not flip the instruction's `writes-pc` metadata.
    in_handler: std::cell::Cell<bool>,
    failed: std::cell::Cell<bool>,
    writes_pc: std::cell::Cell<bool>,
    write_classes: std::cell::RefCell<BTreeSet<String>>,
    pc_value_roots: std::cell::RefCell<Vec<NodeId>>,
}

impl BehaviorEmitter<'_> {
    fn emit_val(&self, root: NodeId) -> Option<SmtVal> {
        self.emit_val_in(root, SmtStateRef::Datatype("st"))
    }

    fn emit_val_in(&self, root: NodeId, state: SmtStateRef<'_>) -> Option<SmtVal> {
        let mut symbols = HashMap::new();
        for (name, id) in &self.behavior.variable_symbols {
            symbols.insert(*id, SmtSymbolInfo::Variable { name: name.clone() });
        }
        for ((class, number), id) in &self.behavior.register_symbols {
            symbols.insert(
                *id,
                SmtSymbolInfo::Register {
                    class: class.clone(),
                    number: *number,
                },
            );
        }
        for (name, id) in &self.behavior.regnum_symbols {
            symbols.insert(*id, SmtSymbolInfo::RegNum { name: name.clone() });
        }
        let locals = self.locals.borrow();
        let resolver = SmtSymbolResolver {
            symbols,
            operands: self.operands,
            locals: &locals,
            // Shared behavior lowering substitutes prior named assignments
            // into later expressions. Remaining symbols are entry snapshots,
            // preserving distinct operands that alias one physical register.
            state,
            ctx: self.ctx,
        };
        let (values, root) = self.behavior.value_graph(root)?;
        emit_sem_expr(&values, root, &resolver).or_else(|| {
            self.failed.set(true);
            None
        })
    }

    /// Wrap the register-write state `w` (or the bare entry state for a
    /// discarded `store_conditional`) with an atomic's memory/reservation
    /// effect. Symbol reads use the sequenced expression graph over entry
    /// snapshots, so the success predicate matches the value facet of the RHS.
    fn atomic_effect(&self, op: &AtomicOp, w: &str) -> Option<String> {
        let xlen = self.ctx.xlen as u32;
        let addr = |a: NodeId| -> Option<String> {
            let (e, wa, sa) = self.emit_val(a)?.as_bv();
            Some(fit_smt(&e, wa, sa, xlen))
        };
        match op {
            AtomicOp::LoadReserved { addr: a } => Some(format!("(set_res {} {})", w, addr(*a)?)),
            AtomicOp::StoreConditional {
                addr: a,
                bytes,
                value,
            } => {
                if !MEM_ACCESS_BYTES.contains(&(*bytes as u16)) {
                    return None;
                }
                let addr = addr(*a)?;
                let (v, wv, sv) = self.emit_val(*value)?.as_bv();
                let val = fit_smt(&v, wv, sv, *bytes as u32 * 8);
                Some(format!(
                    "(ite {succ} (write_mem_{bytes} (clear_res {w}) {addr} {val}) (clear_res {w}))",
                    succ = sc_success("st", &addr)
                ))
            }
            AtomicOp::AtomicRmw {
                op,
                addr: a,
                bytes,
                value,
            } => {
                if !MEM_ACCESS_BYTES.contains(&(*bytes as u16)) {
                    return None;
                }
                let width = *bytes as u32 * 8;
                let addr = addr(*a)?;
                let old = format!("(read_mem_{} st {})", bytes, addr);
                let (v, wv, sv) = self.emit_val(*value)?.as_bv();
                let val = fit_smt(&v, wv, sv, width);
                let new = amo_combine(*op, &old, &val)?;
                Some(format!("(write_mem_{} {} {} {})", bytes, w, addr, new))
            }
        }
    }
}

impl sem_expr_state::BehaviorEmitter for BehaviorEmitter<'_> {
    type State = String;

    fn assign(
        &self,
        destination: &sem_expr_state::Destination,
        value: NodeId,
        st_name: &String,
    ) -> Option<String> {
        let ctx = self.ctx;
        let rhs = self.emit_val(value)?;
        let (expr, width, signed) = rhs.as_bv();
        let fit = |target: u16| fit_smt(&expr, width, signed, target as u32);
        let write_pc = || {
            if !self.in_handler.get() {
                self.writes_pc.set(true);
                self.pc_value_roots.borrow_mut().push(value);
            }
            format!("(write_pc {} {})", st_name, fit(ctx.xlen))
        };
        // An atomic RHS threads its memory/reservation effect around the
        // register write (`w`); a plain assignment is just `w`.
        let wrap = |w: String| match self
            .behavior
            .value_graph(value)
            .and_then(|(g, r)| find_atomic(&g, r))
        {
            Some(op) => self.atomic_effect(&op, &w),
            None => Some(w),
        };
        let dest_name = match destination {
            sem_expr_state::Destination::Ident(name) => Some(name.as_str()),
            sem_expr_state::Destination::Path { members, .. } if members.len() == 1 => {
                Some(members[0].as_str())
            }
            sem_expr_state::Destination::FixedRegister { name, .. } => Some(name.as_str()),
            _ => None,
        };
        if dest_name == Some("pc") {
            return wrap(write_pc());
        }
        if let Some(name) = dest_name {
            match self.operands.get(name) {
                Some(Type::Struct(rc)) if ctx.pc_classes.contains(&rc.to_lowercase()) => {
                    return wrap(write_pc());
                }
                Some(Type::Struct(rc)) => {
                    self.write_classes.borrow_mut().insert(rc.to_lowercase());
                    return wrap(format!(
                        "(write_{} {} {} {})",
                        rc.to_lowercase(),
                        st_name,
                        name.to_lowercase(),
                        fit(ctx.val_width(rc))
                    ));
                }
                _ => {}
            }
        }
        // Writes to a fixed register named by class path (`GPR::x30`,
        // `PSTATE::n`).
        if let sem_expr_state::Destination::FixedRegister { class, index, .. } = destination {
            let class = class.to_lowercase();
            self.write_classes.borrow_mut().insert(class.clone());
            return wrap(format!(
                "(write_{} {} (_ bv{} {}) {})",
                class,
                st_name,
                index,
                ctx.idx_width(&class),
                fit(ctx.val_width(&class))
            ));
        }
        None
    }

    fn value_effect(
        &self,
        kind: tir::sem::SymKind,
        value: NodeId,
        st_name: &String,
    ) -> Option<String> {
        if kind == tir::sem::SymKind::StateFence {
            return Some(st_name.clone());
        }
        if kind == tir::sem::SymKind::StateStoreConditional {
            let (values, root) = self.behavior.value_graph(value)?;
            return self.atomic_effect(&atomic_of_node(&values, root)?, st_name);
        }
        let children = self.behavior.graph.children(value).collect::<Vec<_>>();
        let (byte_values, byte_root) = self.behavior.value_graph(*children.get(1)?)?;
        let bytes = eval_const_subtree(&byte_values, byte_root)?.0 as u16;
        if !MEM_ACCESS_BYTES.contains(&bytes) {
            return None;
        }
        let (addr, wa, sa) = self.emit_val(*children.first()?)?.as_bv();
        let (val, wv, sv) = self.emit_val(*children.get(2)?)?.as_bv();
        Some(format!(
            "(write_mem_{bytes} {st_name} {} {})",
            fit_smt(&addr, wa, sa, self.ctx.xlen as u32),
            fit_smt(&val, wv, sv, bytes as u32 * 8)
        ))
    }

    fn trap(
        &self,
        arguments: &[NodeId],
        params: &[String],
        handler: Option<NodeId>,
        st_name: &String,
        compile: &dyn Fn(NodeId, &String) -> String,
    ) -> Option<String> {
        let xlen = self.ctx.xlen as u32;
        // Bind handler parameters to the call arguments; missing trailing
        // arguments (ecall has no tval) read as zero.
        let mut shadowed = Vec::new();
        for (i, param) in params.iter().enumerate() {
            let value = match arguments.get(i) {
                Some(arg) => self.emit_val(*arg)?,
                None => SmtVal::bv(format!("(_ bv0 {})", xlen), xlen, false),
            };
            shadowed.push((
                param.clone(),
                self.locals.borrow_mut().insert(param.clone(), value),
            ));
        }
        let state = compile(handler?, st_name);
        for (param, previous) in shadowed {
            let mut locals = self.locals.borrow_mut();
            match previous {
                Some(value) => locals.insert(param, value),
                None => locals.remove(&param),
            };
        }
        Some(state)
    }

    fn branch(
        &self,
        condition: NodeId,
        _entry: &String,
        then_state: &String,
        else_state: &String,
    ) -> String {
        let cond = self
            .emit_val(condition)
            .map(|value| value.as_bool())
            .unwrap_or_else(|| "false".to_string());
        format!("(ite {cond} {then_state} {else_state})")
    }

    fn try_except(
        &self,
        body: NodeId,
        handlers: &[NodeId],
        st_name: &String,
        compile: &dyn Fn(NodeId, &String) -> String,
    ) -> Option<String> {
        let operations = effect_memory_operations(self.behavior, body)?;
        if operations.len() > 1 {
            return None;
        }
        let operation = operations.first();
        let xlen = self.ctx.xlen;
        let variable = format!("exc_addr{}", self.let_counter.get());
        let mut arms = Vec::new();
        for &handler in handlers {
            let Some(sem_expr_state::EffectPayload::Handler { kind, binding }) =
                self.behavior.effect_payload(handler)
            else {
                return None;
            };
            let wanted = match kind.as_str() {
                "misaligned_load" => MemOpKind::Load,
                "misaligned_store" => MemOpKind::Store,
                _ => return None,
            };
            let Some(operation) = operation.filter(|operation| operation.kind == wanted) else {
                continue;
            };
            if operation.bytes <= 1 {
                continue;
            }
            if !operation.bytes.is_power_of_two() {
                return None;
            }
            let condition = format!(
                "(distinct (bvand {variable} (_ bv{} {xlen})) (_ bv0 {xlen}))",
                operation.bytes - 1
            );
            if let Some(binding) = binding {
                self.locals.borrow_mut().insert(
                    binding.clone(),
                    SmtVal::bv(variable.clone(), xlen as u32, false),
                );
            }
            let child = self.behavior.graph.children(handler).next()?;
            let was_in_handler = self.in_handler.replace(true);
            let handler_state = compile(child, st_name);
            self.in_handler.set(was_in_handler);
            if let Some(binding) = binding {
                self.locals.borrow_mut().remove(binding);
            }
            arms.push((condition, handler_state));
        }
        let body_state = compile(body, st_name);
        if arms.is_empty() {
            return Some(body_state);
        }
        let operation = operation?;
        let (address, width, signed) = self.emit_val(operation.addr)?.as_bv();
        let address = fit_smt(&address, width, signed, xlen as u32);
        self.let_counter.set(self.let_counter.get() + 1);
        let folded = arms
            .into_iter()
            .rev()
            .fold(body_state, |otherwise, (condition, handler)| {
                format!("(ite {condition} {handler} {otherwise})")
            });
        Some(format!("(let (({variable} {address})) {folded})"))
    }

    fn unsupported(&self) {
        self.failed.set(true);
    }
}

struct FlatBehaviorEmitter<'a> {
    values: BehaviorEmitter<'a>,
    initial: FlatState,
}

impl FlatBehaviorEmitter<'_> {
    fn emit_val(&self, expression: NodeId) -> Option<SmtVal> {
        self.values
            .emit_val_in(expression, SmtStateRef::Flat(&self.initial))
    }
}

impl sem_expr_state::BehaviorEmitter for FlatBehaviorEmitter<'_> {
    type State = FlatState;

    fn assign(
        &self,
        destination: &sem_expr_state::Destination,
        value: NodeId,
        state: &FlatState,
    ) -> Option<FlatState> {
        if self
            .values
            .behavior
            .value_graph(value)
            .and_then(|(g, r)| find_atomic(&g, r))
            .is_some()
        {
            return None;
        }
        let ctx = self.values.ctx;
        let (expression, width, signed) = self.emit_val(value)?.as_bv();
        let fit = |target: u16| fit_smt(&expression, width, signed, target as u32);
        let destination_name = match destination {
            sem_expr_state::Destination::Ident(name) => Some(name.as_str()),
            sem_expr_state::Destination::Path { members, .. } if members.len() == 1 => {
                Some(members[0].as_str())
            }
            sem_expr_state::Destination::FixedRegister { name, .. } => Some(name.as_str()),
            _ => None,
        };
        if destination_name == Some("pc") {
            let mut next = state.clone();
            next.fields.insert("pc".to_string(), fit(ctx.xlen));
            return Some(next);
        }
        if let Some(name) = destination_name {
            match self.values.operands.get(name) {
                Some(Type::Struct(class)) if ctx.pc_classes.contains(&class.to_lowercase()) => {
                    let mut next = state.clone();
                    next.fields.insert("pc".to_string(), fit(ctx.xlen));
                    return Some(next);
                }
                Some(Type::Struct(class)) => {
                    let class = class.to_lowercase();
                    return Some(state.write_register(
                        ctx,
                        &class,
                        &name.to_lowercase(),
                        &fit(ctx.val_width(&class)),
                    ));
                }
                _ => {}
            }
        }
        if let sem_expr_state::Destination::FixedRegister { class, index, .. } = destination {
            let class = class.to_lowercase();
            return Some(state.write_register(
                ctx,
                &class,
                &format!("(_ bv{} {})", index, ctx.idx_width(&class)),
                &fit(ctx.val_width(&class)),
            ));
        }
        None
    }

    fn value_effect(
        &self,
        kind: tir::sem::SymKind,
        value: NodeId,
        state: &FlatState,
    ) -> Option<FlatState> {
        if kind == tir::sem::SymKind::StateFence {
            return Some(state.clone());
        }
        if kind == tir::sem::SymKind::StateStoreConditional {
            return None;
        }
        let children = self
            .values
            .behavior
            .graph
            .children(value)
            .collect::<Vec<_>>();
        let (byte_values, byte_root) = self.values.behavior.value_graph(*children.get(1)?)?;
        let bytes = eval_const_subtree(&byte_values, byte_root)?.0 as u16;
        if !MEM_ACCESS_BYTES.contains(&bytes) {
            return None;
        }
        let xlen = self.values.ctx.xlen;
        let (address, address_width, address_signed) = self.emit_val(*children.first()?)?.as_bv();
        let (value, value_width, value_signed) = self.emit_val(*children.get(2)?)?.as_bv();
        Some(state.write_memory(
            xlen,
            bytes,
            &fit_smt(&address, address_width, address_signed, u32::from(xlen)),
            &fit_smt(&value, value_width, value_signed, u32::from(bytes) * 8),
        ))
    }

    fn trap(
        &self,
        arguments: &[NodeId],
        params: &[String],
        handler: Option<NodeId>,
        state: &FlatState,
        compile: &dyn Fn(NodeId, &FlatState) -> FlatState,
    ) -> Option<FlatState> {
        let xlen = u32::from(self.values.ctx.xlen);
        let mut shadowed = Vec::new();
        for (index, parameter) in params.iter().enumerate() {
            let value = match arguments.get(index) {
                Some(argument) => self.emit_val(*argument)?,
                None => SmtVal::bv(format!("(_ bv0 {xlen})"), xlen, false),
            };
            shadowed.push((
                parameter.clone(),
                self.values
                    .locals
                    .borrow_mut()
                    .insert(parameter.clone(), value),
            ));
        }
        let result = compile(handler?, state);
        for (parameter, previous) in shadowed {
            let mut locals = self.values.locals.borrow_mut();
            match previous {
                Some(value) => locals.insert(parameter, value),
                None => locals.remove(&parameter),
            };
        }
        Some(result)
    }

    fn branch(
        &self,
        condition: NodeId,
        _entry: &FlatState,
        then_state: &FlatState,
        else_state: &FlatState,
    ) -> FlatState {
        let condition = self
            .emit_val(condition)
            .map(|value| value.as_bool())
            .unwrap_or_else(|| "false".to_string());
        FlatState::select(&condition, then_state, else_state)
    }

    fn try_except(
        &self,
        body: NodeId,
        handlers: &[NodeId],
        state: &FlatState,
        compile: &dyn Fn(NodeId, &FlatState) -> FlatState,
    ) -> Option<FlatState> {
        let operations = effect_memory_operations(self.values.behavior, body)?;
        if operations.len() > 1 {
            return None;
        }
        let operation = operations.first();
        let xlen = self.values.ctx.xlen;
        let variable = format!("exc_addr{}", self.values.let_counter.get());
        let mut arms = Vec::new();
        for &handler in handlers {
            let Some(sem_expr_state::EffectPayload::Handler { kind, binding }) =
                self.values.behavior.effect_payload(handler)
            else {
                return None;
            };
            let wanted = match kind.as_str() {
                "misaligned_load" => MemOpKind::Load,
                "misaligned_store" => MemOpKind::Store,
                _ => return None,
            };
            let Some(operation) = operation.filter(|operation| operation.kind == wanted) else {
                continue;
            };
            if operation.bytes <= 1 {
                continue;
            }
            if !operation.bytes.is_power_of_two() {
                return None;
            }
            let condition = format!(
                "(distinct (bvand {variable} (_ bv{} {xlen})) (_ bv0 {xlen}))",
                operation.bytes - 1
            );
            if let Some(binding) = binding {
                self.values.locals.borrow_mut().insert(
                    binding.clone(),
                    SmtVal::bv(variable.clone(), u32::from(xlen), false),
                );
            }
            let child = self.values.behavior.graph.children(handler).next()?;
            let handler_state = compile(child, state);
            if let Some(binding) = binding {
                self.values.locals.borrow_mut().remove(binding);
            }
            arms.push((condition, handler_state));
        }
        let mut result = compile(body, state);
        if arms.is_empty() {
            return Some(result);
        }
        let operation = operation?;
        let (address, width, signed) = self.emit_val(operation.addr)?.as_bv();
        let address = fit_smt(&address, width, signed, u32::from(xlen));
        self.values
            .let_counter
            .set(self.values.let_counter.get() + 1);
        for (condition, handler) in arms.into_iter().rev() {
            result = FlatState::select(&condition, &handler, &result);
        }
        for expression in result.fields.values_mut() {
            *expression = format!("(let (({variable} {address})) {expression})");
        }
        Some(result)
    }

    fn unsupported(&self) {
        self.values.failed.set(true);
    }
}

/// Translate an instruction behavior into an SMT state-transition expression.
/// Returns `None` when the behavior uses constructs the SMT model cannot
/// express (e.g. `trap()`); callers must not pretend such instructions have
/// identity semantics.
struct BehaviorMetadata {
    body: String,
    writes_pc: bool,
    write_classes: Vec<String>,
    pc_source_names: BTreeSet<String>,
    memory_accesses: Vec<MemoryAccessMetadata>,
    uses_reservation: bool,
    trap_kinds: Vec<String>,
    flat_execute: Option<BTreeMap<String, String>>,
}

struct GraphMemOp {
    kind: MemOpKind,
    addr: NodeId,
    bytes: u64,
    reservation: bool,
}

fn graph_memory_operations(behavior: &sem_expr_state::BehaviorGraph) -> Option<Vec<GraphMemOp>> {
    let mut operations = Vec::new();
    let nodes = behavior
        .value_roots()
        .into_iter()
        .flat_map(|root| behavior.graph.preorder(root))
        .collect::<std::collections::HashSet<_>>();
    for node in nodes {
        let children = behavior.graph.children(node).collect::<Vec<_>>();
        let (kind, address, bytes, reservation) = match behavior.graph.get_node(node) {
            tir::sem::SymKind::LoadMemory => (MemOpKind::Load, 0, 1, false),
            tir::sem::SymKind::LoadReserved => (MemOpKind::Load, 0, 1, true),
            tir::sem::SymKind::StoreMemory => (MemOpKind::Store, 0, 1, false),
            tir::sem::SymKind::StoreConditional => (MemOpKind::Store, 0, 1, true),
            tir::sem::SymKind::AtomicRmw => (MemOpKind::Store, 1, 2, true),
            _ => continue,
        };
        operations.push(GraphMemOp {
            kind,
            addr: *children.get(address)?,
            bytes: {
                let (values, root) = behavior.value_graph(*children.get(bytes)?)?;
                eval_const_subtree(&values, root)?.0
            },
            reservation,
        });
    }
    Some(operations)
}

fn effect_memory_operations(
    behavior: &sem_expr_state::BehaviorGraph,
    effect: NodeId,
) -> Option<Vec<GraphMemOp>> {
    let mut value_nodes = std::collections::HashSet::new();
    for node in behavior.graph.preorder(effect) {
        let children: Vec<_> = behavior.graph.children(node).collect();
        let roots: Vec<NodeId> = match behavior.graph.get_node(node) {
            tir::sem::SymKind::StateAssign
            | tir::sem::SymKind::StateStore
            | tir::sem::SymKind::StateStoreConditional
            | tir::sem::SymKind::StateFence
            | tir::sem::SymKind::StateIf => children.into_iter().take(1).collect(),
            tir::sem::SymKind::StateTrap => match behavior.effect_payload(node) {
                Some(sem_expr_state::EffectPayload::Trap { argument_count, .. }) => {
                    children.into_iter().take(*argument_count).collect()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        for root in roots {
            value_nodes.extend(behavior.graph.preorder(root));
        }
    }
    let mut operations = Vec::new();
    for node in value_nodes {
        let children = behavior.graph.children(node).collect::<Vec<_>>();
        let (kind, address, bytes, reservation) = match behavior.graph.get_node(node) {
            tir::sem::SymKind::LoadMemory => (MemOpKind::Load, 0, 1, false),
            tir::sem::SymKind::LoadReserved => (MemOpKind::Load, 0, 1, true),
            tir::sem::SymKind::StoreMemory => (MemOpKind::Store, 0, 1, false),
            tir::sem::SymKind::StoreConditional => (MemOpKind::Store, 0, 1, true),
            tir::sem::SymKind::AtomicRmw => (MemOpKind::Store, 1, 2, true),
            _ => continue,
        };
        operations.push(GraphMemOp {
            kind,
            addr: *children.get(address)?,
            bytes: {
                let (values, root) = behavior.value_graph(*children.get(bytes)?)?;
                eval_const_subtree(&values, root)?.0
            },
            reservation,
        });
    }
    Some(operations)
}

fn graph_trap_kinds(behavior: &sem_expr_state::BehaviorGraph) -> BTreeSet<String> {
    behavior
        .effect_nodes()
        .filter_map(|node| match behavior.effect_payload(node) {
            Some(sem_expr_state::EffectPayload::Handler { kind, .. }) => Some(kind.clone()),
            _ => None,
        })
        .collect()
}

fn build_smt_behavior<'a>(
    ctx: &SmtCtx<'_>,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    instruction: &'a ast::Instruction,
    operands: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<BehaviorMetadata> {
    let operands = operands.iter().cloned().collect::<HashMap<_, _>>();
    let mut numeric_params: HashMap<String, i64> =
        resolve_isa_param_values(instruction, item_cache);
    // The target ISA's own values win over the cross-ISA maximum (an
    // instruction shared by RV32I and RV64I must see XLEN=32 on RV32I).
    numeric_params.extend(ctx.isa_params.iter().map(|(k, v)| (k.clone(), *v)));
    numeric_params.extend(
        resolve_params_for_instruction(instruction, item_cache)
            .into_iter()
            .filter_map(|(name, (_ty, val))| match val {
                Some(ast::Expr::Lit(ast::Lit::Int(li))) => {
                    Some((name, parse_literal_value_u128(&li) as i64))
                }
                _ => None,
            }),
    );
    let behavior_graph = sem_expr_state::lower_behavior(
        &instruction.behavior,
        ctx.trap_handler,
        &numeric_params,
        &ctx.isa_params,
        register_index_map,
    )?;

    let emitter = BehaviorEmitter {
        ctx,
        operands: &operands,
        behavior: &behavior_graph,
        locals: Default::default(),
        let_counter: Default::default(),
        in_handler: Default::default(),
        failed: Default::default(),
        writes_pc: Default::default(),
        write_classes: Default::default(),
        pc_value_roots: Default::default(),
    };
    let body = sem_expr_state::fold_behavior(&behavior_graph, &"st".to_string(), &emitter);
    let mem_ops = graph_memory_operations(&behavior_graph)?;
    let uses_reservation = mem_ops.iter().any(|op| op.reservation);
    let trap_kinds = graph_trap_kinds(&behavior_graph);
    let initial_flat_state = FlatState::initial(ctx);
    let flat_emitter = FlatBehaviorEmitter {
        values: BehaviorEmitter {
            ctx,
            operands: &operands,
            behavior: &behavior_graph,
            locals: Default::default(),
            let_counter: Default::default(),
            in_handler: Default::default(),
            failed: Default::default(),
            writes_pc: Default::default(),
            write_classes: Default::default(),
            pc_value_roots: Default::default(),
        },
        initial: initial_flat_state.clone(),
    };
    let flat_state =
        sem_expr_state::fold_behavior(&behavior_graph, &initial_flat_state, &flat_emitter);
    let flat_execute = (!flat_emitter.values.failed.get()).then_some(flat_state.fields);
    let memory_accesses = mem_ops
        .iter()
        .map(|operation| {
            let (address, width, signed) = emitter.emit_val(operation.addr)?.as_bv();
            let (flat_address, flat_width, flat_signed) =
                flat_emitter.emit_val(operation.addr)?.as_bv();
            Some(MemoryAccessMetadata {
                kind: match operation.kind {
                    MemOpKind::Load => "load",
                    MemOpKind::Store => "store",
                },
                bytes: operation.bytes,
                address: fit_smt(&address, width, signed, u32::from(ctx.xlen)),
                flat_address: fit_smt(&flat_address, flat_width, flat_signed, u32::from(ctx.xlen)),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if emitter.failed.get() {
        None
    } else {
        Some(BehaviorMetadata {
            body,
            writes_pc: emitter.writes_pc.get(),
            write_classes: emitter.write_classes.into_inner().into_iter().collect(),
            pc_source_names: behavior_graph
                .variable_symbols
                .iter()
                .filter(|(_, symbol)| {
                    emitter.pc_value_roots.borrow().iter().any(|root| {
                        behavior_graph.graph.preorder(*root).any(|node| {
                            matches!(
                                behavior_graph.graph.get_leaf_data(node),
                                Some(sem_expr_state::BehaviorPayload::Value(tir::sem::SymPayload::SymbolId(id))) if *id == **symbol
                            )
                        })
                    })
                })
                .map(|(name, _)| name.to_lowercase())
                .collect(),
            memory_accesses,
            uses_reservation,
            trap_kinds: trap_kinds.into_iter().collect(),
            flat_execute,
        })
    }
}

// ---------------------------------------------------------------------------
// Decoder (instruction word → TMDLInstr)
// ---------------------------------------------------------------------------

fn build_decoder<'a>(
    dialect: &str,
    ctx: &SmtCtx<'_>,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    files: &'a [ast::File],
    output: &mut Box<dyn Write>,
) -> Result<(), TMDLError> {
    let instructions: Vec<&ast::Instruction> = files
        .iter()
        .flat_map(|f| f.instructions())
        .filter(|i| item_supports_isa(&i.for_isas, ctx.isa, item_cache))
        .collect();
    if instructions.is_empty() {
        return Ok(());
    }

    let mut arms: Vec<(String, String)> = vec![];

    for i in &instructions {
        let name_upper = i.name.to_uppercase();
        let operand_list = resolved_operands(ctx, i, item_cache);
        let operands: HashMap<String, Type> = operand_list.iter().cloned().collect();
        let params = resolve_params_for_instruction(i, item_cache);
        let encoding_arms = get_encoding_arms(i, item_cache);

        // For each operand: collect (op_lo, op_hi, word_lo, word_hi) pieces.
        let mut operand_pieces: HashMap<String, Vec<(u16, u16, u16, u16)>> = HashMap::new();
        let mut guards: Vec<String> = vec![];

        for arm in &encoding_arms {
            let word_lo = arm.start;
            let word_hi = arm.end.unwrap_or(arm.start);
            let word_width = word_hi - word_lo + 1;

            match &arm.value {
                ast::Expr::Lit(ast::Lit::Int(li)) => {
                    let val = parse_literal_value_u128(li);
                    guards.push(format!(
                        "(= ((_ extract {} {}) word) (_ bv{} {}))",
                        word_hi, word_lo, val, word_width
                    ));
                }
                ast::Expr::Ident(id) => {
                    let name = &id.name;
                    if operands.contains_key(name) {
                        // The entire word field holds bits [0..word_width-1] of the operand.
                        operand_pieces.entry(name.clone()).or_default().push((
                            0,
                            word_width - 1,
                            word_lo,
                            word_hi,
                        ));
                    } else if let Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) =
                        params.get(name)
                    {
                        let val = parse_literal_value_u128(li);
                        guards.push(format!(
                            "(= ((_ extract {} {}) word) (_ bv{} {}))",
                            word_hi, word_lo, val, word_width
                        ));
                    }
                    // Unresolved param: no guard emitted (treated as don't-care).
                }
                ast::Expr::Slice(s) => {
                    if let ast::Expr::Ident(id) = &*s.base
                        && operands.contains_key(&id.name)
                    {
                        operand_pieces
                            .entry(id.name.clone())
                            .or_default()
                            .push((s.start, s.end, word_lo, word_hi));
                    }
                }
                ast::Expr::IndexAccess(s) => {
                    if let ast::Expr::Ident(id) = &*s.base
                        && operands.contains_key(&id.name)
                    {
                        operand_pieces
                            .entry(id.name.clone())
                            .or_default()
                            .push((s.index, s.index, word_lo, word_hi));
                    }
                }
                _ => {}
            }
        }

        let guard = match guards.len() {
            0 => "true".to_string(),
            1 => guards.remove(0),
            _ => format!("(and {})", guards.join(" ")),
        };

        // Build the constructor arguments in operand declaration order.
        let constructor_args: Vec<String> = operand_list
            .iter()
            .map(|(op_name, op_ty)| {
                let target_width = match op_ty {
                    Type::Struct(rc) => ctx.idx_width(rc),
                    _ => ctx.xlen,
                };

                let Some(mut pieces) = operand_pieces.remove(op_name) else {
                    return zero_bv(target_width);
                };

                // Sort pieces by op_hi descending so the concat builds high→low.
                pieces.sort_by_key(|piece| std::cmp::Reverse(piece.1));

                // Reconstruct the operand from its pieces, filling any gaps
                // between non-contiguous slices with zero bits.
                // `expected_hi` tracks the next op bit we expect; it starts at
                // the top bit of the highest piece and steps downward.
                let mut fragments: Vec<String> = vec![];
                let mut raw_width: u16 = 0;
                let mut expected_hi = pieces[0].1;

                for (op_lo, op_hi, word_lo, word_hi) in &pieces {
                    // Fill any gap between the previous piece and this one.
                    if *op_hi < expected_hi {
                        let gap = expected_hi - op_hi; // bits [expected_hi..op_hi+1]
                        fragments.push(zero_bv(gap));
                        raw_width += gap;
                    }
                    fragments.push(format!("((_ extract {} {}) word)", word_hi, word_lo));
                    raw_width += op_hi - op_lo + 1;
                    expected_hi = op_lo.saturating_sub(1);
                }
                // Fill any gap below the lowest piece (bits [op_lo-1..0]).
                let lowest_op_lo = pieces.last().map(|(lo, _, _, _)| *lo).unwrap_or(0);
                if lowest_op_lo > 0 {
                    fragments.push(zero_bv(lowest_op_lo));
                    raw_width += lowest_op_lo;
                }

                let raw = fragments
                    .into_iter()
                    .reduce(|acc, f| format!("(concat {} {})", acc, f))
                    .unwrap_or_else(|| zero_bv(target_width));

                cast_bv_smt(&raw, raw_width, target_width)
            })
            .collect();

        let constructor = if constructor_args.is_empty() {
            name_upper.clone()
        } else {
            format!("({name_upper} {})", constructor_args.join(" "))
        };
        arms.push((guard, constructor));
    }

    // Build a fallback: the first instruction with all-zero operands.
    let first = &instructions[0];
    let first_ops = resolved_operands(ctx, first, item_cache);
    let fallback = {
        let zeros: Vec<String> = first_ops
            .iter()
            .map(|(_, ty)| {
                zero_bv(match ty {
                    Type::Struct(rc) => ctx.idx_width(rc),
                    _ => ctx.xlen,
                })
            })
            .collect();
        if zeros.is_empty() {
            first.name.to_uppercase()
        } else {
            format!("({} {})", first.name.to_uppercase(), zeros.join(" "))
        }
    };

    // Fold arms into nested ites, first arm wins.
    let body = arms
        .iter()
        .rev()
        .fold(fallback, |else_branch, (guard, then_branch)| {
            format!("(ite {}\n    {}\n    {})", guard, then_branch, else_branch)
        });

    let word_width = ctx.word_width;
    writeln!(
        output,
        "\n(define-fun decode_{dialect} ((word (_ BitVec {word_width}))) TMDLInstr\n  {})",
        body
    )?;

    writeln!(
        output,
        "\n(define-fun execute_by_word_{dialect} ((state TMDLState) (word (_ BitVec {word_width}))) TMDLState\n  (execute_{dialect} state (decode_{dialect} word)))"
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Bitvector rendering helpers
// ---------------------------------------------------------------------------

fn render_lit_bitvec(width: u16, lit: &ast::LitInt) -> String {
    let value = parse_literal_value_u128(lit);
    format!("(_ bv{} {})", value, width)
}

fn zero_bv(width: u16) -> String {
    format!("(_ bv0 {})", width)
}

/// SMT-lib needs the full u128 range for large bitvector literals.
fn parse_literal_value_u128(lit: &ast::LitInt) -> u128 {
    let v = lit.value();
    if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16).unwrap_or(0)
    } else if let Some(bin) = v.strip_prefix("0b") {
        u128::from_str_radix(bin, 2).unwrap_or(0)
    } else {
        v.parse::<u128>().unwrap_or(0)
    }
}

fn cast_bv(name: &str, from_width: u16, to_width: u16) -> String {
    cast_bv_smt(name, from_width, to_width)
}

/// Like `cast_bv` but accepts an arbitrary SMT-LIB expression instead of a
/// plain identifier.  When `from_width == to_width` the expression is returned
/// as-is; otherwise it is wrapped in `zero_extend` or `extract`.
fn cast_bv_smt(expr: &str, from_width: u16, to_width: u16) -> String {
    match from_width.cmp(&to_width) {
        std::cmp::Ordering::Equal => expr.to_string(),
        std::cmp::Ordering::Less => {
            format!("((_ zero_extend {}) {})", to_width - from_width, expr)
        }
        std::cmp::Ordering::Greater => {
            format!("((_ extract {} 0) {})", to_width - 1, expr)
        }
    }
}

// AUFDTBV: Arrays, Uninterpreted Functions, Datatypes (for TMDLInstr),
// BitVectors.  Use ALL as an alias that Z3 and CVC5 both accept.
const HEADER: &str = "; Automatically generated by TMDL compiler\n(set-logic ALL)\n";
