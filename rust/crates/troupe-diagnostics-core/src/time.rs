use std::{
    fmt,
    time::{Duration, Instant},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::wire::{deserialize_string, parse_canonical_u64};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ElapsedNs(u64);

impl ElapsedNs {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn from_duration(duration: Duration) -> Result<Self, TimeError> {
        u64::try_from(duration.as_nanos())
            .map(Self)
            .map_err(|_| TimeError::ElapsedOverflow)
    }
}

impl Serialize for ElapsedNs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ElapsedNs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, |value| parse_canonical_u64(value).map(Self))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RunClock {
    origin: Instant,
}

impl RunClock {
    pub const fn from_origin(origin: Instant) -> Self {
        Self { origin }
    }

    pub const fn origin(self) -> Instant {
        self.origin
    }

    pub fn elapsed_now(self) -> Result<ElapsedNs, TimeError> {
        self.elapsed_at(Instant::now())
    }

    pub fn elapsed_at(self, observed: Instant) -> Result<ElapsedNs, TimeError> {
        let duration = observed
            .checked_duration_since(self.origin)
            .ok_or(TimeError::BeforeOrigin)?;
        ElapsedNs::from_duration(duration)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeError {
    BeforeOrigin,
    ElapsedOverflow,
}

impl fmt::Display for TimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeOrigin => formatter.write_str("observation precedes the Run origin"),
            Self::ElapsedOverflow => {
                formatter.write_str("elapsed nanoseconds exceed the u64 schema")
            }
        }
    }
}

impl std::error::Error for TimeError {}
