// Small pieces the profile's views share: a removable chip, buttons that
// change the look, rounded numbers that keep their exact value on hover,
// a share drawn as a bar, and what a view says before its answer comes or
// when it failed.

import { type ReactNode, useCallback, useMemo } from "react";

import { type ProfileLook, withLook } from "../../profile/look";
import { formatBytes, formatCount, formatDuration, heat } from "../../profile/paint";
import type { RequestError } from "../../protocol";
import { useShownScheme } from "../../theme";
import { useProfile } from "./context";

export function Chip({
  label,
  title,
  remove,
  removeLabel = `Remove ${label}`,
}: {
  label: string;
  title: string;
  remove: () => void;
  removeLabel?: string | undefined;
}) {
  return (
    // A long label is cut short, so the title also says it whole.
    <span className="chip profile-chip" title={`${label}\n${title}`}>
      <span className="label">{label}</span>
      <button type="button" aria-label={removeLabel} onClick={remove}>
        ×
      </button>
    </span>
  );
}

/** A chip for one part of the look, which removing clears. */
export function ClearChip({
  field,
  label,
  title,
  removeLabel,
}: {
  field: keyof ProfileLook;
  label: string;
  title: string;
  removeLabel?: string;
}) {
  const { setLook } = useProfile();
  const clear = useCallback(
    () =>
      setLook((prev) =>
        // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
        withLook(prev, field, undefined),
      ),
    [setLook, field],
  );
  return <Chip label={label} title={title} removeLabel={removeLabel} remove={clear} />;
}

/** A chip for the threads, or tasks, the samples are cut to. */
export function LaneChip() {
  const { summary, look } = useProfile();
  const taskName = (id: string) =>
    id === "none"
      ? "no task known"
      : (summary.tasks.find((task) => String(task.row) === id)?.name ?? `task ${id}`);
  if (look.th) {
    return (
      <ClearChip
        field="th"
        label={formatCount(look.th.length, "thread")}
        title="Samples of these threads"
      />
    );
  }
  if (look.tk) {
    return (
      <ClearChip
        field="tk"
        label={
          look.tk.length === 1 ? taskName(look.tk[0] ?? "") : formatCount(look.tk.length, "task")
        }
        title={look.tk.map(taskName).join("\n")}
      />
    );
  }
  return null;
}

/** A button that sets one part of the look, pressed when it is set so. */
export function LookButton<K extends keyof ProfileLook>({
  field,
  value,
  title,
  pressed,
  replace = false,
  children,
}: {
  field: K;
  value: ProfileLook[K] | undefined;
  title?: string;
  pressed: boolean;
  /** Replaces the address rather than pushing a new entry. */
  replace?: boolean;
  children: ReactNode;
}) {
  const { setLook } = useProfile();
  const pick = useCallback(
    () => setLook((prev) => withLook(prev, field, value), !replace),
    [setLook, field, value, replace],
  );
  return (
    <button type="button" title={title} aria-pressed={pressed} onClick={pick}>
      {children}
    </button>
  );
}

/** Nanoseconds, rounded, with the exact count on hover. */
export function Duration({ ns }: { ns: number }) {
  return <span title={`${formatCount(Math.round(ns))} ns`}>{formatDuration(ns)}</span>;
}

/** Bytes, rounded, with the exact count on hover. */
export function Bytes({ bytes }: { bytes: number }) {
  return <span title={formatCount(bytes, "byte")}>{formatBytes(bytes)}</span>;
}

/** What a view shows in place of an answer: why it failed, or what it is
 * reading. */
export function Unanswered({
  error,
  reading,
  className = "empty",
}: {
  error: RequestError | undefined;
  reading: string;
  className?: string;
}) {
  return error ? (
    <div className={`${className} error`}>{error.message}</div>
  ) : (
    <div className={`${className} muted`}>{reading}</div>
  );
}

/** The share of the samples a function's own time needs to be hot. */
export const HOT_OWN = 0.05;

/** A row's percent over a bar as wide as its share of the cell, warmer
 * as the share grows. A hot share is bold; a dim one is a column read
 * second. */
export function Share({
  share,
  tone,
  children,
}: {
  share: number;
  tone: "plain" | "dim" | "hot";
  children: string;
}) {
  const dark = useShownScheme() === "dark";
  const bar = useMemo(
    () => ({ width: `${Math.min(Math.max(share, 0), 1) * 100}%`, background: heat(share, dark) }),
    [share, dark],
  );
  return (
    <span className={`percent share ${tone}`}>
      <span className="bar" style={bar} />
      <span className="value">{children}</span>
    </span>
  );
}

/** How an own share reads: bold once hot, else as `otherwise`. */
export function ownTone(share: number, otherwise: "plain" | "dim"): "plain" | "dim" | "hot" {
  return share >= HOT_OWN ? "hot" : otherwise;
}
