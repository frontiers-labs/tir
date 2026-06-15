//! The `fcc` diagnostic system: numbered, self-describing errors and warnings
//! rendered with [`ariadne`], in the spirit of `rustc` and the Microsoft C
//! compiler.
//!
//! Every diagnostic carries a stable [`Code`] (e.g. `E0001`, `W0300`) whose
//! title, long-form explanation and standard reference live in one catalog, so
//! `fcc --explain E0001` and the inline report draw from the same source of
//! truth.
//!
//! Source positions are [`Span`]s: a single `u64` packing an interned [`FileId`]
//! (high 32 bits) with a byte offset (low 32 bits). Because the file is part of
//! the span, a diagnostic raised inside an `#include`d file resolves to that
//! file's own text — there is no need to translate offsets back to the primary
//! translation unit. The interner ([`intern_file`]) owns each file's name and
//! source so a [`Diagnostic`] can render itself without the caller threading
//! that text around.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

use ariadne::{Color, Config, IndexType, Label, Report, ReportKind, Source};

// ---------------------------------------------------------------------------
// Source files and spans
// ---------------------------------------------------------------------------

/// Handle to an interned source file (its name and text). See [`intern_file`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct FileId(u32);

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
// Diagnostic catalog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
}

/// A stable diagnostic identifier. The numeric ranges group related problems:
/// `E0001..` syntax, `E02xx` name resolution, `E03xx`/`W03xx` preprocessor,
/// `E09xx` constructs `fcc` does not yet implement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Code {
    UnexpectedToken,
    UnexpectedEof,
    UndeclaredIdentifier,
    PreprocError,
    PreprocWarning,
    UnsupportedConstruct,
    EmptyTranslationUnit,
}

impl Code {
    pub fn severity(self) -> Severity {
        match self {
            Code::PreprocWarning => Severity::Warning,
            _ => Severity::Error,
        }
    }

    /// The printable identifier, e.g. `"E0001"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::UnexpectedToken => "E0001",
            Code::UnexpectedEof => "E0002",
            Code::UndeclaredIdentifier => "E0200",
            Code::PreprocError => "E0300",
            Code::PreprocWarning => "W0300",
            Code::UnsupportedConstruct => "E0900",
            Code::EmptyTranslationUnit => "E0901",
        }
    }

    /// The one-line summary shown as the report message.
    pub fn title(self) -> &'static str {
        match self {
            Code::UnexpectedToken => "unexpected token",
            Code::UnexpectedEof => "unexpected end of file",
            Code::UndeclaredIdentifier => "use of undeclared identifier",
            Code::PreprocError => "#error directive",
            Code::PreprocWarning => "#warning directive",
            Code::UnsupportedConstruct => "unsupported construct",
            Code::EmptyTranslationUnit => "empty translation unit",
        }
    }

    /// A standard reference, printed as a `note:` so the user can read the rule
    /// the diagnostic enforces. Section numbers follow ISO/IEC 9899:2018 (C17).
    pub fn reference(self) -> Option<&'static str> {
        match self {
            Code::UnexpectedToken | Code::UnexpectedEof => Some(
                "C17 6.9: an external declaration must be a function definition or a declaration",
            ),
            Code::UndeclaredIdentifier => {
                Some("C17 6.5.1: an identifier must be visibly declared before it is used")
            }
            Code::PreprocError => {
                Some("C17 6.10.5: the #error directive renders the program ill-formed")
            }
            Code::PreprocWarning => {
                Some("C23 6.10.6: #warning emits a diagnostic without halting translation")
            }
            Code::UnsupportedConstruct | Code::EmptyTranslationUnit => None,
        }
    }

    /// The long-form text shown by `fcc --explain <CODE>`.
    pub fn explanation(self) -> &'static str {
        match self {
            Code::UnexpectedToken => {
                "\
The parser reached a token that cannot continue the current grammar rule. This
usually means a missing or stray token: a forgotten semicolon, an unbalanced
brace or parenthesis, or an operator without an operand.

Read the label to see what the parser expected at that point, then add the
missing token or remove the unexpected one."
            }
            Code::UnexpectedEof => {
                "\
The source ended while the parser was still expecting more input, for example a
closing brace for a function body or the rest of an unfinished expression.

Make sure every `{`, `(` and statement is closed before the end of the file."
            }
            Code::UndeclaredIdentifier => {
                "\
A variable was read or assigned before any declaration introduced it into
scope. C has no implicit declarations: a name must be declared with a type
before it is used.

Declare the variable before the statement that uses it, e.g. `int total = 0;`,
and check the spelling of the identifier."
            }
            Code::PreprocError => {
                "\
The translation unit contains an active `#error` directive. The preprocessor
emits the directive's text and the program is rejected.

Remove the `#error`, or satisfy the `#if` condition that guards it (often a
missing `-D` define or include path)."
            }
            Code::PreprocWarning => {
                "\
An active `#warning` directive emitted its message. Unlike `#error`, this does
not stop compilation; it flags a condition the author wanted you to notice.

Address the cause described by the message, or remove the directive once it no
longer applies."
            }
            Code::UnsupportedConstruct => {
                "\
The construct is valid C but `fcc` does not lower it to IR yet. The frontend
parses a wider language than the code generator currently supports.

Rewrite the function using the supported subset, or pick an earlier `--stage`
(such as `ast`) that does not require code generation."
            }
            Code::EmptyTranslationUnit => {
                "\
Code generation was asked to lower a translation unit that contains no
functions. There is nothing to emit.

Provide at least one function definition in the input."
            }
        }
    }

    pub fn from_code(code: &str) -> Option<Code> {
        const ALL: [Code; 7] = [
            Code::UnexpectedToken,
            Code::UnexpectedEof,
            Code::UndeclaredIdentifier,
            Code::PreprocError,
            Code::PreprocWarning,
            Code::UnsupportedConstruct,
            Code::EmptyTranslationUnit,
        ];
        ALL.into_iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(code))
    }
}

