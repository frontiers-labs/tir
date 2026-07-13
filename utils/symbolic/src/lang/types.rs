use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeVar(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WidthVar(u32);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Width {
    Var(WidthVar),
    Const(u32),
    Add(Box<Width>, Box<Width>),
}

impl Width {
    pub fn constant(value: u32) -> Self {
        Self::Const(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FloatFormat {
    pub exponent: Width,
    pub mantissa: Width,
}

impl FloatFormat {
    pub fn new(exponent: u32, mantissa: u32) -> Self {
        Self {
            exponent: Width::Const(exponent),
            mantissa: Width::Const(mantissa),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SemType {
    Var(TypeVar),
    Bits(Width),
    Float(FloatFormat),
    Iterator(Box<SemType>),
    Pair(Box<SemType>, Box<SemType>),
    RawBits(Width),
    State,
    Unit,
}

impl SemType {
    pub fn bits(width: u32) -> Self {
        Self::Bits(Width::Const(width))
    }

    pub fn raw_bits(width: u32) -> Self {
        Self::RawBits(Width::Const(width))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeError {
    Mismatch(SemType, SemType),
    WidthMismatch(Width, Width),
    Infinite(TypeVar, SemType),
    InfiniteWidth(WidthVar, Width),
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::Mismatch(lhs, rhs) => write!(f, "type mismatch: {lhs:?} vs {rhs:?}"),
            TypeError::WidthMismatch(lhs, rhs) => {
                write!(f, "width mismatch: {lhs:?} vs {rhs:?}")
            }
            TypeError::Infinite(var, ty) => write!(f, "infinite type: {var:?} occurs in {ty:?}"),
            TypeError::InfiniteWidth(var, width) => {
                write!(f, "infinite width: {var:?} occurs in {width:?}")
            }
        }
    }
}

impl std::error::Error for TypeError {}

#[derive(Default)]
pub struct TypeUnifier {
    next_type: u32,
    next_width: u32,
    types: HashMap<TypeVar, SemType>,
    widths: HashMap<WidthVar, Width>,
}

impl TypeUnifier {
    pub(crate) fn fresh_type(&mut self) -> SemType {
        let var = TypeVar(self.next_type);
        self.next_type += 1;
        SemType::Var(var)
    }

    pub(crate) fn fresh_width(&mut self) -> Width {
        let var = WidthVar(self.next_width);
        self.next_width += 1;
        Width::Var(var)
    }

    pub(crate) fn fresh_bits(&mut self) -> SemType {
        SemType::Bits(self.fresh_width())
    }

    pub(crate) fn fresh_float(&mut self) -> SemType {
        SemType::Float(FloatFormat {
            exponent: self.fresh_width(),
            mantissa: self.fresh_width(),
        })
    }

    pub fn unify(&mut self, lhs: &SemType, rhs: &SemType) -> Result<(), TypeError> {
        let lhs = self.resolve(lhs);
        let rhs = self.resolve(rhs);
        match (&lhs, &rhs) {
            (SemType::Var(a), SemType::Var(b)) if a == b => Ok(()),
            (SemType::Var(var), ty) | (ty, SemType::Var(var)) => self.bind_type(*var, ty),
            (SemType::Bits(a), SemType::Bits(b)) | (SemType::RawBits(a), SemType::RawBits(b)) => {
                self.unify_width(a, b)
            }
            (SemType::RawBits(raw), SemType::Bits(bits))
            | (SemType::Bits(bits), SemType::RawBits(raw)) => self.unify_width(raw, bits),
            (SemType::RawBits(raw), SemType::Float(format))
            | (SemType::Float(format), SemType::RawBits(raw)) => self.unify_width(
                raw,
                &Width::Add(
                    Box::new(Width::Const(1)),
                    Box::new(Width::Add(
                        Box::new(format.exponent.clone()),
                        Box::new(format.mantissa.clone()),
                    )),
                ),
            ),
            (SemType::Float(a), SemType::Float(b)) => {
                self.unify_width(&a.exponent, &b.exponent)?;
                self.unify_width(&a.mantissa, &b.mantissa)
            }
            (SemType::Iterator(a), SemType::Iterator(b)) => self.unify(a, b),
            (SemType::Pair(a1, a2), SemType::Pair(b1, b2)) => {
                self.unify(a1, b1)?;
                self.unify(a2, b2)
            }
            (SemType::State, SemType::State) | (SemType::Unit, SemType::Unit) => Ok(()),
            _ => Err(TypeError::Mismatch(lhs, rhs)),
        }
    }

    fn bind_type(&mut self, var: TypeVar, ty: &SemType) -> Result<(), TypeError> {
        if occurs_type(var, ty) {
            return Err(TypeError::Infinite(var, ty.clone()));
        }
        self.types.insert(var, ty.clone());
        Ok(())
    }

    fn unify_width(&mut self, lhs: &Width, rhs: &Width) -> Result<(), TypeError> {
        let lhs = self.resolve_width(lhs);
        let rhs = self.resolve_width(rhs);
        match (&lhs, &rhs) {
            (Width::Var(a), Width::Var(b)) if a == b => Ok(()),
            (Width::Var(var), width) | (width, Width::Var(var)) => {
                if occurs_width(*var, width) {
                    return Err(TypeError::InfiniteWidth(*var, width.clone()));
                }
                self.widths.insert(*var, width.clone());
                Ok(())
            }
            (Width::Const(a), Width::Const(b)) if a == b => Ok(()),
            (Width::Add(a1, a2), Width::Add(b1, b2)) => {
                self.unify_width(a1, b1)?;
                self.unify_width(a2, b2)
            }
            _ => Err(TypeError::WidthMismatch(lhs, rhs)),
        }
    }

    pub fn resolve(&self, ty: &SemType) -> SemType {
        match ty {
            SemType::Var(var) => self
                .types
                .get(var)
                .map(|ty| self.resolve(ty))
                .unwrap_or_else(|| ty.clone()),
            SemType::Bits(width) => SemType::Bits(self.resolve_width(width)),
            SemType::Float(format) => SemType::Float(FloatFormat {
                exponent: self.resolve_width(&format.exponent),
                mantissa: self.resolve_width(&format.mantissa),
            }),
            SemType::Iterator(element) => SemType::Iterator(Box::new(self.resolve(element))),
            SemType::Pair(lhs, rhs) => {
                SemType::Pair(Box::new(self.resolve(lhs)), Box::new(self.resolve(rhs)))
            }
            SemType::RawBits(width) => SemType::RawBits(self.resolve_width(width)),
            SemType::State | SemType::Unit => ty.clone(),
        }
    }

    fn resolve_width(&self, width: &Width) -> Width {
        match width {
            Width::Var(var) => self
                .widths
                .get(var)
                .map(|width| self.resolve_width(width))
                .unwrap_or_else(|| width.clone()),
            Width::Add(lhs, rhs) => {
                let lhs = self.resolve_width(lhs);
                let rhs = self.resolve_width(rhs);
                match (&lhs, &rhs) {
                    (Width::Const(lhs), Width::Const(rhs)) => Width::Const(lhs + rhs),
                    _ => Width::Add(Box::new(lhs), Box::new(rhs)),
                }
            }
            Width::Const(_) => width.clone(),
        }
    }
}

fn occurs_type(var: TypeVar, ty: &SemType) -> bool {
    match ty {
        SemType::Var(other) => *other == var,
        SemType::Iterator(element) => occurs_type(var, element),
        SemType::Pair(lhs, rhs) => occurs_type(var, lhs) || occurs_type(var, rhs),
        _ => false,
    }
}

fn occurs_width(var: WidthVar, width: &Width) -> bool {
    match width {
        Width::Var(other) => *other == var,
        Width::Add(lhs, rhs) => occurs_width(var, lhs) || occurs_width(var, rhs),
        Width::Const(_) => false,
    }
}
