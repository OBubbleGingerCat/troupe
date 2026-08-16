import { describe, expect, it } from "vitest";

import {
  classifyArchivedViewRecord,
  decodeViewCapabilities,
  decodeViewRecord,
  decodeViewResponse,
  encodeViewRecordJson,
  encodeViewResponseJson,
} from "../../src/protocol/view.ts";
import {
  loadViewFixture,
  loadViewManifest,
} from "../support/diagnostic-fixtures.ts";


describe("canonical diagnostic view protocol", () => {
  it("decodes all compatible records and renderer responses losslessly", () => {
    const compatible = loadViewFixture("compatible.json") as {
      capabilities: unknown;
      records: readonly unknown[];
    };
    expect(decodeViewCapabilities(compatible.capabilities).max_time_series_points).toBe(1024);
    expect(compatible.records.map((record) => decodeViewRecord(record).renderer)).toEqual([
      "timeline",
      "metric",
      "table",
      "time_series",
    ]);

    for (const entry of loadViewManifest().fixtures) {
      if (entry.format !== "renderer_fixture") {
        continue;
      }
      const fixture = loadViewFixture(entry.file) as { descriptor: unknown; response: unknown };
      const descriptor = decodeViewRecord(fixture.descriptor);
      const response = decodeViewResponse(fixture.response, descriptor);
      expect(JSON.parse(encodeViewRecordJson(descriptor))).toEqual(fixture.descriptor);
      expect(JSON.parse(encodeViewResponseJson(response))).toEqual(fixture.response);
      expect(response.renderer).toBe(descriptor.renderer);
      expect(typeof response.binding.captured_watermark).toBe("string");
      expect(typeof response.binding.captured_elapsed_end_ns).toBe("string");
    }
  });

  it("keeps metric aggregate identities unit-qualified", () => {
    type MutableMetricFixture = {
      descriptor: Record<string, unknown> & { query: { source: unknown } };
      response: Record<string, unknown> & { series: Array<Record<string, unknown>> };
    };
    const metricFixture = (): MutableMetricFixture => structuredClone(
      loadViewFixture("metric.json"),
    ) as MutableMetricFixture;

    const canonical = metricFixture();
    const canonicalResponse = decodeViewResponse(
      canonical.response,
      decodeViewRecord(canonical.descriptor),
    );
    expect(canonicalResponse.renderer).toBe("metric");
    if (canonicalResponse.renderer === "metric") {
      expect(canonicalResponse.series[0]!.unit).toBe("tokens");
    }

    const custom = metricFixture();
    custom.descriptor.query.source = {
      source: "counter_value",
      selector: { selector: "custom", name: "app.queue" },
      selection: "latest_before_reduce",
    };
    const items = structuredClone(custom.response.series[0]!);
    items.unit = "items";
    custom.response.series = [items, { ...structuredClone(items), unit: "bytes" }];
    const customResponse = decodeViewResponse(
      custom.response,
      decodeViewRecord(custom.descriptor),
    );
    expect(customResponse.renderer === "metric" && customResponse.series).toHaveLength(2);

    const duplicate = metricFixture();
    duplicate.response.series.push(structuredClone(duplicate.response.series[0]!));
    expect(() => decodeViewResponse(
      duplicate.response,
      decodeViewRecord(duplicate.descriptor),
    )).toThrow(/metric_identity/);

    for (const invalidUnit of [null, "count", ""] as const) {
      const invalid = metricFixture();
      invalid.response.series[0]!.unit = invalidUnit;
      expect(() => decodeViewResponse(
        invalid.response,
        decodeViewRecord(invalid.descriptor),
      )).toThrow(/metric_unit/);
    }

    const oversized = structuredClone(custom);
    oversized.response.series[0]!.unit = "u".repeat(33);
    expect(() => decodeViewResponse(
      oversized.response,
      decodeViewRecord(oversized.descriptor),
    )).toThrow(/metric_unit/);

    const missing = metricFixture();
    delete missing.response.series[0]!.unit;
    expect(() => decodeViewResponse(
      missing.response,
      decodeViewRecord(missing.descriptor),
    )).toThrow(/fields/);
  });

  it("rejects the shared executable and open-ended descriptor shapes", () => {
    const fixture = loadViewFixture("invalid-descriptor.json") as {
      cases: readonly { name: string; record: unknown }[];
    };
    for (const testCase of fixture.cases) {
      expect(() => decodeViewRecord(testCase.record), testCase.name).toThrow();
    }
  });

  it("classifies newer and corrupt archived records without coupling versions", () => {
    for (const file of ["newer.json", "corrupt.json"] as const) {
      const fixture = loadViewFixture(file) as { record: unknown; expected_reason: string };
      const result = classifyArchivedViewRecord(fixture.record);
      expect(result.status).toBe("incompatible");
      if (result.status === "incompatible") {
        expect(result.reason).toBe(fixture.expected_reason);
      }
    }
  });

  it("keeps table and time-series limits exact without converting identities to number", () => {
    const tableFixture = loadViewFixture("table.json") as { descriptor: unknown; response: unknown };
    const table = decodeViewResponse(tableFixture.response, decodeViewRecord(tableFixture.descriptor));
    expect(table.renderer).toBe("table");
    if (table.renderer === "table") {
      expect(table.rows).toHaveLength(500);
      expect(typeof table.rows[499]!.sequence).toBe("string");
    }

    const seriesFixture = loadViewFixture("timeseries.json") as {
      descriptor: unknown;
      response: unknown;
    };
    const series = decodeViewResponse(
      seriesFixture.response,
      decodeViewRecord(seriesFixture.descriptor),
    );
    expect(series.renderer).toBe("time_series");
    if (series.renderer === "time_series") {
      expect(series.series[0]!.points).toHaveLength(1024);
      expect(typeof series.bucket_width_ns).toBe("string");
    }
  });
});
