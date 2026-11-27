use core::fmt;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{fs, io};

use ariadne::{Color, Label, Report, ReportKind, sources};
use chumsky::error::{Cheap, Rich};
use clap::{CommandFactory, Parser, ValueEnum};

use crate::btor2gen::generate_btor2;
use crate::error::TMDLError;
use crate::expander::{Diag, MacroTable, ScopedMacros, StringArena, collect_macros, expand};
use crate::lexer::{Token, lex};
use crate::markdown::{generate_markdown, generate_markdown_book};
use crate::module_names::{self, Names};
use crate::module_tree::{ModuleSource, ModuleTree};
use crate::parser::parse;
use crate::rustgen::{SEM_BLOB_FILE, generate_operation_list, generate_rust_modules};
use crate::sema_analyze;
use crate::smtlibgen::generate_smtlib;
use crate::{Span, Spanned};

pub(crate) type RustWriter<'a> =
    dyn Fn(&str, &str) -> Result<String, Box<dyn std::error::Error>> + 'a;

pub struct Compiler {
    action: Action,
    inputs: Vec<String>,
    input_sources: Vec<Option<String>>,
    module_source: Option<ModuleSource>,
    output: OutputKind,
    dialect: Option<String>,
    isa: Option<String>,
    text_only: bool,
    split_inputs: Vec<String>,
    custom_assembly: bool,
    btor2_isas: Option<Vec<String>>,
}

