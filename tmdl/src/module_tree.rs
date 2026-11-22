use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::TMDLError;
use crate::lexer::{Token, lex};
use crate::{Span, Spanned};

pub(crate) const ROOT_FILE: &str = "mod.tmdl";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ModulePath(Vec<String>);

impl ModulePath {
    fn root() -> Self {
        Self(Vec::new())
    }

    fn child(&self, name: String) -> Self {
        let mut parts = self.0.clone();
        parts.push(name);
        Self(parts)
    }

    pub fn qualify(&self, name: &str) -> String {
        format!("{self}::{name}")
    }

    pub fn resolve(&self, reference: &str) -> Option<(Self, String)> {
        let (prefix, name) = reference.rsplit_once("::").unwrap_or(("", reference));
        let mut parts = prefix
            .split("::")
            .filter(|part| !part.is_empty())
            .peekable();
        let mut module = self.0.clone();
        match parts.peek().copied() {
            Some("crate") => {
                parts.next();
                module.clear();
            }
            Some("self") => {
                parts.next();
            }
            Some("super") => {
                while parts.next_if_eq(&"super").is_some() {
                    module.pop()?;
                }
            }
            _ => {}
        }
        module.extend(parts.map(str::to_string));
        Some((Self(module), name.to_string()))
    }

    pub fn page(&self) -> PathBuf {
        let mut path: PathBuf = self.0.iter().collect();
        if path.as_os_str().is_empty() || path == Path::new("index") {
            path.push("mod");
        }
        path.with_extension("md")
    }
}

impl std::fmt::Display for ModulePath {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("crate")?;
        for part in &self.0 {
            write!(out, "::{part}")?;
        }
        Ok(())
    }
}

pub(crate) enum ModuleSource {
    Directory(PathBuf),
    Embedded(BTreeMap<PathBuf, String>),
}

impl ModuleSource {
    fn exists(&self, file: &Path) -> bool {
        match self {
            Self::Directory(root) => root.join(file).is_file(),
            Self::Embedded(files) => files.contains_key(file),
        }
    }

    fn read(&self, file: &Path) -> Result<(String, PathBuf), TMDLError> {
        match self {
            Self::Directory(root) => {
                let path = root.join(file);
                let read = || Ok((fs::read_to_string(&path)?, path.canonicalize()?));
                read().map_err(|error: std::io::Error| {
                    TMDLError::IO(format!("{}: {error}", path.display()))
                })
            }
            Self::Embedded(files) => files
                .get(file)
                .map(|source| (source.clone(), file.into()))
                .ok_or_else(|| TMDLError::IO(format!("missing embedded {}", file.display()))),
        }
    }
}

pub(crate) struct Module {
    pub path: ModulePath,
    pub source_path: PathBuf,
    #[cfg(feature = "rust-cli")]
    pub source_key: PathBuf,
    pub source: String,
    pub imports: Vec<(String, Span)>,
    directives: Vec<Span>,
}

impl Module {
    pub fn tokens(&self) -> Vec<Spanned<Token<'_>>> {
        let mut directives = self.directives.iter().peekable();
        let (mut tokens, _) = lex(&self.source);
        tokens.retain(|(_, token)| {
            while directives
                .peek()
                .is_some_and(|span| span.end <= token.start)
            {
                directives.next();
            }
            !directives
                .peek()
                .is_some_and(|span| span.start <= token.start)
        });
        tokens
    }
}

pub(crate) struct ModuleTree {
    modules: BTreeMap<ModulePath, Module>,
    order: Vec<ModulePath>,
}

