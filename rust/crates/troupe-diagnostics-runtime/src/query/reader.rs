use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use troupe_diagnostics_core::{
    event::{DiagnosticEvent, EVENT_SCHEMA_VERSION},
    id::CanonicalUuid,
    scalar::SchemaU64,
};

use crate::{
    archive::lease::{
        ActiveArchiveLeaseGuard, ArchiveLeaseError, ArchiveLeaseErrorCode, SharedArchiveLease,
    },
    store::{
        connection::{
            StoreOpenError, StoreOpenErrorCode, open_immutable_read_only, validate_store_state,
        },
        key::SortableU64Key,
        schema::{DIAGNOSTIC_DATABASE_FILENAME, STORE_SCHEMA_IDENTITY, STORE_SCHEMA_VERSION},
    },
};

pub const CAPTURED_EVENT_PAGE_SIZE: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderProfile {
    Active,
    Archive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderFailureClass {
    CoreFatal,
    ArchiveOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderErrorCode {
    ArchiveLease,
    InvalidActiveGuard,
    SqliteOpen,
    ReadOnlyConfiguration,
    CaptureBegin,
    StoreValidation,
    MetadataRead,
    EventRead,
    EventDecode,
    EventInvariant,
}

impl ReaderErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArchiveLease => "diagnostic_reader.archive_lease",
            Self::InvalidActiveGuard => "diagnostic_reader.invalid_active_guard",
            Self::SqliteOpen => "diagnostic_reader.sqlite_open",
            Self::ReadOnlyConfiguration => "diagnostic_reader.read_only_configuration",
            Self::CaptureBegin => "diagnostic_reader.capture_begin",
            Self::StoreValidation => "diagnostic_reader.store_validation",
            Self::MetadataRead => "diagnostic_reader.metadata_read",
            Self::EventRead => "diagnostic_reader.event_read",
            Self::EventDecode => "diagnostic_reader.event_decode",
            Self::EventInvariant => "diagnostic_reader.event_invariant",
        }
    }
}

#[derive(Debug)]
enum ReaderFailureSource {
    Lease(ArchiveLeaseError),
    Store(StoreOpenError),
    Sqlite(rusqlite::Error),
    Detail(String),
}

#[derive(Debug)]
pub struct ReaderFailure {
    class: ReaderFailureClass,
    profile: ReaderProfile,
    code: ReaderErrorCode,
    path: PathBuf,
    source: ReaderFailureSource,
}

impl ReaderFailure {
    fn archive_lease(error: ArchiveLeaseError) -> Self {
        Self {
            class: ReaderFailureClass::ArchiveOperation,
            profile: ReaderProfile::Archive,
            code: ReaderErrorCode::ArchiveLease,
            path: error.path().to_path_buf(),
            source: ReaderFailureSource::Lease(error),
        }
    }

    fn sqlite(
        profile: ReaderProfile,
        code: ReaderErrorCode,
        path: &Path,
        error: rusqlite::Error,
    ) -> Self {
        Self {
            class: failure_class(profile),
            profile,
            code,
            path: path.to_path_buf(),
            source: ReaderFailureSource::Sqlite(error),
        }
    }

    fn store(profile: ReaderProfile, path: &Path, error: StoreOpenError) -> Self {
        Self {
            class: failure_class(profile),
            profile,
            code: ReaderErrorCode::StoreValidation,
            path: path.to_path_buf(),
            source: ReaderFailureSource::Store(error),
        }
    }

    fn detail(
        profile: ReaderProfile,
        code: ReaderErrorCode,
        path: &Path,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            class: failure_class(profile),
            profile,
            code,
            path: path.to_path_buf(),
            source: ReaderFailureSource::Detail(detail.into()),
        }
    }

    pub const fn class(&self) -> ReaderFailureClass {
        self.class
    }

    pub const fn profile(&self) -> ReaderProfile {
        self.profile
    }

    pub const fn code(&self) -> ReaderErrorCode {
        self.code
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn store_code(&self) -> Option<StoreOpenErrorCode> {
        match &self.source {
            ReaderFailureSource::Store(error) => Some(error.code()),
            _ => None,
        }
    }

    pub const fn lease_code(&self) -> Option<ArchiveLeaseErrorCode> {
        match &self.source {
            ReaderFailureSource::Lease(error) => Some(error.code()),
            _ => None,
        }
    }
}

