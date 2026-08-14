use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::wire::{WireValueError, deserialize_string};

pub(crate) const MAX_RUN_LOCAL_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CanonicalUuid(Uuid);

impl CanonicalUuid {
    pub(crate) const fn new(value: Uuid) -> Self {
        Self(value)
    }

    pub(crate) fn parse(value: &str) -> Result<Self, WireValueError> {
        let parsed = Uuid::parse_str(value).map_err(|_| WireValueError::new("UUID is invalid"))?;
        if parsed.hyphenated().to_string() != value {
            return Err(WireValueError::new(
                "UUID must use lowercase canonical hyphenated form",
            ));
        }
        Ok(Self(parsed))
    }

    pub(crate) const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for CanonicalUuid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), formatter)
    }
}

impl Serialize for CanonicalUuid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CanonicalUuid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, Self::parse)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RunLocalId(String);

impl RunLocalId {
    pub(crate) fn parse(value: &str) -> Result<Self, WireValueError> {
        if value.is_empty() {
            return Err(WireValueError::new("Run-local ID must not be empty"));
        }
        if value.len() > MAX_RUN_LOCAL_ID_BYTES {
            return Err(WireValueError::new("Run-local ID is too long"));
        }
        if !value.is_ascii() {
            return Err(WireValueError::new("Run-local ID must be ASCII"));
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for RunLocalId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RunLocalId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, Self::parse)
    }
}