impl ModuleTree {
    pub fn load(source: &ModuleSource) -> Result<Self, TMDLError> {
        let mut tree = Self {
            modules: BTreeMap::new(),
            order: Vec::new(),
        };
        let mut source_keys = HashSet::new();
        let mut pending = vec![(ModulePath::root(), PathBuf::from(ROOT_FILE))];
        while let Some((path, source_path)) = pending.pop() {
            let (text, source_key) = source.read(&source_path)?;
            if !source_keys.insert(source_key.clone()) {
                return Err(TMDLError::Codegen(format!(
                    "{}: source already loaded as another module",
                    source_path.display()
                )));
            }
            let (tokens, errors) = lex(&text);
            if let Some(error) = errors.first() {
                return Err(located_error(
                    &source_path,
                    &text,
                    *error.span(),
                    "unexpected token",
                ));
            }
            let mut module = Module {
                path: path.clone(),
                source_path,
                #[cfg(feature = "rust-cli")]
                source_key,
                source: String::new(),
                imports: Vec::new(),
                directives: Vec::new(),
            };
            let children = module.header(&text, &tokens)?;
            let directory: PathBuf = path.0.iter().collect();
            for (name, span) in children.into_iter().rev() {
                let sibling = directory.join(format!("{name}.tmdl"));
                let child = directory.join(&name).join(ROOT_FILE);
                let file = match (source.exists(&sibling), source.exists(&child)) {
                    (true, false) => sibling,
                    (false, true) => child,
                    (true, true) => {
                        return Err(located_error(
                            &module.source_path,
                            &text,
                            span,
                            format!(
                                "ambiguous module `{name}`: {} and {}",
                                sibling.display(),
                                child.display()
                            ),
                        ));
                    }
                    (false, false) => {
                        return Err(located_error(
                            &module.source_path,
                            &text,
                            span,
                            format!(
                                "missing module `{name}`: expected {} or {}",
                                sibling.display(),
                                child.display()
                            ),
                        ));
                    }
                };
                pending.push((path.child(name), file));
            }
            module.source = text;
            tree.order.push(path.clone());
            tree.modules.insert(path, module);
        }
        Ok(tree)
    }

    pub fn modules(&self) -> impl Iterator<Item = &Module> {
        self.order.iter().map(|path| &self.modules[path])
    }
}

impl Module {
    fn header(
        &mut self,
        source: &str,
        tokens: &[Spanned<Token<'_>>],
    ) -> Result<Vec<(String, Span)>, TMDLError> {
        let mut children = Vec::new();
        let mut child_names = HashSet::new();
        let mut depth = 0usize;
        let mut item_start = true;
        let mut outer_doc = None;
        let mut pos = 0;
        while let Some((token, span)) = tokens.get(pos) {
            let macro_call = matches!(tokens.get(pos + 1), Some((Token::Bang, _)))
                || matches!(
                    tokens.get(pos + 1..pos + 3),
                    Some([(Token::Colon, _), (Token::Colon, _)])
                );
            if depth == 0
                && item_start
                && matches!(token, Token::Identifier("mod" | "use"))
                && !macro_call
            {
                let directive = *span;
                let is_mod = *token == Token::Identifier("mod");
                let error = |message| located_error(&self.source_path, source, directive, message);
                pos += 1;
                let Some((Token::Identifier(first), _)) = tokens.get(pos) else {
                    return Err(error("expected a name after `mod` or `use`"));
                };
                let mut parts = vec![*first];
                pos += 1;
                while !is_mod
                    && let [
                        (Token::Colon, _),
                        (Token::Colon, _),
                        (Token::Identifier(part), _),
                        ..,
                    ] = &tokens[pos..]
                {
                    parts.push(part);
                    pos += 3;
                }
                let Some((Token::Semicolon, end)) = tokens.get(pos) else {
                    return Err(error("expected `;` after module declaration or import"));
                };
                let span = (outer_doc.take().unwrap_or(directive.start)..end.end).into();
                if is_mod {
                    if matches!(*first, "mod" | "crate" | "self" | "super") {
                        return Err(error("reserved module name"));
                    }
                    if !child_names.insert(*first) {
                        return Err(error("duplicate module declaration"));
                    }
                    children.push((first.to_string(), span));
                } else {
                    self.imports.push((parts.join("::"), span));
                }
                self.directives.push(span);
            } else {
                match token {
                    Token::LBrace | Token::LBracket | Token::LParen => depth += 1,
                    Token::RBrace | Token::RBracket | Token::RParen => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            item_start = true;
                        }
                    }
                    Token::Semicolon if depth == 0 => item_start = true,
                    Token::OuterDocComment(..) if depth == 0 && item_start => {
                        outer_doc.get_or_insert(span.start);
                    }
                    Token::InnerDocComment(..) => {}
                    _ if depth == 0 => {
                        item_start = false;
                        outer_doc = None;
                    }
                    _ => {}
                }
            }
            pos += 1;
        }
        Ok(children)
    }
}

pub(crate) fn located_error(
    file: &Path,
    source: &str,
    span: Span,
    message: impl std::fmt::Display,
) -> TMDLError {
    let prefix = &source[..span.start.min(source.len())];
    let line = prefix.bytes().filter(|&c| c == b'\n').count() + 1;
    TMDLError::Codegen(format!("{}:{line}: {message}", file.display()))
}
