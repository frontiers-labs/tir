use crate::ty::{Ctor, Kind, TypeCtor};
use crate::{Context, Error, Operation, Verifiable, dialect, operation};

/// The constructor of an explicit control-flow token.
pub static CONTROL: Ctor = Ctor::new(TypeCtor {
    dialect: "son",
    name: "control",
    params: &[],
    kind: Kind::Data,
    validate: tir_symbolic::term::valid,
    carrier: tir_symbolic::term::no_carrier,
});

operation! {
    MergeOp {
        name: "merge",
        dialect: "son",
        scheme: "(son.control*) -> son.control",
        operands: O { incoming },
        results: R { control },
        regions: R { arguments: Region { kind: Nodes, } },
        verifier: "true",
    }
}

impl Verifiable for MergeOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let region = self.arguments();
        if self.operands().is_empty() || !region.op_ids().is_empty() || !region.results().is_empty()
        {
            return Err(Error::ExpectedType);
        }
        for incoming in self.operands() {
            let edge = context
                .get_value(incoming)
                .defining_op()
                .and_then(|id| context.get_op(id).as_op::<EdgeOp>())
                .ok_or(Error::ExpectedOperation("son", "edge"))?;
            let args = edge.operands();
            if args.len() != region.ports().len() + 1
                || args[1..]
                    .iter()
                    .zip(region.ports())
                    .any(|(&arg, port)| context.get_value(arg).ty() != port.ty())
            {
                return Err(Error::ExpectedType);
            }
        }
        Ok(())
    }
}

operation! {
    EdgeOp {
        name: "edge",
        dialect: "son",
        scheme: "(son.control, _*) -> son.control",
        operands: O { control, arguments },
        results: R { edge },
        state: "in",
    }
}

operation! {
    BranchOp {
        name: "branch",
        dialect: "son",
        scheme: "(son.control, int<1>) -> son.control*",
        operands: O { control, predicate },
        results: R { arms },
        verifier: "true",
    }
}

impl Verifiable for BranchOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        if self.0.results().len() != 2
            || context.get_value(self.operands()[1]).ty()
                != tir::builtin::IntegerType::new(context, 1)
        {
            return Err(Error::ExpectedType);
        }
        Ok(())
    }
}

dialect! {
    SonDialect {
        name: "son",
        operations: [MergeOp, EdgeOp, BranchOp],
        types: [Ctor(&CONTROL)],
    }
}