impl fmt::Display for ReaderFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic reader failed [{}] at {}: ",
            self.code.as_str(),
            self.path.display()
        )?;
        match &self.source {
            ReaderFailureSource::Lease(error) => fmt::Display::fmt(error, formatter),
            ReaderFailureSource::Store(error) => fmt::Display::fmt(error, formatter),
            ReaderFailureSource::Sqlite(error) => fmt::Display::fmt(error, formatter),
            ReaderFailureSource::Detail(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ReaderFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.source {
            ReaderFailureSource::Lease(error) => Some(error),
            ReaderFailureSource::Store(error) => Some(error),
            ReaderFailureSource::Sqlite(error) => Some(error),
            ReaderFailureSource::Detail(_) => None,
        }
    }
}

const fn failure_class(profile: ReaderProfile) -> ReaderFailureClass {
    match profile {
        ReaderProfile::Active => ReaderFailureClass::CoreFatal,
        ReaderProfile::Archive => ReaderFailureClass::ArchiveOperation,
    }
}

enum HeldLease<'lease> {
    Active(ActiveArchiveLeaseGuard<'lease>),
    Archive(SharedArchiveLease),
}

impl HeldLease<'_> {
    const fn profile(&self) -> ReaderProfile {
        match self {
            Self::Active(_) => ReaderProfile::Active,
            Self::Archive(_) => ReaderProfile::Archive,
        }
    }

    fn anchor_path(&self) -> &Path {
        match self {
            Self::Active(guard) => guard.anchor_path(),
            Self::Archive(lease) => lease.anchor_path(),
        }
    }
}

pub struct DiagnosticReader<'lease> {
    connection: Connection,
    expected_run_id: CanonicalUuid,
    run_directory: PathBuf,
    database_path: PathBuf,
    lease: HeldLease<'lease>,
}

impl fmt::Debug for DiagnosticReader<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticReader")
            .field("profile", &self.profile())
            .field("expected_run_id", &self.expected_run_id)
            .field("run_directory", &self.run_directory)
            .field("database_path", &self.database_path)
            .field("lease_anchor", &self.lease.anchor_path())
            .finish_non_exhaustive()
    }
}

impl<'lease> DiagnosticReader<'lease> {
    pub fn open_active(
        expected_run_id: CanonicalUuid,
        guard: ActiveArchiveLeaseGuard<'lease>,
    ) -> Result<Self, ReaderFailure> {
        let anchor_path = guard.anchor_path().to_path_buf();
        let run_directory = anchor_path
            .parent()
            .ok_or_else(|| {
                ReaderFailure::detail(
                    ReaderProfile::Active,
                    ReaderErrorCode::InvalidActiveGuard,
                    &anchor_path,
                    "active lease anchor has no Run directory",
                )
            })?
            .to_path_buf();
        Self::open(expected_run_id, &run_directory, HeldLease::Active(guard))
    }

    fn open(
        expected_run_id: CanonicalUuid,
        run_directory: &Path,
        lease: HeldLease<'lease>,
    ) -> Result<Self, ReaderFailure> {
        let profile = lease.profile();
        let database_path = run_directory.join(DIAGNOSTIC_DATABASE_FILENAME);
        let connection = match profile {
            ReaderProfile::Active => Connection::open_with_flags(
                &database_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ),
            ReaderProfile::Archive => open_immutable_read_only(&database_path),
        }
        .map_err(|error| {
            ReaderFailure::sqlite(profile, ReaderErrorCode::SqliteOpen, &database_path, error)
        })?;
        configure_read_only(&connection, profile, &database_path)?;
        Ok(Self {
            connection,
            expected_run_id,
            run_directory: run_directory.to_path_buf(),
            database_path,
            lease,
        })
    }

    pub const fn profile(&self) -> ReaderProfile {
        self.lease.profile()
    }

