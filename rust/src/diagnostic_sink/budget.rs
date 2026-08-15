use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const RUNTIME_MAX_EVENTS: usize = 16_384;
pub(crate) const RUNTIME_MAX_ENCODED_BYTES: usize = 64 * 1024 * 1024;

const PACKED_FIELD_BITS: u32 = 32;
const PACKED_FIELD_MASK: u64 = u32::MAX as u64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BudgetUsage {
    events: usize,
    encoded_bytes: usize,
}

impl BudgetUsage {
    pub(crate) const ZERO: Self = Self::new(0, 0);

    pub(crate) const fn new(events: usize, encoded_bytes: usize) -> Self {
        Self {
            events,
            encoded_bytes,
        }
    }

    pub(crate) const fn events(self) -> usize {
        self.events
    }

    pub(crate) const fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }

    pub(crate) const fn fits_within(self, limits: BudgetLimits) -> bool {
        self.events <= limits.max_events && self.encoded_bytes <= limits.max_encoded_bytes
    }

    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self::new(
            self.events.checked_add(other.events)?,
            self.encoded_bytes.checked_add(other.encoded_bytes)?,
        ))
    }

    pub(crate) fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self::new(
            self.events.checked_sub(other.events)?,
            self.encoded_bytes.checked_sub(other.encoded_bytes)?,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BudgetLimits {
    max_events: usize,
    max_encoded_bytes: usize,
}

impl BudgetLimits {
    pub(crate) const fn new(max_events: usize, max_encoded_bytes: usize) -> Self {
        Self {
            max_events,
            max_encoded_bytes,
        }
    }

    pub(crate) const fn max_events(self) -> usize {
        self.max_events
    }

    pub(crate) const fn max_encoded_bytes(self) -> usize {
        self.max_encoded_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BudgetError {
    LimitExceeded {
        limits: BudgetLimits,
        current: BudgetUsage,
        attempted: BudgetUsage,
    },
    ReleaseExceedsUsage {
        current: BudgetUsage,
        released: BudgetUsage,
    },
    ArithmeticOverflow,
    Unrepresentable(BudgetUsage),
}

impl fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded { .. } => formatter.write_str("diagnostic runtime budget exceeded"),
            Self::ReleaseExceedsUsage { .. } => {
                formatter.write_str("diagnostic runtime budget release exceeds current usage")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("diagnostic runtime budget arithmetic overflow")
            }
            Self::Unrepresentable(_) => {
                formatter.write_str("diagnostic runtime budget usage is not representable")
            }
        }
    }
}

impl std::error::Error for BudgetError {}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeBudget {
    inner: Arc<RuntimeBudgetInner>,
}

#[derive(Debug)]
struct RuntimeBudgetInner {
    limits: BudgetLimits,
    packed_usage: AtomicU64,
}

impl RuntimeBudget {
    pub(crate) fn new() -> Self {
        Self::with_limits(BudgetLimits::new(
            RUNTIME_MAX_EVENTS,
            RUNTIME_MAX_ENCODED_BYTES,
        ))
    }

    pub(crate) fn with_limits(limits: BudgetLimits) -> Self {
        assert!(
            pack(BudgetUsage::new(
                limits.max_events(),
                limits.max_encoded_bytes(),
            ))
            .is_some(),
            "runtime diagnostic limits must fit the atomic budget representation"
        );
        Self {
            inner: Arc::new(RuntimeBudgetInner {
                limits,
                packed_usage: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn limits(&self) -> BudgetLimits {
        self.inner.limits
    }

    pub(crate) fn usage(&self) -> BudgetUsage {
        unpack(self.inner.packed_usage.load(Ordering::Acquire))
    }

    /// Replaces already-owned usage and admits new usage in one two-dimensional CAS.
    pub(crate) fn try_replace(
        &self,
        released: BudgetUsage,
        admitted: BudgetUsage,
    ) -> Result<BudgetUsage, BudgetError> {
        if pack(released).is_none() {
            return Err(BudgetError::Unrepresentable(released));
        }
        if pack(admitted).is_none() {
            return Err(BudgetError::Unrepresentable(admitted));
        }

        let mut observed = self.inner.packed_usage.load(Ordering::Acquire);
        loop {
            let current = unpack(observed);
            let Some(after_release) = current.checked_sub(released) else {
                return Err(BudgetError::ReleaseExceedsUsage { current, released });
            };
            let Some(attempted) = after_release.checked_add(admitted) else {
                return Err(BudgetError::ArithmeticOverflow);
            };
            if !attempted.fits_within(self.inner.limits) {
                return Err(BudgetError::LimitExceeded {
                    limits: self.inner.limits,
                    current,
                    attempted,
                });
            }
            let Some(next) = pack(attempted) else {
                return Err(BudgetError::Unrepresentable(attempted));
            };

            match self.inner.packed_usage.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(attempted),
                Err(changed) => observed = changed,
            }
        }
    }
}

impl Default for RuntimeBudget {
    fn default() -> Self {
        Self::new()
    }
}

fn pack(usage: BudgetUsage) -> Option<u64> {
    let events = u32::try_from(usage.events()).ok()?;
    let encoded_bytes = u32::try_from(usage.encoded_bytes()).ok()?;
    Some((u64::from(events) << PACKED_FIELD_BITS) | u64::from(encoded_bytes))
}

fn unpack(packed: u64) -> BudgetUsage {
    BudgetUsage::new(
        (packed >> PACKED_FIELD_BITS) as usize,
        (packed & PACKED_FIELD_MASK) as usize,
    )
}
