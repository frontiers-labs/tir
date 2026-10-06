use std::path::PathBuf;

pub fn xtask() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_xtask")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_xtask").into())
}

pub fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into())
}
