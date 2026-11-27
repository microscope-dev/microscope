import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";

import { Share } from "../src/ui/Share";
import { handled } from "./fake-server";
import { mountPane, unmountPane } from "./pane";

afterEach(unmountPane);

it("keeps the link for the access chosen last", async () => {
  const server = await mountPane(() => <Share />);
  await userEvent.click(page.getByRole("button", { name: "Share" }));
  await expect.element(page.getByText("Making a link…")).toBeVisible();
  const viewing = await server.next("share");
  await userEvent.click(page.getByRole("button", { name: "Can control" }));
  const controlling = await server.next("share");

  server.answer(controlling, { url: "http://x/join?control" });
  await expect
    .element(page.getByRole("textbox", { name: "Join link" }))
    .toHaveValue("http://x/join?control");
  server.answer(viewing, { url: "http://x/join?view" });

  await handled();
  expect(page.getByRole("textbox", { name: "Join link" }).element()).toHaveValue(
    "http://x/join?control",
  );
});

it("shows a failure in place of the link it was making", async () => {
  const server = await mountPane(() => <Share />);
  await userEvent.click(page.getByRole("button", { name: "Share" }));
  await expect.element(page.getByText("Making a link…")).toBeVisible();
  server.send({
    type: "error",
    id: (await server.next("share")).id,
    error: { kind: "forbidden", message: "this link cannot share" },
  });
  await expect.element(page.getByText("this link cannot share")).toBeVisible();
  await expect.element(page.getByText("Making a link…")).not.toBeInTheDocument();
});

it("uploads an open profile capture when signed in to Microscope services", async () => {
  const server = await mountPane(() => <Share />);
  server.send({
    type: "profiles",
    profiles: [
      {
        id: "p0",
        name: "workstation.usprof",
        revision: 1,
        recording: false,
        ended: null,
      },
    ],
  });

  await userEvent.click(page.getByRole("button", { name: "Share" }));
  await expect.element(page.getByRole("tab", { name: "Upload capture" })).toBeVisible();
  await expect.element(page.getByRole("tab", { name: "Live link" })).toBeVisible();
  await userEvent.click(page.getByRole("tab", { name: "Upload capture" }));

  const statusReq = await server.next("cloudStatus");
  server.answer(statusReq, {
    loggedIn: true,
    server: "https://microscope.example",
    defaultServer: "https://microscope.dev",
    username: "alice",
    email: "alice@example.com",
    projects: [{ id: 17, name: "checkout", orgId: 4, orgName: "Acme" }],
    error: null,
  });

  await expect.element(page.getByText("alice")).toBeVisible();
  await userEvent.click(page.getByRole("button", { name: "Upload & get link" }));

  const uploadReq = await server.next("shareProfile");
  expect(uploadReq.params).toEqual({
    profile: "p0",
    projectId: 17,
    name: "workstation.usprof",
  });
  server.answer(uploadReq, {
    url: "https://microscope.example/s/99",
    sessionId: 99,
    captureId: 12,
    projectName: "checkout",
    byteLen: 4096,
    redactedCount: 3,
  });

  await expect
    .element(page.getByRole("textbox", { name: "Shared capture link" }))
    .toHaveValue("https://microscope.example/s/99");
  await expect.element(page.getByText(/3 redacted/u)).toBeVisible();
});
