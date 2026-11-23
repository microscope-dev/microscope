import { useNavigate } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, onSettled, Show } from "solid-js";

import { checkAccount, useAccount } from "../../account";
import { captureDownloadUrl } from "../../captures";
import {
  activeView,
  closeDashboards,
  closeSaveDashboardDialog,
  type DashboardRecord,
  dashboardsApi,
  dashboardsHash,
  type DashboardsListResponse,
  type DashboardsTab,
  dashboardsWindow,
  formatBytes,
  openDashboards,
  parseDashboardsHash,
  type PinEstimate,
  type SessionRecord,
  type SessionsListResponse,
  useDashboardsWindow,
} from "../../dashboards";
import { ago, day } from "../../fleet";
import { toHref, useHistory } from "../../history";
import { useDirectory } from "../../orgs";
import { flash } from "../../tab";
import { standalone } from "../../target";
import { isTextBox } from "../../workbench/keys";
import { saveLayout } from "../../workbench/saved";
import { workbench } from "../../workbench/store";
import { useDialogFocus } from "../dialog";
import { useLoaded } from "../Orgs";

function useDashboardsUrl() {
  const history = useHistory();
  const open = useDashboardsWindow((w) => w.open);
  const tab = useDashboardsWindow((w) => w.tab);
  const org = useDashboardsWindow((w) => w.org);
  let written: string | null = null;

  onSettled(() => {
    const fromUrl = (moved: boolean) => {
      const parsed = parseDashboardsHash(history.location.hash);
      if (parsed) {
        written = dashboardsHash(parsed.tab, parsed.org);
        const now = dashboardsWindow.getState();
        dashboardsWindow.setState(
          now.open ? { tab: parsed.tab, org: parsed.org } : { waiting: parsed, pushed: false },
        );
      } else if (moved && dashboardsWindow.getState().open) {
        written = null;
        closeDashboards();
        dashboardsWindow.setState({ pushed: false });
      }
    };
    fromUrl(false);
    return history.subscribe(({ action }) => {
      if (action.type === "PUSH" || action.type === "REPLACE") {
        return;
      }
      fromUrl(true);
    });
  });

  createEffect(
    () => [open(), tab(), org()] as const,
    ([isOpen, curTab, curOrg]) => {
      const here = `${history.location.pathname}${history.location.search}`;
      if (isOpen) {
        const hash = dashboardsHash(curTab, curOrg);
        if (hash === written && parseDashboardsHash(history.location.hash) !== null) {
          return;
        }
        const st = dashboardsWindow.getState();
        written = hash;
        if (parseDashboardsHash(history.location.hash) === null && !st.pushed) {
          dashboardsWindow.setState({ pushed: true });
          history.push(`${here}#${hash}`);
        } else {
          history.replace(`${here}#${hash}`, history.location.state);
        }
      } else if (written !== null) {
        written = null;
        const pushed = dashboardsWindow.getState().pushed;
        dashboardsWindow.setState({ pushed: false });
        if (pushed && history.canGoBack()) {
          history.back();
        } else if (parseDashboardsHash(history.location.hash) !== null) {
          history.replace(here, history.location.state);
        }
      }
    },
  );
}

function useDashboardsSignedIn() {
  const now = useAccount();
  const open = useDashboardsWindow((w) => w.open);
  const waiting = useDashboardsWindow((w) => w.waiting);
  createEffect(
    () => [open(), waiting(), now().kind] as const,
    ([isOpen, pending, kind]) => {
      if ((isOpen || pending) && kind === "checking") {
        void checkAccount();
      }
    },
  );
  createEffect(
    () => [now().kind, waiting(), open()] as const,
    ([kind, pending, isOpen]) => {
      if (kind === "in" && pending) {
        openDashboards(pending.tab, pending.org);
      }
      if (kind !== "in" && kind !== "checking" && isOpen) {
        closeDashboards();
      }
    },
  );
}

function OrgSelect(props: { org: number | null; onChange: (org: number | null) => void }) {
  const dir = useDirectory();
  const orgs = createMemo(() => dir().orgs ?? []);
  return (
    <Show when={orgs().length > 0}>
      <select
        class="input compact"
        aria-label="Organization"
        value={props.org ?? ""}
        onChange={(e) =>
          props.onChange(e.currentTarget.value === "" ? null : Number(e.currentTarget.value))
        }
      >
        <option value="">Personal</option>
        <For each={orgs()}>{(o) => <option value={o.id}>{o.name}</option>}</For>
      </select>
    </Show>
  );
}

