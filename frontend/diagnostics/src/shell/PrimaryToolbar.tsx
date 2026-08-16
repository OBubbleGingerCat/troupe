import {
  Activity,
  Gauge,
  LayoutGrid,
  List,
  MessageSquare,
  Pause,
  Play,
} from "lucide-preact";


export const PRIMARY_SECTIONS = ["timeline", "agent", "events", "usage", "views"] as const;
export type PrimarySection = typeof PRIMARY_SECTIONS[number];

export interface PrimaryToolbarProps {
  readonly activeSection: PrimarySection;
  readonly paused: boolean;
  readonly unseenCount: string;
  readonly onSectionChange: (section: PrimarySection) => void;
  readonly onPauseToggle: () => void;
}

const SECTION_LABELS: Readonly<Record<PrimarySection, string>> = {
  timeline: "Timeline",
  agent: "Agent",
  events: "Events",
  usage: "Usage",
  views: "Views",
};

function adjacentSection(section: PrimarySection, offset: number): PrimarySection {
  const index = PRIMARY_SECTIONS.indexOf(section);
  return PRIMARY_SECTIONS[(index + offset + PRIMARY_SECTIONS.length) % PRIMARY_SECTIONS.length]!;
}

function SectionIcon({ section }: { readonly section: PrimarySection }) {
  switch (section) {
    case "timeline":
      return <Activity aria-hidden="true" />;
    case "agent":
      return <MessageSquare aria-hidden="true" />;
    case "events":
      return <List aria-hidden="true" />;
    case "usage":
      return <Gauge aria-hidden="true" />;
    case "views":
      return <LayoutGrid aria-hidden="true" />;
  }
}

export function PrimaryToolbar({
  activeSection,
  paused,
  unseenCount,
  onSectionChange,
  onPauseToggle,
}: PrimaryToolbarProps) {
  const pauseLabel = paused ? "Resume live presentation" : "Pause live presentation";
  return (
    <div class="primary-toolbar">
      <div
        class="primary-toolbar__tabs"
        role="tablist"
        aria-label="Primary diagnostics views"
        aria-orientation="horizontal"
      >
        {PRIMARY_SECTIONS.map((section) => (
          <button
            key={section}
            id={`diagnostic-tab-${section}`}
            class="primary-toolbar__tab"
            type="button"
            role="tab"
            aria-selected={activeSection === section}
            aria-controls="diagnostic-primary-panel"
            tabIndex={activeSection === section ? 0 : -1}
            title={SECTION_LABELS[section]}
            onClick={() => onSectionChange(section)}
            onKeyDown={(event) => {
              let next: PrimarySection | null = null;
              if (event.key === "ArrowRight") {
                next = adjacentSection(section, 1);
              } else if (event.key === "ArrowLeft") {
                next = adjacentSection(section, -1);
              } else if (event.key === "Home") {
                next = PRIMARY_SECTIONS[0]!;
              } else if (event.key === "End") {
                next = PRIMARY_SECTIONS[PRIMARY_SECTIONS.length - 1]!;
              }
              if (next !== null) {
                event.preventDefault();
                onSectionChange(next);
                event.currentTarget.parentElement
                  ?.querySelector<HTMLButtonElement>(`#diagnostic-tab-${next}`)
                  ?.focus();
              }
            }}
          >
            <SectionIcon section={section} />
            <span>{SECTION_LABELS[section]}</span>
          </button>
        ))}
      </div>

      <div class="primary-toolbar__controls" aria-label="Presentation controls">
        {paused && unseenCount !== "0" ? (
          <output class="primary-toolbar__unseen" aria-label="Unseen sequences">
            {unseenCount} unseen
          </output>
        ) : null}
        <button
          class="primary-toolbar__icon-button"
          type="button"
          aria-label={pauseLabel}
          title={pauseLabel}
          aria-pressed={paused}
          onClick={onPauseToggle}
        >
          {paused ? <Play aria-hidden="true" /> : <Pause aria-hidden="true" />}
        </button>
      </div>
    </div>
  );
}
