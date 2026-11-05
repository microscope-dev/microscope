// What every address opens: the one workbench, as `microscope web` starts
// it and as a page served on its own does, and the panes that start
// something from the File menu.

import { spawn } from "node:child_process";

import type { Page } from "@playwright/test";

import { expect, expectErrors, fixture, join, menu, test } from "./server";
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
