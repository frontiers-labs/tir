//! x86-64 backend prototype, generated from the TMDL descriptions in `defs/`.

include!(concat!(env!("OUT_DIR"), "/model_check_sources.rs"));

pub use isa::{
    Feature, get_isel_rules, instruction_infos, register_info, register_views, register_widths,
};
pub use isa::{TargetConfig, X86_64Dialect};

mod isa {
    // Generated code: not everything is used by this asm-focused prototype.
    #![allow(dead_code, unused_variables, unused_mut, clippy::all)]

    use tir::Operation;
    use tir::attributes::AttributeValue;
    use tir::backend::{RegSlot, fresh_reg, phys_attr};
    use tir::backend::{VirtualBranchOp, VirtualCallOp, VirtualIndirectCallOp, VirtualReturnOp};
    use tir::helpers::{dialect, operation};

    include!(concat!(env!("OUT_DIR"), "/x86_64.rs"));

    dialect! {
        X86_64Dialect {
            name: "x86_64",
            operation_file: concat!(env!("OUT_DIR"), "/x86_64_ops.rs"),
            type_parsers: reg_class_type_parsers(),
        }
    }

    fn lower_func_and_return_to_asm_symbol(
        context: &tir::Context,
        op: &tir::OperationRef,
    ) -> Result<bool, tir::PassError> {
        tir::backend::lower::lower_function_and_return(context, op, |ty| {
            let ty = context.get_type_data(ty);
            Ok(
                if (ty.as_ref() as &dyn std::any::Any)
                    .downcast_ref::<tir::builtin::FloatType>()
                    .is_some()
                {
                    RegClass::XMM.id()
                } else {
                    RegClass::GPR.id()
                },
            )
        })
    }

    /// Pre-RA: materialize a `constantf` that survived instruction selection
    /// into `movabs r64, bits` + `movq xmm, r64`.
    fn lower_float_constant(
        context: &tir::Context,
        op: &tir::OperationRef,
    ) -> Result<bool, tir::PassError> {
        use tir::builtin::ConstantFOp;

        let Some(constant) = op.as_op::<ConstantFOp>() else {
            return Ok(false);
        };
        let Some(bits) = tir::backend::f64_constant_bits(context, &constant) else {
            return Ok(false);
        };
        let temp = fresh_reg(context, RegClass::GPR.id());
        let materialize = MovAbsOpBuilder::new(context)
            .result_values(vec![temp])
            .attr("imm", AttributeValue::Int(bits))
            .build();
        let move_bits = MovqXmmGprOpBuilder::new(context)
            .result_types(vec![tir::backend::RegClassType::new(
                context,
                RegClass::XMMzx.id(),
            )])
            .src(temp)
            .build();
        context.insert_op_before(op, &materialize)?;
        context.replace_op(op, &move_bits)?;
        Ok(true)
    }

    /// Emit the branch-if-nonzero fallback for a condition no branch rule
    /// fused: `test cond, 1` + `jne dest`. The condition is a width-1 value, so
    /// only bit 0 of its register is defined — a whole-register `test` would
    /// branch on undefined bits.
    fn emit_branch_nonzero(
        context: &tir::Context,
        condition: tir::ValueId,
        dest: tir::BlockId,
    ) -> Vec<Box<dyn Operation>> {
        vec![
            Box::new(
                TestImm32OpBuilder::new(context)
                    .dst(condition)
                    .attr("imm", AttributeValue::Int(1))
                    .build(),
            ),
            Box::new(
                JumpNotEqOpBuilder::new(context)
                    .attr("imm", AttributeValue::Block(dest))
                    .build(),
            ),
        ]
    }

    /// Pre-RA: materialize a `constant` that survived instruction selection
    /// (one no instruction folded as an immediate) into `mov rd, imm32`.
    fn lower_constant(
        context: &tir::Context,
        op: &tir::OperationRef,
    ) -> Result<bool, tir::PassError> {
        use tir::builtin::ConstantOp;

        let Some(constant) = op.as_op::<ConstantOp>() else {
            return Ok(false);
        };
        let value = tir::backend::int_attr(&constant, "value").ok_or_else(|| {
            tir::PassError::InvalidRuleSet("constant op without an integer value".to_string())
        })?;
        let dst = vec![tir::backend::RegClassType::new(context, RegClass::GPR.id())];
        if i32::try_from(value).is_err() {
            let movabs = MovAbsOpBuilder::new(context)
                .result_types(dst)
                .attr("imm", AttributeValue::Int(value))
                .build();
            context.replace_op(op, &movabs)?;
            return Ok(true);
        }

        let mov = MovImmOpBuilder::new(context)
            .result_types(dst)
            .attr("imm", AttributeValue::Int(value))
            .build();
        context.replace_op(op, &mov)?;
        Ok(true)
    }

