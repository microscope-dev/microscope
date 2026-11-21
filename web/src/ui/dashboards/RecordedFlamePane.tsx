import { createMemo, createSignal, For, Match, onSettled, Show, Switch } from "solid-js";

import { checkAccount, useAccount } from "../../account";
import { KIND } from "../../documents";
import { type Project, reload, request, useDirectory } from "../../orgs";
import { formatCount } from "../../profile/paint";
import {
  type CallTree,
  findPath,
  matching,
  nameOf,
  pathOf,
  treeFromColumns,
} from "../../profile/tree";
import type { ProfileKey } from "../../protocol";
import { useLoaded } from "../Orgs";
import { showPane } from "../panes";
import { FlameGraph } from "../profile/FlameView";
import { useTreeSearch } from "../profile/Search";

interface FlameResponse {
  nodes: number;
  weight: number;
  samples: number;
  unit: string;
  blocks: number;
  truncated?: boolean;
  keys: ProfileKey[];
  own: number[];
  total: number[];
  key: number[];
  depth: number[];
}

interface LoadedFlame {
  tree: CallTree;
  unit: string;
  blocks: number;
  truncated: boolean;
}

async function queryFlame(projectId: number): Promise<LoadedFlame> {
  const answer = await request<FlameResponse>("POST", "/api/query/flame", {
    project_id: projectId,
  });
  return {
    tree: treeFromColumns({
      own: Float64Array.from(answer.own),
      total: Float64Array.from(answer.total),
      key: Uint32Array.from(answer.key),
      depth: Uint32Array.from(answer.depth),
      keys: answer.keys,
      weight: answer.weight,
      samples: answer.samples,
    }),
    unit: answer.unit,
    blocks: answer.blocks,
    truncated: Boolean(answer.truncated),
  };
}

function projectOptionLabel(project: Project): string {
  return project.org ? `${project.org.name} / ${project.name}` : project.name;
}

function showAccount() {
  showPane(KIND.account);
}

function retryProjects() {
  void reload();
}