    pub fn run_directory(&self) -> &Path {
        &self.run_directory
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn capture(&mut self) -> Result<CapturedEventSource<'_>, ReaderFailure> {
        let profile = self.profile();
        let expected_run_id = self.expected_run_id;
        let database_path = self.database_path.clone();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| {
                ReaderFailure::sqlite(
                    profile,
                    ReaderErrorCode::CaptureBegin,
                    &database_path,
                    error,
                )
            })?;
        let persisted = validate_store_state(&transaction, expected_run_id, &database_path)
            .map_err(|error| ReaderFailure::store(profile, &database_path, error))?;
        let (ended_at, production_outcome) = transaction
            .query_row(
                "SELECT ended_at, production_outcome FROM run_metadata WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .map_err(|error| {
                ReaderFailure::sqlite(
                    profile,
                    ReaderErrorCode::MetadataRead,
                    &database_path,
                    error,
                )
            })?;
        let metadata = CapturedStoreMetadata {
            run_id: persisted.run_id(),
            started_at: persisted.started_at().to_owned(),
            ended_at,
            production_outcome,
            configuration_identity: persisted.configuration_identity().to_owned(),
            committed_watermark: persisted.committed_watermark(),
            read_model_watermark: persisted.read_model_watermark(),
            clean_shutdown: persisted.clean_shutdown(),
        };
        Ok(CapturedEventSource {
            transaction,
            metadata,
            profile,
            database_path,
        })
    }
}

impl DiagnosticReader<'static> {
    pub fn open_archive(
        run_directory: &Path,
        expected_run_id: CanonicalUuid,
    ) -> Result<Self, ReaderFailure> {
        let lease =
            SharedArchiveLease::acquire(run_directory).map_err(ReaderFailure::archive_lease)?;
        Self::open(expected_run_id, run_directory, HeldLease::Archive(lease))
    }

    pub fn open_identified_archive(run_directory: &Path) -> Result<Self, ReaderFailure> {
        let lease =
            SharedArchiveLease::acquire(run_directory).map_err(ReaderFailure::archive_lease)?;
        let profile = ReaderProfile::Archive;
        let database_path = run_directory.join(DIAGNOSTIC_DATABASE_FILENAME);
        let connection = open_immutable_read_only(&database_path).map_err(|error| {
            ReaderFailure::sqlite(profile, ReaderErrorCode::SqliteOpen, &database_path, error)
        })?;
        configure_read_only(&connection, profile, &database_path)?;
        let encoded_run_id = connection
            .query_row(
                "SELECT run_id FROM run_metadata WHERE singleton = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| {
                ReaderFailure::sqlite(
                    profile,
                    ReaderErrorCode::MetadataRead,
                    &database_path,
                    error,
                )
            })?;
        let expected_run_id = CanonicalUuid::parse(&encoded_run_id).map_err(|error| {
            ReaderFailure::detail(
                profile,
                ReaderErrorCode::StoreValidation,
                &database_path,
                format!("diagnostic store Run identity is invalid: {error}"),
            )
        })?;
        let mut reader = Self {
            connection,
            expected_run_id,
            run_directory: run_directory.to_path_buf(),
            database_path,
            lease: HeldLease::Archive(lease),
        };
        drop(reader.capture()?);
        Ok(reader)
    }
}

