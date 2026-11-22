import { createEffect, createMemo, createSignal, For, Show } from "solid-js";

import {
  type CreatedProjectTokenResponse,
  dataPolicyApi,
  formatByteSize,
  type SymbolBuildItem,
  type SymbolsResponse,
  type SymbolStatus,
} from "../../dataPolicy";
import { flash } from "../../tab";

type StatusFilter = "all" | "missing" | "available";

function statusLabel(status: SymbolStatus): string {
  switch (status) {
    case "missing":
      return "Missing";
    case "uploaded_ci":
      return "CI upload";
    case "uploaded_agent":
      return "Agent upload";
    case "public_cache":
      return "Public cache";
    case "symbolized_on_host":
      return "On host";
  }
}

function shortBuildId(id: string): string {
  return id.length > 16 ? `${id.slice(0, 12)}…` : id;
}

function baseName(path: string): string {
  if (!path) {
    return "—";
  }
  const idx = path.lastIndexOf("/");
  return idx >= 0 ? path.slice(idx + 1) : path;
}

function formatDate(ms: number | null): string {
  if (ms === null) {
    return "—";
  }
  return new Date(ms).toLocaleString();
}

async function copyText(text: string, label: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    flash(`Copied ${label}.`);
  } catch {
    flash(`Select and copy the ${label} manually.`);
  }
}

