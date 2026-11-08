//! Checked types and exported constraint descriptions.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A checked core type. Nominal identities include their declaring module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    /// Inference metavariable, solved before a monomorphic value is exported.
    Variable(u32),
    /// Rigid erased type parameter.
    Parameter(String),
    /// Rigid static parameter with its kind.
    Index(String, Box<Type>),
    /// Integer-valued static argument.
    IntegerIndex(i128),
    /// Boolean-valued static argument.
    BooleanIndex(bool),
    /// String-valued static argument.
    StringIndex(String),
    /// Predicative universe level.
    Universe(u32),
    /// Semantic Boolean.
    Bool,
    /// Signed, checked 128-bit core integer.
    Integer,
    /// Nonnegative checked 128-bit core integer.
    Nat,
    /// String.
    String,
    /// Nominal inductive or opaque type and invariant arguments.
    Nominal(String, Vec<Type>),
    /// Function with stable source argument labels.
    Function(Vec<(String, Type)>, Box<Type>),
    /// Product; named singleton products remain products.
    Tuple(Vec<(Option<String>, Type)>),
    /// Finite row with an optional row metavariable.
    Record(BTreeMap<String, Type>, Option<Box<Type>>),
    /// Homogeneous immutable list.
    List(Box<Type>),
    /// Typed description of the enclosed decoded type.
    Description(Box<Type>),
    /// Stable qualified identity paired with a payload.
    Named(Box<Type>),
}

/// An inferred statically selected operation obligation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint {
    /// Dictionary class, such as `Add`, `FromInteger`, or `Describe`.
    pub class: String,
    /// Invariant class arguments.
    pub arguments: Vec<Type>,
}
impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}<{}>",
            self.class,
            self.arguments
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Variable(n) => write!(f, "?{n}"),
            Self::Parameter(n) | Self::StringIndex(n) => f.write_str(n),
            Self::Index(n, _) => f.write_str(n),
            Self::IntegerIndex(n) => write!(f, "{n}"),
            Self::BooleanIndex(b) => write!(f, "{b}"),
            Self::Universe(n) => write!(f, "Type_{n}"),
            Self::Bool => f.write_str("Bool"),
            Self::Integer => f.write_str("Integer"),
            Self::Nat => f.write_str("Nat"),
            Self::String => f.write_str("String"),
            Self::Nominal(n, args) => {
                f.write_str(n)?;
                if !args.is_empty() {
                    write!(
                        f,
                        "<{}>",
                        args.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )?;
                }
                Ok(())
            }
            Self::Function(args, result) => write!(
                f,
                "({}) -> {result}",
                args.iter()
                    .map(|(n, t)| format!("{n}: {t}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Tuple(fields) => write!(
                f,
                "({})",
                fields
                    .iter()
                    .map(|(n, t)| n
                        .as_ref()
                        .map_or_else(|| t.to_string(), |n| format!("{n}: {t}")))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Record(fields, tail) => write!(
                f,
                "Record<{{{}{}}}>",
                fields
                    .iter()
                    .map(|(n, t)| format!("{n}: {t}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                tail.as_ref()
                    .map_or_else(String::new, |t| format!(", ..{t}"))
            ),
            Self::List(t) => write!(f, "List<{t}>"),
            Self::Description(t) => write!(f, "TypeRep<{t}>"),
            Self::Named(t) => write!(f, "Named<{t}>"),
        }
    }
}

impl Type {
    pub(crate) fn map(&self, replace: &mut impl FnMut(&Type) -> Option<Type>) -> Type {
        if let Some(ty) = replace(self) {
            return ty;
        }
        match self {
            Self::Index(n, t) => Self::Index(n.clone(), Box::new(t.map(replace))),
            Self::Nominal(n, args) => {
                Self::Nominal(n.clone(), args.iter().map(|t| t.map(replace)).collect())
            }
            Self::Function(args, res) => Self::Function(
                args.iter()
                    .map(|(n, t)| (n.clone(), t.map(replace)))
                    .collect(),
                Box::new(res.map(replace)),
            ),
            Self::Tuple(fields) => Self::Tuple(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), t.map(replace)))
                    .collect(),
            ),
            Self::Record(fields, tail) => {
                let mut fields: BTreeMap<_, _> = fields
                    .iter()
                    .map(|(n, t)| (n.clone(), t.map(replace)))
                    .collect();
                let mut tail = tail.as_ref().map(|t| Box::new(t.map(replace)));
                if let Some(Self::Record(rest, next)) = tail.as_deref() {
                    fields.extend(rest.clone());
                    tail = next.clone();
                }
                Self::Record(fields, tail)
            }
            Self::List(t) => Self::List(Box::new(t.map(replace))),
            Self::Description(t) => Self::Description(Box::new(t.map(replace))),
            Self::Named(t) => Self::Named(Box::new(t.map(replace))),
            _ => self.clone(),
        }
    }
    pub(crate) fn variables(&self) -> BTreeSet<u32> {
        let mut result = BTreeSet::new();
        self.map(&mut |ty| {
            if let Self::Variable(v) = ty {
                result.insert(*v);
            }
            None
        });
        result
    }
    /// Specialize named parameters while preserving the original checked type.
    pub fn substitute(&self, parameters: &BTreeMap<String, Type>) -> Type {
        self.map(&mut |ty| match ty {
            Self::Parameter(n) | Self::Index(n, _) => parameters.get(n).cloned(),
            _ => None,
        })
    }
    /// Apply a checked use-site inference substitution, flattening completed record rows.
    pub fn substitute_variables(&self, variables: &BTreeMap<u32, Type>) -> Type {
        self.map(&mut |ty| match ty {
            Self::Variable(variable) => variables.get(variable).cloned(),
            _ => None,
        })
    }
}
