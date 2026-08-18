import type { JSX } from "preact";
import { ArrowLeft, ArrowRight, Radio, ZoomIn, ZoomOut } from "lucide-preact";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "preact/hooks";

import { EventInspector } from "./inspector/EventInspector.tsx";
import {
  EventTable,
  summarizeEvent,
} from "./inspector/EventTable.tsx";
import {
  EMPTY_EVENT_QUERY,
  FilterBar,
  type EventQueryState,
} from "./inspector/FilterBar.tsx";
import {
  eventSelectionHighlight,
  resolveSelection,
} from "./inspector/selection.ts";
import type { DiagnosticBootstrap } from "./live/bootstrap.ts";
import {
  type LiveDiagnosticsController,
  type LiveDiagnosticsState,
  createLiveDiagnosticsController,
} from "./live/reconnect.ts";
import type { U64String } from "./protocol/decimal.ts";
import type {
  DiagnosticEvent,
  DiagnosticScope,
} from "./protocol/event.ts";
import type {
  ViewRecord,
  ViewRenderer,
} from "./protocol/view.ts";
import type { ViewQueryContext } from "./query/binding.ts";
import {
  type ViewCatalog,
  type ViewQueryClient,
  type ViewQueryResult,
  createViewQueryClient,
  isCompatibleViewCatalogEntry,
  viewCatalogEntryId,
} from "./query/client.ts";
import { AppShell } from "./shell/AppShell.tsx";
import {
  PRIMARY_SECTIONS,
  type PrimarySection,
} from "./shell/PrimaryToolbar.tsx";
import type {
  DiagnosticState,
  SelectionReference,
} from "./state/model.ts";
import {
  type DiagnosticStateAction,
  presentedLiveEdge,
} from "./state/reducer.ts";
import { scopeFromReference } from "./state/selection.ts";
import { TimelineTreegrid } from "./timeline/aria.tsx";
import { TimelineCanvas } from "./timeline/canvas.tsx";
import { buildTimelineHitIndex } from "./timeline/hit_test.ts";
import {
  layoutTimeline,
  selectTimelineModel,
} from "./timeline/layout.ts";
import {
  createTimelineViewport,
  followTimelineViewport,
  panTimelineViewport,
  pixelToElapsed,
  zoomTimelineViewport,
} from "./timeline/viewport.ts";
import type { TimeSeriesSelection } from "./timeseries/model.ts";
import { TimeSeriesRenderer } from "./timeseries/renderer.ts";
import { TranscriptPanel } from "./transcript/TranscriptPanel.tsx";
import { UsagePanel } from "./usage/UsagePanel.tsx";
import { ViewPanelErrorBoundary } from "./views/error_boundary.tsx";
import { MetricView } from "./views/metric.tsx";
import { TableView } from "./views/table.tsx";
import { TimelineView } from "./views/timeline.tsx";
import { TimeSeriesShell } from "./views/timeseries_shell.tsx";


export const REGISTERED_VIEW_RENDERERS = [
  "timeline",
  "metric",
  "table",
  "time_series",
] as const satisfies readonly ViewRenderer[];

export type DiagnosticsLiveController = Pick<
  LiveDiagnosticsController,
  "state" | "subscribe" | "start" | "stop" | "dispatch"
>;

export type DiagnosticsViewQueryClient = Pick<
  ViewQueryClient,
  "loadCatalog" | "query" | "reportRendererFailure" | "invalidateView" | "dispose"
>;

export type DiagnosticsViewClientFactory = (
  bootstrap: DiagnosticBootstrap,
) => DiagnosticsViewQueryClient;

export interface AppProps {
  readonly liveController?: DiagnosticsLiveController;
  readonly viewClientFactory?: DiagnosticsViewClientFactory;
  readonly productionName?: string;
}

type CatalogPresentation =
  | { readonly status: "idle" }
  | { readonly status: "loading" }
  | { readonly status: "ready"; readonly catalog: ViewCatalog; readonly client: DiagnosticsViewQueryClient }
  | { readonly status: "local_error"; readonly code: string; readonly message: string };

