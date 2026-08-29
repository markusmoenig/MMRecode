//! Exact media time representations.

use crate::{Error, Result};

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

#[cfg(test)]
mod tests {
    use super::Rational;

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
}
