import type { JSX } from "@solidjs/web";
import {
  createEffect,
  createMemo,
  createSignal,
  For,
  Match,
  Repeat,
  Show,
  Switch,
  untrack,
} from "solid-js";

import {
  type CodeRow,
  codeRows,
  hottestRun,
  inlinedFrom,
  inlineTree,
  linesAbove,
  openingLine,
  ownCodeAt,
  runsOf,
  shortName,
  shownLine,
  type SourceEntry,
  sourceEntries,
  tintOfPath,
  tintsOf,
  type TreeLine,
} from "../../profile/code";
import { formatCount, formatPercent, largest } from "../../profile/paint";
import type { ProfileCode, ProfileSourceFile, ProfileToken } from "../../protocol";
import { createVirtualizer, type SolidVirtualizer } from "../../virtual";
import { fileName } from "../paths";
import { type Painted, paintLines } from "../source/languages";
import { useAnswer, useProfile } from "./context";
import { ownTone, Share, Unanswered } from "./parts";
import { COLUMN_HELP } from "./TreeView";

export function SourceLines(props: { name: string }) {
  const focus = useProfile();
  const answer = useAnswer("profileLines", () => ({ ...focus().query, function: props.name }));
  let body: HTMLDivElement | undefined;
  useHottestInView(
    () => body,
    () => props.name,
    () => answer().current,
  );
  const files = createMemo(() => answer().data?.files);
  const painted = createMemo(() =>
    files()?.map((file) => (file.text ? paintLines(file.path ?? "", file.text) : [])),
  );
  const weight = createMemo(() => answer().data?.weight ?? 0);
  const most = createMemo(() => {
    let highest = 0;
    for (const file of files() ?? []) {
      highest = largest(
        file.lines.map(([, , total]) => total),
        highest,
      );
    }
    return highest;
  });

  return (
    <Switch>
      <Match when={answer().error || !files()}>
        <Unanswered error={answer().error} reading={`Reading the source lines of ${props.name}…`} />
      </Match>
      <Match when={files()?.length === 0}>
        <div class="empty">No selected sample has a source line in {props.name}.</div>
      </Match>
      <Match when={files()}>
        {(shownFiles) => (
          <div
            ref={(node) => {
              body = node;
            }}
            class={`pane-body annotated ${answer().current ? "" : "stale"}`}
            data-testid="source-lines"
          >
            <For each={shownFiles()} keyed={(file) => file.path ?? ""}>
              {(file, fileAt) => {
                const costs = createMemo(
                  () => new Map(file().lines.map(([line, own, total]) => [line, [own, total]])),
                );
                return (
                  <div class="annotated-file">
                    <div class="annotated-head mono">{file().path ?? "no file"}</div>
                    <Show when={file().text} fallback={<div class="empty">{file().unread}</div>}>
                      {(lines) => (
                        <For each={lines()} keyed={false}>
                          {(text, at) => {
                            const line = createMemo(() => file().first + at);
                            const cost = createMemo(() => costs().get(line()));
                            return (
                              <div
                                class={`row annotated-row ${cost() ? "hot" : ""} ${hottest(cost()?.[1], most())}`}
                              >
                                <Costs own={cost()?.[0]} total={cost()?.[1]} weight={weight()} />
                                <span class="line-number">{line()}</span>
                                <code>
                                  <Tokens runs={painted()?.[fileAt()]?.[at]} text={text()} />
                                </code>
                              </div>
                            );
                          }}
                        </For>
                      )}
                    </Show>
                  </div>
                );
              }}
            </For>
          </div>
        )}
      </Match>
    </Switch>
  );
}

