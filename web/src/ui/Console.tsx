// The console: expressions and microscope's commands, in the frame the tab
// shows. Tab completes from the frame's names, members, and registers, and
// the arrow keys walk the lines run before, or the completions while they
// are open.

import { useId, useRef, useState } from "react";
import { useStore } from "zustand";
import { commonPrefix, consoleStore, finished, started } from "../console";
import type { Completion } from "../protocol";
import { useConnection } from "../store";
import { useFollowBottom } from "./scroll";
import { LocalExpansion, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

export function Console() {
  const entries = useStore(consoleStore, (state) => state.entries);
  // A line run follows to the bottom; answers to older lines do not pull
  // a reader who scrolled up.
  const body = useFollowBottom<HTMLDivElement>(entries, entries.length);

  return (
    <>
      <div
        className="pane-body console"
        ref={body.ref}
        onScroll={body.onScroll}
        data-testid="console"
      >
        {entries.length === 0 ? (
          <div className="empty">
            Expressions and microscope's commands run in the frame shown. Tab completes; ↑ and ↓
            recall.
          </div>
        ) : (
          <LocalExpansion>
            <ul className="console-entries">
              {entries.map((entry) => (
                <li key={entry.id} className="console-entry">
                  <div className="console-line">
                    <span className="prompt">›</span> {entry.line}
                  </div>
                  {entry.error !== undefined && <pre className="error">{entry.error}</pre>}
                  {entry.result?.output != null && <pre>{entry.result.output}</pre>}
                  {entry.result?.row && (
                    <ul className="value-list">
                      <ValueRow row={entry.result.row} parent={`c${entry.id}`} depth={0} />
                    </ul>
                  )}
                </li>
              ))}
            </ul>
          </LocalExpansion>
        )}
      </div>
      <ConsoleInput />
    </>
  );
}

interface Choices {
  start: number;
  items: Completion[];
  /** The completion the arrow keys are on, if they moved. */
  active: number | null;
}

function ConsoleInput() {
  const { at, stale } = useFocus();
  const connection = useConnection();
  const history = useStore(consoleStore, (state) => state.history);
  const [text, setText] = useState("");
  const [recalled, setRecalled] = useState<number | null>(null);
  const [choices, setChoices] = useState<Choices | null>(null);
  const field = useRef<HTMLInputElement>(null);
  const list = useId();
  // Lines run at the frame shown, while its stop is the program's.
  const frame = at && !stale ? at : null;

  const run = () => {
    const line = text.trim();
    if (!line) {
      return;
    }
    setText("");
    setRecalled(null);
    setChoices(null);
    const id = started(line, frame?.stop ?? null);
    connection
      .request("console", frame ? { line, ...frame } : { line })
      .then((result) => finished(id, { result }))
      .catch((failure: Error) => finished(id, { error: failure.message }));
  };

  const insert = (start: number, label: string, cursor: number) => {
    const before = [...text.slice(0, cursor)];
    const replaced = before.slice(0, start).join("") + label;
    setText(replaced + text.slice(cursor));
    requestAnimationFrame(() => field.current?.setSelectionRange(replaced.length, replaced.length));
  };

  const take = (item: Completion) => {
    if (choices) {
      insert(choices.start, item.label, field.current?.selectionStart ?? text.length);
      setChoices(null);
    }
  };

  const complete = () => {
    const cursor = field.current?.selectionStart ?? text.length;
    connection
      .request(
        "complete",
        frame ? { text: text.slice(0, cursor), ...frame } : { text: text.slice(0, cursor) },
      )
      .then(({ start, items }) => {
        if (items.length === 0) {
          setChoices(null);
          return;
        }
        if (items.length === 1) {
          insert(start, (items[0] as Completion).label, cursor);
          setChoices(null);
          return;
        }
        const typed = [...text.slice(0, cursor)].slice(start).join("");
        const shared = commonPrefix(items.map((item) => item.label));
        if (shared.length > typed.length) {
          insert(start, shared, cursor);
        }
        setChoices({ start, items: items.slice(0, 60), active: null });
      })
      .catch(() => setChoices(null));
  };

  const recall = (step: -1 | 1) => {
    if (history.length === 0) {
      return;
    }
    const index =
      recalled === null
        ? step < 0
          ? history.length - 1
          : null
        : recalled + step >= history.length
          ? null
          : Math.max(0, recalled + step);
    setRecalled(index);
    setText(index === null ? "" : (history[index] as string));
  };

  const walk = (step: -1 | 1) => {
    if (!choices) {
      return;
    }
    const count = choices.items.length;
    const active =
      choices.active === null
        ? step > 0
          ? 0
          : count - 1
        : (choices.active + step + count) % count;
    setChoices({ ...choices, active });
  };

  const active = choices?.active ?? null;
  return (
    <form
      className="console-input"
      onSubmit={(event) => {
        event.preventDefault();
        run();
      }}
    >
      {choices && (
        <div className="completions" id={list} role="listbox" aria-label="Completions">
          {choices.items.map((item, index) => (
            <div
              key={item.label}
              id={`${list}-${index}`}
              role="option"
              aria-selected={index === active}
              tabIndex={-1}
              className={index === active ? "on" : ""}
              // The field keeps focus, and the keys the field takes choose;
              // a press here chooses before the field could lose it.
              onMouseDown={(event) => {
                event.preventDefault();
                take(item);
              }}
            >
              {item.label}
              <span className="dim"> {item.kind}</span>
            </div>
          ))}
        </div>
      )}
      <span className="prompt" aria-hidden="true">
        ›
      </span>
      <input
        ref={field}
        className="input quiet"
        aria-label="Console"
        aria-controls={choices ? list : undefined}
        aria-activedescendant={active === null ? undefined : `${list}-${active}`}
        data-pane="6"
        placeholder={
          frame
            ? `an expression or command, in frame ${frame.frame}`
            : "a command; expressions need a stop"
        }
        value={text}
        autoComplete="off"
        spellCheck={false}
        onChange={(event) => {
          setText(event.target.value);
          setChoices(null);
        }}
        onKeyDown={(event) => {
          if (event.key === "Tab" && !event.shiftKey) {
            event.preventDefault();
            complete();
          } else if (event.key === "ArrowUp" || event.key === "ArrowDown") {
            event.preventDefault();
            const step = event.key === "ArrowUp" ? -1 : 1;
            if (choices) {
              walk(step);
            } else {
              recall(step);
            }
          } else if (event.key === "Enter" && choices && active !== null) {
            event.preventDefault();
            take(choices.items[active] as Completion);
          } else if (event.key === "Escape" && choices) {
            event.stopPropagation();
            setChoices(null);
          }
        }}
      />
    </form>
  );
}
