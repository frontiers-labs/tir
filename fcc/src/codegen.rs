//! Lowers the C [`crate::ast`] to TIR using the `builtin` and `ptr` dialects.
//!
//! The lowering is intentionally memory-based (the unoptimised, "no memory
//! SSA" shape a C frontend emits before any mem2reg pass): every parameter and
//! local lives in a stack slot produced by `ptr.alloca`, reads become
//! `ptr.load` and writes become `ptr.store`. Arithmetic uses the `builtin`
//! integer ops; C-only literals and variadic markers use the local `cir` dialect.

use std::collections::HashMap;

use tir::builtin::{IntegerType, ModuleOp, UnitType, ops as b};
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};
use tir::{Context, IRBuilder, Operand, Operation, TypeId, ValueId};

use crate::ast::*;
use crate::cir::{self, VarArgsType};
use crate::diagnostics::{
    Diagnostic, EmptyTranslationUnit, UndeclaredIdentifier, UnsupportedConstruct,
};

/// A local variable: the pointer to its stack slot and the slot's element type.
#[derive(Clone, Copy)]
struct Slot {
    ptr: ValueId,
    elem: TypeId,
}

struct FnCodegen<'a> {
    context: &'a Context,
    ast: &'a Ast,
    builder: IRBuilder,
    locals: HashMap<String, Slot>,
    signatures: &'a HashMap<String, Signature>,
    /// Scratch holding the lowered SSA value of each node in the expression
    /// subtree currently being lowered, indexed by `node.index() - base`. Reused
    /// across expressions to avoid reallocating.
    values: Vec<ValueId>,
}

#[derive(Clone)]
struct Signature {
    ret: TypeId,
    args: Vec<TypeId>,
}

/// Lower a translation unit into a `builtin.module` in `context`.
pub fn codegen(context: &Context, ast: &Ast) -> Result<ModuleOp, Diagnostic> {
    let module = b::module(context, None).build();
    let mut module_builder = IRBuilder::new(module.body());

    let root = ast.root().ok_or_else(EmptyTranslationUnit::new)?;
    let mut signatures = HashMap::new();
    for item in ast.children(root) {
        match ast.get_node(item).kind {
            AstKind::Prototype | AstKind::Function => {
                let (name, sig) = lower_signature(context, ast, item)?;
                signatures.insert(name, sig);
            }
            AstKind::DeclGroup
            | AstKind::RecordDecl
            | AstKind::Typedef
            | AstKind::Global
            | AstKind::Attribute => {}
            _ => return Err(unsupported(ast, item, "top-level item".to_string())),
        }
    }

    for item in ast.children(root) {
        match ast.get_node(item).kind {
            AstKind::Prototype => {
                let AstLeaf::Function { name, .. } = ast.get_leaf_data(item).unwrap() else {
                    unreachable!("prototype node carries a function payload");
                };
                let sig = signatures.get(name).unwrap();
                module_builder.insert(b::declare_op(context, name, sig.ret, &sig.args));
            }
            AstKind::Function => {
                let func_op = lower_function(context, ast, item, &signatures)?;
                module_builder.insert(func_op);
            }
            AstKind::DeclGroup
            | AstKind::RecordDecl
            | AstKind::Typedef
            | AstKind::Global
            | AstKind::Attribute => {}
            _ => unreachable!("top-level item was checked before emission"),
        }
    }
    module_builder.insert(b::module_end(context).build());
    Ok(module)
}

/// Use of a name with no declaration in scope, spanned at the offending node.
fn undeclared(ast: &Ast, node: NodeId, name: &str) -> Diagnostic {
    UndeclaredIdentifier::new(ast.get_node(node).span, name).into()
}

/// A construct the parser accepts but codegen does not lower yet.
fn unsupported(ast: &Ast, node: NodeId, what: String) -> Diagnostic {
    UnsupportedConstruct::new(ast.get_node(node).span, what).into()
}

