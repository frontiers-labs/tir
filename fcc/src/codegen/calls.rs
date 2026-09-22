//! Argument and return lowering for function calls.

use super::{
    AbiParameter, AbiPiece, FnCodegen, LoweredExpr, abi_storage_layout, classify_function_type,
    converted_node_type, lower_type, node_entity, node_type, source_type_layout, unsupported,
};
use crate::ast::{AstKind, AstLeaf};
use crate::cir;
use crate::diagnostics::Diagnostic;
use crate::sema::{QualType, TypeKind};
use tir::ValueId;
use tir::backend::abi::{ValueKind, type_kind};
use tir::builtin::{FnType, IntegerType, TupleType, ops as b};
use tir::func::ops as func_ops;
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};

impl FnCodegen<'_> {
    pub(super) fn materialize(&mut self, expression: LoweredExpr) -> ValueId {
        match expression {
            LoweredExpr::Value(value) => value,
            LoweredExpr::Address { ptr, elem } => {
                self.emit(p::load(self.context, ptr, elem).build()).result()
            }
        }
    }

    pub(super) fn lower_record_copy(
        &mut self,
        node: NodeId,
        destination: ValueId,
        record: &str,
    ) -> Result<(), Diagnostic> {
        let LoweredExpr::Address { ptr: source, .. } = self.lower_expr_value(node)? else {
            return Err(unsupported(
                self.ast,
                node,
                "non-addressable struct source".to_string(),
            ));
        };
        if self.ast.get_node(node).kind == AstKind::Call {
            let (size, _) = source_type_layout(self.typed, node_type(self.typed, node));
            let size = self
                .emit(
                    b::constant(
                        self.context,
                        size as i64,
                        IntegerType::new(self.context, 64),
                    )
                    .build(),
                )
                .result();
            self.emit(p::memcpy(self.context, destination, source, size).build());
            return Ok(());
        }
        self.emit(cir::ops::copy_struct(self.context, destination, source, record).build());
        Ok(())
    }

    fn lower_abi_argument(
        &mut self,
        node: NodeId,
        expression: LoweredExpr,
        parameter: &AbiParameter,
    ) -> Result<Vec<ValueId>, Diagnostic> {
        if parameter.indirect {
            let LoweredExpr::Address { ptr: source, .. } = expression else {
                return Err(unsupported(
                    self.ast,
                    node,
                    "non-addressable aggregate argument".to_string(),
                ));
            };
            let source_ty = node_type(self.typed, node);
            let (size, align) = source_type_layout(self.typed, source_ty);
            let destination = self
                .emit(p::alloca(self.context, size, align, PtrType::opaque(self.context)).build())
                .result();
            let size = self
                .emit(
                    b::constant(
                        self.context,
                        size as i64,
                        IntegerType::new(self.context, 64),
                    )
                    .build(),
                )
                .result();
            self.emit(p::memcpy(self.context, destination, source, size).build());
            return Ok(vec![destination]);
        }
        if matches!(
            self.typed.types().kind(node_type(self.typed, node)),
            TypeKind::Record(_)
        ) {
            let LoweredExpr::Address { ptr, .. } = expression else {
                return Err(unsupported(
                    self.ast,
                    node,
                    "non-addressable aggregate argument".to_string(),
                ));
            };
            let ptr = self.prepare_abi_source(
                ptr,
                node_type(self.typed, node),
                parameter.pieces.as_slice(),
            );
            let values = parameter
                .pieces
                .iter()
                .map(|piece| {
                    let address = self.offset_address(ptr, piece.offset);
                    self.emit(p::load(self.context, address, piece.ty).build())
                        .result()
                })
                .collect::<Vec<_>>();
            if parameter.grouped {
                let ty = TupleType::new(
                    self.context,
                    parameter.pieces.iter().map(|piece| piece.ty).collect(),
                );
                return Ok(vec![
                    self.emit(
                        b::MakeTupleOpBuilder::new(self.context)
                            .elements(values)
                            .result_type(ty)
                            .build(),
                    )
                    .result(),
                ]);
            }
            return Ok(values);
        }
        Ok(vec![self.materialize(expression)])
    }

    fn prepare_abi_source(
        &mut self,
        source: ValueId,
        source_ty: QualType,
        pieces: &[AbiPiece],
    ) -> ValueId {
        let (size, align) = source_type_layout(self.typed, source_ty);
        let Some((abi_size, abi_align)) = abi_storage_layout(self.context, pieces) else {
            return source;
        };
        if abi_size <= size {
            return source;
        }

        let destination = self
            .emit(
                p::alloca(
                    self.context,
                    abi_size,
                    align.max(abi_align),
                    PtrType::opaque(self.context),
                )
                .build(),
            )
            .result();
        for piece in pieces {
            let zero = match type_kind(self.context, piece.ty) {
                ValueKind::Float => self.float_zero(piece.ty),
                ValueKind::Int => self
                    .emit(b::constant(self.context, 0, piece.ty).build())
                    .result(),
                ValueKind::Vector => unreachable!("ABI padding uses scalar carriers"),
            };
            let address = self.offset_address(destination, piece.offset);
            self.emit(p::store(self.context, zero, address).build());
        }
        let size = self
            .emit(
                b::constant(
                    self.context,
                    size as i64,
                    IntegerType::new(self.context, 64),
                )
                .build(),
            )
            .result();
        self.emit(p::memcpy(self.context, destination, source, size).build());
        destination
    }

    /// Load the ABI return value of an aggregate held at `ptr`.
    pub(super) fn abi_return_value(&mut self, ptr: ValueId, source_ty: QualType) -> ValueId {
        let pieces = self.return_abi.aggregate.clone().unwrap();
        let ptr = self.prepare_abi_source(ptr, source_ty, &pieces);
        let values = pieces
            .iter()
            .map(|piece| {
                let address = self.offset_address(ptr, piece.offset);
                self.emit(p::load(self.context, address, piece.ty).build())
                    .result()
            })
            .collect::<Vec<_>>();
        if let [value] = values.as_slice() {
            return *value;
        }
        self.emit(
            b::MakeTupleOpBuilder::new(self.context)
                .elements(values)
                .result_type(self.return_abi.ty)
                .build(),
        )
        .result()
    }

    /// Write what a `return` returns to the slot the function's one exit reads
    /// it back from.
    pub(super) fn store_return_value(&mut self, value: Option<NodeId>) -> Result<(), Diagnostic> {
        match value {
            Some(node) if self.return_abi.indirect => self.lower_indirect_return(node)?,
            Some(node) if self.return_abi.aggregate.is_some() => {
                let slot = self.return_slot.unwrap();
                let expression = self.lower_expr_value(node)?;
                let LoweredExpr::Address { ptr: source, .. } = expression else {
                    return Err(unsupported(
                        self.ast,
                        node,
                        "non-addressable aggregate return value".to_string(),
                    ));
                };
                let (size, _) = source_type_layout(self.typed, node_type(self.typed, node));
                let size = self
                    .emit(
                        b::constant(
                            self.context,
                            size as i64,
                            IntegerType::new(self.context, 64),
                        )
                        .build(),
                    )
                    .result();
                self.emit(p::memcpy(self.context, slot.ptr, source, size).build());
            }
            Some(node) => {
                let slot = self.return_slot.unwrap();
                let source = converted_node_type(self.typed, node);
                let value = self.lower_expr(node)?;
                let source_ty = lower_type(self.context, self.typed, source);
                let value = self.promote_boolean_result(value, source_ty);
                let value = self.convert_scalar(value, source, self.result_type.unwrap());
                self.emit(p::store(self.context, value, slot.ptr).build());
            }
            None => {}
        }
        Ok(())
    }

    fn lower_indirect_return(&mut self, node: NodeId) -> Result<(), Diagnostic> {
        let expression = self.lower_expr_value(node)?;
        let LoweredExpr::Address { ptr: source, .. } = expression else {
            unreachable!("aggregate return expressions lower to addresses");
        };
        let (size, _) = source_type_layout(self.typed, node_type(self.typed, node));
        let size = self
            .emit(
                b::constant(
                    self.context,
                    size as i64,
                    IntegerType::new(self.context, 64),
                )
                .build(),
            )
            .result();
        self.emit(
            p::memcpy(
                self.context,
                self.indirect_return
                    .expect("indirect return has a destination argument"),
                source,
                size,
            )
            .build(),
        );
        Ok(())
    }

    pub(super) fn lower_call(
        &mut self,
        node: NodeId,
        kind: AstKind,
    ) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let designator_ty = ast
            .get_annotation(node)
            .and_then(|semantics| semantics.call_designator_ty)
            .expect("semantic analysis records the call designator type");
        let (name, sig, callee, arguments) = if kind == AstKind::Call {
            let AstLeaf::Call(name) = ast.get_leaf_data(node).unwrap() else {
                unreachable!("call node carries a call payload");
            };
            let entity = node_entity(self.typed, node);
            let (sig, callee) = match self.typed.types().kind(designator_ty) {
                TypeKind::Function { .. } => (self.signatures[&entity].clone(), None),
                TypeKind::Pointer(pointee) => {
                    let sig = classify_function_type(self.context, self.typed, *pointee);
                    let callee = if let Some(slot) = self.locals.get(&entity).copied() {
                        self.materialize(LoweredExpr::Address {
                            ptr: slot.ptr,
                            elem: slot.elem,
                        })
                    } else {
                        let global = self.globals[&entity].clone();
                        let address = self.symbols.data(self.context, &global.name);
                        self.materialize(LoweredExpr::Address {
                            ptr: address,
                            elem: global.elem,
                        })
                    };
                    (sig, Some(callee))
                }
                _ => unreachable!("call designator is a function or function pointer"),
            };
            (
                Some(name.clone()),
                sig,
                callee,
                ast.children(node).collect::<Vec<_>>(),
            )
        } else {
            let children = ast.children(node).collect::<Vec<_>>();
            let callee_node = children[0];
            let function_ty = match self.typed.types().kind(designator_ty) {
                TypeKind::Function { .. } => designator_ty,
                TypeKind::Pointer(pointee) => *pointee,
                _ => {
                    unreachable!("call expression designator is a function or function pointer")
                }
            };
            (
                None,
                classify_function_type(self.context, self.typed, function_ty),
                Some(self.materialize(self.values[&callee_node])),
                children[1..].to_vec(),
            )
        };
        let mut args = Vec::new();
        let mut argument_alignments = Vec::new();
        for (index, &argument) in arguments.iter().enumerate() {
            let mut expression = self.values[&argument];
            if let LoweredExpr::Value(value) = expression {
                expression = LoweredExpr::Value(self.as_value_of_node_type(value, argument));
            }
            if let Some(parameter) = sig.params.get(index) {
                args.extend(self.lower_abi_argument(argument, expression, parameter)?);
                if parameter.grouped {
                    argument_alignments.push(parameter.alignment);
                } else {
                    argument_alignments.extend(std::iter::repeat_n(1, parameter.pieces.len()));
                }
            } else {
                args.push(self.materialize(expression));
                argument_alignments.push(1);
            }
        }
        let source_ty = node_type(self.typed, node);
        let elem = lower_type(self.context, self.typed, source_ty);
        // Direct and indirect calls differ only in where the callee
        // comes from: a λ of the module, or a loaded address.
        let callee = match callee {
            Some(address) => {
                let ty = FnType::new(self.context, &sig.argument_types(self.context), sig.ret.ty);
                self.emit(b::ptr_to_fn(self.context, address, ty).build())
                    .result()
            }
            None => {
                let name = name.clone().expect("direct call has a symbol name");
                self.symbols.function(self.context, &name, &sig)
            }
        };
        Ok(if sig.ret.indirect {
            let (size, align) = source_type_layout(self.typed, source_ty);
            let slot = self.alloca(elem, size, align);
            args.insert(0, slot.ptr);
            argument_alignments.insert(0, 1);
            let mut call = func_ops::CallOpBuilder::new(self.context)
                .callee(callee)
                .args(args)
                .result_address()
                .result_type(sig.ret.ty);
            if argument_alignments.iter().any(|&alignment| alignment > 1) {
                call = call.argument_alignments(&argument_alignments);
            }
            self.emit(call.build());
            LoweredExpr::Address {
                ptr: slot.ptr,
                elem,
            }
        } else {
            let result = {
                let mut call = func_ops::CallOpBuilder::new(self.context)
                    .callee(callee)
                    .args(args)
                    .result_type(sig.ret.ty);
                if argument_alignments.iter().any(|&alignment| alignment > 1) {
                    call = call.argument_alignments(&argument_alignments);
                }
                self.emit(call.build()).result()
            };
            if let Some(pieces) = sig.ret.aggregate.as_deref() {
                let (size, align) = source_type_layout(self.typed, source_ty);
                let (abi_size, abi_align) = abi_storage_layout(self.context, pieces)
                    .expect("classified aggregate returns use scalar ABI pieces");
                let slot = self.alloca(elem, size.max(abi_size), align.max(abi_align));
                for (index, piece) in pieces.iter().enumerate() {
                    let value = if pieces.len() == 1 {
                        result
                    } else {
                        self.emit(
                            b::TupleGetOpBuilder::new(self.context)
                                .tuple(result)
                                .index(index as u64)
                                .result_type(piece.ty)
                                .build(),
                        )
                        .result()
                    };
                    let address = self.offset_address(slot.ptr, piece.offset);
                    self.emit(p::store(self.context, value, address).build());
                }
                LoweredExpr::Address {
                    ptr: slot.ptr,
                    elem,
                }
            } else {
                LoweredExpr::Value(result)
            }
        })
    }
}
