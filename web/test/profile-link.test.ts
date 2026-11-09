// A profile's panes in the workbench: which open, where, and what each
// shares with the others through their link.

import { describe, expect, it } from "vitest";

import { KIND } from "../src/documents";
import type { ProfileView } from "../src/profile/look";
import type { ProfileSummary } from "../src/protocol";
import {
  arrange,
  duplicate,
  lookOf,
  openProfile,
  readLink,
  showAddress,
  showOverview,
  showView,
  storeLook,
  viewOf,
  viewOpen,
} from "../src/ui/profile/link";
import {
  activate,
  checkLayout,
  close,
  emptyLayout,
  type GroupId,
  groupOf,
  groups,
  type Layout,
  type LinkId,
  linkOf,
  open,
  type Pane,
  type PaneId,
  splits,
} from "../src/workbench/layout";

const summary = (change: Partial<ProfileSummary> = {}): ProfileSummary =>
  ({ samples: 10, syscalls: 0, timed: true, zoneSites: [{}], ...change }) as ProfileSummary;

const G0 = "g0" as GroupId;

/** The pane of a view, by view, failing when there is not exactly one. */
function only(layout: Layout, view: ProfileView | null): PaneId {
  const found = (Object.keys(layout.panes) as PaneId[]).filter(
    (id) => viewOf(layout.panes[id] as Pane) === view,
  );
  expect(found, `one pane of ${String(view)}`).toHaveLength(1);
  return found[0] as PaneId;
}

const kindsIn = (layout: Layout, pane: PaneId) =>
  groupOf(layout, pane)?.tabs.map((tab) => viewOf(layout.panes[tab] as Pane));

const weightOf = (layout: Layout) =>
  splits(layout.root)
    .find((each) => each.axis === "column")
    ?.children.map((child) => child.weight);

const focusedPane = (layout: Layout) =>
  groups(layout).find((group) => group.id === layout.focused)?.active;

/** A profile opened and arranged as its summary says. */
function arranged(look = {}, of = summary()): { layout: Layout; link: LinkId } {
  const opened = openProfile(emptyLayout(), "p1", "x.usprof", look, G0);
  const link = linkOf(opened.layout.panes[opened.pane] as Pane) as LinkId;
  const layout = arrange(opened.layout, link, of, false);
  expect(checkLayout(layout)).toEqual([]);
  return { layout, link };
}

