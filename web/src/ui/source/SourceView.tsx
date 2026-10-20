// One source file, read-only, in CodeMirror: the breakpoint gutter, the
// line the program stopped at, the line a caller's frame is at, and the
// lines the link selects. Its keys stay the debugger's: a letter key typed
// here steps or breaks rather than moving a cursor through text.

import {
  Compartment,
  EditorState,
  type Extension,
  RangeSet,
  StateEffect,
  StateField,
} from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  GutterMarker,
  gutter,
  hoverTooltip,
  lineNumbers,
  ViewPlugin,
  type ViewUpdate,
  WidgetType,
} from "@codemirror/view";
import { useEffect, useRef } from "react";
import { PENDING } from "../../breakpoints";
import { expressionAt } from "../../expressions";
import { highlighting, language } from "./languages";

export type MarkKind = "plain" | "conditional" | "log" | "pending";

/** A value shown at the end of a line, such as `e = 0x5555` . */
export interface InlineValue {
  text: string;
  changed: boolean;
}

export interface Marks {
  /** Breakpoints by line. */
  breakpoints: ReadonlyMap<number, MarkKind>;
  /** Lines a breakpoint can stop at. */
  breakable: ReadonlySet<number>;
  /** Where the shown thread stopped, when this file has it. */
  pc: number | null;
  /** Where the shown outer frame is, when this file has it. */
  frame: number | null;
  /** The lines the link selects. */
  selection: { line: number; end: number } | null;
  /** Values beside the lines that compute them. */
  inline: ReadonlyMap<number, readonly InlineValue[]>;
  /** Why the program stopped, under the line it says it of. */
  note: { line: number; text: string } | null;
}

/** What a hover shows: a value and its type. */
interface Hovered {
  text: string;
  type: string | null;
}

interface SourceViewProps {
  path: string;
  text: string;
  marks: Marks;
  /** Scroll so this line shows; changes of it scroll again. */
  reveal: number | null;
  onGutter(line: number): void;
  onLineNumber(line: number, extend: boolean): void;
  onCursor(line: number): void;
  /** Evaluates what the pointer rests on; absent when values are not shown. */
  onHover?: ((expression: string) => Promise<Hovered | null>) | undefined;
}

const setMarks = StateEffect.define<Marks>();

const EMPTY: Marks = {
  breakpoints: new Map(),
  breakable: new Set(),
  pc: null,
  frame: null,
  selection: null,
  inline: new Map(),
  note: null,
};

const marksField = StateField.define<Marks>({
  create: () => EMPTY,
  update(marks, transaction) {
    for (const effect of transaction.effects) {
      if (effect.is(setMarks)) {
        return effect.value;
      }
    }
    return marks;
  },
});

class BreakpointMarker extends GutterMarker {
  constructor(readonly kind: MarkKind) {
    super();
  }
  override eq(other: GutterMarker): boolean {
    return other instanceof BreakpointMarker && other.kind === this.kind;
  }
  override toDOM(): Node {
    const dot = document.createElement("span");
    dot.className = `dot ${this.kind}`;
    if (this.kind === "pending") {
      dot.title = PENDING;
    }
    return dot;
  }
}

class BreakableMarker extends GutterMarker {
  override toDOM(): Node {
    const dot = document.createElement("span");
    dot.className = "dot hint";
    return dot;
  }
}

const breakable = new BreakableMarker();
const markers = new Map<MarkKind, BreakpointMarker>(
  (["plain", "conditional", "log", "pending"] as const).map((kind) => [
    kind,
    new BreakpointMarker(kind),
  ]),
);

