//! Compiled catalog and structural queries for the IR database.

pub const IR_SCHEMA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ir-schema.bin"));
pub const IR_RULES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ir-rules.bin"));

/// Checked analysis inputs, including the requested root.
pub const ANALYSIS_INPUTS: &str = concat!(
    include_str!("ir.cdsl"),
    "\npub let AnalysisRoot: Int -> Prop;"
);
