use super::generated::{
    table_destructure_graph_execution, table_destructure_graph_feedback_capture,
    table_destructure_graph_live_in_state, table_destructure_graph_needed_port,
    table_destructure_graph_needed_result, table_destructure_undemanded_graph_effect,
    table_destructure_unsupported_graph_capture,
};
use crate::{AnalysisManager, Context, OpId, PassError, RegionId, ValueId};
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
        AnalysisManager::ensure_default(context)?;
        let bad = AnalysisManager::require_table(
            context,
            function,
            table_destructure_undemanded_graph_effect,
        )
        .map_err(rule_error)?;
        if let Some(node) = bad.rows().map(|row| bad.base_value::<i64>(row[0])).min() {
            drop(bad);
            let op = context.get_op(OpId::from_number(checked_id(node)?));
            return Err(PassError::InvalidIR {
                pass: "destructure",
                error: crate::Error::VerificationError(format!(
                    "{}.{} leaves a dependency no result demands",
                    op.dialect().as_str(),
                    op.name().as_str()
                )),
            });
        }
        drop(bad);

        let unsupported = AnalysisManager::require_table(
            context,
            function,
            table_destructure_unsupported_graph_capture,
        )
        .map_err(rule_error)?;
        if let Some(node) = unsupported
            .rows()
            .map(|row| unsupported.base_value::<i64>(row[0]))
            .min()
        {
            drop(unsupported);
            return Err(PassError::InvalidIR {
                pass: "destructure",
                error: crate::Error::VerificationError(format!(
                    "cannot preserve the lazy capture of operation {node}"
                )),
            });
        }
        drop(unsupported);

        let feedback = AnalysisManager::require_table(
            context,
            function,
            table_destructure_graph_feedback_capture,
        )
        .map_err(rule_error)?;
        let mut ordered_captures = feedback
            .rows()
            .map(|row| {
                let theta = OpId::from_number(checked_id(feedback.base_value(row[0]))?);
                let rank = feedback.base_value::<i64>(row[1]);
                let node = OpId::from_number(checked_id(feedback.base_value(row[2]))?);
                Ok((theta, rank, node, feedback.base_value::<bool>(row[3])))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        drop(feedback);
        ordered_captures.sort_unstable();
        let mut captures = BTreeMap::<_, Vec<_>>::new();
        let mut replays = BTreeMap::<_, Vec<_>>::new();
        for (theta, _, node, replay) in ordered_captures {
            let schedule = if replay { &mut replays } else { &mut captures };
            schedule.entry(theta).or_default().push(node);
        }

        let used = AnalysisManager::require_table(
            context,
            function,
            table_destructure_graph_needed_result,
        )
        .map_err(rule_error)?;
        let live_results = used
            .rows()
            .map(|row| {
                let node = OpId::from_number(checked_id(used.base_value(row[0]))?);
                let slot = usize::try_from(used.base_value::<i64>(row[1])).map_err(rule_error)?;
                Ok((node, slot))
            })
            .collect::<Result<_, PassError>>()?;
        drop(used);

        let ports =
            AnalysisManager::require_table(context, function, table_destructure_graph_needed_port)
                .map_err(rule_error)?;
        let live_ports = ports
            .rows()
            .map(|row| {
                let region = RegionId::from_number(checked_id(ports.base_value(row[0]))?);
                let port = usize::try_from(ports.base_value::<i64>(row[1])).map_err(rule_error)?;
                Ok((region, port))
            })
            .collect::<Result<_, PassError>>()?;
        drop(ports);

        let execution =
            AnalysisManager::require_table(context, function, table_destructure_graph_execution)
                .map_err(rule_error)?;
        let mut ordered = execution
            .rows()
            .map(|row| {
                let region = RegionId::from_number(checked_id(execution.base_value(row[0]))?);
                let part = Part::parse(&execution.base_value::<String>(row[1]))?;
                let rank = execution.base_value::<i64>(row[2]);
                let op = OpId::from_number(checked_id(execution.base_value(row[3]))?);
                Ok(((region, part), rank, op))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        drop(execution);
        ordered.sort_unstable();
        let mut ops = BTreeMap::<_, Vec<_>>::new();
        for (key, _, op) in ordered {
            ops.entry(key).or_default().push(op);
        }

        let live = AnalysisManager::require_table(
            context,
            function,
            table_destructure_graph_live_in_state,
        )
        .map_err(rule_error)?;
        let mut state_in = BTreeMap::<_, Vec<_>>::new();
        for row in live.rows() {
            let region = RegionId::from_number(checked_id(live.base_value(row[0]))?);
            let part = Part::parse(&live.base_value::<String>(row[1]))?;
            let state = ValueId::from_number(checked_id(live.base_value(row[2]))?);
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

fn checked_id(raw: i64) -> Result<u32, PassError> {
    u32::try_from(raw).map_err(rule_error)
}

fn rule_error(error: impl std::fmt::Display) -> PassError {
    PassError::InvalidRuleSet(error.to_string())
}
