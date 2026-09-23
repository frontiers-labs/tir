//! Affine recurrences in Z/(2^W), independent of integer dependence domains.
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::builtin::{AddIOp, IntegerType, MulIOp, SubIOp, TruncIOp, ops as b};
use crate::{ConstantLike, Context, OpId, Operation, RegionId, Theta, TypeId, ValueId};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Form {
    constant: u64,
    terms: BTreeMap<ValueId, u64>,
}

impl Form {
    fn constant(constant: u64) -> Self {
        Self {
            constant,
            terms: BTreeMap::new(),
        }
    }

    fn scale(mut self, coefficient: u64, mask: u64) -> Self {
        self.constant = self.constant.wrapping_mul(coefficient) & mask;
        self.terms.retain(|_, c| {
            *c = c.wrapping_mul(coefficient) & mask;
            *c != 0
        });
        self
    }

    fn add(mut self, other: Self, mask: u64) -> Self {
        self.constant = self.constant.wrapping_add(other.constant) & mask;
        for (value, coefficient) in other.terms {
            let c = self.terms.entry(value).or_default();
            *c = c.wrapping_add(coefficient) & mask;
        }
        self.terms.retain(|_, c| *c != 0);
        self
    }

    fn zero(&self) -> bool {
        self.constant == 0 && self.terms.is_empty()
    }
}

#[derive(Clone)]
struct Recurrence {
    initial: Form,
    step: Form,
    product: bool,
}

fn width(context: &Context, value: ValueId) -> Option<u32> {
    let ty = context.get_type_data(context.get_value(value).ty());
    let width = (ty.as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()?
        .width();
    (1..=64).contains(&width).then_some(width)
}

fn mask(width: u32) -> u64 {
    u64::MAX >> (64 - width)
}

fn literal(context: &Context, value: ValueId) -> Option<u64> {
    let op = context.get_op(context.get_value(value).defining_op()?);
    Some(
        op.as_interface::<dyn ConstantLike>()?
            .constant_value()
            .to_i64() as u64,
    )
}

struct Reader<'a> {
    context: &'a Context,
    body: RegionId,
    interior: HashSet<ValueId>,
    known: HashMap<ValueId, Option<Recurrence>>,
}

impl Reader<'_> {
    fn read(&mut self, value: ValueId) -> Option<Recurrence> {
        if let Some(form) = self.known.get(&value) {
            return form.clone();
        }
        self.known.insert(value, None);
        let form = self.read_new(value);
        self.known.insert(value, form.clone());
        form
    }

    fn read_new(&mut self, value: ValueId) -> Option<Recurrence> {
        let mask = mask(width(self.context, value)?);
        if let Some(c) = literal(self.context, value) {
            return Some(Recurrence {
                initial: Form::constant(c & mask),
                step: Form::default(),
                product: false,
            });
        }
        if !self.interior.contains(&value) {
            return Some(Recurrence {
                initial: Form {
                    constant: 0,
                    terms: BTreeMap::from([(value, 1)]),
                },
                step: Form::default(),
                product: false,
            });
        }
        let id = self.context.get_value(value).defining_op()?;
        if self.context.parent_nodes_region(id) != Some(self.body) {
            return None;
        }
        let op = self.context.get_op(id);
        let operands = op.operands();
        if op.is::<TruncIOp>() {
            let a = self.read(operands[0])?;
            return Some(Recurrence {
                initial: a.initial.scale(1, mask),
                step: a.step.scale(1, mask),
                product: a.product,
            });
        }
        if !(op.is::<AddIOp>() || op.is::<SubIOp>() || op.is::<MulIOp>()) {
            return None;
        }
        let a = self.read(operands[0])?;
        let mut b = self.read(operands[1])?;
        if op.is::<MulIOp>() {
            let (invariant, varying) = if a.step.zero() {
                (a, b)
            } else if b.step.zero() {
                (b, a)
            } else {
                return None;
            };
            if invariant.initial.terms.is_empty() {
                let c = invariant.initial.constant;
                return Some(Recurrence {
                    initial: varying.initial.scale(c, mask),
                    step: varying.step.scale(c, mask),
                    product: varying.product,
                });
            }
            if !varying.initial.terms.is_empty() || !varying.step.terms.is_empty() {
                return None;
            }
            return Some(Recurrence {
                initial: invariant
                    .initial
                    .clone()
                    .scale(varying.initial.constant, mask),
                step: invariant.initial.scale(varying.step.constant, mask),
                product: !varying.step.zero() || invariant.product || varying.product,
            });
        }
        if op.is::<SubIOp>() {
            b.initial = b.initial.scale(u64::MAX, mask);
            b.step = b.step.scale(u64::MAX, mask);
        }
        Some(Recurrence {
            initial: a.initial.add(b.initial, mask),
            step: a.step.add(b.step, mask),
            product: a.product || b.product,
        })
    }
}

