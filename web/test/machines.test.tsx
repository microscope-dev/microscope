import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  type RouterHistory,
  RouterProvider,
} from "@tanstack/react-router";
import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { type Locator, page, userEvent } from "vitest/browser";

import { account, type User } from "../src/account";
import {
  type Detail,
  type EnrollmentKey,
  HOME,
  type Machine,
  machinesWindow,
  openMachines,
  type Overview,
  setLook,
} from "../src/fleet";
import { directory } from "../src/orgs";
import { COMMANDS } from "../src/ui/commands";
import { MachinesWindow } from "../src/ui/machines/MachinesWindow";

const ME: User = { id: 1, username: "ana", email: "ana@example.com", totp_enabled: false };

type Answer = [number, unknown?];
type Route = Answer | ((body: unknown, url: URL) => Answer | Promise<Answer>);

function services(routes: Record<string, Route>) {
  const asked: { route: string; url: URL; body: unknown }[] = [];
  const real = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init: RequestInit = {}) => {
      const url = new URL(String(input), window.location.origin);
      if (!url.pathname.startsWith("/api/")) {
        return real(input, init);
      }
      const route = `${init.method ?? "GET"} ${url.pathname}`;
      const body = typeof init.body === "string" ? JSON.parse(init.body) : undefined;
      asked.push({ route, url, body });
      const found =
        routes[route] ??
        (route === "GET /api/users/me"
          ? ([200, ME] as Answer)
          : route === "GET /api/orgs"
            ? ([200, []] as Answer)
            : undefined);
      const [status, answer] =
        typeof found === "function"
          ? await found(body, url)
          : (found ?? [404, { error: "no such route" }]);
      return status === 204 || answer === undefined
        ? new Response(null, { status })
        : new Response(JSON.stringify(answer), {
            status,
            headers: { "Content-Type": "application/json" },
          });
    }),
  );
  account.setState({ now: { kind: "in", user: ME }, notice: null });
  return asked;
}

let history: RouterHistory | null = null;

const fragment = () => history?.location.hash.replace(/^#/u, "");

const text = (locator: Locator) => locator.query()?.textContent ?? "";

function mount(at = "/") {
  history = createMemoryHistory({ initialEntries: [at] });
  const router = createRouter({
    routeTree: createRootRoute({ component: MachinesWindow }),
    history,
  });
  render(<RouterProvider router={router as never} />);
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  machinesWindow.setState({ open: false, look: HOME, waiting: null, pushed: false }, true);
  account.setState({ now: { kind: "checking" }, notice: null, reset: null });
  directory.setState(
    { user: null, orgs: null, projects: null, invitations: null, error: null },
    true,
  );
  history = null;
});

const NOW = Math.floor(Date.now() / 1000);

function overview(changes: Partial<Overview> = {}): Overview {
  return {
    org_id: 3,
    role: "owner",
    agents_on: true,
    computed_at: NOW - 40,
    live_window_secs: 1500,
    newest_version: "0.4.2",
    total: { live: 55, quiet: 1, stopped: 30, revoked: 0 },
    groups: [
      {
        environment: "prod",
        cluster: "checkout-us1",
        live: 52,
        quiet: 1,
        stopped: 20,
        enrolled_24h: 6,
        stopped_24h: 5,
        went_quiet_24h: 1,
        live_min_7d: 44,
        live_max_7d: 70,
        hours: Array.from({ length: 168 }, (_, hour) => 44 + (hour % 26)),
        first_hour: NOW - 168 * 3600,
      },
      {
        environment: "staging",
        cluster: null,
        live: 3,
        quiet: 0,
        stopped: 10,
        enrolled_24h: 0,
        stopped_24h: 0,
        went_quiet_24h: 0,
        live_min_7d: 3,
        live_max_7d: 3,
        hours: Array.from({ length: 168 }, () => 3),
        first_hour: NOW - 168 * 3600,
      },
    ],
    environments: ["prod", "staging"],
    clusters: ["checkout-us1"],
    versions: ["0.4.2", "0.4.1"],
    ...changes,
  };
}

