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
      <PathInput id="program" value={value} onChange={setValue} />
      {/* Tab skips these, so the field moving focus on by itself must too. */}
      <button type="button" disabled>
        Off
      </button>
      <span tabIndex={-1}>Not in the tab order</span>
      <input type="hidden" />
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
  await userEvent.click(field());
  return server;
}

afterEach(cleanup);

const field = () => page.getByRole("combobox");
const entries = (...list: PathEntry[]) => ({ entries: list });
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
  await expect.element(page.getByRole("option", { name: /build/u })).toBeVisible();

  await userEvent.type(field(), "build/test-p");
  await userEvent.keyboard("{Tab}");
  // The answer for the empty field is stale; Tab waits for the current one.
  const request = await asked(server, "build/test-p");
  await expect.element(field()).toHaveValue("build/test-p");
  server.answer(request, entries({ text: "build/test-programs/", kind: "directory" }));
  await expect.element(field()).toHaveValue("build/test-programs/");
});

it("chooses with the arrow keys and Enter, and a file closes the list", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  const request = await asked(server, "b");
  server.answer(
    request,
    entries({ text: "bin/", kind: "directory" }, { text: "basic", kind: "executable" }),
  );
  await expect.element(page.getByRole("option", { name: /basic/u })).toBeVisible();
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

it("moves a waiting Tab on when its text has no suggestions", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  const early = await asked(server, "b");
  await userEvent.keyboard("{Tab}");
  await expect.element(field()).toHaveFocus();
  server.answer(early, entries());
  await expect.element(next()).toHaveFocus();
  await expect.element(field()).toHaveValue("b");
});

it("moves a waiting Tab on when completing fails", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  const early = await asked(server, "b");
  await userEvent.keyboard("{Tab}");
  await expect.element(field()).toHaveFocus();
  server.refuse(early, "failed", "no such directory");
  await expect.element(next()).toHaveFocus();
  await expect.element(field()).toHaveValue("b");
});

