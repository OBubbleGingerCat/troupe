const U64_MAX = 18_446_744_073_709_551_615n;
const NONNEGATIVE_INTEGER = /^(?:0|[1-9][0-9]*)$/;
const CANONICAL_INTEGER = /^(?:0|-?[1-9][0-9]*)$/;
const CANONICAL_DECIMAL = /^-?(?:0|[1-9][0-9]*)(?:\.[0-9]*[1-9])?$/;
const CANONICAL_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

declare const u64Brand: unique symbol;
declare const tokenIntegerBrand: unique symbol;
declare const canonicalIntegerBrand: unique symbol;
declare const decimalBrand: unique symbol;
declare const uuidBrand: unique symbol;

export type U64String = string & { readonly [u64Brand]: true };
export type TokenIntegerString = string & { readonly [tokenIntegerBrand]: true };
export type CanonicalIntegerString = string & { readonly [canonicalIntegerBrand]: true };
export type DecimalString = string & { readonly [decimalBrand]: true };
export type CanonicalUuid = string & { readonly [uuidBrand]: true };

export type JsonPrimitive = null | boolean | number | string;
export type JsonValue = JsonPrimitive | JsonObject | readonly JsonValue[];
export interface JsonObject {
  readonly [key: string]: JsonValue;
}

export class ProtocolDecodeError extends Error {
  readonly code: string;
  readonly path: string;

  constructor(code: string, path: string, detail: string) {
    super(`${code} at ${path}: ${detail}`);
    this.name = "ProtocolDecodeError";
    this.code = code;
    this.path = path;
  }
}

export function failProtocol(code: string, path: string, detail: string): never {
  throw new ProtocolDecodeError(code, path, detail);
}

export function expectObject(value: unknown, path: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    failProtocol("type", path, "expected object");
  }
  return value as Record<string, unknown>;
}

export function expectArray(value: unknown, path: string): readonly unknown[] {
  if (!Array.isArray(value)) {
    failProtocol("type", path, "expected array");
  }
  return value;
}

export function expectString(value: unknown, path: string): string {
  if (typeof value !== "string") {
    failProtocol("type", path, "expected string");
  }
  return value;
}

export function expectBoolean(value: unknown, path: string): boolean {
  if (typeof value !== "boolean") {
    failProtocol("type", path, "expected boolean");
  }
  return value;
}

export function expectInteger(value: unknown, path: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) {
    failProtocol("type", path, "expected safe integer number");
  }
  return value;
}

export function expectExactFields(
  value: Record<string, unknown>,
  fields: readonly string[],
  path: string,
): void {
  const expected = new Set(fields);
  const actual = Object.keys(value);
  const missing = fields.filter(
    (field) => !Object.prototype.hasOwnProperty.call(value, field),
  );
  const extra = actual.filter((field) => !expected.has(field));
  if (missing.length > 0 || extra.length > 0) {
    failProtocol(
      "fields",
      path,
      `missing=${JSON.stringify(missing.sort())}, extra=${JSON.stringify(extra.sort())}`,
    );
  }
}

export function expectEnum<const T extends string>(
  value: unknown,
  allowed: readonly T[],
  path: string,
  code = "discriminant",
): T {
  const text = expectString(value, path);
  if (!(allowed as readonly string[]).includes(text)) {
    failProtocol(code, path, `unknown value ${JSON.stringify(text)}`);
  }
  return text as T;
}

export function decodeU64(value: unknown, path = "value"): U64String {
  const text = expectString(value, path);
  if (!NONNEGATIVE_INTEGER.test(text) || BigInt(text) > U64_MAX) {
    failProtocol("u64", path, "expected canonical decimal string in the u64 range");
  }
  return text as U64String;
}

export function decodeTokenInteger(value: unknown, path = "value"): TokenIntegerString {
  const text = expectString(value, path);
  if (!NONNEGATIVE_INTEGER.test(text)) {
    failProtocol("token_integer", path, "expected canonical nonnegative integer string");
  }
  return text as TokenIntegerString;
}

export function decodeCanonicalInteger(
  value: unknown,
  path = "value",
): CanonicalIntegerString {
  const text = expectString(value, path);
  if (!CANONICAL_INTEGER.test(text)) {
    failProtocol("integer", path, "expected canonical integer string");
  }
  return text as CanonicalIntegerString;
}

export function decodeDecimal(value: unknown, path = "value"): DecimalString {
  const text = expectString(value, path);
  if (!CANONICAL_DECIMAL.test(text) || text === "-0") {
    failProtocol("decimal", path, "expected normalized fixed decimal string");
  }
  return text as DecimalString;
}

export function decodeCanonicalUuid(value: unknown, path = "value"): CanonicalUuid {
  const text = expectString(value, path);
  if (!CANONICAL_UUID.test(text)) {
    failProtocol("uuid", path, "expected lowercase canonical hyphenated UUID");
  }
  return text as CanonicalUuid;
}

export function decodeJsonValue(value: unknown, path = "value"): JsonValue {
  if (value === null || typeof value === "string" || typeof value === "boolean") {
    return value;
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      failProtocol("type", path, "JSON number must be finite");
    }
    return value;
  }
  if (Array.isArray(value)) {
    value.forEach((item, index) => decodeJsonValue(item, `${path}[${index}]`));
    return value as readonly JsonValue[];
  }
  const object = expectObject(value, path);
  for (const [key, item] of Object.entries(object)) {
    decodeJsonValue(item, `${path}.${key}`);
  }
  return object as JsonObject;
}

export function parseProtocolJson(text: string, path = "json"): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch (error) {
    failProtocol("json", path, error instanceof Error ? error.message : "invalid JSON");
  }
}

export function u64ToBigInt(value: U64String): bigint {
  return BigInt(value);
}

export function tokenIntegerToBigInt(value: TokenIntegerString): bigint {
  return BigInt(value);
}

export function compareU64(left: U64String, right: U64String): -1 | 0 | 1 {
  const leftValue = BigInt(left);
  const rightValue = BigInt(right);
  return leftValue < rightValue ? -1 : leftValue > rightValue ? 1 : 0;
}

export function viewportDeltaToNumber(
  elapsed: U64String,
  viewportOrigin: U64String,
  maximumMagnitude = Number.MAX_SAFE_INTEGER,
): number {
  if (
    !Number.isSafeInteger(maximumMagnitude)
    || maximumMagnitude < 0
  ) {
    failProtocol("viewport_delta", "maximumMagnitude", "expected nonnegative safe integer");
  }
  const delta = BigInt(elapsed) - BigInt(viewportOrigin);
  const limit = BigInt(maximumMagnitude);
  if (delta < -limit || delta > limit) {
    failProtocol("viewport_delta", "elapsed", "relative value exceeds the validated viewport bound");
  }
  return Number(delta);
}
