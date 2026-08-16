import { RotateCcw } from "lucide-preact";
import { Component } from "preact";
import type {
  ComponentChildren,
  JSX,
} from "preact";


export interface ViewPanelIdentity {
  readonly panel_id: string;
  readonly query_identity: string;
  readonly selection_identity: string | null;
}

export type ViewPanelRuntimeState =
  | { readonly status: "ready" }
  | { readonly status: "loading" }
  | { readonly status: "failed"; readonly code: string; readonly message: string };

export type ViewPanelCompatibilityState =
  | { readonly status: "compatible" }
  | {
    readonly status: "incompatible";
    readonly reason: "newer_view_schema" | "corrupt_record";
    readonly supported_view_schema_version: 1;
    readonly record_view_schema_version: number | null;
  };

export interface ViewPanelLocalError {
  readonly code: "renderer_exception";
  readonly message: string;
}

export interface ViewPanelErrorBoundaryProps {
  readonly identity: ViewPanelIdentity;
  readonly runtime: ViewPanelRuntimeState;
  readonly compatibility: ViewPanelCompatibilityState;
  readonly children: ComponentChildren;
  readonly onError?: ((error: ViewPanelLocalError, identity: ViewPanelIdentity) => void) | undefined;
  readonly onRetry?: ((identity: ViewPanelIdentity) => void) | undefined;
}

interface RendererBoundaryProps {
  readonly identity: ViewPanelIdentity;
  readonly children: ComponentChildren;
  readonly onError: ((error: ViewPanelLocalError, identity: ViewPanelIdentity) => void) | undefined;
  readonly onRetry: ((identity: ViewPanelIdentity) => void) | undefined;
}

interface RendererBoundaryState {
  readonly error: ViewPanelLocalError | null;
  readonly retry_generation: number;
}

function normalizeRendererError(error: unknown): ViewPanelLocalError {
  const message = error instanceof Error && error.message.length > 0
    ? error.message
    : "The renderer threw an unknown error.";
  return { code: "renderer_exception", message };
}

class RendererBoundary extends Component<RendererBoundaryProps, RendererBoundaryState> {
  constructor(props: RendererBoundaryProps) {
    super(props);
    this.state = { error: null, retry_generation: 0 };
  }

  static getDerivedStateFromError(error: unknown): Partial<RendererBoundaryState> {
    return { error: normalizeRendererError(error) };
  }

  override componentDidCatch(error: unknown): void {
    this.props.onError?.(normalizeRendererError(error), this.props.identity);
  }

  private readonly retry = (): void => {
    this.props.onRetry?.(this.props.identity);
    this.setState((state) => ({
      error: null,
      retry_generation: state.retry_generation + 1,
    }));
  };

  override render(): JSX.Element {
    const { error } = this.state;
    if (error !== null) {
      return (
        <section
          class="view-panel-boundary__local-error"
          role="alert"
          data-error-code={error.code}
        >
          <strong>View renderer failed</strong>
          <code>{error.code}</code>
          <p>{error.message}</p>
          <button
            type="button"
            aria-label="Retry view renderer"
            title="Retry view renderer"
            onClick={this.retry}
          >
            <RotateCcw aria-hidden="true" size={17} strokeWidth={1.75} />
            <span>Retry</span>
          </button>
        </section>
      );
    }
    return (
      <div
        key={this.state.retry_generation}
        class="view-panel-boundary__renderer"
        data-retry-generation={String(this.state.retry_generation)}
      >
        {this.props.children}
      </div>
    );
  }
}

function RuntimeStatus({ state }: { readonly state: ViewPanelRuntimeState }): JSX.Element {
  if (state.status === "ready") {
    return <span class="view-panel-boundary__runtime" data-runtime-state="ready" />;
  }
  if (state.status === "loading") {
    return <p class="view-panel-boundary__runtime" data-runtime-state="loading" role="status">Loading view data.</p>;
  }
  return (
    <div class="view-panel-boundary__runtime" data-runtime-state="failed" role="alert">
      <strong>View data failed</strong>
      <code>{state.code}</code>
      <p>{state.message}</p>
    </div>
  );
}

function CompatibilityStatus({
  state,
}: {
  readonly state: ViewPanelCompatibilityState;
}): JSX.Element {
  if (state.status === "compatible") {
    return <span class="view-panel-boundary__compatibility" data-compatibility-state="compatible" />;
  }
  return (
    <div
      class="view-panel-boundary__compatibility"
      data-compatibility-state="incompatible"
      data-compatibility-reason={state.reason}
      role="status"
    >
      {state.reason === "newer_view_schema" ? (
        <p>
          View schema {state.record_view_schema_version} is newer than supported schema
          {" "}{state.supported_view_schema_version}.
        </p>
      ) : (
        <p>
          The stored view record is corrupt. Record schema: {state.record_view_schema_version
            ?? <span>Unknown</span>}.
        </p>
      )}
    </div>
  );
}

function boundaryKey(identity: ViewPanelIdentity): string {
  return JSON.stringify([
    identity.panel_id,
    identity.query_identity,
    identity.selection_identity,
  ]);
}

export function ViewPanelErrorBoundary({
  identity,
  runtime,
  compatibility,
  children,
  onError,
  onRetry,
}: ViewPanelErrorBoundaryProps): JSX.Element {
  return (
    <section
      class="view-panel-boundary"
      data-panel-id={identity.panel_id}
      data-query-identity={identity.query_identity}
      data-selection-identity={identity.selection_identity ?? ""}
      style={{ letterSpacing: 0, minWidth: 0, overflowWrap: "anywhere", wordBreak: "break-word" }}
    >
      <RuntimeStatus state={runtime} />
      <CompatibilityStatus state={compatibility} />
      <RendererBoundary
        key={boundaryKey(identity)}
        identity={identity}
        onError={onError}
        onRetry={onRetry}
      >
        {children}
      </RendererBoundary>
    </section>
  );
}
