import { useLocation, useNavigate } from "@solidjs/router";
import type { JSX } from "@solidjs/web";
import {
  createEffect,
  createMemo,
  createSignal,
  For,
  Match,
  onSettled,
  Show,
  Switch,
} from "solid-js";

import { withinBase } from "../../base";
import { isCaptureProfile } from "../../captures";
import { parseSearch } from "../../focus";
import { toHref } from "../../history";
import {
  cleared,
  duringLabel,
  type ProfileLook,
  type ProfileView,
  parseBand,
  parseInstance,
  parseRange,
  parseTransform,
  queryOf,
  rangeOf,
  scopeOf,
  untransformed,
  validateProfileLook,
  withInstance,
  withLook,
  withoutSite,
} from "../../profile/look";
import {
  formatCount,
  formatDuration,
  formatPercent,
  formatSpan,
  formatWeight,
} from "../../profile/paint";
import {
  bucketLabel,
  lossNotes,
  sampledKernel,
  tracedBy,
  traceNotes,
} from "../../profile/syscalls";
import { threadName } from "../../profile/zones";
import type { ProfileCommand, ProfileInfo, ProfileSummary } from "../../protocol";
import { useConnection, useModel } from "../../store";
import { openShare } from "../../tab";
import type { PaneProps } from "../../workbench/kinds";
import { change, useLayout, useLink, workbench } from "../../workbench/store";
import { useDialogFocus, useDismiss } from "../dialog";
import {
  ProfileContext,
  type ProfileFocus,
  useAnswer,
  useDisconnected,
  useProfile,
} from "./context";
import { CommandBlock, FileDetails } from "./FileDetails";
import { FlameView } from "./FlameView";
import { FunctionsView } from "./FunctionsView";
import { Lanes } from "./Lanes";
import { arrange, leadingPane, lookOf, paneParams, readLink, storeLook, viewOf } from "./link";
import { Chip, ClearChip, LaneChip, LookButton, ShareBar } from "./parts";
import { KernelPane } from "./SyscallsView";
import { TreeView } from "./TreeView";
import { ZonesPane } from "./ZonesPane";

const sameLook = (a: ProfileLook, b: ProfileLook) => JSON.stringify(a) === JSON.stringify(b);

export function ProfilePaneBody(props: PaneProps) {
  const view = createMemo(() => viewOf(props.pane) ?? null);
  const params = createMemo(() => paneParams(props.pane));
  const held = useLink(
    () => params().link,
    (value) => value,
  );
  const linked = createMemo(() => readLink(held()));
  const look = createMemo<ProfileLook>(() => {
    const currentView = view();
    return {
      ...linked()?.look,
      ...params().own,
      ...(currentView === null ? {} : { view: currentView }),
    };
  });
  const profile = createMemo(() => linked()?.profile ?? "");
  const navigate = useNavigate();
  const location = useLocation();

  const pathname = createMemo(() => withinBase(location.pathname));
  const inSession = createMemo(() => pathname().startsWith("/s/"));
  const hosted = createMemo(() => isCaptureProfile(profile()));
  const addressed = createMemo(() => pathname() === `/profile/${profile()}`);
  const focused = useLayout((layout) => leadingPane(layout) === props.id);
  // Only the focused standalone profile pane mirrors its `ProfileLook` into the
  // browser URL; `/s/...` sessions and hosted captures own the URL themselves.
  const leads = createMemo(() => focused() && !inSession() && !hosted());

  const search = createMemo(() => (leads() ? parseSearch(location.search) : null));

  const setLook = (by: (look: ProfileLook) => ProfileLook, push = false, carry = false) => {
    const layout = workbench.getState().layout;
    const now = lookOf(layout, props.id);
    if (!now) {
      return;
    }
    const next = by(now);
    const leading =
      leadingPane(layout) === props.id &&
      !withinBase(location.pathname).startsWith("/s/") &&
      !isCaptureProfile(profile());
    change((each) => storeLook(each, props.id, next, carry).layout);
    if (leading) {
      navigate(toHref(`/profile/${profile()}`, next as Record<string, unknown>), {
        replace: !push,
      });
    }
  };

  let led = false;
  createEffect(
    () => ({
      linked: linked(),
      leads: leads(),
      addressed: addressed(),
      search: search(),
      look: look(),
      profile: profile(),
    }),
    ({
      linked: currentLinked,
      leads: isLeading,
      addressed: isAddressed,
      search: currentSearch,
      look: currentLook,
      profile: currentProfile,
    }) => {
      const took = isLeading && !led;
      led = isLeading;
      if (!currentLinked || !currentSearch) {
        return;
      }
      const kept = validateProfileLook(currentSearch);
      const dropped = Object.keys(currentSearch).some((key) => !(key in kept));
      const stale = took && !sameLook(kept, currentLook);
      if (isLeading && (!isAddressed || stale || dropped)) {
        const shown = !isAddressed || took ? currentLook : kept;
        navigate(toHref(`/profile/${currentProfile}`, shown as Record<string, unknown>), {
          replace: true,
        });
      }
    },
  );

  const offer = (summary: ProfileSummary, live: boolean) => {
    const link = params().link;
    if (link !== null) {
      change((layout) => arrange(layout, link, summary, live));
    }
  };

  return (
    <Show
      when={linked()}
      fallback={<div class="page muted">This pane has lost its profile. Close it.</div>}
    >
      <Profile
        view={view()}
        profile={profile()}
        look={look()}
        setLook={setLook}
        leads={leads()}
        onSummary={offer}
      />
    </Show>
  );
}