export function Code(props: { name: string }) {
  const focus = useProfile();
  const answer = useAnswer("profileCode", () => ({ ...focus().query, function: props.name }));

  let answered = untrack(() => props.name);
  const shownName = createMemo(() => {
    if (answer().current) {
      answered = props.name;
    }
    return answered;
  });
  const code = createMemo(() => answer().data);
  const splitKey = createMemo(() => {
    const current = code();
    if (
      answer().error ||
      !current ||
      current.unavailable ||
      (current.blocks.length === 0 && current.unplaced.length === 0)
    ) {
      return null;
    }
    return `${shownName()}\n${current.blocks[0]?.symbol ?? ""}`;
  });

  const emptyCode = createMemo(() => {
    const current = code();
    return current !== undefined && current.blocks.length === 0 && current.unplaced.length === 0;
  });

  return (
    <Switch>
      <Match when={answer().error || !code()}>
        <Unanswered error={answer().error} reading={`Reading the instructions of ${props.name}…`} />
      </Match>
      <Match when={code()?.unavailable}>{(reason) => <div class="empty">{reason()}</div>}</Match>
      <Match when={emptyCode()}>
        <div class="empty">No selected sample ran an instruction of {props.name}.</div>
      </Match>
      <Match when={splitKey()} keyed>
        {(_key) => (
          <Show when={code()}>
            {(current) => <Split name={shownName()} code={current()} current={answer().current} />}
          </Show>
        )}
      </Match>
    </Switch>
  );
}

interface Chosen {
  line: string;
  run: number | null;
  row: number | null;
}

