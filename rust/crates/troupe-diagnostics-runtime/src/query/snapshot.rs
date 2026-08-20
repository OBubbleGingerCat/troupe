use std::fmt;

use rusqlite::OptionalExtension;
use troupe_diagnostics_core::{id::CanonicalUuid, scalar::SchemaU64, time::ElapsedNs};

use crate::store::{
    key::{SortableU64Key, StoreKeyError},
    projector::{
        counters::COUNTER_READ_MODEL_SCHEMA_VERSION,
        messages::MESSAGE_READ_MODEL_SCHEMA_VERSION,
        plans::PLAN_READ_MODEL_SCHEMA_VERSION,
        snapshot::{SNAPSHOT_READ_MODEL_SCHEMA_VERSION, SnapshotProjector, SnapshotReadModel},
        spans::SPAN_READ_MODEL_SCHEMA_VERSION,
        usage::USAGE_READ_MODEL_SCHEMA_VERSION,
    },
};

use super::reader::{CapturedEventSource, ReaderFailureClass, ReaderProfile};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotSynchronization {
    CaughtUp,
    EventHeadAhead,
}

impl SnapshotSynchronization {
    pub const fn classify(
        event_watermark: SchemaU64,
        read_model_watermark: SchemaU64,
    ) -> Option<Self> {
        if event_watermark.get() < read_model_watermark.get() {
            None
        } else if event_watermark.get() == read_model_watermark.get() {
            Some(Self::CaughtUp)
        } else {
            Some(Self::EventHeadAhead)
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CaughtUp => "caught_up",
            Self::EventHeadAhead => "event_head_ahead",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedSnapshot {
    run_id: CanonicalUuid,
    event_watermark: SchemaU64,
    watermark_sequence: SchemaU64,
    earliest_available_sequence: Option<SchemaU64>,
    synchronization: SnapshotSynchronization,
    state: SnapshotReadModel,
    canonical_state: Box<[u8]>,
}

impl MaterializedSnapshot {
    pub const fn run_id(&self) -> CanonicalUuid {
        self.run_id
    }

    pub const fn event_watermark(&self) -> SchemaU64 {
        self.event_watermark
    }

    pub const fn watermark_sequence(&self) -> SchemaU64 {
        self.watermark_sequence
    }

    pub const fn earliest_available_sequence(&self) -> Option<SchemaU64> {
        self.earliest_available_sequence
    }

    pub const fn synchronization(&self) -> SnapshotSynchronization {
        self.synchronization
    }

    pub const fn state(&self) -> &SnapshotReadModel {
        &self.state
    }

    pub fn canonical_state(&self) -> &[u8] {
        &self.canonical_state
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotQueryErrorCode {
    MaterializedRead,
    MaterializedMissing,
    MaterializedKey,
    ModelDecode,
    ModelSchemaMismatch,
    ModelIdentityMismatch,
    ModelWatermarkMismatch,
    NonCanonicalState,
}

impl SnapshotQueryErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaterializedRead => "diagnostic_snapshot.materialized_read",
            Self::MaterializedMissing => "diagnostic_snapshot.materialized_missing",
            Self::MaterializedKey => "diagnostic_snapshot.materialized_key",
            Self::ModelDecode => "diagnostic_snapshot.model_decode",
            Self::ModelSchemaMismatch => "diagnostic_snapshot.model_schema_mismatch",
            Self::ModelIdentityMismatch => "diagnostic_snapshot.model_identity_mismatch",
            Self::ModelWatermarkMismatch => "diagnostic_snapshot.model_watermark_mismatch",
            Self::NonCanonicalState => "diagnostic_snapshot.noncanonical_state",
        }
    }
}

#[derive(Debug)]
enum SnapshotQueryErrorSource {
    Sqlite(rusqlite::Error),
    Key(StoreKeyError),
    Json(serde_json::Error),
    Detail(String),
}

#[derive(Debug)]
pub struct SnapshotQueryError {
    class: ReaderFailureClass,
    profile: ReaderProfile,
    code: SnapshotQueryErrorCode,
    source: SnapshotQueryErrorSource,
}

impl SnapshotQueryError {
    fn new(
        profile: ReaderProfile,
        code: SnapshotQueryErrorCode,
        source: SnapshotQueryErrorSource,
    ) -> Self {
        let class = match profile {
            ReaderProfile::Active => ReaderFailureClass::CoreFatal,
            ReaderProfile::Archive => ReaderFailureClass::ArchiveOperation,
        };
        Self {
            class,
            profile,
            code,
            source,
        }
    }

    pub const fn class(&self) -> ReaderFailureClass {
        self.class
    }

    pub const fn profile(&self) -> ReaderProfile {
        self.profile
    }

    pub const fn code(&self) -> SnapshotQueryErrorCode {
        self.code
    }
}

impl fmt::Display for SnapshotQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic snapshot query failed [{}]: ",
            self.code.as_str()
        )?;
        match &self.source {
            SnapshotQueryErrorSource::Sqlite(error) => fmt::Display::fmt(error, formatter),
            SnapshotQueryErrorSource::Key(error) => fmt::Display::fmt(error, formatter),
            SnapshotQueryErrorSource::Json(error) => fmt::Display::fmt(error, formatter),
            SnapshotQueryErrorSource::Detail(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for SnapshotQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.source {
            SnapshotQueryErrorSource::Sqlite(error) => Some(error),
            SnapshotQueryErrorSource::Key(error) => Some(error),
            SnapshotQueryErrorSource::Json(error) => Some(error),
            SnapshotQueryErrorSource::Detail(_) => None,
        }
    }
}

struct StoredSnapshot {
    model_schema_version: u32,
    through_key: Vec<u8>,
    through_sequence: String,
    payload: Vec<u8>,
}

pub fn project_snapshot(
    source: &CapturedEventSource<'_>,
) -> Result<MaterializedSnapshot, SnapshotQueryError> {
    let profile = source.profile();
    let metadata = source.metadata();
    let run_id = metadata.run_id();
    let event_watermark = metadata.committed_watermark();
    let watermark_sequence = metadata.read_model_watermark();
    let Some(synchronization) =
        SnapshotSynchronization::classify(event_watermark, watermark_sequence)
    else {
        return Err(detail_error(
            profile,
            SnapshotQueryErrorCode::ModelWatermarkMismatch,
            format!(
                "event watermark {} is behind read-model watermark {}",
                event_watermark.get(),
                watermark_sequence.get()
            ),
        ));
    };

    let stored = source
        .transaction()
        .query_row(
            "SELECT model_schema_version, through_sequence_key, through_sequence, payload_json \
             FROM materialized_snapshot WHERE singleton = 1",
            [],
            |row| {
                Ok(StoredSnapshot {
                    model_schema_version: row.get(0)?,
                    through_key: row.get(1)?,
                    through_sequence: row.get(2)?,
                    payload: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|error| {
            SnapshotQueryError::new(
                profile,
                SnapshotQueryErrorCode::MaterializedRead,
                SnapshotQueryErrorSource::Sqlite(error),
            )
        })?;

    let (state, canonical_state) = match stored {
        Some(stored) => decode_stored_snapshot(profile, run_id, watermark_sequence, stored)?,
        None if watermark_sequence.get() == 0 => {
            let state = SnapshotProjector::new(run_id).into_model();
            let canonical = state.canonical_json().map_err(|error| {
                SnapshotQueryError::new(
                    profile,
                    SnapshotQueryErrorCode::ModelDecode,
                    SnapshotQueryErrorSource::Json(error),
                )
            })?;
            (state, canonical.into_boxed_slice())
        }
        None => {
            return Err(detail_error(
                profile,
                SnapshotQueryErrorCode::MaterializedMissing,
                format!(
                    "materialized snapshot is missing at watermark {}",
                    watermark_sequence.get()
                ),
            ));
        }
    };

    Ok(MaterializedSnapshot {
        run_id,
        event_watermark,
        watermark_sequence,
        earliest_available_sequence: (watermark_sequence.get() != 0).then_some(SchemaU64::new(1)),
        synchronization,
        state,
        canonical_state,
    })
}

fn decode_stored_snapshot(
    profile: ReaderProfile,
    run_id: CanonicalUuid,
    watermark: SchemaU64,
    stored: StoredSnapshot,
) -> Result<(SnapshotReadModel, Box<[u8]>), SnapshotQueryError> {
    if stored.model_schema_version != u32::from(SNAPSHOT_READ_MODEL_SCHEMA_VERSION) {
        return Err(detail_error(
            profile,
            SnapshotQueryErrorCode::ModelSchemaMismatch,
            format!(
                "materialized snapshot schema version is {}, expected {}",
                stored.model_schema_version, SNAPSHOT_READ_MODEL_SCHEMA_VERSION
            ),
        ));
    }
    let binary_key = SortableU64Key::from_slice(&stored.through_key).map_err(|error| {
        SnapshotQueryError::new(
            profile,
            SnapshotQueryErrorCode::MaterializedKey,
            SnapshotQueryErrorSource::Key(error),
        )
    })?;
    let decimal_key =
        SortableU64Key::parse_canonical_decimal(&stored.through_sequence).map_err(|error| {
            SnapshotQueryError::new(
                profile,
                SnapshotQueryErrorCode::MaterializedKey,
                SnapshotQueryErrorSource::Key(error),
            )
        })?;
    if binary_key != decimal_key || binary_key.get() != watermark.get() {
        return Err(detail_error(
            profile,
            SnapshotQueryErrorCode::ModelWatermarkMismatch,
            format!(
                "materialized snapshot row is through {}, expected {}",
                binary_key.get(),
                watermark.get()
            ),
        ));
    }

    let state: SnapshotReadModel = serde_json::from_slice(&stored.payload).map_err(|error| {
        SnapshotQueryError::new(
            profile,
            SnapshotQueryErrorCode::ModelDecode,
            SnapshotQueryErrorSource::Json(error),
        )
    })?;
    validate_model(profile, &state, run_id, watermark)?;
    let canonical = state.canonical_json().map_err(|error| {
        SnapshotQueryError::new(
            profile,
            SnapshotQueryErrorCode::ModelDecode,
            SnapshotQueryErrorSource::Json(error),
        )
    })?;
    if canonical != stored.payload {
        return Err(detail_error(
            profile,
            SnapshotQueryErrorCode::NonCanonicalState,
            "materialized snapshot payload is not canonical JSON".to_owned(),
        ));
    }
    Ok((state, stored.payload.into_boxed_slice()))
}

fn validate_model(
    profile: ReaderProfile,
    state: &SnapshotReadModel,
    run_id: CanonicalUuid,
    watermark: SchemaU64,
) -> Result<(), SnapshotQueryError> {
    if state.model_schema_version() != SNAPSHOT_READ_MODEL_SCHEMA_VERSION {
        return Err(detail_error(
            profile,
            SnapshotQueryErrorCode::ModelSchemaMismatch,
            format!(
                "snapshot state schema version is {}, expected {}",
                state.model_schema_version(),
                SNAPSHOT_READ_MODEL_SCHEMA_VERSION
            ),
        ));
    }
    if state.run_id() != run_id {
        return Err(detail_error(
            profile,
            SnapshotQueryErrorCode::ModelIdentityMismatch,
            format!(
                "snapshot state belongs to {}, expected {run_id}",
                state.run_id()
            ),
        ));
    }
    if state.through_sequence() != watermark {
        return Err(detail_error(
            profile,
            SnapshotQueryErrorCode::ModelWatermarkMismatch,
            format!(
                "snapshot state is through {}, expected {}",
                state.through_sequence().get(),
                watermark.get()
            ),
        ));
    }

    let through_elapsed_ns = state.through_elapsed_ns();
    let children = [
        (
            "spans",
            state.spans().model_schema_version(),
            SPAN_READ_MODEL_SCHEMA_VERSION,
            state.spans().run_id(),
            state.spans().through_sequence(),
            state.spans().through_elapsed_ns(),
        ),
        (
            "messages",
            state.messages().model_schema_version(),
            MESSAGE_READ_MODEL_SCHEMA_VERSION,
            state.messages().run_id(),
            state.messages().through_sequence(),
            state.messages().through_elapsed_ns(),
        ),
        (
            "plans",
            state.plans().model_schema_version(),
            PLAN_READ_MODEL_SCHEMA_VERSION,
            state.plans().run_id(),
            state.plans().through_sequence(),
            state.plans().through_elapsed_ns(),
        ),
        (
            "counters",
            state.counters().model_schema_version(),
            COUNTER_READ_MODEL_SCHEMA_VERSION,
            state.counters().run_id(),
            state.counters().through_sequence(),
            state.counters().through_elapsed_ns(),
        ),
        (
            "usage",
            state.usage().model_schema_version(),
            USAGE_READ_MODEL_SCHEMA_VERSION,
            state.usage().run_id(),
            state.usage().through_sequence(),
            state.usage().through_elapsed_ns(),
        ),
    ];
    for (name, actual_schema, expected_schema, child_run_id, sequence, elapsed_ns) in children {
        if actual_schema != expected_schema {
            return Err(detail_error(
                profile,
                SnapshotQueryErrorCode::ModelSchemaMismatch,
                format!(
                    "snapshot {name} schema version is {actual_schema}, expected {expected_schema}"
                ),
            ));
        }
        if child_run_id != run_id {
            return Err(detail_error(
                profile,
                SnapshotQueryErrorCode::ModelIdentityMismatch,
                format!("snapshot {name} belongs to {child_run_id}, expected {run_id}"),
            ));
        }
        if sequence != watermark || elapsed_ns != through_elapsed_ns {
            return Err(child_watermark_error(
                profile,
                name,
                sequence,
                elapsed_ns,
                watermark,
                through_elapsed_ns,
            ));
        }
    }
    Ok(())
}

fn child_watermark_error(
    profile: ReaderProfile,
    name: &str,
    sequence: SchemaU64,
    elapsed_ns: ElapsedNs,
    expected_sequence: SchemaU64,
    expected_elapsed_ns: ElapsedNs,
) -> SnapshotQueryError {
    detail_error(
        profile,
        SnapshotQueryErrorCode::ModelWatermarkMismatch,
        format!(
            "snapshot {name} is through sequence {} at elapsed {}, expected sequence {} at elapsed {}",
            sequence.get(),
            elapsed_ns.get(),
            expected_sequence.get(),
            expected_elapsed_ns.get()
        ),
    )
}

fn detail_error(
    profile: ReaderProfile,
    code: SnapshotQueryErrorCode,
    detail: String,
) -> SnapshotQueryError {
    SnapshotQueryError::new(profile, code, SnapshotQueryErrorSource::Detail(detail))
}