class Values extends WidgetType {
  constructor(readonly values: readonly InlineValue[]) {
    super();
  }
  override eq(other: WidgetType): boolean {
    return (
      other instanceof Values &&
      other.values.length === this.values.length &&
      other.values.every(
        (value, index) =>
          value.text === this.values[index]?.text && value.changed === this.values[index]?.changed,
      )
    );
  }
  override toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = "cm-inline-values";
    for (const value of this.values) {
      const part = document.createElement("span");
      part.className = value.changed ? "changed" : "";
      part.textContent = value.text;
      span.append(part);
    }
    return span;
  }
  // A click on a line's values puts the cursor on that line, as one beside
  // them does.
  override ignoreEvent(): boolean {
    return false;
  }
}

/** Why the program stopped, as a line of its own under the one it stopped at. */
class Note extends WidgetType {
  constructor(readonly text: string) {
    super();
  }
  override eq(other: WidgetType): boolean {
    return other instanceof Note && other.text === this.text;
  }
  override toDOM(): HTMLElement {
    const note = document.createElement("div");
    note.className = "cm-stop-note";
    note.setAttribute("role", "note");
    note.textContent = this.text;
    return note;
  }
}

function hoverDom(expression: string, value: Hovered): HTMLElement {
  const dom = document.createElement("div");
  dom.className = "cm-value-tooltip";
  dom.setAttribute("role", "tooltip");
  const name = document.createElement("span");
  name.className = "name";
  name.textContent = expression;
  const text = document.createElement("span");
  text.className = "text";
  text.textContent = ` = ${value.text}`;
  dom.append(name, text);
  if (value.type) {
    const type = document.createElement("span");
    type.className = "type";
    type.textContent = value.type;
    dom.append(type);
  }
  return dom;
}

function lineDecorations(state: EditorState): DecorationSet {
  const marks = state.field(marksField);
  const doc = state.doc;
  const ranges = [];
  const lines = (from: number, to: number, className: string) => {
    for (let line = Math.max(1, from); line <= Math.min(to, doc.lines); line += 1) {
      ranges.push(Decoration.line({ class: className }).range(doc.line(line).from));
    }
  };
  if (marks.selection) {
    lines(marks.selection.line, marks.selection.end, "cm-selected-line");
  }
  if (marks.frame !== null) {
    lines(marks.frame, marks.frame, "cm-frame-line");
  }
  if (marks.pc !== null) {
    lines(marks.pc, marks.pc, "cm-pc-line");
  }
  if (marks.note && marks.note.line >= 1 && marks.note.line <= doc.lines) {
    ranges.push(
      Decoration.widget({ widget: new Note(marks.note.text), block: true, side: 1 }).range(
        doc.line(marks.note.line).to,
      ),
    );
  }
  for (const [line, values] of marks.inline) {
    if (line >= 1 && line <= doc.lines && values.length > 0) {
      ranges.push(
        Decoration.widget({ widget: new Values(values), side: 1 }).range(doc.line(line).to),
      );
    }
  }
  return RangeSet.of(ranges, true);
}

const decorations = EditorView.decorations.compute([marksField, "doc"], lineDecorations);

/** Reports the cursor's line as it moves. */
function cursorReporter(report: (line: number) => void): Extension {
  return ViewPlugin.fromClass(
    class {
      update(update: ViewUpdate) {
        if (update.selectionSet) {
          report(update.state.doc.lineAt(update.state.selection.main.head).number);
        }
      }
    },
  );
}

