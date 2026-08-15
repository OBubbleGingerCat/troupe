use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
};

use rusqlite::{Connection, params};
use serde_json::Value;
use troupe_diagnostics_core::{event::DiagnosticEvent, id::CanonicalUuid};
use troupe_diagnostics_runtime::store::{
    connection::{
        DiagnosticStore, InitialStoreMetadata, InitialTransactionHook, OWNER_DIRECTORY_MODE,
        OWNER_FILE_MODE, RealStoreFileSystem, StoreFileSystem, StoreNodeHandle, StoreNodeMetadata,
        StoreOpenErrorCode,
    },
    key::SortableU64Key,
    schema::{self, DIAGNOSTIC_DATABASE_FILENAME},
};

const RUN_ID: &str = "12345678-1234-4234-9234-123456789abc";
const OTHER_RUN_ID: &str = "87654321-4321-4321-8321-cba987654321";
const EVENT_FIXTURE: &[u8] =
    include_bytes!("../../../../tests/fixtures/diagnostics/events/agent-message-delta.json");

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
static UMASK_LOCK: Mutex<()> = Mutex::new(());

unsafe extern "C" {
    fn umask(mask: u32) -> u32;
}

struct UmaskGuard(u32);

impl UmaskGuard {
    fn set(mask: u32) -> Self {
        // SAFETY: the process-global mutation is serialized and restored by Drop.
        Self(unsafe { umask(mask) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: this restores the value captured by the serialized set call.
        unsafe {
            umask(self.0);
        }
    }
}

struct TestRunDirectory(PathBuf);

impl TestRunDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "troupe-s01-store-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test Run directory");
        set_mode(&path, OWNER_DIRECTORY_MODE);
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

fn parse_run_id(value: &str) -> CanonicalUuid {
    CanonicalUuid::parse(value).expect("canonical test Run UUID")
}

fn initial_metadata(value: &str) -> InitialStoreMetadata {
    InitialStoreMetadata::new(
        parse_run_id(value),
        "2026-08-14T00:00:00Z",
        "configuration-sha256:test",
    )
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("inspect filesystem mode")
        .permissions()
        .mode()
        & 0o7777
}

fn set_mode(path: &Path, value: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(value)).expect("set filesystem mode");
}

fn sidecar_paths(database_path: &Path) -> [PathBuf; 2] {
    let filename = database_path
        .file_name()
        .and_then(|value| value.to_str())
        .expect("database filename");
    [
        database_path.with_file_name(format!("{filename}-wal")),
        database_path.with_file_name(format!("{filename}-shm")),
    ]
}

fn scoped_event() -> (DiagnosticEvent, Vec<u8>) {
    let mut fixture: Vec<Value> = serde_json::from_slice(EVENT_FIXTURE).expect("parse fixture");
    let mut value = fixture.remove(0);
    value["scope"] = serde_json::json!({
        "scene_id": "scene-1",
        "actor_id": "actor-1",
        "cue_id": "cue-1",
        "effect_id": "effect-1",
        "act_id": "act-1",
        "tool_call_id": "tool-call-1",
        "session_generation": "9223372036854775808"
    });
    let event: DiagnosticEvent = serde_json::from_value(value).expect("parse typed event");
    let canonical = serde_json::to_vec(&event).expect("encode canonical event");
    (event, canonical)
}

fn insert_event(
    connection: &Connection,
    event: &DiagnosticEvent,
    canonical: &[u8],
    stored_sequence: u64,
    stored_kind: &str,
) {
    let header = event.header();
    let scope = header.scope();
    let sequence = SortableU64Key::new(stored_sequence);
    let elapsed = SortableU64Key::new(header.elapsed_ns().get());
    let session_generation = scope
        .session_generation()
        .map(|value| SortableU64Key::new(value.get()));
    let session_generation_bytes = session_generation.map(SortableU64Key::into_bytes);
    let session_generation_decimal = session_generation.map(SortableU64Key::canonical_decimal);

    connection
        .execute(
            "INSERT INTO events (\
                sequence_key, sequence, run_id, event_schema_version, elapsed_key, elapsed_ns, \
                kind, scene_id, actor_id, cue_id, effect_id, act_id, tool_call_id, \
                session_generation_key, session_generation, canonical_json\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                sequence.as_bytes().as_slice(),
                sequence.canonical_decimal(),
                header.run_id().to_string(),
                header.schema_version(),
                elapsed.as_bytes().as_slice(),
                elapsed.canonical_decimal(),
                stored_kind,
                scope.scene_id().map(|value| value.as_str()),
                scope.actor_id().map(|value| value.as_str()),
                scope.cue_id().map(|value| value.as_str()),
                scope.effect_id().map(|value| value.as_str()),
                scope.act_id().map(|value| value.as_str()),
                scope.tool_call_id().map(|value| value.as_str()),
                session_generation_bytes.as_ref().map(<[u8; 8]>::as_slice),
                session_generation_decimal,
                canonical,
            ],
        )
        .expect("insert test event");
}

