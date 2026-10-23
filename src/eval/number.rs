//! The numbers of the expression language.
//!
//! Arithmetic is exact: an [`Exact`] holds the true result of `+ - * / %`
//! anywhere in [−2^127, 2^128 − 1], every value a 128-bit integer of either
//! signedness holds, and refuses results beyond it. Bit operations act on a
//! [`Bits`] pattern and keep its width, because they describe a
//! representation rather than a quantity. A [`Float`] keeps its IEEE format,
//! and mixed formats compute in the widest.

use std::cmp::Ordering;
use std::fmt;

use rustc_apfloat::ieee::{BFloat, Double, Half, Quad, Single, X87DoubleExtended};
use rustc_apfloat::{Float as _, FloatConvert as _, Round};

use crate::FloatValue;

/// The widest integer type the language names.
pub const MAX_WIDTH: u8 = 128;

// A mask shifts a `u128` by its bits less the width.
const _: () = assert!(MAX_WIDTH as u32 <= u128::BITS);

/// Why an operation on numbers has no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberError {
    /// The exact result lies outside [−2^127, 2^128 − 1].
    OutOfRange,

    /// The divisor of `/` or `%` is zero.
    DivisionByZero,

    /// A shift amount is negative, or at least the shifted type's width.
    ShiftAmount,

    /// An exact operand of a bit operation does not fit the typed operand's
    /// width as either a signed or an unsigned value.
    DoesNotFit(IntType),

    /// A NaN has no integer value.
    NotANumber,
}

impl fmt::Display for NumberError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => formatter
                .write_str("the result is outside the integers 128 bits hold, -2^127 to 2^128 - 1"),
            Self::DivisionByZero => formatter.write_str("division by zero"),
            Self::ShiftAmount => {
                formatter.write_str("the shift amount is negative or not less than the width")
            }
            Self::DoesNotFit(ty) => {
                write!(formatter, "the value does not fit in {} bits", ty.width)
            }
            Self::NotANumber => formatter.write_str("NaN has no integer value"),
        }
    }
}

/// An exact integer in [−2^127, 2^128 − 1].
///
/// Zero is never negative, so equal values are equal representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Exact {
    negative: bool,
    magnitude: u128,
}

/// The magnitude of the most negative exact integer, 2^127.
const MOST_NEGATIVE: u128 = 1 << 127;

impl Exact {
    pub const ZERO: Self = Self {
        negative: false,
        magnitude: 0,
    };

    /// The integer with this sign and magnitude, if it is in range.
    const fn new(negative: bool, magnitude: u128) -> Result<Self, NumberError> {
        if negative && magnitude > MOST_NEGATIVE {
            return Err(NumberError::OutOfRange);
        }

        Ok(Self {
            negative: negative && magnitude != 0,
            magnitude,
        })
    }

    pub const fn is_zero(self) -> bool {
        self.magnitude == 0
    }

    pub const fn is_negative(self) -> bool {
        self.negative
    }

    /// The absolute value.
    pub const fn magnitude(self) -> u128 {
        self.magnitude
    }

    pub const fn to_i128(self) -> Option<i128> {
        if self.negative {
            // The magnitude is at most 2^127, whose negation is i128::MIN.
            Some(0_i128.wrapping_sub_unsigned(self.magnitude))
        } else if self.magnitude <= i128::MAX.unsigned_abs() {
            Some(self.magnitude.cast_signed())
        } else {
            None
        }
    }

    pub const fn to_u128(self) -> Option<u128> {
        if self.negative {
            None
        } else {
            Some(self.magnitude)
        }
    }

    pub fn add(self, rhs: Self) -> Result<Self, NumberError> {
        if self.negative == rhs.negative {
            let magnitude = self
                .magnitude
                .checked_add(rhs.magnitude)
                .ok_or(NumberError::OutOfRange)?;
            return Self::new(self.negative, magnitude);
        }

        match self.magnitude.cmp(&rhs.magnitude) {
            Ordering::Less => Self::new(rhs.negative, rhs.magnitude - self.magnitude),
            Ordering::Equal => Ok(Self::ZERO),
            Ordering::Greater => Self::new(self.negative, self.magnitude - rhs.magnitude),
        }
    }

