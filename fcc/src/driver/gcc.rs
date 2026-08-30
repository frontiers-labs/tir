use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use super::actions::{DriverOptions, InputFile, StopPhase, build_actions};
use super::exec::execute;
use crate::lang_options::LangOptions;

pub(super) fn run_gcc<I, T>(args: I)
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let fail = |e: &dyn fmt::Display| -> ! {
        eprintln!("fcc: error: {e}");
        std::process::exit(1);
    };
    let opts = parse_gcc(args).unwrap_or_else(|e| fail(&e));
    let actions = build_actions(&opts).unwrap_or_else(|e| fail(&e));
    execute(&actions, &opts).unwrap_or_else(|e| fail(&e));
}

#[derive(Debug, PartialEq, Eq)]
pub enum GccError {
    UnrecognizedOption(String),
    MissingArgument(String),
    InvalidStandard(String),
}

impl fmt::Display for GccError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GccError::UnrecognizedOption(opt) => {
                write!(f, "unrecognized command-line option '{opt}'")
            }
            GccError::MissingArgument(opt) => write!(f, "missing argument to '{opt}'"),
            GccError::InvalidStandard(msg) => write!(f, "{msg}"),
        }
    }
}

/// Parse gcc-compatible command-line arguments (the flags only, without the
/// program name) into `DriverOptions`.
pub fn parse_gcc<I, T>(args: I) -> Result<DriverOptions, GccError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|a| a.into().to_string_lossy().into_owned())
        .collect();

    let mut inputs = Vec::new();
    let mut output = None;
    let mut stop = StopPhase::Link;
    let mut lang_options = LangOptions::default();
    let mut defines = Vec::new();
    let mut undefines = Vec::new();
    let mut include_dirs = Vec::new();
    let mut march = None;
    let mut mcpu = None;
    let mut mabi = None;
    let mut lib_dirs = Vec::new();
    let mut libs = Vec::new();
    let mut dry_run = false;
    let mut warned = HashSet::new();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(value) = separated(&args, &mut i, "-o")? {
            output = Some(PathBuf::from(value));
        } else if let Some(value) = separated(&args, &mut i, "-I")? {
            include_dirs.push(PathBuf::from(value));
        } else if let Some(value) = separated(&args, &mut i, "-D")? {
            defines.push(value);
        } else if let Some(value) = separated(&args, &mut i, "-U")? {
            undefines.push(value);
        } else if let Some(value) = separated(&args, &mut i, "-l")? {
            libs.push(value);
        } else if let Some(value) = separated(&args, &mut i, "-L")? {
            lib_dirs.push(PathBuf::from(value));
        } else if arg == "-c" {
            stop = StopPhase::Object;
        } else if arg == "-S" {
            stop = StopPhase::Assembly;
        } else if arg == "-E" {
            stop = StopPhase::Preprocess;
        } else if arg == "-###" {
            dry_run = true;
        } else if let Some(value) = arg.strip_prefix("-std=") {
            lang_options = LangOptions::from_str(value).map_err(GccError::InvalidStandard)?;
        } else if let Some(value) = arg.strip_prefix("-march=") {
            march = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("-mcpu=") {
            mcpu = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("-mabi=") {
            mabi = Some(value.to_string());
        } else if is_value_taking_warn(arg) {
            warn_once(&mut warned, arg);
            if arg == "-MF" || arg == "-MT" || arg == "-MQ" {
                i += 1;
            }
        } else if is_ignored_flag(arg) {
            warn_once(&mut warned, arg);
        } else if arg.starts_with('-') && arg.len() > 1 {
            return Err(GccError::UnrecognizedOption(arg.to_string()));
        } else {
            inputs.push(classify_input(arg)?);
        }
        i += 1;
    }

    if march.is_none()
        && matches!(
            stop,
            StopPhase::Assembly | StopPhase::Object | StopPhase::Link
        )
    {
        march = host_march();
    }

    Ok(DriverOptions {
        inputs,
        output,
        stop,
        lang_options,
        defines,
        undefines,
        include_dirs,
        march,
        mcpu,
        mabi,
        lib_dirs,
        libs,
        pipeline: None,
        shuffle_machine_order: false,
        dry_run,
    })
}

/// Match `flag` in attached (`-oFILE`) or separate (`-o FILE`) form, advancing
/// `i` past a consumed separate value. Returns the value when `flag` matched.
fn separated(args: &[String], i: &mut usize, flag: &str) -> Result<Option<String>, GccError> {
    let arg = args[*i].as_str();
    if arg == flag {
        let value = args
            .get(*i + 1)
            .ok_or_else(|| GccError::MissingArgument(flag.to_string()))?;
        *i += 1;
        Ok(Some(value.clone()))
    } else if let Some(value) = arg.strip_prefix(flag) {
        Ok(Some(value.to_string()))
    } else {
        Ok(None)
    }
}

fn classify_input(arg: &str) -> Result<InputFile, GccError> {
    if arg.ends_with(".c") {
        Ok(InputFile::CSource(OsString::from(arg)))
    } else if arg.ends_with(".o") || arg.ends_with(".a") {
        Ok(InputFile::Object(PathBuf::from(arg)))
    } else {
        Err(GccError::UnrecognizedOption(arg.to_string()))
    }
}

fn is_value_taking_warn(arg: &str) -> bool {
    arg.starts_with("-MF") || arg.starts_with("-MT") || arg.starts_with("-MQ")
}

fn is_ignored_flag(arg: &str) -> bool {
    matches!(arg, "-pipe" | "-pthread")
        || arg.starts_with("-O")
        || arg.starts_with("-g")
        || arg.starts_with("-W")
        || arg.starts_with("-f")
        || arg.starts_with("-M")
        || arg.starts_with("-m")
}

fn warn_once(warned: &mut HashSet<String>, arg: &str) {
    if warned.insert(arg.to_string()) {
        eprintln!("fcc: warning: ignoring unsupported option '{arg}'");
    }
}

fn host_march() -> Option<String> {
    match std::env::consts::ARCH {
        "aarch64" => Some("arm64".to_string()),
        "x86_64" => Some("x86_64".to_string()),
        _ => None,
    }
}
