//! Lowering: SMT-LIB terms and scripts into the [`crate::lang`] graph, built
//! bottom-up (post-order). Widths are tracked during construction because
//! `zero_extend`/`sign_extend`/`rotate` need the operand width before the
//! standalone `infer_widths` pass would run.

use std::collections::HashMap;

use tir_adt::{APInt, Dag, NodeId};

use super::{ConvertError, SymbolInfo};
use crate::lang::{SymKind, SymPayload};
use crate::smtlib::ast::*;

/// A graph rooted at the conjunction of all assertions, plus the per-`SymbolId`
/// table and per-node widths.
pub struct Lowered<V> {
    pub graph: Dag<SymKind, SymPayload<V>>,
    pub root: NodeId,
    pub symbols: Vec<SymbolInfo>,
    pub widths: Vec<Option<u32>>,
}

/// Lower a full script: `declare-const`/nullary `declare-fun` are free symbols,
/// `define-fun` is an inlined macro, assertions are conjoined into the root.
pub fn lower_script<V>(script: &Script) -> Result<Lowered<V>, ConvertError> {
    let mut lw = Lowerer::<V>::new();
    let mut assertions = Vec::new();

    for cmd in &script.0 {
        match cmd {
            Command::DeclareConst(name, sort) => {
                lw.decls.insert(name.0.clone(), sort.clone());
            }
            Command::DeclareFun(name, args, ret) if args.is_empty() => {
                lw.decls.insert(name.0.clone(), ret.clone());
            }
            Command::DeclareFun(name, _, _) => {
                return Err(ConvertError::Unsupported(format!(
                    "uninterpreted function `{}` with arguments",
                    name.0
                )));
            }
            Command::DefineFun(def) => {
                lw.defs.insert(def.name.0.clone(), def.clone());
            }
            Command::Assert(term) => {
                let id = lw.lower_term(term)?;
                assertions.push(id);
            }
            _ => {}
        }
    }

    let root = lw.combine_and(&assertions);
    Ok(Lowered {
        graph: lw.graph,
        root,
        symbols: lw.symbol_info,
        widths: lw.width,
    })
}

struct Lowerer<V> {
    graph: Dag<SymKind, SymPayload<V>>,
    width: Vec<Option<u32>>,
    decls: HashMap<String, Sort>,
    defs: HashMap<String, FunctionDef>,
    symbols: HashMap<String, NodeId>,
    symbol_info: Vec<SymbolInfo>,
    scope: Vec<HashMap<String, NodeId>>,
}

impl<V> Lowerer<V> {
    fn new() -> Self {
        Lowerer {
            graph: Dag::new(),
            width: Vec::new(),
            decls: HashMap::new(),
            defs: HashMap::new(),
            symbols: HashMap::new(),
            symbol_info: Vec::new(),
            scope: Vec::new(),
        }
    }

    fn w(&self, id: NodeId) -> Option<u32> {
        self.width[id.index()]
    }

    fn mk(&mut self, kind: SymKind, children: &[NodeId], width: Option<u32>) -> NodeId {
        let id = self.graph.add_node(kind);
        for &c in children {
            self.graph.add_edge(id, c);
        }
        self.width.push(width);
        id
    }

    fn leaf_const(&mut self, width: u32, value: u64) -> NodeId {
        let id = self.graph.add_node(SymKind::Constant);
        self.graph
            .set_leaf_data(id, SymPayload::Int(APInt::new(width, value)));
        self.width.push(Some(width));
        id
    }

    /// Width follows kind: `concat` sums operands, else takes the left operand.
    fn bin(&mut self, kind: SymKind, a: NodeId, b: NodeId) -> NodeId {
        let width = match kind {
            SymKind::Concat => match (self.w(a), self.w(b)) {
                (Some(x), Some(y)) => Some(x + y),
                _ => None,
            },
            _ => self.w(a),
        };
        self.mk(kind, &[a, b], width)
    }

    fn un(&mut self, kind: SymKind, a: NodeId) -> NodeId {
        let width = self.w(a);
        self.mk(kind, &[a], width)
    }

    fn cmp(&mut self, kind: SymKind, a: NodeId, b: NodeId) -> NodeId {
        self.mk(kind, &[a, b], Some(1))
    }

