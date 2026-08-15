use std::{fmt, iter::FusedIterator, vec};

use troupe_diagnostics_core::scalar::SchemaU64;

use super::reader::{
    CapturedEvent, CapturedEventSource, ReaderErrorCode, ReaderFailure, ReaderFailureClass,
    ReaderProfile,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FiniteEventQuery {
    After {
        after_exclusive: SchemaU64,
    },
    Tail {
        count: SchemaU64,
    },
    Range {
        after_exclusive: SchemaU64,
        through_inclusive: SchemaU64,
    },
}

impl FiniteEventQuery {
    pub const fn after(after_exclusive: SchemaU64) -> Self {
        Self::After { after_exclusive }
    }

    pub const fn tail(count: SchemaU64) -> Self {
        Self::Tail { count }
    }

    pub const fn range(after_exclusive: SchemaU64, through_inclusive: SchemaU64) -> Self {
        Self::Range {
            after_exclusive,
            through_inclusive,
        }
    }

    pub const fn resolve(self, captured_watermark: SchemaU64) -> CapturedEventRange {
        let watermark = captured_watermark.get();
        let (after_exclusive, through_inclusive) = match self {
            Self::After { after_exclusive } => {
                let after = after_exclusive.get();
                (if after < watermark { after } else { watermark }, watermark)
            }
            Self::Tail { count } => (watermark.saturating_sub(count.get()), watermark),
            Self::Range {
                after_exclusive,
                through_inclusive,
            } => {
                let requested_through = through_inclusive.get();
                let through = if requested_through < watermark {
                    requested_through
                } else {
                    watermark
                };
                let requested_after = after_exclusive.get();
                let after = if requested_after < through {
                    requested_after
                } else {
                    through
                };
                (after, through)
            }
        };
        CapturedEventRange {
            captured_watermark,
            after_exclusive: SchemaU64::new(after_exclusive),
            through_inclusive: SchemaU64::new(through_inclusive),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapturedEventRange {
    captured_watermark: SchemaU64,
    after_exclusive: SchemaU64,
    through_inclusive: SchemaU64,
}

impl CapturedEventRange {
    pub const fn captured_watermark(self) -> SchemaU64 {
        self.captured_watermark
    }

    pub const fn after_exclusive(self) -> SchemaU64 {
        self.after_exclusive
    }

    pub const fn through_inclusive(self) -> SchemaU64 {
        self.through_inclusive
    }

    pub const fn first_sequence(self) -> Option<SchemaU64> {
        if self.is_empty() {
            None
        } else {
            Some(SchemaU64::new(self.after_exclusive.get() + 1))
        }
    }

    pub const fn last_sequence(self) -> Option<SchemaU64> {
        if self.is_empty() {
            None
        } else {
            Some(self.through_inclusive)
        }
    }

    pub const fn event_count(self) -> SchemaU64 {
        SchemaU64::new(
            self.through_inclusive
                .get()
                .saturating_sub(self.after_exclusive.get()),
        )
    }

    pub const fn is_empty(self) -> bool {
        self.after_exclusive.get() == self.through_inclusive.get()
    }
}

#[derive(Debug)]
pub enum EventQueryError {
    Reader(ReaderFailure),
    NonDenseSequence {
        profile: ReaderProfile,
        expected: SchemaU64,
        actual: Option<SchemaU64>,
    },
}

impl EventQueryError {
    pub const fn class(&self) -> ReaderFailureClass {
        match self {
            Self::Reader(error) => error.class(),
            Self::NonDenseSequence { profile, .. } => failure_class(*profile),
        }
    }

    pub const fn profile(&self) -> ReaderProfile {
        match self {
            Self::Reader(error) => error.profile(),
            Self::NonDenseSequence { profile, .. } => *profile,
        }
    }

    pub const fn code(&self) -> ReaderErrorCode {
        match self {
            Self::Reader(error) => error.code(),
            Self::NonDenseSequence { .. } => ReaderErrorCode::EventInvariant,
        }
    }

    pub const fn expected_sequence(&self) -> Option<SchemaU64> {
        match self {
            Self::Reader(_) => None,
            Self::NonDenseSequence { expected, .. } => Some(*expected),
        }
    }

    pub const fn actual_sequence(&self) -> Option<SchemaU64> {
        match self {
            Self::Reader(_) => None,
            Self::NonDenseSequence { actual, .. } => *actual,
        }
    }

    pub const fn reader_failure(&self) -> Option<&ReaderFailure> {
        match self {
            Self::Reader(error) => Some(error),
            Self::NonDenseSequence { .. } => None,
        }
    }
}

impl fmt::Display for EventQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reader(error) => fmt::Display::fmt(error, formatter),
            Self::NonDenseSequence {
                expected, actual, ..
            } => match actual {
                Some(actual) => write!(
                    formatter,
                    "finite event query expected sequence {}, found {}",
                    expected.get(),
                    actual.get()
                ),
                None => write!(
                    formatter,
                    "finite event query expected sequence {}, found end of captured prefix",
                    expected.get()
                ),
            },
        }
    }
}

impl std::error::Error for EventQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Reader(error) => Some(error),
            Self::NonDenseSequence { .. } => None,
        }
    }
}

impl From<ReaderFailure> for EventQueryError {
    fn from(error: ReaderFailure) -> Self {
        Self::Reader(error)
    }
}

const fn failure_class(profile: ReaderProfile) -> ReaderFailureClass {
    match profile {
        ReaderProfile::Active => ReaderFailureClass::CoreFatal,
        ReaderProfile::Archive => ReaderFailureClass::ArchiveOperation,
    }
}

pub fn query_events<'source, 'connection>(
    source: &'source CapturedEventSource<'connection>,
    query: FiniteEventQuery,
) -> FiniteEventIter<'source, 'connection> {
    let range = query.resolve(source.captured_watermark());
    FiniteEventIter {
        source,
        range,
        next_sequence: range.first_sequence().map(SchemaU64::get),
        page: Vec::new().into_iter(),
    }
}

pub struct FiniteEventIter<'source, 'connection> {
    source: &'source CapturedEventSource<'connection>,
    range: CapturedEventRange,
    next_sequence: Option<u64>,
    page: vec::IntoIter<CapturedEvent>,
}

impl FiniteEventIter<'_, '_> {
    pub const fn range(&self) -> CapturedEventRange {
        self.range
    }

    fn non_dense(&mut self, expected: u64, actual: Option<SchemaU64>) -> EventQueryError {
        self.next_sequence = None;
        self.page = Vec::new().into_iter();
        EventQueryError::NonDenseSequence {
            profile: self.source.profile(),
            expected: SchemaU64::new(expected),
            actual,
        }
    }
}

impl Iterator for FiniteEventIter<'_, '_> {
    type Item = Result<CapturedEvent, EventQueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let expected = self.next_sequence?;
            if let Some(event) = self.page.next() {
                if event.sequence().get() != expected {
                    let actual = event.sequence();
                    return Some(Err(self.non_dense(expected, Some(actual))));
                }
                self.next_sequence = if expected == self.range.through_inclusive().get() {
                    None
                } else {
                    Some(expected + 1)
                };
                return Some(Ok(event));
            }

            let after_exclusive = SchemaU64::new(expected - 1);
            let page = match self.source.read_event_page(after_exclusive) {
                Ok(page) => page,
                Err(error) => {
                    self.next_sequence = None;
                    return Some(Err(error.into()));
                }
            };
            if page.events().is_empty() {
                return Some(Err(self.non_dense(expected, None)));
            }
            self.page = page.into_events().into_iter();
        }
    }
}

impl FusedIterator for FiniteEventIter<'_, '_> {}
