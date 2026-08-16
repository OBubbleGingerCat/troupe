import { compareU64 } from "../protocol/decimal.ts";
import type { SnapshotResponse } from "../protocol/http.ts";
import type {
  DeliveryGapControl,
  HeartbeatControl,
  ResyncRequiredControl,
  StreamClosedControl,
  StreamReadyControl,
  DecodedSseFrame,
} from "../protocol/sse.ts";
import type {
  DiagnosticState,
  ServerRangeResumeRequest,
} from "../state/model.ts";
import {
  reduceDiagnosticState,
  type DiagnosticStateAction,
} from "../state/reducer.ts";
import {
  bootstrapDiagnostics,
  fetchDiagnosticStatus,
  DiagnosticTransportError,
  type BootstrapOptions,
  type DiagnosticBootstrap,
  type DiagnosticFetch,
  type DiagnosticStatus,
} from "./bootstrap.ts";
import {
  consumeResumeQueryIntent,
  pauseLivePresentation,
  resumeLivePresentation,
} from "./pause.ts";
import {
  fetchDiagnosticSnapshotWindow,
  stateFromSnapshotWindow,
} from "./snapshot.ts";
import {
  openDiagnosticEventStream,
  type DiagnosticEventStream,
  type EventSourceConstructor,
} from "./sse.ts";


export type LiveConnectionPhase =
  | "idle"
  | "bootstrapping"
  | "compatibility"
  | "snapshot"
  | "connecting"
  | "live"
  | "reconnecting"
  | "resyncing"
  | "archive"
  | "closed"
  | "failed"
  | "stopped";

export type ShellConnection = "connected" | "reconnecting" | "offline" | "archive";
export type RunOutcome = "running" | "completed" | "failed" | "cancelled" | "incomplete";
export type LiveSecurityState = "unknown" | "trusted_network" | "unsupported";

export interface LiveControllerError {
  readonly code: string;
  readonly message: string;
}

export interface LiveDiagnosticsState {
  readonly phase: LiveConnectionPhase;
  readonly connection: ShellConnection;
  readonly security: LiveSecurityState;
  readonly security_scope: "trusted_network" | null;
  readonly outcome: RunOutcome | null;
  readonly bootstrap: DiagnosticBootstrap | null;
  readonly status: DiagnosticStatus | null;
  readonly snapshot: SnapshotResponse | null;
  readonly diagnostics: DiagnosticState | null;
  readonly terminal_reason: string | null;
  readonly error: LiveControllerError | null;
}

export interface LiveDiagnosticsControllerOptions extends BootstrapOptions {
  readonly scheduleDraw?: () => void;
}

export type LiveStateListener = (state: LiveDiagnosticsState) => void;

const INITIAL_STATE: LiveDiagnosticsState = {
  phase: "idle",
  connection: "offline",
  security: "unknown",
  security_scope: null,
  outcome: null,
  bootstrap: null,
  status: null,
  snapshot: null,
  diagnostics: null,
  terminal_reason: null,
  error: null,
};

function outcomeFromStatus(status: DiagnosticStatus): RunOutcome {
  switch (status.lifecycle.state) {
    case "active":
      return "running";
    case "completed":
      return "completed";
    case "incomplete":
      return "incomplete";
    case "failed":
      return status.lifecycle.outcome === "cancelled" ? "cancelled" : "failed";
  }
}

function errorState(error: unknown): LiveControllerError {
  if (error instanceof DiagnosticTransportError) {
    return { code: error.code, message: error.message };
  }
  if (error instanceof Error) {
    const code = "code" in error && typeof error.code === "string" ? error.code : "protocol";
    return { code, message: error.message };
  }
  return { code: "unknown", message: String(error) };
}

function isSecurityFailure(error: unknown): boolean {
  return error instanceof DiagnosticTransportError
    ? error.code === "security"
    : error instanceof Error && "code" in error && error.code === "security_scope";
}

export class LiveDiagnosticsController {
  private current: LiveDiagnosticsState = INITIAL_STATE;
  private readonly listeners = new Set<LiveStateListener>();
  private readonly baseUrl: string | URL | undefined;
  private readonly fetchImpl: DiagnosticFetch | undefined;
  private readonly EventSourceImpl: EventSourceConstructor | undefined;
  private readonly scheduleDraw: () => void;
  private stream: DiagnosticEventStream | null = null;
  private generation = 0;
  private readySeen = false;
  private expectedResume: string | null = null;
  private pendingOperation: Promise<void> | null = null;
  private stopped = false;

