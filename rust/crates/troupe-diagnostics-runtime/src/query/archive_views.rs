use std::{collections::HashSet, fmt};

use rusqlite::OptionalExtension;
use serde_json::Value;
use troupe_diagnostics_core::view_protocol::{
    ArchivedViewRecordStatus, IncompatibilityReason, MAX_VIEW_ID_BYTES, Renderer,
    VIEW_SCHEMA_VERSION, ViewRecord, classify_archived_view_record,
};

use crate::store::view_records::{
    MAX_TOTAL_VIEW_RECORD_BYTES, MAX_VIEW_MANIFEST_BYTES, MAX_VIEW_RECORD_BYTES, MAX_VIEW_RECORDS,
    VIEW_MANIFEST_SCHEMA_VERSION, ViewManifest, ViewManifestEntry,
};

use super::reader::{CapturedEventSource, ReaderFailure, ReaderFailureClass, ReaderProfile};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoredViewAvailability {
    Compatible(ViewRecord),
    Unavailable(IncompatibilityReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredViewRecord {
    id: String,
    ordinal: usize,
    renderer: Renderer,
    record_view_schema_version: u64,
    availability: StoredViewAvailability,
}

impl StoredViewRecord {
    fn new(entry: &ViewManifestEntry, availability: StoredViewAvailability) -> Self {
        Self {
            id: entry.id().to_owned(),
            ordinal: entry.ordinal(),
            renderer: entry.renderer(),
            record_view_schema_version: u64::from(entry.view_schema_version()),
            availability,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub const fn renderer(&self) -> Renderer {
        self.renderer
    }

    pub const fn record_view_schema_version(&self) -> u64 {
        self.record_view_schema_version
    }

    pub const fn availability(&self) -> &StoredViewAvailability {
        &self.availability
    }

    pub const fn compatible_record(&self) -> Option<&ViewRecord> {
        match &self.availability {
            StoredViewAvailability::Compatible(record) => Some(record),
            StoredViewAvailability::Unavailable(_) => None,
        }
    }

    pub const fn unavailable_reason(&self) -> Option<IncompatibilityReason> {
        match self.availability {
            StoredViewAvailability::Compatible(_) => None,
            StoredViewAvailability::Unavailable(reason) => Some(reason),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StoredViewCatalog {
    views: Vec<StoredViewRecord>,
}

impl StoredViewCatalog {
    pub fn views(&self) -> &[StoredViewRecord] {
        &self.views
    }

    pub fn get(&self, id: &str) -> Option<&StoredViewRecord> {
        self.views.iter().find(|view| view.id() == id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveViewLoadErrorCode {
    CanonicalEvent,
    ManifestRead,
    ManifestIncompatible,
    ManifestCorrupt,
    RecordRead,
    RecordSetInconsistent,
    ActiveRecordUnavailable,
}

impl ArchiveViewLoadErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CanonicalEvent => "diagnostic_archive_views.canonical_event",
            Self::ManifestRead => "diagnostic_archive_views.manifest_read",
            Self::ManifestIncompatible => "diagnostic_archive_views.manifest_incompatible",
            Self::ManifestCorrupt => "diagnostic_archive_views.manifest_corrupt",
            Self::RecordRead => "diagnostic_archive_views.record_read",
            Self::RecordSetInconsistent => "diagnostic_archive_views.record_set_inconsistent",
            Self::ActiveRecordUnavailable => "diagnostic_archive_views.active_record_unavailable",
        }
    }
}

#[derive(Debug)]
enum ArchiveViewLoadErrorSource {
    Reader(ReaderFailure),
    Sqlite(rusqlite::Error),
    Detail(&'static str),
}

#[derive(Debug)]
pub struct ArchiveViewLoadError {
    class: ReaderFailureClass,
    profile: ReaderProfile,
    code: ArchiveViewLoadErrorCode,
    source: ArchiveViewLoadErrorSource,
}

impl ArchiveViewLoadError {
    fn reader(error: ReaderFailure) -> Self {
        Self {
            class: error.class(),
            profile: error.profile(),
            code: ArchiveViewLoadErrorCode::CanonicalEvent,
            source: ArchiveViewLoadErrorSource::Reader(error),
        }
    }

    fn sqlite(
        profile: ReaderProfile,
        code: ArchiveViewLoadErrorCode,
        error: rusqlite::Error,
    ) -> Self {
        Self {
            class: failure_class(profile),
            profile,
            code,
            source: ArchiveViewLoadErrorSource::Sqlite(error),
        }
    }

    fn detail(
        profile: ReaderProfile,
        code: ArchiveViewLoadErrorCode,
        detail: &'static str,
    ) -> Self {
        Self {
            class: failure_class(profile),
            profile,
            code,
            source: ArchiveViewLoadErrorSource::Detail(detail),
        }
    }

    pub const fn class(&self) -> ReaderFailureClass {
        self.class
    }

    pub const fn profile(&self) -> ReaderProfile {
        self.profile
    }

    pub const fn code(&self) -> ArchiveViewLoadErrorCode {
        self.code
    }

    pub const fn reader_failure(&self) -> Option<&ReaderFailure> {
        match &self.source {
            ArchiveViewLoadErrorSource::Reader(error) => Some(error),
            ArchiveViewLoadErrorSource::Sqlite(_) | ArchiveViewLoadErrorSource::Detail(_) => None,
        }
    }
}

impl fmt::Display for ArchiveViewLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic archive view load failed [{}]: ",
            self.code.as_str()
        )?;
        match &self.source {
            ArchiveViewLoadErrorSource::Reader(error) => fmt::Display::fmt(error, formatter),
            ArchiveViewLoadErrorSource::Sqlite(error) => fmt::Display::fmt(error, formatter),
            ArchiveViewLoadErrorSource::Detail(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ArchiveViewLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.source {
            ArchiveViewLoadErrorSource::Reader(error) => Some(error),
            ArchiveViewLoadErrorSource::Sqlite(error) => Some(error),
            ArchiveViewLoadErrorSource::Detail(_) => None,
        }
    }
}

const fn failure_class(profile: ReaderProfile) -> ReaderFailureClass {
    match profile {
        ReaderProfile::Active => ReaderFailureClass::CoreFatal,
        ReaderProfile::Archive => ReaderFailureClass::ArchiveOperation,
    }
}

pub fn load_stored_view_records(
    source: &CapturedEventSource<'_>,
) -> Result<StoredViewCatalog, ArchiveViewLoadError> {
    for event in source.events() {
        event.map_err(ArchiveViewLoadError::reader)?;
    }

    let manifest = read_manifest(source)?;
    let persisted_count = source
        .transaction()
        .query_row("SELECT count(*) FROM diagnostic_view_records", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| {
            ArchiveViewLoadError::sqlite(
                source.profile(),
                ArchiveViewLoadErrorCode::RecordRead,
                error,
            )
        })?;
    if usize::try_from(persisted_count).ok() != Some(manifest.views().len()) {
        return Err(ArchiveViewLoadError::detail(
            source.profile(),
            ArchiveViewLoadErrorCode::RecordSetInconsistent,
            "stored view record count differs from the manifest",
        ));
    }

    let mut total_record_bytes = 0_usize;
    let mut views = Vec::with_capacity(manifest.views().len());
    for entry in manifest.views() {
        let availability = read_record(source, entry, &mut total_record_bytes)?;
        if source.profile() == ReaderProfile::Active
            && matches!(availability, StoredViewAvailability::Unavailable(_))
        {
            return Err(ArchiveViewLoadError::detail(
                ReaderProfile::Active,
                ArchiveViewLoadErrorCode::ActiveRecordUnavailable,
                "active view records must all be current and valid",
            ));
        }
        views.push(StoredViewRecord::new(entry, availability));
    }
    Ok(StoredViewCatalog { views })
}

fn read_manifest(source: &CapturedEventSource<'_>) -> Result<ViewManifest, ArchiveViewLoadError> {
    let profile = source.profile();
    let (row_count, encoded_length) = source
        .transaction()
        .query_row(
            "SELECT count(*), max(length(manifest_json)) FROM diagnostic_view_manifest",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .map_err(|error| {
            ArchiveViewLoadError::sqlite(profile, ArchiveViewLoadErrorCode::ManifestRead, error)
        })?;
    if row_count != 1 {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest row is missing or duplicated",
        ));
    }
    let Some(encoded_length) = encoded_length.and_then(|value| usize::try_from(value).ok()) else {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest length is invalid",
        ));
    };
    if encoded_length > MAX_VIEW_MANIFEST_BYTES {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest exceeds the V1 byte limit",
        ));
    }

    let (schema_version, record_count, encoded) = source
        .transaction()
        .query_row(
            "SELECT manifest_schema_version, record_count, manifest_json \
             FROM diagnostic_view_manifest WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .map_err(|error| {
            ArchiveViewLoadError::sqlite(profile, ArchiveViewLoadErrorCode::ManifestRead, error)
        })?;
    if schema_version > i64::from(VIEW_MANIFEST_SCHEMA_VERSION) {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestIncompatible,
            "stored view manifest schema is newer than this runtime",
        ));
    }
    if schema_version != i64::from(VIEW_MANIFEST_SCHEMA_VERSION) {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest schema is invalid",
        ));
    }
    let Some(record_count) = usize::try_from(record_count).ok() else {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest count is invalid",
        ));
    };
    if record_count > MAX_VIEW_RECORDS {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest count exceeds the V1 limit",
        ));
    }
    let manifest: ViewManifest = serde_json::from_slice(&encoded).map_err(|_| {
        ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest is not valid current-schema JSON",
        )
    })?;
    let canonical = serde_json::to_vec(&manifest).map_err(|_| {
        ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest cannot be canonically encoded",
        )
    })?;
    if manifest.manifest_schema_version() != VIEW_MANIFEST_SCHEMA_VERSION
        || manifest.views().len() != record_count
        || canonical != encoded
        || !valid_manifest_entries(manifest.views())
    {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::ManifestCorrupt,
            "stored view manifest identity or canonical encoding is inconsistent",
        ));
    }
    Ok(manifest)
}

