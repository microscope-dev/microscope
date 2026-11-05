// The console: expressions and microscope's commands, in the frame the tab
// shows, and run control, which the page runs itself (`console.ts`). Tab
// completes from the frame's names, members, and registers, and the arrow
// keys walk the lines run before, or the completions while they are open. An
// empty line repeats the last step, and Ctrl+L clears.

import {
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
  type MouseEvent,
  useCallback,
  useId,
  useRef,
  useState,
} from "react";
import { useStore } from "zustand";
import type { StoreApi } from "zustand";

import { action } from "../actions";
import { assert } from "../assert";
import type { Connection } from "../connection";
import {
  asked,
  clearConsole,
  commonPrefix,
  consoleStore,
  finished,
  outcome,
  repeats,
  RUN_HELP,
  started,
} from "../console";
import type { At, Look } from "../focus";
import type { Model } from "../model";
import type { Completion } from "../protocol";
import { useConnection, useStoreApi } from "../store";
import { useLook } from "./navigation";
import { useFollowBottom } from "./scroll";
import { LocalExpansion, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

export function Console() {
  const entries = useStore(consoleStore, (state) => state.entries);
  // A line run follows to the bottom; answers to older lines do not pull
  // a reader who scrolled up.
  const { ref, onScroll } = useFollowBottom<HTMLDivElement>(entries, entries.length);

  return (
    <>
      <div className="pane-body console" ref={ref} onScroll={onScroll} data-testid="console">
        {entries.length === 0 ? (
          <div className="empty">
            Expressions and microscope&apos;s commands run in the frame shown, and next, step,
            continue, and finish run the program. Tab completes; ↑ and ↓ recall; help lists the
            commands.
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
                  {entry.said !== undefined && <div className="console-said">{entry.said}</div>}
                  {typeof entry.result?.output === "string" && <pre>{entry.result.output}</pre>}
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
  const store = useStoreApi();
  const look = useLook();
  const history = useStore(consoleStore, (state) => state.history);
  const [text, setText] = useState("");
  const [recalled, setRecalled] = useState<number | null>(null);
  const [choices, setChoices] = useState<Choices | null>(null);
  const field = useRef<HTMLInputElement>(null);
  const list = useId();
  // Lines run at the frame shown, while its stop is the program's.
  const frame = at && !stale ? at : null;

  const run = useCallback(() => {
    const typed = text.trim();
    // An empty line repeats the last step or continue, and nothing else.
    const last = consoleStore.getState().entries.at(-1)?.line;
    const line = typed || (last !== undefined && repeats(last) ? last : "");
    if (!line) {
      return;
    }
    setText("");
    setRecalled(null);
    setChoices(null);
    runLine(line, { store, connection, at, frame, look });
  }, [text, at, frame, store, connection, look]);

  const insert = useCallback(
    (start: number, label: string, cursor: number) => {
      const before = Array.from(text.slice(0, cursor));
      const replaced = before.slice(0, start).join("") + label;
      setText(replaced + text.slice(cursor));
      requestAnimationFrame(() =>
        field.current?.setSelectionRange(replaced.length, replaced.length),
      );
    },
    [text],
  );

  const take = useCallback(
    (item: Completion) => {
      if (choices) {
        insert(choices.start, item.label, field.current?.selectionStart ?? text.length);
        setChoices(null);
      }
    },
    [choices, insert, text.length],
  );

  const complete = useCallback(() => {
    const cursor = field.current?.selectionStart ?? text.length;
    const ask = async () => {
      const { start, items } = await connection.request(
        "complete",
        frame ? { text: text.slice(0, cursor), ...frame } : { text: text.slice(0, cursor) },
      );
      if (items.length === 0) {
        setChoices(null);
        return;
      }
      if (items.length === 1) {
        insert(start, (items[0] as Completion).label, cursor);
        setChoices(null);
        return;
      }
      const typed = Array.from(text.slice(0, cursor)).slice(start).join("");
      const shared = commonPrefix(items.map((item) => item.label));
      if (shared.length > typed.length) {
        insert(start, shared, cursor);
      }
      setChoices({ start, items: items.slice(0, 60), active: null });
    };
    ask().catch(() => setChoices(null));
  }, [text, frame, connection, insert]);

  const recall = useCallback(
    (step: -1 | 1) => {
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
      const recalledLine = index === null ? "" : history[index];
      assert(recalledLine !== undefined, `recalled line ${index} of ${history.length}`);
      setRecalled(index);
      setText(recalledLine);
    },
    [history, recalled],
  );

  const walk = useCallback(
    (step: -1 | 1) => {
      if (!choices) {
        return;
      }
      const count = choices.items.length;
      assert(count >= 2, `choices kept for ${count} completions`);
      const active =
        choices.active === null
          ? step > 0
            ? 0
            : count - 1
          : (choices.active + step + count) % count;
      assert(active >= 0 && active < count, `active completion ${active} of ${count}`);
      setChoices({ ...choices, active });
    },
    [choices],
  );

  const submit = useCallback(
    (event: FormEvent) => {
      event.preventDefault();
      run();
    },
    [run],
  );

  const edit = useCallback((event: ChangeEvent<HTMLInputElement>) => {
    setText(event.target.value);
    setChoices(null);
  }, []);

  const active = choices?.active ?? null;
  const key = useCallback(
    (event: KeyboardEvent) => {
      if (event.key === "l" && event.ctrlKey && !event.altKey && !event.metaKey) {
        event.preventDefault();
        clearConsole();
      } else if (event.key === "Tab" && !event.shiftKey) {
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
        const item = choices.items[active];
        assert(item !== undefined, `active completion ${active} of ${choices.items.length}`);
        take(item);
      } else if (event.key === "Escape" && choices) {
        event.stopPropagation();
        setChoices(null);
      }
    },
    [complete, choices, walk, recall, active, take],
  );
  return (
    <form className="console-input" onSubmit={submit}>
      {choices && (
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- the input's own completions popup, which a select cannot be
        <div className="completions" id={list} role="listbox" aria-label="Completions">
          {choices.items.map((item, index) => (
            <Choice
              key={item.label}
              id={`${list}-${index}`}
              item={item}
              active={index === active}
              take={take}
            />
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
        data-autofocus
        placeholder={
          frame
            ? `an expression or command, in frame ${frame.frame}`
            : "a command; expressions need a stop"
        }
        value={text}
        autoComplete="off"
        spellCheck={false}
        onChange={edit}
        onKeyDown={key}
      />
    </form>
  );
}

interface Context {
  store: StoreApi<Model>;
  connection: Connection;
  /** Where the tab looks, even at a stop the program has left. */
  at: At | null;
  /** Where lines run: `at`, while its stop is the program's. */
  frame: At | null;
  look: ReturnType<typeof useLook>;
}

/** Runs one console line, recording it and its answer. */
function runLine(line: string, { store, connection, at, frame, look }: Context): void {
  const wanted = asked(line);
  if (wanted.kind === "clear") {
    clearConsole();
    return;
  }
  const id = started(line, frame?.stop ?? null);
  const fail = (failure: Error) => finished(id, { error: failure.message });
  switch (wanted.kind) {
    case "usage":
      finished(id, { error: wanted.message });
      return;
    case "disassemble": {
      // The view disassembles at the frame the tab shows, so it needs one.
      if (!at) {
        finished(id, { error: "the program is not stopped" });
        return;
      }
      const show = (address: string | null, said: string) => {
        look((current) => showingCode(current, address), { replace: false });
        finished(id, { said });
      };
      const target = wanted.target;
      if (target === null) {
        show(null, "the frame's code, in Disassembly");
      } else if ("address" in target) {
        show(target.address, `${target.address}, in Disassembly`);
      } else {
        connection
          .request("resolve", { name: target.name })
          .then(({ address }) => {
            show(address, `${target.name} at ${address}, in Disassembly`);
            return null;
          })
          .catch(fail);
      }
      return;
    }
    case "server":
      connection
        .request("console", frame ? { line, ...frame } : { line })
        .then((result) => {
          // Plain help lists microscope's commands; the page adds its own.
          const help = /^(?:help|h|\?)$/u.test(line) && typeof result.output === "string";
          finished(id, { result: help ? { ...result, output: result.output + RUN_HELP } : result });
          return null;
        })
        .catch(fail);
      return;
    case "action":
    case "start": {
      const model = store.getState();
      const inferior = model.state?.inferior.state;
      const name =
        wanted.kind === "action"
          ? wanted.action
          : inferior === "notStarted" || inferior === "exited"
            ? "continue"
            : "restart";
      const chosen = action(name, model, at);
      if (!chosen.enabled) {
        finished(id, { error: chosen.reason });
        return;
      }
      follow(store, id, () => chosen.run(connection)).catch(fail);
      return;
    }
    case "advance":
    case "into": {
      // Both move the stopped thread shown, as a step does.
      const model = store.getState();
      const step = action("over", model, at);
      const inferior = model.state?.inferior;
      if (!step.enabled || inferior?.state !== "stopped") {
        finished(id, { error: step.enabled ? "the program is not stopped" : step.reason });
        return;
      }
      const stop = inferior.stop;
      const thread = at?.thread ?? inferior.thread;
      const task = at?.task ?? null;
      if (wanted.kind === "advance") {
        const location = wanted.location;
        const frameShown = at?.frame ?? 0;
        follow(store, id, () =>
          connection.request("advance", { stop, thread, task, frame: frameShown, location }),
        ).catch(fail);
        return;
      }
      const call = wanted.call;
      connection
        .request("stepTargets", { stop, thread, task })
        .then(({ calls }) => {
          const address = call.startsWith("*") ? call.slice(1).toLowerCase() : null;
          const chosen = calls.find((candidate) =>
            address === null
              ? candidate.callee === call
              : BigInt(candidate.call) === BigInt(address),
          );
          if (!chosen) {
            const named = calls.map((candidate) => candidate.callee ?? candidate.call);
            throw new Error(
              named.length === 0
                ? "this line calls nothing to step into"
                : `this line calls no ${call}; it calls ${named.join(", ")}`,
            );
          }
          return follow(store, id, () =>
            connection.request("step", { stop, thread, task, kind: "into", call: chosen.call }),
          );
        })
        .catch(fail);
    }
  }
}

/** The Disassembly view, at `address` or else the frame's own code. */
function showingCode(current: Look, address: string | null): Look {
  const { asm: _asm, ...kept } = current;
  return { ...kept, ...(address === null ? {} : { asm: address }), view: "disassembly" };
}

/**
 * Sends a run-control request and says in entry `id` where the program
 * stops after it. It watches from before the request goes, since the new
 * state can arrive before the answer.
 */
function follow(store: StoreApi<Model>, id: number, send: () => Promise<unknown>): Promise<null> {
  const before = store.getState().state?.inferior;
  let settled = false;
  const unsubscribe = store.subscribe((model) => {
    const said = outcome(before, model.state?.inferior);
    if (said !== null) {
      settled = true;
      unsubscribe();
      finished(id, { said });
    }
  });
  return send().then(
    () => {
      if (!settled && store.getState().state?.inferior.state === "running") {
        finished(id, { said: "running…" });
      }
      return null;
    },
    (failure: unknown) => {
      unsubscribe();
      throw failure;
    },
  );
}

/** A completion, which a press takes. */
function Choice({
  id,
  item,
  active,
  take,
}: {
  id: string;
  item: Completion;
  active: boolean;
  take: (item: Completion) => void;
}) {
  // The field keeps focus, and the keys the field takes choose; a press
  // here chooses before the field could lose it.
  const press = useCallback(
    (event: MouseEvent) => {
      event.preventDefault();
      take(item);
    },
    [take, item],
  );
  return (
    <div
      id={id}
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an entry of the completions popup
      role="option"
      aria-selected={active}
      tabIndex={-1}
      className={active ? "on" : ""}
      onMouseDown={press}
    >
      {item.label}
      <span className="dim"> {item.kind}</span>
    </div>
  );
}
