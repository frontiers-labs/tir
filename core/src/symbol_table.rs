//! The symbol table of a module: every [`Symbol`] op directly inside its body,
//! indexed by name.
//!
//! The table names symbols for linkage; it is not on the call path. A call
//! takes its callee as a value and a δ reference is the δ's own value, so a
//! transform that renames or duplicates a function never has to be re-resolved
//! against this. What is left is the uniqueness the object format demands:
//! function symbols key on `(name, argument types)` — TIR calls are
//! overloadable — and data symbols carry no signature and own their name
//! exclusively.

use std::collections::HashMap;

use tir_adt::IndexMap;

use crate::analysis::{Analysis, AnalysisManager};
use crate::attributes::AttributeValue;
use crate::builtin::{NamedType, TypeDefOp};
use crate::ty::TypeArg;
use crate::{Context, OpId, Operation, Symbol, TypeId, Visibility};

pub struct SymbolEntry {
    pub op: OpId,
    /// `None` for a data symbol; `Some(argument types)` for a function.
    pub signature: Option<Vec<TypeId>>,
    pub result_type: Option<TypeId>,
    pub visibility: Visibility,
    pub is_definition: bool,
}

pub struct SymbolTable {
    symbols: IndexMap<String, Vec<SymbolEntry>>,
}

impl SymbolTable {
    /// Collect the symbols defined directly in `module`'s body. Nested regions
    /// are not scanned: a module is the only symbol table, and it is flat.
    pub fn build(context: &Context, module: OpId) -> Self {
        let mut symbols: IndexMap<String, Vec<SymbolEntry>> = IndexMap::new();
        let instance = context.get_op(module);
        for region in instance.regions() {
            for block in context.get_region(region).iter(context.clone()) {
                for op in block.op_ids() {
                    let op_instance = context.get_op(op);
                    let Some(symbol) = op_instance.as_interface::<dyn Symbol>() else {
                        continue;
                    };
                    symbols
                        .entry(symbol.symbol_name())
                        .or_default()
                        .push(SymbolEntry {
                            op,
                            signature: symbol.symbol_signature(),
                            result_type: symbol.symbol_result_type(),
                            visibility: symbol.symbol_visibility(),
                            is_definition: symbol.is_definition(),
                        });
                }
            }
        }
        Self { symbols }
    }

    /// Every symbol carrying `name`, in definition order.
    pub fn lookup(&self, name: &str) -> &[SymbolEntry] {
        self.symbols.get(name).map_or(&[], Vec::as_slice)
    }

    /// Distinct symbol names in first-definition order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.symbols.keys().map(String::as_str)
    }

    /// Checks that no two symbols collide: names are unique per signature, and a
    /// name is either a function name or a data name, never both.
    pub fn verify(&self, context: &Context) -> Result<(), crate::Error> {
        for (name, entries) in &self.symbols {
            for (index, entry) in entries.iter().enumerate() {
                let rendered = || format_symbol(context, name, entry.signature.as_deref());
                for earlier in &entries[..index] {
                    if earlier.signature == entry.signature {
                        return Err(crate::Error::VerificationError(format!(
                            "symbol '{}' is defined more than once",
                            rendered()
                        )));
                    }
                    if earlier.signature.is_none() != entry.signature.is_none() {
                        return Err(crate::Error::VerificationError(format!(
                            "symbol '@{name}' is used both as a function and as a data symbol"
                        )));
                    }
                }
                if !entry.is_definition && entry.visibility == Visibility::Private {
                    return Err(crate::Error::VerificationError(format!(
                        "symbol '{}' is declared private but has no definition",
                        rendered()
                    )));
                }
            }
        }
        self.verify_type_definitions(context)
    }

    /// The `type.def` called `!name`.
    pub fn type_definition(&self, context: &Context, name: &str) -> Option<TypeDefOp> {
        let entry = self.lookup(&format!("!{name}")).first()?;
        context.get_op(entry.op).as_op::<TypeDefOp>()
    }

    /// The body `ty` stands for, when it is a named type with a complete
    /// definition here.
    pub fn expand(&self, context: &Context, ty: TypeId) -> Option<TypeId> {
        let named = NamedType::view(context, ty)?;
        self.type_definition(context, named.name())?
            .expand(named.args())
    }

    fn verify_type_definitions(&self, context: &Context) -> Result<(), crate::Error> {
        for (name, entries) in &self.symbols {
            let Some(name) = name.strip_prefix('!') else {
                continue;
            };
            let Some(definition) = context.get_op(entries[0].op).as_op::<TypeDefOp>() else {
                continue;
            };
            let Some(body) = definition.body() else {
                continue;
            };
            let mut error = None;
            crate::builtin::contained_by_value(
                context.terms(),
                body,
                |ty| self.expand(context, ty),
                |ty| {
                    let Some(named) = NamedType::view(context, ty) else {
                        return true;
                    };
                    if named.name() == name {
                        error = Some(format!("type `!{name}` contains itself by value"));
                    } else if self
                        .type_definition(context, named.name())
                        .is_some_and(|inner| inner.body().is_none())
                    {
                        error = Some(format!(
                            "type `!{name}` holds the incomplete type `!{}` by value",
                            named.name()
                        ));
                    }
                    error.is_none()
                },
            );
            if let Some(error) = error {
                return Err(crate::Error::VerificationError(error));
            }
        }
        Ok(())
    }
}

