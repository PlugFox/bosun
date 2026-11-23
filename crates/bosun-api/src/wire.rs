//! Wire types for integers JSON cannot carry exactly (#12).
//!
//! JSON numbers are exact only up to 2^53 in most clients (JavaScript among them). Values that
//! can exceed it are serialised as decimal strings, and the rule is applied by type, not field
//! by field: a byte count or offset is a [`Wide`], an amount of money is a [`Picodollars`].
//! Token and message counts stay JSON integers: they do not approach 2^53. Timestamps are
//! integer milliseconds UTC.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A 64-bit count that can exceed 2^53 (bytes, offsets, cumulative counters), as a decimal
/// string on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Wide(pub u64);

/// An exact amount of money in picodollars (10^-12 USD), as a decimal string on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Picodollars(pub u128);

impl Picodollars {
    /// The amount in dollars, rounded to a float for display.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // Display only; the exact amount is the integer.
    pub fn usd(self) -> f64 {
        self.0 as f64 / 1e12
    }
}

const DECIMAL: &str = "^(0|[1-9][0-9]*)$";

fn decimal_schema(description: &str) -> Schema {
    json_schema!({
        "type": "string",
        "pattern": DECIMAL,
        "description": description,
    })
}

macro_rules! decimal_string {
    ($ty:ident, $int:ty, $description:literal) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Decimal;

                impl Visitor<'_> for Decimal {
                    type Value = $ty;

                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a non-negative integer as a decimal string")
                    }

                    fn visit_str<E: de::Error>(self, s: &str) -> Result<$ty, E> {
                        let canonical = s == "0"
                            || (!s.is_empty()
                                && !s.starts_with('0')
                                && s.bytes().all(|b| b.is_ascii_digit()));
                        if !canonical {
                            return Err(E::invalid_value(de::Unexpected::Str(s), &self));
                        }
                        s.parse::<$int>()
                            .map($ty)
                            .map_err(|_| E::invalid_value(de::Unexpected::Str(s), &self))
                    }
                }

                deserializer.deserialize_str(Decimal)
            }
        }

        impl JsonSchema for $ty {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($ty))
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                decimal_schema($description)
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

decimal_string!(
    Wide,
    u64,
    "A 64-bit unsigned count that may exceed 2^53, as a decimal string."
);
decimal_string!(
    Picodollars,
    u128,
    "An exact amount of money in picodollars (10^-12 USD), as a decimal string."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_values_are_decimal_strings() {
        let big = Wide(u64::MAX);
        let text = serde_json::to_string(&big).unwrap();
        assert_eq!(text, "\"18446744073709551615\"");
        assert_eq!(serde_json::from_str::<Wide>(&text).unwrap(), big);
        let money = Picodollars(u128::MAX);
        let text = serde_json::to_string(&money).unwrap();
        assert_eq!(serde_json::from_str::<Picodollars>(&text).unwrap(), money);
    }

    #[test]
    fn only_canonical_decimals_are_accepted() {
        for bad in [
            "\"\"",
            "\"01\"",
            "\"-1\"",
            "\"1.0\"",
            "\" 1\"",
            "1",
            "\"18446744073709551616\"",
        ] {
            assert!(serde_json::from_str::<Wide>(bad).is_err(), "{bad}");
        }
        assert_eq!(serde_json::from_str::<Wide>("\"0\"").unwrap(), Wide(0));
    }
}
