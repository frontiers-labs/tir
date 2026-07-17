use core::fmt;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::{fs, io};

use ariadne::{Color, Label, Report, ReportKind, sources};
use chumsky::error::{Cheap, Rich};
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, ValueEnum};

use crate::btor2gen::generate_btor2;
use crate::error::TMDLError;
use crate::expander::{Diag, MacroTable, StringArena, collect_macros, expand};
use crate::lexer::{Token, lex};
use crate::markdown::{generate_markdown, generate_markdown_book};
use crate::parser::parse;
use crate::rustgen::{generate_operation_list, generate_rust, generate_rust_modules};
use crate::sema_analyze;
use crate::smtlibgen::generate_smtlib;
use crate::{Span, Spanned};

pub struct Compiler {
    action: Action,
    inputs: Vec<String>,
    input_sources: Vec<Option<String>>,
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
        if matches!(self.action, Action::EmitAstJsonSchema) {
            if !self.inputs.is_empty() || !self.split_inputs.is_empty() {
                return Err(TMDLError::Codegen(
                    "emit-ast-json-schema does not accept inputs".to_string(),
                ));
            }
            let mut output = self.create_output_writer()?;
            serde_json::to_writer_pretty(&mut output, &crate::json::schema())?;
            writeln!(output)?;
            return Ok(());
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
        if self.custom_assembly && !self.text_only {
            return Err(TMDLError::Codegen(
                "custom assembly requires text-only Rust generation".to_string(),
            ));
        }
        match self.action {
            Action::EmitRust
            | Action::EmitOperationList
            | Action::EmitSmtlib
            | Action::EmitBtor2
            | Action::EmitMarkdown => self.compile_whole_program(),
            Action::EmitExpandedTokens => self.compile_expanded_tokens(),
            _ => self.compile_per_file(),
        }
    }

    /// Read all inputs, lex them, collect every `macro` definition into one
    /// shared table (cross-file visibility), then macro-expand each file.
    /// Sources are read up front so their `String`s outlive the borrowed tokens.
    /// Returns `None` after printing diagnostics.
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

    /// Full front end shared by AST-emitting and whole-program actions: read,
    /// lex, expand, parse, resolve inheritance, then run semantic + type
    /// analysis. Prints diagnostics and returns `Ok(None)` on any failure.
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

        crate::ast::resolve_register_class_inheritance(&mut parsed_files);
        crate::ast::resolve_abi_inheritance(&mut parsed_files);

        let sema_diags = sema_analyze(&parsed_files, self.text_only);
        if !sema_diags.is_empty() {
            print_diags(sema_diags, &self.inputs, &sources);
            return Ok(None);
        }

        let (_cache, tc_diags) = crate::type_check(&parsed_files);
        if !tc_diags.is_empty() {
            print_diags(tc_diags, &self.inputs, &sources);
            return Ok(None);
        }

