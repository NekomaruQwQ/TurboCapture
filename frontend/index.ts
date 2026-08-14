import "./global.css";
import { parseCanvasRoute, type CanvasRoute } from "./src/route";
import { ColorKeyRenderer } from "./src/video/color-key";
import { startStreamLoop } from "./src/video/stream-loop";

const canvas = document.querySelector<HTMLCanvasElement>("#canvas");
if (canvas === null) {
  throw new Error("The canvas viewer requires a #canvas element.");
}

const renderer = new ColorKeyRenderer(canvas);
let routeGeneration = 0;
let activeStream: AbortController | null = null;

/**
 * Replaces the active capture stream with the one named by the current hash route.
 * Invalid routes deliberately leave a transparent canvas instead of rendering error UI.
 */
function applyCanvasRoute(): void {
  routeGeneration += 1;
  const generation = routeGeneration;
  activeStream?.abort();
  activeStream = null;
  renderer.clear();

  let route: CanvasRoute;
  try {
    route = parseCanvasRoute(window.location.hash);
  } catch (error) {
    console.error("Invalid TurboCapture canvas route.", error);
    return;
  }

  const controller = new AbortController();
  activeStream = controller;
  let profileActive = false;
  void startStreamLoop(route.websocketUrl, {
    onFrame(frame) {
      if (generation !== routeGeneration || !profileActive) {
        frame.close();
        return;
      }
      renderer.render(frame);
    },
    onRenderConfiguration(configuration) {
      if (generation !== routeGeneration) {
        return;
      }
      profileActive = configuration.profile !== null;
      renderer.updateParams({
        keyColors: configuration.render.keyColors,
        kneeLow: configuration.render.kneeLow,
        kneeHigh: configuration.render.kneeHigh,
        binarizationColor: configuration.render.binarizationColor,
      });
      if (!profileActive) {
        renderer.clear();
      }
    },
    onWaiting() {
      if (generation === routeGeneration) {
        renderer.clear();
      }
    },
  }, controller.signal).catch((error: unknown) => {
    if (!controller.signal.aborted && generation === routeGeneration) {
      renderer.clear();
      console.error("TurboCapture canvas stream stopped.", error);
    }
  });
}

window.addEventListener("hashchange", applyCanvasRoute);
window.addEventListener("pagehide", (event) => {
  activeStream?.abort();
  activeStream = null;
  renderer.clear();
  // A back/forward-cache entry must retain its GL resources for `pageshow`.
  if (!event.persisted) {
    renderer.dispose();
  }
});
window.addEventListener("pageshow", (event) => {
  if (event.persisted) {
    applyCanvasRoute();
  }
});
applyCanvasRoute();
