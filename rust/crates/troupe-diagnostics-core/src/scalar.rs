use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::wire::{
    WireValueError, deserialize_string, parse_canonical_u64, validate_canonical_nonnegative_integer,
};

const MAX_NORMALIZED_DECIMAL_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SchemaU64(u64);

impl SchemaU64 {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for SchemaU64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SchemaU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, |value| parse_canonical_u64(value).map(Self))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TokenCount(String);

impl TokenCount {
    pub(crate) fn parse(value: &str) -> Result<Self, WireValueError> {
        validate_canonical_nonnegative_integer(value)?;
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for TokenCount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TokenCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, Self::parse)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DecimalString(String);

impl DecimalString {
    pub(crate) fn parse(value: &str) -> Result<Self, WireValueError> {
        normalize_decimal(value).map(Self)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for DecimalString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DecimalString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, |value| {
            let normalized = Self::parse(value)?;
            if normalized.as_str() != value {
                return Err(WireValueError::new("decimal wire value is not canonical"));
            }
            Ok(normalized)
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CurrencyCode(String);

impl CurrencyCode {
    pub(crate) fn parse(value: &str) -> Result<Self, WireValueError> {
        if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_uppercase()) {
            return Err(WireValueError::new(
                "currency must be three uppercase ASCII letters",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for CurrencyCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CurrencyCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_string(deserializer, Self::parse)
    }
}

fn normalize_decimal(value: &str) -> Result<String, WireValueError> {
    if value.is_empty() || !value.is_ascii() {
        return Err(WireValueError::new("decimal must be nonempty ASCII"));
    }

    let (negative, unsigned) = match value.as_bytes()[0] {
        b'+' => (false, &value[1..]),
        b'-' => (true, &value[1..]),
        _ => (false, value),
    };
    if unsigned.is_empty() {
        return Err(WireValueError::new("decimal has no digits"));
    }

    let exponent_position = unsigned
        .bytes()
        .position(|byte| matches!(byte, b'e' | b'E'));
    let (mantissa, exponent) = match exponent_position {
        Some(position) => {
            let mantissa = &unsigned[..position];
            let exponent_text = &unsigned[position + 1..];
            if exponent_text
                .bytes()
                .any(|byte| matches!(byte, b'e' | b'E'))
            {
                return Err(WireValueError::new("decimal has multiple exponents"));
            }
            (mantissa, parse_exponent(exponent_text)?)
        }
        None => (unsigned, 0),
    };

    let dot_position = mantissa.bytes().position(|byte| byte == b'.');
    if let Some(position) = dot_position
        && mantissa[position + 1..].contains('.')
    {
        return Err(WireValueError::new("decimal has multiple points"));
    }
    let (integer, fraction) = match dot_position {
        Some(position) => (&mantissa[..position], &mantissa[position + 1..]),
        None => (mantissa, ""),
    };
    if integer.is_empty() && fraction.is_empty() {
        return Err(WireValueError::new("decimal has no digits"));
    }
    if !integer
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return Err(WireValueError::new(
            "decimal contains a non-digit character",
        ));
    }

    let mut digits = String::with_capacity(integer.len() + fraction.len());
    digits.push_str(integer);
    digits.push_str(fraction);
    let Some(first_nonzero) = digits.bytes().position(|byte| byte != b'0') else {
        return Ok("0".to_owned());
    };

    let mut coefficient = &digits[first_nonzero..];
    let mut scale = i128::try_from(fraction.len())
        .map_err(|_| WireValueError::new("decimal scale is out of range"))?
        - i128::from(exponent);
    while scale > 0 && coefficient.ends_with('0') {
        coefficient = &coefficient[..coefficient.len() - 1];
        scale -= 1;
    }

    let sign_bytes = usize::from(negative);
    let output_bytes = if scale <= 0 {
        let zeroes = usize::try_from(-scale)
            .map_err(|_| WireValueError::new("decimal exponent is out of range"))?;
        sign_bytes
            .checked_add(coefficient.len())
            .and_then(|length| length.checked_add(zeroes))
    } else {
        let fractional_digits = usize::try_from(scale)
            .map_err(|_| WireValueError::new("decimal scale is out of range"))?;
        if coefficient.len() > fractional_digits {
            sign_bytes
                .checked_add(coefficient.len())
                .and_then(|length| length.checked_add(1))
        } else {
            sign_bytes
                .checked_add(fractional_digits)
                .and_then(|length| length.checked_add(2))
        }
    }
    .ok_or_else(|| WireValueError::new("decimal representation is too large"))?;
    if output_bytes > MAX_NORMALIZED_DECIMAL_BYTES {
        return Err(WireValueError::new("decimal representation is too large"));
    }

    let mut normalized = String::with_capacity(output_bytes);
    if negative {
        normalized.push('-');
    }
    if scale <= 0 {
        normalized.push_str(coefficient);
        for _ in 0..usize::try_from(-scale).expect("validated decimal output length") {
            normalized.push('0');
        }
    } else {
        let fractional_digits = usize::try_from(scale).expect("validated decimal output length");
        if coefficient.len() > fractional_digits {
            let point = coefficient.len() - fractional_digits;
            normalized.push_str(&coefficient[..point]);
            normalized.push('.');
            normalized.push_str(&coefficient[point..]);
        } else {
            normalized.push_str("0.");
            for _ in 0..fractional_digits - coefficient.len() {
                normalized.push('0');
            }
            normalized.push_str(coefficient);
        }
    }
    Ok(normalized)
}

fn parse_exponent(value: &str) -> Result<i64, WireValueError> {
    if value.is_empty() {
        return Err(WireValueError::new("decimal exponent has no digits"));
    }
    let (negative, digits) = match value.as_bytes()[0] {
        b'+' => (false, &value[1..]),
        b'-' => (true, &value[1..]),
        _ => (false, value),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(WireValueError::new("decimal exponent is invalid"));
    }

    let mut magnitude = 0_i64;
    for digit in digits.bytes() {
        magnitude = magnitude
            .checked_mul(10)
            .and_then(|current| current.checked_add(i64::from(digit - b'0')))
            .ok_or_else(|| WireValueError::new("decimal exponent is out of range"))?;
    }
    if negative {
        magnitude
            .checked_neg()
            .ok_or_else(|| WireValueError::new("decimal exponent is out of range"))
    } else {
        Ok(magnitude)
    }
}
