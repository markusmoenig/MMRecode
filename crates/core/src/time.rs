//! Exact media time representations.

use crate::{Error, Result};

/// Policy used when timestamp rescaling does not produce an integer target value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum TimestampRounding {
    /// Reject a conversion that is not exactly representable in the target time base.
    Exact,
    /// Round toward zero.
    TowardZero,
    /// Round away from zero.
    AwayFromZero,
    /// Round toward negative infinity.
    Floor,
    /// Round toward positive infinity.
    Ceiling,
    /// Round to the nearest integer, resolving exact halves away from zero.
    NearestTiesAway,
}

/// A rational number stored without floating-point loss.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Rational {
    numerator: i64,
    denominator: i64,
}

impl Rational {
    /// Creates a rational number with a non-zero denominator.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidData`] when `denominator` is zero.
    pub fn new(numerator: i64, denominator: i64) -> Result<Self> {
        if denominator == 0 {
            return Err(Error::InvalidData(
                "a rational denominator cannot be zero".into(),
            ));
        }

        if denominator < 0 {
            let numerator = numerator.checked_neg().ok_or_else(|| {
                Error::InvalidData("rational numerator cannot be normalized".into())
            })?;
            let denominator = denominator.checked_neg().ok_or_else(|| {
                Error::InvalidData("rational denominator cannot be normalized".into())
            })?;
            Ok(Self {
                numerator,
                denominator,
            })
        } else {
            Ok(Self {
                numerator,
                denominator,
            })
        }
    }

    /// Returns the numerator.
    #[must_use]
    pub const fn numerator(self) -> i64 {
        self.numerator
    }

    /// Returns the denominator.
    #[must_use]
    pub const fn denominator(self) -> i64 {
        self.denominator
    }
}

/// An integer timestamp paired with an exact time base.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Timestamp {
    /// Timestamp units.
    pub value: i64,
    /// Seconds represented by one timestamp unit.
    pub time_base: Rational,
}

impl Timestamp {
    /// Converts this timestamp to another positive time base with checked integer arithmetic.
    ///
    /// # Errors
    ///
    /// Returns an error when either time base is not positive, intermediate arithmetic overflows,
    /// exact conversion was requested but is impossible, or the result does not fit in `i64`.
    pub fn rescale(self, target: Rational, rounding: TimestampRounding) -> Result<Self> {
        if self.time_base.numerator <= 0 || target.numerator <= 0 {
            return Err(Error::InvalidData(
                "timestamp time bases must be positive".into(),
            ));
        }

        let numerator = i128::from(self.value)
            .checked_mul(i128::from(self.time_base.numerator))
            .and_then(|value| value.checked_mul(i128::from(target.denominator)))
            .ok_or_else(|| Error::InvalidData("timestamp rescale numerator overflows".into()))?;
        let denominator = i128::from(self.time_base.denominator)
            .checked_mul(i128::from(target.numerator))
            .ok_or_else(|| Error::InvalidData("timestamp rescale denominator overflows".into()))?;
        let quotient = numerator / denominator;
        let remainder = numerator % denominator;
        let rounded = round_quotient(quotient, remainder, denominator, rounding)?;
        let value = i64::try_from(rounded)
            .map_err(|_| Error::InvalidData("rescaled timestamp does not fit in i64".into()))?;
        Ok(Self {
            value,
            time_base: target,
        })
    }
}

