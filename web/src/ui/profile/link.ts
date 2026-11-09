// A profile's panes in the workbench. Its overview, flame graph, call
// tree, functions, syscalls and zones each are a pane of their own, and
// every pane that looks at the profile the same way reads one link: which
// profile, which of its samples, and what is selected. What a pane draws
// on its own, such as the flame graph's zoom, it keeps in its params.

import { KIND } from "../../documents";
import {
  type OwnLook,
  type ProfileLook,
  type ProfileView,
  splitLook,
  validateProfileLook,
} from "../../profile/look";
import type { ProfileSummary } from "../../protocol";
import {
  activate,
  type GroupId,
  groupOf,
  groups,
  type Json,
  type KindId,
  type Layout,
  type LinkId,
  type Node,
  linkOf,
  move,
  open,
  openLinked,
  type Pane,
  type PaneId,
  resize,
  setLink,
  setParams,
  splits,
} from "../../workbench/layout";

/** Each view's pane kind; the overview's is KIND.profile. */
const VIEW_KINDS: Readonly<Record<ProfileView, KindId>> = {
  flame: KIND.profileFlame,
  tree: KIND.profileTree,
  functions: KIND.profileFunctions,
  syscalls: KIND.profileSyscalls,
  zones: KIND.profileZones,
};

const VIEWS: ReadonlySet<ProfileView> = new Set(Object.keys(VIEW_KINDS) as ProfileView[]);

const KIND_VIEWS: ReadonlyMap<KindId, ProfileView | null> = new Map<KindId, ProfileView | null>([
  [KIND.profile, null],
  ...(Object.entries(VIEW_KINDS) as [ProfileView, KindId][]).map(
    ([view, kind]): [KindId, ProfileView] => [kind, view],
  ),
]);

/** The view a profile's pane shows, null for its overview, or undefined
 * for a pane of no profile. */
export function viewOf(pane: Pane): ProfileView | null | undefined {
  return KIND_VIEWS.get(pane.kind);
}

/** The views that list samples' stacks, which open together as tabs. */
const STACKS: ReadonlySet<ProfileView> = new Set(["flame", "tree", "functions", "syscalls"]);

/** What a profile's link holds. */
export interface ProfileLink {
  readonly profile: string;
  /** The profile's file or program, for the tabs. */
  readonly name: string;
  /** What its panes share of how they look at it. */
  readonly look: ProfileLook;
  /** The views whose panes were opened as its summary called for them,
   * each once, so a pane closed stays closed; null until it was first read. */
  readonly offered: readonly ProfileView[] | null;
  /** The view an address asked for before its views were offered, and what
   * that view's pane keeps to itself. */
  readonly show: { readonly view: ProfileView; readonly own: OwnLook } | null;
}

export function readLink(value: Json | undefined): ProfileLink | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return null;
  }
  const held = value as Record<string, unknown>;
  if (typeof held.profile !== "string") {
    return null;
  }
  const look = typeof held.look === "object" && held.look !== null ? held.look : {};
  const asked =
    typeof held.show === "object" && held.show !== null
      ? (held.show as Record<string, unknown>)
      : {};
  const view = validateProfileLook({ view: asked.view }).view;
  const own =
    typeof asked.own === "object" && asked.own !== null
      ? splitLook(validateProfileLook(asked.own as Record<string, unknown>)).own
      : {};
  const show = view ? { view, own } : null;
  const offered = Array.isArray(held.offered)
    ? held.offered.filter((each): each is ProfileView => VIEWS.has(each as ProfileView))
    : null;
  return {
    profile: held.profile,
    name: typeof held.name === "string" ? held.name : "",
    look: splitLook(validateProfileLook(look as Record<string, unknown>)).shared,
    offered,
    show,
  };
}

const linkValue = (link: ProfileLink): Json => link as unknown as Json;

/** A profile's pane's params: the link it reads, what it keeps to itself,
 * and, for the overview, its profile's name, which its tab shows. */
export interface ProfilePaneParams {
  readonly link: LinkId | null;
  readonly own: OwnLook;
  readonly name: string;
}

export function paneParams(pane: Pane): ProfilePaneParams {
  const params = (pane.params ?? {}) as { own?: unknown; name?: unknown };
  const own = typeof params.own === "object" && params.own !== null ? params.own : {};
  return {
    link: linkOf(pane),
    own: splitLook(validateProfileLook(own as Record<string, unknown>)).own,
    name: typeof params.name === "string" ? params.name : "",
  };
}

