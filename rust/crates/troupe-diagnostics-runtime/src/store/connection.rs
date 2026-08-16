use std::{
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};
use troupe_diagnostics_core::{
    event::{DiagnosticEvent, EVENT_SCHEMA_VERSION},
    id::CanonicalUuid,
    validate::ReferenceValidator,
};

use super::{
    key::SortableU64Key,
    schema::{
        self, DIAGNOSTIC_DATABASE_FILENAME, STORE_SCHEMA_IDENTITY, STORE_SCHEMA_VERSION,
        SchemaValidationError,
    },
};

pub const OWNER_DIRECTORY_MODE: u32 = 0o700;
pub const OWNER_FILE_MODE: u32 = 0o600;

pub(crate) fn open_immutable_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let mut uri = url::Url::from_file_path(path)
        .map_err(|()| rusqlite::Error::InvalidPath(path.to_path_buf()))?;
    uri.query_pairs_mut().append_pair("immutable", "1");
    Connection::open_with_flags(
        uri.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreNodeKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreNodeMetadata {
    kind: StoreNodeKind,
    mode: u32,
    identity: FileIdentity,
}

impl StoreNodeMetadata {
    fn from_metadata(metadata: &Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            StoreNodeKind::Symlink
        } else if file_type.is_dir() {
            StoreNodeKind::Directory
        } else if file_type.is_file() {
            StoreNodeKind::File
        } else {
            StoreNodeKind::Other
        };
        Self {
            kind,
            mode: metadata_mode(metadata, kind),
            identity: FileIdentity::from_metadata(metadata),
        }
    }

    pub const fn kind(self) -> StoreNodeKind {
        self.kind
    }

    pub const fn mode(self) -> u32 {
        self.mode
    }

    pub const fn same_identity(self, other: Self) -> bool {
        self.identity.first == other.identity.first && self.identity.second == other.identity.second
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    first: u64,
    second: u64,
}

impl FileIdentity {
    #[cfg(unix)]
    fn from_metadata(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            first: metadata.dev(),
            second: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            first: metadata.len(),
            second: u64::from(metadata.permissions().readonly()),
        }
    }
}

pub trait StoreNodeHandle: Send + Sync {
    fn metadata(&self) -> io::Result<StoreNodeMetadata>;
    fn set_mode(&self, mode: u32) -> io::Result<()>;
    fn sync_all(&self) -> io::Result<()>;
}

