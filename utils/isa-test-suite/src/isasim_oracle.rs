//! The simulator-under-test oracle: drives the `tir-isasim` binary (built from
//! the TMDL descriptions) and parses its `--dump-state` JSON. This is target
//! agnostic — it only needs the `--march` string carried by the `Program`.

use crate::oracle::{Oracle, Program};
use crate::state::{ArchState, GPR_COUNT, MemWindow};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct IsasimOracle {
    /// Path to the built `tir-isasim` binary.
    pub bin: PathBuf,
}

/// Mirrors the JSON written by `isasim --dump-state`.
#[derive(Deserialize)]
struct StateDumpJson {
    pc: String,
    gprs: Vec<String>,
    mem: Vec<MemWindowJson>,
}

#[derive(Deserialize)]
struct MemWindowJson {
    addr: String,
    bytes: Vec<u8>,
}

impl Oracle for IsasimOracle {
    fn name(&self) -> &str {
        "isasim"
    }

    fn run(&self, prog: &Program, work_dir: &Path) -> Result<ArchState> {
        let src_path = work_dir.join("isasim.s");
        let dump_path = work_dir.join("isasim-state.json");
        std::fs::write(&src_path, &prog.isasim_source).context("writing isasim source")?;

        let mut cmd = Command::new(&self.bin);
        cmd.arg(format!("--march={}", prog.isasim_march))
            .arg(format!("--entry={}", prog.entry))
            .arg(format!("--until-pc={}", prog.stop))
            .arg("--no-default-memory")
            .arg(format!("--mem-start-address={}", prog.mem_base))
            .arg(format!("--mem-size={}", prog.mem_size))
            .arg("--dump-state")
            .arg(&dump_path);
        for (addr, len) in &prog.windows {
            cmd.arg("--dump-mem").arg(format!("0x{addr:x}:{len}"));
        }
        cmd.arg(&src_path);

        let output = cmd.output().context("spawning tir-isasim")?;
        if !output.status.success() {
            bail!(
                "tir-isasim failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let text = std::fs::read_to_string(&dump_path)
            .context("reading isasim state dump (did the run reach the stop label?)")?;
        let dump: StateDumpJson =
            serde_json::from_str(&text).context("parsing isasim state dump")?;

        if dump.gprs.len() != GPR_COUNT {
            bail!(
                "isasim reported {} gprs, expected {GPR_COUNT}",
                dump.gprs.len()
            );
        }
        let mut gprs = [0u64; GPR_COUNT];
        for (i, raw) in dump.gprs.iter().enumerate() {
            gprs[i] = parse_u64(raw).with_context(|| format!("parsing isasim x{i}"))?;
        }

        let mem = dump
            .mem
            .into_iter()
            .map(|w| {
                Ok(MemWindow {
                    addr: parse_u64(&w.addr).context("parsing isasim mem window addr")?,
                    bytes: w.bytes,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(ArchState {
            gprs,
            pc: parse_u64(&dump.pc).context("parsing isasim pc")?,
            mem,
        })
    }
}

fn parse_u64(s: &str) -> Result<u64> {
    let s = s.trim();
    let value = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)?
    } else {
        s.parse::<u64>()?
    };
    Ok(value)
}