pub struct CompilerBuilder {
    action: Option<Action>,
    inputs: Vec<String>,
    input_sources: Vec<Option<String>>,
    module_source: Option<ModuleSource>,
    output: Option<OutputKind>,
    dialect: Option<String>,
    isa: Option<String>,
    text_only: bool,
    split_inputs: Vec<String>,
    custom_assembly: bool,
    btor2_isas: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub enum OutputKind {
    File(String),
    Batch(String),
    Stdout,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Action {
    EmitTokens,
    EmitExpandedTokens,
    EmitAst,
    EmitAstJson,
    EmitAstJsonSchema,
    EmitRust,
    EmitOperationList,
    EmitSmtlib,
    EmitBtor2,
    EmitMarkdown,
}

#[derive(Debug, Parser)]
pub struct Cli {
    #[arg(value_enum, long)]
    pub action: Action,
    pub inputs: Vec<String>,
    #[arg(short, long)]
    pub output: String,
    #[arg(short, long)]
    pub dialect: Option<String>,
    /// Target ISA name (e.g. RV64I) for ISA-parameterized outputs.
    #[arg(long)]
    pub isa: Option<String>,
    /// Allow objectless (text-only) targets: instructions need no `encoding`
    /// block. For pseudo-ISAs like PTX that have an assembly syntax but no binary
    /// representation.
    #[arg(long)]
    pub text_only: bool,
    /// Emit instruction code from this input into a sibling Rust module.
    #[arg(long)]
    pub split_input: Vec<String>,
    /// The target supplies its own assembly parser and printer.
    #[arg(long, requires = "text_only")]
    pub custom_assembly: bool,
}

impl Compiler {
    pub fn builder() -> CompilerBuilder {
        CompilerBuilder {
            action: None,
            inputs: vec![],
            input_sources: vec![],
            module_source: None,
            output: None,
            dialect: None,
            isa: None,
            text_only: false,
            split_inputs: vec![],
            custom_assembly: false,
            btor2_isas: None,
        }
    }

    pub fn compile(&self) -> Result<(), TMDLError> {
        self.compile_with_writer(None)
    }

    /// Compile with the shared CDSL writer available for Rust emission.
    pub fn compile_with_rust_writer(
        &self,
        writer: impl Fn(&str, &str) -> Result<String, Box<dyn std::error::Error>>,
    ) -> Result<(), TMDLError> {
        self.compile_with_writer(Some(&writer))
    }

    fn compile_with_writer(&self, writer: Option<&RustWriter<'_>>) -> Result<(), TMDLError> {
        if self.custom_assembly && !self.text_only {
            return Err(TMDLError::Codegen(
                "custom assembly requires text-only Rust generation".to_string(),
            ));
        }
        if let Some(source) = &self.module_source {
            if !self.inputs.is_empty() || !self.split_inputs.is_empty() {
                return Err(TMDLError::Codegen(
                    "module sources cannot be combined with flat or split inputs".into(),
                ));
            }
            let names = match self.action {
                Action::EmitBtor2
                | Action::EmitSmtlib
                | Action::EmitRust
                | Action::EmitOperationList => Names::Backend,
                Action::EmitMarkdown => Names::Documentation,
                _ => return Err(TMDLError::Codegen("this action expects flat inputs".into())),
            };
            self.validate_emit_options();
            let tree = ModuleTree::load(source)?;
            let files = check_modules(&tree, names, self.text_only)?;
            return self.emit_program(writer, &files, Some(&tree));
        }
        if matches!(self.action, Action::EmitAstJsonSchema) {
            if !self.inputs.is_empty() || !self.split_inputs.is_empty() {
                return Err(TMDLError::Codegen(
                    "emit-ast-json-schema does not accept inputs".to_string(),
                ));
            }
            let mut output = self.create_output_writer()?;
            return write_ast_json_schema(&mut output);
        }
        if !self.split_inputs.is_empty() && !matches!(self.action, Action::EmitRust) {
            return Err(TMDLError::Codegen(
                "split inputs are only supported by emit-rust".to_string(),
            ));
        }
        if !self.split_inputs.is_empty() && matches!(self.output, OutputKind::Stdout) {
            return Err(TMDLError::Codegen(
                "split Rust output cannot be written to stdout".to_string(),
            ));
        }
        match self.action {
            Action::EmitRust
            | Action::EmitOperationList
            | Action::EmitSmtlib
            | Action::EmitBtor2
            | Action::EmitMarkdown => self.compile_whole_program(writer),
            Action::EmitExpandedTokens => self.compile_expanded_tokens(),
            _ => self.compile_per_file(),
        }
    }

    fn lex_collect_expand<'s>(
        &self,
        sources: &'s [String],
        arena: &'s StringArena,
    ) -> Option<Vec<Vec<Spanned<Token<'s>>>>> {
        let mut lexed = Vec::with_capacity(sources.len());
        for (input, source) in self.inputs.iter().zip(sources) {
            let (tokens, errors) = lex(source);
            if !errors.is_empty() {
                print_cheap_errors(input, source, errors);
                return None;
            }
            lexed.push(tokens);
        }

        let mut table = MacroTable::new();
        let mut diags: Vec<Diag> = vec![];
        let stripped: Vec<_> = self
            .inputs
            .iter()
            .zip(lexed)
            .map(|(input, tokens)| collect_macros(input, tokens, &mut table, &mut diags))
            .collect();
        if !diags.is_empty() {
            print_diags(diags, &self.inputs, sources);
            return None;
        }

        let mut expanded = Vec::with_capacity(stripped.len());
        for (input, tokens) in self.inputs.iter().zip(stripped) {
            let (toks, diags) = expand(input, tokens, &table, arena);
            if !diags.is_empty() {
                print_diags(diags, &self.inputs, sources);
                return None;
            }
            expanded.push(toks);
        }
        Some(expanded)
    }

    fn parse_and_check(&self) -> Result<Option<Vec<crate::ast::File>>, TMDLError> {
        let sources = self.read_sources()?;
        let arena = StringArena::new();
        let Some(expanded) = self.lex_collect_expand(&sources, &arena) else {
            return Ok(None);
        };

        let mut parsed_files = Vec::new();
        for ((input, source), tokens) in self.inputs.iter().zip(&sources).zip(&expanded) {
            let (file, errors) = parse(source, tokens, input);
            if !errors.is_empty() {
                print_errors(input, source, errors);
                return Ok(None);
            }
            parsed_files.push(file.unwrap());
        }

        if check_program(&mut parsed_files, &self.inputs, &sources, self.text_only) {
            Ok(Some(parsed_files))
        } else {
            Ok(None)
        }
    }

