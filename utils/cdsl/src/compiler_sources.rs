//! File and embedded module sources share loading, parsing and source identities.
use crate::error::Error;
use std::{
    fs,
    path::{Path, PathBuf},
};

const SOURCES: &[(&str, &str)] = &[
    ("std.cdsl", include_str!("../stdlib/std.cdsl")),
    ("std/table.cdsl", include_str!("../stdlib/std/table.cdsl")),
    (
        "std/schedule.cdsl",
        include_str!("../stdlib/std/schedule.cdsl"),
    ),
    ("std/option.cdsl", include_str!("../stdlib/std/option.cdsl")),
    ("std/int.cdsl", include_str!("../stdlib/std/int.cdsl")),
    ("std/bits.cdsl", include_str!("../stdlib/std/bits.cdsl")),
    ("std/ir.cdsl", include_str!("../stdlib/std/ir.cdsl")),
    ("std/nat.cdsl", include_str!("../stdlib/std/nat.cdsl")),
    ("std/list.cdsl", include_str!("../stdlib/std/list.cdsl")),
    (
        "std/equality.cdsl",
        include_str!("../stdlib/std/equality.cdsl"),
    ),
];

pub(super) enum Source {
    File { root: PathBuf, file: PathBuf },
    Embedded(PathBuf),
}

impl Source {
    pub fn embedded_std() -> Self {
        Self::Embedded("std.cdsl".into())
    }

    pub fn read(&self) -> Result<(String, PathBuf, PathBuf), Error> {
        match self {
            Self::File { root, file } => {
                let key = file
                    .canonicalize()
                    .map_err(|e| Error(format!("{}: {e}", file.display())))?;
                let text = fs::read_to_string(file)
                    .map_err(|e| Error(format!("{}: {e}", file.display())))?;
                Ok((text, file.strip_prefix(root).unwrap_or(file).into(), key))
            }
            Self::Embedded(file) => {
                let text = SOURCES
                    .iter()
                    .find(|(name, _)| Path::new(name) == file)
                    .ok_or_else(|| Error(format!("missing embedded module {}", file.display())))?
                    .1;
                Ok((text.into(), file.clone(), file.clone()))
            }
        }
    }

    pub fn child(&self, name: &str) -> Result<Self, Error> {
        let file = match self {
            Self::File { file, .. } | Self::Embedded(file) => file,
        };
        let parent = file.parent().unwrap_or(Path::new(""));
        let directory = if file
            .file_name()
            .is_some_and(|n| n == "lib.cdsl" || n == "mod.cdsl")
        {
            parent.to_path_buf()
        } else {
            parent.join(file.file_stem().unwrap_or_default())
        };
        let sibling = directory.join(format!("{name}.cdsl"));
        match self {
            Self::Embedded(_) => Ok(Self::Embedded(sibling)),
            Self::File { root, .. } => {
                let child = directory.join(name).join("mod.cdsl");
                let file = match (child.is_file(), sibling.is_file()) {
                    (true, false) => child,
                    (false, true) => sibling,
                    (true, true) => {
                        return Err(Error(format!(
                            "ambiguous module {name}: {} and {}",
                            child.display(),
                            sibling.display()
                        )));
                    }
                    (false, false) => {
                        return Err(Error(format!(
                            "missing module {name} declared in {}",
                            file.display()
                        )));
                    }
                };
                Ok(Self::File {
                    root: root.clone(),
                    file,
                })
            }
        }
    }
}
