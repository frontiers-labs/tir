// Sem programs are serialized into one per-crate blob instead of being emitted
// as Rust arrays: 275k const array elements made rustc materialize as many MIR
// constants, which dominated the memory footprint of the generated backends.
//
// The builder is ambient rather than threaded through the emitters because
// every one of the seven `emit_dag_as_code` call sites sits several frames deep
// in unrelated emission code. Code generation is single-threaded, and
// [`begin_sem_blob`]/[`finish_sem_blob`] bracket one whole-crate emission.

use std::cell::RefCell;

/// Name of the blob file the generated crate embeds; it is written next to the
/// generated Rust.
pub const SEM_BLOB_FILE: &str = "tmdl_sem.bin";

thread_local! {
    static SEM_BLOB: RefCell<tir_symbolic::sem::SemBlobBuilder> =
        RefCell::new(tir_symbolic::sem::SemBlobBuilder::new());
}

fn begin_sem_blob() {
    SEM_BLOB.with_borrow_mut(|blob| *blob = tir_symbolic::sem::SemBlobBuilder::new());
}

fn finish_sem_blob() -> (Vec<u8>, Vec<tir_symbolic::lang::SymKind>) {
    SEM_BLOB.with_borrow_mut(|blob| {
        std::mem::replace(blob, tir_symbolic::sem::SemBlobBuilder::new()).finish()
    })
}

fn intern_sem_ops(ops: &[tir_symbolic::sem::SemOp]) -> u32 {
    SEM_BLOB.with_borrow_mut(|blob| blob.intern(ops))
}
