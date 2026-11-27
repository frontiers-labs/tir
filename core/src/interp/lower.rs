use std::collections::BTreeMap;
use std::fmt;

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/interp.rs"));
}

use generated::{Class, Index, OperationId};
type Label = Class<generated::Label>;

use super::*;
use crate::attributes::AttributeValue;
use crate::{Context, OpId, Operation, TypeId, ValueId, func::FuncOp};

/// Failure to represent a TIR operation or verify its resulting program.
#[derive(Debug)]
pub enum BuildError {
    Rules(tir_egglog::Error),
    LoadRules(tir_egglog::LoadError),
    Unsupported(String),
    InvalidProgram(VerifyError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadRules(err) => err.fmt(f),
            Self::Rules(err) => err.fmt(f),
            Self::Unsupported(reason) => f.write_str(reason),
            Self::InvalidProgram(err) => write!(f, "{err:?}"),
        }
    }
}
impl std::error::Error for BuildError {}
impl From<tir_egglog::Error> for BuildError {
    fn from(err: tir_egglog::Error) -> Self {
        Self::Rules(err)
    }
}
impl From<Box<dyn std::error::Error>> for BuildError {
    fn from(err: Box<dyn std::error::Error>) -> Self {
        let err = match err.downcast::<tir_egglog::LoadError>() {
            Ok(load) => return Self::LoadRules(*load),
            Err(err) => err,
        };
        match err.downcast::<tir_egglog::Error>() {
            Ok(rules) => Self::Rules(*rules),
            Err(err) => Self::Unsupported(err.to_string()),
        }
    }
}
type Result<T> = std::result::Result<T, BuildError>;

/// Compile a TIR function and its direct callees. The entry function is Func(0).
pub fn build(entry: &FuncOp) -> Result<Program> {
    let ctx = &entry.handle().context;
    crate::check_types(ctx).map_err(|err| BuildError::Unsupported(err.to_string()))?;
    let mut db = ctx.eqsat_database()?;
    let mut rules = generated::Program::load(&mut db)?;
    let root = OperationId::from_raw(entry.id().number());
    rules.insert_interp_root((root,))?;
    rules.compile()?;
    rules.rebuild()?;
    if let Some((id, kind)) = rules.error_op().next().transpose()? {
        let mut detail = kind.as_str().to_owned();
        if detail == "ptr.load" {
            detail = "load result must be an integer".into();
            for row in rules.bad_load_attr() {
                let (load, name) = row?;
                if load == id {
                    detail = format!("load attribute {name}");
                    break;
                }
            }
        }
        return Err(BuildError::Unsupported(format!(
            "unsupported {detail} at operation {}",
            id.raw()
        )));
    }
    let ix = Indexes::new(&mut rules);
    let mut entries = rules
        .entry_classes(tir_egglog::Color::BASE)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|&(f, _)| (f != root, f));
    if entries.first().map(|r| r.0) != Some(root) {
        return Err(BuildError::Unsupported(
            "function has no representable body".into(),
        ));
    }
    let functions = entries
        .iter()
        .enumerate()
        .map(|(i, &(f, _))| (f, Func(i as u32)))
        .collect();
    let mut builder = Builder::default();
    for (function, entry) in entries {
        builder.functions.push(materialize(
            &rules,
            ctx,
            &ix,
            &functions,
            function,
            entry,
            &mut builder.constants,
        )?);
    }
    builder.build().map_err(BuildError::InvalidProgram)
}

struct Indexes {
    owned: Index<OperationId, generated::ReachBlockClassRow>,
    params: Index<Label, generated::OutParamClassRow>,
    code: Index<Label, generated::EmitClassRow>,
    cases: Index<Label, generated::CaseBitsClassRow>,
    flow: Index<Label, generated::FlowClassRow>,
    args: Index<Label, generated::OutArgClassRow>,
    calls: Index<OperationId, generated::ArgRow>,
}

impl Indexes {
    fn new(rules: &mut generated::Program<'_>) -> Self {
        Self {
            owned: rules.index_reach_block_by_function(),
            params: rules.index_out_param_by_block(),
            code: rules.index_emit_by_block(),
            cases: rules.index_case_bits_by_block(),
            flow: rules.index_flow_by_block(),
            args: rules.index_out_arg_by_block(),
            calls: rules.index_arg_by_operation(),
        }
    }
}

fn width(ctx: &Context, ty: TypeId) -> Result<u32> {
    ctx.terms()
        .int_width(ty)
        .and_then(|w| u32::try_from(w).ok())
        .filter(|&w| w > 0)
        .ok_or_else(|| {
            BuildError::Unsupported(format!(
                "expected a VM integer, found {}",
                ctx.type_to_string(ty)
            ))
        })
}

