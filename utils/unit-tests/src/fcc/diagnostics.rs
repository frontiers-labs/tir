//! Diagnostic catalog invariants and the renderings no CLI invocation reaches.

use fcc::diagnostics::{Code, Diagnostic, EmptyTranslationUnit, FileId, Severity, Span};

#[test]
fn codes_round_trip_uniquely_with_prefix_severity() {
    let mut seen = Vec::new();
    for &code in Code::ALL {
        assert_eq!(Code::from_code(code.as_str()), Some(code));
        assert!(!seen.contains(&code.as_str()), "duplicate code string");
        seen.push(code.as_str());
        let expected = if code.as_str().starts_with('W') {
            Severity::Warning
        } else {
            Severity::Error
        };
        assert_eq!(code.severity(), expected);
    }
    assert_eq!(Code::from_code("e0001"), Some(Code::UnexpectedToken));
    assert_eq!(Code::from_code("E9999"), None);
}

#[test]
fn span_packs_file_and_offset() {
    let file = fcc::diagnostics::intern_file("<span-test>", "source");
    let span = Span::new(file, 1234);
    assert_eq!(span.file(), file);
    assert_eq!(span.offset(), 1234);

    let default_file = FileId::default();
    let span = Span::new(default_file, 0);
    assert_eq!(span.file(), default_file);
    assert_eq!(span.offset(), 0);
}

/// Codegen no longer raises `E0901` through the driver, so the compact header
/// (a spanless diagnostic without a source frame) is only reachable through
/// the rendering API.
#[test]
fn spanless_diagnostic_renders_compact_header() {
    let diagnostic: Diagnostic = EmptyTranslationUnit::new().into();
    let mut buf = Vec::new();
    diagnostic.write(&mut buf, false).unwrap();
    let out = String::from_utf8(buf).unwrap();
    assert!(out.starts_with("error[E0901]:"), "{out}");
    assert!(out.contains("no functions"), "{out}");
}
