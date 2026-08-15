use std::collections::{BTreeMap, BTreeSet};

use crate::collect::ProjectionError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentitySpace {
    Track,
    Flow,
}

impl IdentitySpace {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Flow => "flow",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DenseIdentityMap {
    ids: BTreeMap<String, u64>,
}

impl DenseIdentityMap {
    pub(crate) fn assign(
        identities: BTreeSet<String>,
        space: IdentitySpace,
        maximum: u64,
    ) -> Result<Self, ProjectionError> {
        let count = u64::try_from(identities.len()).map_err(|_| {
            ProjectionError::identity_exhausted(space, u64::MAX, maximum)
        })?;
        if count > maximum {
            return Err(ProjectionError::identity_exhausted(
                space, count, maximum,
            ));
        }

        let ids = identities
            .into_iter()
            .enumerate()
            .map(|(index, identity)| {
                let id = u64::try_from(index)
                    .ok()
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(|| {
                        ProjectionError::identity_exhausted(space, count, maximum)
                    })?;
                Ok((identity, id))
            })
            .collect::<Result<_, ProjectionError>>()?;
        Ok(Self { ids })
    }

    pub(crate) fn id(&self, identity: &str) -> Result<u64, ProjectionError> {
        self.ids
            .get(identity)
            .copied()
            .ok_or_else(|| ProjectionError::unknown_identity(identity))
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }
}

pub(crate) fn component(value: &str) -> String {
    format!("{}:{value}", value.len())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ExactCounterValue {
    Integer(i64),
    Double(f64),
}

pub(crate) fn exact_counter_value(value: &str) -> Option<ExactCounterValue> {
    if let Ok(integer) = value.parse::<i64>() {
        return Some(ExactCounterValue::Integer(integer));
    }
    exact_finite_double(value).map(ExactCounterValue::Double)
}

fn exact_finite_double(value: &str) -> Option<f64> {
    const MAX_EXACT_DECIMAL_DIGITS: usize = 2_048;
    const MAX_SIGNIFICAND: u64 = (1_u64 << 53) - 1;

    let parsed = value.parse::<f64>().ok()?;
    if !parsed.is_finite() {
        return None;
    }

    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let (integer, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, ""), |parts| parts);
    let mut coefficient = integer
        .bytes()
        .chain(fraction.bytes())
        .skip_while(|digit| *digit == b'0')
        .map(|digit| digit - b'0')
        .collect::<Vec<_>>();
    if coefficient.is_empty() {
        return (parsed == 0.0).then_some(parsed);
    }
    if coefficient.len() > MAX_EXACT_DECIMAL_DIGITS || parsed == 0.0 {
        return None;
    }

    let scale = fraction.len();
    for _ in 0..scale {
        divide_decimal_exact(&mut coefficient, 5)?;
    }

    let mut factors_of_two = 0_i32;
    while coefficient.last().is_some_and(|digit| digit % 2 == 0) {
        divide_decimal_exact(&mut coefficient, 2)
            .expect("an even decimal integer is exactly divisible by two");
        factors_of_two = factors_of_two.checked_add(1)?;
    }
    let odd_significand = decimal_digits_to_u64(&coefficient)?;
    if odd_significand > MAX_SIGNIFICAND {
        return None;
    }

    let significant_bits = u64::BITS - odd_significand.leading_zeros();
    let binary_exponent = factors_of_two.checked_sub(i32::try_from(scale).ok()?)?;
    let highest_exponent = binary_exponent.checked_add(i32::try_from(significant_bits).ok()? - 1)?;
    if binary_exponent < -1_074 || highest_exponent > 1_023 {
        return None;
    }
    Some(parsed)
}

fn divide_decimal_exact(digits: &mut Vec<u8>, divisor: u8) -> Option<()> {
    let mut remainder = 0_u16;
    for digit in digits.iter_mut() {
        let current = remainder * 10 + u16::from(*digit);
        *digit = u8::try_from(current / u16::from(divisor)).ok()?;
        remainder = current % u16::from(divisor);
    }
    if remainder != 0 {
        return None;
    }
    let first_nonzero = digits.iter().position(|digit| *digit != 0);
    match first_nonzero {
        Some(index) if index != 0 => {
            digits.drain(..index);
        }
        None => digits.clear(),
        _ => {}
    }
    Some(())
}

fn decimal_digits_to_u64(digits: &[u8]) -> Option<u64> {
    digits.iter().try_fold(0_u64, |value, digit| {
        value.checked_mul(10)?.checked_add(u64::from(*digit))
    })
}
