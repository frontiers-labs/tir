//! Shared lowering of an instruction behavior into one semantic graph rooted
//! at an effectful state-transition node.

use std::collections::HashMap;

use tir::ValueId;
use tir::graph::{Dag, GenericDag, MutDag, NodeId};
use tir::sem::{SymKind, SymPayload};

use crate::ast;

pub type ValueGraph = GenericDag<SymKind, SymPayload<ValueId>>;
pub type UnifiedGraph = GenericDag<SymKind, BehaviorPayload>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    Ident(String),
    Path {
        base: String,
        members: Vec<String>,
    },
    FixedRegister {
        class: String,
        name: String,
        index: u32,
    },
    Field {
        base: Box<Destination>,
        member: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectPayload {
    None,
    Assign {
        destination: Destination,
    },
    Trap {
        params: Vec<String>,
        argument_count: usize,
    },
    Handler {
        kind: String,
        binding: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum BehaviorPayload {
    Value(SymPayload<ValueId>),
    Effect(EffectPayload),
}

/// A complete instruction behavior rooted at one effectful semantic term.
pub struct BehaviorGraph {
    pub graph: UnifiedGraph,
    pub root: NodeId,
    pub variable_symbols: HashMap<String, u32>,
    pub register_symbols: HashMap<(String, u32), u32>,
    pub regnum_symbols: HashMap<String, u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Binding {
    Ident(String),
    Path(String, Vec<String>),
}

impl Binding {
    fn from_destination(expr: &ast::Expr) -> Option<Self> {
        match expr {
            ast::Expr::Ident(ident) => Some(Self::Ident(ident.name.clone())),
            ast::Expr::Path(path) => Some(Self::Path(path.base.clone(), path.remainder.clone())),
            _ => None,
        }
    }

    fn expression(&self, span: crate::Span) -> ast::Expr {
        match self {
            Self::Ident(name) => ast::Expr::Ident(ast::Ident {
                name: name.clone(),
                span,
            }),
            Self::Path(base, remainder) => ast::Expr::Path(ast::Path {
                base: base.clone(),
                remainder: remainder.clone(),
                span,
            }),
        }
    }
}

type Bindings = HashMap<Binding, ast::Expr>;

/// Rewrite a behavior block into SSA-like expressions. Operand names and fixed
/// register paths are entry snapshots; an assignment updates only its named
/// binding, so later statements see that value without corrupting a different
/// operand that aliases the same physical register.
fn sequence_behavior(expr: &ast::Expr, bindings: &mut Bindings) -> ast::Expr {
    match expr {
        ast::Expr::Ident(ident) => bindings
            .get(&Binding::Ident(ident.name.clone()))
            .cloned()
            .unwrap_or_else(|| expr.clone()),
        ast::Expr::Path(path) => bindings
            .get(&Binding::Path(path.base.clone(), path.remainder.clone()))
            .cloned()
            .unwrap_or_else(|| expr.clone()),
        ast::Expr::Assign(assign) => {
            let value = sequence_behavior(&assign.value, bindings);
            if let Some(binding) = Binding::from_destination(&assign.dest) {
                bindings.insert(binding, value.clone());
            }
            ast::Expr::Assign(ast::Assign {
                dest: assign.dest.clone(),
                value: Box::new(value),
                span: assign.span,
            })
        }
        ast::Expr::Binary(binary) => ast::Expr::Binary(ast::Binary {
            lhs: Box::new(sequence_behavior(&binary.lhs, bindings)),
            rhs: Box::new(sequence_behavior(&binary.rhs, bindings)),
            op: binary.op.clone(),
            span: binary.span,
        }),
        ast::Expr::Unary(unary) => ast::Expr::Unary(ast::Unary {
            x: Box::new(sequence_behavior(&unary.x, bindings)),
            op: unary.op.clone(),
            span: unary.span,
        }),
        ast::Expr::Block(block) => ast::Expr::Block(ast::Block {
            stmts: block
                .stmts
                .iter()
                .map(|statement| sequence_behavior(statement, bindings))
                .collect(),
            last_expr_return: block.last_expr_return,
            span: block.span,
        }),
        ast::Expr::Call(call) => ast::Expr::Call(ast::Call {
            callee: call.callee.clone(),
            arguments: call
                .arguments
                .iter()
                .map(|argument| sequence_behavior(argument, bindings))
                .collect(),
            span: call.span,
        }),
        ast::Expr::Field(field) => ast::Expr::Field(ast::Field {
            base: Box::new(sequence_behavior(&field.base, bindings)),
            member: field.member.clone(),
            span: field.span,
        }),
        ast::Expr::If(if_expr) => {
            let condition = sequence_behavior(&if_expr.cond, bindings);
            let entry = bindings.clone();
            let mut then_bindings = entry.clone();
            let then = sequence_behavior(&if_expr.then, &mut then_bindings);
            let mut else_bindings = entry.clone();
            let else_ = if_expr
                .else_
                .as_ref()
                .map(|else_expr| sequence_behavior(else_expr, &mut else_bindings));

            let mut keys = then_bindings.keys().cloned().collect::<Vec<_>>();
            keys.extend(else_bindings.keys().cloned());
            keys.sort_by_key(|key| format!("{key:?}"));
            keys.dedup();
            for key in keys {
                let entry_value = entry
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| key.expression(if_expr.span));
                let then_value = then_bindings
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| entry_value.clone());
                let else_value = else_bindings
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| entry_value.clone());
                let value = if then_value == else_value {
                    then_value
                } else {
                    ast::Expr::If(ast::If {
                        cond: Box::new(condition.clone()),
                        then: Box::new(then_value),
                        else_: Some(Box::new(else_value)),
                        span: if_expr.span,
                    })
                };
                bindings.insert(key, value);
            }

            ast::Expr::If(ast::If {
                cond: Box::new(condition),
                then: Box::new(then),
                else_: else_.map(Box::new),
                span: if_expr.span,
            })
        }
        ast::Expr::IndexAccess(index) => ast::Expr::IndexAccess(ast::IndexAccess {
            base: Box::new(sequence_behavior(&index.base, bindings)),
            index: index.index,
            span: index.span,
        }),
        ast::Expr::Slice(slice) => ast::Expr::Slice(ast::Slice {
            base: Box::new(sequence_behavior(&slice.base, bindings)),
            start: slice.start,
            end: slice.end,
            span: slice.span,
        }),
        ast::Expr::Try(try_expr) => {
            let entry = bindings.clone();
            let mut body_bindings = entry.clone();
            let body = sequence_behavior(&try_expr.body, &mut body_bindings);
            let handlers = try_expr
                .handlers
                .iter()
                .map(|handler| {
                    let mut handler_bindings = entry.clone();
                    if let Some(binding) = &handler.binding {
                        handler_bindings.remove(&Binding::Ident(binding.clone()));
                    }
                    ast::ExceptClause {
                        kind: handler.kind.clone(),
                        binding: handler.binding.clone(),
                        body: sequence_behavior(&handler.body, &mut handler_bindings),
                        span: handler.span,
                    }
                })
                .collect();
            // A caught exception rolls the body back, so no single expression
            // describes post-try bindings. Existing ISA behaviors end the block
            // at the try; retain entry bindings for any following statement.
            *bindings = entry;
            ast::Expr::Try(ast::TryExcept {
                body: Box::new(body),
                handlers,
                span: try_expr.span,
            })
        }
        ast::Expr::Lambda(lambda) => {
            let mut lambda_bindings = bindings.clone();
            for parameter in &lambda.params {
                lambda_bindings.remove(&Binding::Ident(parameter.clone()));
            }
            ast::Expr::Lambda(ast::Lambda {
                params: lambda.params.clone(),
                body: Box::new(sequence_behavior(&lambda.body, &mut lambda_bindings)),
                span: lambda.span,
            })
        }
        ast::Expr::Lit(_) | ast::Expr::BuiltinFunction(_) | ast::Expr::Invalid => expr.clone(),
    }
}

impl BehaviorGraph {
    pub fn effect_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.graph.preorder(self.root)
    }

    pub fn value_roots(&self) -> Vec<NodeId> {
        let mut roots = Vec::new();
        for node in self.effect_nodes() {
            let children: Vec<_> = self.graph.children(node).collect();
            match self.graph.get_node(node) {
                SymKind::StateAssign
                | SymKind::StateStore
                | SymKind::StateStoreConditional
                | SymKind::StateFence
                | SymKind::StateIf => roots.extend(children.first()),
                SymKind::StateTrap => {
                    if let Some(EffectPayload::Trap { argument_count, .. }) =
                        self.effect_payload(node)
                    {
                        roots.extend(children.into_iter().take(*argument_count));
                    }
                }
                _ => {}
            }
        }
        roots
    }

    pub fn effect_payload(&self, node: NodeId) -> Option<&EffectPayload> {
        match self.graph.get_leaf_data(node)? {
            BehaviorPayload::Effect(payload) => Some(payload),
            BehaviorPayload::Value(_) => None,
        }
    }

    /// Copy one value term out for APIs that consume a scalar-only SemGraph.
    pub fn value_graph(&self, root: NodeId) -> Option<(ValueGraph, NodeId)> {
        let mut out = ValueGraph::new();
        let mut remap = HashMap::new();
        for node in self.graph.postorder(root) {
            let kind = *self.graph.get_node(node);
            if matches!(
                kind,
                SymKind::StateAssign
                    | SymKind::StateStore
                    | SymKind::StateStoreConditional
                    | SymKind::StateFence
                    | SymKind::StateTrap
                    | SymKind::StateBlock
                    | SymKind::StateIf
                    | SymKind::StateTry
                    | SymKind::StateHandler
            ) {
                return None;
            }
            let new = out.add_node(kind);
            if let Some(BehaviorPayload::Value(payload)) = self.graph.get_leaf_data(node) {
                out.set_leaf_data(new, payload.clone());
            }
            for child in self.graph.children(node) {
                out.add_edge(new, *remap.get(&child.index())?);
            }
            remap.insert(node.index(), new);
        }
        Some((out, *remap.get(&root.index())?))
    }
}

