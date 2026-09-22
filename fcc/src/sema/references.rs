//! C standard references attached to semantic diagnostics.

use crate::ast::AstKind;
use crate::lang_options::{LangOptions, StdVersion};

pub(super) fn operand_reference(options: LangOptions, kind: AstKind) -> String {
    let (old_clause, new_clause) = match kind {
        AstKind::Mul | AstKind::Div | AstKind::Mod => ("6.5.5p2", "6.5.6p2"),
        AstKind::Add | AstKind::Sub => ("6.5.6p2", "6.5.7p2"),
        AstKind::Shl | AstKind::Shr => ("6.5.7p2", "6.5.8p2"),
        AstKind::Lt | AstKind::Gt | AstKind::Le | AstKind::Ge => ("6.5.8p2", "6.5.9p2"),
        AstKind::Eq | AstKind::Ne => ("6.5.9p2", "6.5.10p2"),
        AstKind::BitAnd => ("6.5.10p2", "6.5.11p2"),
        AstKind::BitXor => ("6.5.11p2", "6.5.12p2"),
        AstKind::BitOr => ("6.5.12p2", "6.5.13p2"),
        AstKind::LogAnd => ("6.5.13p2", "6.5.14p2"),
        AstKind::LogOr => ("6.5.14p2", "6.5.15p2"),
        AstKind::Neg | AstKind::Pos | AstKind::BitNot | AstKind::Not => ("6.5.3.3p1", "6.5.4.3p1"),
        AstKind::AddressOf | AstKind::Deref => ("6.5.3.2p1", "6.5.4.2p1"),
        _ => unreachable!(),
    };
    standard_reference(options, old_clause, new_clause, "operator constraints")
}

pub(super) fn assignment_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.16.1p2", "6.5.17.1p2", "assignment operators")
}

pub(super) fn increment_reference(options: LangOptions, kind: AstKind) -> String {
    let (old, new, title) = match kind {
        AstKind::PreInc | AstKind::PreDec => {
            ("6.5.3.1p1", "6.5.4.1p1", "prefix increment and decrement")
        }
        _ => ("6.5.2.4p1", "6.5.3.5p1", "postfix increment and decrement"),
    };
    standard_reference(options, old, new, title)
}

pub(super) fn simple_assignment_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.16.1p1", "6.5.17.2p1", "simple assignment")
}

pub(super) fn call_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.2.2p2", "6.5.3.3p2", "function calls")
}

pub(super) fn call_designator_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.2.2p1", "6.5.3.3p1", "function calls")
}

pub(super) fn return_reference(options: LangOptions) -> String {
    standard_reference(options, "6.8.6.4p1", "6.8.7.5p1", "return statement")
}

pub(super) fn return_conversion_reference(options: LangOptions) -> String {
    standard_reference(options, "6.8.6.4p3", "6.8.7.5p3", "return statement")
}

pub(super) fn break_reference(options: LangOptions) -> String {
    standard_reference(options, "6.8.6.3p1", "6.8.7.4p1", "break statement")
}

pub(super) fn continue_reference(options: LangOptions) -> String {
    standard_reference(options, "6.8.6.2p1", "6.8.7.3p1", "continue statement")
}

pub(super) fn condition_reference(options: LangOptions, kind: AstKind) -> String {
    let (old, new, title) = match kind {
        AstKind::If => ("6.8.4.1p1", "6.8.5.2p1", "if statement"),
        AstKind::Switch => ("6.8.4.2p1", "6.8.5.3p1", "switch statement"),
        _ => ("6.8.5p2", "6.8.6.1p2", "iteration statements"),
    };
    standard_reference(options, old, new, title)
}

pub(super) fn label_reference(options: LangOptions) -> String {
    standard_reference(options, "6.8.1p3", "6.8.2p3", "labeled statements")
}

pub(super) fn goto_reference(options: LangOptions) -> String {
    standard_reference(options, "6.8.6.1p1", "6.8.7.2p1", "goto statement")
}

pub(super) fn switch_label_reference(options: LangOptions) -> String {
    standard_reference(options, "6.8.1p2", "6.8.2p2", "labeled statements")
}

pub(super) fn switch_case_reference(options: LangOptions) -> String {
    standard_reference(options, "6.8.4.2p3", "6.8.5.3p3", "switch statement")
}

pub(super) fn type_specifier_reference(options: LangOptions) -> String {
    standard_reference(options, "6.7.2p2", "6.7.3.1p2", "type specifiers")
}

pub(super) fn integer_literal_reference(options: LangOptions) -> String {
    standard_reference(options, "6.4.4.1p1", "6.4.4.1p1", "integer constants")
}

pub(super) fn qualifier_reference(options: LangOptions) -> String {
    standard_reference(options, "6.7.3p8", "6.7.4.1p3", "type qualifiers")
}

pub(super) fn initializer_reference(options: LangOptions) -> String {
    standard_reference(options, "6.7.9p11", "6.7.11p12", "initialization")
}

pub(super) fn sizeof_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.3.4p1", "6.5.4.4p2", "sizeof operator")
}

pub(super) fn member_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.2.3p2", "6.5.3.2p2", "structure members")
}

pub(super) fn object_type_reference(options: LangOptions) -> String {
    standard_reference(options, "6.2.5p19", "6.2.5p24", "void type")
}

pub(super) fn cast_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.4p2", "6.5.5p2", "cast operators")
}

pub(super) fn conditional_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.15p2", "6.5.16p2", "conditional operator")
}

pub(super) fn undeclared_reference(options: LangOptions) -> String {
    standard_reference(options, "6.5.1p2", "6.5.2p2", "primary expressions")
}

pub(super) fn redefinition_reference(options: LangOptions) -> String {
    match options.std_version {
        StdVersion::C23 => standard_reference(options, "", "6.7.1p4", "declarations"),
        StdVersion::C17 | StdVersion::C11 | StdVersion::C99 => {
            standard_reference(options, "6.7p3", "", "declarations")
        }
        StdVersion::C89 => "ISO/IEC 9899:1990 6.5p2 — declarations".to_string(),
    }
}

pub(super) fn conflicting_declaration_reference(options: LangOptions) -> String {
    match options.std_version {
        StdVersion::C23 => standard_reference(options, "", "6.7.1p5", "declarations"),
        _ => standard_reference(options, "6.7p4", "", "declarations"),
    }
}

fn standard_reference(
    options: LangOptions,
    old_clause: &str,
    c23_clause: &str,
    title: &str,
) -> String {
    match options.std_version {
        StdVersion::C23 => format!(
            "ISO/IEC 9899:2024 (N3220) {c23_clause} — {title}; https://www.open-std.org/jtc1/sc22/wg14/www/docs/n3220.pdf"
        ),
        StdVersion::C17 => format!(
            "ISO/IEC 9899:2018 (N2176) {old_clause} — {title}; https://www.open-std.org/jtc1/sc22/wg14/www/docs/n2176.pdf"
        ),
        StdVersion::C11 => format!(
            "ISO/IEC 9899:2011 (N1570) {old_clause} — {title}; https://www.open-std.org/jtc1/sc22/wg14/www/docs/n1570.pdf"
        ),
        StdVersion::C99 => format!(
            "ISO/IEC 9899:1999 (N1256) {old_clause} — {title}; https://www.open-std.org/jtc1/sc22/wg14/www/docs/n1256.pdf"
        ),
        StdVersion::C89 => format!("ISO/IEC 9899:1990 {old_clause} — {title}"),
    }
}
