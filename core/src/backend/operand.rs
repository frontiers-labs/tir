use crate::Error;
use crate::attributes::AttributeValue;

/// The declared domain of a TMDL immediate in its unscaled representation.
#[derive(Clone, Copy, Debug)]
pub struct ImmediateConstraint {
    /// Zero denotes an integer without a declared bit width.
    pub bits: u16,
    pub align: u32,
    pub nonzero: bool,
}

/// Admit signed and unsigned spellings of the same encoded bit pattern.
pub fn check_immediate(
    constraint: ImmediateConstraint,
    value: &AttributeValue,
) -> Result<(), Error> {
    let value = match value {
        AttributeValue::Int(value) => i128::from(*value),
        AttributeValue::UInt(value) => i128::from(*value),
        _ => {
            return Err(Error::VerificationError(
                "expected an integer immediate".into(),
            ));
        }
    };
    let fits = constraint.bits == 0 || constraint.bits >= 64 || {
        let lower = -(1i128 << (constraint.bits - 1));
        let upper = 1i128 << constraint.bits;
        (lower..upper).contains(&value)
    };
    if !fits
        || constraint.align == 0
        || value % i128::from(constraint.align) != 0
        || (constraint.nonzero && value == 0)
    {
        return Err(Error::VerificationError(format!(
            "immediate {value} is outside its declared operand domain"
        )));
    }
    Ok(())
}

/// Check the integer spellings of a text-only operand. Other text can name a
/// symbol, a special register or a grouped operand.
pub fn check_text_immediate(constraint: ImmediateConstraint, text: &str) -> Result<(), Error> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (radix, digits) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
        .map_or((10, digits), |hex| (16, hex));
    if digits.is_empty()
        || !digits.bytes().all(|digit| {
            if radix == 16 {
                digit.is_ascii_hexdigit()
            } else {
                digit.is_ascii_digit()
            }
        })
    {
        return Ok(());
    }
    let out_of_range = || {
        Error::VerificationError(format!(
            "immediate {text} is outside its declared operand domain"
        ))
    };
    let magnitude = u64::from_str_radix(digits, radix).map_err(|_| out_of_range())?;
    let value = if text.starts_with('-') {
        let signed = i64::try_from(-i128::from(magnitude)).map_err(|_| out_of_range())?;
        AttributeValue::Int(signed)
    } else {
        AttributeValue::UInt(magnitude)
    };
    check_immediate(constraint, &value)
}