/// Target printer callbacks used by [`fold_behavior`]. Control-flow traversal
/// and state threading live here; printers only render individual effects and
/// value roots.
pub trait BehaviorEmitter {
    type State: Clone;

    fn assign(
        &self,
        destination: &Destination,
        value: NodeId,
        state: &Self::State,
    ) -> Option<Self::State>;
    fn value_effect(
        &self,
        kind: SymKind,
        value: NodeId,
        state: &Self::State,
    ) -> Option<Self::State>;
    fn trap(
        &self,
        arguments: &[NodeId],
        params: &[String],
        handler: Option<NodeId>,
        state: &Self::State,
        fold: &dyn Fn(NodeId, &Self::State) -> Self::State,
    ) -> Option<Self::State>;
    fn branch(
        &self,
        condition: NodeId,
        entry_state: &Self::State,
        then_state: &Self::State,
        else_state: &Self::State,
    ) -> Self::State;
    fn try_except(
        &self,
        body: NodeId,
        handlers: &[NodeId],
        state: &Self::State,
        fold: &dyn Fn(NodeId, &Self::State) -> Self::State,
    ) -> Option<Self::State>;
    fn unsupported(&self);
}

pub fn fold_behavior<E: BehaviorEmitter>(
    behavior: &BehaviorGraph,
    state: &E::State,
    emitter: &E,
) -> E::State {
    fn fold<E: BehaviorEmitter>(
        behavior: &BehaviorGraph,
        node: NodeId,
        state: &E::State,
        emitter: &E,
    ) -> E::State {
        let unsupported = |value: Option<E::State>| {
            value.unwrap_or_else(|| {
                emitter.unsupported();
                state.clone()
            })
        };
        let children: Vec<_> = behavior.graph.children(node).collect();
        match (
            *behavior.graph.get_node(node),
            behavior.effect_payload(node),
        ) {
            (SymKind::StateAssign, Some(EffectPayload::Assign { destination })) => {
                let Some(&value) = children.first() else {
                    emitter.unsupported();
                    return state.clone();
                };
                unsupported(emitter.assign(destination, value, state))
            }
            (
                kind @ (SymKind::StateStore | SymKind::StateStoreConditional | SymKind::StateFence),
                _,
            ) => {
                let Some(&value) = children.first() else {
                    emitter.unsupported();
                    return state.clone();
                };
                unsupported(emitter.value_effect(kind, value, state))
            }
            (
                SymKind::StateTrap,
                Some(EffectPayload::Trap {
                    params,
                    argument_count,
                }),
            ) => {
                let (arguments, rest) = children.split_at((*argument_count).min(children.len()));
                unsupported(emitter.trap(
                    arguments,
                    params,
                    rest.first().copied(),
                    state,
                    &|n, s| fold(behavior, n, s, emitter),
                ))
            }
            (SymKind::StateBlock, _) => children
                .into_iter()
                .fold(state.clone(), |s, n| fold(behavior, n, &s, emitter)),
            (SymKind::StateIf, _) => {
                let Some((&condition, branches)) = children.split_first() else {
                    emitter.unsupported();
                    return state.clone();
                };
                let Some(&then_node) = branches.first() else {
                    emitter.unsupported();
                    return state.clone();
                };
                let then_state = fold(behavior, then_node, state, emitter);
                let else_state = branches
                    .get(1)
                    .map_or_else(|| state.clone(), |&n| fold(behavior, n, state, emitter));
                emitter.branch(condition, state, &then_state, &else_state)
            }
            (SymKind::StateTry, _) => {
                let Some((&body, handlers)) = children.split_first() else {
                    emitter.unsupported();
                    return state.clone();
                };
                unsupported(
                    emitter
                        .try_except(body, handlers, state, &|n, s| fold(behavior, n, s, emitter)),
                )
            }
            // Handler nodes are entered by `try_except`, never independently.
            _ => {
                emitter.unsupported();
                state.clone()
            }
        }
    }
    fold(behavior, behavior.root, state, emitter)
}