const toParams = (params: ProfilePaneParams): Json => params as unknown as Json;

/** A profile's pane of a view, reading `link`. Its key tells it apart from
 * the link's other panes of the view: the link's first is keyed by the
 * link alone. */
function profilePane(
  view: ProfileView | null,
  link: LinkId,
  key: string,
  own: OwnLook,
  name = "",
): Pane {
  return {
    kind: view === null ? KIND.profile : VIEW_KINDS[view],
    key,
    params: toParams({ link, own, name }),
  };
}

/** The look a profile's pane shows: its link's, with its own, and its view. */
export function lookOf(layout: Layout, id: PaneId): ProfileLook | null {
  const pane = layout.panes[id];
  const view = pane ? viewOf(pane) : undefined;
  if (!pane || view === undefined) {
    return null;
  }
  const { link, own } = paneParams(pane);
  const held = link === null ? null : readLink(layout.links[link]);
  return held ? { ...held.look, ...own, ...(view === null ? {} : { view }) } : null;
}

/** The profile a pane looks at, if it is one of a profile's. */
export function profileOf(layout: Layout, pane: Pane): string | null {
  const link = viewOf(pane) === undefined ? null : linkOf(pane);
  return link === null ? null : (readLink(layout.links[link])?.profile ?? null);
}

/** The panes reading a link, in the order they were opened. */
function panesOn(layout: Layout, link: LinkId): PaneId[] {
  return (Object.keys(layout.panes) as PaneId[])
    .filter((id) => {
      const pane = layout.panes[id] as Pane;
      return viewOf(pane) !== undefined && linkOf(pane) === link;
    })
    .toSorted((a, b) => Number(a.slice(1)) - Number(b.slice(1)));
}

const paneOfView = (layout: Layout, link: LinkId, view: ProfileView | null): PaneId | null =>
  panesOn(layout, link).find((id) => viewOf(layout.panes[id] as Pane) === view) ?? null;

/** The group of the first pane, of any profile, that a test passes. */
function groupWith(layout: Layout, test: (pane: Pane) => boolean): GroupId | null {
  const found = groups(layout).find((group) =>
    group.tabs.some((tab) => test(layout.panes[tab] as Pane)),
  );
  return found?.id ?? null;
}

/** Opens a pane in `group`, or beside it on `side` when it would be a new
 * group's. */
function openBeside(
  layout: Layout,
  pane: Pane,
  group: GroupId,
  side: "bottom" | "right",
): { layout: Layout; pane: PaneId } {
  const opened = open(layout, pane, group);
  return {
    layout: move(opened.layout, opened.pane, { type: "side", group, side }),
    pane: opened.pane,
  };
}

/** Opens a view's pane on `link` where its kind belongs: among the panes
 * of its sort, of this profile or another; else stacks below the overview,
 * and zones beside Explorer or to the right of the stacks. */
function place(
  layout: Layout,
  link: LinkId,
  view: ProfileView,
  own: OwnLook = {},
): { layout: Layout; pane: PaneId } {
  const pane = profilePane(view, link, link, own);
  const ofThis = (test: (other: Pane) => boolean) => (other: Pane) =>
    linkOf(other) === link && test(other);
  const sorted = (other: Pane) => {
    const shown = viewOf(other);
    return (
      shown !== undefined &&
      shown !== null &&
      (STACKS.has(view) ? STACKS.has(shown) : shown === view)
    );
  };
  const among = groupWith(layout, ofThis(sorted)) ?? groupWith(layout, sorted);
  if (among) {
    return open(layout, pane, among);
  }
  const front = groupWith(
    layout,
    ofThis((other) => viewOf(other) === null),
  );
  if (STACKS.has(view)) {
    return front ? openBeside(layout, pane, front, "bottom") : open(layout, pane);
  }
  const explorer = groupWith(layout, (other) => other.kind === KIND.explorer);
  if (explorer) {
    return open(layout, pane, explorer);
  }
  const stacks = groupWith(
    layout,
    ofThis((other) => STACKS.has(viewOf(other) ?? "zones")),
  );
  const beside = stacks ?? front;
  return beside ? openBeside(layout, pane, beside, "right") : open(layout, pane);
}

