//! Immutable ownership of a checked compiler revision.
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{Error, Program, ast::Path, hm::Scheme};

static NEXT_REVISION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct CheckedProgram {
    revision: u64,
    artifact: Program,
    types: BTreeMap<Path, Scheme>,
}

impl CheckedProgram {
    pub(crate) fn new(artifact: Program, types: BTreeMap<Path, Scheme>) -> Result<Self, Error> {
        let revision = NEXT_REVISION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error("checked revision identities exhausted".into()))?;
        Ok(Self {
            revision,
            artifact,
            types,
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

    /// Diagnostic Rust representation, not an engine IR serialization format.
    pub fn render(&self) -> Result<String, Error> {
        Ok(format!("{self:#?}"))
    }
}
