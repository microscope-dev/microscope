// The page served on its own, as `just web` serves it, and the services
// backend behind its `/api/`: a stand-in, or the real server from the
// services repository with a fake Postmark that keeps every email it is
// asked to send.

import { type ChildProcess, spawn } from "node:child_process";
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
import { root } from "./server";

export type Handler = (request: IncomingMessage, response: ServerResponse) => void;

export interface Served {
  /** The page's origin, such as `http://127.0.0.1:41234`. */
  origin: string;
  server: Server;
}

async function listen(handler: Handler): Promise<Served> {
  const server = createServer(handler);
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const { port } = server.address() as AddressInfo;
  return { origin: `http://127.0.0.1:${port}`, server };
}

/** Serves the built page, and hands every request under `/api/` to `api`. */
export function standalonePage(api: Handler): Promise<Served> {
  const dir = path.join(root, "build", "web");
  return listen((request, response) => {
    const url = new URL(request.url ?? "/", "http://page");
    if (url.pathname.startsWith("/api/")) {
      api(request, response);
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
}

/** Where the services binary is, built from its own repository. */
export const servicesBinary =
  process.env.MICROSCOPE_SERVICES_BINARY ??
  path.join(root, "..", "services", "target", "debug", "microscope-services");

export const haveServices = existsSync(servicesBinary);

/** An email services asked Postmark to send. */
export interface Mail {
  to: string;
  subject: string;
  /** The text body's paragraphs. */
  paragraphs: string[];
  /** The link in it, which leads to the page. */
  link: string;
  /** The login code in it, if it has one. */
  code: string | null;
}

export interface Services extends Served {
  /** The next email to `to` that this test has not taken yet. */
  mail(to: string): Promise<Mail>;
  log: string[];
  stop(): Promise<void>;
}

/** Postmark's single-message API, keeping each message and sending none. */
async function fakePostmark(received: (mail: Mail) => void): Promise<Served> {
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
        link: paragraphs.find((each) => /^https?:\/\//.test(each)) ?? "",
        code: paragraphs.find((each) => /^\d{6}$/.test(each)) ?? null,
      });
      sent += 1;
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ ErrorCode: 0, Message: "OK", MessageID: `fake-${sent}` }));
    });
  });
}

/** Passes a request on to services as `just web`'s proxy does. Services'
 * session cookie is `__Host-session`, which a browser keeps only over
 * HTTPS, so the browser holds it as `dev-session` (vite.config.ts). */
function proxy(target: () => string): Handler {
  return (request, response) => {
    const headers = { ...request.headers };
    if (typeof headers.cookie === "string") {
      headers.cookie = headers.cookie.replace(/(^|;\s*)dev-session=/, "$1__Host-session=");
    }
    const upstream = forward(
      `${target()}${request.url}`,
      { method: request.method, headers },
      (answer) => {
        const cookies = answer.headers["set-cookie"]?.map((cookie) =>
          cookie.replace(/^__Host-session=/, "dev-session=").replace(/;\s*Secure/i, ""),
        );
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

/** Starts the page, a fake Postmark, and services with email turned on,
 * whose links lead to the page. Services needs its database: `just up` in
 * the services repository starts one. */
export async function startServices(): Promise<Services> {
  const inbox: Mail[] = [];
  const waiting: (() => void)[] = [];
  const postmark = await fakePostmark((mail) => {
    inbox.push(mail);
    for (const wake of waiting.splice(0)) {
      wake();
    }
  });

  let api = "";
  const page = await standalonePage(proxy(() => api)).catch((error: Error) => {
    postmark.server.close();
    throw error;
  });

  const log: string[] = [];
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
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  createInterface({ input: child.stderr }).on("line", (line) => log.push(`stderr: ${line}`));
  const stop = async () => {
    await stopChild(child);
    page.server.close();
    postmark.server.close();
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
    // Its graceful shutdown takes over a second, and nothing here needs it.
    child.kill("SIGKILL");
  });
}