fn round_quotient(
    quotient: i128,
    remainder: i128,
    denominator: i128,
    rounding: TimestampRounding,
) -> Result<i128> {
    if remainder == 0 {
        return Ok(quotient);
    }
    let direction = if remainder.is_negative() { -1 } else { 1 };
    match rounding {
        TimestampRounding::Exact => Err(Error::InvalidData(
            "timestamp is not exactly representable in the target time base".into(),
        )),
        TimestampRounding::AwayFromZero => quotient
            .checked_add(direction)
            .ok_or_else(|| Error::InvalidData("timestamp rounding overflows".into())),
        TimestampRounding::Floor if remainder.is_negative() => quotient
            .checked_sub(1)
            .ok_or_else(|| Error::InvalidData("timestamp rounding overflows".into())),
        TimestampRounding::Ceiling if remainder.is_positive() => quotient
            .checked_add(1)
            .ok_or_else(|| Error::InvalidData("timestamp rounding overflows".into())),
        TimestampRounding::NearestTiesAway
            if remainder.unsigned_abs().saturating_mul(2) >= denominator.unsigned_abs() =>
        {
            quotient
                .checked_add(direction)
                .ok_or_else(|| Error::InvalidData("timestamp rounding overflows".into()))
        }
        TimestampRounding::TowardZero
        | TimestampRounding::Floor
        | TimestampRounding::Ceiling
        | TimestampRounding::NearestTiesAway => Ok(quotient),
    }
}

#[cfg(test)]
mod tests {
    use super::{Rational, Timestamp, TimestampRounding};

    #[test]
    fn normalizes_denominator_sign() {
        let value = Rational::new(1, -25).unwrap();
        assert_eq!(value.numerator(), -1);
        assert_eq!(value.denominator(), 25);
    }

    #[test]
    fn rejects_unrepresentable_normalization() {
        assert!(Rational::new(i64::MIN, -1).is_err());
        assert!(Rational::new(1, i64::MIN).is_err());
    }

    #[test]
    fn rescales_exact_timestamp() {
        let milliseconds = Rational::new(1, 1_000).unwrap();
        let ninety_khz = Rational::new(1, 90_000).unwrap();
        let value = Timestamp {
            value: 1_500,
            time_base: milliseconds,
        };
        assert_eq!(
            value.rescale(ninety_khz, TimestampRounding::Exact).unwrap(),
            Timestamp {
                value: 135_000,
                time_base: ninety_khz,
            }
        );
    }

    #[test]
    fn applies_explicit_rounding_to_positive_and_negative_values() {
        let thirds = Rational::new(1, 3).unwrap();
        let seconds = Rational::new(1, 1).unwrap();
        let rescale = |value, rounding| {
            Timestamp {
                value,
                time_base: thirds,
            }
            .rescale(seconds, rounding)
            .unwrap()
            .value
        };

        assert_eq!(rescale(2, TimestampRounding::TowardZero), 0);
        assert_eq!(rescale(2, TimestampRounding::AwayFromZero), 1);
        assert_eq!(rescale(2, TimestampRounding::Floor), 0);
        assert_eq!(rescale(2, TimestampRounding::Ceiling), 1);
        assert_eq!(rescale(2, TimestampRounding::NearestTiesAway), 1);
        assert_eq!(rescale(-2, TimestampRounding::TowardZero), 0);
        assert_eq!(rescale(-2, TimestampRounding::AwayFromZero), -1);
        assert_eq!(rescale(-2, TimestampRounding::Floor), -1);
        assert_eq!(rescale(-2, TimestampRounding::Ceiling), 0);
        assert_eq!(rescale(-2, TimestampRounding::NearestTiesAway), -1);
    }

    #[test]
    fn resolves_half_values_away_from_zero() {
        let halves = Rational::new(1, 2).unwrap();
        let seconds = Rational::new(1, 1).unwrap();
        for (source, expected) in [(1, 1), (-1, -1)] {
            let value = Timestamp {
                value: source,
                time_base: halves,
            }
            .rescale(seconds, TimestampRounding::NearestTiesAway)
            .unwrap();
            assert_eq!(value.value, expected);
        }
    }

    #[test]
    fn exact_policy_rejects_fractional_and_invalid_time_bases() {
        let thirds = Rational::new(1, 3).unwrap();
        let seconds = Rational::new(1, 1).unwrap();
        assert!(
            Timestamp {
                value: 1,
                time_base: thirds,
            }
            .rescale(seconds, TimestampRounding::Exact)
            .is_err()
        );
        assert!(
            Timestamp {
                value: 1,
                time_base: Rational::new(0, 1).unwrap(),
            }
            .rescale(seconds, TimestampRounding::TowardZero)
            .is_err()
        );
    }
}
