import { ChevronDown, ChevronRight } from "lucide-preact";
import type { JSX } from "preact";
import { useRef } from "preact/hooks";

import type { SelectionReference } from "../state/model.ts";
import { sameSelectionReference } from "../state/selection.ts";
import type {
  TimelineLayout,
  TimelineRow,
} from "./layout.ts";


export interface TimelineTreegridProps {
  readonly layout: TimelineLayout;
  readonly selection: SelectionReference | null;
  readonly onSelect: (selection: SelectionReference) => void;
  readonly onToggle: (nodeId: string) => void;
}

function rowLabel(row: TimelineRow): string {
  const status = row.node.status ?? "group";
  return `${row.node.label}, ${row.node.kind}, ${status}`;
}

function isCollapsible(row: TimelineRow): boolean {
  return row.has_children && (row.node.kind === "cue" || row.node.kind === "act");
}

export function TimelineTreegrid({
  layout,
  selection,
  onSelect,
  onToggle,
}: TimelineTreegridProps): JSX.Element {
  const rowElements = useRef(new Map<string, HTMLDivElement>());
  const visible = layout.visible_rows;

  const focusRow = (index: number): void => {
    const row = visible[index];
    if (row !== undefined) {
      rowElements.current.get(row.node.id)?.focus();
    }
  };

  const onRowKeyDown = (
    event: JSX.TargetedKeyboardEvent<HTMLDivElement>,
    row: TimelineRow,
    visibleIndex: number,
  ): void => {
    switch (event.key) {
      case "ArrowDown":
        event.preventDefault();
        focusRow(Math.min(visible.length - 1, visibleIndex + 1));
        break;
      case "ArrowUp":
        event.preventDefault();
        focusRow(Math.max(0, visibleIndex - 1));
        break;
      case "Home":
        event.preventDefault();
        focusRow(0);
        break;
      case "End":
        event.preventDefault();
        focusRow(visible.length - 1);
        break;
      case "ArrowRight":
        if (row.has_children && !row.node.expanded) {
          event.preventDefault();
          onToggle(row.node.id);
        }
        break;
      case "ArrowLeft":
        if (row.has_children && row.node.expanded) {
          event.preventDefault();
          onToggle(row.node.id);
        } else if (row.node.parent_id !== null) {
          event.preventDefault();
          const parentIndex = visible.findIndex((candidate) => candidate.node.id === row.node.parent_id);
          if (parentIndex >= 0) {
            focusRow(parentIndex);
          }
        }
        break;
      case "Enter":
      case " ":
        event.preventDefault();
        onSelect(row.node.selection);
        break;
    }
  };

  const selectedVisible = visible.findIndex((row) => (
    sameSelectionReference(selection, row.node.selection)
  ));
  return (
    <section class="timeline-treegrid-surface" aria-label="Timeline semantic surface">
      {layout.model.needs_server_refresh ? (
        <p role="status">Timeline data are partial.</p>
      ) : null}
      <div
        class="timeline-treegrid"
        role="treegrid"
        aria-label="Production timeline"
        aria-rowcount={layout.rows.length}
        data-visible-row-ids={visible.map((row) => row.node.id).join(",")}
        style={{
          position: "relative",
          height: `${layout.viewport_height}px`,
          minWidth: 0,
          overflow: "hidden",
          letterSpacing: 0,
        }}
      >
        <div
          role="rowgroup"
          style={{
            position: "relative",
            height: `${layout.total_height}px`,
            transform: `translateY(${-layout.scroll_top}px)`,
          }}
        >
          {visible.map((row, visibleIndex) => {
            const selected = sameSelectionReference(selection, row.node.selection);
            const expandable = isCollapsible(row);
            const primitiveCount = layout.lanes_by_row.get(row.node.id)?.assignments.length ?? 0;
            return (
              <div
                key={row.node.id}
                ref={(element) => {
                  if (element === null) {
                    rowElements.current.delete(row.node.id);
                  } else {
                    rowElements.current.set(row.node.id, element);
                  }
                }}
                class="timeline-treegrid__row"
                role="row"
                aria-level={row.depth}
                aria-rowindex={row.index + 1}
                aria-selected={selected}
                aria-expanded={expandable ? row.node.expanded : undefined}
                aria-label={rowLabel(row)}
                tabIndex={selected || (selectedVisible < 0 && visibleIndex === 0) ? 0 : -1}
                data-node-id={row.node.id}
                data-kind={row.node.kind}
                data-selected={selected ? "true" : "false"}
                onKeyDown={(event) => onRowKeyDown(event, row, visibleIndex)}
                onClick={() => onSelect(row.node.selection)}
                style={{
                  position: "absolute",
                  top: `${row.top}px`,
                  left: 0,
                  right: 0,
                  display: "grid",
                  gridTemplateColumns: "minmax(12rem, 18rem) minmax(5rem, 1fr) 6rem",
                  minWidth: 0,
                  height: `${row.height}px`,
                  alignItems: "center",
                  borderBottom: "1px solid #d3dad4",
                  background: selected ? "#dcefeb" : row.index % 2 === 0 ? "#f7f9f7" : "#ffffff",
                  overflow: "hidden",
                }}
              >
                <div
                  role="gridcell"
                  style={{
                    display: "flex",
                    minWidth: 0,
                    height: "100%",
                    alignItems: "center",
                    paddingLeft: `${Math.max(0, row.depth - 1) * 16}px`,
                  }}
                >
                  {expandable ? (
                    <button
                      type="button"
                      aria-label={`${row.node.expanded ? "Collapse" : "Expand"} ${row.node.label}`}
                      title={`${row.node.expanded ? "Collapse" : "Expand"} ${row.node.label}`}
                      onClick={(event) => {
                        event.stopPropagation();
                        onToggle(row.node.id);
                      }}
                      style={{ width: "32px", minWidth: "32px", height: "32px", padding: 0 }}
                    >
                      {row.node.expanded
                        ? <ChevronDown aria-hidden="true" size={16} />
                        : <ChevronRight aria-hidden="true" size={16} />}
                    </button>
                  ) : <span aria-hidden="true" style={{ width: "32px", minWidth: "32px" }} />}
                  <button
                    type="button"
                    onClick={(event) => {
                      event.stopPropagation();
                      onSelect(row.node.selection);
                    }}
                    style={{
                      minWidth: 0,
                      height: "32px",
                      padding: "0 8px",
                      border: 0,
                      background: "transparent",
                      overflow: "hidden",
                      textAlign: "left",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    {row.node.label}
                  </button>
                </div>
                <div role="gridcell">{primitiveCount} timeline item{primitiveCount === 1 ? "" : "s"}</div>
                <div role="gridcell" data-status={row.node.status ?? "group"}>
                  {row.node.status ?? "group"}
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </section>
  );
}
