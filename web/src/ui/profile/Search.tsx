import { type ChangeEvent, type ReactNode, useCallback, useMemo, useRef, useState } from "react";

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

/** What a view's search holds: the text typed while no frame is selected.
 * Selecting a frame shows its name in the search instead, so the text
 * typed before goes, and does not come back once it is let go. */
export function useFrameSearch(): [string, (text: string) => void] {
  const { look } = useProfile();
  const [typed, setTyped] = useState("");
  const [seen, setSeen] = useState(look.fn);
  if (seen !== look.fn) {
    setSeen(look.fn);
    if (look.fn !== undefined) {
      setTyped("");
    }
  }
  return [look.fn === undefined ? typed : "", setTyped];
}

/** The frames of `tree` whose names hold `search`, in any case, and what
 * the search row says of them: the share of the samples they ran in. */
export function useTreeSearch(
  tree: CallTree | null,
  search: string,
): { matched: Set<number> | null; note: string | null } {
  return useMemo(() => {
    const needle = search.toLowerCase();
    if (!tree || !needle) {
      return { matched: null, note: null };
    }
    const found = matching(tree, (name) => name.toLowerCase().includes(needle));
    return {
      matched: found.nodes,
      note: `matched ${formatPercent(found.weight, tree.weight)} of samples`,
    };
  }, [tree, search]);
}

/** One row that searches a view's frames and holds the selected one's
 * details, the same in every view, and ends in its Options. A click on a
 * frame puts its name in the search; typing lets it go, and emptying the
 * search clears, as Clear does. `reset` also clears what else the view keeps, such as a zoom. */
export function SearchRow({
  search,
  onSearch,
  matched,
  reset,
}: {
  search: string;
  onSearch: (text: string) => void;
  /** What the search matches, once it has text. */
  matched: string | null;
  /** Clears the rest of what the view shows, or null when it has none. */
  reset: ((look: ProfileLook) => ProfileLook) | null;
}) {
  const { look, setLook } = useProfile();
  const { fn } = look;
  const searchBox = useRef<HTMLInputElement>(null);
  const clear = useCallback(() => {
    onSearch("");
    setLook(
      // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
      (prev) => withLook(reset ? reset(prev) : prev, "fn", undefined),
      true,
    );
  }, [onSearch, setLook, reset]);
  // The button goes once it has cleared, so the search keeps the focus.
  const clearAndFocus = useCallback(() => {
    clear();
    searchBox.current?.focus();
  }, [clear]);
  const edit = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => {
      const text = event.target.value;
      onSearch(text);
      if (text === "" && (reset !== null || fn !== undefined)) {
        clear();
      } else if (fn !== undefined) {
        // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
        setLook((prev) => withLook(prev, "fn", undefined));
      }
    },
    [reset, fn, onSearch, setLook, clear],
  );
  return (
    // One row whether or not a frame is selected, so the search stays put
    // and keeps its focus as a click selects or typing lets go.
    <div className="profile-details" data-testid={fn === undefined ? undefined : "details"}>
      <input
        ref={searchBox}
        type="search"
        className="input profile-search"
        placeholder="Click or search for a frame"
        aria-label="Search frames"
        title={fn}
        value={fn ?? search}
        onChange={edit}
      />
      {search !== "" || fn !== undefined || reset !== null ? (
        <button type="button" className="tb ghost" onClick={clearAndFocus}>
          Clear
        </button>
      ) : null}
      {fn === undefined ? (
        matched ? (
          <span className="muted" data-testid="matched">
            {matched}
          </span>
        ) : null
      ) : (
        <Selected name={fn} />
      )}
      <span className="profile-options">
        <Options />
      </span>
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
            <span className="muted">None of the selected samples ran this function.</span>
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
          <span className="muted">Loading cost…</span>
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
        title="Who calls this function, and what it calls"
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
        // Focused, the function is the root at 100%; merged or dropped, it
        // is gone. Either way its selection says nothing more, so it goes.
        // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
        return withLook(transformed(look, { op, name }), "fn", undefined);
      }, true),
    [setLook, op, name],
  );
  return (
    <button type="button" className="tb ghost" title={title} onClick={apply}>
      {children}
    </button>
  );
}
