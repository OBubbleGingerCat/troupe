import type { JSX } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";

import {
  type LiveDiagnosticsController,
  type LiveDiagnosticsState,
  createLiveDiagnosticsController,
} from "./live/reconnect.ts";
import type { DiagnosticFetch } from "./live/bootstrap.ts";
import type { DiagnosticState } from "./state/model.ts";
import type { DiagnosticStateAction } from "./state/reducer.ts";
import { ActorTimeline } from "./timeline/actor_timeline.tsx";
import type { TimelineData, TimelineMode } from "./timeline/actor_timeline_model.ts";
import { fetchTimelineHistoryCapture } from "./timeline/history_capture.ts";
import {
  selectCapturedTimelineData,
  selectProductionTimelineData,
} from "./timeline/production_timeline.ts";
import "./styles/base.css";


export type DiagnosticsLiveController = Pick<
  LiveDiagnosticsController,
  "state" | "subscribe" | "start" | "stop" | "dispatch"
>;

export interface AppProps {
  readonly liveController?: DiagnosticsLiveController;
  readonly productionName?: string;
  readonly historyFetch?: DiagnosticFetch;
}

interface HistoryCaptureState {
  readonly status: "idle" | "loading" | "ready" | "error";
  readonly data: TimelineData | null;
  readonly error: string | null;
}

const EMPTY_HISTORY: HistoryCaptureState = {
  status: "idle",
  data: null,
  error: null,
};

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
        <section role="alert">
          <h2>Diagnostics unavailable</h2>
          <code>{live.error?.code}</code>
          <p>{live.error?.message}</p>
        </section>
      ) : (
        <p role="status">Connecting to the diagnostic server.</p>
      )}
    </main>
  );
}

function timelineName(live: LiveDiagnosticsState, productionName: string | undefined): string {
  if (productionName !== undefined && productionName.length > 0) {
    return productionName;
  }
  return live.connection === "archive" ? "Archived production" : "Production";
}

export function App({
  liveController,
  productionName,
  historyFetch,
}: AppProps = {}): JSX.Element {
  const [controller, live] = useLiveDiagnostics(liveController);
  const historyRequest = useRef(0);
  const [history, setHistory] = useState<HistoryCaptureState>(EMPTY_HISTORY);
  const state = live.diagnostics;
  const runId = live.bootstrap?.identity.run_id ?? null;

  useEffect(() => {
    historyRequest.current += 1;
    setHistory(EMPTY_HISTORY);
  }, [runId]);

  if (state === null || live.security_scope !== "trusted_network" || live.outcome === null) {
    return <StaticSurface live={live} />;
  }

  const name = timelineName(live, productionName);
  const data = selectProductionTimelineData(state, live, { productionName: name });
  const dispatch = (action: DiagnosticStateAction): void => controller.dispatch(action);
  const togglePause = (): void => {
    dispatch({ type: state.pause.paused ? "resume" : "pause" });
  };
  const changeTimelineMode = (mode: TimelineMode): void => {
    if (mode !== "history" || live.bootstrap === null) {
      return;
    }
    const request = historyRequest.current + 1;
    historyRequest.current = request;
    const through = state.cursor.committed_watermark;
    setHistory({ status: "loading", data: null, error: null });
    void fetchTimelineHistoryCapture(live.bootstrap, through, historyFetch)
      .then((capture) => {
        if (historyRequest.current !== request) {
          return;
        }
        setHistory({
          status: "ready",
          data: selectCapturedTimelineData(capture.response.events, capture.through, {
            productionName: name,
            connectionLabel: live.connection === "archive" ? "Archive" : live.connection,
            outcomeLabel: live.outcome ?? "running",
          }),
          error: null,
        });
      })
      .catch((error: unknown) => {
        if (historyRequest.current !== request) {
          return;
        }
        setHistory({
          status: "error",
          data: null,
          error: error instanceof Error ? error.message : String(error),
        });
      });
  };

  return (
    <div
      class="diagnostics-root diagnostics-timeline-only"
      data-phase={live.phase}
      data-source={live.status?.source ?? "active"}
    >
      <ActorTimeline
        data={data}
        historyData={history.data}
        historyStatus={history.status}
        historyError={history.error}
        livePaused={state.pause.paused}
        unseenCount={state.pause.unseen_count}
        onPauseToggle={togglePause}
        onModeChange={changeTimelineMode}
      />
    </div>
  );
}

export type { DiagnosticState };