pub trait StoreFileSystem: Send + Sync {
    fn path_metadata(&self, path: &Path) -> io::Result<Option<StoreNodeMetadata>>;
    fn open_directory(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>>;
    fn create_database(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>>;
    fn open_file(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealStoreFileSystem;

impl StoreFileSystem for RealStoreFileSystem {
    fn path_metadata(&self, path: &Path) -> io::Result<Option<StoreNodeMetadata>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(StoreNodeMetadata::from_metadata(&metadata))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn open_directory(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>> {
        File::open(path).map(|file| Box::new(RealStoreNodeHandle(file)) as _)
    }

    fn create_database(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.mode(OWNER_FILE_MODE);
        }
        options
            .open(path)
            .map(|file| Box::new(RealStoreNodeHandle(file)) as _)
    }

    fn open_file(&self, path: &Path) -> io::Result<Box<dyn StoreNodeHandle>> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(|file| Box::new(RealStoreNodeHandle(file)) as _)
    }
}

struct RealStoreNodeHandle(File);

impl StoreNodeHandle for RealStoreNodeHandle {
    fn metadata(&self) -> io::Result<StoreNodeMetadata> {
        self.0
            .metadata()
            .map(|metadata| StoreNodeMetadata::from_metadata(&metadata))
    }

    fn set_mode(&self, mode: u32) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            self.0.set_permissions(fs::Permissions::from_mode(mode))
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            Ok(())
        }
    }

    fn sync_all(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitialStoreMetadata {
    run_id: CanonicalUuid,
    started_at: String,
    configuration_identity: String,
}

impl InitialStoreMetadata {
    pub fn new(
        run_id: CanonicalUuid,
        started_at: impl Into<String>,
        configuration_identity: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            started_at: started_at.into(),
            configuration_identity: configuration_identity.into(),
        }
    }

    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    pub fn configuration_identity(&self) -> &str {
        &self.configuration_identity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedStoreMetadata {
    run_id: CanonicalUuid,
    started_at: String,
    configuration_identity: String,
    committed_watermark: SortableU64Key,
    read_model_watermark: SortableU64Key,
    clean_shutdown: bool,
}

impl PersistedStoreMetadata {
    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    pub fn configuration_identity(&self) -> &str {
        &self.configuration_identity
    }

    pub const fn committed_watermark(&self) -> SortableU64Key {
        self.committed_watermark
    }

    pub const fn read_model_watermark(&self) -> SortableU64Key {
        self.read_model_watermark
    }

    pub const fn clean_shutdown(&self) -> bool {
        self.clean_shutdown
    }
}

pub trait InitialTransactionHook: Send + Sync {
    fn before_commit(&self, transaction: &Transaction<'_>) -> rusqlite::Result<()>;
}

impl InitialTransactionHook for () {
    fn before_commit(&self, _transaction: &Transaction<'_>) -> rusqlite::Result<()> {
        Ok(())
    }
}

pub struct DiagnosticStore {
    connection: Connection,
    filesystem: Arc<dyn StoreFileSystem>,
    run_directory: PathBuf,
    database_path: PathBuf,
    metadata: PersistedStoreMetadata,
}

impl fmt::Debug for DiagnosticStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticStore")
            .field("run_directory", &self.run_directory)
            .field("database_path", &self.database_path)
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl DiagnosticStore {
    pub fn create(
        run_directory: &Path,
        initial: &InitialStoreMetadata,
    ) -> Result<Self, StoreOpenError> {
        Self::create_with(Arc::new(RealStoreFileSystem), run_directory, initial, &())
    }

    pub fn create_with(
        filesystem: Arc<dyn StoreFileSystem>,
        run_directory: &Path,
        initial: &InitialStoreMetadata,
        hook: &dyn InitialTransactionHook,
    ) -> Result<Self, StoreOpenError> {
        validate_initial_metadata(initial, run_directory)?;
        secure_directory(filesystem.as_ref(), run_directory)?;
        let database_path = run_directory.join(DIAGNOSTIC_DATABASE_FILENAME);
        create_database_file(filesystem.as_ref(), &database_path)?;

        let mut connection = open_sqlite(&database_path)?;
        configure_new_connection(&connection, &database_path)?;
        secure_store_files(filesystem.as_ref(), &database_path)?;
        let zero = SortableU64Key::new(0);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                StoreOpenError::sqlite(
                    StoreOpenErrorCode::InitialTransactionFailed,
                    &database_path,
                    error,
                )
            })?;
        secure_store_files(filesystem.as_ref(), &database_path)?;
        schema::install(&transaction).map_err(|error| {
            StoreOpenError::sqlite(
                StoreOpenErrorCode::InitialTransactionFailed,
                &database_path,
                error,
            )
        })?;
        transaction
            .execute(
                "INSERT INTO run_metadata (\
                    singleton, store_schema_version, schema_identity, event_schema_version, \
                    run_id, started_at, configuration_identity, committed_key, \
                    committed_sequence, read_model_key, read_model_sequence, clean_shutdown\
                 ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, '0', ?7, '0', 0)",
                params![
                    STORE_SCHEMA_VERSION,
                    STORE_SCHEMA_IDENTITY,
                    EVENT_SCHEMA_VERSION,
                    initial.run_id().to_string(),
                    initial.started_at(),
                    initial.configuration_identity(),
                    zero.as_bytes().as_slice(),
                ],
            )
            .map_err(|error| {
                StoreOpenError::sqlite(
                    StoreOpenErrorCode::InitialTransactionFailed,
                    &database_path,
                    error,
                )
            })?;
        hook.before_commit(&transaction).map_err(|error| {
            StoreOpenError::sqlite(
                StoreOpenErrorCode::InitialTransactionFailed,
                &database_path,
                error,
            )
        })?;
        transaction.commit().map_err(|error| {
            StoreOpenError::sqlite(
                StoreOpenErrorCode::InitialTransactionFailed,
                &database_path,
                error,
            )
        })?;

        secure_store_files(filesystem.as_ref(), &database_path)?;
        let metadata = validate_store_state(&connection, initial.run_id(), &database_path)?;
        Ok(Self {
            connection,
            filesystem,
            run_directory: run_directory.to_path_buf(),
            database_path,
            metadata,
        })
    }

    pub fn open_validated(
        run_directory: &Path,
        expected_run_id: CanonicalUuid,
    ) -> Result<Self, StoreOpenError> {
        Self::open_validated_with(
            Arc::new(RealStoreFileSystem),
            run_directory,
            expected_run_id,
        )
    }

    pub fn open_validated_with(
        filesystem: Arc<dyn StoreFileSystem>,
        run_directory: &Path,
        expected_run_id: CanonicalUuid,
    ) -> Result<Self, StoreOpenError> {
        secure_directory(filesystem.as_ref(), run_directory)?;
        let database_path = run_directory.join(DIAGNOSTIC_DATABASE_FILENAME);
        secure_existing_file(filesystem.as_ref(), &database_path, true)?;
        let connection = open_sqlite(&database_path)?;
        configure_existing_connection(&connection, &database_path)?;
        secure_store_files(filesystem.as_ref(), &database_path)?;
        let metadata = validate_store_state(&connection, expected_run_id, &database_path)?;
        secure_store_files(filesystem.as_ref(), &database_path)?;
        Ok(Self {
            connection,
            filesystem,
            run_directory: run_directory.to_path_buf(),
            database_path,
            metadata,
        })
    }

    pub fn metadata(&self) -> &PersistedStoreMetadata {
        &self.metadata
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub(crate) fn refresh_metadata_after_commit(
        &mut self,
        expected_watermark: SortableU64Key,
    ) -> Result<(), StoreOpenError> {
        if expected_watermark < self.metadata.committed_watermark() {
            return Err(StoreOpenError::new(
                StoreOpenErrorCode::WatermarkMismatch,
                &self.database_path,
                Some("committed watermark regressed".to_owned()),
            ));
        }
        secure_store_files(self.filesystem.as_ref(), &self.database_path)?;
        let metadata = read_and_validate_metadata(
            &self.connection,
            self.metadata.run_id(),
            &self.database_path,
        )?;
        if metadata.committed_watermark() != expected_watermark
            || metadata.read_model_watermark() != expected_watermark
        {
            return Err(StoreOpenError::new(
                StoreOpenErrorCode::WatermarkMismatch,
                &self.database_path,
                Some(format!(
                    "expected committed watermark {}, found {}",
                    expected_watermark.get(),
                    metadata.committed_watermark().get()
                )),
            ));
        }
        self.metadata = metadata;
        Ok(())
    }

    pub fn checkpoint_and_validate_files(&self) -> Result<(), StoreOpenError> {
        self.connection
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_row| Ok(()))
            .map_err(|error| {
                StoreOpenError::sqlite(
                    StoreOpenErrorCode::CheckpointFailed,
                    &self.database_path,
                    error,
                )
            })?;
        secure_store_files(self.filesystem.as_ref(), &self.database_path)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreOpenErrorCode {
    InvalidMetadata,
    InvalidRunDirectory,
    DatabaseAlreadyExists,
    DatabaseMissing,
    InvalidFilesystemNode,
    FileCreateFailed,
    FileOpenFailed,
    FileInspectFailed,
    FilePermissionFailed,
    FileIdentityChanged,
    FileSyncFailed,
    SqliteOpenFailed,
    PragmaMismatch,
    InitialTransactionFailed,
    CheckpointFailed,
    NewerSchema,
    SchemaMismatch,
    CorruptStore,
    RunIdentityMismatch,
    StoreMetadataInvalid,
    WatermarkMismatch,
    DensePrefixViolation,
    EventIdentityMismatch,
}

impl StoreOpenErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidMetadata => "diagnostic_store.invalid_metadata",
            Self::InvalidRunDirectory => "diagnostic_store.invalid_run_directory",
            Self::DatabaseAlreadyExists => "diagnostic_store.database_already_exists",
            Self::DatabaseMissing => "diagnostic_store.database_missing",
            Self::InvalidFilesystemNode => "diagnostic_store.invalid_filesystem_node",
            Self::FileCreateFailed => "diagnostic_store.file_create_failed",
            Self::FileOpenFailed => "diagnostic_store.file_open_failed",
            Self::FileInspectFailed => "diagnostic_store.file_inspect_failed",
            Self::FilePermissionFailed => "diagnostic_store.file_permission_failed",
            Self::FileIdentityChanged => "diagnostic_store.file_identity_changed",
            Self::FileSyncFailed => "diagnostic_store.file_sync_failed",
            Self::SqliteOpenFailed => "diagnostic_store.sqlite_open_failed",
            Self::PragmaMismatch => "diagnostic_store.pragma_mismatch",
            Self::InitialTransactionFailed => "diagnostic_store.initial_transaction_failed",
            Self::CheckpointFailed => "diagnostic_store.checkpoint_failed",
            Self::NewerSchema => "diagnostic_store.newer_schema",
            Self::SchemaMismatch => "diagnostic_store.schema_mismatch",
            Self::CorruptStore => "diagnostic_store.corrupt_store",
            Self::RunIdentityMismatch => "diagnostic_store.run_identity_mismatch",
            Self::StoreMetadataInvalid => "diagnostic_store.metadata_invalid",
            Self::WatermarkMismatch => "diagnostic_store.watermark_mismatch",
            Self::DensePrefixViolation => "diagnostic_store.dense_prefix_violation",
            Self::EventIdentityMismatch => "diagnostic_store.event_identity_mismatch",
        }
    }
}

#[derive(Debug)]
pub struct StoreOpenError {
    code: StoreOpenErrorCode,
    path: PathBuf,
    detail: Option<String>,
    io_kind: Option<io::ErrorKind>,
}

impl StoreOpenError {
    fn new(code: StoreOpenErrorCode, path: &Path, detail: Option<String>) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            detail,
            io_kind: None,
        }
    }