function ViewBody(props: { view: ProfileView }) {
  return (
    <Switch
      fallback={
        <div class="profile-view">
          <Switch fallback={<FlameView />}>
            <Match when={props.view === "kernel"}>
              <KernelPane />
            </Match>
            <Match when={props.view === "functions"}>
              <FunctionsView />
            </Match>
            <Match when={props.view === "tree"}>
              <TreeView />
            </Match>
          </Switch>
        </div>
      }
    >
      <Match when={props.view === "zones"}>
        <ZonesPane />
      </Match>
      <Match when={props.view === "details"}>
        <FileDetails />
      </Match>
    </Switch>
  );
}

const MISSING_CARD = { width: "min(560px, 100%)" };
const NO_MARGIN = { margin: "0" };

interface FrameProps {
  profile: string;
  look: ProfileLook;
  setLook: ProfileFocus["setLook"];
  leads?: boolean | undefined;
}

function ProfileFrame(
  props: FrameProps & {
    onSummary?: ((summary: ProfileSummary, live: boolean) => void) | undefined;
    children: JSX.Element;
  },
) {
  const info = useModel((model) => model.profiles.find((each) => each.id === props.profile));

  const revision = createMemo(() => info()?.revision ?? 0);
  const summary = useAnswer("profile", () => ({
    profile: props.profile,
    revision: revision(),
  }));
  const disconnected = useDisconnected();
  const live = createMemo(() => info()?.recording ?? false);

  createEffect(
    () => ({ data: summary().data, live: live() }),
    ({ data, live: isLive }) => {
      if (data && props.onSummary) {
        props.onSummary(data, isLive);
      }
    },
  );

  let root: HTMLDivElement | undefined;
  const escape = (event: KeyboardEvent) => {
    const target = event.target as Element;
    const inside = root?.contains(target) ?? false;
    const leads = props.leads ?? true;
    if (
      event.key !== "Escape" ||
      event.defaultPrevented ||
      !(inside || (leads && root !== undefined && target.contains(root))) ||
      target.closest("input, textarea, select, [role=dialog]")
    ) {
      return;
    }
    if (cleared(props.look) !== props.look) {
      event.preventDefault();
      props.setLook(cleared, true);
    }
  };
  onSettled(() => {
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  });

  const focus = createMemo<ProfileFocus | null>(() => {
    const data = summary().data;
    if (!data) {
      return null;
    }
    return {
      profile: props.profile,
      summary: data,
      look: props.look,
      query: { ...queryOf(props.profile, props.look), revision: revision() },
      unit: props.look.src === "waits" ? "nanoseconds" : data.unit,
      setLook: props.setLook,
    };
  });

  return (
    <Switch fallback={<div class="page muted">Reading the profile…</div>}>
      <Match when={summary().error}>
        {(error) => {
          const missing = createMemo(() => !info() && error().kind === "invalid");
          return (
            <div class="page">
              <div class="card" style={MISSING_CARD} data-testid="profile-missing">
                <div class="card-head">
                  <h1>{missing() ? "No such profile" : "Could not read the profile"}</h1>
                </div>
                <div class="card-body">
                  <p style={NO_MARGIN}>{error().message}.</p>
                  <Show when={missing()}>
                    <p class="muted" style={NO_MARGIN}>
                      Serve it with <code>microscope web PROFILE.usprof</code>.
                    </p>
                  </Show>
                </div>
              </div>
            </div>
          );
        }}
      </Match>
      <Match when={focus()}>
        <ProfileContext value={focus}>
          <div
            class="profile"
            data-testid="profile"
            ref={(node) => {
              root = node;
            }}
          >
            <Show when={disconnected()}>
              {/* oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a notice, not a form's result */}
              <div class="profile-untimed profile-disconnected" role="status">
                Disconnected. This pane keeps what it last read until microscope answers again.
              </div>
            </Show>
            {props.children}
          </div>
        </ProfileContext>
      </Match>
    </Switch>
  );
}