    fn compile_expanded_tokens(&self) -> Result<(), TMDLError> {
        let mut output: Box<dyn Write> = self.create_output_writer()?;
        let sources = self.read_sources()?;
        let arena = StringArena::new();
        let Some(expanded) = self.lex_collect_expand(&sources, &arena) else {
            return Err(TMDLError::Codegen("compilation failed".to_string()));
        };
        for tokens in expanded {
            writeln!(output, "{:#?}", tokens)?;
        }
        Ok(())
    }

    fn read_sources(&self) -> Result<Vec<String>, TMDLError> {
        let mut sources = Vec::with_capacity(self.inputs.len());
        for (input, source) in self.inputs.iter().zip(&self.input_sources) {
            sources.push(match source {
                Some(source) => source.clone(),
                None => std::fs::read_to_string(input)?,
            });
        }
        Ok(sources)
    }

    fn compile_per_file(&self) -> Result<(), TMDLError> {
        let mut output: Box<dyn Write> = self.create_output_writer()?;

        // EmitAst still needs whole-program type checking when multiple files are given.
        if matches!(self.action, Action::EmitAst | Action::EmitAstJson) {
            let Some(parsed_files) = self.parse_and_check()? else {
                return Err(TMDLError::Codegen("compilation failed".to_string()));
            };

            match self.action {
                Action::EmitAst => {
                    for f in parsed_files {
                        writeln!(output, "{:#?}", f)?;
                    }
                }
                Action::EmitAstJson => write_ast_json(&parsed_files, &mut output)?,
                _ => unreachable!(),
            }
            return Ok(());
        }

        for (input, source) in self.inputs.iter().zip(&self.input_sources) {
            let source = match source {
                Some(source) => source.clone(),
                None => std::fs::read_to_string(input)?,
            };

            match &self.action {
                Action::EmitTokens => {
                    let (tokens, _errors) = lex(&source);
                    writeln!(output, "{:#?}", tokens)?;
                }
                _ => unreachable!("Non-simple actions should use compile_with_semantic_analysis"),
            }
        }
        Ok(())
    }

    fn validate_emit_options(&self) {
        if matches!(self.action, Action::EmitRust) && self.dialect.is_none() {
            let mut cmd = Cli::command();
            cmd.error(
                clap::error::ErrorKind::ArgumentConflict,
                "--dialect must be specified with --action=emit-rust",
            )
            .exit();
        }
        if matches!(self.action, Action::EmitSmtlib | Action::EmitBtor2) && self.isa.is_none() {
            let mut cmd = Cli::command();
            cmd.error(
                clap::error::ErrorKind::ArgumentConflict,
                "--isa must be specified with --action=emit-smtlib or --action=emit-btor2",
            )
            .exit();
        }
        if matches!(self.action, Action::EmitMarkdown) && self.dialect.is_none() {
            let mut cmd = Cli::command();
            cmd.error(
                clap::error::ErrorKind::ArgumentConflict,
                "--dialect must be specified with --action=emit-markdown",
            )
            .exit();
        }
    }

    fn compile_whole_program(&self, writer: Option<&RustWriter<'_>>) -> Result<(), TMDLError> {
        self.validate_emit_options();
        let parsed_files = self.parse_and_check()?.ok_or_else(compilation_failed)?;
        self.emit_program(writer, &parsed_files, None)
    }

