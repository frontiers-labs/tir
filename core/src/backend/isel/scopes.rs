//! The regions of a function as selection's solving units.
//!
//! A function's body is an unordered region, and so is every arm and body of
//! the structured operations it holds. Each is one unit: it is solved as a
//! whole under the fact its entry proves, and its operations take the order
//! the region's own dependence graph admits, ties broken by insertion order.
//! Where blocks needed a dominator tree, regions need only their nesting: a
//! definition is visible inside a region exactly when it sits in that region
//! or in one enclosing it, and — for an enclosing one — ahead of the operation
//! carrying the region, which the topological order already says, since a
//! region's reads are reads of the operation holding it.

use std::collections::HashMap;

use tir::{Context, OpId, OperationRef, PassError, RegionId};

pub(crate) struct Scopes {
    /// Every region of the function, each ahead of the regions nested in it.
    pub(crate) regions: Vec<RegionId>,
    /// Each region's operations in canonical order.
    pub(crate) order: HashMap<RegionId, Vec<OpId>>,
    /// The place of every operation in its region's order.
    pub(crate) position: HashMap<OpId, usize>,
    /// The region holding every operation.
    pub(crate) op_region: HashMap<OpId, RegionId>,
    /// The region enclosing each nested region, and the operation carrying it.
    parent: HashMap<RegionId, (RegionId, OpId)>,
}

impl Scopes {
    pub(crate) fn build(context: &Context, op: &OperationRef) -> Result<Self, PassError> {
        let mut scopes = Self {
            regions: Vec::new(),
            order: HashMap::new(),
            position: HashMap::new(),
            op_region: HashMap::new(),
            parent: HashMap::new(),
        };
        let mut pending: Vec<RegionId> = op.op().regions().iter().rev().copied().collect();
        while let Some(region) = pending.pop() {
            scopes.regions.push(region);
            let order = crate::region::insertion_topological_order(context, region)
                .map_err(|error| PassError::InvalidRuleSet(error.to_string()))?;
            for (position, &id) in order.iter().enumerate() {
                scopes.position.insert(id, position);
                scopes.op_region.insert(id, region);
                let nested = context.get_op(id).regions();
                for &inner in nested.iter().rev() {
                    scopes.parent.insert(inner, (region, id));
                    pending.push(inner);
                }
            }
            scopes.order.insert(region, order);
        }
        Ok(scopes)
    }

    /// Whether `outer` is `inner` or a region enclosing it.
    pub(crate) fn encloses(&self, outer: RegionId, inner: RegionId) -> bool {
        self.distance(inner, outer) != usize::MAX
    }

    /// Steps out from `from` to `to`; `usize::MAX` when `to` does not enclose it.
    pub(crate) fn distance(&self, from: RegionId, to: RegionId) -> usize {
        let mut distance = 0;
        let mut current = from;
        while current != to {
            let Some(&(parent, _)) = self.parent.get(&current) else {
                return usize::MAX;
            };
            distance += 1;
            current = parent;
        }
        distance
    }

    /// The operation of `ancestor` whose regions hold `from`. `None` when
    /// `ancestor` does not strictly enclose `from`.
    pub(crate) fn carrier_in(&self, from: RegionId, ancestor: RegionId) -> Option<OpId> {
        let mut current = from;
        loop {
            let &(parent, carrier) = self.parent.get(&current)?;
            if parent == ancestor {
                return Some(carrier);
            }
            current = parent;
        }
    }

    /// Whether `a` comes before `b` in their region's order.
    pub(crate) fn is_before(&self, a: OpId, b: OpId) -> bool {
        match (self.position.get(&a), self.position.get(&b)) {
            (Some(a), Some(b)) => a < b,
            _ => false,
        }
    }
}