fn valid_manifest_entries(entries: &[ViewManifestEntry]) -> bool {
    let mut ids = HashSet::with_capacity(entries.len());
    entries.iter().enumerate().all(|(ordinal, entry)| {
        entry.ordinal() == ordinal
            && entry.view_schema_version() >= 1
            && valid_view_id(entry.id())
            && ids.insert(entry.id())
    })
}

fn valid_view_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    !value.is_empty()
        && value.len() <= MAX_VIEW_ID_BYTES
        && bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn read_record(
    source: &CapturedEventSource<'_>,
    entry: &ViewManifestEntry,
    total_record_bytes: &mut usize,
) -> Result<StoredViewAvailability, ArchiveViewLoadError> {
    let profile = source.profile();
    let ordinal = i64::try_from(entry.ordinal()).expect("bounded view ordinal fits SQLite INTEGER");
    let metadata = source
        .transaction()
        .query_row(
            "SELECT view_id, view_schema_version, renderer, length(record_json), \
                    typeof(record_json) \
             FROM diagnostic_view_records WHERE ordinal = ?1",
            [ordinal],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            ArchiveViewLoadError::sqlite(profile, ArchiveViewLoadErrorCode::RecordRead, error)
        })?;
    let Some((id, schema_version, renderer, encoded_length, storage_type)) = metadata else {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::RecordSetInconsistent,
            "stored view record ordinal is missing",
        ));
    };
    if id != entry.id()
        || schema_version != i64::from(entry.view_schema_version())
        || renderer != entry.renderer().as_str()
    {
        return Err(ArchiveViewLoadError::detail(
            profile,
            ArchiveViewLoadErrorCode::RecordSetInconsistent,
            "stored view record identity differs from the manifest",
        ));
    }
    let Some(encoded_length) = usize::try_from(encoded_length).ok() else {
        return Ok(StoredViewAvailability::Unavailable(
            IncompatibilityReason::CorruptRecord,
        ));
    };
    if storage_type != "blob" || encoded_length > MAX_VIEW_RECORD_BYTES {
        return Ok(StoredViewAvailability::Unavailable(
            IncompatibilityReason::CorruptRecord,
        ));
    }
    let Some(next_total) = total_record_bytes.checked_add(encoded_length) else {
        return Ok(StoredViewAvailability::Unavailable(
            IncompatibilityReason::CorruptRecord,
        ));
    };
    if next_total > MAX_TOTAL_VIEW_RECORD_BYTES {
        return Ok(StoredViewAvailability::Unavailable(
            IncompatibilityReason::CorruptRecord,
        ));
    }

    let encoded = source
        .transaction()
        .query_row(
            "SELECT record_json FROM diagnostic_view_records WHERE ordinal = ?1",
            [ordinal],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map_err(|error| {
            ArchiveViewLoadError::sqlite(profile, ArchiveViewLoadErrorCode::RecordRead, error)
        })?;
    *total_record_bytes = next_total;
    Ok(classify_record(entry, &encoded))
}

