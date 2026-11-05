// What every address opens: the one workbench, as `microscope web` starts
// it and as a page served on its own does, and the panes that start
// something from the File menu.

import { spawn } from "node:child_process";
import { createServer } from "node:net";

import type { Page } from "@playwright/test";

import { allowErrors, expect, expectErrors, fixture, join, menu, test } from "./server";
import { type Handler, standalonePage } from "./services";

const tab = (page: Page, name: string) => page.getByRole("tab", { name, exact: true });

test.describe("with nothing to debug", () => {
  test.use({ program: [] });

  test("microscope web starts at Account, needs no account, and File launches a program", async ({
    page,
    microscope,
  }) => {
    await join(page, microscope.link);
    await expect(tab(page, "Account")).toHaveAttribute("aria-selected", "true");

    await menu(page, "File", "Launch a program…");
    const launch = page.getByRole("tabpanel", { name: "Launch" });
    await launch.getByRole("combobox", { name: "Program" }).fill(fixture("spin"));
    await page.keyboard.press("Escape");
    await launch.getByRole("button", { name: "Load", exact: true }).click();

    // The pane closes, and the session opens in the Debug layout.
    await expect(page).toHaveURL(/\/s\/[0-9a-f]+$/u);
    await expect(page.getByTestId("status")).toContainText("Not started");
    await expect(tab(page, "spin.c")).toBeVisible();
    await expect(tab(page, "Launch")).toHaveCount(0);
  });

  test("File attaches to a running process", async ({ page, microscope }) => {
    const program = spawn(fixture("spin"), [], { stdio: "ignore" });
    try {
      await join(page, microscope.link);
      await menu(page, "File", "Attach to a process…");
      const attach = page.getByRole("tabpanel", { name: "Attach" });
      await attach.getByRole("textbox", { name: "Process" }).fill(String(program.pid));
      await attach.getByRole("button", { name: `Attach to ${program.pid}` }).click();
      // Attaching stops the process where it was.
      await expect(page).toHaveURL(/\/s\/[0-9a-f]+\/stop\//u);
      await expect(page).toHaveTitle(/^spin/u);
      await expect(tab(page, "Attach")).toHaveCount(0);
    } finally {
      program.kill();
    }
  });
});

/** A stand-in for the services backend without email, where a password
 * alone signs in. */
function standIn(): Handler {
  let signedIn = false;
  const dev = { id: 1, username: "dev", email: "dev@microscope.dev" };
  return (request, response) => {
    const json = (status: number, body: unknown) => {
      response.writeHead(status, { "Content-Type": "application/json" });
      response.end(JSON.stringify(body));
    };
    if (request.url === "/api/users/me") {
      json(signedIn ? 200 : 401, signedIn ? dev : { error: "not signed in" });
      return;
    }
    if (request.url === "/api/features") {
      const none = { google: false, github: false, saml: false };
      json(200, { email: false, totp: false, sso: none, passkeys: false });
      return;
    }
    // A new account: in no organization, with no projects, invitations, or
    // outside sign-ins.
    const empty: Record<string, unknown> = {
      "/api/orgs": [],
      "/api/projects": [],
      "/api/users/me/invitations": [],
      "/api/users/me/identities": { has_password: true, identities: [], passkeys: [] },
    };
    if (signedIn && request.url && request.url in empty) {
      json(200, empty[request.url]);
      return;
    }
    if (request.url === "/api/login") {
      signedIn = true;
      json(200, dev);
      return;
    }
    json(404, { error: "no such route" });
  };
}

test("a page served on its own signs in, then connects with a join link", async ({
  page,
  microscope,
}) => {
  const served = await standalonePage(standIn());
  // Services says nobody is signed in yet.
  expectErrors(/\/api\/users\/me: 401$/u);
  try {
    await page.goto(`${served.origin}/`);
    await expect(tab(page, "Account")).toHaveAttribute("aria-selected", "true");
    // Nothing to connect to yet, so Connect says how.
    await expect(tab(page, "Connect")).toHaveAttribute("aria-selected", "true");

    const account = page.getByRole("tabpanel", { name: "Account" });
    await account.getByLabel("Username or email").fill("dev");
    await account.getByLabel("Password").fill("microscope");
    await account.getByRole("button", { name: "Sign in" }).click();
    await expect(account).toContainText("Signed in as dev (dev@microscope.dev)");
    await expect(page.getByRole("tabpanel", { name: "Account" })).toContainText("Welcome, dev");

    const connect = page.getByRole("tabpanel", { name: "Connect" });
    await connect.getByLabel("Join link").fill(microscope.link);
    await connect.getByRole("button", { name: "Connect" }).click();
    await expect(page).toHaveURL(new RegExp(`^${served.origin}/s/[0-9a-f]+$`, "u"));
    await expect(page.getByTestId("status")).toContainText("Not started");
  } finally {
    served.server.close();
  }
});

/** A port nothing listens on, as a stopped microscope leaves it. */
async function deadPort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((done) => {
    server.listen(0, "127.0.0.1", done);
  });
  const { port } = server.address() as { port: number };
  await new Promise((done) => {
    server.close(done);
  });
  return port;
}

const session = (url: string) => new URL(url).pathname.match(/^\/s\/([0-9a-f]+)/u)?.[1];

