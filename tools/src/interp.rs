use std::{error::Error, ffi::OsString};

use clap::Args;
use tir::{
    Context, Operation, Symbol,
    builtin::{IntegerType, ModuleOp},
    func::FuncOp,
    interp::{self, Value},
    utils::{APFloat, APInt},
};

use crate::common::read_input;

#[derive(Args)]
pub struct ToolArgs {
    /// The function to interpret.
    #[arg(long, short = 'f', default_value = "main")]
    function: String,

    /// Comma-separated argument values for the function's parameters.
    #[arg(long, short = 'a', value_delimiter = ',')]
    args: Vec<String>,

    /// Give up after this many executed operations, so a program that need
    /// not terminate can still be compared against another.
    #[arg(long = "max-steps")]
    max_steps: Option<u64>,

    /// Input IR file, or `-`/omitted for stdin.
    input: Option<OsString>,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let input = read_input(args.input.as_ref())?;

    let context = Context::with_default_dialects();
    let module = tir::parse::ir::parse_ir::<ModuleOp>(&context, &input)
        .map_err(|(span, err)| format!("failed to parse input at byte {}: {err:?}", span.0))?;

    let function = find_function(&context, module.id(), &args.function)?;
    let body_region = context.get_op(function).regions().to_vec()[0];
    let parameters: Vec<tir::TypeId> = context
        .get_region(body_region)
        .ports()
        .iter()
        .map(|v| v.ty())
        .collect();
    if parameters.len() != args.args.len() {
        return Err(format!(
            "@{} takes {} arguments, got {}",
            args.function,
            parameters.len(),
            args.args.len()
        )
        .into());
    }
    let arguments = parameters
        .iter()
        .zip(&args.args)
        .map(|(&ty, text)| parse_argument(&context, ty, text))
        .collect::<Result<Vec<_>, String>>()?;

    // Reaching a limit the caller set is an outcome to report, not a failure.
    let results =
        match interp::run_function_within(&context, function, arguments, args.max_steps) {
            Err(interp::InterpError::StepLimit) => {
                println!("step limit exceeded");
                return Ok(());
            }
            other => other,
        }
        .map_err(|err| format!("interpretation failed: {err}"))?;

    let ret_types = return_types(&context, function);
    for (index, value) in results.iter().enumerate() {
        let ty = ret_types.get(index).copied();
        println!("{}", format_value(&context, value, ty));
    }
    Ok(())
}

fn find_function(context: &Context, module: tir::OpId, name: &str) -> Result<tir::OpId, String> {
    let instance = context.get_op(module);
    for region in instance.regions() {
        for block in context.get_region(region).iter(context.clone()) {
            for op in block.op_ids() {
                let candidate = context.get_op(op);
                if !candidate.is::<FuncOp>() {
                    continue;
                }
                if FuncOp::from_op_instance(candidate).symbol_name() == name {
                    return Ok(op);
                }
            }
        }
    }
    Err(format!("no function @{name} in module"))
}

fn return_types(context: &Context, function: tir::OpId) -> Vec<tir::TypeId> {
    let func = FuncOp::from_op_instance(context.get_op(function));
    let ret_type = func.ret_type();
    if ret_type == tir::builtin::UnitType::new(context) {
        vec![]
    } else {
        vec![ret_type]
    }
}

fn parse_argument(context: &Context, ty: tir::TypeId, text: &str) -> Result<Value, String> {
    let ty_data = context.get_type_data(ty);
    if let Some(integer) = (ty_data.as_ref() as &dyn std::any::Any).downcast_ref::<IntegerType>() {
        let parsed: i64 = text
            .parse()
            .map_err(|_| format!("'{text}' is not an integer"))?;
        return Ok(Value::Int(APInt::new_signed(integer.width(), parsed)));
    }
    if (ty_data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<tir::builtin::FloatType>()
        .is_some()
    {
        let parsed: f64 = text
            .parse()
            .map_err(|_| format!("'{text}' is not a float"))?;
        return Ok(Value::Float(APFloat::from_f64(parsed)));
    }
    Err("only integer and float arguments are supported".into())
}

fn format_value(context: &Context, value: &Value, ty: Option<tir::TypeId>) -> String {
    let spelled = ty
        .map(|ty| context.type_to_string(ty))
        .unwrap_or_else(|| "unit".into());
    let rendered = match value {
        Value::Int(int) => format!("{}", int.to_i64()),
        Value::Float(float) => format!("{}", float.to_f64()),
        Value::Ptr(address) => format!("{address:#x}"),
        Value::Tuple(elements) => {
            let inner: Vec<String> = elements
                .iter()
                .map(|element| format_value(context, element, None))
                .collect();
            format!("({})", inner.join(", "))
        }
        Value::Function(_) | Value::Token | Value::Unit => spelled.clone(),
    };
    format!("{spelled} {rendered}")
}
