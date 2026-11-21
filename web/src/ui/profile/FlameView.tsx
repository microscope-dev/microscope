import { createEffect, createMemo, createSignal, Match, Show, Switch } from "solid-js";

import { assert } from "../../assert";
import { diffModeOf, type ProfileLook, withLook } from "../../profile/look";
import {
  changeFill,
  changeOf,
  type DiffMode,
  fill,
  formatPercent,
  formatWeight,
} from "../../profile/paint";
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
import { NoSamples, Unanswered } from "./parts";
import { SearchRow, useFrameSearch, useTreeSearch } from "./Search";

// oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
const unzoom = (look: ProfileLook) => withLook(look, "z", undefined);

const ROW = 18;

export function FlameView() {
  const focus = useProfile();
  const mode = createMemo(() => diffModeOf(focus().look));
  const answer = useTree(() => focus().query);
  const tree = createMemo(() => answer().tree);
  const [search, setSearch] = useFrameSearch();
  const treeSearch = useTreeSearch(tree, search);
  const zoom = createMemo(() => {
    const current = tree();
    const z = focus().look.z;
    return current && z ? findPath(current, z) : null;
  });
  const selected = createMemo(() => {
    const current = tree();
    const fn = focus().look.fn;
    return current && fn ? matching(current, (name) => name === fn).nodes : null;
  });
  const zoomTo = (zoomed: CallTree, node: number) => {
    focus().setLook(
      (prev) => ({
        ...withLook(prev, "z", pathOf(zoomed, node)),
        fn: nameOf(zoomed, node),
      }),
      true,
    );
  };

  return (
    <div class="pane grow">
      <SearchRow
        search={search()}
        onSearch={setSearch}
        matched={treeSearch().note}
        reset={focus().look.z === undefined ? null : unzoom}
      />
      <div class={`pane-body ${answer().current ? "" : "stale"}`}>
        <Switch>
          <Match when={answer().error || !tree()}>
            <Unanswered error={answer().error} reading="Reading the flame graph…" />
          </Match>
          <Match when={tree()?.count === 0}>
            <NoSamples />
          </Match>
          <Match when={tree()}>
            {(current) => (
              <FlameGraph
                tree={current()}
                zoom={zoom()}
                matched={treeSearch().matched}
                selected={selected()}
                onZoom={zoomTo}
                unit={focus().unit}
                mode={mode()}
              />
            )}
          </Match>
        </Switch>
      </div>
    </div>
  );
}

export function FlameGraph(props: {
  tree: CallTree;
  zoom: number | null;
  matched: Set<number> | null;
  selected: Set<number> | null;
  onZoom: (tree: CallTree, node: number) => void;
  unit: string;
  mode?: DiffMode | undefined;
}) {
  const [box, width] = useWidth();
  let canvas: HTMLCanvasElement | undefined;
  const scheme = useShownScheme();

  const [hover, setHovered] = createSignal<{
    tree: CallTree;
    rect: Rect;
    left: number;
    top: number;
  } | null>(null);
  const hovered = createMemo(() => {
    const current = hover();
    return current?.tree === props.tree ? current : null;
  });

  const rects = createMemo(() => {
    const w = width();
    return w > 0 ? layout(props.tree, props.zoom, 0.5 / w) : [];
  });
  const depth = createMemo(() =>
    rects().reduce((deepest, rect) => Math.max(deepest, rect.depth + 1), 0),
  );
  const height = createMemo(() => depth() * ROW);
  const mode = createMemo<DiffMode>(() => props.mode ?? "time");

  createEffect(
    () => ({
      rects: rects(),
      tree: props.tree,
      width: width(),
      height: height(),
      matched: props.matched,
      selected: props.selected,
      scheme: scheme(),
      mode: mode(),
    }),
    ({
      rects: drawn,
      tree,
      width: w,
      height: h,
      matched,
      selected,
      scheme: currentScheme,
      mode: currentMode,
    }) => {
      const context = prepare(canvas, w, h);
      if (!context) {
        return;
      }

      const dark = currentScheme === "dark";
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
      for (const rect of drawn) {
        const x = rect.x * w;
        const rectWidth = rect.width * w;
        const y = rect.depth * ROW;
        const key = tree.keys[tree.key[rect.node] ?? 0];
        context.globalAlpha = matched && !matched.has(rect.node) ? 0.35 : 1;
        context.fillStyle = tree.baseTotal
          ? changeFill(
              currentMode,
              [tree.total[rect.node] ?? 0, tree.weight],
              [tree.baseTotal[rect.node] ?? 0, tree.baseWeight ?? 0],
              dark,
            )
          : key
            ? fill(key, dark)
            : surface;
        context.fillRect(x, y, Math.max(rectWidth - 1, 0.5), ROW - 1);
        if (selected?.has(rect.node)) {
          context.strokeStyle = accent;
          context.lineWidth = 2;
          context.strokeRect(x + 1, y + 1, Math.max(rectWidth - 3, 0.5), ROW - 3);
        }
        if (rectWidth > 28) {
          context.fillStyle = ink;
          context.fillText(
            clip(context, nameOf(tree, rect.node), rectWidth - 8),
            x + 4,
            y + ROW / 2,
          );
        }
      }
      context.globalAlpha = 1;
    },
  );

  const at = (event: MouseEvent & { currentTarget: HTMLCanvasElement }) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;
    const rect = hit(rects(), Math.floor(y / ROW), x / bounds.width);
    return rect ? { tree: props.tree, rect, left: x, top: y } : null;
  };
  const move = (event: MouseEvent & { currentTarget: HTMLCanvasElement }) => setHovered(at(event));
  const leave = () => setHovered(null);
  const click = (event: MouseEvent & { currentTarget: HTMLCanvasElement }) => {
    const found = at(event);
    if (found) {
      props.onZoom(props.tree, found.rect.node);
    }
  };

  return (
    <div ref={box} class="flame" style={{ height: `${height()}px` }}>
      <canvas
        ref={(node) => {
          canvas = node;
        }}
        style={{ width: "100%", height: `${height()}px` }}
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a canvas drawn here, not an image file
        role="img"
        aria-label="Flame graph"
        onMouseMove={move}
        onMouseLeave={leave}
        onClick={click}
      />
      <Show when={hovered()}>
        {(current) => (
          <div
            class="flame-tip"
            data-testid="flame-tip"
            style={{
              left: `${Math.min(current().left + 12, Math.max(width() - 320, 0))}px`,
              top: `${current().top + 16}px`,
            }}
          >
            <div class="mono">{nameOf(props.tree, current().rect.node)}</div>
            <div class="muted">
              {formatWeight(props.tree.total[current().rect.node] ?? 0, props.unit)} ·{" "}
              {formatPercent(props.tree.total[current().rect.node] ?? 0, props.tree.weight)} total,{" "}
              {formatPercent(props.tree.own[current().rect.node] ?? 0, props.tree.weight)} own
            </div>
            <Show when={props.tree.baseTotal}>
              {(baseTotal) => (
                <div class="muted">
                  base {formatWeight(baseTotal()[current().rect.node] ?? 0, props.unit)} ·{" "}
                  {formatPercent(baseTotal()[current().rect.node] ?? 0, props.tree.baseWeight ?? 0)}{" "}
                  total ·{" "}
                  {
                    changeOf(
                      mode(),
                      [props.tree.total[current().rect.node] ?? 0, props.tree.weight],
                      [baseTotal()[current().rect.node] ?? 0, props.tree.baseWeight ?? 0],
                      props.unit,
                    ).text
                  }
                </div>
              )}
            </Show>
          </div>
        )}
      </Show>
    </div>
  );
}

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
