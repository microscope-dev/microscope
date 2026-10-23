// The frame's registers: the innermost frame's as the thread stopped, an
// outer frame's as its callees saved them. A value that changed since the
// stop before is marked, as variables are.

import { useState } from "react";
import { useRequest } from "../data";
import type { At } from "../focus";
import type { Register, Row } from "../protocol";
import { isBoolean, read, write } from "../storage";
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
  const [open, setOpen] = useState(() => read("microscope-registers-open", true, isBoolean));
  const why = hidden(focus, "Registers");
  const registers = useRequest("registers", open && why === null ? (focus.at as At) : null);

  let body: React.ReactNode;
  if (why) {
    body = <div className="empty">{why}</div>;
  } else if (registers.error) {
    body = <div className={errorClass(registers.error)}>{registers.error.message}</div>;
  } else if (!registers.data) {
    body = <div className="empty">Reading registers…</div>;
  } else {
    body = (
      <ul className="value-list" data-testid="registers">
        {registers.data.registers.map((register) => (
          <ValueRow
            key={register.name}
            row={row(register, (focus.at?.frame ?? 0) === 0)}
            // Each frame's registers are compared with that frame's.
            parent={`registers@${focus.at?.frame ?? 0}`}
            depth={0}
            missing={register.value === null ? "absent" : undefined}
          />
        ))}
      </ul>
    );
  }
  return (
    <section
      className={`pane ${open ? "registers" : ""}`}
      aria-label="Registers"
      data-pane="7"
      tabIndex={-1}
    >
      <div className="pane-head">
        <button
          type="button"
          className="twist"
          aria-expanded={open}
          aria-label={`${open ? "Close" : "Open"} registers`}
          onClick={() => {
            setOpen(!open);
            write("microscope-registers-open", !open);
          }}
        >
          {open ? "▾" : "▸"}
        </button>
        Registers
        {focus.at && <span className="count">frame {focus.at.frame}</span>}
        <span className="end">Alt+7</span>
      </div>
      {open && (
        <div className="pane-body">
          <LinkedExpansion>{body}</LinkedExpansion>
        </div>
      )}
    </section>
  );
}
