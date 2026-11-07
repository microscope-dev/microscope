// How the profile's frames are drawn, out of the toolbar's way: a button
// that counts what differs from the defaults, and a popover that sets it.

import { type ChangeEvent, type ReactNode, useCallback, useMemo, useRef, useState } from "react";

import { type ProfileLook, type ProfileView, transformed, withLook } from "../../profile/look";
import type { ProfileGranularity, ProfileSummary } from "../../protocol";
import { useDialogFocus, useDismiss } from "../dialog";
import { useProfile } from "./context";

const VIEWS: { view: ProfileView; label: string }[] = [
  { view: "flame", label: "Flame graph" },
  { view: "tree", label: "Call tree" },
  { view: "functions", label: "Functions" },
  { view: "zones", label: "Zones" },
  { view: "syscalls", label: "Syscalls" },
];

/** A trace with no samples opens on its calls; anything else on its
 * flame graph. */
export function defaultView(summary: ProfileSummary): ProfileView {
  return summary.samples === 0 && summary.syscalls > 0 ? "syscalls" : "flame";
}

/** Whether Options offers a view: sample views when there are samples or
 * nothing else, zones when there are some, calls when traced. */
function offers(summary: ProfileSummary, view: ProfileView): boolean {
  switch (view) {
    case "zones":
      return summary.zoneSites.length > 0 || summary.probes.plotted > 0;
    case "syscalls":
      return summary.syscalls > 0;
    default:
      return summary.samples > 0 || summary.syscalls === 0;
  }
}

const COLLAPSE_CYCLES = "collapseCycles";

const GRANULARITIES: { by: ProfileGranularity; label: string }[] = [
  { by: "function", label: "functions" },
  { by: "physical", label: "functions, inlines folded" },
  { by: "line", label: "lines" },
  { by: "address", label: "instructions" },
];

/** A trace of system calls alone has no frames to invert, fold, or compare. */
const sampled = (summary: ProfileSummary) => summary.samples > 0 || summary.syscalls === 0;

/** The Options button and its popover, or nothing for a profile with
 * nothing to set. */
export function Options() {
  const { summary } = useProfile();
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLDivElement>(null);
  const toggle = useCallback(() => setOpen((was) => !was), []);
  const away = useCallback(() => setOpen(false), []);
  useDismiss(anchor, open, away);

  const views = useMemo(() => VIEWS.filter(({ view }) => offers(summary, view)), [summary]);
  if (
    views.length < 2 &&
    !sampled(summary) &&
    summary.snapshots === 0 &&
    summary.labelKeys.length === 0
  ) {
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
      {open ? <Popover views={views} /> : null}
    </div>
  );
}

/** Its focus goes back to the Options button when it closes. */
function Popover({ views }: { views: typeof VIEWS }) {
  const { summary, look, setLook } = useProfile();
  const dialog = useRef<HTMLDivElement>(null);
  useDialogFocus(dialog);
  const snapshots = summary.snapshots > 0;

  const showView = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) => {
      const view = event.target.value as ProfileView;
      setLook(
        (prev) => withLook(prev, "view", view === defaultView(summary) ? undefined : view),
        true,
      );
    },
    [setLook, summary],
  );
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
        // Only this transform goes; those after it still name their frames.
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
      // Escape closes it rather than leave the view.
      data-keys="escape"
    >
      {views.length > 1 ? (
        <label className="options-field">
          View
          <select className="input" value={look.view ?? defaultView(summary)} onChange={showView}>
            {views.map(({ view, label }) => (
              <option key={view} value={view}>
                {label}
              </option>
            ))}
          </select>
        </label>
      ) : null}
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
        <>
          <LookCheck
            field="bt"
            title="Group stacks by the task they ran or waited in"
            checked={look.bt === "1"}
            on="1"
            off={undefined}
          >
            By task
          </LookCheck>
          {look.src === "waits" ? null : (
            <LookCheck
              field="lanes"
              title="A timeline lane per task, not per thread"
              checked={look.lanes === "tasks"}
              on="tasks"
              off={undefined}
            >
              Task lanes
            </LookCheck>
          )}
        </>
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

/** A checkbox that sets one part of the look to `on` or `off`. */
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
