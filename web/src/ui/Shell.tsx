import { Outlet, useLocation } from "@tanstack/react-router";
import { useEffect } from "react";
import { commandFor, isTextBox } from "../keys";
import { commandName } from "../model";
import { useModel } from "../store";
import { useCommands } from "./commands";
import { KeyHelp } from "./KeyHelp";
import { NeedsLink } from "./pages";
import { Strip } from "./Strip";
import { Toolbar } from "./Toolbar";

export function Shell() {
  const link = useModel((model) => model.link);
  const name = useModel((model) => commandName(model.state));
  const inferior = useModel((model) => model.state?.inferior.state);
  const location = useLocation();
  useKeys();

  useEffect(() => {
    const state = inferior === "stopped" ? "stopped" : inferior === "running" ? "running" : null;
    document.title = name ? `${name}${state ? ` · ${state}` : ""} · microscope` : "microscope";
  }, [name, inferior]);

  // Pages outside a session have nothing to control.
  const bare =
    location.pathname === "/join" ? (
      <Outlet />
    ) : link === "unauthorized" ? (
      <NeedsLink reason="This browser has not joined this microscope session yet." />
    ) : link === "incompatible" ? (
      <NeedsLink reason="This page is from another version of microscope. Reload to get this one's." />
    ) : null;
  if (bare) {
    return (
      <div className="app" style={{ gridTemplateRows: "1fr" }}>
        {bare}
      </div>
    );
  }
  return (
    <div className="app">
      <Toolbar />
      <Outlet />
      <Strip />
      <KeyHelp />
    </div>
  );
}

/** Runs the debugger keys, claiming them from the browser while it does. */
function useKeys() {
  const run = useCommands();
  useEffect(() => {
    const listener = (event: KeyboardEvent) => {
      if (event.isComposing) {
        return;
      }
      const typing = isTextBox(document.activeElement);
      const command = commandFor(event, typing);
      if (command === null) {
        // Escape leaves a text box, giving the keys back to the debugger.
        if (typing && event.key === "Escape" && document.activeElement instanceof HTMLElement) {
          const box = document.activeElement;
          // After the box's own handlers, which may close what holds it.
          setTimeout(() => box.blur(), 0);
        }
        return;
      }
      // Function keys are claimed even when unavailable, so F5 never reloads
      // the debugger away; other keys only when they did something.
      if (run(command) || /^F\d+$/.test(event.key)) {
        event.preventDefault();
      }
    };
    // Captured, so the debugger's keys reach it before any pane's handlers.
    window.addEventListener("keydown", listener, { capture: true });
    return () => window.removeEventListener("keydown", listener, { capture: true });
  }, [run]);
}
