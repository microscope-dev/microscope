import { type ChildProcess, spawn } from "node:child_process";
import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import {
  createServer,
  request as forward,
  type IncomingMessage,
  type Server,
  type ServerResponse,
} from "node:http";
import type { AddressInfo } from "node:net";
import * as path from "node:path";
import { createInterface } from "node:readline";

import { fromServices, toServices } from "../dev-cookies";
import { root } from "./server";

export type Handler = (request: IncomingMessage, response: ServerResponse) => void;

export interface Served {
  origin: string;
  server: Server;
}

async function listen(handler: Handler, host = "127.0.0.1"): Promise<Served> {
  const server = createServer(handler);
  await new Promise<void>((done) => {
    server.listen(0, "127.0.0.1", done);
  });
  const { port } = server.address() as AddressInfo;
  return { origin: `http://${host}:${port}`, server };
}

export function standalonePage(api: Handler, host?: string): Promise<Served> {
  const dir = path.join(root, "build", "web");
  return listen(async (request, response) => {
    const url = new URL(request.url ?? "/", "http://page");
    if (url.pathname.startsWith("/api/")) {
      api(request, response);
      return;
    }

    const file = url.pathname.startsWith("/assets/") ? url.pathname : "/index.html";
    try {
      const body = await readFile(path.join(dir, path.normalize(file)));
      response.writeHead(200, {
        "Content-Type": file.endsWith(".js")
          ? "text/javascript"
          : file.endsWith(".css")
            ? "text/css"
            : "text/html",
      });
      response.end(body);
    } catch {
      response.writeHead(404);
      response.end();
    }
  }, host);
}

export const servicesBinary =
  process.env.MICROSCOPE_SERVICES_BINARY ??
  path.join(root, "..", "services", "target", "debug", "microscope-services");

export const haveServices = existsSync(servicesBinary);

const mockIdpBinary = path.join(path.dirname(servicesBinary), "mock-idp");

export interface Mail {
  to: string;
  subject: string;

  paragraphs: string[];

  link: string;

  code: string | null;
}

export interface Services extends Served {
  mail(to: string): Promise<Mail>;

  idp: string;
  log: string[];
  stop(): Promise<void>;
}

function fakePostmark(received: (mail: Mail) => void): Promise<Served> {
  let sent = 0;
  return listen((request, response) => {
    let body = "";
    request.on("data", (chunk: Buffer) => {
      body += chunk.toString();
    });
    request.on("end", () => {
      const message = JSON.parse(body) as { To: string; Subject: string; TextBody: string };
      const paragraphs = message.TextBody.split("\n\n");
      received({
        to: message.To,
        subject: message.Subject,
        paragraphs,
        link: paragraphs.find((each) => /^https?:\/\//u.test(each)) ?? "",
        code: paragraphs.find((each) => /^\d{6}$/u.test(each)) ?? null,
      });
      sent += 1;
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ ErrorCode: 0, Message: "OK", MessageID: `fake-${sent}` }));
    });
  });
}

function json(response: ServerResponse, status: number, body: unknown) {
  response.writeHead(status, { "Content-Type": "application/json" });
  response.end(JSON.stringify(body));
}

const KMS_KEY = "projects/e2e/locations/local/keyRings/services/cryptoKeys/totp";

function fakeKms(): Promise<Served> {
  const key = randomBytes(32);
  return listen((request, response) => {
    const url = new URL(request.url ?? "/", "http://kms");
    if (url.pathname === "/computeMetadata/v1/instance/service-accounts/default/token") {
      if (request.headers["metadata-flavor"] !== "Google") {
        json(response, 403, {});
        return;
      }
      json(response, 200, { access_token: "fake-token", expires_in: 3599, token_type: "Bearer" });
      return;
    }
    let body = "";
    request.on("data", (chunk: Buffer) => {
      body += chunk.toString();
    });
    request.on("end", () => {
      if (request.headers.authorization !== "Bearer fake-token") {
        json(response, 401, { error: { message: "no token" } });
        return;
      }
      const fields = JSON.parse(body) as Record<string, string>;
      const aad = Buffer.from(fields.additionalAuthenticatedData ?? "", "base64");
      if (url.pathname === `/v1/${KMS_KEY}:encrypt`) {
        const iv = randomBytes(12);
        const cipher = createCipheriv("aes-256-gcm", key, iv).setAAD(aad);
        const sealed = Buffer.concat([
          cipher.update(Buffer.from(fields.plaintext ?? "", "base64")),
          cipher.final(),
        ]);
        json(response, 200, {
          name: `${KMS_KEY}/cryptoKeyVersions/1`,
          ciphertext: Buffer.concat([iv, cipher.getAuthTag(), sealed]).toString("base64"),
        });
      } else if (url.pathname === `/v1/${KMS_KEY}:decrypt`) {
        const ciphertext = Buffer.from(fields.ciphertext ?? "", "base64");
        try {
          const decipher = createDecipheriv("aes-256-gcm", key, ciphertext.subarray(0, 12))
            .setAAD(aad)
            .setAuthTag(ciphertext.subarray(12, 28));
          const plaintext = Buffer.concat([
            decipher.update(ciphertext.subarray(28)),
            decipher.final(),
          ]);
          json(response, 200, { plaintext: plaintext.toString("base64") });
        } catch {
          json(response, 400, { error: { message: "Decryption failed" } });
        }
      } else {
        json(response, 404, {});
      }
    });
  });
}

