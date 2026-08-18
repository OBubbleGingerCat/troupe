import {
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import { afterEach, describe, expect, it, vi } from "vitest";

import { ActorTimeline } from "../../src/timeline/actor_timeline.tsx";
import type { TimelineData } from "../../src/timeline/actor_timeline_model.ts";

const DATA: TimelineData = {
  scenes: [{
    id: "scene-1",
    label: "Scene 1",
    start: 0,
    end: 8,
    outcome: "completed",
    tone: "green",
  }],
  actors: [{
    id: "actor-1",
    name: "Worker",
    role: "Temporary actor",
    start: 1,
    end: 1.5,
    outcome: "completed",
    liveSlot: 0,
  }],
  cues: [],
  acts: [],
  customSpans: [],
  customEvents: [],
  totalTime: 10,
  liveNow: 10,
  watermark: "4",
  productionName: "Fixture production",
  connectionLabel: "Archive",
  outcomeLabel: "completed",
  liveSlotCount: 1,
};

const QUEUE_DATA: TimelineData = {
  ...DATA,
  scenes: [{ ...DATA.scenes[0]!, end: 12 }],
  actors: [{
    id: "actor-1",
    name: "Worker",
    role: "Persistent actor",
    start: 0,
    end: 12,
    outcome: "completed",
    liveSlot: 0,
  }],
  cues: [
    {
      id: "cue-1",
      label: "Cue cue-1",
      sceneId: "scene-1",
      actorId: "actor-1",
      admitted: 1,
      execution: 2,
      end: 8,
      outcome: "completed",
      events: [],
    },
    {
      id: "cue-2",
      label: "Cue cue-2",
      sceneId: "scene-1",
      actorId: "actor-1",
      admitted: 2,
      execution: 9,
      end: 10,
      outcome: "completed",
      events: [],
    },
  ],
  acts: [{
    id: "act-1",
    label: "Act act-1",
    cueId: "cue-1",
    start: 2.1,
    end: 8,
    outcome: "completed",
  }],
  totalTime: 12,
  liveNow: 12,
};

afterEach(cleanup);

describe("Actor timeline lifecycle affordances", () => {
  it("labels even a short lifetime and exposes rail/start/end details on hover", () => {
    const view = render(
      <ActorTimeline
        data={DATA}
        livePaused={false}
        unseenCount={0n}
        onPauseToggle={() => undefined}
      />,
    );
    expect(screen.getByText("Worker Actor lifetime")).toBeInTheDocument();

    const lifetime = view.container.querySelector<SVGGElement>(".actor-lifetime-track");
    expect(lifetime).not.toBeNull();
    fireEvent.mouseEnter(lifetime!);
    expect(screen.getByRole("tooltip")).toHaveTextContent("Actor lifetime");
    expect(screen.getByRole("tooltip")).toHaveTextContent("Worker Actor lifetime");
    expect(screen.getByRole("tooltip")).toHaveTextContent("Temporary actor");

    const created = view.container.querySelector<SVGGElement>(
      ".actor-lifecycle-marker[data-event='created']",
    );
    expect(created).not.toBeNull();
    fireEvent.mouseEnter(created!);
    expect(screen.getByRole("tooltip")).toHaveTextContent("Actor created");
    expect(screen.getByRole("tooltip")).toHaveTextContent("Observed");

    const destroyed = view.container.querySelector<SVGGElement>(
      ".actor-lifecycle-marker[data-event='destroyed']",
    );
    expect(destroyed).not.toBeNull();
    fireEvent.mouseEnter(destroyed!);
    expect(screen.getByRole("tooltip")).toHaveTextContent("Actor destroyed");
    expect(screen.getByRole("tooltip")).toHaveTextContent("Completed");
  });

  it("switches to a frozen History dataset containing a completed temporary Actor", () => {
    const onModeChange = vi.fn();
    render(
      <ActorTimeline
        data={{ ...DATA, actors: [] }}
        historyData={DATA}
        historyStatus="ready"
        livePaused={false}
        unseenCount={0n}
        onPauseToggle={() => undefined}
        onModeChange={onModeChange}
      />,
    );

    expect(screen.queryByText("Worker Actor lifetime")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "History" }));
    expect(onModeChange).toHaveBeenCalledWith("history");
    expect(screen.getByText("Worker Actor lifetime")).toBeInTheDocument();
  });

  it("identifies a queued Cue and the Act blocking its mailbox wait", () => {
    const view = render(
      <ActorTimeline
        data={QUEUE_DATA}
        livePaused={false}
        unseenCount={0n}
        onPauseToggle={() => undefined}
      />,
    );

    expect(screen.getByText("Cue wait · cue-2")).toBeInTheDocument();
    const wait = view.container.querySelector<SVGGElement>(
      ".cue-wait-track[data-cue-id='cue-2']",
    );
    expect(wait).not.toBeNull();
    expect(wait).toHaveAttribute("data-blocked-by", "act-1");

    const firstCue = view.container.querySelector<SVGGElement>(
      ".cue-track[data-cue-id='cue-1']",
    );
    const secondCue = view.container.querySelector<SVGGElement>(
      ".cue-track[data-cue-id='cue-2']",
    );
    expect(firstCue).toHaveAttribute("data-cue-lane", "0");
    expect(secondCue).toHaveAttribute("data-cue-lane", "1");
    const firstExecutionY = Number(
      firstCue?.querySelector(".cue-execution-bar")?.getAttribute("y"),
    );
    const secondWaitY = Number(
      secondCue?.querySelector(".cue-wait-bar")?.getAttribute("y"),
    );
    expect(secondWaitY - firstExecutionY).toBe(24);
    expect(view.container.querySelector(".actor-visual")).toHaveAttribute("data-cue-lanes", "2");
    expect(view.container.querySelector(".actor-label")).toHaveStyle({ height: "140px" });

    fireEvent.mouseEnter(wait!);
    expect(screen.getByRole("tooltip")).toHaveTextContent("Cue wait");
    expect(screen.getByRole("tooltip")).toHaveTextContent("Cue cue-2");
    expect(screen.getByRole("tooltip")).toHaveTextContent("Queued behind act-1");
  });
});
