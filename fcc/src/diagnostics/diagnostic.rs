use std::io::{self, Write};

use ariadne::{Color, Config, IndexType, Label, Report, ReportKind, sources};

use super::{Code, Severity, Span, file_source};

/// The rendered form every diagnostic lowers to, built by the catalog's
/// `build` closures. `label` ties the message to a position in a source file;
/// when absent the diagnostic renders as a compact header without a snippet.
#[derive(Debug)]
pub struct Diagnostic {
    code: Code,
    message: String,
    labels: Vec<(Span, String)>,
    help: Option<String>,
    reference: Option<String>,
}

impl Diagnostic {
    /// Start a diagnostic for `code`, defaulting the message to its title.
    pub(super) fn of(code: Code) -> Self {
        Diagnostic {
            code,
            message: code.title().to_string(),
            labels: Vec::new(),
            help: None,
            reference: None,
        }
    }

    pub(super) fn message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    pub(super) fn label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push((span, message.into()));
        self
    }

    pub(super) fn related(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push((span, message.into()));
        self
    }

    pub(super) fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub(super) fn reference(mut self, reference: impl Into<String>) -> Self {
        self.reference = Some(reference.into());
        self
    }

    pub fn code(&self) -> Code {
        self.code
    }

    pub fn is_error(&self) -> bool {
        self.code.severity() == Severity::Error
    }

    /// Render to stderr, with color when stderr is a terminal.
    pub fn eprint(&self) {
        let color = io::IsTerminal::is_terminal(&io::stderr());
        let _ = self.write(&mut io::stderr(), color);
    }

    /// Render to an arbitrary writer; `color` toggles ANSI styling (off for
    /// tests and non-terminal output).
    pub fn write(&self, w: &mut dyn Write, color: bool) -> io::Result<()> {
        match self.labels.first() {
            Some((span, _)) => self.write_report(*span, w, color),
            None => self.write_compact(w, color),
        }
    }

    fn write_report(&self, span: Span, w: &mut dyn Write, color: bool) -> io::Result<()> {
        let source = file_source(span.file());
        // Point spans carry only a start; underline the first byte so the caret
        // has something to sit under, clamping at end of file.
        let off = span.offset();
        let range = off..(off + 1).min(source.len()).max(off);

        let (kind, accent) = match self.code.severity() {
            Severity::Error => (ReportKind::Error, Color::Red),
            Severity::Warning => (ReportKind::Warning, Color::Yellow),
        };
        let mut report = Report::build(kind, (span.file(), range.clone()))
            .with_config(
                Config::new()
                    .with_index_type(IndexType::Byte)
                    .with_color(color),
            )
            .with_code(self.code.as_str())
            .with_message(&self.message);
        for (index, (label_span, message)) in self.labels.iter().enumerate() {
            let label_source = file_source(label_span.file());
            let off = label_span.offset();
            let label_range = off..(off + 1).min(label_source.len()).max(off);
            report = report.with_label(
                Label::new((label_span.file(), label_range))
                    .with_message(message)
                    .with_color(if index == 0 { accent } else { Color::Blue }),
            );
        }
        if let Some(help) = &self.help {
            report = report.with_help(help);
        }
        if let Some(reference) = self.reference.as_deref().or_else(|| self.code.reference()) {
            report = report.with_note(reference);
        }
        let mut source_files = Vec::new();
        for (label_span, _) in &self.labels {
            if !source_files.contains(&label_span.file()) {
                source_files.push(label_span.file());
            }
        }
        let cache = sources(
            source_files
                .into_iter()
                .map(|file| (file, file_source(file))),
        );
        report.finish().write(cache, w)
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
        if let Some(reference) = self.reference.as_deref().or_else(|| self.code.reference()) {
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