function proxy(target: () => string): Handler {
  return (request, response) => {
    const headers = { ...request.headers };
    if (typeof headers.cookie === "string") {
      headers.cookie = toServices(headers.cookie);
    }
    const upstream = forward(
      `${target()}${request.url}`,
      { method: request.method, headers },
      (answer) => {
        const cookies = answer.headers["set-cookie"]?.map(fromServices);
        response.writeHead(answer.statusCode ?? 502, {
          ...answer.headers,
          ...(cookies ? { "set-cookie": cookies } : {}),
        });
        answer.pipe(response);
      },
    );
    upstream.on("error", () => {
      response.writeHead(502);
      response.end();
    });
    request.pipe(upstream);
  };
}

async function mockIdp(log: string[]): Promise<{ base: string; stop: () => Promise<void> }> {
  const child = spawn(mockIdpBinary, ["--listen", "127.0.0.1:0"], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  createInterface({ input: child.stderr }).on("line", (line) => log.push(`mock-idp: ${line}`));
  const stop = () => stopChild(child);
  const base = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`mock-idp never listened: ${log}`)), 10_000);
    createInterface({ input: child.stdout }).on("line", (line) => {
      try {
        const fields = JSON.parse(line) as { message?: string; base_url?: string };
        if (fields.message === "listening" && fields.base_url) {
          clearTimeout(timer);
          resolve(fields.base_url);
        }
      } catch {}
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  }).catch(async (error: Error) => {
    await stop();
    throw error;
  });
  return { base, stop };
}

export async function startServices(
  options: { sso?: boolean; localhost?: boolean; agents?: boolean } = {},
): Promise<Services> {
  const inbox: Mail[] = [];
  const waiting: (() => void)[] = [];
  const postmark = await fakePostmark((mail) => {
    inbox.push(mail);
    for (const wake of waiting.splice(0)) {
      wake();
    }
  });

  const kms = await fakeKms().catch((error: Error) => {
    postmark.server.close();
    throw error;
  });

  let api = "";
  const page = await standalonePage(
    proxy(() => api),
    options.localhost ? "localhost" : undefined,
  ).catch((error: Error) => {
    postmark.server.close();
    kms.server.close();
    throw error;
  });

  const log: string[] = [];
  const idp = options.sso
    ? await mockIdp(log).catch((error: Error) => {
        page.server.close();
        postmark.server.close();
        kms.server.close();
        throw error;
      })
    : null;
  const ssoFlags = idp
    ? [
        "--google-client-id",
        "mock-client",
        "--google-client-secret",
        "mock-secret",
        "--google-issuer-url",
        `${idp.base}/google`,
        "--github-client-id",
        "mock-client",
        "--github-client-secret",
        "mock-secret",
        "--github-url",
        `${idp.base}/github/`,
        "--github-api-url",
        `${idp.base}/github-api/`,
      ]
    : [];
  const child = spawn(
    servicesBinary,
    [
      "--listen",
      "127.0.0.1:0",
      "--insecure-fast-password-hashing",
      "--postmark-server-token",
      "fake",
      "--postmark-api-url",
      `${postmark.origin}/`,
      "--email-from",
      "Microscope <noreply@microscope.dev>",
      "--public-url",
      `${page.origin}/`,
      "--totp-kms-key",
      KMS_KEY,
      "--totp-kms-api-url",
      `${kms.origin}/`,
      "--totp-metadata-url",
      `${kms.origin}/`,
      ...ssoFlags,
      ...(options.agents
        ? [
            "--agent-token-keys",
            `1:${randomBytes(32).toString("base64url")}`,
            "--agent-refresh-secs",
            "2",
            "--agent-token-ttl-secs",
            "60",
          ]
        : []),
    ],
    { stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, RUST_LOG: "info" } },
  );
  createInterface({ input: child.stderr }).on("line", (line) => log.push(`stderr: ${line}`));
  const stop = async () => {
    await stopChild(child);
    await idp?.stop();
    page.server.close();
    postmark.server.close();
    kms.server.close();
  };
  const address = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`services never listened: ${log}`)), 10_000);
    createInterface({ input: child.stdout }).on("line", (line) => {
      log.push(line);
      let fields: { message?: string; address?: string } | undefined;
      try {
        fields = (JSON.parse(line) as { fields?: typeof fields }).fields;
      } catch {
        return;
      }
      if (fields?.message === "listening" && fields.address) {
        clearTimeout(timer);
        resolve(fields.address);
      }
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`services exited with ${code}; is its database up? ${log.join("\n")}`));
    });
  }).catch(async (error: Error) => {
    await stop();
    throw error;
  });
  api = `http://${address}`;

  return {
    ...page,
    log,
    idp: idp?.base ?? "",
    mail: async (to) => {
      const deadline = Date.now() + 5_000;
      for (;;) {
        const index = inbox.findIndex((mail) => mail.to === to);
        if (index >= 0) {
          return inbox.splice(index, 1)[0] as Mail;
        }
        const left = deadline - Date.now();
        if (left <= 0) {
          throw new Error(`no email to ${to} arrived; services said:\n${log.join("\n")}`);
        }
        await new Promise<void>((wake) => {
          waiting.push(wake);
          setTimeout(wake, left);
        });
      }
    },
    stop,
  };
}

function stopChild(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    child.on("exit", () => resolve());

    child.kill("SIGKILL");
  });
}