const EMPTY = overview({
  total: { live: 0, quiet: 0, stopped: 0, revoked: 0 },
  groups: [],
  environments: [],
  clusters: [],
  versions: [],
});

function machine(id: number, hostname: string, changes: Partial<Machine> = {}): Machine {
  return {
    id,
    hostname,
    state: "live",
    primary_ip: "10.0.3.17",
    environment: "prod",
    cluster: "checkout-us1",
    place: "checkout-us1",
    agent_version: "0.4.2",
    created_at: NOW - 3 * 86_400,
    last_seen_at: NOW - 120,
    stopped_at: null,
    revoked_at: null,
    key: { id: 9, name: "prod-eks" },
    warnings: [],
    matched: null,
    ...changes,
  };
}

function detail(changes: Partial<Detail> = {}): Detail {
  return {
    id: 81723,
    org_id: 3,
    hostname: "ip-10-0-3-17.us-west-2.compute.internal",
    state: "live",
    fingerprint: "SHA256:Jw3kq9Ps",
    environment: "prod",
    cluster: "checkout-us1",
    labels: { team: "payments" },
    agent_version: "0.4.2",
    primary_ip: "10.0.3.17",
    egress_ip: "203.0.113.24",
    created_at: NOW - 3 * 86_400,
    last_seen_at: NOW - 120,
    stopped_at: null,
    revoked_at: null,
    key: { id: 9, name: "prod-eks" },
    inventory: {
      cloud: {
        provider: "aws",
        region: "us-west-2",
        zone: "us-west-2b",
        instance_type: "m7i.2xlarge",
        instance_id: "i-0a1b2c3d4e5f60718",
      },
      kubernetes: { node: "ip-10-0-3-17" },
      network: {
        interfaces: [
          {
            name: "eth0",
            kind: "physical",
            state: "up",
            mac: "0a:3f:91:2c:7e:01",
            addresses: ["10.0.3.17/20"],
          },
        ],
        primary: { address: "10.0.3.17", interface: "eth0" },
        hidden: { container_bridge: 2 },
      },
      system: {
        os: "Amazon Linux 2023",
        kernel: "6.1.0",
        arch: "x86_64",
        cpus: 8,
        memory_mib: 32_768,
      },
      capabilities: ["bpf", "perfmon", "sys_ptrace"],
    },
    events: [],
    replaces: null,
    shares_host_ids: [],
    warnings: [],
    ...changes,
  };
}

function key(id: number, name: string, changes: Partial<EnrollmentKey> = {}): EnrollmentKey {
  return {
    id,
    name,
    environment: "prod",
    labels: {},
    live: 100,
    unrevoked: 120,
    uses: 2314,
    max_uses: null,
    created_at: NOW - 90 * 86_400,
    created_by: "ana",
    expires_at: null,
    last_used_at: NOW - 60,
    revoked_at: null,
    ...changes,
  };
}

const machines = () => page.getByRole("dialog", { name: "Machines" });

it("opens on the groups, with how each has moved and only the quiet one in red", async () => {
  const asked = services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/machines": [200, { machines: [], next: null, newest_version: null }],
  });
  mount();
  openMachines({});

  await expect
    .element(machines().getByText(/^55 live in 2 groups · updated 4\d s ago$/u))
    .toBeVisible();
  const prod = machines().getByRole("button", {
    name: /^prod, checkout-us1: 52 live now, 44 to 70 live over the last 7 days/u,
  });
  await expect.element(prod).toBeVisible();
  await expect
    .element(
      machines().getByRole("button", {
        name: /^staging, not Kubernetes: 3 live now, 3 live over the last 7 days/u,
      }),
    )
    .toBeVisible();
  expect(machines().getByRole("img").query()).toBeNull();
  expect(
    machines()
      .getByRole("button", { name: /went quiet/u })
      .all(),
  ).toHaveLength(1);

  await machines().getByRole("button", { name: "1 went quiet" }).click();
  await expect
    .poll(() => asked.findLast((one) => one.route === "GET /api/fleet/machines")?.url.search)
    .toBe("?state=quiet&environment=prod&cluster=checkout-us1");
  expect(fragment()).toBe("machines/list?state=quiet&environment=prod&cluster=checkout-us1");
});

