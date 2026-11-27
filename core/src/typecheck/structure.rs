use super::{Check, Failures};
use crate::{Context, OpId, RegionId, RegionKind, ValueId};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum BindingRole {
    Entry,
    Exit,
    Feedback,
}

pub(super) struct Control {
    pub gamma: bool,
    pub selector: usize,
    pub theta: bool,
    pub body: Option<RegionId>,
    pub predicate: usize,
}

impl Context {
    pub(super) fn typecheck_regions(&self, op: OpId) -> Vec<(i64, RegionId)> {
        let rows: Vec<(i64, i64)> =
            self.index_rows("Region", (&[2], i64::from(op.number())), &[3], [3, 0]);
        rows.into_iter()
            .filter(|(slot, _)| *slot >= 0)
            .map(|(slot, id)| (slot, RegionId::from_number(id as u32)))
            .collect()
    }

    pub(super) fn typecheck_bindings(&self, r: RegionId) -> Vec<(BindingRole, usize, usize)> {
        let rows: Vec<(String, i64, i64)> =
            self.index_rows("Binding", (&[0], i64::from(r.number())), &[], [1, 2, 3]);
        rows.into_iter()
            .map(|(role, from, to)| {
                let role = match role.as_str() {
                    "entry" => BindingRole::Entry,
                    "exit" => BindingRole::Exit,
                    "feedback" => BindingRole::Feedback,
                    _ => unreachable!("IR binding role"),
                };
                (role, from as usize, to as usize)
            })
            .collect()
    }

    pub(super) fn typecheck_control(&self, id: OpId) -> Option<Control> {
        let rows: Vec<(bool, i64, bool, i64, i64)> = self.index_rows(
            "Control",
            (&[0], i64::from(id.number())),
            &[],
            [1, 2, 3, 4, 5],
        );
        let &(gamma, selector, theta, body, predicate) = rows.first()?;
        Some(Control {
            gamma,
            selector: selector as usize,
            theta,
            body: u32::try_from(body).ok().map(RegionId::from_number),
            predicate: predicate as usize,
        })
    }

    pub(super) fn typecheck_readers(&self, id: OpId) -> Vec<RegionId> {
        let rows: Vec<(i64,)> =
            self.index_rows("ReaderAt", (&[0], i64::from(id.number())), &[], [1]);
        rows.into_iter()
            .map(|(id,)| RegionId::from_number(id as u32))
            .collect()
    }
}

