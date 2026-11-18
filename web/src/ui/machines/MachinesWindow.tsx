import { useRouter } from "@tanstack/react-router";
import {
  type ChangeEvent,
  type KeyboardEvent,
  type RefObject,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import { checkAccount, useAccount } from "../../account";
import {
  closeMachines,
  fleetApi,
  forgetPushed,
  HOME,
  type Look,
  lookHash,
  machinesWindow,
  openMachines,
  parseLook,
  setLook,
  useMachinesWindow,
  type View,
} from "../../fleet";
import { useDirectory } from "../../orgs";
import { standalone } from "../../target";
import { isTextBox } from "../../workbench/keys";
import { useDialogFocus } from "../dialog";
import { useLoaded } from "../Orgs";
import { AddView } from "./Add";
import { KeysView } from "./Keys";
import { ListView, Retry } from "./List";
import { OverviewView } from "./Overview";
import { OwnerButton, useNow } from "./parts";

const TABS: readonly { view: View; label: string }[] = [
  { view: "overview", label: "Overview" },
  { view: "list", label: "Machines" },
  { view: "keys", label: "Enrollment keys" },
];

const tabOf = (view: View): View =>
  view === "add" ? "overview" : view === "newkey" ? "keys" : view;

const SEARCH_DELAY_MS = 250;

const OVERVIEW_EVERY_MS = 60_000;

function useUrl() {
  const router = useRouter();
  const open = useMachinesWindow((window) => window.open);
  const look = useMachinesWindow((window) => window.look);
  const written = useRef<string | null>(null);

  useEffect(() => {
    const history = router.history;
    const here = () => `${history.location.pathname}${history.location.search}`;
    const fromUrl = (moved: boolean) => {
      const parsed = parseLook(history.location.hash);
      if (parsed) {
        written.current = lookHash(parsed);
        const now = machinesWindow.getState();
        machinesWindow.setState(now.open ? { look: parsed } : { waiting: parsed, pushed: false });
      } else if (moved && machinesWindow.getState().open) {
        written.current = null;
        closeMachines();
        forgetPushed();
      }
    };
    fromUrl(false);
    return history.subscribe(({ action }) => {
      if (action.type === "PUSH" || action.type === "REPLACE") {
        const state = machinesWindow.getState();
        if (state.open && written.current !== null && parseLook(history.location.hash) === null) {
          history.replace(`${here()}#${written.current}`, history.location.state);
        }
        return;
      }
      fromUrl(true);
    });
  }, [router]);

  useEffect(() => {
    const history = router.history;
    const here = `${history.location.pathname}${history.location.search}`;
    if (open) {
      const hash = lookHash(look);
      if (hash === written.current && parseLook(history.location.hash) !== null) {
        return;
      }
      const state = machinesWindow.getState();
      written.current = hash;
      if (parseLook(history.location.hash) === null && !state.pushed) {
        machinesWindow.setState({ pushed: true });
        history.push(`${here}#${hash}`);
      } else {
        history.replace(`${here}#${hash}`, history.location.state);
      }
    } else if (written.current !== null) {
      written.current = null;
      const pushed = machinesWindow.getState().pushed;
      machinesWindow.setState({ pushed: false });
      if (pushed && history.canGoBack()) {
        history.back();
      } else if (parseLook(history.location.hash) !== null) {
        history.replace(here, history.location.state);
      }
    }
  }, [router, open, look]);
}

function useSignedIn() {
  const now = useAccount();
  const open = useMachinesWindow((window) => window.open);
  const waiting = useMachinesWindow((window) => window.waiting);
  useEffect(() => {
    if ((open || waiting) && now.kind === "checking") {
      void checkAccount();
    }
  }, [open, waiting, now.kind]);
  useEffect(() => {
    if (now.kind === "in" && waiting) {
      openMachines(waiting);
    }
    if (now.kind !== "in" && now.kind !== "checking" && open) {
      closeMachines();
    }
  }, [now.kind, waiting, open]);
}

function Search({ look, field }: { look: Look; field: RefObject<HTMLInputElement | null> }) {
  const [text, setText] = useState(look.q);
  const [shown, setShown] = useState(look.q);
  if (look.q !== shown) {
    setShown(look.q);
    setText(look.q);
  }
  useEffect(() => {
    if (text === look.q) {
      return;
    }
    const timer = setTimeout(() => {
      setShown(text);
      setLook({ q: text, machine: null, ...(look.view === "list" ? {} : { view: "list" }) });
    }, SEARCH_DELAY_MS);
    return () => clearTimeout(timer);
  }, [text, look.q, look.view]);
  const type = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => setText(event.target.value),
    [],
  );
  const finding = look.view === "list" && look.machine === null;
  const [focusFirst] = useState(finding);
  useEffect(() => {
    if (focusFirst) {
      field.current?.focus();
    }
  }, [focusFirst, field]);
  return (
    <input
      ref={field}
      className="input machines-search"
      type="search"
      aria-label="Find a machine"
      placeholder="Find a machine by hostname, IP, MAC, or instance id"
      value={text}
      onChange={type}
    />
  );
}

