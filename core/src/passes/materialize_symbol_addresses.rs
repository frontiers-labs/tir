//! Rewrites a function's uses of module-level λ and δ values into `sym_addr`
//! ops inside the function.
//!
//! The mid-end reasons about globals and functions as values, but machine code
//! cannot reach a value defined outside the function it runs in: an address has
//! to be materialized where it is used. A call keeps its callee operand and
//! records the symbol it resolved to, which is what call lowering emits a
//! direct call to.
//!
//! This runs while the whole module is still in hand: functions are lowered and
//! erased one at a time, and a λ whose definition is gone no longer names
//! anything.

use crate::analysis::AnalysisManager;
use crate::attributes::AttributeValue;
use crate::builtin::{FnToPtrOp, ops as b};
use crate::func::{CallOp, FuncOp};
use crate::{
    Context, OpHandle, OpId, Operation, OperationRef, Pass, PassError, PassTarget, Rewriter,
    Symbol, ValueId,
};

#[derive(Default)]
pub struct MaterializeSymbolAddressesPass;

impl MaterializeSymbolAddressesPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(
    MaterializeSymbolAddressesPass,
    "materialize-symbol-addresses"
);

impl Pass for MaterializeSymbolAddressesPass {
    fn name(&self) -> &'static str {
        "materialize-symbol-addresses"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        operation: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let mut uses = Vec::new();
        collect(context, operation.op(), &mut uses);
        for use_site in uses {
            match use_site {
                Use::Address(op, index, name) => {
                    let address = b::symbol_address(context, &name);
                    let value = address.result();
                    match context.parent_nodes_region(op.op().id) {
                        Some(region) => context.add(region, address.id()),
                        None => rewriter.insert_op_before(&op, &address)?,
                    }
                    context.set_op_operand(op.op().id, index, value);
                }
                Use::Conversion(op, name) => {
                    let address = b::symbol_address(context, &name);
                    match context.parent_nodes_region(op.op().id) {
                        Some(region) => {
                            context.add(region, address.id());
                            let old = op.op().results()[0];
                            context.replace_value_uses(old, address.result());
                            context.rename_region_results(region, old, address.result(), &[]);
                            rewriter.erase_op(&op)?;
                        }
                        None => rewriter.replace_op(&op, &address)?,
                    }
                }
                Use::Callee(op, name) => {
                    let mut attributes = op.op().attributes().to_vec();
                    attributes
                        .push(context.named_attribute("callee", AttributeValue::Str(name.into())));
                    context.set_op_attributes(op.op().id, attributes);
                }
            }
        }
        Ok(())
    }
}

enum Use {
    /// An operand naming a δ definition, and the symbol it names.
    Address(OperationRef, usize, String),
    /// A `fn_to_ptr` of a λ, and the symbol it names.
    Conversion(OperationRef, String),
    /// A call whose callee resolved to a λ, and the symbol it names.
    Callee(OperationRef, String),
}

fn collect(context: &Context, root: &OpHandle, uses: &mut Vec<Use>) {
    for region in root.regions() {
        {
            for op_id in context.get_region(region).op_ids() {
                let instance = context.get_op(op_id);
                let op = OperationRef::new(instance.clone());
                collect(context, op.op(), uses);
                if let Some(name) = instance
                    .clone()
                    .as_op::<FnToPtrOp>()
                    .and_then(|conversion| symbol_of(context, conversion.operands()[0]))
                {
                    uses.push(Use::Conversion(op, name));
                    continue;
                }
                if let Some(name) = instance
                    .clone()
                    .as_op::<CallOp>()
                    .and_then(|call| symbol_of(context, call.callee()))
                {
                    uses.push(Use::Callee(op.clone(), name));
                }
                for (index, &operand) in instance.value_operands().iter().enumerate() {
                    // Only a δ's address has a machine form here. A λ reaching
                    // anything but a call or `fn_to_ptr` is left for selection
                    // to reject rather than silently retyped.
                    if !is_pointer(context, operand) {
                        continue;
                    }
                    if let Some(name) = symbol_of(context, operand) {
                        uses.push(Use::Address(op.clone(), index, name));
                    }
                }
            }
        }
    }
}

fn is_pointer(context: &Context, value: ValueId) -> bool {
    let ty = context.get_type_data(context.get_value(value).ty());
    (ty.as_ref() as &dyn std::any::Any)
        .downcast_ref::<crate::ptr::PtrType>()
        .is_some()
}

/// The symbol `value` is the definition of, when it is one.
fn symbol_of(context: &Context, value: ValueId) -> Option<String> {
    let definition: OpId = context.get_value(value).defining_op()?;
    if !context.has_operation(definition) {
        return None;
    }
    let instance = context.get_op(definition);
    let symbol = instance.as_interface::<dyn Symbol>()?;
    Some(symbol.symbol_name())
}