fn lower_ctype(context: &Context, ty: &CType) -> TypeId {
    match ty {
        CType::Int => IntegerType::new(context, 32),
        CType::Void => UnitType::new(context),
        CType::Char => IntegerType::new(context, 8),
        CType::Bool => IntegerType::new(context, 1),
        CType::Float | CType::Double | CType::Builtin(_) | CType::Named(_) => {
            IntegerType::new(context, 64)
        }
        CType::Record(_, _) | CType::Enum(_) => IntegerType::new(context, 64),
        CType::Const(inner) => lower_ctype(context, inner),
        CType::Volatile(inner) => lower_ctype(context, inner),
        CType::Restrict(inner) => lower_ctype(context, inner),
        CType::Pointer(inner) => PtrType::typed(context, lower_ctype(context, inner)),
        CType::Array(inner, _) => PtrType::typed(context, lower_ctype(context, inner)),
        CType::Function { .. } => IntegerType::new(context, 64),
        CType::Attributed(inner, _) => lower_ctype(context, inner),
    }
}

fn lower_signature(
    context: &Context,
    ast: &Ast,
    item: NodeId,
) -> Result<(String, Signature), Diagnostic> {
    let AstLeaf::Function { name, ret } = ast.get_leaf_data(item).unwrap() else {
        unreachable!("function-like node carries a function payload");
    };
    let mut args = Vec::new();
    for child in ast.children(item) {
        match ast.get_node(child).kind {
            AstKind::Param => {
                let AstLeaf::Param { ty, .. } = ast.get_leaf_data(child).unwrap() else {
                    unreachable!("param node carries a param payload");
                };
                args.push(lower_ctype(context, ty));
            }
            AstKind::VarArgs => args.push(VarArgsType::new(context)),
            _ => break,
        }
    }
    Ok((
        name.clone(),
        Signature {
            ret: lower_ctype(context, ret),
            args,
        },
    ))
}

fn lower_function(
    context: &Context,
    ast: &Ast,
    func: NodeId,
    signatures: &HashMap<String, Signature>,
) -> Result<impl Operation, Diagnostic> {
    let AstLeaf::Function { name, ret } = ast.get_leaf_data(func).unwrap() else {
        unreachable!("function node carries a function payload");
    };
    let ret_ty = lower_ctype(context, ret);

    // Entry block arguments carry the incoming parameter values; parameters are
    // the function node's leading children.
    let mut param_values = Vec::new();
    for param in ast
        .children(func)
        .take_while(|&c| matches!(ast.get_node(c).kind, AstKind::Param))
    {
        let AstLeaf::Param { ty, .. } = ast.get_leaf_data(param).unwrap() else {
            unreachable!("param node carries a param payload");
        };
        param_values.push(context.create_value(lower_ctype(context, ty), None));
    }
    let param_ids: Vec<ValueId> = param_values.iter().map(|v| v.id()).collect();

    let region = context.create_region();
    let block = context.create_block(param_values);
    region.add_block(block.id());

    let func_op = b::func(context, name.as_str(), ret_ty, Some(region.id())).build();

    let mut cg = FnCodegen {
        context,
        ast,
        builder: IRBuilder::new(func_op.body()),
        locals: HashMap::new(),
        signatures,
        values: Vec::new(),
    };
    cg.lower_body(func, &param_ids)?;

    Ok(func_op)
}

