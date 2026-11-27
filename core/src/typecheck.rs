use crate::{Context, Error, OpId, OpSchema};
use std::sync::Arc;
use tir_symbolic::scheme::{Scheme, SchemeSyntax};

pub fn scheme_of(context: &Context, schema: &'static OpSchema) -> Result<Arc<Scheme>, String> {
    SchemeSyntax::parse(schema.scheme)?
        .resolve(context.terms())
        .map(Arc::new)
}

pub fn check_types(_context: &Context, _ops: &[OpId]) -> Result<(), Error> {
    Ok(())
}
