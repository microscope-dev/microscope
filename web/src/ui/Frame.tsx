import { Link, Outlet, useLocation } from "@tanstack/react-router";
import { useCallback, useEffect } from "react";

import { base } from "../base";
import { commandName } from "../model";
import { useModel } from "../store";
import { flash } from "../tab";
import { isTextBox, keepsEscape } from "../workbench/keys";
import { MenuBar } from "../workbench/MenuBar";
import { COMMANDS, useCommands } from "./commands";
import { People } from "./People";
import { RunToolbar } from "./RunToolbar";
import { Share } from "./Share";
import { shown, Status } from "./Status";
import { StatusBar } from "./StatusBar";

const JOINING = { gridTemplateRows: "1fr" };

export function Frame() {
  const name = useModel((model) => commandName(model.state));
  const inferior = useModel((model) => model.state?.inferior.state);

  const loaded = useModel((model) => Boolean(model.state?.target));

  const quiet = useModel((model) => shown(model).pill === "idle");
  const location = useLocation();
  const { availability, run } = useCommands();
  useKeys();
  const runFromMenu = useCallback(
    (id: Parameters<typeof run>[0]) => {
      const ran = run(id);
      if (ran.state === "blocked") {
        flash(ran.why);
      }
    },
    [run],
  );

  useEffect(() => {
    const state = inferior === "stopped" ? "stopped" : inferior === "running" ? "running" : null;
    document.title = name ? `${name}${state ? ` · ${state}` : ""} · microscope` : "microscope";
  }, [name, inferior]);

  if (location.pathname === "/join") {
    return (
      <div className="app" style={JOINING}>
        <Outlet />
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
          <MenuBar registry={COMMANDS} availability={availability} run={runFromMenu} />
          <div className="menu-row-end">
            {!loaded && !quiet && <Status />}
            <People />
            <Share />
          </div>
        </div>
        {loaded && <RunToolbar />}
      </header>
      <Outlet />
      <StatusBar />
    </div>
  );
}

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
        if (typing && event.key === "Escape" && document.activeElement instanceof HTMLElement) {
          const box = document.activeElement;

          setTimeout(() => box.blur(), 0);
        }
        return;
      }
      const ran = run(command);
      if (ran.state === "blocked") {
        flash(ran.why);
      }

      if (ran.state !== "absent" || /^F\d+$/u.test(event.key)) {
        event.preventDefault();
      }
    };

    window.addEventListener("keydown", listener, { capture: true });
    return () => window.removeEventListener("keydown", listener, { capture: true });
  }, [run]);
}