function Split(props: { name: string; code: ProfileCode; current: boolean }) {
  const rows = createMemo(() => codeRows(props.code));
  const tree = createMemo(() => inlineTree(rows()));

  const opening = createMemo(() =>
    openingLine(
      tree(),
      rows(),
      props.code.files,
      props.code.inlined_into ? props.code.functions.indexOf(props.name) : -1,
    ),
  );
  const [open, setOpen] = createSignal<ReadonlySet<string>>(
    untrack(() => {
      const first = opening();
      return new Set(first ? linesAbove(first) : []);
    }),
  );
  const [chosen, setChosen] = createSignal<Chosen | null>(null);
  const [source, setSource] = createSignal<HTMLDivElement | null>(null);
  const [listing, setListing] = createSignal<HTMLDivElement | null>(null);

  const entries = createMemo(() => sourceEntries(tree(), props.code.files, open()));
  const tints = createMemo(() => tintsOf(tree(), open()));
  const from = createMemo(() => inlinedFrom(tree(), props.code.files));
  const line = createMemo(() => {
    const picked = chosen();
    return (picked ? tree().lines.get(picked.line) : undefined) ?? opening();
  });
  const runs = createMemo(() => runsOf(line()?.rows ?? []));
  const run = createMemo(() => chosen()?.run ?? hottestRun(rows(), runs()));
  const shown = createMemo(() => chosen()?.row ?? runs()[run()]?.[0]);
  const selected = createMemo(() => new Set(line()?.rows));
  const paint = createMemo(() => painter(props.code.files));

  const entryOf = createMemo(
    () =>
      new Map(
        entries().flatMap((entry, at) => (entry.kind === "line" ? [[entry.line.key, at]] : [])),
      ),
  );
  const coded = createMemo(() =>
    entries().flatMap((entry) => (entry.kind === "line" ? [entry.line] : [])),
  );

  const items = createMemo(() =>
    rows().flatMap((row, index): Item[] =>
      row.symbol !== null && props.code.blocks.length > 1
        ? [{ symbol: row.symbol }, { row: index }]
        : [{ row: index }],
    ),
  );
  const itemOf = createMemo(() => {
    const at: number[] = [];
    for (const [item, each] of items().entries()) {
      if ("row" in each) {
        at[each.row] = item;
      }
    }
    return at;
  });
  const sourceList = usePane(() => entries().length, source);
  const listingList = usePane(() => items().length, listing);
  const longestSource = createMemo(() =>
    longest(entries().map((entry) => entryText(entry, props.code))),
  );
  const longestListing = createMemo(() =>
    longest(
      rows().map(
        ({ instruction }) =>
          instruction.tokens.map(([, text]) => text).join("") +
          (instruction.target ? ` <${instruction.target}>` : ""),
      ),
    ),
  );

  const brackets = createMemo(() => {
    const firsts: number[] = [];
    const holds = new Set<number>();
    const currentFrom = from();
    const currentSelected = selected();
    for (const [index, function_] of currentFrom.entries()) {
      const first =
        index > 0 && currentFrom[index - 1] === function_ ? (firsts[index - 1] ?? index) : index;
      firsts.push(first);
      if (function_ !== null && currentSelected.has(index)) {
        holds.add(first);
      }
    }
    return { firsts, holds };
  });
  const bracketed = createMemo(() => from().some((function_) => function_ !== null));

  createEffect(
    () => ({
      lineKey: line()?.key,
      shown: shown(),
      entryOf: entryOf(),
      itemOf: itemOf(),
      sourceEl: source(),
      listingEl: listing(),
    }),
    ({ lineKey, shown: shownRow, entryOf: entriesMap, itemOf: itemsMap, sourceEl, listingEl }) => {
      reveal(sourceEl, lineKey === undefined ? -1 : (entriesMap.get(lineKey) ?? -1));
      if (shownRow !== undefined) {
        reveal(listingEl, itemsMap[shownRow] ?? -1);
      }
    },
  );

  const choose = (to: TreeLine, at: number | null, row: number | null) => {
    const above = linesAbove(to);
    setOpen((was) => (above.every((key) => was.has(key)) ? was : new Set([...was, ...above])));
    setChosen({ line: to.key, run: at, row });
  };
  const toggle = (key: string) => {
    setOpen((was) => {
      const now = new Set(was);
      if (!now.delete(key)) {
        now.add(key);
      }
      return now;
    });
  };

  const step = (by: number) => {
    const currentLine = line();
    const currentRuns = runs();
    if (currentLine && currentRuns.length > 0) {
      setChosen({
        line: currentLine.key,
        run: (run() + by + currentRuns.length) % currentRuns.length,
        row: null,
      });
    }
  };
  const previous = () => step(-1);
  const next = () => step(1);

  const keys = (event: KeyboardEvent) => {
    const currentLine = line();
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const lines = coded();
      const at = currentLine ? lines.indexOf(currentLine) : -1;
      const to = lines[at + (event.key === "ArrowDown" ? 1 : -1)];
      if (to && at >= 0) {
        choose(to, null, null);
      }
    } else if (event.key === "Enter" && currentLine && currentLine.inlined.length > 0) {
      event.preventDefault();
      toggle(currentLine.key);
    } else if (event.key === "]" || event.key === "ArrowRight") {
      event.preventDefault();
      step(1);
    } else if (event.key === "[" || event.key === "ArrowLeft") {
      event.preventDefault();
      step(-1);
    }
  };

  const pickLine = (event: MouseEvent) => {
    const target = event.target as Element;
    const opener = target.closest<HTMLElement>("[data-toggle]")?.dataset.toggle;
    if (opener !== undefined) {
      toggle(opener);
      return;
    }
    const picked = tree().lines.get(target.closest<HTMLElement>("[data-line]")?.dataset.line ?? "");
    if (picked) {
      choose(picked, null, null);
    }
  };

  const indexOf = createMemo(
    () => new Map(rows().map((row, index) => [row.instruction.address, index])),
  );

  const pickRow = (event: MouseEvent) => {
    const target = event.target as Element;
    const jump = target.closest<HTMLElement>("[data-jump]");
    const row = target.closest<HTMLElement>("[data-row]");
    const index = jump ? indexOf().get(Number(jump.dataset.jump)) : Number(row?.dataset.row);
    const path = index === undefined ? undefined : tree().paths[index];
    const to = path?.[ownCodeAt(path, props.code.files)];
    if (index === undefined || !to) {
      return;
    }
    const at = runsOf(to.rows).findIndex(([first, last]) => index >= first && index <= last);
    choose(to, at, index);
  };

  const counted = createMemo(() =>
    runs().reduce((sum, [first, last]) => sum + last - first + 1, 0),
  );
  const clicked = createMemo(() => {
    const picked = chosen();
    return picked?.row === null || picked === null ? undefined : rows()[picked.row];
  });

  const lineDigits = createMemo<JSX.CSSProperties>(() => {
    const widest = Math.max(1, ...[...tree().lines.values()].map((each) => each.line));
    return { "--digits": String(widest).length };
  });
  const offsetDigits = createMemo<JSX.CSSProperties>(() => {
    const most = rows().reduce((widest, row) => Math.max(widest, row.offset), 0);
    return { "--digits": most.toString(16).length + 1 };
  });

  return (
    <div class={`code-split ${props.current ? "" : "stale"}`} data-testid="code">
      <div class="code-split-head">
        <span>{props.code.files[0]?.path ?? "no source"}</span>
        <span class="muted end">{props.code.module}</span>
      </div>
      <Show when={props.code.inlined_into}>
        {(into) => (
          <div class="code-split-note">
            {props.name} has no code of its own. Every sample ran it inlined into {into()}, shown
            here.
          </div>
        )}
      </Show>
      <div class="code-split-panes">
        <div
          ref={(node) => setSource(node)}
          class="code-split-pane"
          style={lineDigits()}
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- rows of costs and code a select cannot hold
          role="listbox"
          aria-label="Source"
          tabindex={0}
          onClick={pickLine}
          onKeyDown={keys}
        >
          <CostHead />
          <Sizer text={longestSource()} />
          <Show
            when={entries().length > 0}
            fallback={
              <div class="empty">
                {props.code.unlined ??
                  props.code.files[0]?.unread ??
                  "The instructions have no source lines."}
              </div>
            }
          >
            <Gap list={sourceList} side="before" />
            <For
              each={sourceList.items()}
              keyed={(item) => {
                const entry = entries()[item.index];
                return entry?.kind === "line"
                  ? entry.line.key
                  : `${entry?.kind ?? "none"} ${item.index}`;
              }}
            >
              {(item) => (
                <Show when={entries()[item().index]}>
                  {(entry) => (
                    <SourceRow
                      entry={entry()}
                      code={props.code}
                      chosen={
                        entry().kind === "line" &&
                        (entry() as { kind: "line"; line: TreeLine }).line === line()
                      }
                      open={open()}
                      tints={tints()}
                      paint={paint()}
                    />
                  )}
                </Show>
              )}
            </For>
            <Gap list={sourceList} side="after" />
          </Show>
        </div>
        <div
          ref={(node) => setListing(node)}
          class="code-split-pane"
          style={offsetDigits()}
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- rows of costs and code a select cannot hold
          role="listbox"
          aria-label="Assembly"
          aria-multiselectable="true"
          tabindex={0}
          onClick={pickRow}
          onKeyDown={keys}
        >
          <CostHead from={bracketed()} />
          <Sizer text={longestListing()} from={bracketed()} />
          <Gap list={listingList} side="before" />
          <For
            each={listingList.items()}
            keyed={(item) => {
              const each = items()[item.index];
              if (each && "symbol" in each) {
                return `symbol ${item.index}`;
              }
              const row = rows()[each?.row ?? -1];
              return row ? row.instruction.address : `empty ${item.index}`;
            }}
          >
            {(item) => {
              const each = createMemo(() => items()[item().index]);
              const symbol = createMemo(() => {
                const current = each();
                return current && "symbol" in current ? current.symbol : undefined;
              });
              const rowEntry = createMemo(() => {
                const current = each();
                if (!current || !("row" in current)) {
                  return;
                }
                const row = rows()[current.row];
                return row ? { index: current.row, row } : undefined;
              });
              return (
                <Switch>
                  <Match when={symbol()}>
                    {(name) => <div class="annotated-head">{name()}</div>}
                  </Match>
                  <Match when={rowEntry()}>
                    {(entry) => {
                      const index = createMemo(() => entry().index);
                      const row = createMemo(() => entry().row);
                      const instruction = createMemo(() => row().instruction);
                      const path = createMemo(() => tree().paths[index()] ?? []);
                      const tint = createMemo(() => tintOfPath(path(), open(), tints()));
                      const unsourced = createMemo(() => shownLine(path(), open())?.line === 0);
                      const jump = createMemo(() => {
                        const target = row().jump;
                        return target === null ? null : { target };
                      });
                      return (
                        <div
                          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
                          role="option"
                          aria-selected={selected().has(index()) ? "true" : "false"}
                          data-row={index()}
                          data-tint={tint()}
                          style={tinted(tint())}
                          class={`code-row ${row().leads ? "leads" : ""} ${index() === shown() ? "shown" : ""} ${unsourced() ? "unsourced" : ""}`}
                        >
                          <span class="pin">
                            <Costs
                              own={instruction().own > 0 ? instruction().own : undefined}
                              total={instruction().total > 0 ? instruction().total : undefined}
                              weight={props.code.weight}
                            />
                            <Show when={bracketed()}>
                              <Bracket
                                code={props.code}
                                from={from()}
                                index={index()}
                                holds={brackets().holds.has(brackets().firsts[index()] ?? -1)}
                              />
                            </Show>
                            <span class="offset">+{row().offset.toString(16)}</span>
                          </span>
                          <code class="text">
                            <Show
                              when={jump()}
                              fallback={
                                <>
                                  <AsmTokens tokens={instruction().tokens} />
                                  <Show when={instruction().target}>
                                    {(target) => (
                                      <span class="asm-target"> &lt;{target()}&gt;</span>
                                    )}
                                  </Show>
                                </>
                              }
                            >
                              {(jumped) => (
                                <>
                                  <AsmTokens tokens={instruction().tokens.slice(0, -1)} />
                                  <button
                                    type="button"
                                    class="asm-target"
                                    data-jump={jumped().target}
                                  >
                                    +
                                    {(
                                      rows()[indexOf().get(jumped().target) ?? index()]?.offset ?? 0
                                    ).toString(16)}
                                  </button>
                                </>
                              )}
                            </Show>
                          </code>
                        </div>
                      );
                    }}
                  </Match>
                </Switch>
              );
            }}
          </For>
          <Gap list={listingList} side="after" />
          <For each={props.code.blocks} keyed={(block) => block.symbol}>
            {(block) => (
              <Show when={block().stopped}>
                {(stopped) => <div class="empty">{stopped()}</div>}
              </Show>
            )}
          </For>
          <Show when={props.code.unplaced.length > 0}>
            <div class="empty">
              {formatCount(props.code.unplaced.length, "sampled address", "sampled addresses")}{" "}
              {props.code.unplaced.length === 1 ? "lies" : "lie"} outside every decoded instruction.
            </div>
          </Show>
        </div>
      </div>
      <div class="code-split-foot">
        <span>
          <Show when={line()} fallback="no line">
            {(current) => <>{whereOf(props.code, current())}</>}
          </Show>{" "}
          · {formatCount(counted(), "instruction")} in {formatCount(runs().length, "run")}
        </span>
        <Show when={clicked()}>
          {(current) => (
            <span class="muted chain" title={chainOf(props.code, current(), false)}>
              {chainOf(props.code, current(), true)}
            </span>
          )}
        </Show>
        <span class="end">
          <button
            type="button"
            class="tb ghost"
            aria-label="Previous run"
            disabled={runs().length < 2}
            onClick={previous}
          >
            ‹
          </button>
          <span data-testid="run">
            {runs().length > 0 ? `run ${run() + 1} of ${runs().length}` : "no run"}
          </span>
          <button
            type="button"
            class="tb ghost"
            aria-label="Next run"
            disabled={runs().length < 2}
            onClick={next}
          >
            ›
          </button>
        </span>
      </div>
    </div>
  );
}

