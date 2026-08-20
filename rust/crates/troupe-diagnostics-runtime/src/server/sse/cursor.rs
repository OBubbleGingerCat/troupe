use std::fmt;

use hyper::{HeaderMap, StatusCode};
use troupe_diagnostics_core::scalar::SchemaU64;

use crate::store::key::SortableU64Key;

pub const LAST_EVENT_ID_HEADER: &str = "last-event-id";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorSource {
    QueryAfter,
    LastEventId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveCursor {
    value: SchemaU64,
    source: CursorSource,
}

impl EffectiveCursor {
    pub const fn new(value: SchemaU64, source: CursorSource) -> Self {
        Self { value, source }
    }

    pub const fn value(self) -> SchemaU64 {
        self.value
    }

    pub const fn source(self) -> CursorSource {
        self.source
    }

    pub fn validate_head(self, committed_head: SchemaU64) -> Result<Self, CursorError> {
        if self.value.get() > committed_head.get() {
            Err(CursorError::future(self.value, committed_head))
        } else {
            Ok(self)
        }
    }

    pub const fn is_recoverable_from(self, earliest_available: Option<SchemaU64>) -> bool {
        match earliest_available {
            None => self.value.get() == 0,
            Some(earliest) => self.value.get().saturating_add(1) >= earliest.get(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorErrorKind {
    Missing,
    InvalidQuery,
    InvalidLastEventId,
    AmbiguousLastEventId,
    Future,
    ArchiveFollowUnsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorError {
    kind: CursorErrorKind,
    requested: Option<SchemaU64>,
    committed_head: Option<SchemaU64>,
}

impl CursorError {
    const fn simple(kind: CursorErrorKind) -> Self {
        Self {
            kind,
            requested: None,
            committed_head: None,
        }
    }

    const fn future(requested: SchemaU64, committed_head: SchemaU64) -> Self {
        Self {
            kind: CursorErrorKind::Future,
            requested: Some(requested),
            committed_head: Some(committed_head),
        }
    }

    pub const fn archive_follow_unsupported() -> Self {
        Self::simple(CursorErrorKind::ArchiveFollowUnsupported)
    }

    pub const fn kind(self) -> CursorErrorKind {
        self.kind
    }

    pub const fn requested(self) -> Option<SchemaU64> {
        self.requested
    }

    pub const fn committed_head(self) -> Option<SchemaU64> {
        self.committed_head
    }

    pub const fn status(self) -> StatusCode {
        match self.kind {
            CursorErrorKind::Future => StatusCode::CONFLICT,
            CursorErrorKind::ArchiveFollowUnsupported => StatusCode::METHOD_NOT_ALLOWED,
            CursorErrorKind::Missing
            | CursorErrorKind::InvalidQuery
            | CursorErrorKind::InvalidLastEventId
            | CursorErrorKind::AmbiguousLastEventId => StatusCode::BAD_REQUEST,
        }
    }

    pub const fn code(self) -> &'static str {
        match self.kind {
            CursorErrorKind::Missing => "missing_cursor",
            CursorErrorKind::InvalidQuery | CursorErrorKind::InvalidLastEventId => "invalid_cursor",
            CursorErrorKind::AmbiguousLastEventId => "ambiguous_cursor",
            CursorErrorKind::Future => "cursor_ahead_of_head",
            CursorErrorKind::ArchiveFollowUnsupported => "archive_follow_unsupported",
        }
    }

    pub const fn message(self) -> &'static str {
        match self.kind {
            CursorErrorKind::Missing => {
                "an initial after cursor or a nonempty Last-Event-ID is required"
            }
            CursorErrorKind::InvalidQuery => {
                "after must be the only query parameter and a canonical decimal u64"
            }
            CursorErrorKind::InvalidLastEventId => "Last-Event-ID must be a canonical decimal u64",
            CursorErrorKind::AmbiguousLastEventId => {
                "more than one nonempty Last-Event-ID was supplied"
            }
            CursorErrorKind::Future => "the event cursor is ahead of the committed head",
            CursorErrorKind::ArchiveFollowUnsupported => {
                "archived Runs do not provide a live event stream"
            }
        }
    }
}

impl fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for CursorError {}

pub fn resolve_effective_cursor(
    query: Option<&str>,
    headers: &HeaderMap,
) -> Result<EffectiveCursor, CursorError> {
    if let Some(value) = nonempty_last_event_id(headers)? {
        if let Some(query) = query.filter(|query| !query.is_empty()) {
            let _ = after_query_value(query)?;
        }
        return parse_cursor(value)
            .map(|value| EffectiveCursor::new(value, CursorSource::LastEventId))
            .map_err(|()| CursorError::simple(CursorErrorKind::InvalidLastEventId));
    }

    let value = parse_after_query(query)?;
    Ok(EffectiveCursor::new(value, CursorSource::QueryAfter))
}

fn nonempty_last_event_id(headers: &HeaderMap) -> Result<Option<&str>, CursorError> {
    let mut selected = None;
    for value in headers.get_all(LAST_EVENT_ID_HEADER) {
        let value = value
            .to_str()
            .map_err(|_| CursorError::simple(CursorErrorKind::InvalidLastEventId))?;
        if value.is_empty() {
            continue;
        }
        if selected.replace(value).is_some() {
            return Err(CursorError::simple(CursorErrorKind::AmbiguousLastEventId));
        }
    }
    Ok(selected)
}

fn parse_after_query(query: Option<&str>) -> Result<SchemaU64, CursorError> {
    let Some(query) = query.filter(|value| !value.is_empty()) else {
        return Err(CursorError::simple(CursorErrorKind::Missing));
    };
    let value = after_query_value(query)?;
    parse_cursor(value).map_err(|()| CursorError::simple(CursorErrorKind::InvalidQuery))
}

fn after_query_value(query: &str) -> Result<&str, CursorError> {
    let Some((name, value)) = query.split_once('=') else {
        return Err(CursorError::simple(CursorErrorKind::InvalidQuery));
    };
    if name != "after"
        || value.is_empty()
        || value.contains('&')
        || name.bytes().any(|byte| matches!(byte, b'%' | b'+'))
        || value.bytes().any(|byte| matches!(byte, b'%' | b'+'))
    {
        return Err(CursorError::simple(CursorErrorKind::InvalidQuery));
    }
    Ok(value)
}

fn parse_cursor(value: &str) -> Result<SchemaU64, ()> {
    SortableU64Key::parse_canonical_decimal(value)
        .map(|value| SchemaU64::new(value.get()))
        .map_err(|_| ())
}
