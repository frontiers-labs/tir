use std::str::FromStr;

use fcc::lang_options::{LangOptions, StdVersion};

#[test]
fn dialect_spellings_select_version_and_extensions() {
    assert_eq!(
        LangOptions::default(),
        LangOptions {
            std_version: StdVersion::C17,
            gnu_extensions: true,
        }
    );
    for (name, version, gnu_extensions) in [
        ("c89", StdVersion::C89, false),
        ("gnu89", StdVersion::C89, true),
        ("c99", StdVersion::C99, false),
        ("gnu99", StdVersion::C99, true),
        ("c11", StdVersion::C11, false),
        ("gnu11", StdVersion::C11, true),
        ("c17", StdVersion::C17, false),
        // c18 is an alias for c17.
        ("c18", StdVersion::C17, false),
        ("gnu17", StdVersion::C17, true),
        ("c23", StdVersion::C23, false),
        ("gnu23", StdVersion::C23, true),
    ] {
        assert_eq!(
            LangOptions::from_str(name).unwrap(),
            LangOptions {
                std_version: version,
                gnu_extensions,
            }
        );
    }
}