test("a page served on its own gets out of a microscope it can't reach", async ({
  page,
  microscope,
}) => {
  const served = await standalonePage(standIn());
  // Firefox reports a server that never answered as a failed CORS request.
  allowErrors(/\/api\/users\/me: 401$/u, /CORS request did not succeed/u);
  try {
    // The microscope this browser used last has stopped.
    const gone = `http://127.0.0.1:${await deadPort()}/`;
    await page.goto(`${served.origin}/`);
    await page.evaluate((server) => {
      localStorage.setItem("microscope-remote", JSON.stringify({ server, token: "c-gone" }));
    }, gone);
    await page.reload();

    // The page starts as it would with none, and Connect says why.
    await expect(tab(page, "Account")).toHaveAttribute("aria-selected", "true");
    await expect(tab(page, "Connect")).toHaveAttribute("aria-selected", "true");
    const connect = page.getByRole("tabpanel", { name: "Connect" });
    await expect(connect).toContainText(`Can't reach the microscope at ${gone}`);
    await expect(page.getByTestId("status")).toContainText("Can't reach microscope");

    await connect.getByRole("button", { name: "Disconnect" }).click();
    await expect(connect.getByLabel("Join link")).toBeVisible();
    await expect(connect).not.toContainText("Can't reach");
    expect(await page.evaluate(() => localStorage.getItem("microscope-remote"))).toBe("null");

    await connect.getByLabel("Join link").fill(microscope.link);
    await connect.getByRole("button", { name: "Connect", exact: true }).click();
    await expect(page).toHaveURL(new RegExp(`^${served.origin}/s/[0-9a-f]+$`, "u"));
    await expect(page.getByTestId("status")).toContainText("Not started");
  } finally {
    served.server.close();
  }
});

test("each tab of a page served on its own keeps its microscope, and swaps a lost one", async ({
  page,
  microscope,
}) => {
  const served = await standalonePage(standIn());
  allowErrors(/\/api\/users\/me: 401$/u, /CORS request did not succeed/u);
  const connectTo = async (on: typeof page, link: string) => {
    await menu(on, "File", "Connect…");
    const connect = on.getByRole("tabpanel", { name: "Connect" });
    await connect.getByLabel("Join link").fill(link);
    await connect.getByRole("button", { name: "Connect", exact: true }).click();
  };
  try {
    await page.goto(`${served.origin}/`);
    await connectTo(page, microscope.link);
    await expect(page.getByTestId("status")).toContainText("Not started");
    const first = session(page.url());
    expect(first).toBeDefined();

    // Another tab connects to another microscope.
    const other = await microscope.start([fixture("spin")]);
    const second = await page.context().newPage();
    await second.goto(`${served.origin}/`);
    await connectTo(second, other.link);
    await expect(second.getByTestId("status")).toContainText("Not started");
    const secondSession = session(second.url());
    expect(secondSession).toBeDefined();
    expect(secondSession).not.toBe(first);

    // The first tab stays with its own, reloaded or not.
    await page.reload();
    await expect(page.getByTestId("status")).toContainText("Not started");
    expect(session(page.url())).toBe(first);
    await menu(page, "View", "Explorer");
    await expect(page.getByRole("tabpanel", { name: "Explorer" })).toContainText(
      `${microscope.origin}/`,
    );

    // The second's microscope stops: it says so, and connects elsewhere.
    await other.stop();
    await expect(second.getByTestId("status")).toContainText("Reconnecting");
    await menu(second, "File", "Connect…");
    await expect(second.getByRole("tabpanel", { name: "Connect" })).toContainText(
      `reconnecting to ${other.origin}/`,
    );
    await connectTo(second, microscope.link);
    await expect(second.getByTestId("status")).toContainText("Not started");
    expect(session(second.url())).toBe(first);
  } finally {
    served.server.close();
  }
});

test("a page served on its own sets a session's layout aside while it has none", async ({
  page,
  microscope,
}) => {
  const served = await standalonePage(standIn());
  allowErrors(/\/api\/users\/me: 401$/u, /CORS request did not succeed/u);
  const connectTo = async (link: string) => {
    const connect = page.getByRole("tabpanel", { name: "Connect" });
    await connect.getByLabel("Join link").fill(link);
    await connect.getByRole("button", { name: "Connect", exact: true }).click();
    await expect(page.getByTestId("status")).toContainText("Not started");
  };
  const registers = () => page.getByRole("tab", { name: /^Registers/u });
  try {
    await page.goto(`${served.origin}/`);
    await connectTo(microscope.link);
    // Arranged the person's own way: Registers alone.
    await registers().click();
    await page.keyboard.press("Alt+Enter");
    await expect(tab(page, "Threads")).toBeHidden();

    // The microscope stops, and the page is opened again with nothing to
    // debug: it starts from Account, with Connect in front saying why.
    await microscope.stop();
    await page.goto(`${served.origin}/`);
    await expect(tab(page, "Account")).toHaveAttribute("aria-selected", "true");
    await expect(tab(page, "Connect")).toHaveAttribute("aria-selected", "true");
    await expect(page.getByRole("tabpanel", { name: "Connect" })).toContainText("Can't reach");
    await expect(registers()).toHaveCount(0);

    // The next session comes back to the arrangement it had.
    const next = await microscope.start([fixture("spin")]);
    await connectTo(next.link);
    await expect(registers()).toHaveAttribute("aria-selected", "true");
    await expect(tab(page, "Threads")).toBeHidden();
  } finally {
    served.server.close();
  }
});
