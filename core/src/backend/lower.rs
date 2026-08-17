//! A pass that applies a list of per-op lowering functions, reusing the same
//! [`OpLowering`] shape instruction selection uses for its structural
//! lowerings. Targets contribute lowerings for the virtual ops that survive
//! earlier stages (wide constants before register allocation; `vret`/`vbr`
//! after).

use tir::{AnalysisManager, Context, OperationRef, Pass, PassError, PassTarget, Rewriter, TypeId};

use crate::backend::isel::OpLowering;
use crate::backend::regalloc::RegClassId;

pub fn lower_function_and_return(
    context: &Context,
    op: &OperationRef,
    rewriter: &mut Rewriter,
    argument_class: impl Fn(TypeId) -> Result<RegClassId, PassError>,
) -> Result<bool, PassError> {
    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::builtin::{MakeTupleOp, TupleGetOp, TupleType};
    use tir::func::{FuncOp, ReturnOp};
    use tir::{Operation, Symbol};

    if let Some(func) = op.as_op::<FuncOp>() {
        let body = func.body();
        if body
            .op_ids()
            .last()
            .is_none_or(|id| !context.get_op(*id).is::<super::SymbolEndOp>())
        {
            body.append_op(super::SymbolEndOpBuilder::new(context).build());
        }
        let name = match context.get_op(func.id()).attr("sym_name") {
            Some(AttributeValue::Str(name)) => name.to_string(),
            _ => "unknown".to_string(),
        };
        let def_use = tir::analysis::DefUse::new(context, func.id());
        let mut tuple_extracts = Vec::new();
        let mut arguments = Vec::new();
        let function_arguments = func.body().arguments().to_vec();
        let mut argument_alignments = func.argument_alignments();
        if argument_alignments.is_empty() {
            argument_alignments.resize(function_arguments.len(), 1);
        } else if argument_alignments.len() != function_arguments.len() {
            return Err(PassError::InvalidRuleSet(
                "function argument alignment count does not match its arguments".to_string(),
            ));
        }
        let mut function_arguments = function_arguments.into_iter();
        let mut argument_alignments = argument_alignments.into_iter();
        let result_address = if func.has_result_address() {
            let argument = function_arguments.next().ok_or_else(|| {
                PassError::InvalidRuleSet(
                    "result-address function has no destination argument".to_string(),
                )
            })?;
            argument_alignments.next();
            Some(AttributeValue::Register(RegisterAttr::Virtual {
                id: argument.id().number(),
                class: Some(argument_class(argument.ty())?),
            }))
        } else {
            None
        };
        for (argument, alignment) in function_arguments.zip(argument_alignments) {
            let ty = context.get_type_data(argument.ty());
            let Some(tuple) = (ty.as_ref() as &dyn std::any::Any).downcast_ref::<TupleType>()
            else {
                arguments.push(AttributeValue::Register(RegisterAttr::Virtual {
                    id: argument.id().number(),
                    class: Some(argument_class(argument.ty())?),
                }));
                continue;
            };

            let element_types = tuple.elements(context);
            let mut elements = vec![None; element_types.len()];
            for user in def_use.users_of(argument.id().number()) {
                let extract_instance = context.get_op(*user);
                if extract_instance.operands().first() != Some(&argument.id()) {
                    continue;
                }
                let Some(extract) = extract_instance.clone().as_op::<TupleGetOp>() else {
                    continue;
                };
                let Some(element) = elements.get_mut(extract.index()) else {
                    return Err(PassError::InvalidRuleSet(
                        "tuple_get index is out of bounds".to_string(),
                    ));
                };
                match *element {
                    Some(canonical) => {
                        context.replace_value_uses(extract.result(), canonical);
                    }
                    None => *element = Some(extract.result()),
                }
                let block = context.parent_block(*user).ok_or_else(|| {
                    PassError::InvalidRuleSet("tuple_get has no parent block".to_string())
                })?;
                tuple_extracts.push((*user, block));
            }

            let group = element_types
                .into_iter()
                .zip(elements)
                .map(|(ty, value)| {
                    let value = value.unwrap_or_else(|| context.create_value(ty, None).id());
                    Ok(AttributeValue::Register(RegisterAttr::Virtual {
                        id: value.number(),
                        class: Some(argument_class(ty)?),
                    }))
                })
                .collect::<Result<Vec<_>, PassError>>()?;
            if alignment == 1 {
                arguments.push(AttributeValue::Array(group.into()));
            } else {
                arguments.push(AttributeValue::Dict(Box::new(
                    std::collections::BTreeMap::from([
                        ("alignment".to_string(), AttributeValue::UInt(alignment)),
                        ("members".to_string(), AttributeValue::Array(group.into())),
                    ]),
                )));
            }
        }
        let mut symbol = super::SymbolOpBuilder::new(context)
            .body(op.op().regions()[0])
            .attr("name", AttributeValue::Str(name.into()))
            .attr("arg_regs", AttributeValue::Array(arguments.into()));
        if let Some(result_address) = result_address {
            symbol = symbol.attr("result_address", result_address);
        }
        if func.symbol_visibility() == tir::Visibility::Private {
            symbol = symbol.attr("binding", AttributeValue::Str("local".to_string().into()));
        }
        let symbol = symbol.build();
        rewriter.replace_op(op, &symbol)?;
        for (extract, block) in tuple_extracts {
            rewriter.erase_op(&OperationRef::new(
                context.get_op(extract),
                Some(context.get_block(block)),
                None,
            ))?;
        }
        return Ok(true);
    }

    if let Some(ret) = op.as_op::<ReturnOp>() {
        let mut tuple_source = None;
        let values = match ret.operands().first().copied() {
            None => Vec::new(),
            Some(value) => {
                let ty = context.get_type_data(context.get_value(value).ty());
                if (ty.as_ref() as &dyn std::any::Any)
                    .downcast_ref::<TupleType>()
                    .is_none()
                {
                    vec![value]
                } else {
                    let defining_op = context.get_value(value).defining_op().ok_or_else(|| {
                        PassError::InvalidRuleSet(
                            "returned tuple has no defining operation".to_string(),
                        )
                    })?;
                    let tuple = context
                        .get_op(defining_op)
                        .clone()
                        .as_op::<MakeTupleOp>()
                        .ok_or_else(|| {
                            PassError::InvalidRuleSet(
                                "returned tuple must be assembled from scalar values".to_string(),
                            )
                        })?;
                    tuple_source = Some((value, defining_op));
                    tuple.operands().to_vec()
                }
            }
        };
        rewriter.replace_op(
            op,
            &super::VirtualReturnOpBuilder::new(context)
                .values(values)
                .build(),
        )?;
        if let Some((value, defining_op)) = tuple_source {
            let block = context.parent_block(defining_op).ok_or_else(|| {
                PassError::InvalidRuleSet("tuple construction has no parent block".to_string())
            })?;
            let enclosing = context.parent_op(defining_op).ok_or_else(|| {
                PassError::InvalidRuleSet("tuple construction has no enclosing op".to_string())
            })?;
            if tir::analysis::DefUse::new(context, enclosing).is_used(value.number()) {
                return Ok(true);
            }
            rewriter.erase_op(&OperationRef::new(
                context.get_op(defining_op),
                Some(context.get_block(block)),
                None,
            ))?;
        }
        return Ok(true);
    }

    Ok(false)
}

pub struct OpLoweringPass {
    name: &'static str,
    lowerings: Vec<OpLowering>,
}

impl OpLoweringPass {
    pub fn new(name: &'static str, lowerings: Vec<OpLowering>) -> Self {
        Self { name, lowerings }
    }
}

impl Pass for OpLoweringPass {
    fn name(&self) -> &'static str {
        self.name
    }

    fn target(&self) -> PassTarget {
        PassTarget::Any
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        for lowering in &self.lowerings {
            if lowering(context, op, rewriter)? {
                return Ok(());
            }
        }
        Ok(())
    }
}