function painter(
  files: readonly ProfileSourceFile[],
): (file: number, number: number) => Painted | undefined {
  const painted = new Map<number, Painted[]>();
  return (file, number) => {
    const shown = files[file];
    let lines = painted.get(file);
    if (!lines && shown?.text) {
      lines = paintLines(shown.path ?? "", shown.text);
      painted.set(file, lines);
    }
    return lines?.[number - (shown?.first ?? 0)];
  };
}

function tinted(tint: number | undefined): JSX.CSSProperties | undefined {
  return tint === undefined ? undefined : { "--tint": tint };
}

function SourceRow(props: {
  entry: SourceEntry;
  code: ProfileCode;
  chosen: boolean;
  open: ReadonlySet<string>;
  tints: ReadonlyMap<string, number>;
  paint: (file: number, number: number) => Painted | undefined;
}) {
  return (
    <Switch>
      <Match when={props.entry.kind === "call" ? props.entry : undefined}>
        {(entry) => {
          const full = createMemo(() => props.code.functions[entry().call.function] ?? "?");
          const at = createMemo(() => entry().call.at);
          const path = createMemo(() => props.code.files[entry().call.lines[0]?.file ?? -1]?.path);
          const total = createMemo(() =>
            entry().call.lines.reduce((sum, each) => sum + each.total, 0),
          );
          return (
            <div
              // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
              role="option"
              aria-selected="false"
              aria-disabled="true"
              class="code-row code-call nocode"
            >
              <span class="pin">
                <Costs
                  own={undefined}
                  total={total() > 0 ? total() : undefined}
                  weight={props.code.weight}
                />
                <span class="line-number" />
                <span class="inline-toggle" />
              </span>
              <code class="text">
                <Indent depth={entry().depth} />
                <span class="tk-f" title={full()}>
                  {at() ? shortName(full()) : full()}
                </span>
                <Show when={path()}>
                  {(filePath) => <span class="muted"> {fileName(filePath())}</span>}
                </Show>
                <Show when={at()}>
                  {(place) => <span class="muted"> · inlined at line {place().line}</span>}
                </Show>
              </code>
            </div>
          );
        }}
      </Match>
      <Match when={props.entry.kind === "gap" ? props.entry : undefined}>
        {(entry) => (
          <div class="code-row nocode" aria-hidden="true">
            <span class="pin">
              <Costs own={undefined} total={undefined} weight={props.code.weight} />
              <span class="line-number" />
              <span class="inline-toggle" />
            </span>
            <code class="text muted">
              <Indent depth={entry().depth} />⋯ {formatCount(entry().count, "line")}
            </code>
          </div>
        )}
      </Match>
      <Match when={props.entry.kind === "text" ? props.entry : undefined}>
        {(entry) => {
          const shownFile = createMemo(() => props.code.files[entry().file]);
          const lineText = createMemo(() => {
            const file = shownFile();
            return file?.text?.[entry().number - file.first] ?? "";
          });
          return (
            <div
              // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
              role="option"
              aria-selected="false"
              aria-disabled="true"
              class="code-row nocode"
            >
              <span class="pin">
                <Costs own={undefined} total={undefined} weight={props.code.weight} />
                <span class="line-number">{entry().number}</span>
                <span class="inline-toggle" />
              </span>
              <code class="text">
                <Indent depth={entry().depth} />
                <Tokens runs={props.paint(entry().file, entry().number)} text={lineText()} />
              </code>
            </div>
          );
        }}
      </Match>
      <Match when={props.entry.kind === "line" ? props.entry : undefined}>
        {(entry) => {
          const line = createMemo(() => entry().line);
          const tint = createMemo(() => props.tints.get(line().key));
          const opened = createMemo(() => props.open.has(line().key));
          const shownFile = createMemo(() => {
            const file = line().file;
            return file === null ? undefined : props.code.files[file];
          });
          const text = createMemo(() => {
            const file = shownFile();
            return file?.text?.[line().line - file.first];
          });
          const fallbackLabel = createMemo(() => {
            const path = shownFile()?.path;
            return path ? `${fileName(path)}:${line().line}` : `line ${line().line}`;
          });
          const lined = createMemo(() => {
            const file = line().file;
            const content = text();
            return file !== null && content !== undefined ? { file, content } : undefined;
          });
          return (
            <div
              // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
              role="option"
              aria-selected={props.chosen ? "true" : "false"}
              data-line={line().key}
              data-tint={tint()}
              style={tinted(tint())}
              class={`code-row ${line().line === 0 ? "unsourced" : ""}`}
            >
              <span class="pin">
                <Costs
                  own={line().own > 0 ? line().own : undefined}
                  total={line().total > 0 ? line().total : undefined}
                  weight={props.code.weight}
                />
                <span class="line-number">{line().line > 0 ? line().line : ""}</span>
                <Show when={line().inlined.length > 0} fallback={<span class="inline-toggle" />}>
                  <button
                    type="button"
                    class="inline-toggle"
                    data-toggle={line().key}
                    aria-expanded={opened() ? "true" : "false"}
                    aria-label={`${opened() ? "Hide" : "Show"} the code inlined at line ${line().line}`}
                  >
                    {opened() ? "▾" : "▸"}
                  </button>
                </Show>
              </span>
              <code class="text">
                <Indent depth={entry().depth} />
                <Switch
                  fallback={
                    <span class="muted" title={shownFile()?.unread ?? undefined}>
                      {fallbackLabel()}
                    </span>
                  }
                >
                  <Match when={line().line === 0}>
                    <span class="unsourced-note">
                      no associated source line · {formatCount(line().rows.length, "instruction")}
                    </span>
                  </Match>
                  <Match when={lined()}>
                    {(shown) => (
                      <Tokens
                        runs={props.paint(shown().file, line().line)}
                        text={shown().content}
                      />
                    )}
                  </Match>
                </Switch>
                <Show when={line().inlined.length > 0 && !opened()}>
                  <span class="muted inlines">
                    {"  inlines "}
                    <For each={line().inlined.slice(0, INLINES_NAMED)} keyed={(call) => call.key}>
                      {(call, at) => (
                        <>
                          {at() > 0 ? ", " : null}
                          <span title={props.code.functions[call().function]}>
                            {shortName(props.code.functions[call().function] ?? "?")}
                          </span>
                        </>
                      )}
                    </For>
                    {line().inlined.length > INLINES_NAMED
                      ? ` +${line().inlined.length - INLINES_NAMED}`
                      : null}
                  </span>
                </Show>
              </code>
            </div>
          );
        }}
      </Match>
    </Switch>
  );
}

