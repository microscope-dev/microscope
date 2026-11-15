import { useVirtualizer, type Virtualizer } from "@tanstack/react-virtual";
import {
  type CSSProperties,
  Fragment,
  type KeyboardEvent,
  type MouseEvent,
  type RefObject,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

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
import { fileName } from "../paths";
import { type Painted, paintLines } from "../source/languages";
import { useAnswer, useProfile } from "./context";
import { ownTone, Share, Unanswered } from "./parts";
import { COLUMN_HELP } from "./TreeView";

export function SourceLines({ name }: { name: string }) {
  const { query } = useProfile();
  const answer = useAnswer(
    "profileLines",
    useMemo(() => ({ ...query, function: name }), [query, name]),
  );
  const body = useRef<HTMLDivElement>(null);
  useHottestInView(body, name, answer.current);
  const files = answer.data?.files;
  const painted = useMemo(
    () => files?.map((file) => (file.text ? paintLines(file.path ?? "", file.text) : [])),
    [files],
  );
  if (answer.error || !files) {
    return <Unanswered error={answer.error} reading={`Reading the source lines of ${name}…`} />;
  }
  if (files.length === 0) {
    return <div className="empty">No selected sample has a source line in {name}.</div>;
  }
  const weight = answer.data?.weight ?? 0;
  let most = 0;
  for (const file of files) {
    most = largest(
      file.lines.map(([, , total]) => total),
      most,
    );
  }
  return (
    <div
      ref={body}
      className={`pane-body annotated ${answer.current ? "" : "stale"}`}
      data-testid="source-lines"
    >
      {files.map((file, fileAt) => {
        const costs = new Map(file.lines.map(([line, own, total]) => [line, [own, total]]));
        return (
          <div key={file.path ?? ""} className="annotated-file">
            <div className="annotated-head mono">{file.path ?? "no file"}</div>
            {file.text ? (
              file.text.map((text, at) => {
                const line = file.first + at;
                const cost = costs.get(line);
                return (
                  <div
                    key={line}
                    className={`row annotated-row ${cost ? "hot" : ""} ${hottest(cost?.[1], most)}`}
                  >
                    <Costs own={cost?.[0]} total={cost?.[1]} weight={weight} />
                    <span className="line-number">{line}</span>
                    <code>
                      <Tokens runs={painted?.[fileAt]?.[at]} text={text} />
                    </code>
                  </div>
                );
              })
            ) : (
              <div className="empty">{file.unread}</div>
            )}
          </div>
        );
      })}
    </div>
  );
}

export function Code({ name }: { name: string }) {
  const { query } = useProfile();
  const answer = useAnswer(
    "profileCode",
    useMemo(() => ({ ...query, function: name }), [query, name]),
  );

  const answered = useRef(name);
  if (answer.current) {
    // oxlint-disable-next-line react/refs -- follows the answer without a second render
    answered.current = name;
  }
  // oxlint-disable-next-line react/refs -- follows the answer without a second render
  const shownName = answered.current;
  const code = answer.data;
  if (answer.error || !code) {
    return <Unanswered error={answer.error} reading={`Reading the instructions of ${name}…`} />;
  }
  if (code.unavailable) {
    return <div className="empty">{code.unavailable}</div>;
  }
  if (code.blocks.length === 0 && code.unplaced.length === 0) {
    return <div className="empty">No selected sample ran an instruction of {name}.</div>;
  }

  return (
    <Split
      key={`${shownName}\n${code.blocks[0]?.symbol ?? ""}`}
      name={shownName}
      code={code}
      current={answer.current}
    />
  );
}

interface Chosen {
  line: string;
  run: number | null;
  row: number | null;
}

function Split({ name, code, current }: { name: string; code: ProfileCode; current: boolean }) {
  const rows = useMemo(() => codeRows(code), [code]);
  const tree = useMemo(() => inlineTree(rows), [rows]);

  const opening = useMemo(
    () =>
      openingLine(tree, rows, code.files, code.inlined_into ? code.functions.indexOf(name) : -1),
    [tree, rows, code.files, code.inlined_into, code.functions, name],
  );
  const [open, setOpen] = useState<ReadonlySet<string>>(
    () => new Set(opening ? linesAbove(opening) : []),
  );
  const [chosen, setChosen] = useState<Chosen | null>(null);
  const source = useRef<HTMLDivElement>(null);
  const listing = useRef<HTMLDivElement>(null);

  const entries = useMemo(() => sourceEntries(tree, code.files, open), [tree, code.files, open]);
  const tints = useMemo(() => tintsOf(tree, open), [tree, open]);
  const from = useMemo(() => inlinedFrom(tree, code.files), [tree, code.files]);
  const line = (chosen ? tree.lines.get(chosen.line) : undefined) ?? opening;
  const runs = useMemo(() => runsOf(line?.rows ?? []), [line]);
  const run = chosen?.run ?? hottestRun(rows, runs);
  const shown = chosen?.row ?? runs[run]?.[0];
  const selected = useMemo(() => new Set(line?.rows), [line]);
  const paint = useMemo(() => painter(code.files), [code.files]);

  const entryOf = useMemo(
    () =>
      new Map(
        entries.flatMap((entry, at) => (entry.kind === "line" ? [[entry.line.key, at]] : [])),
      ),
    [entries],
  );
  const coded = useMemo(
    () => entries.flatMap((entry) => (entry.kind === "line" ? [entry.line] : [])),
    [entries],
  );

  const items = useMemo(
    () =>
      rows.flatMap((row, index): Item[] =>
        row.symbol !== null && code.blocks.length > 1
          ? [{ symbol: row.symbol }, { row: index }]
          : [{ row: index }],
      ),
    [rows, code.blocks.length],
  );
  const itemOf = useMemo(() => {
    const at: number[] = [];
    for (const [item, each] of items.entries()) {
      if ("row" in each) {
        at[each.row] = item;
      }
    }
    return at;
  }, [items]);
  const sourceList = usePane(entries.length, source);
  const listingList = usePane(items.length, listing);
  const longestSource = useMemo(
    () => longest(entries.map((entry) => entryText(entry, code))),
    [entries, code],
  );
  const longestListing = useMemo(
    () =>
      longest(
        rows.map(
          ({ instruction }) =>
            instruction.tokens.map(([, text]) => text).join("") +
            (instruction.target ? ` <${instruction.target}>` : ""),
        ),
      ),
    [rows],
  );

  const brackets = useMemo(() => {
    const firsts: number[] = [];
    const holds = new Set<number>();
    for (const [index, function_] of from.entries()) {
      const first =
        index > 0 && from[index - 1] === function_ ? (firsts[index - 1] ?? index) : index;
      firsts.push(first);
      if (function_ !== null && selected.has(index)) {
        holds.add(first);
      }
    }
    return { firsts, holds };
  }, [from, selected]);
  const bracketed = from.some((function_) => function_ !== null);

  const lineKey = line?.key;
  useEffect(() => {
    reveal(source.current, lineKey === undefined ? -1 : (entryOf.get(lineKey) ?? -1));
    if (shown !== undefined) {
      reveal(listing.current, itemOf[shown] ?? -1);
    }
  }, [lineKey, shown, entryOf, itemOf]);

  const choose = useCallback((to: TreeLine, at: number | null, row: number | null) => {
    const above = linesAbove(to);
    setOpen((was) => (above.every((key) => was.has(key)) ? was : new Set([...was, ...above])));
    setChosen({ line: to.key, run: at, row });
  }, []);
  const toggle = useCallback((key: string) => {
    setOpen((was) => {
      const now = new Set(was);
      if (!now.delete(key)) {
        now.add(key);
      }
      return now;
    });
  }, []);

  const step = useCallback(
    (by: number) => {
      if (line && runs.length > 0) {
        setChosen({ line: line.key, run: (run + by + runs.length) % runs.length, row: null });
      }
    },
    [line, run, runs.length],
  );
  const previous = useCallback(() => step(-1), [step]);
  const next = useCallback(() => step(1), [step]);

  const keys = useCallback(
    (event: KeyboardEvent<HTMLDivElement>) => {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const at = line ? coded.indexOf(line) : -1;
        const to = coded[at + (event.key === "ArrowDown" ? 1 : -1)];
        if (to && at >= 0) {
          choose(to, null, null);
        }
      } else if (event.key === "Enter" && line && line.inlined.length > 0) {
        event.preventDefault();
        toggle(line.key);
      } else if (event.key === "]" || event.key === "ArrowRight") {
        event.preventDefault();
        step(1);
      } else if (event.key === "[" || event.key === "ArrowLeft") {
        event.preventDefault();
        step(-1);
      }
    },
    [coded, line, choose, toggle, step],
  );

  const pickLine = useCallback(
    (event: MouseEvent<HTMLDivElement>) => {
      const target = event.target as Element;
      const opener = target.closest<HTMLElement>("[data-toggle]")?.dataset.toggle;
      if (opener !== undefined) {
        toggle(opener);
        return;
      }
      const picked = tree.lines.get(target.closest<HTMLElement>("[data-line]")?.dataset.line ?? "");
      if (picked) {
        choose(picked, null, null);
      }
    },
    [tree, choose, toggle],
  );

  const indexOf = useMemo(
    () => new Map(rows.map((row, index) => [row.instruction.address, index])),
    [rows],
  );

  const pickRow = useCallback(
    (event: MouseEvent<HTMLDivElement>) => {
      const target = event.target as Element;
      const jump = target.closest<HTMLElement>("[data-jump]");
      const row = target.closest<HTMLElement>("[data-row]");
      const index = jump ? indexOf.get(Number(jump.dataset.jump)) : Number(row?.dataset.row);
      const path = index === undefined ? undefined : tree.paths[index];
      const to = path?.[ownCodeAt(path, code.files)];
      if (index === undefined || !to) {
        return;
      }
      const at = runsOf(to.rows).findIndex(([first, last]) => index >= first && index <= last);
      choose(to, at, index);
    },
    [indexOf, tree, code.files, choose],
  );

  const counted = runs.reduce((sum, [first, last]) => sum + last - first + 1, 0);
  const clicked = chosen?.row === null || chosen === null ? undefined : rows[chosen.row];

  const lineDigits = useMemo(() => {
    const widest = Math.max(1, ...[...tree.lines.values()].map((each) => each.line));
    return { "--digits": String(widest).length } as CSSProperties;
  }, [tree]);
  const offsetDigits = useMemo(() => {
    const most = rows.reduce((widest, row) => Math.max(widest, row.offset), 0);
    return { "--digits": most.toString(16).length + 1 } as CSSProperties;
  }, [rows]);

  return (
    <div className={`code-split ${current ? "" : "stale"}`} data-testid="code">
      <div className="code-split-head">
        <span>{code.files[0]?.path ?? "no source"}</span>
        <span className="muted end">{code.module}</span>
      </div>
      {code.inlined_into ? (
        <div className="code-split-note">
          {name} has no code of its own. Every sample ran it inlined into {code.inlined_into}, shown
          here.
        </div>
      ) : null}
      <div className="code-split-panes">
        <div
          ref={source}
          className="code-split-pane"
          style={lineDigits}
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- rows of costs and code a select cannot hold
          role="listbox"
          aria-label="Source"
          tabIndex={0}
          onClick={pickLine}
          onKeyDown={keys}
        >
          <CostHead />
          <Sizer text={longestSource} />
          {entries.length > 0 ? (
            <>
              <Gap list={sourceList} side="before" />
              {sourceList.getVirtualItems().map(({ index: at }) => {
                const entry = entries[at];
                return entry ? (
                  <SourceRow
                    key={entry.kind === "line" ? entry.line.key : `${entry.kind} ${at}`}
                    entry={entry}
                    code={code}
                    chosen={entry.kind === "line" && entry.line === line}
                    open={open}
                    tints={tints}
                    paint={paint}
                  />
                ) : null;
              })}
              <Gap list={sourceList} side="after" />
            </>
          ) : (
            <div className="empty">
              {code.unlined ?? code.files[0]?.unread ?? "The instructions have no source lines."}
            </div>
          )}
        </div>
        <div
          ref={listing}
          className="code-split-pane"
          style={offsetDigits}
          // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- rows of costs and code a select cannot hold
          role="listbox"
          aria-label="Assembly"
          aria-multiselectable
          tabIndex={0}
          onClick={pickRow}
          onKeyDown={keys}
        >
          <CostHead from={bracketed} />
          <Sizer text={longestListing} from={bracketed} />
          <Gap list={listingList} side="before" />
          {listingList.getVirtualItems().map(({ index: item }) => {
            const each = items[item];
            if (each && "symbol" in each) {
              return (
                <div key={`symbol ${item}`} className="annotated-head">
                  {each.symbol}
                </div>
              );
            }
            const index = each?.row ?? -1;
            const row = rows[index];
            if (!row) {
              return null;
            }
            const instruction = row.instruction;
            const path = tree.paths[index] ?? [];
            const tint = tintOfPath(path, open, tints);
            const unsourced = shownLine(path, open)?.line === 0;
            return (
              <div
                key={instruction.address}
                // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
                role="option"
                aria-selected={selected.has(index)}
                data-row={index}
                data-tint={tint}
                style={tinted(tint)}
                className={`code-row ${row.leads ? "leads" : ""} ${index === shown ? "shown" : ""} ${unsourced ? "unsourced" : ""}`}
              >
                <span className="pin">
                  <Costs
                    own={instruction.own > 0 ? instruction.own : undefined}
                    total={instruction.total > 0 ? instruction.total : undefined}
                    weight={code.weight}
                  />
                  {bracketed ? (
                    <Bracket
                      code={code}
                      from={from}
                      index={index}
                      holds={brackets.holds.has(brackets.firsts[index] ?? -1)}
                    />
                  ) : null}
                  <span className="offset">+{row.offset.toString(16)}</span>
                </span>
                <code className="text">
                  {row.jump === null ? (
                    <>
                      <AsmTokens tokens={instruction.tokens} />
                      {instruction.target ? (
                        <span className="asm-target"> &lt;{instruction.target}&gt;</span>
                      ) : null}
                    </>
                  ) : (
                    <>
                      {}
                      <AsmTokens tokens={instruction.tokens.slice(0, -1)} />
                      <button type="button" className="asm-target" data-jump={row.jump}>
                        +{(rows[indexOf.get(row.jump) ?? index]?.offset ?? 0).toString(16)}
                      </button>
                    </>
                  )}
                </code>
              </div>
            );
          })}
          <Gap list={listingList} side="after" />
          {code.blocks.map((block) =>
            block.stopped ? (
              <div key={block.symbol} className="empty">
                {block.stopped}
              </div>
            ) : null,
          )}
          {code.unplaced.length > 0 ? (
            <div className="empty">
              {formatCount(code.unplaced.length, "sampled address", "sampled addresses")}{" "}
              {code.unplaced.length === 1 ? "lies" : "lie"} outside every decoded instruction.
            </div>
          ) : null}
        </div>
      </div>
      <div className="code-split-foot">
        <span>
          {line ? whereOf(code, line) : "no line"} · {formatCount(counted, "instruction")} in{" "}
          {formatCount(runs.length, "run")}
        </span>
        {clicked ? (
          <span className="muted chain" title={chainOf(code, clicked, false)}>
            {chainOf(code, clicked, true)}
          </span>
        ) : null}
        <span className="end">
          <button
            type="button"
            className="tb ghost"
            aria-label="Previous run"
            disabled={runs.length < 2}
            onClick={previous}
          >
            ‹
          </button>
          <span data-testid="run">
            {runs.length > 0 ? `run ${run + 1} of ${runs.length}` : "no run"}
          </span>
          <button
            type="button"
            className="tb ghost"
            aria-label="Next run"
            disabled={runs.length < 2}
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

function tinted(tint: number | undefined): CSSProperties | undefined {
  return tint === undefined ? undefined : ({ "--tint": tint } as CSSProperties);
}

function SourceRow({
  entry,
  code,
  chosen,
  open,
  tints,
  paint,
}: {
  entry: SourceEntry;
  code: ProfileCode;
  chosen: boolean;
  open: ReadonlySet<string>;
  tints: ReadonlyMap<string, number>;
  paint: (file: number, number: number) => Painted | undefined;
}) {
  if (entry.kind === "call") {
    const full = code.functions[entry.call.function] ?? "?";
    const at = entry.call.at;
    const path = code.files[entry.call.lines[0]?.file ?? -1]?.path;
    const total = entry.call.lines.reduce((sum, each) => sum + each.total, 0);
    return (
      <div
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
        role="option"
        aria-selected={false}
        aria-disabled
        className="code-row code-call nocode"
      >
        <span className="pin">
          <Costs own={undefined} total={total > 0 ? total : undefined} weight={code.weight} />
          <span className="line-number" />
          <span className="inline-toggle" />
        </span>
        <code className="text">
          <Indent depth={entry.depth} />
          <span className="tk-f" title={full}>
            {at ? shortName(full) : full}
          </span>
          {path ? <span className="muted"> {fileName(path)}</span> : null}
          {at ? <span className="muted"> · inlined at line {at.line}</span> : null}
        </code>
      </div>
    );
  }

  if (entry.kind === "gap") {
    return (
      <div className="code-row nocode" aria-hidden="true">
        <span className="pin">
          <Costs own={undefined} total={undefined} weight={code.weight} />
          <span className="line-number" />
          <span className="inline-toggle" />
        </span>
        <code className="text muted">
          <Indent depth={entry.depth} />⋯ {formatCount(entry.count, "line")}
        </code>
      </div>
    );
  }

  if (entry.kind === "text") {
    const shownFile = code.files[entry.file];
    return (
      <div
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
        role="option"
        aria-selected={false}
        aria-disabled
        className="code-row nocode"
      >
        <span className="pin">
          <Costs own={undefined} total={undefined} weight={code.weight} />
          <span className="line-number">{entry.number}</span>
          <span className="inline-toggle" />
        </span>
        <code className="text">
          <Indent depth={entry.depth} />
          <Tokens
            runs={paint(entry.file, entry.number)}
            text={shownFile?.text?.[entry.number - shownFile.first] ?? ""}
          />
        </code>
      </div>
    );
  }

  const line = entry.line;
  const tint = tints.get(line.key);
  const opened = open.has(line.key);
  const shownFile = line.file === null ? undefined : code.files[line.file];
  const text = shownFile?.text?.[line.line - shownFile.first];
  return (
    <div
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
      role="option"
      aria-selected={chosen}
      data-line={line.key}
      data-tint={tint}
      style={tinted(tint)}
      className={`code-row ${line.line === 0 ? "unsourced" : ""}`}
    >
      <span className="pin">
        <Costs
          own={line.own > 0 ? line.own : undefined}
          total={line.total > 0 ? line.total : undefined}
          weight={code.weight}
        />
        <span className="line-number">{line.line > 0 ? line.line : ""}</span>
        {line.inlined.length > 0 ? (
          <button
            type="button"
            className="inline-toggle"
            data-toggle={line.key}
            aria-expanded={opened}
            aria-label={`${opened ? "Hide" : "Show"} the code inlined at line ${line.line}`}
          >
            {opened ? "▾" : "▸"}
          </button>
        ) : (
          <span className="inline-toggle" />
        )}
      </span>
      <code className="text">
        <Indent depth={entry.depth} />
        {line.line === 0 ? (
          <span className="unsourced-note">
            no associated source line · {formatCount(line.rows.length, "instruction")}
          </span>
        ) : text !== undefined && line.file !== null ? (
          <Tokens runs={paint(line.file, line.line)} text={text} />
        ) : (
          <span className="muted" title={shownFile?.unread ?? undefined}>
            {shownFile?.path ? `${fileName(shownFile.path)}:${line.line}` : `line ${line.line}`}
          </span>
        )}
        {line.inlined.length > 0 && !opened ? (
          <span className="muted inlines">
            {"  inlines "}
            {line.inlined.slice(0, INLINES_NAMED).map((call, at) => (
              <Fragment key={call.key}>
                {at > 0 ? ", " : null}
                <span title={code.functions[call.function]}>
                  {shortName(code.functions[call.function] ?? "?")}
                </span>
              </Fragment>
            ))}
            {line.inlined.length > INLINES_NAMED
              ? ` +${line.inlined.length - INLINES_NAMED}`
              : null}
          </span>
        ) : null}
      </code>
    </div>
  );
}

const INLINES_NAMED = 3;

function Indent({ depth }: { depth: number }) {
  return depth > 0 ? (
    <span className="code-indent" aria-hidden="true">
      {Array.from({ length: depth }, (_, at) => (
        <span key={at} />
      ))}
    </span>
  ) : null;
}

function Bracket({
  code,
  from,
  index,
  holds,
}: {
  code: ProfileCode;
  from: readonly (number | null)[];
  index: number;
  holds: boolean;
}) {
  const inlined = from[index] ?? null;
  if (inlined === null) {
    return <span className="inlined-from" />;
  }
  const before = from[index - 1] === inlined;
  const after = from[index + 1] === inlined;
  const part = before && after ? "middle" : before ? "end" : after ? "start" : "one";
  const full = code.functions[inlined] ?? "?";
  return (
    <span
      className={`inlined-from ${holds ? "holds" : ""}`}
      data-bracket={part}
      title={before ? undefined : full}
    >
      {before ? null : shortName(full)}
    </span>
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

function Tokens({ runs, text }: { runs: Painted | undefined; text: string }) {
  if (!runs) {
    return text;
  }
  return runs.map(([part, token], at) =>
    token ? (
      // oxlint-disable-next-line react/no-array-index-key -- runs are a line's text in order, never reordered
      <span key={at} className={token}>
        {part}
      </span>
    ) : (
      // oxlint-disable-next-line react/no-array-index-key -- runs are a line's text in order, never reordered
      <Fragment key={at}>{part}</Fragment>
    ),
  );
}

function AsmTokens({ tokens }: { tokens: readonly ProfileToken[] }) {
  return tokens.map(([kind, text], at) => (
    // oxlint-disable-next-line react/no-array-index-key -- an instruction's pieces in order, never reordered
    <span key={at} className={`tk-${kind}`}>
      {text}
    </span>
  ));
}

function CostHead({ from = false }: { from?: boolean }) {
  return (
    <div className="code-row code-head" aria-hidden="true">
      <span className="pin">
        {(["own", "total"] as const).map((column) => (
          <span key={column} className="percent" title={COLUMN_HELP[column]}>
            <span className="hover-help">{column}</span>
          </span>
        ))}
        {from ? <span className="inlined-from">inlined from</span> : null}
        <span className="line-number" />
      </span>
    </div>
  );
}

const ROW = 20;

type Item = { row: number } | { symbol: string };

function usePane(count: number, pane: RefObject<HTMLDivElement | null>) {
  // oxlint-disable-next-line react/incompatible-library -- the app is not built with the React Compiler
  return useVirtualizer({
    count,
    getScrollElement: () => pane.current,
    estimateSize: () => ROW,
    overscan: 30,
    scrollMargin: ROW,
  });
}

function Gap({
  list,
  side,
}: {
  list: Virtualizer<HTMLDivElement, Element>;
  side: "before" | "after";
}) {
  const drawn = list.getVirtualItems();
  const height =
    side === "before"
      ? (drawn[0]?.start ?? ROW) - ROW
      : list.getTotalSize() - ((drawn.at(-1)?.end ?? ROW) - ROW);
  const style = useMemo(() => ({ height }), [height]);
  return <div style={style} aria-hidden="true" />;
}

function Sizer({ text, from = false }: { text: string; from?: boolean }) {
  return (
    <div className="code-row code-sizer" aria-hidden="true">
      <span className="pin">
        <span className="percent" />
        <span className="percent" />
        {from ? <span className="inlined-from" /> : null}
        <span className="line-number" />
        <span className="inline-toggle" />
      </span>
      <code className="text" data-text={text} />
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

function Costs({
  own,
  total,
  weight,
}: {
  own: number | undefined;
  total: number | undefined;
  weight: number;
}) {
  const share = (part: number | undefined) => (part ?? 0) / Math.max(weight, 1);
  const text = (part: number | undefined) =>
    part === undefined ? "" : formatPercent(part, weight);
  return (
    <>
      <Share share={share(own)} tone={ownTone(share(own), "plain")}>
        {text(own)}
      </Share>
      <Share share={share(total)} tone="dim">
        {text(total)}
      </Share>
    </>
  );
}

function hottest(total: number | undefined, most: number): string {
  return most > 0 && total === most ? "hottest" : "";
}

function useHottestInView(body: RefObject<HTMLDivElement | null>, name: string, current: boolean) {
  const shown = useRef<string | null>(null);
  useEffect(() => {
    if (current && shown.current !== name) {
      shown.current = name;
      body.current?.querySelector(".hottest")?.scrollIntoView({ block: "center" });
    }
  });
}
