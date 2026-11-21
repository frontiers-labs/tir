use std::{any::TypeId, cmp::Ordering};

use super::{BaseValue, Database, Error, Signature};

/// A declaration imported from Rust, identified by its owning Rust type.
pub trait NativeDeclaration: 'static {
    const NAME: &'static str;
    const SYMBOL: &'static str = "";
    const PORTS: Option<(usize, usize)> = None;
    fn label() -> &'static str {
        Self::NAME
    }
}

/// An opaque reference to an imported declaration. Its label is diagnostic.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NativeIdentity {
    owner: TypeId,
    label: &'static str,
    ports: Option<(usize, usize)>,
}

impl Ord for NativeIdentity {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.label, self.owner, self.ports).cmp(&(other.label, other.owner, other.ports))
    }
}

impl PartialOrd for NativeIdentity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl BaseValue for NativeIdentity {}

impl NativeIdentity {
    pub fn of<T: NativeDeclaration>() -> Self {
        Self {
            owner: TypeId::of::<T>(),
            label: T::label(),
            ports: T::PORTS,
        }
    }

    pub fn as_str(self) -> &'static str {
        self.label
    }

    pub fn ports(self) -> Option<(usize, usize)> {
        self.ports
    }
}

impl Database {
    /// Bind a checked program import to its native declaration.
    pub fn bind_declaration<T: NativeDeclaration>(&mut self) -> Result<(), Error> {
        let symbol = T::SYMBOL;
        if symbol.is_empty() {
            return Err(Error::Unsupported(
                "native declaration has no import symbol".into(),
            ));
        }
        let output = self.sort("NativeIdentity")?;
        let declaration = NativeIdentity::of::<T>();
        if let Some(previous) = self.declaration(symbol) {
            return if previous == declaration {
                Ok(())
            } else {
                Err(Error::Unsupported(format!(
                    "conflicting owner for native declaration {symbol}"
                )))
            };
        }
        let id = self.add_primitive(
            symbol,
            Signature {
                inputs: vec![],
                output,
            },
            move |p, _| Some(p.base(declaration)),
        );
        self.prims[id.index()].declaration = Some(declaration);
        Ok(())
    }

    /// Resolve a serialized import at the program load boundary.
    pub fn declaration(&self, symbol: &str) -> Option<NativeIdentity> {
        self.prims.iter().find_map(|p| {
            if p.name == symbol {
                p.declaration
            } else {
                None
            }
        })
    }

    /// Admit a declaration for a Rust witness that owns a checked contract.
    pub fn admit_declaration<W: 'static>(&mut self, declaration: NativeIdentity) {
        self.base
            .admissions
            .insert((TypeId::of::<W>(), declaration));
    }

    pub fn admits_declaration<W: 'static>(&self, declaration: NativeIdentity) -> bool {
        self.base
            .admissions
            .contains(&(TypeId::of::<W>(), declaration))
    }
}
