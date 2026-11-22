import { createEffect, createMemo, For, Match, onSettled, Show, Switch } from "solid-js";

import { checkAccount, useAccount } from "../../account";
import {
  closeData,
  DATA_TABS,
  dataHash,
  type DataTab,
  dataWindow,
  openData,
  parseDataHash,
  useDataWindow,
} from "../../dataPolicy";
import { useHistory } from "../../history";
import { useDirectory } from "../../orgs";
import { standalone } from "../../target";
import { isTextBox } from "../../workbench/keys";
import { useDialogFocus } from "../dialog";
import { PrivacyView } from "./PrivacyView";
import { RecordingView } from "./RecordingView";
import { ServicesView } from "./ServicesView";
import { StorageView } from "./StorageView";
import { SymbolsView } from "./SymbolsView";

function useDataUrl() {
  const history = useHistory();
  const open = useDataWindow((w) => w.open);
  const tab = useDataWindow((w) => w.tab);
  const org = useDataWindow((w) => w.org);
  let written: string | null = null;

  onSettled(() => {
    const fromUrl = (moved: boolean) => {
      const parsed = parseDataHash(history.location.hash);
      if (parsed) {
        written = dataHash(parsed.tab, parsed.org);
        const now = dataWindow.getState();
        dataWindow.setState(
          now.open ? { tab: parsed.tab, org: parsed.org } : { waiting: parsed, pushed: false },
        );
      } else if (moved && dataWindow.getState().open) {
        written = null;
        closeData();
        dataWindow.setState({ pushed: false });
      }
    };
    fromUrl(false);
    return history.subscribe(({ action }) => {
      if (action.type === "PUSH" || action.type === "REPLACE") {
        return;
      }
      fromUrl(true);
    });
  });

  createEffect(
    () => [open(), tab(), org()] as const,
    ([isOpen, curTab, curOrg]) => {
      const here = `${history.location.pathname}${history.location.search}`;
      if (isOpen) {
        const hash = dataHash(curTab, curOrg);
        if (hash === written && parseDataHash(history.location.hash) !== null) {
          return;
        }
        const st = dataWindow.getState();
        written = hash;
        if (parseDataHash(history.location.hash) === null && !st.pushed) {
          dataWindow.setState({ pushed: true });
          history.push(`${here}#${hash}`);
        } else {
          history.replace(`${here}#${hash}`, history.location.state);
        }
      } else if (written !== null) {
        written = null;
        const pushed = dataWindow.getState().pushed;
        dataWindow.setState({ pushed: false });
        if (pushed && history.canGoBack()) {
          history.back();
        } else if (parseDataHash(history.location.hash) !== null) {
          history.replace(here, history.location.state);
        }
      }
    },
  );
}

function useDataSignedIn() {
  const now = useAccount();
  const open = useDataWindow((w) => w.open);
  const waiting = useDataWindow((w) => w.waiting);
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
        openData(pending.tab, pending.org);
      }
      if (kind !== "in" && kind !== "checking" && isOpen) {
        closeData();
      }
    },
  );
}

function OrgSelect(props: { org: number | null; onChange: (org: number | null) => void }) {
  const dir = useDirectory();
  const orgs = createMemo(() => dir().orgs ?? []);
  return (
    <Show when={orgs().length > 0}>
      <select
        class="input compact"
        aria-label="Organization"
        value={props.org ?? ""}
        onChange={(e) =>
          props.onChange(e.currentTarget.value === "" ? null : Number(e.currentTarget.value))
        }
      >
        <option value="">Personal</option>
        <For each={orgs()}>{(o) => <option value={o.id}>{o.name}</option>}</For>
      </select>
    </Show>
  );
}

function selectTab(next: DataTab) {
  dataWindow.setState({ tab: next });
}

function onTabKeyDown(event: KeyboardEvent, idx: number) {
  if (event.key === "ArrowRight") {
    event.preventDefault();
    const next = DATA_TABS[(idx + 1) % DATA_TABS.length];
    if (next) {
      selectTab(next.id);
    }
  } else if (event.key === "ArrowLeft") {
    event.preventDefault();
    const prev = DATA_TABS[(idx + DATA_TABS.length - 1) % DATA_TABS.length];
    if (prev) {
      selectTab(prev.id);
    }
  }
}

function OpenDataWindow() {
  const tab = useDataWindow((w) => w.tab);
  const org = useDataWindow((w) => w.org);
  let dialog: HTMLDivElement | undefined;
  let searchEl: HTMLInputElement | undefined;
  useDialogFocus(() => dialog);

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      closeData();
      return;
    }
    if (event.key === "/" && !isTextBox(document.activeElement) && tab() === "services") {
      event.preventDefault();
      searchEl?.focus();
    }
  };

  return (
    <div class="overlay machines-overlay">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions */}
      <div
        class="machines data-window"
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
        role="dialog"
        aria-modal="true"
        aria-label="Data"
        data-keys="escape"
        ref={(node) => {
          dialog = node;
        }}
        onKeyDown={onKeyDown}
      >
        <header class="machines-head">
          <div class="data-head-left">
            <h2>Data</h2>
            <div class="data-tabs" role="tablist" aria-label="Data sections">
              <For each={DATA_TABS}>
                {(item, idx) => (
                  <button
                    type="button"
                    role="tab"
                    class={`data-tab ${tab() === item.id ? "active" : ""}`}
                    aria-selected={tab() === item.id ? "true" : "false"}
                    onClick={() => selectTab(item.id)}
                    onKeyDown={(e) => onTabKeyDown(e, idx())}
                  >
                    {item.label}
                  </button>
                )}
              </For>
            </div>
          </div>
          <div class="data-head-right">
            <OrgSelect org={org()} onChange={(nextOrg) => dataWindow.setState({ org: nextOrg })} />
            <button type="button" class="machines-close" aria-label="Close" onClick={closeData}>
              ×
            </button>
          </div>
        </header>

        <div class="machines-body data-body">
          <Switch>
            <Match when={tab() === "services"}>
              <ServicesView
                org={org()}
                searchRef={(el) => {
                  searchEl = el;
                }}
              />
            </Match>
            <Match when={tab() === "recording"}>
              <RecordingView org={org()} />
            </Match>
            <Match when={tab() === "privacy"}>
              <PrivacyView org={org()} />
            </Match>
            <Match when={tab() === "symbols"}>
              <SymbolsView />
            </Match>
            <Match when={tab() === "storage"}>
              <StorageView org={org()} />
            </Match>
          </Switch>
        </div>
      </div>
    </div>
  );
}

export function DataWindow() {
  if (!standalone) {
    return null;
  }
  return <HostedData />;
}

function HostedData() {
  useDataUrl();
  useDataSignedIn();
  const open = useDataWindow((w) => w.open);
  return (
    <Show when={open()}>
      <OpenDataWindow />
    </Show>
  );
}
