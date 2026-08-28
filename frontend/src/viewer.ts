import { parseCanvasRoute, type CanvasRoute } from "./route";
import type { ColorKeyRenderer } from "./video/color-key";
import { startStreamLoop } from "./video/stream-loop";

/** Coordinates URL-owned presentation and the lifetime of one decoded capture stream. */
export class CanvasViewer {
  /** Identity also gates callbacks that arrive after an old stream was cancelled. */
  private activeStream: AbortController | null = null;
  /** Render-only navigation must not discard the active decoder or request a keyframe. */
  private activePort: number | null = null;

  /** Accepts the renderer and stream runner explicitly so lifecycle behavior needs no DOM in tests. */
  constructor(
    private readonly renderer: Pick<ColorKeyRenderer, "updateParams" | "render" | "clear">,
    private readonly runStream: typeof startStreamLoop = startStreamLoop,
  ) {}

  /**
   * Applies a complete URL snapshot, reconnecting only when its capture port changes.
   * Invalid routes or render failures stop the stream, clear stale pixels, and rethrow.
   */
  applyRoute(hash: string): void {
    let route: CanvasRoute;
    try {
      route = parseCanvasRoute(hash);
      this.renderer.updateParams(route.render);
    } catch (error) {
      this.stop();
      throw error;
    }
    if (this.activeStream !== null && route.port === this.activePort) {
      return;
    }

    this.stop();
    const controller = new AbortController();
    this.activeStream = controller;
    this.activePort = route.port;
    let profileActive = false;
    void this.runStream(route.websocketUrl, {
      onFrame: (frame) => {
        if (this.activeStream !== controller || !profileActive) {
          frame.close();
          return;
        }
        this.renderer.render(frame);
      },
      onStreamState: (state) => {
        if (this.activeStream !== controller) {
          return;
        }
        profileActive = state.profile !== null;
        if (!profileActive) {
          this.renderer.clear();
        }
      },
      onWaiting: () => {
        if (this.activeStream === controller) {
          this.renderer.clear();
        }
      },
    }, controller.signal).catch((error: unknown) => {
      if (this.activeStream === controller) {
        this.stop();
        console.error("TurboCapture canvas stream stopped.", error);
      }
    });
  }

  /** Cancels the stream and clears pixels without disposing reusable GPU resources. */
  stop(): void {
    const controller = this.activeStream;
    this.activeStream = null;
    this.activePort = null;
    controller?.abort();
    this.renderer.clear();
  }
}