/// Move every symbol of module `from` into module `into`, before its end.
///
/// Type definitions follow C's compatible-type rule: a `!N` that `into`
/// defines with the same body and layout options is one type, so `from`'s
/// definition goes and its uses already name it. An incomplete `!N` and a
/// complete one are one type too: the complete definition stays. A `!N` with
/// another body is renamed `!N.1` (the first free suffix) and every type
/// `from` writes it in is rewritten to the new name. Bodies are compared with
/// those renames applied, so a `!A` naming a renamed `!B` is no longer
/// `into`'s `!A`.
pub fn merge_modules(context: &Context, into: OpId, from: OpId) -> Result<(), crate::Error> {
    let table = SymbolTable::build(context, into);
    let incoming = SymbolTable::build(context, from);
    let body = |module: OpId| {
        let region = context.get_op(module).regions()[0];
        context.get_region(region).block_ids()[0]
    };
    let (target, source) = (context.get_block(body(into)), context.get_block(body(from)));
    let definitions: Vec<(TypeDefOp, TypeDefOp)> = source
        .op_ids()
        .into_iter()
        .filter_map(|op| {
            let definition = context.get_op(op).as_op::<TypeDefOp>()?;
            let existing = table.type_definition(context, &definition.sym_name())?;
            Some((existing, definition))
        })
        .collect();
    let mut renames = HashMap::new();
    loop {
        let mut memo = HashMap::new();
        let differing: Vec<String> = definitions
            .iter()
            .filter(|(_, definition)| !renames.contains_key(&definition.sym_name()))
            .filter(|(existing, definition)| {
                let (Some(old), Some(new)) = (existing.body(), definition.body()) else {
                    return false;
                };
                old != rename_type(context, new, &renames, &mut memo)
                    || existing.params() != definition.params()
                    || existing.layout_options() != definition.layout_options()
                    || existing.layout_region().is_some()
                    || definition.layout_region().is_some()
            })
            .map(|(_, definition)| definition.sym_name())
            .collect();
        if differing.is_empty() {
            break;
        }
        for name in differing {
            let free = (1..)
                .map(|suffix| format!("{name}.{suffix}"))
                .find(|renamed| {
                    let symbol = format!("!{renamed}");
                    table.lookup(&symbol).is_empty() && incoming.lookup(&symbol).is_empty()
                })
                .expect("a free name");
            renames.insert(name, free);
        }
    }
    // Parsing `from` gave each shared name its body's kind; a renamed one
    // takes it along, and `into`'s keep their own.
    for (existing, definition) in &definitions {
        let Some(renamed) = renames.get(&definition.sym_name()) else {
            continue;
        };
        for (name, body) in [
            (renamed.clone(), definition.body()),
            (existing.sym_name(), existing.body()),
        ] {
            if let Some(body) = body {
                crate::builtin::define(context, &name, body);
            }
        }
    }
    let erase = |op: OpId| {
        context
            .erase_op(&crate::OperationRef::new(context.get_op(op)))
            .map_err(|error| crate::Error::VerificationError(error.to_string()))
    };
    let mut moved = Vec::new();
    for op in source.op_ids() {
        let instance = context.get_op(op);
        if instance.is::<crate::builtin::ModuleEndOp>() {
            continue;
        }
        if let Some(definition) = instance.as_op::<TypeDefOp>()
            && let Some(existing) = table.type_definition(context, &definition.sym_name())
            && !renames.contains_key(&definition.sym_name())
        {
            if existing.body().is_some() || definition.body().is_none() {
                erase(op)?;
                continue;
            }
            erase(existing.id())?;
        }
        source.remove_op(op);
        target.insert(target.len() - 1, op);
        moved.push(op);
    }
    if !renames.is_empty() {
        rename_types_in(context, &moved, &renames);
    }
    SymbolTable::build(context, into).verify(context)
}

