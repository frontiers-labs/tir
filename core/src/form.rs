use crate::{Context, Error, OpId};

/// The set of dialects a region tree may be spelled in.
///
/// A pass precondition like "this runs on CFG form" used to live in prose, so
/// nothing checked it. A form names the dialects that are legal at a point in
/// the pipeline, which turns that precondition into a property the pass
/// manager verifies before handing IR to a pass.
pub struct Form {
    name: &'static str,
    dialects: &'static [&'static str],
}

impl Form {
    /// Unstructured control flow: branches and blocks, no structured regions.
    pub fn cfg() -> Self {
        Self {
            name: "cfg",
            dialects: &["builtin", "cfg", "func", "state", "ptr", "vector"],
        }
    }

    /// Structured control flow: regions carry the control structure, no branches.
    pub fn rvsdg() -> Self {
        Self {
            name: "rvsdg",
            dialects: &["builtin", "scf", "state", "func", "ptr", "vector"],
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn allows(&self, dialect: &str) -> bool {
        self.dialects.contains(&dialect)
    }
}

/// Checks that every operation under `root` belongs to a dialect `form` allows.
pub fn verify_form(context: &Context, root: OpId, form: &Form) -> Result<(), Error> {
    let mut worklist = vec![root];
    while let Some(op_id) = worklist.pop() {
        let instance = context.get_op(op_id);
        let dialect = instance.dialect().as_str();
        if !form.allows(dialect) {
            return Err(Error::VerificationError(format!(
                "operation '{}.{}': dialect '{}' is not part of the '{}' form",
                dialect,
                instance.name(),
                dialect,
                form.name()
            )));
        }
        for region_id in instance.regions().iter().rev() {
            for block in context.get_region(*region_id).iter(context.clone()) {
                worklist.extend(block.op_ids().iter().rev());
            }
        }
    }
    Ok(())
}