function Tab({ view, label, chosen }: { view: View; label: string; chosen: boolean }) {
  const pick = useCallback(() => setLook({ view, machine: null }), [view]);
  const key = useCallback(
    (event: KeyboardEvent<HTMLButtonElement>) => {
      if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") {
        return;
      }
      event.preventDefault();
      const at = TABS.findIndex((tab) => tab.view === view);
      const next = TABS[(at + (event.key === "ArrowRight" ? 1 : TABS.length - 1)) % TABS.length];
      if (next) {
        setLook({ view: next.view, machine: null });
        requestAnimationFrame(() =>
          document.querySelector<HTMLElement>(`[data-machines-tab="${next.view}"]`)?.focus(),
        );
      }
    },
    [view],
  );
  return (
    <button
      type="button"
      role="tab"
      className="machines-tab"
      aria-selected={chosen}
      tabIndex={chosen ? 0 : -1}
      data-machines-tab={view}
      onClick={pick}
      onKeyDown={key}
    >
      {label}
    </button>
  );
}

function OrgPicker({ look }: { look: Look }) {
  const directory = useDirectory();
  const orgs = directory.orgs ?? [];
  const pick = useCallback(
    (event: ChangeEvent<HTMLSelectElement>) =>
      setLook({
        ...HOME,
        view: look.view === "newkey" || look.view === "add" ? look.view : "overview",
        org: event.target.value === "" ? null : Number(event.target.value),
      }),
    [look.view],
  );
  if (orgs.length === 0) {
    return null;
  }
  return (
    <select
      className="machines-org"
      aria-label="Organization"
      value={look.org ?? ""}
      onChange={pick}
    >
      <option value="">Personal</option>
      {orgs.map((org) => (
        <option key={org.id} value={org.id}>
          {org.name}
        </option>
      ))}
    </select>
  );
}

function summary(live: number, groups: number, computedAt: number, now: number): string {
  const secs = Math.max(0, Math.round(now - computedAt));
  const updated =
    secs < 10 ? "just now" : secs < 120 ? `${secs} s ago` : `${Math.round(secs / 60)} min ago`;
  return `${live.toLocaleString("en-US")} live in ${groups} ${groups === 1 ? "group" : "groups"} · updated ${updated}`;
}

const newKeyBack = () => setLook({ view: "keys", machine: null });

