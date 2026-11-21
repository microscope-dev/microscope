import type { JSX } from "@solidjs/web";
import { createEffect, createMemo, createSignal, For, Match, Show, Switch } from "solid-js";

import { toggleBreakpoint } from "../breakpoints";
import { useRequest } from "../data";
import { type Look, stringifySearch } from "../focus";
import { controls } from "../model";
import type { Breakpoint, Disassembled, Instruction, SourceLine, Syntax } from "../protocol";
import { isString, read, write } from "../storage";
import { useConnection, useModel } from "../store";
import { createVirtualizer } from "../virtual";
import { useLook, useShowSource } from "./navigation";
import { fileName } from "./paths";
import { errorClass, hidden } from "./Values";
import { useFocus } from "./Workspace";

function useSyntax(): [() => Syntax, (syntax: Syntax) => void] {
  const [syntax, setSyntax] = createSignal<Syntax>(
    read(
      "microscope-syntax",
      "intel",
      (value): value is Syntax => isString(value) && value === "att",
    ),
  );
  const choose = (chosen: Syntax) => {
    setSyntax(chosen);
    write("microscope-syntax", chosen);
  };
  return [syntax, choose];
}

export function Disassembly() {
  const focus = useFocus();
  const [syntax, setSyntax] = useSyntax();
  const why = createMemo(() => hidden(focus(), "Instructions"));
  const answer = useRequest("disassemble", () => {
    const current = focus();
    return why() === null && current.at
      ? { ...current.at, address: current.look.asm ?? null, syntax: syntax() }
      : null;
  });
  const disassembled = createMemo(() => answer().data);
  const [body, setBody] = createSignal<HTMLDivElement | null>(null);

  return (
    <div class="asm" data-testid="disassembly">
      <div class="view-head">
        <span class="asm-function" data-testid="disassembly-function">
          {disassembled()?.function ?? (focus().look.asm ? "no function" : "")}
        </span>
        <Show when={focus().look.asm}>{(asm) => <span class="dim">{asm()}</span>}</Show>
        <fieldset class="segmented small" aria-label="Syntax">
          <For each={SYNTAXES}>
            {(choice) => (
              <SyntaxButton syntax={choice} pressed={syntax() === choice} choose={setSyntax} />
            )}
          </For>
        </fieldset>
      </div>
      <div
        class={["asm-body", { stale: !answer().current }]}
        ref={(node) => {
          setBody(node);
        }}
      >
        <Switch fallback={<div class="empty center-message">Disassembling…</div>}>
          <Match when={why()}>
            {(reason) => <div class="empty center-message">{reason()}</div>}
          </Match>
          <Match when={answer().error}>
            {(err) => <div class={[errorClass(err()), "center-message"]}>{err().message}</div>}
          </Match>
          <Match when={disassembled()}>
            {(data) => (
              <Lines
                disassembled={data()}
                body={body}
                current={answer().current}
                innermost={focus().at?.frame === 0}
              />
            )}
          </Match>
        </Switch>
      </div>
    </div>
  );
}

const SYNTAXES = ["intel", "att"] as const;

function SyntaxButton(props: {
  syntax: Syntax;
  pressed: boolean;
  choose: (syntax: Syntax) => void;
}) {
  return (
    <button
      type="button"
      aria-pressed={props.pressed ? "true" : "false"}
      onClick={() => props.choose(props.syntax)}
    >
      {props.syntax === "intel" ? "Intel" : "AT&T"}
    </button>
  );
}

const LINE = 20;

type Line =
  | { kind: "note"; key: string; note: string }
  | { kind: "source"; key: string; source: SourceLine }
  | { kind: "instruction"; key: string; instruction: Instruction };

function Lines(props: {
  disassembled: Disassembled;
  body: () => HTMLDivElement | null;
  current: boolean;
  innermost: boolean;
}) {
  const lines = createMemo(() => {
    const built: Line[] = props.disassembled.notes.map((note, index) => ({
      kind: "note",
      key: `note:${index}`,
      note,
    }));
    for (const instruction of props.disassembled.instructions) {
      if (instruction.source) {
        built.push({
          kind: "source",
          key: `source:${instruction.address}`,
          source: instruction.source,
        });
      }
      built.push({ kind: "instruction", key: instruction.address, instruction });
    }
    return built;
  });

  const rows = createVirtualizer(() => {
    const element = props.body();
    return {
      count: lines().length,
      getScrollElement: () => element,
      estimateSize: () => LINE,
      overscan: 40,
    };
  });

  const scrollTarget = createMemo(() => ({
    marked: props.current ? props.disassembled.marked : undefined,
    current: props.current,
    lines: lines(),
  }));

  createEffect(
    () => scrollTarget(),
    ({ marked, current, lines: list }) => {
      const index = marked
        ? list.findIndex((line) => line.kind === "instruction" && line.key === marked)
        : -1;
      if (index >= 0) {
        rows.scrollToIndex(index, { align: "center" });
      } else if (current) {
        rows.scrollToOffset(0);
      }
    },
  );

  const start = createMemo(() =>
    props.disassembled.function ? props.disassembled.instructions[0]?.address : undefined,
  );

  const rendered = createMemo(() => {
    const list = lines();
    return rows.items().flatMap((item) => {
      const line = list[item.index];
      return line ? [{ key: line.key, start: item.start, line }] : [];
    });
  });

  return (
    <div class="asm-lines" style={{ height: `${rows.totalSize()}px` }}>
      <For each={rendered()} keyed={(entry) => entry.key}>
        {(entry) => (
          <Slot start={entry().start}>
            <Switch>
              <Match when={entry().line.kind === "note" ? entry().line : null}>
                {(line) => (
                  <div class="asm-note">{(line() as Extract<Line, { kind: "note" }>).note}</div>
                )}
              </Match>
              <Match when={entry().line.kind === "source" ? entry().line : null}>
                {(line) => (
                  <SourceHead source={(line() as Extract<Line, { kind: "source" }>).source} />
                )}
              </Match>
              <Match when={entry().line.kind === "instruction" ? entry().line : null}>
                {(line) => (
                  <Row
                    instruction={(line() as Extract<Line, { kind: "instruction" }>).instruction}
                    start={start()}
                    marked={
                      (line() as Extract<Line, { kind: "instruction" }>).instruction.address ===
                      props.disassembled.marked
                    }
                    innermost={props.innermost}
                  />
                )}
              </Match>
            </Switch>
          </Slot>
        )}
      </For>
    </div>
  );
}