fn set_watermarks(connection: &Connection, value: u64) {
    let key = SortableU64Key::new(value);
    let decimal = key.canonical_decimal();
    connection
        .execute(
            "UPDATE run_metadata SET \
                committed_key = ?1, committed_sequence = ?2, \
                read_model_key = ?1, read_model_sequence = ?2 \
             WHERE singleton = 1",
            params![key.as_bytes().as_slice(), decimal],
        )
        .expect("advance test watermarks");
}

fn create_and_close(directory: &TestRunDirectory, run_id: &str) {
    let store = DiagnosticStore::create(directory.path(), &initial_metadata(run_id))
        .expect("create diagnostic store");
    drop(store);
}

#[test]
fn sortable_u64_keys_preserve_the_complete_unsigned_order() {
    let values = [0, 1, (1_u64 << 63) - 1, 1_u64 << 63, u64::MAX];
    let mut encoded = values
        .into_iter()
        .rev()
        .map(SortableU64Key::new)
        .collect::<Vec<_>>();
    encoded.sort();
    assert_eq!(
        encoded.iter().map(|key| key.get()).collect::<Vec<_>>(),
        values
    );

    let connection = Connection::open_in_memory().expect("open in-memory SQLite");
    connection
        .execute(
            "CREATE TABLE keys (value BLOB PRIMARY KEY) WITHOUT ROWID",
            [],
        )
        .expect("create key table");
    for key in encoded.iter().rev() {
        connection
            .execute(
                "INSERT INTO keys (value) VALUES (?1)",
                params![key.as_bytes().as_slice()],
            )
            .expect("insert key");
    }
    let mut statement = connection
        .prepare("SELECT value FROM keys ORDER BY value")
        .expect("prepare sorted query");
    let sqlite_values = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("query sorted keys")
        .map(|row| {
            SortableU64Key::from_slice(&row.expect("read key"))
                .expect("decode key")
                .get()
        })
        .collect::<Vec<_>>();
    assert_eq!(sqlite_values, values);

    for value in values {
        let decimal = value.to_string();
        assert_eq!(
            SortableU64Key::parse_canonical_decimal(&decimal)
                .expect("parse boundary")
                .get(),
            value
        );
    }
    for invalid in ["", "00", "01", "+1", "-1", "18446744073709551616"] {
        assert!(SortableU64Key::parse_canonical_decimal(invalid).is_err());
    }
}

