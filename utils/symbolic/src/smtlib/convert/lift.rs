//! Lifting: the [`crate::lang`] graph back into SMT-LIB AST. The graph blurs
//! Bool and 1-bit bit-vectors, so a structural pass (see [`Lifter::is_bool`])
//! decides which nodes are boolean and boolean context flows down to pick
//! `and`/`true` vs `bvand`/`#b1`.

use std::collections::{HashMap, HashSet};

use tir_graph::{Dag, NodeId};

use super::{ConvertError, SymbolInfo};
use crate::lang::{SymKind, SymPayload, infer_widths};
use crate::smtlib::ast::*;

/// Lift the graph rooted at `root` into a single term.
pub fn lift_term<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    root: NodeId,
    symbols: &[SymbolInfo],
) -> Result<Term, ConvertError> {
    let mut lifter = Lifter::new(graph, symbols);
    let ctx = lifter.is_bool(root);
    Ok(lifter.lift(root, ctx)?.0)
}

/// Lift the graph into a `declare-const`s + `(assert root)` script. `assert` is
/// a boolean position, so the root is lifted in boolean context.
pub fn lift_script<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    root: NodeId,
    symbols: &[SymbolInfo],
) -> Result<Script, ConvertError> {
    let mut lifter = Lifter::new(graph, symbols);
    let (term, _) = lifter.lift(root, true)?;

    let mut ids: Vec<u32> = lifter.used.into_iter().collect();
    ids.sort_unstable();
    let mut commands = Vec::with_capacity(ids.len() + 1);
    for sid in ids {
        let info = &symbols[sid as usize];
        let sort = if info.is_bool {
            bool_sort()
        } else {
            let width = info
                .width
                .ok_or_else(|| ConvertError::UnknownWidth(format!("symbol `{}`", info.name)))?;
            bitvec_sort(width)
        };
        commands.push(Command::DeclareConst(Symbol(info.name.clone()), sort));
    }
    commands.push(Command::Assert(term));
    Ok(Script(commands))
}

struct Lifter<'a, G> {
    graph: &'a G,
    symbols: &'a [SymbolInfo],
    widths: Vec<Option<u32>>,
    /// Cache of structural boolean-ness, independent of context.
    bool_cache: HashMap<NodeId, bool>,
    /// Lifted terms, keyed by node and the boolean context it was lifted in.
    memo: HashMap<(NodeId, bool), (Term, bool)>,
    used: HashSet<u32>,
}