    fn lookup(&self, name: &str) -> Option<NodeId> {
        self.scope
            .iter()
            .rev()
            .find_map(|frame| frame.get(name).copied())
    }

    fn symbol_node(&mut self, name: &str) -> Result<NodeId, ConvertError> {
        if let Some(&id) = self.symbols.get(name) {
            return Ok(id);
        }
        let sort = self
            .decls
            .get(name)
            .ok_or_else(|| ConvertError::UnknownSymbol(name.into()))?
            .clone();
        let (width, is_bool) = sort_width(&sort)?;
        let sid = self.symbol_info.len() as u32;
        let id = self.graph.add_node(SymKind::Symbol);
        self.graph.set_leaf_data(id, SymPayload::SymbolId(sid));
        self.width.push(Some(width));
        self.symbols.insert(name.to_string(), id);
        self.symbol_info.push(SymbolInfo {
            name: name.to_string(),
            width: Some(width),
            is_bool,
        });
        Ok(id)
    }

    fn combine_and(&mut self, ids: &[NodeId]) -> NodeId {
        match ids.split_first() {
            None => self.leaf_const(1, 1),
            Some((&first, rest)) => {
                let mut acc = first;
                for &x in rest {
                    acc = self.bin(SymKind::And, acc, x);
                }
                acc
            }
        }
    }

    fn lower_term(&mut self, term: &Term) -> Result<NodeId, ConvertError> {
        match term {
            Term::Constant(c) => self.lower_constant(c),
            Term::Ident(q) => self.lower_ident(q),
            Term::App(q, args) => self.lower_app(q, args),
            Term::Let(binds, body) => {
                let mut pairs = Vec::with_capacity(binds.len());
                for b in binds {
                    pairs.push((b.var.0.clone(), self.lower_term(&b.term)?));
                }
                self.scope.push(pairs.into_iter().collect());
                let result = self.lower_term(body);
                self.scope.pop();
                result
            }
            Term::Exists(..) => Err(ConvertError::Unsupported("quantifier".into())),
            Term::Annotated(inner, _) => self.lower_term(inner),
        }
    }

    fn lower_constant(&mut self, c: &SpecConstant) -> Result<NodeId, ConvertError> {
        match c {
            SpecConstant::Hexadecimal(s) => {
                let width = checked_width((s.len() * 4) as u128, "hexadecimal literal")?;
                Ok(self.leaf_const(width, parse_radix(s, 16, "hexadecimal")?))
            }
            SpecConstant::Binary(s) => {
                let width = checked_width(s.len() as u128, "binary literal")?;
                Ok(self.leaf_const(width, parse_radix(s, 2, "binary")?))
            }
            SpecConstant::Numeral(_) => Err(ConvertError::Unsupported(
                "integer literal (use bit-vector literals)".into(),
            )),
            SpecConstant::Decimal(_) => Err(ConvertError::Unsupported("decimal literal".into())),
            SpecConstant::String(_) => Err(ConvertError::Unsupported("string literal".into())),
        }
    }

    fn lower_ident(&mut self, q: &QualIdentifier) -> Result<NodeId, ConvertError> {
        let id = q.identifier();
        let name = id.symbol.0.as_str();
        if id.indices.is_empty() {
            if let Some(node) = self.lookup(name) {
                return Ok(node);
            }
            match name {
                "true" => return Ok(self.leaf_const(1, 1)),
                "false" => return Ok(self.leaf_const(1, 0)),
                _ => {}
            }
            if let Some(def) = self.defs.get(name).cloned() {
                if !def.params.is_empty() {
                    return Err(ConvertError::BadArity {
                        op: name.into(),
                        expected: def.params.len().to_string(),
                        got: 0,
                    });
                }
                return self.lower_term(&def.body);
            }
            return self.symbol_node(name);
        }
        if let Some((value, width)) = bv_literal(name, &id.indices) {
            let width = checked_width(width, "bit-vector literal")?;
            return Ok(self.leaf_const(width, value));
        }
        Err(ConvertError::Unsupported(format!(
            "indexed identifier `{name}`"
        )))
    }