    fn io(code: StoreOpenErrorCode, path: &Path, error: io::Error) -> Self {
        Self {
            code,
            path: path.to_path_buf(),
            detail: Some(error.to_string()),
            io_kind: Some(error.kind()),
        }
    }

    fn sqlite(code: StoreOpenErrorCode, path: &Path, error: rusqlite::Error) -> Self {
        Self::new(code, path, Some(error.to_string()))
    }

    pub const fn code(&self) -> StoreOpenErrorCode {
        self.code
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn source_kind(&self) -> Option<io::ErrorKind> {
        self.io_kind
    }
}

impl fmt::Display for StoreOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic store failed [{}] at {}",
            self.code.as_str(),
            self.path.display()
        )?;
        if let Some(detail) = &self.detail {
            write!(formatter, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for StoreOpenError {}

fn validate_initial_metadata(
    initial: &InitialStoreMetadata,
    run_directory: &Path,
) -> Result<(), StoreOpenError> {
    if initial.started_at().is_empty() || initial.configuration_identity().is_empty() {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::InvalidMetadata,
            run_directory,
            None,
        ));
    }
    Ok(())
}

fn open_sqlite(path: &Path) -> Result<Connection, StoreOpenError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::SqliteOpenFailed, path, error))
}

fn configure_new_connection(connection: &Connection, path: &Path) -> Result<(), StoreOpenError> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .and_then(|()| connection.pragma_update(None, "foreign_keys", "ON"))
        .and_then(|()| connection.pragma_update(None, "journal_mode", "WAL"))
        .and_then(|()| connection.pragma_update(None, "synchronous", "FULL"))
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::PragmaMismatch, path, error))?;
    validate_pragmas(connection, path)
}