impl<'a, V, G> Lifter<'a, G>
where
    G: Dag<Node = SymKind, Leaf = SymPayload<V>>,
{
    fn new(graph: &'a G, symbols: &'a [SymbolInfo]) -> Self {
        let widths = infer_widths(graph, |id| match graph.get_leaf_data(id) {
            Some(SymPayload::SymbolId(sid)) => symbols.get(*sid as usize).and_then(|s| s.width),
            _ => None,
        });
        Lifter {
            graph,
            symbols,
            widths,
            bool_cache: HashMap::new(),
            memo: HashMap::new(),
            used: HashSet::new(),
        }
    }

    fn children(&self, id: NodeId) -> Vec<NodeId> {
        self.graph.children(id).collect()
    }

    /// Whether `id` *definitely* denotes a boolean; a neutral 1-bit constant
    /// adapts to context and so returns `false`.
    fn is_bool(&mut self, id: NodeId) -> bool {
        if let Some(&b) = self.bool_cache.get(&id) {
            return b;
        }
        use SymKind::*;
        let b = match *self.graph.get_kind(id) {
            Eq | Ne | Lt | Le | Gt | Ge | ULt | ULe | UGt | UGe => true,
            And | Or | Xor => self.children(id).into_iter().any(|c| self.is_bool(c)),
            Not => self.children(id).first().is_some_and(|&c| self.is_bool(c)),
            If => {
                let ch = self.children(id);
                ch.len() == 3 && (self.is_bool(ch[1]) || self.is_bool(ch[2]))
            }
            Symbol => matches!(
                self.graph.get_leaf_data(id),
                Some(SymPayload::SymbolId(sid)) if self.symbols.get(*sid as usize).is_some_and(|s| s.is_bool)
            ),
            _ => false,
        };
        self.bool_cache.insert(id, b);
        b
    }

    fn const_value(&self, id: NodeId) -> Result<u64, ConvertError> {
        match self.graph.get_leaf_data(id) {
            Some(SymPayload::Int(v)) => Ok(v.to_u64()),
            _ => Err(ConvertError::Unsupported(
                "expected a constant operand".into(),
            )),
        }
    }

    fn lift(&mut self, id: NodeId, bool_ctx: bool) -> Result<(Term, bool), ConvertError> {
        if let Some(cached) = self.memo.get(&(id, bool_ctx)) {
            return Ok(cached.clone());
        }
        let result = self.lift_uncached(id, bool_ctx)?;
        self.memo.insert((id, bool_ctx), result.clone());
        Ok(result)
    }

    fn lift_uncached(&mut self, id: NodeId, bool_ctx: bool) -> Result<(Term, bool), ConvertError> {
        use SymKind::*;
        match *self.graph.get_kind(id) {
            Symbol => {
                let sid = match self.graph.get_leaf_data(id) {
                    Some(SymPayload::SymbolId(s)) => *s,
                    _ => return Err(ConvertError::Unsupported("symbol without an id".into())),
                };
                let info = self
                    .symbols
                    .get(sid as usize)
                    .ok_or_else(|| ConvertError::UnknownSymbol(format!("symbol id {sid}")))?;
                self.used.insert(sid);
                Ok((ident(&info.name), info.is_bool))
            }
            Constant => self.lift_constant(id, bool_ctx),

            And => self.logical("and", "bvand", id),
            Or => self.logical("or", "bvor", id),
            Xor => self.logical("xor", "bvxor", id),
            Not => self.logical_unary("not", "bvnot", id),

            Eq => self.equality(id, false),
            Ne => self.equality(id, true),
            Lt => self.ordered("bvslt", id),
            Le => self.ordered("bvsle", id),
            Gt => self.ordered("bvsgt", id),
            Ge => self.ordered("bvsge", id),
            ULt => self.ordered("bvult", id),
            ULe => self.ordered("bvule", id),
            UGt => self.ordered("bvugt", id),
            UGe => self.ordered("bvuge", id),

            Add => self.bv_app("bvadd", id),
            Sub => self.bv_app("bvsub", id),
            Mul => self.bv_app("bvmul", id),
            Div => self.bv_app("bvsdiv", id),
            UDiv => self.bv_app("bvudiv", id),
            SRem => self.bv_app("bvsrem", id),
            URem => self.bv_app("bvurem", id),
            Neg => self.bv_app("bvneg", id),
            ShiftLeft => self.bv_app("bvshl", id),
            ShiftRightLogic => self.bv_app("bvlshr", id),
            ShiftRightArithmetic => self.bv_app("bvashr", id),
            Concat => self.bv_app("concat", id),
            Bitcast => self.lift(self.children(id)[0], bool_ctx),

            If => self.ite(id),
            Extract => self.extract(id),
            ZExt => self.extend("zero_extend", id),
            SExt => self.extend("sign_extend", id),

            other => Err(ConvertError::Unsupported(format!("node `{other:?}`"))),
        }
    }

    fn lift_constant(&self, id: NodeId, bool_ctx: bool) -> Result<(Term, bool), ConvertError> {
        let (value, width) = match self.graph.get_leaf_data(id) {
            Some(SymPayload::Int(v)) => (v.to_u64(), v.width()),
            _ => return Err(ConvertError::Unsupported("non-integer constant".into())),
        };
        if bool_ctx && width == 1 {
            Ok((bool_const(value != 0), true))
        } else {
            Ok((bv_const(value, width), false))
        }
    }

    /// Lift every child in non-boolean context and apply `op`; `result_bool`
    /// is the sort of the result (boolean for comparisons, else bit-vector).
    fn nary(
        &mut self,
        op: &str,
        id: NodeId,
        result_bool: bool,
    ) -> Result<(Term, bool), ConvertError> {
        let mut terms = Vec::new();
        for child in self.children(id) {
            terms.push(self.lift(child, false)?.0);
        }
        Ok((app(op, terms), result_bool))
    }

    fn bv_app(&mut self, op: &str, id: NodeId) -> Result<(Term, bool), ConvertError> {
        self.nary(op, id, false)
    }

    fn logical(
        &mut self,
        bool_op: &str,
        bv_op: &str,
        id: NodeId,
    ) -> Result<(Term, bool), ConvertError> {
        let node_bool = self.is_bool(id);
        let mut terms = Vec::new();
        for child in self.children(id) {
            terms.push(self.lift(child, node_bool)?.0);
        }
        let op = if node_bool { bool_op } else { bv_op };
        Ok((app(op, terms), node_bool))
    }

    fn logical_unary(
        &mut self,
        bool_op: &str,
        bv_op: &str,
        id: NodeId,
    ) -> Result<(Term, bool), ConvertError> {
        let node_bool = self.is_bool(id);
        let child = first_child(self.graph, id)?;
        let term = self.lift(child, node_bool)?.0;
        let op = if node_bool { bool_op } else { bv_op };
        Ok((app(op, vec![term]), node_bool))
    }

    /// `=` / `(not (= ..))`; operand context is boolean iff any operand is.
    fn equality(&mut self, id: NodeId, negate: bool) -> Result<(Term, bool), ConvertError> {
        let children = self.children(id);
        let operand_ctx = children.iter().any(|&c| self.is_bool(c));
        let mut terms = Vec::with_capacity(children.len());
        for child in children {
            terms.push(self.lift(child, operand_ctx)?.0);
        }
        let eq = app("=", terms);
        let term = if negate { app("not", vec![eq]) } else { eq };
        Ok((term, true))
    }

    /// Signed/unsigned ordered comparisons take bit-vector operands.
    fn ordered(&mut self, op: &str, id: NodeId) -> Result<(Term, bool), ConvertError> {
        self.nary(op, id, true)
    }

    fn ite(&mut self, id: NodeId) -> Result<(Term, bool), ConvertError> {
        let children = self.children(id);
        if children.len() != 3 {
            return Err(ConvertError::Unsupported("ite without 3 operands".into()));
        }
        let node_bool = self.is_bool(id);
        let cond = self.lift(children[0], true)?.0;
        let then = self.lift(children[1], node_bool)?.0;
        let other = self.lift(children[2], node_bool)?.0;
        Ok((app("ite", vec![cond, then, other]), node_bool))
    }

    fn extract(&mut self, id: NodeId) -> Result<(Term, bool), ConvertError> {
        let children = self.children(id);
        if children.len() != 3 {
            return Err(ConvertError::Unsupported(
                "extract without 3 operands".into(),
            ));
        }
        let high = self.const_value(children[1])?;
        let low = self.const_value(children[2])?;
        let value = self.lift(children[0], false)?.0;
        Ok((
            indexed_app(
                "extract",
                vec![Index::Numeral(high as u128), Index::Numeral(low as u128)],
                vec![value],
            ),
            false,
        ))
    }

    fn extend(&mut self, op: &str, id: NodeId) -> Result<(Term, bool), ConvertError> {
        let children = self.children(id);
        if children.len() != 2 {
            return Err(ConvertError::Unsupported(format!(
                "{op} without 2 operands"
            )));
        }
        let operand_width = self.widths[children[0].index()]
            .ok_or_else(|| ConvertError::UnknownWidth(op.into()))?;
        let target = self.const_value(children[1])? as u32;
        let added = target.checked_sub(operand_width).ok_or_else(|| {
            ConvertError::Unsupported(format!(
                "{op} target width {target} below operand width {operand_width}"
            ))
        })?;
        let value = self.lift(children[0], false)?.0;
        Ok((
            indexed_app(op, vec![Index::Numeral(added as u128)], vec![value]),
            false,
        ))
    }
}

