use crate::{Context, Error, Operation, Terminator, ValueId, dialect, operation};

use crate as tir;
use crate::Any as AnyConstraint;

pub mod nodes;
pub use nodes::{ForOp, ForOpBuilder, LoopOp, LoopOpBuilder, SwitchOp, SwitchOpBuilder};

pub mod ops {
    pub use super::nodes::{r#for, r#loop, switch};
    pub use super::{YieldOp, r#yield};
}

dialect! {
    ScfDialect {
        name: "scf",
        operations: [
            LoopOp,
            SwitchOp,
            ForOp,
            YieldOp,
        ],
        types: [],
    }
}

// The back edge of an `scf.for` whose body is still a block list: it names
// what the next iteration carries. An unordered body names its results
// outright and has no terminator, so nothing else needs this.
operation! {
    YieldOp {
        name: "yield",
        dialect: "scf",
        format: "custom",
        operands: O {
            values: "*AnyConstraint",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for YieldOp {}

impl YieldOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.write("scf.yield")?;
        let values = self.value_operands();
        for (index, value) in values.iter().enumerate() {
            fmt.write(if index == 0 { " " } else { ", " })?;
            fmt.write(format!("%{}", value.number()))?;
        }
        tir::dependency::print_dep_operands(fmt, &self.0)?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let mut values: Vec<ValueId> = vec![];
        while let Some(name) = parser.parse_value_ref() {
            values.push(parser.resolve_value(context, name));
            if !parser.parse_token(",") {
                break;
            }
        }
        let mut builder = YieldOpBuilder::new(context).values(values);
        for dep in tir::dependency::parse_dep_operands(parser, context)? {
            builder = builder.dep_operand(dep);
        }
        Ok(Box::new(builder.build()))
    }
}
