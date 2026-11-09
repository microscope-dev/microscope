// Every view as a pane kind, what each needs before it can show anything,
// and the presets that arrange them: Debug, as the page always has been
// (threads and the stack on the left, the code and the console in the
// middle, values on the right), Start, and Profile.

import { useContext } from "react";

import { DOCUMENT_KINDS, documentsGroup, KIND, single, sourcePane } from "../documents";
import type { View } from "../focus";
import type { ProfileLook } from "../profile/look";
import { useModel } from "../store";
import { tab } from "../tab";
import { remote, standalone } from "../target";
import { focusPane } from "../workbench/arrange";
import { kinds, type PaneKind, type PaneProps } from "../workbench/kinds";
import {
  activate,
  build,
  close,
  type KindId,
  type Layout,
  type LinkId,
  open,
  type PaneId,
  paneOf,
} from "../workbench/layout";
import { lastLayout } from "../workbench/persist";
import { change, useLink, workbench } from "../workbench/store";
import { AccountBody, ConnectBody } from "./Account";
import { Breakpoints, BreakpointsBadge } from "./Breakpoints";
import { Console } from "./Console";
import {
  closeDocument,
  DisassemblyBody,
  DrawingsBadge,
  SourceBody,
  showDocument,
  showView,
} from "./Documents";
import { Drawings } from "./Drawings";
import { ExplorerBody } from "./Explorer";
import { Memory } from "./Memory";
import { OrgBody, orgName, ProjectsBody } from "./Orgs";
import { Output, OutputBadge } from "./Output";
import { fileName } from "./paths";
import { StartBody } from "./Picker";
import { Processes, ProcessesBadge } from "./Processes";
import { overview, paneParams, readLink, showProfile as openOverview } from "./profile/link";
import { ProfilePaneBody } from "./profile/ProfilePage";
import { Registers, RegistersBadge } from "./Registers";
import { KeysBody, SettingsBody } from "./Settings";
import { Modules, Signals } from "./Signals";
import { Stack, StackBadge } from "./Stack";
import { Stops } from "./StatusBar";
import { Tasks, TasksBadge } from "./Tasks";
import { Threads } from "./Threads";
import { Variables, Watch, WatchBadge } from "./Values";
import { FocusContext } from "./Workspace";

/** What a pane needs before it can show anything: a session the tab is
 * looking at, a connected microscope, or one this page may control. */
export type Need = "session" | "connection" | "control";

type Kind = PaneKind<Need>;

/** A view there is one of, which needs a session unless it says otherwise. */
function view(
  id: (typeof KIND)[keyof typeof KIND],
  title: string,
  Body: Kind["Body"],
  more: Partial<Kind> = {},
): Kind {
  return {
    id,
    title: () => title,
    Body,
    needs: "session",
    keepAlive: true,
    closable: () => true,
    ...more,
  };
}

export const KINDS = kinds<Need>([
  view(KIND.threads, "Threads", Threads, {
    Badge: ThreadsBadge,
  }),
  view(KIND.tasks, "Tasks", Tasks, { Badge: TasksBadge }),
  view(KIND.processes, "Processes", Processes, { Badge: ProcessesBadge }),
  view(KIND.stack, "Call Stack", Stack, { Badge: StackBadge }),
  view(KIND.breakpoints, "Breakpoints", Breakpoints, { Badge: BreakpointsBadge }),
  view(KIND.stops, "Stops", Stops),
  view(KIND.watch, "Watch", Watch, { Badge: WatchBadge }),
  view(KIND.variables, "Variables", Variables),
  view(KIND.registers, "Registers", Registers, { Badge: RegistersBadge }),
  view(KIND.console, "Console", Console),
  view(KIND.output, "Output", Output, { Badge: OutputBadge }),
  view(KIND.signals, "Signals", Signals),
  view(KIND.modules, "Modules", Modules),
  {
    id: KIND.source,
    title: (pane) => (pane.key === "" ? "Source" : fileName(pane.key)),
    Body: SourceBody,
    needs: "session",
    // Only the file shown keeps its editor; another's opens again when chosen.
    keepAlive: false,
    closable: (pane) => pane.key !== "",
    chosen: (pane) => (pane.key === "" ? showView("source") : showDocument(pane.key)),
    close: (_, pane) => closeDocument(pane.key),
  },
  view(KIND.disassembly, "Disassembly", DisassemblyBody, {
    chosen: () => showView("disassembly"),
    close: closesView("disassembly"),
  }),
  view(KIND.memory, "Memory", Memory, {
    chosen: () => showView("memory"),
    close: closesView("memory"),
  }),
  view(KIND.drawings, "Drawings", Drawings, {
    Badge: DrawingsBadge,
    // Live drawings hold workers and canvases.
    keepAlive: false,
    chosen: () => showView("drawings"),
    close: closesView("drawings"),
  }),
  view(KIND.explorer, "Explorer", ExplorerBody, { needs: null }),
  view(KIND.connect, "Connect", ConnectBody, { needs: null }),
  view(KIND.account, "Account", AccountBody, { needs: null }),
  view(KIND.projects, "Projects", ProjectsBody, { needs: null }),
  {
    id: KIND.org,
    title: (pane) => orgName(pane) || "Organization",
    Body: OrgBody,
    needs: null,
    keepAlive: true,
    closable: () => true,
  },
  view(KIND.launch, "Launch", StartBody, { needs: "control" }),
  view(KIND.attach, "Attach", StartBody, { needs: "control" }),
  view(KIND.core, "Core dump", StartBody, { needs: "control" }),
  view(KIND.record, "Record", StartBody, { needs: "control" }),
  view(KIND.settings, "Settings", SettingsBody, { needs: null }),
  view(KIND.keys, "Keyboard shortcuts", KeysBody, { needs: null }),
  profileKind(KIND.profile, (pane) => fileName(paneParams(pane).name) || "Profile"),
  profileKind(KIND.profileFlame, () => "Flame graph"),
  profileKind(KIND.profileTree, () => "Call tree"),
  profileKind(KIND.profileFunctions, () => "Functions"),
  profileKind(KIND.profileSyscalls, () => "Syscalls"),
  profileKind(KIND.profileZones, () => "Zones"),
]);

