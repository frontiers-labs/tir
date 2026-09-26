//! Target data layout and ABI properties used by C semantic analysis.

use super::{IntegerKind, TypeKind};
use tir::backend::abi::{
    ArgumentGroupAlignment, ArgumentGroupPolicy, ClassifierKind, GroupRollback, Overflow, ValueKind,
};

/// The TIR layout classes fcc's scalar C types map onto. Which class a C type
/// takes is C's rule; how wide and how aligned that class is, is the target's
/// to declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayoutClass {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    Pointer,
}

impl LayoutClass {
    const ALL: [Self; 7] = [
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::F32,
        Self::F64,
        Self::Pointer,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Pointer => "p",
        }
    }

    fn of_width(bits: u32) -> Self {
        match bits {
            8 => Self::I8,
            16 => Self::I16,
            32 => Self::I32,
            _ => Self::I64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetProfile {
    /// Size and ABI alignment of each [`LayoutClass`], in bytes, as the
    /// target's data layout declares them.
    scalars: [(u64, u64); LayoutClass::ALL.len()],
    pub(super) plain_char_signed: bool,
    abi_classifier: ClassifierKind,
    integer_argument_registers: usize,
    float_argument_registers: usize,
    float_argument_overflow: Overflow,
    argument_group_alignment: Option<ArgumentGroupAlignment>,
    argument_group_policy: Option<ArgumentGroupPolicy>,
    indirect_result_argument_slots: Option<(ValueKind, usize)>,
}

impl TargetProfile {
    pub fn for_march(march: &str) -> Result<Self, String> {
        let target = tir::backend::select_target(march, None, None)?;
        Self::for_target(march, target.as_ref())
    }

    /// Derive the C profile from what the target itself declares: scalar sizes
    /// and alignments from its data layout, the rest from its ABI. fcc keeps no
    /// table of its own, so every spelling the backend accepts works here too.
    pub(crate) fn for_target(
        march: &str,
        machine: &dyn tir::backend::TargetMachine,
    ) -> Result<Self, String> {
        let layout = machine
            .data_layout()
            .as_ref()
            .and_then(tir::DataLayout::from_value)
            .ok_or_else(|| format!("target '{march}' declares no data layout"))?;
        let mut scalars = [(0, 0); LayoutClass::ALL.len()];
        for class in LayoutClass::ALL {
            let (size, align) = layout
                .class_layout(class.key())
                .ok_or_else(|| format!("target '{march}' declares no '{}' layout", class.key()))?;
            scalars[class as usize] = (u64::from(size / 8), u64::from(align / 8));
        }
        let abi = machine.abi();
        let float_arguments = abi
            .args
            .iter()
            .find(|sequence| sequence.kind == ValueKind::Float);
        Ok(Self {
            scalars,
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
            argument_group_policy: abi.argument_group_policy,
            indirect_result_argument_slots: abi.indirect_result_argument_slots(),
        })
    }

    pub fn host() -> Result<Self, String> {
        Self::for_march(std::env::consts::ARCH)
    }

    pub fn pointer_width(self) -> u32 {
        self.class_layout(LayoutClass::Pointer).0 as u32 * 8
    }

    /// Size and ABI alignment of a layout class, in bytes.
    pub(crate) fn class_layout(self, class: LayoutClass) -> (u64, u64) {
        self.scalars[class as usize]
    }

    /// Size and ABI alignment of a scalar C type, in bytes. Aggregates have no
    /// layout class of their own and are laid out from their members instead.
    pub(crate) fn scalar_layout(self, kind: &TypeKind) -> Option<(u64, u64)> {
        let class = match kind {
            TypeKind::Integer(kind) => LayoutClass::of_width(self.integer_width(*kind)),
            TypeKind::Enum(_) => LayoutClass::I32,
            TypeKind::Float => LayoutClass::F32,
            TypeKind::Double => LayoutClass::F64,
            TypeKind::ComplexFloat => {
                let (size, align) = self.class_layout(LayoutClass::F32);
                return Some((2 * size, align));
            }
            TypeKind::ComplexDouble => {
                let (size, align) = self.class_layout(LayoutClass::F64);
                return Some((2 * size, align));
            }
            TypeKind::ComplexLongDouble => return Some((32, 16)),
            TypeKind::VaList => return Some((24, 8)),
            // `long double` maps onto no TIR layout class: fcc carries it as an
            // opaque object rather than an arithmetic type, so its ABI size and
            // alignment have nowhere in the data layout to come from.
            TypeKind::LongDouble => return Some((16, 16)),
            TypeKind::Pointer(_) => LayoutClass::Pointer,
            _ => return None,
        };
        Some(self.class_layout(class))
    }

    /// The C integer kind of a pointer difference, and of an object size. A
    /// 32-bit layout ranks them as `int`, every wider one as `long`.
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

    pub(crate) fn argument_group_fits_register_limit(self, members: usize) -> bool {
        self.argument_group_policy
            .is_none_or(|policy| policy.fits_register_limit(members))
    }

    pub(crate) fn argument_group_rollback(self) -> GroupRollback {
        self.argument_group_policy
            .map_or(GroupRollback::Exhaust, |policy| policy.rollback)
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
