//! The compilers and optimization levels every program benchmark compares,
//! one benchmark per pair so that a filter can select it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use regex::Regex;
use tir_bench::Experiment;

/// One compiler at one optimization level, building and running one program.
pub struct Build<'a> {
    name: String,
    driver: &'a Path,
    prefix: &'a [&'a str],
    pub level: &'a str,
    /// Directory for this build's objects, executable and run log.
    pub output: PathBuf,
}

impl Build<'_> {
    /// An experiment named `<compiler>/<program>/<level>/<step>` running the
    /// compiler.
    pub fn compiler(&self, step: &str) -> Experiment {
        self.step(step, self.driver).args(self.prefix)
    }

    /// An experiment named like [`Self::compiler`]'s running `program`.
    pub fn step(&self, step: &str, program: &Path) -> Experiment {
        Experiment::new(format!("{}/{step}", self.name), program)
    }
}

/// The sources of a program under `utils/third-party`.
pub fn sources(program: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("utils/third-party")
        .join(program)
}

/// Which benchmarks a run selects.
enum Filter {
    All,
    Exact(String),
    Pattern(Regex),
}

impl Filter {
    fn selects(&self, name: &str) -> bool {
        match self {
            Filter::All => true,
            Filter::Exact(exact) => name == exact,
            Filter::Pattern(pattern) => pattern.is_match(name),
        }
    }
}

/// The libtest arguments that cargo bench and nextest pass to a benchmark,
/// see <https://nexte.st/docs/design/custom-test-harnesses/>.
struct Arguments {
    list: bool,
    ignored: bool,
    filter: Filter,
}

fn arguments() -> io::Result<Arguments> {
    let (mut list, mut ignored, mut exact, mut filter) = (false, false, false, None);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => list = true,
            "--ignored" => ignored = true,
            "--exact" => exact = true,
            "--format" => {
                args.next();
            }
            _ if arg.starts_with('-') => {}
            _ => filter = filter.or(Some(arg)),
        }
    }
    let filter = match filter {
        None => Filter::All,
        Some(name) if exact => Filter::Exact(name),
        Some(pattern) => Filter::Pattern(Regex::new(&pattern).map_err(io::Error::other)?),
    };
    Ok(Arguments {
        list,
        ignored,
        filter,
    })
}

/// Runs `bench` for every compiler and level whose benchmark name
/// `<compiler>/<program>/<level>` the arguments select, or lists those
/// names. Compilers listed in `unsupported` are recorded as skipped with the
/// reason.
pub fn each(
    program: &str,
    unsupported: &[(&str, &str)],
    mut bench: impl FnMut(&Build) -> io::Result<()>,
) -> io::Result<()> {
    let fcc = Path::new(env!("CARGO_BIN_EXE_fcc"));
    let arguments = arguments()?;
    for (compiler, driver, prefix) in [
        ("fcc", fcc, &["cc"][..]),
        ("gcc", Path::new("gcc"), &[][..]),
        ("clang", Path::new("clang"), &[][..]),
    ] {
        for level in ["-O0", "-O2"] {
            let name = format!("{compiler}/{program}/{level}");
            if !arguments.filter.selects(&name) {
                continue;
            }
            if arguments.list {
                if !arguments.ignored {
                    println!("{name}: benchmark");
                }
                continue;
            }
            if let Some((_, reason)) = unsupported.iter().find(|(name, _)| *name == compiler) {
                Experiment::new(name, driver).skip(reason)?;
                continue;
            }
            let output = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(program)
                .join(compiler)
                .join(level);
            fs::create_dir_all(&output)?;
            bench(&Build {
                name,
                driver,
                prefix,
                level,
                output,
            })?;
        }
    }
    Ok(())
}
