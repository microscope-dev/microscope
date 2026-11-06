import { type ReactNode, useCallback } from "react";

import { transformed, withLook } from "../../profile/look";
import { formatPercent, formatWeight } from "../../profile/paint";
import type { TransformOp } from "../../protocol";
import { useAnswer, useProfile } from "./context";

const ACTIONS: { op: TransformOp; label: string; title: string }[] = [
  { op: "focus", label: "Focus", title: "Only samples through it, rooted at it" },
  { op: "merge", label: "Merge", title: "Hide it, giving its cost to its callers" },
  { op: "drop", label: "Drop", title: "Remove every sample through it" },
  { op: "collapse", label: "Collapse recursion", title: "Merge its calls of itself" },
];

/** The selected function in a bar of its own: its name, then what
 * `Selected` shows. */
export function Details({ name }: { name: string }) {
  return (
    <div className="profile-details" data-testid="details">
      <span className="mono name" title={name}>
        {name}
      </span>
      <Selected name={name} />
    </div>
  );
}

/** What the selected function costs under the query, as the server counts
 * each function, and what can be done with it, for a bar that names it.
 * Functions of one name in several modules each show their own cost. */
export function Selected({ name }: { name: string }) {
  const { query, unit, setLook } = useProfile();
  const answer = useAnswer("profileFunctions", query);
  const functions = answer.data;
  const costs = (functions?.rows ?? []).flatMap((row) => {
    const key = functions?.keys[row.key];
    return key?.name === name ? [{ row, key }] : [];
  });
  const located = costs.find(({ key }) => key.file)?.key;
  const weight = functions?.weight ?? 0;
  const showCallers = useCallback(
    () => setLook((look) => ({ ...withLook(look, "view", "functions"), fn: name }), true),
    [setLook, name],
  );
  return (
    <>
      {/* Takes the room the actions leave, cut short before they wrap. */}
      <span className="cost">
        {located?.file ? (
          <span className="muted mono">
            {located.file.slice(located.file.lastIndexOf("/") + 1)}:{located.line}
          </span>
        ) : null}
        {answer.error ? (
          <span className="error">{answer.error.message}</span>
        ) : functions ? (
          costs.length === 0 ? (
            <span className="muted">No selected sample runs it.</span>
          ) : (
            costs.map(({ row, key }) => (
              <span key={row.key} className="muted">
                {costs.length > 1 ? `${key.module ?? "no module"}: ` : ""}
                {formatWeight(row.total, unit)} · {formatPercent(row.total, weight)} total,{" "}
                {formatPercent(row.own, weight)} own
              </span>
            ))
          )
        ) : (
          <span className="muted">Reading its cost…</span>
        )}
      </span>
      {ACTIONS.map(({ op, label, title }) => (
        <Action key={op} op={op} name={name} title={title}>
          {label}
        </Action>
      ))}
      <button
        type="button"
        className="tb ghost"
        title="Its callers and callees"
        onClick={showCallers}
      >
        Callers and callees
      </button>
    </>
  );
}

/** A button that transforms the profile by the selected function. */
function Action({
  op,
  name,
  title,
  children,
}: {
  op: TransformOp;
  name: string;
  title: string;
  children: ReactNode;
}) {
  const { setLook } = useProfile();
  const apply = useCallback(
    () =>
      setLook((look) => {
        const next = transformed(look, { op, name });
        // Focused, the function is the root at 100%; merged or dropped, it
        // is gone. Either way its selection says nothing more, so it goes.
        // Collapsing recursion leaves it where it was.
        // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
        return op === "collapse" ? next : withLook(next, "fn", undefined);
      }, true),
    [setLook, op, name],
  );
  return (
    <button type="button" className="tb ghost" title={title} onClick={apply}>
      {children}
    </button>
  );
}