    pub(crate) fn emit_program(
        &self,
        writer: Option<&RustWriter<'_>>,
        parsed_files: &[crate::ast::File],
        tree: Option<&ModuleTree>,
    ) -> Result<(), TMDLError> {
        let item_cache: HashMap<&str, _> = parsed_files
            .iter()
            .flat_map(|f| f.items.iter().map(|i| (i.name(), i)))
            .collect();

        match &self.action {
            Action::EmitRust => {
                let writer = writer.ok_or_else(|| {
                    TMDLError::Codegen("Rust emission requires the CDSL writer".into())
                })?;
                let generated = generate_rust_modules(
                    self.dialect.as_ref().unwrap(),
                    parsed_files,
                    &item_cache,
                    self.text_only,
                    self.custom_assembly,
                    &self.split_inputs,
                    writer,
                )?;
                let mut output = self.create_output_writer()?;
                output.write_all(generated.root.as_bytes())?;
                output.flush()?;
                // Stdout output is for inspection only: the generated Rust
                // embeds the sem blob by relative path, so there is nowhere to
                // put it.
                let output_dir = match &self.output {
                    OutputKind::File(path) => Some(
                        PathBuf::from(path)
                            .parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .unwrap_or_else(|| std::path::Path::new("."))
                            .to_path_buf(),
                    ),
                    OutputKind::Batch(path) => Some(PathBuf::from(path)),
                    OutputKind::Stdout => None,
                };
                if let Some(output_dir) = output_dir {
                    fs::create_dir_all(&output_dir)?;
                    fs::write(output_dir.join(SEM_BLOB_FILE), &generated.sem_blob)?;
                    for (file_name, contents) in generated.modules.into_iter().chain(generated.cdsl)
                    {
                        fs::write(output_dir.join(file_name), contents)?;
                    }
                }
            }
            Action::EmitOperationList => {
                let output: Box<dyn Write> = self.create_output_writer()?;
                generate_operation_list(parsed_files, output)?;
            }
            Action::EmitSmtlib => {
                let metadata_path = match &self.output {
                    OutputKind::File(path) => {
                        Some(PathBuf::from(path).with_extension("metadata.json"))
                    }
                    _ => None,
                };
                let writer: Box<dyn Write> = self.create_output_writer()?;
                let metadata = generate_smtlib(
                    self.dialect.as_ref().unwrap(),
                    self.isa.as_ref().unwrap(),
                    parsed_files,
                    &item_cache,
                    writer,
                )?;
                if let Some(path) = metadata_path {
                    fs::write(path, serde_json::to_vec_pretty(&metadata)?)?;
                }
            }
            Action::EmitBtor2 => {
                let writer: Box<dyn Write> = self.create_output_writer()?;
                generate_btor2(
                    self.isa.as_ref().unwrap(),
                    self.btor2_isas.as_deref(),
                    parsed_files,
                    &item_cache,
                    writer,
                )?;
            }
            Action::EmitMarkdown => match &self.output {
                OutputKind::Batch(path) => generate_markdown_book(
                    self.dialect.as_ref().unwrap(),
                    parsed_files,
                    Path::new(path),
                    tree,
                )?,
                _ => {
                    let output: Box<dyn Write> = self.create_output_writer()?;
                    generate_markdown(self.dialect.as_ref().unwrap(), parsed_files, output)?;
                }
            },
            _ => unreachable!("Only complex actions should use this path"),
        }

        Ok(())
    }

    fn create_output_writer(&self) -> Result<Box<dyn Write>, TMDLError> {
        let output: Box<dyn Write> = match &self.output {
            OutputKind::Stdout => Box::new(io::BufWriter::new(io::stdout())),
            OutputKind::File(path) => {
                let file = fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(path)?;
                Box::new(io::BufWriter::new(file))
            }
            OutputKind::Batch(out_dir) => {
                let mut path = PathBuf::from(out_dir);
                // Generate a default output filename for single file output
                path.push("output.rs");

                fs::create_dir_all(path.parent().as_ref().unwrap())?;

                let file = fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .read(true)
                    .open(&path)?;
                Box::new(io::BufWriter::new(file))
            }
        };
        Ok(output)
    }
}

impl CompilerBuilder {
    pub fn action(self, action: Action) -> Self {
        Self {
            action: Some(action),
            ..self
        }
    }

