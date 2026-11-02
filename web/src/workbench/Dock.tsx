// The dock: the layout's tree of splits and groups, filling the space it is
// given, or the one maximized group until it is restored.

import { DockContext, type DockSetup } from "./context";
import { GroupView } from "./Group";
import { NodeView } from "./SplitView";
import { useLayout } from "./store";

export function Dock({ setup }: { setup: DockSetup }) {
  const root = useLayout((layout) => layout.root);
  const maximized = useLayout((layout) => layout.maximized);
  return (
    <DockContext value={setup}>
      <div className="dock">
        {maximized ? <GroupView id={maximized} /> : <NodeView node={root} />}
      </div>
    </DockContext>
  );
}
