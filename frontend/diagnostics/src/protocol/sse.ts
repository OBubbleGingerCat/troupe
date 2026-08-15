import {
  type CanonicalUuid,
  type U64String,
  decodeCanonicalUuid,
  decodeU64,
  expectExactFields,
  expectObject,
  expectString,
  failProtocol,
  parseProtocolJson,
} from "./decimal.ts";
import {
  type DiagnosticEvent,
  decodeDiagnosticEvent,
} from "./event.ts";


export const SSE_CONTROL_NAMES = [
  "stream_ready",
  "heartbeat",
  "delivery_gap",
  "resync_required",
  "stream_closed",
] as const;

export type SseControlName = typeof SSE_CONTROL_NAMES[number];

interface SseControlBase {
  readonly control_schema_version: 1;
  readonly run_id: CanonicalUuid;
}

export interface StreamReadyControl extends SseControlBase {
  readonly resume_after: U64String;
  readonly replay_through: U64String;
}

export interface HeartbeatControl extends SseControlBase {
  readonly committed_watermark: U64String;
}

export interface DeliveryGapControl extends SseControlBase {
  readonly reason: string;
  readonly last_delivered_sequence: U64String;
  readonly committed_watermark: U64String;
}

export interface ResyncRequiredControl extends SseControlBase {
  readonly reason: string;
  readonly committed_watermark: U64String;
  readonly earliest_available_sequence: U64String | null;
}

export interface StreamClosedControl extends SseControlBase {
  readonly reason: string;
  readonly committed_watermark: U64String;
}

export type SseControl =
  | { readonly name: "stream_ready"; readonly payload: StreamReadyControl }
  | { readonly name: "heartbeat"; readonly payload: HeartbeatControl }
  | { readonly name: "delivery_gap"; readonly payload: DeliveryGapControl }
  | { readonly name: "resync_required"; readonly payload: ResyncRequiredControl }
  | { readonly name: "stream_closed"; readonly payload: StreamClosedControl };

export interface RawSseFrame {
  readonly event: string;
  readonly id: string | null;
  readonly data: unknown;
}

export type DecodedSseFrame =
  | {
    readonly frame_type: "event";
    readonly name: "diagnostic_event";
    readonly id: U64String;
    readonly event: DiagnosticEvent;
  }
  | {
    readonly frame_type: "control";
    readonly name: SseControlName;
    readonly id: null;
    readonly control: SseControl["payload"];
  };

function decodeData(value: unknown, path: string): unknown {
  return typeof value === "string" ? parseProtocolJson(value, path) : value;
}

function validateControlBase(payload: Record<string, unknown>, path: string): void {
  if (payload.control_schema_version !== 1) {
    failProtocol("control_schema_version", `${path}.control_schema_version`, "expected integer 1");
  }
  decodeCanonicalUuid(payload.run_id, `${path}.run_id`);
}

function nonemptyReason(value: unknown, path: string): string {
  const reason = expectString(value, path);
  if (reason.length === 0) {
    failProtocol("control", path, "reason must be nonempty");
  }
  return reason;
}

export function decodeSseControl(
  name: SseControlName,
  value: unknown,
  path = `control.${name}`,
): SseControl["payload"] {
  const payload = expectObject(decodeData(value, path), path);
  if (name === "stream_ready") {
    expectExactFields(
      payload,
      ["control_schema_version", "run_id", "resume_after", "replay_through"],
      path,
    );
    validateControlBase(payload, path);
    const resume = decodeU64(payload.resume_after, `${path}.resume_after`);
    const replay = decodeU64(payload.replay_through, `${path}.replay_through`);
    if (BigInt(resume) > BigInt(replay)) {
      failProtocol("control", path, "resume cursor is ahead of replay watermark");
    }
  } else if (name === "heartbeat") {
    expectExactFields(payload, ["control_schema_version", "run_id", "committed_watermark"], path);
    validateControlBase(payload, path);
    decodeU64(payload.committed_watermark, `${path}.committed_watermark`);
  } else if (name === "delivery_gap") {
    expectExactFields(
      payload,
      [
        "control_schema_version",
        "run_id",
        "reason",
        "last_delivered_sequence",
        "committed_watermark",
      ],
      path,
    );
    validateControlBase(payload, path);
    nonemptyReason(payload.reason, `${path}.reason`);
    const delivered = decodeU64(payload.last_delivered_sequence, `${path}.last_delivered_sequence`);
    const watermark = decodeU64(payload.committed_watermark, `${path}.committed_watermark`);
    if (BigInt(delivered) > BigInt(watermark)) {
      failProtocol("control", path, "last delivered sequence exceeds committed watermark");
    }
  } else if (name === "resync_required") {
    expectExactFields(
      payload,
      [
        "control_schema_version",
        "run_id",
        "reason",
        "committed_watermark",
        "earliest_available_sequence",
      ],
      path,
    );
    validateControlBase(payload, path);
    nonemptyReason(payload.reason, `${path}.reason`);
    const watermark = decodeU64(payload.committed_watermark, `${path}.committed_watermark`);
    if (payload.earliest_available_sequence !== null) {
      const earliest = decodeU64(
        payload.earliest_available_sequence,
        `${path}.earliest_available_sequence`,
      );
      if (earliest === "0" || BigInt(earliest) > BigInt(watermark)) {
        failProtocol("control", path, "earliest available sequence is outside retained history");
      }
    }
  } else {
    expectExactFields(
      payload,
      ["control_schema_version", "run_id", "reason", "committed_watermark"],
      path,
    );
    validateControlBase(payload, path);
    nonemptyReason(payload.reason, `${path}.reason`);
    decodeU64(payload.committed_watermark, `${path}.committed_watermark`);
  }
  return payload as unknown as SseControl["payload"];
}

export function decodeSseFrame(frame: RawSseFrame, path = "sse_frame"): DecodedSseFrame {
  if (frame.event === "diagnostic_event") {
    const id = decodeU64(frame.id, `${path}.id`);
    const event = decodeDiagnosticEvent(decodeData(frame.data, `${path}.data`), `${path}.data`);
    if (id !== event.sequence) {
      failProtocol("sse_id", `${path}.id`, "frame ID differs from canonical event sequence");
    }
    return { frame_type: "event", name: "diagnostic_event", id, event };
  }
  if (!(SSE_CONTROL_NAMES as readonly string[]).includes(frame.event)) {
    failProtocol("discriminant", `${path}.event`, `unknown SSE event ${JSON.stringify(frame.event)}`);
  }
  if (frame.id !== null) {
    failProtocol("sse_id", `${path}.id`, "control frames must not carry an SSE ID");
  }
  const name = frame.event as SseControlName;
  return {
    frame_type: "control",
    name,
    id: null,
    control: decodeSseControl(name, frame.data, `${path}.data`),
  };
}