function Body({ look, field }: { look: Look; field: RefObject<HTMLInputElement | null> }) {
  const now = useNow(5_000);
  const overview = useLoaded(() => fleetApi.overview(look.org), String(look.org));
  const keysWanted = look.view === "keys" || look.view === "add" || look.view === "newkey";
  const keys = useLoaded(
    () => (keysWanted ? fleetApi.keys(look.org) : Promise.resolve(null)),
    `${look.org}:${keysWanted}`,
  );
  const againOverview = overview.again;
  const againKeys = keys.again;
  useEffect(() => {
    const timer = setInterval(againOverview, OVERVIEW_EVERY_MS);
    return () => clearInterval(timer);
  }, [againOverview]);
  const changed = useCallback(() => {
    againOverview();
    againKeys();
  }, [againOverview, againKeys]);
  const add = useCallback(() => setLook({ view: "add", machine: null }), []);
  const newKey = useCallback(() => setLook({ view: "newkey", machine: null }), []);
  const back = useCallback(() => setLook({ view: "overview", machine: null }), []);

  const data = overview.data;
  const empty =
    data !== null &&
    data.total.live + data.total.quiet + data.total.stopped + data.total.revoked === 0;
  const skipToAdd = empty && look.q === "" && (look.view === "overview" || look.view === "list");
  useEffect(() => {
    if (skipToAdd) {
      setLook({ view: "add", machine: null });
    }
  }, [skipToAdd]);

  if (!data || skipToAdd) {
    return <Retry error={overview.error} again={againOverview} />;
  }
  const owner = data.role === "owner";
  const adding = look.view === "add" || look.view === "newkey";

  return (
    <>
      {adding ? (
        empty && look.view === "add" ? null : (
          <div className="machines-bar">
            <button
              type="button"
              className="link-button"
              onClick={look.view === "newkey" ? newKeyBack : back}
            >
              {look.view === "newkey" ? "Back to enrollment keys" : "Back to machines"}
            </button>
          </div>
        )
      ) : (
        <div className="machines-bar">
          <div className="machines-tabs" role="tablist" aria-label="Views">
            {TABS.map((tab) => (
              <Tab
                key={tab.view}
                view={tab.view}
                label={tab.label}
                chosen={tabOf(look.view) === tab.view}
              />
            ))}
          </div>
          {look.view === "keys" ? (
            <OwnerButton owner={owner} onClick={newKey}>
              New key
            </OwnerButton>
          ) : (
            <OwnerButton owner={owner} onClick={add}>
              Add machines
            </OwnerButton>
          )}
        </div>
      )}
      {!adding && (
        <div className="machines-find">
          <Search look={look} field={field} />
          <span className="muted machines-summary">
            {summary(data.total.live, data.groups.length, data.computed_at, now)}
          </span>
        </div>
      )}
      <div
        className="machines-view"
        role={adding ? undefined : "tabpanel"}
        aria-label={adding ? undefined : TABS.find((tab) => tab.view === tabOf(look.view))?.label}
      >
        {look.view === "overview" && <OverviewView overview={data} />}
        {look.view === "list" && (
          <ListView look={look} overview={data} now={now} changed={changed} />
        )}
        {look.view === "keys" &&
          (keys.data ? (
            <KeysView keys={keys.data} owner={owner} now={now} changed={changed} />
          ) : (
            <Retry error={keys.error} again={againKeys} what="enrollment keys" />
          ))}
        {adding &&
          (keys.data ? (
            <AddView
              key={`${look.view}:${look.org}`}
              overview={data}
              keys={keys.data}
              install={look.view === "add"}
              now={now}
              changed={changed}
            />
          ) : (
            <Retry error={keys.error} again={againKeys} what="enrollment keys" />
          ))}
      </div>
    </>
  );
}

function Open({ look }: { look: Look }) {
  const dialog = useRef<HTMLDivElement>(null);
  const field = useRef<HTMLInputElement>(null);
  useDialogFocus(dialog, look.view === "list" && look.machine === null ? field : undefined);
  const now = useAccount();
  const key = useCallback((event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === "Escape") {
      event.preventDefault();
      closeMachines();
    } else if (event.key === "/" && !isTextBox(document.activeElement)) {
      event.preventDefault();
      field.current?.focus();
    }
  }, []);
  const title =
    look.view === "add"
      ? "Add machines"
      : look.view === "newkey"
        ? "New enrollment key"
        : "Machines";
  return (
    <div className="overlay machines-overlay">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- Escape and / anywhere in the window */}
      <div
        className="machines"
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- an overlay the page draws, not a <dialog>
        role="dialog"
        aria-modal="true"
        aria-label={title}
        data-keys="escape"
        ref={dialog}
        onKeyDown={key}
      >
        <header className="machines-head">
          <h2>{title}</h2>
          <OrgPicker look={look} />
          <button
            type="button"
            className="machines-close"
            aria-label="Close"
            onClick={closeMachines}
          >
            ×
          </button>
        </header>
        {now.kind === "in" ? (
          <Body look={look} field={field} />
        ) : (
          <p className="muted machines-loading">Checking whether you are signed in…</p>
        )}
      </div>
    </div>
  );
}

export function MachinesWindow() {
  if (!standalone) {
    return null;
  }
  return <Hosted />;
}

function Hosted() {
  useUrl();
  useSignedIn();
  const open = useMachinesWindow((window) => window.open);
  const look = useMachinesWindow((window) => window.look);
  return open ? <Open look={look} /> : null;
}
