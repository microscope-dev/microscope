import { createComponent, createMemo, Show, useContext } from "solid-js";

import { isCaptureProfile } from "../captures";
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
  type Pane,
  type PaneId,
  paneOf,
} from "../workbench/layout";
import { lastLayout } from "../workbench/persist";
import { change, useLayout, useLink, workbench } from "../workbench/store";
import { AccountBody, ConnectBody } from "./Account";
import { Breakpoints, BreakpointsBadge } from "./Breakpoints";
import { Console } from "./Console";
import { BreakdownPane } from "./dashboards/BreakdownPane";
import { ByMachinePane } from "./dashboards/ByMachinePane";
import { FleetFlamePane } from "./dashboards/FleetFlamePane";
import { FleetFunctionsPane } from "./dashboards/FleetFunctionsPane";
import { FleetTreePane } from "./dashboards/FleetTreePane";
import { OverTimePane } from "./dashboards/OverTimePane";
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
import { overview, paneParams, profileOf, showOverview, viewOf } from "./profile/link";
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

export type Need = "session" | "connection" | "control";

type Kind = PaneKind<Need>;

function deferred(get: () => Kind["Body"]): Kind["Body"] {
  return (props) => createComponent(get(), props);
}

function view(
  id: (typeof KIND)[keyof typeof KIND],
  title: string,
  Body: () => Kind["Body"],
  more: Partial<Kind> = {},
): Kind {
  return {
    id,
    title: () => title,
    Body: deferred(Body),
    needs: "session",
    keepAlive: true,
    closable: () => true,
    ...more,
  };
}

export const KINDS = kinds<Need>([
  view(KIND.threads, "Threads", () => Threads, {
    Badge: deferred(() => ThreadsBadge),
  }),
  view(KIND.tasks, "Tasks", () => Tasks, { Badge: deferred(() => TasksBadge) }),
  view(KIND.processes, "Processes", () => Processes, { Badge: deferred(() => ProcessesBadge) }),
  view(KIND.stack, "Call Stack", () => Stack, { Badge: deferred(() => StackBadge) }),
  view(KIND.breakpoints, "Breakpoints", () => Breakpoints, {
    Badge: deferred(() => BreakpointsBadge),
  }),
  view(KIND.stops, "Stops", () => Stops),
  view(KIND.watch, "Watch", () => Watch, { Badge: deferred(() => WatchBadge) }),
  view(KIND.variables, "Variables", () => Variables),
  view(KIND.registers, "Registers", () => Registers, { Badge: deferred(() => RegistersBadge) }),
  view(KIND.console, "Console", () => Console),
  view(KIND.output, "Output", () => Output, { Badge: deferred(() => OutputBadge) }),
  view(KIND.signals, "Signals", () => Signals),
  view(KIND.modules, "Modules", () => Modules),
  {
    id: KIND.source,
    title: (pane) => (pane.key === "" ? "Source" : fileName(pane.key)),
    tooltip: (pane) => pane.key,
    Body: deferred(() => SourceBody),
    needs: "session",

    keepAlive: false,
    closable: (pane) => pane.key !== "",
    chosen: (pane) => (pane.key === "" ? showView("source") : showDocument(pane.key)),
    close: (_, pane) => closeDocument(pane.key),
  },
  view(KIND.disassembly, "Disassembly", () => DisassemblyBody, {
    chosen: () => showView("disassembly"),
    close: closesView("disassembly"),
  }),
  view(KIND.memory, "Memory", () => Memory, {
    chosen: () => showView("memory"),
    close: closesView("memory"),
  }),
  view(KIND.drawings, "Drawings", () => Drawings, {
    Badge: deferred(() => DrawingsBadge),

    keepAlive: false,
    chosen: () => showView("drawings"),
    close: closesView("drawings"),
  }),
  view(KIND.explorer, "Explorer", () => ExplorerBody, { needs: null }),
  view(KIND.connect, "Connect", () => ConnectBody, { needs: null }),
  view(KIND.account, "Account", () => AccountBody, { needs: null }),
  view(KIND.projects, "Projects", () => ProjectsBody, { needs: null }),
  {
    id: KIND.org,
    title: (pane) => orgName(pane) || "Organization",
    Body: deferred(() => OrgBody),
    needs: null,
    keepAlive: true,
    closable: () => true,
  },
  view(KIND.launch, "Launch", () => StartBody, { needs: "control" }),
  view(KIND.attach, "Attach", () => StartBody, { needs: "control" }),
  view(KIND.core, "Core dump", () => StartBody, { needs: "control" }),
  view(KIND.record, "Record", () => StartBody, { needs: "control" }),
  view(KIND.settings, "Settings", () => SettingsBody, { needs: null }),
  view(KIND.keys, "Keyboard shortcuts", () => KeysBody, { needs: null }),
  view(KIND.fleetOverTime, "Over time", () => OverTimePane, { needs: null }),
  view(KIND.fleetFlame, "Flame graph", () => FleetFlamePane, { needs: null }),
  view(KIND.fleetTree, "Call tree", () => FleetTreePane, { needs: null }),
  view(KIND.fleetFunctions, "Functions", () => FleetFunctionsPane, { needs: null }),
  view(KIND.fleetByMachine, "By machine", () => ByMachinePane, { needs: null }),
  view(KIND.fleetBreakdown, "Breakdown", () => BreakdownPane, { needs: null }),
  profileKind(KIND.profile, (pane) => fileName(paneParams(pane).name) || "Profile"),
  profileKind(KIND.profileFlame, () => "Flame graph"),
  profileKind(KIND.profileTree, () => "Call tree"),
  profileKind(KIND.profileFunctions, () => "Functions"),
  profileKind(KIND.profileKernel, () => "Kernel"),
  profileKind(KIND.profileZones, () => "Zones"),
  profileKind(KIND.profileDetails, () => "File details"),
]);

