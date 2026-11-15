import { type CSSProperties, type ReactNode, useCallback, useMemo } from "react";

import { type ProfileLook, withLook } from "../../profile/look";
import { formatBytes, formatCount, formatDuration } from "../../profile/paint";
import { sampledKernel } from "../../profile/syscalls";
import type { RequestError } from "../../protocol";
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
    <span className="chip profile-chip" title={`${label}\n${title}`}>
      <span className="label">{label}</span>
      <button type="button" aria-label={removeLabel} onClick={remove}>
        ×
      </button>
    </span>
  );
}

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

export function Duration({ ns }: { ns: number }) {
  return <span title={`${formatCount(Math.round(ns))} ns`}>{formatDuration(ns)}</span>;
}

export function Bytes({ bytes }: { bytes: number }) {
  return <span title={formatCount(bytes, "byte")}>{formatBytes(bytes)}</span>;
}

export function NoSamples() {
  const { look, summary } = useProfile();
  return (
    <div className="empty">
      {look.du && look.sk !== "1" && !sampledKernel(summary.settings)
        ? "No sample falls inside a system call: this recording sampled user code only. Turn on System calls as leaves in Options to see the calls' own time, or record with --kernel-stacks."
        : "No samples are selected."}
    </div>
  );
}

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

export const HOT_OWN = 0.05;

function Fill({ share }: { share: number }) {
  const style = useMemo(
    () => ({ "--share": String(Math.min(Math.max(share, 0), 1)) }) as CSSProperties,
    [share],
  );
  return <span className="share-fill" style={style} />;
}

export function Share({
  share,
  tone,
  children,
}: {
  share: number;
  tone: "plain" | "dim" | "hot";
  children: string;
}) {
  return (
    <span className={`percent share ${tone === "hot" ? "strong" : tone}`}>
      <Fill share={share} />
      <span className="value">{children}</span>
    </span>
  );
}

export function ShareBar({ share }: { share: number }) {
  return (
    <span className="stat-bar" aria-hidden="true">
      <Fill share={share} />
    </span>
  );
}

export function ownTone(share: number, otherwise: "plain" | "dim"): "plain" | "dim" | "hot" {
  return share >= HOT_OWN ? "hot" : otherwise;
}
