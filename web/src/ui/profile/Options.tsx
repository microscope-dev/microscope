import type { JSX } from "@solidjs/web";
import { createMemo, createSignal, For, Show } from "solid-js";

import { type ProfileLook, transformed, withLook } from "../../profile/look";
import type { ProfileGranularity, ProfileSummary } from "../../protocol";
import { useDialogFocus, useDismiss } from "../dialog";
import { useProfile } from "./context";

const COLLAPSE_CYCLES = "collapseCycles";

const GRANULARITIES: { by: ProfileGranularity; label: string }[] = [
  { by: "function", label: "functions" },
  { by: "physical", label: "functions, inlines folded" },
  { by: "line", label: "lines" },
  { by: "address", label: "instructions" },
];

const sampled = (summary: ProfileSummary) => summary.samples > 0 || summary.syscalls === 0;

export function Options() {
  const focus = useProfile();
  const [open, setOpen] = createSignal(false);
  let anchor: HTMLDivElement | undefined;
  const toggle = () => setOpen((was) => !was);
  const away = () => setOpen(false);
  useDismiss(() => anchor, open, away);

  return (
    <Show
      when={
        sampled(focus().summary) ||
        focus().summary.snapshots > 0 ||
        focus().summary.labelKeys.length > 0
      }
    >
      <div
        class="anchor"
        ref={(node) => {
          anchor = node;
        }}
      >
        <button
          type="button"
          class="tb ghost"
          aria-haspopup="dialog"
          aria-expanded={open() ? "true" : "false"}
          onClick={toggle}
        >
          Options
        </button>
        <Show when={open()}>
          <Popover />
        </Show>
      </div>
    </Show>
  );
}

function Popover() {
  const focus = useProfile();
  let dialog: HTMLDivElement | undefined;
  useDialogFocus(() => dialog);
  const snapshots = createMemo(() => focus().summary.snapshots > 0);
  const isSampled = createMemo(() => sampled(focus().summary));

  const framesBy = (event: Event & { currentTarget: HTMLSelectElement }) => {
    const by = event.currentTarget.value as ProfileGranularity;
    focus().setLook((prev) => withLook(prev, "by", by === "function" ? undefined : by));
  };
  const showStacks = (event: Event & { currentTarget: HTMLSelectElement }) => {
    const stacks = event.currentTarget.value as ProfileLook["stacks"] | "code";
    focus().setLook((prev) => withLook(prev, "stacks", stacks === "code" ? undefined : stacks));
  };
  const rootByLabel = (event: Event & { currentTarget: HTMLSelectElement }) =>
    focus().setLook((prev) => withLook(prev, "bl", event.currentTarget.value || undefined));
  const collapsed = createMemo(() => (focus().look.tf ?? []).includes(COLLAPSE_CYCLES));
  const collapseCycles = (event: Event & { currentTarget: HTMLInputElement }) => {
    const on = event.currentTarget.checked;
    focus().setLook((prev) => {
      if (on) {
        return transformed(prev, { op: "collapseCycles", name: "" });
      }
      const kept = (prev.tf ?? []).filter((entry) => entry !== COLLAPSE_CYCLES);
      return withLook(prev, "tf", kept.length > 0 ? kept : undefined);
    }, true);
  };

  return (
    <div
      ref={(node) => {
        dialog = node;
      }}
      class="popover profile-options"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a popover, not a modal <dialog>
      role="dialog"
      aria-label="Options"
      data-keys="escape"
    >
      <Show when={isSampled()}>
        <label class="options-field">
          Frames
          <select class="input" value={focus().look.by ?? "function"} onChange={framesBy}>
            <For each={GRANULARITIES}>
              {(item) => <option value={item.by}>{item.label}</option>}
            </For>
          </select>
        </label>
      </Show>
      <Show when={isSampled()}>
        <label class="options-field">
          Stacks
          <select class="input" value={focus().look.stacks ?? "code"} onChange={showStacks}>
            <option value="code">your code</option>
            <Show when={focus().summary.interpreted}>
              <option value="tasks">with the interpreter</option>
            </Show>
            <option value="native">native frames</option>
          </select>
        </label>
      </Show>
      <Show when={focus().summary.labelKeys.length > 0}>
        <label class="options-field">
          Root by label
          <select class="input" value={focus().look.bl ?? ""} onChange={rootByLabel}>
            <option value="">none</option>
            <For each={focus().summary.labelKeys}>
              {(key) => <option value={key}>{key}</option>}
            </For>
          </select>
        </label>
      </Show>
      <Show when={isSampled()}>
        <LookCheck field="invert" checked={focus().look.invert === "1"} on="1" off={undefined}>
          Inverted
        </LookCheck>
        <Show
          when={
            focus().summary.syscalls > 0 &&
            focus().summary.unit === "nanoseconds" &&
            focus().look.src !== "waits"
          }
        >
          <LookCheck
            field="sk"
            title="Show the time each system call spent off its CPU under the code that made it"
            checked={focus().look.sk === "1"}
            on="1"
            off={undefined}
          >
            System calls as leaves
          </LookCheck>
        </Show>
      </Show>
      <Show when={snapshots()}>
        <LookCheck
          field="bt"
          title="Group stacks by the task they ran or waited in"
          checked={focus().look.bt === "1"}
          on="1"
          off={undefined}
        >
          By task
        </LookCheck>
      </Show>
      <Show when={isSampled()}>
        <label class="check" title="Merge recursive calls, direct or indirect, into the first call">
          <input type="checkbox" checked={collapsed()} onChange={collapseCycles} />
          Collapse recursion
        </label>
      </Show>
    </div>
  );
}

function LookCheck<K extends keyof ProfileLook>(props: {
  field: K;
  title?: string | undefined;
  checked: boolean;
  on: ProfileLook[K] | undefined;
  off: ProfileLook[K] | undefined;
  children: JSX.Element;
}) {
  const focus = useProfile();
  const toggle = (event: Event & { currentTarget: HTMLInputElement }) => {
    const value = event.currentTarget.checked ? props.on : props.off;
    focus().setLook((prev) => withLook(prev, props.field, value));
  };
  return (
    <label class="check" title={props.title}>
      <input type="checkbox" checked={props.checked} onChange={toggle} />
      {props.children}
    </label>
  );
}