function Slot(props: { start: number; children: JSX.Element }) {
  return (
    <div class="asm-slot" style={{ transform: `translateY(${props.start}px)` }}>
      {props.children}
    </div>
  );
}

function Row(props: {
  instruction: Instruction;
  start: string | undefined;
  marked: boolean;
  innermost: boolean;
}) {
  const offset = createMemo(() =>
    props.start ? BigInt(props.instruction.address) - BigInt(props.start) : null,
  );
  return (
    <div
      class={["asm-row", props.marked ? (props.innermost ? "pc" : "frame") : ""]}
      data-address={props.instruction.address}
      aria-current={props.marked ? "true" : undefined}
    >
      <Gutter address={props.instruction.address} />
      <span class="asm-mark" aria-hidden="true">
        {props.marked ? "▶" : ""}
      </span>
      <span class="asm-address">{props.instruction.address}</span>
      <span class="asm-offset">
        {offset() === null ? (props.instruction.symbol ?? "") : `+${offset()}`}
      </span>
      <span class="asm-bytes">{props.instruction.bytes}</span>
      <span class="asm-text">
        <Show
          when={props.instruction.invalid}
          fallback={
            <For each={props.instruction.tokens} keyed={false}>
              {(token) => <span class={`tk-${token().kind}`}>{token().text}</span>}
            </For>
          }
        >
          {(invalid) => <span class="error">{invalid()}</span>}
        </Show>
        <Show when={props.instruction.target?.name ? props.instruction.target : null}>
          {(target) => <Target address={target().address} name={target().name} />}
        </Show>
        <Show when={props.instruction.comment}>
          {(comment) => <span class="asm-comment"> ; {comment()}</span>}
        </Show>
      </span>
    </div>
  );
}

function SourceHead(props: { source: SourceLine }) {
  const showSource = useShowSource();
  const request = useRequest("source", () => ({ path: props.source.path }));
  const line = createMemo(() => {
    const text = request().data;
    return text?.path === props.source.path
      ? text.text.split("\n")[props.source.line - 1]
      : undefined;
  });
  const place = createMemo(() => `${fileName(props.source.path)}:${props.source.line}`);
  return (
    <button
      type="button"
      class="asm-source"
      aria-label={`Show the source at ${place()}`}
      onClick={() => showSource(props.source.path, props.source.line)}
    >
      <span class="asm-place">{place()}</span>
      <span class="asm-line">{line()?.trim()}</span>
    </button>
  );
}

function Target(props: { address: string; name: string | null }) {
  const look = useLook();
  const focus = useFocus();
  const there = (from: Look): Look => ({ ...from, asm: props.address, view: "disassembly" });
  const follow = (event: MouseEvent) => {
    if (event.button === 0 && !event.ctrlKey && !event.metaKey && !event.shiftKey) {
      event.preventDefault();
      look(there, { replace: false });
    }
  };
  return (
    <a
      class="asm-target"
      href={window.location.pathname + stringifySearch({ ...there(focus().look) })}
      title={props.address}
      onClick={follow}
    >
      {props.name}
    </a>
  );
}

function Gutter(props: { address: string }) {
  const focus = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const existing = createMemo(() =>
    focus().state.breakpoints.find((breakpoint: Breakpoint) =>
      breakpoint.places.some((place) => place.address === props.address),
    ),
  );
  const toggle = () => {
    void toggleBreakpoint(connection, existing(), props.address);
  };
  return (
    <Show
      when={control()}
      fallback={
        <span class="asm-gutter">
          <span class={existing() ? "dot plain" : "dot none"} aria-hidden="true" />
        </span>
      }
    >
      <button
        type="button"
        class="asm-gutter"
        aria-label={`${existing() ? "Remove the breakpoint" : "Breakpoint"} at ${props.address}`}
        onClick={toggle}
      >
        <span class={existing() ? "dot plain" : "dot none"} aria-hidden="true" />
      </button>
    </Show>
  );
}