export function SourceView({
  path,
  text,
  marks,
  reveal,
  onGutter,
  onLineNumber,
  onCursor,
  onHover,
}: SourceViewProps) {
  const parent = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const languageSlot = useRef(new Compartment());
  // The text the view was last moved to a line in.
  const revealedIn = useRef<string | null>(null);
  // Handlers change every render; the editor calls through these.
  const handlers = useRef({ onGutter, onLineNumber, onCursor, onHover });
  handlers.current = { onGutter, onLineNumber, onCursor, onHover };
  // The path the text is of, which says how to read a hovered word.
  const shownPath = useRef(path);
  shownPath.current = path;

  useEffect(() => {
    if (!parent.current) {
      return;
    }
    const editor = new EditorView({
      parent: parent.current,
      state: EditorState.create({
        doc: "",
        extensions: [
          EditorState.readOnly.of(true),
          marksField,
          decorations,
          gutter({
            class: "cm-breakpoints",
            markers: (current) => {
              const marks = current.state.field(marksField);
              const doc = current.state.doc;
              const lines = [...new Set([...marks.breakpoints.keys(), ...marks.breakable])]
                .filter((line) => line >= 1 && line <= doc.lines)
                .sort((left, right) => left - right);
              return RangeSet.of(
                lines.map((line) => {
                  const kind = marks.breakpoints.get(line);
                  const marker = kind ? (markers.get(kind) as GutterMarker) : breakable;
                  return marker.range(doc.line(line).from);
                }),
              );
            },
            lineMarkerChange: (update) =>
              update.transactions.some((transaction) =>
                transaction.effects.some((effect) => effect.is(setMarks)),
              ),
            domEventHandlers: {
              mousedown: (current, block) => {
                handlers.current.onGutter(current.state.doc.lineAt(block.from).number);
                return true;
              },
            },
          }),
          lineNumbers({
            domEventHandlers: {
              mousedown: (current, block, event) => {
                handlers.current.onLineNumber(
                  current.state.doc.lineAt(block.from).number,
                  (event as MouseEvent).shiftKey,
                );
                return true;
              },
            },
          }),
          highlighting,
          languageSlot.current.of([]),
          cursorReporter((line) => handlers.current.onCursor(line)),
          hoverTooltip(
            async (current, position) => {
              const evaluate = handlers.current.onHover;
              const line = current.state.doc.lineAt(position);
              const found = evaluate
                ? expressionAt(line.text, position - line.from, shownPath.current)
                : null;
              if (!evaluate || !found) {
                return null;
              }
              const value = await evaluate(found.text);
              if (!value) {
                return null;
              }
              return {
                pos: line.from + found.from,
                end: line.from + found.to,
                above: true,
                create: () => ({ dom: hoverDom(found.text, value) }),
              };
            },
            { hoverTime: 300 },
          ),
          EditorView.contentAttributes.of({ "aria-label": "Source", "data-keys": "debugger" }),
        ],
      }),
    });
    view.current = editor;
    return () => {
      editor.destroy();
      view.current = null;
    };
  }, []);

  // A new file replaces the text and its language.
  useEffect(() => {
    const editor = view.current;
    if (!editor) {
      return;
    }
    editor.dispatch({
      changes: { from: 0, to: editor.state.doc.length, insert: text },
      effects: languageSlot.current.reconfigure(language(path)),
    });
  }, [path, text]);

  useEffect(() => {
    view.current?.dispatch({ effects: setMarks.of(marks) });
  }, [marks]);

  useEffect(() => {
    const editor = view.current;
    // The text arriving is a reason to look for the line again.
    if (!editor || !text || reveal === null || reveal < 1 || reveal > editor.state.doc.lines) {
      return;
    }
    const line = editor.state.doc.line(reveal);
    // Only a line out of sight moves the view, so stepping nearby keeps it
    // still. Text the editor has not yet shown has only estimated heights,
    // which can place a line below the fold within sight, so it always moves.
    const shown = revealedIn.current === text;
    revealedIn.current = text;
    const block = editor.lineBlockAt(line.from);
    const { top, bottom } = editor.scrollDOM.getBoundingClientRect();
    const height = bottom - top;
    const scrolled = editor.scrollDOM.scrollTop;
    if (
      shown &&
      height > 0 &&
      block.top >= scrolled + 20 &&
      block.bottom <= scrolled + height - 20
    ) {
      return;
    }
    editor.dispatch({ effects: EditorView.scrollIntoView(line.from, { y: "center" }) });
  }, [reveal, text]);

  return <div className="source-view" ref={parent} data-testid="source" data-path={path} />;
}
