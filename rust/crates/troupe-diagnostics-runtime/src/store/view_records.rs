use std::{collections::HashSet, fmt, path::Path};

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use troupe_diagnostics_core::{
    id::CanonicalUuid,
    view_protocol::{Renderer, VIEW_SCHEMA_VERSION, ViewRecord},
};

use super::connection::DiagnosticStore;

pub const VIEW_MANIFEST_SCHEMA_VERSION: u8 = 1;
pub const MAX_VIEW_RECORDS: usize = 64;
pub const MAX_VIEW_RECORD_BYTES: usize = 256 * 1024;
pub const MAX_TOTAL_VIEW_RECORD_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_VIEW_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewManifestEntry {
    id: String,
    ordinal: usize,
    renderer: Renderer,
    view_schema_version: u8,
}

impl ViewManifestEntry {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub const fn renderer(&self) -> Renderer {
        self.renderer
    }

    pub const fn view_schema_version(&self) -> u8 {
        self.view_schema_version
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewManifest {
    manifest_schema_version: u8,
    views: Vec<ViewManifestEntry>,
}

impl ViewManifest {
    pub const fn manifest_schema_version(&self) -> u8 {
        self.manifest_schema_version
    }

    pub fn views(&self) -> &[ViewManifestEntry] {
        &self.views
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledViewRecord {
    record: ViewRecord,
    canonical_json: Box<[u8]>,
}

impl CompiledViewRecord {
    pub fn id(&self) -> &str {
        self.record.id()
    }

    pub const fn renderer(&self) -> Renderer {
        self.record.renderer()
    }

    pub const fn view_schema_version(&self) -> u8 {
        VIEW_SCHEMA_VERSION
    }

    pub const fn record(&self) -> &ViewRecord {
        &self.record
    }

    pub fn canonical_json(&self) -> &[u8] {
        &self.canonical_json
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledViewSet {
    records: Vec<CompiledViewRecord>,
    total_record_bytes: usize,
    manifest: ViewManifest,
    manifest_json: Box<[u8]>,
}

impl CompiledViewSet {
    pub fn from_json_records<I, B>(records: I) -> Result<Self, ViewSetCompileError>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let mut compiled = Vec::new();
        let mut ids = HashSet::new();
        let mut total_record_bytes = 0_usize;

        for (ordinal, bytes) in records.into_iter().enumerate() {
            if ordinal == MAX_VIEW_RECORDS {
                return Err(ViewSetCompileError::new(
                    ViewSetCompileErrorCode::TooManyRecords,
                    Some(ordinal),
                    "diagnostic view count exceeds the V1 limit",
                ));
            }
            let bytes = bytes.as_ref();
            if bytes.len() > MAX_VIEW_RECORD_BYTES {
                return Err(ViewSetCompileError::new(
                    ViewSetCompileErrorCode::RecordTooLarge,
                    Some(ordinal),
                    "diagnostic view record exceeds the V1 byte limit",
                ));
            }
            total_record_bytes = total_record_bytes.checked_add(bytes.len()).ok_or_else(|| {
                ViewSetCompileError::new(
                    ViewSetCompileErrorCode::TotalBytesExceeded,
                    Some(ordinal),
                    "diagnostic view record bytes overflowed",
                )
            })?;
            if total_record_bytes > MAX_TOTAL_VIEW_RECORD_BYTES {
                return Err(ViewSetCompileError::new(
                    ViewSetCompileErrorCode::TotalBytesExceeded,
                    Some(ordinal),
                    "diagnostic view records exceed the V1 total byte limit",
                ));
            }

            let record: ViewRecord = serde_json::from_slice(bytes).map_err(|error| {
                ViewSetCompileError::new(
                    ViewSetCompileErrorCode::InvalidRecord,
                    Some(ordinal),
                    error.to_string(),
                )
            })?;
            let canonical = serde_json::to_vec(&record).map_err(|error| {
                ViewSetCompileError::new(
                    ViewSetCompileErrorCode::InvalidRecord,
                    Some(ordinal),
                    error.to_string(),
                )
            })?;
            if canonical != bytes {
                return Err(ViewSetCompileError::new(
                    ViewSetCompileErrorCode::NonCanonicalRecord,
                    Some(ordinal),
                    "diagnostic view record is not canonical C05 JSON",
                ));
            }
            if !ids.insert(record.id().to_owned()) {
                return Err(ViewSetCompileError::new(
                    ViewSetCompileErrorCode::DuplicateId,
                    Some(ordinal),
                    format!("duplicate diagnostic view ID {:?}", record.id()),
                ));
            }
            compiled.push(CompiledViewRecord {
                record,
                canonical_json: canonical.into_boxed_slice(),
            });
        }

        let views = compiled
            .iter()
            .enumerate()
            .map(|(ordinal, record)| ViewManifestEntry {
                id: record.id().to_owned(),
                ordinal,
                renderer: record.renderer(),
                view_schema_version: record.view_schema_version(),
            })
            .collect();
        let manifest = ViewManifest {
            manifest_schema_version: VIEW_MANIFEST_SCHEMA_VERSION,
            views,
        };
        let manifest_json = serde_json::to_vec(&manifest).map_err(|error| {
            ViewSetCompileError::new(
                ViewSetCompileErrorCode::ManifestTooLarge,
                None,
                error.to_string(),
            )
        })?;
        if manifest_json.len() > MAX_VIEW_MANIFEST_BYTES {
            return Err(ViewSetCompileError::new(
                ViewSetCompileErrorCode::ManifestTooLarge,
                None,
                "diagnostic view manifest exceeds the V1 byte limit",
            ));
        }

        Ok(Self {
            records: compiled,
            total_record_bytes,
            manifest,
            manifest_json: manifest_json.into_boxed_slice(),
        })
    }

    pub fn records(&self) -> &[CompiledViewRecord] {
        &self.records
    }

    pub const fn total_record_bytes(&self) -> usize {
        self.total_record_bytes
    }

    pub const fn manifest(&self) -> &ViewManifest {
        &self.manifest
    }

    pub fn manifest_json(&self) -> &[u8] {
        &self.manifest_json
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewSetCompileErrorCode {
    TooManyRecords,
    RecordTooLarge,
    TotalBytesExceeded,
    InvalidRecord,
    NonCanonicalRecord,
    DuplicateId,
    ManifestTooLarge,
}

impl ViewSetCompileErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooManyRecords => "diagnostic_views.too_many_records",
            Self::RecordTooLarge => "diagnostic_views.record_too_large",
            Self::TotalBytesExceeded => "diagnostic_views.total_bytes_exceeded",
            Self::InvalidRecord => "diagnostic_views.invalid_record",
            Self::NonCanonicalRecord => "diagnostic_views.noncanonical_record",
            Self::DuplicateId => "diagnostic_views.duplicate_id",
            Self::ManifestTooLarge => "diagnostic_views.manifest_too_large",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewSetCompileError {
    code: ViewSetCompileErrorCode,
    ordinal: Option<usize>,
    message: String,
}

impl ViewSetCompileError {
    fn new(
        code: ViewSetCompileErrorCode,
        ordinal: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            ordinal,
            message: message.into(),
        }
    }

    pub const fn code(&self) -> ViewSetCompileErrorCode {
        self.code
    }

    pub const fn ordinal(&self) -> Option<usize> {
        self.ordinal
    }
}

impl fmt::Display for ViewSetCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic view compilation failed [{}]",
            self.code.as_str()
        )?;
        if let Some(ordinal) = self.ordinal {
            write!(formatter, " at ordinal {ordinal}")?;
        }
        write!(formatter, ": {}", self.message)
    }
}

impl std::error::Error for ViewSetCompileError {}

pub trait ViewRecordTransactionHook: Send + Sync {
    fn before_commit(&self, transaction: &Transaction<'_>) -> rusqlite::Result<()>;
}

impl ViewRecordTransactionHook for () {
    fn before_commit(&self, _transaction: &Transaction<'_>) -> rusqlite::Result<()> {
        Ok(())
    }
}

pub fn persist_view_set(
    run_directory: &Path,
    run_id: CanonicalUuid,
    compiled: &CompiledViewSet,
) -> Result<(), ViewRecordStoreError> {
    persist_view_set_with_hook(run_directory, run_id, compiled, &())
}

pub fn persist_view_set_with_hook(
    run_directory: &Path,
    run_id: CanonicalUuid,
    compiled: &CompiledViewSet,
    hook: &dyn ViewRecordTransactionHook,
) -> Result<(), ViewRecordStoreError> {
    let mut store = DiagnosticStore::open_validated(run_directory, run_id).map_err(|error| {
        ViewRecordStoreError::new(ViewRecordStoreErrorCode::StoreOpen, error.to_string())
    })?;
    let transaction = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            ViewRecordStoreError::new(
                ViewRecordStoreErrorCode::BeginTransaction,
                error.to_string(),
            )
        })?;
    let existing = transaction
        .query_row(
            "SELECT (SELECT count(*) FROM diagnostic_view_manifest), \
                    (SELECT count(*) FROM diagnostic_view_records)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|error| {
            ViewRecordStoreError::new(ViewRecordStoreErrorCode::ReadExisting, error.to_string())
        })?;
    if existing != (0, 0) {
        return Err(ViewRecordStoreError::new(
            ViewRecordStoreErrorCode::AlreadyPersisted,
            "diagnostic view records are already present",
        ));
    }

    for (ordinal, record) in compiled.records().iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO diagnostic_view_records (\
                    view_id, ordinal, view_schema_version, renderer, record_json\
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    record.id(),
                    i64::try_from(ordinal).expect("bounded view ordinal fits SQLite INTEGER"),
                    record.view_schema_version(),
                    record.renderer().as_str(),
                    record.canonical_json(),
                ],
            )
            .map_err(|error| {
                ViewRecordStoreError::new(ViewRecordStoreErrorCode::Statement, error.to_string())
            })?;
    }
    transaction
        .execute(
            "INSERT INTO diagnostic_view_manifest (\
                singleton, manifest_schema_version, record_count, manifest_json\
             ) VALUES (1, ?1, ?2, ?3)",
            params![
                compiled.manifest().manifest_schema_version(),
                i64::try_from(compiled.records().len())
                    .expect("bounded view count fits SQLite INTEGER"),
                compiled.manifest_json(),
            ],
        )
        .map_err(|error| {
            ViewRecordStoreError::new(ViewRecordStoreErrorCode::Statement, error.to_string())
        })?;
    hook.before_commit(&transaction).map_err(|error| {
        ViewRecordStoreError::new(ViewRecordStoreErrorCode::BeforeCommit, error.to_string())
    })?;
    transaction.commit().map_err(|error| {
        ViewRecordStoreError::new(ViewRecordStoreErrorCode::Commit, error.to_string())
    })?;

    validate_persisted_view_set(store.connection(), compiled)
}