fn configure_read_only(
    connection: &Connection,
    profile: ReaderProfile,
    database_path: &Path,
) -> Result<(), ReaderFailure> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .and_then(|()| connection.pragma_update(None, "query_only", true))
        .map_err(|error| {
            ReaderFailure::sqlite(
                profile,
                ReaderErrorCode::ReadOnlyConfiguration,
                database_path,
                error,
            )
        })?;
    let query_only: bool = connection
        .pragma_query_value(None, "query_only", |row| row.get(0))
        .map_err(|error| {
            ReaderFailure::sqlite(
                profile,
                ReaderErrorCode::ReadOnlyConfiguration,
                database_path,
                error,
            )
        })?;
    let sqlite_read_only = connection.is_readonly("main").map_err(|error| {
        ReaderFailure::sqlite(
            profile,
            ReaderErrorCode::ReadOnlyConfiguration,
            database_path,
            error,
        )
    })?;
    if !query_only || !sqlite_read_only {
        return Err(ReaderFailure::detail(
            profile,
            ReaderErrorCode::ReadOnlyConfiguration,
            database_path,
            "SQLite connection is not query-only and read-only",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedStoreMetadata {
    run_id: CanonicalUuid,
    started_at: String,
    ended_at: Option<String>,
    production_outcome: Option<String>,
    configuration_identity: String,
    committed_watermark: SortableU64Key,
    read_model_watermark: SortableU64Key,
    clean_shutdown: bool,
}

impl CapturedStoreMetadata {
    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn store_schema_version(&self) -> u32 {
        STORE_SCHEMA_VERSION
    }

    pub const fn store_schema_identity(&self) -> &'static str {
        STORE_SCHEMA_IDENTITY
    }

    pub const fn event_schema_version(&self) -> u8 {
        EVENT_SCHEMA_VERSION
    }

    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    pub fn ended_at(&self) -> Option<&str> {
        self.ended_at.as_deref()
    }

    pub fn production_outcome(&self) -> Option<&str> {
        self.production_outcome.as_deref()
    }

    pub fn configuration_identity(&self) -> &str {
        &self.configuration_identity
    }

    pub const fn committed_watermark(&self) -> SchemaU64 {
        SchemaU64::new(self.committed_watermark.get())
    }

    pub const fn read_model_watermark(&self) -> SchemaU64 {
        SchemaU64::new(self.read_model_watermark.get())
    }

    pub const fn clean_shutdown(&self) -> bool {
        self.clean_shutdown
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedEvent {
    event: DiagnosticEvent,
    canonical_bytes: Box<[u8]>,
}

impl CapturedEvent {
    pub const fn event(&self) -> &DiagnosticEvent {
        &self.event
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    pub const fn sequence(&self) -> SchemaU64 {
        self.event.header().sequence()
    }

    pub fn into_parts(self) -> (DiagnosticEvent, Box<[u8]>) {
        (self.event, self.canonical_bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedEventPage {
    events: Vec<CapturedEvent>,
    next_after: Option<SchemaU64>,
}

impl CapturedEventPage {
    pub fn events(&self) -> &[CapturedEvent] {
        &self.events
    }

    pub const fn next_after(&self) -> Option<SchemaU64> {
        self.next_after
    }

    pub fn into_events(self) -> Vec<CapturedEvent> {
        self.events
    }
}

pub struct CapturedEventSource<'connection> {
    transaction: Transaction<'connection>,
    metadata: CapturedStoreMetadata,
    profile: ReaderProfile,
    database_path: PathBuf,
}

impl fmt::Debug for CapturedEventSource<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedEventSource")
            .field("profile", &self.profile)
            .field("database_path", &self.database_path)
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl CapturedEventSource<'_> {
    pub const fn profile(&self) -> ReaderProfile {
        self.profile
    }

    pub const fn metadata(&self) -> &CapturedStoreMetadata {
        &self.metadata
    }

    pub const fn captured_watermark(&self) -> SchemaU64 {
        self.metadata.committed_watermark()
    }

    pub fn events(&self) -> CapturedEventIter<'_, '_> {
        let next_sequence = (self.captured_watermark().get() != 0).then_some(1);
        CapturedEventIter {
            source: self,
            next_sequence,
        }
    }

    pub fn read_event_page(
        &self,
        after_exclusive: SchemaU64,
    ) -> Result<CapturedEventPage, ReaderFailure> {
        let watermark = self.captured_watermark().get();
        if after_exclusive.get() >= watermark {
            return Ok(CapturedEventPage {
                events: Vec::new(),
                next_after: None,
            });
        }
        let after = SortableU64Key::new(after_exclusive.get());
        let through = SortableU64Key::new(watermark);
        let mut statement = self
            .transaction
            .prepare(
                "SELECT canonical_json FROM events \
                 WHERE sequence_key > ?1 AND sequence_key <= ?2 \
                 ORDER BY sequence_key LIMIT ?3",
            )
            .map_err(|error| self.sqlite_failure(ReaderErrorCode::EventRead, error))?;
        let rows = statement
            .query_map(
                params![
                    after.as_bytes().as_slice(),
                    through.as_bytes().as_slice(),
                    i64::try_from(CAPTURED_EVENT_PAGE_SIZE)
                        .expect("captured event page size fits SQLite INTEGER")
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .map_err(|error| self.sqlite_failure(ReaderErrorCode::EventRead, error))?;
        let mut events = Vec::with_capacity(CAPTURED_EVENT_PAGE_SIZE);
        for row in rows {
            let canonical_bytes =
                row.map_err(|error| self.sqlite_failure(ReaderErrorCode::EventRead, error))?;
            events.push(self.decode_event(canonical_bytes)?);
        }
        let next_after = events
            .last()
            .and_then(|event| (event.sequence().get() < watermark).then_some(event.sequence()));
        Ok(CapturedEventPage { events, next_after })
    }

    pub(crate) const fn transaction(&self) -> &Transaction<'_> {
        &self.transaction
    }

    fn read_event(&self, sequence: u64) -> Result<CapturedEvent, ReaderFailure> {
        let key = SortableU64Key::new(sequence);
        let canonical_bytes = self
            .transaction
            .query_row(
                "SELECT canonical_json FROM events WHERE sequence_key = ?1",
                [key.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|error| self.sqlite_failure(ReaderErrorCode::EventRead, error))?
            .ok_or_else(|| {
                ReaderFailure::detail(
                    self.profile,
                    ReaderErrorCode::EventInvariant,
                    &self.database_path,
                    format!("captured dense prefix is missing sequence {sequence}"),
                )
            })?;
        let event = self.decode_event(canonical_bytes)?;
        if event.sequence().get() != sequence {
            return Err(ReaderFailure::detail(
                self.profile,
                ReaderErrorCode::EventInvariant,
                &self.database_path,
                format!(
                    "captured event sequence {} differs from requested {sequence}",
                    event.sequence().get()
                ),
            ));
        }
        Ok(event)
    }

    fn decode_event(&self, canonical_bytes: Vec<u8>) -> Result<CapturedEvent, ReaderFailure> {
        let event: DiagnosticEvent = serde_json::from_slice(&canonical_bytes).map_err(|error| {
            ReaderFailure::detail(
                self.profile,
                ReaderErrorCode::EventDecode,
                &self.database_path,
                error.to_string(),
            )
        })?;
        let reencoded = serde_json::to_vec(&event).map_err(|error| {
            ReaderFailure::detail(
                self.profile,
                ReaderErrorCode::EventDecode,
                &self.database_path,
                error.to_string(),
            )
        })?;
        let header = event.header();
        if header.run_id() != self.metadata.run_id()
            || header.sequence().get() == 0
            || header.sequence().get() > self.captured_watermark().get()
            || header.schema_version() != EVENT_SCHEMA_VERSION
            || reencoded != canonical_bytes
        {
            return Err(ReaderFailure::detail(
                self.profile,
                ReaderErrorCode::EventInvariant,
                &self.database_path,
                "captured event identity or canonical encoding is inconsistent",
            ));
        }
        Ok(CapturedEvent {
            event,
            canonical_bytes: canonical_bytes.into_boxed_slice(),
        })
    }

    fn sqlite_failure(&self, code: ReaderErrorCode, error: rusqlite::Error) -> ReaderFailure {
        ReaderFailure::sqlite(self.profile, code, &self.database_path, error)
    }
}

pub struct CapturedEventIter<'source, 'connection> {
    source: &'source CapturedEventSource<'connection>,
    next_sequence: Option<u64>,
}

impl Iterator for CapturedEventIter<'_, '_> {
    type Item = Result<CapturedEvent, ReaderFailure>;

    fn next(&mut self) -> Option<Self::Item> {
        let sequence = self.next_sequence?;
        let watermark = self.source.captured_watermark().get();
        self.next_sequence = if sequence == watermark {
            None
        } else {
            sequence.checked_add(1)
        };
        Some(self.source.read_event(sequence))
    }
}