export function RecordedFlamePane() {
  const now = useAccount();
  const directory = useDirectory();
  const [chosenId, setChosenId] = createSignal<number | null>(null);
  const [search, setSearch] = createSignal("");
  const [zoomPath, setZoomPath] = createSignal<string[] | null>(null);
  const [selectedName, setSelectedName] = createSignal<string | null>(null);

  onSettled(() => {
    void checkAccount();
  });

  const projects = createMemo(() => {
    const list = directory().projects ?? [];
    return list.toSorted((a, b) => a.name.localeCompare(b.name) || a.id - b.id);
  });

  const activeProjectId = createMemo(() => {
    const chosen = chosenId();
    const list = projects();
    if (chosen !== null && list.some((project) => project.id === chosen)) {
      return chosen;
    }
    return list[0]?.id ?? null;
  });

  const loadKey = createMemo(() => {
    const id = activeProjectId();
    return now().kind === "in" && id !== null ? `project:${id}` : "idle";
  });
  const loaded = useLoaded<LoadedFlame | null>(() => {
    const id = activeProjectId();
    return now().kind === "in" && id !== null ? queryFlame(id) : Promise.resolve(null);
  }, loadKey);

  const current = loaded.data;
  const tree = createMemo(() => current()?.tree ?? null);
  const searched = useTreeSearch(tree, search);
  const zoom = createMemo(() => {
    const currentTree = tree();
    const path = zoomPath();
    return currentTree && path ? findPath(currentTree, path) : null;
  });
  const selected = createMemo(() => {
    const currentTree = tree();
    const name = selectedName();
    return currentTree && name
      ? matching(currentTree, (candidate) => candidate === name).nodes
      : null;
  });

  const onProjectChange = (event: Event & { currentTarget: HTMLSelectElement }) => {
    setChosenId(Number(event.currentTarget.value));
    setZoomPath(null);
    setSelectedName(null);
  };
  const onSearchInput = (event: InputEvent & { currentTarget: HTMLInputElement }) => {
    setSearch(event.currentTarget.value);
    setSelectedName(null);
  };
  const onClear = () => {
    setSearch("");
    setZoomPath(null);
    setSelectedName(null);
  };
  const onZoom = (zoomed: CallTree, node: number) => {
    setZoomPath(pathOf(zoomed, node));
    setSelectedName(nameOf(zoomed, node));
  };

  const unavailable = createMemo(() => {
    const accountNow = now();
    return accountNow.kind === "unavailable" ? accountNow : null;
  });

  return (
    <Show
      when={now().kind === "in"}
      fallback={
        <div class="pane-body pane-text">
          <Switch
            fallback={
              <>
                <p>Sign in to view recorded flame graphs.</p>
                <div class="actions">
                  <button type="button" class="button" onClick={showAccount}>
                    Sign in
                  </button>
                </div>
              </>
            }
          >
            <Match when={now().kind === "checking"}>
              <p class="muted">Checking whether you are signed in…</p>
            </Match>
            <Match when={unavailable()}>
              {(state) => (
                <p class="muted">
                  Recorded dashboards need the services backend, which is unavailable ({state().why}
                  ).
                </p>
              )}
            </Match>
          </Switch>
        </div>
      }
    >
      <div class="pane grow">
        <div class="profile-details">
          <Show when={projects().length > 0}>
            <select
              class="input compact"
              aria-label="Project"
              value={activeProjectId() ?? ""}
              onChange={onProjectChange}
            >
              <For each={projects()} keyed={(project) => project.id}>
                {(project) => (
                  <option value={project().id} selected={project().id === activeProjectId()}>
                    {projectOptionLabel(project())}
                  </option>
                )}
              </For>
            </select>
          </Show>
          <button
            type="button"
            class="tb ghost"
            disabled={activeProjectId() === null}
            onClick={loaded.again}
          >
            Refresh
          </button>
          <input
            type="search"
            class="input profile-search"
            placeholder="Search frames"
            aria-label="Search frames"
            value={search()}
            onInput={onSearchInput}
          />
          <Show when={search() !== "" || zoomPath() !== null || selectedName() !== null}>
            <button type="button" class="tb ghost" onClick={onClear}>
              Clear
            </button>
          </Show>
          <Show when={searched().note}>
            {(note) => (
              <span class="muted" data-testid="matched">
                {note()}
              </span>
            )}
          </Show>
          <Show when={current()}>
            {(flame) => (
              <span class="muted" data-testid="recorded-summary">
                {formatCount(flame().blocks, "block")} ·{" "}
                {formatCount(flame().tree.samples, "sample")}
                {flame().truncated ? " · truncated" : ""}
              </span>
            )}
          </Show>
        </div>
        <div class="pane-body">
          <Switch fallback={<div class="empty muted">Reading the flame graph…</div>}>
            <Match when={directory().projects === null}>
              <Show
                when={directory().error}
                fallback={<div class="empty muted">Loading projects…</div>}
              >
                {(message) => (
                  <div class="empty error" role="alert">
                    <p>{message()}</p>
                    <div class="actions">
                      <button type="button" class="button" onClick={retryProjects}>
                        Retry
                      </button>
                    </div>
                  </div>
                )}
              </Show>
            </Match>
            <Match when={projects().length === 0}>
              <div class="empty muted">
                No projects yet. Create a project to query recorded data.
              </div>
            </Match>
            <Match when={loaded.error()}>
              {(message) => (
                <div class="empty error" role="alert">
                  {message()}
                </div>
              )}
            </Match>
            <Match when={current()}>
              {(flame) => (
                <Show
                  when={flame().tree.count > 0}
                  fallback={<div class="empty muted">No recorded samples in this project yet.</div>}
                >
                  <FlameGraph
                    tree={flame().tree}
                    zoom={zoom()}
                    matched={searched().matched}
                    selected={selected()}
                    onZoom={onZoom}
                    unit={flame().unit}
                  />
                </Show>
              )}
            </Match>
          </Switch>
        </div>
      </div>
    </Show>
  );
}