fn emit(context: &Context, region: RegionId, ty: TypeId, form: &Form) -> ValueId {
    let mut parts = Vec::new();
    if form.constant != 0 || form.terms.is_empty() {
        parts.push(super::lower::literal_at(
            context,
            region,
            form.constant as i128,
            ty,
        ));
    }
    for (&symbol, &coefficient) in &form.terms {
        let mut value = symbol;
        if context.get_value(symbol).ty() != ty {
            let op = b::trunci(context, symbol, ty).build();
            context.add(region, op.id());
            value = op.result();
        }
        if coefficient != 1 {
            let c = super::lower::literal_at(context, region, coefficient as i128, ty);
            let op = b::muli(context, value, c, ty).build();
            context.add(region, op.id());
            value = op.result();
        }
        parts.push(value);
    }
    let mut value = parts[0];
    for &rhs in &parts[1..] {
        let op = b::addi(context, value, rhs, ty).build();
        context.add(region, op.id());
        value = op.result();
    }
    value
}

pub(super) fn run(context: &Context, root: OpId) {
    let mut pending = vec![root];
    let mut loops = Vec::new();
    while let Some(id) = pending.pop() {
        let op = context.get_op(id);
        if op.has_interface::<dyn Theta>() {
            loops.push(id);
        }
        for region in op.regions().to_vec() {
            pending.extend(context.get_region(region).op_ids());
        }
    }
    for id in loops.into_iter().rev() {
        reduce(context, id);
    }
}

fn reduce(context: &Context, id: OpId) {
    let Some(parent) = context.parent_nodes_region(id) else {
        return;
    };
    let op = context.get_op(id);
    let theta = op.clone().as_interface::<dyn Theta>().expect("a theta");
    let body = theta.body();
    let mut interior = HashSet::new();
    for region in context.nested_regions(body) {
        let region = context.get_region(region);
        interior.extend(region.ports().iter().map(crate::Value::id));
        for op in region.op_ids() {
            interior.extend(context.get_op(op).results().iter().copied());
        }
    }
    let mut reader = Reader {
        context,
        body,
        interior,
        known: HashMap::new(),
    };
    let sides = crate::binding::carried(context, &op);
    let mut available = Vec::new();
    for side in sides {
        let Some(w) = width(context, side.port) else {
            continue;
        };
        let Some(next) = context.get_value(side.next).defining_op() else {
            continue;
        };
        let next = context.get_op(next);
        if !next.is::<AddIOp>() {
            continue;
        }
        let operands = next.operands();
        let gained = if operands[0] == side.port {
            operands[1]
        } else if operands[1] == side.port {
            operands[0]
        } else {
            continue;
        };
        let Some(step) = literal(context, gained) else {
            continue;
        };
        let Some(initial) = reader.read(side.init).filter(|r| r.step.zero()) else {
            continue;
        };
        let r = Recurrence {
            initial: initial.initial,
            step: Form::constant(step & mask(w)),
            product: false,
        };
        reader.known.insert(side.port, Some(r.clone()));
        if let Some((_, _, canonical)) =
            available
                .iter()
                .find(|(width, existing, _): &&(u32, Recurrence, ValueId)| {
                    *width == w && existing.initial == r.initial && existing.step == r.step
                })
        {
            context.replace_value_uses(side.port, *canonical);
            // Counted loops must exit with each port in its original slot.
            // Merge readers and continuations without changing that binding.
            let exits = context.get_region(body).results();
            context.rename_region_results(body, side.port, *canonical, &[]);
            let mut results = context.get_region(body).results();
            for index in theta.binding().exit {
                results[index] = exits[index];
            }
            context.set_region_results(body, results);
        } else {
            available.push((w, r, side.port));
        }
    }
    let ops = context.get_region(body).op_ids();
    let mut candidates = Vec::new();
    for &id in &ops {
        let op = context.get_op(id);
        if op.results().len() != 1 {
            continue;
        }
        let value = op.results()[0];
        if let Some(r) = reader.read(value).filter(|r| r.product && !r.step.zero()) {
            candidates.push((value, r));
        }
    }
    // Only maximal affine expressions need a recurrence; their arithmetic
    // inputs disappear with ordinary dead-code elimination.
    let inputs: HashSet<_> = candidates
        .iter()
        .flat_map(|(v, _)| {
            context
                .get_op(context.get_value(*v).defining_op().unwrap())
                .operands()
                .to_vec()
        })
        .collect();
    for (old, r) in candidates.into_iter().filter(|(v, _)| !inputs.contains(v)) {
        let w = width(context, old).unwrap();
        let ty = context.get_value(old).ty();
        let (base, initial) = if let Some((_, existing, port)) = available
            .iter()
            .find(|(width, existing, _)| *width == w && existing.step == r.step)
        {
            (*port, existing.initial.clone())
        } else {
            let init = emit(context, parent, ty, &r.initial);
            let step = emit(context, parent, ty, &r.step);
            let mut port = None;
            context.grow_port(id, ty, Some(init), |region, argument| {
                let argument = argument.expect("theta port");
                port = Some(argument);
                let next = b::addi(context, argument, step, ty).build();
                context.add(region, next.id());
                Some(next.result())
            });
            let port = port.expect("theta body");
            available.push((w, r.clone(), port));
            (port, r.initial.clone())
        };
        let offset = r.initial.add(initial.scale(u64::MAX, mask(w)), mask(w));
        let replacement = if offset.zero() {
            base
        } else {
            let offset = emit(context, parent, ty, &offset);
            let add = b::addi(context, base, offset, ty).build();
            context.add(body, add.id());
            add.result()
        };
        context.replace_value_uses(old, replacement);
        context.rename_region_results(body, old, replacement, &[]);
    }
}
