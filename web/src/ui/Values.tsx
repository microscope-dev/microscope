// The right column: watches, which the link carries, and the frame's
// variables. Both read the stop the tab shows, and only while it is the
// program's: a passed stop's values are gone, and a running program's are
// moving.

import { useState } from "react";
import { useRequest } from "../data";
import type { At } from "../focus";
import { hidden } from "./hidden";
import { useLook } from "./navigation";
import { Earlier, LinkedExpansion, messageRow, RowList, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

// Disassembly and Drawings still import these from here.
export { errorClass, hidden } from "./hidden";

export function Variables() {
  const focus = useFocus();
  const why = hidden(focus);
  const scopes = useRequest("scopes", why === null ? (focus.at as At) : null);
  const statics = focus.look.x?.includes("statics") ?? false;
  const look = useLook();

  let body: React.ReactNode;
  if (why) {
    body = <div className="empty">{why}</div>;
  } else if (scopes.error) {
    body = <div className="empty error">{scopes.error.message}</div>;
  } else if (!scopes.data) {
    body = <div className="empty">Reading variables…</div>;
  } else {
    body = (
      <Earlier when={!scopes.current}>
        <ul className="value-list" aria-label="Variables" data-testid="variables">
          {scopes.data.scopes.map((scope) => {
            const closable = scope.key === "statics";
            const open = !closable || statics;
            return (
              <li key={scope.key} className="scope">
                <div className="scope-head">
                  {closable ? (
                    <button
                      type="button"
                      className="twist"
                      aria-label={`${open ? "Close" : "Open"} ${scope.name}`}
                      aria-expanded={open}
                      onClick={() =>
                        look((current) => {
                          const x = (current.x ?? []).filter((key) => key !== "statics");
                          const next = open ? x : [...x, "statics"];
                          const { x: _x, ...rest } = current;
                          return next.length > 0 ? { ...rest, x: next } : rest;
                        })
                      }
                    >
                      {open ? "▾" : "▸"}
                    </button>
                  ) : null}
                  {scope.name}
                  {scope.rows.length > 0 && <span className="count">{scope.rows.length}</span>}
                </div>
                {open && (
                  <ul className="value-list">
                    {scope.problem && <li className="empty error">{scope.problem}</li>}
                    {scope.note && <li className="empty">{scope.note}</li>}
                    {scope.rows.length === 0 && !scope.problem && <li className="empty">none</li>}
                    <RowList rows={scope.rows} parent={scope.key} depth={0} />
                  </ul>
                )}
              </li>
            );
          })}
        </ul>
      </Earlier>
    );
  }
  return (
    <section className="pane grow" aria-label="Variables" data-pane="5" tabIndex={-1}>
      <div className="pane-head">
        Variables
        <span className="end">Alt+5</span>
      </div>
      <div className="pane-body">
        <LinkedExpansion>{body}</LinkedExpansion>
      </div>
    </section>
  );
}

export function Watch() {
  const focus = useFocus();
  const watches = focus.look.w ?? [];
  const look = useLook();
  const [text, setText] = useState("");
  const why = hidden(focus);

  const remove = (index: number) =>
    look((current) => {
      const w = (current.w ?? []).filter((_, at) => at !== index);
      const { w: _w, ...rest } = current;
      return w.length > 0 ? { ...rest, w } : rest;
    });

  return (
    <section className="pane watch" aria-label="Watch" data-pane="4" tabIndex={-1}>
      <div className="pane-head">
        Watch
        {watches.length > 0 && <span className="count">{watches.length}</span>}
        <span className="end">Alt+4</span>
      </div>
      <div className="pane-body">
        {why && watches.length > 0 && <div className="empty">{why}</div>}
        <LinkedExpansion>
          <ul className="value-list" aria-label="Watches" data-testid="watches">
            {watches.map((expression, index) => {
              // A repeated watch is its own row, as a repeated name is.
              const occurrence = watches.slice(0, index).filter((w) => w === expression).length + 1;
              return (
                <WatchRow
                  key={`${expression}~${occurrence}`}
                  expression={expression}
                  occurrence={occurrence}
                  hidden={why}
                  onRemove={() => remove(index)}
                />
              );
            })}
          </ul>
        </LinkedExpansion>
        <form
          className="watch-add"
          onSubmit={(event) => {
            event.preventDefault();
            const expression = text.trim();
            if (expression) {
              look((current) => ({ ...current, w: [...(current.w ?? []), expression] }));
              setText("");
            }
          }}
        >
          <input
            className="input quiet"
            aria-label="Add a watch"
            placeholder="+ expression"
            value={text}
            onChange={(event) => setText(event.target.value)}
          />
        </form>
      </div>
    </section>
  );
}

function WatchRow({
  expression,
  occurrence,
  hidden,
  onRemove,
}: {
  expression: string;
  occurrence: number;
  hidden: string | null;
  onRemove(): void;
}) {
  const { at } = useFocus();
  const value = useRequest("evaluate", hidden === null && at ? { ...at, expression } : null);
  const remove = (
    <button
      type="button"
      className="icon remove"
      aria-label={`Remove the watch ${expression}`}
      title="Remove"
      onClick={onRemove}
    >
      ×
    </button>
  );
  if (hidden !== null) {
    return (
      <ValueRow
        row={messageRow(expression, "not read")}
        parent="w"
        depth={0}
        occurrence={occurrence}
        actions={remove}
        missing="absent"
      />
    );
  }
  if (value.error) {
    return (
      <ValueRow
        row={messageRow(expression, value.error.message)}
        parent="w"
        depth={0}
        occurrence={occurrence}
        actions={remove}
        missing="failed"
      />
    );
  }
  if (!value.data) {
    return (
      <Earlier when>
        <ValueRow
          row={messageRow(expression, "…")}
          parent="w"
          depth={0}
          occurrence={occurrence}
          actions={remove}
        />
      </Earlier>
    );
  }
  return (
    <Earlier when={!value.current}>
      <ValueRow row={value.data} parent="w" depth={0} occurrence={occurrence} actions={remove} />
    </Earlier>
  );
}