it("opens on adding machines when there are none, without tabs, a way back, or an organization picker", async () => {
  services({ "GET /api/fleet": [200, EMPTY], "GET /api/fleet/keys": [200, []] });
  mount();
  openMachines({});

  const add = page.getByRole("dialog", { name: "Add machines" });
  await expect.element(add.getByRole("button", { name: "Create key" })).toBeVisible();
  expect(fragment()).toBe("machines/add");
  expect(add.getByRole("tablist").query()).toBeNull();
  expect(add.getByRole("button", { name: "Back to machines" }).query()).toBeNull();
  expect(add.getByRole("combobox", { name: "Organization" }).query()).toBeNull();
});

it("says why it could not load, keeps the window open, and tries again", async () => {
  let fail = true;
  services({
    "GET /api/fleet": () =>
      fail ? [503, { error: "the database is not answering" }] : [200, overview()],
  });
  mount();
  openMachines({});

  await expect
    .element(machines().getByRole("alert"))
    .toHaveTextContent("Could not load machines: the database is not answering.");
  fail = false;
  await machines().getByRole("button", { name: "Try again" }).click();
  await expect.element(machines().getByText(/^55 live in 2 groups/u)).toBeVisible();
});

it("searches every state, marks the match, and says which address matched when it is not shown", async () => {
  const asked = services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/machines": (_, url) =>
      url.searchParams.get("q") === "10.0.3"
        ? [
            200,
            {
              machines: [
                machine(1, "ip-10-0-3-17.us-west-2.compute.internal"),
                machine(2, "db-03.ams1", {
                  state: "stopped",
                  primary_ip: "192.168.40.3",
                  stopped_at: NOW - 86_400,
                  matched: "bond0.30 10.0.3.5",
                  warnings: ["old_version"],
                }),
              ],
              next: null,
              newest_version: "0.4.2",
            },
          ]
        : [200, { machines: [], next: null, newest_version: "0.4.2" }],
  });
  mount();
  openMachines({ view: "list" });

  await machines().getByRole("searchbox", { name: "Find a machine" }).fill("10.0.3");
  await expect.element(machines().getByText("matched bond0.30 10.0.3.5")).toBeVisible();
  const search = asked.findLast((one) => one.route === "GET /api/fleet/machines");
  expect(search?.url.searchParams.get("q")).toBe("10.0.3");
  expect(search?.url.searchParams.has("state")).toBe(false);
  await expect.element(machines().getByRole("combobox", { name: "State" })).toHaveValue("any");
  await expect.element(machines().getByText("old version")).toBeVisible();
  expect(
    machines().getByRole("button", { name: "db-03.ams1, stopped" }).element().textContent,
  ).toContain("yesterday");
  expect(fragment()).toBe("machines/list?q=10.0.3");
});

it("searches only on the Machines tab, and / goes there to search", async () => {
  services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/machines": [200, { machines: [], next: null, newest_version: null }],
  });
  mount();
  openMachines({});
  await expect.element(machines().getByRole("tab", { name: "Overview" })).toBeVisible();
  expect(machines().getByRole("searchbox").query()).toBeNull();

  await userEvent.keyboard("/");
  const search = machines().getByRole("searchbox", { name: "Find a machine" });
  await expect.element(search).toHaveFocus();
  expect(fragment()).toBe("machines/list");
});

it("says nothing matched, and offers to clear only the filters a chip set", async () => {
  services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/machines": [200, { machines: [], next: null, newest_version: null }],
  });
  mount("/#machines/list?q=zz&version=0.4.1");
  await expect
    .element(
      machines().getByText("No machine has zz in its hostname, addresses, MACs, or instance id."),
    )
    .toBeVisible();
  await machines().getByRole("button", { name: "Clear filters" }).click();
  await expect.poll(() => fragment()).toBe("machines/list?q=zz");
  expect(machines().getByRole("button", { name: "Clear filters" }).query()).toBeNull();
});

