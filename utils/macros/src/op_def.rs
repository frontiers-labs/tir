use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use proc_macro::TokenStream;
use quote::quote;
use syn::{Expr, Ident, LitStr, Token, braced, parse::Parse, parse_macro_input};
use tir_cdsl::output::{Operation, Region, StatePorts, ValueSpec, check_operation, emit_operation};
use tir_cdsl::types::Type;
use tir_cdsl::{SourceMap, Value};

struct Input {
    file: LitStr,
    hosts: BTreeMap<String, Expr>,
}

fn path_name(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

impl Parse for Input {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let file = if input.peek(LitStr) {
            input.parse()?
        } else {
            let key: Ident = input.parse()?;
            if key != "file" {
                return Err(input.error("expected file or a CDSL path literal"));
            }
            input.parse::<Token![:]>()?;
            input.parse()?
        };
        let mut result = Self {
            file,
            hosts: BTreeMap::new(),
        };
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            let content;
            braced!(content in input);
            match key.to_string().as_str() {
                "hosts" => {
                    while !content.is_empty() {
                        let path = content.parse()?;
                        content.parse::<Token![=>]>()?;
                        result.hosts.insert(path_name(&path), content.parse()?);
                        if !content.is_empty() {
                            content.parse::<Token![,]>()?;
                        }
                    }
                }
                _ => return Err(syn::Error::new_spanned(key, "unknown output option")),
            }
        }
        Ok(result)
    }
}

fn diagnostics(errors: Vec<tir_cdsl::diagnostic::Diagnostic>) -> String {
    errors
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
fn fields(value: &Value) -> Result<&[(String, Value)], String> {
    match value {
        Value::Record(fields) => Ok(fields),
        Value::Named { payload, .. } => fields(payload),
        _ => Err("operation must evaluate to a record".into()),
    }
}
fn field<'a>(fields: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|(label, value)| (label == name).then_some(value))
}
fn host<'a>(input: &'a Input, value: &Value) -> Result<&'a Expr, String> {
    let Value::Foreign { identity, .. } = value else {
        return Err("host hook must reference a checked extern binding".into());
    };
    let name = identity.strip_prefix("main::").unwrap_or(identity);
    input
        .hosts
        .get(name)
        .ok_or_else(|| format!("missing Rust binding for checked host `{name}`"))
}