const INLINES_NAMED = 3;

function Indent(props: { depth: number }) {
  return (
    <Show when={props.depth > 0}>
      <span class="code-indent" aria-hidden="true">
        <Repeat count={props.depth}>{() => <span />}</Repeat>
      </span>
    </Show>
  );
}

function Bracket(props: {
  code: ProfileCode;
  from: readonly (number | null)[];
  index: number;
  holds: boolean;
}) {
  const inlined = createMemo(() => props.from[props.index] ?? null);
  const before = createMemo(() => props.from[props.index - 1] === inlined());
  const after = createMemo(() => props.from[props.index + 1] === inlined());
  const part = createMemo(() =>
    before() && after() ? "middle" : before() ? "end" : after() ? "start" : "one",
  );
  const full = createMemo(() => props.code.functions[inlined() ?? -1] ?? "?");
  return (
    <Show when={inlined() !== null} fallback={<span class="inlined-from" />}>
      <span
        class={`inlined-from ${props.holds ? "holds" : ""}`}
        data-bracket={part()}
        title={before() ? undefined : full()}
      >
        {before() ? null : shortName(full())}
      </span>
    </Show>
  );
}

function whereOf(code: ProfileCode, line: TreeLine): string {
  const number = line.line > 0 ? `line ${line.line}` : "no line";
  return line.call.at
    ? `${number} of ${shortName(code.functions[line.call.function] ?? "?")}`
    : number;
}