    pub fn add_input(self, path: &str) -> Self {
        let mut inputs = self.inputs;
        let mut input_sources = self.input_sources;
        inputs.push(path.to_string());
        input_sources.push(None);

        Self {
            inputs,
            input_sources,
            ..self
        }
    }

    /// Add a named source without requiring a file at compile time.
    pub fn add_source(self, name: &str, source: &str) -> Self {
        let mut inputs = self.inputs;
        let mut input_sources = self.input_sources;
        inputs.push(name.to_string());
        input_sources.push(Some(source.to_string()));

        Self {
            inputs,
            input_sources,
            ..self
        }
    }

    /// Load a package of TMDL modules rooted at `mod.tmdl` in this directory.
    pub fn module_root(self, directory: impl AsRef<Path>) -> Self {
        Self {
            module_source: Some(ModuleSource::Directory(directory.as_ref().into())),
            ..self
        }
    }

    /// Load an embedded package with names relative to `mod.tmdl`.
    pub fn module_sources(self, sources: &[(&str, &str)]) -> Self {
        Self {
            module_source: Some(ModuleSource::Embedded(
                sources
                    .iter()
                    .map(|(name, source)| (PathBuf::from(name), (*source).into()))
                    .collect(),
            )),
            ..self
        }
    }

    pub fn output(self, output: OutputKind) -> Self {
        Self {
            output: Some(output),
            ..self
        }
    }

    pub fn dialect(self, dialect: Option<String>) -> Self {
        Self { dialect, ..self }
    }

    pub fn isa(self, isa: Option<String>) -> Self {
        Self { isa, ..self }
    }

    pub fn text_only(self, text_only: bool) -> Self {
        Self { text_only, ..self }
    }

    pub fn split_input(self, path: &str) -> Self {
        let mut split_inputs = self.split_inputs;
        split_inputs.push(path.to_string());
        Self {
            split_inputs,
            ..self
        }
    }

    pub fn custom_assembly(self, custom_assembly: bool) -> Self {
        Self {
            custom_assembly,
            ..self
        }
    }

    /// Restrict BTOR2 emission to the selected ISA and extension names.
    pub fn btor2_isas(self, btor2_isas: Vec<String>) -> Self {
        Self {
            btor2_isas: Some(btor2_isas),
            ..self
        }
    }

    pub fn build(self) -> Compiler {
        Compiler {
            action: self.action.unwrap(),
            inputs: self.inputs,
            input_sources: self.input_sources,
            module_source: self.module_source,
            output: self.output.unwrap(),
            dialect: self.dialect,
            isa: self.isa,
            text_only: self.text_only,
            split_inputs: self.split_inputs,
            custom_assembly: self.custom_assembly,
            btor2_isas: self.btor2_isas,
        }
    }
}

#[cfg(feature = "rust-cli")]
pub(crate) fn compile_command(
    args: Cli,
    writer: impl Fn(&str, &str) -> Result<String, Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let output = match args.output.as_str() {
        "-" => OutputKind::Stdout,
        _ => OutputKind::File(args.output.clone()),
    };
    let mut compiler_builder = Compiler::builder()
        .action(args.action)
        .dialect(args.dialect.clone())
        .isa(args.isa.clone())
        .text_only(args.text_only)
        .custom_assembly(args.custom_assembly)
        .output(output);

    for input in &args.inputs {
        compiler_builder = compiler_builder.add_input(input);
    }
    for input in &args.split_input {
        compiler_builder = compiler_builder.split_input(input);
    }

    let compiler = compiler_builder.build();

    compiler
        .compile_with_rust_writer(writer)
        .map_err(|err| Box::new(err).into())
}

fn compilation_failed() -> TMDLError {
    TMDLError::Codegen("compilation failed".into())
}