/** One of a profile's panes. Each of its views names its profile's file,
 * so the panes of one profile read as one group wherever they sit. */
function profileKind(id: KindId, title: Kind["title"]): Kind {
  return {
    id,
    title,
    Body: ProfilePaneBody,
    ...(id === KIND.profile ? {} : { Badge: ProfileBadge }),
    needs: "connection",
    // Their canvases and timeline are costly to keep hidden.
    keepAlive: false,
    closable: () => true,
  };
}

function ProfileBadge({ pane }: PaneProps) {
  const link = paneParams(pane).link;
  const name = useLink(link, (value) => readLink(value)?.name ?? "");
  // The space keeps the tab's name "Flame graph go.usprof" for a screen
  // reader; the tab's gap spaces them on screen.
  return name ? <span className="muted"> {fileName(name)}</span> : null;
}

/** What the dock shows panes with. */
export const DOCK = { kinds: KINDS, useMissing };

/** Closes a document's pane; closing the one the address shows goes back
 * to the source. */
function closesView(shown: View): (id: PaneId) => void {
  return (id) => {
    change((layout) => close(layout, id));
    if (tab.getState().look.view === shown) {
      showView("source");
    }
  };
}

function ThreadsBadge() {
  const focus = useContext(FocusContext);
  const count = focus?.state.threads.length ?? 0;
  return count > 0 ? <span className="count">{count}</span> : null;
}

/** Why a pane has nothing to show, or null when it has. */
export function useMissing(need: string | null): string | null {
  const focus = useContext(FocusContext);
  const link = useModel((model) => model.link);
  const role = useModel((model) => model.hello?.role ?? null);
  const debugging = useModel((model) => model.state?.session ?? null);
  const connection =
    link === "open"
      ? null
      : link === "unauthorized"
        ? standalone && !remote()
          ? "Not connected to a microscope. Connect with a join link from the File menu."
          : "microscope refused this page's link. Open a new join link."
        : link === "incompatible"
          ? "This page is from another version of microscope. Reload to update it."
          : link === "unreachable"
            ? "Can't reach microscope. It may have stopped; this page keeps trying."
            : "Connecting to microscope…";
  switch (need) {
    case "session":
      if (focus) {
        return null;
      }
      return (
        connection ??
        (debugging
          ? "Open the session from Explorer to see this."
          : "Nothing is being debugged. Launch, attach, or open a core dump from the File menu.")
      );
    case "connection":
      return connection;
    case "control":
      return connection ?? (role === "control" ? null : "This link can only view the session.");
    default:
      return null;
  }
}

/** Shows the pane of a kind there is one of, opening it where it belongs
 * when it is not open, and moves the keyboard into it. */
export function showPane(kind: KindId): void {
  let shown: PaneId | null = null;
  change((layout) => {
    const found = paneOf(layout, kind, "");
    if (found) {
      shown = found;
      return activate(layout, found);
    }
    // A document, or a page of its own such as Settings, opens where there
    // is room for it; a session's view opens where the person is.
    const roomy = DOCUMENT_KINDS.has(kind) || KINDS.get(kind).needs !== "session";
    const into = roomy ? documentsGroup(layout, tab.getState().files) : layout.focused;
    const opened = open(layout, single(kind), into);
    shown = opened.pane;
    return activate(opened.layout, opened.pane);
  });
  // Once the pane is on screen.
  requestAnimationFrame(() => shown && focusPane(shown));
}