        Ok(Some(parsed_files))
    }

    fn compile_expanded_tokens(&self) -> Result<(), TMDLError> {
        let mut output: Box<dyn Write> = self.create_output_writer()?;
        let sources = self.read_sources()?;
        let arena = StringArena::new();
        let Some(expanded) = self.lex_collect_expand(&sources, &arena) else {
            return Ok(());
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
                return Ok(());
            };

            match self.action {
                Action::EmitAst => {
                    for f in parsed_files {
                        writeln!(output, "{:#?}", f)?;
                    }
                }
                Action::EmitAstJson => {
                    let document = crate::json::Document::from_ast(&parsed_files);
                    serde_json::to_writer_pretty(&mut output, &document)?;
                    writeln!(output)?;
                }
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

    fn compile_whole_program(&self) -> Result<(), TMDLError> {
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

        let Some(parsed_files) = self.parse_and_check()? else {
            return Ok(());
        };

        let item_cache: HashMap<&str, _> = parsed_files
            .iter()
            .flat_map(|f| f.items.iter().map(|i| (i.name(), i)))
            .collect();

        match &self.action {
            Action::EmitRust => {
                if self.split_inputs.is_empty() {
                    let output: Box<dyn Write> = self.create_output_writer()?;
                    generate_rust(
                        self.dialect.as_ref().unwrap(),
                        &parsed_files,
                        &item_cache,
                        self.text_only,
                        self.custom_assembly,
                        output,
                    )?;
                } else {
                    let generated = generate_rust_modules(
                        self.dialect.as_ref().unwrap(),
                        &parsed_files,
                        &item_cache,
                        self.text_only,
                        self.custom_assembly,
                        &self.split_inputs,
                    )?;
                    let mut output = self.create_output_writer()?;
                    output.write_all(generated.root.as_bytes())?;
                    output.flush()?;
                    let output_dir = match &self.output {
                        OutputKind::File(path) => PathBuf::from(path)
                            .parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .unwrap_or_else(|| std::path::Path::new("."))
                            .to_path_buf(),
                        OutputKind::Batch(path) => PathBuf::from(path),
                        OutputKind::Stdout => {
                            return Err(TMDLError::Codegen(
                                "split Rust output cannot be written to stdout".to_string(),
                            ));
                        }
                    };
                    fs::create_dir_all(&output_dir)?;
                    for (file_name, contents) in generated.modules {
                        fs::write(output_dir.join(file_name), contents)?;
                    }
                }
            }
            Action::EmitOperationList => {
                let output: Box<dyn Write> = self.create_output_writer()?;
                generate_operation_list(&parsed_files, output)?;
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
                    &parsed_files,
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
                    &parsed_files,
                    &item_cache,
                    writer,
                )?;
            }
            Action::EmitMarkdown => match &self.output {
                OutputKind::Batch(path) => generate_markdown_book(
                    self.dialect.as_ref().unwrap(),
                    &parsed_files,
                    std::path::Path::new(path),
                )?,
                _ => {
                    let output: Box<dyn Write> = self.create_output_writer()?;
                    generate_markdown(self.dialect.as_ref().unwrap(), &parsed_files, output)?;
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

pub fn compiler_main(args: Option<&ArgMatches>) -> Result<(), Box<dyn std::error::Error>> {
    let args = match args {
        Some(args) => Cli::from_arg_matches(args),
        None => Ok(Cli::parse()),
    }?;

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

    compiler.compile().map_err(|err| Box::new(err).into())
}

fn print_errors<'src, T>(file_name: &str, source: &'src str, errors: Vec<Rich<'src, T, Span>>)
where
    T: fmt::Display,
{
    errors.into_iter().for_each(|e| {
        Report::build(
            ReportKind::Error,
            (file_name.to_string(), e.span().into_range()),
        )
        .with_config(ariadne::Config::new().with_index_type(ariadne::IndexType::Byte))
        .with_message(e.to_string())
        .with_label(
            Label::new((file_name.to_string(), e.span().into_range()))
                .with_message(e.reason().to_string())
                .with_color(Color::Red),
        )
        .with_labels(e.contexts().map(|(label, span)| {
            Label::new((file_name.to_string(), span.into_range()))
                .with_message(format!("while parsing this {}", label))
                .with_color(Color::Yellow)
        }))
        .finish()
        .print(sources([(file_name.to_string(), source.to_string())]))
        .unwrap()
    })
}

/// Print grouped-by-file diagnostics using the already-in-memory sources
/// (`inputs[i]` names the file whose text is `sources[i]`). Diagnostics may
/// reference any input file; unknown files are skipped.
fn print_diags(diags: Vec<Diag>, inputs: &[String], sources: &[String]) {
    use std::collections::BTreeMap;
    let mut by_file: BTreeMap<String, Vec<Rich<'static, String, Span>>> = BTreeMap::new();
    for (fname, d) in diags {
        by_file.entry(fname).or_default().push(d);
    }
    for (fname, errors) in by_file {
        if let Some(source) = inputs.iter().position(|i| *i == fname).map(|i| &sources[i]) {
            print_errors(&fname, source, errors);
        }
    }
}

fn print_cheap_errors(file_name: &str, source: &str, errors: Vec<Cheap<Span>>) {
    errors.into_iter().for_each(|e| {
        Report::build(
            ReportKind::Error,
            (file_name.to_string(), e.span().into_range()),
        )
        .with_config(ariadne::Config::new().with_index_type(ariadne::IndexType::Byte))
        .with_message("Unexpected token")
        .with_label(
            Label::new((file_name.to_string(), e.span().into_range()))
                .with_message("Unexpected token")
                .with_color(Color::Red),
        )
        .finish()
        .print(sources([(file_name.to_string(), source.to_string())]))
        .unwrap()
    })
}
