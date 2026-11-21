import type { JSX } from "@solidjs/web";
import { Match, Show, Switch } from "solid-js";

import { type ProfileLook, withLook } from "../../profile/look";
import { formatBytes, formatCount, formatDuration } from "../../profile/paint";
import { sampledKernel } from "../../profile/syscalls";
import type { RequestError } from "../../protocol";
import { useProfile } from "./context";

export function Chip(props: {
  label: string;
  title: string;
  remove: () => void;
  removeLabel?: string | undefined;
}) {
  return (
    <span class="chip profile-chip" title={`${props.label}\n${props.title}`}>
      <span class="label">{props.label}</span>
      <button
        type="button"
        aria-label={props.removeLabel ?? `Remove ${props.label}`}
        onClick={() => props.remove()}
      >
        ×
      </button>
    </span>
  );
}

export function ClearChip(props: {
  field: keyof ProfileLook;
  label: string;
  title: string;
  removeLabel?: string | undefined;
}) {
  const focus = useProfile();
  const clear = () =>
    focus().setLook((prev) =>
      // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
      withLook(prev, props.field, undefined),
    );
  return (
    <Chip label={props.label} title={props.title} removeLabel={props.removeLabel} remove={clear} />
  );
}

export function LaneChip() {
  const focus = useProfile();
  const taskName = (id: string) =>
    id === "none"
      ? "no task known"
      : (focus().summary.tasks.find((task) => String(task.row) === id)?.name ?? `task ${id}`);
  return (
    <Switch>
      <Match when={focus().look.th}>
        {(threads) => (
          <ClearChip
            field="th"
            label={formatCount(threads().length, "thread")}
            title="Samples of these threads"
          />
        )}
      </Match>
      <Match when={focus().look.tk}>
        {(tasks) => (
          <ClearChip
            field="tk"
            label={
              tasks().length === 1
                ? taskName(tasks()[0] ?? "")
                : formatCount(tasks().length, "task")
            }
            title={tasks()
              .map((id) => taskName(id))
              .join("\n")}
          />
        )}
      </Match>
    </Switch>
  );
}

export function LookButton<K extends keyof ProfileLook>(props: {
  field: K;
  value: ProfileLook[K] | undefined;
  title?: string | undefined;
  pressed: boolean;
  replace?: boolean | undefined;
  children: JSX.Element;
}) {
  const focus = useProfile();
  const pick = () =>
    focus().setLook((prev) => withLook(prev, props.field, props.value), !(props.replace ?? false));
  return (
    <button
      type="button"
      title={props.title}
      aria-pressed={props.pressed ? "true" : "false"}
      onClick={pick}
    >
      {props.children}
    </button>
  );
}

export function Duration(props: { ns: number }) {
  return <span title={`${formatCount(Math.round(props.ns))} ns`}>{formatDuration(props.ns)}</span>;
}

export function Bytes(props: { bytes: number }) {
  return <span title={formatCount(props.bytes, "byte")}>{formatBytes(props.bytes)}</span>;
}

export function NoSamples() {
  const focus = useProfile();
  return (
    <div class="empty">
      {focus().look.du && focus().look.sk !== "1" && !sampledKernel(focus().summary.settings)
        ? "No sample falls inside a system call: this recording sampled user code only. Turn on System calls as leaves in Options to see the calls' own time, or record with --kernel-stacks."
        : "No samples are selected."}
    </div>
  );
}

export function Unanswered(props: {
  error: RequestError | undefined;
  reading: string;
  className?: string | undefined;
}) {
  return (
    <Show
      when={props.error}
      fallback={<div class={`${props.className ?? "empty"} muted`}>{props.reading}</div>}
    >
      {(error) => <div class={`${props.className ?? "empty"} error`}>{error().message}</div>}
    </Show>
  );
}

export const HOT_OWN = 0.05;

function Fill(props: { share: number }) {
  return (
    <span class="share-fill" style={{ "--share": String(Math.min(Math.max(props.share, 0), 1)) }} />
  );
}

export function Share(props: { share: number; tone: "plain" | "dim" | "hot"; children: string }) {
  return (
    <span class={`percent share ${props.tone === "hot" ? "strong" : props.tone}`}>
      <Fill share={props.share} />
      <span class="value">{props.children}</span>
    </span>
  );
}

export function ShareBar(props: { share: number }) {
  return (
    <span class="stat-bar" aria-hidden="true">
      <Fill share={props.share} />
    </span>
  );
}

export function ownTone(share: number, otherwise: "plain" | "dim"): "plain" | "dim" | "hot" {
  return share >= HOT_OWN ? "hot" : otherwise;
}