export function Profile(
  props: FrameProps & {
    view?: ProfileView | null | undefined;
    onSummary?: ((summary: ProfileSummary, live: boolean) => void) | undefined;
  },
) {
  return (
    <ProfileFrame
      profile={props.profile}
      look={props.look}
      setLook={props.setLook}
      leads={props.leads}
      onSummary={props.onSummary}
    >
      <Show when={props.view} fallback={<Overview />}>
        {(view) => <ViewBody view={view()} />}
      </Show>
    </ProfileFrame>
  );
}

function Overview() {
  const focus = useProfile();
  const info = useModel((model) => model.profiles.find((each) => each.id === focus().profile));
  const recordingInfo = createMemo(() => {
    const current = info();
    return current && (current.recording || current.ended) ? current : undefined;
  });
  return (
    <>
      <Accounting summary={focus().summary} />
      <Show when={recordingInfo()}>{(current) => <Recording info={current()} />}</Show>
      <ProfileHead />
      <Lanes />
    </>
  );
}

function ProfileHead() {
  const focus = useProfile();
  const range = createMemo(() => rangeOf(focus().look));
  const snapshots = createMemo(() => focus().summary.snapshots > 0);
  const sampled = createMemo(() => focus().summary.samples > 0 || focus().summary.syscalls === 0);
  const zoned = createMemo(() => focus().summary.zoneSites.length > 0);
  const showZones = () => focus().setLook((prev) => withLook(prev, "view", "zones"), true);
  const showDetails = () => focus().setLook((prev) => withLook(prev, "view", "details"), true);
  const open = useModel((model) => model.link === "open");
  const canShareUpload = createMemo(() => open() && !isCaptureProfile(focus().profile));

  return (
    <div class="profile-head">
      <button type="button" class="tb ghost" onClick={showDetails}>
        File details
      </button>
      <Show when={canShareUpload()}>
        <button
          type="button"
          class="tb ghost"
          data-testid="profile-share"
          title="Upload this capture to Microscope and get a shareable link"
          onClick={() => openShare(focus().profile)}
        >
          Share
        </button>
      </Show>
      <Show when={snapshots()}>
        <div class="segmented">
          <For each={SOURCES}>
            {([src, label, title]) => (
              <LookButton field="src" value={src} title={title} pressed={focus().look.src === src}>
                {label}
              </LookButton>
            )}
          </For>
        </div>
      </Show>
      <Show when={snapshots() && focus().look.src !== "waits"}>
        <div class="segmented" title="What the timeline has a lane for">
          <For each={LANES}>
            {([lanes, label]) => (
              <LookButton field="lanes" value={lanes} pressed={focus().look.lanes === lanes}>
                {label}
              </LookButton>
            )}
          </For>
        </div>
      </Show>
      <Show when={zoned()}>
        <button
          type="button"
          class="tb ghost"
          title="Every zone site, and one's lengths"
          onClick={showZones}
        >
          Zones
        </button>
      </Show>
      <div class="profile-query">
        <Show when={focus().look.zi ? undefined : range()}>
          {(currentRange) => (
            <ClearChip
              field="range"
              label={formatSpan(currentRange())}
              title="Samples in this time range"
            />
          )}
        </Show>
        <ZoneChips />
        <LaneChip />
        <For each={focus().look.lb ?? []}>{(label) => <LabelChip label={label} />}</For>
        <For each={focus().look.tf ?? []}>
          {(entry, index) => <TransformChip entry={entry} index={index()} />}
        </For>
        <Show when={focus().look.sf}>
          {(sf) => (
            <ClearChip
              field="sf"
              label={`syscalls: ${sf()}`}
              title="System calls this selection keeps, in strace's -e syntax"
              removeLabel="Remove the system call filter"
            />
          )}
        </Show>
        <Show when={focus().look.sl}>
          {(sl) => (
            <ClearChip
              field="sl"
              label={`syscalls ${bucketLabel(Number(sl()))}`}
              title="System calls of this latency"
              removeLabel="Remove the latency band"
            />
          )}
        </Show>
        <Show when={focus().look.du}>
          {(du) => (
            <ClearChip
              field="du"
              label={`during ${duringLabel(du())}`}
              title="Samples taken while their thread was in these system calls"
              removeLabel="Keep samples outside these calls too"
            />
          )}
        </Show>
        <Show when={sampled() && focus().look.bz !== "rest"}>
          <Compare />
        </Show>
      </div>
    </div>
  );
}