it("passes Tab through an empty field and stops descending at a directory", async () => {
  const server = await mount();
  server.answer(await asked(server, ""), entries({ text: "build/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /build/u })).toBeVisible();
  await userEvent.keyboard("{Tab}");
  await expect.element(next()).toHaveFocus();
  await expect.element(field()).toHaveValue("");

  await userEvent.click(field());
  await userEvent.type(field(), "b");
  server.answer(await asked(server, "b"), entries({ text: "build/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /build/u })).toBeVisible();
  await userEvent.keyboard("{Tab}");
  await expect.element(field()).toHaveValue("build/");
  server.answer(await asked(server, "build/"), entries({ text: "build/web/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /web/u })).toBeVisible();
  await userEvent.keyboard("{Tab}");
  await expect.element(next()).toHaveFocus();
  await expect.element(field()).toHaveValue("build/");

  // An entry the arrow keys picked is taken, directory or not.
  await userEvent.click(field());
  server.answer(await asked(server, "build/"), entries({ text: "build/web/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /web/u })).toBeVisible();
  await userEvent.keyboard("{ArrowDown}{Tab}");
  await expect.element(field()).toHaveValue("build/web/");
});

it("forgets a waiting Tab when the field loses focus", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  const early = await asked(server, "b");
  await userEvent.keyboard("{Tab}");
  await expect.element(field()).toHaveFocus();

  await userEvent.click(next());
  server.answer(early, entries({ text: "build/", kind: "directory" }));
  await userEvent.click(field());
  server.answer(await asked(server, "b"), entries({ text: "build/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /build/u })).toBeVisible();
  await expect.element(field()).toHaveValue("b");
});

it("forgets a waiting Tab on Escape", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  const early = await asked(server, "b");
  await userEvent.keyboard("{Tab}{Escape}");
  server.answer(early, entries({ text: "build/", kind: "directory" }));

  await userEvent.click(next());
  await userEvent.click(field());
  server.answer(await asked(server, "b"), entries({ text: "build/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /build/u })).toBeVisible();
  await expect.element(field()).toHaveValue("b");
});

it("ignores the answer Escape closed on, and lets the next Tab leave", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  const early = await asked(server, "b");
  await userEvent.keyboard("{Tab}{Escape}");
  server.answer(early, entries({ text: "build/", kind: "directory" }));
  await userEvent.keyboard("{Tab}");
  await expect.element(next()).toHaveFocus();
  await expect.element(field()).toHaveValue("b");
});

it("drops a waiting Tab when the text changes before its answer", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  const early = await asked(server, "b");
  await userEvent.keyboard("{Tab}");
  await userEvent.type(field(), "u");
  server.answer(early, entries({ text: "build/", kind: "directory" }));
  server.answer(await asked(server, "bu"), entries({ text: "bus/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /bus/u })).toBeVisible();
  await expect.element(field()).toHaveValue("bu");
});

it("keeps an arrow choice when the same text's suggestions come again", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  server.answer(
    await asked(server, "b"),
    entries({ text: "bin/", kind: "directory" }, { text: "basic", kind: "executable" }),
  );
  await expect.element(page.getByRole("option", { name: /basic/u })).toBeVisible();

  // Coming back shows the old suggestions while new ones are fetched.
  await userEvent.click(next());
  await userEvent.click(field());
  await expect.element(page.getByRole("option", { name: /basic/u })).toBeVisible();
  await userEvent.keyboard("{ArrowDown}");
  await expect
    .element(page.getByRole("option", { name: /basic/u }))
    .toHaveAttribute("aria-selected", "true");
  server.answer(
    await asked(server, "b"),
    entries(
      { text: "bin/", kind: "directory" },
      { text: "basic", kind: "executable" },
      { text: "bump", kind: "executable" },
    ),
  );
  await expect.element(page.getByRole("option", { name: /bump/u })).toBeVisible();
  await userEvent.keyboard("{Enter}");
  await expect.element(field()).toHaveValue("basic");
});

it("lets Tab take an arrow choice after a directory when its suggestions come again", async () => {
  const server = await mount();
  await userEvent.type(field(), "build/");
  server.answer(await asked(server, "build/"), entries({ text: "build/web/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /web/u })).toBeVisible();

  await userEvent.click(next());
  await userEvent.click(field());
  await userEvent.keyboard("{ArrowDown}");
  server.answer(
    await asked(server, "build/"),
    entries({ text: "build/web/", kind: "directory" }, { text: "build/wasm/", kind: "directory" }),
  );
  await expect.element(page.getByRole("option", { name: /wasm/u })).toBeVisible();
  await userEvent.keyboard("{Tab}");
  await expect.element(field()).toHaveValue("build/web/");
});

it("moves an arrow choice onto the last suggestion when fewer come again", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  server.answer(
    await asked(server, "b"),
    entries({ text: "bin/", kind: "directory" }, { text: "basic", kind: "executable" }),
  );
  await userEvent.keyboard("{ArrowDown}");
  await expect
    .element(page.getByRole("option", { name: /basic/u }))
    .toHaveAttribute("aria-selected", "true");

  await userEvent.click(next());
  await userEvent.click(field());
  server.answer(await asked(server, "b"), entries({ text: "bin/", kind: "directory" }));
  await expect.element(page.getByRole("option", { name: /basic/u })).not.toBeInTheDocument();
  await expect
    .element(page.getByRole("option", { name: /bin/u }))
    .toHaveAttribute("aria-selected", "true");
  await userEvent.keyboard("{Enter}");
  await expect.element(field()).toHaveValue("bin/");
});

it("starts the suggestions for newly typed text at the first", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  server.answer(
    await asked(server, "b"),
    entries({ text: "bin/", kind: "directory" }, { text: "basic", kind: "executable" }),
  );
  await userEvent.keyboard("{ArrowDown}");
  await expect
    .element(page.getByRole("option", { name: /basic/u }))
    .toHaveAttribute("aria-selected", "true");

  await userEvent.type(field(), "a");
  server.answer(
    await asked(server, "ba"),
    entries({ text: "bar/", kind: "directory" }, { text: "basic", kind: "executable" }),
  );
  await expect
    .element(page.getByRole("option", { name: /bar/u }))
    .toHaveAttribute("aria-selected", "true");
});

it("starts a chosen directory's suggestions at the first", async () => {
  const server = await mount();
  await userEvent.type(field(), "b");
  server.answer(
    await asked(server, "b"),
    entries({ text: "bin/", kind: "directory" }, { text: "build/", kind: "directory" }),
  );
  await userEvent.keyboard("{ArrowDown}{Enter}");
  await expect.element(field()).toHaveValue("build/");
  server.answer(
    await asked(server, "build/"),
    entries({ text: "build/app/", kind: "directory" }, { text: "build/web/", kind: "directory" }),
  );
  await expect
    .element(page.getByRole("option", { name: /app/u }))
    .toHaveAttribute("aria-selected", "true");
});