// Rewrite the renamed names in every type the ops under `roots` hold: their
// results, attributes, ports and block arguments.
fn rename_types_in(context: &Context, roots: &[OpId], renames: &HashMap<String, String>) {
    let mut memo = HashMap::new();
    let mut rename = |ty| rename_type(context, ty, renames, &mut memo);
    for op in context.subtree_ops(roots) {
        let instance = context.get_op(op);
        for result in instance.results().iter() {
            context.retype_value(*result, rename(context.get_value(*result).ty()));
        }
        let renamed = instance
            .clone()
            .as_op::<TypeDefOp>()
            .and_then(|definition| renames.get(&definition.sym_name()));
        let attributes = instance
            .attributes()
            .into_iter()
            .map(|mut attribute| {
                attribute.value = match renamed {
                    Some(name) if context.resolve(attribute.name) == "sym_name" => {
                        AttributeValue::from(name.as_str())
                    }
                    _ => attribute.value.map_types(&mut rename),
                };
                attribute
            })
            .collect();
        context.set_op_attributes(op, attributes);
        for region in instance.regions().iter() {
            let region = context.get_region(*region);
            if region.is_nodes() {
                for port in region.ports() {
                    context.retype_value(port.id(), rename(port.ty()));
                }
            }
            for block in region.block_ids() {
                for (index, argument) in context.get_block(block).arguments().iter().enumerate() {
                    context.retype_block_argument(block, index, rename(argument.ty()));
                }
            }
        }
    }
}

fn rename_type(
    context: &Context,
    ty: TypeId,
    renames: &HashMap<String, String>,
    memo: &mut HashMap<TypeId, TypeId>,
) -> TypeId {
    if let Some(&renamed) = memo.get(&ty) {
        return renamed;
    }
    let term = context.terms().term(ty);
    let args: Vec<TypeArg> = term
        .args()
        .iter()
        .map(|arg| rename_arg(context, arg, renames, memo))
        .collect();
    let renamed = match NamedType::view(context, ty) {
        Some(named) => {
            let name = renames
                .get(named.name())
                .map_or(named.name(), String::as_str);
            let TypeArg::List(args) = &args[1] else {
                unreachable!("a named type's arguments are a list")
            };
            NamedType::get(context, name, args.to_vec()).expect("a renamed type")
        }
        None if args == term.args() => ty,
        None => context
            .terms()
            .intern(term.ctor(), args)
            .expect("a renamed type"),
    };
    memo.insert(ty, renamed);
    renamed
}

fn rename_arg(
    context: &Context,
    arg: &TypeArg,
    renames: &HashMap<String, String>,
    memo: &mut HashMap<TypeId, TypeId>,
) -> TypeArg {
    match arg {
        TypeArg::Type(ty) => TypeArg::Type(rename_type(context, *ty, renames, memo)),
        TypeArg::List(items) => TypeArg::List(
            items
                .iter()
                .map(|item| rename_arg(context, item, renames, memo))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub(crate) fn signature_accepts(context: &Context, signature: &[TypeId], args: &[TypeId]) -> bool {
    match signature.split_last() {
        Some((&last, fixed)) if context.is_variadic_tail(last) => {
            args.len() >= fixed.len() && &args[..fixed.len()] == fixed
        }
        _ => signature == args,
    }
}

/// Renders a symbol as `@name(t1, t2)`, or `@name` when it has no signature.
pub fn format_symbol(context: &Context, name: &str, signature: Option<&[TypeId]>) -> String {
    match signature {
        None => format!("@{name}"),
        Some(types) => {
            let types: Vec<_> = types.iter().map(|ty| context.type_to_string(*ty)).collect();
            format!("@{name}({})", types.join(", "))
        }
    }
}

impl Analysis for SymbolTable {
    fn build(_analyses: &AnalysisManager, context: &Context, op: OpId) -> Self {
        Self::build(context, op)
    }
}

pub fn symbol_name_of(op: &impl Operation) -> String {
    match op.attr("sym_name") {
        Some(AttributeValue::Str(name)) => name.to_string(),
        _ => panic!("symbol must carry sym_name"),
    }
}

/// Reads the optional `sym_visibility` attribute; a symbol without one is public.
pub fn visibility_of(op: &impl Operation) -> Visibility {
    let private = matches!(
        op.attr("sym_visibility"),
        Some(AttributeValue::Str(value)) if &*value == "private"
    );
    if private {
        Visibility::Private
    } else {
        Visibility::Public
    }
}
