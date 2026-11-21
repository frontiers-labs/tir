//! Thin installer and refresh entry point for the CDSL CFG, Control Tree,
//! Demand Annotation, and Memory Chain analyses in `AnalysisManager`.

use crate::{AnalysisManager, Context, OpId, PassError};

const CFG_SCHEMA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cfg-analysis-schema.bin"));
const CFG_RULES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cfg-analysis-rules.bin"));
const CFG_PRODUCERS: &[(&str, &[&str], &[&str])] =
    include!(concat!(env!("OUT_DIR"), "/cfg-analysis-producers.rs"));

/// Terminal tables whose transitive prerequisites cover the full Reissmann §5.1.2 catalog.
const TERMINAL_TABLES: &[&str] = &[
    "CfgIdom",
    "DagTopoRank",
    "TailLoop",
    "BranchOutput",
    "LoopExitOnly",
    "TopHammock",
    "IrredLoop",
];

/// Ensure the CDSL CFG + Reissmann §5.1.2 analysis catalog is installed on `context`.
pub fn ensure_installed(context: &Context) -> Result<(), PassError> {
    if !context.analyses().is_installed() {
        AnalysisManager::install_precompiled(context, CFG_SCHEMA, CFG_RULES, CFG_PRODUCERS)
            .map_err(|error| PassError::InvalidRuleSet(error.to_string()))?;
    }
    Ok(())
}

/// Refresh all Reissmann §5.1.2 CFG, Control Tree, Demand, and Memory Chain tables for `root`.
pub fn refresh(context: &Context, root: OpId) -> Result<(), PassError> {
    ensure_installed(context)?;
    for &table in TERMINAL_TABLES {
        AnalysisManager::require(context, root, table)
            .map_err(|error| PassError::InvalidRuleSet(error.to_string()))?;
    }
    Ok(())
}
