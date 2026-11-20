//! Immutable ownership of a checked compiler revision.
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    Error, Program,
    ast::{Expr, Path},
    hm::{Scheme, Ty},
};

static NEXT_REVISION: AtomicU64 = AtomicU64::new(0);

/// Resolved declaration identity of an interface, independent of its import aliases.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InterfaceId(u64, Path);

impl InterfaceId {
    /// Canonical source path of the interface declaration.
    pub fn path(&self) -> &[String] {
        &self.1
    }
}

/// Resolved declaration identity of an operation schema.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OperationKindId(u64, Path);

impl OperationKindId {
    /// Canonical source path of the operation declaration.
    pub fn path(&self) -> &[String] {
        &self.1
    }
}

/// An implementation checked in its operation's attribute scope.
#[derive(Debug)]
pub struct CheckedMethods {
    expression: Expr,
    contract: Ty,
    arguments: Vec<Ty>,
}

impl CheckedMethods {
    pub(crate) fn new(expression: Expr, contract: Ty, arguments: Vec<Ty>) -> Self {
        Self {
            expression,
            contract,
            arguments,
        }
    }

    pub(crate) fn map_types(&mut self, transform: impl Fn(&Ty) -> Ty) {
        self.contract = transform(&self.contract);
        for ty in &mut self.arguments {
            *ty = transform(ty);
        }
    }

    /// The checked method record expression.
    pub fn expression(&self) -> &Expr {
        &self.expression
    }

    /// The specialized method record type in the checked revision.
    pub fn contract(&self) -> &Ty {
        &self.contract
    }

    /// Inferred arguments, in the interface declaration's parameter order.
    pub fn arguments(&self) -> &[Ty] {
        &self.arguments
    }
}

/// The interface implementations of one operation in a checked revision.
#[derive(Debug)]
pub struct CheckedOpInterfaces {
    operation: OperationKindId,
    interfaces: BTreeMap<InterfaceId, CheckedMethods>,
}

impl CheckedOpInterfaces {
    /// The operation that declares these implementations.
    pub fn operation(&self) -> &OperationKindId {
        &self.operation
    }

    /// Find an implementation by its resolved interface identity.
    pub fn get(&self, interface: &InterfaceId) -> Option<&CheckedMethods> {
        self.interfaces.get(interface)
    }

    /// Iterate over implementations in canonical interface path order.
    pub fn implementations(&self) -> impl Iterator<Item = (&InterfaceId, &CheckedMethods)> {
        self.interfaces.iter()
    }
}

#[derive(Debug)]
pub struct CheckedProgram {
    revision: u64,
    artifact: Program,
    types: BTreeMap<Path, Scheme>,
    operation_interfaces: Vec<CheckedOpInterfaces>,
}

impl CheckedProgram {
    /// Compile a standalone executable against its declared schema.
    pub fn to_binary(&self) -> Result<Vec<u8>, Error> {
        crate::Schema::new(self)?.compile_bytes(self)
    }
    pub(crate) fn new(
        artifact: Program,
        types: BTreeMap<Path, Scheme>,
        mut operations: BTreeMap<Path, BTreeMap<Path, CheckedMethods>>,
    ) -> Result<Self, Error> {
        let revision = NEXT_REVISION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error("checked revision identities exhausted".into()))?;
        let mut operation_interfaces = Vec::new();
        for module in &artifact.modules {
            for declaration in &module.ast.declarations {
                let crate::ast::Declaration::Let { binding, .. } = &declaration.value else {
                    continue;
                };
                let mut path = module.path.clone();
                path.extend(binding.path.iter().cloned());
                if let Some(interfaces) = operations.remove(&path) {
                    operation_interfaces.push(CheckedOpInterfaces {
                        operation: OperationKindId(revision, path),
                        interfaces: interfaces
                            .into_iter()
                            .map(|(path, methods)| (InterfaceId(revision, path), methods))
                            .collect(),
                    });
                }
            }
        }
        Ok(Self {
            revision,
            artifact,
            types,
            operation_interfaces,
        })
    }

    /// Identity of this immutable compilation within the current process.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Resolved source owned by this checked revision.
    pub fn artifact(&self) -> &Program {
        &self.artifact
    }

    /// Inferred schemes after applying the final substitution.
    pub fn types(&self) -> &BTreeMap<Path, Scheme> {
        &self.types
    }

    /// Checked interface implementations of operations in the selected modules.
    pub fn operation_interfaces(&self) -> &[CheckedOpInterfaces] {
        &self.operation_interfaces
    }

    /// Diagnostic Rust representation, not an engine IR serialization format.
    pub fn render(&self) -> Result<String, Error> {
        Ok(format!("{self:#?}"))
    }
}
