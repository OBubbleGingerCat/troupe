#![allow(dead_code)] // D07 wires this private grammar into the application entry point.

use std::{fmt, str::FromStr, time::Duration};

use troupe_diagnostics_core::id::CanonicalUuid;
use troupe_diagnostics_runtime::registry::model::{BindEndpoint, WebBaseUrl};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ValueParseError(&'static str);

impl ValueParseError {
    const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for ValueParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ValueParseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunId(CanonicalUuid);

impl RunId {
    pub(crate) const fn get(self) -> CanonicalUuid {
        self.0
    }
}

impl FromStr for RunId {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        CanonicalUuid::parse(value)
            .map(Self)
            .map_err(|_| ValueParseError::new("Run ID must be a canonical lowercase UUID"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticBaseUrl(WebBaseUrl);

impl DiagnosticBaseUrl {
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub(crate) fn into_inner(self) -> WebBaseUrl {
        self.0
    }
}

impl FromStr for DiagnosticBaseUrl {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        WebBaseUrl::parse(value).map(Self).map_err(|_| {
            ValueParseError::new(
                "URL must be an absolute HTTP(S) base URL without userinfo, query, or fragment",
            )
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BindHost(String);

impl BindHost {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for BindHost {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        BindEndpoint::new(value, 1)
            .map(|endpoint| Self(endpoint.host().to_owned()))
            .map_err(|_| ValueParseError::new("diagnostic bind host is invalid"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalU64(u64);

impl CanonicalU64 {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl FromStr for CanonicalU64 {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_canonical_decimal(value, true).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Count(u64);

impl Count {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl FromStr for Count {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_unsigned_decimal(value, true).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Port(u16);

impl Port {
    pub(crate) const fn get(self) -> u16 {
        self.0
    }
}

impl FromStr for Port {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = parse_unsigned_decimal(value, true)?;
        u16::try_from(value)
            .map(Self)
            .map_err(|_| ValueParseError::new("port must be in 0..=65535"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ByteSize(u64);

impl ByteSize {
    pub(crate) const fn bytes(self) -> u64 {
        self.0
    }
}

impl FromStr for ByteSize {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (digits, multiplier) = parse_suffix(
            value,
            &[
                ("KiB", 1_u64 << 10),
                ("MiB", 1_u64 << 20),
                ("GiB", 1_u64 << 30),
                ("TiB", 1_u64 << 40),
            ],
        );
        let amount = parse_canonical_decimal(digits, false)?;
        amount
            .checked_mul(multiplier)
            .map(Self)
            .ok_or_else(|| ValueParseError::new("byte size exceeds the u64 domain"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeDuration(Duration);

impl RuntimeDuration {
    pub(crate) const fn get(self) -> Duration {
        self.0
    }
}

impl FromStr for RuntimeDuration {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (digits, multiplier_ms) = required_suffix(
            value,
            &[
                ("ms", 1_u64),
                ("s", 1_000_u64),
                ("m", 60_000_u64),
                ("h", 3_600_000_u64),
            ],
            "duration must be a positive integer followed by ms, s, m, or h",
        )?;
        let amount = parse_unsigned_decimal(digits, false)?;
        amount
            .checked_mul(multiplier_ms)
            .map(Duration::from_millis)
            .map(Self)
            .ok_or_else(|| ValueParseError::new("duration exceeds the u64 millisecond domain"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArchiveAge(Duration);

impl ArchiveAge {
    pub(crate) const fn get(self) -> Duration {
        self.0
    }
}

impl FromStr for ArchiveAge {
    type Err = ValueParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (digits, multiplier_seconds) = required_suffix(
            value,
            &[("h", 3_600_u64), ("d", 86_400_u64), ("w", 604_800_u64)],
            "archive age must be a positive integer followed by h, d, or w",
        )?;
        let amount = parse_unsigned_decimal(digits, false)?;
        amount
            .checked_mul(multiplier_seconds)
            .map(Duration::from_secs)
            .map(Self)
            .ok_or_else(|| ValueParseError::new("archive age exceeds the u64 second domain"))
    }
}

fn parse_canonical_decimal(value: &str, allow_zero: bool) -> Result<u64, ValueParseError> {
    if value.len() > 1 && value.starts_with('0') {
        return Err(ValueParseError::new(
            "value must be a canonical unsigned decimal integer",
        ));
    }
    parse_unsigned_decimal(value, allow_zero)
}

fn parse_unsigned_decimal(value: &str, allow_zero: bool) -> Result<u64, ValueParseError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ValueParseError::new(
            "value must be an unsigned decimal integer",
        ));
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|_| ValueParseError::new("value exceeds the unsigned 64-bit integer domain"))?;
    if !allow_zero && parsed == 0 {
        return Err(ValueParseError::new("value must be positive"));
    }
    Ok(parsed)
}

fn parse_suffix<'a>(value: &'a str, units: &[(&str, u64)]) -> (&'a str, u64) {
    units
        .iter()
        .find_map(|(suffix, multiplier)| {
            value
                .strip_suffix(suffix)
                .map(|digits| (digits, *multiplier))
        })
        .unwrap_or((value, 1))
}

fn required_suffix<'a>(
    value: &'a str,
    units: &[(&str, u64)],
    message: &'static str,
) -> Result<(&'a str, u64), ValueParseError> {
    units
        .iter()
        .find_map(|(suffix, multiplier)| {
            value
                .strip_suffix(suffix)
                .map(|digits| (digits, *multiplier))
        })
        .ok_or_else(|| ValueParseError::new(message))
}