fn first_child<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    id: NodeId,
) -> Result<NodeId, ConvertError> {
    graph
        .children(id)
        .next()
        .ok_or_else(|| ConvertError::Unsupported("unary node without an operand".into()))
}

fn ident(name: &str) -> Term {
    Term::Ident(QualIdentifier::Plain(Identifier::simple(name)))
}

fn bool_const(value: bool) -> Term {
    ident(if value { "true" } else { "false" })
}

fn bv_const(value: u64, width: u32) -> Term {
    Term::Ident(QualIdentifier::Plain(Identifier {
        symbol: Symbol(format!("bv{value}")),
        indices: vec![Index::Numeral(width as u128)],
    }))
}

fn app(op: &str, args: Vec<Term>) -> Term {
    Term::App(QualIdentifier::Plain(Identifier::simple(op)), args)
}

fn indexed_app(op: &str, indices: Vec<Index>, args: Vec<Term>) -> Term {
    Term::App(
        QualIdentifier::Plain(Identifier {
            symbol: Symbol(op.into()),
            indices,
        }),
        args,
    )
}

fn bitvec_sort(width: u32) -> Sort {
    Sort::simple(Identifier {
        symbol: Symbol("BitVec".into()),
        indices: vec![Index::Numeral(width as u128)],
    })
}

fn bool_sort() -> Sort {
    Sort::simple(Identifier::simple("Bool"))
}