it("opens a machine beside the list, and walks the rows with the arrow keys", async () => {
  const base = detail();
  const network = base.inventory.network ?? { interfaces: [] };
  const inventory = {
    ...base.inventory,
    network: {
      ...network,
      interfaces: [
        ...network.interfaces,
        { name: "eth1", kind: "physical", state: "down", mac: "0a:00:00:00:00:01" },
        { name: "eth2", kind: "physical", mac: "0a:00:00:00:00:02" },
      ],
    },
  };
  services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/machines": [
      200,
      {
        machines: [machine(81723, "ip-10-0-3-17"), machine(81724, "ip-10-0-3-41")],
        next: null,
        newest_version: null,
      },
    ],
    "GET /api/fleet/machines/81723": [
      200,
      detail({
        inventory,
        shares_host_ids: [
          { kind: "same_image", source: "machine-id", live_machines: 3 },
          { kind: "same_machine", source: "smbios-uuid", live_machines: 1 },
        ],
      }),
    ],
  });
  mount("/#machines/list");

  const first = machines().getByRole("button", { name: "ip-10-0-3-17, live" });
  await first.click();
  const panel = machines().getByRole("complementary", { name: "Machine" });
  await expect
    .element(panel.getByRole("heading", { name: "ip-10-0-3-17.us-west-2.compute.internal" }))
    .toBeVisible();
  await expect.element(panel.getByText("us-west-2b · m7i.2xlarge")).toBeVisible();
  await expect.element(panel.getByText("8 CPUs · 32 GB")).toBeVisible();
  await expect.element(panel.getByText("BPF, perf events, ptrace")).toBeVisible();
  await expect.element(panel.getByText("2 container bridges not listed")).toBeVisible();
  await expect.element(panel.getByText("(down)")).toBeVisible();
  expect(panel.getByText("()").elements()).toHaveLength(0);
  await expect.element(panel.getByText(/Enrolled 3 days ago with key prod-eks\./u)).toBeVisible();
  await expect
    .element(panel.getByText(/^3 other live machines have the same machine-id\./u))
    .toBeVisible();
  await expect.element(panel.getByText(/so it runs on this machine too\.$/u)).toBeVisible();
  expect(fragment()).toBe("machines/m/81723");

  await userEvent.keyboard("{ArrowDown}");
  await expect
    .element(machines().getByRole("button", { name: "ip-10-0-3-41, live" }))
    .toHaveFocus();
});

it("asks before removing, and Escape closes the question before the window", async () => {
  const asked = services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/machines": [
      200,
      { machines: [machine(81723, "db-03.ams1")], next: null, newest_version: null },
    ],
    "GET /api/fleet/machines/81723": [200, detail({ hostname: "db-03.ams1" })],
    "DELETE /api/fleet/machines/81723": [204],
  });
  mount("/#machines/m/81723");

  await page.getByRole("button", { name: "Remove" }).click();
  const question = page.getByRole("alertdialog", { name: "Remove db-03.ams1?" });
  await expect.poll(() => text(question)).toContain("it enrolls again as a new machine");
  await expect.element(question.getByRole("button", { name: "Cancel" })).toHaveFocus();
  await userEvent.keyboard("{Escape}");
  expect(question.query()).toBeNull();
  await expect.element(machines()).toBeVisible();

  await page.getByRole("button", { name: "Remove" }).click();
  await question.getByRole("button", { name: "Remove" }).click();
  await expect
    .poll(() => asked.some((one) => one.route === "DELETE /api/fleet/machines/81723"))
    .toBe(true);
  await expect.poll(() => fragment()).toBe("machines/list");

  await userEvent.keyboard("{Escape}");
  await expect.poll(() => machinesWindow.getState().open).toBe(false);
  expect(fragment()).toBe("");
});

