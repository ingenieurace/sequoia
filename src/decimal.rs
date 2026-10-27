//! DECIMAL: exact decimal numbers of up to 38 digits, as a mantissa and a
//! scale, the number of its digits after the point.
//!
//! ```text
//! 19.99      mantissa 1999, scale 2
//! -0.050     mantissa -50,  scale 3
//! 7          mantissa 7,    scale 0
//! ```
//!
//! The mantissa is an `i128` below 10^38 in size, so every value has at
//! most 38 digits, before and after the point together ([`MAX_PRECISION`]).
//! A value keeps its scale, as it prints with it: 1.50 is not 1.5 written
//! differently, though the two are equal.
//!
//! Arithmetic is exact, as in PostgreSQL, but for division, and fails
//! rather than lose digits before the point:
//!
//! ```text
//! a + b, a - b   scale max(sa, sb)
//! a * b          scale sa + sb (rounded to 38 places if more)
//! a / b          scale max(sa, sb) + 6 (at most 38), rounded
//! ```
//!
//! Rounding is half away from zero, as PostgreSQL rounds a NUMERIC.

use std::cmp::Ordering;
use std::fmt;

use crate::error::{Result, err};

/// The most digits a DECIMAL has, before and after the point together.
pub const MAX_PRECISION: u8 = 38;

/// The places a quotient has beyond those of its operands.
const DIVISION_PLACES: u8 = 6;

/// An exact decimal number: `mantissa / 10^scale`.
#[derive(Debug, Clone, Copy)]
pub struct Decimal {
    mantissa: i128,
    scale: u8,
}

/// 10^n, for n up to 38.
fn ten(n: u8) -> i128 {
    10i128.pow(n as u32)
}

fn out_of_range() -> crate::error::Error {
    crate::error::Error("DECIMAL out of range: more than 38 digits".into())
}

impl Decimal {
    /// `mantissa / 10^scale`, if it has at most 38 digits.
    pub fn new(mantissa: i128, scale: u8) -> Result<Decimal> {
        if scale > MAX_PRECISION || mantissa.unsigned_abs() >= ten(MAX_PRECISION) as u128 {
            return Err(out_of_range());
        }
        Ok(Decimal { mantissa, scale })
    }

    pub fn from_int(n: i64) -> Decimal {
        Decimal {
            mantissa: n as i128,
            scale: 0,
        }
    }

    pub fn mantissa(self) -> i128 {
        self.mantissa
    }

    pub fn scale(self) -> u8 {
        self.scale
    }

    /// How many digits it has, at least one: those of its mantissa, or of
    /// its scale if more (0.05 has two).
    pub fn precision(self) -> u8 {
        let digits = self
            .mantissa
            .unsigned_abs()
            .checked_ilog10()
            .map_or(1, |d| d as u8 + 1);
        digits.max(self.scale)
    }

    /// The digits before the point, if it has no more than [`MAX_PRECISION`]
    /// minus `scale` of them: how many a DECIMAL(p, s) lets it have.
    pub fn whole_digits(self) -> u8 {
        let whole = self.mantissa.unsigned_abs() / ten(self.scale) as u128;
        whole.checked_ilog10().map_or(0, |d| d as u8 + 1)
    }

    /// Reads `[+-]digits[.digits][e[+-]digits]`, with digits on at least one
    /// side of the point; spaces around it are ignored. The scale is that
    /// of the digits written, less the exponent.
    pub fn parse(text: &str) -> Result<Decimal> {
        let bad = || err(format!("invalid DECIMAL: '{text}'"));
        let s = text.trim();
        let (negative, s) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let (number, exponent) = match s.find(['e', 'E']) {
            Some(at) => match s[at + 1..].parse::<i32>() {
                Ok(e) => (&s[..at], e),
                Err(_) => return bad(),
            },
            None => (s, 0),
        };
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        if (whole.is_empty() && fraction.is_empty()) || !digits(whole) || !digits(fraction) {
            return bad();
        }
        // The digits, without leading zeros, and the scale they have.
        let all = format!("{whole}{fraction}");
        let all = all.trim_start_matches('0');
        let mut scale = fraction.len() as i64 - exponent as i64;
        let mut mantissa: i128 = 0;
        for b in all.bytes() {
            mantissa = mantissa
                .checked_mul(10)
                .and_then(|m| m.checked_add((b - b'0') as i128))
                .ok_or_else(out_of_range)?;
        }
        // A negative scale: more zeros before the point.
        while scale < 0 {
            mantissa = mantissa.checked_mul(10).ok_or_else(out_of_range)?;
            scale += 1;
        }
        // More places than a DECIMAL holds: rounded to 38.
        let mut value = match u8::try_from(scale) {
            Ok(scale) if scale <= MAX_PRECISION => Decimal { mantissa, scale },
            _ => {
                let drop = scale - MAX_PRECISION as i64;
                let mantissa = match u32::try_from(drop).ok().and_then(|d| 10i128.checked_pow(d)) {
                    Some(divisor) => round_div(mantissa, divisor),
                    None => 0,
                };
                Decimal {
                    mantissa,
                    scale: MAX_PRECISION,
                }
            }
        };
        if negative {
            value.mantissa = -value.mantissa;
        }
        Decimal::new(value.mantissa, value.scale)
    }