  constructor(options: LiveDiagnosticsControllerOptions = {}) {
    this.baseUrl = options.baseUrl;
    this.fetchImpl = options.fetch ?? (
      typeof globalThis.fetch === "function" ? globalThis.fetch.bind(globalThis) : undefined
    );
    this.EventSourceImpl = options.EventSource ?? (
      typeof globalThis.EventSource === "function"
        ? globalThis.EventSource as unknown as EventSourceConstructor
        : undefined
    );
    this.scheduleDraw = options.scheduleDraw ?? (() => undefined);
  }

  get state(): LiveDiagnosticsState {
    return this.current;
  }

  subscribe(listener: LiveStateListener): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  async start(): Promise<void> {
    if (this.current.phase !== "idle") {
      return;
    }
    this.update({
      phase: "bootstrapping",
      connection: "reconnecting",
      error: null,
    });
    try {
      const options: BootstrapOptions = {
        ...(this.baseUrl === undefined ? {} : { baseUrl: this.baseUrl }),
        ...(this.fetchImpl === undefined ? {} : { fetch: this.fetchImpl }),
        ...(this.EventSourceImpl === undefined ? {} : { EventSource: this.EventSourceImpl }),
      };
      const bootstrap = await bootstrapDiagnostics(options);
      if (this.stopped) {
        return;
      }
      const shell = {
        bootstrap,
        status: bootstrap.status,
        security: "trusted_network" as const,
        security_scope: "trusted_network" as const,
        outcome: outcomeFromStatus(bootstrap.status),
      };
      if (bootstrap.compatibility.mode !== "interactive") {
        this.update({
          ...shell,
          phase: "compatibility",
          connection: "offline",
        });
        return;
      }
      this.update({ ...shell, phase: "snapshot", connection: "reconnecting" });
      const snapshotWindow = await fetchDiagnosticSnapshotWindow(bootstrap, this.requireFetch());
      if (this.stopped) {
        return;
      }
      const { snapshot } = snapshotWindow;
      const diagnostics = stateFromSnapshotWindow(snapshotWindow);
      if (bootstrap.status.source === "archive") {
        this.update({
          snapshot,
          diagnostics,
          phase: "archive",
          connection: "archive",
        }, true);
        return;
      }
      if (bootstrap.status.lifecycle.state !== "active") {
        this.update({
          snapshot,
          diagnostics,
          phase: "closed",
          connection: "offline",
        }, true);
        return;
      }
      this.update({
        snapshot,
        diagnostics,
        phase: "connecting",
        connection: "reconnecting",
      }, true);
      this.openStream(snapshot.watermark_sequence);
    } catch (error) {
      this.fail(error);
    }
  }

  stop(): void {
    if (this.stopped) {
      return;
    }
    this.stopped = true;
    this.generation += 1;
    this.closeStream();
    this.update({ phase: "stopped", connection: "offline" });
  }

  dispatch(action: DiagnosticStateAction): void {
    const diagnostics = this.requireState();
    if (action.type === "pause") {
      this.commitDiagnostic(pauseLivePresentation(diagnostics));
      return;
    }
    if (action.type === "resume") {
      this.commitDiagnostic(resumeLivePresentation(diagnostics).state);
      return;
    }
    if (action.type === "resume_request_consumed") {
      this.commitDiagnostic(consumeResumeQueryIntent(diagnostics));
      return;
    }
    this.commitDiagnostic(reduceDiagnosticState(diagnostics, action));
  }

  pause(): void {
    this.commitDiagnostic(pauseLivePresentation(this.requireState()));
  }

  resume(): ServerRangeResumeRequest | null {
    const result = resumeLivePresentation(this.requireState());
    this.commitDiagnostic(result.state);
    return result.query_intent;
  }

  consumeResumeQueryIntent(): void {
    this.commitDiagnostic(consumeResumeQueryIntent(this.requireState()));
  }

  async whenSettled(): Promise<void> {
    while (this.pendingOperation !== null) {
      await this.pendingOperation;
    }
  }

  private requireFetch(): DiagnosticFetch {
    if (this.fetchImpl === undefined) {
      throw new DiagnosticTransportError("browser_capability", "native fetch is unavailable");
    }
    return this.fetchImpl;
  }

  private requireState(): DiagnosticState {
    if (this.current.diagnostics === null) {
      throw new DiagnosticTransportError("protocol", "diagnostic state is not initialized");
    }
    return this.current.diagnostics;
  }

  private update(patch: Partial<LiveDiagnosticsState>, draw = false): void {
    this.current = { ...this.current, ...patch };
    if (draw) {
      this.scheduleDraw();
    }
    for (const listener of this.listeners) {
      listener(this.current);
    }
  }

