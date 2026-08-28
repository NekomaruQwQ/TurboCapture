import {
  parseViewerBinaryMessage,
  parseViewerControlMessage,
  type StreamState,
} from "../protocol";
import { runReconnectingWebSocket, type MarkConnectionHealthy } from "../ws";
import { H264Decoder } from "./decoder";

/** Rendering and lifecycle events emitted by the capture stream. */
export interface StreamLoopCallbacks {
  /** Receives ownership of a decoded frame, which the renderer must close. */
  readonly onFrame: (frame: VideoFrame) => void;
  /** Reports target availability independently of URL-owned shader parameters. */
  readonly onStreamState: (state: StreamState) => void;
  /** Clears stale pixels while disconnected, reconfiguring, or awaiting a keyframe. */
  readonly onWaiting: () => void;
}

/** Runs the decoded-video stream until the route's abort signal is cancelled. */
export async function startStreamLoop(
  websocketUrl: string,
  callbacks: StreamLoopCallbacks,
  signal: AbortSignal,
): Promise<void> {
  callbacks.onWaiting();
  await runReconnectingWebSocket(websocketUrl, signal, async (socket, markHealthy) => {
    try {
      await serveConnection(socket, callbacks, markHealthy);
    } finally {
      callbacks.onWaiting();
    }
  });
}

/** Owns the decoder and event handlers for exactly one WebSocket connection. */
function serveConnection(
  socket: WebSocket,
  callbacks: StreamLoopCallbacks,
  markHealthy: MarkConnectionHealthy,
): Promise<void> {
  return new Promise((resolve, reject) => {
    let settled = false;
    const finish = (error?: Error): void => {
      if (settled) {
        return;
      }
      settled = true;
      decoder.close();
      socket.onmessage = null;
      socket.onclose = null;
      socket.onerror = null;
      if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) {
        socket.close();
      }
      if (error === undefined) {
        resolve();
      } else {
        reject(error);
      }
    };
    const decoder = new H264Decoder((frame) => {
      try {
        callbacks.onFrame(frame);
        markHealthy();
      } catch (error) {
        finish(asError(error));
      }
    }, (error) => finish(error));

    socket.onmessage = (event: MessageEvent<unknown>): void => {
      try {
        if (typeof event.data === "string") {
          const message = parseViewerControlMessage(event.data);
          if (message.type === "error") {
            finish(new Error(`Capture server ${message.code}: ${message.message}`));
          } else {
            callbacks.onStreamState(message.state);
          }
          return;
        }
        if (!(event.data instanceof ArrayBuffer)) {
          throw new Error("Viewer WebSocket received an unsupported message payload.");
        }

        const message = parseViewerBinaryMessage(event.data);
        if (message.kind === "codec") {
          callbacks.onWaiting();
          decoder.configure(message);
        } else {
          decoder.decode(message);
        }
      } catch (error) {
        finish(asError(error));
      }
    };
    socket.onclose = (): void => finish();
    socket.onerror = (): void => finish(new Error("Viewer WebSocket encountered a network error."));
  });
}

/** Converts arbitrary callback failures into errors suitable for reconnection logs. */
function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error(String(value));
}
