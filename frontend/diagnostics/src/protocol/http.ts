import {
  type CanonicalUuid,
  type JsonObject,
  type U64String,
  decodeCanonicalUuid,
  decodeJsonValue,
  decodeU64,
  expectArray,
  expectExactFields,
  expectObject,
  expectString,
  failProtocol,
} from "./decimal.ts";
import {
  type DiagnosticEvent,
  decodeDiagnosticEvent,
} from "./event.ts";


export interface VersionedApiObject {
  readonly api_schema_version: number;
  readonly run_id: CanonicalUuid;
  readonly [key: string]: unknown;
}

export interface SnapshotResponse {
  readonly api_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly watermark_sequence: U64String;
  readonly earliest_available_sequence: U64String | null;
  readonly state: JsonObject;
}

export interface EventsResponse {
  readonly api_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly captured_watermark: U64String;
  readonly events: readonly DiagnosticEvent[];
  readonly next_after: U64String | null;
}

export interface HttpErrorResponse {
  readonly api_schema_version: 1;
  readonly run_id: CanonicalUuid | null;
  readonly error: {
    readonly code: string;
    readonly message: string;
    readonly details: JsonObject | null;
  };
}

function requireApiVersion(value: unknown, path: string): void {
  if (value !== 1) {
    failProtocol("api_schema_version", path, "expected integer 1");
  }
}

export function decodeVersionedApiObject(value: unknown, path = "response"): VersionedApiObject {
  const response = expectObject(value, path);
  if (
    !Object.prototype.hasOwnProperty.call(response, "api_schema_version")
    || !Object.prototype.hasOwnProperty.call(response, "run_id")
  ) {
    failProtocol("fields", path, "versioned API object requires api_schema_version and run_id");
  }
  requireApiVersion(response.api_schema_version, `${path}.api_schema_version`);
  decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  decodeJsonValue(response, path);
  return response as unknown as VersionedApiObject;
}

export function decodeSnapshotResponse(value: unknown, path = "snapshot"): SnapshotResponse {
  const response = expectObject(value, path);
  expectExactFields(
    response,
    ["api_schema_version", "run_id", "watermark_sequence", "earliest_available_sequence", "state"],
    path,
  );
  requireApiVersion(response.api_schema_version, `${path}.api_schema_version`);
  decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  const watermark = decodeU64(response.watermark_sequence, `${path}.watermark_sequence`);
  const earliest = response.earliest_available_sequence === null
    ? null
    : decodeU64(response.earliest_available_sequence, `${path}.earliest_available_sequence`);
  const state = expectObject(response.state, `${path}.state`);
  decodeJsonValue(state, `${path}.state`);
  if ((watermark === "0") !== (earliest === null)) {
    failProtocol("snapshot", path, "empty watermark and earliest replay sequence disagree");
  }
  if (earliest !== null && (earliest === "0" || BigInt(earliest) > BigInt(watermark))) {
    failProtocol("snapshot", `${path}.earliest_available_sequence`, "replay range is invalid");
  }
  return response as unknown as SnapshotResponse;
}

export function decodeEventsResponse(value: unknown, path = "events_response"): EventsResponse {
  const response = expectObject(value, path);
  expectExactFields(
    response,
    ["api_schema_version", "run_id", "captured_watermark", "events", "next_after"],
    path,
  );
  requireApiVersion(response.api_schema_version, `${path}.api_schema_version`);
  const runId = decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  const watermark = decodeU64(response.captured_watermark, `${path}.captured_watermark`);
  const events = expectArray(response.events, `${path}.events`);
  let previous = 0n;
  events.forEach((raw, index) => {
    const event = decodeDiagnosticEvent(raw, `${path}.events[${index}]`);
    const sequence = BigInt(event.sequence);
    if (event.run_id !== runId) {
      failProtocol("run_id", `${path}.events[${index}].run_id`, "event belongs to another run");
    }
    if (sequence <= previous || sequence > BigInt(watermark)) {
      failProtocol("sequence", `${path}.events[${index}].sequence`, "event is outside captured order");
    }
    previous = sequence;
  });
  const nextAfter = response.next_after === null
    ? null
    : decodeU64(response.next_after, `${path}.next_after`);
  const finalEvent = events[events.length - 1] as { sequence: string } | undefined;
  if (nextAfter !== null && (finalEvent === undefined || nextAfter !== finalEvent.sequence)) {
    failProtocol("cursor", `${path}.next_after`, "page cursor must identify the final returned event");
  }
  return response as unknown as EventsResponse;
}

export function decodeHttpErrorResponse(value: unknown, path = "error_response"): HttpErrorResponse {
  const response = expectObject(value, path);
  expectExactFields(response, ["api_schema_version", "run_id", "error"], path);
  requireApiVersion(response.api_schema_version, `${path}.api_schema_version`);
  if (response.run_id !== null) {
    decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  }
  const error = expectObject(response.error, `${path}.error`);
  expectExactFields(error, ["code", "message", "details"], `${path}.error`);
  const code = expectString(error.code, `${path}.error.code`);
  const message = expectString(error.message, `${path}.error.message`);
  if (code.length === 0 || message.length === 0) {
    failProtocol("http_error", `${path}.error`, "error code and message must be nonempty");
  }
  if (error.details !== null) {
    decodeJsonValue(expectObject(error.details, `${path}.error.details`), `${path}.error.details`);
  }
  return response as unknown as HttpErrorResponse;
}
