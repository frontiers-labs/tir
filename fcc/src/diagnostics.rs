//! The `fcc` diagnostic system: numbered, self-describing errors and warnings
//! rendered with [`ariadne`], in the spirit of `rustc` and the Microsoft C
//! compiler.
//!
//! The whole catalog is one [`diagnostics!`] table. Each row declares a stable
//! [`Code`] (e.g. `E0001`, `W0300`), its title, standard reference and the
//! long-form text shown by `fcc --explain`, plus a concrete builder type
//! (`UnexpectedToken`, `UndeclaredIdentifier`, …) constructed with `new` and
//! converted into a [`Diagnostic`] with `.into()`. Severity is read from the
//! code's first letter (`W` = warning).
//!
//! Source positions are [`Span`]s: a single `u64` packing an interned [`FileId`]
//! (high 32 bits) with a byte offset (low 32 bits). Because the file is part of
//! the span, a diagnostic raised inside an `#include`d file resolves to that
//! file's own text. The interner ([`intern_file`]) owns each file's name and
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
// Catalog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
}

/// Declare the diagnostic catalog: the [`Code`] enum with its metadata plus one
/// builder type per row. `build` maps the type's fields (`d`) to a
/// [`Diagnostic`]; `fields: {}` is allowed for diagnostics without payload.
macro_rules! diagnostics {
    ($(
        $(#[$meta:meta])*
        $name:ident = $code:literal {
            title: $title:literal,
            reference: $reference:expr,
            explain: $explain:literal,
            fields: { $($field:ident: $fty:ty),* $(,)? },
            build: |$d:ident| $build:expr,
        }
    )*) => {
        /// A stable diagnostic identifier. The numeric ranges group related
        /// problems: `E0001..` syntax, `E02xx` name resolution, `E03xx`/`W03xx`
        /// preprocessor, `E09xx` constructs `fcc` does not yet implement.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Code {
            $($name),*
        }

        impl Code {
            pub const ALL: &'static [Code] = &[$(Code::$name),*];

            /// The printable identifier, e.g. `"E0001"`.
            pub fn as_str(self) -> &'static str {
                match self { $(Code::$name => $code),* }
            }

            /// The one-line summary shown as the report message.
            pub fn title(self) -> &'static str {
                match self { $(Code::$name => $title),* }
            }

            /// A standard reference, printed as a `note:`. Section numbers
            /// follow ISO/IEC 9899:2018 (C17) unless stated otherwise.
            pub fn reference(self) -> Option<&'static str> {
                match self { $(Code::$name => $reference),* }
            }

            /// The long-form text shown by `fcc --explain <CODE>`.
            pub fn explanation(self) -> &'static str {
                match self { $(Code::$name => $explain),* }
            }
        }

        $(
            $(#[$meta])*
            pub struct $name {
                $(pub $field: $fty),*
            }

            impl $name {
                // Payload-free diagnostics get an argument-less `new`; that is
                // the intended constructor, not a missing `Default`.
                #[allow(clippy::new_without_default)]
                pub fn new($($field: impl Into<$fty>),*) -> Self {
                    Self { $($field: $field.into()),* }
                }
            }

            impl From<$name> for Diagnostic {
                fn from($d: $name) -> Diagnostic {
                    $build
                }
            }
        )*
    };
}

