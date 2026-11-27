//! Reusable pointer-offset, allocation/escape, and memory-access analysis
//! (`core/defs/analysis/memory.cdsl`), enriched with host `DataLayout` facts.

use tir_egglog::{Database, LocalTable};

use crate::passes::common::{resolve_layout_queries, rule_error};
use crate::{Context, OpId, PassError};

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/memory_analysis.rs"));
}

// Populate `analysis::memory` tables into `db` using precomputed `value_info`
// bindings and host `DataLayout` queries.
pub(crate) fn populate(
    context: &Context,
    db: &mut Database,
    value_inputs: &[(&str, &LocalTable)],
) -> Result<(), PassError> {
    let mut program = generated::Program::load_with_tables(db, value_inputs).map_err(rule_error)?;
    program.query_layouts().map_err(rule_error)?;

    let requests = program
        .mem_layout_query()
        .map(|row| {
            let (id, kind) = row.map_err(rule_error)?;
            Ok((OpId::from_number(id.raw()), kind.declaration()))
        })
        .collect::<Result<Vec<_>, PassError>>()?;
    let answers = resolve_layout_queries(context, &requests)?;
    for (id, width) in answers.pointer_sizes {
        program
            .insert_mem_pointer_size((generated::OperationId::from_raw(id.number()), width))
            .map_err(rule_error)?;
    }
    for (id, size, align) in answers.store_layouts {
        program
            .insert_mem_store_layout((generated::OperationId::from_raw(id.number()), size, align))
            .map_err(rule_error)?;
    }
    for id in answers.scoped_ptradds {
        program
            .insert_mem_layout_scoped((generated::OperationId::from_raw(id.number()),))
            .map_err(rule_error)?;
    }
    program.run_analysis().map_err(rule_error)?;
    Ok(())
}
