//! A minimal in-process JIT for TIR modules on Linux.
//!
//! Give it a TIR IR module as text; it runs the shared backend pipeline
//! (instruction selection, register allocation, finalization), maps the
//! resulting machine code into executable memory, resolves relocations against
//! runtime addresses, and hands back callable function pointers. External calls
//! into the host process are supported by registering host symbols before
//! compiling.
//!
//! Compiled functions follow the target's C calling convention (System V on
//! x86-64, AAPCS on AArch64), so they are callable as `extern "C"` fn pointers.
//! Only register-passed arguments are supported, matching the backend's ABI
//! lowering. x86-64 and AArch64 are supported; RISC-V is best-effort.

// Linked purely so each backend's `register_target!` entry reaches the final
// binary, making the host target resolvable through `select_target`.
use tir_arm64 as _;
use tir_riscv as _;
use tir_x86_64 as _;

mod loader;
mod reloc;

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;
use std::fmt;

use tir::backend::pipeline::{StopAfter, build_pipeline};
use tir::builtin::{FuncOp, ModuleOp};
use tir::{Context, Operation, PassManager};

/// A JIT compiler for a single target.
pub struct Jit {
    march: String,
    mcpu: Option<String>,
    host_symbols: HashMap<String, usize>,
}

impl Jit {
    /// Create a JIT for the host architecture.
    pub fn host() -> Result<Self, JitError> {
        let march = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "arm64",
            "riscv64" => "riscv64",
            other => return Err(JitError::UnknownTarget(other.to_string())),
        };
        Ok(Self::new(march, None))
    }

    /// Create a JIT for an explicit `--march`/`--mcpu`.
    pub fn new(march: impl Into<String>, mcpu: Option<String>) -> Self {
        Self {
            march: march.into(),
            mcpu,
            host_symbols: HashMap::new(),
        }
    }

    /// Register a host function (or data) the compiled module may reference by
    /// symbol name. `addr` is a raw pointer to the host item.
    pub fn define_symbol(&mut self, name: impl Into<String>, addr: *const c_void) {
        self.host_symbols.insert(name.into(), addr as usize);
    }

    /// Compile a TIR IR module (textual form) into executable memory.
    pub fn compile(&self, ir: &str) -> Result<Module, JitError> {
        let target = tir::backend::select_target(&self.march, self.mcpu.as_deref(), None)
            .map_err(JitError::UnknownTarget)?;

        let context = Context::with_default_dialects();
        target.register_dialects(&context);

        let module = tir::parse::ir::parse_ir::<ModuleOp>(&context, ir)
            .map_err(|(span, err)| JitError::Parse(format!("at byte {}: {err:?}", span.0)))?;
        let module_op = context.get_op(module.id());

        // Promote memory to SSA so alloca/load/store IR reaches selectable form,
        // mirroring the frontend pipeline.
        let mut pm = PassManager::new();
        pm.nest(FuncOp::name())
            .add_pass(tir::passes::Mem2RegPass::new());
        pm.run(&context, module_op.clone())
            .map_err(|e| JitError::Pipeline(format!("mem2reg: {e}")))?;

        let mut pm = build_pipeline(target.as_ref(), &context, StopAfter::Finalize);
        pm.run(&context, module_op)
            .map_err(|e| JitError::Pipeline(e.to_string()))?;

        let fmt = target
            .object_format()
            .ok_or_else(|| JitError::NoObjectSupport(self.march.clone()))?;
        let writer = target
            .binary_writer(&context)
            .ok_or_else(|| JitError::NoObjectSupport(self.march.clone()))?;
        let obj = writer
            .write_module(&context, &module, &fmt)
            .map_err(|e| JitError::Emit {
                message: e.to_string(),
            })?;

        let loaded = loader::load(&obj, &fmt, &self.host_symbols)?;
        Ok(Module {
            _map: loaded.map,
            functions: loaded.symbols,
        })
    }
}

/// A compiled, executable module. Dropping it unmaps the code.
pub struct Module {
    _map: ExecMap,
    functions: HashMap<String, usize>,
}

impl Module {
    /// Runtime address of a defined symbol, or `None` if absent.
    pub fn address(&self, name: &str) -> Option<usize> {
        self.functions.get(name).copied()
    }

    /// Get a compiled function as a typed `extern "C"` pointer.
    ///
    /// # Safety
    /// `F` must be an `extern "C"` function-pointer type whose signature
    /// matches the compiled function's ABI; calling through a mismatched
    /// signature is undefined behavior.
    pub unsafe fn get<F: Copy>(&self, name: &str) -> Option<F> {
        assert_eq!(
            std::mem::size_of::<F>(),
            std::mem::size_of::<usize>(),
            "F must be a function pointer"
        );
        let addr = self.functions.get(name)?;
        Some(unsafe { std::mem::transmute_copy::<usize, F>(addr) })
    }
}

/// An `mmap`'d region owning compiled code; unmapped on drop.
pub struct ExecMap {
    ptr: *mut u8,
    len: usize,
}

impl ExecMap {
    fn new(len: usize) -> Result<Self, JitError> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let len = (len + page - 1) & !(page - 1);
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(JitError::Mmap(std::io::Error::last_os_error().to_string()));
        }
        Ok(Self {
            ptr: ptr as *mut u8,
            len,
        })
    }

    fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    fn make_executable(&self) -> Result<(), JitError> {
        let rc = unsafe {
            libc::mprotect(
                self.ptr as *mut c_void,
                self.len,
                libc::PROT_READ | libc::PROT_EXEC,
            )
        };
        if rc != 0 {
            return Err(JitError::Mmap(std::io::Error::last_os_error().to_string()));
        }
        Ok(())
    }
}

impl Drop for ExecMap {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut c_void, self.len);
        }
    }
}

#[derive(Debug)]
pub enum JitError {
    UnknownTarget(String),
    Parse(String),
    Pipeline(String),
    Emit { message: String },
    NoObjectSupport(String),
    UnresolvedSymbol(String),
    Mmap(String),
    RelocUnsupported { machine: u16, r_type: u32 },
    RelocRange { r_type: u32, value: i64 },
}

impl fmt::Display for JitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JitError::UnknownTarget(t) => write!(f, "unknown or unsupported target: {t}"),
            JitError::Parse(m) => write!(f, "failed to parse IR: {m}"),
            JitError::Pipeline(m) => write!(f, "backend pipeline failed: {m}"),
            JitError::Emit { message } => write!(f, "machine-code emission failed: {message}"),
            JitError::NoObjectSupport(t) => {
                write!(f, "target '{t}' cannot emit machine code")
            }
            JitError::UnresolvedSymbol(s) => write!(f, "unresolved external symbol: {s}"),
            JitError::Mmap(e) => write!(f, "executable memory allocation failed: {e}"),
            JitError::RelocUnsupported { machine, r_type } => {
                write!(
                    f,
                    "unsupported relocation r_type {r_type} for machine {machine}"
                )
            }
            JitError::RelocRange { r_type, value } => {
                write!(f, "relocation r_type {r_type} value {value} out of range")
            }
        }
    }
}

impl Error for JitError {}
