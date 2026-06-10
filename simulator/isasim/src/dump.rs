//! `--dump-state`: a structured JSON snapshot of final architectural state
//! (PC, GPRs, requested memory windows), consumed by the differential ISA test
//! suite to compare against a golden oracle.

use serde::Serialize;
use tir_be_common::MachineContext;
use tir_sim::Executor;

use crate::parse_addr;

/// JSON shape emitted by `--dump-state`. Hex strings keep large addresses/values
/// readable and avoid any signedness ambiguity across the oracle boundary.
#[derive(Serialize)]
struct StateDump {
    pc: String,
    /// `gprs[i]` is the value of integer register `x{i}` (unwritten reads as 0).
    gprs: Vec<String>,
    mem: Vec<MemWindowDump>,
}

#[derive(Serialize)]
struct MemWindowDump {
    addr: String,
    bytes: Vec<u8>,
}

/// Snapshot the final architectural state to `path` as JSON. `mem_windows` are
/// `addr:len` specs whose bytes are read out one at a time so any window that
/// runs past the configured memory simply reports a hard error rather than
/// silently truncating.
pub fn write_state_dump(executor: &Executor, path: &str, mem_windows: &[String]) {
    let mut gprs = Vec::with_capacity(32);
    for index in 0..32u16 {
        let value = executor
            .read_register("GPR", index)
            .expect("failed to read GPR for state dump");
        gprs.push(format!("0x{:x}", value.to_u64()));
    }

    let mut mem = Vec::with_capacity(mem_windows.len());
    for spec in mem_windows {
        let (addr, len) = parse_mem_window(spec);
        let mut bytes = Vec::with_capacity(len);
        for offset in 0..len {
            let address = addr
                .checked_add(offset as u64)
                .expect("memory window address overflow");
            let byte = executor
                .read_memory(address, 1)
                .expect("memory window does not fit configured memory window");
            bytes.push(byte as u8);
        }
        mem.push(MemWindowDump {
            addr: format!("0x{addr:x}"),
            bytes,
        });
    }

    let dump = StateDump {
        pc: format!("0x{:x}", executor.read_pc()),
        gprs,
        mem,
    };
    let json = serde_json::to_string_pretty(&dump).expect("failed to serialize state dump");
    std::fs::write(path, json).expect("failed to write state dump");
}

/// Parse a `--dump-mem` spec of the form `addr:len`, where `addr` is a hex/decimal
/// address and `len` is a byte count.
fn parse_mem_window(spec: &str) -> (u64, usize) {
    let (addr, len) = spec
        .split_once(':')
        .unwrap_or_else(|| panic!("--dump-mem expects 'addr:len', got '{spec}'"));
    let addr = parse_addr(addr.trim());
    let len = len
        .trim()
        .parse::<usize>()
        .unwrap_or_else(|_| panic!("--dump-mem length must be a byte count, got '{len}'"));
    (addr, len)
}
