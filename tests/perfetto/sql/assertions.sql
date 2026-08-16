WITH
  track_rows AS (
    SELECT
      track.id,
      track.name,
      track.type,
      parent.name AS parent_name
    FROM track
    LEFT JOIN track AS parent ON parent.id = track.parent_id
    ORDER BY track.id
  ),
  slice_rows AS (
    SELECT
      slice.id,
      slice.name,
      CAST(slice.ts AS TEXT) AS ts,
      CAST(slice.dur AS TEXT) AS dur,
      track.name AS track_name,
      slice.depth
    FROM slice
    JOIN track ON track.id = slice.track_id
    ORDER BY slice.id
  ),
  counter_rows AS (
    SELECT
      counter.id,
      counter_track.name,
      CAST(counter.ts AS TEXT) AS ts,
      printf('%.17g', counter.value) AS value,
      counter_track.type AS track_type
    FROM counter
    JOIN counter_track ON counter_track.id = counter.track_id
    ORDER BY counter.id
  ),
  flow_rows AS (
    SELECT
      flow.id,
      outgoing.name AS outgoing_name,
      incoming.name AS incoming_name
    FROM flow
    LEFT JOIN slice AS outgoing ON outgoing.id = flow.slice_out
    LEFT JOIN slice AS incoming ON incoming.id = flow.slice_in
    ORDER BY flow.id
  ),
  troupe_arg_keys AS (
    SELECT DISTINCT flat_key
    FROM args
    WHERE flat_key GLOB 'debug.troupe_*'
    ORDER BY flat_key
  )
SELECT hex(CAST(json_object(
  'schema', 'troupe.perfetto.sql-result.v1',
  'counts', json_object(
    'tracks', (SELECT count(*) FROM track),
    'slices', (SELECT count(*) FROM slice),
    'counters', (SELECT count(*) FROM counter),
    'flows', (SELECT count(*) FROM flow),
    'args', (SELECT count(*) FROM args),
    'metadata', (SELECT count(*) FROM metadata),
    'track_event_stats', (
      SELECT count(*) FROM stats WHERE name GLOB 'track_event_*'
    )
  ),
  'tracks', json((
    SELECT json_group_array(json_object(
      'name', name,
      'type', type,
      'parent', parent_name
    ))
    FROM track_rows
  )),
  'slices', json((
    SELECT json_group_array(json_object(
      'name', name,
      'ts', ts,
      'dur', dur,
      'track', track_name,
      'depth', depth
    ))
    FROM slice_rows
  )),
  'counters', json((
    SELECT json_group_array(json_object(
      'name', name,
      'ts', ts,
      'value', value,
      'track_type', track_type
    ))
    FROM counter_rows
  )),
  'flows', json((
    SELECT json_group_array(json_object(
      'outgoing', outgoing_name,
      'incoming', incoming_name
    ))
    FROM flow_rows
  )),
  'args', json_object(
    'troupe_count', (
      SELECT count(*) FROM args WHERE flat_key GLOB 'debug.troupe_*'
    ),
    'keys', json((
      SELECT json_group_array(flat_key) FROM troupe_arg_keys
    ))
  ),
  'metadata', json_object(
    'trace_type', (
      SELECT str_value FROM metadata WHERE name = 'trace_type'
    ),
    'trace_size_bytes', CAST((
      SELECT int_value FROM metadata WHERE name = 'trace_size_bytes'
    ) AS TEXT),
    'production_roots', (
      SELECT count(*) FROM track WHERE name GLOB 'Troupe Production *'
    )
  ),
  'stats', json_object(
    'missing_sequence_id', coalesce((
      SELECT value FROM stats WHERE name = 'track_event_missing_sequence_id'
    ), -1),
    'invalid_counter_track_uuid', coalesce((
      SELECT value FROM stats WHERE name = 'track_event_counter_invalid_track_uuid'
    ), -1),
    'nonzero_errors', (
      SELECT count(*) FROM stats WHERE severity = 'error' AND value != 0
    )
  ),
  'facts', json_object(
    'open_slices', (
      SELECT count(*) FROM slice WHERE dur = -1
    ),
    'overlapping_cue_pairs', (
      SELECT count(*)
      FROM slice AS first
      JOIN slice AS second ON first.id < second.id
      WHERE first.name = 'cue.execution'
        AND second.name = 'cue.execution'
        AND first.dur >= 0
        AND second.dur >= 0
        AND first.ts < second.ts + second.dur
        AND second.ts < first.ts + first.dur
    ),
    'non_exact_fallbacks', (
      SELECT count(*)
      FROM slice
      WHERE slice.name = 'numeric.not_exact'
        AND EXISTS (
          SELECT 1 FROM args
          WHERE args.arg_set_id = slice.arg_set_id
            AND args.flat_key = 'debug.troupe_counter_projection'
            AND args.string_value = 'not_exact'
        )
        AND EXISTS (
          SELECT 1 FROM args
          WHERE args.arg_set_id = slice.arg_set_id
            AND args.flat_key = 'debug.troupe_counter_value_decimal'
            AND args.string_value = '0.1'
        )
    ),
    'fallback_counter_tracks', (
      SELECT count(*) FROM counter_track WHERE name = 'numeric.not_exact'
    ),
    'i64_max_counters', (
      SELECT count(*) FROM counter_track
      JOIN counter ON counter.track_id = counter_track.id
      WHERE counter_track.name = 'numeric.i64_max'
    )
  )
) AS BLOB)) AS result_hex;
