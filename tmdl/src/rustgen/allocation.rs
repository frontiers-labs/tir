use std::collections::BTreeMap;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::shapes::{CmpOp, Predicate};
use crate::utils::OperandConstraint;

pub(super) enum Value {
    Integer(i64),
    String(String),
    Bool(bool),
}
impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}
impl From<&String> for Value {
    fn from(value: &String) -> Self {
        Self::String(value.clone())
    }
}
impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}
impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
macro_rules! integer {
    ($($ty:ty),*) => { $(impl From<$ty> for Value {
        fn from(value: $ty) -> Self { Self::Integer(value as i64) }
    })* };
}
integer!(i64, i32, u16, u32, usize);

impl quote::ToTokens for Value {
    fn to_tokens(&self, out: &mut TokenStream) {
        match self {
            Self::Integer(value) => out.extend(quote! { #value }),
            Self::String(value) => out.extend(quote! { #value }),
            Self::Bool(value) => out.extend(quote! { #value }),
        }
    }
}

pub(super) struct Fact {
    table: &'static str,
    values: Vec<Value>,
}
pub(super) fn fact(table: &'static str, values: Vec<Value>) -> Fact {
    Fact { table, values }
}

#[derive(Default)]
pub(super) struct Compiler {
    facts: Vec<Fact>,
    profile: Vec<TokenStream>,
    abis: Vec<TokenStream>,
    next_guard: usize,
}
impl Compiler {
    pub fn fact(&mut self, table: &'static str, values: Vec<Value>) {
        self.facts.push(fact(table, values));
    }
    pub fn facts(&mut self, facts: Vec<Fact>) {
        self.facts.extend(facts);
    }
    pub fn profile(&mut self, code: TokenStream) {
        self.profile.push(code);
    }
    pub fn abi(&mut self, name: &str, code: TokenStream) {
        let name = name.to_ascii_lowercase();
        self.abis.push(quote! { #name => { #code } });
    }
    pub fn insert(table: &str, values: &[TokenStream]) -> TokenStream {
        quote! {
            {
                let table = db.table(#table)?;
                db.insert(table, &[scope, #(db.base((#values).to_owned())),*])?;
            }
        }
    }
    fn predicate(&mut self, kind: &str, predicate: &Predicate) -> usize {
        let id = self.next_guard;
        self.next_guard += 1;
        let (tag, a, b, x, y, bits, comparison) = match predicate {
            Predicate::Always => ("true", "", "", 0, 0, 0, ""),
            Predicate::Not(child) => {
                let child = self.predicate(kind, child);
                self.fact(
                    "TMDLGuardChild",
                    vec![kind.into(), id.into(), 0.into(), child.into()],
                );
                ("not", "", "", 1, 0, 0, "")
            }
            Predicate::And(children) | Predicate::Or(children) => {
                for (slot, child) in children.iter().enumerate() {
                    let child = self.predicate(kind, child);
                    self.fact(
                        "TMDLGuardChild",
                        vec![kind.into(), id.into(), slot.into(), child.into()],
                    );
                }
                (
                    if matches!(predicate, Predicate::And(_)) {
                        "and"
                    } else {
                        "or"
                    },
                    "",
                    "",
                    children.len() as i64,
                    0,
                    0,
                    "",
                )
            }
            Predicate::Bit { op, bit } => ("bit", op.as_str(), "", i64::from(*bit), 0, 0, ""),
            Predicate::SliceEq { op, lo, hi, value } => (
                "slice-eq",
                op.as_str(),
                "",
                i64::from(*lo),
                i64::from(*hi),
                *value,
                "",
            ),
            Predicate::Same { a, b, width } => {
                ("same", a.as_str(), b.as_str(), i64::from(*width), 0, 0, "")
            }
            Predicate::Fits {
                op,
                width,
                bits,
                signed,
            } => (
                if *signed {
                    "signed-fits"
                } else {
                    "unsigned-fits"
                },
                op.as_str(),
                "",
                i64::from(*width),
                i64::from(*bits),
                0,
                "",
            ),
            Predicate::Cmp {
                op,
                width,
                cmp_width,
                cmp,
                value,
            } => {
                let comparison = match cmp {
                    CmpOp::Eq => "eq",
                    CmpOp::Ne => "ne",
                    CmpOp::Lt => "lt",
                    CmpOp::Le => "le",
                    CmpOp::Gt => "gt",
                    CmpOp::Ge => "ge",
                    CmpOp::ULt => "ult",
                    CmpOp::ULe => "ule",
                    CmpOp::UGt => "ugt",
                    CmpOp::UGe => "uge",
                };
                (
                    "cmp",
                    op.as_str(),
                    "",
                    i64::from(*width),
                    i64::from(*cmp_width),
                    *value as u128,
                    comparison,
                )
            }
        };
        self.fact(
            "TMDLGuard",
            vec![
                kind.into(),
                id.into(),
                tag.into(),
                a.into(),
                b.into(),
                x.into(),
                y.into(),
                0.into(),
                format!("{bits:x}").into(),
                comparison.into(),
            ],
        );
        id
    }
    pub fn shape(
        &mut self,
        kind: &str,
        shape: usize,
        predicate: &Predicate,
        width: u8,
        word: u128,
    ) {
        let guard = self.predicate(kind, predicate);
        self.fact(
            "TMDLEncodingShape",
            vec![
                kind.into(),
                shape.into(),
                guard.into(),
                i64::from(width).into(),
                format!("{word:x}").into(),
            ],
        );
    }
    pub fn field(
        &mut self,
        kind: &str,
        shape: usize,
        name: &str,
        index: usize,
        bounded: Option<u16>,
        constraint: OperandConstraint,
    ) {
        self.fact(
            "TMDLEncodingField",
            vec![
                kind.into(),
                shape.into(),
                index.into(),
                name.into(),
                format!("{:x}", constraint.align - 1).into(),
                constraint.nonzero.into(),
            ],
        );
        if let Some(n) = bounded {
            self.fact(
                "TMDLEncodingRange",
                vec![
                    kind.into(),
                    shape.into(),
                    index.into(),
                    (-(1i64 << (n - 1))).into(),
                    (1i64 << n).into(),
                    format!("{:x}", 1u128 << n).into(),
                ],
            );
        }
    }
    pub fn finish(self) -> TokenStream {
        let mut tables = BTreeMap::<&str, Vec<Vec<Value>>>::new();
        for fact in self.facts {
            tables.entry(fact.table).or_default().push(fact.values);
        }
        let facts = tables.iter().map(|(table, rows)| {
            let variables: Vec<_> = (0..rows[0].len()).map(|n| format_ident!("v{n}")).collect();
            let entries = rows.iter().map(|row| quote! { (#(#row,)*) });
            let insert = quote! {
                db.insert(table, &[scope, #(db.base(#variables.to_owned())),*])?;
            };
            let body = if let [row] = rows.as_slice() {
                quote! {
                    let (#(#variables,)*) = (#(#row,)*);
                    #insert
                }
            } else {
                quote! {
                    for &(#(#variables,)*) in &[#(#entries),*] { #insert }
                }
            };
            quote! {
                {
                    let table = db.table(#table)?;
                    #body
                }
            }
        });
        let invocation = Self::insert("TMDLTarget", &[]);
        let profile = self.profile;
        let abis = self.abis;
        let select_abi = if abis.is_empty() {
            quote! { let _ = abi; }
        } else {
            quote! {
                match abi.to_ascii_lowercase().as_str() {
                    #(#abis,)*
                    _ => return Err(format!("unknown target ABI '{abi}'").into()),
                }
            }
        };
        let lint_allow = super::generated_lint_allow();
        quote! {
            /// Install the selected target's canonical metadata into this context.
            #lint_allow
            #[allow(non_snake_case, unused_variables)]
            pub fn register_target_facts(context: &tir::Context, enabled: &[Feature], abi: &str) -> Result<(), Box<dyn std::error::Error>> {
                let mut engine = context.egraph();
                let db = engine.database_mut();
                let target_tables: Vec<_> = db.schema().tables()
                    .filter(|(_, table)| table.name().starts_with("TMDL"))
                    .map(|(id, _)| id)
                    .collect();
                for table in target_tables { db.clear(table); }
                let scope = db.base(0i64);
                #invocation
                #(#facts)*
                #(#profile)*
                #select_abi
                Ok(())
            }
        }
    }
}