    pub fn sub(self, rhs: Self) -> Result<Self, NumberError> {
        // The negated operand may be out of range though the difference is
        // not, so negate without the range check.
        let negated = Self {
            negative: !rhs.negative && rhs.magnitude != 0,
            magnitude: rhs.magnitude,
        };

        negated.add(self)
    }

    pub fn mul(self, rhs: Self) -> Result<Self, NumberError> {
        let magnitude = self
            .magnitude
            .checked_mul(rhs.magnitude)
            .ok_or(NumberError::OutOfRange)?;
        Self::new(self.negative != rhs.negative, magnitude)
    }

    /// Division truncating toward zero.
    pub const fn div(self, rhs: Self) -> Result<Self, NumberError> {
        if rhs.is_zero() {
            return Err(NumberError::DivisionByZero);
        }

        Self::new(
            self.negative != rhs.negative,
            self.magnitude / rhs.magnitude,
        )
    }

    /// The remainder of truncating division, with the dividend's sign.
    pub const fn rem(self, rhs: Self) -> Result<Self, NumberError> {
        if rhs.is_zero() {
            return Err(NumberError::DivisionByZero);
        }

        Self::new(self.negative, self.magnitude % rhs.magnitude)
    }

    pub const fn neg(self) -> Result<Self, NumberError> {
        Self::new(!self.negative, self.magnitude)
    }

    /// The value as an infinite two's complement pattern: its sign
    /// extension and its low 128 bits.
    const fn twos_complement(self) -> (bool, u128) {
        if self.negative {
            (true, self.magnitude.wrapping_neg())
        } else {
            (false, self.magnitude)
        }
    }

    /// The value an infinite two's complement pattern denotes.
    const fn from_twos_complement(sign: bool, low: u128) -> Result<Self, NumberError> {
        if sign {
            // `low - 2^128`; a zero `low` would be −2^128.
            if low == 0 {
                return Err(NumberError::OutOfRange);
            }

            Self::new(true, low.wrapping_neg())
        } else {
            Self::new(false, low)
        }
    }

    pub const fn not(self) -> Result<Self, NumberError> {
        let (sign, low) = self.twos_complement();
        Self::from_twos_complement(!sign, !low)
    }

    pub const fn bitwise(self, operator: BitOperator, rhs: Self) -> Result<Self, NumberError> {
        let (left_sign, left) = self.twos_complement();
        let (right_sign, right) = rhs.twos_complement();
        let (sign, low) = match operator {
            BitOperator::And => (left_sign & right_sign, left & right),
            BitOperator::Or => (left_sign | right_sign, left | right),
            BitOperator::Xor => (left_sign ^ right_sign, left ^ right),
        };

        Self::from_twos_complement(sign, low)
    }

    /// `self · 2^amount`.
    pub fn shl(self, amount: Self) -> Result<Self, NumberError> {
        let amount = shift_amount(amount)?;
        if self.is_zero() {
            return Ok(self);
        }

        if amount >= 128 || self.magnitude.leading_zeros() < amount {
            return Err(NumberError::OutOfRange);
        }

        Self::new(self.negative, self.magnitude << amount)
    }

    /// `floor(self / 2^amount)`, an arithmetic shift.
    pub fn shr(self, amount: Self) -> Result<Self, NumberError> {
        let amount = shift_amount(amount)?;
        if !self.negative {
            return Self::new(false, self.magnitude.checked_shr(amount).unwrap_or(0));
        }

        if amount >= 128 {
            return Self::new(true, 1);
        }

        // Rounding the magnitude up rounds the value down. The magnitude is
        // at most 2^127, so adding at most 2^127 − 1 cannot overflow.
        let rounded = self.magnitude + ((1 << amount) - 1);
        Self::new(true, rounded >> amount)
    }
}

/// A shift amount, which must not be negative. Amounts beyond `u32` are
/// clamped, since every shift by 128 or more has the same result.
fn shift_amount(amount: Exact) -> Result<u32, NumberError> {
    if amount.negative {
        return Err(NumberError::ShiftAmount);
    }

    Ok(u32::try_from(amount.magnitude).unwrap_or(u32::MAX))
}

impl From<crate::IntegerValue> for Exact {
    fn from(value: crate::IntegerValue) -> Self {
        match value {
            crate::IntegerValue::Signed(value) => Self::from(value),
            crate::IntegerValue::Unsigned(value) => Self::from(value),
        }
    }
}

