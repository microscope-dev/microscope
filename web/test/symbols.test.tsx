import { afterEach, describe, expect, it, vi } from "vitest";
import { page } from "vitest/browser";

import { account } from "../src/account";
import { AppRouter, createMemoryHistory } from "../src/router";
import { SessionContext } from "../src/store";
import { SymbolsView } from "../src/ui/data/SymbolsView";
import { fakeServer } from "./fake-server";
import { cleanup, render } from "./render";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  account.setState({ now: { kind: "checking" }, notice: null, challenge: null });
});

describe("SymbolsView", () => {
  it("renders missing banner, filters builds, shows upload commands, and creates tokens", async () => {
    const responseBody = {
      orgId: 7,
      isOwner: true,
      hours: 24,
      serverUrl: "http://127.0.0.1:3000",
      projects: [{ id: 11, name: "payments", canWrite: true }],
      missingSummary: { missingBuildCount: 1, missingTimePercent: 72.5 },
      builds: [
        {
          buildId: "abcdef0123456789abcdef0123456789abcdef01",
          projectId: 11,
          projectName: "payments",
          serviceName: "payments-api",
          modulePath: "/srv/payments/api",
          gitCommit: null,
          status: "missing",
          sharePercent: 72.5,
          unsymbolizedSharePercent: 72.5,
          byteLen: null,
          hasDwarf: false,
          hasSymtab: false,
          symbolCount: 0,
          lastSeenAt: 1_700_000_000_000,
          uploadedAt: null,
          uploadCommand:
            "MICROSCOPE_SYMBOL_TOKEN=<token> microscope symbols upload --server http://127.0.0.1:3000 --service payments-api api",
        },
        {
          buildId: "1111111111111111111111111111111111111111",
          projectId: 11,
          projectName: "payments",
          serviceName: "worker",
          modulePath: "/srv/payments/worker",
          gitCommit: "deadbeef1234",
          status: "uploaded_ci",
          sharePercent: 27.5,
          unsymbolizedSharePercent: 0,
          byteLen: 2_097_152,
          hasDwarf: true,
          hasSymtab: true,
          symbolCount: 420,
          lastSeenAt: 1_700_000_000_000,
          uploadedAt: 1_700_000_000_500,
          uploadCommand:
            "MICROSCOPE_SYMBOL_TOKEN=<token> microscope symbols upload --server http://127.0.0.1:3000 --service worker worker",
        },
      ],
      tokens: [],
    };

    const fetchMock = vi.fn<typeof fetch>((input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      if (url.startsWith("/api/data/symbols/tokens") && init?.method === "POST") {
        return Promise.resolve(
          new Response(
            JSON.stringify({
              id: 99,
              projectId: 11,
              projectName: "payments",
              name: "ci-prod",
              token: "msp_secret_token_value",
              tokenPrefix: "msp_secret",
              createdAt: 1_700_000_001_000,
            }),
            { status: 201, headers: { "Content-Type": "application/json" } },
          ),
        );
      }
      if (url.startsWith("/api/data/symbols")) {
        return Promise.resolve(
          new Response(JSON.stringify(responseBody), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          }),
        );
      }
      return Promise.resolve(new Response(null, { status: 404 }));
    });
    vi.stubGlobal("fetch", fetchMock);

    const { container } = render(() => <SymbolsView org={7} />);

    await expect
      .poll(() => container.querySelector('[data-testid="symbols-missing-banner"]')?.textContent)
      .toContain("72.5%");
    expect(container.textContent).toContain("payments-api");
    expect(container.textContent).toContain("deadbeef1234");

    const showButton = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent === "Show",
    );
    showButton?.click();
    await expect
      .poll(() => container.querySelector('[data-testid="symbols-upload-cmd"]')?.textContent)
      .toContain("microscope symbols upload");

    await page.getByLabelText("Upload token name").fill("ci-prod");
    await page.getByRole("button", { name: "Create upload token" }).click();

    await expect
      .poll(() => container.querySelector('[data-testid="symbols-token-secret"]')?.textContent)
      .toBe("msp_secret_token_value");
    expect(container.querySelector('[data-testid="symbols-upload-cmd"]')?.textContent).toContain(
      "msp_secret_token_value",
    );
  });
});

describe("CliLogin route", () => {
  it("approves a confirmation code for a signed-in user", async () => {
    account.setState({
      now: {
        kind: "in",
        user: { id: 1, username: "dev", email: "dev@example.test", totp_enabled: false },
      },
      notice: null,
      challenge: null,
    });

    const approveCalls: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>((input, init) => {
        const url = typeof input === "string" ? input : input.toString();
        if (url === "/api/cli/login/approve" && init?.method === "POST") {
          approveCalls.push(String(init.body ?? ""));
          return Promise.resolve(
            new Response(JSON.stringify({ ok: true }), {
              status: 200,
              headers: { "Content-Type": "application/json" },
            }),
          );
        }
        return Promise.resolve(new Response(null, { status: 404 }));
      }),
    );

    const server = await fakeServer();
    const history = createMemoryHistory("/cli-login?code=ABCD-1234");
    const { container } = render(() => (
      <SessionContext value={server.session}>
        <AppRouter history={history} />
      </SessionContext>
    ));

    await expect
      .poll(() => container.querySelector('[data-testid="cli-login-approve-btn"]'))
      .not.toBeNull();
    const approveBtn = container.querySelector<HTMLButtonElement>(
      '[data-testid="cli-login-approve-btn"]',
    );
    approveBtn?.click();

    await expect
      .poll(() => container.querySelector('[data-testid="cli-login-approved"]')?.textContent)
      .toContain("CLI sign-in approved");
    expect(approveCalls).toEqual([JSON.stringify({ user_code: "ABCD-1234" })]);
  });
});
