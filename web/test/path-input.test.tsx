// Completing a path while the server's answers arrive late.

import { cleanup, render } from "@testing-library/react";
import { useState } from "react";
import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import type { PathEntry } from "../src/protocol";
import { SessionContext } from "../src/store";
import { PathInput } from "../src/ui/Picker";
import { type FakeServer, fakeServer } from "./fake-server";

function Field() {
  const [value, setValue] = useState("");
  return (
    <>
      <PathInput id="program" value={value} onChange={setValue} autoFocus />
      <button type="button">Next</button>
    </>
  );
}

async function mount(): Promise<FakeServer> {
  const server = await fakeServer();
  render(
    <SessionContext value={server.session}>
      <Field />
    </SessionContext>,
  );
  return server;
}

afterEach(cleanup);

const field = () => page.getByRole("combobox");
const entries = (...entries: PathEntry[]) => ({ entries });
const next = () => page.getByRole("button", { name: "Next" });

/** The request completing `text`, skipping those for older text. */
async function asked(server: FakeServer, text: string) {
  let request = await server.next("completePath");
  while ((request.params as { text: string }).text !== text) {
    request = await server.next("completePath");
  }
  return request;
}

it("never completes typed text with suggestions for older text", async () => {
  const server = await mount();
  server.answer(await server.next("completePath"), entries({ text: "build/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /build/ })).toBeVisible();

  await userEvent.type(field(), "build/test-p");
  await userEvent.keyboard("{Tab}");
  // The answer for the empty field is stale; Tab waits for the current one.
  let asked = await server.next("completePath");
  while ((asked.params as { text: string }).text !== "build/test-p") {
    asked = await server.next("completePath");
  }
  await expect.element(field()).toHaveValue("build/test-p");
  server.answer(asked, entries({ text: "build/test-programs/", kind: "directory" }));
  await expect.element(field()).toHaveValue("build/test-programs/");
});

it("chooses with the arrow keys and Enter, and a file closes the list", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  let asked = await server.next("completePath");
  while ((asked.params as { text: string }).text !== "b") {
    asked = await server.next("completePath");
  }
  server.answer(
    asked,
    entries({ text: "bin/", kind: "directory" }, { text: "basic", kind: "executable" }),
  );
  await expect.element(page.getByRole("option", { name: /basic/ })).toBeVisible();
  await userEvent.keyboard("{ArrowDown}{Enter}");
  await expect.element(field()).toHaveValue("basic");
  await expect.element(page.getByRole("listbox")).not.toBeInTheDocument();
});

it("lets Tab leave the field when completing fails", async () => {
  const server = await mount();
  await userEvent.type(field(), "src");
  server.send({
    type: "error",
    id: (await asked(server, "src")).id,
    error: { kind: "failed", message: "no such directory" },
  });
  await userEvent.keyboard("{Tab}");
  await expect.element(next()).toHaveFocus();
  await expect.element(field()).toHaveValue("src");
});

it("passes Tab through an empty field and stops descending at a directory", async () => {
  const server = await mount();
  server.answer(await asked(server, ""), entries({ text: "build/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /build/ })).toBeVisible();
  await userEvent.keyboard("{Tab}");
  await expect.element(next()).toHaveFocus();
  await expect.element(field()).toHaveValue("");

  await userEvent.click(field());
  await userEvent.type(field(), "b");
  server.answer(await asked(server, "b"), entries({ text: "build/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /build/ })).toBeVisible();
  await userEvent.keyboard("{Tab}");
  await expect.element(field()).toHaveValue("build/");
  server.answer(await asked(server, "build/"), entries({ text: "build/web/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /web/ })).toBeVisible();
  await userEvent.keyboard("{Tab}");
  await expect.element(next()).toHaveFocus();
  await expect.element(field()).toHaveValue("build/");

  // An entry the arrow keys picked is taken, directory or not.
  await userEvent.click(field());
  server.answer(await asked(server, "build/"), entries({ text: "build/web/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /web/ })).toBeVisible();
  await userEvent.keyboard("{ArrowDown}{Tab}");
  await expect.element(field()).toHaveValue("build/web/");
});
