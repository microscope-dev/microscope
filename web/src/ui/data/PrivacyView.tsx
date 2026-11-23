import { createEffect, createSignal, For, Show } from "solid-js";

import { dataPolicyApi, type PreviewResponse, type PrivacyResponse } from "../../dataPolicy";
import { flash } from "../../tab";

const ACTIONS = ["redact", "hash", "mask", "drop-field", "drop-record"] as const;

export function PrivacyView(props: { org: number | null }) {
  const [data, setData] = createSignal<PrivacyResponse | null>(null);
  const [hours, setHours] = createSignal(24);
  const [loading, setLoading] = createSignal(true);
  const [err, setErr] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [rev, setRev] = createSignal(0);

  const [ruleName, setRuleName] = createSignal("");
  const [rulePattern, setRulePattern] = createSignal("");
  const [ruleAction, setRuleAction] = createSignal("redact");
  const [ruleFields, setRuleFields] = createSignal<string[]>(["syscall.string"]);

  const [previewField, setPreviewField] = createSignal("syscall.string");
  const [previewText, setPreviewText] = createSignal("");
  const [previewResult, setPreviewResult] = createSignal<PreviewResponse | null>(null);

  let seq = 0;
  let lastOrg: number | null | undefined;
  createEffect(
    () => [props.org, hours(), rev()] as const,
    ([org, h]) => {
      const id = ++seq;
      if (lastOrg !== undefined && lastOrg !== org) {
        setData(null);
        setPreviewResult(null);
      }
      lastOrg = org;
      setLoading(true);
      setErr(null);
      void (async () => {
        try {
          const res = await dataPolicyApi.privacy(org, h);
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

  const toggleBuiltin = async (name: string, enabled: boolean) => {
    const res = data();
    if (!res || !res.isOwner) {
      return;
    }
    const next = new Set(res.builtins.filter((b) => !b.defaultOn && b.enabled).map((b) => b.name));
    if (enabled) {
      next.add(name);
    } else {
      next.delete(name);
    }
    setBusy(true);
    setErr(null);
    try {
      await dataPolicyApi.setBuiltins({ orgId: res.orgId, enabledBuiltins: [...next] });
      setRev((r) => r + 1);
    } catch (e: unknown) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const toggleField = (field: string) => {
    setRuleFields((prev) =>
      prev.includes(field) ? prev.filter((f) => f !== field) : [...prev, field],
    );
  };

  const onCreateRule = async (event: SubmitEvent) => {
    event.preventDefault();
    const res = data();
    if (!res || !ruleName().trim() || !rulePattern().trim() || ruleFields().length === 0) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await dataPolicyApi.createSanitizerRule({
        orgId: res.orgId,
        name: ruleName().trim(),
        fields: ruleFields(),
        pattern: rulePattern(),
        action: ruleAction(),
      });
      setRuleName("");
      setRulePattern("");
      flash("Sanitizer rule added.");
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
      await dataPolicyApi.deleteSanitizerRule(id);
      flash("Sanitizer rule removed.");
      setRev((r) => r + 1);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const onPreview = async (event: SubmitEvent) => {
    event.preventDefault();
    const res = data();
    if (!res) {
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      const draftRule =
        rulePattern().trim() && ruleFields().length > 0
          ? {
              name: ruleName().trim() || "draft",
              fields: ruleFields(),
              pattern: rulePattern(),
              action: ruleAction(),
            }
          : null;
      const out = await dataPolicyApi.previewPrivacy({
        orgId: res.orgId,
        field: previewField(),
        text: previewText(),
        draftRule,
      });
      setPreviewResult(out);
    } catch (error: unknown) {
      setErr(error instanceof Error ? error.message : String(error));
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

      <Show
        when={data()}
        fallback={<p class="muted">{loading() ? "Loading privacy settings…" : ""}</p>}
      >
        {(res) => (
          <>
            <section class="data-section">
              <div class="data-section-head">
                <h3>Built-in sanitizers</h3>
                <span class="muted">
                  Default built-ins always run on agents and ingest; they can only be disabled in
                  agent.toml.
                </span>
              </div>
              <table class="data-table">
                <thead>
                  <tr>
                    <th>Built-in</th>
                    <th>Action</th>
                    <th>Fields</th>
                    <th>Status</th>
                  </tr>
                </thead>
                <tbody>
                  <For each={res().builtins}>
                    {(b) => (
                      <tr>
                        <td>
                          <strong>{b.name}</strong>
                        </td>
                        <td class="mono">{b.action}</td>
                        <td class="muted">{b.fields.join(", ")}</td>
                        <td aria-label="Status">
                          <Show
                            when={!b.locked}
                            fallback={<span class="data-pill ok">Always on</span>}
                          >
                            <label class="data-toggle">
                              <input
                                type="checkbox"
                                checked={b.enabled}
                                disabled={!res().isOwner || busy()}
                                onChange={(e) =>
                                  void toggleBuiltin(b.name, e.currentTarget.checked)
                                }
                              />
                              <span>{b.enabled ? "On" : "Off"}</span>
                            </label>
                          </Show>
                        </td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </section>

            <section class="data-section">
              <div class="data-section-head">
                <h3>Custom rules</h3>
              </div>
              <Show
                when={res().rules.length > 0}
                fallback={<p class="muted">No custom sanitizer rules yet.</p>}
              >
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Fields</th>
                      <th>Pattern</th>
                      <th>Action</th>
                      <th>Scope</th>
                      <Show when={res().isOwner}>
                        <th class="num">Action</th>
                      </Show>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={res().rules}>
                      {(r) => (
                        <tr>
                          <td>
                            <strong>{r.name}</strong>
                          </td>
                          <td class="mono">{r.fields.join(", ")}</td>
                          <td class="mono">{r.pattern}</td>
                          <td class="mono">{r.action}</td>
                          <td>{r.projectName ?? "Organization"}</td>
                          <Show when={res().isOwner}>
                            <td class="num">
                              <button
                                type="button"
                                class="tb ghost"
                                disabled={busy()}
                                onClick={() => void onDeleteRule(r.id)}
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
                <form class="data-card-form" onSubmit={(e) => void onCreateRule(e)}>
                  <h4>Add custom rule</h4>
                  <div class="data-form-grid">
                    <label class="dash-field">
                      <span>Name</span>
                      <input
                        type="text"
                        class="input compact"
                        placeholder="e.g. customer id"
                        value={ruleName()}
                        onInput={(e) => setRuleName(e.currentTarget.value)}
                        required
                      />
                    </label>
                    <label class="dash-field">
                      <span>Regex pattern</span>
                      <input
                        type="text"
                        class="input compact mono"
                        placeholder="e.g. cust_[0-9]+"
                        value={rulePattern()}
                        onInput={(e) => setRulePattern(e.currentTarget.value)}
                        required
                      />
                    </label>
                    <label class="dash-field">
                      <span>Action</span>
                      <select
                        class="input compact"
                        value={ruleAction()}
                        onChange={(e) => setRuleAction(e.currentTarget.value)}
                      >
                        <For each={ACTIONS}>{(a) => <option value={a}>{a}</option>}</For>
                      </select>
                    </label>
                  </div>
                  <div class="data-field-checks">
                    <For each={res().fieldClasses}>
                      {(fc) => (
                        <label class="data-check-chip">
                          <input
                            type="checkbox"
                            checked={ruleFields().includes(fc)}
                            onChange={() => toggleField(fc)}
                          />
                          <span>{fc}</span>
                        </label>
                      )}
                    </For>
                  </div>
                  <div class="actions">
                    <button
                      type="submit"
                      class="button compact"
                      disabled={busy() || ruleFields().length === 0}
                    >
                      Add sanitizer rule
                    </button>
                  </div>
                </form>
              </Show>
            </section>

            <section class="data-section">
              <div class="data-section-head">
                <h3>Preview</h3>
                <span class="muted">
                  Tests active rules (plus any draft pattern above) on sample text
                </span>
              </div>
              <form class="data-card-form" onSubmit={(e) => void onPreview(e)}>
                <div class="data-inline-form">
                  <select
                    class="input compact"
                    aria-label="Preview field class"
                    value={previewField()}
                    onChange={(e) => setPreviewField(e.currentTarget.value)}
                  >
                    <For each={res().fieldClasses}>{(fc) => <option value={fc}>{fc}</option>}</For>
                  </select>
                  <input
                    type="text"
                    class="input compact data-preview-input"
                    aria-label="Preview sample text"
                    placeholder="Paste sample text to test redaction…"
                    value={previewText()}
                    onInput={(e) => setPreviewText(e.currentTarget.value)}
                  />
                  <button type="submit" class="tb" disabled={busy()}>
                    Test
                  </button>
                </div>
                <Show when={previewResult()}>
                  {(pr) => (
                    <div class="data-preview-out">
                      <div>
                        <span class="data-pill">{pr().outcome}</span>{" "}
                        <code class="mono">{pr().output || "(dropped)"}</code>
                      </div>
                      <Show when={pr().matches.length > 0}>
                        <div class="muted">
                          Matched:{" "}
                          {pr()
                            .matches.map((m) => `${m.rule} (${m.count})`)
                            .join(", ")}
                        </div>
                      </Show>
                    </div>
                  )}
                </Show>
              </form>
            </section>

            <section class="data-section">
              <div class="data-section-head">
                <h3>Redaction counts</h3>
                <select
                  class="input compact"
                  aria-label="Redaction window"
                  value={hours()}
                  onChange={(e) => setHours(Number(e.currentTarget.value))}
                >
                  <option value={24}>Last 24 hours</option>
                  <option value={168}>Last 7 days</option>
                  <option value={720}>Last 30 days</option>
                </select>
              </div>
              <Show
                when={res().counts.length > 0}
                fallback={<p class="muted">No redactions recorded in this window.</p>}
              >
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Rule</th>
                      <th>Field</th>
                      <th class="num">Fired</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={res().counts}>
                      {(c) => (
                        <tr>
                          <td>{c.rule}</td>
                          <td class="mono">{c.field}</td>
                          <td class="num mono">{c.fired.toLocaleString()}</td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </Show>
            </section>
          </>
        )}
      </Show>
    </div>
  );
}