fn destination(
    expr: &ast::Expr,
    register_indices: &HashMap<(String, String), u32>,
) -> Option<Destination> {
    match expr {
        ast::Expr::Ident(i) => Some(Destination::Ident(i.name.clone())),
        ast::Expr::Path(p) => {
            let name = p.remainder.last()?;
            match register_indices.get(&(p.base.clone(), name.clone())) {
                Some(&index) => Some(Destination::FixedRegister {
                    class: p.base.clone(),
                    name: name.clone(),
                    index,
                }),
                None => Some(Destination::Path {
                    base: p.base.clone(),
                    members: p.remainder.clone(),
                }),
            }
        }
        ast::Expr::Field(f) => Some(Destination::Field {
            base: Box::new(destination(&f.base, register_indices)?),
            member: f.member.clone(),
        }),
        _ => None,
    }
}

fn builtin(expr: &ast::Expr) -> Option<ast::BuiltinFunction> {
    let ast::Expr::Call(call) = expr else {
        return None;
    };
    let ast::Expr::BuiltinFunction(kind) = &*call.callee else {
        return None;
    };
    Some(kind.clone())
}

fn collect_values<'a>(
    expr: &'a ast::Expr,
    trap_handler: Option<&'a ast::TrapHandler>,
    out: &mut Vec<&'a ast::Expr>,
) -> Option<()> {
    match expr {
        ast::Expr::Assign(a) => out.push(&a.value),
        ast::Expr::Call(c) => match builtin(expr)? {
            ast::BuiltinFunction::Trap => {
                out.extend(c.arguments.iter());
                if let Some(handler) = trap_handler {
                    collect_values(&handler.body, None, out)?;
                }
            }
            ast::BuiltinFunction::Store
            | ast::BuiltinFunction::StoreConditional
            | ast::BuiltinFunction::Fence
            | ast::BuiltinFunction::FenceI => out.push(expr),
            _ => return None,
        },
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_values(stmt, trap_handler, out)?;
            }
        }
        ast::Expr::If(i) => {
            out.push(&i.cond);
            collect_values(&i.then, trap_handler, out)?;
            if let Some(e) = &i.else_ {
                collect_values(e, trap_handler, out)?;
            }
        }
        ast::Expr::Try(t) => {
            collect_values(&t.body, trap_handler, out)?;
            for handler in &t.handlers {
                collect_values(&handler.body, trap_handler, out)?;
            }
        }
        _ => return None,
    }
    Some(())
}

