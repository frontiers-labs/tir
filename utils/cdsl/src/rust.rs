use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fmt::Write,
};

use crate::{
    CheckedProgram, CompiledModule, Error, Program,
    ast::{Declaration, Expression, Node, OpField, Path},
    lexer::Span,
};

mod native;
mod operation;
mod value;

struct SelectedExports<'a> {
    entries: &'a BTreeMap<String, Path>,
}

impl CheckedProgram {
    /// Check standalone CDSL source and emit the Rust for its root exports.
    #[doc(hidden)]
    pub fn emit_rust_source(source: &str, _package: &str) -> Result<String, Error> {
        crate::compile_source(source)?.render_rust_module(&[])
    }
}

impl Program {
    pub(crate) fn render_rust_module(&self, module: &[String]) -> Result<String, Error> {
        let entries = self
            .exported_module(module)
            .and_then(|path| self.module_exports.get(path))
            .ok_or_else(|| Error(format!("unknown module {}", module.join("::"))))?;
        self.render_rust_exports(SelectedExports { entries })
    }

    fn render_rust_exports(&self, selection: SelectedExports<'_>) -> Result<String, Error> {
        let mut aliases: BTreeMap<&Path, (&str, Vec<&str>)> = BTreeMap::new();
        for (export, target) in selection.entries {
            if self.module_exports.contains_key(target) {
                continue;
            }
            match aliases.entry(target) {
                Entry::Vacant(entry) => {
                    entry.insert((export, Vec::new()));
                }
                Entry::Occupied(mut entry) => entry.get_mut().1.push(export),
            }
        }
        let mut output = String::new();
        let mut values = BTreeSet::new();
        for (target, (name, aliases)) in aliases {
            let (source, declaration) = self.find_rust_declaration(target).ok_or_else(|| {
                Error(format!(
                    "Rust export {name} has no declaration at {}",
                    target.join("::")
                ))
            })?;
            if native::emit(&mut output, &declaration.value)
                .map_err(|error| export_error(source, declaration, target, error))?
            {
                continue;
            }
            if let Declaration::Let { binding, .. } = &declaration.value
                && let Expression::Op(fields) = &binding.value.value
            {
                let module = target
                    .strip_suffix(&binding.path[..])
                    .ok_or_else(|| Error("operation has no owning module".into()))?;
                for field in fields {
                    if let OpField::Interfaces { entries, .. } = &field.value {
                        for (_, path, _) in entries {
                            if operation::rust_trait(self, path)?.is_none() {
                                values.insert(path.clone());
                            }
                        }
                    }
                }
                operation::emit(self, name, module, binding, fields, &mut output)
                    .map_err(|error| export_error(source, declaration, target, error))?;
                for alias in aliases {
                    writeln!(output, "pub use self::r#{name} as r#{alias};")
                        .map_err(|error| Error(error.to_string()))?;
                }
            } else {
                values.insert(target.clone());
                let canonical = target
                    .last()
                    .ok_or_else(|| Error("empty Rust export path".into()))?;
                for export in std::iter::once(name).chain(aliases) {
                    writeln!(
                        output,
                        "pub use self::__cdsl_values::r#{canonical} as r#{export};"
                    )
                    .map_err(|error| Error(error.to_string()))?;
                }
            }
        }
        if !values.is_empty() {
            output.push_str("mod __cdsl_values {\n");
            self.render_rust_values(&mut output, values)?;
            output.push_str("}\n");
        }
        Ok(output)
    }

    fn render_rust_values(
        &self,
        output: &mut String,
        mut pending: BTreeSet<Path>,
    ) -> Result<(), Error> {
        let mut emitted = BTreeMap::new();
        while let Some(target) = pending.pop_first() {
            let name = target
                .last()
                .ok_or_else(|| Error("empty Rust export path".into()))?;
            if let Some(previous) = emitted.get(name) {
                if previous != &target {
                    return Err(Error(format!(
                        "ambiguous Rust name {name} in filter output"
                    )));
                }
                continue;
            }
            let (source, declaration) = self
                .find_rust_declaration(&target)
                .ok_or_else(|| Error(format!("missing Rust declaration {}", target.join("::"))))?;
            let dependencies = value::emit(self, output, name, &target, &declaration.value)
                .map_err(|error| export_error(source, declaration, &target, error))?;
            pending.extend(dependencies);
            emitted.insert(name.clone(), target);
        }
        Ok(())
    }

    fn find_rust_export(&self, target: &[String]) -> Option<&Declaration> {
        self.find_rust_declaration(target)
            .map(|(_, declaration)| &declaration.value)
    }

    fn find_rust_declaration(
        &self,
        target: &[String],
    ) -> Option<(&CompiledModule, &Node<Declaration>)> {
        self.all_modules().find_map(|module| {
            let local = target.strip_prefix(module.path.as_slice())?;
            module.ast.declarations.iter().find_map(|node| {
                let name = match &node.value {
                    Declaration::Let { binding, .. } | Declaration::Interface { binding, .. } => {
                        binding.path.as_slice()
                    }
                    Declaration::Axiom { name, .. }
                    | Declaration::Enum { name, .. }
                    | Declaration::Mod { name, .. } => std::slice::from_ref(name),
                    Declaration::Use { .. } => return None,
                };
                (local == name).then_some((module, node))
            })
        })
    }
}

