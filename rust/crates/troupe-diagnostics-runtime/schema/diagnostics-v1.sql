PRAGMA user_version = 1;

CREATE TABLE run_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    store_schema_version INTEGER NOT NULL CHECK (store_schema_version = 1),
    schema_identity TEXT NOT NULL CHECK (schema_identity = 'troupe.diagnostics.store.v1'),
    event_schema_version INTEGER NOT NULL CHECK (event_schema_version = 1),
    run_id TEXT NOT NULL UNIQUE CHECK (
        length(run_id) = 36 AND
        run_id = lower(run_id) AND
        substr(run_id, 9, 1) = '-' AND
        substr(run_id, 14, 1) = '-' AND
        substr(run_id, 19, 1) = '-' AND
        substr(run_id, 24, 1) = '-'
    ),
    started_at TEXT NOT NULL CHECK (length(started_at) > 0),
    ended_at TEXT,
    production_outcome TEXT CHECK (production_outcome IN ('completed', 'failed', 'cancelled')),
    configuration_identity TEXT NOT NULL CHECK (length(configuration_identity) > 0),
    committed_key BLOB NOT NULL CHECK (typeof(committed_key) = 'blob' AND length(committed_key) = 8),
    committed_sequence TEXT NOT NULL CHECK (
        committed_sequence NOT GLOB '*[^0-9]*' AND
        (committed_sequence = '0' OR substr(committed_sequence, 1, 1) BETWEEN '1' AND '9')
    ),
    read_model_key BLOB NOT NULL CHECK (typeof(read_model_key) = 'blob' AND length(read_model_key) = 8),
    read_model_sequence TEXT NOT NULL CHECK (
        read_model_sequence NOT GLOB '*[^0-9]*' AND
        (read_model_sequence = '0' OR substr(read_model_sequence, 1, 1) BETWEEN '1' AND '9')
    ),
    clean_shutdown INTEGER NOT NULL CHECK (clean_shutdown IN (0, 1)),
    CHECK (committed_key = read_model_key),
    CHECK (committed_sequence = read_model_sequence)
) STRICT;

CREATE TABLE events (
    sequence_key BLOB PRIMARY KEY CHECK (typeof(sequence_key) = 'blob' AND length(sequence_key) = 8),
    sequence TEXT NOT NULL UNIQUE CHECK (
        sequence NOT GLOB '*[^0-9]*' AND
        substr(sequence, 1, 1) BETWEEN '1' AND '9'
    ),
    run_id TEXT NOT NULL,
    event_schema_version INTEGER NOT NULL CHECK (event_schema_version = 1),
    elapsed_key BLOB NOT NULL CHECK (typeof(elapsed_key) = 'blob' AND length(elapsed_key) = 8),
    elapsed_ns TEXT NOT NULL CHECK (
        elapsed_ns NOT GLOB '*[^0-9]*' AND
        (elapsed_ns = '0' OR substr(elapsed_ns, 1, 1) BETWEEN '1' AND '9')
    ),
    kind TEXT NOT NULL CHECK (length(kind) > 0),
    scene_id TEXT,
    actor_id TEXT,
    cue_id TEXT,
    effect_id TEXT,
    act_id TEXT,
    tool_call_id TEXT,
    session_generation_key BLOB CHECK (
        session_generation_key IS NULL OR
        (typeof(session_generation_key) = 'blob' AND length(session_generation_key) = 8)
    ),
    session_generation TEXT CHECK (
        session_generation IS NULL OR (
            session_generation NOT GLOB '*[^0-9]*' AND
            substr(session_generation, 1, 1) BETWEEN '1' AND '9'
        )
    ),
    canonical_json BLOB NOT NULL CHECK (
        typeof(canonical_json) = 'blob' AND json_valid(CAST(canonical_json AS TEXT))
    ),
    FOREIGN KEY (run_id) REFERENCES run_metadata(run_id),
    CHECK ((session_generation_key IS NULL) = (session_generation IS NULL))
) STRICT, WITHOUT ROWID;

CREATE INDEX events_kind_sequence ON events(kind, sequence_key);
CREATE INDEX events_scene_sequence ON events(scene_id, sequence_key) WHERE scene_id IS NOT NULL;
CREATE INDEX events_actor_sequence ON events(actor_id, sequence_key) WHERE actor_id IS NOT NULL;
CREATE INDEX events_cue_sequence ON events(cue_id, sequence_key) WHERE cue_id IS NOT NULL;
CREATE INDEX events_effect_sequence ON events(effect_id, sequence_key) WHERE effect_id IS NOT NULL;
CREATE INDEX events_act_sequence ON events(act_id, sequence_key) WHERE act_id IS NOT NULL;
CREATE INDEX events_tool_call_sequence ON events(tool_call_id, sequence_key) WHERE tool_call_id IS NOT NULL;
CREATE INDEX events_elapsed_sequence ON events(elapsed_key, sequence_key);

CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'diagnostic events are append-only');
END;

CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'diagnostic events are append-only');
END;