impl From<i128> for Exact {
    fn from(value: i128) -> Self {
        Self {
            negative: value < 0,
            magnitude: value.unsigned_abs(),
        }
    }
}

impl From<u128> for Exact {
    fn from(value: u128) -> Self {
        Self {
            negative: false,
            magnitude: value,
        }
    }
}

impl From<u64> for Exact {
    fn from(value: u64) -> Self {
        Self::from(u128::from(value))
    }
}

impl Ord for Exact {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, false) => self.magnitude.cmp(&other.magnitude),
            (true, true) => other.magnitude.cmp(&self.magnitude),
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
        }
    }
}

impl PartialOrd for Exact {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Exact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            formatter.write_str("-")?;
        }

        write!(formatter, "{}", self.magnitude)
    }
}

/// A bitwise operator that combines two operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOperator {
    And,
    Or,
    Xor,
}

/// The width and signedness of an integer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IntType {
    width: u8,
    signed: bool,
}

impl IntType {
    /// The 128-bit signed integer type.
    pub const I128: Self = Self {
        width: MAX_WIDTH,
        signed: true,
    };

    /// The type of this width and signedness; widths run from 1 to 128.
    pub const fn new(width: u8, signed: bool) -> Option<Self> {
        if width == 0 || width > MAX_WIDTH {
            return None;
        }

        Some(Self { width, signed })
    }

    pub const fn width(self) -> u8 {
        self.width
    }

    pub const fn is_signed(self) -> bool {
        self.signed
    }

    /// The bits of a pattern of this width.
    const fn mask(self) -> u128 {
        u128::MAX >> (128 - self.width as u32)
    }

    pub const fn min(self) -> Exact {
        if self.signed {
            Exact {
                negative: true,
                magnitude: 1 << (self.width - 1),
            }
        } else {
            Exact::ZERO
        }
    }

    pub fn max(self) -> Exact {
        let bits = if self.signed {
            self.width - 1
        } else {
            self.width
        };

        Exact::from(u128::MAX.checked_shr(128 - u32::from(bits)).unwrap_or(0))
    }

    /// Whether this type holds `value` exactly.
    pub fn contains(self, value: Exact) -> bool {
        self.min() <= value && value <= self.max()
    }

    /// The type two typed operands of a bit operation share: the wider,
    /// and at equal widths the unsigned.
    pub fn common(self, other: Self) -> Self {
        match self.width.cmp(&other.width) {
            Ordering::Less => other,
            Ordering::Greater => self,
            Ordering::Equal => Self {
                width: self.width,
                signed: self.signed && other.signed,
            },
        }
    }
}

/// A two's complement pattern of a typed integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Bits {
    ty: IntType,

    /// The pattern, with every bit above the width clear.
    raw: u128,
}

impl Bits {
    /// The pattern of `value` truncated to `ty`'s width, as casts truncate.
    pub const fn truncate(ty: IntType, value: Exact) -> Self {
        let (_, low) = value.twos_complement();
        Self {
            ty,
            raw: low & ty.mask(),
        }
    }

    /// The pattern of `value` in `ty`, if `ty` holds it exactly.
    pub fn exactly(ty: IntType, value: Exact) -> Result<Self, NumberError> {
        if ty.contains(value) {
            Ok(Self::truncate(ty, value))
        } else {
            Err(NumberError::DoesNotFit(ty))
        }
    }

    /// The pattern of an exact operand meeting a typed one in a bit
    /// operation, which must fit the width as a signed or unsigned value.
    pub fn fitting(ty: IntType, value: Exact) -> Result<Self, NumberError> {
        let signed = IntType {
            width: ty.width,
            signed: true,
        };

        let unsigned = IntType {
            width: ty.width,
            signed: false,
        };

        if signed.contains(value) || unsigned.contains(value) {
            Ok(Self::truncate(ty, value))
        } else {
            Err(NumberError::DoesNotFit(ty))
        }
    }

    /// The pattern in `raw`'s low bits.
    pub const fn from_raw(ty: IntType, raw: u128) -> Self {
        Self {
            ty,
            raw: raw & ty.mask(),
        }
    }

    pub const fn ty(self) -> IntType {
        self.ty
    }

    pub const fn raw(self) -> u128 {
        self.raw
    }