fn configure_existing_connection(
    connection: &Connection,
    path: &Path,
) -> Result<(), StoreOpenError> {
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::PragmaMismatch,
            path,
            Some(format!("journal_mode={journal_mode}")),
        ));
    }
    connection
        .busy_timeout(Duration::from_secs(5))
        .and_then(|()| connection.pragma_update(None, "foreign_keys", "ON"))
        .and_then(|()| connection.pragma_update(None, "synchronous", "FULL"))
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::PragmaMismatch, path, error))?;
    validate_pragmas(connection, path)
}

fn validate_pragmas(connection: &Connection, path: &Path) -> Result<(), StoreOpenError> {
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::PragmaMismatch, path, error))?;
    let synchronous: u32 = connection
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::PragmaMismatch, path, error))?;
    let foreign_keys: u32 = connection
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::PragmaMismatch, path, error))?;
    if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 || foreign_keys != 1 {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::PragmaMismatch,
            path,
            Some(format!(
                "journal_mode={journal_mode}, synchronous={synchronous}, foreign_keys={foreign_keys}"
            )),
        ));
    }
    Ok(())
}

fn read_and_validate_metadata(
    connection: &Connection,
    expected_run_id: CanonicalUuid,
    path: &Path,
) -> Result<PersistedStoreMetadata, StoreOpenError> {
    let row = connection
        .query_row(
            "SELECT store_schema_version, schema_identity, event_schema_version, run_id, \
                    started_at, configuration_identity, committed_key, committed_sequence, \
                    read_model_key, read_model_sequence, clean_shutdown \
             FROM run_metadata WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u8>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Vec<u8>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .map_err(|error| {
            StoreOpenError::sqlite(StoreOpenErrorCode::StoreMetadataInvalid, path, error)
        })?;
    if row.0 != STORE_SCHEMA_VERSION
        || row.1 != STORE_SCHEMA_IDENTITY
        || row.2 != EVENT_SCHEMA_VERSION
    {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::SchemaMismatch,
            path,
            None,
        ));
    }
    if row.3 != expected_run_id.to_string() {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::RunIdentityMismatch,
            path,
            Some(format!("expected {expected_run_id}, found {}", row.3)),
        ));
    }
    if row.4.is_empty() || row.5.is_empty() || !matches!(row.10, 0 | 1) {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::StoreMetadataInvalid,
            path,
            None,
        ));
    }
    let committed = decode_key_pair(&row.6, &row.7, path)?;
    let read_model = decode_key_pair(&row.8, &row.9, path)?;
    if committed != read_model {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::WatermarkMismatch,
            path,
            None,
        ));
    }
    Ok(PersistedStoreMetadata {
        run_id: expected_run_id,
        started_at: row.4,
        configuration_identity: row.5,
        committed_watermark: committed,
        read_model_watermark: read_model,
        clean_shutdown: row.10 == 1,
    })
}