const LANES = [
  [undefined, "Thread lanes"],
  ["tasks", "Task lanes"],
] as const;

const SOURCES = [
  [undefined, "CPU", "Where the program ran"],
  ["waits", "Waits", "Where task snapshots found tasks waiting"],
] as const;

const DIFF_MEASURES = [
  [undefined, "time", "Each function's cost, as report --base compares it"],
  ["share", "share", "Each function's share of its own profile"],
] as const;

function ZoneChips() {
  const focus = useProfile();
  const instance = createMemo(() => {
    const zi = focus().look.zi;
    return zi ? parseInstance(zi) : null;
  });
  const scope = createMemo(() => scopeOf(focus().look));
  const dropSite = () => focus().setLook(withoutSite, true);
  const dropInstance = () => focus().setLook((prev) => withInstance(prev, null), true);
  const dropScope = () =>
    // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
    focus().setLook((prev) => withLook(withLook(prev, "zf", undefined), "bz", undefined), true);
  const dropRest = () =>
    // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
    focus().setLook((prev) => withLook(withLook(prev, "bz", undefined), "dm", undefined), true);
  const banded = createMemo(() => {
    const zb = focus().look.zb;
    const band = zb ? parseBand(zb) : null;
    return band ? ` ${formatDuration(band[0])} – ${formatDuration(band[1])}` : "";
  });
  return (
    <>
      <Show when={focus().look.zs}>
        {(zs) => (
          <Chip
            label={`zone: ${zs()}${banded()}`}
            title="The zone site chosen; removing it clears the zone selection"
            remove={dropSite}
          />
        )}
      </Show>
      <Show when={instance()}>
        {(current) => (
          <Chip
            label={`${formatDuration(current().span[1] - current().span[0])}, ${threadName(focus().summary.threads, current().thread)}`}
            title="Samples of the threads selected during this zone"
            remove={dropInstance}
          />
        )}
      </Show>
      <Show when={focus().look.bz === "rest" ? undefined : scope()}>
        {(currentScope) => (
          <Chip
            label={currentScope().band ? "inside the band" : "inside every one"}
            title="Samples inside these zones, nested ones included"
            remove={dropScope}
          />
        )}
      </Show>
      <Show when={scope() && focus().look.bz === "rest"}>
        <Chip
          label="the band vs the rest"
          title="Red grew and blue shrank in the band's zones against the site's others"
          remove={dropRest}
        />
        <DiffMeasure />
      </Show>
    </>
  );
}

