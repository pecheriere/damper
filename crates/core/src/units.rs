//! Parsing of byte sizes and durations as written in the config file.

use std::time::Duration;

use thiserror::Error;

/// Why a size or duration string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UnitError {
    /// The string has no number.
    #[error("missing number")]
    MissingNumber,
    /// The number is not a non-negative integer that fits in 64 bits.
    #[error("invalid number")]
    InvalidNumber,
    /// The suffix is not one of the accepted units.
    #[error("unknown unit")]
    UnknownUnit,
    /// The value does not fit in 64 bits once multiplied by its unit.
    #[error("value too large")]
    Overflow,
}

/// Splits `s` into its leading digits and the trimmed rest.
fn split_number(s: &str) -> Result<(u64, &str), UnitError> {
    let s = s.trim();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, rest) = s.split_at(end);
    if digits.is_empty() {
        return Err(UnitError::MissingNumber);
    }
    let n = digits
        .parse::<u64>()
        .map_err(|_| UnitError::InvalidNumber)?;
    Ok((n, rest.trim()))
}

/// Parses a byte size: `"1073741824"`, `"512MiB"`, `"24GiB"`.
///
/// Units are `B`, `KiB`, `MiB`, `GiB` and `TiB` (binary multiples, case
/// sensitive). A bare number is a count of bytes.
///
/// # Errors
///
/// Returns [`UnitError`] when the string is not a number with an optional
/// known unit, or when the result overflows `u64`.
pub fn parse_bytes(s: &str) -> Result<u64, UnitError> {
    let (n, unit) = split_number(s)?;
    let multiplier: u64 = match unit {
        "" | "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        _ => return Err(UnitError::UnknownUnit),
    };
    n.checked_mul(multiplier).ok_or(UnitError::Overflow)
}

/// Parses a duration: `"90s"`, `"10m"`, `"1h"`.
///
/// A unit is required: `s`, `m` or `h`.
///
/// # Errors
///
/// Returns [`UnitError`] when the string is not a number followed by a known
/// unit, or when the result overflows `u64` seconds.
pub fn parse_duration(s: &str) -> Result<Duration, UnitError> {
    let (n, unit) = split_number(s)?;
    let multiplier: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(UnitError::UnknownUnit),
    };
    n.checked_mul(multiplier)
        .map(Duration::from_secs)
        .ok_or(UnitError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn bytes_examples() {
        assert_eq!(parse_bytes("24GiB"), Ok(25_769_803_776));
        assert_eq!(parse_bytes("512MiB"), Ok(536_870_912));
        assert_eq!(parse_bytes("1073741824"), Ok(1_073_741_824));
        assert_eq!(parse_bytes(" 2 KiB "), Ok(2048));
        assert_eq!(parse_bytes("7B"), Ok(7));
    }

    #[test]
    fn bytes_rejects_garbage() {
        assert_eq!(parse_bytes(""), Err(UnitError::MissingNumber));
        assert_eq!(parse_bytes("lots"), Err(UnitError::MissingNumber));
        assert_eq!(parse_bytes("-1GiB"), Err(UnitError::MissingNumber));
        assert_eq!(parse_bytes("1.5GiB"), Err(UnitError::UnknownUnit));
        assert_eq!(parse_bytes("4GB"), Err(UnitError::UnknownUnit));
        assert_eq!(
            parse_bytes("99999999999999999999"),
            Err(UnitError::InvalidNumber)
        );
        assert_eq!(
            parse_bytes("18446744073709551615GiB"),
            Err(UnitError::Overflow)
        );
    }

    #[test]
    fn duration_examples() {
        assert_eq!(parse_duration("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
    }

    #[test]
    fn duration_rejects_garbage() {
        assert_eq!(parse_duration("10"), Err(UnitError::UnknownUnit));
        assert_eq!(parse_duration("m"), Err(UnitError::MissingNumber));
        assert_eq!(parse_duration("5d"), Err(UnitError::UnknownUnit));
        assert_eq!(
            parse_duration("18446744073709551615h"),
            Err(UnitError::Overflow)
        );
    }

    proptest! {
        #[test]
        fn parse_bytes_round_trips(
            n in any::<u32>(),
            (unit, mult) in prop::sample::select(vec![
                ("B", 1_u64),
                ("KiB", 1 << 10),
                ("MiB", 1 << 20),
                ("GiB", 1 << 30),
            ]),
        ) {
            let expected = u64::from(n).checked_mul(mult).ok_or(UnitError::Overflow);
            prop_assert_eq!(parse_bytes(&format!("{n}{unit}")), expected);
        }
    }
}