pub(crate) fn validate_store_state(
    connection: &Connection,
    expected_run_id: CanonicalUuid,
    path: &Path,
) -> Result<PersistedStoreMetadata, StoreOpenError> {
    schema::validate(connection).map_err(|error| map_schema_error(path, error))?;
    let metadata = read_and_validate_metadata(connection, expected_run_id, path)?;
    validate_dense_prefix(connection, &metadata, path)?;
    validate_materialized_state(connection, &metadata, path)?;
    Ok(metadata)
}

fn decode_key_pair(
    bytes: &[u8],
    decimal: &str,
    path: &Path,
) -> Result<SortableU64Key, StoreOpenError> {
    let key = SortableU64Key::from_slice(bytes).map_err(|error| {
        StoreOpenError::new(
            StoreOpenErrorCode::WatermarkMismatch,
            path,
            Some(error.to_string()),
        )
    })?;
    let decimal_key = SortableU64Key::parse_canonical_decimal(decimal).map_err(|error| {
        StoreOpenError::new(
            StoreOpenErrorCode::WatermarkMismatch,
            path,
            Some(error.to_string()),
        )
    })?;
    if key != decimal_key {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::WatermarkMismatch,
            path,
            None,
        ));
    }
    Ok(key)
}

fn validate_dense_prefix(
    connection: &Connection,
    metadata: &PersistedStoreMetadata,
    path: &Path,
) -> Result<(), StoreOpenError> {
    let mut statement = connection
        .prepare(
            "SELECT sequence_key, sequence, run_id, event_schema_version, \
                    elapsed_key, elapsed_ns, kind, scene_id, actor_id, cue_id, effect_id, \
                    act_id, tool_call_id, session_generation_key, session_generation, canonical_json \
             FROM events ORDER BY sequence_key",
        )
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error))?;
    let rows = statement
        .query_map([], StoredEventRow::from_row)
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error))?;
    let mut previous = 0_u64;
    let mut references = ReferenceValidator::new();
    for row in rows {
        let row = row.map_err(|error| {
            StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error)
        })?;
        let key = decode_event_key(&row.sequence_key, &row.sequence, path)?;
        let expected = previous.checked_add(1).ok_or_else(|| {
            StoreOpenError::new(StoreOpenErrorCode::DensePrefixViolation, path, None)
        })?;
        if key.get() != expected {
            return Err(StoreOpenError::new(
                StoreOpenErrorCode::DensePrefixViolation,
                path,
                Some(format!("expected sequence {expected}, found {}", key.get())),
            ));
        }
        let event: DiagnosticEvent =
            serde_json::from_slice(&row.canonical_json).map_err(|error| {
                StoreOpenError::new(
                    StoreOpenErrorCode::EventIdentityMismatch,
                    path,
                    Some(error.to_string()),
                )
            })?;
        let reencoded = serde_json::to_vec(&event).map_err(|error| {
            StoreOpenError::new(
                StoreOpenErrorCode::EventIdentityMismatch,
                path,
                Some(error.to_string()),
            )
        })?;
        let elapsed = decode_sortable_pair(
            &row.elapsed_key,
            &row.elapsed_ns,
            StoreOpenErrorCode::EventIdentityMismatch,
            path,
        )?;
        let session_generation = decode_optional_sortable_pair(
            row.session_generation_key.as_deref(),
            row.session_generation.as_deref(),
            StoreOpenErrorCode::EventIdentityMismatch,
            path,
        )?;
        let scope = event.header().scope();
        let columns_match = row.run_id == metadata.run_id().to_string()
            && row.event_schema_version == event.header().schema_version()
            && row.kind == event.kind().as_str()
            && elapsed.get() == event.header().elapsed_ns().get()
            && row.scene_id.as_deref() == scope.scene_id().map(|value| value.as_str())
            && row.actor_id.as_deref() == scope.actor_id().map(|value| value.as_str())
            && row.cue_id.as_deref() == scope.cue_id().map(|value| value.as_str())
            && row.effect_id.as_deref() == scope.effect_id().map(|value| value.as_str())
            && row.act_id.as_deref() == scope.act_id().map(|value| value.as_str())
            && row.tool_call_id.as_deref() == scope.tool_call_id().map(|value| value.as_str())
            && session_generation.map(SortableU64Key::get)
                == scope.session_generation().map(|value| value.get());
        if !columns_match
            || event.header().run_id() != metadata.run_id()
            || event.header().sequence().get() != key.get()
            || reencoded != row.canonical_json
        {
            return Err(StoreOpenError::new(
                StoreOpenErrorCode::EventIdentityMismatch,
                path,
                None,
            ));
        }
        references.validate(&event).map_err(|error| {
            StoreOpenError::new(
                StoreOpenErrorCode::EventIdentityMismatch,
                path,
                Some(error.to_string()),
            )
        })?;
        previous = key.get();
    }
    if previous != metadata.committed_watermark().get() {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::DensePrefixViolation,
            path,
            Some(format!(
                "event head {previous} differs from committed watermark {}",
                metadata.committed_watermark().get()
            )),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct StoredEventRow {
    sequence_key: Vec<u8>,
    sequence: String,
    run_id: String,
    event_schema_version: u8,
    elapsed_key: Vec<u8>,
    elapsed_ns: String,
    kind: String,
    scene_id: Option<String>,
    actor_id: Option<String>,
    cue_id: Option<String>,
    effect_id: Option<String>,
    act_id: Option<String>,
    tool_call_id: Option<String>,
    session_generation_key: Option<Vec<u8>>,
    session_generation: Option<String>,
    canonical_json: Vec<u8>,
}

impl StoredEventRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            sequence_key: row.get(0)?,
            sequence: row.get(1)?,
            run_id: row.get(2)?,
            event_schema_version: row.get(3)?,
            elapsed_key: row.get(4)?,
            elapsed_ns: row.get(5)?,
            kind: row.get(6)?,
            scene_id: row.get(7)?,
            actor_id: row.get(8)?,
            cue_id: row.get(9)?,
            effect_id: row.get(10)?,
            act_id: row.get(11)?,
            tool_call_id: row.get(12)?,
            session_generation_key: row.get(13)?,
            session_generation: row.get(14)?,
            canonical_json: row.get(15)?,
        })
    }
}

