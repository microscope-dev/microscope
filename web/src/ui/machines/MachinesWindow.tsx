import { createEffect, createMemo, createSignal, For, onSettled, Show, untrack } from "solid-js";

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
import { useHistory } from "../../history";
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

interface FieldRef {
  current: HTMLInputElement | undefined;
}

/**
 * Bidirectional sync between `machinesWindow` and the URL hash: opening pushes a single history
 * entry, in-window navigation replaces it, closing pops it back if we pushed it, and deep-linked
 * hashes are parked in `waiting` until auth resolves.
 */
function useUrl() {
  const history = useHistory();
  const open = useMachinesWindow((window) => window.open);
  const look = useMachinesWindow((window) => window.look);
  let written: string | null = null;

  onSettled(() => {
    const here = () => `${history.location.pathname}${history.location.search}`;
    const fromUrl = (moved: boolean) => {
      const parsed = parseLook(history.location.hash);
      if (parsed) {
        written = lookHash(parsed);
        const now = machinesWindow.getState();
        machinesWindow.setState(now.open ? { look: parsed } : { waiting: parsed, pushed: false });
      } else if (moved && machinesWindow.getState().open) {
        written = null;
        closeMachines();
        forgetPushed();
      }
    };
    fromUrl(false);
    return history.subscribe(({ action }) => {
      if (action.type === "PUSH" || action.type === "REPLACE") {
        const state = machinesWindow.getState();
        if (state.open && written !== null && parseLook(history.location.hash) === null) {
          history.replace(`${here()}#${written}`, history.location.state);
        }
        return;
      }
      fromUrl(true);
    });
  });

  createEffect(
    () => [open(), look()] as const,
    ([isOpen, currentLook]) => {
      const here = `${history.location.pathname}${history.location.search}`;
      if (isOpen) {
        const hash = lookHash(currentLook);
        if (hash === written && parseLook(history.location.hash) !== null) {
          return;
        }
        const state = machinesWindow.getState();
        written = hash;
        if (parseLook(history.location.hash) === null && !state.pushed) {
          machinesWindow.setState({ pushed: true });
          history.push(`${here}#${hash}`);
        } else {
          history.replace(`${here}#${hash}`, history.location.state);
        }
      } else if (written !== null) {
        written = null;
        const pushed = machinesWindow.getState().pushed;
        machinesWindow.setState({ pushed: false });
        if (pushed && history.canGoBack()) {
          history.back();
        } else if (parseLook(history.location.hash) !== null) {
          history.replace(here, history.location.state);
        }
      }
    },
  );
}

function useSignedIn() {
  const now = useAccount();
  const open = useMachinesWindow((window) => window.open);
  const waiting = useMachinesWindow((window) => window.waiting);
  createEffect(
    () => [open(), waiting(), now().kind] as const,
    ([isOpen, pending, kind]) => {
      if ((isOpen || pending) && kind === "checking") {
        void checkAccount();
      }
    },
  );
  createEffect(
    () => [now().kind, waiting(), open()] as const,
    ([kind, pending, isOpen]) => {
      if (kind === "in" && pending) {
        openMachines(pending);
      }
      if (kind !== "in" && kind !== "checking" && isOpen) {
        closeMachines();
      }
    },
  );
}

function Search(props: { look: Look; field: FieldRef }) {
  const initialQ = untrack(() => props.look.q);
  const [text, setText] = createSignal(initialQ);
  let shown = initialQ;

  createEffect(
    () => props.look.q,
    (q) => {
      if (q !== shown) {
        shown = q;
        setText(q);
      }
    },
  );

  const pending = createMemo(() => ({ text: text(), q: props.look.q, view: props.look.view }), {
    equals: (a, b) => a.text === b.text && a.q === b.q && a.view === b.view,
  });

  createEffect(
    () => pending(),
    ({ text: currentText, q, view }) => {
      if (currentText === q) {
        return;
      }
      const typedOver = machinesWindow.getState().look.machine;
      const timer = setTimeout(() => {
        shown = currentText;
        const picked = machinesWindow.getState().look.machine !== typedOver;
        setLook({
          q: currentText,
          ...(picked ? {} : { machine: null }),
          ...(view === "list" ? {} : { view: "list" }),
        });
      }, SEARCH_DELAY_MS);
      return () => clearTimeout(timer);
    },
  );

  const focusFirst = untrack(() => props.look.view === "list" && props.look.machine === null);
  onSettled(() => {
    if (focusFirst) {
      props.field.current?.focus();
    }
    return () => {
      props.field.current = undefined;
    };
  });

  return (
    <input
      ref={(node) => {
        props.field.current = node;
      }}
      class="input machines-search"
      type="search"
      aria-label="Find a machine"
      placeholder="Find a machine by hostname, IP, MAC, or instance id"
      value={text()}
      onInput={(event) => setText(event.currentTarget.value)}
    />
  );
}

