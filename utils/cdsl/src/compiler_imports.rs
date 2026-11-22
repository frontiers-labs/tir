//! Import bindings and public exports refer to canonical declaration paths.
use std::collections::BTreeMap;

use super::{CompiledModule, Entry, NameError, anchored_path};
use crate::{ast::Declaration, error::Error, lexer::Span};

type Path = Vec<String>;
type Names = BTreeMap<String, Path>;
pub(super) type Externs = BTreeMap<Path, Names>;

pub(super) struct Imports {
    pub bindings: BTreeMap<Path, Names>,
    pub exports: BTreeMap<Path, Names>,
}

#[derive(Clone)]
struct Request {
    path: Path,
    glob: bool,
    alias: Option<String>,
    public: bool,
    location: String,
}

enum State {
    Loading,
    Ready { bindings: Names, exports: Names },
}

struct Resolver<'a> {
    entries: &'a BTreeMap<Path, Entry>,
    externs: &'a Externs,
    requests: BTreeMap<Path, Vec<Request>>,
    states: BTreeMap<Path, State>,
}

impl Resolver<'_> {
    fn local(&self, module: &[String], public: bool) -> Names {
        self.entries
            .iter()
            .filter(|(p, e)| {
                p.len() == module.len() + 1
                    && p.starts_with(module)
                    && (e.package == self.entries[module].package || e.package == "<embedded std>")
                    && (!public || e.public)
            })
            .map(|(p, _)| (p.last().unwrap().clone(), p.clone()))
            .collect()
    }

    fn module(&mut self, module: &[String]) -> Result<(), NameError> {
        match self.states.get(module) {
            Some(State::Ready { .. }) => return Ok(()),
            Some(State::Loading) => {
                return Err(Error(format!(
                    "cyclic module imports involving {}",
                    module.join("::")
                ))
                .into());
            }
            None => {}
        }
        self.states.insert(module.to_vec(), State::Loading);
        let mut bindings = self.local(module, false);
        let mut exports = self.local(module, true);
        let mut pending = self.requests.get(module).cloned().unwrap_or_default();
        while !pending.is_empty() {
            let count = pending.len();
            let mut unresolved = Vec::new();
            for request in pending {
                let result = (|| {
                    let Some(target) = self.target(module, &bindings, &request.path)? else {
                        unresolved.push(request.clone());
                        return Ok(());
                    };
                    let names = if request.glob {
                        if !self.entries.get(&target).is_some_and(|e| e.module) {
                            return Err(Error(format!(
                                "import {} is not a module",
                                request.path.join("::")
                            ))
                            .into());
                        }
                        self.module(&target)?;
                        self.exports(&target)
                    } else {
                        // A reference to a declaration depends on its owning module too.
                        let owner = if self.entries.get(&target).is_some_and(|e| e.module) {
                            target.clone()
                        } else {
                            self.owner(&target)
                        };
                        if owner != module {
                            self.module(&owner)?;
                        }
                        BTreeMap::from([(
                            request
                                .alias
                                .clone()
                                .unwrap_or_else(|| request.path.last().unwrap().clone()),
                            target,
                        )])
                    };
                    for (name, target) in names {
                        bind(&mut bindings, &name, &target)?;
                        if request.public {
                            bind(&mut exports, &name, &target)?;
                        }
                    }
                    Ok(())
                })();
                result.map_err(|mut failure: NameError| {
                    failure.error = Error(format!("{}: {}", request.location, failure.error));
                    failure
                })?;
            }
            if unresolved.len() == count {
                let request = &unresolved[0];
                let error = Error(format!(
                    "{}: unresolved or private import {}",
                    request.location,
                    request.path.join("::")
                ));
                let entry = &self.entries[module];
                return Err(NameError::unresolved(
                    error,
                    &module[..entry.root_depth],
                    unresolved.iter().map(|request| request.path[0].as_str()),
                ));
            }
            pending = unresolved;
        }
        self.states
            .insert(module.to_vec(), State::Ready { bindings, exports });
        Ok(())
    }

    fn owner(&self, path: &[String]) -> Path {
        (0..path.len())
            .rev()
            .find_map(|n| {
                self.requests
                    .contains_key(&path[..n])
                    .then(|| path[..n].to_vec())
            })
            .unwrap_or_default()
    }

    fn exports(&self, module: &[String]) -> Names {
        match &self.states[module] {
            State::Ready { exports, .. } => exports.clone(),
            State::Loading => unreachable!("exports are read only after module resolution"),
        }
    }

    fn target(
        &mut self,
        module: &[String],
        bindings: &Names,
        path: &[String],
    ) -> Result<Option<Path>, NameError> {
        let entry = &self.entries[module];
        let package = &entry.package;
        let root = &module[..entry.root_depth];
        let (candidates, tail) = if let Some((root, tail)) =
            anchored_path(path, module, entry.root_depth)
        {
            (vec![(root, true)], tail)
        } else {
            let mut candidates = Vec::new();
            if let Some(bound) = bindings.get(&path[0]) {
                candidates.push((bound.clone(), true));
            }
            for depth in (entry.root_depth..=module.len()).rev() {
                let mut first = module[..depth].to_vec();
                first.push(path[0].clone());
                candidates.push((first, false));
            }
            if package != "<embedded std>"
                && let Some(target) = self.externs.get(root).and_then(|names| names.get(&path[0]))
            {
                candidates.push((target.clone(), true));
            }
            (candidates, &path[1..])
        };
        for (mut current, imported) in candidates {
            let package = &self.entries[module].package;
            if !imported
                && !self.entries.get(&current).is_some_and(|e| {
                    e.public
                        || (e.package == *package
                            && module.starts_with(&current[..current.len() - 1]))
                })
            {
                continue;
            }
            let mut valid = true;
            for part in tail {
                let mut direct = current.clone();
                direct.push(part.clone());
                if let Some(entry) = self.entries.get(&direct)
                    && (entry.public || (entry.package == *package && module.starts_with(&current)))
                {
                    current = direct;
                    continue;
                }
                if self.requests.contains_key(&current) {
                    self.module(&current)?;
                    if let Some(export) = self.exports(&current).get(part) {
                        current = export.clone();
                        continue;
                    }
                }
                valid = false;
                break;
            }
            if valid {
                return Ok(Some(current));
            }
        }
        Ok(None)
    }
}