fn validate_materialized_state(
    connection: &Connection,
    metadata: &PersistedStoreMetadata,
    path: &Path,
) -> Result<(), StoreOpenError> {
    let watermark = metadata.read_model_watermark();
    validate_materialized_rows(
        connection,
        "materialized_spans",
        "SELECT latest_sequence_key, latest_sequence, model_schema_version, payload_json \
         FROM materialized_spans",
        watermark,
        path,
    )?;
    validate_sorted_key_rows(
        connection,
        "materialized_spans.span_key",
        "SELECT span_key, span_sequence FROM materialized_spans",
        watermark,
        path,
    )?;
    validate_materialized_rows(
        connection,
        "materialized_messages",
        "SELECT latest_sequence_key, latest_sequence, model_schema_version, payload_json \
         FROM materialized_messages",
        watermark,
        path,
    )?;
    validate_materialized_rows(
        connection,
        "materialized_plans",
        "SELECT latest_sequence_key, latest_sequence, model_schema_version, payload_json \
         FROM materialized_plans",
        watermark,
        path,
    )?;
    validate_materialized_rows(
        connection,
        "materialized_counters",
        "SELECT latest_sequence_key, latest_sequence, model_schema_version, payload_json \
         FROM materialized_counters",
        watermark,
        path,
    )?;
    validate_materialized_rows(
        connection,
        "materialized_usage",
        "SELECT through_sequence_key, through_sequence, model_schema_version, payload_json \
         FROM materialized_usage",
        watermark,
        path,
    )?;
    validate_materialized_rows(
        connection,
        "materialized_snapshot",
        "SELECT through_sequence_key, through_sequence, model_schema_version, payload_json \
         FROM materialized_snapshot",
        watermark,
        path,
    )
}

fn validate_materialized_rows(
    connection: &Connection,
    table: &str,
    query: &str,
    watermark: SortableU64Key,
    path: &Path,
) -> Result<(), StoreOpenError> {
    let mut statement = connection
        .prepare(query)
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error))?;
    for row in rows {
        let (key, decimal, version, payload) = row.map_err(|error| {
            StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error)
        })?;
        let key =
            decode_sortable_pair(&key, &decimal, StoreOpenErrorCode::WatermarkMismatch, path)?;
        if key > watermark {
            return Err(StoreOpenError::new(
                StoreOpenErrorCode::WatermarkMismatch,
                path,
                Some(format!(
                    "{table} sequence {} exceeds read-model watermark {}",
                    key.get(),
                    watermark.get()
                )),
            ));
        }
        if version != 1 {
            return Err(StoreOpenError::new(
                StoreOpenErrorCode::SchemaMismatch,
                path,
                Some(format!("{table} model schema version is {version}")),
            ));
        }
        serde_json::from_slice::<serde_json::Value>(&payload).map_err(|error| {
            StoreOpenError::new(
                StoreOpenErrorCode::CorruptStore,
                path,
                Some(format!("{table} payload is invalid: {error}")),
            )
        })?;
    }
    Ok(())
}

fn validate_sorted_key_rows(
    connection: &Connection,
    column: &str,
    query: &str,
    watermark: SortableU64Key,
    path: &Path,
) -> Result<(), StoreOpenError> {
    let mut statement = connection
        .prepare(query)
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error))?;
    for row in rows {
        let (key, decimal) = row.map_err(|error| {
            StoreOpenError::sqlite(StoreOpenErrorCode::CorruptStore, path, error)
        })?;
        let key =
            decode_sortable_pair(&key, &decimal, StoreOpenErrorCode::WatermarkMismatch, path)?;
        if key > watermark {
            return Err(StoreOpenError::new(
                StoreOpenErrorCode::WatermarkMismatch,
                path,
                Some(format!(
                    "{column} {} exceeds read-model watermark {}",
                    key.get(),
                    watermark.get()
                )),
            ));
        }
    }
    Ok(())
}

