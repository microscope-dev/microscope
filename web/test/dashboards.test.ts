import { describe, expect, it } from "vitest";

import {
  dashboardsHash,
  DEFAULT_SCOPE,
  formatBytes,
  formatDimensionLabel,
  normalizeScope,
  parseDashboardsHash,
  scopeToSearch,
  searchToScope,
  type ViewScope,
} from "../src/dashboards";

describe("dashboards URL and formatting helpers", () => {
  it("round-trips a full scope through query string parameters", () => {
    const scope: ViewScope = {
      org_id: 42,
      project_ids: [3, 9],
      filters: [
        { dimension: "service.name", value: "checkout" },
        { dimension: "k8s.pod.name", value: "pod-alpha" },
      ],
      time_range: { kind: "relative", duration_secs: 6 * 3600 },
      refresh: "s30",
      compare: { kind: "previous_version" },
      selection: { kind: "function", name: "hot_function" },
    };

    const search = scopeToSearch(scope);
    const parsed = searchToScope(search, DEFAULT_SCOPE);
    expect(parsed).toEqual(scope);
  });

  it("round-trips absolute time ranges and shifted compares", () => {
    const scope: ViewScope = {
      org_id: null,
      project_ids: [],
      filters: [{ dimension: "deployment.environment", value: "prod" }],
      time_range: {
        kind: "absolute",
        start_time: 1_763_700_000_000_000_000,
        end_time: 1_763_703_600_000_000_000,
      },
      refresh: "off",
      compare: { kind: "shift", offset_secs: 3600 },
      selection: { kind: "zone", name: "db.query" },
    };

    const search = scopeToSearch(scope);
    expect(searchToScope(search, DEFAULT_SCOPE)).toEqual(scope);
  });

  it("round-trips dashboards modal window hash fragments", () => {
    expect(parseDashboardsHash(`#${dashboardsHash("dashboards", null)}`)).toEqual({
      tab: "dashboards",
      org: null,
    });
    expect(parseDashboardsHash(`#${dashboardsHash("sessions", 17)}`)).toEqual({
      tab: "sessions",
      org: 17,
    });
    expect(parseDashboardsHash("#machines")).toBeNull();
  });

  it("formats byte sizes and dimension names cleanly", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(2048)).toBe("2.0 KB");
    expect(formatBytes(5 * 1024 * 1024)).toBe("5.0 MB");
    expect(formatDimensionLabel("service.name")).toBe("service");
    expect(formatDimensionLabel("k8s.pod.name")).toBe("pod");
    expect(formatDimensionLabel("custom.label")).toBe("custom.label");
    expect(normalizeScope(null)).toEqual(DEFAULT_SCOPE);
  });
});