/** Shows a profile's overview, opening it among the documents when it is
 * not open. */
export function showProfile(id: string, name: string): void {
  change(
    (layout) => openOverview(layout, id, name, documentsGroup(layout, tab.getState().files)).layout,
  );
}

/** Shows a shared organization's pane, opening it where there is room
 * when it is not open, and moves the keyboard into it. */
export function showOrg(id: number, name: string): void {
  const before = workbench.getState().layout;
  const found = paneOf(before, KIND.org, String(id));
  const opened = found
    ? { layout: before, pane: found }
    : open(
        before,
        { kind: KIND.org, key: String(id), params: { name } },
        documentsGroup(before, tab.getState().files),
      );
  change(() => activate(opened.layout, opened.pane));
  requestAnimationFrame(() => focusPane(opened.pane));
}

/** Today's arrangement: nothing moves for someone who knows the page. */
export function debugLayout(): Layout {
  return build({
    axis: "row",
    children: [
      [
        0.19,
        {
          axis: "column",
          children: [
            [0.3, [single(KIND.threads), single(KIND.tasks), single(KIND.processes)]],
            [0.4, [single(KIND.stack), single(KIND.stops)]],
            [0.3, [single(KIND.breakpoints)]],
          ],
        },
      ],
      [
        0.57,
        {
          axis: "column",
          children: [
            [
              0.72,
              [
                sourcePane(""),
                single(KIND.disassembly),
                single(KIND.memory),
                single(KIND.drawings),
              ],
            ],
            [
              0.28,
              [
                single(KIND.output),
                single(KIND.console),
                single(KIND.signals),
                single(KIND.modules),
              ],
            ],
          ],
        },
      ],
      [
        0.24,
        {
          axis: "column",
          children: [
            [0.2, [single(KIND.watch)]],
            [0.45, [single(KIND.variables)]],
            [0.35, [single(KIND.registers)]],
          ],
        },
      ],
    ],
  });
}

/** Puts a whole layout in place, the tab's open files in its documents. */
export function applyLayout(layout: Layout): void {
  workbench.setState({ layout });
  tab.setState((state) => ({ files: [...state.files] }));
}

/** Where a page with nothing open starts: at Account, which offers what can
 * be done from here. A page served on its own has Connect beside it; one
 * `microscope web` serves, the forms that choose what to debug. */
export function startLayout(): Layout {
  return standalone
    ? build({
        axis: "row",
        children: [
          [0.6, [single(KIND.account)]],
          [0.4, [single(KIND.connect), single(KIND.explorer)]],
        ],
      })
    : build({
        axis: "row",
        children: [
          [
            0.68,
            [
              single(KIND.account),
              single(KIND.launch),
              single(KIND.attach),
              single(KIND.core),
              single(KIND.record),
            ],
          ],
          [0.32, [single(KIND.explorer)]],
        ],
      });
}

/** What an address wants the workbench to hold: a session's panes, or
 * something to start from. */
export type Wants = "session" | "start";

/** Whether the layout in use is wrong for what an address wants: an empty
 * one is wrong for any; a session wants a pane that shows one, and a page
 * with no session wants none of them, since each would only say there is
 * nothing to show. */
export function misplaced(layout: Layout, wants: Wants): boolean {
  const panes = Object.values(layout.panes);
  const session = panes.some((pane) => KINDS.get(pane.kind).needs === "session");
  return panes.length === 0 || (wants === "session" ? !session : session);
}

/** Puts back the layout this browser used last when the workbench holds
 * nothing, and says why it could not when it could not. */
export function restoreLayout(): string | null {
  if (Object.keys(workbench.getState().layout.panes).length > 0) {
    return null;
  }
  const kept = lastLayout(KINDS);
  if (kept?.ok) {
    workbench.setState({ layout: kept.layout });
  }
  return kept && !kept.ok ? kept.why : null;
}

/** A profile, with Explorer beside it to open another. */
export function profileLayout(id: string, name: string, look: ProfileLook): Layout {
  const link = "l0" as LinkId;
  const { pane, value } = overview(id, name, look, link);
  return build(
    {
      axis: "row",
      children: [
        [0.78, [pane]],
        [0.22, [single(KIND.explorer), single(KIND.account)]],
      ],
    },
    { [link]: value },
  );
}