    /// The integer the pattern denotes under its type's signedness.
    pub fn value(self) -> Exact {
        let top = 1_u128 << (self.ty.width - 1);
        if self.ty.signed && self.raw & top != 0 {
            // `raw − 2^width`, whose magnitude is `2^width − raw`.
            Exact {
                negative: true,
                magnitude: (!self.raw & self.ty.mask()) + 1,
            }
        } else {
            Exact::from(self.raw)
        }
    }

    /// The same value converted to `ty`, truncating as a cast does.
    pub fn cast(self, ty: IntType) -> Self {
        Self::truncate(ty, self.value())
    }

    pub const fn not(self) -> Self {
        Self::from_raw(self.ty, !self.raw)
    }

    /// Combines two patterns of one type.
    pub fn bitwise(self, operator: BitOperator, rhs: Self) -> Self {
        debug_assert_eq!(self.ty, rhs.ty);
        let raw = match operator {
            BitOperator::And => self.raw & rhs.raw,
            BitOperator::Or => self.raw | rhs.raw,
            BitOperator::Xor => self.raw ^ rhs.raw,
        };

        Self::from_raw(self.ty, raw)
    }

    fn checked_amount(self, amount: Exact) -> Result<u32, NumberError> {
        let amount = shift_amount(amount)?;
        if amount >= u32::from(self.ty.width) {
            return Err(NumberError::ShiftAmount);
        }

        Ok(amount)
    }

    /// Shifts left, dropping the bits shifted past the width.
    pub fn shl(self, amount: Exact) -> Result<Self, NumberError> {
        let amount = self.checked_amount(amount)?;
        Ok(Self::from_raw(self.ty, self.raw << amount))
    }

    /// Shifts right: arithmetically for a signed type, logically otherwise.
    pub fn shr(self, amount: Exact) -> Result<Self, NumberError> {
        let amount = self.checked_amount(amount)?;
        if self.ty.signed {
            let shifted = self.value().shr(Exact::from(u128::from(amount)))?;
            return Ok(Self::truncate(self.ty, shifted));
        }

        Ok(Self::from_raw(self.ty, self.raw >> amount))
    }
}

/// An integer operand: exact, or a typed pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Integer {
    Exact(Exact),
    Typed(Bits),
}

impl Integer {
    pub fn value(self) -> Exact {
        match self {
            Self::Exact(value) => value,
            Self::Typed(bits) => bits.value(),
        }
    }

    pub fn not(self) -> Result<Self, NumberError> {
        match self {
            Self::Exact(value) => value.not().map(Self::Exact),
            Self::Typed(bits) => Ok(Self::Typed(bits.not())),
        }
    }

    /// `& | ^`: at the width of the widest typed operand, keeping its type,
    /// or in infinite two's complement when both are exact.
    pub fn bitwise(self, operator: BitOperator, rhs: Self) -> Result<Self, NumberError> {
        let (left, right) = match (self, rhs) {
            (Self::Exact(left), Self::Exact(right)) => {
                return left.bitwise(operator, right).map(Self::Exact);
            }
            (Self::Typed(left), Self::Typed(right)) => {
                let ty = left.ty.common(right.ty);
                (left.cast(ty), right.cast(ty))
            }
            (Self::Typed(left), Self::Exact(right)) => (left, Bits::fitting(left.ty, right)?),
            (Self::Exact(left), Self::Typed(right)) => (Bits::fitting(right.ty, left)?, right),
        };

        Ok(Self::Typed(left.bitwise(operator, right)))
    }

    /// `<<`: the shifted operand's type alone decides the result's.
    pub fn shl(self, amount: Exact) -> Result<Self, NumberError> {
        match self {
            Self::Exact(value) => value.shl(amount).map(Self::Exact),
            Self::Typed(bits) => bits.shl(amount).map(Self::Typed),
        }
    }

    /// `>>`: the shifted operand's type alone decides the result's.
    pub fn shr(self, amount: Exact) -> Result<Self, NumberError> {
        match self {
            Self::Exact(value) => value.shr(amount).map(Self::Exact),
            Self::Typed(bits) => bits.shr(amount).map(Self::Typed),
        }
    }
}

/// An IEEE binary floating-point format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FloatFormat {
    Binary16,
    BFloat16,
    Binary32,
    Binary64,
    X87Extended,
    Binary128,
}

