use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StdVersion {
    C89,
    C99,
    C11,
    C17,
    C23,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LangOptions {
    pub std_version: StdVersion,
    pub gnu_extensions: bool,
}

impl LangOptions {
    pub(crate) fn is_strict_c89(self) -> bool {
        self.std_version == StdVersion::C89 && !self.gnu_extensions
    }
}

impl Default for LangOptions {
    fn default() -> Self {
        Self {
            std_version: StdVersion::C17,
            gnu_extensions: true,
        }
    }
}

impl FromStr for LangOptions {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (std_version, gnu_extensions) = match value {
            "c89" | "c90" | "iso9899:1990" => (StdVersion::C89, false),
            "gnu89" | "gnu90" => (StdVersion::C89, true),
            "c99" | "iso9899:1999" => (StdVersion::C99, false),
            "gnu99" => (StdVersion::C99, true),
            "c11" | "iso9899:2011" => (StdVersion::C11, false),
            "gnu11" => (StdVersion::C11, true),
            "c17" | "c18" | "iso9899:2017" | "iso9899:2018" => (StdVersion::C17, false),
            "gnu17" | "gnu18" => (StdVersion::C17, true),
            "c23" | "iso9899:2024" => (StdVersion::C23, false),
            "gnu23" => (StdVersion::C23, true),
            _ => return Err(format!("unsupported C language standard '{value}'")),
        };
        Ok(Self {
            std_version,
            gnu_extensions,
        })
    }
}

impl fmt::Display for LangOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = if self.gnu_extensions { "gnu" } else { "c" };
        let version = match self.std_version {
            StdVersion::C89 => "89",
            StdVersion::C99 => "99",
            StdVersion::C11 => "11",
            StdVersion::C17 => "17",
            StdVersion::C23 => "23",
        };
        write!(f, "{prefix}{version}")
    }
}