it("revokes a key only once its name is typed, alone unless asked to take its machines too", async () => {
  const asked = services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/keys": [
      200,
      [
        key(9, "prod-eks"),
        key(4, "old-prod", { live: 0, uses: 41, revoked_at: NOW - 15 * 86_400 }),
      ],
    ],
    "POST /api/fleet/keys/9/revoke": [200, { revoked_machines: 100 }],
  });
  mount("/#machines/keys");

  await expect.element(machines().getByRole("button", { name: "2,314" })).toBeVisible();
  expect(machines().getByRole("searchbox").query()).toBeNull();
  await expect.element(machines().getByRole("row", { name: /old-prod/u })).toHaveClass("revoked");
  await machines().getByRole("button", { name: "Revoke" }).click();
  const question = page.getByRole("alertdialog", { name: "Revoke key prod-eks?" });
  const also = question.getByRole("checkbox");
  await expect.element(also).not.toBeChecked();
  const typed = question.getByRole("textbox", { name: "Type prod-eks to confirm" });
  await expect.element(typed).toHaveFocus();
  const revoke = question.getByRole("button", { name: "Revoke key" });
  await expect.element(revoke).toBeDisabled();
  await typed.fill("prod-ek");
  await expect.element(revoke).toBeDisabled();
  await userEvent.keyboard("{Enter}");
  await typed.fill("PROD-EKS");
  await expect.element(revoke).toBeDisabled();
  expect(asked.some((one) => one.route === "POST /api/fleet/keys/9/revoke")).toBe(false);
  await typed.fill("prod-eks");
  await revoke.click();
  await expect
    .poll(() => asked.find((one) => one.route === "POST /api/fleet/keys/9/revoke")?.body)
    .toEqual({ machines: false });

  await machines().getByRole("button", { name: "Revoke" }).click();
  const again = page.getByRole("alertdialog");
  await again.getByRole("checkbox").click();
  await again.getByRole("textbox").fill("prod-eks");
  await userEvent.keyboard("{Enter}");
  await expect
    .poll(() => asked.findLast((one) => one.route === "POST /api/fleet/keys/9/revoke")?.body)
    .toEqual({ machines: true });
});

it("revokes the machines of a key that is already revoked, once its name is typed", async () => {
  const asked = services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/keys": [
      200,
      [key(4, "old-prod", { live: 3, unrevoked: 7, uses: 41, revoked_at: NOW - 15 * 86_400 })],
    ],
    "POST /api/fleet/keys/4/revoke": [200, { revoked_machines: 7 }],
  });
  mount("/#machines/keys");

  await machines().getByRole("button", { name: "Revoke machines" }).click();
  const question = page.getByRole("alertdialog", {
    name: "Revoke the 7 machines old-prod enrolled?",
  });
  const revoke = question.getByRole("button", { name: "Revoke machines" });
  await expect.element(revoke).toBeDisabled();
  await question.getByRole("textbox", { name: "Type old-prod to confirm" }).fill("old-prod");
  await revoke.click();
  await expect
    .poll(() => asked.find((one) => one.route === "POST /api/fleet/keys/4/revoke")?.body)
    .toEqual({ machines: true });
});

it("lists a key's machines from its count, and the key chip names and clears the filter", async () => {
  const asked = services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/keys": [200, [key(9, "prod-eks")]],
    "GET /api/fleet/machines": [
      200,
      { machines: [machine(81723, "db-03")], next: null, newest_version: null },
    ],
  });
  mount("/#machines/keys");

  await machines().getByRole("button", { name: "100", exact: true }).click();
  await expect.poll(() => fragment()).toBe("machines/list?state=live&key=9");
  await machines().getByRole("tab", { name: "Enrollment keys" }).click();
  await machines().getByRole("button", { name: "2,314" }).click();
  await expect.poll(() => fragment()).toBe("machines/list?state=any&key=9");
  await expect
    .poll(() => asked.findLast((one) => one.route === "GET /api/fleet/machines")?.url.search)
    .toBe("?state=any&key=9");
  const chip = machines().getByRole("combobox", { name: /Key/u });
  await expect.element(chip).toHaveDisplayValue("prod-eks");
  await chip.selectOptions("any");
  await expect.poll(() => fragment()).toBe("machines/list?state=any");
});

