import { describe, expect, it, vi } from "vitest";

import { account } from "../src/account";
import {
  activeView,
  clearActiveView,
  computeLagMs,
  dashboardsApi,
  dashboardsHash,
  DEFAULT_SCOPE,
  ensureSessionUrl,
  formatBytes,
  formatDataLag,
  formatDimensionLabel,
  normalizeScope,
  openDashboardStream,
  parseDashboardsHash,
  parseSseChunk,
  registerViewHooks,
  scopeToSearch,
  searchToScope,
  updateViewScope,
  type SessionRecord,
  type ViewScope,
} from "../src/dashboards";
import { KIND } from "../src/documents";
import { updatePaneParams } from "../src/ui/dashboards/paneScope";
import { emptyLayout, type GroupId, type PaneId } from "../src/workbench/layout";
import { loadLayout, saveLayout } from "../src/workbench/saved";
import { resetWorkbench, workbench } from "../src/workbench/store";

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
    expect(formatDimensionLabel("host.id")).toBe("machine");
    expect(formatDimensionLabel("k8s.pod.name")).toBe("pod");
    expect(formatDimensionLabel("custom.label")).toBe("custom.label");
    expect(normalizeScope(null)).toEqual(DEFAULT_SCOPE);
  });

  it("persists pane settings and unlinked scopes in workbench layout serialization", () => {
    const p0 = "p0" as PaneId;
    const g0 = "g0" as GroupId;
    const base = {
      ...emptyLayout(),
      root: { type: "group" as const, id: g0, tabs: [p0], active: p0 },
      focused: g0,
      panes: {
        [p0]: {
          kind: KIND.fleetOverTime,
          key: "fleet-over-time",
          params: { groupBy: "service.version" },
        },
      },
      next: 1,
    };
    resetWorkbench(base);
    updatePaneParams(p0, {
      groupBy: "deployment.environment",
      unlinkedScope: {
        org_id: 7,
        project_ids: [],
        filters: [{ dimension: "service.name", value: "checkout" }],
        time_range: { kind: "relative", duration_secs: 900 },
        refresh: "off",
        compare: null,
        selection: null,
      },
    });
    const saved = saveLayout(workbench.getState().layout);
    const reloaded = loadLayout(saved);
    expect(reloaded.ok).toBe(true);
    const restored = (reloaded.ok ? reloaded.layout.panes[p0]?.params : undefined) as
      | { groupBy?: string; unlinkedScope?: { filters: { dimension: string; value: string }[] } }
      | undefined;
    expect(restored?.groupBy).toBe("deployment.environment");
    expect(restored?.unlinkedScope?.filters).toEqual([
      { dimension: "service.name", value: "checkout" },
    ]);
  });

  it("flushes dirty edits when clearing an active session view and serializes Copy link with autosaves", async () => {
    const prevAccount = account.getState();
    vi.stubGlobal("window", { location: { origin: "http://localhost" } });
    account.setState({
      now: {
        kind: "in",
        user: {
          id: 1,
          username: "tester",
          email: "t@example.com",
          totp_enabled: false,
        },
      },
    });
    const patched: { id: number; refresh?: string }[] = [];
    let resolveGate!: () => void;
    const gate = new Promise<void>((resolve) => {
      resolveGate = resolve;
    });
    const updateSpy = vi
      .spyOn(dashboardsApi, "updateSession")
      .mockImplementation(async (id, body) => {
        if (patched.length === 0) {
          await gate;
        }
        patched.push({ id, ...(body.scope ? { refresh: body.scope.refresh } : {}) });
        const res: SessionRecord = {
          id,
          orgId: 1,
          projectId: null,
          ownerUserId: 1,
          ownerUsername: "tester",
          isOwner: true,
          name: null,
          sourceDashboardId: null,
          sourceBuiltin: "fleet-overview",
          layout: body.layout ?? {},
          scope: body.scope ?? DEFAULT_SCOPE,
          notes: "",
          sharedAt: null,
          keptAt: null,
          pinStatus: "none",
          pinnedBytes: 0,
          expiresAt: null,
          lastOpenedAt: 0,
          createdAt: 0,
          updatedAt: 0,
          visitorCount: 0,
          locked: false,
        };
        return res;
      });
    const unhook = registerViewHooks({
      moveToSession: () => {},
      serializeLayout: () => ({ v: 1 }),
    });
    try {
      activeView.setState({
        active: true,
        mode: "session",
        dashboardId: null,
        builtin: false,
        sessionId: 77,
        orgId: 1,
        projectId: null,
        title: "Session #77",
        canEdit: true,
        isOwner: true,
        locked: false,
        movedToSession: true,
        scope: { ...DEFAULT_SCOPE, refresh: "m1" },
        dimensions: null,
        revision: 0,
        lastFetchedAt: null,
        pinStatus: "none",
        pinnedBytes: 0,
        keptAt: null,
        expiresAt: null,
      });

      const copyPromise = ensureSessionUrl();
      await Promise.resolve();
      updateViewScope((prev) => ({ ...prev, refresh: "s5" }));
      clearActiveView();
      expect(activeView.getState().active).toBe(false);

      resolveGate();
      const url = await copyPromise;
      expect(url).toContain("/s/77");
      await new Promise<void>((resolve) => {
        setTimeout(resolve, 10);
      });

      expect(patched).toEqual([
        { id: 77, refresh: "m1" },
        { id: 77, refresh: "s5" },
      ]);
    } finally {
      unhook();
      updateSpy.mockRestore();
      vi.unstubAllGlobals();
      account.setState(prevAccount);
      clearActiveView();
    }
  });

  it("parses chunked SSE streams and formats pane lag cleanly", () => {
    const first = parseSseChunk("", ': ping\n\nevent: status\ndata: {"watching":');
    expect(first.events).toEqual([]);
    const second = parseSseChunk(
      first.rest,
      'true}\r\n\r\nevent: pane\ndata: {"id":"p1"}\n\npartial',
    );
    expect(second.events).toEqual([
      { event: "status", data: '{"watching":true}' },
      { event: "pane", data: '{"id":"p1"}' },
    ]);
    expect(second.rest).toBe("partial");

    expect(formatDataLag(null)).toBeNull();
    expect(formatDataLag(500)).toBe("fresh");
    expect(formatDataLag(4_800)).toBe("5s behind");
    expect(formatDataLag(125_000)).toBe("2m behind");
    expect(computeLagMs(1_000_000_000_000, 1_003_500_000_000, 10_000, 11_500)).toBe(5_000);
  });

  it("streams status, pane, dimensions, and heartbeat events over POST /api/query/stream", async () => {
    const enc = new TextEncoder();
    const events: string[] = [];
    let aborted = false;

    const fetchMock = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>(
      (_url: string, init?: RequestInit) => {
        init?.signal?.addEventListener("abort", () => {
          aborted = true;
        });
        const body = new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(
              enc.encode(
                'event: status\ndata: {"watching":true,"watchedMachines":4,"effectiveIntervalMs":5000,"locked":false,"serverTimeNs":1000000000,"newestDataNs":999000000}\n\n',
              ),
            );
            controller.enqueue(
              enc.encode(
                'event: pane\ndata: {"id":"p0","kind":"flame","serverTimeNs":1000000000,"newestDataNs":999000000,"data":{"nodes":1}}\n\n',
              ),
            );
            controller.enqueue(
              enc.encode(
                'event: heartbeat\ndata: {"serverTimeNs":2000000000,"newestDataNs":1999000000,"watchedMachines":4,"effectiveIntervalMs":5000}\n\n',
              ),
            );
          },
        });
        return Promise.resolve(
          new Response(body, {
            status: 200,
            headers: { "Content-Type": "text/event-stream" },
          }),
        );
      },
    );
    vi.stubGlobal("fetch", fetchMock);
    try {
      const handle = openDashboardStream(
        {
          scope: { ...DEFAULT_SCOPE, refresh: "s5" },
          panes: [{ kind: "flame", id: "p0", invert: false }],
        },
        {
          onOpen: () => events.push("open"),
          onStatus: (ev) => events.push(`status:${ev.watching}:${ev.watchedMachines}`),
          onPane: (ev) => events.push(`pane:${ev.id}:${ev.kind}`),
          onHeartbeat: (ev) => events.push(`hb:${ev.watchedMachines}`),
        },
      );
      await vi.waitFor(() => {
        expect(events).toContain("hb:4");
      });
      expect(events).toEqual(["open", "status:true:4", "pane:p0:flame", "hb:4"]);
      handle.close();
      expect(aborted).toBe(true);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("clears streamed panes and reports disconnect on 502 so direct queries can resume", async () => {
    let disconnects = 0;
    const fetchMock = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>(() =>
      Promise.resolve(new Response("bad gateway", { status: 502 })),
    );
    vi.stubGlobal("fetch", fetchMock);
    try {
      const handle = openDashboardStream(
        {
          scope: { ...DEFAULT_SCOPE, refresh: "s5" },
          panes: [{ kind: "flame", id: "p0", invert: false }],
        },
        {
          onDisconnect: () => {
            disconnects += 1;
          },
        },
      );
      await vi.waitFor(() => {
        expect(disconnects).toBeGreaterThanOrEqual(1);
      });
      handle.close();
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