    /// Pre-RA: materialize an `asm.symbol_address` as `lea rd, [rip + sym]`.
    /// The encoder leaves the disp32 as a fixup emitted with R_X86_64_PC32.
    fn lower_symbol_address(
        context: &tir::Context,
        op: &tir::OperationRef,
    ) -> Result<bool, tir::PassError> {
        use tir::backend::SymbolAddressOp;

        let Some(addr_of) = op.as_op::<SymbolAddressOp>() else {
            return Ok(false);
        };
        let lea = LeaRipOpBuilder::new(context)
            .result_types(vec![tir::backend::RegClassType::new(
                context,
                RegClass::GPR.id(),
            )])
            .attr("imm", AttributeValue::Str(addr_of.sym_name().into()))
            .build();
        context.replace_op(op, &lea)?;
        Ok(true)
    }

    /// A register-to-register `mov dst, src`.
    fn mv(context: &tir::Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation> {
        let builder = MovOpBuilder::new(context);
        let builder = tir::reg_use!(builder, src, src);
        Box::new(tir::reg_def!(builder, dst, dst).build())
    }

    fn abi_copy(context: &tir::Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation> {
        let class = tir::backend::slot_class(context, dst).expect("ABI copies target a register");
        match class.name() {
            "GPR" => mv(context, dst, src),
            "XMM" => {
                let builder = MovsdOpBuilder::new(context);
                let builder = tir::reg_use!(builder, src, src);
                Box::new(tir::reg_def!(builder, dst, dst).build())
            }
            other => unreachable!("unknown x86-64 ABI register class {other}"),
        }
    }

    struct X86CallEmitter;

    impl tir::backend::call_lowering::CallEmitter for X86CallEmitter {
        fn copy(&self, context: &tir::Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation> {
            abi_copy(context, dst, src)
        }

        fn stack_arg_store(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            value: tir::ValueId,
            class: tir::backend::regalloc::RegClassId,
            offset: i64,
        ) -> Result<Box<dyn Operation>, tir::PassError> {
            let base = phys_attr(abi.sp);
            let offset = AttributeValue::Int(offset);
            match class.name() {
                "GPR" => Ok(Box::new(
                    MovStoreDispOpBuilder::new(context)
                        .attr("base", base)
                        .attr("imm", offset)
                        .src(value)
                        .build(),
                )),
                "XMM" => Ok(Box::new(
                    MovsdStoreDispOpBuilder::new(context)
                        .attr("base", base)
                        .attr("imm", offset)
                        .src(value)
                        .build(),
                )),
                other => Err(tir::PassError::InvalidRuleSet(format!(
                    "x86-64 stack arguments for register class {other} are not supported"
                ))),
            }
        }

        fn call_prefix(
            &self,
            context: &tir::Context,
            _abi: &tir::backend::abi::AbiInfo,
            _outgoing_size: u32,
            vector_register_args: u8,
        ) -> Vec<Box<dyn Operation>> {
            vec![Box::new(
                MovImm32OpBuilder::new(context)
                    .attr("dst", phys_attr((RegClass::GPR32.id(), 0)))
                    .attr("imm", AttributeValue::Int(i64::from(vector_register_args)))
                    .build(),
            )]
        }

        fn call_suffix(
            &self,
            _context: &tir::Context,
            _abi: &tir::backend::abi::AbiInfo,
            _outgoing_size: u32,
        ) -> Vec<Box<dyn Operation>> {
            Vec::new()
        }
    }

    /// Post-RA: `vret` becomes `ret`; `vbr` becomes `jmp dest`.
    fn finalize_virtual_ops(
        context: &tir::Context,
        op: &tir::OperationRef,
    ) -> Result<bool, tir::PassError> {
        if op.as_op::<VirtualReturnOp>().is_some() {
            let ret = RetOpBuilder::new(context).build();
            context.replace_op(op, &ret)?;
            return Ok(true);
        }

        if let Some(br) = op.as_op::<VirtualBranchOp>() {
            if !br.operands().is_empty() {
                return Err(tir::PassError::InvalidRuleSet(
                    "block arguments on branch edges are not supported by codegen yet".to_string(),
                ));
            }
            let jump = JmpOpBuilder::new(context)
                .attr("imm", AttributeValue::Block(br.dest()))
                .build();
            context.replace_op(op, &jump)?;
            return Ok(true);
        }

        // `vcall callee` becomes `call callee`: the symbol operand survives into
        // the encoder as a fixup, emitted as an R_X86_64_PLT32 relocation since the
        // callee's address is unknown until link time.
        if let Some(call) = op.as_op::<VirtualCallOp>() {
            let real = CallOpBuilder::new(context)
                .attr("imm", AttributeValue::Str(call.callee().into()))
                .build();
            tir::backend::forward_state(context, op.op(), &real);
            context.replace_op(op, &real)?;
            return Ok(true);
        }

        // `vcall_indirect` becomes `call *target`; the target register is the one
        // the allocator gave the call's callee operand.
        if let Some(call) = op.as_op::<VirtualIndirectCallOp>() {
            let target = call.operands().first().copied().ok_or_else(|| {
                tir::PassError::InvalidRuleSet("indirect call has no callee register".to_string())
            })?;
            let real: Box<dyn Operation> =
                Box::new(CallIndirectOpBuilder::new(context).target(target).build());
            tir::backend::forward_state(context, op.op(), real.as_ref());
            context.replace_op(op, real.as_ref())?;
            return Ok(true);
        }

        Ok(false)
    }

    /// The move family a register class is copied and spilled with. A class is a
    /// view over a register file, so the family follows from that view — the file
    /// it draws from, the width of the view and where the view starts — and never
    /// from the class name: two classes over the same file, width and offset
    /// move alike whatever they are called.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum MoveKind {
        Gpr64,
        Gpr32,
        Gpr16,
        Gpr8,
        Gpr8High,
        Xmm64,
        Xmm32,
    }

    /// Register allocation target. Frame adjustment is `add rsp, ±size`; GPR
    /// spills use the displacement-based `mov` memory forms.
    struct X86RegAlloc {
        /// Architectural width per register class under the enabled features
        /// (`GPR` is `XLEN` wide, so this is not a compile-time constant).
        widths: Vec<(&'static str, u32)>,
    }

    impl X86RegAlloc {
        fn new(features: &[Feature]) -> Self {
            Self {
                widths: register_widths(features),
            }
        }

        fn move_kind(&self, class: tir::backend::regalloc::RegClassId) -> Option<MoveKind> {
            let width = self
                .widths
                .iter()
                .find(|(name, _)| *name == class.name())
                .map(|(_, width)| *width)?;
            match (class.file(), width, class.view.bit_offset) {
                ("GPR", 64, 0) => Some(MoveKind::Gpr64),
                ("GPR", 32, 0) => Some(MoveKind::Gpr32),
                ("GPR", 16, 0) => Some(MoveKind::Gpr16),
                ("GPR", 8, 0) => Some(MoveKind::Gpr8),
                ("GPR", 8, 8) => Some(MoveKind::Gpr8High),
                ("XMM128", 64, 0) => Some(MoveKind::Xmm64),
                ("XMM128", 32, 0) => Some(MoveKind::Xmm32),
                _ => None,
            }
        }
    }

    impl tir::backend::regalloc::TargetRegAlloc for X86RegAlloc {
        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            register_info()
        }

        fn emit_spill_store(
            &self,
            context: &tir::Context,
            value: tir::ValueId,
            class: tir::backend::regalloc::RegClassId,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Box<dyn Operation> {
            macro_rules! store {
                ($Builder:ident) => {
                    Box::new(
                        $Builder::new(context)
                            .attr("base", phys_attr(*frame))
                            .attr("imm", AttributeValue::Int(offset))
                            .src(value)
                            .build(),
                    )
                };
            }
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => store!(MovStoreDispOpBuilder),
                Some(MoveKind::Gpr32) => store!(Mov32StoreDispOpBuilder),
                Some(MoveKind::Gpr16) => store!(Mov16StoreDispOpBuilder),
                Some(MoveKind::Gpr8) => store!(Mov8StoreDispOpBuilder),
                Some(MoveKind::Xmm64) => store!(MovsdStoreDispOpBuilder),
                Some(MoveKind::Xmm32) => store!(MovssStoreDispOpBuilder),
                _ => unimplemented!("x86-64 spilling for {} is not implemented", class.name()),
            }
        }

        fn emit_spill_reload(
            &self,
            context: &tir::Context,
            value: tir::ValueId,
            class: tir::backend::regalloc::RegClassId,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Box<dyn Operation> {
            macro_rules! load {
                ($Builder:ident) => {
                    Box::new(
                        $Builder::new(context)
                            .result_values(vec![value])
                            .attr("base", phys_attr(*frame))
                            .attr("imm", AttributeValue::Int(offset))
                            .build(),
                    )
                };
            }
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => load!(MovLoadDispOpBuilder),
                Some(MoveKind::Gpr32) => load!(Mov32LoadDispOpBuilder),
                Some(MoveKind::Gpr16) => load!(Mov16LoadDispOpBuilder),
                Some(MoveKind::Gpr8) => load!(Mov8LoadDispOpBuilder),
                Some(MoveKind::Xmm64) => load!(MovsdLoadDispOpBuilder),
                Some(MoveKind::Xmm32) => load!(MovssLoadDispOpBuilder),
                _ => unimplemented!("x86-64 spilling for {} is not implemented", class.name()),
            }
        }

        fn emit_copy(
            &self,
            context: &tir::Context,
            class: tir::backend::regalloc::RegClassId,
            dst: RegSlot,
            src: RegSlot,
        ) -> Box<dyn Operation> {
            macro_rules! move_op {
                ($Builder:ident) => {{
                    let builder = $Builder::new(context);
                    let builder = tir::reg_use!(builder, src, src);
                    Box::new(tir::reg_def!(builder, dst, dst).build())
                }};
            }
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => move_op!(MovOpBuilder),
                Some(MoveKind::Gpr32) => move_op!(Mov32OpBuilder),
                Some(MoveKind::Gpr16) => move_op!(Mov16OpBuilder),
                Some(MoveKind::Gpr8) => move_op!(Mov8OpBuilder),
                Some(MoveKind::Gpr8High) => move_op!(Mov8HOpBuilder),
                Some(MoveKind::Xmm64) => move_op!(MovsdOpBuilder),
                Some(MoveKind::Xmm32) => move_op!(MovssOpBuilder),
                None => unreachable!("unknown x86-64 register class {}", class.name()),
            }
        }

        fn emit_prologue(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            size: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            let mut ops: Vec<Box<dyn Operation>> = Vec::new();
            for ((class, index), _) in saves {
                ops.push(Box::new(
                    PushOpBuilder::new(context)
                        .attr("reg", phys_attr((*class, *index)))
                        .build(),
                ));
            }
            if size > 0 {
                ops.push(adjust_rsp(context, abi, -(size as i64)));
            }
            ops
        }

        fn emit_epilogue(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            size: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            let mut ops: Vec<Box<dyn Operation>> = Vec::new();
            if size > 0 {
                ops.push(adjust_rsp(context, abi, size as i64));
            }
            for ((class, index), _) in saves.iter().rev() {
                ops.push(Box::new(
                    PopOpBuilder::new(context)
                        .attr("reg", phys_attr((*class, *index)))
                        .build(),
                ));
            }
            ops
        }

        fn emit_frame_address(
            &self,
            context: &tir::Context,
            dst: tir::ValueId,
            class: tir::backend::regalloc::RegClassId,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Result<Vec<Box<dyn Operation>>, tir::PassError> {
            if self.move_kind(class) != Some(MoveKind::Gpr64) {
                return Err(tir::PassError::InvalidRuleSet(format!(
                    "x86-64 stack allocation addresses for register class {} are not supported",
                    class.name()
                )));
            }
            // `lea`, not `mov` plus `add`: a frame address is materialized
            // wherever the value is read, and `add` would leave the flags of
            // whatever compare stands there destroyed. `lea` computes the same
            // address in one instruction and writes no flags.
            if offset == 0 {
                return Ok(vec![mv(
                    context,
                    RegSlot::Value(dst),
                    RegSlot::Phys((frame.0, frame.1)),
                )]);
            }
            Ok(vec![Box::new(
                LeaBaseDispOpBuilder::new(context)
                    .result_values(vec![dst])
                    .attr("base", phys_attr(*frame))
                    .attr("imm", AttributeValue::Int(offset))
                    .build(),
            )])
        }
    }

    fn adjust_rsp(
        context: &tir::Context,
        abi: &tir::backend::abi::AbiInfo,
        amount: i64,
    ) -> Box<dyn Operation> {
        Box::new(
            AddImmOpBuilder::new(context)
                .attr("dst", phys_attr(abi.sp))
                .attr("dst_tied", phys_attr(abi.sp))
                .attr("imm", AttributeValue::Int(amount))
                .build(),
        )
    }

    // R_X86_64_PC32 = 2, R_X86_64_PLT32 = 4. Both scatter `S + A - P` into a
    // 4-byte pc-relative field; the addend is -4 because `P` addresses the field
    // start while the displacement is measured from the instruction's end.
    const R_X86_64_PC32: u32 = 2;
    const R_X86_64_PLT32: u32 = 4;
    const R_X86_64_64: u32 = 1;

    /// The mnemonic an op name encodes. The base-ISA (`_legacy`) forms of the
    /// pc-relative branches share their 64-bit counterpart's encoding, so they
    /// relocate and measure their displacement identically.
    fn branch_mnemonic(op: &str) -> &str {
        op.strip_suffix("_legacy").unwrap_or(op)
    }

    fn object_format() -> tir::backend::binary::ObjectFormatInfo {
        use tir::backend::binary::{EM_X86_64, ElfClass, ObjectFormatInfo, RelocKind};
        ObjectFormatInfo {
            elf_machine: EM_X86_64,
            elf_class: ElfClass::Elf64,
            elf_flags: 0,
            absolute_reloc: |width| (width == 8).then_some(R_X86_64_64),
            reloc_for: |op| match branch_mnemonic(op) {
                // `call rel32`: the disp32 follows the 1-byte opcode.
                "call" => Some(RelocKind {
                    r_type: R_X86_64_PLT32,
                    addend: -4,
                    field_offset: 1,
                }),
                // `lea r64, [rip + disp32]`: the disp32 follows REX, opcode, ModR/M.
                "lea" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 3,
                }),
                // `jmp rel32` (E9 + disp32): the disp32 follows the 1-byte opcode.
                "jmp" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 1,
                }),
                // `jcc rel32` (0F 8x + disp32): the disp32 follows the 2-byte opcode.
                "je" | "jne" | "jl" | "jge" | "jb" | "jae" | "jle" | "jg" | "jbe" | "ja" | "js"
                | "jns" | "jo" | "jno" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 2,
                }),
                _ => None,
            },
            pc_rel_scale: |_| 0,
            // rel32 displacements are measured from the end of the instruction
            // (RIP points past the branch when the displacement applies).
            pc_rel_from_end: |op| {
                matches!(
                    branch_mnemonic(op),
                    "jmp"
                        | "je"
                        | "jne"
                        | "jl"
                        | "jge"
                        | "jb"
                        | "jae"
                        | "jle"
                        | "jg"
                        | "jbe"
                        | "ja"
                        | "js"
                        | "jns"
                        | "jo"
                        | "jno"
                        | "call"
                )
            },
        }
    }

    /// Parsed x86-64 target selection.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TargetConfig {
        features: Vec<Feature>,
        machine: Option<String>,
    }

    impl TargetConfig {
        /// Parse an x86-64 architecture name.
        pub fn parse(march: &str, mcpu: Option<&str>, mattr: Option<&str>) -> Result<Self, String> {
            match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "x86_64" | "amd64" | "x64" => {}
                other => return Err(format!("unknown x86-64 architecture '{other}'")),
            }
            let mut features = vec![Feature::X86, Feature::X86_64, Feature::SSE, Feature::SSE2];
            if let Some(mattr) = mattr {
                tir::backend::apply_mattr(&mut features, mattr, "x86-64", |name| {
                    Feature::from_name(&name.to_ascii_lowercase().replace('-', "_"))
                        .map(|feature| vec![feature])
                })?;
            }
            validate_features(&features)?;
            if !features.contains(&Feature::X86_64) {
                return Err("--mattr must not disable the base ISA 'X86_64'".to_string());
            }
            let machine = match mcpu {
                None => None,
                Some(mcpu) => resolve_machine(mcpu, &features)?,
            };
            Ok(Self { features, machine })
        }

        /// The enabled ISA set.
        pub fn features(&self) -> &[Feature] {
            &self.features
        }

        /// The target name this configuration selects.
        pub fn canonical_name(&self) -> &'static str {
            "x86_64"
        }
    }

    /// Resolve `--mcpu` to the machine model the compiler and the instrument
    /// both schedule against. `generic` selects no machine.
    fn resolve_machine(mcpu: &str, features: &[Feature]) -> Result<Option<String>, String> {
        let name = mcpu.trim().to_ascii_lowercase();
        if name == "generic" {
            return Ok(None);
        }
        if machine_model(&name, features).is_some() {
            return Ok(Some(name));
        }
        Err(format!(
            "unknown x86-64 cpu '{mcpu}' (expected 'generic' or one of: {})",
            machines(Feature::ALL).join(", ")
        ))
    }

    struct X86Target {
        config: TargetConfig,
        selected_abi: &'static tir::backend::abi::AbiInfo,
    }

    fn create_isel_pass_for(
        context: &tir::Context,
        features: &[Feature],
        abi: &'static tir::backend::abi::AbiInfo,
    ) -> tir::backend::isel::InstructionSelectPass {
        tir::backend::isel::InstructionSelectPass::new(get_isel_rules(context, features))
            .with_rules(include_str!("isel.pdl"))
            .with_branch_emitters(tir::backend::isel::BranchEmitters {
                uncond: tir::backend::emit_uncond_branch,
                cond_nonzero: emit_branch_nonzero,
            })
            .with_op_lowering(lower_func_and_return_to_asm_symbol)
            .with_call_lowering(abi, Box::new(X86CallEmitter))
    }

    tir::impl_target_machine! {
        X86Target,
        dialect: X86_64Dialect,
        isa: |_| "X86_64",
        pointer_bits: |_| 64,
        regalloc: |features| X86RegAlloc::new(features),
        abis: x86_64_abis,
        sources: super::MODEL_CHECK_SOURCES,

        fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
            vec![
                Box::new(lower_float_constant),
                Box::new(lower_constant),
                Box::new(lower_symbol_address),
            ]
        }

        fn finalize_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
            vec![Box::new(finalize_virtual_ops)]
        }

        fn register_views(&self) -> Vec<(&'static str, tir::backend::regalloc::RegisterView)> {
            register_views(self.config.features())
        }

        fn object_format(&self) -> Option<tir::backend::binary::ObjectFormatInfo> {
            Some(object_format())
        }
    }

    fn select_x86_64(
        march: &str,
        mcpu: Option<&str>,
        mattr: Option<&str>,
        mabi: Option<&str>,
    ) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
        match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "x86_64" | "amd64" | "x64" => {
                let config = TargetConfig::parse(march, mcpu, mattr)?;
                let selected_abi = match mabi {
                    Some(name) => x86_64_abi_by_name(name).ok_or_else(|| {
                        format!(
                            "unknown ABI '{name}' for x86_64 (available: {})",
                            x86_64_abis()
                                .iter()
                                .map(|abi| abi.name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
                    None => x86_64_default_abi(),
                };
                Ok(Some(Box::new(X86Target {
                    config,
                    selected_abi,
                })))
            }
            _ => Ok(None),
        }
    }

    tir::register_target!(select_x86_64, ["x86_64"]);

    tir::target_abis!(x86_64_abis, x86_64_abi_by_name, |abi| {
        tir::backend::abi::AbiInfo {
            indirect_result: Some((RegClass::GPR.id(), 7)),
            argument_group_policy: Some(tir::backend::abi::ArgumentGroupPolicy {
                register_limit: Some(2),
                rollback: tir::backend::abi::GroupRollback::Preserve,
            }),
            ..*abi
        }
    });

    fn x86_64_default_abi() -> &'static tir::backend::abi::AbiInfo {
        &x86_64_abis()[0]
    }
}
