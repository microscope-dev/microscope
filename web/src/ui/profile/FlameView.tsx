import { type MouseEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";

import { assert } from "../../assert";
import { diffModeOf, type ProfileLook, withLook } from "../../profile/look";
import { changeFill, changeOf, fill, formatPercent, formatWeight } from "../../profile/paint";
import {
  type CallTree,
  findPath,
  hit,
  layout,
  matching,
  nameOf,
  pathOf,
  type Rect,
} from "../../profile/tree";
import { useShownScheme } from "../../theme";
import { prepare, useWidth } from "./canvas";
import { token, useProfile, useTree } from "./context";
import { Unanswered } from "./parts";
import { SearchRow, useFrameSearch, useTreeSearch } from "./Search";

// oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
const unzoom = (look: ProfileLook) => withLook(look, "z", undefined);

/** One row of the flame graph, in CSS pixels. */
const ROW = 18;

/** The flame graph, roots on top, below one row that searches its frames
 * and holds the selected one's details. Clicking a frame zooms to it and
 * selects it, which puts its name in the search. */
export function FlameView() {
  const { query, look, setLook } = useProfile();
  const answer = useTree(query);
  const { tree } = answer;
  const [search, setSearch] = useFrameSearch();
  const { matched, note } = useTreeSearch(tree, search);
  const zoom = tree && look.z ? findPath(tree, look.z) : null;
  const { fn } = look;
  const selected = useMemo(
    () => (tree && fn ? matching(tree, (name) => name === fn).nodes : null),
    [tree, fn],
  );
  const hasZoom = look.z !== undefined;
  const zoomTo = useCallback(
    (zoomed: CallTree, node: number) => {
      setLook(
        (prev) => ({
          ...withLook(prev, "z", pathOf(zoomed, node)),
          fn: nameOf(zoomed, node),
        }),
        true,
      );
    },
    [setLook],
  );

  return (
    <div className="pane grow">
      <SearchRow
        search={search}
        onSearch={setSearch}
        matched={note}
        reset={hasZoom ? unzoom : null}
      />
      <div className={`pane-body ${answer.current ? "" : "stale"}`}>
        {answer.error || !tree ? (
          <Unanswered error={answer.error} reading="Reading the flame graph…" />
        ) : tree.count === 0 ? (
          <div className="empty">No samples are selected.</div>
        ) : (
          <FlameGraph
            tree={tree}
            zoom={zoom}
            matched={matched}
            selected={selected}
            onZoom={zoomTo}
          />
        )}
      </div>
    </div>
  );
}

function FlameGraph({
  tree,
  zoom,
  matched,
  selected,
  onZoom,
}: {
  tree: CallTree;
  zoom: number | null;
  /** Frames a search found, which stay bright as the rest dim. */
  matched: Set<number> | null;
  /** Frames of the selected function, which are outlined. */
  selected: Set<number> | null;
  onZoom: (tree: CallTree, node: number) => void;
}) {
  const { unit, look } = useProfile();
  const mode = diffModeOf(look);
  const [box, width] = useWidth();
  const canvas = useRef<HTMLCanvasElement>(null);
  const scheme = useShownScheme();
  // What is hovered belongs to one tree; another answer hovers nothing.
  const [hover, setHovered] = useState<{
    tree: CallTree;
    rect: Rect;
    left: number;
    top: number;
  } | null>(null);
  const hovered = hover?.tree === tree ? hover : null;

  const rects = useMemo(
    () => (width > 0 ? layout(tree, zoom, 0.5 / width) : []),
    [tree, zoom, width],
  );
  const depth = rects.reduce((deepest, rect) => Math.max(deepest, rect.depth + 1), 0);
  const height = depth * ROW;

  useEffect(() => {
    const context = prepare(canvas.current, width, height);
    if (!context) {
      return;
    }
    // Painted in the scheme shown, and again when it changes.
    const dark = scheme === "dark";
    const ink = token("--ink");
    const surface = token("--surface");
    const accent = token("--accent");
    assert(
      (tree.baseTotal === null) === (tree.baseWeight === null),
      "a diff tree's base totals and base weight disagree",
    );
    assert(
      tree.baseTotal === null || tree.baseTotal.length === tree.count,
      `${tree.baseTotal?.length} base totals for ${tree.count} nodes`,
    );
    context.font = `11px ${token("--mono")}`;
    context.textBaseline = "middle";
    for (const rect of rects) {
      const x = rect.x * width;
      const w = rect.width * width;
      const y = rect.depth * ROW;
      const key = tree.keys[tree.key[rect.node] ?? 0];
      context.globalAlpha = matched && !matched.has(rect.node) ? 0.35 : 1;
      context.fillStyle = tree.baseTotal
        ? changeFill(
            mode,
            [tree.total[rect.node] ?? 0, tree.weight],
            [tree.baseTotal[rect.node] ?? 0, tree.baseWeight ?? 0],
            dark,
          )
        : key
          ? fill(key, dark)
          : surface;
      context.fillRect(x, y, Math.max(w - 1, 0.5), ROW - 1);
      if (selected?.has(rect.node)) {
        context.strokeStyle = accent;
        context.lineWidth = 2;
        context.strokeRect(x + 1, y + 1, Math.max(w - 3, 0.5), ROW - 3);
      }
      if (w > 28) {
        context.fillStyle = ink;
        context.fillText(clip(context, nameOf(tree, rect.node), w - 8), x + 4, y + ROW / 2);
      }
    }
    context.globalAlpha = 1;
  }, [rects, tree, width, height, matched, selected, scheme, mode]);

  const at = useCallback(
    (event: MouseEvent) => {
      const bounds = event.currentTarget.getBoundingClientRect();
      const x = event.clientX - bounds.left;
      const y = event.clientY - bounds.top;
      const rect = hit(rects, Math.floor(y / ROW), x / bounds.width);
      return rect ? { tree, rect, left: x, top: y } : null;
    },
    [rects, tree],
  );
  const move = useCallback((event: MouseEvent) => setHovered(at(event)), [at]);
  const leave = useCallback(() => setHovered(null), []);
  const click = useCallback(
    (event: MouseEvent) => {
      const found = at(event);
      if (found) {
        onZoom(tree, found.rect.node);
      }
    },
    [at, onZoom, tree],
  );
  const boxStyle = useMemo(() => ({ height }), [height]);
  const canvasStyle = useMemo(() => ({ width: "100%", height }), [height]);
  const tipStyle = useMemo(
    () =>
      hovered
        ? { left: Math.min(hovered.left + 12, Math.max(width - 320, 0)), top: hovered.top + 16 }
        : undefined,
    [hovered, width],
  );
  const shown = hovered ? hovered.rect.node : null;
  return (
    <div ref={box} className="flame" style={boxStyle}>
      <canvas
        ref={canvas}
        style={canvasStyle}
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a canvas drawn here, not an image file
        role="img"
        aria-label="Flame graph"
        onMouseMove={move}
        onMouseLeave={leave}
        onClick={click}
      />
      {hovered && shown !== null ? (
        <div className="flame-tip" data-testid="flame-tip" style={tipStyle}>
          <div className="mono">{nameOf(tree, shown)}</div>
          <div className="muted">
            {formatWeight(tree.total[shown] ?? 0, unit)} ·{" "}
            {formatPercent(tree.total[shown] ?? 0, tree.weight)} total,{" "}
            {formatPercent(tree.own[shown] ?? 0, tree.weight)} own
          </div>
          {tree.baseTotal ? (
            <div className="muted">
              base {formatWeight(tree.baseTotal[shown] ?? 0, unit)} ·{" "}
              {formatPercent(tree.baseTotal[shown] ?? 0, tree.baseWeight ?? 0)} total ·{" "}
              {
                changeOf(
                  mode,
                  [tree.total[shown] ?? 0, tree.weight],
                  [tree.baseTotal[shown] ?? 0, tree.baseWeight ?? 0],
                  unit,
                ).text
              }
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

/** The longest start of `text`, with an ellipsis, that fits `width`. */
function clip(context: CanvasRenderingContext2D, text: string, width: number): string {
  if (context.measureText(text).width <= width) {
    return text;
  }
  let low = 0;
  let high = text.length;
  while (low < high) {
    const middle = Math.ceil((low + high) / 2);
    if (context.measureText(`${text.slice(0, middle)}…`).width <= width) {
      low = middle;
    } else {
      high = middle - 1;
    }
  }
  return low > 0 ? `${text.slice(0, low)}…` : "";
}