diagnostics! {
    /// `E0001`: the parser met a token that cannot continue the current rule.
    UnexpectedToken = "E0001" {
        title: "unexpected token",
        reference: Some("C17 6.9: an external declaration must be a function definition or a declaration"),
        explain: "\
The parser reached a token that cannot continue the current grammar rule. This
usually means a missing or stray token: a forgotten semicolon, an unbalanced
brace or parenthesis, or an operator without an operand.

Read the label to see what the parser expected at that point, then add the
missing token or remove the unexpected one.",
        fields: { span: Span, reason: String },
        build: |d| Diagnostic::of(Code::UnexpectedToken)
            .label(d.span, d.reason)
            .help("check for a missing or misplaced token near here"),
    }

    /// `E0002`: input ended while the parser still expected more.
    UnexpectedEof = "E0002" {
        title: "unexpected end of file",
        reference: Some("C17 6.9: an external declaration must be a function definition or a declaration"),
        explain: "\
The source ended while the parser was still expecting more input, for example a
closing brace for a function body or the rest of an unfinished expression.

Make sure every `{`, `(` and statement is closed before the end of the file.",
        fields: { span: Span, reason: String },
        build: |d| Diagnostic::of(Code::UnexpectedEof)
            .label(d.span, d.reason)
            .help("a brace, parenthesis or statement is left unclosed"),
    }

    /// `E0200`: a name is used without any declaration in scope.
    UndeclaredIdentifier = "E0200" {
        title: "use of undeclared identifier",
        reference: Some("C17 6.5.1: an identifier must be visibly declared before it is used"),
        explain: "\
A variable was read or assigned before any declaration introduced it into
scope. C has no implicit declarations: a name must be declared with a type
before it is used.

Declare the variable before the statement that uses it, e.g. `int total = 0;`,
and check the spelling of the identifier.",
        fields: { span: Span, name: String },
        build: |d| Diagnostic::of(Code::UndeclaredIdentifier)
            .message(format!("use of undeclared identifier '{}'", d.name))
            .label(d.span, "not declared in this scope")
            .help(format!("declare '{}' with a type before using it", d.name)),
    }

    /// `E0300`: an active `#error` directive.
    PreprocError = "E0300" {
        title: "#error directive",
        reference: Some("C17 6.10.5: the #error directive renders the program ill-formed"),
        explain: "\
The translation unit contains an active `#error` directive. The preprocessor
emits the directive's text and the program is rejected.

Remove the `#error`, or satisfy the `#if` condition that guards it (often a
missing `-D` define or include path).",
        fields: { span: Span, text: String },
        build: |d| Diagnostic::of(Code::PreprocError)
            .message(directive_message(Code::PreprocError, d.text))
            .label(d.span, "#error directive encountered"),
    }

    /// `W0300`: an active `#warning` directive.
    PreprocWarning = "W0300" {
        title: "#warning directive",
        reference: Some("C23 6.10.6: #warning emits a diagnostic without halting translation"),
        explain: "\
An active `#warning` directive emitted its message. Unlike `#error`, this does
not stop compilation; it flags a condition the author wanted you to notice.

Address the cause described by the message, or remove the directive once it no
longer applies.",
        fields: { span: Span, text: String },
        build: |d| Diagnostic::of(Code::PreprocWarning)
            .message(directive_message(Code::PreprocWarning, d.text))
            .label(d.span, "#warning directive encountered"),
    }

    /// `E0900`: valid C that the code generator does not lower yet.
    UnsupportedConstruct = "E0900" {
        title: "unsupported construct",
        reference: None,
        explain: "\
The construct is valid C but `fcc` does not lower it to IR yet. The frontend
parses a wider language than the code generator currently supports.

Rewrite the function using the supported subset, or pick an earlier `--stage`
(such as `ast`) that does not require code generation.",
        fields: { span: Span, what: String },
        build: |d| Diagnostic::of(Code::UnsupportedConstruct)
            .message(format!("codegen not yet implemented for {}", d.what))
            .label(d.span, "not supported by codegen yet"),
    }

    /// `E0901`: code generation reached a translation unit with no functions.
    EmptyTranslationUnit = "E0901" {
        title: "empty translation unit",
        reference: None,
        explain: "\
Code generation was asked to lower a translation unit that contains no
functions. There is nothing to emit.

Provide at least one function definition in the input.",
        fields: {},
        build: |_d| Diagnostic::of(Code::EmptyTranslationUnit)
            .message("translation unit contains no functions"),
    }
}

impl Code {
    pub fn severity(self) -> Severity {
        if self.as_str().as_bytes()[0] == b'W' {
            Severity::Warning
        } else {
            Severity::Error
        }
    }

    pub fn from_code(code: &str) -> Option<Code> {
        Code::ALL
            .iter()
            .copied()
            .find(|c| c.as_str().eq_ignore_ascii_case(code))
    }
}

/// Message for a `#error`/`#warning`: the directive's own text, or the code's
/// title when the directive carried none.
fn directive_message(code: Code, text: String) -> String {
    if text.is_empty() {
        code.title().to_string()
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// Diagnostic
// ---------------------------------------------------------------------------

/// The rendered form every diagnostic lowers to, built by the catalog's
/// `build` closures. `label` ties the message to a position in a source file;
/// when absent the diagnostic renders as a compact header without a snippet.
#[derive(Debug)]
pub struct Diagnostic {
    code: Code,
    message: String,
    label: Option<(Span, String)>,
    help: Option<String>,
}

impl Diagnostic {
    /// Start a diagnostic for `code`, defaulting the message to its title.
    fn of(code: Code) -> Self {
        Diagnostic {
            code,
            message: code.title().to_string(),
            label: None,
            help: None,
        }
    }

    fn message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    fn label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.label = Some((span, message.into()));
        self
    }

    fn help(mut self, help: impl Into<String>) -> Self {
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

    #[test]
    fn codes_round_trip_and_are_unique() {
        let mut seen = Vec::new();
        for &code in Code::ALL {
            assert_eq!(Code::from_code(code.as_str()), Some(code));
            assert!(!seen.contains(&code.as_str()), "duplicate code string");
            seen.push(code.as_str());
        }
        assert_eq!(Code::from_code("e0001"), Some(Code::UnexpectedToken));
        assert_eq!(Code::from_code("E9999"), None);
    }

    #[test]
    fn severity_follows_code_prefix() {
        for &code in Code::ALL {
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
        let out = render(EmptyTranslationUnit::new().into());
        assert!(out.starts_with("error[E0901]:"), "{out}");
        assert!(out.contains("no functions"), "{out}");
    }

    #[test]
    fn undeclared_identifier_points_at_its_span() {
        let src = "int main(void) { return x; }";
        let file = intern_file("<undeclared-test>", src);
        let at = src.find('x').unwrap();
        let out = render(UndeclaredIdentifier::new(Span::new(file, at), "x").into());
        assert!(out.contains("[E0200]"), "{out}");
        assert!(out.contains("undeclared identifier 'x'"), "{out}");
        assert!(out.contains("not declared in this scope"), "{out}");
        assert!(out.contains("Help"), "{out}");
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