type QueryPresentation =
  | { readonly status: "idle"; readonly view_id: null }
  | { readonly status: "loading"; readonly view_id: string }
  | { readonly status: "ready"; readonly view_id: string; readonly result: ViewQueryResult }
  | {
    readonly status: "local_error";
    readonly view_id: string;
    readonly code: string;
    readonly message: string;
  };

const IDLE_QUERY: QueryPresentation = { status: "idle", view_id: null };
const RUN_ORIGIN_NS = "0" as U64String;
const MAX_U64 = (2n ** 64n) - 1n;

function localError(error: unknown): { readonly code: string; readonly message: string } {
  if (error instanceof Error) {
    return {
      code: "code" in error && typeof error.code === "string" ? error.code : "local",
      message: error.message,
    };
  }
  return { code: "local", message: String(error) };
}

function defaultViewClientFactory(bootstrap: DiagnosticBootstrap): DiagnosticsViewQueryClient {
  return createViewQueryClient({ bootstrap });
}

function initialSection(): PrimarySection {
  if (typeof window === "undefined") {
    return "timeline";
  }
  const candidate = window.location.hash.replace(/^#\/?/, "").split("/", 1)[0];
  return PRIMARY_SECTIONS.find((section) => section === candidate) ?? "timeline";
}

function useLiveDiagnostics(
  provided: DiagnosticsLiveController | undefined,
): readonly [DiagnosticsLiveController, LiveDiagnosticsState] {
  const controllerRef = useRef<DiagnosticsLiveController | null>(null);
  if (controllerRef.current === null) {
    controllerRef.current = provided ?? createLiveDiagnosticsController();
  }
  const controller = controllerRef.current;
  const [state, setState] = useState<LiveDiagnosticsState>(controller.state);

  useEffect(() => {
    setState(controller.state);
    const unsubscribe = controller.subscribe(setState);
    void controller.start().catch(() => undefined);
    return () => {
      unsubscribe();
      controller.stop();
    };
  }, [controller]);

  return [controller, state];
}

function useCatalog(
  live: LiveDiagnosticsState,
  factory: DiagnosticsViewClientFactory,
): CatalogPresentation {
  const [presentation, setPresentation] = useState<CatalogPresentation>({ status: "idle" });
  const bootstrap = live.bootstrap;
  const interactive = bootstrap?.compatibility.mode === "interactive";
  const origin = bootstrap?.origin ?? null;
  const apiBaseUrl = bootstrap?.api_base_url ?? null;
  const runId = bootstrap?.identity.run_id ?? null;
  const diagnosticsRunId = live.diagnostics?.run_id ?? null;

  useEffect(() => {
    if (!interactive || bootstrap === null || diagnosticsRunId === null) {
      setPresentation({ status: "idle" });
      return;
    }
    const client = factory(bootstrap);
    let current = true;
    setPresentation({ status: "loading" });
    void client.loadCatalog().then((catalog) => {
      if (current) {
        setPresentation({ status: "ready", catalog, client });
      }
    }).catch((error: unknown) => {
      if (current) {
        setPresentation({ status: "local_error", ...localError(error) });
      }
    });
    return () => {
      current = false;
      client.dispose();
    };
  }, [apiBaseUrl, diagnosticsRunId, factory, interactive, origin, runId]);

  return presentation;
}

function boundedEvents(state: DiagnosticState): readonly DiagnosticEvent[] {
  const bySequence = new Map<string, DiagnosticEvent>();
  for (const event of state.windows.visible?.events ?? []) {
    bySequence.set(event.sequence, event);
  }
  for (const event of presentedLiveEdge(state).events) {
    bySequence.set(event.sequence, event);
  }
  return [...bySequence.values()].sort((left, right) => {
    const a = BigInt(left.sequence);
    const b = BigInt(right.sequence);
    return a < b ? -1 : a > b ? 1 : 0;
  });
}

function querySelectionScope(
  state: DiagnosticState,
  events: readonly DiagnosticEvent[],
): DiagnosticScope | null {
  const edge = presentedLiveEdge(state);
  const selection = state.presentation.selection;
  if (selection === null) {
    return null;
  }
  const selected = scopeFromReference(selection)
    ?? resolveSelection(selection, events)?.scope
    ?? (selection.kind === "message"
      ? edge.projection.messages.items.find((message) => message.message_id === selection.id)?.scope
      : selection.kind === "span"
        ? (() => {
          const span = edge.projection.spans.items.find((candidate) => candidate.span_id === selection.id);
          return span?.start?.scope ?? span?.finish?.scope;
        })()
        : undefined)
    ?? null;
  if (
    selected === null
    || [selected.scene_id, selected.actor_id, selected.cue_id, selected.act_id]
      .every((value) => value === null)
  ) {
    return null;
  }
  return {
    scene_id: selected.scene_id,
    actor_id: selected.actor_id,
    cue_id: selected.cue_id,
    effect_id: null,
    act_id: selected.act_id,
    tool_call_id: null,
    session_generation: selected.session_generation,
  };
}

function capturedElapsedEndNs(
  observedElapsedNs: U64String,
  capturedWatermark: U64String,
): U64String | null {
  if (capturedWatermark === "0") {
    return "0" as U64String;
  }
  const observed = BigInt(observedElapsedNs);
  return observed === MAX_U64 ? null : ((observed + 1n).toString() as U64String);
}

function queryContext(state: DiagnosticState, events: readonly DiagnosticEvent[]): ViewQueryContext {
  const edge = presentedLiveEdge(state);
  const capturedEnd = capturedElapsedEndNs(
    edge.observed_elapsed_ns,
    state.cursor.committed_watermark,
  ) ?? edge.observed_elapsed_ns;
  const viewport = state.presentation.viewport ?? {
    start_ns: RUN_ORIGIN_NS,
    end_ns: capturedEnd,
  };
  return {
    captured_watermark: state.cursor.committed_watermark,
    captured_elapsed_end_ns: capturedEnd,
    selection: state.presentation.selection,
    selected_scope: querySelectionScope(state, events),
    viewport,
  };
}

function contextIdentity(context: ViewQueryContext): string {
  return JSON.stringify([
    context.captured_watermark,
    context.captured_elapsed_end_ns,
    context.selection?.kind ?? null,
    context.selection?.id ?? null,
    context.selected_scope,
    context.viewport,
  ]);
}

function isIssue(event: DiagnosticEvent): boolean {
  switch (event.kind) {
    case "observation_gap":
      return true;
    case "span_finished":
    case "custom_span_finished":
      return event.outcome === "failed";
    case "custom_instant_occurred":
      return event.severity === "warning" || event.severity === "error";
    case "instant_occurred":
      return event.instant_kind === "diagnostic.component_failed"
        || event.instant_kind === "agent.session.broken"
        || event.instant_kind === "result.rejected"
        || event.instant_kind === "result.missing";
    default:
      return false;
  }
}

function matchesEventQuery(event: DiagnosticEvent, query: EventQueryState): boolean {
  if (query.actor_id !== null && event.scope.actor_id !== query.actor_id) {
    return false;
  }
  if (query.scene_id !== null && event.scope.scene_id !== query.scene_id) {
    return false;
  }
  if (query.event_kinds.length > 0 && !query.event_kinds.includes(event.kind)) {
    return false;
  }
  if (query.error_filter !== "all" && !isIssue(event)) {
    return false;
  }
  const text = query.text.trim().toLocaleLowerCase();
  return text.length === 0 || [event.kind, event.scope.actor_id, summarizeEvent(event)]
    .filter((value): value is string => value !== null)
    .some((value) => value.toLocaleLowerCase().includes(text));
}

function selectedEvent(
  state: DiagnosticState,
  events: readonly DiagnosticEvent[],
): DiagnosticEvent | null {
  const selection = state.presentation.selection;
  if (selection === null) {
    return null;
  }
  return events.find((event) => (
    eventSelectionHighlight(event, selection, events) === "selected"
  )) ?? null;
}

interface CanonicalTimelineProps {
  readonly state: DiagnosticState;
  readonly productionName: string;
  readonly dispatch: (action: DiagnosticStateAction) => void;
}

function CanonicalTimeline({ state, productionName, dispatch }: CanonicalTimelineProps): JSX.Element {
  const model = selectTimelineModel(state, productionName);
  const height = Math.min(512, Math.max(224, model.nodes.length * 32));
  const width = 960;
  const layout = layoutTimeline(model, { scroll_top: 0, height });
  const selectedViewport = state.presentation.viewport;
  const earliest = model.primitives.reduce(
    (value, primitive) => BigInt(primitive.start_ns) < BigInt(value) ? primitive.start_ns : value,
    model.live_now_ns,
  );
  const viewport = createTimelineViewport(
    selectedViewport?.start_ns ?? earliest,
    selectedViewport?.end_ns ?? model.live_now_ns,
    width,
    state.presentation.follow_live,
  );
  const hitIndex = buildTimelineHitIndex(layout);
  const select = (selection: SelectionReference): void => {
    dispatch({ type: "select", selection });
  };
  const setViewport = (
    next: ReturnType<typeof createTimelineViewport>,
    zoom: DiagnosticState["presentation"]["zoom"] = state.presentation.zoom,
  ): void => {
    dispatch({ type: "viewport_set", viewport: next });
    dispatch({ type: "follow_live_set", follow_live: next.follow_live });
    dispatch({ type: "zoom_set", zoom });
  };
  const pan = (fraction: number): void => {
    setViewport(
      panTimelineViewport(viewport, viewport.width_px * fraction, model.live_now_ns),
      null,
    );
  };
  const zoom = (factor: number): void => {
    const anchorPixel = viewport.width_px / 2;
    const anchorNs = pixelToElapsed(anchorPixel, viewport);
    setViewport(
      zoomTimelineViewport(viewport, factor, anchorPixel, model.live_now_ns),
      { anchor_ns: anchorNs, scale: factor },
    );
  };
  return (
    <section class="diagnostic-canonical-timeline" aria-label="Canonical production timeline">
      <header><h2>Production timeline</h2></header>
      <div
        role="toolbar"
        aria-label="Timeline navigation"
        style={{
          display: "flex",
          minWidth: 0,
          alignItems: "center",
          flexWrap: "wrap",
          gap: "0.375rem",
          marginBlockEnd: "0.5rem",
        }}
      >
        <button
          class="primary-toolbar__icon-button"
          type="button"
          aria-label="Pan timeline earlier"
          title="Pan timeline earlier"
          onClick={() => pan(-0.25)}
        >
          <ArrowLeft aria-hidden="true" />
        </button>
        <button
          class="primary-toolbar__icon-button"
          type="button"
          aria-label="Pan timeline later"
          title="Pan timeline later"
          onClick={() => pan(0.25)}
        >
          <ArrowRight aria-hidden="true" />
        </button>
        <button
          class="primary-toolbar__icon-button"
          type="button"
          aria-label="Zoom timeline in"
          title="Zoom timeline in"
          onClick={() => zoom(0.5)}
        >
          <ZoomIn aria-hidden="true" />
        </button>
        <button
          class="primary-toolbar__icon-button"
          type="button"
          aria-label="Zoom timeline out"
          title="Zoom timeline out"
          onClick={() => zoom(2)}
        >
          <ZoomOut aria-hidden="true" />
        </button>
        <button
          class="primary-toolbar__icon-button"
          type="button"
          aria-label="Follow live timeline"
          title="Follow live timeline"
          aria-pressed={viewport.follow_live}
          onClick={() => setViewport(
            followTimelineViewport(viewport, model.live_now_ns),
            null,
          )}
        >
          <Radio aria-hidden="true" />
        </button>
        <output
          aria-label="Timeline viewport"
          data-start-ns={viewport.start_ns}
          data-end-ns={viewport.end_ns}
          style={{ minWidth: 0, overflowWrap: "anywhere", fontVariantNumeric: "tabular-nums" }}
        >
          {viewport.start_ns} - {viewport.end_ns} ns
        </output>
      </div>
      <div
        role="region"
        aria-label="Timeline canvas viewport"
        tabIndex={0}
        style={{ minWidth: 0, overflowX: "auto" }}
      >
        <TimelineCanvas
          layout={layout}
          viewport={viewport}
          hit_index={hitIndex}
          selection={state.presentation.selection}
          width={width}
          height={height}
          onSelect={select}
        />
      </div>
      <TimelineTreegrid
        layout={layout}
        selection={state.presentation.selection}
        onSelect={select}
        onToggle={(nodeId) => dispatch({ type: "toggle_expanded", id: nodeId })}
      />
    </section>
  );
}

interface EventsPanelProps {
  readonly state: DiagnosticState;
  readonly events: readonly DiagnosticEvent[];
  readonly errorFilter: EventQueryState["error_filter"];
  readonly onErrorFilterChange: (filter: EventQueryState["error_filter"]) => void;
  readonly dispatch: (action: DiagnosticStateAction) => void;
}

function EventsPanel({
  state,
  events,
  errorFilter,
  onErrorFilterChange,
  dispatch,
}: EventsPanelProps): JSX.Element {
  const query: EventQueryState = { ...state.presentation.filters, error_filter: errorFilter };
  const visible = events.filter((event) => matchesEventQuery(event, query));
  const actorIds = [...new Set(events.flatMap((event) => (
    event.scope.actor_id === null ? [] : [event.scope.actor_id]
  )))];
  const select = (selection: SelectionReference): void => {
    dispatch({ type: "select", selection });
  };
  return (
    <section class="diagnostic-events-workspace" aria-label="Event explorer">
      <FilterBar
        query={query}
        actors={actorIds.map((id) => ({ id, label: id }))}
        onQueryChange={(next) => {
          onErrorFilterChange(next.error_filter);
          dispatch({
            type: "filters_set",
            filters: {
              actor_id: next.actor_id,
              event_kinds: next.event_kinds,
              scene_id: next.scene_id,
              text: next.text,
            },
          });
        }}
      />
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(min(100%, 320px), 1fr))",
        }}
      >
        <EventTable
          page={{
            events: visible,
            captured_through: state.cursor.committed_watermark,
            previous: null,
            next: null,
          }}
          selection={state.presentation.selection}
          selectionEvents={events}
          onSelectionChange={select}
          onPageRequest={() => undefined}
        />
        <EventInspector event={selectedEvent(state, events)} onSelectionChange={select} />
      </div>
    </section>
  );
}