    /// The REAL's shortest decimal digits that read back as it, as it
    /// prints: 0.1 is 0.1, not the binary fraction just above it.
    pub fn from_real(r: f64) -> Result<Decimal> {
        Decimal::parse(&format!("{r:?}"))
    }

    /// The nearest REAL.
    pub fn to_real(self) -> f64 {
        self.to_string().parse().expect("digits read as a REAL")
    }

    /// The INT it equals, if it has no fraction and fits in one.
    pub fn to_int(self) -> Option<i64> {
        let divisor = ten(self.scale);
        (self.mantissa % divisor == 0)
            .then(|| i64::try_from(self.mantissa / divisor).ok())
            .flatten()
    }

    /// Rounded to the nearest INT, halves away from zero.
    pub fn round_to_int(self) -> Result<i64> {
        let rounded = round_div(self.mantissa, ten(self.scale));
        i64::try_from(rounded).map_err(|_| crate::error::Error("integer overflow".into()))
    }

    /// The same value with `scale` places: more zeros, or rounded.
    pub fn rescale(self, scale: u8) -> Result<Decimal> {
        match scale.cmp(&self.scale) {
            Ordering::Equal => Ok(self),
            Ordering::Greater => {
                let factor = ten(scale - self.scale);
                let mantissa = self.mantissa.checked_mul(factor).ok_or_else(out_of_range)?;
                Decimal::new(mantissa, scale)
            }
            Ordering::Less => {
                Decimal::new(round_div(self.mantissa, ten(self.scale - scale)), scale)
            }
        }
    }

    /// The same value with `scale` places, if that loses no digit.
    pub fn rescale_exactly(self, scale: u8) -> Option<Decimal> {
        let rescaled = self.rescale(scale).ok()?;
        (rescaled.compare(self) == Ordering::Equal).then_some(rescaled)
    }

    /// The same value without the zeros at the end of its fraction.
    pub fn normalize(self) -> Decimal {
        let (mut mantissa, mut scale) = (self.mantissa, self.scale);
        while scale > 0 && mantissa % 10 == 0 {
            mantissa /= 10;
            scale -= 1;
        }
        Decimal { mantissa, scale }
    }

    /// The largest whole number not above it, and the fraction above that,
    /// as digits after the point without the zeros at their end: 3.140 is
    /// (3, [1, 4]), -0.5 is (-1, [5]).
    pub fn floor_and_fraction(self) -> (i128, Vec<u8>) {
        let divisor = ten(self.scale);
        let floor = self.mantissa.div_euclid(divisor);
        let mut fraction = self.mantissa.rem_euclid(divisor);
        let mut digits = Vec::new();
        let mut place = divisor / 10;
        while fraction > 0 {
            digits.push((fraction / place) as u8);
            fraction %= place;
            place /= 10;
        }
        (floor, digits)
    }

    /// The value `floor + 0.digits` (see [`Decimal::floor_and_fraction`]).
    pub fn from_floor_and_fraction(floor: i128, digits: &[u8]) -> Result<Decimal> {
        let scale = u8::try_from(digits.len()).map_err(|_| out_of_range())?;
        if scale > MAX_PRECISION {
            return Err(out_of_range());
        }
        let fraction = digits.iter().fold(0i128, |n, &d| n * 10 + d as i128);
        let mantissa = floor
            .checked_mul(ten(scale))
            .and_then(|m| m.checked_add(fraction))
            .ok_or_else(out_of_range)?;
        Decimal::new(mantissa, scale)
    }

    pub fn compare(self, other: Decimal) -> Ordering {
        // The whole parts first, then the fractions at the larger scale, so
        // that nothing overflows.
        let (a_floor, b_floor) = (
            self.mantissa.div_euclid(ten(self.scale)),
            other.mantissa.div_euclid(ten(other.scale)),
        );
        a_floor.cmp(&b_floor).then_with(|| {
            let scale = self.scale.max(other.scale);
            let fraction = |d: Decimal| d.mantissa.rem_euclid(ten(d.scale)) * ten(scale - d.scale);
            fraction(self).cmp(&fraction(other))
        })
    }

