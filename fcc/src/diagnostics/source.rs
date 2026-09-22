use std::sync::{Arc, Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Source files and spans
// ---------------------------------------------------------------------------

/// Handle to an interned source file (its name and text). See [`intern_file`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct FileId(u32);

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&file_name(*self))
    }
}

/// A source position: an interned file in the high 32 bits, a byte offset into
/// that file in the low 32 bits. One `u64` covers every position fcc reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span(u64);

impl Span {
    pub fn new(file: FileId, offset: usize) -> Span {
        Span((u64::from(file.0) << 32) | u64::from(offset as u32))
    }

    pub fn file(self) -> FileId {
        FileId((self.0 >> 32) as u32)
    }

    pub fn offset(self) -> usize {
        (self.0 & 0xffff_ffff) as usize
    }
}

type FileTable = Mutex<Vec<(String, Arc<str>)>>;

fn files() -> &'static FileTable {
    static FILES: OnceLock<FileTable> = OnceLock::new();
    FILES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a source file and return its handle. Each call appends a fresh
/// entry, so a file `#include`d twice gets two ids (each with its own text),
/// which is exactly what the renderer needs.
pub fn intern_file(name: &str, source: &str) -> FileId {
    let mut files = files().lock().unwrap();
    files.push((name.to_string(), Arc::from(source)));
    FileId((files.len() - 1) as u32)
}

pub fn file_source(file: FileId) -> Arc<str> {
    Arc::clone(&files().lock().unwrap()[file.0 as usize].1)
}

fn file_name(file: FileId) -> String {
    files().lock().unwrap()[file.0 as usize].0.clone()
}

// ---------------------------------------------------------------------------
// Catalog