impl FnCodegen<'_> {
    fn alloca(&mut self, elem: TypeId) -> Slot {
        let ptr_ty = PtrType::typed(self.context, elem);
        let op = self.builder.insert(p::alloca(self.context, ptr_ty).build());
        Slot {
            ptr: op.result(),
            elem,
        }
    }

    /// Lower a function: spill parameters into stack slots, then lower each body
    /// statement in source order (statement order is a side-effect ordering, so it
    /// stays top-down; only the expressions within use the post-order iterator).
    fn lower_body(&mut self, func: NodeId, param_ids: &[ValueId]) -> Result<(), Diagnostic> {
        let ast = self.ast;

        let mut idx = 0;
        for param in ast
            .children(func)
            .take_while(|&c| matches!(ast.get_node(c).kind, AstKind::Param))
        {
            let AstLeaf::Param { name, ty } = ast.get_leaf_data(param).unwrap() else {
                unreachable!("param node carries a param payload");
            };
            let elem = lower_ctype(self.context, ty);
            let slot = self.alloca(elem);
            self.builder
                .insert(p::store(self.context, param_ids[idx], slot.ptr).build());
            idx += 1;
            self.locals.insert(name.clone(), slot);
        }

        for stmt in ast.children(func).skip(idx) {
            self.lower_stmt(stmt)?;
        }

        Ok(())
    }

    fn lower_stmt(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        match ast.get_node(stmt).kind {
            AstKind::Decl => {
                let AstLeaf::Decl { name, ty } = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("decl node carries a decl payload");
                };
                let elem = lower_ctype(self.context, ty);
                let slot = self.alloca(elem);
                if let Some(init) = ast.children(stmt).next() {
                    let value = self.lower_expr(init)?;
                    self.builder
                        .insert(p::store(self.context, value, slot.ptr).build());
                }
                self.locals.insert(name.clone(), slot);
                Ok(())
            }
            AstKind::Assign => {
                let AstLeaf::Assign(name) = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("assign node carries an assign payload");
                };
                let slot = *self
                    .locals
                    .get(name)
                    .ok_or_else(|| undeclared(ast, stmt, name))?;
                let value = ast.children(stmt).next().unwrap();
                let v = self.lower_expr(value)?;
                self.builder
                    .insert(p::store(self.context, v, slot.ptr).build());
                Ok(())
            }
            AstKind::Return => {
                let operand = match ast.children(stmt).next() {
                    Some(e) => Operand::from(self.lower_expr(e)?),
                    None => Operand::none(),
                };
                self.builder
                    .insert(b::r#return(self.context, operand).build());
                Ok(())
            }
            AstKind::ExprStmt => {
                if let Some(expr) = ast.children(stmt).next() {
                    self.lower_expr(expr)?;
                }
                Ok(())
            }
            // Control flow and expression statements are parsed but not yet
            // lowered; codegen for them is stubbed out for now.
            kind => Err(unsupported(ast, stmt, format!("statement {kind:?}"))),
        }
    }

    /// Lower an expression subtree in one post-order pass: operands precede their
    /// operator, so each node's value is ready when its parent is reached. The
    /// AST is a tree, so the subtree is a contiguous index range `[base, root]`;
    /// values are pushed in index order, letting children be read by offset
    /// without hashing.
    fn lower_expr(&mut self, root: NodeId) -> Result<ValueId, Diagnostic> {
        let ast = self.ast;
        let i32_ty = IntegerType::new(self.context, 32);
        self.values.clear();
        let mut base = root.index();

        for node in ast.postorder(root) {
            if self.values.is_empty() {
                base = node.index();
            }
            debug_assert_eq!(
                node.index(),
                base + self.values.len(),
                "subtree not contiguous"
            );

            let value = match ast.get_node(node).kind {
                AstKind::Int => {
                    let AstLeaf::Int(n) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("int node carries an int payload");
                    };
                    self.builder
                        .insert(b::constant(self.context, *n, i32_ty).build())
                        .result()
                }
                AstKind::String => {
                    let AstLeaf::String(value) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("string node carries a string payload");
                    };
                    let i8_ty = IntegerType::new(self.context, 8);
                    let ptr_ty = PtrType::typed(self.context, i8_ty);
                    self.builder
                        .insert(cir::string_op(self.context, value, ptr_ty))
                        .result()
                }
                AstKind::Var => {
                    let AstLeaf::Var(name) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("var node carries a var payload");
                    };
                    let slot = *self
                        .locals
                        .get(name)
                        .ok_or_else(|| undeclared(ast, node, name))?;
                    self.builder
                        .insert(p::load(self.context, slot.ptr, slot.elem).build())
                        .result()
                }
                AstKind::Call => {
                    let AstLeaf::Call(name) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("call node carries a call payload");
                    };
                    let sig = self
                        .signatures
                        .get(name)
                        .ok_or_else(|| undeclared(ast, node, name))?;
                    let args = ast
                        .children(node)
                        .map(|arg| self.values[arg.index() - base])
                        .collect::<Vec<_>>();
                    self.builder
                        .insert(b::call(self.context, args, name.as_str(), sig.ret).build())
                        .result()
                }
                kind @ (AstKind::Add | AstKind::Sub | AstKind::Mul) => {
                    let mut children = ast.children(node);
                    let l = self.values[children.next().unwrap().index() - base];
                    let r = self.values[children.next().unwrap().index() - base];
                    match kind {
                        AstKind::Add => self
                            .builder
                            .insert(b::addi(self.context, l, r, i32_ty).build())
                            .result(),
                        AstKind::Sub => self
                            .builder
                            .insert(b::subi(self.context, l, r, i32_ty).build())
                            .result(),
                        _ => self
                            .builder
                            .insert(b::muli(self.context, l, r, i32_ty).build())
                            .result(),
                    }
                }
                // The richer operators (division, comparison, logical, unary,
                // calls) are parsed but not yet lowered; stub them out for now.
                kind => {
                    return Err(unsupported(ast, node, format!("expression {kind:?}")));
                }
            };
            self.values.push(value);
        }

        Ok(*self.values.last().unwrap())
    }
}

