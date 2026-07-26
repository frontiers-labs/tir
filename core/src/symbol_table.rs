//! The symbol table of a module: every [`Symbol`] op directly inside its body,
//! indexed by name.
//!
//! Calls in TIR are overloadable, so a name alone does not identify a function.
//! Function symbols key on `(name, argument types)`; data symbols carry no
//! signature and own their name exclusively. Resolution is by exact argument
//! type match — no promotions, and the return type never participates.

use std::collections::HashMap;

use crate::analysis::{Analysis, AnalysisManager};
use crate::attributes::AttributeValue;
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
    symbols: HashMap<String, Vec<SymbolEntry>>,
}

impl SymbolTable {
    /// Collect the symbols defined directly in `module`'s body. Nested regions
    /// are not scanned: a module is the only symbol table, and it is flat.
    pub fn build(context: &Context, module: OpId) -> Self {
        let mut symbols: HashMap<String, Vec<SymbolEntry>> = HashMap::new();
        let instance = context.get_op(module);
        for region in &instance.regions {
            for block in context.get_region(*region).iter(context.clone()) {
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

    /// The overload of `name` whose argument types are exactly `args`.
    pub fn resolve(&self, name: &str, args: &[TypeId]) -> Option<&SymbolEntry> {
        self.lookup(name)
            .iter()
            .find(|entry| entry.signature.as_deref() == Some(args))
    }

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
        Ok(())
    }
}

/// Resolves `name` against the table of the module enclosing `user`, keying on
/// `args` when the reference carries argument types. Detached IR has no
/// enclosing table and is left unchecked.
pub fn resolve_symbol_use(
    context: &Context,
    user: OpId,
    name: &str,
    args: Option<&[TypeId]>,
) -> Result<Option<ResolvedSymbol>, crate::Error> {
    let Some(module) = enclosing_symbol_table(context, user) else {
        return Ok(None);
    };
    let table = SymbolTable::build(context, module);
    let candidates = table.lookup(name);
    if candidates.is_empty() {
        return Err(crate::Error::VerificationError(format!(
            "symbol '{}' is not defined in this module",
            format_symbol(context, name, args)
        )));
    }

    let entry = match args {
        Some(_) if candidates.iter().all(|entry| entry.signature.is_none()) => {
            return Err(crate::Error::VerificationError(format!(
                "symbol '@{name}' is not a function"
            )));
        }
        Some(args) => table.resolve(name, args).ok_or_else(|| {
            crate::Error::VerificationError(format!(
                "no overload matches {}; candidates: {}",
                format_symbol(context, name, Some(args)),
                describe_candidates(context, name, candidates)
            ))
        })?,
        None if candidates.len() > 1 => {
            return Err(crate::Error::VerificationError(format!(
                "reference to '@{name}' is ambiguous; candidates: {}",
                describe_candidates(context, name, candidates)
            )));
        }
        None => &candidates[0],
    };
    Ok(Some(ResolvedSymbol {
        result_type: entry.result_type,
    }))
}

/// The symbol a reference resolved to.
pub struct ResolvedSymbol {
    pub result_type: Option<TypeId>,
}

fn describe_candidates(context: &Context, name: &str, candidates: &[SymbolEntry]) -> String {
    candidates
        .iter()
        .map(|entry| format_symbol(context, name, entry.signature.as_deref()))
        .collect::<Vec<_>>()
        .join(", ")
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

/// The nearest enclosing op that owns a symbol table, walking out through
/// parent blocks. `None` for detached IR, which has no table to check against.
pub fn enclosing_symbol_table(context: &Context, op: OpId) -> Option<OpId> {
    let mut current = op;
    loop {
        let block = context.parent_block(current)?;
        let region = context.parent_region(block)?;
        let parent = context.get_region(region).parent_op()?;
        if context.get_op(parent).is::<crate::builtin::ModuleOp>() {
            return Some(parent);
        }
        current = parent;
    }
}

pub fn symbol_name_of(op: &impl Operation) -> String {
    op.attributes()
        .iter()
        .find(|attr| attr.name == "sym_name")
        .and_then(|attr| match &attr.value {
            AttributeValue::Str(name) => Some(name.clone()),
            _ => None,
        })
        .expect("symbol must carry sym_name")
}

/// Reads the optional `sym_visibility` attribute; a symbol without one is public.
pub fn visibility_of(op: &impl Operation) -> Visibility {
    let private = op.attributes().iter().any(|attr| {
        attr.name == "sym_visibility"
            && matches!(&attr.value, AttributeValue::Str(value) if value == "private")
    });
    if private {
        Visibility::Private
    } else {
        Visibility::Public
    }
}