describe("a profile's panes", () => {
  it("open once as its summary calls for: stacks as tabs below, zones beside", () => {
    const { layout, link } = arranged();
    const overview = only(layout, null);
    const flame = only(layout, "flame");
    expect(kindsIn(layout, overview)).toEqual([null]);
    expect(kindsIn(layout, flame)).toEqual(["flame", "tree", "functions"]);
    expect(groupOf(layout, flame)?.active).toBe(flame);
    expect(kindsIn(layout, only(layout, "zones"))).toEqual(["zones"]);
    // The overview keeps the focus it had.
    expect(focusedPane(layout)).toBe(overview);

    // A pane closed stays closed.
    const closed = close(layout, only(layout, "tree"));
    const again = arrange(closed, link, summary());
    expect(again).toBe(closed);
    expect(viewOpen(again, "p1", "tree")).toBe(false);
  });

  it("give a trace's calls the room its overview, with no timeline, does not need", () => {
    expect(weightOf(arranged().layout)).toEqual([0.5, 0.5]);
    const trace = arranged({}, summary({ samples: 0, syscalls: 5, zoneSites: [] })).layout;
    expect(weightOf(trace)).toEqual([0.2, 0.8]);
  });

  it("open as a recording grows into them, each once, leaving what is shown alone", () => {
    const opened = openProfile(emptyLayout(), "p1", "live", { view: "zones" }, G0);
    const link = linkOf(opened.layout.panes[opened.pane] as Pane) as LinkId;

    // Nothing recorded yet calls for nothing, and the asked-for view waits.
    const empty = summary({ samples: 0, syscalls: 0, zoneSites: [] });
    const waiting = arrange(opened.layout, link, empty, true);
    expect(Object.keys(waiting.panes)).toEqual([opened.pane]);
    expect(readLink(waiting.links[link])?.show?.view).toBe("zones");

    // Samples open the stacks. The zones it asked for are not there, so it
    // lets go of them and keeps the focus.
    const sampled = arrange(waiting, link, summary({ zoneSites: [] }), true);
    expect(viewOpen(sampled, "p1", "flame")).toBe(true);
    expect(readLink(sampled.links[link])?.show).toBeNull();
    expect(focusedPane(sampled)).toBe(opened.pane);
    expect(arrange(sampled, link, summary({ zoneSites: [] }), true)).toBe(sampled);

    // The tree shown stays shown as zones open beside it.
    const tree = only(sampled, "tree");
    const looking = activate(sampled, tree);
    const zoned = arrange(looking, link, summary(), true);
    expect(viewOpen(zoned, "p1", "zones")).toBe(true);
    expect(groupOf(zoned, tree)?.active).toBe(tree);
    expect(focusedPane(zoned)).toBe(tree);
    expect(checkLayout(zoned)).toEqual([]);

    // Closed, a view stays closed as the recording goes on and ends.
    const closed = close(zoned, only(zoned, "zones"));
    expect(arrange(closed, link, summary(), false)).toBe(closed);
  });

  it("open a live trace's calls, not stacks it has none of", () => {
    const opened = openProfile(emptyLayout(), "p1", "live", {}, G0);
    const link = linkOf(opened.layout.panes[opened.pane] as Pane) as LinkId;
    const empty = summary({ samples: 0, syscalls: 0, zoneSites: [] });
    const started = arrange(opened.layout, link, empty, true);
    const traced = arrange(
      started,
      link,
      summary({ samples: 0, syscalls: 3, zoneSites: [] }),
      true,
    );
    expect(Object.values(traced.panes).map((pane) => viewOf(pane))).toEqual([null, "syscalls"]);
    // Done, and still without samples, it calls for no stacks.
    const done = summary({ samples: 0, syscalls: 9, zoneSites: [] });
    expect(arrange(traced, link, done, false)).toBe(traced);
  });

  it("open only what a profile has", () => {
    const traced = arranged({}, summary({ samples: 0, syscalls: 5, zoneSites: [] })).layout;
    expect(Object.values(traced.panes).map((pane) => viewOf(pane))).toEqual([null, "syscalls"]);
    const untimed = arranged({}, summary({ timed: false })).layout;
    expect(viewOpen(untimed, "p1", "zones")).toBe(false);
  });

  it("show the view an address asked for, with what it keeps, once arranged", () => {
    const { layout } = arranged({ view: "tree", invert: "1", fn: "leaf" });
    const tree = only(layout, "tree");
    expect(focusedPane(layout)).toBe(tree);
    expect(lookOf(layout, tree)).toMatchObject({ view: "tree", invert: "1", fn: "leaf" });
    // The inversion was the tree's; the function chosen is everyone's.
    expect(lookOf(layout, only(layout, "flame"))?.invert).toBeUndefined();
    expect(lookOf(layout, only(layout, "flame"))?.fn).toBe("leaf");
  });

  it("share a selection, and keep how each draws it to itself", () => {
    const { layout } = arranged();
    const flame = only(layout, "flame");
    const tree = only(layout, "tree");
    const look = lookOf(layout, flame) ?? {};
    const stored = storeLook(layout, flame, { ...look, fn: "work", z: ["2"], range: "1-5" });
    expect(stored.pane).toBe(flame);
    expect(lookOf(stored.layout, tree)).toEqual({ view: "tree", fn: "work", range: "1-5" });
    expect(lookOf(stored.layout, only(stored.layout, "zones"))?.z).toBeUndefined();
    expect(lookOf(stored.layout, flame)?.z).toEqual(["2"]);
  });

  it("show another view's pane as it draws, with the selection shared", () => {
    const { layout } = arranged();
    const flame = only(layout, "flame");
    const functions = only(layout, "functions");
    const byAddress = storeLook(layout, functions, { view: "functions", by: "address" }).layout;
    const inverted = storeLook(byAddress, flame, { view: "flame", invert: "1" }).layout;
    const stored = storeLook(inverted, flame, { view: "functions", invert: "1", fn: "work" });
    expect(stored.pane).toBe(functions);
    expect(groupOf(stored.layout, functions)?.active).toBe(functions);
    expect(lookOf(stored.layout, functions)).toEqual({
      view: "functions",
      by: "address",
      fn: "work",
    });
    expect(lookOf(stored.layout, flame)?.invert).toBe("1");
  });

  it("reopen a closed overview, and show it from Explorer on the same link", () => {
    const { layout, link } = arranged();
    const flame = only(layout, "flame");
    const withFn = storeLook(layout, flame, { view: "flame", fn: "work" }).layout;
    const closed = close(withFn, only(withFn, null));

    const shown = showView(closed, link, null);
    expect(shown.pane).toBe(only(shown.layout, null));
    expect(focusedPane(shown.layout)).toBe(shown.pane);
    expect(lookOf(shown.layout, shown.pane as PaneId)?.fn).toBe("work");
    expect(checkLayout(shown.layout)).toEqual([]);

    const explored = showOverview(closed, "p1", "x.usprof", G0);
    expect(Object.keys(explored.layout.links)).toEqual([link]);
    expect(linkOf(explored.layout.panes[explored.pane] as Pane)).toBe(link);
    expect(viewOf(explored.layout.panes[explored.pane] as Pane)).toBeNull();
    // Arranging it again opens nothing new.
    const again = arrange(explored.layout, link, summary());
    expect(Object.keys(again.panes)).toHaveLength(Object.keys(explored.layout.panes).length);
  });

  it("reopen a closed view where its sort is", () => {
    const { layout, link } = arranged();
    const closed = close(layout, only(layout, "functions"));
    const shown = showView(closed, link, "functions");
    expect(shown.pane).not.toBeNull();
    expect(groupOf(shown.layout, shown.pane as PaneId)?.tabs).toContain(only(closed, "flame"));
    expect(checkLayout(shown.layout)).toEqual([]);
  });

  it("open File details only when asked, among the stacks, from an address too", () => {
    const { layout, link } = arranged();
    expect(viewOpen(layout, "p1", "details")).toBe(false);
    const shown = showView(layout, link, "details");
    expect(kindsIn(shown.layout, shown.pane as PaneId)?.toSorted()).toEqual([
      "details",
      "flame",
      "functions",
      "tree",
    ]);
    expect(focusedPane(shown.layout)).toBe(shown.pane);

    // An address asks for it before the summary is read.
    const asked = arranged({ view: "details" }).layout;
    expect(focusedPane(asked)).toBe(only(asked, "details"));
    expect(checkLayout(asked)).toEqual([]);
  });

  it("show the overview, not a view, before they are arranged", () => {
    const opened = openProfile(emptyLayout(), "p1", "x.usprof", {}, G0);
    const link = linkOf(opened.layout.panes[opened.pane] as Pane) as LinkId;
    const shown = showView(opened.layout, link, "zones");
    expect(shown.pane).toBe(opened.pane);
    expect(readLink(shown.layout.links[link])?.show?.view).toBe("zones");
    const later = arrange(shown.layout, link, summary());
    expect(focusedPane(later)).toBe(only(later, "zones"));
  });
});