function DiffMeasure() {
  const focus = useProfile();
  return (
    <div class="segmented" title="What the change column compares">
      <For each={DIFF_MEASURES}>
        {([dm, label, title]) => (
          <LookButton field="dm" value={dm} title={title} pressed={focus().look.dm === dm}>
            {label}
          </LookButton>
        )}
      </For>
    </div>
  );
}

function LabelChip(props: { label: string }) {
  const focus = useProfile();
  const remove = () =>
    focus().setLook((prev) => {
      const kept = (prev.lb ?? []).filter((each) => each !== props.label);
      return withLook(prev, "lb", kept.length > 0 ? kept : undefined);
    });
  return <Chip label={props.label} title="Samples with this label" remove={remove} />;
}

function TransformChip(props: { entry: string; index: number }) {
  const focus = useProfile();
  const remove = () => focus().setLook((prev) => untransformed(prev, props.index), true);
  const transform = createMemo(() => {
    const parsed = parseTransform(props.entry);
    return parsed && parsed.op !== "collapseCycles" ? parsed : undefined;
  });

  return (
    <Show when={transform()}>
      {(current) => (
        <Chip
          label={`${current().op} ${current().name}`}
          title="Remove this transform"
          remove={remove}
        />
      )}
    </Show>
  );
}

function Compare() {
  const focus = useProfile();
  const profiles = useModel((model) => model.profiles);
  const others = createMemo(() => profiles().filter((each) => each.id !== focus().profile));
  const baseRange = createMemo(() => {
    const br = focus().look.br;
    return br ? parseRange(br) : null;
  });

  const removeBase = () =>
    focus().setLook(
      (prev) =>
        // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
        withLook(withLook(withLook(prev, "bp", undefined), "br", undefined), "dm", undefined),
      true,
    );
  const chooseBase = (event: Event & { currentTarget: HTMLSelectElement }) => {
    const chosen = event.currentTarget.value;
    event.currentTarget.value = "";
    focus().setLook(
      (prev) =>
        chosen === "range"
          ? // oxlint-disable-next-line unicorn/no-useless-undefined -- withLook takes undefined to remove the key
            withLook(withLook(prev, "br", prev.range), "range", undefined)
          : withLook(prev, "bp", chosen || undefined),
      true,
    );
  };

  const diffLabel = createMemo(() => {
    const bp = focus().look.bp;
    const br = baseRange();
    const of =
      bp && bp !== focus().profile
        ? (others().find((each) => each.id === bp)?.name ?? bp)
        : "this profile";
    const during = br ? ` ${formatSpan(br)}` : "";
    return `diff with ${of}${during}`;
  });

  return (
    <Switch>
      <Match when={focus().look.bp || baseRange()}>
        <Chip
          label={diffLabel()}
          title="Red grew and blue shrank against this base"
          remove={removeBase}
        />
        <DiffMeasure />
      </Match>
      <Match when={focus().look.range || others().length > 0}>
        <select class="input profile-by" aria-label="Compare with" value="" onChange={chooseBase}>
          <option value="">no diff</option>
          <Show when={focus().look.range}>
            <option value="range">diff with the selected range</option>
          </Show>
          <For each={others()} keyed={(each) => each.id}>
            {(each) => <option value={each().id}>diff with {each().name}</option>}
          </For>
        </select>
      </Match>
    </Switch>
  );
}

function Recording(props: { info: ProfileInfo }) {
  const connection = useConnection();
  const control = useModel((model) => model.hello?.role === "control");
  const [failure, setFailure] = createSignal<string | null>(null);
  const stop = () => {
    void connection
      .request("stopRecording", { profile: props.info.id })
      .catch((error: Error) => setFailure(error.message));
  };
  return (
    <div class="profile-recording" data-testid="recording">
      <Show
        when={props.info.recording}
        fallback={
          <span class="muted">
            {props.info.name} ended: {props.info.ended}
          </span>
        }
      >
        <span class="pill running">Recording {props.info.name}</span>
      </Show>
      <Show when={props.info.recording && control()}>
        <button type="button" class="tb ghost" onClick={stop}>
          Stop
        </button>
      </Show>
      <Show when={failure()}>{(message) => <span class="error">{message()}</span>}</Show>
    </div>
  );
}