interface TimeSeriesRendererPanelProps {
  readonly record: Extract<ViewRecord, { readonly renderer: "time_series" }>;
  readonly result: ViewQueryResult;
  readonly selection: TimeSeriesSelection | null;
  readonly onSelectionChange: (selection: TimeSeriesSelection | null) => void;
}

function TimeSeriesRendererPanel({
  record,
  result,
  selection,
  onSelectionChange,
}: TimeSeriesRendererPanelProps): JSX.Element {
  const shell = useRef<HTMLDivElement | null>(null);
  const model = result.time_series;
  useLayoutEffect(() => {
    if (model === null) {
      return;
    }
    const host = shell.current?.querySelector<HTMLElement>(".timeseries-shell__plot-mount") ?? null;
    if (host === null) {
      return;
    }
    const renderer = new TimeSeriesRenderer(host, {
      model,
      title: record.title,
      selection,
      onSelectionChange,
    });
    return () => renderer.destroy();
  }, [model, onSelectionChange, record.title, selection]);
  if (model === null) {
    throw new Error("time-series query did not produce a columnar model");
  }
  if (result.response.renderer !== "time_series") {
    throw new Error("time-series panel received another renderer response");
  }
  return (
    <div ref={shell} class="diagnostic-timeseries-panel">
      <TimeSeriesShell record={record} state={{ status: "ready", response: result.response }} />
    </div>
  );
}