function SessionDetail(props: {
  session: SessionRecord;
  onChanged: () => void;
  onOpen: (id: number) => void;
}) {
  const [name, setName] = createSignal(props.session.name ?? "");
  const [notes, setNotes] = createSignal(props.session.notes);
  const [estimate, setEstimate] = createSignal<{ sessionId: number; data: PinEstimate } | null>(
    null,
  );
  const [pendingNotesOnConfirm, setPendingNotesOnConfirm] = createSignal(false);
  const [estimateFailedForSave, setEstimateFailedForSave] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal<string | null>(null);
  let estSeq = 0;
  let lastSessionId = props.session.id;

  createEffect(
    () => props.session.id,
    (id) => {
      if (id === lastSessionId) {
        return;
      }
      lastSessionId = id;
      estSeq += 1;
      setName(props.session.name ?? "");
      setNotes(props.session.notes);
      setEstimate(null);
      setPendingNotesOnConfirm(false);
      setEstimateFailedForSave(false);
      setBusy(false);
      setErr(null);
    },
  );

  const requestEstimate = async (fromSave = false) => {
    const reqSessionId = props.session.id;
    const reqSeq = ++estSeq;
    setBusy(true);
    setErr(null);
    setEstimateFailedForSave(false);
    try {
      const est = await dashboardsApi.pinEstimate(reqSessionId);
      if (reqSeq !== estSeq || props.session.id !== reqSessionId) {
        return;
      }
      setPendingNotesOnConfirm(fromSave);
      setEstimate({ sessionId: reqSessionId, data: est });
    } catch (e) {
      if (reqSeq !== estSeq || props.session.id !== reqSessionId) {
        return;
      }
      setEstimate(null);
      setEstimateFailedForSave(fromSave);
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      if (reqSeq === estSeq && props.session.id === reqSessionId) {
        setBusy(false);
      }
    }
  };

  const saveMeta = async (skipEstimate = false) => {
    const trimmed = name().trim();
    const firstTimeNaming =
      props.session.name === null && props.session.pinStatus === "none" && trimmed !== "";
    if (!skipEstimate && firstTimeNaming) {
      await requestEstimate(true);
      return;
    }
    setBusy(true);
    setErr(null);
    setEstimateFailedForSave(false);
    try {
      await dashboardsApi.updateSession(props.session.id, {
        name: trimmed,
        notes: notes(),
        pin: false,
      });
      props.onChanged();
      flash("Session updated.");
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const confirmKeep = async (pin: boolean) => {
    const current = estimate();
    if (!current || current.sessionId !== props.session.id) {
      setEstimate(null);
      return;
    }
    const targetId = current.sessionId;
    const targetName = name().trim();
    const targetNotes = notes();
    const shouldPatchNotes = pendingNotesOnConfirm() || targetNotes !== props.session.notes;
    setBusy(true);
    setErr(null);
    try {
      if (shouldPatchNotes) {
        await dashboardsApi.updateSession(targetId, {
          notes: targetNotes,
          pin: false,
        });
      }
      const kept = await dashboardsApi.keepSession(targetId, {
        ...(targetName === "" ? {} : { name: targetName }),
        pin,
      });
      if (props.session.id === targetId) {
        setEstimate(null);
        setPendingNotesOnConfirm(false);
      }
      props.onChanged();
      if (kept.pinAttempt === "cap_exceeded" && kept.pinStatus === "pinned") {
        flash("Pin cap exceeded — kept existing pinned data.");
      } else if (kept.pinStatus === "cap_exceeded") {
        flash("Pin cap exceeded — session kept without pinning.");
      } else if (kept.pinStatus === "pinned") {
        flash("Session kept and pinned.");
      } else {
        flash("Session kept.");
      }
    } catch (e) {
      if (props.session.id === targetId) {
        setErr(e instanceof Error ? e.message : String(e));
      }
    } finally {
      if (props.session.id === targetId) {
        setBusy(false);
      }
    }
  };

  const onUnpin = async () => {
    setBusy(true);
    setErr(null);
    try {
      await dashboardsApi.unpinSession(props.session.id);
      props.onChanged();
      flash("Session unpinned.");
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const onDelete = async () => {
    setBusy(true);
    setErr(null);
    try {
      await dashboardsApi.deleteSession(props.session.id);
      props.onChanged();
      flash("Session deleted.");
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const copyLink = () => {
    const url = `${window.location.origin}/s/${props.session.id}`;
    void navigator.clipboard
      .writeText(url)
      .then(() => flash("Copied link to this session."))
      .catch(() => flash("Could not copy session link."));
  };

  return (
    <div class="dash-session-detail">
      <div class="dash-detail-head">
        <h3>
          {props.session.name || props.session.capture?.name || `Session #${props.session.id}`}
        </h3>
        <div class="actions">
          <button type="button" class="button" onClick={() => props.onOpen(props.session.id)}>
            Open
          </button>
          <button type="button" class="tb ghost" onClick={copyLink}>
            Copy link
          </button>
          <Show when={props.session.captureId && !props.session.capture?.deleted}>
            <a
              class="tb ghost"
              href={captureDownloadUrl(props.session.captureId as number)}
              download
            >
              Download .usprof
            </a>
          </Show>
        </div>
      </div>

      <div class="dash-detail-meta muted">
        <span>By {props.session.ownerUsername}</span>
        <Show when={props.session.capture}>
          {(cap) => (
            <span>
              · Capture ({cap().projectName}, {formatBytes(cap().byteLen)})
            </span>
          )}
        </Show>
        <Show when={props.session.keptAt !== null}>
          <span>· Kept</span>
        </Show>
        <Show when={props.session.sharedAt !== null}>
          <span>· Shared ({props.session.visitorCount} visitors)</span>
        </Show>
        <Show when={props.session.pinStatus === "pinned"}>
          <span>· Pinned ({formatBytes(props.session.pinnedBytes)})</span>
        </Show>
        <Show when={props.session.pinStatus === "cap_exceeded"}>
          <span>· Pin cap exceeded (data follows org retention)</span>
        </Show>
        <Show when={props.session.expiresAt}>{(exp) => <span>· Expires {day(exp())}</span>}</Show>
      </div>

      <Show when={props.session.isOwner}>
        <label class="dash-field">
          <span>Name</span>
          <input
            type="text"
            class="input"
            value={name()}
            placeholder="Name this session to keep it…"
            onInput={(e) => setName(e.currentTarget.value)}
          />
        </label>
        <label class="dash-field">
          <span>Notes</span>
          <textarea
            class="input dash-notes"
            rows={4}
            value={notes()}
            placeholder="Investigation notes…"
            onInput={(e) => setNotes(e.currentTarget.value)}
          />
        </label>
        <div class="actions">
          <button type="button" class="button" disabled={busy()} onClick={() => void saveMeta()}>
            Save changes
          </button>
          <Show when={props.session.pinStatus !== "pinned"}>
            <button
              type="button"
              class="tb ghost"
              disabled={busy()}
              onClick={() => void requestEstimate()}
            >
              Keep &amp; pin…
            </button>
          </Show>
          <Show when={props.session.pinStatus === "pinned"}>
            <button type="button" class="tb ghost" disabled={busy()} onClick={() => void onUnpin()}>
              Unpin
            </button>
          </Show>
          <button type="button" class="tb ghost" disabled={busy()} onClick={() => void onDelete()}>
            Delete
          </button>
        </div>
      </Show>

      <Show when={estimate()?.sessionId === props.session.id ? estimate()?.data : null}>
        {(est) => (
          <div class="dash-pin-confirm">
            <p>
              Pinning this session will copy <strong>{formatBytes(est().estimatedBytes)}</strong>{" "}
              across {est().machineCount} scoped machine{est().machineCount === 1 ? "" : "s"}.
            </p>
            <p class="muted">
              Organization pin quota: {formatBytes(est().orgPinnedBytes)} /{" "}
              {formatBytes(est().pinCapBytes)}.
              {est().wouldExceedCap
                ? " Exceeds pin cap — session will be kept without pinning."
                : ""}
            </p>
            <div class="actions">
              <button
                type="button"
                class="button"
                disabled={busy()}
                onClick={() => void confirmKeep(!est().wouldExceedCap)}
              >
                {est().wouldExceedCap ? "Keep without pinning" : "Confirm keep & pin"}
              </button>
              <button type="button" class="tb ghost" onClick={() => setEstimate(null)}>
                Cancel
              </button>
            </div>
          </div>
        )}
      </Show>

      <Show when={err()}>
        {(msg) => (
          <div class="dash-error-block">
            <p class="error" role="alert">
              {msg()}
            </p>
            <Show when={estimateFailedForSave()}>
              <div class="actions">
                <button
                  type="button"
                  class="button"
                  disabled={busy()}
                  onClick={() => void saveMeta(true)}
                >
                  Save name without pinning
                </button>
              </div>
            </Show>
          </div>
        )}
      </Show>
    </div>
  );
}

function OpenDashboardsWindow() {
  const navigate = useNavigate();
  const tab = useDashboardsWindow((w) => w.tab);
  const org = useDashboardsWindow((w) => w.org);
  const q = useDashboardsWindow((w) => w.q);
  const selectedSessionId = useDashboardsWindow((w) => w.selectedSessionId);

  const [rev, setRev] = createSignal(0);
  let dialog: HTMLDivElement | undefined;
  let searchInput: HTMLInputElement | undefined;
  useDialogFocus(() => dialog);

  const dashKey = createMemo(() => `d:${org() ?? "personal"}:${rev()}`);
  const sessKey = createMemo(() => `s:${org() ?? "personal"}:${rev()}`);

  const dashData = useLoaded<DashboardsListResponse>(
    () => dashboardsApi.listDashboards(org()),
    dashKey,
  );
  const sessData = useLoaded<SessionsListResponse>(
    () => dashboardsApi.listSessions(org()),
    sessKey,
  );
  const [cachedSessions, setCachedSessions] = createSignal<SessionsListResponse | null>(null);
  createEffect(
    () => org(),
    () => {
      setCachedSessions(null);
    },
  );
  createEffect(
    () => sessData.data(),
    (d) => {
      if (d) {
        setCachedSessions(d);
      }
    },
  );

  const nowSec = Math.floor(Date.now() / 1000);

  const filteredDashboards = createMemo(() => {
    const list = dashData.data()?.dashboards ?? [];
    const term = q().trim().toLowerCase();
    return term === ""
      ? list
      : list.filter(
          (d) => d.name.toLowerCase().includes(term) || d.description.toLowerCase().includes(term),
        );
  });

  const filteredSessions = createMemo(() => {
    const list = (sessData.data() ?? cachedSessions())?.sessions ?? [];
    const term = q().trim().toLowerCase();
    return term === ""
      ? list
      : list.filter(
          (s) =>
            (s.name ?? s.capture?.name ?? `Session #${s.id}`).toLowerCase().includes(term) ||
            s.ownerUsername.toLowerCase().includes(term) ||
            s.notes.toLowerCase().includes(term),
        );
  });

  const activeSession = createMemo(() => {
    const list = filteredSessions();
    const sel = selectedSessionId();
    return list.find((s) => s.id === sel) ?? list[0] ?? null;
  });

  const openDashboard = (record: DashboardRecord) => {
    const chosenOrg = org();
    const wasPushed = dashboardsWindow.getState().pushed;
    dashboardsWindow.setState({ open: false, waiting: null, pushed: false });
    const qs = chosenOrg === null ? "" : `?org=${chosenOrg}`;
    navigate(toHref(`/d/${encodeURIComponent(record.id)}${qs}`), { replace: wasPushed });
  };

  const openSession = (id: number) => {
    const wasPushed = dashboardsWindow.getState().pushed;
    dashboardsWindow.setState({ open: false, waiting: null, pushed: false });
    navigate(toHref(`/s/${id}`), { replace: wasPushed });
  };

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      closeDashboards();
    } else if (event.key === "/" && !isTextBox(document.activeElement)) {
      event.preventDefault();
      searchInput?.focus();
    } else if (event.key === "Enter" && !isTextBox(document.activeElement)) {
      if (tab() === "dashboards") {
        const first = filteredDashboards()[0];
        if (first) {
          event.preventDefault();
          openDashboard(first);
        }
      } else {
        const cur = activeSession();
        if (cur) {
          event.preventDefault();
          openSession(cur.id);
        }
      }
    }
  };

  const quota = createMemo(() =>
    tab() === "dashboards" ? dashData.data()?.storage : sessData.data()?.storage,
  );

  return (
    <div class="overlay machines-overlay">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions */}
      <div
        class="machines dash-window"
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
        role="dialog"
        aria-modal="true"
        aria-label="Dashboards"
        data-keys="escape"
        ref={(node) => {
          dialog = node;
        }}
        onKeyDown={onKeyDown}
      >
        <header class="machines-head">
          <h2>Dashboards</h2>
          <OrgSelect
            org={org()}
            onChange={(nextOrg) => dashboardsWindow.setState({ org: nextOrg })}
          />
          <button type="button" class="machines-close" aria-label="Close" onClick={closeDashboards}>
            ×
          </button>
        </header>

        <div class="machines-bar">
          <div class="machines-tabs" role="tablist" aria-label="Dashboard tabs">
            <button
              type="button"
              role="tab"
              aria-selected={tab() === "dashboards" ? "true" : "false"}
              class={`machines-tab ${tab() === "dashboards" ? "active" : ""}`}
              onClick={() => dashboardsWindow.setState({ tab: "dashboards" as DashboardsTab })}
            >
              Dashboards
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={tab() === "sessions" ? "true" : "false"}
              class={`machines-tab ${tab() === "sessions" ? "active" : ""}`}
              onClick={() => dashboardsWindow.setState({ tab: "sessions" as DashboardsTab })}
            >
              Sessions
            </button>
          </div>
          <Show when={quota()}>
            {(st) => (
              <span class="muted">
                Pinned: {formatBytes(st().pinnedBytes)} / {formatBytes(st().pinCapBytes)}
              </span>
            )}
          </Show>
        </div>

        <div class="machines-find">
          <input
            type="search"
            class="input"
            placeholder={tab() === "dashboards" ? "Filter dashboards (/)" : "Filter sessions (/)"}
            aria-label="Search dashboards or sessions"
            value={q()}
            ref={(el) => {
              searchInput = el;
            }}
            onInput={(e) => dashboardsWindow.setState({ q: e.currentTarget.value })}
          />
        </div>

        <div
          class="machines-view dash-body"
          role="tabpanel"
          aria-label={tab() === "dashboards" ? "Dashboards" : "Sessions"}
        >
          <Show when={tab() === "dashboards"}>
            <Show
              when={!dashData.error()}
              fallback={
                <div class="empty error" role="alert">
                  {dashData.error()}
                </div>
              }
            >
              <div class="dash-grid">
                <div class="dash-list">
                  <For each={filteredDashboards()}>
                    {(d) => (
                      <button
                        type="button"
                        class="dash-card"
                        aria-label={d.name}
                        data-testid={`dashboard-item-${d.id}`}
                        onClick={() => openDashboard(d)}
                      >
                        <div class="dash-card-head">
                          <strong>{d.name}</strong>
                          <Show when={d.builtin}>
                            <span class="scope-badge">built-in</span>
                          </Show>
                        </div>
                        <Show when={d.description !== ""}>
                          <p class="muted">{d.description}</p>
                        </Show>
                      </button>
                    )}
                  </For>
                </div>

                <Show when={(dashData.data()?.recentSessions.length ?? 0) > 0}>
                  <aside class="dash-recent">
                    <h4>Recent sessions</h4>
                    <For each={dashData.data()?.recentSessions ?? []}>
                      {(s) => (
                        <button
                          type="button"
                          class="dash-recent-row"
                          onClick={() => openSession(s.id)}
                        >
                          <span>{s.name || s.capture?.name || `Session #${s.id}`}</span>
                          <span class="muted">{ago(s.lastOpenedAt, nowSec)}</span>
                        </button>
                      )}
                    </For>
                  </aside>
                </Show>
              </div>
            </Show>
          </Show>

          <Show when={tab() === "sessions"}>
            <Show
              when={!sessData.error()}
              fallback={
                <div class="empty error" role="alert">
                  {sessData.error()}
                </div>
              }
            >
              <Show
                when={filteredSessions().length > 0}
                fallback={
                  <div class="empty muted">
                    No sessions yet. Open a dashboard and explore to start a session.
                  </div>
                }
              >
                <div class="dash-sessions-split">
                  <div class="dash-sessions-list">
                    <For each={filteredSessions()}>
                      {(s) => (
                        <button
                          type="button"
                          class={`dash-session-item ${activeSession()?.id === s.id ? "active" : ""}`}
                          onClick={() => dashboardsWindow.setState({ selectedSessionId: s.id })}
                          onDblClick={() => openSession(s.id)}
                        >
                          <div class="dash-card-head">
                            <strong>{s.name || s.capture?.name || `Session #${s.id}`}</strong>
                            <Show when={s.captureId !== null && s.captureId !== undefined}>
                              <span class="scope-badge">capture</span>
                            </Show>
                            <Show when={s.pinStatus === "pinned"}>
                              <span class="scope-badge scope-badge-pin">pinned</span>
                            </Show>
                            <Show when={s.sharedAt !== null}>
                              <span class="scope-badge">shared</span>
                            </Show>
                          </div>
                          <span class="muted">
                            {s.ownerUsername} · {ago(s.lastOpenedAt, nowSec)}
                          </span>
                        </button>
                      )}
                    </For>
                  </div>
                  <Show when={activeSession()}>
                    {(sel) => (
                      <SessionDetail
                        session={sel()}
                        onChanged={() => setRev((r) => r + 1)}
                        onOpen={openSession}
                      />
                    )}
                  </Show>
                </div>
              </Show>
            </Show>
          </Show>
        </div>
      </div>
    </div>
  );
}

function SaveDashboardDialog() {
  const navigate = useNavigate();
  const [name, setName] = createSignal("");
  const [notes, setNotes] = createSignal("");
  const [projectId, setProjectId] = createSignal<string>("");
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal<string | null>(null);
  let dialog: HTMLDivElement | undefined;
  useDialogFocus(() => dialog);

  const cur = activeView.getState();
  const projects = createMemo(() => cur.dimensions?.projects ?? []);

  const onSubmit = async (event: SubmitEvent) => {
    event.preventDefault();
    if (name().trim() === "") {
      setErr("Dashboard name is required.");
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const savedLayout = JSON.parse(saveLayout(workbench.getState().layout)) as unknown;
      const created = await dashboardsApi.createDashboard({
        org_id: cur.orgId,
        project_id: projectId() === "" ? null : Number(projectId()),
        name: name().trim(),
        layout: savedLayout,
        scope: cur.scope,
        notes: notes(),
      });
      closeSaveDashboardDialog();
      flash(`Saved dashboard "${created.name}".`);
      navigate(toHref(`/d/${encodeURIComponent(created.id)}`));
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="overlay machines-overlay">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions */}
      <div
        class="machines dash-save-dialog"
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
        role="dialog"
        aria-modal="true"
        aria-label="Save as dashboard"
        data-keys="escape"
        ref={(node) => {
          dialog = node;
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            closeSaveDashboardDialog();
          }
        }}
      >
        <header class="machines-head">
          <h2>Save as dashboard</h2>
          <button
            type="button"
            class="machines-close"
            aria-label="Close"
            onClick={closeSaveDashboardDialog}
          >
            ×
          </button>
        </header>
        <form class="dash-save-form" onSubmit={(e) => void onSubmit(e)}>
          <label class="dash-field">
            <span>Name</span>
            <input
              type="text"
              class="input"
              required
              value={name()}
              placeholder="e.g. Checkout latency"
              onInput={(e) => setName(e.currentTarget.value)}
            />
          </label>
          <Show when={projects().length > 0}>
            <label class="dash-field">
              <span>Project</span>
              <select
                class="input"
                value={projectId()}
                onChange={(e) => setProjectId(e.currentTarget.value)}
              >
                <option value="">Organization-wide (owners only)</option>
                <For each={projects()}>{(p) => <option value={p.id}>{p.name}</option>}</For>
              </select>
            </label>
          </Show>
          <label class="dash-field">
            <span>Description</span>
            <textarea
              class="input dash-notes"
              rows={3}
              value={notes()}
              onInput={(e) => setNotes(e.currentTarget.value)}
            />
          </label>
          <Show when={err()}>
            {(msg) => (
              <p class="error" role="alert">
                {msg()}
              </p>
            )}
          </Show>
          <div class="actions">
            <button type="submit" class="button" disabled={busy()}>
              Save dashboard
            </button>
            <button type="button" class="tb ghost" onClick={closeSaveDashboardDialog}>
              Cancel
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

export function DashboardsWindow() {
  if (!standalone) {
    return null;
  }
  return <HostedDashboards />;
}

function HostedDashboards() {
  useDashboardsUrl();
  useDashboardsSignedIn();
  const open = useDashboardsWindow((w) => w.open);
  const saveOpen = useDashboardsWindow((w) => w.saveDialogOpen);
  return (
    <>
      <Show when={open()}>
        <OpenDashboardsWindow />
      </Show>
      <Show when={saveOpen()}>
        <SaveDashboardDialog />
      </Show>
    </>
  );
}
