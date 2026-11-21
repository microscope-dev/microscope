import { createEffect, createMemo, createSignal, createUniqueId, For, Show } from "solid-js";

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
import { type Store, useSelector } from "../state";
import { useConnection, useStoreApi } from "../store";
import { useLook } from "./navigation";
import { useFollowBottom } from "./scroll";
import { LocalExpansion, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

export function Console() {
  const entries = useSelector(consoleStore, (state) => state.entries);

  const { ref, onScroll } = useFollowBottom<HTMLDivElement>(entries, () => entries().length);

  return (
    <>
      <div class="pane-body console" ref={ref} onScroll={onScroll} data-testid="console">
        <Show
          when={entries().length > 0}
          fallback={
            <div class="empty">
              Expressions and microscope&apos;s commands run in the frame shown, and next, step,
              continue, and finish run the program. Tab completes; ↑ and ↓ recall; help lists the
              commands.
            </div>
          }
        >
          <LocalExpansion>
            <ul class="console-entries">
              <For each={entries()} keyed={(entry) => entry.id}>
                {(entry) => (
                  <li class="console-entry">
                    <div class="console-line">
                      <span class="prompt">›</span> {entry().line}
                    </div>
                    <Show when={entry().error !== undefined}>
                      <pre class="error">{entry().error}</pre>
                    </Show>
                    <Show when={entry().said !== undefined}>
                      <div class="console-said">{entry().said}</div>
                    </Show>
                    <Show when={typeof entry().result?.output === "string"}>
                      <pre>{entry().result?.output}</pre>
                    </Show>
                    <Show when={entry().result?.row}>
                      {(row) => (
                        <ul class="value-list">
                          <ValueRow row={row()} parent={`c${entry().id}`} depth={0} />
                        </ul>
                      )}
                    </Show>
                  </li>
                )}
              </For>
            </ul>
          </LocalExpansion>
        </Show>
      </div>
      <ConsoleInput />
    </>
  );
}

interface Choices {
  start: number;
  items: Completion[];

  active: number | null;
}

function ConsoleInput() {
  const focus = useFocus();
  const connection = useConnection();
  const store = useStoreApi();
  const look = useLook();
  const history = useSelector(consoleStore, (state) => state.history);
  const [text, setText] = createSignal("");
  const [recalled, setRecalled] = createSignal<number | null>(null);
  const [choices, setChoices] = createSignal<Choices | null>(null);
  let field: HTMLInputElement | undefined;
  const list = createUniqueId();

  const frame = createMemo(() => {
    const { at, stale } = focus();
    return at && !stale ? at : null;
  });

  const active = createMemo(() => choices()?.active ?? null);

  const placeholder = createMemo(() => {
    const current = frame();
    return current
      ? `an expression or command, in frame ${current.frame}`
      : "a command; expressions need a stop";
  });

  const run = () => {
    const typed = text().trim();
    const last = consoleStore.getState().entries.at(-1)?.line;
    const line = typed || (last !== undefined && repeats(last) ? last : "");
    if (!line) {
      return;
    }
    setText("");
    setRecalled(null);
    setChoices(null);
    runLine(line, { store, connection, at: focus().at, frame: frame(), look });
  };

  const insert = (start: number, label: string, cursor: number) => {
    const current = text();
    const before = Array.from(current.slice(0, cursor));
    const replaced = before.slice(0, start).join("") + label;
    setText(replaced + current.slice(cursor));
    requestAnimationFrame(() => field?.setSelectionRange(replaced.length, replaced.length));
  };

  const take = (item: Completion) => {
    const current = choices();
    if (current) {
      insert(current.start, item.label, field?.selectionStart ?? text().length);
      setChoices(null);
    }
  };

  const complete = () => {
    const currentText = text();
    const currentFrame = frame();
    const cursor = field?.selectionStart ?? currentText.length;
    const ask = async () => {
      const { start, items } = await connection.request(
        "complete",
        currentFrame
          ? { text: currentText.slice(0, cursor), ...currentFrame }
          : { text: currentText.slice(0, cursor) },
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
      const typed = Array.from(currentText.slice(0, cursor)).slice(start).join("");
      const shared = commonPrefix(items.map((item) => item.label));
      if (shared.length > typed.length) {
        insert(start, shared, cursor);
      }
      setChoices({ start, items: items.slice(0, 60), active: null });
    };
    ask().catch(() => setChoices(null));
  };

  const recall = (step: -1 | 1) => {
    const lines = history();
    if (lines.length === 0) {
      return;
    }
    const currentRecalled = recalled();
    const index =
      currentRecalled === null
        ? step < 0
          ? lines.length - 1
          : null
        : currentRecalled + step >= lines.length
          ? null
          : Math.max(0, currentRecalled + step);
    const recalledLine = index === null ? "" : lines[index];
    assert(recalledLine !== undefined, `recalled line ${index} of ${lines.length}`);
    setRecalled(index);
    setText(recalledLine);
  };

  const walk = (step: -1 | 1) => {
    const current = choices();
    if (!current) {
      return;
    }
    const count = current.items.length;
    assert(count >= 2, `choices kept for ${count} completions`);
    const next =
      current.active === null
        ? step > 0
          ? 0
          : count - 1
        : (current.active + step + count) % count;
    assert(next >= 0 && next < count, `active completion ${next} of ${count}`);
    setChoices({ ...current, active: next });
  };

  const submit = (event: SubmitEvent) => {
    event.preventDefault();
    run();
  };

  const edit = (event: InputEvent & { currentTarget: HTMLInputElement }) => {
    setText(event.currentTarget.value);
    setChoices(null);
  };

  const key = (event: KeyboardEvent) => {
    const currentChoices = choices();
    const currentActive = active();
    if (event.key === "l" && event.ctrlKey && !event.altKey && !event.metaKey) {
      event.preventDefault();
      clearConsole();
    } else if (event.key === "Tab" && currentChoices) {
      event.preventDefault();
      walk(event.shiftKey ? -1 : 1);
    } else if (event.key === "Tab" && !event.shiftKey) {
      event.preventDefault();
      complete();
    } else if (event.key === "ArrowUp" || event.key === "ArrowDown") {
      event.preventDefault();
      const step = event.key === "ArrowUp" ? -1 : 1;
      if (currentChoices) {
        walk(step);
      } else {
        recall(step);
      }
    } else if (event.key === "Enter" && currentChoices && currentActive !== null) {
      event.preventDefault();
      const item = currentChoices.items[currentActive];
      assert(
        item !== undefined,
        `active completion ${currentActive} of ${currentChoices.items.length}`,
      );
      take(item);
    } else if (event.key === "Escape" && currentChoices) {
      event.stopPropagation();
      setChoices(null);
    }
  };

  return (
    <form class="console-input" onSubmit={submit}>
      <Show when={choices()}>
        {(current) => (
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- the input's own completions popup, which a select cannot be
          <div class="completions" id={list} role="listbox" aria-label="Completions">
            <For each={current().items} keyed={(item) => item.label}>
              {(item, index) => (
                <Choice
                  id={`${list}-${index()}`}
                  item={item()}
                  active={index() === active()}
                  take={take}
                />
              )}
            </For>
          </div>
        )}
      </Show>
      <span class="prompt" aria-hidden="true">
        ›
      </span>
      <input
        ref={(node) => {
          field = node;
        }}
        class="input quiet"
        aria-label="Console"
        aria-controls={choices() ? list : undefined}
        aria-activedescendant={active() === null ? undefined : `${list}-${active()}`}
        data-autofocus
        placeholder={placeholder()}
        value={text()}
        autocomplete="off"
        spellcheck={false}
        onInput={edit}
        onKeyDown={key}
      />
    </form>
  );
}

interface Context {
  store: Store<Model>;
  connection: Connection;

  at: At | null;

  frame: At | null;
  look: ReturnType<typeof useLook>;
}

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
                : `this line does not call ${call}; it calls ${named.join(", ")}`,
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

function showingCode(current: Look, address: string | null): Look {
  const { asm: _asm, ...kept } = current;
  return { ...kept, ...(address === null ? {} : { asm: address }), view: "disassembly" };
}

function follow(store: Store<Model>, id: number, send: () => Promise<unknown>): Promise<null> {
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

function Choice(props: {
  id: string;
  item: Completion;
  active: boolean;
  take: (item: Completion) => void;
}) {
  let option: HTMLDivElement | undefined;

  createEffect(
    () => props.active,
    (isActive) => {
      if (isActive) {
        option?.scrollIntoView({ block: "nearest" });
      }
    },
  );

  const press = (event: MouseEvent) => {
    event.preventDefault();
    props.take(props.item);
  };

  return (
    <div
      ref={(node) => {
        option = node;
      }}
      id={props.id}
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an entry of the completions popup
      role="option"
      aria-selected={props.active ? "true" : "false"}
      tabindex={-1}
      class={{ on: props.active }}
      onMouseDown={press}
    >
      {props.item.label}
      <span class="dim"> {props.item.kind}</span>
    </div>
  );
}