  private commitDiagnostic(next: DiagnosticState): void {
    if (next === this.current.diagnostics) {
      return;
    }
    this.update({ diagnostics: next }, true);
  }

  private closeStream(): void {
    this.stream?.close();
    this.stream = null;
    this.readySeen = false;
    this.expectedResume = null;
  }

  private openStream(after: DiagnosticState["cursor"]["delivered_through"]): void {
    const bootstrap = this.current.bootstrap;
    if (bootstrap === null) {
      throw new DiagnosticTransportError("protocol", "cannot open SSE before bootstrap");
    }
    const generation = ++this.generation;
    this.readySeen = false;
    this.expectedResume = after;
    this.stream = openDiagnosticEventStream({
      bootstrap,
      after,
      ...(this.EventSourceImpl === undefined ? {} : { EventSource: this.EventSourceImpl }),
      onOpen: () => {
        if (!this.activeGeneration(generation)) {
          return;
        }
        this.readySeen = false;
        this.expectedResume = this.requireState().cursor.delivered_through;
        this.update({
          phase: this.current.phase === "connecting" ? "connecting" : "reconnecting",
          connection: "reconnecting",
          error: null,
        });
      },
      onFrame: (frame) => {
        if (this.activeGeneration(generation)) {
          this.handleFrame(frame);
        }
      },
      onError: () => {
        if (!this.activeGeneration(generation) || this.isTerminal()) {
          return;
        }
        this.readySeen = false;
        this.expectedResume = this.requireState().cursor.delivered_through;
        this.update({
          phase: "reconnecting",
          connection: "reconnecting",
          error: null,
        });
      },
      onProtocolError: (error) => {
        if (this.activeGeneration(generation)) {
          this.fail(error);
        }
      },
    });
  }

  private activeGeneration(generation: number): boolean {
    return !this.stopped && generation === this.generation;
  }

  private isTerminal(): boolean {
    return this.current.phase === "archive"
      || this.current.phase === "closed"
      || this.current.phase === "failed"
      || this.current.phase === "stopped"
      || this.current.phase === "compatibility";
  }

  private handleFrame(frame: DecodedSseFrame): void {
    const diagnostics = this.requireState();
    if (frame.frame_type === "event") {
      if (!this.readySeen) {
        this.fail(new DiagnosticTransportError("protocol", "event arrived before stream_ready"));
        return;
      }
      if (frame.event.run_id !== diagnostics.run_id) {
        this.fail(new DiagnosticTransportError("identity", "SSE event belongs to another Run"));
        return;
      }
      const next = reduceDiagnosticState(diagnostics, {
        type: "event_received",
        event: frame.event,
      });
      if (next === diagnostics) {
        return;
      }
      this.update({ diagnostics: next }, true);
      if (next.delivery_issue !== null) {
        this.beginResync(next.delivery_issue.kind);
      }
      return;
    }
    if (frame.control.run_id !== diagnostics.run_id) {
      this.fail(new DiagnosticTransportError("identity", "SSE control belongs to another Run"));
      return;
    }
    switch (frame.name) {
      case "stream_ready":
        this.handleReady(frame.control as StreamReadyControl);
        return;
      case "heartbeat":
        if (!this.requireReadyControl()) {
          return;
        }
        this.observeWatermark((frame.control as HeartbeatControl).committed_watermark);
        return;
      case "delivery_gap": {
        const control = frame.control as DeliveryGapControl;
        this.observeWatermark(control.committed_watermark);
        this.beginResync(control.reason);
        return;
      }
      case "resync_required": {
        const control = frame.control as ResyncRequiredControl;
        this.observeWatermark(control.committed_watermark);
        this.beginResync(control.reason);
        return;
      }
      case "stream_closed":
        this.handleClosed(frame.control as StreamClosedControl);
        return;
    }
  }

  private requireReadyControl(): boolean {
    if (!this.readySeen) {
      this.fail(new DiagnosticTransportError("protocol", "control arrived before stream_ready"));
      return false;
    }
    return true;
  }

  private handleReady(control: StreamReadyControl): void {
    if (this.readySeen) {
      this.fail(new DiagnosticTransportError("protocol", "duplicate stream_ready control"));
      return;
    }
    const diagnostics = this.requireState();
    if (
      this.expectedResume === null
      || control.resume_after !== this.expectedResume
      || compareU64(control.replay_through, diagnostics.cursor.committed_watermark) < 0
    ) {
      this.beginResync("stream_ready_cursor_mismatch");
      return;
    }
    this.readySeen = true;
    const next = reduceDiagnosticState(diagnostics, {
      type: "watermark_observed",
      through_sequence: control.replay_through,
    });
    this.update({
      diagnostics: next,
      phase: "live",
      connection: "connected",
      error: null,
    }, next !== diagnostics);
  }

