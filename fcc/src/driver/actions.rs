use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::lang_options::LangOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopPhase {
    Preprocess,
    Tokens,
    Ast,
    Ir,
    Assembly,
    Object,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputFile {
    CSource(OsString),
    // Constructed by gcc-mode positional parsing; build_actions routes it into the link fan-in.
    Object(PathBuf),
}

#[derive(Debug)]
pub struct DriverOptions {
    pub inputs: Vec<InputFile>,
    pub output: Option<PathBuf>,
    pub stop: StopPhase,
    pub lang_options: LangOptions,
    pub defines: Vec<String>,
    pub undefines: Vec<String>,
    pub include_dirs: Vec<PathBuf>,
    pub march: Option<String>,
    pub mcpu: Option<String>,
    pub mabi: Option<String>,
    pub lib_dirs: Vec<PathBuf>,
    pub libs: Vec<String>,
    pub pipeline: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Stdout,
    File(PathBuf),
    Temp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkInput {
    Object(PathBuf),
    CompileOutput(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Compile {
        input: InputFile,
        stop: StopPhase,
        output: Output,
    },
    Link {
        inputs: Vec<LinkInput>,
        output: PathBuf,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum DriverError {
    MultipleFilesWithOutput,
    NoInputFiles,
    LinkFailed(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::MultipleFilesWithOutput => {
                write!(f, "cannot specify -o with -c, -S or -E with multiple files")
            }
            DriverError::NoInputFiles => write!(f, "no input files"),
            DriverError::LinkFailed(message) => write!(f, "{message}"),
        }
    }
}

pub fn build_actions(opts: &DriverOptions) -> Result<Vec<Action>, DriverError> {
    let named_file = opts.output.as_deref().filter(|p| *p != Path::new("-"));
    if opts.stop != StopPhase::Link && named_file.is_some() && opts.inputs.len() > 1 {
        return Err(DriverError::MultipleFilesWithOutput);
    }

    let mut actions = Vec::new();
    let mut link_inputs = Vec::new();
    for input in &opts.inputs {
        match input {
            InputFile::Object(path) => link_inputs.push(LinkInput::Object(path.clone())),
            InputFile::CSource(_) => {
                let stop = if opts.stop == StopPhase::Link {
                    StopPhase::Object
                } else {
                    opts.stop
                };
                let output = compile_output(opts, input, stop);
                link_inputs.push(LinkInput::CompileOutput(actions.len()));
                actions.push(Action::Compile {
                    input: input.clone(),
                    stop,
                    output,
                });
            }
        }
    }

    if opts.stop == StopPhase::Link {
        if link_inputs.is_empty() {
            return Err(DriverError::NoInputFiles);
        }
        let output = opts
            .output
            .clone()
            .unwrap_or_else(|| PathBuf::from("a.out"));
        actions.push(Action::Link {
            inputs: link_inputs,
            output,
        });
    }

    Ok(actions)
}

fn compile_output(opts: &DriverOptions, input: &InputFile, stop: StopPhase) -> Output {
    if opts.stop == StopPhase::Link {
        return Output::Temp;
    }
    match opts.output.as_deref() {
        Some(p) if p == Path::new("-") => Output::Stdout,
        Some(p) => Output::File(p.to_path_buf()),
        None => match stop {
            StopPhase::Assembly => Output::File(default_named(input, "s")),
            StopPhase::Object => Output::File(default_named(input, "o")),
            _ => Output::Stdout,
        },
    }
}

fn default_named(input: &InputFile, extension: &str) -> PathBuf {
    let path: &Path = match input {
        InputFile::CSource(p) => Path::new(p),
        InputFile::Object(p) => p.as_path(),
    };
    let stem = path.file_stem().unwrap_or_default();
    PathBuf::from(stem).with_extension(extension)
}