function entryText(entry: SourceEntry, code: ProfileCode): string {
  const indent = "  ".repeat(entry.depth);
  if (entry.kind === "text") {
    const file = code.files[entry.file];
    return indent + (file?.text?.[entry.number - file.first] ?? "");
  }
  if (entry.kind === "line") {
    const file = entry.line.file === null ? undefined : code.files[entry.line.file];
    return indent + (file?.text?.[entry.line.line - file.first] ?? "");
  }
  return indent;
}

function chainOf(code: ProfileCode, row: CodeRow, short: boolean): string {
  const instruction = row.instruction;
  const where = instruction.lines.map(([function_, file, line]) => {
    const full = code.functions[function_] ?? "?";
    const name = short ? shortName(full) : full;
    const path = file === null ? undefined : code.files[file]?.path;
    const base = path?.split("/").at(-1);
    return base && line > 0 ? `${name} ${base}:${line}` : name;
  });
  return `+${row.offset.toString(16)} ${where.join(" ← ")}`;
}

function Tokens(props: { runs: Painted | undefined; text: string }) {
  return (
    <Show when={props.runs} fallback={props.text}>
      {(runs) => (
        <For each={runs()} keyed={false}>
          {(run) => (
            <Show when={run()[1]} fallback={run()[0]}>
              {(token) => <span class={token()}>{run()[0]}</span>}
            </Show>
          )}
        </For>
      )}
    </Show>
  );
}