describe("another pane like one", () => {
  it("shares its selection when linked, and starts from a copy when apart", () => {
    const { layout } = arranged();
    const flame = only(layout, "flame");
    const withFn = storeLook(layout, flame, { view: "flame", fn: "work", invert: "1" }).layout;

    const twin = duplicate(withFn, flame, false);
    const fork = duplicate(withFn, flame, true);
    if (!twin || !fork) {
      throw new Error("a profile pane duplicates");
    }
    expect(lookOf(twin.layout, twin.pane)).toEqual({ view: "flame", fn: "work", invert: "1" });
    expect(lookOf(fork.layout, fork.pane)).toEqual({ view: "flame", fn: "work", invert: "1" });

    const twinChanged = storeLook(twin.layout, flame, { view: "flame", fn: "leaf" }).layout;
    expect(lookOf(twinChanged, twin.pane)?.fn).toBe("leaf");
    const forkChanged = storeLook(fork.layout, flame, { view: "flame", fn: "leaf" }).layout;
    expect(lookOf(forkChanged, fork.pane)?.fn).toBe("work");

    // Two twins do not collide, and the fork outlives what it was copied from.
    const third = duplicate(twin.layout, flame, false);
    expect(third && Object.keys(third.layout.panes)).toHaveLength(7);
    let alone = fork.layout;
    for (const id of Object.keys(alone.panes) as PaneId[]) {
      if (id !== fork.pane) {
        alone = close(alone, id);
      }
    }
    expect(lookOf(alone, fork.pane)?.fn).toBe("work");
    expect(checkLayout(alone)).toEqual([]);
  });

  it("is nothing for a pane of no profile", () => {
    const { layout, pane } = open(emptyLayout(), { kind: KIND.explorer, key: "", params: null });
    expect(duplicate(layout, pane, false)).toBeNull();
    expect(storeLook(layout, pane, { fn: "x" }).layout).toBe(layout);
  });
});

