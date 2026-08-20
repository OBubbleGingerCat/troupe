use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SortableU64Key([u8; Self::WIDTH]);

impl SortableU64Key {
    pub const WIDTH: usize = 8;

    pub const fn new(value: u64) -> Self {
        Self(value.to_be_bytes())
    }

    pub const fn from_bytes(bytes: [u8; Self::WIDTH]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, StoreKeyError> {
        let bytes: [u8; Self::WIDTH] = bytes.try_into().map_err(|_| StoreKeyError::InvalidWidth)?;
        Ok(Self::from_bytes(bytes))
    }

    pub fn parse_canonical_decimal(value: &str) -> Result<Self, StoreKeyError> {
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(StoreKeyError::NonCanonicalDecimal);
        }
        value
            .parse::<u64>()
            .map(Self::new)
            .map_err(|_| StoreKeyError::OutOfRange)
    }

    pub const fn get(self) -> u64 {
        u64::from_be_bytes(self.0)
    }

    pub const fn as_bytes(&self) -> &[u8; Self::WIDTH] {
        &self.0
    }

    pub const fn into_bytes(self) -> [u8; Self::WIDTH] {
        self.0
    }

    pub fn canonical_decimal(self) -> String {
        self.get().to_string()
    }
}

impl AsRef<[u8]> for SortableU64Key {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreKeyError {
    InvalidWidth,
    NonCanonicalDecimal,
    OutOfRange,
}

impl fmt::Display for StoreKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidWidth => "store u64 key must contain exactly eight bytes",
            Self::NonCanonicalDecimal => "store u64 decimal is not canonical",
            Self::OutOfRange => "store u64 decimal is out of range",
        })
    }
}

impl std::error::Error for StoreKeyError {}