    pub fn is_zero(self) -> bool {
        self.mantissa == 0
    }

    pub fn neg(self) -> Decimal {
        Decimal {
            mantissa: -self.mantissa,
            scale: self.scale,
        }
    }

    pub fn abs(self) -> Decimal {
        Decimal {
            mantissa: self.mantissa.abs(),
            scale: self.scale,
        }
    }

    pub fn add(self, other: Decimal) -> Result<Decimal> {
        let scale = self.scale.max(other.scale);
        let (a, b) = (self.rescale(scale)?, other.rescale(scale)?);
        Decimal::new(
            a.mantissa
                .checked_add(b.mantissa)
                .ok_or_else(out_of_range)?,
            scale,
        )
    }

    pub fn sub(self, other: Decimal) -> Result<Decimal> {
        self.add(other.neg())
    }

    pub fn mul(self, other: Decimal) -> Result<Decimal> {
        let mantissa = self
            .mantissa
            .checked_mul(other.mantissa)
            .ok_or_else(out_of_range)?;
        let scale = self.scale as u32 + other.scale as u32;
        match scale > MAX_PRECISION as u32 {
            true => Decimal::new(
                round_div(mantissa, 10i128.pow(scale - MAX_PRECISION as u32)),
                MAX_PRECISION,
            ),
            false => Decimal::new(mantissa, scale as u8),
        }
    }

    /// The quotient, rounded to `max(sa, sb) + 6` places (fewer if the
    /// dividend's digits would then overflow); an error for division by zero.
    pub fn div(self, other: Decimal) -> Result<Decimal> {
        if other.is_zero() {
            return err("division by zero");
        }
        let target = (self.scale.max(other.scale) + DIVISION_PLACES).min(MAX_PRECISION);
        // mantissa_a 10^k / mantissa_b has scale sa - sb + k: as many places
        // as wanted, or as fit.
        let mut k = target as i32 - self.scale as i32 + other.scale as i32;
        loop {
            let quotient = || {
                let numerator = self.mantissa.checked_mul(10i128.checked_pow(k as u32)?)?;
                let quotient = round_div(numerator, other.mantissa);
                let scale = self.scale as i32 - other.scale as i32 + k;
                match u8::try_from(scale) {
                    Ok(scale) => Decimal::new(quotient, scale).ok(),
                    // A whole quotient with zeros to add before the point.
                    Err(_) => {
                        let factor = 10i128.checked_pow((-scale) as u32)?;
                        Decimal::new(quotient.checked_mul(factor)?, 0).ok()
                    }
                }
            };
            match quotient() {
                Some(quotient) => return Ok(quotient),
                None if k > 0 => k -= 1,
                None => return Err(out_of_range()),
            }
        }
    }

    /// Rounded to `places` places (none if negative), halves away from zero.
    pub fn round(self, places: i64) -> Result<Decimal> {
        let places = places.clamp(0, MAX_PRECISION as i64) as u8;
        match places >= self.scale {
            true => Ok(self),
            false => self.rescale(places),
        }
    }
}

/// `a / b` rounded to the nearest whole number, halves away from zero.
fn round_div(a: i128, b: i128) -> i128 {
    let (quotient, remainder) = (a / b, a % b);
    if remainder.unsigned_abs() * 2 >= b.unsigned_abs() {
        quotient + if (a < 0) == (b < 0) { 1 } else { -1 }
    } else {
        quotient
    }
}

impl fmt::Display for Decimal {
    /// Its digits, with as many after the point as its scale: `-0.050`.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let digits = self.mantissa.unsigned_abs().to_string();
        let sign = if self.mantissa < 0 { "-" } else { "" };
        let scale = self.scale as usize;
        if scale == 0 {
            return write!(f, "{sign}{digits}");
        }
        let digits = format!("{digits:0>width$}", width = scale + 1);
        let (whole, fraction) = digits.split_at(digits.len() - scale);
        write!(f, "{sign}{whole}.{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(text: &str) -> Decimal {
        Decimal::parse(text).unwrap()
    }