fn bind(names: &mut Names, name: &str, target: &[String]) -> Result<(), Error> {
    if name == "std" && target != ["std"] {
        return Err(Error(
            "std is reserved for the embedded standard library".into(),
        ));
    }
    if names.get(name).is_some_and(|old| old != target) {
        return Err(Error(format!("conflicting import name {name}")));
    }
    names.insert(name.into(), target.to_vec());
    Ok(())
}

pub(super) fn resolve(
    modules: &[CompiledModule],
    entries: &BTreeMap<Path, Entry>,
    externs: &Externs,
) -> Result<Imports, NameError> {
    let requests = modules
        .iter()
        .map(|m| {
            let requests = m
                .ast
                .declarations
                .iter()
                .filter_map(|d| {
                    if let Declaration::Use {
                        public,
                        path,
                        glob,
                        alias,
                    } = &d.value
                    {
                        let offset = match m.ast.token_spans.get(d.range.start) {
                            Some(Span::Source(r)) => r.start,
                            _ => 0,
                        };
                        Some(Request {
                            public: *public,
                            path: path.clone(),
                            glob: *glob,
                            alias: alias.clone(),
                            location: format!("{}:{offset}", m.file.display()),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            (m.path.clone(), requests)
        })
        .collect();
    let mut resolver = Resolver {
        entries,
        externs,
        requests,
        states: BTreeMap::new(),
    };
    for (path, entry) in entries {
        if entry.module {
            resolver.module(path)?;
        }
    }
    let mut result = Imports {
        bindings: BTreeMap::new(),
        exports: BTreeMap::new(),
    };
    for (module, state) in resolver.states {
        let State::Ready { bindings, exports } = state else {
            unreachable!()
        };
        result.exports.insert(module.clone(), exports);
        result.bindings.insert(module, bindings);
    }
    Ok(result)
}