fn decode_event_key(
    bytes: &[u8],
    decimal: &str,
    path: &Path,
) -> Result<SortableU64Key, StoreOpenError> {
    let bytes = SortableU64Key::from_slice(bytes).map_err(|error| {
        StoreOpenError::new(
            StoreOpenErrorCode::DensePrefixViolation,
            path,
            Some(error.to_string()),
        )
    })?;
    let decimal = SortableU64Key::parse_canonical_decimal(decimal).map_err(|error| {
        StoreOpenError::new(
            StoreOpenErrorCode::DensePrefixViolation,
            path,
            Some(error.to_string()),
        )
    })?;
    if bytes != decimal {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::EventIdentityMismatch,
            path,
            None,
        ));
    }
    Ok(bytes)
}

fn decode_sortable_pair(
    bytes: &[u8],
    decimal: &str,
    code: StoreOpenErrorCode,
    path: &Path,
) -> Result<SortableU64Key, StoreOpenError> {
    let bytes = SortableU64Key::from_slice(bytes)
        .map_err(|error| StoreOpenError::new(code, path, Some(error.to_string())))?;
    let decimal = SortableU64Key::parse_canonical_decimal(decimal)
        .map_err(|error| StoreOpenError::new(code, path, Some(error.to_string())))?;
    if bytes != decimal {
        return Err(StoreOpenError::new(code, path, None));
    }
    Ok(bytes)
}

fn decode_optional_sortable_pair(
    bytes: Option<&[u8]>,
    decimal: Option<&str>,
    code: StoreOpenErrorCode,
    path: &Path,
) -> Result<Option<SortableU64Key>, StoreOpenError> {
    match (bytes, decimal) {
        (None, None) => Ok(None),
        (Some(bytes), Some(decimal)) => decode_sortable_pair(bytes, decimal, code, path).map(Some),
        _ => Err(StoreOpenError::new(code, path, None)),
    }
}

fn secure_directory(filesystem: &dyn StoreFileSystem, path: &Path) -> Result<(), StoreOpenError> {
    let expected = filesystem
        .path_metadata(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?;
    let expected = expected
        .ok_or_else(|| StoreOpenError::new(StoreOpenErrorCode::InvalidRunDirectory, path, None))?;
    if expected.kind() != StoreNodeKind::Directory {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::InvalidRunDirectory,
            path,
            None,
        ));
    }
    let handle = filesystem
        .open_directory(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileOpenFailed, path, error))?;
    secure_handle(
        filesystem,
        path,
        handle.as_ref(),
        expected,
        StoreNodeKind::Directory,
        OWNER_DIRECTORY_MODE,
    )
}

fn create_database_file(
    filesystem: &dyn StoreFileSystem,
    path: &Path,
) -> Result<(), StoreOpenError> {
    if filesystem
        .path_metadata(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?
        .is_some()
    {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::DatabaseAlreadyExists,
            path,
            None,
        ));
    }
    let handle = filesystem.create_database(path).map_err(|error| {
        let code = if error.kind() == io::ErrorKind::AlreadyExists {
            StoreOpenErrorCode::DatabaseAlreadyExists
        } else {
            StoreOpenErrorCode::FileCreateFailed
        };
        StoreOpenError::io(code, path, error)
    })?;
    let expected = handle
        .metadata()
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?;
    secure_handle(
        filesystem,
        path,
        handle.as_ref(),
        expected,
        StoreNodeKind::File,
        OWNER_FILE_MODE,
    )?;
    handle
        .sync_all()
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileSyncFailed, path, error))?;
    revalidate_handle_binding(
        filesystem,
        path,
        handle.as_ref(),
        StoreNodeKind::File,
        OWNER_FILE_MODE,
    )?;
    let parent = path
        .parent()
        .ok_or_else(|| StoreOpenError::new(StoreOpenErrorCode::InvalidRunDirectory, path, None))?;
    sync_directory(filesystem, parent)
}

fn secure_existing_file(
    filesystem: &dyn StoreFileSystem,
    path: &Path,
    required: bool,
) -> Result<bool, StoreOpenError> {
    let expected = filesystem
        .path_metadata(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?;
    let Some(expected) = expected else {
        if required {
            return Err(StoreOpenError::new(
                StoreOpenErrorCode::DatabaseMissing,
                path,
                None,
            ));
        }
        return Ok(false);
    };
    if expected.kind() != StoreNodeKind::File {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::InvalidFilesystemNode,
            path,
            None,
        ));
    }
    let handle = filesystem
        .open_file(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileOpenFailed, path, error))?;
    secure_handle(
        filesystem,
        path,
        handle.as_ref(),
        expected,
        StoreNodeKind::File,
        OWNER_FILE_MODE,
    )?;
    Ok(true)
}