    fn lower_app(&mut self, q: &QualIdentifier, args: &[Term]) -> Result<NodeId, ConvertError> {
        let id = q.identifier();
        let name = id.symbol.0.clone();

        if id.is_simple()
            && let Some(def) = self.defs.get(&name).cloned()
        {
            require_arity(&name, args.len(), def.params.len())?;
            let mut nodes = Vec::with_capacity(args.len());
            for t in args {
                nodes.push(self.lower_term(t)?);
            }
            let frame = def
                .params
                .iter()
                .map(|p| p.var.0.clone())
                .zip(nodes)
                .collect();
            self.scope.push(frame);
            let result = self.lower_term(&def.body);
            self.scope.pop();
            return result;
        }

        let mut a = Vec::with_capacity(args.len());
        for t in args {
            a.push(self.lower_term(t)?);
        }

        match id.indices.as_slice() {
            [] => self.lower_op(&name, &a),
            [Index::Numeral(hi), Index::Numeral(lo)] if name == "extract" => {
                self.lower_extract(*hi, *lo, &a)
            }
            [Index::Numeral(k)] if name == "zero_extend" => self.lower_extend(true, *k, &a),
            [Index::Numeral(k)] if name == "sign_extend" => self.lower_extend(false, *k, &a),
            [Index::Numeral(k)] if name == "repeat" => self.lower_repeat(*k, &a),
            [Index::Numeral(k)] if name == "rotate_left" => self.lower_rotate(true, *k, &a),
            [Index::Numeral(k)] if name == "rotate_right" => self.lower_rotate(false, *k, &a),
            _ => Err(ConvertError::Unsupported(format!(
                "`{name}` with these indices"
            ))),
        }
    }

    fn lower_op(&mut self, name: &str, a: &[NodeId]) -> Result<NodeId, ConvertError> {
        use SymKind::*;
        match name {
            "and" => self.fold(And, a, "and"),
            "or" => self.fold(Or, a, "or"),
            "xor" => self.fold(Xor, a, "xor"),
            "not" => self.unary(Not, a, "not"),
            "=>" => self.implies(a),
            "=" => self.chain_eq(a),
            "distinct" => self.distinct(a),
            "ite" => {
                require_arity("ite", a.len(), 3)?;
                let width = self.w(a[1]);
                Ok(self.mk(If, &[a[0], a[1], a[2]], width))
            }

            "bvand" => self.fold(And, a, "bvand"),
            "bvor" => self.fold(Or, a, "bvor"),
            "bvxor" => self.fold(Xor, a, "bvxor"),
            "bvnot" => self.unary(Not, a, "bvnot"),
            "bvnand" => self.binary_not(And, a, "bvnand"),
            "bvnor" => self.binary_not(Or, a, "bvnor"),
            "bvxnor" => self.binary_not(Xor, a, "bvxnor"),

            "bvadd" => self.fold(Add, a, "bvadd"),
            "bvmul" => self.fold(Mul, a, "bvmul"),
            "bvsub" => self.binary(Sub, a, "bvsub"),
            "bvneg" => self.unary(Neg, a, "bvneg"),
            "bvudiv" => self.binary(UDiv, a, "bvudiv"),
            "bvurem" => self.binary(URem, a, "bvurem"),
            "bvsdiv" => self.binary(Div, a, "bvsdiv"),
            "bvsrem" => self.binary(SRem, a, "bvsrem"),
            "bvsmod" => Err(ConvertError::Unsupported(
                "bvsmod (no signed-modulo node)".into(),
            )),

            "bvshl" => self.binary(ShiftLeft, a, "bvshl"),
            "bvlshr" => self.binary(ShiftRightLogic, a, "bvlshr"),
            "bvashr" => self.binary(ShiftRightArithmetic, a, "bvashr"),

            "bvult" => self.compare(ULt, a, "bvult"),
            "bvule" => self.compare(ULe, a, "bvule"),
            "bvugt" => self.compare(UGt, a, "bvugt"),
            "bvuge" => self.compare(UGe, a, "bvuge"),
            "bvslt" => self.compare(Lt, a, "bvslt"),
            "bvsle" => self.compare(Le, a, "bvsle"),
            "bvsgt" => self.compare(Gt, a, "bvsgt"),
            "bvsge" => self.compare(Ge, a, "bvsge"),
            "bvcomp" => self.compare(Eq, a, "bvcomp"),

            "concat" => self.fold(Concat, a, "concat"),

            _ => Err(ConvertError::Unsupported(format!("operator `{name}`"))),
        }
    }

