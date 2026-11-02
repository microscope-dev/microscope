// How the documents group follows the address, whatever a person has done to
// the layout: a Source pane for each open file in order, the stand-in when
// none is open, the shown document active, and nothing else disturbed.

import fc from "fast-check";
import { describe, expect, it } from "vitest";
import { arrange, documentsGroup, KIND } from "../src/documents";
import type { View } from "../src/focus";
import {
  checkLayout,
  groupOf,
  groups,
  type Layout,
  type Pane,
  type PaneId,
  paneOf,
} from "../src/workbench/layout";
import { layout } from "./arbitraries/layout";

const view = fc.constantFrom<View>("source", "disassembly", "memory", "drawings");
const files = fc.uniqueArray(fc.constantFrom("a", "b", "c", "/x/main.c", "/x/util.c"), {
  maxLength: 4,
});

const sources = (laid: Layout) =>
  (Object.entries(laid.panes) as [PaneId, Pane][])
    .filter(([, pane]) => pane.kind === KIND.source)
    .map(([, pane]) => pane.key)
    .sort();

describe("arranging the documents", () => {
  it("opens a Source pane for each file, in order, and shows what the address names", () => {
    fc.assert(
      fc.property(layout, files, view, fc.nat(), (before, open, shown, which) => {
        const file = open.length > 0 ? (open[which % open.length] as string) : null;
        const after = arrange(before, open, file, shown);

        expect(checkLayout(after)).toEqual([]);
        expect(sources(after)).toEqual(open.length > 0 ? [...open].sort() : [""]);

        const into = documentsGroup(after, open);
        const group = groups(after).find((candidate) => candidate.id === into);
        // Files lead the documents group, in the order they were opened.
        const tabs = (group?.tabs ?? []).map((tab) => after.panes[tab] as Pane);
        const order = tabs.filter((pane) => pane.kind === KIND.source).map((pane) => pane.key);
        expect(tabs.slice(0, order.length).every((pane) => pane.kind === KIND.source)).toBe(true);
        if (open.length > 0) {
          expect(order).toEqual(open.filter((path) => order.includes(path)));
        }

        const wanted =
          shown === "source"
            ? paneOf(after, KIND.source, open.length > 0 ? (file as string) : "")
            : paneOf(after, KIND[shown], "");
        expect(wanted).not.toBeNull();
        expect(groupOf(after, wanted as PaneId)?.active).toBe(wanted);

        // What the person was working in stays theirs.
        if (groups(after).some((candidate) => candidate.id === before.focused)) {
          expect(after.focused).toBe(before.focused);
        }
        for (const [id, pane] of Object.entries(before.panes) as [PaneId, Pane][]) {
          if (pane.kind !== KIND.source) {
            expect(after.panes[id]).toEqual(pane);
          }
        }
      }),
    );
  });

  it("leaves a layout already in step as it is", () => {
    fc.assert(
      fc.property(layout, files, view, (before, open, shown) => {
        const file = open[0] ?? null;
        const once = arrange(before, open, file, shown);
        expect(arrange(once, open, file, shown)).toBe(once);
      }),
    );
  });
});
