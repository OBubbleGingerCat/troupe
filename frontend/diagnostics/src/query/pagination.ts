import { failProtocol } from "../protocol/decimal.ts";
import type {
  ViewCapabilities,
  ViewRecord,
  ViewResponse,
} from "../protocol/view.ts";


export interface ViewPaginationInput {
  readonly cursor?: string | null;
  readonly page_size?: number;
}

export interface FrozenViewPagination {
  readonly cursor: string | null;
  readonly page_size: number | null;
}

function validateCursor(value: string | null): string | null {
  if (value === null) {
    return null;
  }
  if (value.length === 0 || value.length > 512 || !/^[\x00-\x7f]+$/.test(value)) {
    failProtocol("cursor", "query.cursor", "opaque cursor is out of bounds");
  }
  return value;
}

function validatePageSize(value: number, maximum: number): number {
  if (!Number.isSafeInteger(value) || value < 1 || value > maximum || value > 500) {
    failProtocol("page_size", "query.page_size", "page size is out of bounds");
  }
  return value;
}

export function freezeViewPagination(
  record: ViewRecord,
  capabilities: ViewCapabilities,
  input: ViewPaginationInput = {},
): FrozenViewPagination {
  const rowRenderer = record.renderer === "timeline" || record.renderer === "table";
  const cursor = input.cursor ?? null;
  if (!rowRenderer) {
    if (cursor !== null || input.page_size !== undefined) {
      failProtocol("pagination", "query", "aggregate renderer cannot be paginated");
    }
    return { cursor: null, page_size: null };
  }
  const descriptorPageSize = record.renderer === "table"
    ? (record.query as { readonly page_size: number }).page_size
    : capabilities.max_page_rows;
  const pageSize = validatePageSize(
    input.page_size ?? descriptorPageSize,
    capabilities.max_page_rows,
  );
  return { cursor: validateCursor(cursor), page_size: pageSize };
}

export function appendViewPaginationParameters(
  parameters: URLSearchParams,
  pagination: FrozenViewPagination,
): void {
  if (pagination.page_size !== null) {
    parameters.set("page_size", String(pagination.page_size));
  }
  if (pagination.cursor !== null) {
    parameters.set("cursor", pagination.cursor);
  }
}

export function viewPaginationKey(pagination: FrozenViewPagination): string {
  return JSON.stringify([pagination.page_size, pagination.cursor]);
}

export function assertViewResponsePagination(
  response: ViewResponse,
  pagination: FrozenViewPagination,
): void {
  if (pagination.page_size === null) {
    if (response.pagination !== null) {
      failProtocol("pagination", "response.pagination", "aggregate response is paginated");
    }
    return;
  }
  if (
    response.pagination === null
    || response.pagination.page_size !== pagination.page_size
  ) {
    failProtocol(
      "pagination",
      "response.pagination",
      "response page size differs from the frozen request",
    );
  }
}

export function nextViewPagination(
  response: ViewResponse,
): FrozenViewPagination | null {
  if (response.pagination === null || response.pagination.next_cursor === null) {
    return null;
  }
  return {
    cursor: validateCursor(response.pagination.next_cursor),
    page_size: validatePageSize(response.pagination.page_size, 500),
  };
}