impl FloatFormat {
    /// The format of a float type's values, when microscope computes with it.
    pub(crate) const fn of(layout: crate::model::FloatLayout) -> Self {
        use crate::model::FloatLayout;
        match layout {
            FloatLayout::Binary16 => Self::Binary16,
            FloatLayout::BFloat16 => Self::BFloat16,
            FloatLayout::Binary32 => Self::Binary32,
            FloatLayout::Binary64 => Self::Binary64,
            FloatLayout::X87Extended => Self::X87Extended,
            FloatLayout::Binary128 => Self::Binary128,
        }
    }

    /// How many bytes a value of the format takes in memory.
    pub const fn size(self) -> u64 {
        match self {
            Self::Binary16 | Self::BFloat16 => 2,
            Self::Binary32 => 4,
            Self::Binary64 => 8,
            Self::X87Extended | Self::Binary128 => 16,
        }
    }

    /// The format's name in microscope's expressions.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Binary16 => "f16",
            Self::BFloat16 => "bf16",
            Self::Binary32 => "f32",
            Self::Binary64 => "f64",
            Self::X87Extended => "f80",
            Self::Binary128 => "f128",
        }
    }
}

/// A floating-point value in its own format.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Float {
    Binary16(Half),
    BFloat16(BFloat),
    Binary32(Single),
    Binary64(Double),
    X87Extended(X87DoubleExtended),
    Binary128(Quad),
}

/// The format two floats' operations are done in: the wider, and binary32
/// for binary16 with bfloat16, neither of which holds the other's values.
fn wider(left: FloatFormat, right: FloatFormat) -> FloatFormat {
    match (left, right) {
        (FloatFormat::Binary16, FloatFormat::BFloat16)
        | (FloatFormat::BFloat16, FloatFormat::Binary16) => FloatFormat::Binary32,
        _ => left.max(right),
    }
}

/// A floating-point operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatOperator {
    Add,
    Sub,
    Mul,
    Div,

    /// The remainder of truncating division, with the dividend's sign.
    Rem,
}

/// Applies `$body` to the value inside a [`Float`] of any format, binding
/// it to `$value`.
macro_rules! each_format {
    ($float:expr, $value:ident => $body:expr) => {
        match $float {
            Float::Binary16($value) => $body,
            Float::BFloat16($value) => $body,
            Float::Binary32($value) => $body,
            Float::Binary64($value) => $body,
            Float::X87Extended($value) => $body,
            Float::Binary128($value) => $body,
        }
    };
}

impl Float {
    pub fn from_value(value: FloatValue) -> Self {
        match value {
            FloatValue::Binary16(bits) => Self::Binary16(Half::from_bits(u128::from(bits))),
            FloatValue::BFloat16(bits) => Self::BFloat16(BFloat::from_bits(u128::from(bits))),
            FloatValue::Binary128(bits) => Self::Binary128(Quad::from_bits(bits)),
            FloatValue::Binary32(bits) => Self::Binary32(Single::from_bits(u128::from(bits))),
            FloatValue::Binary64(bits) => Self::Binary64(Double::from_bits(u128::from(bits))),
            FloatValue::X87Extended {
                significand,
                sign_exponent,
            } => Self::X87Extended(X87DoubleExtended::from_bits(
                u128::from(sign_exponent) << 64 | u128::from(significand),
            )),
        }
    }

    pub fn to_value(self) -> FloatValue {
        match self {
            // The bit patterns are exactly as wide as the narrowing casts.
            #[allow(clippy::cast_possible_truncation, reason = "binary16 has 16 bits")]
            Self::Binary16(value) => FloatValue::Binary16(value.to_bits() as u16),

            #[allow(clippy::cast_possible_truncation, reason = "bfloat16 has 16 bits")]
            Self::BFloat16(value) => FloatValue::BFloat16(value.to_bits() as u16),
            Self::Binary128(value) => FloatValue::Binary128(value.to_bits()),

            #[allow(clippy::cast_possible_truncation, reason = "binary32 has 32 bits")]
            Self::Binary32(value) => FloatValue::Binary32(value.to_bits() as u32),

            #[allow(clippy::cast_possible_truncation, reason = "binary64 has 64 bits")]
            Self::Binary64(value) => FloatValue::Binary64(value.to_bits() as u64),

            #[allow(clippy::cast_possible_truncation, reason = "x87 values have 80 bits")]
            Self::X87Extended(value) => {
                let bits = value.to_bits();
                FloatValue::X87Extended {
                    significand: bits as u64,
                    sign_exponent: (bits >> 64) as u16,
                }
            }
        }
    }