function AsmTokens(props: { tokens: readonly ProfileToken[] }) {
  return (
    <For each={props.tokens} keyed={false}>
      {(token) => <span class={`tk-${token()[0]}`}>{token()[1]}</span>}
    </For>
  );
}

const COST_COLUMNS = ["own", "total"] as const;

function CostHead(props: { from?: boolean | undefined }) {
  return (
    <div class="code-row code-head" aria-hidden="true">
      <span class="pin">
        <For each={COST_COLUMNS}>
          {(column) => (
            <span class="percent" title={COLUMN_HELP[column]}>
              <span class="hover-help">{column}</span>
            </span>
          )}
        </For>
        <Show when={props.from}>
          <span class="inlined-from">inlined from</span>
        </Show>
        <span class="line-number" />
      </span>
    </div>
  );
}

const ROW = 20;

type Item = { row: number } | { symbol: string };

function usePane(count: () => number, pane: () => HTMLDivElement | null) {
  return createVirtualizer<HTMLDivElement, Element>(() => {
    const element = pane();
    return {
      count: count(),
      getScrollElement: () => element,
      estimateSize: () => ROW,
      overscan: 30,
      scrollMargin: ROW,
    };
  });
}

function Gap(props: { list: SolidVirtualizer<HTMLDivElement, Element>; side: "before" | "after" }) {
  const height = createMemo(() => {
    const drawn = props.list.items();
    return props.side === "before"
      ? (drawn[0]?.start ?? ROW) - ROW
      : props.list.totalSize() - ((drawn.at(-1)?.end ?? ROW) - ROW);
  });
  return <div style={{ height: `${height()}px` }} aria-hidden="true" />;
}