/// Decode the C escape sequences of a string literal's source text into the
/// bytes the program observes. `cir.string` keeps the source spelling; the
/// hoisted data must hold real bytes.
fn decode_c_escapes(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Hoist every `cir.string` into a module-level `.rodata` section and rewrite
/// its use into a `builtin.addr_of` of the string's local symbol; identical
/// literals share one symbol. Runs only ahead of the machine backend — the
/// asm-dialect data ops it creates mean nothing to earlier stages.
pub fn hoist_strings(context: &Context, module: &ModuleOp) -> Result<(), tir::PassError> {
    use tir::attributes::AttributeValue;
    use tir::backend::{
        LiteralOpBuilder, SectionEndOpBuilder, SectionOpBuilder, SymbolEndOpBuilder,
        SymbolOpBuilder,
    };

    let mut rewriter = tir::Rewriter::new(context.clone());
    let mut strings: Vec<(String, String)> = Vec::new();
    let mut labels: HashMap<String, String> = HashMap::new();

    for op_id in module.body().op_ids() {
        let op = context.get_op(op_id);
        if op.clone().as_op::<tir::builtin::FuncOp>().is_none() {
            continue;
        }
        let region = context.get_region(op.regions[0]);
        for block in region.iter(context.clone()) {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);
                let Some(string) = op.clone().as_op::<cir::StringOp>() else {
                    continue;
                };
                let value = string
                    .attributes()
                    .iter()
                    .find(|attr| attr.name == "value")
                    .and_then(|attr| match &attr.value {
                        AttributeValue::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .expect("cir.string must carry a value");
                let label = labels
                    .entry(value.clone())
                    .or_insert_with(|| {
                        let label = format!(".L.str{}", strings.len());
                        strings.push((label.clone(), decode_c_escapes(&value)));
                        label
                    })
                    .clone();
                let result_ty = context.get_value(string.result()).ty();
                let addr = b::addr_of_op(context, &label, result_ty);
                rewriter.replace_op(
                    &tir::OperationRef::new(op, Some(block.clone()), None),
                    &addr,
                )?;
            }
        }
    }

    if strings.is_empty() {
        return Ok(());
    }

    let section = SectionOpBuilder::new(context)
        .attr("name", AttributeValue::Str(".rodata".to_string()))
        .build();
    let mut section_builder = IRBuilder::new(section.body());
    for (label, value) in strings {
        let symbol = SymbolOpBuilder::new(context)
            .attr("name", AttributeValue::Str(label))
            .attr("binding", AttributeValue::Str("local".to_string()))
            .attr("kind", AttributeValue::Str("object".to_string()))
            .build();
        let mut symbol_builder = IRBuilder::new(symbol.body());
        symbol_builder.insert(
            LiteralOpBuilder::new(context)
                .attr("kind", AttributeValue::Str("asciz".to_string()))
                .attr("value", AttributeValue::Str(value))
                .build(),
        );
        symbol_builder.insert(SymbolEndOpBuilder::new(context).build());
        section_builder.insert(symbol);
    }
    section_builder.insert(SectionEndOpBuilder::new(context).build());

    // Splice the section in ahead of the module terminator.
    let body = module.body();
    let end = body.op_ids().len().saturating_sub(1);
    body.insert(end, section.id());
    Ok(())
}