    fn fold(&mut self, kind: SymKind, a: &[NodeId], name: &str) -> Result<NodeId, ConvertError> {
        let (&first, rest) = a.split_first().ok_or_else(|| ConvertError::BadArity {
            op: name.into(),
            expected: "at least 1".into(),
            got: 0,
        })?;
        let mut acc = first;
        for &x in rest {
            acc = self.bin(kind, acc, x);
        }
        Ok(acc)
    }

    fn binary(&mut self, kind: SymKind, a: &[NodeId], name: &str) -> Result<NodeId, ConvertError> {
        require_arity(name, a.len(), 2)?;
        Ok(self.bin(kind, a[0], a[1]))
    }

    fn binary_not(
        &mut self,
        kind: SymKind,
        a: &[NodeId],
        name: &str,
    ) -> Result<NodeId, ConvertError> {
        require_arity(name, a.len(), 2)?;
        let inner = self.bin(kind, a[0], a[1]);
        Ok(self.un(SymKind::Not, inner))
    }

    fn unary(&mut self, kind: SymKind, a: &[NodeId], name: &str) -> Result<NodeId, ConvertError> {
        require_arity(name, a.len(), 1)?;
        Ok(self.un(kind, a[0]))
    }

    fn compare(&mut self, kind: SymKind, a: &[NodeId], name: &str) -> Result<NodeId, ConvertError> {
        require_arity(name, a.len(), 2)?;
        Ok(self.cmp(kind, a[0], a[1]))
    }

    fn implies(&mut self, a: &[NodeId]) -> Result<NodeId, ConvertError> {
        let (&last, init) = a.split_last().ok_or_else(|| ConvertError::BadArity {
            op: "=>".into(),
            expected: "at least 1".into(),
            got: 0,
        })?;
        let mut acc = last;
        for &p in init.iter().rev() {
            let not_p = self.un(SymKind::Not, p);
            acc = self.bin(SymKind::Or, not_p, acc);
        }
        Ok(acc)
    }

    fn chain_eq(&mut self, a: &[NodeId]) -> Result<NodeId, ConvertError> {
        require_min_arity("=", a.len(), 2)?;
        let mut eqs = Vec::with_capacity(a.len() - 1);
        for pair in a.windows(2) {
            eqs.push(self.cmp(SymKind::Eq, pair[0], pair[1]));
        }
        Ok(self.combine_and(&eqs))
    }

    fn distinct(&mut self, a: &[NodeId]) -> Result<NodeId, ConvertError> {
        require_min_arity("distinct", a.len(), 2)?;
        let mut nes = Vec::new();
        for i in 0..a.len() {
            for j in (i + 1)..a.len() {
                nes.push(self.cmp(SymKind::Ne, a[i], a[j]));
            }
        }
        Ok(self.combine_and(&nes))
    }

    fn lower_extract(&mut self, hi: u128, lo: u128, a: &[NodeId]) -> Result<NodeId, ConvertError> {
        require_arity("extract", a.len(), 1)?;
        if hi < lo {
            return Err(ConvertError::Unsupported(format!(
                "extract high {hi} below low {lo}"
            )));
        }
        if let Some(w) = self.w(a[0])
            && hi >= w as u128
        {
            return Err(ConvertError::Unsupported(format!(
                "extract high {hi} out of range for width {w}"
            )));
        }
        let high = self.leaf_const(32, hi as u64);
        let low = self.leaf_const(32, lo as u64);
        let width = (hi - lo + 1) as u32;
        Ok(self.mk(SymKind::Extract, &[a[0], high, low], Some(width)))
    }

    fn lower_extend(&mut self, zero: bool, k: u128, a: &[NodeId]) -> Result<NodeId, ConvertError> {
        let op = if zero { "zero_extend" } else { "sign_extend" };
        require_arity(op, a.len(), 1)?;
        let operand_width = self
            .w(a[0])
            .ok_or_else(|| ConvertError::UnknownWidth(op.into()))?;
        let k = u32::try_from(k)
            .ok()
            .and_then(|k| operand_width.checked_add(k))
            .filter(|target| *target <= MAX_WIDTH);
        let target = k.ok_or_else(|| {
            ConvertError::Unsupported(format!("{op} produces a width above {MAX_WIDTH}"))
        })?;
        let target_const = self.leaf_const(32, target as u64);
        let kind = if zero { SymKind::ZExt } else { SymKind::SExt };
        Ok(self.mk(kind, &[a[0], target_const], Some(target)))
    }

