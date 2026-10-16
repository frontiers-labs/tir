//! RISC-V target specifics: how a test body is composed into a full program, the
//! shared memory layout, and the Spike golden oracle.

use crate::oracle::{Oracle, Program};
use crate::state::{ArchState, GPR_COUNT, MemWindow};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Base address the image is linked/loaded at. Spike maps devices over the low
/// address space, so `0x8000_0000` (its default DRAM base) is the safe choice.
pub const MEM_BASE: u64 = 0x8000_0000;
/// Data memory provided to both oracles (1 MiB). Spike DRAM and isasim memory
/// both start zeroed here.
pub const MEM_SIZE: u64 = 0x10_0000;
/// Scratch region base that the harness preloads into `x10`/`a0`. Snippets that
/// touch memory do so through this pointer so both oracles use the same address.
pub const SCRATCH: u64 = 0x8000_8000;
/// Bytes of the scratch region compared after each run.
pub const WINDOW_LEN: usize = 256;

const ISASIM_MARCH: &str = "riscv64";
const SPIKE_ISA: &str = "rv64imafdc";
/// Assemble without the `c` (compressed) extension so GAS emits 4-byte
/// instructions, matching isasim's fixed-width layout and keeping `done`'s
/// address identical across both oracles.
const GCC_MARCH: &str = "rv64imafd";
const GCC_ABI: &str = "lp64d";

/// External tools this target's golden oracle needs.
pub const REQUIRED_TOOLS: &[&str] = &["spike", "riscv64-unknown-elf-gcc", "riscv64-unknown-elf-nm"];

/// ABI register names in `x0..x31` order, used to map Spike's `reg 0` dump back
/// to numeric indices.
const ABI_NAMES: [&str; GPR_COUNT] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// Wrap a test body in the shared harness: a prologue that zeroes `x1..x31`
/// (wiping Spike's boot-ROM scratch so both oracles start identically) and loads
/// the scratch pointer into `x10`, then the body, then the `done` stop label.
pub fn build_program(body: &str) -> Program {
    // The `_start` block: prologue (zero x1..x31, set the scratch pointer) then
    // the test body. Both oracles run this at the base address. The `.global`
    // must sit immediately before its label — isasim's parser ties the symbol to
    // the directive's position, and grouping all globals up top misplaces them.
    let mut start_block = String::from(".global _start\n_start:\n");
    for i in 1..GPR_COUNT {
        // Zero every register except x0 (already hardwired to 0).
        start_block.push_str(&format!("  addi x{i}, x0, 0\n"));
    }
    // Materialize the scratch base in x10. A bare `lui` sign-extends on RV64, so
    // we zero-extend the 32-bit value back to a positive address.
    let hi = SCRATCH >> 12;
    debug_assert_eq!(hi << 12, SCRATCH, "SCRATCH must be 4 KiB aligned");
    start_block.push_str(&format!("  lui x10, 0x{hi:x}\n"));
    start_block.push_str("  slli x10, x10, 32\n");
    start_block.push_str("  srli x10, x10, 32\n");
    // isasim's parser rejects inline `#` comments, so strip comments here; this
    // keeps snippet files readable while feeding both oracles clean assembly.
    start_block.push_str(strip_comments(body).trim_end());
    start_block.push('\n');

    let done_block = ".global done\ndone:\n  add x0, x0, x0\n";

    // Canonical order for a normal assembler: body, then the trailing stop label.
    let source = format!("{start_block}{done_block}");
    // isasim assigns label-block addresses in reverse source order, so emitting
    // `done` first lands it immediately after the body — the same layout.
    let isasim_source = format!("{done_block}{start_block}");

    Program {
        source,
        isasim_source,
        isasim_march: ISASIM_MARCH.to_string(),
        mem_base: MEM_BASE,
        mem_size: MEM_SIZE,
        entry: "_start".to_string(),
        stop: "done".to_string(),
        windows: vec![(SCRATCH, WINDOW_LEN)],
    }
}

