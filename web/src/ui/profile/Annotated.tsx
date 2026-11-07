import {
  type ChangeEvent,
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
  lineIn,
  linesOf,
  openingLine,
  runsOf,
  shortName,
  tintsOf,
} from "../../profile/code";
import { formatCount, formatPercent, largest } from "../../profile/paint";
import type { ProfileCode } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { ownTone, Share, Unanswered } from "./parts";
import { COLUMN_HELP } from "./TreeView";

/** A function's samples on its source lines, by file, as each reads now. */
export function SourceLines({ name }: { name: string }) {
  const { query } = useProfile();
  const answer = useAnswer(
    "profileLines",
    useMemo(() => ({ ...query, function: name }), [query, name]),
  );
  const body = useRef<HTMLDivElement>(null);
  useHottestInView(body, name, answer.current);
  const files = answer.data?.files;
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
      {files.map((file) => {
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
                    <code>{text}</code>
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

/** A function's instructions beside the source they were compiled from,
 * or why its code is unknown. */
export function Code({ name }: { name: string }) {
  const { query } = useProfile();
  const answer = useAnswer(
    "profileCode",
    useMemo(() => ({ ...query, function: name }), [query, name]),
  );
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
  // A kept answer of the function shown before must not leave its
  // selection behind once this one's arrives.
  return (
    <Split
      key={`${name}\n${code.blocks[0]?.symbol ?? ""}`}
      name={name}
      code={code}
      current={answer.current}
    />
  );
}

/** The line chosen, which of its runs, and the instruction clicked. A
 * null run is the hottest; a null row is the run's first. */
interface Chosen {
  line: number;
  run: number | null;
  row: number | null;
}

/** Source on the left, instructions on the right. Each of the file's
 * hottest lines has a tint its instructions share; the chosen line's
 * instructions are selected, and its runs are stepped through. */
function Split({ name, code, current }: { name: string; code: ProfileCode; current: boolean }) {
  const [fileAt, setFileAt] = useState(0);
  const [chosen, setChosen] = useState<Chosen | null>(null);
  const source = useRef<HTMLDivElement>(null);
  const listing = useRef<HTMLDivElement>(null);

  const file = code.files[fileAt];
  const rows = useMemo(() => codeRows(code), [code]);
  const lines = useMemo(
    () => rows.map((row) => lineIn(row.instruction.lines, fileAt)),
    [rows, fileAt],
  );
  const tints = useMemo(() => (file ? tintsOf(file) : new Map<number, number>()), [file]);
  const coded = useMemo(
    () => [...new Set(lines)].filter((line) => line > 0).toSorted((a, b) => a - b),
    [lines],
  );
  // A function with no code of its own opens on its lines, not those of
  // the function it was inlined into.
  const inlinedLines = useMemo(
    () =>
      code.inlined_into ? linesOf(rows, code.functions.indexOf(name), fileAt) : new Set<number>(),
    [rows, code.inlined_into, code.functions, name, fileAt],
  );
  const line = chosen?.line ?? openingLine(file, lines, inlinedLines);
  const runs = useMemo(() => runsOf(lines, line), [lines, line]);
  const run = chosen?.run ?? hottestRun(rows, runs);
  const shown = chosen?.row ?? runs[run]?.[0];

  useEffect(() => {
    reveal(source.current, source.current?.querySelector(`[data-line="${line}"]`));
    if (shown !== undefined) {
      reveal(listing.current, listing.current?.querySelector(`[data-row="${shown}"]`));
    }
  }, [line, shown]);

  const step = useCallback(
    (by: number) => {
      if (runs.length > 0) {
        setChosen({ line, run: (run + by + runs.length) % runs.length, row: null });
      }
    },
    [line, run, runs.length],
  );
  const previous = useCallback(() => step(-1), [step]);
  const next = useCallback(() => step(1), [step]);

  const keys = useCallback(
    (event: KeyboardEvent<HTMLDivElement>) => {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        const down = event.key === "ArrowDown";
        const to = down ? coded.find((each) => each > line) : coded.findLast((each) => each < line);
        event.preventDefault();
        if (to !== undefined) {
          setChosen({ line: to, run: null, row: null });
        }
      } else if (event.key === "]" || event.key === "ArrowRight") {
        event.preventDefault();
        step(1);
      } else if (event.key === "[" || event.key === "ArrowLeft") {
        event.preventDefault();
        step(-1);
      }
    },
    [coded, line, step],
  );

  const pickLine = useCallback(
    (event: MouseEvent<HTMLDivElement>) => {
      const row = (event.target as Element).closest<HTMLElement>("[data-line]");
      const picked = Number(row?.dataset.line);
      if (coded.includes(picked)) {
        setChosen({ line: picked, run: null, row: null });
      }
    },
    [coded],
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
      if (index === undefined || Number.isNaN(index)) {
        return;
      }
      const picked = lines[index] ?? 0;
      const at = runsOf(lines, picked).findIndex(
        ([first, last]) => index >= first && index <= last,
      );
      setChosen({ line: picked, run: at, row: index });
    },
    [indexOf, lines],
  );

  const chooseFile = useCallback((event: ChangeEvent<HTMLSelectElement>) => {
    setFileAt(Number(event.target.value));
    setChosen(null);
  }, []);

  const costs = new Map(file?.lines.map(([at, own, total]) => [at, [own, total] as const]));
  const counted = runs.reduce((sum, [first, last]) => sum + last - first + 1, 0);
  const clicked = chosen?.row === null || chosen === null ? undefined : rows[chosen.row];
  // Each pane's numbers take as many characters as its widest one.
  const lastLine = file?.text ? file.first + file.text.length - 1 : 0;
  const lineDigits = useMemo(
    () => ({ "--digits": String(lastLine).length }) as CSSProperties,
    [lastLine],
  );
  const offsetDigits = useMemo(() => {
    const most = rows.reduce((widest, row) => Math.max(widest, row.offset), 0);
    return { "--digits": most.toString(16).length + 1 } as CSSProperties;
  }, [rows]);

  return (
    <div className={`code-split ${current ? "" : "stale"}`} data-testid="code">
      <div className="code-split-head">
        {code.files.length > 1 ? (
          <select className="input" aria-label="Source file" value={fileAt} onChange={chooseFile}>
            {code.files.map((each, at) => (
              <option key={each.path ?? at} value={at}>
                {each.path ?? "no file"}
              </option>
            ))}
          </select>
        ) : (
          <span>{file?.path ?? "no source"}</span>
        )}
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
          {file?.text ? (
            file.text.map((text, at) => {
              const number = file.first + at;
              const cost = costs.get(number);
              const has = coded.includes(number);
              return (
                <div
                  key={number}
                  // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
                  role="option"
                  aria-selected={number === line}
                  aria-disabled={!has}
                  data-line={number}
                  data-tint={tints.get(number)}
                  className={`code-row ${has ? "" : "nocode"}`}
                >
                  <span className="pin">
                    <Costs
                      own={cost && cost[0] > 0 ? cost[0] : undefined}
                      total={cost && cost[1] > 0 ? cost[1] : undefined}
                      weight={code.weight}
                    />
                    <span className="line-number">{number}</span>
                  </span>
                  <code className="text">{text}</code>
                </div>
              );
            })
          ) : (
            <div className="empty">
              {file?.unread ?? code.unlined ?? "The instructions have no source lines."}
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
          <CostHead />
          {rows.map((row, index) => {
            const instruction = row.instruction;
            const at = lines[index] ?? 0;
            return (
              <Fragment key={instruction.address}>
                {row.symbol !== null && code.blocks.length > 1 ? (
                  <div className="annotated-head">{row.symbol}</div>
                ) : null}
                <div
                  // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a row of a listbox a select cannot hold
                  role="option"
                  aria-selected={at === line}
                  data-row={index}
                  data-tint={at > 0 ? tints.get(at) : undefined}
                  className={`code-row ${row.leads ? "leads" : ""} ${index === shown ? "shown" : ""}`}
                >
                  <span className="pin">
                    <Costs
                      own={instruction.own > 0 ? instruction.own : undefined}
                      total={instruction.total > 0 ? instruction.total : undefined}
                      weight={code.weight}
                    />
                    <span className="offset">+{row.offset.toString(16)}</span>
                  </span>
                  <code className="text">
                    {row.jump === null ? (
                      <>
                        {instruction.text}
                        {instruction.target ? (
                          <span className="asm-target"> &lt;{instruction.target}&gt;</span>
                        ) : null}
                      </>
                    ) : (
                      <>
                        {instruction.text.replace(/\s*0x[0-9a-f]+$/u, " ")}
                        <button type="button" className="asm-target" data-jump={row.jump}>
                          +{(rows[indexOf.get(row.jump) ?? index]?.offset ?? 0).toString(16)}
                        </button>
                      </>
                    )}
                  </code>
                </div>
              </Fragment>
            );
          })}
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
          {line > 0 ? `line ${line}` : "no line in this file"} ·{" "}
          {formatCount(counted, "instruction")} in {formatCount(runs.length, "run")}
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

/** Where an instruction came from, innermost inlined function first, as
 * `step app.c:8 ← scan app.c:14`, its names shortened or in full. */
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

/** The columns' names, pinned over a code pane's rows. */
function CostHead() {
  return (
    <div className="code-row code-head" aria-hidden="true">
      <span className="pin">
        {(["own", "total"] as const).map((column) => (
          <span key={column} className="percent" title={COLUMN_HELP[column]}>
            <span className="hover-help">{column}</span>
          </span>
        ))}
        <span className="line-number" />
      </span>
    </div>
  );
}

/** Scrolls `pane` to center `row` below its column head when it is out of
 * view. */
function reveal(pane: HTMLElement | null, row: Element | null | undefined): void {
  if (!pane || !(row instanceof HTMLElement)) {
    return;
  }
  const head = pane.querySelector<HTMLElement>(".code-head")?.offsetHeight ?? 0;
  const top = row.offsetTop;
  const bottom = top + row.offsetHeight;
  if (top < pane.scrollTop + head || bottom > pane.scrollTop + pane.clientHeight) {
    pane.scrollTop = top - head - (pane.clientHeight - head - row.offsetHeight) / 2;
  }
}

/** The class of the row that weighs `total` when it weighs the most. */
/** A line's or instruction's own and total shares as bars, as the
 * functions list draws a function's; blank where nothing ran there. */
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

/** Brings the hottest row into view once each function's own rows
 * arrive; until then the rows shown are another function's. */
function useHottestInView(body: RefObject<HTMLDivElement | null>, name: string, current: boolean) {
  const shown = useRef<string | null>(null);
  useEffect(() => {
    if (current && shown.current !== name) {
      shown.current = name;
      body.current?.querySelector(".hottest")?.scrollIntoView({ block: "center" });
    }
  });
}
