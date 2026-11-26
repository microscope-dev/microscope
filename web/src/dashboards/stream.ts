import type {
  DataGap,
  StreamDimensionsEvent,
  StreamHeartbeatEvent,
  StreamPaneEvent,
  StreamPaneSpec,
  StreamStatusEvent,
  ViewScope,
} from "./types";

export interface ParsedSseEvent {
  event: string;
  data: string;
}

export function parseSseChunk(
  buffer: string,
  chunk: string,
): { events: ParsedSseEvent[]; rest: string } {
  const combined = (buffer + chunk).replaceAll("\r\n", "\n");
  const events: ParsedSseEvent[] = [];
  let offset = 0;
  for (;;) {
    const sep = combined.indexOf("\n\n", offset);
    if (sep === -1) {
      break;
    }
    const block = combined.slice(offset, sep);
    offset = sep + 2;
    let eventName = "message";
    const dataLines: string[] = [];
    for (const line of block.split("\n")) {
      if (line === "" || line.startsWith(":")) {
        continue;
      }
      const colon = line.indexOf(":");
      const field = colon === -1 ? line : line.slice(0, colon);
      let value = colon === -1 ? "" : line.slice(colon + 1);
      if (value.startsWith(" ")) {
        value = value.slice(1);
      }
      if (field === "event") {
        eventName = value;
      } else if (field === "data") {
        dataLines.push(value);
      }
    }
    if (dataLines.length > 0) {
      events.push({ event: eventName, data: dataLines.join("\n") });
    }
  }
  return { events, rest: combined.slice(offset) };
}

export interface DashboardStreamCallbacks {
  onOpen?: () => void;
  onStatus?: (event: StreamStatusEvent) => void;
  onPane?: (event: StreamPaneEvent) => void;
  onDimensions?: (event: StreamDimensionsEvent) => void;
  onHeartbeat?: (event: StreamHeartbeatEvent) => void;
  onDisconnect?: () => void;
  onFatal?: (status: number) => void;
}

export function openDashboardStream(
  body: { scope: ViewScope; session_id?: number; panes: StreamPaneSpec[] },
  callbacks: DashboardStreamCallbacks,
): { close: () => void } {
  let closed = false;
  let controller: AbortController | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let backoffMs = 500;

  const scheduleReconnect = () => {
    if (closed) {
      return;
    }
    const delay = backoffMs;
    backoffMs = Math.min(backoffMs * 2, 5_000);
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      void connect();
    }, delay);
  };

  const dispatch = (ev: ParsedSseEvent) => {
    try {
      const parsed = JSON.parse(ev.data) as unknown;
      if (ev.event === "status") {
        callbacks.onStatus?.(parsed as StreamStatusEvent);
      } else if (ev.event === "pane") {
        callbacks.onPane?.(parsed as StreamPaneEvent);
      } else if (ev.event === "dimensions") {
        callbacks.onDimensions?.(parsed as StreamDimensionsEvent);
      } else if (ev.event === "heartbeat") {
        callbacks.onHeartbeat?.(parsed as StreamHeartbeatEvent);
      }
    } catch {}
  };

  const connect = async () => {
    if (closed) {
      return;
    }
    const ctrl = new AbortController();
    controller = ctrl;
    let readyTimer: ReturnType<typeof setTimeout> | null = setTimeout(() => {
      readyTimer = null;
      ctrl.abort();
    }, 5_000);
    const clearReadyTimer = () => {
      if (readyTimer !== null) {
        clearTimeout(readyTimer);
        readyTimer = null;
      }
    };
    try {
      const response = await fetch("/api/query/stream", {
        method: "POST",
        credentials: "same-origin",
        headers: {
          "Content-Type": "application/json",
          Accept: "text/event-stream",
        },
        body: JSON.stringify(body),
        signal: ctrl.signal,
      });
      if (closed) {
        return;
      }
      if (!response.ok || !response.body) {
        clearReadyTimer();
        if (
          response.status === 400 ||
          response.status === 401 ||
          response.status === 403 ||
          response.status === 404 ||
          response.status === 503
        ) {
          closed = true;
          callbacks.onFatal?.(response.status);
          return;
        }
        callbacks.onDisconnect?.();
        scheduleReconnect();
        return;
      }
      backoffMs = 500;
      callbacks.onOpen?.();

      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      const handleEvent = (ev: ParsedSseEvent) => {
        if (ev.event === "status") {
          clearReadyTimer();
        }
        dispatch(ev);
      };
      const pump = async (): Promise<void> => {
        const { done, value } = await reader.read();
        if (done || closed) {
          return;
        }
        const chunk = decoder.decode(value, { stream: true });
        const parsed = parseSseChunk(buffer, chunk);
        buffer = parsed.rest;
        for (const ev of parsed.events) {
          handleEvent(ev);
        }
        await pump();
      };
      try {
        await pump();
        const trailing = decoder.decode();
        if (trailing !== "" && !closed) {
          const parsed = parseSseChunk(buffer, trailing);
          for (const ev of parsed.events) {
            handleEvent(ev);
          }
        }
      } finally {
        reader.releaseLock();
      }
      if (!closed) {
        callbacks.onDisconnect?.();
        scheduleReconnect();
      }
    } catch {
      if (!closed) {
        callbacks.onDisconnect?.();
        scheduleReconnect();
      }
    } finally {
      clearReadyTimer();
      if (controller === ctrl) {
        controller = null;
      }
    }
  };

  void connect();

  return {
    close: () => {
      closed = true;
      if (reconnectTimer !== null) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      controller?.abort();
      controller = null;
    },
  };
}

export function computeLagMs(
  newestDataNs: number | null,
  serverTimeNs: number | null,
  receivedAtMs: number | null,
  nowMs: number,
): number | null {
  if (newestDataNs === null) {
    return null;
  }
  if (serverTimeNs !== null && receivedAtMs !== null) {
    const serverLag = Math.max(0, (serverTimeNs - newestDataNs) / 1_000_000);
    const elapsed = Math.max(0, nowMs - receivedAtMs);
    return Math.round(serverLag + elapsed);
  }
  return Math.max(0, Math.round(nowMs - newestDataNs / 1_000_000));
}

export function formatDataLag(lagMs: number | null): string | null {
  if (lagMs === null || !Number.isFinite(lagMs)) {
    return null;
  }
  if (lagMs < 2_000) {
    return "fresh";
  }
  const secs = Math.round(lagMs / 1_000);
  if (secs < 60) {
    return `${secs}s behind`;
  }
  const mins = Math.round(secs / 60);
  if (mins < 60) {
    return `${mins}m behind`;
  }
  const hours = Math.round(mins / 60);
  return `${hours}h behind`;
}

export function describeGaps(gaps: readonly DataGap[] | undefined): string | null {
  if (!gaps || gaps.length === 0) {
    return null;
  }
  const lost = gaps.filter((g) => g.reason === "lost").length;
  const count = `${gaps.length} ${gaps.length === 1 ? "gap" : "gaps"}`;
  if (lost === 0) {
    return `${count} · retrying`;
  }
  if (lost === gaps.length) {
    return `${count} · lost`;
  }
  return `${count} · ${lost} lost`;
}
