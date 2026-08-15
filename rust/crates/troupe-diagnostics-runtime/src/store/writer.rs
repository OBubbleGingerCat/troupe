use std::fmt;

use super::{
    batch::EventBatch,
    connection::{DiagnosticStore, StoreOpenError},
    key::SortableU64Key,
    projector::snapshot::{SnapshotProjectionError, SnapshotProjector, SnapshotReadModel},
    watermark::{CommitNotification, CommitObserver, CommittedWatermark, WatermarkError},
};
use rusqlite::{Params, Transaction, TransactionBehavior, params};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterializedTable {
    Spans,
    Messages,
    Plans,
    Counters,
    Usage,
    Snapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteStatement {
    AppendEvent { sequence: u64 },
    ClearMaterialized { table: MaterializedTable },
    InsertMaterialized { table: MaterializedTable },
    AdvanceWatermarks,
}

pub trait WriterTransactionHook {
    fn after_statement(
        &mut self,
        _ordinal: usize,
        _statement: WriteStatement,
        _transaction: &Transaction<'_>,
    ) -> rusqlite::Result<()> {
        Ok(())
    }

    fn before_commit(&mut self, _transaction: &Transaction<'_>) -> rusqlite::Result<()> {
        Ok(())
    }
}

impl WriterTransactionHook for () {}

#[derive(Debug)]
pub struct TransactionalWriter<O> {
    store: DiagnosticStore,
    projector: SnapshotProjector,
    watermark: CommittedWatermark,
    observer: O,
}

impl<O> TransactionalWriter<O>
where
    O: CommitObserver,
{
    pub fn new(store: DiagnosticStore, observer: O) -> Result<Self, WriterError> {
        let metadata = store.metadata();
        if metadata.committed_watermark().get() != 0
            || metadata.read_model_watermark().get() != 0
            || metadata.clean_shutdown()
        {
            return Err(WriterError::NonFreshStore {
                committed: metadata.committed_watermark(),
                read_model: metadata.read_model_watermark(),
                clean_shutdown: metadata.clean_shutdown(),
            });
        }
        let run_id = metadata.run_id();
        Ok(Self {
            store,
            projector: SnapshotProjector::new(run_id),
            watermark: CommittedWatermark::fresh(run_id),
            observer,
        })
    }

    pub const fn watermark(&self) -> CommittedWatermark {
        self.watermark
    }

    pub const fn snapshot(&self) -> &SnapshotReadModel {
        self.projector.model()
    }

    pub const fn store(&self) -> &DiagnosticStore {
        &self.store
    }

    pub fn into_store(self) -> DiagnosticStore {
        self.store
    }

    pub fn commit_batch(&mut self, batch: &EventBatch) -> Result<CommitNotification, WriterError> {
        self.commit_batch_with_hook(batch, &mut ())
    }

    pub fn commit_batch_with_hook(
        &mut self,
        batch: &EventBatch,
        hook: &mut dyn WriterTransactionHook,
    ) -> Result<CommitNotification, WriterError> {
        let notification = self
            .watermark
            .candidate(batch)
            .map_err(WriterError::Watermark)?;

        let mut candidate = self.projector.clone();
        for accepted in batch.events() {
            candidate
                .apply(accepted.event())
                .map_err(WriterError::Projection)?;
        }
        let materialized = EncodedMaterializedState::encode(candidate.model())
            .map_err(WriterError::Serialization)?;

        let mut ordinal = 1_usize;
        let transaction = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(WriterError::BeginTransaction)?;

        for accepted in batch.events() {
            append_event(&transaction, hook, &mut ordinal, accepted)?;
        }
        materialized.replace_all(&transaction, hook, &mut ordinal)?;
        advance_persisted_watermarks(&transaction, hook, &mut ordinal, notification.committed())?;
        hook.before_commit(&transaction)
            .map_err(WriterError::BeforeCommit)?;
        transaction.commit().map_err(WriterError::Commit)?;

        self.store
            .refresh_metadata_after_commit(notification.committed())
            .map_err(WriterError::PostCommitValidation)?;
        self.projector = candidate;
        self.watermark
            .advance(notification)
            .expect("a committed watermark candidate must still match its writer");
        self.observer.committed(notification);
        Ok(notification)
    }
}

fn append_event(
    transaction: &Transaction<'_>,
    hook: &mut dyn WriterTransactionHook,
    ordinal: &mut usize,
    accepted: &troupe_diagnostics_core::hub::AcceptedDiagnosticEvent,
) -> Result<(), WriterError> {
    let event = accepted.event();
    let header = event.header();
    let scope = header.scope();
    let sequence = SortableU64Key::new(header.sequence().get());
    let elapsed = SortableU64Key::new(header.elapsed_ns().get());
    let session_generation = scope
        .session_generation()
        .map(|value| SortableU64Key::new(value.get()));
    let session_generation_bytes = session_generation.map(SortableU64Key::into_bytes);
    let session_generation_decimal = session_generation.map(SortableU64Key::canonical_decimal);

    execute_statement(
        transaction,
        hook,
        ordinal,
        WriteStatement::AppendEvent {
            sequence: header.sequence().get(),
        },
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
            event.kind().as_str(),
            scope.scene_id().map(|value| value.as_str()),
            scope.actor_id().map(|value| value.as_str()),
            scope.cue_id().map(|value| value.as_str()),
            scope.effect_id().map(|value| value.as_str()),
            scope.act_id().map(|value| value.as_str()),
            scope.tool_call_id().map(|value| value.as_str()),
            session_generation_bytes.as_ref().map(<[u8; 8]>::as_slice),
            session_generation_decimal,
            accepted.canonical_bytes(),
        ],
    )
}

