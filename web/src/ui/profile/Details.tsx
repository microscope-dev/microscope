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

/** The selected function: what it costs under the query, as the server
 * counts each function, and what can be done with it. Functions of one
 * name in several modules each show their own cost. */
export function Details({ name }: { name: string }) {
  const { query, unit, setLook } = useProfile();
  const answer = useAnswer("profileFunctions", query);
  const functions = answer.data;
  const costs = (functions?.rows ?? []).flatMap((row) => {
    const key = functions?.keys[row.key];
    return key?.name === name ? [{ row, key }] : [];
  });
  const located = costs.find(({ key }) => key.file)?.key;
  const weight = functions?.weight ?? 0;
  return (
    <div className="profile-details" data-testid="details">
      <span className="mono name" title={name}>
        {name}
      </span>
      {located?.file ? (
        <span className="muted mono">
          {located.file.slice(located.file.lastIndexOf("/") + 1)}:{located.line}
        </span>
      ) : null}
      {answer.error ? (
        <span className="error">{answer.error.message}</span>
      ) : !functions ? (
        <span className="muted">Reading its cost…</span>
      ) : costs.length === 0 ? (
        <span className="muted">No selected sample runs it.</span>
      ) : (
        costs.map(({ row, key }) => (
          <span key={row.key} className="muted">
            {costs.length > 1 ? `${key.module ?? "no module"}: ` : ""}
            {formatWeight(row.total, unit)} · {formatPercent(row.total, weight)} total,{" "}
            {formatPercent(row.own, weight)} own
          </span>
        ))
      )}
      <span className="spacer" />
      {ACTIONS.map(({ op, label, title }) => (
        <button
          key={op}
          type="button"
          className="tb ghost"
          title={title}
          onClick={() => setLook((look) => transformed(look, { op, name }), true)}
        >
          {label}
        </button>
      ))}
      <button
        type="button"
        className="tb ghost"
        title="Its callers and callees"
        onClick={() =>
          setLook((look) => ({ ...withLook(look, "view", "functions"), fn: name }), true)
        }
      >
        Callers and callees
      </button>
    </div>
  );
}
