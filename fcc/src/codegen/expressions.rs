//! Expression traversal and C expression lowering.

use super::{
    FnCodegen, LoweredExpr, converted_node_type, lower_type, node_entity, node_type,
    source_type_layout, unsupported,
};
use crate::ast::{AstKind, AstLeaf};
use crate::cir;
use crate::diagnostics::Diagnostic;
use crate::lexer::decode_character_constant;
use crate::sema::{TypeKind, ValueCategory};
use tir::ValueId;
use tir::attributes::Predicate;
use tir::builtin::{IntegerType, ops as b};
use tir::cfg::ops as cb;
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};

impl FnCodegen<'_> {
    pub(super) fn lower_expr(&mut self, root: NodeId) -> Result<ValueId, Diagnostic> {
        let expression = self.lower_expr_value(root)?;
        Ok(self.materialize(expression))
    }

    pub(super) fn lower_discarded_expr(&mut self, node: NodeId) -> Result<(), Diagnostic> {
        let expression = self.lower_expr_value(node)?;
        if !matches!(
            self.typed.types().kind(node_type(self.typed, node)),
            TypeKind::Record(_)
        ) {
            self.materialize(expression);
        }
        Ok(())
    }

    pub(super) fn lower_expr_value(&mut self, root: NodeId) -> Result<LoweredExpr, Diagnostic> {
        self.values.clear();
        self.lower_expr_node(root)
    }

    fn lower_expr_node(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        if let Some(expression) = self.values.get(&node) {
            return Ok(*expression);
        }
        let ast = self.ast;
        let kind = ast.get_node(node).kind;
        if matches!(
            self.typed.types().kind(node_type(self.typed, node)),
            TypeKind::LongDouble
        ) {
            return Err(unsupported(
                ast,
                node,
                "long double expressions".to_string(),
            ));
        }
        if let Some(value) = ast.get_annotation(node).and_then(|info| info.constant)
            && matches!(
                self.typed.types().kind(node_type(self.typed, node)),
                TypeKind::Integer(_) | TypeKind::Enum(_)
            )
        {
            let expression = self.const_of_node(node, value);
            let expression = self.apply_conversions(node, expression);
            self.values.insert(node, expression);
            return Ok(expression);
        }
        if matches!(
            kind,
            AstKind::LogAnd | AstKind::LogOr | AstKind::Conditional
        ) {
            let expression = if kind == AstKind::Conditional {
                self.lower_conditional(node)?
            } else {
                self.lower_logical(node, kind)?
            };
            let expression = self.apply_conversions(node, expression);
            self.values.insert(node, expression);
            return Ok(expression);
        }

        for child in ast.children(node) {
            self.lower_expr_node(child)?;
        }

        {
            let expression = match ast.get_node(node).kind {
                AstKind::Int => {
                    let AstLeaf::Int(n) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("int node carries an int payload");
                    };
                    self.const_of_node(node, n.value.to_i64())
                }
                AstKind::FloatLiteral => {
                    let AstLeaf::Float(n) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("floating literal node carries a floating payload");
                    };
                    LoweredExpr::Value(self.float_constant(
                        n.value.to_bits() as u64,
                        lower_type(self.context, self.typed, node_type(self.typed, node)),
                    ))
                }
                AstKind::Character => {
                    let AstLeaf::Character(spelling) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("character node carries a character payload");
                    };
                    let Some(value) = decode_character_constant(spelling) else {
                        return Err(unsupported(
                            ast,
                            node,
                            "multi-character constant".to_string(),
                        ));
                    };
                    self.const_of_node(node, value)
                }
                AstKind::SizeofType | AstKind::SizeofExpr => {
                    let value = ast.get_annotation(node).unwrap().constant.unwrap();
                    self.const_of_node(node, value)
                }
                AstKind::String => {
                    let AstLeaf::String(value) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("string node carries a string payload");
                    };
                    let label = self.strings[value].clone();
                    LoweredExpr::Value(self.symbols.data(self.context, &label))
                }
                AstKind::Var => self.lower_var(node)?,
                AstKind::Member => self.lower_member(node)?,
                kind @ (AstKind::Call | AstKind::CallExpr) => self.lower_call(node, kind)?,
                kind @ (AstKind::Add
                | AstKind::Sub
                | AstKind::Mul
                | AstKind::Div
                | AstKind::Mod) => self.lower_arithmetic(node, kind)?,
                kind @ (AstKind::BitAnd
                | AstKind::BitXor
                | AstKind::BitOr
                | AstKind::Shl
                | AstKind::Shr) => self.lower_bitwise(node, kind)?,
                kind @ (AstKind::Neg | AstKind::Pos | AstKind::Not | AstKind::BitNot) => {
                    self.lower_unary(node, kind)?
                }
                AstKind::AddressOf => {
                    let child = ast.children(node).next().unwrap();
                    let LoweredExpr::Address { ptr, .. } = self.values[&child] else {
                        return Err(unsupported(
                            ast,
                            node,
                            "non-addressable address-of operand".to_string(),
                        ));
                    };
                    LoweredExpr::Value(ptr)
                }
                AstKind::Deref => {
                    let child = ast.children(node).next().unwrap();
                    let ptr = self.materialize(self.values[&child]);
                    if ast
                        .get_annotation(node)
                        .is_some_and(|info| info.category == ValueCategory::Function)
                    {
                        LoweredExpr::Value(ptr)
                    } else {
                        let elem =
                            lower_type(self.context, self.typed, node_type(self.typed, node));
                        LoweredExpr::Address { ptr, elem }
                    }
                }
                kind
                @ (AstKind::PreInc | AstKind::PreDec | AstKind::PostInc | AstKind::PostDec) => {
                    self.lower_increment(node, kind)?
                }
                kind @ (AstKind::Lt
                | AstKind::Gt
                | AstKind::Le
                | AstKind::Ge
                | AstKind::Eq
                | AstKind::Ne) => self.lower_comparison(node, kind)?,
                AstKind::Comma => {
                    let rhs = ast.children(node).nth(1).unwrap();
                    LoweredExpr::Value(self.materialize(self.values[&rhs]))
                }
                AstKind::Cast => self.lower_cast(node)?,
                kind @ (AstKind::AddAssign
                | AstKind::SubAssign
                | AstKind::MulAssign
                | AstKind::DivAssign
                | AstKind::ModAssign
                | AstKind::ShlAssign
                | AstKind::ShrAssign
                | AstKind::AndAssign
                | AstKind::XorAssign
                | AstKind::OrAssign) => self.lower_compound_assign(node, kind)?,
                AstKind::AssignExpr => self.lower_assignment(node)?,
                // The richer operators (division, comparison, logical, unary,
                // calls) are parsed but not yet lowered; stub them out for now.
                kind => {
                    return Err(unsupported(ast, node, format!("expression {kind:?}")));
                }
            };
            let expression = if ast
                .get_annotation(node)
                .is_some_and(|semantics| !semantics.conversions.is_empty())
            {
                self.apply_conversions(node, expression)
            } else {
                expression
            };
            self.values.insert(node, expression);
            Ok(expression)
        }
    }

    /// The integer constant `value`, typed as the C type `node` carries.
    fn const_of_node(&mut self, node: NodeId, value: i64) -> LoweredExpr {
        let ty = lower_type(self.context, self.typed, node_type(self.typed, node));
        LoweredExpr::Value(
            self.emit(b::constant(self.context, value, ty).build())
                .result(),
        )
    }

    fn lower_var(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let AstLeaf::Var(name) = ast.get_leaf_data(node).unwrap() else {
            unreachable!("var node carries a var payload");
        };
        if let Some(value) = ast.get_annotation(node).and_then(|info| info.constant) {
            return Ok(self.const_of_node(node, value));
        }
        if ast
            .get_annotation(node)
            .is_some_and(|info| info.category == ValueCategory::Function)
        {
            // A function's address is data: the λ value itself never
            // lives in memory.
            let signature = self.signatures[&node_entity(self.typed, node)].clone();
            let lambda = self.symbols.function(self.context, name, &signature);
            let ptr_ty = PtrType::opaque(self.context);
            return Ok(LoweredExpr::Value(
                self.emit(b::fn_to_ptr(self.context, lambda, ptr_ty).build())
                    .result(),
            ));
        }
        let entity = node_entity(self.typed, node);
        if let Some(slot) = self.locals.get(&entity).copied() {
            return Ok(LoweredExpr::Address {
                ptr: slot.ptr,
                elem: slot.elem,
            });
        }
        let global = self.globals[&entity].clone();
        Ok(LoweredExpr::Address {
            ptr: self.symbols.data(self.context, &global.name),
            elem: global.elem,
        })
    }

    fn lower_member(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let AstLeaf::Member { indirect, .. } = ast.get_leaf_data(node).unwrap() else {
            unreachable!("member node carries a member payload");
        };
        let base_node = ast.children(node).next().unwrap();
        let base_value = self.values[&base_node];
        let base_ptr = if *indirect {
            self.materialize(base_value)
        } else if let LoweredExpr::Address { ptr, .. } = base_value {
            ptr
        } else {
            return Err(unsupported(
                ast,
                node,
                "non-addressable member base".to_string(),
            ));
        };
        let elem = lower_type(self.context, self.typed, node_type(self.typed, node));
        let ptr_ty = PtrType::opaque(self.context);
        let field = ast.get_annotation(node).unwrap().member_index.unwrap() as u64;
        let base_ty = node_type(self.typed, base_node);
        let record = match self.typed.types().kind(base_ty) {
            TypeKind::Record(id) => self.typed.record(*id).unwrap(),
            TypeKind::Pointer(pointee) => {
                let TypeKind::Record(id) = self.typed.types().kind(*pointee) else {
                    unreachable!("member base has a record type")
                };
                self.typed.record(*id).unwrap()
            }
            _ => unreachable!("member base has a record type"),
        };
        let member = self.emit(
            cir::ops::get_member(self.context, base_ptr, field, record.name.as_str(), ptr_ty)
                .build(),
        );
        Ok(LoweredExpr::Address {
            ptr: member.result(),
            elem,
        })
    }

    fn lower_arithmetic(&mut self, node: NodeId, kind: AstKind) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(node);
        let lhs_node = children.next().unwrap();
        let rhs_node = children.next().unwrap();
        let lhs = self.values[&lhs_node];
        let rhs = self.values[&rhs_node];
        let l = self.materialize(lhs);
        let r = self.materialize(rhs);
        // Each side is read as a value of its own type: a relational
        // operand is one bit wide until something asks it to be an `int`.
        let l = self.as_value_of_node_type(l, lhs_node);
        let r = self.as_value_of_node_type(r, rhs_node);
        let source_ty = node_type(self.typed, node);
        let lhs_ty = converted_node_type(self.typed, lhs_node);
        let rhs_ty = converted_node_type(self.typed, rhs_node);
        let value = match (
            kind,
            self.typed.types().kind(lhs_ty),
            self.typed.types().kind(rhs_ty),
        ) {
            (AstKind::Sub, TypeKind::Pointer(_), TypeKind::Pointer(_)) => {
                self.lower_pointer_difference(l, r, lhs_ty, source_ty)
            }
            (AstKind::Add | AstKind::Sub, TypeKind::Pointer(_), TypeKind::Integer(_)) => {
                self.lower_pointer_offset(l, r, rhs_ty, lhs_ty, kind == AstKind::Sub)
            }
            (AstKind::Add, TypeKind::Integer(_), TypeKind::Pointer(_)) => {
                self.lower_pointer_offset(r, l, lhs_ty, rhs_ty, false)
            }
            _ if matches!(
                self.typed.types().kind(source_ty),
                TypeKind::Float | TypeKind::Double
            ) =>
            {
                self.lower_floating_binary(kind, l, r, source_ty)
            }
            _ => self.lower_integer_binary(kind, l, r, source_ty),
        };
        Ok(LoweredExpr::Value(value))
    }

    fn lower_bitwise(&mut self, node: NodeId, kind: AstKind) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(node);
        let lhs_node = children.next().unwrap();
        let rhs_node = children.next().unwrap();
        let result_ty = lower_type(self.context, self.typed, node_type(self.typed, node));
        let lhs = self.materialize(self.values[&lhs_node]);
        let rhs = self.materialize(self.values[&rhs_node]);
        let lhs = self.promote_boolean_result(lhs, result_ty);
        let rhs = self.promote_boolean_result(rhs, result_ty);
        Ok(LoweredExpr::Value(self.lower_integer_binary(
            kind,
            lhs,
            rhs,
            node_type(self.typed, node),
        )))
    }

    fn lower_unary(&mut self, node: NodeId, kind: AstKind) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let child = ast.children(node).next().unwrap();
        let operand = self.materialize(self.values[&child]);
        let result_ty = lower_type(self.context, self.typed, node_type(self.typed, node));
        let value = match kind {
            AstKind::Pos => operand,
            AstKind::Neg
                if matches!(
                    self.typed.types().kind(node_type(self.typed, node)),
                    TypeKind::Float | TypeKind::Double
                ) =>
            {
                let width =
                    (source_type_layout(self.typed, node_type(self.typed, node)).0 * 8) as u32;
                let integer_ty = IntegerType::new(self.context, width);
                let bits = self
                    .emit(
                        b::BitcastOpBuilder::new(self.context)
                            .input(operand)
                            .result_type(integer_ty)
                            .build(),
                    )
                    .result();
                let sign = self
                    .emit(
                        b::constant(
                            self.context,
                            if width == 64 {
                                i64::MIN
                            } else {
                                i64::from(i32::MIN)
                            },
                            integer_ty,
                        )
                        .build(),
                    )
                    .result();
                let negated = self
                    .emit(b::xori(self.context, bits, sign, integer_ty).build())
                    .result();
                self.emit(
                    b::BitcastOpBuilder::new(self.context)
                        .input(negated)
                        .result_type(result_ty)
                        .build(),
                )
                .result()
            }
            AstKind::Neg => {
                let zero = self
                    .emit(b::constant(self.context, 0, result_ty).build())
                    .result();
                self.emit(b::subi(self.context, zero, operand, result_ty).build())
                    .result()
            }
            AstKind::BitNot => {
                let ones = self
                    .emit(b::constant(self.context, -1, result_ty).build())
                    .result();
                self.emit(b::xori(self.context, operand, ones, result_ty).build())
                    .result()
            }
            AstKind::Not => {
                let comparison = self.compare_against_zero(operand, Predicate::Eq);
                self.emit(b::extui(self.context, comparison, result_ty).build())
                    .result()
            }
            _ => unreachable!(),
        };
        Ok(LoweredExpr::Value(value))
    }

    fn lower_increment(&mut self, node: NodeId, kind: AstKind) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let child = ast.children(node).next().unwrap();
        let LoweredExpr::Address { ptr, elem } = self.values[&child] else {
            return Err(unsupported(
                ast,
                node,
                "non-addressable increment operand".to_string(),
            ));
        };
        let old = self.emit(p::load(self.context, ptr, elem).build()).result();
        let operand_ty = node_type(self.typed, child);
        let increment = matches!(kind, AstKind::PreInc | AstKind::PostInc);
        let new = if let TypeKind::Pointer(pointee) = self.typed.types().kind(operand_ty) {
            let offset_ty = IntegerType::new(self.context, self.typed.target().pointer_width());
            let size = source_type_layout(self.typed, *pointee).0 as i64;
            let offset = self
                .emit(
                    b::constant(
                        self.context,
                        if increment { size } else { -size },
                        offset_ty,
                    )
                    .build(),
                )
                .result();
            self.emit(p::ptradd(self.context, old, offset, elem).build())
                .result()
        } else if matches!(
            self.typed.types().kind(operand_ty),
            TypeKind::Float | TypeKind::Double
        ) {
            let one = self.float_one(elem);
            self.emit_fp_binop(
                if increment {
                    AstKind::Add
                } else {
                    AstKind::Sub
                },
                old,
                one,
                elem,
            )
        } else {
            let one = self
                .emit(b::constant(self.context, 1, elem).build())
                .result();
            if increment {
                self.emit(b::addi(self.context, old, one, elem).build())
                    .result()
            } else {
                self.emit(b::subi(self.context, old, one, elem).build())
                    .result()
            }
        };
        self.emit(p::store(self.context, new, ptr).build());
        Ok(LoweredExpr::Value(
            if matches!(kind, AstKind::PostInc | AstKind::PostDec) {
                old
            } else {
                new
            },
        ))
    }

    fn lower_comparison(&mut self, node: NodeId, kind: AstKind) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(node);
        let lhs_node = children.next().unwrap();
        let rhs_node = children.next().unwrap();
        let lhs = self.materialize(self.values[&lhs_node]);
        let rhs = self.materialize(self.values[&rhs_node]);
        // The common type of the usual arithmetic conversions, not
        // the operand's own type: it decides signed vs unsigned.
        let operand_ty = converted_node_type(self.typed, lhs_node);
        let value = match self.typed.types().kind(operand_ty) {
            TypeKind::Float | TypeKind::Double => self.lower_floating_compare(kind, lhs, rhs),
            TypeKind::Pointer(_) | TypeKind::Array(_, _) => {
                let predicate = match kind {
                    AstKind::Lt => Predicate::Ult,
                    AstKind::Gt => Predicate::Ugt,
                    AstKind::Le => Predicate::Ule,
                    AstKind::Ge => Predicate::Uge,
                    AstKind::Eq => Predicate::Eq,
                    _ => Predicate::Ne,
                };
                self.lower_pointer_compare(predicate, lhs, rhs)
            }
            _ => self.lower_integer_compare(kind, lhs, rhs, operand_ty),
        };
        Ok(LoweredExpr::Value(value))
    }

    /// A lowered value with the width its own C type asks for. A relational
    /// operator is one bit wide where the machine decides a branch on it, and
    /// `int` everywhere it is read as a value, so a reader of the second kind
    /// asks here rather than trusting the two to agree.
    pub(super) fn as_value_of_node_type(&mut self, value: ValueId, node: NodeId) -> ValueId {
        let target = lower_type(
            self.context,
            self.typed,
            converted_node_type(self.typed, node),
        );
        if self.context.get_value(value).ty() == target {
            return value;
        }
        self.emit(b::extui(self.context, value, target).build())
            .result()
    }

    fn lower_cast(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let child = ast.children(node).next().unwrap();
        let value = self.materialize(self.values[&child]);
        let source = node_type(self.typed, child);
        let target = node_type(self.typed, node);
        let value = if self.typed.integer_width(source).is_some()
            && matches!(self.typed.types().kind(target), TypeKind::Pointer(_))
            && ast
                .get_annotation(child)
                .is_some_and(|semantics| semantics.constant == Some(0))
        {
            let target = lower_type(self.context, self.typed, target);
            self.emit(p::null(self.context, target).build()).result()
        } else {
            self.convert_scalar(value, source, target)
        };
        Ok(LoweredExpr::Value(value))
    }

    fn lower_compound_assign(
        &mut self,
        node: NodeId,
        kind: AstKind,
    ) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(node);
        let lhs_node = children.next().unwrap();
        let LoweredExpr::Address { ptr, elem } = self.values[&lhs_node] else {
            return Err(unsupported(
                ast,
                node,
                "non-addressable compound assignment".to_string(),
            ));
        };
        let rhs_node = children.next().unwrap();
        let rhs = self.materialize(self.values[&rhs_node]);
        let rhs = self.as_value_of_node_type(rhs, rhs_node);
        let lhs = self.emit(p::load(self.context, ptr, elem).build()).result();
        let source_ty = node_type(self.typed, lhs_node);
        let value = if let TypeKind::Pointer(_) = self.typed.types().kind(source_ty) {
            self.lower_pointer_offset(
                lhs,
                rhs,
                node_type(self.typed, rhs_node),
                source_ty,
                kind == AstKind::SubAssign,
            )
        } else {
            let operand_ty = converted_node_type(self.typed, rhs_node);
            let lhs = self.convert_scalar(lhs, source_ty, operand_ty);
            let result = match self.typed.types().kind(operand_ty) {
                TypeKind::Float | TypeKind::Double => {
                    self.lower_floating_binary(kind, lhs, rhs, operand_ty)
                }
                _ => self.lower_integer_binary(kind, lhs, rhs, operand_ty),
            };
            self.convert_scalar(result, operand_ty, source_ty)
        };
        self.emit(p::store(self.context, value, ptr).build());
        Ok(LoweredExpr::Value(value))
    }

    fn lower_assignment(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(node);
        let lhs_node = children.next().unwrap();
        let lhs = self.values[&lhs_node];
        let rhs = self.values[&children.next().unwrap()];
        let LoweredExpr::Address { ptr, elem } = lhs else {
            return Err(unsupported(
                ast,
                node,
                "non-addressable assignment".to_string(),
            ));
        };
        if let TypeKind::Record(id) = self.typed.types().kind(node_type(self.typed, lhs_node)) {
            let LoweredExpr::Address { ptr: source, .. } = rhs else {
                return Err(unsupported(
                    ast,
                    node,
                    "non-addressable struct source".to_string(),
                ));
            };
            self.emit(
                cir::ops::copy_struct(
                    self.context,
                    ptr,
                    source,
                    self.typed.record(*id).unwrap().name.as_str(),
                )
                .build(),
            );
            return Ok(LoweredExpr::Address { ptr, elem });
        }
        let value = self.materialize(rhs);
        let value = self.store_scalar(value, ptr, elem);
        Ok(LoweredExpr::Value(value))
    }
    fn lower_logical(&mut self, node: NodeId, kind: AstKind) -> Result<LoweredExpr, Diagnostic> {
        let mut children = self.ast.children(node);
        let lhs_node = children.next().unwrap();
        let rhs_node = children.next().unwrap();
        let lhs = self.lower_expr_node(lhs_node)?;
        let lhs = self.materialize(lhs);
        let condition = self.truth_value(lhs);
        let result_ty = IntegerType::new(self.context, 32);

        let rhs_block = self.new_block();
        let merge = self.new_block();
        let result = self
            .context
            .append_block_argument(merge.id(), result_ty)
            .id();

        let short_circuit = i64::from(kind == AstKind::LogOr);
        let short_circuit = self
            .emit(b::constant(self.context, short_circuit, result_ty).build())
            .result();
        let (if_true, true_args, if_false, false_args) = if kind == AstKind::LogAnd {
            (&rhs_block, vec![], &merge, vec![short_circuit])
        } else {
            (&merge, vec![short_circuit], &rhs_block, vec![])
        };
        self.emit(
            cb::cond_br(
                self.context,
                condition,
                true_args,
                false_args,
                if_true.id(),
                if_false.id(),
            )
            .build(),
        );
        self.terminated = true;

        self.enter_block(rhs_block);
        let rhs = self.lower_expr_node(rhs_node)?;
        let rhs = self.materialize(rhs);
        let rhs = self.truth_value(rhs);
        let rhs = self
            .emit(b::extui(self.context, rhs, result_ty).build())
            .result();
        self.branch_to(&merge, vec![rhs]);

        self.enter_block(merge);
        let expression = LoweredExpr::Value(result);
        self.values.insert(node, expression);
        Ok(expression)
    }

    fn lower_conditional(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        let mut children = self.ast.children(node);
        let condition_node = children.next().unwrap();
        let then_node = children.next().unwrap();
        let else_node = children.next().unwrap();
        let condition = self.lower_expr_node(condition_node)?;
        let condition = self.materialize(condition);
        let condition = self.truth_value(condition);
        let source_ty = node_type(self.typed, node);
        let result_ty = lower_type(self.context, self.typed, source_ty);

        let then_block = self.new_block();
        let else_block = self.new_block();
        let merge = self.new_block();
        let result = self
            .context
            .append_block_argument(merge.id(), result_ty)
            .id();

        self.branch_on(condition, &then_block, &else_block);
        for (arm, block) in [(then_node, then_block), (else_node, else_block)] {
            self.enter_block(block);
            let value = self.lower_expr_node(arm)?;
            let value = self.materialize(value);
            self.branch_to(&merge, vec![value]);
        }

        self.enter_block(merge);
        let expression = LoweredExpr::Value(result);
        self.values.insert(node, expression);
        Ok(expression)
    }
}
