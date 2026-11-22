import { useLocation } from "@solidjs/router";
import type { JSX } from "@solidjs/web";
import { createEffect, createMemo, onSettled, Show } from "solid-js";

import { base, withinBase } from "../base";
import { dashboardsWindow } from "../dashboards";
import { dataWindow } from "../dataPolicy";
import { machinesWindow } from "../fleet";
import { toHref } from "../history";
import { commandName } from "../model";
import { useModel } from "../store";
import { flash } from "../tab";
import { isTextBox, keepsEscape } from "../workbench/keys";
import { MenuBar } from "../workbench/MenuBar";
import { COMMANDS, useCommands } from "./commands";
import { DashboardsWindow } from "./dashboards/DashboardsWindow";
import { ScopeBar } from "./dashboards/ScopeBar";
import { DataWindow } from "./data/DataWindow";
import { MachinesWindow } from "./machines/MachinesWindow";
import { People } from "./People";
import { RunToolbar } from "./RunToolbar";
import { Share } from "./Share";
import { shown, Status } from "./Status";
import { StatusBar } from "./StatusBar";

const JOINING = { "grid-template-rows": "1fr" };

export function Frame(props: { children?: JSX.Element }) {
  const name = useModel((model) => commandName(model.state));
  const inferior = useModel((model) => model.state?.inferior.state);

  const loaded = useModel((model) => Boolean(model.state?.target));

  const quiet = useModel((model) => shown(model).pill === "idle");
  const location = useLocation();
  const { availability, run } = useCommands();
  useKeys();
  const runFromMenu = (id: Parameters<typeof run>[0]) => {
    const ran = run(id);
    if (ran.state === "blocked") {
      flash(ran.why);
    }
  };

  createEffect(
    () => [name(), inferior()] as const,
    ([currentName, currentInferior]) => {
      const state =
        currentInferior === "stopped"
          ? "stopped"
          : currentInferior === "running"
            ? "running"
            : null;
      document.title = currentName
        ? `${currentName}${state ? ` · ${state}` : ""} · microscope`
        : "microscope";
    },
  );

  const joining = createMemo(() => withinBase(location.pathname) === "/join");

  return (
    <Show
      when={!joining()}
      fallback={
        <div class="app" style={JOINING}>
          {props.children}
        </div>
      }
    >
      <div class="app">
        <header class="frame-head">
          <div class="menu-row">
            <a href={toHref("/")} class="brand" aria-label="microscope home">
              <img src={`${base}favicon.svg`} alt="" />
              microscope
            </a>
            <MenuBar registry={COMMANDS} availability={availability} run={runFromMenu} />
            <div class="menu-row-end">
              <Show when={!loaded() && !quiet()}>
                <Status />
              </Show>
              <People />
              <Share />
            </div>
          </div>
          <Show when={loaded()}>
            <RunToolbar />
          </Show>
          <ScopeBar />
        </header>
        {props.children}
        <StatusBar />
        <MachinesWindow />
        <DashboardsWindow />
        <DataWindow />
      </div>
    </Show>
  );
}

function useKeys() {
  const { run } = useCommands();
  const listener = (event: KeyboardEvent) => {
    if (
      event.isComposing ||
      machinesWindow.getState().open ||
      dashboardsWindow.getState().open ||
      dashboardsWindow.getState().saveDialogOpen ||
      dataWindow.getState().open ||
      (event.key === "Escape" && keepsEscape(document.activeElement))
    ) {
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
  onSettled(() => {
    window.addEventListener("keydown", listener, { capture: true });
    return () => window.removeEventListener("keydown", listener, { capture: true });
  });
}