fn advance_persisted_watermarks(
    transaction: &Transaction<'_>,
    hook: &mut dyn WriterTransactionHook,
    ordinal: &mut usize,
    watermark: SortableU64Key,
) -> Result<(), WriterError> {
    execute_statement(
        transaction,
        hook,
        ordinal,
        WriteStatement::AdvanceWatermarks,
        "UPDATE run_metadata SET \
            committed_key = ?1, committed_sequence = ?2, \
            read_model_key = ?1, read_model_sequence = ?2 \
         WHERE singleton = 1",
        params![
            watermark.as_bytes().as_slice(),
            watermark.canonical_decimal()
        ],
    )
}

fn execute_statement<P>(
    transaction: &Transaction<'_>,
    hook: &mut dyn WriterTransactionHook,
    ordinal: &mut usize,
    statement: WriteStatement,
    sql: &str,
    params: P,
) -> Result<(), WriterError>
where
    P: Params,
{
    let current = *ordinal;
    transaction
        .execute(sql, params)
        .map_err(|source| WriterError::Statement {
            ordinal: current,
            statement,
            source,
        })?;
    hook.after_statement(current, statement, transaction)
        .map_err(|source| WriterError::Statement {
            ordinal: current,
            statement,
            source,
        })?;
    *ordinal = current
        .checked_add(1)
        .expect("a transaction cannot execute usize::MAX statements");
    Ok(())
}

#[derive(Debug)]
struct EncodedMaterializedState {
    spans: Vec<EncodedSpan>,
    messages: Vec<EncodedMessage>,
    plans: Vec<EncodedPlan>,
    counters: Vec<EncodedCounter>,
    usage: EncodedSingleton,
    snapshot: EncodedSingleton,
}

impl EncodedMaterializedState {
    fn encode(model: &SnapshotReadModel) -> Result<Self, serde_json::Error> {
        let spans = model
            .spans()
            .spans()
            .iter()
            .map(|span| {
                Ok(EncodedSpan {
                    span_id: SortableU64Key::new(span.span_id().get()),
                    latest: SortableU64Key::new(span.latest_sequence().get()),
                    payload: serde_json::to_vec(span)?,
                })
            })
            .collect::<Result<_, serde_json::Error>>()?;
        let messages = model
            .messages()
            .messages()
            .iter()
            .map(|message| {
                Ok(EncodedMessage {
                    message_id: message.message_id().as_str().to_owned(),
                    latest: SortableU64Key::new(message.latest_sequence().get()),
                    payload: serde_json::to_vec(message)?,
                })
            })
            .collect::<Result<_, serde_json::Error>>()?;
        let plans = model
            .plans()
            .plans()
            .iter()
            .map(|plan| {
                Ok(EncodedPlan {
                    scope_key: plan.scope_key()?,
                    latest: SortableU64Key::new(plan.sequence().get()),
                    payload: serde_json::to_vec(plan)?,
                })
            })
            .collect::<Result<_, serde_json::Error>>()?;
        let counters = model
            .counters()
            .series()
            .iter()
            .map(|counter| {
                Ok(EncodedCounter {
                    series_key: counter.series_key().to_owned(),
                    latest: SortableU64Key::new(counter.sequence().get()),
                    payload: serde_json::to_vec(counter)?,
                })
            })
            .collect::<Result<_, serde_json::Error>>()?;

        Ok(Self {
            spans,
            messages,
            plans,
            counters,
            usage: EncodedSingleton {
                through: SortableU64Key::new(model.usage().through_sequence().get()),
                payload: model.usage().canonical_json()?,
            },
            snapshot: EncodedSingleton {
                through: SortableU64Key::new(model.through_sequence().get()),
                payload: model.canonical_json()?,
            },
        })
    }