/** Opens a link's overview above the first of its panes, or alone. */
function placeOverview(
  layout: Layout,
  link: LinkId,
  name: string,
): { layout: Layout; pane: PaneId } {
  const pane = profilePane(null, link, link, {}, name);
  const first = panesOn(layout, link)[0];
  const group = first ? groupOf(layout, first) : null;
  if (!group) {
    return open(layout, pane);
  }
  const opened = open(layout, pane, group.id);
  return {
    layout: move(opened.layout, opened.pane, { type: "side", group: group.id, side: "top" }),
    pane: opened.pane,
  };
}

/** Shows the pane of a view on `link`, opening it where it belongs when
 * there is none, and focuses it. Before its views are offered, it asks for
 * the view to be shown once they are, and shows the overview. */
export function showView(
  layout: Layout,
  link: LinkId,
  view: ProfileView | null,
): { layout: Layout; pane: PaneId | null } {
  const held = readLink(layout.links[link]);
  if (!held) {
    return { layout, pane: null };
  }
  const found = paneOfView(layout, link, view);
  if (found) {
    return { layout: activate(layout, found), pane: found };
  }
  if (view === null || held.offered === null) {
    const show = view === null ? null : { view, own: {} };
    const asked = setLink(layout, link, linkValue({ ...held, show }));
    const front = paneOfView(asked, link, null);
    if (front) {
      return { layout: activate(asked, front), pane: front };
    }
    if (view !== null) {
      return { layout: asked, pane: null };
    }
  }
  const placed = view === null ? placeOverview(layout, link, held.name) : place(layout, link, view);
  return { layout: activate(placed.layout, placed.pane), pane: placed.pane };
}

/** A profile's overview, reading `link`, and what the link starts with:
 * how `look` looks at the profile, and its view to show once arranged. */
export function overview(
  profile: string,
  name: string,
  look: ProfileLook,
  link: LinkId,
): { pane: Pane; value: Json } {
  const { shared, own } = splitLook(look);
  const show = look.view ? { view: look.view, own } : null;
  return {
    pane: profilePane(null, link, link, look.view ? {} : own, name),
    value: linkValue({ profile, name, look: shared, offered: null, show }),
  };
}

/** Shows a profile's overview, reopening it on its link when it was
 * closed, or opening it in `group` reading a new link when no pane shows
 * the profile. */
export function showOverview(
  layout: Layout,
  profile: string,
  name: string,
  group: GroupId,
): { layout: Layout; pane: PaneId } {
  const link = linkTo(layout, profile, {});
  const shown = link ? showView(layout, link, null) : null;
  return shown?.pane
    ? { layout: shown.layout, pane: shown.pane }
    : openProfile(layout, profile, name, {}, group);
}

/** Opens a profile's overview, reading a new link that looks at it as
 * `look` does, and its pane of the look's view once its summary arranges
 * its panes. */
export function openProfile(
  layout: Layout,
  profile: string,
  name: string,
  look: ProfileLook,
  group: GroupId,
): { layout: Layout; pane: PaneId } {
  const value = overview(profile, name, look, "l0" as LinkId).value;
  const opened = openLinked(
    layout,
    (link) => overview(profile, name, look, link).pane,
    value,
    group,
  );
  return { layout: activate(opened.layout, opened.pane), pane: opened.pane };
}

/** The views a profile's summary calls for, in their tabs' order: its
 * stacks when it has samples, or nothing else and is done recording, its
 * calls when traced, and its zones when it has any. */
function viewsFor(summary: ProfileSummary, live: boolean): ProfileView[] {
  const sampled = summary.samples > 0 || (summary.syscalls === 0 && !live);
  return [
    ...(sampled ? (["flame", "tree", "functions"] as const) : []),
    ...(summary.syscalls > 0 ? (["syscalls"] as const) : []),
    ...(summary.timed && summary.zoneSites.length > 0 ? (["zones"] as const) : []),
  ];
}

/** Whether a profile's overview draws its timeline: not when it keeps no
 * times, nor for a trace alone, whose Syscalls pane draws its lanes. */
export function drawsTimeline(summary: ProfileSummary): boolean {
  const traceOnly = summary.samples === 0 && summary.syscalls > 0 && summary.zoneSites.length === 0;
  return summary.timed && !traceOnly;
}

/** An overview with no timeline is short, so its views below it get the
 * room, when the two share a split with nothing else's. */
