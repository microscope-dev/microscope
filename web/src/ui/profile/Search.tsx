import type { JSX } from "@solidjs/web";
import { createEffect, createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import { type ProfileLook, transformed, withLook } from "../../profile/look";
import { formatPercent, formatWeight } from "../../profile/paint";
import { type CallTree, matching } from "../../profile/tree";
import type { TransformOp } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Options } from "./Options";

const ACTIONS: { op: TransformOp; label: string; title: string }[] = [
  {
    op: "focus",
    label: "Focus",
    title: "Show only stacks through this function, starting from it",
  },
  { op: "merge", label: "Merge", title: "Hide this function and add its time to its callers" },
  { op: "drop", label: "Drop", title: "Remove every stack through this function" },
];

export function useFrameSearch(): [() => string, (text: string) => void] {
  const focus = useProfile();
  const [typed, setTyped] = createSignal("");
  createEffect(
    () => focus().look.fn,
    (fn) => {
      if (fn !== undefined) {
        setTyped("");
      }
    },
  );
  const value = createMemo(() => (focus().look.fn === undefined ? typed() : ""));
  return [value, setTyped];
}

export function useTreeSearch(
  tree: () => CallTree | null,
  search: () => string,
): () => { matched: Set<number> | null; note: string | null } {
  return createMemo(() => {
    const current = tree();
    const needle = search().toLowerCase();
    if (!current || !needle) {
      return { matched: null, note: null };
    }
    const found = matching(current, (name) => name.toLowerCase().includes(needle));
    return {
      matched: found.nodes,
      note: `matched ${formatPercent(found.weight, current.weight)} of samples`,
    };
  });
}

export function SearchRow(props: {
  search: string;
  onSearch: (text: string) => void;
  matched: string | null;
  reset: ((look: ProfileLook) => ProfileLook) | null;
}) {
  const focus = useProfile();
  let searchBox: HTMLInputElement | undefined;
  const clear = () => {
    props.onSearch("");
    const reset = props.reset;
    focus().setLook(
      // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
      (prev) => withLook(reset ? reset(prev) : prev, "fn", undefined),
      true,
    );
  };

  const clearAndFocus = () => {
    clear();
    searchBox?.focus();
  };
  const edit = (event: InputEvent & { currentTarget: HTMLInputElement }) => {
    const text = event.currentTarget.value;
    const fn = focus().look.fn;
    props.onSearch(text);
    if (text === "" && (props.reset !== null || fn !== undefined)) {
      clear();
    } else if (fn !== undefined) {
      // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
      focus().setLook((prev) => withLook(prev, "fn", undefined));
    }
  };
  return (
    <div
      class="profile-details"
      data-testid={focus().look.fn === undefined ? undefined : "details"}
    >
      <input
        ref={(node) => {
          searchBox = node;
        }}
        type="search"
        class="input profile-search"
        placeholder="Click or search for a frame"
        aria-label="Search frames"
        title={focus().look.fn}
        value={focus().look.fn ?? props.search}
        onInput={edit}
      />
      <Show when={props.search !== "" || focus().look.fn !== undefined || props.reset !== null}>
        <button type="button" class="tb ghost" onClick={clearAndFocus}>
          Clear
        </button>
      </Show>
      <Show
        when={focus().look.fn}
        fallback={
          <Show when={props.matched}>
            {(matched) => (
              <span class="muted" data-testid="matched">
                {matched()}
              </span>
            )}
          </Show>
        }
      >
        {(fn) => <Selected name={fn()} />}
      </Show>
      <span class="profile-options">
        <Options />
      </span>
    </div>
  );
}

export function Selected(props: { name: string }) {
  const focus = useProfile();
  const answer = useAnswer("profileFunctions", () => focus().query);
  const functions = createMemo(() => answer().data);
  const costs = createMemo(() => {
    const data = functions();
    return (data?.rows ?? []).flatMap((row) => {
      const key = data?.keys[row.key];
      return key?.name === props.name ? [{ row, key }] : [];
    });
  });
  const located = createMemo(() => costs().find(({ key }) => key.file)?.key);
  const weight = createMemo(() => functions()?.weight ?? 0);
  const showCallers = () =>
    focus().setLook((look) => ({ ...withLook(look, "view", "functions"), fn: props.name }), true);
  return (
    <>
      <span class="cost">
        <Show when={located()?.file}>
          {(file) => (
            <span class="muted mono">
              {file().slice(file().lastIndexOf("/") + 1)}:{located()?.line}
            </span>
          )}
        </Show>
        <Switch fallback={<span class="muted">Loading cost…</span>}>
          <Match when={answer().error}>
            {(error) => <span class="error">{error().message}</span>}
          </Match>
          <Match when={functions()}>
            <Show
              when={costs().length > 0}
              fallback={<span class="muted">None of the selected samples ran this function.</span>}
            >
              <For each={costs()} keyed={(item) => item.row.key}>
                {(item) => (
                  <span class="muted">
                    {costs().length > 1 ? `${item().key.module ?? "no module"}: ` : ""}
                    {formatWeight(item().row.total, focus().unit)} ·{" "}
                    {formatPercent(item().row.total, weight())} total,{" "}
                    {formatPercent(item().row.own, weight())} own
                  </span>
                )}
              </For>
            </Show>
          </Match>
        </Switch>
      </span>
      <For each={ACTIONS}>
        {(action) => (
          <Action op={action.op} name={props.name} title={action.title}>
            {action.label}
          </Action>
        )}
      </For>
      <button
        type="button"
        class="tb ghost"
        title="Who calls this function, and what it calls"
        onClick={showCallers}
      >
        Callers and callees
      </button>
    </>
  );
}

function Action(props: { op: TransformOp; name: string; title: string; children: JSX.Element }) {
  const focus = useProfile();
  const apply = () =>
    focus().setLook((look) => {
      // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
      return withLook(transformed(look, { op: props.op, name: props.name }), "fn", undefined);
    }, true);
  return (
    <button type="button" class="tb ghost" title={props.title} onClick={apply}>
      {props.children}
    </button>
  );
}