fn export_error(
    source: &CompiledModule,
    declaration: &Node<Declaration>,
    target: &[String],
    error: Error,
) -> Error {
    let offset = match source.ast.token_spans.get(declaration.range.start) {
        Some(Span::Source(range)) => range.start,
        _ => 0,
    };
    Error(format!(
        "{}:{offset}: Rust export {}: {error}",
        source.file.display(),
        target.join("::")
    ))
}

// Compile the carrier scheme once, alongside its operation descriptor.
fn emit_scheme(text: &str) -> Result<String, Error> {
    let mut emitter = SchemeEmitter {
        remaining: text,
        binders: BTreeMap::new(),
    };
    let mut kinds = Vec::new();
    if emitter.eat("forall ") {
        loop {
            let name = emitter.word(false)?.to_owned();
            let kind = if emitter.eat(":") {
                let rest = emitter.remaining;
                match emitter.word(false)? {
                    "nat" => "tir::schema::BinderKind::Nat".into(),
                    "sym" => "tir::schema::BinderKind::Sym".into(),
                    "data" => "tir::schema::BinderKind::Data".into(),
                    _ => {
                        emitter.remaining = rest;
                        let mut shapes = vec![emitter.pattern()?];
                        while emitter.eat("|") {
                            shapes.push(emitter.pattern()?);
                        }
                        format!("tir::schema::BinderKind::Shapes(&[{}])", shapes.join(", "))
                    }
                }
            } else {
                "tir::schema::BinderKind::Data".into()
            };
            let index = kinds.len();
            kinds.push(kind);
            if emitter.binders.insert(name, index).is_some() {
                return Err(Error("duplicate operation signature binder".into()));
            }
            if !emitter.eat(",") {
                break;
            }
        }
        emitter.expect(".")?;
    }
    let inputs = emitter.ports()?;
    emitter.expect("->")?;
    let outputs = emitter.ports()?;
    if !emitter.remaining.trim().is_empty() {
        return Err(Error(format!(
            "unexpected signature suffix {:?}",
            emitter.remaining
        )));
    }
    Ok(format!(
        "&tir::schema::Signature {{ binders: &[{}], inputs: &[{inputs}], outputs: &[{outputs}] }}",
        kinds.join(", ")
    ))
}

struct SchemeEmitter<'a> {
    remaining: &'a str,
    binders: BTreeMap<String, usize>,
}

impl<'a> SchemeEmitter<'a> {
    fn eat(&mut self, token: &str) -> bool {
        self.remaining = self.remaining.trim_start();
        match self.remaining.strip_prefix(token) {
            Some(rest) => {
                self.remaining = rest;
                true
            }
            None => false,
        }
    }

    fn expect(&mut self, token: &str) -> Result<(), Error> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(Error(format!(
                "expected {token:?} in signature at {:?}",
                self.remaining
            )))
        }
    }

    fn word(&mut self, qualified: bool) -> Result<&'a str, Error> {
        self.remaining = self.remaining.trim_start();
        let len = self
            .remaining
            .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && !(qualified && ch == '.'))
            .unwrap_or(self.remaining.len());
        if len == 0 {
            return Err(Error(format!(
                "expected type in signature at {:?}",
                self.remaining
            )));
        }
        let len = self.remaining[..len].trim_end_matches('.').len();
        let (word, rest) = self.remaining.split_at(len);
        self.remaining = rest;
        Ok(word)
    }

    fn ports(&mut self) -> Result<String, Error> {
        self.expect("(")?;
        let mut ports = Vec::new();
        if !self.eat(")") {
            loop {
                let pattern = self.pattern()?;
                let mode = if self.eat("*") {
                    "Many"
                } else if self.eat("?") {
                    "Optional"
                } else {
                    "Fixed"
                };
                ports.push(format!("tir::schema::Port::{mode}({pattern})"));
                if self.eat(")") {
                    break;
                }
                self.expect(",")?;
            }
        }
        Ok(ports.join(", "))
    }

    fn pattern(&mut self) -> Result<String, Error> {
        let name = self.word(true)?;
        if name == "_" {
            return Ok("tir::schema::Pattern::Wildcard".into());
        }
        if let Ok(number) = name.parse::<u64>() {
            return Ok(format!("tir::schema::Pattern::Nat({number})"));
        }
        if let Some(index) = self.binders.get(name) {
            return Ok(format!("tir::schema::Pattern::Bound({index})"));
        }
        let qualified = match name {
            "int" => "builtin.i".to_owned(),
            "float" => "builtin.f".to_owned(),
            name if name.contains('.') => name.to_owned(),
            _ => format!("builtin.{name}"),
        };
        let mut args = Vec::new();
        if self.eat("<") {
            loop {
                args.push(self.pattern()?);
                if self.eat(">") {
                    break;
                }
                self.expect(",")?;
            }
        }
        let args = args.join(", ");
        if qualified == "nat.add" {
            Ok(format!("tir::schema::Pattern::NatSum(&[{args}])"))
        } else {
            let (dialect, name) = qualified
                .split_once('.')
                .ok_or_else(|| Error("unqualified type constructor".into()))?;
            Ok(format!(
                "tir::schema::Pattern::Constructor({dialect:?}, {name:?}, &[{args}])"
            ))
        }
    }
}