pub(crate) fn check_modules(
    tree: &ModuleTree,
    names: Names,
    text_only: bool,
) -> Result<Vec<crate::ast::File>, TMDLError> {
    let inputs: Vec<_> = tree
        .modules()
        .map(|module| module.source_path.to_string_lossy().into_owned())
        .collect();
    let sources: Vec<_> = tree
        .modules()
        .map(|module| module.source.as_str())
        .collect();
    let arena = StringArena::new();
    let mut tables = Vec::new();
    let mut stripped = Vec::new();
    let mut diagnostics = Vec::new();
    for (input, module) in inputs.iter().zip(tree.modules()) {
        let mut table = MacroTable::new();
        stripped.push(collect_macros(
            input,
            module.tokens(),
            &mut table,
            &mut diagnostics,
        ));
        tables.push(table);
    }
    if !diagnostics.is_empty() {
        print_diags(diagnostics, &inputs, &sources);
        return Err(compilation_failed());
    }

    let macros = ScopedMacros::new(tree, tables);
    let mut files = Vec::new();
    for (index, ((input, source), tokens)) in inputs.iter().zip(&sources).zip(stripped).enumerate()
    {
        let (tokens, diagnostics) = macros.expand(index, input, tokens, &arena);
        if !diagnostics.is_empty() {
            print_diags(diagnostics, &inputs, &sources);
            return Err(compilation_failed());
        }
        let (file, errors) = parse(source, &tokens, input);
        if !errors.is_empty() {
            print_errors(input, source, errors);
            return Err(compilation_failed());
        }
        files.push(file.ok_or_else(compilation_failed)?);
    }

    let output_names = match module_names::resolve(tree, &mut files, macros.names(), names) {
        Ok(names) => names,
        Err(diagnostics) => {
            print_diags(diagnostics, &inputs, &sources);
            return Err(compilation_failed());
        }
    };
    if !check_program(&mut files, &inputs, &sources, text_only) {
        return Err(compilation_failed());
    }
    output_names.lower(&mut files);
    Ok(files)
}

fn check_program(
    files: &mut [crate::ast::File],
    inputs: &[String],
    sources: &[impl AsRef<str>],
    text_only: bool,
) -> bool {
    crate::ast::resolve_register_class_inheritance(files);
    crate::ast::resolve_abi_inheritance(files);

    let diagnostics = crate::fninline::inline_functions(files);
    if !diagnostics.is_empty() {
        print_diags(diagnostics, inputs, sources);
        return false;
    }

    let diagnostics = crate::widths::resolve_width_calls(files);
    if !diagnostics.is_empty() {
        print_diags(diagnostics, inputs, sources);
        return false;
    }

    let diagnostics = sema_analyze(files, text_only);
    if !diagnostics.is_empty() {
        print_diags(diagnostics, inputs, sources);
        return false;
    }

    let (_, diagnostics) = crate::type_check(files);
    if !diagnostics.is_empty() {
        print_diags(diagnostics, inputs, sources);
        return false;
    }
    true
}

fn print_errors<'src, T>(file_name: &str, source: &'src str, errors: Vec<Rich<'src, T, Span>>)
where
    T: fmt::Display,
{
    for e in errors {
        let contexts = e
            .contexts()
            .map(|(label, span)| (format!("while parsing this {label}"), span.into_range()))
            .collect();
        print_report(
            file_name,
            source,
            e.span().into_range(),
            e.to_string(),
            e.reason().to_string(),
            contexts,
        );
    }
}

fn print_report(
    file_name: &str,
    source: &str,
    span: std::ops::Range<usize>,
    message: String,
    label: String,
    contexts: Vec<(String, std::ops::Range<usize>)>,
) {
    Report::build(ReportKind::Error, (file_name.to_string(), span.clone()))
        .with_config(ariadne::Config::new().with_index_type(ariadne::IndexType::Byte))
        .with_message(message)
        .with_label(
            Label::new((file_name.to_string(), span))
                .with_message(label)
                .with_color(Color::Red),
        )
        .with_labels(contexts.into_iter().map(|(message, span)| {
            Label::new((file_name.to_string(), span))
                .with_message(message)
                .with_color(Color::Yellow)
        }))
        .finish()
        .print(sources([(file_name.to_string(), source.to_string())]))
        .unwrap()
}