    pub fn from_f32(value: f32) -> Self {
        Self::Binary32(Single::from_bits(u128::from(value.to_bits())))
    }

    pub fn from_f64(value: f64) -> Self {
        Self::Binary64(Double::from_bits(u128::from(value.to_bits())))
    }

    /// The value as a double, rounded to nearest, ties to even.
    pub fn to_f64(self) -> f64 {
        let value: Double = self.to();

        #[allow(clippy::cast_possible_truncation, reason = "binary64 has 64 bits")]
        f64::from_bits(value.to_bits() as u64)
    }

    pub const fn format(self) -> FloatFormat {
        match self {
            Self::Binary16(_) => FloatFormat::Binary16,
            Self::BFloat16(_) => FloatFormat::BFloat16,
            Self::Binary32(_) => FloatFormat::Binary32,
            Self::Binary64(_) => FloatFormat::Binary64,
            Self::X87Extended(_) => FloatFormat::X87Extended,
            Self::Binary128(_) => FloatFormat::Binary128,
        }
    }

    /// The value in `format`, rounded to nearest, ties to even.
    pub fn convert(self, format: FloatFormat) -> Self {
        self.rounded(format).0
    }

    /// The value in `format`, where it holds it exactly.
    pub fn convert_exactly(self, format: FloatFormat) -> Option<Self> {
        let (converted, loses_info) = self.rounded(format);
        (!loses_info).then_some(converted)
    }

    /// The value in `format`, rounded to nearest, ties to even, and
    /// whether rounding changed it.
    fn rounded(self, format: FloatFormat) -> (Self, bool) {
        let mut loses_info = false;
        let converted = each_format!(self, value => match format {
            FloatFormat::Binary16 => Self::Binary16(value.convert(&mut loses_info).value),
            FloatFormat::BFloat16 => Self::BFloat16(value.convert(&mut loses_info).value),
            FloatFormat::Binary128 => Self::Binary128(value.convert(&mut loses_info).value),
            FloatFormat::Binary32 => Self::Binary32(value.convert(&mut loses_info).value),
            FloatFormat::Binary64 => Self::Binary64(value.convert(&mut loses_info).value),
            FloatFormat::X87Extended => {
                Self::X87Extended(value.convert(&mut loses_info).value)
            }
        });

        (converted, loses_info)
    }

    /// The integer's nearest value in `format`, ties to even.
    pub fn from_exact(value: Exact, format: FloatFormat) -> Self {
        fn convert<F: rustc_apfloat::Float>(value: Exact) -> F {
            let magnitude = F::from_u128_r(value.magnitude, Round::NearestTiesToEven).value;
            if value.negative {
                -magnitude
            } else {
                magnitude
            }
        }

        match format {
            FloatFormat::Binary16 => Self::Binary16(convert(value)),
            FloatFormat::BFloat16 => Self::BFloat16(convert(value)),
            FloatFormat::Binary32 => Self::Binary32(convert(value)),
            FloatFormat::Binary64 => Self::Binary64(convert(value)),
            FloatFormat::X87Extended => Self::X87Extended(convert(value)),
            FloatFormat::Binary128 => Self::Binary128(convert(value)),
        }
    }

    /// Applies an operator in the wider of the two formats.
    pub fn binary(operator: FloatOperator, left: Self, right: Self) -> Self {
        fn apply<F: rustc_apfloat::Float>(operator: FloatOperator, left: F, right: F) -> F {
            let round = Round::NearestTiesToEven;
            match operator {
                FloatOperator::Add => left.add_r(right, round).value,
                FloatOperator::Sub => left.sub_r(right, round).value,
                FloatOperator::Mul => left.mul_r(right, round).value,
                FloatOperator::Div => left.div_r(right, round).value,
                FloatOperator::Rem => left.c_fmod(right).value,
            }
        }

        match wider(left.format(), right.format()) {
            FloatFormat::Binary16 => Self::Binary16(apply(operator, left.to(), right.to())),
            FloatFormat::BFloat16 => Self::BFloat16(apply(operator, left.to(), right.to())),
            FloatFormat::Binary32 => Self::Binary32(apply(operator, left.to(), right.to())),
            FloatFormat::Binary64 => Self::Binary64(apply(operator, left.to(), right.to())),
            FloatFormat::X87Extended => Self::X87Extended(apply(operator, left.to(), right.to())),
            FloatFormat::Binary128 => Self::Binary128(apply(operator, left.to(), right.to())),
        }
    }