    fn lower_repeat(&mut self, k: u128, a: &[NodeId]) -> Result<NodeId, ConvertError> {
        require_arity("repeat", a.len(), 1)?;
        if k == 0 {
            return Err(ConvertError::Unsupported("repeat count 0".into()));
        }
        let mut acc = a[0];
        for _ in 1..k {
            acc = self.bin(SymKind::Concat, acc, a[0]);
        }
        Ok(acc)
    }

    fn lower_rotate(&mut self, left: bool, k: u128, a: &[NodeId]) -> Result<NodeId, ConvertError> {
        let op = if left { "rotate_left" } else { "rotate_right" };
        require_arity(op, a.len(), 1)?;
        let width = self
            .w(a[0])
            .ok_or_else(|| ConvertError::UnknownWidth(op.into()))?;
        if width == 0 {
            return Err(ConvertError::Unsupported(format!(
                "{op} of zero-width value"
            )));
        }
        let shift = (k % width as u128) as u32;
        if shift == 0 {
            return Ok(a[0]);
        }
        let by = self.leaf_const(width, shift as u64);
        let complement = self.leaf_const(width, (width - shift) as u64);
        let (lo, hi) = if left {
            (
                self.bin(SymKind::ShiftLeft, a[0], by),
                self.bin(SymKind::ShiftRightLogic, a[0], complement),
            )
        } else {
            (
                self.bin(SymKind::ShiftRightLogic, a[0], by),
                self.bin(SymKind::ShiftLeft, a[0], complement),
            )
        };
        Ok(self.bin(SymKind::Or, lo, hi))
    }
}

/// The widest bit-vector the `u64`-backed `APInt` can represent.
const MAX_WIDTH: u32 = 64;

/// Returns `(width, is_bool)`: `Bool` -> `(1, true)`, `(_ BitVec n)` -> `(n, false)`.
fn sort_width(sort: &Sort) -> Result<(u32, bool), ConvertError> {
    if !sort.params.is_empty() {
        return Err(ConvertError::Unsupported(format!(
            "parametric sort `{sort}`"
        )));
    }
    match sort.id.symbol.0.as_str() {
        "Bool" if sort.id.indices.is_empty() => Ok((1, true)),
        "BitVec" => match sort.id.indices.as_slice() {
            [Index::Numeral(n)] => Ok((checked_width(*n, "BitVec sort")?, false)),
            _ => Err(ConvertError::Unsupported("BitVec without a width".into())),
        },
        other => Err(ConvertError::Unsupported(format!("sort `{other}`"))),
    }
}

/// Recognise a `(_ bvN m)` bit-vector literal, returning `(value, width)`.
fn bv_literal(name: &str, indices: &[Index]) -> Option<(u64, u128)> {
    let digits = name.strip_prefix("bv")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match indices {
        [Index::Numeral(width)] => Some((digits.parse().ok()?, *width)),
        _ => None,
    }
}

/// Reject widths outside `1..=MAX_WIDTH` before they reach `APInt::new` (panic).
fn checked_width(width: u128, what: &str) -> Result<u32, ConvertError> {
    if width == 0 || width > MAX_WIDTH as u128 {
        Err(ConvertError::BadLiteral(format!(
            "{what} width {width} must be between 1 and {MAX_WIDTH}"
        )))
    } else {
        Ok(width as u32)
    }
}

fn parse_radix(s: &str, radix: u32, what: &str) -> Result<u64, ConvertError> {
    u64::from_str_radix(s, radix)
        .map_err(|_| ConvertError::BadLiteral(format!("{what} `{s}` exceeds 64 bits")))
}

fn require_arity(op: &str, got: usize, expected: usize) -> Result<(), ConvertError> {
    if got == expected {
        Ok(())
    } else {
        Err(ConvertError::BadArity {
            op: op.into(),
            expected: expected.to_string(),
            got,
        })
    }
}

fn require_min_arity(op: &str, got: usize, min: usize) -> Result<(), ConvertError> {
    if got >= min {
        Ok(())
    } else {
        Err(ConvertError::BadArity {
            op: op.into(),
            expected: format!("at least {min}"),
            got,
        })
    }
}