interface CompatibleViewPanelProps {
  readonly record: ViewRecord;
  readonly query: QueryPresentation;
  readonly selection: TimeSeriesSelection | null;
  readonly onTimeSelectionChange: (selection: TimeSeriesSelection | null) => void;
}

function CompatibleViewPanel({
  record,
  query,
  selection,
  onTimeSelectionChange,
}: CompatibleViewPanelProps): JSX.Element {
  const current = query.view_id === record.id ? query : { status: "loading" as const };
  const error = current.status === "local_error"
    ? { code: current.code, message: current.message }
    : null;
  switch (record.renderer) {
    case "timeline":
      return <TimelineView record={record} state={
        current.status === "ready" && current.result.response.renderer === "timeline"
          ? { status: "ready", response: current.result.response }
          : error === null ? { status: "loading" } : { status: "local_error", error }
      } />;
    case "metric":
      return <MetricView record={record} state={
        current.status === "ready" && current.result.response.renderer === "metric"
          ? { status: "ready", response: current.result.response }
          : error === null ? { status: "loading" } : { status: "local_error", error }
      } />;
    case "table":
      return <TableView record={record} state={
        current.status === "ready" && current.result.response.renderer === "table"
          ? { status: "ready", response: current.result.response }
          : error === null ? { status: "loading" } : { status: "local_error", error }
      } />;
    case "time_series":
      if (current.status === "ready" && current.result.response.renderer === "time_series") {
        return (
          <TimeSeriesRendererPanel
            record={record}
            result={current.result}
            selection={selection}
            onSelectionChange={onTimeSelectionChange}
          />
        );
      }
      return <TimeSeriesShell record={record} state={
        error === null ? { status: "loading" } : { status: "local_error", error }
      } />;
    default: {
      const exhaustive: never = record;
      return exhaustive;
    }
  }
}