    fn replace_all(
        &self,
        transaction: &Transaction<'_>,
        hook: &mut dyn WriterTransactionHook,
        ordinal: &mut usize,
    ) -> Result<(), WriterError> {
        clear_table(transaction, hook, ordinal, MaterializedTable::Spans)?;
        for row in &self.spans {
            execute_statement(
                transaction,
                hook,
                ordinal,
                WriteStatement::InsertMaterialized {
                    table: MaterializedTable::Spans,
                },
                "INSERT INTO materialized_spans (\
                    span_key, span_sequence, model_schema_version, latest_sequence_key, \
                    latest_sequence, payload_json\
                 ) VALUES (?1, ?2, 1, ?3, ?4, ?5)",
                params![
                    row.span_id.as_bytes().as_slice(),
                    row.span_id.canonical_decimal(),
                    row.latest.as_bytes().as_slice(),
                    row.latest.canonical_decimal(),
                    row.payload.as_slice(),
                ],
            )?;
        }

        clear_table(transaction, hook, ordinal, MaterializedTable::Messages)?;
        for row in &self.messages {
            execute_statement(
                transaction,
                hook,
                ordinal,
                WriteStatement::InsertMaterialized {
                    table: MaterializedTable::Messages,
                },
                "INSERT INTO materialized_messages (\
                    message_id, model_schema_version, latest_sequence_key, latest_sequence, payload_json\
                 ) VALUES (?1, 1, ?2, ?3, ?4)",
                params![
                    row.message_id,
                    row.latest.as_bytes().as_slice(),
                    row.latest.canonical_decimal(),
                    row.payload.as_slice(),
                ],
            )?;
        }

        clear_table(transaction, hook, ordinal, MaterializedTable::Plans)?;
        for row in &self.plans {
            execute_statement(
                transaction,
                hook,
                ordinal,
                WriteStatement::InsertMaterialized {
                    table: MaterializedTable::Plans,
                },
                "INSERT INTO materialized_plans (\
                    scope_key, model_schema_version, latest_sequence_key, latest_sequence, payload_json\
                 ) VALUES (?1, 1, ?2, ?3, ?4)",
                params![
                    row.scope_key,
                    row.latest.as_bytes().as_slice(),
                    row.latest.canonical_decimal(),
                    row.payload.as_slice(),
                ],
            )?;
        }

        clear_table(transaction, hook, ordinal, MaterializedTable::Counters)?;
        for row in &self.counters {
            execute_statement(
                transaction,
                hook,
                ordinal,
                WriteStatement::InsertMaterialized {
                    table: MaterializedTable::Counters,
                },
                "INSERT INTO materialized_counters (\
                    series_key, model_schema_version, latest_sequence_key, latest_sequence, payload_json\
                 ) VALUES (?1, 1, ?2, ?3, ?4)",
                params![
                    row.series_key,
                    row.latest.as_bytes().as_slice(),
                    row.latest.canonical_decimal(),
                    row.payload.as_slice(),
                ],
            )?;
        }

        clear_table(transaction, hook, ordinal, MaterializedTable::Usage)?;
        insert_singleton(
            transaction,
            hook,
            ordinal,
            MaterializedTable::Usage,
            "INSERT INTO materialized_usage (\
                singleton, model_schema_version, through_sequence_key, through_sequence, payload_json\
             ) VALUES (1, 1, ?1, ?2, ?3)",
            &self.usage,
        )?;

        clear_table(transaction, hook, ordinal, MaterializedTable::Snapshot)?;
        insert_singleton(
            transaction,
            hook,
            ordinal,
            MaterializedTable::Snapshot,
            "INSERT INTO materialized_snapshot (\
                singleton, model_schema_version, through_sequence_key, through_sequence, payload_json\
             ) VALUES (1, 1, ?1, ?2, ?3)",
            &self.snapshot,
        )
    }
}

fn clear_table(
    transaction: &Transaction<'_>,
    hook: &mut dyn WriterTransactionHook,
    ordinal: &mut usize,
    table: MaterializedTable,
) -> Result<(), WriterError> {
    let sql = match table {
        MaterializedTable::Spans => "DELETE FROM materialized_spans",
        MaterializedTable::Messages => "DELETE FROM materialized_messages",
        MaterializedTable::Plans => "DELETE FROM materialized_plans",
        MaterializedTable::Counters => "DELETE FROM materialized_counters",
        MaterializedTable::Usage => "DELETE FROM materialized_usage",
        MaterializedTable::Snapshot => "DELETE FROM materialized_snapshot",
    };
    execute_statement(
        transaction,
        hook,
        ordinal,
        WriteStatement::ClearMaterialized { table },
        sql,
        [],
    )
}

