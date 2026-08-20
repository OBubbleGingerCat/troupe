import {
  compareU64,
  decodeU64,
  type U64String,
} from "../protocol/decimal.ts";
import {
  decodeEventsResponse,
  decodeSnapshotResponse,
  type EventsResponse,
  type SnapshotResponse,
} from "../protocol/http.ts";
import type { DiagnosticState } from "../state/model.ts";
import { hydrateDiagnosticStateFromSnapshot } from "../state/reducer.ts";
import {
  DiagnosticTransportError,
  diagnosticApiUrl,
  fetchSameOriginJson,
  type DiagnosticBootstrap,
  type DiagnosticFetch,
} from "./bootstrap.ts";


export const SNAPSHOT_SUFFIX_MAX_EVENTS = 4_096;
const SNAPSHOT_SUFFIX_MAX_EVENTS_BIGINT = 4_096n;

export interface DiagnosticSnapshotWindow {
  readonly snapshot: SnapshotResponse;
  readonly after: U64String;
  readonly suffix: EventsResponse;
}

export function snapshotSuffixAfter(watermark: U64String): U64String {
  const through = BigInt(watermark);
  return decodeU64(String(
    through > SNAPSHOT_SUFFIX_MAX_EVENTS_BIGINT
      ? through - SNAPSHOT_SUFFIX_MAX_EVENTS_BIGINT
      : 0n,
  ));
}

export function diagnosticSnapshotSuffixUrl(
  bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url">,
  after: U64String,
  through: U64String,
): URL {
  const url = diagnosticApiUrl(bootstrap, "events");
  url.searchParams.set("after", after);
  url.searchParams.set("through", through);
  return url;
}

export async function fetchDiagnosticSnapshot(
  bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url" | "identity">,
  fetchImpl: DiagnosticFetch = globalThis.fetch,
): Promise<SnapshotResponse> {
  if (typeof fetchImpl !== "function") {
    throw new DiagnosticTransportError("browser_capability", "native fetch is unavailable");
  }
  const snapshot = decodeSnapshotResponse(await fetchSameOriginJson(
    diagnosticApiUrl(bootstrap, "snapshot"),
    bootstrap.origin,
    fetchImpl,
    "diagnostic snapshot",
  ));
  if (snapshot.run_id !== bootstrap.identity.run_id) {
    throw new DiagnosticTransportError("identity", "snapshot belongs to another Run");
  }
  return snapshot;
}

export async function fetchDiagnosticSnapshotWindow(
  bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url" | "identity">,
  fetchImpl: DiagnosticFetch = globalThis.fetch,
): Promise<DiagnosticSnapshotWindow> {
  const snapshot = await fetchDiagnosticSnapshot(bootstrap, fetchImpl);
  const after = snapshotSuffixAfter(snapshot.watermark_sequence);
  const response = decodeEventsResponse(await fetchSameOriginJson(
    diagnosticSnapshotSuffixUrl(bootstrap, after, snapshot.watermark_sequence),
    bootstrap.origin,
    fetchImpl,
    "diagnostic snapshot suffix",
  ));
  if (response.run_id !== snapshot.run_id) {
    throw new DiagnosticTransportError("identity", "snapshot suffix belongs to another Run");
  }
  if (compareU64(response.captured_watermark, snapshot.watermark_sequence) < 0) {
    throw new DiagnosticTransportError(
      "protocol",
      "snapshot suffix was captured before the snapshot watermark",
    );
  }
  if (response.next_after !== null) {
    throw new DiagnosticTransportError(
      "protocol",
      "finite snapshot suffix unexpectedly returned a continuation cursor",
    );
  }
  if (response.events.length > SNAPSHOT_SUFFIX_MAX_EVENTS) {
    throw new DiagnosticTransportError(
      "protocol",
      `snapshot suffix exceeds ${SNAPSHOT_SUFFIX_MAX_EVENTS} events`,
    );
  }

  const expectedCount = Number(BigInt(snapshot.watermark_sequence) - BigInt(after));
  if (response.events.length !== expectedCount) {
    throw new DiagnosticTransportError(
      "protocol",
      `snapshot suffix is not the exact dense range (${after},${snapshot.watermark_sequence}]`,
    );
  }
  let expectedSequence = BigInt(after) + 1n;
  for (const event of response.events) {
    if (event.run_id !== snapshot.run_id || BigInt(event.sequence) !== expectedSequence) {
      throw new DiagnosticTransportError(
        event.run_id === snapshot.run_id ? "protocol" : "identity",
        `snapshot suffix is not the exact dense range (${after},${snapshot.watermark_sequence}]`,
      );
    }
    expectedSequence += 1n;
  }
  return { snapshot, after, suffix: response };
}

export function stateFromSnapshotWindow(
  window: DiagnosticSnapshotWindow,
  previous: DiagnosticState | null = null,
): DiagnosticState {
  const { snapshot } = window;
  if (previous !== null) {
    if (previous.run_id !== snapshot.run_id) {
      throw new DiagnosticTransportError("identity", "cannot rebase state across Runs");
    }
    if (compareU64(snapshot.watermark_sequence, previous.cursor.delivered_through) < 0) {
      throw new DiagnosticTransportError(
        "protocol",
        "server snapshot watermark moved behind the delivered cursor",
      );
    }
  }
  return hydrateDiagnosticStateFromSnapshot({
    snapshot,
    suffix: window.suffix,
    after: window.after,
    ...(previous === null ? {} : { previous }),
  });
}
