import { type ChangeEvent, type ReactNode, useCallback, useRef, useState } from "react";

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
  const { summary } = useProfile();
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLDivElement>(null);
  const toggle = useCallback(() => setOpen((was) => !was), []);
  const away = useCallback(() => setOpen(false), []);
  useDismiss(anchor, open, away);

  if (!sampled(summary) && summary.snapshots === 0 && summary.labelKeys.length === 0) {
    return null;
  }
  return (
    <div className="anchor" ref={anchor}>
      <button
        type="button"
        className="tb ghost"
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={toggle}
      >
        Options
      </button>
      {open ? <Popover /> : null}
    </div>
  );
}

function Popover() {
  const { summary, look, setLook } = useProfile();
  const dialog = useRef<HTMLDivElement>(null);
  useDialogFocus(dialog);
  const snapshots = summary.snapshots > 0;

  const framesBy = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const by = event.target.value as ProfileGranularity;
      setLook((prev) => withLook(prev, "by", by === "function" ? undefined : by));
    },
    [setLook],
  );
  const rootByLabel = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) =>
      setLook((prev) => withLook(prev, "bl", event.target.value || undefined)),
    [setLook],
  );
  const collapsed = (look.tf ?? []).includes(COLLAPSE_CYCLES);
  const collapseCycles = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => {
      const on = event.target.checked;
      setLook((prev) => {
        if (on) {
          return transformed(prev, { op: "collapseCycles", name: "" });
        }

        const kept = (prev.tf ?? []).filter((entry) => entry !== COLLAPSE_CYCLES);
        return withLook(prev, "tf", kept.length > 0 ? kept : undefined);
      }, true);
    },
    [setLook],
  );

  return (
    <div
      ref={dialog}
      className="popover profile-options"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a popover, not a modal <dialog>
      role="dialog"
      aria-label="Options"

      data-keys="escape"
    >
      {sampled(summary) ? (
        <label className="options-field">
          Frames
          <select className="input" value={look.by ?? "function"} onChange={framesBy}>
            {GRANULARITIES.map(({ by, label }) => (
              <option key={by} value={by}>
                {label}
              </option>
            ))}
          </select>
        </label>
      ) : null}
      {summary.labelKeys.length > 0 ? (
        <label className="options-field">
          Root by label
          <select className="input" value={look.bl ?? ""} onChange={rootByLabel}>
            <option value="">none</option>
            {summary.labelKeys.map((key) => (
              <option key={key} value={key}>
                {key}
              </option>
            ))}
          </select>
        </label>
      ) : null}
      {sampled(summary) ? (
        <>
          <LookCheck field="invert" checked={look.invert === "1"} on="1" off={undefined}>
            Inverted
          </LookCheck>
          <LookCheck
            field="native"
            title="Show each async task's frames as a stack of its own"
            checked={look.native !== "1"}
            on={undefined}
            off="1"
          >
            Async stacks
          </LookCheck>
          {summary.syscalls > 0 && summary.unit === "nanoseconds" && look.src !== "waits" ? (
            <LookCheck
              field="sk"
              title="Show the time each system call spent off its CPU under the code that made it"
              checked={look.sk === "1"}
              on="1"
              off={undefined}
            >
              System calls as leaves
            </LookCheck>
          ) : null}
          {summary.interpreted ? (
            <LookCheck
              field="py"
              title="Show Python frames only, without the interpreter's frames"
              checked={look.py === "1"}
              on="1"
              off={undefined}
            >
              Python stacks
            </LookCheck>
          ) : null}
        </>
      ) : null}
      {snapshots ? (
        <LookCheck
          field="bt"
          title="Group stacks by the task they ran or waited in"
          checked={look.bt === "1"}
          on="1"
          off={undefined}
        >
          By task
        </LookCheck>
      ) : null}
      {sampled(summary) ? (
        <label
          className="check"
          title="Merge recursive calls, direct or indirect, into the first call"
        >
          <input type="checkbox" checked={collapsed} onChange={collapseCycles} />
          Collapse recursion
        </label>
      ) : null}
    </div>
  );
}

function LookCheck<K extends keyof ProfileLook>({
  field,
  title,
  checked,
  on,
  off,
  children,
}: {
  field: K;
  title?: string;
  checked: boolean;
  on: ProfileLook[K] | undefined;
  off: ProfileLook[K] | undefined;
  children: ReactNode;
}) {
  const { setLook } = useProfile();
  const toggle = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => {
      const value = event.target.checked ? on : off;
      setLook((prev) => withLook(prev, field, value));
    },
    [setLook, field, on, off],
  );
  return (
    <label className="check" title={title}>
      <input type="checkbox" checked={checked} onChange={toggle} />
      {children}
    </label>
  );
}