function Stat(props: {
  label: string;
  title?: string | undefined;
  warn?: boolean | undefined;
  children: JSX.Element;
}) {
  return (
    <div class="stat" title={props.title}>
      <span class="stat-label">{props.label}</span>
      <span class={props.warn ? "stat-value warn" : "stat-value"}>{props.children}</span>
    </div>
  );
}

function Program(props: { summary: ProfileSummary }) {
  const [open, setOpen] = createSignal(false);
  let anchor: HTMLDivElement | undefined;
  const toggle = () => setOpen((was) => !was);
  const away = () => setOpen(false);
  useDismiss(() => anchor, open, away);
  const program = createMemo(
    () =>
      props.summary.command?.arguments?.[0] ??
      props.summary.command?.text.split(" ")[0] ??
      props.summary.program,
  );
  const name = createMemo(() => {
    const full = program();
    return full ? full.slice(full.lastIndexOf("/") + 1) : "";
  });
  return (
    <Show when={program()}>
      <div
        class="stat anchor"
        ref={(node) => {
          anchor = node;
        }}
      >
        <span class="stat-label">Program</span>
        <Show when={props.summary.command} fallback={<span class="stat-value">{name()}</span>}>
          {(command) => (
            <>
              <button
                type="button"
                class="stat-value stat-program"
                aria-haspopup="dialog"
                aria-expanded={open() ? "true" : "false"}
                title="Show the whole command"
                onClick={toggle}
              >
                {name()}
                <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
                  <path
                    d="M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8s-2.4 4.5-6.5 4.5S1.5 8 1.5 8Z"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.3"
                    stroke-linejoin="round"
                  />
                  <circle cx="8" cy="8" r="2" fill="currentColor" />
                </svg>
              </button>
              <Show when={open()}>
                <CommandPopover command={command()} />
              </Show>
            </>
          )}
        </Show>
      </div>
    </Show>
  );
}

function CommandPopover(props: { command: ProfileCommand }) {
  let dialog: HTMLDivElement | undefined;
  useDialogFocus(() => dialog);
  return (
    <div
      class="popover command-popover"
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- a popover, not a modal <dialog>
      role="dialog"
      aria-label="Command"
      tabindex={-1}
      ref={(node) => {
        dialog = node;
      }}
    >
      <CommandBlock command={props.command} />
    </div>
  );
}

const VDSO =
  "Calls through the vDSO, such as clock_gettime, never enter the kernel and are not traced";

