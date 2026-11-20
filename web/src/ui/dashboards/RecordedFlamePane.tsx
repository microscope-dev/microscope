import { type ChangeEvent, useCallback, useEffect, useMemo, useState } from "react";

import { checkAccount, useAccount } from "../../account";
import { KIND } from "../../documents";
import { type Project, request, useDirectory } from "../../orgs";
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
  };
}

function projectOptionLabel(project: Project): string {
  return project.org ? `${project.org.name} / ${project.name}` : project.name;
}

function showAccount() {
  showPane(KIND.account);
}

export function RecordedFlamePane() {
  const now = useAccount();
  const directory = useDirectory();
  const [chosenId, setChosenId] = useState<number | null>(null);
  const [search, setSearch] = useState("");
  const [zoomPath, setZoomPath] = useState<string[] | null>(null);
  const [selectedName, setSelectedName] = useState<string | null>(null);

  useEffect(() => {
    void checkAccount();
  }, []);

  const projects = useMemo(() => {
    const list = directory.projects ?? [];
    return list.toSorted((a, b) => a.name.localeCompare(b.name) || a.id - b.id);
  }, [directory.projects]);

  const activeProjectId = useMemo(() => {
    if (chosenId !== null && projects.some((project) => project.id === chosenId)) {
      return chosenId;
    }
    return projects[0]?.id ?? null;
  }, [chosenId, projects]);

  const loadKey =
    now.kind === "in" && activeProjectId !== null ? `project:${activeProjectId}` : "idle";
  const loaded = useLoaded<LoadedFlame | null>(
    () =>
      now.kind === "in" && activeProjectId !== null
        ? queryFlame(activeProjectId)
        : Promise.resolve(null),
    loadKey,
  );

  const current = loaded.data;
  const tree = current?.tree ?? null;
  const { matched, note } = useTreeSearch(tree, search);
  const zoom = useMemo(
    () => (tree && zoomPath ? findPath(tree, zoomPath) : null),
    [tree, zoomPath],
  );
  const selected = useMemo(
    () => (tree && selectedName ? matching(tree, (name) => name === selectedName).nodes : null),
    [tree, selectedName],
  );

  const onProjectChange = useCallback((event: ChangeEvent<HTMLSelectElement>) => {
    setChosenId(Number(event.target.value));
    setZoomPath(null);
    setSelectedName(null);
  }, []);
  const onSearchChange = useCallback((event: ChangeEvent<HTMLInputElement>) => {
    setSearch(event.target.value);
    setSelectedName(null);
  }, []);
  const onClear = useCallback(() => {
    setSearch("");
    setZoomPath(null);
    setSelectedName(null);
  }, []);
  const onZoom = useCallback((zoomed: CallTree, node: number) => {
    setZoomPath(pathOf(zoomed, node));
    setSelectedName(nameOf(zoomed, node));
  }, []);

  if (now.kind !== "in") {
    return (
      <div className="pane-body pane-text">
        {now.kind === "checking" ? (
          <p className="muted">Checking whether you are signed in…</p>
        ) : now.kind === "unavailable" ? (
          <p className="muted">
            Recorded dashboards need the services backend, which is unavailable ({now.why}).
          </p>
        ) : (
          <>
            <p>Sign in to view recorded flame graphs.</p>
            <div className="actions">
              <button type="button" className="button" onClick={showAccount}>
                Sign in
              </button>
            </div>
          </>
        )}
      </div>
    );
  }

  return (
    <div className="pane grow">
      <div className="profile-details">
        {projects.length > 0 ? (
          <select
            className="input compact"
            aria-label="Project"
            value={activeProjectId ?? ""}
            onChange={onProjectChange}
          >
            {projects.map((project) => (
              <option key={project.id} value={project.id}>
                {projectOptionLabel(project)}
              </option>
            ))}
          </select>
        ) : null}
        <button
          type="button"
          className="tb ghost"
          disabled={activeProjectId === null}
          onClick={loaded.again}
        >
          Refresh
        </button>
        <input
          type="search"
          className="input profile-search"
          placeholder="Search frames"
          aria-label="Search frames"
          value={search}
          onChange={onSearchChange}
        />
        {search !== "" || zoomPath !== null || selectedName !== null ? (
          <button type="button" className="tb ghost" onClick={onClear}>
            Clear
          </button>
        ) : null}
        {note ? (
          <span className="muted" data-testid="matched">
            {note}
          </span>
        ) : null}
        {current ? (
          <span className="muted" data-testid="recorded-summary">
            {formatCount(current.blocks, "block")} · {formatCount(current.tree.samples, "sample")}
          </span>
        ) : null}
      </div>
      <div className="pane-body">
        {directory.projects === null ? (
          <div className="empty muted">Loading projects…</div>
        ) : projects.length === 0 ? (
          <div className="empty muted">
            No projects yet. Create a project to query recorded data.
          </div>
        ) : loaded.error ? (
          <div className="empty error" role="alert">
            {loaded.error}
          </div>
        ) : current ? (
          current.tree.count === 0 ? (
            <div className="empty muted">No recorded samples in this project yet.</div>
          ) : (
            <FlameGraph
              tree={current.tree}
              zoom={zoom}
              matched={matched}
              selected={selected}
              onZoom={onZoom}
              unit={current.unit}
            />
          )
        ) : (
          <div className="empty muted">Reading the flame graph…</div>
        )}
      </div>
    </div>
  );
}