fn print_diags(diags: Vec<Diag>, inputs: &[String], sources: &[impl AsRef<str>]) {
    use std::collections::BTreeMap;
    let mut by_file: BTreeMap<String, Vec<Rich<'static, String, Span>>> = BTreeMap::new();
    for (fname, d) in diags {
        by_file.entry(fname).or_default().push(d);
    }
    for (fname, errors) in by_file {
        if let Some(source) = inputs.iter().position(|i| *i == fname).map(|i| &sources[i]) {
            print_errors(&fname, source.as_ref(), errors);
        }
    }
}

fn print_cheap_errors(file_name: &str, source: &str, errors: Vec<Cheap<Span>>) {
    for e in errors {
        let message = "Unexpected token".to_string();
        print_report(
            file_name,
            source,
            e.span().into_range(),
            message.clone(),
            message,
            vec![],
        );
    }
}

#[cfg(feature = "json")]
fn write_ast_json_schema(output: &mut dyn Write) -> Result<(), TMDLError> {
    serde_json::to_writer_pretty(&mut *output, &crate::json::schema())?;
    writeln!(output)?;
    Ok(())
}

#[cfg(feature = "json")]
fn write_ast_json(files: &[crate::ast::File], output: &mut dyn Write) -> Result<(), TMDLError> {
    let document = crate::json::Document::from_ast(files);
    serde_json::to_writer_pretty(&mut *output, &document)?;
    writeln!(output)?;
    Ok(())
}

#[cfg(not(feature = "json"))]
fn write_ast_json_schema(_output: &mut dyn Write) -> Result<(), TMDLError> {
    Err(json_disabled())
}

#[cfg(not(feature = "json"))]
fn write_ast_json(_files: &[crate::ast::File], _output: &mut dyn Write) -> Result<(), TMDLError> {
    Err(json_disabled())
}

#[cfg(not(feature = "json"))]
fn json_disabled() -> TMDLError {
    TMDLError::Codegen("tmdl was built without the `json` feature".to_string())
}

/// Generate a backend from its TMDL `inputs` in a build script: the Rust ISA
/// model (`<dialect>.rs`), its operation list (`<dialect>_ops.rs`) and the
/// `MODEL_CHECK_SOURCES` table (`model_check_sources.rs`), all in `OUT_DIR`.
///
/// The table names every input by file and inlines it with `include_str!`.
/// A backend `include!`s it for hardware model checking, so the sources it
/// embeds cannot drift from the ones it was generated from.
#[cfg(feature = "rust-cli")]
pub fn build_backend(dialect: &str, inputs: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let writer = |source: &str, package: &str| {
        Ok(tir_cdsl::CheckedProgram::emit_rust_source(source, package)?)
    };
    let out_dir = std::env::var("OUT_DIR")?;
    for input in inputs {
        println!("cargo:rerun-if-changed={input}");
    }
    let compile = |action, output| {
        let mut builder = Compiler::builder()
            .output(OutputKind::File(format!("{out_dir}/{dialect}{output}")))
            .dialect(Some(dialect.to_string()))
            .action(action);
        for input in inputs {
            builder = builder.add_input(input);
        }
        builder.build().compile_with_rust_writer(writer)
    };

    let mut table = String::from("const MODEL_CHECK_SOURCES: &[(&str, &str)] = &[\n");
    for input in inputs {
        let path = std::fs::canonicalize(input)?;
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let path = path.to_string_lossy().into_owned();
        table.push_str(&format!("    ({name:?}, include_str!({path:?})),\n"));
    }
    table.push_str("];\n");
    std::fs::write(format!("{out_dir}/model_check_sources.rs"), table)?;

    compile(Action::EmitRust, ".rs")?;
    compile(Action::EmitOperationList, "_ops.rs")?;
    Ok(())
}
