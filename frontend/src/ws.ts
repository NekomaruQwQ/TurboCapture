const INITIAL_DELAY_MS = 100;
const MAX_DELAY_MS = 5_000;

/** Marks a connection healthy only after its first decoded frame is rendered. */
export type MarkConnectionHealthy = () => void;

/** Owns one WebSocket connection until it closes or encounters a fatal error. */
export type WebSocketConnectionBody = (
  socket: WebSocket,
  markHealthy: MarkConnectionHealthy,
) => Promise<void>;

/**
 * Runs a localhost WebSocket connection with bounded exponential reconnection.
 * Opening a socket does not reset backoff; only `markHealthy` does, preventing
 * a rapidly opening but unusable server from becoming a hot retry loop.
 */
export async function runReconnectingWebSocket(
  websocketUrl: string,
  signal: AbortSignal,
  body: WebSocketConnectionBody,
): Promise<void> {
  let delayMs = INITIAL_DELAY_MS;

  while (!signal.aborted) {
    const socket = new WebSocket(websocketUrl);
    socket.binaryType = "arraybuffer";
    let healthy = false;
    const markHealthy = (): void => {
      healthy = true;
    };
    const closeOnAbort = (): void => socket.close();
    signal.addEventListener("abort", closeOnAbort, { once: true });

    try {
      await body(socket, markHealthy);
    } catch (error) {
      if (!signal.aborted) {
        console.warn("TurboCapture WebSocket connection failed.", error);
      }
    } finally {
      signal.removeEventListener("abort", closeOnAbort);
      if (socket.readyState !== WebSocket.CLOSED) {
        socket.close();
      }
    }

    if (signal.aborted) {
      break;
    }
    const waitMs = healthy ? INITIAL_DELAY_MS : delayMs;
    if (!await abortableSleep(waitMs, signal)) {
      break;
    }
    delayMs = nextReconnectDelay(waitMs, healthy);
  }
}

/** Returns the delay used after the next failed connection attempt. */
export function nextReconnectDelay(currentDelayMs: number, healthy: boolean): number {
  return healthy ? INITIAL_DELAY_MS : Math.min(currentDelayMs * 2, MAX_DELAY_MS);
}

/** Waits without leaving an uninterruptible timer behind after route changes. */
function abortableSleep(milliseconds: number, signal: AbortSignal): Promise<boolean> {
  return new Promise((resolve) => {
    if (signal.aborted) {
      resolve(false);
      return;
    }
    const timer = window.setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve(true);
    }, milliseconds);
    const onAbort = (): void => {
      window.clearTimeout(timer);
      resolve(false);
    };
    signal.addEventListener("abort", onAbort, { once: true });
  });
}