impl Check<'_> {
    fn block_position(&self, id: OpId) -> Option<i64> {
        let position: Vec<(i64,)> = self.ctx.index_rows(
            "Operation",
            (&[0, 2], (i64::from(id.number()), "block")),
            &[],
            [4],
        );
        Some(position.first()?.0)
    }

    fn visible(&self, value: ValueId, region: RegionId) -> bool {
        if !self.ctx.has_value(value) {
            return false;
        }
        let producer = self.ctx.get_value(value).defining_op();
        let definition = if let Some(producer) = producer {
            if !self.ctx.has_operation(producer) {
                return false;
            }
            let region = self.ctx.region_of_op(producer);
            if region.is_none() {
                return self.ctx.parent_block(producer).is_none();
            }
            region
        } else {
            self.ctx.region_of_port(value).or_else(|| {
                self.ctx
                    .block_of_argument(value)
                    .and_then(|b| self.ctx.parent_region(b))
            })
        };
        definition.is_some_and(|r| self.ctx.region_under(region, r))
    }

    pub(super) fn structure(&self, errors: &mut Failures) {
        let id = self.op.id;
        let ctx = self.ctx;
        let n = i64::from(id.number());
        let cycle: Vec<()> = ctx.index_rows("CyclePath", (&[1, 2], (n, n)), &[], []);
        errors.require(cycle.is_empty(), "unordered dependency cycle");
        let operands = self.op.operands();
        let results = self.op.results();
        let region = ctx.region_of_op(id);
        let block = ctx.parent_block(id);
        let position = block.and_then(|_| self.block_position(id));
        for value in operands.iter().chain(results.iter()).copied() {
            if !ctx.has_value(value) {
                errors.report(format!("references unknown value %{}", value.number()));
                continue;
            }
            let producer = ctx.get_value(value).defining_op();
            errors.require(
                producer.is_some_and(|p| ctx.has_operation(p))
                    || ctx.is_block_argument(value)
                    || ctx.is_region_port(value),
                "dangling value",
            );
        }
        for &v in &operands {
            if let Some(r) = region {
                errors.require(self.visible(v, r), "value outside its region scope");
            }
            let Some(use_block) = block else { continue };
            let producer = ctx
                .has_value(v)
                .then(|| ctx.get_value(v).defining_op())
                .flatten();
            let def_block = producer
                .and_then(|p| ctx.parent_block(p))
                .or_else(|| ctx.block_of_argument(v));
            if let (Some(def), Some(r)) = (def_block, region) {
                let bypass: Vec<()> = ctx.index_rows(
                    "Bypass",
                    (
                        &[0, 1, 2],
                        (
                            i64::from(r.number()),
                            i64::from(def.number()),
                            i64::from(use_block.number()),
                        ),
                    ),
                    &[],
                    [],
                );
                errors.require(
                    bypass.is_empty(),
                    "value definition does not dominate its use",
                );
            }
            if let Some(p) = producer
                && def_block == block
            {
                errors.require(
                    self.block_position(p) < position,
                    "value used before its definition",
                );
            }
        }
        let terminator = ctx.kind_has_interface::<dyn crate::Terminator>(&self.kind);
        if let Some(block) = block
            && !terminator
        {
            errors.require(
                ctx.block_tail_position(block) != position,
                "unterminated block",
            );
        }
        errors.require(
            !(terminator && ctx.parent_nodes_region(id).is_some()),
            "terminator in unordered region",
        );
        self.region_shapes(errors);
        self.successors(errors);
    }

    fn region_shapes(&self, errors: &mut Failures) {
        let id = self.op.id;
        let ctx = self.ctx;
        let operands = self.op.operands();
        let results = self.op.results();
        let regions = ctx.typecheck_regions(id);
        let specs = self.schema.map_or(&[][..], |s| s.regions);
        for (i, &(_, variadic)) in specs.iter().enumerate() {
            errors.require(
                variadic || regions.iter().any(|(s, _)| *s == i as i64),
                "region shape",
            );
        }
        for (i, rid) in regions {
            let i = i as usize;
            let r = ctx.get_region(rid);
            let accepts = |kind| match kind {
                RegionKind::Any => true,
                RegionKind::Nodes => r.is_nodes(),
                RegionKind::Blocks => !r.is_nodes(),
            };
            errors.require(
                specs
                    .iter()
                    .enumerate()
                    .any(|(n, &(k, v))| ((n == i) || (v && i >= n)) && accepts(k)),
                "region shape",
            );
            errors.require(
                r.block_ids().iter().all(|b| !ctx.block_ops(*b).is_empty()),
                "empty block",
            );
            let yields = r.results();
            if r.is_nodes() {
                for &v in &yields {
                    errors.require(self.visible(v, rid), "value outside its region scope");
                }
            }
            let params = ctx.region_ports(rid);
            for (role, from, to) in ctx.typecheck_bindings(rid) {
                let (left, right, label) = match role {
                    BindingRole::Entry => (
                        params.get(from).and_then(|p| self.ty(p.id())),
                        operands.get(to).and_then(|v| self.ty(*v)),
                        "region entry",
                    ),
                    BindingRole::Exit => (
                        results.get(from).and_then(|v| self.ty(*v)),
                        yields.get(to).and_then(|v| self.ty(*v)),
                        "region exit",
                    ),
                    BindingRole::Feedback => (
                        params.get(from).and_then(|p| self.ty(p.id())),
                        yields.get(to).and_then(|v| self.ty(*v)),
                        "region feedback",
                    ),
                };
                errors.require(left.is_some() && left == right, label);
            }
        }
    }

    fn successors(&self, errors: &mut Failures) {
        let ctx = self.ctx;
        let n = i64::from(self.op.id.number());
        let operands = self.op.operands();
        let successors: Vec<(i64, i64)> = ctx.index_rows("Successor", (&[0], n), &[], [1, 2]);
        let mut matched_slots = 0;
        for (edge, b) in successors {
            let args: Vec<()> = ctx.index_rows("SuccessorArgument", (&[0, 1], (n, edge)), &[], []);
            matched_slots += args.len();
            let params: Vec<(i64, i64)> =
                ctx.index_rows("Value", (&[2, 3], ("block", b)), &[], [4, 1]);
            errors.require(
                params.len() == args.len()
                    && params.iter().all(|(slot, ty)| {
                        let operand: Vec<(i64,)> = ctx.index_rows(
                            "SuccessorArgument",
                            (&[0, 1, 2], (n, edge, *slot)),
                            &[],
                            [3],
                        );
                        operand
                            .first()
                            .and_then(|&(i,)| operands.get(i as usize))
                            .and_then(|v| self.ty(*v))
                            .map(|t| i64::from(t.number()))
                            == Some(*ty)
                    }),
                "branch argument",
            );
        }
        let slots: Vec<()> = ctx.index_rows("SuccessorArgument", (&[0], n), &[], []);
        errors.require(matched_slots == slots.len(), "branch argument");
    }
}