  private observeWatermark(watermark: DiagnosticState["cursor"]["committed_watermark"]): void {
    const diagnostics = this.requireState();
    const next = reduceDiagnosticState(diagnostics, {
      type: "watermark_observed",
      through_sequence: watermark,
    });
    this.commitDiagnostic(next);
  }

  private handleClosed(control: StreamClosedControl): void {
    const diagnostics = this.requireState();
    const next = reduceDiagnosticState(diagnostics, {
      type: "watermark_observed",
      through_sequence: control.committed_watermark,
    });
    this.generation += 1;
    this.closeStream();
    this.update({
      diagnostics: next,
      phase: "closed",
      connection: "offline",
      terminal_reason: control.reason,
      error: null,
    }, next !== diagnostics);
    this.trackOperation(this.refreshTerminalStatus());
  }

  private beginResync(reason: string): void {
    if (this.stopped || this.pendingOperation !== null || this.isTerminal()) {
      return;
    }
    const generation = ++this.generation;
    this.closeStream();
    this.update({
      phase: "resyncing",
      connection: "reconnecting",
      error: null,
    });
    this.trackOperation(this.resynchronize(generation));
  }

  private async resynchronize(generation: number): Promise<void> {
    try {
      const bootstrap = this.current.bootstrap;
      if (bootstrap === null) {
        throw new DiagnosticTransportError("protocol", "cannot resync before bootstrap");
      }
      const status = await fetchDiagnosticStatus(bootstrap, this.requireFetch());
      const snapshotWindow = await fetchDiagnosticSnapshotWindow(bootstrap, this.requireFetch());
      if (!this.activeGeneration(generation)) {
        return;
      }
      const { snapshot } = snapshotWindow;
      const diagnostics = stateFromSnapshotWindow(snapshotWindow, this.requireState());
      const refreshedBootstrap = { ...bootstrap, status };
      const outcome = outcomeFromStatus(status);
      if (status.source === "archive") {
        this.update({
          bootstrap: refreshedBootstrap,
          status,
          snapshot,
          diagnostics,
          outcome,
          phase: "archive",
          connection: "archive",
        }, true);
        return;
      }
      if (status.lifecycle.state !== "active") {
        this.update({
          bootstrap: refreshedBootstrap,
          status,
          snapshot,
          diagnostics,
          outcome,
          phase: "closed",
          connection: "offline",
        }, true);
        return;
      }
      this.update({
        bootstrap: refreshedBootstrap,
        status,
        snapshot,
        diagnostics,
        outcome,
        phase: "connecting",
        connection: "reconnecting",
      }, true);
      this.openStream(snapshot.watermark_sequence);
    } catch (error) {
      if (this.activeGeneration(generation)) {
        this.fail(error);
      }
    }
  }

  private async refreshTerminalStatus(): Promise<void> {
    const bootstrap = this.current.bootstrap;
    if (bootstrap === null) {
      return;
    }
    try {
      const status = await fetchDiagnosticStatus(bootstrap, this.requireFetch());
      if (this.stopped || this.current.phase !== "closed") {
        return;
      }
      this.update({
        bootstrap: { ...bootstrap, status },
        status,
        outcome: outcomeFromStatus(status),
      });
    } catch {
      // The Runtime may close its listener immediately after stream_closed.
    }
  }

  private trackOperation(operation: Promise<void>): void {
    this.pendingOperation = operation;
    void operation.finally(() => {
      if (this.pendingOperation === operation) {
        this.pendingOperation = null;
      }
    });
  }

  private fail(error: unknown): void {
    if (this.stopped) {
      return;
    }
    this.generation += 1;
    this.closeStream();
    this.update({
      phase: "failed",
      connection: "offline",
      security: isSecurityFailure(error) ? "unsupported" : this.current.security,
      security_scope: isSecurityFailure(error) ? null : this.current.security_scope,
      error: errorState(error),
    });
  }
}

export function createLiveDiagnosticsController(
  options: LiveDiagnosticsControllerOptions = {},
): LiveDiagnosticsController {
  return new LiveDiagnosticsController(options);
}

export async function startLiveDiagnostics(
  options: LiveDiagnosticsControllerOptions = {},
): Promise<LiveDiagnosticsController> {
  const controller = createLiveDiagnosticsController(options);
  await controller.start();
  return controller;
}
