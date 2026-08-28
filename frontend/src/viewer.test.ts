import { describe, expect, mock, test } from "bun:test";
import type { ColorKeyParams } from "./video/color-key";
import type { StreamLoopCallbacks } from "./video/stream-loop";
import { CanvasViewer } from "./viewer";

/** One fake stream whose callbacks remain accessible after cancellation to test stale events. */
interface TestConnection {
  /** Validated endpoint received by the stream runner. */
  readonly url: string;
  /** Events normally emitted by the decoder and server. */
  readonly callbacks: StreamLoopCallbacks;
  /** Cancellation signal owned by the viewer. */
  readonly signal: AbortSignal;
}

/** Records rendering and connections without patching browser globals or opening sockets. */
function testViewer() {
  const connections: TestConnection[] = [];
  const renderer = {
    updateParams: mock((_params: ColorKeyParams) => {}),
    render: mock((frame: VideoFrame) => frame.close()),
    clear: mock(() => {}),
  };
  const runStream = mock((url: string, callbacks: StreamLoopCallbacks, signal: AbortSignal) => {
    connections.push({ url, callbacks, signal });
    return new Promise<void>((resolve) => {
      signal.addEventListener("abort", () => resolve(), { once: true });
    });
  });
  return {
    viewer: new CanvasViewer(renderer, runStream),
    renderer,
    runStream,
    /** Fails immediately if the expected connection was never opened. */
    connection(index = 0): TestConnection {
      const connection = connections[index];
      if (connection === undefined) {
        throw new Error(`Missing test connection ${index}.`);
      }
      return connection;
    },
  };
}

/** Only frame ownership is exercised; the fake renderer never reads browser video buffers. */
function testFrame(): VideoFrame {
  const frame: Pick<VideoFrame, "close"> = { close: mock(() => {}) };
  return frame as VideoFrame;
}

describe("CanvasViewer", () => {
  test("updates render-only navigation without reconnecting or clearing", () => {
    const { viewer, renderer, runStream, connection } = testViewer();
    viewer.applyRoute("#/canvas?port=48100");
    const stream = connection();
    const clears = renderer.clear.mock.calls.length;
    stream.callbacks.onStreamState({ profile: "code" });

    viewer.applyRoute("#/canvas?port=48100&key_colors=00ff00&color_key_knee=0.01,0.2");
    const frame = testFrame();
    stream.callbacks.onFrame(frame);

    expect(runStream).toHaveBeenCalledTimes(1);
    expect(stream.signal.aborted).toBe(false);
    expect(renderer.clear).toHaveBeenCalledTimes(clears);
    expect(renderer.updateParams).toHaveBeenLastCalledWith({
      keyColors: [[0, 255, 0]], kneeLow: 0.01, kneeHigh: 0.2, binarizationColor: undefined,
    });
    expect(renderer.render).toHaveBeenCalledWith(frame);
    viewer.stop();
  });

  test("removing parameters restores all defaults on the same connection", () => {
    const { viewer, renderer, runStream } = testViewer();
    viewer.applyRoute("#/canvas?port=1&key_colors=00ff00&color_key_knee=0,1&binarization_color=ffffff");

    viewer.applyRoute("#/canvas?port=1");

    expect(renderer.updateParams).toHaveBeenLastCalledWith({
      keyColors: [], kneeLow: 0.02, kneeHigh: 0.98, binarizationColor: undefined,
    });
    expect(runStream).toHaveBeenCalledTimes(1);
    viewer.stop();
  });

  test("changing ports cancels the old stream and ignores its late callbacks", () => {
    const { viewer, renderer, connection } = testViewer();
    viewer.applyRoute("#/canvas?port=48100");
    const oldStream = connection();
    oldStream.callbacks.onStreamState({ profile: "code" });

    viewer.applyRoute("#/canvas?port=48101");
    const currentStream = connection(1);
    currentStream.callbacks.onStreamState({ profile: "game" });
    const clears = renderer.clear.mock.calls.length;
    oldStream.callbacks.onStreamState({ profile: null });
    oldStream.callbacks.onWaiting();
    const staleFrame = testFrame();
    oldStream.callbacks.onFrame(staleFrame);
    const currentFrame = testFrame();
    currentStream.callbacks.onFrame(currentFrame);

    expect(oldStream.signal.aborted).toBe(true);
    expect(currentStream.url).toBe("ws://127.0.0.1:48101/api/video");
    expect(renderer.clear).toHaveBeenCalledTimes(clears);
    expect(staleFrame.close).toHaveBeenCalledTimes(1);
    expect(renderer.render).toHaveBeenCalledTimes(1);
    expect(renderer.render).toHaveBeenCalledWith(currentFrame);
    viewer.stop();
  });

  test("an invalid URL clears and cancels before a corrected URL starts again", () => {
    const { viewer, renderer, runStream, connection } = testViewer();
    viewer.applyRoute("#/canvas?port=48100");
    const oldStream = connection();
    const clears = renderer.clear.mock.calls.length;

    expect(() => viewer.applyRoute("#/canvas?port=48100&color_key_knee=1,0")).toThrow();

    expect(oldStream.signal.aborted).toBe(true);
    expect(renderer.clear).toHaveBeenCalledTimes(clears + 1);
    viewer.applyRoute("#/canvas?port=48100");
    expect(runStream).toHaveBeenCalledTimes(2);
    viewer.stop();
  });

  test("target loss clears pixels and closes frames until a target returns", () => {
    const { viewer, renderer, connection } = testViewer();
    viewer.applyRoute("#/canvas?port=1&key_colors=00ff00");
    const stream = connection();
    const initialFrame = testFrame();
    stream.callbacks.onFrame(initialFrame);
    stream.callbacks.onStreamState({ profile: "code" });
    stream.callbacks.onFrame(testFrame());
    const clears = renderer.clear.mock.calls.length;

    stream.callbacks.onStreamState({ profile: null });
    const inactiveFrame = testFrame();
    stream.callbacks.onFrame(inactiveFrame);

    expect(renderer.clear).toHaveBeenCalledTimes(clears + 1);
    expect(initialFrame.close).toHaveBeenCalledTimes(1);
    expect(inactiveFrame.close).toHaveBeenCalledTimes(1);
    expect(renderer.render).toHaveBeenCalledTimes(1);
    stream.callbacks.onStreamState({ profile: "game" });
    stream.callbacks.onFrame(testFrame());
    expect(renderer.render).toHaveBeenCalledTimes(2);
    expect(renderer.updateParams).toHaveBeenCalledTimes(1);
    viewer.stop();
  });

  test("waiting clears pixels without forgetting the profile during decoder reset", () => {
    const { viewer, renderer, connection } = testViewer();
    viewer.applyRoute("#/canvas?port=1");
    const stream = connection();
    stream.callbacks.onStreamState({ profile: "code" });
    const clears = renderer.clear.mock.calls.length;

    stream.callbacks.onWaiting();
    stream.callbacks.onFrame(testFrame());

    expect(renderer.clear).toHaveBeenCalledTimes(clears + 1);
    expect(renderer.render).toHaveBeenCalledTimes(1);
    viewer.stop();
  });

  test("stopping for pagehide allows the same route to restart on pageshow", () => {
    const { viewer, runStream, connection } = testViewer();
    viewer.applyRoute("#/canvas?port=1");
    const first = connection();

    viewer.stop();
    viewer.applyRoute("#/canvas?port=1");

    expect(first.signal.aborted).toBe(true);
    expect(runStream).toHaveBeenCalledTimes(2);
    expect(connection(1).signal.aborted).toBe(false);
    viewer.stop();
  });
});
