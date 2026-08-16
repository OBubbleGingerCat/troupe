import type {
  TokenIntegerString,
  U64String,
} from "../protocol/decimal.ts";


export const UNKNOWN_USAGE_VALUE = "Unknown";

export function formatExactInteger(value: TokenIntegerString | U64String): string {
  return value.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

export function formatTokenCount(value: TokenIntegerString | null): string {
  return value === null ? UNKNOWN_USAGE_VALUE : formatExactInteger(value);
}

export function formatU64Count(value: U64String | null): string {
  return value === null ? UNKNOWN_USAGE_VALUE : formatExactInteger(value);
}

export function contextOccupancyPercent(
  used: U64String | null,
  size: U64String | null,
): number | null {
  if (used === null || size === null || size === "0") {
    return null;
  }
  const usedValue = BigInt(used);
  const sizeValue = BigInt(size);
  const bounded = usedValue >= sizeValue ? 100n : (usedValue * 100n) / sizeValue;
  return Number(bounded);
}

export function formatCoverage(reported: U64String, finalized: U64String): string {
  return `${formatExactInteger(reported)} / ${formatExactInteger(finalized)} Acts`;
}

export function formatUnavailableReason(reason: string): string {
  return reason.split("_").map((part) => (
    part.length === 0 ? part : `${part[0]!.toUpperCase()}${part.slice(1)}`
  )).join(" ");
}
