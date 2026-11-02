// The debugger's views as pane kinds, and the Debug layout that arranges
// them as the page always has: threads and the stack on the left, the code
// and the console in the middle, values on the right.

import { useContext } from "react";
import { KIND, single, sourcePane } from "../documents";
import type { View } from "../focus";
import { tab } from "../tab";
import { kinds, type PaneKind, type PaneProps } from "../workbench/kinds";
import { build, close, type Layout, type Pane, type PaneId } from "../workbench/layout";
import { change, workbench } from "../workbench/store";
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
import { Memory } from "./Memory";
import { Output, OutputBadge } from "./Output";
import { Processes, ProcessesBadge } from "./Processes";
import { fileName } from "./paths";
import { Registers, RegistersBadge } from "./Registers";
import { Modules, Signals } from "./Signals";
import { Stack, StackBadge } from "./Stack";
import { Stops } from "./StatusBar";
import { Tasks, TasksBadge } from "./Tasks";
import { Threads } from "./Threads";
import { Variables, Watch, WatchBadge } from "./Values";
import { FocusContext } from "./Workspace";

/** What a debugger pane needs: a session the tab is looking at. */
export type Need = "session";

type Kind = PaneKind<Need>;

/** A view there is one of, which needs a session. */
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

/** Wraps a body that takes no props. */
const plain =
  (Body: () => React.ReactNode) =>
  (_: PaneProps): React.ReactNode => <Body />;

export const DEBUG_KINDS = kinds<Need>([
  view(KIND.threads, "Threads", plain(Threads), {
    Badge: () => <ThreadsBadge />,
  }),
  view(KIND.tasks, "Tasks", plain(Tasks), { Badge: plain(TasksBadge) }),
  view(KIND.processes, "Processes", plain(Processes), { Badge: plain(ProcessesBadge) }),
  view(KIND.stack, "Call Stack", plain(Stack), { Badge: plain(StackBadge) }),
  view(KIND.breakpoints, "Breakpoints", plain(Breakpoints), { Badge: plain(BreakpointsBadge) }),
  view(KIND.stops, "Stops", plain(Stops)),
  view(KIND.watch, "Watch", plain(Watch), { Badge: plain(WatchBadge) }),
  view(KIND.variables, "Variables", plain(Variables)),
  view(KIND.registers, "Registers", plain(Registers), { Badge: plain(RegistersBadge) }),
  view(KIND.console, "Console", plain(Console)),
  view(KIND.output, "Output", plain(Output), { Badge: plain(OutputBadge) }),
  view(KIND.signals, "Signals", plain(Signals)),
  view(KIND.modules, "Modules", plain(Modules)),
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
  view(KIND.disassembly, "Disassembly", plain(DisassemblyBody), {
    chosen: () => showView("disassembly"),
    close: closesView("disassembly"),
  }),
  view(KIND.memory, "Memory", plain(Memory), {
    chosen: () => showView("memory"),
    close: closesView("memory"),
  }),
  view(KIND.drawings, "Drawings", plain(Drawings), {
    Badge: plain(DrawingsBadge),
    // Live drawings hold workers and canvases.
    keepAlive: false,
    chosen: () => showView("drawings"),
    close: closesView("drawings"),
  }),
]);

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

/** Why a debugger pane has nothing to show, or null when it has. */
export function useMissing(need: string | null): string | null {
  const focus = useContext(FocusContext);
  return need === "session" && !focus ? "Nothing is being debugged in this tab." : null;
}

/** Today's arrangement: nothing moves for someone who knows the page. */
export function debugLayout(): Layout {
  const one = (id: Parameters<typeof single>[0]): Pane => single(id);
  return build({
    axis: "row",
    children: [
      [
        0.19,
        {
          axis: "column",
          children: [
            [0.3, [one(KIND.threads), one(KIND.tasks), one(KIND.processes)]],
            [0.4, [one(KIND.stack), one(KIND.stops)]],
            [0.3, [one(KIND.breakpoints)]],
          ],
        },
      ],
      [
        0.57,
        {
          axis: "column",
          children: [
            [0.72, [sourcePane(""), one(KIND.disassembly), one(KIND.memory), one(KIND.drawings)]],
            [0.28, [one(KIND.output), one(KIND.console), one(KIND.signals), one(KIND.modules)]],
          ],
        },
      ],
      [
        0.24,
        {
          axis: "column",
          children: [
            [0.2, [one(KIND.watch)]],
            [0.45, [one(KIND.variables)]],
            [0.35, [one(KIND.registers)]],
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
