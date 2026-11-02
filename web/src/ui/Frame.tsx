// The page's frame: the menu bar and run toolbar above, the status bar
// below, and the keys, which work wherever the page is. Pages outside a
// session, such as joining one, show alone.

import { Link, Outlet, useLocation } from "@tanstack/react-router";
import { useEffect } from "react";
import { base } from "../base";
import { commandName } from "../model";
import { useModel } from "../store";
import { flash } from "../tab";
import { standalone } from "../target";
import { isTextBox, keepsEscape } from "../workbench/keys";
import { MenuBar } from "../workbench/MenuBar";
import { COMMANDS, useCommands } from "./commands";
import { KeyHelp } from "./KeyHelp";
import { NeedsLink } from "./pages";
import { RunToolbar } from "./RunToolbar";
import { Start } from "./Start";
import { StatusBar } from "./StatusBar";

export function Frame() {
  const link = useModel((model) => model.link);
  const name = useModel((model) => commandName(model.state));
  const inferior = useModel((model) => model.state?.inferior.state);
  const location = useLocation();
  const commands = useCommands();
  useKeys();

  useEffect(() => {
    const state = inferior === "stopped" ? "stopped" : inferior === "running" ? "running" : null;
    document.title = name ? `${name}${state ? ` · ${state}` : ""} · microscope` : "microscope";
  }, [name, inferior]);

  // Pages outside a session have nothing to control.
  const bare =
    location.pathname === "/join" || location.pathname === "/home" ? (
      <Outlet />
    ) : link === "unauthorized" ? (
      standalone ? (
        <Start />
      ) : (
        <NeedsLink reason="This browser has not joined this microscope session yet." />
      )
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
      <header className="frame-head">
        <div className="menu-row">
          <Link to="/" className="brand" aria-label="microscope home">
            <img src={`${base}favicon.svg`} alt="" />
            microscope
          </Link>
          <MenuBar
            registry={COMMANDS}
            availability={commands.availability}
            run={(id) => {
              const ran = commands.run(id);
              if (ran.state === "blocked") {
                flash(ran.why);
              }
            }}
          />
        </div>
        <RunToolbar />
      </header>
      <Outlet />
      <StatusBar />
      <KeyHelp />
    </div>
  );
}

/** Runs the debugger keys, claiming them from the browser while it does. */
function useKeys() {
  const { run } = useCommands();
  useEffect(() => {
    const listener = (event: KeyboardEvent) => {
      if (event.isComposing || (event.key === "Escape" && keepsEscape(document.activeElement))) {
        return;
      }
      const typing = isTextBox(document.activeElement);
      const command = COMMANDS.match(event, typing);
      if (command === null) {
        // Escape leaves a text box, giving the keys back to the debugger.
        if (typing && event.key === "Escape" && document.activeElement instanceof HTMLElement) {
          const box = document.activeElement;
          // After the box's own handlers, which may close what holds it.
          setTimeout(() => box.blur(), 0);
        }
        return;
      }
      const ran = run(command);
      if (ran.state === "blocked") {
        flash(ran.why);
      }
      // Function keys are claimed even when they mean nothing here, so F5
      // never reloads the debugger away.
      if (ran.state !== "absent" || /^F\d+$/.test(event.key)) {
        event.preventDefault();
      }
    };
    // Captured, so the debugger's keys reach it before any pane's handlers.
    window.addEventListener("keydown", listener, { capture: true });
    return () => window.removeEventListener("keydown", listener, { capture: true });
  }, [run]);
}
