use std::fmt;

use serde::{Deserialize, Deserializer, de};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireValueError(&'static str);

impl WireValueError {
    pub(crate) const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for WireValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for WireValueError {}

pub(crate) fn parse_canonical_u64(value: &str) -> Result<u64, WireValueError> {
    validate_canonical_nonnegative_integer(value)?;
    value
        .parse::<u64>()
        .map_err(|_| WireValueError::new("schema u64 is out of range"))
}

pub(crate) fn validate_canonical_nonnegative_integer(value: &str) -> Result<(), WireValueError> {
    if value.is_empty() {
        return Err(WireValueError::new("integer must not be empty"));
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(WireValueError::new(
            "integer must contain only ASCII decimal digits",
        ));
    }
    if value.len() > 1 && value.starts_with('0') {
        return Err(WireValueError::new(
            "integer must not contain leading zeroes",
        ));
    }
    Ok(())
}

pub(crate) fn deserialize_string<'de, D, T, F>(deserializer: D, parser: F) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    F: FnOnce(&str) -> Result<T, WireValueError>,
{
    let value = String::deserialize(deserializer)?;
    parser(&value).map_err(de::Error::custom)
}