function Tab(props: { view: View; label: string; chosen: boolean }) {
  const pick = () => setLook({ view: props.view, machine: null });
  const key = (event: KeyboardEvent) => {
    if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") {
      return;
    }
    event.preventDefault();
    const at = TABS.findIndex((tab) => tab.view === props.view);
    const next = TABS[(at + (event.key === "ArrowRight" ? 1 : TABS.length - 1)) % TABS.length];
    if (next) {
      setLook({ view: next.view, machine: null });
      requestAnimationFrame(() =>
        document.querySelector<HTMLElement>(`[data-machines-tab="${next.view}"]`)?.focus(),
      );
    }
  };
  return (
    <button
      type="button"
      role="tab"
      class="machines-tab"
      aria-selected={props.chosen ? "true" : "false"}
      tabindex={props.chosen ? 0 : -1}
      data-machines-tab={props.view}
      onClick={pick}
      onKeyDown={key}
    >
      {props.label}
    </button>
  );
}

function OrgPicker(props: { look: Look }) {
  const directory = useDirectory();
  const orgs = createMemo(() => directory().orgs ?? []);
  let select: HTMLSelectElement | undefined;
  createEffect(
    () => [orgs(), props.look.org] as const,
    ([, org]) => {
      if (select) {
        select.value = org === null ? "" : String(org);
      }
    },
  );
  const pick = (event: Event & { currentTarget: HTMLSelectElement }) =>
    setLook({
      ...HOME,
      view:
        props.look.view === "newkey" || props.look.view === "add" ? props.look.view : "overview",
      org: event.currentTarget.value === "" ? null : Number(event.currentTarget.value),
    });
  return (
    <Show when={orgs().length > 0}>
      <select
        ref={(node) => {
          select = node;
        }}
        class="machines-org"
        aria-label="Organization"
        value={props.look.org ?? ""}
        onChange={pick}
      >
        <option value="">Personal</option>
        <For each={orgs()}>
          {(org) => (
            <option value={org.id} selected={props.look.org === org.id}>
              {org.name}
            </option>
          )}
        </For>
      </select>
    </Show>
  );
}

function summary(live: number, groups: number, computedAt: number, now: number): string {
  const secs = Math.max(0, Math.round(now - computedAt));
  const updated =
    secs < 10 ? "just now" : secs < 120 ? `${secs} s ago` : `${Math.round(secs / 60)} min ago`;
  return `${live.toLocaleString("en-US")} live in ${groups} ${groups === 1 ? "group" : "groups"} · updated ${updated}`;
}

const newKeyBack = () => setLook({ view: "keys", machine: null });
const openAdd = () => setLook({ view: "add", machine: null });
const openNewKey = () => setLook({ view: "newkey", machine: null });
const backToOverview = () => setLook({ view: "overview", machine: null });