interface ViewsPanelProps {
  readonly catalog: CatalogPresentation;
  readonly context: ViewQueryContext;
  readonly contextKey: string;
  readonly queryReady: boolean;
  readonly selection: TimeSeriesSelection | null;
  readonly onTimeSelectionChange: (selection: TimeSeriesSelection | null) => void;
}

function ViewsPanel({
  catalog,
  context,
  contextKey,
  queryReady,
  selection,
  onTimeSelectionChange,
}: ViewsPanelProps): JSX.Element {
  const [activeViewId, setActiveViewId] = useState<string | null>(null);
  const [query, setQuery] = useState<QueryPresentation>(IDLE_QUERY);
  const [retryGeneration, setRetryGeneration] = useState(0);
  const entries = catalog.status === "ready" ? catalog.catalog.views : [];

  useEffect(() => {
    setActiveViewId((current) => (
      current !== null && entries.some((entry) => viewCatalogEntryId(entry) === current)
        ? current
        : entries[0] === undefined ? null : viewCatalogEntryId(entries[0])
    ));
  }, [entries]);

  const activeEntry = entries.find((entry) => viewCatalogEntryId(entry) === activeViewId) ?? null;
  useEffect(() => {
    if (
      catalog.status !== "ready"
      || activeEntry === null
      || !isCompatibleViewCatalogEntry(activeEntry)
    ) {
      setQuery(IDLE_QUERY);
      return;
    }
    // A control frame can announce a newer committed head before its events
    // reach the live projection. Keep the last coherent result until the
    // delivered projection catches up (and while presentation is paused).
    if (!queryReady) {
      return;
    }
    const viewId = activeEntry.id;
    let current = true;
    setQuery({ status: "loading", view_id: viewId });
    void catalog.client.query(viewId, context).then((result) => {
      if (current) {
        setQuery({ status: "ready", view_id: viewId, result });
      }
    }).catch((raw: unknown) => {
      if (current) {
        setQuery({ status: "local_error", view_id: viewId, ...localError(raw) });
      }
    });
    return () => {
      current = false;
    };
  }, [activeEntry, catalog, contextKey, queryReady, retryGeneration]);

  if (catalog.status === "idle" || catalog.status === "loading") {
    return <section aria-label="Compiled views"><p role="status">Loading compiled views.</p></section>;
  }
  if (catalog.status === "local_error") {
    return (
      <section aria-label="Compiled views" role="alert">
        <h2>Views unavailable</h2><code>{catalog.code}</code><p>{catalog.message}</p>
      </section>
    );
  }
  if (entries.length === 0) {
    return (
      <section aria-label="Compiled views" data-state="empty">
        <h2>Views</h2><p>No compiled views are available for this production.</p>
      </section>
    );
  }
  if (activeEntry === null) {
    return <section aria-label="Compiled views"><p role="status">Selecting a view.</p></section>;
  }

  const viewId = viewCatalogEntryId(activeEntry);
  const queryIdentity = query.view_id === viewId && query.status === "ready"
    ? query.result.generation.key
    : `${query.status}:${contextKey}:${retryGeneration}`;
  const selectionIdentity = context.selection === null
    ? null
    : `${context.selection.kind}:${context.selection.id}`;
  const runtime = query.view_id !== viewId || query.status === "loading"
    ? { status: "loading" as const }
    : query.status === "local_error"
      ? { status: "failed" as const, code: query.code, message: query.message }
      : { status: "ready" as const };
  const compatibility = isCompatibleViewCatalogEntry(activeEntry)
    ? { status: "compatible" as const }
    : { status: "incompatible" as const, ...activeEntry.incompatible };

  return (
    <section class="diagnostic-views" aria-label="Compiled views">
      <div role="tablist" aria-label="Compiled view catalog">
        {entries.map((entry) => {
          const id = viewCatalogEntryId(entry);
          return (
            <button
              key={id}
              id={`compiled-view-tab-${id}`}
              type="button"
              role="tab"
              aria-selected={id === viewId}
              aria-controls="diagnostic-view-panel"
              tabIndex={id === viewId ? 0 : -1}
              data-renderer={entry.renderer}
              onClick={() => setActiveViewId(id)}
            >
              {isCompatibleViewCatalogEntry(entry) ? entry.title : id}
            </button>
          );
        })}
      </div>
      <div
        id="diagnostic-view-panel"
        role="tabpanel"
        aria-labelledby={`compiled-view-tab-${viewId}`}
      >
        <ViewPanelErrorBoundary
          identity={{ panel_id: viewId, query_identity: queryIdentity, selection_identity: selectionIdentity }}
          runtime={isCompatibleViewCatalogEntry(activeEntry) ? runtime : { status: "ready" }}
          compatibility={compatibility}
          onError={(error) => {
            if (catalog.status === "ready" && isCompatibleViewCatalogEntry(activeEntry)) {
              const reported = catalog.client.reportRendererFailure(activeEntry.id, error);
              setQuery({
                status: "local_error",
                view_id: activeEntry.id,
                code: reported.code,
                message: reported.message,
              });
            }
          }}
          onRetry={() => {
            if (catalog.status === "ready" && isCompatibleViewCatalogEntry(activeEntry)) {
              catalog.client.invalidateView(activeEntry.id);
              setRetryGeneration((value) => value + 1);
            }
          }}
        >
          {isCompatibleViewCatalogEntry(activeEntry) ? (
            <CompatibleViewPanel
              record={activeEntry}
              query={query}
              selection={selection}
              onTimeSelectionChange={onTimeSelectionChange}
            />
          ) : (
            <section aria-label={`Unavailable view ${viewId}`}><h3>{viewId}</h3></section>
          )}
        </ViewPanelErrorBoundary>
      </div>
    </section>
  );
}