it("makes a key, shows the install with it once, and lists machines as they arrive", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
  let arrived = false;
  const asked = services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/keys": [200, []],
    "POST /api/fleet/keys": [
      201,
      {
        ...key(12, "prod", { live: 0, uses: 0 }),
        secret: "mse_x",
        enroll_url: "https://mse_x@microscope.test",
      },
    ],
    "GET /api/fleet/keys/12/machines": () => [
      200,
      arrived
        ? [
            {
              id: 5,
              hostname: "ip-10-0-7-12",
              primary_ip: "10.0.7.12",
              cluster: "checkout-us2",
              created_at: NOW,
            },
          ]
        : [],
    ],
  });
  mount("/#machines/add");

  const add = page.getByRole("dialog", { name: "Add machines" });
  await add.getByRole("textbox", { name: "Environment" }).fill("prod");
  await add.getByRole("textbox", { name: /Labels/u }).fill("team=payments");
  await add.getByRole("button", { name: "Create key" }).click();

  await expect
    .poll(() => asked.find((one) => one.route === "POST /api/fleet/keys")?.body)
    .toEqual({
      org_id: 3,
      name: "prod",
      environment: "prod",
      labels: { team: "payments" },
    });
  await expect
    .poll(() => text(add.getByRole("tabpanel", { name: "Kubernetes" })))
    .toContain("--from-literal=enroll='https://mse_x@microscope.test'");
  await expect.element(add.getByText(/only time you can see it/u)).toBeVisible();
  await expect.element(add.getByText(/^Waiting for the first machine/u)).toBeVisible();

  await add.getByRole("tab", { name: "Linux (systemd)" }).click();
  await expect
    .poll(() => text(add.getByRole("tabpanel", { name: "Linux (systemd)" })))
    .toContain('enroll = "https://mse_x@microscope.test"');

  arrived = true;
  vi.advanceTimersByTime(3000);
  await expect.element(add.getByText("ip-10-0-7-12")).toBeVisible();
  vi.useRealTimers();
});

it("names a bad label before asking the server", async () => {
  const asked = services({ "GET /api/fleet": [200, overview()], "GET /api/fleet/keys": [200, []] });
  mount("/#machines/add");
  const add = page.getByRole("dialog", { name: "Add machines" });
  await add.getByRole("textbox", { name: "Environment" }).fill("prod");
  await add.getByRole("textbox", { name: /Labels/u }).fill("team");
  await add.getByRole("button", { name: "Create key" }).click();
  await expect
    .element(add.getByRole("alert"))
    .toHaveTextContent("Labels: write team as name=value.");
  expect(asked.some((one) => one.route === "POST /api/fleet/keys")).toBe(false);
});

it("installs with an existing key, asking only which one and leaving its URL to fill in", async () => {
  const asked = services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/keys": [200, [key(9, "prod-eks"), key(4, "staging")]],
    "GET /api/fleet/keys/4/machines": [200, []],
  });
  mount("/#machines/add");
  const add = page.getByRole("dialog", { name: "Add machines" });
  await add.getByRole("radio", { name: "Use an existing key" }).click();
  expect(add.getByRole("textbox", { name: "Environment" }).query()).toBeNull();
  await add.getByRole("combobox", { name: "Existing key" }).selectOptions("staging");
  await add.getByRole("button", { name: "Use this key" }).click();

  await expect.element(add.getByText(/^Replace ENROLL_URL with the enroll URL/u)).toBeVisible();
  await expect
    .poll(() => text(add.getByRole("tabpanel", { name: "Kubernetes" })))
    .toContain("--from-literal=enroll='ENROLL_URL'");
  expect(add.getByText(/only time you can see it/u).query()).toBeNull();
  expect(asked.some((one) => one.route === "POST /api/fleet/keys")).toBe(false);

  await add.getByRole("button", { name: "Use a different key" }).click();
  await expect.element(add.getByRole("button", { name: "Create key" })).toBeVisible();
});

