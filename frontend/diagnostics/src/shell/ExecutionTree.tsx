import { ChevronDown, ChevronRight } from "lucide-preact";

import type { SelectionReference } from "../state/model.ts";
import type { ExecutionNode, ExecutionStatus, ExecutionTreeModel } from "./selectors.ts";


export interface ExecutionTreeProps {
  readonly model: ExecutionTreeModel;
  readonly onSelect: (selection: SelectionReference) => void;
  readonly onToggle: (key: string) => void;
}

const STATUS_LABELS: Readonly<Record<ExecutionStatus, string>> = {
  queued: "queued",
  waiting: "waiting",
  running: "running",
  completed: "completed",
  failed: "failed",
  cancelled: "cancelled",
  partial: "partial",
};

function Status({ value }: { readonly value: ExecutionStatus }) {
  return (
    <span class="execution-tree__status" data-status={value}>
      {STATUS_LABELS[value]}
    </span>
  );
}

function NodeRow({
  node,
  level,
  onSelect,
  onToggle,
}: {
  readonly node: ExecutionNode;
  readonly level: number;
  readonly onSelect: ExecutionTreeProps["onSelect"];
  readonly onToggle: ExecutionTreeProps["onToggle"];
}) {
  const toggleLabel = `${node.expanded ? "Collapse" : "Expand"} ${node.label}`;
  return (
    <li
      class="execution-tree__item"
      role="treeitem"
      aria-level={level}
      aria-selected={node.selected}
      aria-expanded={node.expandable ? node.expanded : undefined}
      data-kind={node.kind}
    >
      <div class="execution-tree__row" data-selected={node.selected ? "true" : "false"}>
        {node.expandable ? (
          <button
            class="execution-tree__toggle"
            type="button"
            aria-label={toggleLabel}
            title={toggleLabel}
            onClick={() => onToggle(node.key)}
          >
            {node.expanded ? <ChevronDown aria-hidden="true" /> : <ChevronRight aria-hidden="true" />}
          </button>
        ) : <span class="execution-tree__toggle-spacer" aria-hidden="true" />}

        <button
          class="execution-tree__select"
          type="button"
          aria-label={node.secondaryLabel === null
            ? node.label
            : `${node.label}, ${node.secondaryLabel}`}
          onClick={() => onSelect(node.selection)}
        >
          <span class="execution-tree__label-block">
            <span class="execution-tree__label">{node.label}</span>
            {node.secondaryLabel === null ? null : (
              <span class="execution-tree__secondary">{node.secondaryLabel}</span>
            )}
          </span>
        </button>

        {node.actorSummary === null ? null : (
          <span class="execution-tree__actor-summary" aria-label="Cue summary">
            {node.actorSummary.done} done / {node.actorSummary.running} running / {node.actorSummary.queued} queued
          </span>
        )}

        {node.cueStages === null ? null : (
          <span class="execution-tree__cue-stages">
            <span>wait {STATUS_LABELS[node.cueStages.wait]}</span>
            <span>execution {STATUS_LABELS[node.cueStages.execution]}</span>
          </span>
        )}

        {node.status === null ? null : <Status value={node.status} />}
      </div>

      {node.children.length > 0 && (!node.expandable || node.expanded) ? (
        <ul class="execution-tree__group" role="group">
          {node.children.map((child) => (
            <NodeRow
              key={child.key}
              node={child}
              level={level + 1}
              onSelect={onSelect}
              onToggle={onToggle}
            />
          ))}
        </ul>
      ) : null}
    </li>
  );
}

export function ExecutionTree({ model, onSelect, onToggle }: ExecutionTreeProps) {
  return (
    <div class="execution-tree">
      {model.needsServerRefresh ? (
        <p class="execution-tree__notice" role="status">Partial execution data</p>
      ) : null}
      <ul class="execution-tree__root" role="tree" aria-label="Production execution">
        <NodeRow node={model.root} level={1} onSelect={onSelect} onToggle={onToggle} />
      </ul>
    </div>
  );
}
