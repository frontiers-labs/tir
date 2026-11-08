use crate::ast::{Import, Item, ItemKind, Path, Span};
use crate::compiler::SourceMap;
use crate::diagnostic::{Diagnostic, Phase};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub(crate) struct Entry {
    pub module: String,
    pub source: String,
    pub public: bool,
    pub span: Span,
    pub kind: ItemKind,
}
#[derive(Clone)]
struct Use {
    module: String,
    source: String,
    span: Span,
    public: bool,
    path: Path,
    import: Import,
}
pub(crate) struct Symbols {
    pub entries: BTreeMap<String, Entry>,
    modules: BTreeMap<String, bool>,
    imports: BTreeMap<(String, String), (String, bool)>,
    uses: Vec<Use>,
    capabilities: BTreeSet<String>,
    constructors: BTreeMap<String, String>,
}
pub(crate) fn qualify(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.into()
    } else {
        format!("{module}::{name}")
    }
}
impl Symbols {
    pub fn load(sources: &SourceMap) -> Result<Self, Vec<Diagnostic>> {
        let mut symbols = Self {
            entries: BTreeMap::new(),
            modules: BTreeMap::from([(String::new(), true)]),
            imports: BTreeMap::new(),
            uses: Vec::new(),
            capabilities: sources.namespace_capabilities.clone(),
            constructors: BTreeMap::new(),
        };
        let items = crate::parser::parse(&sources.root_name, &sources.root)?;
        let mut active = BTreeSet::new();
        symbols.collect("", &sources.root_name, items, sources, &mut active)?;
        symbols.resolve_imports()?;
        symbols.validate_namespaces()?;
        Ok(symbols)
    }
    fn collect(
        &mut self,
        module: &str,
        source: &str,
        items: Vec<Item>,
        sources: &SourceMap,
        active: &mut BTreeSet<String>,
    ) -> Result<(), Vec<Diagnostic>> {
        if !active.insert(module.into()) {
            return Err(vec![Diagnostic::new(
                source,
                0..0,
                Phase::Resolve,
                "cyclic source module",
            )]);
        }
        for item in items {
            let declaration = item.node;
            match declaration.kind {
                ItemKind::Module(name, body) => {
                    let target = qualify(module, &name);
                    if target == "relational"
                        && (body.is_some()
                            || sources.modules.get(&target).map(String::as_str)
                                != Some(crate::relations::SOURCE))
                    {
                        return Err(vec![Diagnostic::new(
                            source,
                            item.span,
                            Phase::Resolve,
                            "trusted relational module requires the canonical standard-library source",
                        )]);
                    }
                    if self.entries.contains_key(&target)
                        || self
                            .modules
                            .insert(target.clone(), declaration.public)
                            .is_some()
                    {
                        return Err(vec![Diagnostic::new(
                            source,
                            item.span,
                            Phase::Resolve,
                            format!("duplicate module `{target}`"),
                        )]);
                    }
                    match body {
                        Some(items) => self.collect(&target, source, items, sources, active)?,
                        None => {
                            let text = sources.modules.get(&target).ok_or_else(|| {
                                vec![Diagnostic::new(
                                    source,
                                    item.span.clone(),
                                    Phase::Resolve,
                                    format!("module `{target}` missing from source map"),
                                )]
                            })?;
                            let items = crate::parser::parse(&target, text)?;
                            self.collect(&target, &target, items, sources, active)?;
                        }
                    }
                }
                ItemKind::Use(path, import) => self.uses.push(Use {
                    module: module.into(),
                    source: source.into(),
                    span: item.span,
                    public: declaration.public,
                    path,
                    import,
                }),
                kind => {
                    let name = match &kind {
                        ItemKind::Type(n, _, _)
                        | ItemKind::Enum(n, _, _)
                        | ItemKind::ExternalType(n, _, _) => qualify(module, n),
                        ItemKind::Let { path, .. } | ItemKind::ExternalValue(path, _, _) => {
                            qualify(module, &path.text())
                        }
                        ItemKind::Use(..) | ItemKind::Module(..) => continue,
                    };
                    if name.rsplit("::").next().is_some_and(reserved_name) {
                        return Err(vec![Diagnostic::new(
                            source,
                            item.span.clone(),
                            Phase::Resolve,
                            format!("reserved core name `{name}` cannot be redeclared"),
                        )]);
                    }
                    if let ItemKind::Enum(_, _, variants) = &kind {
                        for variant in variants {
                            self.constructors
                                .insert(qualify(&name, &variant.name), name.clone());
                        }
                    }
                    let entry = Entry {
                        module: module.into(),
                        source: source.into(),
                        public: declaration.public,
                        span: item.span.clone(),
                        kind,
                    };
                    if self.modules.contains_key(&name)
                        || self.entries.insert(name.clone(), entry).is_some()
                    {
                        return Err(vec![Diagnostic::new(
                            source,
                            item.span,
                            Phase::Resolve,
                            format!("duplicate definition `{name}`"),
                        )]);
                    }
                }
            }
        }
        active.remove(module);
        Ok(())
    }
    fn resolve_imports(&mut self) -> Result<(), Vec<Diagnostic>> {
        loop {
            let before = self.imports.clone();
            let mut unresolved = Vec::new();
            for import in self.uses.clone() {
                match self.install_import(&import) {
                    Ok(true) => {}
                    Ok(false) => unresolved.push(import),
                    Err(error) => return Err(vec![error]),
                }
            }
            let renamed = self.canonicalize_namespaces()?;
            if before == self.imports && !renamed {
                if unresolved.is_empty() {
                    return Ok(());
                }
                return Err(unresolved
                    .iter()
                    .map(|i| {
                        Diagnostic::new(
                            &i.source,
                            i.span.clone(),
                            Phase::Resolve,
                            format!("unresolved or private import `{}`", i.path.text()),
                        )
                    })
                    .collect());
            }
        }
    }
    fn canonicalize_namespaces(&mut self) -> Result<bool, Vec<Diagnostic>> {
        let mut renames = BTreeMap::new();
        for (old, entry) in &self.entries {
            let path = match &entry.kind {
                ItemKind::Let { path, .. } | ItemKind::ExternalValue(path, _, _) => path,
                _ => continue,
            };
            if path.absolute || path.segments.len() < 2 {
                continue;
            }
            let namespace = Path {
                absolute: false,
                segments: path.segments[..path.segments.len() - 1].to_vec(),
            };
            if let Some(namespace) = self.resolve(&entry.module, &namespace) {
                let new = qualify(&namespace, &path.segments[path.segments.len() - 1]);
                if *old != new {
                    renames.insert(old.clone(), new);
                }
            }
        }
        for (old, new) in &renames {
            let Some(entry) = self.entries.remove(old) else {
                continue;
            };
            if self.entries.contains_key(new) {
                return Err(vec![Diagnostic::new(
                    &entry.source,
                    entry.span,
                    Phase::Resolve,
                    format!("duplicate qualified definition `{new}`"),
                )]);
            }
            self.entries.insert(new.clone(), entry);
        }
        for (target, _) in self.imports.values_mut() {
            if let Some(new) = renames.get(target) {
                *target = new.clone();
            }
        }
        Ok(!renames.is_empty())
    }
    fn install_import(&mut self, import: &Use) -> Result<bool, Diagnostic> {
        let Some(target) = self.resolve(&import.module, &import.path) else {
            return Ok(false);
        };
        let selections = match &import.import {
            Import::Name(alias) => vec![(
                alias
                    .clone()
                    .unwrap_or_else(|| import.path.segments.last().cloned().unwrap_or_default()),
                target,
            )],
            Import::Group(group) => {
                let mut result = Vec::new();
                for (name, alias) in group {
                    let child = qualify(&target, name);
                    let Some(child) = self.canonical_target(&import.module, &child) else {
                        return Ok(false);
                    };
                    result.push((alias.clone().unwrap_or_else(|| name.clone()), child));
                }
                result
            }
            Import::Glob => {
                let prefix = format!("{target}::");
                let names = self
                    .entries
                    .keys()
                    .chain(self.modules.keys())
                    .chain(self.constructors.keys())
                    .cloned()
                    .chain(
                        self.imports
                            .keys()
                            .map(|(module, alias)| qualify(module, alias)),
                    );
                names
                    .filter_map(|name| {
                        name.strip_prefix(&prefix)
                            .filter(|n| !n.contains("::"))
                            .map(|n| (n.to_string(), name.clone()))
                    })
                    .filter_map(|(alias, name)| {
                        self.canonical_target(&import.module, &name)
                            .map(|name| (alias, name))
                    })
                    .collect()
            }
        };
        for (alias, target) in selections {
            let key = (import.module.clone(), alias.clone());
            if reserved_name(&alias)
                || self.entries.contains_key(&qualify(&import.module, &alias))
                || self.modules.contains_key(&qualify(&import.module, &alias))
            {
                return Err(Diagnostic::new(
                    &import.source,
                    import.span.clone(),
                    Phase::Resolve,
                    format!("import `{alias}` conflicts with a declaration"),
                ));
            }
            if let Some((old, _)) = self.imports.get(&key)
                && old != &target
            {
                return Err(Diagnostic::new(
                    &import.source,
                    import.span.clone(),
                    Phase::Resolve,
                    format!("ambiguous import `{alias}`"),
                ));
            }
            let public = import.public || self.imports.get(&key).is_some_and(|(_, public)| *public);
            self.imports.insert(key, (target, public));
        }
        Ok(true)
    }
    fn validate_namespaces(&self) -> Result<(), Vec<Diagnostic>> {
        for entry in self.entries.values() {
            let path = match &entry.kind {
                ItemKind::Let { path, .. } | ItemKind::ExternalValue(path, _, _) => path,
                _ => continue,
            };
            if path.absolute {
                return Err(vec![Diagnostic::new(
                    &entry.source,
                    entry.span.clone(),
                    Phase::Resolve,
                    "a definition must use a relative owned namespace",
                )]);
            }
            if path.segments.len() > 1 {
                let namespace = Path {
                    absolute: false,
                    segments: path.segments[..path.segments.len() - 1].to_vec(),
                };
                let owned = self
                    .resolve(&entry.module, &namespace)
                    .and_then(|n| self.entries.get(&n).map(|e| (n, e)))
                    .is_some_and(|(n, e)| {
                        matches!(e.kind, ItemKind::ExternalType(..))
                            && (e.module == entry.module || self.capabilities.contains(&n))
                    });
                if !owned {
                    return Err(vec![Diagnostic::new(
                        &entry.source,
                        entry.span.clone(),
                        Phase::Resolve,
                        format!(
                            "qualified definition needs an owned nominal namespace `{}`",
                            namespace.text()
                        ),
                    )]);
                }
            }
        }
        Ok(())
    }
    pub fn resolve(&self, module: &str, path: &Path) -> Option<String> {
        let text = path.segments.join("::");
        if path.absolute {
            return self.canonical_target(module, &text);
        }
        if let Some(first) = path.segments.first()
            && let Some((target, _)) = self.imports.get(&(module.into(), first.clone()))
        {
            let rest = path.segments[1..].join("::");
            let candidate = if rest.is_empty() {
                target.clone()
            } else {
                qualify(target, &rest)
            };
            if let Some(target) = self.canonical_target(module, &candidate) {
                return Some(target);
            }
        }
        self.canonical_target(module, &qualify(module, &text))
            .or_else(|| self.canonical_target(module, &text))
    }
    fn canonical_target(&self, module: &str, name: &str) -> Option<String> {
        let mut name = name.to_string();
        let mut seen = BTreeSet::new();
        loop {
            let (owner, alias) = name.rsplit_once("::").unwrap_or(("", name.as_str()));
            let Some((target, public)) = self.imports.get(&(owner.into(), alias.into())) else {
                break;
            };
            if !seen.insert(name.clone())
                || (owner != module && !public)
                || !self.module_visible(module, owner)
            {
                return None;
            }
            name = target.clone();
        }
        self.visible(module, &name).then_some(name)
    }
    fn visible(&self, module: &str, name: &str) -> bool {
        if let Some(parent) = self.constructors.get(name) {
            return self.visible(module, parent);
        }
        if let Some(e) = self.entries.get(name) {
            return self.module_visible(module, &e.module) && (e.module == module || e.public);
        }
        if let Some(public) = self.modules.get(name) {
            return *public || name.rsplit_once("::").map_or("", |(p, _)| p) == module;
        }
        if let Some((owner, alias)) = name.rsplit_once("::")
            && let Some((target, public)) = self.imports.get(&(owner.into(), alias.into()))
        {
            return (owner == module || *public)
                && self.module_visible(module, owner)
                && self.visible(module, target);
        }
        false
    }
    fn module_visible(&self, from: &str, target: &str) -> bool {
        let mut prefix = String::new();
        for segment in target.split("::").filter(|s| !s.is_empty()) {
            let parent = prefix.clone();
            prefix = qualify(&prefix, segment);
            if self.modules.get(&prefix) == Some(&false)
                && from != parent
                && from != prefix
                && !from.starts_with(&format!("{prefix}::"))
            {
                return false;
            }
        }
        true
    }
    pub fn entry(&self, module: &str, path: &Path) -> Option<(String, Entry)> {
        let name = self.resolve(module, path)?;
        let entry = self.entries.get(&name)?.clone();
        Some((name, entry))
    }
}

pub(crate) fn reserved_name(name: &str) -> bool {
    matches!(
        name,
        "Bool"
            | "Integer"
            | "Nat"
            | "String"
            | "Unit"
            | "Type"
            | "List"
            | "Record"
            | "Option"
            | "TypeRep"
            | "Named"
            | "Some"
            | "None"
    ) || name
        .strip_prefix("Type_")
        .is_some_and(|n| n.parse::<u32>().is_ok())
}
