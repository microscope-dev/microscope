import { useEffect, useMemo, useRef, useState } from "react";
import { withLook } from "../../profile/look";
import {
  diffFill,
  fill,
  formatChange,
  formatPercent,
  formatWeight,
  shareChange,
} from "../../profile/paint";
import {
  type CallTree,
  decodeTree,
  findPath,
  hit,
  layout,
  nameOf,
  pathOf,
  type Rect,
} from "../../profile/tree";
import { useShownScheme } from "../../theme";
import { prepare, useWidth } from "./canvas";
import { token, useAnswer, useProfile } from "./context";
import { Details } from "./Details";
import { Unanswered } from "./parts";

/** One row of the flame graph, in CSS pixels. */
const ROW = 18;

/** The flame graph, roots on top, with a search and the selected node's
 * details. Clicking a frame zooms to it. */
export function FlameView() {
  const { query, look, setLook } = useProfile();
  const answer = useAnswer("profileTree", query);
  const tree = useMemo(
    () => (answer.data ? decodeTree(answer.data, answer.data.payload) : null),
    [answer.data],
  );
  const [search, setSearch] = useState("");
  const zoom = tree && look.z ? findPath(tree, look.z) : null;
  const matched = useMemo(() => (tree && search ? matching(tree, search) : null), [tree, search]);

  return (
    <div className="pane grow">
      <div className="pane-head">
        Flame graph
        {zoom !== null ? (
          <button
            type="button"
            className="link-button"
            onClick={() => setLook((look) => withLook(look, "z", undefined), true)}
          >
            Reset zoom
          </button>
        ) : null}
        <input
          className="input profile-search"
          placeholder="Search"
          aria-label="Search frames"
          value={search}
          onChange={(event) => setSearch(event.target.value)}
        />
        {matched && tree ? (
          <span className="count" data-testid="matched">
            matched {formatPercent(matched.weight, tree.weight)} of samples
          </span>
        ) : null}
      </div>
      <div className={`pane-body ${answer.current ? "" : "stale"}`}>
        {answer.error || !tree ? (
          <Unanswered error={answer.error} reading="Reading the flame graph…" />
        ) : tree.count === 0 ? (
          <div className="empty">No samples are selected.</div>
        ) : (
          <FlameGraph
            tree={tree}
            zoom={zoom}
            matched={matched?.nodes ?? null}
            onZoom={(node) =>
              setLook(
                (look) => ({
                  ...withLook(look, "z", pathOf(tree, node)),
                  fn: nameOf(tree, node),
                }),
                true,
              )
            }
          />
        )}
      </div>
      {look.fn ? <Details name={look.fn} /> : null}
    </div>
  );
}

/** The nodes whose names hold `search`, and the weight under them,
 * counting nested matches once. */
function matching(tree: CallTree, search: string) {
  const needle = search.toLowerCase();
  const nodes = new Set<number>();
  let weight = 0;
  for (let node = 0; node < tree.count; ) {
    if (nameOf(tree, node).toLowerCase().includes(needle)) {
      weight += tree.total[node] ?? 0;
      for (let inner = node; inner < (tree.end[node] ?? node + 1); inner += 1) {
        if (nameOf(tree, inner).toLowerCase().includes(needle)) {
          nodes.add(inner);
        }
      }
      node = tree.end[node] ?? node + 1;
    } else {
      node += 1;
    }
  }
  return { nodes, weight };
}

function FlameGraph({
  tree,
  zoom,
  matched,
  onZoom,
}: {
  tree: CallTree;
  zoom: number | null;
  matched: Set<number> | null;
  onZoom: (node: number) => void;
}) {
  const { unit } = useProfile();
  const [box, width] = useWidth();
  const canvas = useRef<HTMLCanvasElement>(null);
  const scheme = useShownScheme();
  const [hovered, setHovered] = useState<{ rect: Rect; left: number; top: number } | null>(null);

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
    context.font = `11px ${token("--mono")}`;
    context.textBaseline = "middle";
    for (const rect of rects) {
      const x = rect.x * width;
      const w = rect.width * width;
      const y = rect.depth * ROW;
      const key = tree.keys[tree.key[rect.node] ?? 0];
      context.globalAlpha = matched && !matched.has(rect.node) ? 0.35 : 1;
      context.fillStyle = tree.baseTotal
        ? diffFill(
            (tree.total[rect.node] ?? 0) / Math.max(tree.weight, 1),
            (tree.baseTotal[rect.node] ?? 0) / Math.max(tree.baseWeight ?? 0, 1),
            dark,
          )
        : key
          ? fill(key, dark)
          : surface;
      context.fillRect(x, y, Math.max(w - 1, 0.5), ROW - 1);
      if (w > 28) {
        context.fillStyle = ink;
        context.fillText(clip(context, nameOf(tree, rect.node), w - 8), x + 4, y + ROW / 2);
      }
    }
    context.globalAlpha = 1;
  }, [rects, tree, width, height, matched, scheme]);

  const at = (event: React.MouseEvent) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;
    const rect = hit(rects, Math.floor(y / ROW), x / bounds.width);
    return rect ? { rect, left: x, top: y } : null;
  };
  const shown = hovered ? hovered.rect.node : null;
  return (
    <div ref={box} className="flame" style={{ height }}>
      <canvas
        ref={canvas}
        style={{ width: "100%", height }}
        role="img"
        aria-label="Flame graph"
        onMouseMove={(event) => setHovered(at(event))}
        onMouseLeave={() => setHovered(null)}
        onClick={(event) => {
          const found = at(event);
          if (found) {
            onZoom(found.rect.node);
          }
        }}
      />
      {hovered && shown !== null ? (
        <div
          className="flame-tip"
          data-testid="flame-tip"
          style={{
            left: Math.min(hovered.left + 12, Math.max(width - 320, 0)),
            top: hovered.top + 16,
          }}
        >
          <div className="mono">{nameOf(tree, shown)}</div>
          <div className="muted">
            {formatWeight(tree.total[shown] ?? 0, unit)} ·{" "}
            {formatPercent(tree.total[shown] ?? 0, tree.weight)} total,{" "}
            {formatPercent(tree.own[shown] ?? 0, tree.weight)} own
          </div>
          {tree.baseTotal ? (
            <div className="muted">
              base {formatPercent(tree.baseTotal[shown] ?? 0, tree.baseWeight ?? 0)} total ·{" "}
              {formatChange(
                shareChange(
                  tree.total[shown] ?? 0,
                  tree.weight,
                  tree.baseTotal[shown] ?? 0,
                  tree.baseWeight ?? 0,
                ),
              )}
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
