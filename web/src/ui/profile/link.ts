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
  linkOf,
  move,
  open,
  openLinked,
  type Pane,
  type PaneId,
  setLink,
  setParams,
} from "../../workbench/layout";

const VIEW_KINDS: Readonly<Record<ProfileView, KindId>> = {
  flame: KIND.profileFlame,
  tree: KIND.profileTree,
  functions: KIND.profileFunctions,
  kernel: KIND.profileKernel,
  zones: KIND.profileZones,
  details: KIND.profileDetails,
};

const VIEWS: ReadonlySet<ProfileView> = new Set(Object.keys(VIEW_KINDS) as ProfileView[]);

const KIND_VIEWS: ReadonlyMap<KindId, ProfileView | null> = new Map<KindId, ProfileView | null>([
  [KIND.profile, null],
  ...(Object.entries(VIEW_KINDS) as [ProfileView, KindId][]).map(
    ([view, kind]): [KindId, ProfileView] => [kind, view],
  ),
]);

export function viewOf(pane: Pane): ProfileView | null | undefined {
  return KIND_VIEWS.get(pane.kind);
}

const STACKS: ReadonlySet<ProfileView> = new Set([
  "flame",
  "tree",
  "functions",
  "kernel",
  "details",
]);

/**
 * Shared state for a group of linked profile panes in `layout.links`.
 * The link holds the shared query scope (`look`), while each pane keeps its own
 * view-local display options (`OwnLook`) in `pane.params`.
 */
export interface ProfileLink {
  readonly profile: string;

  readonly name: string;

  readonly look: ProfileLook;

  readonly offered: readonly ProfileView[] | null;

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

export function profileOf(layout: Layout, pane: Pane): string | null {
  const link = viewOf(pane) === undefined ? null : linkOf(pane);
  return link === null ? null : (readLink(layout.links[link])?.profile ?? null);
}

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

function groupWith(layout: Layout, test: (pane: Pane) => boolean): GroupId | null {
  const found = groups(layout).find((group) =>
    group.tabs.some((tab) => test(layout.panes[tab] as Pane)),
  );
  return found?.id ?? null;
}

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

/**
 * Default pane placement: stack views (`flame`, `tree`, `functions`, `kernel`,
 * `details`) tab together below the overview; `zones` tabs with the explorer
 * or opens to the right of the stack views.
 */
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

function viewsFor(summary: ProfileSummary, live: boolean): ProfileView[] {
  const sampled = summary.samples > 0 || !live;
  return [
    ...(sampled ? (["flame", "tree", "functions"] as const) : []),
    ...(sampled || summary.syscalls > 0 ? (["kernel"] as const) : []),
    ...(summary.zoneSites.length > 0 ? (["zones"] as const) : []),
  ];
}

/**
 * Lazily opens profile views as data becomes available (for example, `zones` or
 * `kernel` appear mid-recording once the first events arrive). Tracks `offered`
 * on the link so closed panes are not re-opened, and preserves the user's active
 * tab on incremental updates after the initial layout.
 */
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

  const asked = first && held.show?.view === "details" ? (["details"] as const) : [];
  for (const view of [...fresh, ...asked]) {
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

  const wanted = first && held.show ? paneOfView(next, link, held.show.view) : null;
  const focus = wanted ?? (back && next.panes[back] ? back : null);
  next = focus ? activate(next, focus) : next;
  return setLink(
    next,
    link,
    linkValue({ ...held, offered: [...before, ...fresh], show: first ? null : held.show }),
  );
}

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

export function leadingPane(layout: Layout): PaneId | null {
  const active = groups(layout).find((group) => group.id === layout.focused)?.active;
  const pane = active ? layout.panes[active] : undefined;
  return active && pane && viewOf(pane) !== undefined ? active : null;
}

export function shownProfilePane(layout: Layout): PaneId | null {
  const shown = groups(layout)
    .toSorted((a, b) => Number(b.id === layout.focused) - Number(a.id === layout.focused))
    .flatMap((group) => (group.active ? [group.active] : []));
  return shown.find((id) => viewOf(layout.panes[id] as Pane) !== undefined) ?? null;
}

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

export function viewOpen(layout: Layout, profile: string, view: ProfileView | null): boolean {
  return Object.values(layout.panes).some(
    (pane) => viewOf(pane) === view && profileOf(layout, pane) === profile,
  );
}

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