fn classify_record(entry: &ViewManifestEntry, encoded: &[u8]) -> StoredViewAvailability {
    let Ok(value) = serde_json::from_slice::<Value>(encoded) else {
        return StoredViewAvailability::Unavailable(IncompatibilityReason::CorruptRecord);
    };
    let Some(object) = value.as_object() else {
        return StoredViewAvailability::Unavailable(IncompatibilityReason::CorruptRecord);
    };
    let version = object.get("view_schema_version").and_then(Value::as_u64);
    if version != Some(u64::from(entry.view_schema_version()))
        || object.get("id").and_then(Value::as_str) != Some(entry.id())
        || object.get("renderer").and_then(Value::as_str) != Some(entry.renderer().as_str())
    {
        return StoredViewAvailability::Unavailable(IncompatibilityReason::CorruptRecord);
    }

    match classify_archived_view_record(&value) {
        ArchivedViewRecordStatus::Compatible(record) => {
            let canonical = serde_json::to_vec(&record);
            if canonical.is_ok_and(|canonical| canonical == encoded) {
                StoredViewAvailability::Compatible(record)
            } else {
                StoredViewAvailability::Unavailable(IncompatibilityReason::CorruptRecord)
            }
        }
        ArchivedViewRecordStatus::Incompatible(reason) => {
            if reason == IncompatibilityReason::NewerViewSchema
                && version.is_some_and(|version| version > u64::from(VIEW_SCHEMA_VERSION))
            {
                StoredViewAvailability::Unavailable(IncompatibilityReason::NewerViewSchema)
            } else {
                StoredViewAvailability::Unavailable(IncompatibilityReason::CorruptRecord)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use rusqlite::{Connection, params};
    use serde_json::json;
    use troupe_diagnostics_core::{
        event::{CounterSampled, DiagnosticEvent, DiagnosticEventHeader, DiagnosticScope},
        hub::{
            AcceptedDiagnosticEvent, AdmissionReservation, AdmissionReserver, AdmissionSize,
            DeliveryFailure, EventIdentity, LiveEventNotifier, MandatoryDurableReserver,
            ProductionDiagnosticHub,
        },
        id::CanonicalUuid,
        kinds::CounterKind,
        scalar::SchemaU64,
        time::ElapsedNs,
    };

    use crate::{
        archive::lease::ActiveArchiveLease,
        store::{
            batch::EventBatch,
            connection::{DiagnosticStore, InitialStoreMetadata, StoreOpenErrorCode},
            schema::DIAGNOSTIC_DATABASE_FILENAME,
            view_records::{CompiledViewSet, persist_view_set},
            writer::TransactionalWriter,
        },
    };

    use super::*;
    use crate::query::reader::{DiagnosticReader, ReaderErrorCode};

    const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestRunDirectory(PathBuf);

    impl TestRunDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "troupe-b13-archive-views-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test Run directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn database_path(&self) -> PathBuf {
            self.0.join(DIAGNOSTIC_DATABASE_FILENAME)
        }
    }

    impl Drop for TestRunDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_id() -> CanonicalUuid {
        CanonicalUuid::parse(RUN_ID).expect("canonical test Run UUID")
    }

    fn create_store(directory: &Path) -> DiagnosticStore {
        DiagnosticStore::create(
            directory,
            &InitialStoreMetadata::new(
                run_id(),
                "2026-08-16T00:00:00Z",
                "configuration-sha256:b13",
            ),
        )
        .expect("create diagnostic store")
    }

    fn timeline_record(id: &str) -> Vec<u8> {
        let record: ViewRecord = serde_json::from_value(json!({
            "renderer": "timeline",
            "view_schema_version": 1,
            "id": id,
            "title": format!("View {id}"),
            "time_range": "run",
            "scope": "run",
            "query": {
                "source": {
                    "source": "span",
                    "selector": {"selector": "built_in", "kind": "cue.execution"}
                },
                "filters": [],
                "group_by": null
            }
        }))
        .expect("parse typed timeline view");
        serde_json::to_vec(&record).expect("encode canonical timeline view")
    }

    fn create_views(directory: &Path, ids: &[&str]) {
        drop(create_store(directory));
        let records = ids.iter().map(|id| timeline_record(id)).collect::<Vec<_>>();
        let compiled = CompiledViewSet::from_json_records(records.iter().map(Vec::as_slice))
            .expect("compile test view set");
        persist_view_set(directory, run_id(), &compiled).expect("persist test view set");
    }

    fn open_archive_catalog(
        directory: &TestRunDirectory,
    ) -> Result<StoredViewCatalog, ArchiveViewLoadError> {
        let mut reader = DiagnosticReader::open_archive(directory.path(), run_id())
            .expect("open archive reader");
        let source = reader
            .capture()
            .expect("capture structurally valid archive");
        load_stored_view_records(&source)
    }

    fn update_manifest_version(connection: &Connection, ordinal: usize, version: u64) {
        let encoded: Vec<u8> = connection
            .query_row(
                "SELECT manifest_json FROM diagnostic_view_manifest WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read manifest JSON");
        let mut manifest: Value = serde_json::from_slice(&encoded).expect("decode manifest JSON");
        manifest["views"][ordinal]["view_schema_version"] = json!(version);
        connection
            .execute(
                "UPDATE diagnostic_view_manifest SET manifest_json = ?1 WHERE singleton = 1",
                [serde_json::to_vec(&manifest).expect("encode modified manifest")],
            )
            .expect("update manifest JSON");
    }

    fn replace_record(connection: &Connection, id: &str, schema_version: u64, encoded: &[u8]) {
        let schema_version =
            i64::try_from(schema_version).expect("test view schema version fits SQLite INTEGER");
        connection
            .execute(
                "UPDATE diagnostic_view_records \
                 SET view_schema_version = ?1, record_json = ?2 WHERE view_id = ?3",
                params![schema_version, encoded, id],
            )
            .expect("replace stored view record");
    }

    fn remove_event_write_guards(connection: &Connection) -> (String, String) {
        let update = connection
            .query_row(
                "SELECT sql FROM sqlite_schema \
                 WHERE type = 'trigger' AND name = 'events_no_update'",
                [],
                |row| row.get(0),
            )
            .expect("read update guard definition");
        let delete = connection
            .query_row(
                "SELECT sql FROM sqlite_schema \
                 WHERE type = 'trigger' AND name = 'events_no_delete'",
                [],
                |row| row.get(0),
            )
            .expect("read delete guard definition");
        connection
            .execute_batch("DROP TRIGGER events_no_update; DROP TRIGGER events_no_delete;")
            .expect("remove append-only guards for fault injection");
        (update, delete)
    }

    fn restore_event_write_guards(connection: &Connection, definitions: (String, String)) {
        connection
            .execute_batch(&definitions.0)
            .expect("restore update guard after fault injection");
        connection
            .execute_batch(&definitions.1)
            .expect("restore delete guard after fault injection");
    }

    #[test]
    fn archive_localizes_newer_and_corrupt_records_and_keeps_compatible_views() {
        let directory = TestRunDirectory::new("local-records");
        let active_lease =
            ActiveArchiveLease::acquire(directory.path()).expect("acquire active lease");
        create_views(directory.path(), &["first", "future", "broken", "last"]);
        let marker = directory.path().join("embedded-content-executed");
        let connection = Connection::open(directory.database_path()).expect("open test database");

        let future = serde_json::to_vec(&json!({
            "renderer": "timeline",
            "view_schema_version": 2,
            "id": "future",
            "title": "<script>write_marker()</script>",
            "time_range": "run",
            "scope": "run",
            "query": {
                "python": format!(
                    "__import__('pathlib').Path({:?}).write_text('bad')",
                    marker.display().to_string()
                )
            }
        }))
        .expect("encode future opaque record");
        update_manifest_version(&connection, 1, 2);
        replace_record(&connection, "future", 2, &future);

        let broken = serde_json::to_vec(&json!({
            "renderer": "timeline",
            "view_schema_version": 1,
            "id": "broken",
            "title": "Broken",
            "time_range": "run",
            "scope": "run"
        }))
        .expect("encode corrupt current record");
        replace_record(&connection, "broken", 1, &broken);
        drop(connection);
        append_counter(directory.path());
        drop(active_lease);

        let mut reader = DiagnosticReader::open_archive(directory.path(), run_id())
            .expect("open archive reader");
        let source = reader.capture().expect("capture archive prefix");
        let catalog = load_stored_view_records(&source).expect("load isolated archive records");
        assert_eq!(catalog.views().len(), 4);
        assert_eq!(catalog.views()[0].id(), "first");
        assert!(catalog.views()[0].compatible_record().is_some());
        assert_eq!(
            catalog.views()[1].unavailable_reason(),
            Some(IncompatibilityReason::NewerViewSchema)
        );
        assert_eq!(catalog.views()[1].record_view_schema_version(), 2);
        assert_eq!(
            catalog.views()[2].unavailable_reason(),
            Some(IncompatibilityReason::CorruptRecord)
        );
        assert_eq!(catalog.views()[3].id(), "last");
        assert!(catalog.views()[3].compatible_record().is_some());
        let events = source
            .events()
            .collect::<Result<Vec<_>, _>>()
            .expect("canonical events remain queryable");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence().get(), 1);
        assert!(!marker.exists(), "stored strings must never be executed");
        assert_eq!(
            IncompatibilityReason::NewerViewSchema.as_str(),
            "newer_view_schema"
        );
        assert_eq!(
            IncompatibilityReason::CorruptRecord.as_str(),
            "corrupt_record"
        );
    }

    #[test]
    fn active_profile_rejects_incompatible_records_without_a_partial_catalog() {
        let directory = TestRunDirectory::new("active-invalid");
        let active_lease =
            ActiveArchiveLease::acquire(directory.path()).expect("acquire active lease");
        create_views(directory.path(), &["valid", "broken"]);
        let connection = Connection::open(directory.database_path()).expect("open test database");
        replace_record(&connection, "broken", 1, b"not-json");
        drop(connection);

        let mut reader = DiagnosticReader::open_active(run_id(), active_lease.guard())
            .expect("open active reader");
        let source = reader.capture().expect("capture active store");
        let error = load_stored_view_records(&source).expect_err("active mode must fail closed");
        assert_eq!(error.class(), ReaderFailureClass::CoreFatal);
        assert_eq!(error.profile(), ReaderProfile::Active);
        assert_eq!(
            error.code(),
            ArchiveViewLoadErrorCode::ActiveRecordUnavailable
        );
    }

    #[test]
    fn manifest_and_record_set_damage_fail_the_whole_archive_operation() {
        let corrupt_manifest = TestRunDirectory::new("manifest-corrupt");
        let active_lease =
            ActiveArchiveLease::acquire(corrupt_manifest.path()).expect("acquire active lease");
        create_views(corrupt_manifest.path(), &["only"]);
        Connection::open(corrupt_manifest.database_path())
            .expect("open test database")
            .execute(
                "UPDATE diagnostic_view_manifest SET manifest_json = ?1 WHERE singleton = 1",
                [b"{}".as_slice()],
            )
            .expect("corrupt manifest JSON");
        drop(active_lease);
        let error = open_archive_catalog(&corrupt_manifest).expect_err("manifest must fail");
        assert_eq!(error.class(), ReaderFailureClass::ArchiveOperation);
        assert_eq!(error.code(), ArchiveViewLoadErrorCode::ManifestCorrupt);

        let inconsistent = TestRunDirectory::new("record-count");
        let active_lease =
            ActiveArchiveLease::acquire(inconsistent.path()).expect("acquire active lease");
        create_views(inconsistent.path(), &["first", "second"]);
        Connection::open(inconsistent.database_path())
            .expect("open test database")
            .execute(
                "DELETE FROM diagnostic_view_records WHERE view_id = 'second'",
                [],
            )
            .expect("remove one stored record");
        drop(active_lease);
        let error = open_archive_catalog(&inconsistent).expect_err("record set must fail");
        assert_eq!(error.class(), ReaderFailureClass::ArchiveOperation);
        assert_eq!(
            error.code(),
            ArchiveViewLoadErrorCode::RecordSetInconsistent
        );
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct AcceptAll;

    #[derive(Debug)]
    struct AcceptedReservation;

    impl AdmissionReservation for AcceptedReservation {
        fn commit(self, _event: AcceptedDiagnosticEvent) {}
    }

    impl AdmissionReserver for AcceptAll {
        type Error = Infallible;
        type Reservation = AcceptedReservation;

        fn try_reserve(&mut self, _size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
            Ok(AcceptedReservation)
        }
    }

    impl MandatoryDurableReserver for AcceptAll {}

    #[derive(Debug)]
    struct IgnoreLive;

    impl LiveEventNotifier for IgnoreLive {
        fn notify(&mut self, _event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
            Ok(())
        }
    }

    fn counter_event(identity: EventIdentity) -> DiagnosticEvent {
        let sequence = identity.sequence();
        DiagnosticEvent::CounterSampled(CounterSampled::new(
            DiagnosticEventHeader::new(
                identity.run_id(),
                sequence,
                ElapsedNs::new(sequence.get()),
                DiagnosticScope::new(None, None, None, None, None, None, None),
                Vec::new(),
            )
            .expect("valid test event header"),
            CounterKind::DiagnosticDroppedEvents,
            SchemaU64::new(sequence.get()),
        ))
    }

    fn append_counter(directory: &Path) {
        let store = DiagnosticStore::open_validated(directory, run_id())
            .expect("open store for canonical event append");
        let mut writer =
            TransactionalWriter::new(store, ()).expect("construct transactional writer");
        let hub = ProductionDiagnosticHub::production(run_id(), AcceptAll, Box::new(IgnoreLive));
        let accepted = hub
            .admit(counter_event, None)
            .expect("admit test event")
            .accepted()
            .clone();
        writer
            .commit_batch(&EventBatch::new(vec![accepted]).expect("one-event batch"))
            .expect("commit canonical event");
    }

    #[test]
    fn canonical_event_damage_remains_an_archive_operation_failure() {
        let directory = TestRunDirectory::new("event-corrupt");
        let active_lease =
            ActiveArchiveLease::acquire(directory.path()).expect("acquire active lease");
        drop(create_store(directory.path()));
        append_counter(directory.path());
        let compiled = CompiledViewSet::from_json_records([timeline_record("view")])
            .expect("compile test view");
        persist_view_set(directory.path(), run_id(), &compiled).expect("persist test view");
        let connection = Connection::open(directory.database_path()).expect("open test database");
        let guards = remove_event_write_guards(&connection);
        connection
            .execute(
                "UPDATE events SET canonical_json = ?1",
                [b"null".as_slice()],
            )
            .expect("corrupt canonical event JSON");
        restore_event_write_guards(&connection, guards);
        drop(connection);
        drop(active_lease);

        let mut reader = DiagnosticReader::open_archive(directory.path(), run_id())
            .expect("open archive reader");
        let error = reader
            .capture()
            .expect_err("Q00 must reject canonical event corruption");
        assert_eq!(error.class(), ReaderFailureClass::ArchiveOperation);
        assert_eq!(error.code(), ReaderErrorCode::StoreValidation);
        assert_eq!(
            error.store_code(),
            Some(StoreOpenErrorCode::EventIdentityMismatch)
        );
    }
}