describe("an address", () => {
  it("opens a profile no pane shows", () => {
    const layout = showAddress(emptyLayout(), "p1", "x.usprof", { fn: "f" }, G0);
    expect(lookOf(layout, only(layout, null))).toEqual({ fn: "f" });
  });

  it("changes the link of the focused pane when two show the profile", () => {
    const { layout } = arranged();
    const flame = only(layout, "flame");
    const fork = duplicate(layout, flame, true);
    if (!fork) {
      throw new Error("a profile pane duplicates");
    }
    const changed = showAddress(fork.layout, "p1", "x.usprof", { view: "flame", fn: "f" }, G0);
    expect(lookOf(changed, fork.pane)?.fn).toBe("f");
    expect(lookOf(changed, flame)?.fn).toBeUndefined();
  });

  it("changes the link whose look it is when no profile pane is focused", () => {
    const { layout } = arranged();
    const flame = only(layout, "flame");
    const fork = duplicate(layout, flame, true);
    if (!fork) {
      throw new Error("a profile pane duplicates");
    }
    const look = { view: "flame", fn: "f", range: "1-5" } as const;
    const forked = storeLook(fork.layout, fork.pane, look).layout;
    const explorer = open(forked, { kind: KIND.explorer, key: "", params: null }).layout;

    const reloaded = showAddress(explorer, "p1", "x.usprof", look, G0);
    expect(lookOf(reloaded, fork.pane)).toEqual(look);
    expect(lookOf(reloaded, flame)).toEqual({ view: "flame" });
  });

  it("names the profile's overview once its name is known", () => {
    const opened = openProfile(emptyLayout(), "p1", "", {}, G0);
    const named = showAddress(opened.layout, "p1", "x.usprof", {}, G0);
    expect(named.panes[opened.pane]?.params).toMatchObject({ name: "x.usprof" });
    const link = linkOf(named.panes[opened.pane] as Pane) as LinkId;
    expect(readLink(named.links[link])?.name).toBe("x.usprof");
    // An address that does not know it yet keeps it.
    const kept = showAddress(named, "p1", "", { fn: "f" }, G0);
    expect(kept.panes[opened.pane]?.params).toMatchObject({ name: "x.usprof" });
  });

  it("reads a link that is not a profile's as none", () => {
    for (const value of [null, 3, "p1", [], {}, { profile: 3 }]) {
      expect(readLink(value)).toBeNull();
    }
    expect(
      readLink({ profile: "p1", look: { range: "9-3", fn: "f" }, show: { view: "nope" } }),
    ).toEqual({ profile: "p1", name: "", look: { fn: "f" }, offered: null, show: null });
  });
});
