// What every address opens: the one workbench, as `microscope web` starts
// it and as a page served on its own does, and the panes that start
// something from the File menu.

import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import * as path from "node:path";
import type { Page } from "@playwright/test";
import { expect, expectErrors, fixture, join, menu, root, test } from "./server";

const tab = (page: Page, name: string) => page.getByRole("tab", { name, exact: true });

test.describe("with nothing to debug", () => {
  test.use({ program: [] });

  test("microscope web starts at Welcome, needs no account, and File launches a program", async ({
    page,
    microscope,
  }) => {
    await join(page, microscope.link);
    await expect(tab(page, "Welcome")).toHaveAttribute("aria-selected", "true");
    await expect(tab(page, "Account")).toHaveCount(0);

    await menu(page, "File", "Launch a program…");
    const launch = page.getByRole("tabpanel", { name: "Launch" });
    await launch.getByRole("combobox", { name: "Program" }).fill(fixture("spin"));
    await page.keyboard.press("Escape");
    await launch.getByRole("button", { name: "Load", exact: true }).click();

    // The pane closes, and the session opens in the Debug layout.
    await expect(page).toHaveURL(/\/s\/[0-9a-f]+$/);
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
      await expect(page).toHaveURL(/\/s\/[0-9a-f]+\/stop\//);
      await expect(page).toHaveTitle(/^spin/);
      await expect(tab(page, "Attach")).toHaveCount(0);
    } finally {
      program.kill();
    }
  });
});

/** Serves the built page on its own, as `just web` does, with a stand-in
 * for the services backend's accounts under `/api/`. */
async function standalonePage(): Promise<{ origin: string; server: Server }> {
  const dir = path.join(root, "build", "web");
  let signedIn = false;
  const dev = { id: 1, username: "dev", email: "dev@microscope.dev" };
  const server = createServer((request, response) => {
    const url = new URL(request.url ?? "/", "http://page");
    const json = (status: number, body: unknown) => {
      response.writeHead(status, { "Content-Type": "application/json" });
      response.end(JSON.stringify(body));
    };
    if (url.pathname === "/api/users/me") {
      json(signedIn ? 200 : 401, signedIn ? dev : { error: "not signed in" });
      return;
    }
    if (url.pathname === "/api/login") {
      signedIn = true;
      json(200, dev);
      return;
    }
    // Assets by name, and the page for every other address.
    const file = url.pathname.startsWith("/assets/") ? url.pathname : "/index.html";
    readFile(path.join(dir, path.normalize(file)))
      .then((body) => {
        response.writeHead(200, {
          "Content-Type": file.endsWith(".js")
            ? "text/javascript"
            : file.endsWith(".css")
              ? "text/css"
              : "text/html",
        });
        response.end(body);
      })
      .catch(() => {
        response.writeHead(404);
        response.end();
      });
  });
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const address = server.address();
  if (address === null || typeof address === "string") {
    throw new Error("the page's server has no port");
  }
  return { origin: `http://127.0.0.1:${address.port}`, server };
}

test("a page served on its own signs in, then connects with a join link", async ({
  page,
  microscope,
}) => {
  const served = await standalonePage();
  // Services says nobody is signed in yet.
  expectErrors(/\/api\/users\/me: 401$/);
  try {
    await page.goto(`${served.origin}/`);
    await expect(tab(page, "Welcome")).toHaveAttribute("aria-selected", "true");
    // Nothing to connect to yet, so Connect says how.
    await expect(tab(page, "Connect")).toHaveAttribute("aria-selected", "true");

    const account = page.getByRole("tabpanel", { name: "Account" });
    await account.getByLabel("Username or email").fill("dev");
    await account.getByLabel("Password").fill("microscope");
    await account.getByRole("button", { name: "Sign in" }).click();
    await expect(account).toContainText("Signed in as dev (dev@microscope.dev)");
    await expect(page.getByRole("tabpanel", { name: "Welcome" })).toContainText("Welcome, dev");

    const connect = page.getByRole("tabpanel", { name: "Connect" });
    await connect.getByLabel("Join link").fill(microscope.link);
    await connect.getByRole("button", { name: "Connect" }).click();
    await expect(page).toHaveURL(new RegExp(`^${served.origin}/s/[0-9a-f]+$`));
    await expect(page.getByTestId("status")).toContainText("Not started");
  } finally {
    served.server.close();
  }
});