// ---------------------------------------------------------------------------
// Diagnostic
// ---------------------------------------------------------------------------

/// The rendered form every diagnostic lowers to. It is rarely built directly:
/// each problem has its own concrete type (see below) constructed with `new`
/// and converted with `.into()`, so the message, label and help that belong to
/// a [`Code`] live in one place.
///
/// `label` ties the message to a position in a source file; when absent
/// (codegen has no source spans yet) the diagnostic renders as a compact header
/// without a snippet.
#[derive(Debug)]
pub struct Diagnostic {
    code: Code,
    message: String,
    label: Option<(Span, String)>,
    help: Option<String>,
}

impl Diagnostic {
    fn new(code: Code, message: impl Into<String>) -> Self {
        Diagnostic {
            code,
            message: message.into(),
            label: None,
            help: None,
        }
    }

    fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.label = Some((span, message.into()));
        self
    }

    fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn code(&self) -> Code {
        self.code
    }

    pub fn is_error(&self) -> bool {
        self.code.severity() == Severity::Error
    }

    /// Render to stderr with color (the interactive default).
    pub fn eprint(&self) {
        let _ = self.write(&mut io::stderr(), true);
    }

    /// Render to an arbitrary writer; `color` toggles ANSI styling (off for
    /// tests and non-terminal output).
    pub fn write(&self, w: &mut dyn Write, color: bool) -> io::Result<()> {
        match &self.label {
            Some((span, label)) => self.write_report(*span, label, w, color),
            None => self.write_compact(w, color),
        }
    }

    fn write_report(
        &self,
        span: Span,
        label: &str,
        w: &mut dyn Write,
        color: bool,
    ) -> io::Result<()> {
        let name = file_name(span.file());
        let source = file_source(span.file());
        // Point spans carry only a start; underline the first byte so the caret
        // has something to sit under, clamping at end of file.
        let off = span.offset();
        let range = off..(off + 1).min(source.len()).max(off);

        let (kind, accent) = match self.code.severity() {
            Severity::Error => (ReportKind::Error, Color::Red),
            Severity::Warning => (ReportKind::Warning, Color::Yellow),
        };
        let mut report = Report::build(kind, (name.clone(), range.clone()))
            .with_config(
                Config::new()
                    .with_index_type(IndexType::Byte)
                    .with_color(color),
            )
            .with_code(self.code.as_str())
            .with_message(&self.message)
            .with_label(
                Label::new((name.clone(), range))
                    .with_message(label)
                    .with_color(accent),
            );
        if let Some(help) = &self.help {
            report = report.with_help(help);
        }
        if let Some(reference) = self.code.reference() {
            report = report.with_note(reference);
        }
        report.finish().write((name, Source::from(&*source)), w)
    }

    /// Spanless rendering: `kind[CODE]: message` plus help/note lines, matching
    /// ariadne's header style without a source frame.
    fn write_compact(&self, w: &mut dyn Write, color: bool) -> io::Result<()> {
        let (word, accent) = match self.code.severity() {
            Severity::Error => ("error", "\x1b[31m"),
            Severity::Warning => ("warning", "\x1b[33m"),
        };
        let (a, r) = if color { (accent, "\x1b[0m") } else { ("", "") };
        writeln!(w, "{a}{word}[{}]{r}: {}", self.code.as_str(), self.message)?;
        if let Some(help) = &self.help {
            writeln!(w, "  = help: {help}")?;
        }
        if let Some(reference) = self.code.reference() {
            writeln!(w, "  = note: {reference}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Concrete diagnostics
//
// Each problem the compiler can report is its own type, built with `new` and
// turned into a `Diagnostic` with `.into()`. This keeps the message, span label
// and fix hint for a given `Code` next to the data they need.
// ---------------------------------------------------------------------------

/// `E0001`: the parser met a token that cannot continue the current rule.
pub struct UnexpectedToken {
    pub span: Span,
    /// The parser's account of what it expected versus what it found.
    pub reason: String,
}

impl UnexpectedToken {
    pub fn new(span: Span, reason: impl Into<String>) -> Self {
        UnexpectedToken {
            span,
            reason: reason.into(),
        }
    }
}

impl From<UnexpectedToken> for Diagnostic {
    fn from(d: UnexpectedToken) -> Diagnostic {
        Diagnostic::new(Code::UnexpectedToken, Code::UnexpectedToken.title())
            .with_label(d.span, d.reason)
            .with_help("check for a missing or misplaced token near here")
    }
}

/// `E0002`: input ended while the parser still expected more.
pub struct UnexpectedEof {
    pub span: Span,
    pub reason: String,
}

impl UnexpectedEof {
    pub fn new(span: Span, reason: impl Into<String>) -> Self {
        UnexpectedEof {
            span,
            reason: reason.into(),
        }
    }
}

impl From<UnexpectedEof> for Diagnostic {
    fn from(d: UnexpectedEof) -> Diagnostic {
        Diagnostic::new(Code::UnexpectedEof, Code::UnexpectedEof.title())
            .with_label(d.span, d.reason)
            .with_help("a brace, parenthesis or statement is left unclosed")
    }
}

/// `E0200`: a name is used without any declaration in scope.
pub struct UndeclaredIdentifier {
    pub name: String,
}

impl UndeclaredIdentifier {
    pub fn new(name: impl Into<String>) -> Self {
        UndeclaredIdentifier { name: name.into() }
    }
}

impl From<UndeclaredIdentifier> for Diagnostic {
    fn from(d: UndeclaredIdentifier) -> Diagnostic {
        Diagnostic::new(
            Code::UndeclaredIdentifier,
            format!("use of undeclared identifier '{}'", d.name),
        )
        .with_help(format!("declare '{}' with a type before using it", d.name))
    }
}

/// `E0900`: valid C that the code generator does not lower yet.
pub struct UnsupportedConstruct {
    pub what: String,
}

impl UnsupportedConstruct {
    pub fn new(what: impl Into<String>) -> Self {
        UnsupportedConstruct { what: what.into() }
    }
}

impl From<UnsupportedConstruct> for Diagnostic {
    fn from(d: UnsupportedConstruct) -> Diagnostic {
        Diagnostic::new(
            Code::UnsupportedConstruct,
            format!("codegen not yet implemented for {}", d.what),
        )
    }
}

/// `E0901`: code generation reached a translation unit with no functions.
pub struct EmptyTranslationUnit;

impl From<EmptyTranslationUnit> for Diagnostic {
    fn from(_: EmptyTranslationUnit) -> Diagnostic {
        Diagnostic::new(
            Code::EmptyTranslationUnit,
            "translation unit contains no functions",
        )
    }
}

/// `E0300`: an active `#error` directive.
pub struct PreprocError {
    pub span: Span,
    /// The directive's text (empty for a bare `#error`).
    pub text: String,
}

impl PreprocError {
    pub fn new(span: Span, text: impl Into<String>) -> Self {
        PreprocError {
            span,
            text: text.into(),
        }
    }
}

impl From<PreprocError> for Diagnostic {
    fn from(d: PreprocError) -> Diagnostic {
        let message = if d.text.is_empty() {
            Code::PreprocError.title().to_string()
        } else {
            d.text
        };
        Diagnostic::new(Code::PreprocError, message)
            .with_label(d.span, "#error directive encountered")
    }
}

/// `W0300`: an active `#warning` directive.
pub struct PreprocWarning {
    pub span: Span,
    pub text: String,
}

impl PreprocWarning {
    pub fn new(span: Span, text: impl Into<String>) -> Self {
        PreprocWarning {
            span,
            text: text.into(),
        }
    }
}

impl From<PreprocWarning> for Diagnostic {
    fn from(d: PreprocWarning) -> Diagnostic {
        let message = if d.text.is_empty() {
            Code::PreprocWarning.title().to_string()
        } else {
            d.text
        };
        Diagnostic::new(Code::PreprocWarning, message)
            .with_label(d.span, "#warning directive encountered")
    }
}

/// The body of `fcc --explain <CODE>`: the title line followed by the long-form
/// explanation and, where it exists, the standard reference.
pub fn explain(code: &str) -> Option<String> {
    let code = Code::from_code(code)?;
    let word = match code.severity() {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let mut out = format!(
        "{word}[{}]: {}\n\n{}\n",
        code.as_str(),
        code.title(),
        code.explanation()
    );
    if let Some(reference) = code.reference() {
        out.push_str(&format!("\nReference: {reference}\n"));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a diagnostic to a plain (color-free) string.
    fn render(diag: Diagnostic) -> String {
        let mut buf = Vec::new();
        diag.write(&mut buf, false).unwrap();
        String::from_utf8(buf).unwrap()
    }

    const CODES: [Code; 7] = [
        Code::UnexpectedToken,
        Code::UnexpectedEof,
        Code::UndeclaredIdentifier,
        Code::PreprocError,
        Code::PreprocWarning,
        Code::UnsupportedConstruct,
        Code::EmptyTranslationUnit,
    ];

    #[test]
    fn codes_round_trip_and_are_unique() {
        let mut seen = Vec::new();
        for code in CODES {
            assert_eq!(Code::from_code(code.as_str()), Some(code));
            assert!(!seen.contains(&code.as_str()), "duplicate code string");
            seen.push(code.as_str());
        }
        assert_eq!(Code::from_code("e0001"), Some(Code::UnexpectedToken));
        assert_eq!(Code::from_code("E9999"), None);
    }

    #[test]
    fn severity_follows_code_prefix() {
        for code in CODES {
            let expected = if code.as_str().starts_with('W') {
                Severity::Warning
            } else {
                Severity::Error
            };
            assert_eq!(code.severity(), expected);
        }
    }

    #[test]
    fn span_packs_file_and_offset() {
        let file = intern_file("<span-test>", "source");
        let span = Span::new(file, 1234);
        assert_eq!(span.file(), file);
        assert_eq!(span.offset(), 1234);
    }

    #[test]
    fn spanned_report_points_at_source() {
        let src = "int main(void) { return; }";
        let file = intern_file("<report-test>", src);
        let at = src.find("return").unwrap();
        let diag: Diagnostic = UnexpectedToken::new(Span::new(file, at), "found ';'").into();

        let out = render(diag);
        assert!(out.contains("[E0001]"), "{out}");
        assert!(out.contains("unexpected token"), "{out}");
        assert!(out.contains("found ';'"), "{out}");
        assert!(out.contains("<report-test>"), "{out}");
        // The standard reference is attached automatically from the catalog.
        assert!(out.contains("6.9"), "{out}");
    }

    #[test]
    fn spanless_diagnostic_renders_compact_header() {
        let out = render(UndeclaredIdentifier::new("count").into());
        assert!(out.starts_with("error[E0200]:"), "{out}");
        assert!(out.contains("undeclared identifier 'count'"), "{out}");
        assert!(out.contains("= help:"), "{out}");
        assert!(out.contains("= note:"), "{out}");
    }

    #[test]
    fn warning_uses_warning_severity() {
        let file = intern_file("<warn-test>", "#warning hi");
        let diag: Diagnostic = PreprocWarning::new(Span::new(file, 0), "hi").into();
        assert!(!diag.is_error());
        assert_eq!(diag.code(), Code::PreprocWarning);
    }

    #[test]
    fn explain_known_and_unknown() {
        let text = explain("E0300").unwrap();
        assert!(text.contains("error[E0300]"));
        assert!(text.contains("#error"));
        assert!(text.contains("Reference:"));
        assert!(explain("nope").is_none());
    }
}
