use super::generated::{OperationId, destructure_recover};
use crate::{Context, OpId, PassError, RegionId, ValueId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Part {
    Yield,
    Head,
    Feedback,
    Exit,
}

impl Part {
    fn parse(part: &str) -> Result<Self, PassError> {
        match part {
            "yield" => Ok(Self::Yield),
            "head" => Ok(Self::Head),
            "feedback" => Ok(Self::Feedback),
            "exit" => Ok(Self::Exit),
            _ => Err(PassError::InvalidRuleSet(format!(
                "unknown graph execution part {part}"
            ))),
        }
    }
}

type RegionPart = (RegionId, Part);

/// The ordered operations and incoming state for each execution part of a graph region.
pub(crate) struct Schedule {
    ops: BTreeMap<RegionPart, Vec<OpId>>,
    state_in: BTreeMap<RegionPart, Vec<ValueId>>,
    captures: BTreeMap<OpId, Vec<OpId>>,
    replays: BTreeMap<OpId, Vec<OpId>>,
    live_results: BTreeSet<(OpId, usize)>,
    live_ports: BTreeSet<(RegionId, usize)>,
}

impl Schedule {
    pub(crate) fn load(context: &Context, function: OpId) -> Result<Self, PassError> {
        let revision = context.subtree_revision(function);
        let mut engine = context.egraph();
        let db = engine.database_mut();
        let facts = destructure_recover(db, revision, OperationId::from_raw(function.number()))
            .map_err(rule_error)?;
        let undemanded = first_operation(facts.undemanded_graph_effect(db).map_err(rule_error)?)?;
        if let Some(node) = undemanded {
            drop(engine);
            let op = context.get_op(OpId::from_number(node));
            return Err(PassError::InvalidIR {
                pass: "destructure",
                error: crate::Error::VerificationError(format!(
                    "{}.{} leaves a dependency no result demands",
                    op.dialect().as_str(),
                    op.name().as_str()
                )),
            });
        }
        if let Some(node) =
            first_operation(facts.unsupported_graph_capture(db).map_err(rule_error)?)?
        {
            return Err(PassError::InvalidIR {
                pass: "destructure",
                error: crate::Error::VerificationError(format!(
                    "cannot preserve the lazy capture of operation {node}"
                )),
            });
        }

        let mut ordered_captures = facts
            .graph_feedback_capture(db)
            .map_err(rule_error)?
            .map(|row| {
                let (theta, rank, node, replay) = row.map_err(rule_error)?;
                Ok((
                    OpId::from_number(theta.raw()),
                    rank,
                    OpId::from_number(node.raw()),
                    replay,
                ))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        ordered_captures.sort_unstable();
        let mut captures = BTreeMap::<_, Vec<_>>::new();
        let mut replays = BTreeMap::<_, Vec<_>>::new();
        for (theta, _, node, replay) in ordered_captures {
            let schedule = if replay { &mut replays } else { &mut captures };
            schedule.entry(theta).or_default().push(node);
        }
        let live_results = facts
            .graph_needed_result(db)
            .map_err(rule_error)?
            .map(|row| {
                let (node, slot) = row.map_err(rule_error)?;
                Ok((
                    OpId::from_number(node.raw()),
                    usize::try_from(slot).map_err(rule_error)?,
                ))
            })
            .collect::<Result<_, PassError>>()?;
        let live_ports = facts
            .graph_needed_port(db)
            .map_err(rule_error)?
            .map(|row| {
                let (region, port) = row.map_err(rule_error)?;
                Ok((
                    RegionId::from_number(checked_id(region)?),
                    usize::try_from(port).map_err(rule_error)?,
                ))
            })
            .collect::<Result<_, PassError>>()?;
        let mut ordered = facts
            .graph_execution(db)
            .map_err(rule_error)?
            .map(|row| {
                let (region, part, rank, op) = row.map_err(rule_error)?;
                Ok((
                    (
                        RegionId::from_number(checked_id(region)?),
                        Part::parse(&part)?,
                    ),
                    rank,
                    OpId::from_number(op.raw()),
                ))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        ordered.sort_unstable();
        let mut ops = BTreeMap::<_, Vec<_>>::new();
        for (key, _, op) in ordered {
            ops.entry(key).or_default().push(op);
        }

        let mut state_in = BTreeMap::<_, Vec<_>>::new();
        for row in facts.graph_live_in_state(db).map_err(rule_error)? {
            let (region, part, state) = row.map_err(rule_error)?;
            let region = RegionId::from_number(checked_id(region)?);
            let part = Part::parse(&part)?;
            let state = ValueId::from_number(state.raw());
            state_in.entry((region, part)).or_default().push(state);
        }
        for states in state_in.values_mut() {
            states.sort_unstable();
        }

        Ok(Self {
            ops,
            state_in,
            captures,
            replays,
            live_results,
            live_ports,
        })
    }

    pub(crate) fn captures(&self, theta: OpId) -> &[OpId] {
        self.captures.get(&theta).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn replays(&self, theta: OpId) -> &[OpId] {
        self.replays.get(&theta).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn port_used(&self, region: RegionId, slot: usize) -> bool {
        self.live_ports.contains(&(region, slot))
    }

    pub(crate) fn result_used(&self, op: OpId, slot: usize) -> bool {
        self.live_results.contains(&(op, slot))
    }

    pub(crate) fn ops(&self, region: RegionId, part: Part) -> &[OpId] {
        self.ops.get(&(region, part)).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn state_in(&self, region: RegionId, part: Part) -> &[ValueId] {
        self.state_in
            .get(&(region, part))
            .map_or(&[], Vec::as_slice)
    }
}

fn first_operation(
    mut rows: impl Iterator<Item = Result<(OperationId,), Box<dyn std::error::Error>>>,
) -> Result<Option<u32>, PassError> {
    rows.try_fold(None, |first: Option<u32>, row| {
        let (id,) = row.map_err(rule_error)?;
        Ok(Some(first.map_or(id.raw(), |first| first.min(id.raw()))))
    })
}

fn checked_id(raw: i64) -> Result<u32, PassError> {
    u32::try_from(raw).map_err(rule_error)
}

fn rule_error(error: impl std::fmt::Display) -> PassError {
    PassError::InvalidRuleSet(error.to_string())
}