fn strip_comments(body: &str) -> String {
    body.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct SpikeOracle;

impl Oracle for SpikeOracle {
    fn name(&self) -> &str {
        "spike"
    }

    fn run(&self, prog: &Program, work_dir: &Path) -> Result<ArchState> {
        let src = work_dir.join("spike.s");
        let elf = work_dir.join("spike.elf");
        let ld = work_dir.join("link.ld");
        let cmds = work_dir.join("spike-cmds.txt");

        std::fs::write(&src, &prog.source).context("writing spike source")?;
        std::fs::write(&ld, linker_script(prog.mem_base)).context("writing linker script")?;

        // Assemble + link the bare-metal image at the shared base address.
        run_tool(
            Command::new("riscv64-unknown-elf-gcc")
                .arg("-nostdlib")
                .arg("-nostartfiles")
                .arg("-mno-relax")
                .arg(format!("-march={GCC_MARCH}"))
                .arg(format!("-mabi={GCC_ABI}"))
                .arg("-Wl,-T")
                .arg(&ld)
                .arg("-o")
                .arg(&elf)
                .arg(&src),
        )
        .context("assembling snippet with riscv64-unknown-elf-gcc")?;

        let stop_addr = resolve_symbol(&elf, &prog.stop)?;

        // Drive Spike's interactive debugger: run to the stop label, then read
        // pc, all GPRs, and each memory window doubleword.
        let mut script = String::new();
        script.push_str(&format!("until pc 0 0x{stop_addr:x}\n"));
        script.push_str("pc 0\n");
        script.push_str("reg 0\n");
        for (addr, len) in &prog.windows {
            for off in (0..*len).step_by(8) {
                script.push_str(&format!("mem 0x{:x}\n", addr + off as u64));
            }
        }
        script.push_str("quit\n");
        std::fs::write(&cmds, script).context("writing spike command script")?;

        // Spike's interactive debugger prints register/pc/memory dumps to stderr,
        // so capture and parse that stream.
        let output = Command::new("spike")
            .arg(format!("-m0x{:x}:0x{:x}", prog.mem_base, prog.mem_size))
            .arg(format!("--isa={SPIKE_ISA}"))
            .arg("-d")
            .arg("--debug-cmd")
            .arg(&cmds)
            .arg(&elf)
            .output()
            .context("running spike")?;
        if !output.status.success() {
            bail!(
                "spike failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let dump = String::from_utf8_lossy(&output.stderr);

        parse_spike_output(&dump, &prog.windows)
    }
}

fn linker_script(base: u64) -> String {
    format!(
        "OUTPUT_ARCH(riscv)\n\
         ENTRY(_start)\n\
         SECTIONS {{\n\
         \x20 . = 0x{base:x};\n\
         \x20 .text : {{ *(.text*) }}\n\
         \x20 .rodata : {{ *(.rodata*) }}\n\
         \x20 .data : {{ *(.data*) }}\n\
         \x20 .bss : {{ *(.bss*) *(COMMON) }}\n\
         \x20 /DISCARD/ : {{ *(.riscv.attributes) *(.comment) *(.note*) }}\n\
         }}\n"
    )
}

fn resolve_symbol(elf: &Path, name: &str) -> Result<u64> {
    let out = run_tool(Command::new("riscv64-unknown-elf-nm").arg(elf))
        .context("reading symbols with riscv64-unknown-elf-nm")?;
    for line in out.lines() {
        let mut fields = line.split_whitespace();
        let addr = fields.next();
        let _kind = fields.next();
        let sym = fields.next();
        if sym == Some(name)
            && let Some(addr) = addr
        {
            return u64::from_str_radix(addr, 16)
                .with_context(|| format!("parsing address of symbol '{name}'"));
        }
    }
    bail!("symbol '{name}' not found in linked image")
}

fn parse_spike_output(stdout: &str, windows: &[(u64, usize)]) -> Result<ArchState> {
    let mut pc: Option<u64> = None;
    let mut gprs = [0u64; GPR_COUNT];
    let mut mem_words: Vec<u64> = Vec::new();
    let mut seen_regs = false;

    for line in stdout.lines() {
        let pairs = parse_reg_pairs(line);
        if !pairs.is_empty() {
            seen_regs = true;
            for (name, value) in pairs {
                if let Some(idx) = ABI_NAMES.iter().position(|n| *n == name) {
                    gprs[idx] = value;
                }
            }
            continue;
        }
        if let Some(value) = parse_bare_hex(line) {
            if !seen_regs {
                pc.get_or_insert(value);
            } else {
                mem_words.push(value);
            }
        }
    }

    let pc = pc.context("spike output missing pc")?;

    // Expand the captured doublewords back into per-window byte vectors.
    let mut mem = Vec::with_capacity(windows.len());
    let mut word_iter = mem_words.into_iter();
    for (addr, len) in windows {
        let mut bytes = Vec::with_capacity(*len);
        let words_needed = len.div_ceil(8);
        for _ in 0..words_needed {
            let word = word_iter
                .next()
                .context("spike output missing a memory doubleword")?;
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.truncate(*len);
        mem.push(MemWindow { addr: *addr, bytes });
    }

    Ok(ArchState { gprs, pc, mem })
}

fn parse_reg_pairs(line: &str) -> Vec<(&str, u64)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let mut pairs = Vec::new();
    let mut i = 0;
    while i + 1 < tokens.len() {
        if let Some(name) = tokens[i].strip_suffix(':')
            && ABI_NAMES.contains(&name)
            && let Some(value) = parse_bare_hex(tokens[i + 1])
        {
            pairs.push((name, value));
            i += 2;
            continue;
        }
        i += 1;
    }
    pairs
}

fn parse_bare_hex(token: &str) -> Option<u64> {
    let token = token.trim();
    let hex = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))?;
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

fn run_tool(cmd: &mut Command) -> Result<String> {
    let output = cmd
        .output()
        .with_context(|| format!("spawning {}", cmd.get_program().to_string_lossy()))?;
    if !output.status.success() {
        bail!(
            "{} failed ({}):\n{}",
            cmd.get_program().to_string_lossy(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