fn ordered<R, K: Ord>(
    rows: impl Iterator<Item = std::result::Result<R, Box<dyn std::error::Error>>>,
    key: impl FnMut(&R) -> K,
) -> Result<Vec<R>> {
    let mut rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    rows.sort_by_key(key);
    Ok(rows)
}

struct Slots<'a> {
    ctx: &'a Context,
    temp_types: BTreeMap<i64, i64>,
    pointer_width: Option<u32>,
    numbers: BTreeMap<i64, Reg>,
    widths: Vec<u32>,
}

impl Slots<'_> {
    fn reg(&mut self, v: i64) -> Result<Reg> {
        if let Some(r) = self.numbers.get(&v) {
            return Ok(*r);
        }
        let like = self.temp_types.get(&v).copied().unwrap_or(v);
        let w = if like == -1 {
            1
        } else {
            let id = u32::try_from(like).map_err(|_| {
                BuildError::Unsupported(format!("missing type for virtual slot {v}"))
            })?;
            let ty = self.ctx.get_value(ValueId::from_number(id)).ty();
            if crate::ptr::PtrType::view(self.ctx, ty).is_some() {
                self.pointer_width.ok_or_else(|| {
                    BuildError::Unsupported(
                        "pointer argument needs a layout with a pointer width".into(),
                    )
                })?
            } else {
                width(self.ctx, ty)?
            }
        };
        let r = Reg(self.widths.len() as u32);
        self.widths.push(w);
        self.numbers.insert(v, r);
        Ok(r)
    }
}