fn validate_persisted_view_set(
    connection: &rusqlite::Connection,
    compiled: &CompiledViewSet,
) -> Result<(), ViewRecordStoreError> {
    let manifest = connection
        .query_row(
            "SELECT manifest_schema_version, record_count, manifest_json \
             FROM diagnostic_view_manifest WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, u8>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            ViewRecordStoreError::new(
                ViewRecordStoreErrorCode::PostCommitValidation,
                error.to_string(),
            )
        })?;
    let expected_count =
        i64::try_from(compiled.records().len()).expect("bounded view count fits SQLite INTEGER");
    if manifest.as_ref().is_none_or(|(version, count, bytes)| {
        *version != VIEW_MANIFEST_SCHEMA_VERSION
            || *count != expected_count
            || bytes.as_slice() != compiled.manifest_json()
    }) {
        return Err(ViewRecordStoreError::new(
            ViewRecordStoreErrorCode::PostCommitValidation,
            "persisted diagnostic view manifest differs from the committed candidate",
        ));
    }

    let mut statement = connection
        .prepare(
            "SELECT view_id, ordinal, view_schema_version, renderer, record_json \
             FROM diagnostic_view_records ORDER BY ordinal",
        )
        .map_err(|error| {
            ViewRecordStoreError::new(
                ViewRecordStoreErrorCode::PostCommitValidation,
                error.to_string(),
            )
        })?;
    let persisted = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, u8>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })
        .map_err(|error| {
            ViewRecordStoreError::new(
                ViewRecordStoreErrorCode::PostCommitValidation,
                error.to_string(),
            )
        })?;
    let persisted = persisted
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| {
            ViewRecordStoreError::new(
                ViewRecordStoreErrorCode::PostCommitValidation,
                error.to_string(),
            )
        })?;
    if persisted.len() != compiled.records().len()
        || persisted.iter().zip(compiled.records()).enumerate().any(
            |(ordinal, (actual, expected))| {
                actual.0 != expected.id()
                    || actual.1
                        != i64::try_from(ordinal).expect("bounded view ordinal fits SQLite INTEGER")
                    || actual.2 != expected.view_schema_version()
                    || actual.3 != expected.renderer().as_str()
                    || actual.4.as_slice() != expected.canonical_json()
            },
        )
    {
        return Err(ViewRecordStoreError::new(
            ViewRecordStoreErrorCode::PostCommitValidation,
            "persisted diagnostic view records differ from the committed candidate",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewRecordStoreErrorCode {
    StoreOpen,
    BeginTransaction,
    ReadExisting,
    AlreadyPersisted,
    Statement,
    BeforeCommit,
    Commit,
    PostCommitValidation,
}

impl ViewRecordStoreErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StoreOpen => "diagnostic_views.store_open",
            Self::BeginTransaction => "diagnostic_views.begin_transaction",
            Self::ReadExisting => "diagnostic_views.read_existing",
            Self::AlreadyPersisted => "diagnostic_views.already_persisted",
            Self::Statement => "diagnostic_views.statement",
            Self::BeforeCommit => "diagnostic_views.before_commit",
            Self::Commit => "diagnostic_views.commit",
            Self::PostCommitValidation => "diagnostic_views.post_commit_validation",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewRecordStoreError {
    code: ViewRecordStoreErrorCode,
    message: String,
}

impl ViewRecordStoreError {
    fn new(code: ViewRecordStoreErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub const fn code(&self) -> ViewRecordStoreErrorCode {
        self.code
    }
}

impl fmt::Display for ViewRecordStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic view persistence failed [{}]: {}",
            self.code.as_str(),
            self.message
        )
    }
}

impl std::error::Error for ViewRecordStoreError {}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use rusqlite::Connection;
    use serde_json::json;
    use troupe_diagnostics_core::id::CanonicalUuid;

    use crate::store::connection::{DiagnosticStore, InitialStoreMetadata};

    use super::*;

    const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestRunDirectory(PathBuf);

    impl TestRunDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "troupe-b08-view-records-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test Run directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
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

    fn create_store(directory: &Path) {
        drop(
            DiagnosticStore::create(
                directory,
                &InitialStoreMetadata::new(
                    run_id(),
                    "2026-08-16T00:00:00Z",
                    "configuration-sha256:b08",
                ),
            )
            .expect("create diagnostic store"),
        );
    }

    fn timeline_record(id: &str) -> Vec<u8> {
        let record: troupe_diagnostics_core::view_protocol::ViewRecord =
            serde_json::from_value(json!({
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
            .expect("parse typed view record");
        serde_json::to_vec(&record).expect("encode canonical view record")
    }

    #[test]
    fn compiles_canonical_records_into_a_stable_bounded_manifest() {
        let records = [timeline_record("first"), timeline_record("second")];
        let compiled = CompiledViewSet::from_json_records(records.iter().map(Vec::as_slice))
            .expect("compile valid view records");

        assert_eq!(compiled.records().len(), 2);
        assert_eq!(
            compiled.total_record_bytes(),
            records.iter().map(Vec::len).sum::<usize>()
        );
        assert_eq!(compiled.records()[0].id(), "first");
        assert_eq!(compiled.records()[0].renderer().as_str(), "timeline");
        assert_eq!(compiled.records()[0].canonical_json(), records[0]);
        assert_eq!(compiled.records()[1].id(), "second");
        assert_eq!(compiled.manifest().manifest_schema_version(), 1);
        assert_eq!(compiled.manifest().views().len(), 2);
        assert_eq!(compiled.manifest().views()[0].ordinal(), 0);
        assert_eq!(compiled.manifest().views()[1].ordinal(), 1);
        assert_eq!(compiled.manifest().views()[1].id(), "second");
        assert!(compiled.manifest_json().len() <= MAX_VIEW_MANIFEST_BYTES);
        assert_eq!(
            serde_json::to_vec(compiled.manifest()).expect("encode manifest"),
            compiled.manifest_json()
        );
    }

    #[test]
    fn rejects_duplicate_noncanonical_incompatible_and_resource_exhausted_sets_atomically() {
        let duplicate =
            CompiledViewSet::from_json_records([timeline_record("same"), timeline_record("same")]);
        assert_eq!(
            duplicate.expect_err("duplicate view ID").code(),
            ViewSetCompileErrorCode::DuplicateId
        );

        let mut noncanonical = timeline_record("spaced");
        noncanonical.push(b' ');
        assert_eq!(
            CompiledViewSet::from_json_records([noncanonical])
                .expect_err("noncanonical JSON")
                .code(),
            ViewSetCompileErrorCode::NonCanonicalRecord
        );

        let mut incompatible: serde_json::Value =
            serde_json::from_slice(&timeline_record("future")).expect("parse record");
        incompatible["view_schema_version"] = json!(2);
        assert_eq!(
            CompiledViewSet::from_json_records([
                serde_json::to_vec(&incompatible).expect("encode future record")
            ])
            .expect_err("future schema is incompatible")
            .code(),
            ViewSetCompileErrorCode::InvalidRecord
        );

        let too_many = (0..=MAX_VIEW_RECORDS)
            .map(|ordinal| timeline_record(&format!("view_{ordinal}")))
            .collect::<Vec<_>>();
        assert_eq!(
            CompiledViewSet::from_json_records(too_many)
                .expect_err("view count must be bounded")
                .code(),
            ViewSetCompileErrorCode::TooManyRecords
        );

        assert_eq!(
            CompiledViewSet::from_json_records([vec![b'x'; MAX_VIEW_RECORD_BYTES + 1]])
                .expect_err("individual record must be bounded")
                .code(),
            ViewSetCompileErrorCode::RecordTooLarge
        );

        let oversized_total = (0..20)
            .map(|ordinal| {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&timeline_record(&format!("large_{ordinal}")))
                        .expect("parse timeline record");
                value["query"]["source"]["selector"] =
                    json!({"selector": "custom", "name": "example.operation"});
                value["query"]["filters"] = json!([{
                    "filter": "attribute_equals",
                    "key": "payload",
                    "value": {"type": "string", "value": "x".repeat(220 * 1024)}
                }]);
                let record: troupe_diagnostics_core::view_protocol::ViewRecord =
                    serde_json::from_value(value).expect("parse large typed view record");
                let bytes = serde_json::to_vec(&record).expect("encode large view record");
                assert!(bytes.len() <= MAX_VIEW_RECORD_BYTES);
                bytes
            })
            .collect::<Vec<_>>();
        assert_eq!(
            CompiledViewSet::from_json_records(oversized_total)
                .expect_err("total view record bytes must be bounded")
                .code(),
            ViewSetCompileErrorCode::TotalBytesExceeded
        );
    }

    #[test]
    fn persists_manifest_and_records_in_one_transaction_without_advancing_watermarks() {
        let directory = TestRunDirectory::new("persist");
        create_store(directory.path());
        let records = [timeline_record("first"), timeline_record("second")];
        let compiled = CompiledViewSet::from_json_records(records.iter().map(Vec::as_slice))
            .expect("compile valid view records");

        persist_view_set(directory.path(), run_id(), &compiled).expect("persist view set");

        let store = DiagnosticStore::open_validated(directory.path(), run_id())
            .expect("reopen persisted store");
        assert_eq!(store.metadata().committed_watermark().get(), 0);
        assert_eq!(store.metadata().read_model_watermark().get(), 0);
        assert!(!store.metadata().clean_shutdown());
        let manifest: (i64, i64, Vec<u8>) = store
            .connection()
            .query_row(
                "SELECT manifest_schema_version, record_count, manifest_json \
                 FROM diagnostic_view_manifest WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read persisted manifest");
        assert_eq!(manifest.0, 1);
        assert_eq!(manifest.1, 2);
        assert_eq!(manifest.2, compiled.manifest_json());

        let mut statement = store
            .connection()
            .prepare(
                "SELECT view_id, ordinal, view_schema_version, renderer, record_json \
                 FROM diagnostic_view_records ORDER BY ordinal",
            )
            .expect("prepare persisted records query");
        let persisted = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })
            .expect("query persisted records")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect persisted records");
        assert_eq!(
            persisted[0],
            ("first".into(), 0, 1, "timeline".into(), records[0].clone())
        );
        assert_eq!(
            persisted[1],
            ("second".into(), 1, 1, "timeline".into(), records[1].clone())
        );

        assert_eq!(
            persist_view_set(directory.path(), run_id(), &compiled)
                .expect_err("view set is one-shot")
                .code(),
            ViewRecordStoreErrorCode::AlreadyPersisted
        );
    }

    struct FailBeforeCommit;

    impl ViewRecordTransactionHook for FailBeforeCommit {
        fn before_commit(&self, _transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
            Err(rusqlite::Error::InvalidQuery)
        }
    }

    #[test]
    fn transaction_failure_leaves_no_partial_manifest_or_record() {
        let directory = TestRunDirectory::new("rollback");
        create_store(directory.path());
        let compiled = CompiledViewSet::from_json_records([timeline_record("only")])
            .expect("compile valid view record");

        assert_eq!(
            persist_view_set_with_hook(directory.path(), run_id(), &compiled, &FailBeforeCommit,)
                .expect_err("force transaction rollback")
                .code(),
            ViewRecordStoreErrorCode::BeforeCommit
        );

        let connection = Connection::open(
            directory
                .path()
                .join(crate::store::schema::DIAGNOSTIC_DATABASE_FILENAME),
        )
        .expect("open store after rollback");
        let manifest_count: i64 = connection
            .query_row("SELECT count(*) FROM diagnostic_view_manifest", [], |row| {
                row.get(0)
            })
            .expect("count manifests");
        let record_count: i64 = connection
            .query_row("SELECT count(*) FROM diagnostic_view_records", [], |row| {
                row.get(0)
            })
            .expect("count records");
        assert_eq!((manifest_count, record_count), (0, 0));
    }
}