fn insert_singleton(
    transaction: &Transaction<'_>,
    hook: &mut dyn WriterTransactionHook,
    ordinal: &mut usize,
    table: MaterializedTable,
    sql: &str,
    row: &EncodedSingleton,
) -> Result<(), WriterError> {
    execute_statement(
        transaction,
        hook,
        ordinal,
        WriteStatement::InsertMaterialized { table },
        sql,
        params![
            row.through.as_bytes().as_slice(),
            row.through.canonical_decimal(),
            row.payload.as_slice(),
        ],
    )
}

#[derive(Debug)]
struct EncodedSpan {
    span_id: SortableU64Key,
    latest: SortableU64Key,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct EncodedMessage {
    message_id: String,
    latest: SortableU64Key,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct EncodedPlan {
    scope_key: String,
    latest: SortableU64Key,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct EncodedCounter {
    series_key: String,
    latest: SortableU64Key,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct EncodedSingleton {
    through: SortableU64Key,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterErrorCode {
    NonFreshStore,
    Watermark,
    Projection,
    Serialization,
    BeginTransaction,
    Statement,
    BeforeCommit,
    Commit,
    PostCommitValidation,
}

impl WriterErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NonFreshStore => "diagnostic_writer.non_fresh_store",
            Self::Watermark => "diagnostic_writer.watermark",
            Self::Projection => "diagnostic_writer.projection",
            Self::Serialization => "diagnostic_writer.serialization",
            Self::BeginTransaction => "diagnostic_writer.begin_transaction",
            Self::Statement => "diagnostic_writer.statement",
            Self::BeforeCommit => "diagnostic_writer.before_commit",
            Self::Commit => "diagnostic_writer.commit",
            Self::PostCommitValidation => "diagnostic_writer.post_commit_validation",
        }
    }
}

#[derive(Debug)]
pub enum WriterError {
    NonFreshStore {
        committed: SortableU64Key,
        read_model: SortableU64Key,
        clean_shutdown: bool,
    },
    Watermark(WatermarkError),
    Projection(SnapshotProjectionError),
    Serialization(serde_json::Error),
    BeginTransaction(rusqlite::Error),
    Statement {
        ordinal: usize,
        statement: WriteStatement,
        source: rusqlite::Error,
    },
    BeforeCommit(rusqlite::Error),
    Commit(rusqlite::Error),
    PostCommitValidation(StoreOpenError),
}

impl WriterError {
    pub const fn code(&self) -> WriterErrorCode {
        match self {
            Self::NonFreshStore { .. } => WriterErrorCode::NonFreshStore,
            Self::Watermark(_) => WriterErrorCode::Watermark,
            Self::Projection(_) => WriterErrorCode::Projection,
            Self::Serialization(_) => WriterErrorCode::Serialization,
            Self::BeginTransaction(_) => WriterErrorCode::BeginTransaction,
            Self::Statement { .. } => WriterErrorCode::Statement,
            Self::BeforeCommit(_) => WriterErrorCode::BeforeCommit,
            Self::Commit(_) => WriterErrorCode::Commit,
            Self::PostCommitValidation(_) => WriterErrorCode::PostCommitValidation,
        }
    }

    pub const fn statement_ordinal(&self) -> Option<usize> {
        match self {
            Self::Statement { ordinal, .. } => Some(*ordinal),
            _ => None,
        }
    }

    pub const fn statement(&self) -> Option<WriteStatement> {
        match self {
            Self::Statement { statement, .. } => Some(*statement),
            _ => None,
        }
    }
}

impl fmt::Display for WriterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic writer failed [{}]",
            self.code().as_str()
        )?;
        match self {
            Self::NonFreshStore {
                committed,
                read_model,
                clean_shutdown,
            } => write!(
                formatter,
                ": store state is committed={}, read_model={}, clean_shutdown={clean_shutdown}",
                committed.get(),
                read_model.get()
            ),
            Self::Statement {
                ordinal,
                statement,
                source,
            } => write!(formatter, ": statement {ordinal} {statement:?}: {source}"),
            Self::Watermark(source) => write!(formatter, ": {source}"),
            Self::Projection(source) => write!(formatter, ": {source}"),
            Self::Serialization(source) => write!(formatter, ": {source}"),
            Self::BeginTransaction(source) | Self::BeforeCommit(source) | Self::Commit(source) => {
                write!(formatter, ": {source}")
            }
            Self::PostCommitValidation(source) => write!(formatter, ": {source}"),
        }
    }
}

impl std::error::Error for WriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NonFreshStore { .. } => None,
            Self::Watermark(source) => Some(source),
            Self::Projection(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::BeginTransaction(source) | Self::BeforeCommit(source) | Self::Commit(source) => {
                Some(source)
            }
            Self::Statement { source, .. } => Some(source),
            Self::PostCommitValidation(source) => Some(source),
        }
    }
}