fn materialize(
    rules: &generated::Program<'_>,
    ctx: &Context,
    ix: &Indexes,
    functions: &BTreeMap<OperationId, Func>,
    function: OperationId,
    entry: Label,
    constants: &mut Vec<APInt>,
) -> Result<Function> {
    let op = |k: OperationId| ctx.get_op(OpId::from_number(k.raw()));
    let Some(AttributeValue::Type(ty)) = op(function).attr("ret_type") else {
        return Err(BuildError::Unsupported(
            "expected a single integer return type".into(),
        ));
    };
    let result_width = width(ctx, ty)?;
    let mut labels: Vec<_> = rules
        .listed(&ix.owned, function)?
        .map(|r| r.map(|(_, label)| label))
        .collect::<std::result::Result<_, _>>()?;
    labels.sort_by_key(|&l| (l != entry, l));
    let block_ids: BTreeMap<_, _> = labels
        .iter()
        .enumerate()
        .map(|(i, &l)| (l, i as u32))
        .collect();
    let mut slots = Slots {
        ctx,
        temp_types: rules.temp().collect::<std::result::Result<_, _>>()?,
        pointer_width: crate::layout::DataLayout::for_op(ctx, op(function).id)
            .and_then(|l| l.pointer_size()),
        numbers: BTreeMap::new(),
        widths: Vec::new(),
    };
    let mut f = Function {
        result_width,
        widths: vec![],
        blocks: vec![],
        args: vec![],
        edges: vec![],
    };
    for label in labels {
        let params = ordered(rules.listed(&ix.params, label)?, |r| (r.1, r.2))?
            .into_iter()
            .map(|(_, _, _, value)| slots.reg(value))
            .collect::<Result<Vec<_>>>()?;
        if label == entry {
            let function_op = op(function)
                .as_op::<FuncOp>()
                .ok_or_else(|| BuildError::Unsupported("expected a function definition".into()))?;
            if function_op.has_result_address() || function_op.parameters().len() != params.len() {
                return Err(BuildError::Unsupported(format!(
                    "unrepresentable entry parameters for function {}",
                    function.raw()
                )));
            }
        }
        let mut targets = BTreeMap::new();
        let arguments = ordered(rules.listed(&ix.args, label)?, |r| (r.2, r.3))?;
        for flow in rules.listed(&ix.flow, label)? {
            let (_, side, target) = flow?;
            let arguments = arguments
                .iter()
                .filter(|r| r.1 == side)
                .map(|r| slots.reg(r.4))
                .collect::<Result<Vec<_>>>()?;
            let dest = *block_ids
                .get(&target)
                .ok_or_else(|| BuildError::Unsupported("edge leaves function".into()))?;
            targets.insert(side, EdgeId(f.edges.len() as u32));
            f.edges.push(Edge {
                block: dest,
                args: Args(f.args.len() as u32),
            });
            f.args.push(arguments);
        }
        let edge = |i| {
            targets
                .get(&i)
                .copied()
                .ok_or_else(|| BuildError::Unsupported(format!("missing edge {i}")))
        };
        let mut code = Vec::new();
        for (_, _, _, operation, kind, output, lhs, rhs) in
            ordered(rules.listed(&ix.code, label)?, |r| (r.1, r.2))?
        {
            let inst = match kind.as_str() {
                kind @ ("constant" | "zero" | "one" | "signbit" | "case_literal") => {
                    let dst = slots.reg(output)?;
                    let w = u64::from(slots.widths[dst.0 as usize]);
                    let bits = match kind {
                        "case_literal" => {
                            let (_, index, text) = rules
                                .listed(&ix.cases, label)?
                                .next()
                                .transpose()?
                                .ok_or_else(|| {
                                    BuildError::Unsupported("missing switch literal".into())
                                })?;
                            let case = if text.is_empty() {
                                use crate::CaseGuard;
                                op(operation)
                                    .as_op::<crate::cfg::SwitchOp>()
                                    .and_then(|s| s.cases().get(index as usize).copied())
                            } else {
                                text.parse().ok()
                            }
                            .ok_or_else(|| {
                                BuildError::Unsupported("invalid switch literal".into())
                            })?;
                            APInt::new(w, case)
                        }
                        "zero" => APInt::zero(w),
                        "one" => APInt::one(w),
                        "signbit" => APInt::one(w).shl(w - 1),
                        _ => match op(operation).attr("value") {
                            Some(AttributeValue::Int(i)) => APInt::new_signed(w, i),
                            Some(AttributeValue::UInt(i)) => APInt::new(w, i),
                            Some(AttributeValue::Bool(b)) => APInt::new(w, u64::from(b)),
                            Some(AttributeValue::Wide(limbs)) => APInt::from_limbs(w, &limbs),
                            _ => {
                                return Err(BuildError::Unsupported(
                                    "expected an integer literal".into(),
                                ));
                            }
                        },
                    };
                    let index = Const(constants.len() as u32);
                    constants.push(bits);
                    Inst::Constant(dst, index)
                }
                "add" => Inst::Add(slots.reg(output)?, slots.reg(lhs)?, slots.reg(rhs)?),
                "sub" => Inst::Sub(slots.reg(output)?, slots.reg(lhs)?, slots.reg(rhs)?),
                "eq" => Inst::Eq(slots.reg(output)?, slots.reg(lhs)?, slots.reg(rhs)?),
                "ult" => Inst::Ult(slots.reg(output)?, slots.reg(lhs)?, slots.reg(rhs)?),
                "poison_if" => Inst::PoisonIf(slots.reg(output)?, slots.reg(lhs)?, slots.reg(rhs)?),
                "load" => {
                    let dst = slots.reg(output)?;
                    let address = slots.reg(lhs)?;
                    let load = op(operation);
                    let layout =
                        crate::layout::DataLayout::for_op(ctx, load.id).ok_or_else(|| {
                            BuildError::Unsupported("load has no pointer layout".into())
                        })?;
                    let result_ty = ctx.get_value(ValueId::from_number(output as u32)).ty();
                    if layout.pointer_size() != Some(slots.widths[address.0 as usize])
                        || layout
                            .layout_of(result_ty)
                            .is_some_and(|ty| ty.size != u64::from(slots.widths[dst.0 as usize]))
                    {
                        return Err(BuildError::Unsupported(format!(
                            "load at {:?} requires a different address or storage width",
                            load.id
                        )));
                    }
                    Inst::Load(dst, address)
                }
                "require" => Inst::Require(slots.reg(lhs)?),
                "return" => Inst::Return(slots.reg(lhs)?),
                "branch" => Inst::Branch(slots.reg(lhs)?, edge(0)?, edge(1)?),
                "jump" => Inst::Jump(edge(0)?),
                "call" => {
                    let args = ordered(rules.listed(&ix.calls, operation)?, |r| r.1)?;
                    let (target, values) = args
                        .split_first()
                        .ok_or_else(|| BuildError::Unsupported("missing callee".into()))?;
                    let target = u32::try_from(target.2)
                        .map_err(|_| BuildError::Unsupported("invalid callee ID".into()))?;
                    let callee = *functions
                        .get(&OperationId::from_raw(target))
                        .ok_or_else(|| BuildError::Unsupported("callee has no body".into()))?;
                    let id = Args(f.args.len() as u32);
                    f.args.push(
                        values
                            .iter()
                            .map(|arg| slots.reg(arg.2))
                            .collect::<Result<_>>()?,
                    );
                    Inst::Call(slots.reg(output)?, callee, id)
                }
                _ => {
                    return Err(BuildError::Unsupported(format!(
                        "unsupported instruction {kind} at operation {}",
                        operation.raw()
                    )));
                }
            };
            code.push(inst);
        }
        f.blocks.push(Block { params, code });
    }
    f.widths = slots.widths;
    Ok(f)
}