#[test]
fn creates_a_durable_wal_store_and_recovers_a_canonical_dense_prefix() {
    let _lock = UMASK_LOCK.lock().expect("lock umask mutation");
    let directory = TestRunDirectory::new("create");
    set_mode(directory.path(), 0o1777);
    let _umask = UmaskGuard::set(0);

    let store = DiagnosticStore::create(directory.path(), &initial_metadata(RUN_ID))
        .expect("create diagnostic store");
    let database_path = directory.database_path();
    let sidecars = sidecar_paths(&database_path);

    assert_eq!(store.database_path(), database_path);
    assert_eq!(mode(directory.path()), OWNER_DIRECTORY_MODE);
    assert_eq!(mode(&database_path), OWNER_FILE_MODE);
    for sidecar in &sidecars {
        assert!(
            sidecar.is_file(),
            "missing SQLite sidecar {}",
            sidecar.display()
        );
        assert_eq!(mode(sidecar), OWNER_FILE_MODE);
    }

    let journal_mode: String = store
        .connection()
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("read journal mode");
    let synchronous: u32 = store
        .connection()
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .expect("read synchronous mode");
    let foreign_keys: u32 = store
        .connection()
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .expect("read foreign-key mode");
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    assert_eq!(synchronous, 2);
    assert_eq!(foreign_keys, 1);
    schema::validate(store.connection()).expect("validate installed schema");

    assert_eq!(store.metadata().run_id(), parse_run_id(RUN_ID));
    assert_eq!(store.metadata().committed_watermark().get(), 0);
    assert_eq!(store.metadata().read_model_watermark().get(), 0);
    assert!(!store.metadata().clean_shutdown());
    let metadata_row: (String, String, i64) = store
        .connection()
        .query_row(
            "SELECT committed_sequence, read_model_sequence, clean_shutdown \
             FROM run_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read initial metadata row");
    assert_eq!(metadata_row, ("0".into(), "0".into(), 0));

    let (event, canonical) = scoped_event();
    insert_event(
        store.connection(),
        &event,
        &canonical,
        1,
        event.kind().as_str(),
    );
    set_watermarks(store.connection(), 1);
    assert!(
        store
            .connection()
            .execute("UPDATE events SET kind = kind WHERE sequence = '1'", [])
            .is_err(),
        "committed events must reject updates"
    );
    assert!(
        store
            .connection()
            .execute("DELETE FROM events WHERE sequence = '1'", [])
            .is_err(),
        "committed events must reject deletes"
    );

    set_mode(directory.path(), 0o1777);
    set_mode(&database_path, 0o2666);
    for sidecar in &sidecars {
        set_mode(sidecar, 0o2666);
    }
    store
        .checkpoint_and_validate_files()
        .expect("checkpoint and tighten files");
    assert_eq!(mode(directory.path()), OWNER_DIRECTORY_MODE);
    assert_eq!(mode(&database_path), OWNER_FILE_MODE);
    for sidecar in &sidecars {
        assert_eq!(mode(sidecar), OWNER_FILE_MODE);
    }
    drop(store);

    set_mode(directory.path(), 0o1777);
    set_mode(&database_path, 0o2666);
    let reopened = DiagnosticStore::open_validated(directory.path(), parse_run_id(RUN_ID))
        .expect("reopen valid store");
    assert_eq!(reopened.metadata().committed_watermark().get(), 1);
    assert_eq!(reopened.metadata().read_model_watermark().get(), 1);
    assert_eq!(mode(directory.path()), OWNER_DIRECTORY_MODE);
    assert_eq!(mode(&database_path), OWNER_FILE_MODE);
}

struct RollBackInitialTransaction;

impl InitialTransactionHook for RollBackInitialTransaction {
    fn before_commit(&self, transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        transaction.execute_batch("ROLLBACK")
    }
}

#[test]
fn initial_commit_failure_leaves_no_durable_schema_or_identity() {
    let directory = TestRunDirectory::new("initial-rollback");
    let error = DiagnosticStore::create_with(
        Arc::new(RealStoreFileSystem),
        directory.path(),
        &initial_metadata(RUN_ID),
        &RollBackInitialTransaction,
    )
    .expect_err("force initial commit failure");
    assert_eq!(error.code(), StoreOpenErrorCode::InitialTransactionFailed);

    let connection =
        Connection::open(directory.database_path()).expect("open rolled-back database");
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read rolled-back schema version");
    let objects: u32 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .expect("count rolled-back schema objects");
    assert_eq!(version, 0);
    assert_eq!(objects, 0);
}

