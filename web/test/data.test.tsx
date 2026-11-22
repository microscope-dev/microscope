import { describe, expect, it } from "vitest";

import {
  closeData,
  dataHash,
  dataWindow,
  formatBytesPerDay,
  openData,
  parseDataHash,
} from "../src/dataPolicy";

describe("dataPolicy store and URL hash", () => {
  it("round-trips tabs and optional org in URL hashes", () => {
    expect(parseDataHash("#data/services")).toEqual({ tab: "services", org: null });
    expect(parseDataHash("#data/recording?org=42")).toEqual({ tab: "recording", org: 42 });
    expect(parseDataHash("#data/privacy?org=7")).toEqual({ tab: "privacy", org: 7 });
    expect(parseDataHash("#data/symbols")).toEqual({ tab: "symbols", org: null });
    expect(parseDataHash("#data/storage?org=9")).toEqual({ tab: "storage", org: 9 });
    expect(parseDataHash("#data/unknown")).toEqual({ tab: "services", org: null });
    expect(parseDataHash("#machines/overview")).toBeNull();

    expect(dataHash("privacy", null)).toBe("data/privacy");
    expect(dataHash("storage", 12)).toBe("data/storage?org=12");
  });

  it("opens, switches tab, and closes cleanly", () => {
    openData("recording", 5);
    expect(dataWindow.getState().open).toBe(true);
    expect(dataWindow.getState().tab).toBe("recording");
    expect(dataWindow.getState().org).toBe(5);

    closeData();
    expect(dataWindow.getState().open).toBe(false);
  });

  it("formats estimated daily storage volume", () => {
    expect(formatBytesPerDay(0)).toBe("0 MB/day");
    expect(formatBytesPerDay(25_000_000)).toBe("25 MB/day");
    expect(formatBytesPerDay(1_550_000_000)).toBe("1.55 GB/day");
    expect(formatBytesPerDay(24_500_000_000)).toBe("24.5 GB/day");
  });
});
