import { type RefObject, useEffect, useMemo, useRef } from "react";
import { formatCount, formatPercent, largest } from "../../profile/paint";
import { useAnswer, useProfile } from "./context";
import { Unanswered } from "./parts";

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
                    <span className="percent">
                      {cost ? formatPercent(cost[0] ?? 0, weight) : ""}
                    </span>
                    <span className="percent dim">
                      {cost ? formatPercent(cost[1] ?? 0, weight) : ""}
                    </span>
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

/** A function's samples on its instructions, or why its code is unknown. */
export function Code({ name }: { name: string }) {
  const { query } = useProfile();
  const answer = useAnswer(
    "profileCode",
    useMemo(() => ({ ...query, function: name }), [query, name]),
  );
  const body = useRef<HTMLDivElement>(null);
  useHottestInView(body, name, answer.current);
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
  const weight = code.weight;
  let most = 0;
  for (const block of code.blocks) {
    most = largest(
      block.instructions.map((each) => each.total),
      most,
    );
  }
  return (
    <div
      ref={body}
      className={`pane-body annotated ${answer.current ? "" : "stale"}`}
      data-testid="code"
    >
      {code.blocks.map((block) => (
        <div key={block.symbol} className="annotated-file">
          <div className="annotated-head mono">
            {block.symbol} <span className="muted">{code.module}</span>
          </div>
          {block.instructions.map((instruction) => (
            <div
              key={instruction.address}
              className={`row annotated-row ${instruction.total > 0 ? "hot" : ""} ${hottest(instruction.total, most)}`}
            >
              <span className="percent">
                {instruction.own > 0 ? formatPercent(instruction.own, weight) : ""}
              </span>
              <span className="percent dim">
                {instruction.total > 0 ? formatPercent(instruction.total, weight) : ""}
              </span>
              <span className="line-number">{instruction.address.toString(16)}</span>
              <code>
                {instruction.text}
                {instruction.target ? (
                  <span className="asm-target">&lt;{instruction.target}&gt;</span>
                ) : null}
              </code>
            </div>
          ))}
          {block.stopped ? <div className="empty">{block.stopped}</div> : null}
        </div>
      ))}
      {code.unplaced.length > 0 ? (
        <div className="empty">
          {formatCount(code.unplaced.length, "sampled address", "sampled addresses")}{" "}
          {code.unplaced.length === 1 ? "lies" : "lie"} outside every decoded instruction.
        </div>
      ) : null}
    </div>
  );
}

/** The class of the row that weighs `total` when it weighs the most. */
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
