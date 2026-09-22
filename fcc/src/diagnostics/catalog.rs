//! The whole catalog is one [`diagnostics!`] table. Each row declares a stable
//! [`Code`] (e.g. `E0001`, `W0300`), its title, standard reference and the
//! long-form text shown by `fcc --explain`, plus a concrete builder type
//! (`UnexpectedToken`, `UndeclaredIdentifier`, …) constructed with `new` and
//! converted into a [`Diagnostic`] with `.into()`. Severity is read from the
//! code's first letter (`W` = warning).

use super::{Diagnostic, Span};

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

    /// `E0003`: valid syntax requires a newer selected language standard.
    LanguageFeatureUnavailable = "E0003" {
        title: "language feature unavailable",
        reference: None,
        explain: "\
The source uses syntax that is not part of the selected C language standard.
Select a newer standard with -std, use a GNU dialect when the construct is a
supported extension, or rewrite the construct for the selected standard.",
        fields: { span: Span, feature: String, standard: String },
        build: |d| Diagnostic::of(Code::LanguageFeatureUnavailable)
            .message(format!("{} is unavailable in {}", d.feature, d.standard))
            .label(d.span, format!("{} requires a newer language mode", d.feature))
            .help("select a newer language standard with -std"),
    }

    /// `E0200`: a name is used without any declaration in scope.
    UndeclaredIdentifier = "E0200" {
        title: "use of undeclared identifier",
        reference: None,
        explain: "\
A variable was read or assigned before any declaration introduced it into
scope. C has no implicit declarations: a name must be declared with a type
before it is used.

Declare the variable before the statement that uses it, e.g. `int total = 0;`,
and check the spelling of the identifier.",
        fields: { span: Span, name: String, reference: String },
        build: |d| Diagnostic::of(Code::UndeclaredIdentifier)
            .message(format!("use of undeclared identifier '{}'", d.name))
            .label(d.span, "not declared in this scope")
            .help(format!("declare '{}' with a type before using it", d.name))
            .reference(d.reference),
    }

    /// `E0201`: a declaration introduces a second entity in the same scope.
    Redefinition = "E0201" {
        title: "redefinition",
        reference: None,
        explain: "\
An identifier with no linkage can be declared only once in the same scope.
Rename or remove the second declaration.",
        fields: { span: Span, previous: Span, name: String, reference: String },
        build: |d| Diagnostic::of(Code::Redefinition)
            .message(format!("redefinition of '{}'", d.name))
            .label(d.span, format!("this declaration redefines '{}'", d.name))
            .related(d.previous, "previous declaration is here")
            .reference(d.reference),
    }

    /// `E0202`: declarations of one entity specify incompatible types.
    ConflictingDeclaration = "E0202" {
        title: "conflicting declaration",
        reference: None,
        explain: "Declarations of the same object or function must specify compatible types.",
        fields: { span: Span, previous: Span, name: String, reference: String },
        build: |d| Diagnostic::of(Code::ConflictingDeclaration)
            .message(format!("conflicting declarations for '{}'", d.name))
            .label(d.span, "this declaration has an incompatible type")
            .related(d.previous, "previous declaration is here")
            .reference(d.reference),
    }

    /// `E0203`: a label name is defined twice in one function.
    DuplicateLabel = "E0203" {
        title: "duplicate label",
        reference: None,
        explain: "Label names have function scope and must be unique within a function.",
        fields: { span: Span, previous: Span, name: String, reference: String },
        build: |d| Diagnostic::of(Code::DuplicateLabel)
            .message(format!("duplicate label '{}'", d.name))
            .label(d.span, "label is defined again here")
            .related(d.previous, "previous definition is here")
            .reference(d.reference),
    }

    /// `E0204`: goto names no label in the enclosing function.
    UnknownLabel = "E0204" {
        title: "use of undeclared label",
        reference: None,
        explain: "A goto target must be a label in the same function.",
        fields: { span: Span, name: String, reference: String },
        build: |d| Diagnostic::of(Code::UnknownLabel)
            .message(format!("use of undeclared label '{}'", d.name))
            .label(d.span, "no label with this name exists in the function")
            .reference(d.reference),
    }

    /// `E0402`: an operator's operands do not satisfy its constraints.
    InvalidOperands = "E0402" {
        title: "invalid operands",
        reference: None,
        explain: "\
An operator was applied to values whose types do not satisfy the operator's
constraints. Change an operand or add an appropriate explicit conversion.",
        fields: { span: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::InvalidOperands)
            .message(d.message.clone())
            .label(d.span, d.message)
            .reference(d.reference),
    }

    /// `E0400`: declaration specifiers do not form a C type.
    InvalidTypeSpecifiers = "E0400" {
        title: "invalid type specifiers",
        reference: None,
        explain: "Declaration type specifiers must form one of the combinations permitted by C.",
        fields: { span: Span, spelling: String, reference: String },
        build: |d| Diagnostic::of(Code::InvalidTypeSpecifiers)
            .message(format!("invalid type specifier combination '{}'", d.spelling))
            .label(d.span, "these specifiers do not form a type")
            .reference(d.reference),
    }

    /// `E0401`: an integer constant has an invalid suffix or no representable type.
    InvalidIntegerLiteral = "E0401" {
        title: "invalid integer literal",
        reference: None,
        explain: "An integer suffix must be a valid combination of u/U and l/L or ll/LL.",
        fields: { span: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::InvalidIntegerLiteral)
            .message(d.message.clone())
            .label(d.span, d.message)
            .reference(d.reference),
    }

    /// `E0403`: an operation requires a modifiable lvalue.
    ModifiableLvalueRequired = "E0403" {
        title: "modifiable lvalue required",
        reference: None,
        explain: "\
Assignments and increment or decrement operators can only update an object
designated by a modifiable lvalue.",
        fields: { span: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::ModifiableLvalueRequired)
            .message(d.message.clone())
            .label(d.span, d.message)
            .reference(d.reference),
    }

    /// `E0404`: an implicit assignment conversion is not permitted.
    IncompatibleConversion = "E0404" {
        title: "incompatible conversion",
        reference: None,
        explain: "The source value cannot be implicitly converted to the destination type.",
        fields: { span: Span, previous: Option<Span>, message: String, reference: String },
        build: |d| {
            let diagnostic = Diagnostic::of(Code::IncompatibleConversion)
                .message(d.message.clone())
                .label(d.span, d.message)
                .reference(d.reference);
            if let Some(previous) = d.previous {
                diagnostic.related(previous, "previous declaration is here")
            } else {
                diagnostic
            }
        },
    }

    /// `E0405`: a call designator does not have function type.
    CalledObjectNotFunction = "E0405" {
        title: "called object is not a function",
        reference: None,
        explain: "The expression before a call's parentheses must designate a function.",
        fields: { span: Span, previous: Span, name: String, reference: String },
        build: |d| Diagnostic::of(Code::CalledObjectNotFunction)
            .message(format!("called object '{}' is not a function", d.name))
            .label(d.span, "called here")
            .related(d.previous, "previous declaration is here")
            .reference(d.reference),
    }

    /// `E0406`: a call supplies the wrong number of arguments.
    ArgumentMismatch = "E0406" {
        title: "function argument mismatch",
        reference: None,
        explain: "A function call must supply the parameters required by its declaration.",
        fields: { span: Span, previous: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::ArgumentMismatch)
            .message(d.message.clone())
            .label(d.span, d.message)
            .related(d.previous, "previous declaration is here")
            .reference(d.reference),
    }

    /// `E0407`: a context requires an integer constant expression.
    IntegerConstantRequired = "E0407" {
        title: "integer constant expression required",
        reference: None,
        explain: "This context requires a value computable as an integer during translation.",
        fields: { span: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::IntegerConstantRequired)
            .message(d.message.clone())
            .label(d.span, d.message)
            .reference(d.reference),
    }

    /// `E0408`: a type qualifier is applied where C does not permit it.
    InvalidTypeQualifier = "E0408" {
        title: "invalid type qualifier",
        reference: None,
        explain: "Type qualifiers have constraints based on the type they qualify.",
        fields: { span: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::InvalidTypeQualifier)
            .message(d.message.clone())
            .label(d.span, d.message)
            .reference(d.reference),
    }

    /// `E0409`: an operation requires a complete object type.
    CompleteObjectTypeRequired = "E0409" {
        title: "complete object type required",
        reference: None,
        explain: "Some operations, including sizeof, require a complete object type.",
        fields: { span: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::CompleteObjectTypeRequired)
            .message(d.message.clone())
            .label(d.span, d.message)
            .reference(d.reference),
    }

    /// `E0505`: a return statement does not match its function return type.
    InvalidReturn = "E0505" {
        title: "invalid return statement",
        reference: None,
        explain: "\
A void function cannot return a value, and a function returning a value cannot
use a bare return statement.",
        fields: { span: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::InvalidReturn)
            .message(d.message.clone())
            .label(d.span, d.message)
            .reference(d.reference),
    }

    /// `E0503`: break appears outside a loop or switch.
    MisplacedBreak = "E0503" {
        title: "misplaced break statement",
        reference: None,
        explain: "A break statement can only appear in a loop or switch body.",
        fields: { span: Span, reference: String },
        build: |d| Diagnostic::of(Code::MisplacedBreak)
            .message("break statement is not inside a loop or switch")
            .label(d.span, "no enclosing loop or switch")
            .reference(d.reference),
    }

    /// `E0504`: continue appears outside a loop.
    MisplacedContinue = "E0504" {
        title: "misplaced continue statement",
        reference: None,
        explain: "A continue statement can only appear in a loop body.",
        fields: { span: Span, reference: String },
        build: |d| Diagnostic::of(Code::MisplacedContinue)
            .message("continue statement is not inside a loop")
            .label(d.span, "no enclosing loop")
            .reference(d.reference),
    }

    /// `E0500`: a selection or iteration condition has the wrong type.
    InvalidControllingExpression = "E0500" {
        title: "invalid controlling expression",
        reference: None,
        explain: "Selection and loop conditions require scalar types; switch requires an integer type.",
        fields: { span: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::InvalidControllingExpression)
            .message(d.message.clone())
            .label(d.span, d.message)
            .reference(d.reference),
    }

    /// `E0501`: case or default appears outside a switch.
    MisplacedSwitchLabel = "E0501" {
        title: "misplaced switch label",
        reference: None,
        explain: "A case or default label must appear within a switch statement.",
        fields: { span: Span, reference: String },
        build: |d| Diagnostic::of(Code::MisplacedSwitchLabel)
            .message("case or default label is not inside a switch")
            .label(d.span, "no enclosing switch")
            .reference(d.reference),
    }

    /// `E0502`: a switch repeats a case value or default label.
    DuplicateSwitchLabel = "E0502" {
        title: "duplicate switch label",
        reference: None,
        explain: "One switch cannot contain duplicate converted case values or more than one default.",
        fields: { span: Span, previous: Span, message: String, reference: String },
        build: |d| Diagnostic::of(Code::DuplicateSwitchLabel)
            .message(d.message.clone())
            .label(d.span, d.message)
            .related(d.previous, "previous case is here")
            .reference(d.reference),
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

    /// `E0301`: an `#include` names a file that cannot be found.
    MissingInclude = "E0301" {
        title: "include file not found",
        reference: Some("C17 6.10.2: the named source file is searched for in an implementation-defined manner"),
        explain: "\
An `#include` directive names a header that was not found in any searched
directory. Quoted includes search the including file's directory first, then the
`-I` directories and the system directories; angle includes skip the including
file's directory.

Check the spelling of the header and add the directory holding it with `-I`.",
        fields: { span: Span, path: String },
        build: |d| Diagnostic::of(Code::MissingInclude)
            .message(format!("'{}' file not found", d.path))
            .label(d.span, "file not found"),
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