fn expand(input: &Input) -> Result<proc_macro2::TokenStream, String> {
    let root =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").map_err(|error| error.to_string())?)
            .join(input.file.value());
    let (sources, dependencies) = SourceMap::from_file(&root).map_err(diagnostics)?;
    let checked = tir_cdsl::output::compile_operations(&sources).map_err(diagnostics)?;
    let mut output = proc_macro2::TokenStream::new();
    for file in dependencies {
        let file = file.to_str().ok_or("source path must be UTF-8")?;
        output.extend(quote! { const _: &str = include_str!(#file); });
    }
    let exports: BTreeMap<_, _> = checked
        .bindings()
        .filter(|(name, _)| tir_cdsl::output::is_operation(&checked, name))
        .collect();
    let mut emitted = BTreeSet::new();
    for binding in exports.keys() {
        let contract = check_operation(&checked, binding).map_err(|error| error.to_string())?;
        let value = checked
            .evaluate_template(binding)
            .map_err(|error| error.to_string())?;
        if !matches!(value, Value::Named { .. }) {
            return Err("operation export lost its checked nominal identity".into());
        }
        let identity = *binding;
        if !emitted.insert(identity.to_owned()) {
            continue;
        }
        let record = fields(&value)?;
        if field(record, "environment").is_some() {
            return Err(format!(
                "checked operation `{binding}`: `environment` lowering is not available in this output profile yet"
            ));
        }

        let (dialect, name) = identity
            .rsplit_once("::")
            .ok_or("operation must have a dialect namespace")?;
        let mut regions = Vec::new();
        if let Some(value) = field(record, "regions") {
            for (name, value) in fields(value)? {
                let region = fields(value)?;
                let Some(Value::Constructor(kind, args)) = field(region, "kind") else {
                    return Err("region kind must use RegionKind".into());
                };
                if !args.is_empty() {
                    return Err("region kind has unexpected arguments".into());
                }
                let kind = kind
                    .rsplit("::")
                    .next()
                    .ok_or("region kind missing")?
                    .to_owned();
                let variadic = matches!(field(region, "variadic"), Some(Value::Bool(true)));
                regions.push(Region {
                    name: name.clone(),
                    kind,
                    variadic,
                });
            }
        }
        let mut interfaces = Vec::new();
        let mut interface_callbacks = Vec::new();
        if let Some(Value::List(values)) = field(record, "interfaces") {
            for value in values {
                let expression = host(input, value)?;
                match expression {
                    Expr::Tuple(pair) if pair.elems.len() == 2 => {
                        let mut values = pair.elems.iter();
                        let Some(Expr::Path(path)) = values.next() else {
                            return Err("interface callback pair needs a trait path".into());
                        };
                        let Some(callback) = values.next() else {
                            return Err("interface callback missing".into());
                        };
                        interfaces.push(path.path.clone());
                        interface_callbacks.push((path.path.clone(), callback.clone()));
                    }
                    Expr::Path(path) => interfaces.push(path.path.clone()),
                    _ => return Err("interface binding requires (Trait,typed callbacks)".into()),
                }
            }
        }
        let verify_hook = field(record, "verify")
            .map(|verify| host(input, verify).cloned())
            .transpose()?;
        let custom_verifier = verify_hook.is_some();
        check_sort_witnesses(&checked, &contract)?;
        let (operands, results, scheme) = signature_fields(&contract)?;
        let (attributes, defaults) = attribute_fields(record)?;
        let (binds, counted) = field(record, "bindings")
            .map(|value| {
                decode_bindings(value, &operands, &results, &regions)
                    .map(|(binds, counted)| (Some(binds), counted))
            })
            .transpose()?
            .unwrap_or((None, None));
        let format = if let Some(assembly) = field(record, "assembly") {
            let assembly = fields(assembly)?;
            let print = host(
                input,
                field(assembly, "print").ok_or("assembly printer missing")?,
            )?
            .clone();
            let parse = host(
                input,
                field(assembly, "parse").ok_or("assembly parser missing")?,
            )?
            .clone();
            tir_cdsl::output::RustFormat::Callbacks {
                print,
                parse: Box::new(parse),
            }
        } else {
            tir_cdsl::output::RustFormat::Generic
        };

        let sem = field(record, "exec")
            .map(|value| {
                crate::checked_sem::lower(&checked, value, &contract, &defaults, &input.hosts)
            })
            .transpose()?;
        let poison = field(record, "poison")
            .map(|value| {
                crate::checked_sem::lower(&checked, value, &contract, &defaults, &input.hosts)
            })
            .transpose()?;
        let requires = field(record, "requires")
            .map(|value| {
                crate::checked_sem::lower(&checked, value, &contract, &defaults, &input.hosts)
            })
            .transpose()?;
        let state_ports = operands
            .iter()
            .chain(&results)
            .any(|port| port.ty == "*state<_>");
        if sem.is_some()
            && state_ports
            && !interfaces.iter().any(|path: &syn::Path| {
                path.segments
                    .last()
                    .is_some_and(|part| part.ident == "ResourceEffects")
            })
        {
            return Err(format!(
                "stateful semantic model `{binding}` needs its ResourceEffects binding"
            ));
        }
        let splice_callbacks = [&sem, &poison, &requires]
            .into_iter()
            .flatten()
            .flat_map(|semantic| semantic.splices.iter())
            .map(|name| {
                input
                    .hosts
                    .get(name)
                    .cloned()
                    .map(|callback| (name.clone(), callback))
                    .ok_or_else(|| format!("missing typed semantic callback `{name}`"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        output.extend(emit_operation(Operation {
            name: name.into(),
            dialect: dialect.into(),
            regions,
            attributes,
            operands,
            results,
            interfaces,
            interface_callbacks,
            splice_callbacks,
            format,
            sem,
            semantic_hook: None,
            poison,
            requires,
            custom_verifier,
            verify_hook,
            state: StatePorts::default(),
            binds,
            counted,
            scheme,
        }));
    }
    Ok(output)
}

pub fn construct_op_def(tokens: TokenStream) -> TokenStream {
    let input = parse_macro_input!(tokens as Input);
    expand(&input)
        .unwrap_or_else(|error| syn::Error::new_spanned(&input.file, error).into_compile_error())
        .into()
}

fn signature_fields(
    operation: &tir_cdsl::output::CheckedOperation<'_>,
) -> Result<(Vec<ValueSpec>, Vec<ValueSpec>, String), String> {
    let mut occurrences = BTreeMap::<String, usize>::new();
    let mut counts = BTreeSet::<String>::new();
    fn count(ty: &Type, out: &mut BTreeMap<String, usize>, counts: &mut BTreeSet<String>) {
        match ty {
            Type::Parameter(name) | Type::Index(name, _) => {
                *out.entry(name.clone()).or_default() += 1
            }
            Type::Nominal(name, _)
                if matches!(name.rsplit("::").next(), Some("States" | "State")) => {}
            Type::Nominal(name, args) => {
                if name.ends_with("::Count")
                    && let Some(Type::Parameter(name)) = args.first()
                {
                    counts.insert(name.clone());
                }
                for arg in args {
                    count(arg, out, counts);
                }
            }
            Type::Tuple(fields) => {
                for (_, ty) in fields {
                    count(ty, out, counts);
                }
            }
            _ => {}
        }
    }
    for (_, ty) in &operation.operands {
        count(ty, &mut occurrences, &mut counts);
    }
    count(&operation.result, &mut occurrences, &mut counts);
    fn pattern(ty: &Type, occurrences: &BTreeMap<String, usize>) -> Result<String, String> {
        let argument = |ty: &Type| match ty {
            Type::IntegerIndex(value) => Ok(value.to_string()),
            Type::Parameter(name) | Type::Index(name, _) => {
                Ok(if occurrences.get(name).copied().unwrap_or(0) > 1 {
                    name.clone()
                } else {
                    "_".into()
                })
            }
            _ => pattern(ty, occurrences),
        };
        match ty {
            Type::Parameter(_) | Type::Index(_, _) => argument(ty),
            Type::Nominal(name, args) => {
                let family = name.rsplit("::").next().ok_or("sort name missing")?;
                let literal = match family {
                    "Any" => Some("_"),
                    "AnyInt" => Some("int<_>"),
                    "AnyFloat" => Some("float<_, _>"),
                    "AnyPtr" => Some("ptr.p<_>"),
                    "AnyFn" => Some("fn<_, _>"),
                    "AnyTuple" => Some("tuple<_>"),
                    "Index" => Some("index"),
                    "Control" => Some("son.control"),
                    "FpEnv" => Some("fp.env"),
                    "FpRounding" => Some("fp.rounding"),
                    "LayoutFields" => Some("layout.fields"),
                    "LayoutType" => Some("layout.type"),
                    "AnyReg" => Some("backend.reg<_, _>"),
                    "VaList" => Some("cir.va_list"),
                    "AnyState" | "State" => Some("state<_>"),
                    _ => None,
                };
                if let Some(literal) = literal {
                    return Ok(literal.into());
                }
                let expected = match family {
                    "Int" | "Ptr" | "Count" | "Many" | "Optional" | "States" => 1,
                    "Float" | "Vector" => 2,
                    _ => return Err(format!("no runtime IrSort binding for {ty}")),
                };
                if args.len() != expected {
                    return Err(format!("wrong IrSort arity for {ty}"));
                }
                match family {
                    "Int" => Ok(format!("int<{}>", argument(&args[0])?)),
                    "Ptr" => Ok(format!("ptr.p<{}>", argument(&args[0])?)),
                    "Float" => Ok(format!(
                        "float<{}, {}>",
                        argument(&args[0])?,
                        argument(&args[1])?
                    )),
                    "Vector" => Ok(format!(
                        "vector.v<{}, {}>",
                        argument(&args[0])?,
                        argument(&args[1])?
                    )),
                    "Count" => argument(&args[0]),
                    "Many" => Ok(format!("*{}", pattern(&args[0], occurrences)?)),
                    "Optional" => Ok(format!("?{}", pattern(&args[0], occurrences)?)),
                    "States" => Ok("*state<_>".into()),
                    _ => Err("sort binding disappeared".into()),
                }
            }
            _ => Err(format!("{ty} has no runtime IrSort binding")),
        }
    }
    let port = |name: String, ty: &Type| -> Result<ValueSpec, String> {
        let ty = pattern(ty, &occurrences)?;
        Ok(ValueSpec {
            name,
            variadic: ty.starts_with('*'),
            ty,
        })
    };
    let operands = operation
        .operands
        .iter()
        .map(|(name, ty)| port(name.clone(), ty))
        .collect::<Result<Vec<_>, _>>()?;
    let results = match &operation.result {
        Type::Tuple(fields) => fields
            .iter()
            .enumerate()
            .map(|(index, (name, ty))| {
                port(name.clone().unwrap_or_else(|| format!("result{index}")), ty)
            })
            .collect::<Result<Vec<_>, _>>()?,
        ty => vec![port("result".into(), ty)?],
    };
    let binders = operation
        .definition
        .scheme
        .parameters
        .iter()
        .filter(|(name, _)| occurrences.get(name).copied().unwrap_or(0) > 1)
        .map(|(name, kind)| {
            if counts.contains(name) {
                Ok(format!("{name}: index | int<_>"))
            } else {
                match kind {
                    Type::Nat => Ok(format!("{name}: nat")),
                    Type::Universe(_) => Ok(name.clone()),
                    _ => Err(format!("unsupported scheme binder {name}:{kind}")),
                }
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let prefix = if binders.is_empty() {
        String::new()
    } else {
        format!("forall {}. ", binders.join(", "))
    };
    let slots = |ports: &[ValueSpec]| {
        ports
            .iter()
            .map(|port| {
                if let Some(pattern) = port.ty.strip_prefix('*') {
                    format!("{pattern}*")
                } else if let Some(pattern) = port.ty.strip_prefix('?') {
                    format!("{pattern}?")
                } else {
                    port.ty.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let scheme = tir_symbolic::scheme::SchemeSyntax::parse(&format!(
        "{prefix}({}) -> ({})",
        slots(&operands),
        slots(&results)
    ))
    .map_err(|error| error.to_string())?
    .to_string();
    Ok((operands, results, scheme))
}
fn check_sort_witnesses(
    module: &tir_cdsl::CheckedModule,
    operation: &tir_cdsl::output::CheckedOperation<'_>,
) -> Result<(), String> {
    for (_, ty) in &operation.operands {
        check_sort_witness(module, ty)?;
    }
    check_sort_witness(module, &operation.result)
}
fn check_sort_witness(module: &tir_cdsl::CheckedModule, ty: &Type) -> Result<(), String> {
    match ty {
        Type::Parameter(_) => Ok(()),
        Type::Tuple(fields) => {
            for (_, ty) in fields {
                check_sort_witness(module, ty)?;
            }
            Ok(())
        }
        Type::Nominal(name, args) => {
            let (namespace, family) = name
                .rsplit_once("::")
                .ok_or("sort has no checked library namespace")?;
            if !matches!(namespace, "ops" | "library") {
                return Err(format!("unregistered sort namespace `{namespace}`"));
            }
            if matches!(family, "Many" | "Optional")
                && let Some(inner) = args.first()
            {
                check_sort_witness(module, inner)?;
            }
            let witness = match name.as_str() {
                "ops::Int" => "ops::int_sort".into(),
                "ops::Ptr" => "ops::ptr_sort".into(),
                "ops::States" => "ops::states_sort".into(),
                _ => format!("{namespace}::{}_sort", family.to_lowercase()),
            };
            let definition = module
                .definition(&witness)
                .ok_or_else(|| format!("missing checked IrSort witness `{witness}`"))?;
            if definition.scheme.parameters.len() != args.len() {
                return Err(format!("wrong IrSort witness arity `{witness}`"));
            }
            let arguments = definition
                .scheme
                .parameters
                .iter()
                .zip(args)
                .map(|((name, _), ty)| (name.clone(), ty.clone()))
                .collect();
            let expected = Type::Nominal("ops::IrSort".into(), vec![ty.clone()]);
            if definition.scheme.ty.substitute(&arguments) != expected {
                return Err(format!("IrSort witness `{witness}` does not describe {ty}"));
            }
            Ok(())
        }
        _ => Err(format!("{ty} has no portable IrSort contract")),
    }
}
fn attribute_fields(
    record: &[(String, Value)],
) -> Result<(Vec<tir_cdsl::output::AttrSpec>, BTreeMap<String, bool>), String> {
    let mut attributes = Vec::new();
    let mut defaults = BTreeMap::new();
    let Some(value) = field(record, "attributes") else {
        return Ok((attributes, defaults));
    };
    for (name, value) in fields(value)? {
        let (ty, defaulted, codec, vocabulary) = match value {
            Value::Description(ty) => (ty, false, None, None),
            Value::Record(descriptor) => {
                let Some(Value::Description(ty)) = field(descriptor, "ty") else {
                    return Err("attribute requires a checked description".into());
                };
                if let Some(Value::Bool(value)) = field(descriptor, "default") {
                    defaults.insert(name.clone(), *value);
                }
                (
                    ty,
                    field(descriptor, "default").is_some(),
                    field(descriptor, "codec"),
                    field(descriptor, "vocabulary"),
                )
            }
            _ => return Err("attribute requires a checked description".into()),
        };
        let optional =
            matches!(ty,Type::Nominal(name,args) if name=="library::Optional"&&args.len()==1);
        let inner = if let Type::Nominal(_, args) = ty {
            if optional { &args[0] } else { ty }
        } else {
            ty
        };
        let code = if let Some(Value::Foreign { identity, .. }) = codec {
            Some(
                identity
                    .strip_prefix("main::")
                    .unwrap_or(identity)
                    .rsplit("::")
                    .next()
                    .ok_or("codec name missing")?,
            )
        } else {
            None
        };
        let storage = match (inner, code) {
            (Type::Bool, Some("flag_codec" | "_flag_codec")) => "Flag",
            (Type::Bool, None | Some("bool_codec")) => "Bool",
            (Type::Integer, None) => "Int",
            (Type::Nat, None) => "UInt",
            (Type::String, None | Some("str_codec")) => "Str",
            (Type::Nominal(name, _), Some(codec)) => match (name.rsplit("::").next(), codec) {
                (Some("AttrArray"), "array_codec" | "_array_codec") => "Array",
                (Some("AttrDict"), "dict_codec" | "_dict_codec") => "Dict",
                (Some("AttrBlock"), "block_codec") => "Block",
                (Some("AttrType"), "type_codec" | "_type_codec") => "Type",
                (Some("AttrInt"), "int_codec") => "Int",
                (Some("AttrUInt"), "uint_codec") => "UInt",
                (Some("AttrPredicate"), "predicate_codec") => "Predicate",
                (Some("AttrRegister"), "register_codec" | "_register_codec") => "Register",
                (Some("AttrAny"), "any_codec" | "_any_codec") => "any",
                (Some("AttrFpSemantics"), "fpsemantics_codec") => "FpSemantics",
                (Some("AttrEvaluationContract"), "evaluationcontract_codec") => {
                    "EvaluationContract"
                }
                _ => return Err(format!("unsupported portable codec `{codec}` for {inner}")),
            },
            _ => return Err(format!("attribute `{name}` has no portable codec")),
        };
        let vocabulary = match vocabulary {
            None => None,
            Some(Value::String(name))
                if ["INTEGER", "FLOAT", "POINTER"].contains(&name.as_str()) =>
            {
                Some(name.clone())
            }
            _ => return Err("unknown predicate vocabulary".into()),
        };
        attributes.push(tir_cdsl::output::AttrSpec {
            name: name.clone(),
            ty: format!("{}{storage}", if optional || defaulted { "?" } else { "" }),
            vocabulary,
        });
    }
    Ok((attributes, defaults))
}

fn decode_bindings(
    value: &Value,
    operands: &[ValueSpec],
    results: &[ValueSpec],
    regions: &[Region],
) -> Result<
    (
        tir_cdsl::output::binds::Binds,
        Option<tir_cdsl::output::binds::Counted>,
    ),
    String,
> {
    use tir_cdsl::output::binds::{BindKind, Binds, Chain, Counted, Idx, Slice, Term};
    fn integer(value: &Value) -> Result<usize, String> {
        match value {
            Value::Integer(value) => {
                usize::try_from(*value).map_err(|_| "binding index must fit usize".into())
            }
            _ => Err("binding index must be Nat".into()),
        }
    }
    fn text(value: &Value) -> Result<String, String> {
        match value {
            Value::String(value) => Ok(value.clone()),
            _ => Err("binding label must be String".into()),
        }
    }
    fn index(value: &Value) -> Result<Idx, String> {
        let fields = fields(value)?;
        Ok(Idx {
            n: matches!(field(fields, "n"), Some(Value::Bool(true))),
            offset: integer(field(fields, "offset").ok_or("binding offset missing")?)?,
        })
    }
    fn optional(value: &Value) -> Result<Option<&Value>, String> {
        match value {
            Value::Constructor(name, args) if name.ends_with("::None") && args.is_empty() => {
                Ok(None)
            }
            Value::Constructor(name, args) if name.ends_with("::Some") && args.len() == 1 => {
                Ok(args.first())
            }
            _ => Err("binding range end must be Optional<BindIndex>".into()),
        }
    }
    fn slice(value: &Value) -> Result<Slice, String> {
        let Value::Constructor(name, args) = value else {
            return Err("binding slice must use BindSlice".into());
        };
        match (name.rsplit("::").next(), args.as_slice()) {
            (Some("All"), []) => Ok(Slice::All),
            (Some("One"), [at]) => Ok(Slice::One(index(at)?)),
            (Some("Range"), [start, end]) => Ok(Slice::Range(
                index(start)?,
                optional(end)?.map(index).transpose()?,
            )),
            _ => Err("invalid binding slice".into()),
        }
    }
    fn ident(value: &Value) -> Result<Ident, String> {
        syn::parse_str(&text(value)?).map_err(|error| error.to_string())
    }
    fn term(value: &Value, operands: &[ValueSpec], regions: &[Region]) -> Result<Term, String> {
        let Value::Constructor(name, args) = value else {
            return Err("binding term must use BindTerm".into());
        };
        match (name.rsplit("::").next(), args.as_slice()) {
            (Some("Operands"), [Value::List(names)]) => {
                if names.is_empty() {
                    return Err("binding operand group cannot be empty".into());
                }
                let names = names.iter().map(ident).collect::<Result<Vec<_>, _>>()?;
                let indices = names
                    .iter()
                    .map(|name| {
                        operands
                            .iter()
                            .position(|port| *name == port.name)
                            .ok_or_else(|| format!("unknown binding operand `{name}`"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if !indices
                    .windows(2)
                    .all(|indices| indices[1] == indices[0] + 1)
                {
                    return Err("binding operand groups must be consecutive".into());
                }
                Ok(Term::Operands(names))
            }
            (Some("Results"), [at]) => Ok(Term::Results(slice(at)?)),
            (Some("Region"), [record]) => {
                let record = fields(record)?;
                let name = ident(field(record, "name").ok_or("binding region missing")?)?;
                if !regions.iter().any(|region| name == region.name) {
                    return Err(format!("unknown binding region `{name}`"));
                }
                let Some(Value::Bool(ports)) = field(record, "ports") else {
                    return Err("binding region selector must be Bool".into());
                };
                Ok(Term::Region {
                    name,
                    ports: *ports,
                    slice: slice(field(record, "slice").ok_or("binding region slice missing")?)?,
                })
            }
            _ => Err("invalid binding term".into()),
        }
    }
    let record = fields(value)?;
    let Some(Value::Constructor(kind, args)) = field(record, "kind") else {
        return Err("bindings require BindKind".into());
    };
    if !args.is_empty() {
        return Err("binding kind takes no arguments".into());
    }
    let kind = match kind.rsplit("::").next() {
        Some("Theta") => BindKind::Theta,
        Some("Gamma") => BindKind::Gamma,
        _ => return Err("unknown binding kind".into()),
    };
    let predicate = term(
        field(record, "predicate").ok_or("binding predicate missing")?,
        operands,
        regions,
    )?;
    let Some(Value::List(chains)) = field(record, "chains") else {
        return Err("binding chains missing".into());
    };
    let chains = chains
        .iter()
        .map(|chain| {
            let record = fields(chain)?;
            let name = text(field(record, "name").ok_or("binding chain name missing")?)?;
            let Some(Value::List(terms)) = field(record, "terms") else {
                return Err("binding chain terms missing".into());
            };
            Ok((
                name,
                Chain(
                    terms
                        .iter()
                        .map(|value| term(value, operands, regions))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let counted = field(record, "counted")
        .map(|counted| {
            let record = fields(counted)?;
            let get = |name| {
                field(record, name).ok_or_else(|| format!("counted binding `{name}` missing"))
            };
            let lb = ident(get("lb")?)?;
            let ub = ident(get("ub")?)?;
            let step = ident(get("step")?)?;
            for name in [&lb, &ub, &step] {
                if !operands.iter().any(|port| *name == port.name) {
                    return Err(format!("unknown counted operand `{name}`"));
                }
            }
            Ok(Counted {
                induction: integer(get("induction")?)?,
                lb,
                ub,
                step,
            })
        })
        .transpose()?;
    if operands.iter().any(|operand| operand.ty.starts_with('?')) {
        return Err("bindings do not support optional operand groups".into());
    }
    if results
        .iter()
        .filter(|result| result.ty == "*state<_>")
        .count()
        > 1
    {
        return Err("operation has multiple state result groups".into());
    }
    Ok((
        Binds {
            kind,
            predicate,
            chains,
        },
        counted,
    ))
}