function shrinkOverview(layout: Layout, link: LinkId): Layout {
  const front = paneOfView(layout, link, null);
  const group = front ? groupOf(layout, front) : null;
  const ours = (node: Node) =>
    node.type === "group" && node.tabs.every((tab) => linkOf(layout.panes[tab] as Pane) === link);
  const parent = splits(layout.root).find(
    (each) =>
      each.axis === "column" &&
      each.children.length === 2 &&
      each.children.some((child) => child.node.id === group?.id) &&
      each.children.every((child) => ours(child.node)),
  );
  if (!group || !parent) {
    return layout;
  }
  return resize(
    layout,
    parent.id,
    parent.children.map((child) => (child.node.id === group.id ? 0.2 : 0.8)),
  );
}

/** Opens the panes a profile's summary calls for that it has not called
 * for before, as a recording grows. The first that offers any shows each
 * group's first tab, and the view the address asked for, else keeps the
 * focus where it was; later ones leave what each group shows alone. */
export function arrange(
  layout: Layout,
  link: LinkId,
  summary: ProfileSummary,
  live = false,
): Layout {
  const held = readLink(layout.links[link]);
  if (!held) {
    return layout;
  }
  const before = held.offered ?? [];
  const fresh = viewsFor(summary, live).filter((view) => !before.includes(view));
  if (held.offered !== null && fresh.length === 0) {
    return layout;
  }
  const first = before.length === 0 && fresh.length > 0;
  const back = groups(layout).find((group) => group.id === layout.focused)?.active ?? null;
  const shownBefore = new Map(groups(layout).map((group) => [group.id, group.active]));

  let next = layout;
  const firsts = new Map<GroupId, PaneId>();
  for (const view of fresh) {
    if (paneOfView(next, link, view)) {
      continue;
    }
    const placed = place(next, link, view, held.show?.view === view ? held.show.own : {});
    next = placed.layout;
    const group = groupOf(next, placed.pane)?.id;
    if (group && !firsts.has(group)) {
      firsts.set(group, placed.pane);
    }
  }
  for (const [group, pane] of firsts) {
    const kept = shownBefore.get(group);
    next = activate(next, !first && kept && next.panes[kept] ? kept : pane);
  }
  if (firsts.size > 0 && !drawsTimeline(summary)) {
    next = shrinkOverview(next, link);
  }

  // What the address asked for is shown when views are first offered, or
  // let go of if there is none of it.
  const asked = first && held.show ? paneOfView(next, link, held.show.view) : null;
  const focus = asked ?? (back && next.panes[back] ? back : null);
  next = focus ? activate(next, focus) : next;
  return setLink(
    next,
    link,
    linkValue({ ...held, offered: [...before, ...fresh], show: first ? null : held.show }),
  );
}

/** Keeps the look a profile's pane shows: what its panes share in its
 * link, what it keeps to itself in its params. A look of another view
 * shows that view's pane, which is returned; that pane keeps how it draws
 * unless `carry`, as when an address says how. */
export function storeLook(
  layout: Layout,
  id: PaneId,
  look: ProfileLook,
  carry = false,
): { layout: Layout; pane: PaneId } {
  const pane = layout.panes[id];
  const view = pane ? viewOf(pane) : undefined;
  const params = pane ? paneParams(pane) : null;
  const held = params?.link ? readLink(layout.links[params.link]) : null;
  if (!pane || view === undefined || !params?.link || !held) {
    return { layout, pane: id };
  }
  const { shared, own } = splitLook(look);
  const linked = setLink(layout, params.link, linkValue({ ...held, look: shared }));
  const wanted = look.view ?? null;
  if (wanted === view) {
    return { layout: setParams(linked, id, toParams({ ...params, own })), pane: id };
  }
  const shown = showView(linked, params.link, wanted);
  const target = shown.pane;
  const into = target ? shown.layout.panes[target] : undefined;
  if (!target || !into || viewOf(into) !== wanted) {
    // Its view opens once the views are offered, keeping what it keeps.
    const pending = readLink(shown.layout.links[params.link]);
    const asked =
      pending && wanted !== null && carry
        ? setLink(shown.layout, params.link, linkValue({ ...pending, show: { view: wanted, own } }))
        : shown.layout;
    return { layout: asked, pane: target ?? id };
  }
  return {
    layout: carry
      ? setParams(shown.layout, target, toParams({ ...paneParams(into), own }))
      : shown.layout,
    pane: target,
  };
}

/** The focused group's shown pane, if it is a profile's. */
export function leadingPane(layout: Layout): PaneId | null {
  const active = groups(layout).find((group) => group.id === layout.focused)?.active;
  const pane = active ? layout.panes[active] : undefined;
  return active && pane && viewOf(pane) !== undefined ? active : null;
}

