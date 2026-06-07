use crate::{BlockId, Context, Operation, ValueId};

pub trait Commutative {
    fn is_commutative(&self) -> bool {
        true
    }
    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

pub trait Terminator {
    fn is_terminator(&self) -> bool {
        true
    }

    fn successors(&self) -> Vec<BlockId> {
        Vec::new()
    }

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

pub trait SameOperandType {
    fn verify_interface(
        &self,
        this: &dyn Operation,
        context: &Context,
    ) -> Result<(), crate::Error> {
        if this.operands().is_empty() {
            return Ok(());
        }

        let first_operand = *this.operands().first().unwrap();
        let first_type = context.get_value(first_operand).ty();

        let result = this
            .operands()
            .iter()
            .all(|&operand| context.get_value(operand).ty() == first_type);

        if !result {
            return Err(crate::Error::VerificationError(
                "operand types must be the same".to_string(),
            ));
        }

        Ok(())
    }
}

/// Identifies an operation that creates a memory location eligible for local SSA
/// promotion. Implementations describe the location generically rather than tying
/// mem2reg to a concrete pointer dialect.
pub trait PromotableAllocation {
    /// The SSA value that names the promotable memory location.
    fn promoted_location(&self) -> ValueId;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// Identifies an operation that reads a value from a memory location.
pub trait MemoryRead {
    /// The memory location being read.
    fn read_location(&self) -> ValueId;
    /// The SSA value produced by the read.
    fn read_value(&self) -> ValueId;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// Identifies an operation that writes a value to a memory location.
pub trait MemoryWrite {
    /// The memory location being written.
    fn write_location(&self) -> ValueId;
    /// The SSA value stored into the memory location.
    fn written_value(&self) -> ValueId;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}