function profileKind(id: KindId, title: Kind["title"]): Kind {
  return {
    id,
    title,
    Body: deferred(() => ProfilePaneBody),
    ...(id === KIND.profile ? {} : { Badge: deferred(() => ProfileBadge) }),
    needs: "connection",

    keepAlive: false,
    closable: () => true,
  };
}

function ProfileBadge(props: PaneProps) {
  const link = createMemo(() => paneParams(props.pane).link);
  const name = useLink(link, (value) => {
    const held = value as { name?: unknown } | null | undefined;
    return typeof held?.name === "string" ? held.name : "";
  });

  return <Show when={name()}>{(shown) => <span class="muted"> {fileName(shown())}</span>}</Show>;
}

export const DOCK = { kinds: KINDS, useMissing };

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
  const count = createMemo(() => focus()?.state.threads.length ?? 0);
  return <Show when={count() > 0}>{(n) => <span class="count">{n()}</span>}</Show>;
}

export function useMissing(need: () => string | null, pane?: () => Pane): () => string | null {
  const focus = useContext(FocusContext);
  const link = useModel((model) => model.link);
  const role = useModel((model) => model.hello?.role ?? null);
  const debugging = useModel((model) => model.state?.session ?? null);
  const hostedCapture = useLayout((layout) => {
    const currentPane = pane?.();
    if (!currentPane || viewOf(currentPane) === undefined) {
      return false;
    }
    return isCaptureProfile(profileOf(layout, currentPane));
  });
  return createMemo(() => {
    const currentLink = link();
    const connection =
      currentLink === "open"
        ? null
        : currentLink === "unauthorized"
          ? standalone && !remote()
            ? "Not connected to a microscope. Connect with a join link from the File menu."
            : "microscope refused this page's link. Open a new join link."
          : currentLink === "incompatible"
            ? "This page is from another version of microscope. Reload to update it."
            : currentLink === "unreachable"
              ? "Can't reach microscope. It may have stopped; this page keeps trying."
              : "Connecting to microscope…";
    switch (need()) {
      case "session":
        if (focus()) {
          return null;
        }
        return (
          connection ??
          (debugging()
            ? "Open the session from Explorer to see this."
            : "Nothing is being debugged. Launch, attach, or open a core dump from the File menu.")
        );
      case "connection":
        return hostedCapture() ? null : connection;
      case "control":
        return connection ?? (role() === "control" ? null : "This link can only view the session.");
      default:
        return null;
    }
  });
}

export function showPane(kind: KindId): void {
  let shown: PaneId | null = null;
  change((layout) => {
    const found = paneOf(layout, kind, "");
    if (found) {
      shown = found;
      return activate(layout, found);
    }

    const roomy = DOCUMENT_KINDS.has(kind) || KINDS.get(kind).needs !== "session";
    const into = roomy ? documentsGroup(layout, tab.getState().files) : layout.focused;
    const opened = open(layout, single(kind), into);
    shown = opened.pane;
    return activate(opened.layout, opened.pane);
  });

  requestAnimationFrame(() => shown && focusPane(shown));
}

export function showProfile(id: string, name: string): void {
  change(
    (layout) => showOverview(layout, id, name, documentsGroup(layout, tab.getState().files)).layout,
  );
}

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

export function applyLayout(layout: Layout): void {
  workbench.setState({ layout });
  tab.setState((state) => ({ files: [...state.files] }));
}

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

export type Wants = "session" | "start" | "fleet" | "capture";

const FLEET_KINDS: ReadonlySet<KindId> = new Set([
  KIND.fleetOverTime,
  KIND.fleetFlame,
  KIND.fleetTree,
  KIND.fleetFunctions,
  KIND.fleetByMachine,
  KIND.fleetBreakdown,
]);

export function fleetOverviewLayout(): Layout {
  return build({
    axis: "column",
    children: [
      [0.34, [single(KIND.fleetOverTime)]],
      [
        0.66,
        {
          axis: "row",
          children: [
            [0.56, [single(KIND.fleetFlame), single(KIND.fleetTree), single(KIND.fleetFunctions)]],
            [0.44, [single(KIND.fleetByMachine), single(KIND.fleetBreakdown)]],
          ],
        },
      ],
    ],
  });
}

export function misplaced(layout: Layout, wants: Wants): boolean {
  const panes = Object.values(layout.panes);
  if (panes.length === 0) {
    return true;
  }
  const session = panes.some((pane) => KINDS.get(pane.kind).needs === "session");
  const hasCapture = panes.some((pane) => isCaptureProfile(profileOf(layout, pane)));
  if (wants === "fleet") {
    return session || hasCapture || !panes.some((pane) => FLEET_KINDS.has(pane.kind));
  }
  if (wants === "capture") {
    return session || !panes.some((pane) => viewOf(pane) !== undefined);
  }
  return wants === "session" ? !session : session || hasCapture;
}

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
