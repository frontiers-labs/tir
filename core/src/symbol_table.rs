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

pub(crate) fn signature_accepts(context: &Context, signature: &[TypeId], args: &[TypeId]) -> bool {
    match signature.split_last() {
        Some((&last, fixed)) if context.get_type_data(last).is_variadic_tail() => {
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