#[test]
fn reopen_rejects_schema_pragma_identity_and_database_corruption() {
    let schema_directory = TestRunDirectory::new("schema-corrupt");
    create_and_close(&schema_directory, RUN_ID);
    let connection = Connection::open(schema_directory.database_path()).expect("open schema store");
    connection
        .execute_batch("DROP INDEX events_kind_sequence")
        .expect("corrupt schema definition");
    drop(connection);
    let error = DiagnosticStore::open_validated(schema_directory.path(), parse_run_id(RUN_ID))
        .expect_err("reject schema definition drift");
    assert_eq!(error.code(), StoreOpenErrorCode::SchemaMismatch);

    let newer_directory = TestRunDirectory::new("newer-schema");
    create_and_close(&newer_directory, RUN_ID);
    let connection = Connection::open(newer_directory.database_path()).expect("open newer store");
    connection
        .pragma_update(None, "user_version", 2)
        .expect("set newer schema version");
    drop(connection);
    let error = DiagnosticStore::open_validated(newer_directory.path(), parse_run_id(RUN_ID))
        .expect_err("reject newer schema");
    assert_eq!(error.code(), StoreOpenErrorCode::NewerSchema);

    let pragma_directory = TestRunDirectory::new("pragma-mismatch");
    create_and_close(&pragma_directory, RUN_ID);
    let connection = Connection::open(pragma_directory.database_path()).expect("open pragma store");
    let mode: String = connection
        .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
        .expect("change journal mode");
    assert_eq!(mode.to_ascii_lowercase(), "delete");
    drop(connection);
    let error = DiagnosticStore::open_validated(pragma_directory.path(), parse_run_id(RUN_ID))
        .expect_err("reject non-WAL store");
    assert_eq!(error.code(), StoreOpenErrorCode::PragmaMismatch);

    let identity_directory = TestRunDirectory::new("identity-mismatch");
    create_and_close(&identity_directory, RUN_ID);
    let error =
        DiagnosticStore::open_validated(identity_directory.path(), parse_run_id(OTHER_RUN_ID))
            .expect_err("reject wrong Run identity");
    assert_eq!(error.code(), StoreOpenErrorCode::RunIdentityMismatch);

    let corrupt_directory = TestRunDirectory::new("database-corrupt");
    create_and_close(&corrupt_directory, RUN_ID);
    fs::write(corrupt_directory.database_path(), b"not a sqlite database")
        .expect("corrupt database bytes");
    set_mode(&corrupt_directory.database_path(), OWNER_FILE_MODE);
    let error = DiagnosticStore::open_validated(corrupt_directory.path(), parse_run_id(RUN_ID))
        .expect_err("reject corrupt database bytes");
    assert_eq!(error.code(), StoreOpenErrorCode::CorruptStore);
}

#[test]
fn reopen_rejects_gaps_and_event_index_columns_that_disagree_with_canonical_json() {
    let gap_directory = TestRunDirectory::new("dense-gap");
    let store = DiagnosticStore::create(gap_directory.path(), &initial_metadata(RUN_ID))
        .expect("create gap store");
    let (event, canonical) = scoped_event();
    insert_event(
        store.connection(),
        &event,
        &canonical,
        2,
        event.kind().as_str(),
    );
    set_watermarks(store.connection(), 2);
    drop(store);
    let error = DiagnosticStore::open_validated(gap_directory.path(), parse_run_id(RUN_ID))
        .expect_err("reject sequence gap");
    assert_eq!(error.code(), StoreOpenErrorCode::DensePrefixViolation);

    let kind_directory = TestRunDirectory::new("event-kind-mismatch");
    let store = DiagnosticStore::create(kind_directory.path(), &initial_metadata(RUN_ID))
        .expect("create kind mismatch store");
    insert_event(store.connection(), &event, &canonical, 1, "counter_sampled");
    set_watermarks(store.connection(), 1);
    drop(store);
    let error = DiagnosticStore::open_validated(kind_directory.path(), parse_run_id(RUN_ID))
        .expect_err("reject kind mismatch");
    assert_eq!(error.code(), StoreOpenErrorCode::EventIdentityMismatch);

    let encoding_directory = TestRunDirectory::new("event-encoding-mismatch");
    let store = DiagnosticStore::create(encoding_directory.path(), &initial_metadata(RUN_ID))
        .expect("create encoding mismatch store");
    let noncanonical = serde_json::to_string_pretty(&event)
        .expect("encode noncanonical event")
        .into_bytes();
    insert_event(
        store.connection(),
        &event,
        &noncanonical,
        1,
        event.kind().as_str(),
    );
    set_watermarks(store.connection(), 1);
    drop(store);
    let error = DiagnosticStore::open_validated(encoding_directory.path(), parse_run_id(RUN_ID))
        .expect_err("reject noncanonical event bytes");
    assert_eq!(error.code(), StoreOpenErrorCode::EventIdentityMismatch);
}

#[test]
fn reopen_rejects_materialized_keys_beyond_the_read_model_watermark() {
    let directory = TestRunDirectory::new("materialized-watermark");
    let store = DiagnosticStore::create(directory.path(), &initial_metadata(RUN_ID))
        .expect("create materialized store");
    let key = SortableU64Key::new(1);
    store
        .connection()
        .execute(
            "INSERT INTO materialized_messages (\
                message_id, model_schema_version, latest_sequence_key, latest_sequence, payload_json\
             ) VALUES ('message-1', 1, ?1, '1', ?2)",
            params![key.as_bytes().as_slice(), b"{}".as_slice()],
        )
        .expect("insert impossible materialized row");
    drop(store);

    let error = DiagnosticStore::open_validated(directory.path(), parse_run_id(RUN_ID))
        .expect_err("reject materialized state beyond W");
    assert_eq!(error.code(), StoreOpenErrorCode::WatermarkMismatch);
}

