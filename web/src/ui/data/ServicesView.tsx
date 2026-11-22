import { createEffect, createMemo, createSignal, For, Show } from "solid-js";

import { dataPolicyApi, type ServicesResponse, type UnassignedProgramItem } from "../../dataPolicy";
import { flash } from "../../tab";

export function ServicesView(props: {
  org: number | null;
  searchRef?: (el: HTMLInputElement) => void;
}) {
  const [data, setData] = createSignal<ServicesResponse | null>(null);
  const [loading, setLoading] = createSignal(true);
  const [err, setErr] = createSignal<string | null>(null);
  const [query, setQuery] = createSignal("");
  const [rev, setRev] = createSignal(0);

  const [kind, setKind] = createSignal("process");
  const [action, setAction] = createSignal("record");
  const [pattern, setPattern] = createSignal("");
  const [projectId, setProjectId] = createSignal("");
  const [serviceName, setServiceName] = createSignal("");
  const [busy, setBusy] = createSignal(false);

  const [assigning, setAssigning] = createSignal<UnassignedProgramItem | null>(null);
  const [assignProjectId, setAssignProjectId] = createSignal("");
  const [assignServiceName, setAssignServiceName] = createSignal("");

  let seq = 0;
  createEffect(
    () => [props.org, rev()] as const,
    ([org]) => {
      const id = ++seq;
      setLoading(true);
      setErr(null);
      void (async () => {
        try {
          const res = await dataPolicyApi.services(org);
          if (id === seq) {
            setData(res);
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

  const assignableProjects = createMemo(() =>
    (data()?.projects ?? []).filter((p) => !p.isUnassigned),
  );

  const filteredServices = createMemo(() => {
    const q = query().trim().toLowerCase();
    const list = data()?.services ?? [];
    if (!q) {
      return list;
    }
    return list.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        s.projectName.toLowerCase().includes(q) ||
        s.matchedBy.toLowerCase().includes(q),
    );
  });

  const onCreateRule = async (event: SubmitEvent) => {
    event.preventDefault();
    const res = data();
    if (!res || !pattern().trim()) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await dataPolicyApi.createRule({
        orgId: res.orgId,
        kind: kind(),
        action: action(),
        pattern: pattern().trim(),
        projectId: action() === "exclude" || !projectId() ? null : Number(projectId()),
        serviceName: action() === "exclude" || !serviceName().trim() ? null : serviceName().trim(),
      });
      setPattern("");
      setServiceName("");
      flash("Recording rule added.");
      setRev((r) => r + 1);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const onDeleteRule = async (id: number) => {
    setBusy(true);
    setErr(null);
    try {
      await dataPolicyApi.deleteRule(id);
      flash("Recording rule removed.");
      setRev((r) => r + 1);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const startAssign = (item: UnassignedProgramItem) => {
    setAssigning(item);
    setAssignServiceName(item.program);
    const first = assignableProjects()[0];
    setAssignProjectId(first ? String(first.id) : "");
  };

  const onAssign = async (event: SubmitEvent) => {
    event.preventDefault();
    const res = data();
    const item = assigning();
    if (!res || !item || !assignProjectId() || !assignServiceName().trim()) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await dataPolicyApi.assignUnassigned({
        orgId: res.orgId,
        program: item.program,
        projectId: Number(assignProjectId()),
        serviceName: assignServiceName().trim(),
        buildId: item.buildId,
      });
      setAssigning(null);
      flash(`Assigned ${item.program} to project.`);
      setRev((r) => r + 1);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const onStopUnassigned = async (program: string) => {
    const res = data();
    if (!res) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await dataPolicyApi.stopUnassigned({ orgId: res.orgId, program });
      flash(`Added exclude rule for ${program}.`);
      setRev((r) => r + 1);
    } catch (e: unknown) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
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

      <Show when={data()} fallback={<p class="muted">{loading() ? "Loading services…" : ""}</p>}>
        {(res) => (
          <>
            <div class="data-toolbar">
              <input
                type="search"
                class="input compact data-search"
                placeholder="Filter services, projects, or match rules…"
                aria-label="Filter services"
                value={query()}
                ref={(el) => props.searchRef?.(el)}
                onInput={(e) => setQuery(e.currentTarget.value)}
              />
              <span class="muted data-count">
                {filteredServices().length}{" "}
                {filteredServices().length === 1 ? "service" : "services"}
              </span>
            </div>

            <div class="data-table-wrap">
              <table class="data-table">
                <thead>
                  <tr>
                    <th>Service</th>
                    <th>Project</th>
                    <th>Matched by</th>
                    <th class="num">Machines</th>
                    <th class="num">Processes</th>
                    <th>Versions running</th>
                  </tr>
                </thead>
                <tbody>
                  <Show
                    when={filteredServices().length > 0}
                    fallback={
                      <tr>
                        <td colspan="6" class="muted">
                          No recorded services yet. Mark processes with pod annotations,
                          MICROSCOPE_PROJECT, or a recording rule below.
                        </td>
                      </tr>
                    }
                  >
                    <For each={filteredServices()}>
                      {(svc) => (
                        <tr>
                          <td>
                            <strong>{svc.name}</strong>
                          </td>
                          <td>{svc.projectName}</td>
                          <td class="muted">{svc.matchedBy || "—"}</td>
                          <td class="num">{svc.machines}</td>
                          <td class="num">{svc.processesNow}</td>
                          <td class="mono">
                            {svc.versionsRunning.length > 0 ? svc.versionsRunning.join(", ") : "—"}
                          </td>
                        </tr>
                      )}
                    </For>
                  </Show>
                </tbody>
              </table>
            </div>

            <Show when={res().isOwner && res().unassigned.length > 0}>
              <section class="data-section data-unassigned">
                <output class="data-banner-warn">
                  <strong>Unassigned programs</strong> — visible only to organization owners. Assign
                  each program to a project or stop recording it.
                </output>
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Program</th>
                      <th class="num">Machines</th>
                      <th class="num">Processes</th>
                      <th>Matched by</th>
                      <th>Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={res().unassigned}>
                      {(item) => (
                        <tr>
                          <td class="mono">{item.program}</td>
                          <td class="num">{item.machines}</td>
                          <td class="num">{item.processesNow}</td>
                          <td class="muted">{item.matchedBy || "—"}</td>
                          <td class="data-actions-cell">
                            <button
                              type="button"
                              class="tb"
                              disabled={busy()}
                              onClick={() => startAssign(item)}
                            >
                              Assign to a project
                            </button>
                            <button
                              type="button"
                              class="tb ghost"
                              disabled={busy()}
                              onClick={() => void onStopUnassigned(item.program)}
                            >
                              Stop recording
                            </button>
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>

                <Show when={assigning()}>
                  {(item) => (
                    <form class="data-inline-form" onSubmit={(e) => void onAssign(e)}>
                      <strong>Assign {item().program}</strong>
                      <label class="data-inline-field">
                        <span>Project</span>
                        <select
                          class="input compact"
                          value={assignProjectId()}
                          onChange={(e) => setAssignProjectId(e.currentTarget.value)}
                          required
                        >
                          <For each={assignableProjects()}>
                            {(p) => <option value={p.id}>{p.name}</option>}
                          </For>
                        </select>
                      </label>
                      <label class="data-inline-field">
                        <span>Service name</span>
                        <input
                          type="text"
                          class="input compact"
                          value={assignServiceName()}
                          onInput={(e) => setAssignServiceName(e.currentTarget.value)}
                          required
                        />
                      </label>
                      <button type="submit" class="button compact" disabled={busy()}>
                        Assign
                      </button>
                      <button type="button" class="tb ghost" onClick={() => setAssigning(null)}>
                        Cancel
                      </button>
                    </form>
                  )}
                </Show>
              </section>
            </Show>

            <section class="data-section">
              <div class="data-section-head">
                <h3>Recording rules</h3>
                <span class="muted">
                  Applied by agents unless <code>allow_remote_rules = false</code> in agent.toml
                </span>
              </div>

              <Show when={res().rules.length > 0}>
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Action</th>
                      <th>Match</th>
                      <th>Pattern</th>
                      <th>Project</th>
                      <th>Service</th>
                      <Show when={res().isOwner}>
                        <th class="num">Action</th>
                      </Show>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={res().rules}>
                      {(rule) => (
                        <tr>
                          <td>
                            <span class={`data-pill ${rule.action === "exclude" ? "warn" : "ok"}`}>
                              {rule.action}
                            </span>
                          </td>
                          <td>{rule.kind}</td>
                          <td class="mono">{rule.pattern}</td>
                          <td>
                            {rule.projectName ?? (rule.action === "exclude" ? "—" : "Unassigned")}
                          </td>
                          <td>{rule.serviceName ?? "—"}</td>
                          <Show when={res().isOwner}>
                            <td class="num">
                              <button
                                type="button"
                                class="tb ghost"
                                disabled={busy()}
                                onClick={() => void onDeleteRule(rule.id)}
                              >
                                Remove
                              </button>
                            </td>
                          </Show>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </Show>

              <Show when={res().isOwner}>
                <form class="data-inline-form" onSubmit={(e) => void onCreateRule(e)}>
                  <select
                    class="input compact"
                    aria-label="Rule action"
                    value={action()}
                    onChange={(e) => setAction(e.currentTarget.value)}
                  >
                    <option value="record">Record</option>
                    <option value="exclude">Exclude</option>
                  </select>
                  <select
                    class="input compact"
                    aria-label="Rule kind"
                    value={kind()}
                    onChange={(e) => setKind(e.currentTarget.value)}
                  >
                    <option value="process">process</option>
                    <option value="unit">systemd unit</option>
                    <option value="container">container</option>
                    <option value="image">image</option>
                  </select>
                  <input
                    type="text"
                    class="input compact"
                    aria-label="Rule pattern"
                    placeholder="Pattern (e.g. checkout-*)"
                    value={pattern()}
                    onInput={(e) => setPattern(e.currentTarget.value)}
                    required
                  />
                  <Show when={action() === "record"}>
                    <select
                      class="input compact"
                      aria-label="Target project"
                      value={projectId()}
                      onChange={(e) => setProjectId(e.currentTarget.value)}
                    >
                      <option value="">Unassigned</option>
                      <For each={assignableProjects()}>
                        {(p) => <option value={p.id}>{p.name}</option>}
                      </For>
                    </select>
                    <input
                      type="text"
                      class="input compact"
                      aria-label="Service name"
                      placeholder="Service name (optional)"
                      value={serviceName()}
                      onInput={(e) => setServiceName(e.currentTarget.value)}
                    />
                  </Show>
                  <button type="submit" class="button compact" disabled={busy()}>
                    Add rule
                  </button>
                </form>
              </Show>
            </section>
          </>
        )}
      </Show>
    </div>
  );
}
