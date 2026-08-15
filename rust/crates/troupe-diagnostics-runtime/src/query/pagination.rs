use std::fmt;

use troupe_diagnostics_core::view_protocol::OpaqueCursor;

const CURSOR_VERSION: &str = "q1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorKey([u8; 32]);

impl CursorKey {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn random() -> Self {
        let first = *uuid::Uuid::new_v4().as_bytes();
        let second = *uuid::Uuid::new_v4().as_bytes();
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(&first);
        bytes[16..].copy_from_slice(&second);
        Self(bytes)
    }
}

#[derive(Clone, Debug)]
pub struct CursorCodec {
    key: CursorKey,
}

impl CursorCodec {
    pub const fn new(key: CursorKey) -> Self {
        Self { key }
    }

    pub fn encode(&self, offset: u64, query_binding: &[u8]) -> OpaqueCursor {
        let fingerprint = hash64(QUERY_FINGERPRINT_DOMAIN, &self.key.0, query_binding);
        let payload = format!("{CURSOR_VERSION}.{offset:016x}.{fingerprint:016x}");
        let tag = hash64(CURSOR_TAG_DOMAIN, &self.key.0, payload.as_bytes());
        OpaqueCursor::parse(&format!("{payload}.{tag:016x}"))
            .expect("Q01 cursor encoding is bounded canonical ASCII")
    }

    pub fn decode(&self, cursor: &OpaqueCursor, query_binding: &[u8]) -> Result<u64, CursorError> {
        let mut parts = cursor.as_str().split('.');
        let version = parts.next().ok_or(CursorError::Malformed)?;
        let offset_text = parts.next().ok_or(CursorError::Malformed)?;
        let fingerprint_text = parts.next().ok_or(CursorError::Malformed)?;
        let tag_text = parts.next().ok_or(CursorError::Malformed)?;
        if parts.next().is_some()
            || version != CURSOR_VERSION
            || offset_text.len() != 16
            || fingerprint_text.len() != 16
            || tag_text.len() != 16
        {
            return Err(CursorError::Malformed);
        }
        let offset = parse_hex(offset_text)?;
        let fingerprint = parse_hex(fingerprint_text)?;
        let tag = parse_hex(tag_text)?;
        let payload = format!("{version}.{offset_text}.{fingerprint_text}");
        let expected_tag = hash64(CURSOR_TAG_DOMAIN, &self.key.0, payload.as_bytes());
        if !constant_time_equal(tag, expected_tag) {
            return Err(CursorError::Tampered);
        }
        let expected_fingerprint = hash64(QUERY_FINGERPRINT_DOMAIN, &self.key.0, query_binding);
        if !constant_time_equal(fingerprint, expected_fingerprint) {
            return Err(CursorError::CrossQuery);
        }
        Ok(offset)
    }
}

impl Default for CursorCodec {
    fn default() -> Self {
        Self::new(CursorKey::random())
    }
}

const QUERY_FINGERPRINT_DOMAIN: u64 = 0x7472_6f75_7065_7131;
const CURSOR_TAG_DOMAIN: u64 = 0x6375_7273_6f72_7131;

fn hash64(domain: u64, key: &[u8; 32], input: &[u8]) -> u64 {
    // A keyed, deterministic cursor authenticator. It is deliberately private to
    // the process and is not a stable wire hash or a query identifier.
    let mut state = 0xcbf2_9ce4_8422_2325_u64 ^ domain;
    for byte in key.iter().copied().chain(input.iter().copied()) {
        state ^= u64::from(byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
        state ^= state.rotate_left(17).wrapping_add(domain);
    }
    state
        ^ u64::try_from(input.len())
            .unwrap_or(u64::MAX)
            .rotate_left(29)
}

fn parse_hex(value: &str) -> Result<u64, CursorError> {
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(CursorError::Malformed);
    }
    u64::from_str_radix(value, 16).map_err(|_| CursorError::Malformed)
}

fn constant_time_equal(left: u64, right: u64) -> bool {
    (left ^ right) == 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorError {
    Malformed,
    Tampered,
    CrossQuery,
}

impl fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Malformed => "opaque cursor is malformed",
            Self::Tampered => "opaque cursor authentication failed",
            Self::CrossQuery => "opaque cursor belongs to a different query binding",
        })
    }
}

impl std::error::Error for CursorError {}
