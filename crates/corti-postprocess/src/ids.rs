use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

const MAX_IDENTIFIER_BYTES: usize = 512;

/// An invalid content-free identifier supplied at a domain boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IdentifierError {
    #[error("identifier must not be empty")]
    Empty,
    #[error("identifier exceeds {MAX_IDENTIFIER_BYTES} UTF-8 bytes")]
    TooLong,
    #[error("identifier contains a control character")]
    ControlCharacter,
}

fn validate_identifier(value: &str) -> Result<(), IdentifierError> {
    if value.is_empty() {
        return Err(IdentifierError::Empty);
    }
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(IdentifierError::TooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(IdentifierError::ControlCharacter);
    }
    Ok(())
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                validate_identifier(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdentifierError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

string_id!(
    /// Unique identity of one provider call.
    CallId
);
string_id!(
    /// Identity shared by chunk calls that must apply atomically.
    RequestGroupId
);
string_id!(
    /// Stable transcript row identity.
    RowId
);
string_id!(
    /// Optional aggregate target identity carried by provider events.
    TargetId
);
string_id!(
    /// Stable provider identity (for example `openai`).
    ProviderId
);
string_id!(
    /// Stable transport identity (for example `openai_api`).
    TransportId
);
string_id!(
    /// Exact provider model or snapshot identity. No aliases are substituted.
    ModelId
);
string_id!(
    /// Opaque, Corti-local identity for an account/project configuration.
    ConnectionScopeId
);

/// A process incarnation used to reject events from a previous coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessEpoch(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ids_reject_ambiguous_or_log_injecting_values() {
        assert_eq!(CallId::new(""), Err(IdentifierError::Empty));
        assert_eq!(
            ModelId::new("model\nforged"),
            Err(IdentifierError::ControlCharacter)
        );
        assert_eq!(ProviderId::new("openai").unwrap().as_str(), "openai");
    }

    #[test]
    fn deserialization_preserves_validation() {
        assert!(serde_json::from_str::<RowId>("\"row-1\"").is_ok());
        assert!(serde_json::from_str::<RowId>("\"bad\\u0000row\"").is_err());
    }
}