#[derive(Clone, Copy)]
enum FaultPoint {
    Metadata,
    SetMode,
    Sync,
}

struct FaultFileSystem {
    point: FaultPoint,
}

impl FaultFileSystem {
    const fn new(point: FaultPoint) -> Self {
        Self { point }
    }

    fn wrap(&self, handle: Box<dyn StoreNodeHandle>) -> Box<dyn StoreNodeHandle> {
        Box::new(FaultHandle {
            inner: handle,
            point: self.point,
        })
    }
}

impl StoreFileSystem for FaultFileSystem {
    fn path_metadata(&self, path: &Path) -> io::Result<Option<StoreNodeMetadata>> {
        RealStoreFileSystem.path_metadata(path)
    }

    fn open_directory(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>> {
        RealStoreFileSystem
            .open_directory(path)
            .map(|handle| self.wrap(handle))
    }

    fn create_database(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>> {
        RealStoreFileSystem
            .create_database(path)
            .map(|handle| self.wrap(handle))
    }

    fn open_file(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>> {
        RealStoreFileSystem
            .open_file(path)
            .map(|handle| self.wrap(handle))
    }
}

struct FaultHandle {
    inner: Box<dyn StoreNodeHandle>,
    point: FaultPoint,
}

impl StoreNodeHandle for FaultHandle {
    fn metadata(&self) -> io::Result<StoreNodeMetadata> {
        if matches!(self.point, FaultPoint::Metadata) {
            Err(io::Error::other("injected fstat failure"))
        } else {
            self.inner.metadata()
        }
    }

    fn set_mode(&self, mode: u32) -> io::Result<()> {
        if matches!(self.point, FaultPoint::SetMode) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected fchmod failure",
            ))
        } else {
            self.inner.set_mode(mode)
        }
    }

    fn sync_all(&self) -> io::Result<()> {
        if matches!(self.point, FaultPoint::Sync) {
            Err(io::Error::other("injected fsync failure"))
        } else {
            self.inner.sync_all()
        }
    }
}

#[test]
fn permission_metadata_and_sync_failures_are_fatal() {
    for (label, point, expected) in [
        (
            "fstat-fault",
            FaultPoint::Metadata,
            StoreOpenErrorCode::FileInspectFailed,
        ),
        (
            "fchmod-fault",
            FaultPoint::SetMode,
            StoreOpenErrorCode::FilePermissionFailed,
        ),
        (
            "fsync-fault",
            FaultPoint::Sync,
            StoreOpenErrorCode::FileSyncFailed,
        ),
    ] {
        let directory = TestRunDirectory::new(label);
        let error = DiagnosticStore::create_with(
            Arc::new(FaultFileSystem::new(point)),
            directory.path(),
            &initial_metadata(RUN_ID),
            &(),
        )
        .expect_err("injected filesystem failure must be fatal");
        assert_eq!(error.code(), expected, "wrong code for {label}");
    }
}

#[test]
fn distinct_runs_do_not_share_database_wal_or_connection_state() {
    let first_directory = TestRunDirectory::new("run-one");
    let second_directory = TestRunDirectory::new("run-two");
    let first = DiagnosticStore::create(first_directory.path(), &initial_metadata(RUN_ID))
        .expect("create first Run store");
    let second = DiagnosticStore::create(second_directory.path(), &initial_metadata(OTHER_RUN_ID))
        .expect("create second Run store");

    assert_ne!(first.database_path(), second.database_path());
    assert_ne!(
        sidecar_paths(first.database_path()),
        sidecar_paths(second.database_path())
    );
    assert_eq!(first.metadata().run_id(), parse_run_id(RUN_ID));
    assert_eq!(second.metadata().run_id(), parse_run_id(OTHER_RUN_ID));
    let collision =
        DiagnosticStore::create(first_directory.path(), &initial_metadata(OTHER_RUN_ID))
            .expect_err("one Run directory must not accept a second database identity");
    assert_eq!(collision.code(), StoreOpenErrorCode::DatabaseAlreadyExists);

    let (event, canonical) = scoped_event();
    insert_event(
        first.connection(),
        &event,
        &canonical,
        1,
        event.kind().as_str(),
    );
    let first_count: u32 = first
        .connection()
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .expect("count first Run events");
    let second_count: u32 = second
        .connection()
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .expect("count second Run events");
    assert_eq!(first_count, 1);
    assert_eq!(second_count, 0);
}