function Accounting(props: { summary: ProfileSummary }) {
  const truncated = createMemo(() =>
    props.summary.truncations.reduce((sum, each) => sum + each.count, 0),
  );
  const source = createMemo(
    () => props.summary.settings.find((each) => each.name === "source")?.value,
  );
  const rate = createMemo(() => source()?.match(/^CPU time at (\d+ Hz)$/u)?.[1]);
  const kernel = createMemo(() => sampledKernel(props.summary.settings));
  const behind = createMemo(() =>
    props.summary.behind.reduce((sum, [from, to]) => sum + (to - from), 0),
  );
  const span = createMemo(() => Math.max(props.summary.duration, 1));
  const offCpu = createMemo(() =>
    props.summary.threads.reduce((sum, each) => sum + each.blocked + each.preempted, 0),
  );
  const cpu = createMemo(() => props.summary.unit === "nanoseconds");
  const busy = createMemo(() => props.summary.weight / span());
  const by = createMemo(() => tracedBy(props.summary.settings));
  const probesLost = createMemo(
    () =>
      props.summary.probes.overwritten +
      props.summary.probes.noRing +
      props.summary.probes.interrupted +
      props.summary.probes.malformed,
  );
  const sampled = createMemo(() => props.summary.samples > 0 || props.summary.syscalls === 0);
  const notes = createMemo(() => [
    ...lossNotes(props.summary.syscallsLost),
    ...traceNotes(props.summary.settings),
  ]);

  return (
    <div class="profile-accounting" data-testid="accounting">
      <Program summary={props.summary} />
      <Show when={sampled() && source()}>
        {(src) => (
          <Stat label="Sampling" title={kernel() ? `${src()}, kernel too` : src()}>
            {rate() ?? src()}
            {kernel() ? " + kernel" : ""}
          </Stat>
        )}
      </Show>
      <Stat label="Ran for" title="How long the recording ran">
        {formatDuration(props.summary.duration)}
      </Stat>
      <Show when={props.summary.threads.length > 0}>
        <Stat label="Threads">{formatCount(props.summary.threads.length)}</Stat>
      </Show>
      <Switch>
        <Match when={sampled() && cpu()}>
          <Stat
            label="Cores busy"
            title={`On average ${busy().toFixed(1)} CPUs ran the program at once${props.summary.threads.length > 0 ? `, of ${props.summary.threads.length} threads` : ""}`}
          >
            ≈{busy().toFixed(1)}
            <Show when={props.summary.threads.length > 0}>
              <ShareBar share={busy() / props.summary.threads.length} />
            </Show>
          </Stat>
          <Stat label="On CPU" title="CPU time sampled, every thread's added up">
            {formatDuration(props.summary.weight)}
          </Stat>
        </Match>
        <Match when={sampled()}>
          <Stat label="Sampled">{formatWeight(props.summary.weight, props.summary.unit)}</Stat>
        </Match>
      </Switch>
      <Show when={offCpu() > 0}>
        <Stat label="Off CPU" title="Time the threads spent blocked or preempted, added up">
          {formatDuration(offCpu())}
        </Stat>
      </Show>
      <Show when={props.summary.syscalls > 0}>
        <Stat
          label="Calls traced"
          title={by().title ? `${by().title}\n${VDSO}` : VDSO}
          warn={by().warn ?? false}
        >
          {formatCount(props.summary.syscalls)}
          {by().text}
        </Stat>
      </Show>
      <Show when={props.summary.throttles > 0}>
        <Stat label="Throttled" warn>
          {formatCount(props.summary.throttles)}
        </Stat>
      </Show>
      <Show when={truncated() > 0}>
        <Stat
          label="Cut short"
          title={props.summary.truncations.map((each) => `${each.what}: ${each.count}`).join("\n")}
          warn
        >
          {formatPercent(truncated(), props.summary.samples)}
        </Stat>
      </Show>
      <Show when={props.summary.behind.length > 0}>
        <Stat
          label="Paused to catch up"
          title={`Sampling was off ${formatCount(props.summary.behind.length, "time")} while microscope caught up on the samples it had read; a lower --rate or --stack-bytes keeps more`}
          warn
        >
          {formatPercent(behind(), span())}
        </Stat>
      </Show>
      <Show
        when={
          props.summary.probes.events > 0 ||
          probesLost() > 0 ||
          props.summary.probeProblems.length > 0
        }
      >
        <Stat
          label="Probe events"
          title={[
            `${formatCount(props.summary.probes.overwritten, "record")} overwritten before microscope read them`,
            `${formatCount(props.summary.probes.noRing, "event")} had no ring buffer to go to`,
            `${formatCount(props.summary.probes.interrupted, "event")} dropped by a signal handler`,
            `${formatCount(props.summary.probes.malformed, "record")} broke the probes ABI`,
            ...props.summary.probeProblems.map((problem) => problem.text),
          ].join("\n")}
          warn={probesLost() > 0 || props.summary.probeProblems.length > 0}
        >
          {formatCount(props.summary.probes.events)}, lost {formatCount(probesLost())}
        </Stat>
      </Show>
      <Show when={props.summary.snapshots > 0}>
        <Stat
          label="Task snapshots"
          title={[
            `${formatPercent(props.summary.taskSamples, props.summary.samples)} of samples have a known task`,
            ...props.summary.snapshotProblems.map(
              (problem) => `${formatDuration(problem.time)}: ${problem.text}`,
            ),
          ].join("\n")}
          warn={props.summary.snapshotProblems.length > 0}
        >
          {formatCount(props.summary.snapshots)}, stopped{" "}
          {formatPercent(props.summary.snapshotPause, span())}
        </Stat>
      </Show>
      <For each={notes()} keyed={(note) => note.text}>
        {(note) => (
          <span class={note().warn ? "stat-note warn" : "stat-note"} title={note().title}>
            {note().text}
          </span>
        )}
      </For>
    </div>
  );
}
