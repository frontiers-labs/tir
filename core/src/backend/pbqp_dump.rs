use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use tir_pbqp::PbqpProblem;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) enum PbqpTaskKind {
    Isel,
    RegAlloc,
}

impl PbqpTaskKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Isel => "isel",
            Self::RegAlloc => "regalloc",
        }
    }
}

pub(super) fn dump(problem: &PbqpProblem, kind: PbqpTaskKind) {
    let Some(directory) = dump_directory() else {
        return;
    };
    if let Err(error) = fs::create_dir_all(directory) {
        eprintln!(
            "tir-pbqp: cannot create dump directory '{}': {error}",
            directory.display()
        );
        return;
    }

    let (path, file) = loop {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "{}-{}-{sequence}.json",
            kind.as_str(),
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                eprintln!("tir-pbqp: cannot create '{}': {error}", path.display());
                return;
            }
        }
    };

    let mut writer = BufWriter::new(file);
    let result = problem
        .write_json(&mut writer, kind.as_str())
        .and_then(|()| writer.flush());
    drop(writer);
    if let Err(error) = result {
        eprintln!("tir-pbqp: cannot write '{}': {error}", path.display());
        if let Err(remove_error) = fs::remove_file(&path) {
            eprintln!(
                "tir-pbqp: cannot remove partial dump '{}': {remove_error}",
                path.display()
            );
        }
    }
}

fn dump_directory() -> Option<&'static Path> {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            std::env::var_os("TIR_PBQP_DUMP_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .as_deref()
}