it("starts a new enrollment key fresh after choosing a key on Add machines", async () => {
  services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/keys": [200, [key(4, "staging")]],
    "GET /api/fleet/keys/4/machines": [200, []],
  });
  mount("/#machines/add");
  const add = page.getByRole("dialog", { name: "Add machines" });
  await add.getByRole("radio", { name: "Use an existing key" }).click();
  await add.getByRole("button", { name: "Use this key" }).click();
  await expect.element(add.getByRole("heading", { name: "Install the agent" })).toBeVisible();

  setLook({ view: "newkey" });
  const fresh = page.getByRole("dialog", { name: "New enrollment key" });
  await expect.element(fresh.getByRole("button", { name: "Create key" })).toBeVisible();
  expect(fresh.getByText(/^Created the key/u).query()).toBeNull();
});

it("shows members everything, with the owners' buttons greyed and saying why", async () => {
  const asked = services({
    "GET /api/fleet": [200, overview({ role: "member" })],
    "GET /api/fleet/machines": [
      200,
      { machines: [machine(81723, "db-03")], next: null, newest_version: null },
    ],
    "GET /api/fleet/machines/81723": [200, detail()],
  });
  mount("/#machines/m/81723");

  for (const name of ["Add machines", "Remove", "Revoke"]) {
    const button = machines().getByRole("button", { name, exact: true });
    await expect.element(button).toHaveAttribute("aria-disabled", "true");
    await expect.element(button).toHaveAttribute("title", "Only owners can do this");
  }
  await machines().getByRole("button", { name: "Remove" }).click({ force: true });
  expect(page.getByRole("alertdialog").query()).toBeNull();
  expect(asked.some((one) => one.route.startsWith("DELETE"))).toBe(false);
});

it("closes when its person signs out, and its menu greys out with the reason", async () => {
  services({ "GET /api/fleet": [200, overview()] });
  mount();
  openMachines({});
  await expect.element(machines()).toBeVisible();
  expect(COMMANDS.get("machines").available({} as never)).toEqual({ state: "ready" });

  account.setState({ now: { kind: "out" } });
  await expect.poll(() => machines().query()).toBeNull();
  expect(COMMANDS.get("addMachines").available({} as never)).toEqual({
    state: "blocked",
    why: "Sign in to see your machines",
  });
});

it("opens a pasted link after sign-in, and Back closes it", async () => {
  services({
    "GET /api/fleet": [200, overview()],
    "GET /api/fleet/keys": [200, [key(9, "prod-eks")]],
  });
  account.setState({ now: { kind: "out" } });
  mount("/#machines/keys");
  expect(page.getByRole("dialog").query()).toBeNull();

  account.setState({ now: { kind: "in", user: ME } });
  await expect
    .element(machines().getByRole("tab", { name: "Enrollment keys" }))
    .toHaveAttribute("aria-selected", "true");
  await machines().getByRole("tab", { name: "Overview" }).click();
  await expect.poll(() => fragment()).toBe("machines");
  await userEvent.keyboard("{Escape}");
  await expect.poll(() => machines().query()).toBeNull();
  expect(fragment()).toBe("");
});

it("takes one history entry when opened, so Back closes it and keeps the window out of other moves", async () => {
  services({ "GET /api/fleet": [200, overview()] });
  mount("/");
  openMachines({});
  await expect.element(machines()).toBeVisible();
  await expect.poll(() => fragment()).toBe("machines");

  history?.replace("/?look=1");
  await expect.poll(() => fragment()).toBe("machines");
  expect(history?.location.search).toBe("?look=1");

  history?.back();
  await expect.poll(() => machines().query()).toBeNull();
  expect(fragment()).toBe("");
});