struct EffectLowerer<'a> {
    graph: UnifiedGraph,
    roots: HashMap<usize, NodeId>,
    _exprs: Vec<&'a ast::Expr>,
    trap_handler: Option<&'a ast::TrapHandler>,
    register_indices: HashMap<(String, String), u32>,
}

impl EffectLowerer<'_> {
    fn add(&mut self, kind: SymKind, payload: EffectPayload, children: &[NodeId]) -> NodeId {
        let node = self.graph.add_node(kind);
        for &child in children {
            self.graph.add_edge(node, child);
        }
        self.graph
            .set_leaf_data(node, BehaviorPayload::Effect(payload));
        node
    }

    fn value(&self, expr: &ast::Expr) -> Option<NodeId> {
        self.roots
            .get(&(expr as *const ast::Expr as usize))
            .copied()
    }

    fn lower(&mut self, expr: &ast::Expr) -> Option<NodeId> {
        match expr {
            ast::Expr::Assign(a) => {
                let payload = EffectPayload::Assign {
                    destination: destination(&a.dest, &self.register_indices)?,
                };
                Some(self.add(SymKind::StateAssign, payload, &[self.value(&a.value)?]))
            }
            ast::Expr::Call(c) => {
                let kind = builtin(expr)?;
                match kind {
                    ast::BuiltinFunction::Trap => {
                        let arguments: Vec<NodeId> = c
                            .arguments
                            .iter()
                            .map(|e| self.value(e))
                            .collect::<Option<_>>()?;
                        let argument_count = arguments.len();
                        let (params, handler) = match self.trap_handler {
                            Some(handler) => {
                                (handler.params.clone(), vec![self.lower(&handler.body)?])
                            }
                            None => (Vec::new(), Vec::new()),
                        };
                        Some(self.add(
                            SymKind::StateTrap,
                            EffectPayload::Trap {
                                params,
                                argument_count,
                            },
                            &[arguments, handler].concat(),
                        ))
                    }
                    ast::BuiltinFunction::Store => Some(self.add(
                        SymKind::StateStore,
                        EffectPayload::None,
                        &[self.value(expr)?],
                    )),
                    ast::BuiltinFunction::StoreConditional => Some(self.add(
                        SymKind::StateStoreConditional,
                        EffectPayload::None,
                        &[self.value(expr)?],
                    )),
                    ast::BuiltinFunction::Fence | ast::BuiltinFunction::FenceI => Some(self.add(
                        SymKind::StateFence,
                        EffectPayload::None,
                        &[self.value(expr)?],
                    )),
                    _ => None,
                }
            }
            ast::Expr::Block(b) => {
                let children = b
                    .stmts
                    .iter()
                    .map(|e| self.lower(e))
                    .collect::<Option<Vec<_>>>()?;
                Some(self.add(SymKind::StateBlock, EffectPayload::None, &children))
            }
            ast::Expr::If(i) => {
                let then_node = self.lower(&i.then)?;
                let mut children = vec![self.value(&i.cond)?, then_node];
                if let Some(e) = &i.else_ {
                    children.push(self.lower(e)?);
                }
                Some(self.add(SymKind::StateIf, EffectPayload::None, &children))
            }
            ast::Expr::Try(t) => {
                let body = self.lower(&t.body)?;
                let mut children = vec![body];
                for h in &t.handlers {
                    let handler_body = self.lower(&h.body)?;
                    children.push(self.add(
                        SymKind::StateHandler,
                        EffectPayload::Handler {
                            kind: h.kind.clone(),
                            binding: h.binding.clone(),
                        },
                        &[handler_body],
                    ));
                }
                Some(self.add(SymKind::StateTry, EffectPayload::None, &children))
            }
            _ => None,
        }
    }
}

