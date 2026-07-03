use core::fmt;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::{fs, io};

use ariadne::{Color, Label, Report, ReportKind, sources};
use chumsky::error::{Cheap, Rich};
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, ValueEnum};

use crate::error::TMDLError;
use crate::expander::{Diag, MacroTable, StringArena, collect_macros, expand};
use crate::lexer::{Token, lex};
use crate::parser::parse;
use crate::rustgen::generate_rust;
use crate::sema_analyze;
use crate::smtlibgen::generate_smtlib;
use crate::{Span, Spanned};

pub struct Compiler {
    action: Action,
    inputs: Vec<String>,
    output: OutputKind,
    dialect: Option<String>,
    isa: Option<String>,
    text_only: bool,
}

pub struct CompilerBuilder {
    action: Option<Action>,
    inputs: Vec<String>,
    output: Option<OutputKind>,
    dialect: Option<String>,
    isa: Option<String>,
    text_only: bool,
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
    EmitRust,
    EmitSmtlib,
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
}

impl Compiler {
    pub fn builder() -> CompilerBuilder {
        CompilerBuilder {
            action: None,
            inputs: vec![],
            output: None,
            dialect: None,
            isa: None,
            text_only: false,
        }
    }

    pub fn compile(&self) -> Result<(), TMDLError> {
        match self.action {
            Action::EmitRust | Action::EmitSmtlib => self.compile_whole_program(),
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
        for input in &self.inputs {
            sources.push(std::fs::read_to_string(input)?);
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
                    serde_json::to_writer_pretty(&mut output, &parsed_files)?;
                    writeln!(output)?;
                }
                _ => unreachable!(),
            }
            return Ok(());
        }

        for input in &self.inputs {
            let source = std::fs::read_to_string(input)?;

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
        if matches!(self.action, Action::EmitSmtlib) && self.isa.is_none() {
            let mut cmd = Cli::command();
            cmd.error(
                clap::error::ErrorKind::ArgumentConflict,
                "--isa must be specified with --action=emit-smtlib",
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
                let output: Box<dyn Write> = self.create_output_writer()?;
                generate_rust(
                    self.dialect.as_ref().unwrap(),
                    &parsed_files,
                    &item_cache,
                    self.text_only,
                    output,
                )?
            }
            Action::EmitSmtlib => {
                let writer: Box<dyn Write> = self.create_output_writer()?;
                generate_smtlib(
                    self.dialect.as_ref().unwrap(),
                    self.isa.as_ref().unwrap(),
                    &parsed_files,
                    &item_cache,
                    writer,
                )?;
            }
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
        inputs.push(path.to_string());

        Self { inputs, ..self }
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

    pub fn build(self) -> Compiler {
        Compiler {
            action: self.action.unwrap(),
            inputs: self.inputs,
            output: self.output.unwrap(),
            dialect: self.dialect,
            isa: self.isa,
            text_only: self.text_only,
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
        .output(output);

    for input in &args.inputs {
        compiler_builder = compiler_builder.add_input(input);
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