function StaticSurface({ live }: { readonly live: LiveDiagnosticsState }): JSX.Element {
  const compatibility = live.bootstrap?.compatibility;
  return (
    <main class="diagnostics-root diagnostics-static" data-phase={live.phase}>
      <h1>Troupe Diagnostics</h1>
      {live.phase === "compatibility" && compatibility !== undefined ? (
        <section role="status" aria-label="Compatibility status">
          <h2>Interactive diagnostics unavailable</h2>
          {compatibility.missingBrowserCapabilities.length > 0 ? (
            <p>Required browser capabilities are unavailable.</p>
          ) : <p>The server and interface schema versions are incompatible.</p>}
        </section>
      ) : live.phase === "failed" ? (
        <section role="alert"><h2>Diagnostics unavailable</h2><code>{live.error?.code}</code><p>{live.error?.message}</p></section>
      ) : (
        <p role="status">Connecting to the diagnostic server.</p>
      )}
    </main>
  );
}

export function App({
  liveController,
  viewClientFactory = defaultViewClientFactory,
  productionName,
}: AppProps = {}): JSX.Element {
  const [controller, live] = useLiveDiagnostics(liveController);
  const [section, setSection] = useState<PrimarySection>(initialSection);
  const [errorFilter, setErrorFilter] = useState<EventQueryState["error_filter"]>(
    EMPTY_EVENT_QUERY.error_filter,
  );
  const catalog = useCatalog(live, viewClientFactory);
  const state = live.diagnostics;

  useEffect(() => {
    const onHashChange = (): void => setSection(initialSection());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  const changeSection = useCallback((next: PrimarySection): void => {
    setSection(next);
    window.history.replaceState(null, "", `#/${next}`);
  }, []);
  const dispatch = useCallback((action: DiagnosticStateAction): void => {
    controller.dispatch(action);
  }, [controller]);
  const changeTimeSelection = useCallback((selection: TimeSeriesSelection | null): void => {
    dispatch({ type: "viewport_set", viewport: selection });
  }, [dispatch]);

  if (state === null || live.security_scope !== "trusted_network" || live.outcome === null) {
    return <StaticSurface live={live} />;
  }

  const events = boundedEvents(state);
  const context = queryContext(state, events);
  const contextKey = contextIdentity(context);
  const edge = presentedLiveEdge(state);
  const queryReady = !state.pause.paused
    && state.delivery_issue === null
    && state.cursor.delivered_through === state.cursor.committed_watermark
    && capturedElapsedEndNs(edge.observed_elapsed_ns, state.cursor.committed_watermark) !== null;
  const name = productionName ?? (live.connection === "archive" ? "Archived production" : "Production");
  let panel: JSX.Element;
  switch (section) {
    case "timeline":
      panel = <CanonicalTimeline state={state} productionName={name} dispatch={dispatch} />;
      break;
    case "agent":
      panel = <TranscriptPanel state={state} onSelectionChange={(selection) => dispatch({ type: "select", selection })} />;
      break;
    case "events":
      panel = (
        <EventsPanel
          state={state}
          events={events}
          errorFilter={errorFilter}
          onErrorFilterChange={setErrorFilter}
          dispatch={dispatch}
        />
      );
      break;
    case "usage":
      panel = <UsagePanel state={state} />;
      break;
    case "views":
      panel = (
        <ViewsPanel
          catalog={catalog}
          context={context}
          contextKey={contextKey}
          queryReady={queryReady}
          selection={state.presentation.viewport}
          onTimeSelectionChange={changeTimeSelection}
        />
      );
      break;
    default: {
      const exhaustive: never = section;
      panel = exhaustive;
    }
  }

  return (
    <div class="diagnostics-root" data-phase={live.phase} data-source={live.status?.source ?? "active"}>
      <AppShell
        state={state}
        productionName={name}
        connection={live.connection}
        outcome={live.outcome}
        securityScope="trusted_network"
        activeSection={section}
        dispatch={dispatch}
        onSectionChange={changeSection}
      >
        {panel}
      </AppShell>
    </div>
  );
}
