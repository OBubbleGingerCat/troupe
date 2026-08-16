import type { ComponentChildren } from "preact";
import { Archive, RefreshCw, ShieldCheck, Wifi, WifiOff } from "lucide-preact";

import type { DiagnosticState, SelectionReference } from "../state/model.ts";
import type { DiagnosticStateAction } from "../state/reducer.ts";
import { ExecutionTree } from "./ExecutionTree.tsx";
import { PrimaryToolbar, type PrimarySection } from "./PrimaryToolbar.tsx";
import { selectExecutionTree, selectShellReadout } from "./selectors.ts";
import "./shell.css";


export type ShellConnection = "connected" | "reconnecting" | "offline" | "archive";
export type RunOutcome = "running" | "completed" | "failed" | "cancelled" | "incomplete";

export interface AppShellProps {
  readonly state: DiagnosticState;
  readonly productionName: string;
  readonly connection: ShellConnection;
  readonly outcome: RunOutcome;
  readonly securityScope: "trusted_network";
  readonly activeSection: PrimarySection;
  readonly dispatch: (action: DiagnosticStateAction) => void;
  readonly onSectionChange: (section: PrimarySection) => void;
  readonly children?: ComponentChildren;
}

const CONNECTION_LABELS: Readonly<Record<ShellConnection, string>> = {
  connected: "Connected",
  reconnecting: "Reconnecting",
  offline: "Offline",
  archive: "Archive",
};

function ConnectionIcon({ connection }: { readonly connection: ShellConnection }) {
  switch (connection) {
    case "connected":
      return <Wifi aria-hidden="true" />;
    case "reconnecting":
      return <RefreshCw aria-hidden="true" />;
    case "offline":
      return <WifiOff aria-hidden="true" />;
    case "archive":
      return <Archive aria-hidden="true" />;
  }
}

export function AppShell({
  state,
  productionName,
  connection,
  outcome,
  securityScope,
  activeSection,
  dispatch,
  onSectionChange,
  children,
}: AppShellProps) {
  const tree = selectExecutionTree(state, productionName);
  const readout = selectShellReadout(state);
  const select = (selection: SelectionReference) => {
    dispatch({ type: "select", selection });
  };

  return (
    <div class="troupe-shell">
      <header class="troupe-shell__header">
        <div class="troupe-shell__identity">
          <h1>Troupe Diagnostics</h1>
          <span>{productionName}</span>
        </div>

        <dl class="troupe-shell__run-status" aria-label="Run status">
          <div data-connection={connection} aria-live="polite">
            <dt>Connection</dt>
            <dd><ConnectionIcon connection={connection} />{CONNECTION_LABELS[connection]}</dd>
          </div>
          <div>
            <dt>Run</dt>
            <dd title={state.run_id}>{state.run_id}</dd>
          </div>
          <div data-outcome={outcome}>
            <dt>Outcome</dt>
            <dd>{outcome}</dd>
          </div>
          <div>
            <dt>Scope</dt>
            <dd><ShieldCheck aria-hidden="true" />{securityScope}</dd>
          </div>
          <div>
            <dt>Watermark</dt>
            <dd>{readout.deliveredThrough} / {readout.committedWatermark}</dd>
          </div>
        </dl>
      </header>

      <PrimaryToolbar
        activeSection={activeSection}
        paused={readout.paused}
        unseenCount={readout.unseenCount}
        onSectionChange={onSectionChange}
        onPauseToggle={() => dispatch({ type: readout.paused ? "resume" : "pause" })}
      />

      <div class="troupe-shell__body">
        <aside class="troupe-shell__tree-panel" aria-label="Execution tree">
          <div class="troupe-shell__panel-heading">
            <h2>Execution</h2>
            <span>{tree.root.children.length} scenes</span>
          </div>
          <ExecutionTree
            model={tree}
            onSelect={select}
            onToggle={(key) => dispatch({ type: "toggle_expanded", id: key })}
          />
        </aside>

        <main
          id="diagnostic-primary-panel"
          class="troupe-shell__workspace"
          role="tabpanel"
          aria-labelledby={`diagnostic-tab-${activeSection}`}
          tabIndex={0}
        >
          {children}
        </main>
      </div>
    </div>
  );
}