pub fn lower_behavior<'a>(
    expr: &'a ast::Expr,
    trap_handler: Option<&'a ast::TrapHandler>,
    params: &HashMap<String, i64>,
    isa_consts: &HashMap<String, i64>,
    register_indices: &HashMap<(String, String), u32>,
) -> Option<BehaviorGraph> {
    let expr = sequence_behavior(expr, &mut HashMap::new());
    let trap_handler = trap_handler.cloned().map(|mut handler| {
        handler.body = sequence_behavior(&handler.body, &mut HashMap::new());
        handler
    });
    let mut exprs = Vec::new();
    collect_values(&expr, trap_handler.as_ref(), &mut exprs)?;
    let mut values = ValueGraph::new();
    let (value_roots, variable_symbols, register_symbols, regnum_symbols) = if exprs.is_empty() {
        (Vec::new(), HashMap::new(), HashMap::new(), HashMap::new())
    } else {
        let (roots, symbols) = ast::Expr::lower_all_to_sema_with_isa(
            &exprs,
            &mut values,
            params,
            isa_consts,
            register_indices,
        )?;
        (
            roots,
            symbols.variable_symbols,
            symbols.register_symbols,
            symbols.regnum_symbols,
        )
    };
    let mut graph = UnifiedGraph::new();
    let mut remap = HashMap::new();
    for &root in &value_roots {
        for node in values.postorder(root) {
            if remap.contains_key(&node.index()) {
                continue;
            }
            let new = graph.add_node(*values.get_node(node));
            if let Some(payload) = values.get_leaf_data(node) {
                graph.set_leaf_data(new, BehaviorPayload::Value(payload.clone()));
            }
            for child in values.children(node) {
                graph.add_edge(new, *remap.get(&child.index())?);
            }
            remap.insert(node.index(), new);
        }
    }
    let roots = exprs
        .iter()
        .zip(value_roots)
        .map(|(e, root)| Some((*e as *const ast::Expr as usize, *remap.get(&root.index())?)))
        .collect::<Option<HashMap<_, _>>>()?;
    let mut lowerer = EffectLowerer {
        graph,
        roots,
        _exprs: exprs,
        trap_handler: trap_handler.as_ref(),
        register_indices: register_indices.clone(),
    };
    let root = lowerer.lower(&expr)?;
    Some(BehaviorGraph {
        graph: lowerer.graph,
        root,
        variable_symbols,
        register_symbols,
        regnum_symbols,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Span;
    use chumsky::span::Span as _;

    fn ident(name: &str) -> ast::Expr {
        ast::Expr::Ident(ast::Ident {
            name: name.to_string(),
            span: Span::new((), 0..0),
        })
    }

    fn int(value: &str) -> ast::Expr {
        ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new(
            value.to_string(),
            Span::new((), 0..0),
        )))
    }

    #[test]
    fn later_reads_use_named_writeback_without_changing_other_operands() {
        let updated = ast::Expr::Binary(ast::Binary {
            lhs: Box::new(ident("rn")),
            rhs: Box::new(int("1")),
            op: ast::BinOp::Add,
            span: Span::new((), 0..0),
        });
        let behavior = ast::Expr::Block(ast::Block {
            stmts: vec![
                ast::Expr::Assign(ast::Assign {
                    dest: Box::new(ident("rn")),
                    value: Box::new(updated.clone()),
                    span: Span::new((), 0..0),
                }),
                ast::Expr::Call(ast::Call {
                    callee: Box::new(ast::Expr::BuiltinFunction(ast::BuiltinFunction::Store)),
                    arguments: vec![ident("rn"), int("8"), ident("rt")],
                    span: Span::new((), 0..0),
                }),
            ],
            last_expr_return: false,
            span: Span::new((), 0..0),
        });

        let ast::Expr::Block(sequenced) = sequence_behavior(&behavior, &mut HashMap::new()) else {
            panic!("block expected")
        };
        let ast::Expr::Call(store) = &sequenced.stmts[1] else {
            panic!("store expected")
        };
        assert_eq!(store.arguments[0], updated);
        assert_eq!(store.arguments[2], ident("rt"));
    }

    #[test]
    fn assignment_rhs_is_reachable_from_state_root() {
        let rhs = ast::Expr::Binary(ast::Binary {
            lhs: Box::new(ident("rs")),
            rhs: Box::new(ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new(
                "1".to_string(),
                Span::new((), 0..0),
            )))),
            op: ast::BinOp::Add,
            span: Span::new((), 0..0),
        });
        let behavior = ast::Expr::Assign(ast::Assign {
            dest: Box::new(ident("rd")),
            value: Box::new(rhs),
            span: Span::new((), 0..0),
        });
        let graph = lower_behavior(
            &behavior,
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        let EffectPayload::Assign { destination } = graph.effect_payload(graph.root).unwrap()
        else {
            panic!("assignment payload expected")
        };
        let value = graph.graph.children(graph.root).next().unwrap();
        assert_eq!(destination, &Destination::Ident("rd".to_string()));
        assert_eq!(graph.graph.get_node(value), &SymKind::Add);
        assert!(graph.graph.preorder(graph.root).any(|node| node == value));
        assert_eq!(graph.graph.preorder(graph.root).count(), graph.graph.len());
        assert_eq!(graph.variable_symbols.get("rs"), Some(&0));
    }

    #[test]
    fn empty_block_lowers_without_a_value_root() {
        let behavior = ast::Expr::Block(ast::Block {
            stmts: Vec::new(),
            last_expr_return: false,
            span: Span::new((), 0..0),
        });
        let graph = lower_behavior(
            &behavior,
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .expect("empty behavior must lower");

        assert_eq!(*graph.graph.get_node(graph.root), SymKind::StateBlock);
        assert_eq!(graph.graph.children(graph.root).count(), 0);
        assert_eq!(graph.graph.len(), 1);
    }
}
