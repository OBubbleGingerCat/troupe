import type {
  DiagnosticState,
  ServerRangeResumeRequest,
} from "../state/model.ts";
import { reduceDiagnosticState } from "../state/reducer.ts";


export interface ResumePresentationResult {
  readonly state: DiagnosticState;
  readonly query_intent: ServerRangeResumeRequest | null;
}

export function pauseLivePresentation(state: DiagnosticState): DiagnosticState {
  return reduceDiagnosticState(state, { type: "pause" });
}

export function resumeLivePresentation(state: DiagnosticState): ResumePresentationResult {
  const next = reduceDiagnosticState(state, { type: "resume" });
  return {
    state: next,
    query_intent: next.pause.resume_request,
  };
}

export function consumeResumeQueryIntent(state: DiagnosticState): DiagnosticState {
  return reduceDiagnosticState(state, { type: "resume_request_consumed" });
}
