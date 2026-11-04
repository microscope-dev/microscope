// A function's instructions: the frame's own when the link names no
// address, with the instruction the frame executes marked. Source lines head
// the instructions compiled from them and lead back to the source; calls
// and jumps link where they go.

import { useVirtualizer } from "@tanstack/react-virtual";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { assert } from "../assert";
import { toggleBreakpoint } from "../breakpoints";
import { useRequest } from "../data";
import { type Look, stringifySearch } from "../focus";
import { controls } from "../model";
import type { Breakpoint, Disassembled, Instruction, SourceLine, Syntax } from "../protocol";
import { isString, read, write } from "../storage";
import { useConnection, useModel } from "../store";
import { useLook, useShowSource } from "./navigation";
import { fileName } from "./paths";
import { errorClass, hidden } from "./Values";
import { useFocus } from "./Workspace";

/** The syntax this browser reads, Intel unless AT&T was chosen. */
function useSyntax(): [Syntax, (syntax: Syntax) => void] {
  const [syntax, setSyntax] = useState<Syntax>(() =>
    read(
      "microscope-syntax",
      "intel",
      (value): value is Syntax => isString(value) && value === "att",
    ),
  );
  const choose = useCallback((chosen: Syntax) => {
    setSyntax(chosen);
    write("microscope-syntax", chosen);
  }, []);
  return [syntax, choose];
}

export function Disassembly() {
  const focus = useFocus();
  const { at, look } = focus;
  const [syntax, setSyntax] = useSyntax();
  const why = hidden(focus, "Instructions");
  const answer = useRequest(
    "disassemble",
    why === null && at ? { ...at, address: look.asm ?? null, syntax } : null,
  );
  const disassembled = answer.data;
  const body = useRef<HTMLDivElement>(null);

  let content: React.ReactNode;
  if (why) {
    content = <div className="empty center-message">{why}</div>;
  } else if (answer.error) {
    content = (
      <div className={`${errorClass(answer.error)} center-message`}>{answer.error.message}</div>
    );
  } else if (disassembled) {
    content = (
      <Lines
        disassembled={disassembled}
        body={body}
        current={answer.current}
        innermost={at?.frame === 0}
      />
    );
  } else {
    content = <div className="empty center-message">Disassembling…</div>;
  }

  return (
    <div className="asm" data-testid="disassembly">
      <div className="view-head">
        <span className="asm-function" data-testid="disassembly-function">
          {disassembled?.function ?? (look.asm ? "no function" : "")}
        </span>
        {look.asm && <span className="dim">{look.asm}</span>}
        <fieldset className="segmented small" aria-label="Syntax">
          {SYNTAXES.map((choice) => (
            <SyntaxButton
              key={choice}
              syntax={choice}
              pressed={syntax === choice}
              choose={setSyntax}
            />
          ))}
        </fieldset>
      </div>
      <div className={`asm-body ${answer.current ? "" : "stale"}`} ref={body}>
        {content}
      </div>
    </div>
  );
}

const SYNTAXES = ["intel", "att"] as const;

function SyntaxButton({
  syntax,
  pressed,
  choose,
}: {
  syntax: Syntax;
  pressed: boolean;
  choose: (syntax: Syntax) => void;
}) {
  const click = useCallback(() => choose(syntax), [choose, syntax]);
  return (
    <button type="button" aria-pressed={pressed} onClick={click}>
      {syntax === "intel" ? "Intel" : "AT&T"}
    </button>
  );
}

/** Every line of a listing is one row high, so only the rows in view exist. */
const LINE = 20;

type Line =
  | { kind: "note"; key: string; note: string }
  | { kind: "source"; key: string; source: SourceLine }
  | { kind: "instruction"; key: string; instruction: Instruction };

/**
 * The listing, with notes first and each source line before the
 * instructions compiled from it. A function holds thousands of
 * instructions, so the rows are virtual.
 */
