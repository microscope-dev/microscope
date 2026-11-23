import { describe, expect, it } from "vitest";

import {
  captureDownloadUrl,
  decodeFramedResponse,
  isCaptureProfile,
  parseCaptureProfileId,
} from "../src/captures";
import {
  buildCurlCommand,
  buildQueryCommand,
  DEFAULT_SCOPE,
  type ViewScope,
} from "../src/dashboards";

describe("captures client helpers", () => {
  it("parses capture profile ids strictly", () => {
    expect(parseCaptureProfileId("capture:42")).toBe(42);
    expect(isCaptureProfile("capture:42")).toBe(true);
    expect(parseCaptureProfileId("capture:0")).toBeNull();
    expect(parseCaptureProfileId("capture:-1")).toBeNull();
    expect(parseCaptureProfileId("capture:abc")).toBeNull();
    expect(parseCaptureProfileId("p0")).toBeNull();
    expect(parseCaptureProfileId(null)).toBeNull();
    expect(captureDownloadUrl(7)).toBe("/api/captures/7/download");
  });

  it("decodes framed binary responses with 4-byte LE JSON length prefix", () => {
    const meta = { nodes: 2, weight: 40_000 };
    const jsonBytes = new TextEncoder().encode(JSON.stringify(meta));
    const payload = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]);
    const buf = new ArrayBuffer(4 + jsonBytes.byteLength + payload.byteLength);
    new DataView(buf).setUint32(0, jsonBytes.byteLength, true);
    new Uint8Array(buf, 4, jsonBytes.byteLength).set(jsonBytes);
    new Uint8Array(buf, 4 + jsonBytes.byteLength).set(payload);

    const decoded = decodeFramedResponse<{ nodes: number; weight: number }>(buf);
    expect(decoded.nodes).toBe(2);
    expect(decoded.weight).toBe(40_000);
    expect([...decoded.payload]).toEqual([1, 2, 3, 4, 5, 6, 7, 8]);
  });

  it("rejects truncated framed binary responses", () => {
    expect(() => decodeFramedResponse(new ArrayBuffer(2))).toThrow("truncated");
    const bad = new ArrayBuffer(8);
    new DataView(bad).setUint32(0, 999, true);
    expect(() => decodeFramedResponse(bad)).toThrow("malformed");
  });
});

describe("copy query command builders", () => {
  const scope: ViewScope = {
    ...DEFAULT_SCOPE,
    org_id: 9,
    project_ids: [3],
    filters: [
      { dimension: "service.name", value: "checkout" },
      { dimension: "host.name", value: "prod box 1" },
    ],
    time_range: { kind: "relative", duration_secs: 6 * 3600 },
    compare: { kind: "previous_version" },
  };

  it("formats runnable microscope query CLI commands for every pane kind", () => {
    const flame = buildQueryCommand(
      { kind: "flame", invert: true },
      scope,
      12,
      "http://127.0.0.1:3000",
    );
    expect(flame).toBe(
      "microscope query flame --invert --server http://127.0.0.1:3000 --session 12 --exact-scope --org 9 --project 3 --filter service.name=checkout --filter 'host.name=prod box 1' --last 6h --compare previous-version",
    );

    const overTime = buildQueryCommand(
      { kind: "over-time", groupBy: "service.version" },
      { ...DEFAULT_SCOPE, time_range: { kind: "absolute", start_time: 100, end_time: 200 } },
      null,
      "https://microscope.dev",
    );
    expect(overTime).toBe(
      "microscope query over-time --group-by service.version --from 100 --to 200",
    );

    const breakdown = buildQueryCommand(
      { kind: "breakdown", dimension: "k8s.pod.name" },
      DEFAULT_SCOPE,
      null,
    );
    expect(breakdown).toContain("microscope query breakdown --dimension k8s.pod.name --last 1h");
  });

  it("formats runnable curl commands with JSON body and bearer token placeholder", () => {
    const cmd = buildCurlCommand(
      { kind: "breakdown", dimension: "service.name" },
      scope,
      12,
      "http://127.0.0.1:3000/",
    );
    expect(cmd).toContain("curl -sS -X POST http://127.0.0.1:3000/api/query/breakdown");
    expect(cmd).toContain('"Authorization: Bearer $MICROSCOPE_TOKEN"');
    expect(cmd).toContain('"dimension":"service.name"');
    expect(cmd).toContain('"session_id":12');
  });
});
