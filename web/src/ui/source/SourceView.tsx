// oxlint-disable-next-line max-classes-per-file -- CodeMirror markers and widgets are small classes
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
import { createEffect, createMemo, createSignal, onSettled } from "solid-js";

import { assert } from "../../assert";
import { PENDING } from "../../breakpoints";
import { expressionAt } from "../../expressions";
import { highlighting, language } from "./languages";

export type MarkKind = "plain" | "conditional" | "log" | "pending";

export interface InlineValue {
  text: string;
  changed: boolean;
}

export interface Marks {
  breakpoints: ReadonlyMap<number, MarkKind>;

  breakable: ReadonlySet<number>;

  pc: number | null;

  frame: number | null;

  selection: { line: number; end: number } | null;

  inline: ReadonlyMap<number, readonly InlineValue[]>;

  note: { line: number; text: string } | null;
}

interface Hovered {
  text: string;
  type: string | null;
}

interface SourceViewProps {
  path: string;
  text: string;
  marks: Marks;

  reveal: number | null;
  onGutter(line: number): void;
  onLineNumber(line: number, extend: boolean): void;
  onCursor(line: number): void;

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
  readonly kind: MarkKind;

  constructor(kind: MarkKind) {
    super();
    this.kind = kind;
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
  readonly values: readonly InlineValue[];

  constructor(values: readonly InlineValue[]) {
    super();
    this.values = values;
  }
  override eq(other: WidgetType): boolean {
    return (
      other instanceof Values &&
      other.values.length === this.values.length &&
      other.values.every((value, index) => {
        const mine = this.values[index];
        assert(mine !== undefined, "inline value index past an equal length");
        return value.text === mine.text && value.changed === mine.changed;
      })
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

  override ignoreEvent(): boolean {
    return false;
  }
}

class Note extends WidgetType {
  readonly text: string;

  constructor(text: string) {
    super();
    this.text = text;
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
    assert(
      marks.selection.line <= marks.selection.end,
      `selection ${marks.selection.line}-${marks.selection.end} ends before it starts`,
    );
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

export function SourceView(props: SourceViewProps) {
  let parent: HTMLDivElement | undefined;
  const [view, setView] = createSignal<EditorView | null>(null);
  const languageSlot = new Compartment();
  let revealedIn: string | null = null;

  onSettled(() => {
    assert(parent !== undefined, "source view mounted without its element");
    const editor = new EditorView({
      parent,
      state: EditorState.create({
        doc: "",
        extensions: [
          EditorState.readOnly.of(true),
          marksField,
          decorations,
          gutter({
            class: "cm-breakpoints",
            markers: (current) => {
              const shown = current.state.field(marksField);
              const doc = current.state.doc;
              const lines = [...new Set([...shown.breakpoints.keys(), ...shown.breakable])]
                .filter((line) => line >= 1 && line <= doc.lines)
                .toSorted((left, right) => left - right);
              return RangeSet.of(
                lines.map((line) => {
                  const kind = shown.breakpoints.get(line);
                  const marker = kind ? markers.get(kind) : breakable;
                  assert(marker !== undefined, "no gutter marker for a breakpoint kind");
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
                props.onGutter(current.state.doc.lineAt(block.from).number);
                return true;
              },
            },
          }),
          lineNumbers({
            domEventHandlers: {
              mousedown: (current, block, event) => {
                props.onLineNumber(
                  current.state.doc.lineAt(block.from).number,
                  (event as MouseEvent).shiftKey,
                );
                return true;
              },
            },
          }),
          highlighting,
          languageSlot.of([]),
          cursorReporter((line) => props.onCursor(line)),
          hoverTooltip(
            async (current, position) => {
              const evaluate = props.onHover;
              const line = current.state.doc.lineAt(position);
              const found = evaluate
                ? expressionAt(line.text, position - line.from, props.path)
                : null;
              if (!evaluate || !found) {
                return null;
              }
              assert(found.from <= found.to, "hovered expression ends before it starts");
              assert(found.to <= line.length, "hovered expression ends past its line");
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
    setView(editor);
    return () => {
      editor.destroy();
      setView(null);
    };
  });

  const path = createMemo(() => props.path);
  const text = createMemo(() => props.text);
  const marks = createMemo(() => props.marks);
  const reveal = createMemo(() => props.reveal);

  createEffect(
    () => [view(), path(), text()] as const,
    ([editor, currentPath, currentText]) => {
      if (!editor) {
        return;
      }
      editor.dispatch({
        changes: { from: 0, to: editor.state.doc.length, insert: currentText },
        effects: languageSlot.reconfigure(language(currentPath)),
      });
    },
  );

  createEffect(
    () => [view(), marks()] as const,
    ([editor, currentMarks]) => {
      if (!editor) {
        return;
      }
      editor.dispatch({ effects: setMarks.of(currentMarks) });
    },
  );

  createEffect(
    () => [view(), reveal(), text()] as const,
    ([editor, currentReveal, currentText]) => {
      if (!editor) {
        return;
      }
      if (
        !currentText ||
        currentReveal === null ||
        currentReveal < 1 ||
        currentReveal > editor.state.doc.lines
      ) {
        return;
      }
      const line = editor.state.doc.line(currentReveal);

      const shown = revealedIn === currentText;
      revealedIn = currentText;
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
    },
  );

  return (
    <div
      class="source-view"
      ref={(node) => {
        parent = node;
      }}
      data-testid="source"
      data-path={props.path}
    />
  );
}
