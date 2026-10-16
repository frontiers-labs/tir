//! Target data layout and ABI properties used by C semantic analysis.

use super::IntegerKind;
use tir::TypeArg;
use tir::backend::abi::{ArgumentGroupAlignment, ClassifierKind, Overflow, ValueKind};
use tir::layout::{LayoutSpec, TypeEntry};

// The psABI rule for `_BitInt(N)` as a layout spec entry, in bits: the size
// and alignment of the smallest standard integer that holds `N`, and past the
// widest alignment, `max_align`, an array of chunks that wide. The core
// evaluates it, as it does every entry of a spec.
fn bit_int_rule(context: &tir::Context, max_align: u64) -> LayoutSpec {
    let terms = context.terms();
    let function = |name, args| {
        let ctor = terms.ctor_named("nat", name).expect("a layout function");
        TypeArg::Type(terms.intern(ctor, args).expect("a width term"))
    };
    let width = TypeArg::Var(0);
    let pow2 = function("pow2_ceil", vec![width.clone()]);
    let align = function(
        "min",
        vec![
            function("max", vec![pow2, TypeArg::Nat(8)]),
            TypeArg::Nat(max_align),
        ],
    );
    let key = terms
        .intern(terms.ctor(terms.int(8)), vec![width.clone()])
        .expect("the _BitInt pattern");
    LayoutSpec::new().with_entry(TypeEntry {
        key,
        vars: vec!["W".to_string()],
        size: function("align_to", vec![width, align.clone()]),
        abi: align,
        preferred: None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetProfile {
    // Width of a pointer, in bits, as the target's data layout declares it.
    pointer_width: u32,
    pub(super) plain_char_signed: bool,
    abi_classifier: ClassifierKind,
    integer_argument_registers: usize,
    float_argument_registers: usize,
    float_argument_overflow: Overflow,
    argument_group_alignment: Option<ArgumentGroupAlignment>,
    indirect_result_argument_slots: Option<(ValueKind, usize)>,
}

impl TargetProfile {
    /// Derive the C profile from what the target itself declares: the pointer
    /// width from its data layout, the rest from its ABI. fcc keeps no table
    /// of its own, so every spelling the backend accepts works here too.
    pub fn for_target(
        context: &tir::Context,
        march: &str,
        machine: &dyn tir::backend::TargetMachine,
    ) -> Result<Self, String> {
        let layout = machine
            .data_layout(context)
            .as_ref()
            .and_then(|layout| tir::DataLayout::from_value(context, layout))
            .ok_or_else(|| format!("target '{march}' declares no data layout"))?;
        let pointer_width = layout
            .pointer_size()
            .ok_or_else(|| format!("target '{march}' declares no pointer layout"))?;
        let abi = machine.abi();
        let float_arguments = abi
            .args
            .iter()
            .find(|sequence| sequence.kind == ValueKind::Float);
        Ok(Self {
            pointer_width,
            plain_char_signed: abi.classifier != ClassifierKind::Aapcs64,
            abi_classifier: abi.classifier,
            integer_argument_registers: abi
                .args
                .iter()
                .find(|sequence| sequence.kind == ValueKind::Int)
                .map_or(0, |sequence| sequence.regs.len()),
            float_argument_registers: float_arguments.map_or(0, |sequence| sequence.regs.len()),
            float_argument_overflow: float_arguments
                .map_or(Overflow::Chain(ValueKind::Int), |sequence| {
                    sequence.overflow
                }),
            argument_group_alignment: abi.argument_group_alignment,
            indirect_result_argument_slots: abi.indirect_result_argument_slots(),
        })
    }

    pub fn pointer_width(self) -> u32 {
        self.pointer_width
    }

    // The type `long double` is laid out as: x87 extended precision under
    // the SysV ABI, IEEE binary128 under the others. Either takes 16 bytes
    // aligned to 16.
    pub(crate) fn long_double(self, context: &tir::Context) -> tir::TypeId {
        match self.uses_sysv_abi() {
            true => context.terms().float(15, 64),
            false => context.terms().float(15, 112),
        }
    }

    /// The target's layout spec with the C types it does not name itself:
    /// `_BitInt` and `long double`.
    pub fn layout_spec(
        self,
        context: &tir::Context,
        machine: &dyn tir::backend::TargetMachine,
    ) -> Option<tir::attributes::AttributeValue> {
        let tir::attributes::AttributeValue::LayoutSpec(spec) = machine.data_layout(context)?
        else {
            return None;
        };
        let mut spec = (*spec).clone();
        // AAPCS64 aligns a `_BitInt` wider than 64 bits to 16 bytes; every
        // other ABI here takes the x86-64 psABI's 8.
        let bit_int_align = if self.uses_aapcs64_abi() { 128 } else { 64 };
        spec.merge(&bit_int_rule(context, bit_int_align));
        let long_double = self.long_double(context);
        spec.merge(&LayoutSpec::new().with_type(long_double, 128, 128));
        Some(tir::attributes::AttributeValue::LayoutSpec(spec.into()))
    }

    pub(super) fn pointer_sized_integer(self) -> IntegerKind {
        if self.pointer_width() == 32 {
            IntegerKind::Int
        } else {
            IntegerKind::Long
        }
    }

    pub fn integer_width(self, kind: IntegerKind) -> u32 {
        match kind {
            IntegerKind::Bool
            | IntegerKind::Char
            | IntegerKind::SignedChar
            | IntegerKind::UnsignedChar => 8,
            IntegerKind::Short | IntegerKind::UnsignedShort => 16,
            IntegerKind::Int | IntegerKind::UnsignedInt => 32,
            IntegerKind::Long | IntegerKind::UnsignedLong => self.pointer_width(),
            IntegerKind::LongLong | IntegerKind::UnsignedLongLong => 64,
            IntegerKind::BitInt { width, .. } => width,
        }
    }

    pub(crate) fn uses_riscv_hard_float_abi(self) -> bool {
        self.abi_classifier == ClassifierKind::Riscv && self.float_argument_registers > 0
    }

    pub(crate) fn uses_riscv_abi(self) -> bool {
        self.abi_classifier == ClassifierKind::Riscv
    }

    pub(crate) fn uses_aapcs64_abi(self) -> bool {
        self.abi_classifier == ClassifierKind::Aapcs64
    }

    pub(crate) fn uses_sysv_abi(self) -> bool {
        self.abi_classifier == ClassifierKind::Sysv
    }

    pub(crate) fn argument_registers(self, kind: ValueKind) -> usize {
        match kind {
            ValueKind::Int => self.integer_argument_registers,
            ValueKind::Float => self.float_argument_registers,
            ValueKind::Vector => 0,
        }
    }

    pub(crate) fn float_argument_overflow(self) -> Overflow {
        self.float_argument_overflow
    }

    pub(crate) fn align_argument_slot(
        self,
        kind: ValueKind,
        source_alignment: u64,
        slot: usize,
    ) -> usize {
        self.argument_group_alignment.map_or(slot, |alignment| {
            alignment.align_slot(kind, source_alignment, slot)
        })
    }

    pub(crate) fn indirect_result_argument_slots(self) -> Option<(ValueKind, usize)> {
        self.indirect_result_argument_slots
    }
}
