use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::Path,
    process::ExitCode,
};
use tir_cdsl::format::{FormatOptions, format_source};

fn main() -> ExitCode {
    match run() {
        Ok(changed) => ExitCode::from(u8::from(changed)),
        Err(error) => {
            eprintln!("cdsl-format: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let mut write = false;
    let mut check = false;
    let mut path: Option<OsString> = None;
    for argument in std::env::args_os().skip(1) {
        if argument == "--write" {
            write = true;
        } else if argument == "--check" {
            check = true;
        } else if argument == "--help" {
            println!("Usage: cdsl-format [--write | --check] [FILE | -]");
            return Ok(false);
        } else if argument.to_string_lossy().starts_with('-') && argument != "-" {
            return Err(format!("unknown option {}", argument.to_string_lossy()).into());
        } else if path.replace(argument).is_some() {
            return Err("expected at most one input file".into());
        }
    }
    if write && check {
        return Err("--write and --check are mutually exclusive".into());
    }
    let path = path.unwrap_or_else(|| OsString::from("-"));
    let source = if path == "-" {
        if write {
            return Err("--write requires a file".into());
        }
        let mut source = String::new();
        io::stdin().read_to_string(&mut source)?;
        source
    } else {
        std::fs::read_to_string(&path)?
    };
    let formatted = format_source(&source, FormatOptions::default())?;
    if check {
        return Ok(formatted != source);
    }
    if write {
        if formatted != source {
            replace(Path::new(&path), formatted.as_bytes())?;
        }
    } else {
        io::stdout().lock().write_all(formatted.as_bytes())?;
    }
    Ok(false)
}

fn replace(path: &Path, text: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::other("--write requires a regular file"));
    }
    let mut sequence = 0u64;
    let (temporary, mut file) = loop {
        let temporary = parent.join(format!(
            ".cdsl-format-{}-{sequence}.tmp",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => sequence += 1,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.set_permissions(metadata.permissions())?;
        file.write_all(text)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
