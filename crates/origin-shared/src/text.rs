//! Values written as text in the HTTP API: ids, names, paths, principals
//! and verdicts are JSON strings in the form they display as, and parse
//! from.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{ObjectBuilder, Schema, Type};

use crate::policy::{KindPattern, Principals};
use crate::{
    AcceptanceId, BlobId, ChangeId, Digest, EntryName, Kind, NodePublicKey, ObjectId, Principal,
    PrincipalName, RecordId, RepoPath, RevisionId, StatementId, TargetId, TargetName, TreeId,
    Verdict,
};

/// Serializes each type as the string it displays as, deserializes it by
/// parsing one, and describes it in the OpenAPI document as a string, with
/// an example.
macro_rules! as_text {
    ($($type:ty => $example:expr;)*) => {$(
        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }

        impl utoipa::PartialSchema for $type {
            fn schema() -> RefOr<Schema> {
                ObjectBuilder::new()
                    .schema_type(Type::String)
                    .examples([$example])
                    .into()
            }
        }

        impl utoipa::ToSchema for $type {}
    )*};
}

/// An example of a 32-byte id.
const ID: &str = "48fc721fbbc172e0925fa27af1671de225ba927134802998b10a1568a188652b";

as_text! {
    ObjectId => ID;
    BlobId => ID;
    TreeId => ID;
    RevisionId => ID;
    StatementId => ID;
    AcceptanceId => ID;
    TargetId => ID;
    Digest => ID;
    ChangeId => "3fa2c1d0e9b84f6a7c5d2e1f0a9b8c7d";
    NodePublicKey => "3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29";
    RecordId => format!("revision:{ID}");
    Kind => "ci.test";
    TargetName => "main";
    PrincipalName => "alice";
    EntryName => "main.rs";
    RepoPath => "src/main.rs";
    Principal => "user:alice";
    Verdict => "pass";
    Principals => "group:runners";
    KindPattern => "ci.*";
    Period => "30s";
}

/// A length of time, written as a whole number of seconds (`s`), minutes
/// (`m`), hours (`h`) or days (`d`), such as `90d` or `30s`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Period(pub Duration);

impl Period {
    /// The units, longest first, so a period displays in the largest one
    /// that divides it.
    const UNITS: [(char, u64); 4] = [('d', 24 * 60 * 60), ('h', 60 * 60), ('m', 60), ('s', 1)];
}

impl FromStr for Period {
    type Err = InvalidPeriod;

    fn from_str(period: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidPeriod(period.to_owned());
        let unit = period.chars().last().ok_or_else(invalid)?;
        let &(_, seconds) = Self::UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .ok_or_else(invalid)?;
        let count: u64 = period[..period.len() - 1].parse().map_err(|_| invalid())?;
        let total = count.checked_mul(seconds).ok_or_else(invalid)?;
        Ok(Self(Duration::from_secs(total)))
    }
}

/// In whole seconds, in the largest unit that divides them.
impl fmt::Display for Period {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs();
        let (unit, size) = Self::UNITS
            .into_iter()
            .find(|&(_, size)| seconds.is_multiple_of(size) && seconds >= size)
            .unwrap_or(('s', 1));
        write!(f, "{}{unit}", seconds / size)
    }
}

/// A string that [`Period`] doesn't parse.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a period like `90d`, `12h`, `30m` or `45s`")]
pub struct InvalidPeriod(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_parse_as_they_display() {
        for (text, seconds) in [
            ("90d", 90 * 86_400),
            ("12h", 43_200),
            ("30m", 1800),
            ("45s", 45),
        ] {
            let period: Period = text.parse().unwrap();
            assert_eq!(period.0, Duration::from_secs(seconds));
            assert_eq!(period.to_string(), text);
        }
        assert_eq!(Period(Duration::from_secs(120)).to_string(), "2m");
        assert_eq!(Period(Duration::ZERO).to_string(), "0s");
        for invalid in ["", "d", "5", "5w", "-1s", "1.5h"] {
            assert!(invalid.parse::<Period>().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn values_are_json_strings() {
        let record = RecordId::Revision(ID.parse().unwrap());
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(json, format!("\"revision:{ID}\""));
        assert_eq!(serde_json::from_str::<RecordId>(&json).unwrap(), record);
        assert!(serde_json::from_str::<Kind>("\"Not A Kind\"").is_err());
    }
}