export function SymbolsView(props: {
  org: number | null;
  searchRef?: (el: HTMLInputElement) => void;
}) {
  const [data, setData] = createSignal<SymbolsResponse | null>(null);
  const [loading, setLoading] = createSignal(true);
  const [err, setErr] = createSignal<string | null>(null);
  const [query, setQuery] = createSignal("");
  const [statusFilter, setStatusFilter] = createSignal<StatusFilter>("all");
  const [hours, setHours] = createSignal<number>(24);
  const [rev, setRev] = createSignal(0);

  const [expandedCmd, setExpandedCmd] = createSignal<string | null>(null);
  const [tokenProjectId, setTokenProjectId] = createSignal("");
  const [tokenName, setTokenName] = createSignal("");
  const [createdToken, setCreatedToken] = createSignal<CreatedProjectTokenResponse | null>(null);
  const [busy, setBusy] = createSignal(false);

  let seq = 0;
  createEffect(
    () => [props.org, hours(), rev()] as const,
    ([org, windowHours]) => {
      const id = ++seq;
      setLoading(true);
      setErr(null);
      void (async () => {
        try {
          const res = await dataPolicyApi.symbols(org, windowHours);
          if (id === seq) {
            setData(res);
            const firstWritable = res.projects.find((p) => p.canWrite);
            if (!tokenProjectId() && firstWritable) {
              setTokenProjectId(String(firstWritable.id));
            }
          }
        } catch (error: unknown) {
          if (id === seq) {
            setErr(error instanceof Error ? error.message : String(error));
          }
        } finally {
          if (id === seq) {
            setLoading(false);
          }
        }
      })();
    },
  );

  const writableProjects = createMemo(() => (data()?.projects ?? []).filter((p) => p.canWrite));
  const selectedProjectId = createMemo(() => {
    const cur = tokenProjectId();
    if (cur) {
      return cur;
    }
    const first = writableProjects()[0];
    return first ? String(first.id) : "";
  });

  const filteredBuilds = createMemo(() => {
    const q = query().trim().toLowerCase();
    const filter = statusFilter();
    const list = data()?.builds ?? [];
    return list.filter((b) => {
      if (filter === "missing" && b.status !== "missing") {
        return false;
      }
      if (filter === "available" && b.status === "missing") {
        return false;
      }
      if (!q) {
        return true;
      }
      return (
        b.buildId.toLowerCase().includes(q) ||
        b.serviceName.toLowerCase().includes(q) ||
        b.projectName.toLowerCase().includes(q) ||
        b.modulePath.toLowerCase().includes(q) ||
        (b.gitCommit ?? "").toLowerCase().includes(q)
      );
    });
  });

  const onCreateToken = async (event: SubmitEvent) => {
    event.preventDefault();
    const res = data();
    const pid = selectedProjectId();
    if (!res || !pid || !tokenName().trim()) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const created = await dataPolicyApi.createSymbolToken({
        orgId: res.orgId,
        projectId: Number(pid),
        name: tokenName().trim(),
      });
      setCreatedToken(created);
      setTokenName("");
      flash(`Created upload token ${created.name}.`);
      setRev((r) => r + 1);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const onRevokeToken = async (id: number, name: string) => {
    setBusy(true);
    setErr(null);
    try {
      await dataPolicyApi.revokeSymbolToken(id);
      if (createdToken()?.id === id) {
        setCreatedToken(null);
      }
      flash(`Revoked upload token ${name}.`);
      setRev((r) => r + 1);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const buildCommandFor = (item: SymbolBuildItem): string => {
    const tok = createdToken();
    if (tok && tok.projectId === item.projectId) {
      return item.uploadCommand.replace("<token>", tok.token);
    }
    return item.uploadCommand;
  };

  return (
    <div class="data-panel">
      <Show when={err()}>
        {(msg) => (
          <p class="error" role="alert">
            {msg()}
          </p>
        )}
      </Show>

      <Show when={data()} fallback={<p class="muted">{loading() ? "Loading symbols…" : ""}</p>}>
        {(res) => (
          <>
            <Show when={res().missingSummary.missingBuildCount > 0}>
              <output class="data-banner-warn" data-testid="symbols-missing-banner">
                <strong>
                  {res().missingSummary.missingBuildCount}{" "}
                  {res().missingSummary.missingBuildCount === 1 ? "build is" : "builds are"} missing
                  debug symbols
                </strong>{" "}
                ({res().missingSummary.missingTimePercent.toFixed(1)}% of recorded CPU time in the
                last {res().hours >= 48 ? `${Math.round(res().hours / 24)}d` : `${res().hours}h`}).
                Upload debug files from CI to resolve addresses immediately without re-recording.
              </output>
            </Show>

            <div class="data-toolbar">
              <input
                type="search"
                class="input compact data-search"
                placeholder="Filter build ID, service, binary, commit…"
                aria-label="Filter symbols"
                value={query()}
                ref={(el) => props.searchRef?.(el)}
                onInput={(e) => setQuery(e.currentTarget.value)}
              />
              <div class="data-actions-cell">
                <div class="data-seg">
                  <button
                    type="button"
                    class={`data-seg-btn ${statusFilter() === "all" ? "active" : ""}`}
                    onClick={() => setStatusFilter("all")}
                  >
                    All
                  </button>
                  <button
                    type="button"
                    class={`data-seg-btn ${statusFilter() === "missing" ? "active" : ""}`}
                    onClick={() => setStatusFilter("missing")}
                  >
                    Missing
                  </button>
                  <button
                    type="button"
                    class={`data-seg-btn ${statusFilter() === "available" ? "active" : ""}`}
                    onClick={() => setStatusFilter("available")}
                  >
                    Available
                  </button>
                </div>
                <div class="data-seg">
                  <button
                    type="button"
                    class={`data-seg-btn ${hours() === 24 ? "active" : ""}`}
                    onClick={() => setHours(24)}
                  >
                    24h
                  </button>
                  <button
                    type="button"
                    class={`data-seg-btn ${hours() === 168 ? "active" : ""}`}
                    onClick={() => setHours(168)}
                  >
                    7d
                  </button>
                </div>
                <span class="muted data-count">
                  {filteredBuilds().length} {filteredBuilds().length === 1 ? "build" : "builds"}
                </span>
              </div>
            </div>

            <div class="data-table-wrap">
              <table class="data-table" data-testid="symbols-builds-table">
                <thead>
                  <tr>
                    <th>Status</th>
                    <th>Service / Binary</th>
                    <th>Project</th>
                    <th>Build ID</th>
                    <th>Commit</th>
                    <th class="num">CPU Share</th>
                    <th class="num">Size</th>
                    <th>Upload</th>
                  </tr>
                </thead>
                <tbody>
                  <Show
                    when={filteredBuilds().length > 0}
                    fallback={
                      <tr>
                        <td colspan="8" class="muted">
                          No builds match the current filter. Recorded native binaries appear here
                          automatically as agents report profiles.
                        </td>
                      </tr>
                    }
                  >
                    <For each={filteredBuilds()}>
                      {(build) => {
                        const rowKey = `${build.projectId}:${build.buildId}`;
                        const cmd = () => buildCommandFor(build);
                        return (
                          <>
                            <tr>
                              <td>
                                <span
                                  class={`data-pill ${build.status === "missing" ? "warn" : "ok"}`}
                                >
                                  {statusLabel(build.status)}
                                </span>
                              </td>
                              <td>
                                <strong>{build.serviceName || baseName(build.modulePath)}</strong>
                                <Show when={build.modulePath}>
                                  <div class="muted mono" title={build.modulePath}>
                                    {baseName(build.modulePath)}
                                  </div>
                                </Show>
                              </td>
                              <td>{build.projectName || "—"}</td>
                              <td class="mono" title={build.buildId}>
                                {shortBuildId(build.buildId)}
                              </td>
                              <td class="mono">{build.gitCommit ?? "—"}</td>
                              <td class="num">{build.sharePercent.toFixed(1)}%</td>
                              <td class="num">{formatByteSize(build.byteLen)}</td>
                              <td class="data-actions-cell">
                                <button
                                  type="button"
                                  class="tb"
                                  onClick={() => void copyText(cmd(), "upload command")}
                                >
                                  Copy command
                                </button>
                                <button
                                  type="button"
                                  class="tb ghost"
                                  onClick={() =>
                                    setExpandedCmd((cur) => (cur === rowKey ? null : rowKey))
                                  }
                                >
                                  {expandedCmd() === rowKey ? "Hide" : "Show"}
                                </button>
                              </td>
                            </tr>
                            <Show when={expandedCmd() === rowKey}>
                              <tr>
                                <td colspan="8">
                                  <code class="mono" data-testid="symbols-upload-cmd">
                                    {cmd()}
                                  </code>
                                </td>
                              </tr>
                            </Show>
                          </>
                        );
                      }}
                    </For>
                  </Show>
                </tbody>
              </table>
            </div>

            <Show when={writableProjects().length > 0}>
              <section class="data-section" data-testid="symbols-tokens-section">
                <div class="data-section-head">
                  <h3>Project upload tokens</h3>
                  <span class="muted">
                    Scoped to <code>symbols:upload</code> for CI pipelines (<code>msp_…</code>)
                  </span>
                </div>

                <Show when={createdToken()}>
                  {(tok) => (
                    <div class="data-card-form" data-testid="symbols-created-token">
                      <div class="data-section-head">
                        <h4>
                          Token created for {tok().projectName} ({tok().name})
                        </h4>
                        <button
                          type="button"
                          class="tb ghost"
                          onClick={() => setCreatedToken(null)}
                        >
                          Dismiss
                        </button>
                      </div>
                      <p class="muted" style={{ margin: "0" }}>
                        Copy this token now — it will not be shown again.
                      </p>
                      <div class="data-actions-cell">
                        <code class="mono" data-testid="symbols-token-secret">
                          {tok().token}
                        </code>
                        <button
                          type="button"
                          class="tb"
                          onClick={() => void copyText(tok().token, "upload token")}
                        >
                          Copy token
                        </button>
                      </div>
                    </div>
                  )}
                </Show>

                <Show when={res().tokens.length > 0}>
                  <div class="data-table-wrap">
                    <table class="data-table">
                      <thead>
                        <tr>
                          <th>Name</th>
                          <th>Project</th>
                          <th>Prefix</th>
                          <th>Created</th>
                          <th>Last used</th>
                          <th class="num">Action</th>
                        </tr>
                      </thead>
                      <tbody>
                        <For each={res().tokens}>
                          {(tok) => (
                            <tr>
                              <td>
                                <strong>{tok.name}</strong>
                              </td>
                              <td>{tok.projectName}</td>
                              <td class="mono">{tok.tokenPrefix}…</td>
                              <td class="muted">{formatDate(tok.createdAt)}</td>
                              <td class="muted">{formatDate(tok.lastUsedAt)}</td>
                              <td class="num">
                                <button
                                  type="button"
                                  class="tb ghost"
                                  disabled={busy()}
                                  onClick={() => void onRevokeToken(tok.id, tok.name)}
                                >
                                  Revoke
                                </button>
                              </td>
                            </tr>
                          )}
                        </For>
                      </tbody>
                    </table>
                  </div>
                </Show>

                <form class="data-inline-form" onSubmit={(e) => void onCreateToken(e)}>
                  <label class="data-inline-field">
                    <span>Project</span>
                    <select
                      class="input compact"
                      aria-label="Upload token project"
                      value={selectedProjectId()}
                      onChange={(e) => setTokenProjectId(e.currentTarget.value)}
                      required
                    >
                      <For each={writableProjects()}>
                        {(p) => (
                          <option
                            value={String(p.id)}
                            selected={String(p.id) === selectedProjectId()}
                          >
                            {p.name}
                          </option>
                        )}
                      </For>
                    </select>
                  </label>
                  <label class="data-inline-field">
                    <span>Token name</span>
                    <input
                      type="text"
                      class="input compact"
                      placeholder="github-actions-release"
                      aria-label="Upload token name"
                      value={tokenName()}
                      onInput={(e) => setTokenName(e.currentTarget.value)}
                      required
                    />
                  </label>
                  <button type="submit" class="button compact" disabled={busy()}>
                    Create upload token
                  </button>
                </form>
              </section>
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}