fn secure_store_files(
    filesystem: &dyn StoreFileSystem,
    database_path: &Path,
) -> Result<(), StoreOpenError> {
    secure_existing_file(filesystem, database_path, true)?;
    let filename = database_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            StoreOpenError::new(
                StoreOpenErrorCode::InvalidFilesystemNode,
                database_path,
                None,
            )
        })?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = database_path.with_file_name(format!("{filename}{suffix}"));
        secure_existing_file(filesystem, &sidecar, false)?;
    }
    let parent = database_path.parent().ok_or_else(|| {
        StoreOpenError::new(StoreOpenErrorCode::InvalidRunDirectory, database_path, None)
    })?;
    sync_directory(filesystem, parent)
}

fn sync_directory(filesystem: &dyn StoreFileSystem, path: &Path) -> Result<(), StoreOpenError> {
    let expected = filesystem
        .path_metadata(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?
        .ok_or_else(|| StoreOpenError::new(StoreOpenErrorCode::InvalidRunDirectory, path, None))?;
    if expected.kind() != StoreNodeKind::Directory {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::InvalidRunDirectory,
            path,
            None,
        ));
    }
    let handle = filesystem
        .open_directory(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileOpenFailed, path, error))?;
    secure_handle(
        filesystem,
        path,
        handle.as_ref(),
        expected,
        StoreNodeKind::Directory,
        OWNER_DIRECTORY_MODE,
    )?;
    handle
        .sync_all()
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileSyncFailed, path, error))?;
    revalidate_handle_binding(
        filesystem,
        path,
        handle.as_ref(),
        StoreNodeKind::Directory,
        OWNER_DIRECTORY_MODE,
    )
}

fn secure_handle(
    filesystem: &dyn StoreFileSystem,
    path: &Path,
    handle: &dyn StoreNodeHandle,
    expected: StoreNodeMetadata,
    expected_kind: StoreNodeKind,
    mode: u32,
) -> Result<(), StoreOpenError> {
    let opened = handle
        .metadata()
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?;
    if opened.kind() != expected_kind || !opened.same_identity(expected) {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::FileIdentityChanged,
            path,
            None,
        ));
    }
    handle.set_mode(mode).map_err(|error| {
        StoreOpenError::io(StoreOpenErrorCode::FilePermissionFailed, path, error)
    })?;
    let secured = handle
        .metadata()
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?;
    if secured.kind() != expected_kind || !secured.same_identity(opened) || secured.mode() != mode {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::FilePermissionFailed,
            path,
            None,
        ));
    }
    let rebound = filesystem
        .path_metadata(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?;
    if !rebound.is_some_and(|metadata| metadata.same_identity(secured)) {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::FileIdentityChanged,
            path,
            None,
        ));
    }
    Ok(())
}

fn revalidate_handle_binding(
    filesystem: &dyn StoreFileSystem,
    path: &Path,
    handle: &dyn StoreNodeHandle,
    expected_kind: StoreNodeKind,
    expected_mode: u32,
) -> Result<(), StoreOpenError> {
    let metadata = handle
        .metadata()
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?;
    if metadata.kind() != expected_kind || metadata.mode() != expected_mode {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::FilePermissionFailed,
            path,
            None,
        ));
    }
    let rebound = filesystem
        .path_metadata(path)
        .map_err(|error| StoreOpenError::io(StoreOpenErrorCode::FileInspectFailed, path, error))?;
    if !rebound.is_some_and(|rebound| rebound.same_identity(metadata)) {
        return Err(StoreOpenError::new(
            StoreOpenErrorCode::FileIdentityChanged,
            path,
            None,
        ));
    }
    Ok(())
}

fn map_schema_error(path: &Path, error: SchemaValidationError) -> StoreOpenError {
    let code = if error.is_newer() {
        StoreOpenErrorCode::NewerSchema
    } else if error.is_integrity_failure() {
        StoreOpenErrorCode::CorruptStore
    } else {
        StoreOpenErrorCode::SchemaMismatch
    };
    StoreOpenError::new(code, path, Some(error.to_string()))
}

#[cfg(unix)]
fn metadata_mode(metadata: &Metadata, _kind: StoreNodeKind) -> u32 {
    use std::os::unix::fs::MetadataExt;

    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn metadata_mode(_metadata: &Metadata, kind: StoreNodeKind) -> u32 {
    match kind {
        StoreNodeKind::Directory => OWNER_DIRECTORY_MODE,
        StoreNodeKind::File | StoreNodeKind::Symlink | StoreNodeKind::Other => OWNER_FILE_MODE,
    }
}