    /// The value in the format of `IeeeFloat<S>`, rounded to nearest.
    fn to<S: rustc_apfloat::ieee::Semantics>(self) -> rustc_apfloat::ieee::IeeeFloat<S> {
        let mut loses_info = false;
        each_format!(self, value => value.convert(&mut loses_info).value)
    }

    #[must_use]
    pub fn neg(self) -> Self {
        match self {
            Self::Binary16(value) => Self::Binary16(-value),
            Self::BFloat16(value) => Self::BFloat16(-value),
            Self::Binary32(value) => Self::Binary32(-value),
            Self::Binary64(value) => Self::Binary64(-value),
            Self::X87Extended(value) => Self::X87Extended(-value),
            Self::Binary128(value) => Self::Binary128(-value),
        }
    }

    pub fn is_nan(self) -> bool {
        each_format!(self, value => value.is_nan())
    }

    pub fn is_zero(self) -> bool {
        each_format!(self, value => value.is_zero())
    }

    /// The IEEE order of two floats, in the wider format; `None` for NaN.
    pub fn compare(self, other: Self) -> Option<Ordering> {
        fn compare<F: rustc_apfloat::Float>(left: F, right: F) -> Option<Ordering> {
            left.partial_cmp(&right)
        }

        match wider(self.format(), other.format()) {
            FloatFormat::Binary16 => compare::<Half>(self.to(), other.to()),
            FloatFormat::BFloat16 => compare::<BFloat>(self.to(), other.to()),
            FloatFormat::Binary32 => compare::<Single>(self.to(), other.to()),
            FloatFormat::Binary64 => compare::<Double>(self.to(), other.to()),
            FloatFormat::X87Extended => compare::<X87DoubleExtended>(self.to(), other.to()),
            FloatFormat::Binary128 => compare::<Quad>(self.to(), other.to()),
        }
    }

    /// The value truncated toward zero, saturated to the exact range, and
    /// whether truncation lost a fraction or saturation lost magnitude.
    fn truncated(self) -> Result<(Exact, bool), NumberError> {
        fn truncate<F: rustc_apfloat::Float>(value: F) -> (Exact, bool) {
            let mut is_exact = false;
            let exact = if value.is_negative() {
                Exact::from(value.to_i128_r(128, Round::TowardZero, &mut is_exact).value)
            } else {
                Exact::from(value.to_u128_r(128, Round::TowardZero, &mut is_exact).value)
            };

            // Negative zero converts exactly to zero.
            (exact, is_exact || value.is_zero())
        }

        if self.is_nan() {
            return Err(NumberError::NotANumber);
        }
        Ok(each_format!(self, value => truncate(value)))
    }

    /// The exact order of a float and an integer; `None` for NaN.
    pub fn compare_exact(self, integer: Exact) -> Option<Ordering> {
        let (truncated, is_exact) = self.truncated().ok()?;
        match truncated.cmp(&integer) {
            Ordering::Equal if is_exact => Some(Ordering::Equal),
            // A lost fraction or magnitude lies on the float's side of zero.
            Ordering::Equal => Some(if each_format!(self, value => value.is_negative()) {
                Ordering::Less
            } else {
                Ordering::Greater
            }),
            order => Some(order),
        }
    }

    /// Converts to an integer type as a cast does: truncating toward zero
    /// and saturating at the type's bounds. NaN has no integer value.
    pub fn to_int(self, ty: IntType) -> Result<Bits, NumberError> {
        let (value, _) = self.truncated()?;
        let saturated = value.clamp(ty.min(), ty.max());
        Ok(Bits::truncate(ty, saturated))
    }
}

impl fmt::Display for Float {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        each_format!(self, value => write!(formatter, "{value}"))
    }
}

#[cfg(test)]
mod tests;