function Body(props: { look: Look; field: FieldRef }) {
  const now = useNow(5_000);
  const overview = useLoaded(
    () => fleetApi.overview(props.look.org, props.look.split),
    () => `${props.look.org}:${props.look.split}`,
    () => String(props.look.org),
  );
  const keysWanted = createMemo(
    () =>
      props.look.view === "keys" ||
      props.look.view === "add" ||
      props.look.view === "newkey" ||
      props.look.view === "list",
  );
  const keys = useLoaded(
    () => (keysWanted() ? fleetApi.keys(props.look.org) : Promise.resolve(null)),
    () => `${props.look.org}:${keysWanted()}`,
  );
  onSettled(() => {
    const timer = setInterval(overview.again, OVERVIEW_EVERY_MS);
    return () => clearInterval(timer);
  });
  const changed = () => {
    overview.again();
    keys.again();
  };
  const leaveAdding = () => {
    if (props.look.view === "newkey") {
      newKeyBack();
    } else {
      backToOverview();
    }
  };

  const empty = createMemo(() => {
    const data = overview.data();
    return (
      data !== null &&
      data.total.live + data.total.quiet + data.total.stopped + data.total.revoked === 0
    );
  });
  const skipToAdd = createMemo(
    () =>
      empty() &&
      props.look.q === "" &&
      (props.look.view === "overview" || props.look.view === "list"),
  );
  createEffect(
    () => skipToAdd(),
    (shouldSkip) => {
      if (shouldSkip) {
        setLook({ view: "add", machine: null });
      }
    },
  );

  const readyData = createMemo(() => (skipToAdd() ? null : overview.data()));
  const adding = createMemo(() => props.look.view === "add" || props.look.view === "newkey");

  return (
    <Show when={readyData()} fallback={<Retry error={overview.error()} again={overview.again} />}>
      {(data) => {
        const owner = createMemo(() => data().role === "owner");
        return (
          <>
            <Show
              when={adding()}
              fallback={
                <div class="machines-bar">
                  <div class="machines-tabs" role="tablist" aria-label="Views">
                    <For each={TABS}>
                      {(tab) => (
                        <Tab
                          view={tab.view}
                          label={tab.label}
                          chosen={tabOf(props.look.view) === tab.view}
                        />
                      )}
                    </For>
                  </div>
                  <Show
                    when={props.look.view === "keys"}
                    fallback={
                      <OwnerButton owner={owner()} onClick={openAdd}>
                        Add machines
                      </OwnerButton>
                    }
                  >
                    <OwnerButton owner={owner()} onClick={openNewKey}>
                      New key
                    </OwnerButton>
                  </Show>
                </div>
              }
            >
              <Show when={!(empty() && props.look.view === "add")}>
                <div class="machines-bar">
                  <button type="button" class="link-button" onClick={leaveAdding}>
                    {props.look.view === "newkey" ? "Back to enrollment keys" : "Back to machines"}
                  </button>
                </div>
              </Show>
            </Show>
            <Show when={props.look.view === "overview" || props.look.view === "list"}>
              <div class="machines-find">
                <Show when={props.look.view === "list"}>
                  <Search look={props.look} field={props.field} />
                </Show>
                <span class="muted machines-summary">
                  {summary(data().total.live, data().groups.length, data().computed_at, now())}
                </span>
              </div>
            </Show>
            <div
              class="machines-view"
              role={adding() ? undefined : "tabpanel"}
              aria-label={
                adding()
                  ? undefined
                  : TABS.find((tab) => tab.view === tabOf(props.look.view))?.label
              }
            >
              <Show when={props.look.view === "overview"}>
                <OverviewView overview={data()} />
              </Show>
              <Show when={props.look.view === "list"}>
                <ListView
                  look={props.look}
                  overview={data()}
                  keys={keys.data()}
                  now={now()}
                  changed={changed}
                />
              </Show>
              <Show when={props.look.view === "keys"}>
                <Show
                  when={keys.data()}
                  fallback={
                    <Retry error={keys.error()} again={keys.again} what="enrollment keys" />
                  }
                >
                  {(keyList) => (
                    <KeysView keys={keyList()} owner={owner()} now={now()} changed={changed} />
                  )}
                </Show>
              </Show>
              <Show when={adding()}>
                <Show
                  when={keys.data()}
                  fallback={
                    <Retry error={keys.error()} again={keys.again} what="enrollment keys" />
                  }
                >
                  {(keyList) => (
                    <Show when={`${props.look.view}:${props.look.org}`} keyed>
                      <AddView
                        overview={data()}
                        keys={keyList()}
                        install={props.look.view === "add"}
                        changed={changed}
                      />
                    </Show>
                  )}
                </Show>
              </Show>
            </div>
          </>
        );
      }}
    </Show>
  );
}

function Open(props: { look: Look }) {
  let dialog: HTMLDivElement | undefined;
  const field: FieldRef = { current: undefined };
  useDialogFocus(
    () => dialog,
    () => (props.look.view === "list" && props.look.machine === null ? field.current : undefined),
  );
  const now = useAccount();
  const key = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      closeMachines();
    } else if (event.key === "/" && !isTextBox(document.activeElement)) {
      event.preventDefault();
      if (field.current) {
        field.current.focus();
      } else {
        setLook({ view: "list", machine: null });
      }
    }
  };
  const title = createMemo(() =>
    props.look.view === "add"
      ? "Add machines"
      : props.look.view === "newkey"
        ? "New enrollment key"
        : "Machines",
  );
  return (
    <div class="overlay machines-overlay">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions */}
      <div
        class="machines"
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
        role="dialog"
        aria-modal="true"
        aria-label={title()}
        data-keys="escape"
        ref={(node) => {
          dialog = node;
        }}
        onKeyDown={key}
      >
        <header class="machines-head">
          <h2>{title()}</h2>
          <OrgPicker look={props.look} />
          <button type="button" class="machines-close" aria-label="Close" onClick={closeMachines}>
            ×
          </button>
        </header>
        <Show
          when={now().kind === "in"}
          fallback={<p class="muted machines-loading">Checking whether you are signed in…</p>}
        >
          <Body look={props.look} field={field} />
        </Show>
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
  return (
    <Show when={open()}>
      <Open look={look()} />
    </Show>
  );
}
