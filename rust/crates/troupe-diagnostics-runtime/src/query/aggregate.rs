use std::{cmp::Ordering, fmt};

use num_bigint::BigInt;
use troupe_diagnostics_core::{
    detail::CanonicalInteger,
    scalar::{DecimalString, SchemaU64, TokenCount},
    view_protocol::{
        AggregateValue, Coverage, CoverageStatus, ExactNumber, ExcludedCounts, Reducer,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExactNumeric {
    coefficient: BigInt,
    scale: usize,
    decimal: bool,
}

impl ExactNumeric {
    pub(crate) fn integer(value: impl Into<BigInt>) -> Self {
        Self {
            coefficient: value.into(),
            scale: 0,
            decimal: false,
        }
    }

    pub(crate) fn parse_integer(value: &str) -> Result<Self, AggregateError> {
        let coefficient =
            BigInt::parse_bytes(value.as_bytes(), 10).ok_or(AggregateError::InvalidExactNumber)?;
        Ok(Self::integer(coefficient))
    }

    pub(crate) fn parse_decimal(value: &str) -> Result<Self, AggregateError> {
        let (negative, unsigned) = value
            .strip_prefix('-')
            .map_or((false, value), |value| (true, value));
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let mut digits =
            String::with_capacity(whole.len() + fraction.len() + usize::from(negative));
        if negative {
            digits.push('-');
        }
        digits.push_str(whole);
        digits.push_str(fraction);
        let coefficient =
            BigInt::parse_bytes(digits.as_bytes(), 10).ok_or(AggregateError::InvalidExactNumber)?;
        Ok(Self {
            coefficient,
            scale: fraction.len(),
            decimal: true,
        }
        .normalized())
    }

    pub(crate) fn add(&self, other: &Self) -> Self {
        let scale = self.scale.max(other.scale);
        let left = scaled_coefficient(self, scale);
        let right = scaled_coefficient(other, scale);
        Self {
            coefficient: left + right,
            scale,
            decimal: self.decimal || other.decimal,
        }
        .normalized()
    }

    fn normalized(mut self) -> Self {
        if self.coefficient == BigInt::from(0_u8) {
            self.scale = 0;
            return self;
        }
        let ten = BigInt::from(10_u8);
        while self.scale > 0 && (&self.coefficient % &ten) == BigInt::from(0_u8) {
            self.coefficient /= &ten;
            self.scale -= 1;
        }
        self
    }

    pub(crate) fn into_exact_number(self) -> Result<ExactNumber, AggregateError> {
        if !self.decimal {
            return CanonicalInteger::parse(&self.coefficient.to_string())
                .map(ExactNumber::Integer)
                .map_err(|_| AggregateError::InvalidExactNumber);
        }
        DecimalString::parse(&render_decimal(&self.coefficient, self.scale))
            .map(ExactNumber::Decimal)
            .map_err(|_| AggregateError::InvalidExactNumber)
    }
}

impl Ord for ExactNumeric {
    fn cmp(&self, other: &Self) -> Ordering {
        let scale = self.scale.max(other.scale);
        scaled_coefficient(self, scale).cmp(&scaled_coefficient(other, scale))
    }
}

impl PartialOrd for ExactNumeric {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn scaled_coefficient(value: &ExactNumeric, scale: usize) -> BigInt {
    debug_assert!(scale >= value.scale);
    let exponent = u32::try_from(scale - value.scale)
        .expect("canonical decimal scale is bounded below u32::MAX");
    &value.coefficient * BigInt::from(10_u8).pow(exponent)
}

fn render_decimal(coefficient: &BigInt, scale: usize) -> String {
    if scale == 0 {
        return coefficient.to_string();
    }
    let negative = coefficient < &BigInt::from(0_u8);
    let digits = coefficient.to_string();
    let unsigned = digits.strip_prefix('-').unwrap_or(&digits);
    let mut output = String::with_capacity(unsigned.len().max(scale + 1) + 2);
    if negative {
        output.push('-');
    }
    if unsigned.len() > scale {
        let point = unsigned.len() - scale;
        output.push_str(&unsigned[..point]);
        output.push('.');
        output.push_str(&unsigned[point..]);
    } else {
        output.push_str("0.");
        output.extend(std::iter::repeat_n('0', scale - unsigned.len()));
        output.push_str(unsigned);
    }
    output
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Exclusion {
    OpenSpan,
    MissingValue,
    NonNumericValue,
    UnavailableValue,
    ResourceTruncated,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CoverageTally {
    matched: u64,
    contributing: u64,
    open_spans: u64,
    missing_values: u64,
    non_numeric_values: u64,
    unavailable_values: u64,
    resource_truncated: u64,
    gaps: u64,
}

impl CoverageTally {
    pub(crate) fn contribute(&mut self) -> Result<(), AggregateError> {
        self.matched = checked_increment(self.matched)?;
        self.contributing = checked_increment(self.contributing)?;
        Ok(())
    }

    pub(crate) fn exclude(&mut self, exclusion: Exclusion) -> Result<(), AggregateError> {
        self.matched = checked_increment(self.matched)?;
        let value = match exclusion {
            Exclusion::OpenSpan => &mut self.open_spans,
            Exclusion::MissingValue => &mut self.missing_values,
            Exclusion::NonNumericValue => &mut self.non_numeric_values,
            Exclusion::UnavailableValue => &mut self.unavailable_values,
            Exclusion::ResourceTruncated => &mut self.resource_truncated,
        };
        *value = checked_increment(*value)?;
        Ok(())
    }

    pub(crate) fn add_gaps(&mut self, count: u64) -> Result<(), AggregateError> {
        self.gaps = self
            .gaps
            .checked_add(count)
            .ok_or(AggregateError::CountOverflow)?;
        Ok(())
    }

    pub(crate) fn merge(&mut self, other: &Self) -> Result<(), AggregateError> {
        self.matched = checked_add(self.matched, other.matched)?;
        self.contributing = checked_add(self.contributing, other.contributing)?;
        self.open_spans = checked_add(self.open_spans, other.open_spans)?;
        self.missing_values = checked_add(self.missing_values, other.missing_values)?;
        self.non_numeric_values = checked_add(self.non_numeric_values, other.non_numeric_values)?;
        self.unavailable_values = checked_add(self.unavailable_values, other.unavailable_values)?;
        self.resource_truncated = checked_add(self.resource_truncated, other.resource_truncated)?;
        self.gaps = checked_add(self.gaps, other.gaps)?;
        Ok(())
    }

    pub(crate) fn merge_without_gaps(&mut self, other: &Self) -> Result<(), AggregateError> {
        self.matched = checked_add(self.matched, other.matched)?;
        self.contributing = checked_add(self.contributing, other.contributing)?;
        self.open_spans = checked_add(self.open_spans, other.open_spans)?;
        self.missing_values = checked_add(self.missing_values, other.missing_values)?;
        self.non_numeric_values = checked_add(self.non_numeric_values, other.non_numeric_values)?;
        self.unavailable_values = checked_add(self.unavailable_values, other.unavailable_values)?;
        self.resource_truncated = checked_add(self.resource_truncated, other.resource_truncated)?;
        Ok(())
    }

    pub(crate) const fn matched(&self) -> u64 {
        self.matched
    }

    pub(crate) const fn contributing(&self) -> u64 {
        self.contributing
    }

    pub(crate) const fn resource_truncated(&self) -> u64 {
        self.resource_truncated
    }

    pub(crate) fn into_coverage(self) -> Result<Coverage, AggregateError> {
        let excluded = self
            .open_spans
            .checked_add(self.missing_values)
            .and_then(|value| value.checked_add(self.non_numeric_values))
            .and_then(|value| value.checked_add(self.unavailable_values))
            .and_then(|value| value.checked_add(self.resource_truncated))
            .ok_or(AggregateError::CountOverflow)?;
        let status = if self.contributing == 0 && self.matched > 0 {
            CoverageStatus::Unavailable
        } else if excluded > 0 || self.gaps > 0 {
            CoverageStatus::Partial
        } else {
            CoverageStatus::Complete
        };
        Coverage::new(
            status,
            SchemaU64::new(self.matched),
            SchemaU64::new(self.contributing),
            SchemaU64::new(excluded),
            ExcludedCounts::new(
                SchemaU64::new(self.open_spans),
                SchemaU64::new(self.missing_values),
                SchemaU64::new(self.non_numeric_values),
                SchemaU64::new(self.unavailable_values),
                SchemaU64::new(self.resource_truncated),
            ),
            SchemaU64::new(self.gaps),
        )
        .map_err(|_| AggregateError::InvalidCoverage)
    }
}

fn checked_increment(value: u64) -> Result<u64, AggregateError> {
    value.checked_add(1).ok_or(AggregateError::CountOverflow)
}

fn checked_add(left: u64, right: u64) -> Result<u64, AggregateError> {
    left.checked_add(right).ok_or(AggregateError::CountOverflow)
}

pub(crate) fn reduce(
    reducer: Reducer,
    values: &[(u64, ExactNumeric)],
) -> Result<Option<AggregateValue>, AggregateError> {
    if values.is_empty() {
        return Ok(None);
    }
    let exact = match reducer {
        Reducer::Count => ExactNumeric::integer(values.len()),
        Reducer::Sum | Reducer::Mean => values
            .iter()
            .map(|(_, value)| value)
            .fold(ExactNumeric::integer(0_u8), |total, value| total.add(value)),
        Reducer::Min => values
            .iter()
            .map(|(_, value)| value)
            .min()
            .expect("nonempty aggregate input")
            .clone(),
        Reducer::Max => values
            .iter()
            .map(|(_, value)| value)
            .max()
            .expect("nonempty aggregate input")
            .clone(),
        Reducer::Latest => values
            .iter()
            .max_by_key(|(sequence, _)| *sequence)
            .expect("nonempty aggregate input")
            .1
            .clone(),
    };
    let numerator = exact.into_exact_number()?;
    if reducer == Reducer::Mean {
        let contributing_count = TokenCount::parse(&values.len().to_string())
            .map_err(|_| AggregateError::InvalidExactNumber)?;
        Ok(Some(AggregateValue::Mean {
            numerator,
            contributing_count,
        }))
    } else {
        Ok(Some(AggregateValue::Exact { value: numerator }))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AggregateError {
    CountOverflow,
    InvalidExactNumber,
    InvalidCoverage,
}

impl fmt::Display for AggregateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CountOverflow => "query coverage count overflowed",
            Self::InvalidExactNumber => "query produced an invalid exact number",
            Self::InvalidCoverage => "query produced inconsistent coverage",
        })
    }
}

impl std::error::Error for AggregateError {}
