use tir_be_common::SimTrap;

#[derive(Debug)]
pub enum Error {
    ProgramAlreadyLoaded,
    ProgramNotLoaded,
    EntrySymbolNotFound(String),
    MissingSymbolName,
    NoSymbolsFound,
    MissingFallthrough { pc: u64 },
    Trap(SimTrap),
}

impl From<SimTrap> for Error {
    fn from(value: SimTrap) -> Self {
        Self::Trap(value)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProgramAlreadyLoaded => write!(f, "a program is already loaded"),
            Self::ProgramNotLoaded => write!(f, "no program is loaded"),
            Self::EntrySymbolNotFound(name) => write!(f, "entry symbol '{name}' not found"),
            Self::MissingSymbolName => write!(f, "symbol operation has no name attribute"),
            Self::NoSymbolsFound => write!(f, "program contains no symbols"),
            Self::MissingFallthrough { pc } => {
                write!(f, "execution ran off the end of the program at pc=0x{pc:x}")
            }
            Self::Trap(trap) => write!(f, "simulation trap: {trap:?}"),
        }
    }
}

impl std::error::Error for Error {}