    #[test]
    fn parses_and_prints_with_its_scale() {
        for text in [
            "0",
            "1.50",
            "-0.050",
            "19.99",
            "123456789012345678901234567890.12345678",
        ] {
            assert_eq!(d(text).to_string(), text);
        }
        assert_eq!(d("+.5").to_string(), "0.5");
        assert_eq!(d("5.").to_string(), "5");
        assert_eq!(d(" 007 ").to_string(), "7");
        assert_eq!(d("1.5e2").to_string(), "150");
        assert_eq!(d("15e-3").to_string(), "0.015");
        assert_eq!(d("-0").to_string(), "0");
        for bad in ["", ".", "1.2.3", "abc", "1e", "--1", "1 2"] {
            assert!(Decimal::parse(bad).is_err(), "{bad}");
        }
        assert!(Decimal::parse(&"9".repeat(39)).is_err());
        assert_eq!(d(&"9".repeat(38)).precision(), 38);
        // More places than 38 are rounded.
        assert_eq!(
            d(&format!("0.{}5", "0".repeat(38))).to_string(),
            format!("0.{}1", "0".repeat(37))
        );
    }

    #[test]
    fn compares_by_value_across_scales() {
        assert_eq!(d("1.5").compare(d("1.50")), Ordering::Equal);
        assert_eq!(d("-0.5").compare(d("-0.25")), Ordering::Less);
        assert_eq!(d("2").compare(d("1.999")), Ordering::Greater);
        let big = d(&"9".repeat(38));
        let small = d(&format!("0.{}", "9".repeat(38)));
        assert_eq!(big.compare(small), Ordering::Greater);
        assert_eq!(small.neg().compare(big.neg()), Ordering::Greater);
    }

    #[test]
    fn arithmetic_is_exact() {
        assert_eq!(d("0.1").add(d("0.2")).unwrap().to_string(), "0.3");
        assert_eq!(d("1.50").sub(d("2")).unwrap().to_string(), "-0.50");
        assert_eq!(d("1.5").mul(d("-0.25")).unwrap().to_string(), "-0.375");
        assert_eq!(d("1").div(d("3")).unwrap().to_string(), "0.333333");
        assert_eq!(d("2.00").div(d("3")).unwrap().to_string(), "0.66666667");
        assert_eq!(d("-1").div(d("8")).unwrap().to_string(), "-0.125000");
        assert!(d("1").div(d("0")).is_err());
        let max = d(&"9".repeat(38));
        assert!(max.add(d("1")).is_err());
        assert!(max.mul(d("10")).is_err());
        assert_eq!(max.div(d("9")).unwrap().to_string(), "1".repeat(38));
        // Fewer places, for the digits before the point to fit.
        assert_eq!(
            d("1e30").div(d("0.001")).unwrap().to_string(),
            format!("1{}.0000", "0".repeat(33))
        );
    }

    #[test]
    fn rounds_half_away_from_zero() {
        assert_eq!(d("2.675").round(2).unwrap().to_string(), "2.68");
        assert_eq!(d("-2.5").round(0).unwrap().to_string(), "-3");
        assert_eq!(d("1.234").round(5).unwrap().to_string(), "1.234");
        assert_eq!(d("1.25").rescale(1).unwrap().to_string(), "1.3");
        assert_eq!(d("1.2").rescale(3).unwrap().to_string(), "1.200");
        assert_eq!(d("1.25").rescale_exactly(1).map(|d| d.to_string()), None);
        assert_eq!(
            d("1.20").rescale_exactly(1).map(|d| d.to_string()),
            Some("1.2".into())
        );
        assert_eq!(d("-7.5").round_to_int().unwrap(), -8);
        assert_eq!(d("12.000").to_int(), Some(12));
        assert_eq!(d("12.5").to_int(), None);
    }

    #[test]
    fn splits_into_a_floor_and_the_digits_of_a_fraction() {
        assert_eq!(d("3.140").floor_and_fraction(), (3, vec![1, 4]));
        assert_eq!(d("-0.5").floor_and_fraction(), (-1, vec![5]));
        assert_eq!(d("-2").floor_and_fraction(), (-2, vec![]));
        for text in ["3.14", "-0.5", "-2", "0.000001", "-123.456"] {
            let (floor, digits) = d(text).floor_and_fraction();
            let back = Decimal::from_floor_and_fraction(floor, &digits).unwrap();
            assert_eq!(back.compare(d(text)), Ordering::Equal, "{text}");
        }
    }

    #[test]
    fn converts_to_and_from_reals_by_their_shortest_digits() {
        assert_eq!(Decimal::from_real(0.1).unwrap().to_string(), "0.1");
        assert_eq!(
            Decimal::from_real(1e20).unwrap().to_string(),
            "100000000000000000000"
        );
        assert_eq!(Decimal::from_real(-2.5e-3).unwrap().to_string(), "-0.0025");
        assert_eq!(d("0.1").to_real(), 0.1);
        assert_eq!(d("123.456").to_real(), 123.456);
    }
}
