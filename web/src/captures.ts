import { account } from "./account";
import { type Method, RequestError, type Results } from "./protocol";

const CAPTURE_PREFIX = "capture:";
const FRAMED_CONTENT_TYPE = "application/vnd.microscope.framed";
const CACHE_LIMIT = 200;

export function parseCaptureProfileId(profile: string | null | undefined): number | null {
  if (!profile || !profile.startsWith(CAPTURE_PREFIX)) {
    return null;
  }
  const rest = profile.slice(CAPTURE_PREFIX.length);
  if (!/^[1-9]\d*$/u.test(rest)) {
    return null;
  }
  const parsed = Number(rest);
  return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : null;
}

export function isCaptureProfile(profile: string | null | undefined): boolean {
  return parseCaptureProfileId(profile) !== null;
}

export function captureDownloadUrl(captureId: number): string {
  return `/api/captures/${captureId}/download`;
}

export function decodeFramedResponse<T extends Record<string, unknown>>(
  buffer: ArrayBuffer,
): T & { payload: Uint8Array } {
  if (buffer.byteLength < 4) {
    throw new RequestError("failed", "truncated capture query response");
  }
  const view = new DataView(buffer);
  const jsonLen = view.getUint32(0, true);
  if (4 + jsonLen > buffer.byteLength) {
    throw new RequestError("failed", "malformed capture query response frame");
  }
  const jsonText = new TextDecoder().decode(new Uint8Array(buffer, 4, jsonLen));
  const parsed = JSON.parse(jsonText) as T;
  const payload = new Uint8Array(buffer, 4 + jsonLen);
  return { ...parsed, payload };
}

export async function queryCapture<M extends Method>(
  captureId: number,
  method: M,
  params: unknown,
): Promise<Results[M]> {
  let response: Response;
  try {
    response = await fetch(`/api/captures/${captureId}/query`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ method, params: params ?? {} }),
    });
  } catch (error) {
    throw new RequestError(
      "disconnected",
      error instanceof Error ? error.message : "Could not reach server",
    );
  }

  if (!response.ok) {
    let message = `server answered ${response.status}`;
    try {
      const body = (await response.json()) as { error?: unknown };
      if (typeof body.error === "string" && body.error !== "") {
        message = body.error;
      }
    } catch {}
    const kind =
      response.status === 400 || response.status === 404 || response.status === 403
        ? "invalid"
        : "failed";
    throw new RequestError(kind, message);
  }

  const contentType = response.headers.get("content-type") ?? "";
  if (contentType.includes(FRAMED_CONTENT_TYPE)) {
    const buffer = await response.arrayBuffer();
    return decodeFramedResponse<Record<string, unknown>>(buffer) as unknown as Results[M];
  }
  return (await response.json()) as Results[M];
}

export type CaptureSettled = { ok: true; value: unknown } | { ok: false; error: RequestError };

interface CaptureCacheEntry {
  promise: Promise<CaptureSettled>;
  settled?: CaptureSettled;
}

class CaptureCache {
  readonly #entries = new Map<string, CaptureCacheEntry>();

  peek(key: string): CaptureCacheEntry | undefined {
    return this.#entries.get(key);
  }

  get<M extends Method>(captureId: number, method: M, params: unknown): CaptureCacheEntry {
    const key = captureCacheKey(captureId, method, params);
    const found = this.#entries.get(key);
    if (found) {
      this.#entries.delete(key);
      this.#entries.set(key, found);
      return found;
    }
    const entry: CaptureCacheEntry = {
      promise: queryCapture(captureId, method, params).then(
        (value): CaptureSettled => ({ ok: true, value }),
        (error: unknown): CaptureSettled => ({
          ok: false,
          error: error instanceof RequestError ? error : new RequestError("failed", String(error)),
        }),
      ),
    };
    void (async () => {
      const settled = await entry.promise;
      entry.settled = settled;
      if (!settled.ok && this.#entries.get(key) === entry) {
        this.#entries.delete(key);
      }
    })();
    this.#entries.set(key, entry);
    while (this.#entries.size > CACHE_LIMIT) {
      const oldest = this.#entries.keys().next().value;
      if (oldest === undefined) {
        break;
      }
      this.#entries.delete(oldest);
    }
    return entry;
  }

  clear(): void {
    this.#entries.clear();
  }
}

export function captureCacheKey(captureId: number, method: Method, params: unknown): string {
  return `${captureId} ${method} ${JSON.stringify(params ?? null)}`;
}

export const captureCache = new CaptureCache();

account.subscribe((state, prev) => {
  const prevId = prev.now.kind === "in" ? prev.now.user.id : null;
  const nextId = state.now.kind === "in" ? state.now.user.id : null;
  if (prevId !== nextId) {
    captureCache.clear();
  }
});
