use std::any::Any;
use std::sync::Arc;

use crate::parse::Span;
use crate::parse::text::Parser as IRParser;
use crate::{Context, Error, OpHandle, Operation};

pub type OperationParser = fn(&mut IRParser, &Context) -> Result<Box<dyn Operation>, (Span, Error)>;

pub trait Dialect: 'static + Send + Sync + Any {
    #[allow(clippy::new_ret_no_self)]
    fn new() -> Arc<dyn Dialect>
    where
        Self: Sized;

    fn name() -> &'static str
    where
        Self: Sized;

    fn register_operations(&mut self, context: &Context);
    fn register_types(&mut self, context: &Context);

    fn get_dyn_op(&self, op: OpHandle) -> Box<dyn Operation>;

    fn get_parser(&self, name: &str) -> Result<OperationParser, Error>;
}