function Lines({
  disassembled,
  body,
  current,
  innermost,
}: {
  disassembled: Disassembled;
  body: React.RefObject<HTMLDivElement | null>;
  current: boolean;
  innermost: boolean;
}) {
  const lines = useMemo(() => {
    const built: Line[] = disassembled.notes.map((note, index) => ({
      kind: "note",
      key: `note:${index}`,
      note,
    }));
    for (const instruction of disassembled.instructions) {
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
  }, [disassembled]);
  // oxlint-disable-next-line react/incompatible-library -- the app is not built with the React Compiler
  const rows = useVirtualizer({
    count: lines.length,
    getScrollElement: () => body.current,
    estimateSize: () => LINE,
    overscan: 40,
  });

  // The marked instruction comes into view whenever it moves.
  const marked = current ? disassembled.marked : undefined;
  useEffect(() => {
    const index = marked
      ? lines.findIndex((line) => line.kind === "instruction" && line.key === marked)
      : -1;
    if (index >= 0) {
      rows.scrollToIndex(index, { align: "center" });
    } else if (current) {
      rows.scrollToOffset(0);
    }
  }, [marked, current, lines, rows]);

  const start = disassembled.function ? disassembled.instructions[0]?.address : undefined;
  const size = rows.getTotalSize();
  const sized = useMemo(() => ({ height: size }), [size]);
  return (
    <div className="asm-lines" style={sized}>
      {rows.getVirtualItems().map((item) => {
        const line = lines[item.index];
        assert(line !== undefined, "virtual row past the listing's lines");
        return (
          <Slot key={line.key} start={item.start}>
            {line.kind === "note" ? (
              <div className="asm-note">{line.note}</div>
            ) : line.kind === "source" ? (
              <SourceHead source={line.source} />
            ) : (
              <Row
                instruction={line.instruction}
                start={start}
                marked={line.instruction.address === disassembled.marked}
                innermost={innermost}
              />
            )}
          </Slot>
        );
      })}
    </div>
  );
}

/** A virtual row, placed `start` pixels down. */
function Slot({ start, children }: { start: number; children: React.ReactNode }) {
  const style = useMemo(() => ({ transform: `translateY(${start}px)` }), [start]);
  return (
    <div className="asm-slot" style={style}>
      {children}
    </div>
  );
}

function Row({
  instruction,
  start,
  marked,
  innermost,
}: {
  instruction: Instruction;
  start: string | undefined;
  marked: boolean;
  innermost: boolean;
}) {
  const offset = start ? BigInt(instruction.address) - BigInt(start) : null;
  return (
    <div
      className={`asm-row ${marked ? (innermost ? "pc" : "frame") : ""}`}
      data-address={instruction.address}
      aria-current={marked ? "true" : undefined}
    >
      <Gutter address={instruction.address} />
      <span className="asm-mark" aria-hidden="true">
        {marked ? "▶" : ""}
      </span>
      <span className="asm-address">{instruction.address}</span>
      <span className="asm-offset">
        {offset === null ? (instruction.symbol ?? "") : `+${offset}`}
      </span>
      <span className="asm-bytes">{instruction.bytes}</span>
      <span className="asm-text">
        {instruction.invalid ? (
          <span className="error">{instruction.invalid}</span>
        ) : (
          instruction.tokens.map((token, index) => (
            // oxlint-disable-next-line react/no-array-index-key -- tokens repeat, and never move
            <span key={index} className={`tk-${token.kind}`}>
              {token.text}
            </span>
          ))
        )}
        {instruction.target?.name && <Target {...instruction.target} />}
        {instruction.comment && <span className="asm-comment"> ; {instruction.comment}</span>}
      </span>
    </div>
  );
}

/** The source line instructions after it were compiled from. */
function SourceHead({ source }: { source: SourceLine }) {
  const showSource = useShowSource();
  const text = useRequest("source", { path: source.path }).data;
  const line = text?.path === source.path ? text.text.split("\n")[source.line - 1] : undefined;
  const place = `${fileName(source.path)}:${source.line}`;
  const show = useCallback(
    () => showSource(source.path, source.line),
    [showSource, source.path, source.line],
  );
  return (
    <button
      type="button"
      className="asm-source"
      aria-label={`Show the source at ${place}`}
      onClick={show}
    >
      <span className="asm-place">{place}</span>
      <span className="asm-line">{line?.trim()}</span>
    </button>
  );
}

/** Where a call or jump goes, as a link to its code. */
function Target({ address, name }: { address: string; name: string | null }) {
  const look = useLook();
  const { look: current } = useFocus();
  const there = useCallback(
    (from: Look): Look => ({ ...from, asm: address, view: "disassembly" }),
    [address],
  );
  const follow = useCallback(
    (event: React.MouseEvent) => {
      if (event.button === 0 && !event.ctrlKey && !event.metaKey && !event.shiftKey) {
        event.preventDefault();
        look(there, { replace: false });
      }
    },
    [look, there],
  );
  return (
    <a
      className="asm-target"
      // The link opens elsewhere as itself; a plain click moves this tab.
      href={window.location.pathname + stringifySearch({ ...there(current) })}
      title={address}
      onClick={follow}
    >
      {name}
    </a>
  );
}

/** A breakpoint's dot at an instruction, which toggles it for controllers. */
function Gutter({ address }: { address: string }) {
  const { state } = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const existing = state.breakpoints.find((breakpoint: Breakpoint) =>
    breakpoint.places.some((place) => place.address === address),
  );
  const toggle = useCallback(() => {
    void toggleBreakpoint(connection, existing, address);
  }, [connection, existing, address]);
  const dot = <span className={existing ? "dot plain" : "dot none"} aria-hidden="true" />;
  if (!control) {
    return <span className="asm-gutter">{dot}</span>;
  }
  return (
    <button
      type="button"
      className="asm-gutter"
      aria-label={`${existing ? "Remove the breakpoint" : "Breakpoint"} at ${address}`}
      onClick={toggle}
    >
      {dot}
    </button>
  );
}
