import { describe, expect, it, vi } from "vitest";

import { account } from "../src/account";
import {
  activeView,
  clearActiveView,
  dashboardsApi,
  dashboardsHash,
  DEFAULT_SCOPE,
  ensureSessionUrl,
  formatBytes,
  formatDimensionLabel,
  normalizeScope,
  parseDashboardsHash,
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
});
