/// The comparison a `cmpi`, `cmpf` or `ptr.cmp` performs. Each op declares the
/// vocabulary it accepts ([`Predicate::INTEGER`], [`Predicate::FLOAT`],
/// [`Predicate::POINTER`]); a predicate outside it cannot be built or parsed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Predicate {
    Eq,
    Ne,
    Slt,
    Sgt,
    Sle,
    Sge,
    Ult,
    Ugt,
    Ule,
    Uge,
    Oeq,
    Ogt,
    Oge,
    Olt,
    Ole,
    Une,
}

impl Predicate {
    pub const ALL: &'static [Predicate] = &[
        Predicate::Eq,
        Predicate::Ne,
        Predicate::Slt,
        Predicate::Sgt,
        Predicate::Sle,
        Predicate::Sge,
        Predicate::Ult,
        Predicate::Ugt,
        Predicate::Ule,
        Predicate::Uge,
        Predicate::Oeq,
        Predicate::Ogt,
        Predicate::Oge,
        Predicate::Olt,
        Predicate::Ole,
        Predicate::Une,
    ];

    /// Integer comparisons: equality plus the signed and unsigned orderings.
    pub const INTEGER: &'static [Predicate] = &[
        Predicate::Eq,
        Predicate::Ne,
        Predicate::Slt,
        Predicate::Sgt,
        Predicate::Sle,
        Predicate::Sge,
        Predicate::Ult,
        Predicate::Ugt,
        Predicate::Ule,
        Predicate::Uge,
    ];

    /// Floating comparisons: ordered relations plus the unordered-inclusive
    /// inequality C's `!=` needs.
    pub const FLOAT: &'static [Predicate] = &[
        Predicate::Oeq,
        Predicate::Ogt,
        Predicate::Oge,
        Predicate::Olt,
        Predicate::Ole,
        Predicate::Une,
    ];

    /// Addresses are unsigned, so a signed comparison of two of them has no
    /// meaning to declare.
    pub const POINTER: &'static [Predicate] = &[
        Predicate::Eq,
        Predicate::Ne,
        Predicate::Ult,
        Predicate::Ugt,
        Predicate::Ule,
        Predicate::Uge,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Predicate::Eq => "eq",
            Predicate::Ne => "ne",
            Predicate::Slt => "slt",
            Predicate::Sgt => "sgt",
            Predicate::Sle => "sle",
            Predicate::Sge => "sge",
            Predicate::Ult => "ult",
            Predicate::Ugt => "ugt",
            Predicate::Ule => "ule",
            Predicate::Uge => "uge",
            Predicate::Oeq => "oeq",
            Predicate::Ogt => "ogt",
            Predicate::Oge => "oge",
            Predicate::Olt => "olt",
            Predicate::Ole => "ole",
            Predicate::Une => "une",
        }
    }

    pub fn parse(name: &str) -> Option<Predicate> {
        Self::ALL.iter().copied().find(|p| p.name() == name)
    }

    /// The predicate that holds of the swapped operands: `a slt b` is
    /// `b sgt a`.
    pub fn swapped(self) -> Predicate {
        match self {
            Predicate::Eq => Predicate::Eq,
            Predicate::Ne => Predicate::Ne,
            Predicate::Slt => Predicate::Sgt,
            Predicate::Sgt => Predicate::Slt,
            Predicate::Sle => Predicate::Sge,
            Predicate::Sge => Predicate::Sle,
            Predicate::Ult => Predicate::Ugt,
            Predicate::Ugt => Predicate::Ult,
            Predicate::Ule => Predicate::Uge,
            Predicate::Uge => Predicate::Ule,
            Predicate::Oeq => Predicate::Oeq,
            Predicate::Une => Predicate::Une,
            Predicate::Ogt => Predicate::Olt,
            Predicate::Olt => Predicate::Ogt,
            Predicate::Oge => Predicate::Ole,
            Predicate::Ole => Predicate::Oge,
        }
    }

    /// The named vocabulary an op's schema declares, if it names one.
    pub fn vocabulary(name: &str) -> Option<&'static [Predicate]> {
        match name {
            "INTEGER" => Some(Self::INTEGER),
            "FLOAT" => Some(Self::FLOAT),
            "POINTER" => Some(Self::POINTER),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for &p in Predicate::ALL {
            assert_eq!(Predicate::parse(p.name()), Some(p));
        }
        assert_eq!(Predicate::parse("bogus"), None);
    }

    #[test]
    fn swapping_twice_is_the_identity() {
        for &p in Predicate::ALL {
            assert_eq!(p.swapped().swapped(), p);
        }
    }

    #[test]
    fn vocabularies_partition_the_integer_and_float_predicates() {
        assert_eq!(Predicate::INTEGER.len() + Predicate::FLOAT.len(), 16);
        assert!(
            Predicate::INTEGER
                .iter()
                .all(|p| !Predicate::FLOAT.contains(p))
        );
        assert!(
            Predicate::POINTER
                .iter()
                .all(|p| Predicate::INTEGER.contains(p))
        );
    }
}
