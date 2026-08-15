use std::{collections::BTreeSet, fmt};

use rusqlite::Connection;

pub const DIAGNOSTIC_DATABASE_FILENAME: &str = "diagnostics.sqlite3";
pub const STORE_SCHEMA_VERSION: u32 = 1;
pub const STORE_SCHEMA_IDENTITY: &str = "troupe.diagnostics.store.v1";
pub const STORE_SCHEMA_SQL: &str = include_str!("../../schema/diagnostics-v1.sql");

type SchemaObject = (String, String, String, Option<String>);

pub fn install(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(STORE_SCHEMA_SQL)
}

pub fn validate(connection: &Connection) -> Result<(), SchemaValidationError> {
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(SchemaValidationError::Sqlite)?;
    if version > STORE_SCHEMA_VERSION {
        return Err(SchemaValidationError::NewerVersion(version));
    }
    if version != STORE_SCHEMA_VERSION {
        return Err(SchemaValidationError::VersionMismatch(version));
    }

    let integrity: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(SchemaValidationError::Sqlite)?;
    if integrity != "ok" {
        return Err(SchemaValidationError::Integrity(integrity));
    }

    let actual = schema_objects(connection)?;
    let expected_connection =
        Connection::open_in_memory().map_err(SchemaValidationError::Sqlite)?;
    install(&expected_connection).map_err(SchemaValidationError::Sqlite)?;
    let expected = schema_objects(&expected_connection)?;
    if actual != expected {
        return Err(SchemaValidationError::DefinitionMismatch { expected, actual });
    }
    Ok(())
}

fn schema_objects(
    connection: &Connection,
) -> Result<BTreeSet<SchemaObject>, SchemaValidationError> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_schema \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .map_err(SchemaValidationError::Sqlite)?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(SchemaValidationError::Sqlite)?
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .map_err(SchemaValidationError::Sqlite)
}

#[derive(Debug)]
pub enum SchemaValidationError {
    NewerVersion(u32),
    VersionMismatch(u32),
    Integrity(String),
    DefinitionMismatch {
        expected: BTreeSet<SchemaObject>,
        actual: BTreeSet<SchemaObject>,
    },
    Sqlite(rusqlite::Error),
}

impl SchemaValidationError {
    pub const fn is_newer(&self) -> bool {
        matches!(self, Self::NewerVersion(_))
    }

    pub const fn is_integrity_failure(&self) -> bool {
        matches!(self, Self::Integrity(_) | Self::Sqlite(_))
    }
}

impl fmt::Display for SchemaValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewerVersion(version) => write!(
                formatter,
                "store schema version {version} is newer than {STORE_SCHEMA_VERSION}"
            ),
            Self::VersionMismatch(version) => write!(
                formatter,
                "store schema version {version} does not equal {STORE_SCHEMA_VERSION}"
            ),
            Self::Integrity(detail) => write!(formatter, "store integrity check failed: {detail}"),
            Self::DefinitionMismatch { expected, actual } => write!(
                formatter,
                "store schema object set differs: expected {expected:?}, actual {actual:?}"
            ),
            Self::Sqlite(error) => write!(formatter, "store schema inspection failed: {error}"),
        }
    }
}

impl std::error::Error for SchemaValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}