CREATE TABLE materialized_spans (
    span_key BLOB PRIMARY KEY CHECK (typeof(span_key) = 'blob' AND length(span_key) = 8),
    span_sequence TEXT NOT NULL UNIQUE CHECK (
        span_sequence NOT GLOB '*[^0-9]*' AND
        substr(span_sequence, 1, 1) BETWEEN '1' AND '9'
    ),
    model_schema_version INTEGER NOT NULL CHECK (model_schema_version = 1),
    latest_sequence_key BLOB NOT NULL CHECK (typeof(latest_sequence_key) = 'blob' AND length(latest_sequence_key) = 8),
    latest_sequence TEXT NOT NULL CHECK (
        latest_sequence NOT GLOB '*[^0-9]*' AND
        substr(latest_sequence, 1, 1) BETWEEN '1' AND '9'
    ),
    payload_json BLOB NOT NULL CHECK (typeof(payload_json) = 'blob' AND json_valid(CAST(payload_json AS TEXT)))
) STRICT, WITHOUT ROWID;

CREATE TABLE materialized_messages (
    message_id TEXT PRIMARY KEY CHECK (length(message_id) > 0),
    model_schema_version INTEGER NOT NULL CHECK (model_schema_version = 1),
    latest_sequence_key BLOB NOT NULL CHECK (typeof(latest_sequence_key) = 'blob' AND length(latest_sequence_key) = 8),
    latest_sequence TEXT NOT NULL CHECK (
        latest_sequence NOT GLOB '*[^0-9]*' AND
        substr(latest_sequence, 1, 1) BETWEEN '1' AND '9'
    ),
    payload_json BLOB NOT NULL CHECK (typeof(payload_json) = 'blob' AND json_valid(CAST(payload_json AS TEXT)))
) STRICT, WITHOUT ROWID;

CREATE TABLE materialized_plans (
    scope_key TEXT PRIMARY KEY CHECK (length(scope_key) > 0),
    model_schema_version INTEGER NOT NULL CHECK (model_schema_version = 1),
    latest_sequence_key BLOB NOT NULL CHECK (typeof(latest_sequence_key) = 'blob' AND length(latest_sequence_key) = 8),
    latest_sequence TEXT NOT NULL CHECK (
        latest_sequence NOT GLOB '*[^0-9]*' AND
        substr(latest_sequence, 1, 1) BETWEEN '1' AND '9'
    ),
    payload_json BLOB NOT NULL CHECK (typeof(payload_json) = 'blob' AND json_valid(CAST(payload_json AS TEXT)))
) STRICT, WITHOUT ROWID;

CREATE TABLE materialized_counters (
    series_key TEXT PRIMARY KEY CHECK (length(series_key) > 0),
    model_schema_version INTEGER NOT NULL CHECK (model_schema_version = 1),
    latest_sequence_key BLOB NOT NULL CHECK (typeof(latest_sequence_key) = 'blob' AND length(latest_sequence_key) = 8),
    latest_sequence TEXT NOT NULL CHECK (
        latest_sequence NOT GLOB '*[^0-9]*' AND
        substr(latest_sequence, 1, 1) BETWEEN '1' AND '9'
    ),
    payload_json BLOB NOT NULL CHECK (typeof(payload_json) = 'blob' AND json_valid(CAST(payload_json AS TEXT)))
) STRICT, WITHOUT ROWID;

CREATE TABLE materialized_usage (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    model_schema_version INTEGER NOT NULL CHECK (model_schema_version = 1),
    through_sequence_key BLOB NOT NULL CHECK (typeof(through_sequence_key) = 'blob' AND length(through_sequence_key) = 8),
    through_sequence TEXT NOT NULL CHECK (
        through_sequence NOT GLOB '*[^0-9]*' AND
        (through_sequence = '0' OR substr(through_sequence, 1, 1) BETWEEN '1' AND '9')
    ),
    payload_json BLOB NOT NULL CHECK (typeof(payload_json) = 'blob' AND json_valid(CAST(payload_json AS TEXT)))
) STRICT;

CREATE TABLE materialized_snapshot (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    model_schema_version INTEGER NOT NULL CHECK (model_schema_version = 1),
    through_sequence_key BLOB NOT NULL CHECK (typeof(through_sequence_key) = 'blob' AND length(through_sequence_key) = 8),
    through_sequence TEXT NOT NULL CHECK (
        through_sequence NOT GLOB '*[^0-9]*' AND
        (through_sequence = '0' OR substr(through_sequence, 1, 1) BETWEEN '1' AND '9')
    ),
    payload_json BLOB NOT NULL CHECK (typeof(payload_json) = 'blob' AND json_valid(CAST(payload_json AS TEXT)))
) STRICT;

CREATE TABLE diagnostic_view_manifest (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    manifest_schema_version INTEGER NOT NULL CHECK (manifest_schema_version = 1),
    record_count INTEGER NOT NULL CHECK (record_count >= 0),
    manifest_json BLOB NOT NULL CHECK (
        typeof(manifest_json) = 'blob' AND json_valid(CAST(manifest_json AS TEXT))
    )
) STRICT;

CREATE TABLE diagnostic_view_records (
    view_id TEXT PRIMARY KEY CHECK (length(view_id) > 0),
    ordinal INTEGER NOT NULL UNIQUE CHECK (ordinal >= 0),
    view_schema_version INTEGER NOT NULL CHECK (view_schema_version >= 1),
    renderer TEXT NOT NULL CHECK (length(renderer) > 0),
    record_json BLOB NOT NULL CHECK (typeof(record_json) = 'blob')
) STRICT, WITHOUT ROWID;
