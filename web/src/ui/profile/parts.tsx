// Small pieces the profile's views share: a removable chip, rounded
// numbers that keep their exact value on hover, and what a view says
// before its answer comes or when it failed.

import { formatBytes, formatCount, formatDuration } from "../../profile/paint";
import type { RequestError } from "../../protocol";

export function Chip({
  label,
  title,
  remove,
  removeLabel = `Remove ${label}`,
}: {
  label: string;
  title: string;
  remove: () => void;
  removeLabel?: string;
}) {
  return (
    <span className="chip profile-chip" title={title}>
      {label}
      <button type="button" aria-label={removeLabel} onClick={remove}>
        ×
      </button>
    </span>
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
