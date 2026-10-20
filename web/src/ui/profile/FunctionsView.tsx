import { useVirtualizer } from "@tanstack/react-virtual";
import { useMemo, useRef, useState } from "react";
import { withLook } from "../../profile/look";
import {
  formatChange,
  formatCount,
  formatPercent,
  formatWeight,
  shareChange,
} from "../../profile/paint";
import type { ProfileQuery } from "../../protocol";
import { Code, SourceLines } from "./Annotated";
import { useAnswer, useProfile } from "./context";
import { Unanswered } from "./parts";
import { useActiveRow } from "./rows";
import { Change, ColumnHead, TreeRows, useTree } from "./TreeView";

const ROW = 20;

type Order = "own" | "total";

/** Every function's cost, and for the selected one a sandwich: its
 * callers above and its callees below, merged from every call. */
export function FunctionsView() {
  const { query, look, unit, setLook } = useProfile();
  const answer = useAnswer("profileFunctions", query);
  const [order, setOrder] = useState<Order>("own");
  const body = useRef<HTMLDivElement>(null);
  const rows = useMemo(() => {
    const all = answer.data?.rows ?? [];
    return order === "own" ? all : [...all].sort((a, b) => b.total - a.total || b.own - a.own);
  }, [answer.data, order]);
  const virtual = useVirtualizer({
    count: rows.length,
    getScrollElement: () => body.current,
    estimateSize: () => ROW,
    overscan: 30,
  });
  const keys = useActiveRow(rows.length, virtual);
  const weight = answer.data?.weight ?? 0;
  const names = answer.data?.keys ?? [];
  const base = answer.data?.base ?? null;
  const select = (index: number) => {
    const name = names[rows[index]?.key ?? -1]?.name;
    if (name !== undefined) {
      setLook((look) => ({ ...look, fn: name }));
    }
  };
  return (
    <div className="profile-functions">
      <div className="pane grow">
        <div className="pane-head">
          Functions
          <span className="count">{formatCount(rows.length)}</span>
          <div className="segmented">
            {(["own", "total"] as const).map((each) => (
              <button
                key={each}
                type="button"
                aria-pressed={order === each}
                onClick={() => setOrder(each)}
              >
                by {each === "own" && base ? "change" : each}
              </button>
            ))}
          </div>
        </div>
        <ColumnHead columns={base ? ["own", "total", "change"] : ["own", "total"]} end="module" />
        {answer.error || !answer.data ? (
          <Unanswered error={answer.error} reading="Reading functions…" />
        ) : rows.length === 0 ? (
          <div className="empty">No samples are selected.</div>
        ) : (
          <div
            ref={body}
            className={`pane-body ${answer.current ? "" : "stale"}`}
            role="listbox"
            aria-label="Functions"
            tabIndex={0}
            aria-activedescendant={keys.rowId(keys.active)}
            onKeyDown={(event) =>
              keys.onKeyDown(event, (key) => {
                if (key !== "Enter") {
                  return false;
                }
                select(keys.active);
                return true;
              })
            }
          >
            <div className="tree-rows" style={{ height: virtual.getTotalSize() }}>
              {virtual.getVirtualItems().map((item) => {
                const row = rows[item.index];
                const key = row ? names[row.key] : undefined;
                if (!row || !key) {
                  return null;
                }
                const selected = look.fn === key.name;
                const own = formatPercent(row.own, weight);
                const total = formatPercent(row.total, weight);
                const change = base ? shareChange(row.own, weight, row.baseOwn, base.weight) : null;
                return (
                  // biome-ignore lint/a11y/useFocusableInteractive: the list keeps focus and names its active row
                  // biome-ignore lint/a11y/useKeyWithClickEvents: the list takes the keys for its rows
                  <div
                    // Functions of one name in two modules are two rows.
                    key={row.key}
                    id={keys.rowId(item.index)}
                    role="option"
                    aria-selected={selected}
                    aria-label={`${key.name}${key.module ? ` in ${key.module}` : ""}, ${own} own, ${total} total${base ? `, ${formatChange(change)} change` : ""}`}
                    className={`row tree-row ${selected ? "selected" : ""} ${item.index === keys.active ? "active" : ""}`}
                    style={{ transform: `translateY(${item.start}px)` }}
                    title={formatWeight(row.total, unit)}
                    onClick={() => {
                      keys.choose(item.index);
                      select(item.index);
                    }}
                  >
                    <span className="percent">{own}</span>
                    <span className="percent dim">{total}</span>
                    {base ? <Change points={change} /> : null}
                    <span className="name">{key.name}</span>
                    {key.module ? <span className="end">{key.module}</span> : null}
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </div>
      {look.fn ? <Selected name={look.fn} /> : null}
    </div>
  );
}

const PANELS = [
  [undefined, "Callers and callees"],
  ["lines", "Source"],
  ["code", "Assembly"],
] as const;

/** The selected function: its callers and callees, or its samples on its
 * source lines or on its instructions. */
function Selected({ name }: { name: string }) {
  const { query, look, setLook } = useProfile();
  return (
    <div className="profile-selected">
      <div className="pane-head">
        <div className="segmented">
          {PANELS.map(([ann, label]) => (
            <button
              key={label}
              type="button"
              aria-pressed={look.ann === ann}
              onClick={() => setLook((look) => withLook(look, "ann", ann), true)}
            >
              {label}
            </button>
          ))}
        </div>
        {look.ann && query.transforms.length > 0 ? (
          <span className="muted">Every selected sample counts here, without the transforms.</span>
        ) : null}
      </div>
      {look.ann === "lines" ? (
        <SourceLines name={name} />
      ) : look.ann === "code" ? (
        <Code name={name} />
      ) : (
        <Sandwich query={query} name={name} />
      )}
    </div>
  );
}

/** A function's callers, inverted from it, and its callees, from it. */
function Sandwich({ query, name }: { query: ProfileQuery; name: string }) {
  const focused = useMemo(
    () => ({ ...query, transforms: [...query.transforms, { op: "focus" as const, name }] }),
    [query, name],
  );
  const callers = useTree(useMemo(() => ({ ...focused, invert: !query.invert }), [focused, query]));
  const callees = useTree(focused);
  return (
    <div className="profile-sandwich" data-testid="sandwich">
      <div className="pane grow">
        <div className="pane-head">
          Callers of <span className="subject">{name}</span>
        </div>
        <TreeRows tree={callers} label="Callers" reading={`Reading the callers of ${name}…`} />
      </div>
      <div className="pane grow">
        <div className="pane-head">
          Callees of <span className="subject">{name}</span>
        </div>
        <TreeRows tree={callees} label="Callees" reading={`Reading the callees of ${name}…`} />
      </div>
    </div>
  );
}
