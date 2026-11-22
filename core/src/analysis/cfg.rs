//! Thin installer and refresh entry point for the CDSL CFG, Control Tree,
//! Demand Annotation, and Memory Chain analyses in `AnalysisManager`.

use crate::{AnalysisManager, Context, OpId, PassError};

pub use generated::{ScopeFacts, scope};

#[allow(dead_code)]
pub(super) mod generated {
    include!(concat!(env!("OUT_DIR"), "/cfg_analysis.rs"));
}

/// Ensure the CDSL CFG + Reissmann §5.1.2 analysis catalog is installed on `context`.
pub fn ensure_installed(context: &Context) -> Result<(), PassError> {
    AnalysisManager::ensure_generated(context, generated::load_producers)
        .map_err(|error| PassError::InvalidRuleSet(error.to_string()))
}

/// Refresh all Reissmann §5.1.2 CFG, Control Tree, Demand, and Memory Chain tables for `root`.
pub fn refresh(context: &Context, root: OpId) -> Result<(), PassError> {
    ensure_installed(context)?;
    use generated::*;
    type ResolveTable = fn(&tir_egglog::Database) -> Result<tir_egglog::TableId, tir_egglog::Error>;
    let tables: [ResolveTable; 7] = [
        table_cfg_idom,
        table_dag_topo_rank,
        table_tail_loop,
        table_branch_output,
        table_loop_exit_only,
        table_top_hammock,
        table_irred_loop,
    ];
    for table in tables {
        AnalysisManager::require_table(context, root, table)
            .map_err(|error| PassError::InvalidRuleSet(error.to_string()))?;
    }
    Ok(())
}