function Sizer(props: { text: string; from?: boolean | undefined }) {
  return (
    <div class="code-row code-sizer" aria-hidden="true">
      <span class="pin">
        <span class="percent" />
        <span class="percent" />
        <Show when={props.from}>
          <span class="inlined-from" />
        </Show>
        <span class="line-number" />
        <span class="inline-toggle" />
      </span>
      <code class="text" data-text={props.text} />
    </div>
  );
}

function longest(lines: readonly string[]): string {
  let most = "";
  for (const line of lines) {
    const wide = line.replaceAll("\t", "        ");
    if (wide.length > most.length) {
      most = wide;
    }
  }
  return most;
}

function reveal(pane: HTMLElement | null, index: number): void {
  if (!pane || index < 0) {
    return;
  }
  const top = ROW * (index + 1);
  if (top < pane.scrollTop + ROW || top + ROW > pane.scrollTop + pane.clientHeight) {
    pane.scrollTop = top - ROW - (pane.clientHeight - 2 * ROW) / 2;
  }
}

function Costs(props: { own: number | undefined; total: number | undefined; weight: number }) {
  const share = (part: number | undefined) => (part ?? 0) / Math.max(props.weight, 1);
  const text = (part: number | undefined) =>
    part === undefined ? "" : formatPercent(part, props.weight);
  return (
    <>
      <Share share={share(props.own)} tone={ownTone(share(props.own), "plain")}>
        {text(props.own)}
      </Share>
      <Share share={share(props.total)} tone="dim">
        {text(props.total)}
      </Share>
    </>
  );
}

function hottest(total: number | undefined, most: number): string {
  return most > 0 && total === most ? "hottest" : "";
}

function useHottestInView(
  body: () => HTMLDivElement | undefined,
  name: () => string,
  current: () => boolean,
) {
  let shown: string | null = null;
  createEffect(
    () => ({ name: name(), current: current() }),
    ({ name: currentName, current: isCurrent }) => {
      if (isCurrent && shown !== currentName) {
        shown = currentName;
        body()?.querySelector(".hottest")?.scrollIntoView({ block: "center" });
      }
    },
  );
}