/** A profile's pane on screen: the focused group's, else the first other
 * group's that shows one. */
export function shownProfilePane(layout: Layout): PaneId | null {
  const shown = groups(layout)
    .toSorted((a, b) => Number(b.id === layout.focused) - Number(a.id === layout.focused))
    .flatMap((group) => (group.active ? [group.active] : []));
  return shown.find((id) => viewOf(layout.panes[id] as Pane) !== undefined) ?? null;
}

/** The link a look at a profile is shown through: the leading pane's if
 * it shows that profile, else the first link to it already looking as
 * `look` does, as after a reload, else the first link to it. */
function linkTo(layout: Layout, profile: string, look: ProfileLook): LinkId | null {
  const lead = leadingPane(layout);
  const leadLink = lead ? linkOf(layout.panes[lead] as Pane) : null;
  if (leadLink && readLink(layout.links[leadLink])?.profile === profile) {
    return leadLink;
  }
  const shared = JSON.stringify(splitLook(validateProfileLook({ ...look })).shared);
  const links = (Object.keys(layout.links) as LinkId[])
    .filter((link) => readLink(layout.links[link])?.profile === profile)
    .toSorted((a, b) => Number(a.slice(1)) - Number(b.slice(1)));
  const same = links.find((link) => JSON.stringify(readLink(layout.links[link])?.look) === shared);
  return same ?? links[0] ?? null;
}

/** Shows what an address names: the profile's pane of its view, looking
 * as it says, opening the profile among the documents when no pane shows
 * it. */
export function showAddress(
  layout: Layout,
  profile: string,
  name: string,
  look: ProfileLook,
  documents: GroupId,
): Layout {
  const link = linkTo(layout, profile, look);
  if (!link) {
    return openProfile(layout, profile, name, look, documents).layout;
  }
  const named = name ? nameProfile(layout, profile, name) : layout;
  const lead = leadingPane(named);
  const from =
    lead && linkOf(named.panes[lead] as Pane) === link ? lead : paneOfView(named, link, null);
  if (from) {
    return storeLook(named, from, look, true).layout;
  }
  const shown = showView(named, link, look.view ?? null);
  return shown.pane ? storeLook(shown.layout, shown.pane, look, true).layout : shown.layout;
}

/** Names a profile in each link to it, as its overviews' tabs and its
 * views' badges show it. */
function nameProfile(layout: Layout, profile: string, name: string): Layout {
  let named = layout;
  for (const link of Object.keys(layout.links) as LinkId[]) {
    const held = readLink(layout.links[link]);
    if (held?.profile !== profile || held.name === name) {
      continue;
    }
    named = setLink(named, link, linkValue({ ...held, name }));
    for (const id of panesOn(named, link)) {
      const pane = named.panes[id] as Pane;
      if (viewOf(pane) === null) {
        named = setParams(named, id, toParams({ ...paneParams(pane), name }));
      }
    }
  }
  return named;
}

/** Whether a pane of the profile, of the view an address names, is open. */
export function viewOpen(layout: Layout, profile: string, view: ProfileView | null): boolean {
  return Object.values(layout.panes).some(
    (pane) => viewOf(pane) === view && profileOf(layout, pane) === profile,
  );
}

/** Opens another of a profile's pane beside it: reading its link, or a
 * link of its own that starts as a copy, which then changes apart. */
export function duplicate(
  layout: Layout,
  id: PaneId,
  apart: boolean,
): { layout: Layout; pane: PaneId } | null {
  const pane = layout.panes[id];
  const view = pane ? viewOf(pane) : undefined;
  const group = groupOf(layout, id);
  const params = pane ? paneParams(pane) : null;
  if (!pane || view === undefined || !group || !params?.link) {
    return null;
  }
  const held = layout.links[params.link];
  if (apart) {
    const opened = openLinked(
      layout,
      (link) => profilePane(view, link, link, params.own, params.name),
      held ?? null,
      group.id,
    );
    return { layout: activate(opened.layout, opened.pane), pane: opened.pane };
  }
  const taken = new Set(
    Object.values(layout.panes)
      .filter((other) => other.kind === pane.kind)
      .map((other) => other.key),
  );
  let copy = 2;
  while (taken.has(`${params.link}+${copy}`)) {
    copy++;
  }
  const opened = open(
    layout,
    profilePane(view, params.link, `${params.link}+${copy}`, params.own, params.name),
    group.id,
  );
  return { layout: activate(opened.layout, opened.pane), pane: opened.pane };
}
