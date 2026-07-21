use std::io;

use ariadne::{Color, Config, IndexType, Label, Report, ReportKind, sources};

use crate::Span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub message: String,
    pub label: String,
    pub span: Span,
}

impl Diagnostic {
    pub(crate) fn new(message: impl Into<String>, label: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            label: label.into(),
            span,
        }
    }

    pub fn write(
        &self,
        file_name: &str,
        source: &str,
        writer: &mut impl io::Write,
    ) -> io::Result<()> {
        let file_name = file_name.to_string();
        let range = self.span.into_range();
        Report::build(ReportKind::Error, (file_name.clone(), range.clone()))
            .with_config(Config::new().with_index_type(IndexType::Byte))
            .with_message(&self.message)
            .with_label(
                Label::new((file_name.clone(), range))
                    .with_message(&self.label)
                    .with_color(Color::Red),
            )
            .finish()
            .write(sources([(file_name, source.to_string())]), writer)
    }
}
