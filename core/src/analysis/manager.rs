use crate::{Context, OpId};
use std::rc::Rc;

pub trait Analysis: Sized + 'static {
    fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self;
}

#[derive(Default)]
pub struct AnalysisManager;
impl AnalysisManager {
    pub fn new() -> Self {
        Self
    }
    pub fn get<A: Analysis>(&self, context: &Context, op: impl Into<OpId>) -> Rc<A> {
        Rc::new(A::build(self, context, op.into()))
    }
}
