// The frame's registers: the innermost frame's as the thread stopped, an
// outer frame's as its callees saved them. A value that changed since the
// stop before is marked, as variables are.

import { assert } from "../assert";
import { useRequest } from "../data";
import type { At } from "../focus";
import type { Register, Row } from "../protocol";
import { errorClass, hidden } from "./hidden";
import { LinkedExpansion, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

/** A register's row: the innermost frame's are the thread's, and change. */
function row(register: Register, innermost: boolean): Row {
  return {
    name: register.name,
    text: register.value ?? "not saved",
    detail: null,
    type: register.role,
    path: `$${register.name}`,
    children: null,
    editable: innermost && register.value !== null,
    memory: null,
    memoryBytes: null,
    truncated: false,
    drawings: [],
  };
}

export function Registers() {
  const focus = useFocus();
  const why = hidden(focus, "Registers");
  assert(why !== null || focus.at !== null, "registers shown with no stop");
  const registers = useRequest("registers", why === null ? (focus.at as At) : null);

  let body: React.ReactNode;
  if (why) {
    body = <div className="empty">{why}</div>;
  } else if (registers.error) {
    body = <div className={errorClass(registers.error)}>{registers.error.message}</div>;
  } else if (!registers.data) {
    body = <div className="empty">Reading registers…</div>;
  } else {
    const at = focus.at;
    assert(at !== null, "registers listed with no stop");
    body = (
      <ul className="value-list" data-testid="registers">
        {registers.data.registers.map((register) => (
          <ValueRow
            key={register.name}
            row={row(register, at.frame === 0)}
            // Each frame's registers are compared with that frame's.
            parent={`registers@${at.frame}`}
            depth={0}
            missing={register.value === null ? "absent" : undefined}
          />
        ))}
      </ul>
    );
  }
  return (
    <div className="pane-body registers">
      <LinkedExpansion>{body}</LinkedExpansion>
    </div>
  );
}

/** Whose registers they are: the frame's. */
export function RegistersBadge() {
  const at = useFocus().at;
  return at ? <span className="count">frame {at.frame}</span> : null;
}
